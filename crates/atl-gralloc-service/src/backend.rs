// SPDX-License-Identifier: MIT
//! The GBM backend: linear dma-bufs from the host's DRM device.
//! GBM objects are not `Send`, so one thread owns the device and serves allocation requests over a channel; the binder threads only send requests.
//! Node choice is by trial, not by driver name: every `renderD*` then every `card*` node under the DRM directory is opened and asked for a test buffer, and the first that gives one wins.
//! Render nodes are tried first because on real GPUs they allocate without master rights; a driver that only allocates dumb / linear buffers on its card node (the kernel's `vgem`, which the WSL setup uses: measured 2026-11-04 on 6.6.87.2-microsoft-standard-WSL2, `DRM_IOCTL_MODE_CREATE_DUMB` on `renderD128` fails EACCES, on `card0` works) is found the same way.

use std::fmt;
use std::fs::OpenOptions;
use std::io;
use std::os::fd::{AsFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Sender, channel};
use std::thread::JoinHandle;

use atl_gralloc::descriptor::{Checked, Descriptor};
use atl_gralloc::format::{
    BLOB, BLOB_ROW_BYTES, DRM_FORMAT_MOD_INVALID, DRM_FORMAT_MOD_LINEAR, RGBA_8888,
};
use atl_gralloc::usage;
use gbm::{BufferObjectFlags, Device, Format};
use rustix::fs::{SeekFrom, seek};

use crate::prime;

/// Where DRM nodes live (the kernel's, `drm_core`).
pub const DRM_DIR: &str = "/dev/dri";

/// The size of the buffer a node is asked for to prove it allocates. Any valid size does; 64 x 64 is what the gate allocates.
const PROBE_EDGE: i32 = 64;

/// GBM flags per usage bit, OR'd over the request's usage. Empty now: every buffer is LINEAR (the only modifier `Checked` admits), which [`flags_for`] adds. Step 2 adds GPU_RENDER_TARGET / GPU_TEXTURE -> RENDERING here, with non-linear modifiers.
const USAGE_FLAGS: &[(u64, BufferObjectFlags)] = &[];

/// The GBM flags for a request: the usage table plus LINEAR when the modifier set allows only LINEAR.
fn flags_for(usage: u64, modifiers: &[u64]) -> BufferObjectFlags {
    let mut flags = USAGE_FLAGS
        .iter()
        .filter(|(bit, _)| usage & bit != 0)
        .fold(BufferObjectFlags::empty(), |acc, (_, f)| acc | *f);
    if modifiers == [DRM_FORMAT_MOD_LINEAR] {
        flags |= BufferObjectFlags::LINEAR;
    }
    flags
}

/// The request a node must serve to be chosen: the window buffer shape the gate allocates.
fn probe_request() -> Checked {
    Descriptor {
        name: b"probe".to_vec(),
        width: PROBE_EDGE,
        height: PROBE_EDGE,
        layer_count: 1,
        format: RGBA_8888,
        usage: usage::CPU_MASK,
        reserved_size: 0,
        option_names: vec![],
        modifiers: vec![],
    }
    .check()
    .expect("the probe request is in the supported set")
}

/// Counters of the backend's fallbacks.
#[derive(Debug, Default)]
pub struct Counters {
    /// LINEAR buffers the driver reported with modifier INVALID, stored as LINEAR. vgem does this (WSL); retired with the GBM modifier path (G5).
    pub modifier_invalid_linear: AtomicU64,
}

/// One linear buffer.
#[derive(Debug)]
pub struct Dmabuf {
    pub fd: OwnedFd,
    /// Bytes per row.
    pub pitch: u32,
    /// Bytes the dma-buf holds.
    pub size: u64,
    /// The layout's DRM modifier: `LINEAR` (0) for what we asked for, even when the driver answers "invalid" for it (vgem does).
    pub modifier: u64,
}

#[derive(Debug)]
pub enum BackendError {
    /// No node can allocate; one line per node tried.
    NoNode(Vec<String>),
    /// The device refused this buffer.
    Alloc(String),
    /// The GBM thread is gone.
    Gone,
}

impl fmt::Display for BackendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoNode(tried) if tried.is_empty() => write!(
                f,
                "no DRM node under {DRM_DIR}: gralloc needs a DRM device that allocates buffers (on WSL: `sudo modprobe vgem`, and be in groups video and render)"
            ),
            Self::NoNode(tried) => write!(
                f,
                "no DRM node can allocate a linear buffer ({}); on WSL: `sudo modprobe vgem`, and be in groups video and render",
                tried.join("; ")
            ),
            Self::Alloc(why) => write!(f, "GBM allocation failed: {why}"),
            Self::Gone => write!(f, "the GBM thread has ended"),
        }
    }
}

