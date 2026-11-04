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
use std::sync::mpsc::{Sender, channel};
use std::thread::JoinHandle;

use gbm::{BufferObjectFlags, Device, Format};
use rustix::fs::{SeekFrom, seek};

use crate::prime;

/// Where DRM nodes live (the kernel's, `drm_core`).
pub const DRM_DIR: &str = "/dev/dri";

/// `DRM_FORMAT_MOD_LINEAR` and `DRM_FORMAT_MOD_INVALID` (drm_fourcc.h: `0` and `fourcc_mod_code(NONE, (1ULL << 56) - 1)`).
const MOD_LINEAR: u64 = 0;
const MOD_INVALID: u64 = (1 << 56) - 1;

/// The size of the buffer a node is asked for to prove it allocates. Any valid size does; 64 x 64 is what the gate allocates.
const PROBE_EDGE: u32 = 64;

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
    width: u32,
    height: u32,
    fourcc: u32,
    reply: Sender<Result<Dmabuf, BackendError>>,
}

/// A handle to the GBM thread.
pub struct Backend {
    tx: Option<Sender<Request>>,
    thread: Option<JoinHandle<()>>,
    node: PathBuf,
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

fn alloc(
    dev: &Device<std::fs::File>,
    width: u32,
    height: u32,
    fourcc: u32,
) -> Result<Dmabuf, BackendError> {
    let format = gbm_format(fourcc)?;
    // LINEAR: the CPU path maps the dma-buf and indexes it by pitch (the layout atl-gralloc's plane layout states)
    let bo = dev
        .create_buffer_object::<()>(width, height, format, BufferObjectFlags::LINEAR)
        .map_err(|e| BackendError::Alloc(e.to_string()))?;
    // not `bo.fd()`: that descriptor is read-only on some drivers (prime.rs)
    let fd = prime::export_dmabuf(dev.as_fd(), prime::gem_handle(&bo))
        .map_err(|e| BackendError::Alloc(format!("exporting the dma-buf: {e}")))?;
    let pitch = bo.stride();
    // a dma-buf reports its size by seeking to its end
    let size = seek(&fd, SeekFrom::End(0))
        .map_err(|e| BackendError::Alloc(format!("sizing the dma-buf: {e}")))?;
    let m = u64::from(bo.modifier());
    Ok(Dmabuf {
        fd,
        pitch,
        size,
        modifier: if m == MOD_INVALID { MOD_LINEAR } else { m },
    })
}

/// Opens `node` and proves it allocates.
fn open_node(node: &Path, fourcc: u32) -> Result<Device<std::fs::File>, String> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(node)
        .map_err(|e| format!("{}: open: {e}", node.display()))?;
    let dev = Device::new(file).map_err(|e| format!("{}: gbm device: {e}", node.display()))?;
    alloc(&dev, PROBE_EDGE, PROBE_EDGE, fourcc).map_err(|e| format!("{}: {e}", node.display()))?;
    Ok(dev)
}

impl Backend {
    /// Starts the GBM thread on `node`, or on the first node of `dir` that allocates a linear `probe_fourcc` buffer.
    pub fn start(dir: &Path, node: Option<&Path>, probe_fourcc: u32) -> Result<Self, BackendError> {
        let nodes = match node {
            Some(n) => vec![n.to_path_buf()],
            None => candidates(dir),
        };
        let (tx, rx) = channel::<Request>();
        let (ready_tx, ready_rx) = channel::<Result<PathBuf, BackendError>>();
        let thread = std::thread::Builder::new()
            .name("gbm".into())
            .spawn(move || {
                let mut tried = Vec::new();
                let mut found = None;
                for n in nodes {
                    match open_node(&n, probe_fourcc) {
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
                    let _ = r.reply.send(alloc(&dev, r.width, r.height, r.fourcc));
                }
            })
            .map_err(|e| BackendError::Alloc(format!("starting the GBM thread: {e}")))?;
        match ready_rx.recv() {
            Ok(Ok(node)) => Ok(Self {
                tx: Some(tx),
                thread: Some(thread),
                node,
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

    /// One linear buffer of `fourcc`.
    pub fn allocate(&self, width: u32, height: u32, fourcc: u32) -> Result<Dmabuf, BackendError> {
        let (reply, answer) = channel();
        self.tx
            .as_ref()
            .ok_or(BackendError::Gone)?
            .send(Request {
                width,
                height,
                fourcc,
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
    use atl_gralloc::format::DRM_FORMAT_ABGR8888;
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
        let e = Backend::start(Path::new("/nonexistent-dri"), None, DRM_FORMAT_ABGR8888)
            .err()
            .expect("no node");
        assert!(e.to_string().contains("no DRM node under /dev/dri"), "{e}");
        assert!(e.to_string().contains("vgem"));
    }

    #[test]
    fn a_node_that_is_not_drm_is_reported_by_name() {
        let e = Backend::start(
            Path::new(DRM_DIR),
            Some(Path::new("/dev/null")),
            DRM_FORMAT_ABGR8888,
        )
        .err()
        .expect("not drm");
        assert!(e.to_string().contains("/dev/null"), "{e}");
    }

    /// Allocates when the host has a DRM node that does (this machine: vgem); without one the failure must say how to get one.
    #[test]
    fn allocates_or_explains() {
        match Backend::start(Path::new(DRM_DIR), None, DRM_FORMAT_ABGR8888) {
            Ok(b) => {
                let d = b.allocate(64, 48, DRM_FORMAT_ABGR8888).expect("allocate");
                // a 64-pixel ABGR8888 row is 256 bytes; drivers may pad the pitch, never shrink it
                assert!(d.pitch >= 256, "pitch {}", d.pitch);
                assert!(d.size >= u64::from(d.pitch) * 48, "size {}", d.size);
                assert_eq!(d.modifier, MOD_LINEAR);
                // writable, whatever the driver exported (`read_write`)
                let mode = fcntl_getfl(&d.fd).expect("flags") & OFlags::ACCMODE;
                assert_ne!(mode, OFlags::RDONLY);
                assert!(b.allocate(64, 48, 0).is_err(), "fourcc 0 is no format");
            }
            Err(e) => assert!(e.to_string().contains("DRM"), "{e}"),
        }
    }
}