impl std::error::Error for BackendError {}

struct Request {
    checked: Checked,
    reply: Sender<Result<Dmabuf, BackendError>>,
}

/// A handle to the GBM thread.
pub struct Backend {
    tx: Option<Sender<Request>>,
    thread: Option<JoinHandle<()>>,
    node: PathBuf,
    counters: Arc<Counters>,
}

/// `renderD*` nodes in number order, then `card*` nodes (names sorted, which orders same-width numbers).
fn candidates(dir: &Path) -> Vec<PathBuf> {
    let mut render = Vec::new();
    let mut card = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with("renderD") {
                render.push(e.path());
            } else if name.starts_with("card") {
                card.push(e.path());
            }
        }
    }
    render.sort();
    card.sort();
    render.extend(card);
    render
}

fn gbm_format(fourcc: u32) -> Result<Format, BackendError> {
    Format::try_from(fourcc)
        .map_err(|_| BackendError::Alloc(format!("fourcc {fourcc:#x} is not a DRM format")))
}

/// The one allocation entry: GBM flags from [`flags_for`], LINEAR only now (the CPU path maps the dma-buf and indexes it by pitch, the layout atl-gralloc's plane layout states).
fn alloc(
    dev: &Device<std::fs::File>,
    c: &Checked,
    counters: &Counters,
) -> Result<Dmabuf, BackendError> {
    let format = gbm_format(c.format.fourcc)?;
    // a BLOB of n bytes: R8 rows of BLOB_ROW_BYTES, ceil(n / row) of them; reported as one row of n bytes
    let blob = c.format.pixel_format == BLOB;
    let (width, height) = if blob {
        (BLOB_ROW_BYTES, c.width.div_ceil(BLOB_ROW_BYTES))
    } else {
        (c.width, c.height)
    };
    let bo = dev
        .create_buffer_object::<()>(width, height, format, flags_for(c.usage, &c.modifiers))
        .map_err(|e| BackendError::Alloc(e.to_string()))?;
    if blob && bo.stride() != BLOB_ROW_BYTES {
        return Err(BackendError::Alloc(format!(
            "BLOB backing pitch {} != {BLOB_ROW_BYTES}: rows not contiguous",
            bo.stride()
        )));
    }
    // not `bo.fd()`: that descriptor is read-only on some drivers (prime.rs)
    let fd = prime::export_dmabuf(dev.as_fd(), prime::gem_handle(&bo))
        .map_err(|e| BackendError::Alloc(format!("exporting the dma-buf: {e}")))?;
    let pitch = if blob { c.width } else { bo.stride() };
    // a dma-buf reports its size by seeking to its end
    let size = seek(&fd, SeekFrom::End(0))
        .map_err(|e| BackendError::Alloc(format!("sizing the dma-buf: {e}")))?;
    let mut modifier = u64::from(bo.modifier());
    // vgem answers INVALID for the LINEAR buffer it was asked for; only then is INVALID read as LINEAR
    if modifier == DRM_FORMAT_MOD_INVALID && c.modifiers == [DRM_FORMAT_MOD_LINEAR] {
        counters
            .modifier_invalid_linear
            .fetch_add(1, Ordering::Relaxed);
        modifier = DRM_FORMAT_MOD_LINEAR;
    }
    Ok(Dmabuf {
        fd,
        pitch,
        size,
        modifier,
    })
}

/// Opens `node` and proves it allocates.
fn open_node(node: &Path, counters: &Counters) -> Result<Device<std::fs::File>, String> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(node)
        .map_err(|e| format!("{}: open: {e}", node.display()))?;
    let dev = Device::new(file).map_err(|e| format!("{}: gbm device: {e}", node.display()))?;
    alloc(&dev, &probe_request(), counters).map_err(|e| format!("{}: {e}", node.display()))?;
    Ok(dev)
}

impl Backend {
    /// Starts the GBM thread on `node`, or on the first node of `dir` that allocates the probe request (a linear RGBA_8888 buffer).
    pub fn start(dir: &Path, node: Option<&Path>) -> Result<Self, BackendError> {
        let nodes = match node {
            Some(n) => vec![n.to_path_buf()],
            None => candidates(dir),
        };
        let counters = Arc::new(Counters::default());
        let thread_counters = Arc::clone(&counters);
        let (tx, rx) = channel::<Request>();
        let (ready_tx, ready_rx) = channel::<Result<PathBuf, BackendError>>();
        let thread = std::thread::Builder::new()
            .name("gbm".into())
            .spawn(move || {
                let mut tried = Vec::new();
                let mut found = None;
                for n in nodes {
                    match open_node(&n, &thread_counters) {
                        Ok(dev) => {
                            found = Some((n, dev));
                            break;
                        }
                        Err(e) => tried.push(e),
                    }
                }
                let Some((node, dev)) = found else {
                    let _ = ready_tx.send(Err(BackendError::NoNode(tried)));
                    return;
                };
                let _ = ready_tx.send(Ok(node));
                while let Ok(r) = rx.recv() {
                    let _ = r.reply.send(alloc(&dev, &r.checked, &thread_counters));
                }
            })
            .map_err(|e| BackendError::Alloc(format!("starting the GBM thread: {e}")))?;
        match ready_rx.recv() {
            Ok(Ok(node)) => Ok(Self {
                tx: Some(tx),
                thread: Some(thread),
                node,
                counters,
            }),
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(e)
            }
            Err(_) => Err(BackendError::Gone),
        }
    }

    /// The node the backend allocates from.
    pub fn node(&self) -> &Path {
        &self.node
    }

    /// The backend's fallback counters.
    pub fn counters(&self) -> &Counters {
        &self.counters
    }

    /// One buffer for `checked`.
    pub fn allocate(&self, checked: &Checked) -> Result<Dmabuf, BackendError> {
        let (reply, answer) = channel();
        self.tx
            .as_ref()
            .ok_or(BackendError::Gone)?
            .send(Request {
                checked: checked.clone(),
                reply,
            })
            .map_err(|_| BackendError::Gone)?;
        answer.recv().map_err(|_| BackendError::Gone)?
    }
}

impl Drop for Backend {
    fn drop(&mut self) {
        // closing the channel ends the thread's loop
        self.tx = None;
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl From<BackendError> for io::Error {
    fn from(e: BackendError) -> Self {
        io::Error::other(e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use atl_gralloc::format::FormatInfo;
    use rustix::fs::{OFlags, fcntl_getfl};

    use super::*;

    /// `renderD*` in number order before any `card*`.
    #[test]
    fn candidate_order() {
        let dir = std::env::temp_dir().join(format!("atl-gralloc-dri-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        for n in [
            "card1",
            "renderD129",
            "card0",
            "renderD128",
            "by-path",
            "controlD64",
        ] {
            std::fs::write(dir.join(n), b"").expect("file");
        }
        let names: Vec<String> = candidates(&dir)
            .iter()
            .map(|p| p.file_name().expect("name").to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["renderD128", "renderD129", "card0", "card1"]);
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn an_empty_or_missing_directory_explains_itself() {
        let e = Backend::start(Path::new("/nonexistent-dri"), None)
            .err()
            .expect("no node");
        assert!(e.to_string().contains("no DRM node under /dev/dri"), "{e}");
        assert!(e.to_string().contains("vgem"));
    }

    #[test]
    fn a_node_that_is_not_drm_is_reported_by_name() {
        let e = Backend::start(Path::new(DRM_DIR), Some(Path::new("/dev/null")))
            .err()
            .expect("not drm");
        assert!(e.to_string().contains("/dev/null"), "{e}");
    }

    /// LINEAR-only modifier sets get GBM LINEAR; any other set gets no LINEAR flag (the usage table is empty now).
    #[test]
    fn flags_follow_the_modifier_set() {
        let any = usage::CPU_MASK | usage::GPU_TEXTURE;
        assert_eq!(
            flags_for(any, &[DRM_FORMAT_MOD_LINEAR]),
            BufferObjectFlags::LINEAR
        );
        assert_eq!(
            flags_for(any, &[DRM_FORMAT_MOD_LINEAR, 1]),
            BufferObjectFlags::empty()
        );
    }

    /// Allocates when the host has a DRM node that does (this machine: vgem); without one the failure must say how to get one.
    #[test]
    fn allocates_or_explains() {
        match Backend::start(Path::new(DRM_DIR), None) {
            Ok(b) => {
                let mut c = probe_request();
                c.height = 48;
                let d = b.allocate(&c).expect("allocate");
                // a 64-pixel ABGR8888 row is 256 bytes; drivers may pad the pitch, never shrink it
                assert!(d.pitch >= 256, "pitch {}", d.pitch);
                assert!(d.size >= u64::from(d.pitch) * 48, "size {}", d.size);
                assert_eq!(d.modifier, DRM_FORMAT_MOD_LINEAR);
                // writable, whatever the driver exported (`read_write`)
                let mode = fcntl_getfl(&d.fd).expect("flags") & OFlags::ACCMODE;
                assert_ne!(mode, OFlags::RDONLY);
                let no_format: &'static FormatInfo = Box::leak(Box::new(FormatInfo {
                    fourcc: 0,
                    ..*c.format
                }));
                c.format = no_format;
                assert!(b.allocate(&c).is_err(), "fourcc 0 is no format");
            }
            Err(e) => assert!(e.to_string().contains("DRM"), "{e}"),
        }
    }
}
