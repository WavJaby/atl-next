// SPDX-License-Identifier: MIT
//! atl-screencap: a display capture as AOSP's screencap takes it (ISurfaceComposer.captureDisplayById, the reply on IScreenCaptureListener), for the harness. atld spawns it for its control
//! command `capture` (the shell uid; atld main.rs). It asks the registry for the compositor of the package's newest UI-mode launch
//! (GET_COMPOSER), captures the display whole, waits for the reply's fence, reads the reply's GraphicBuffer, and writes the window picture as RGBA bytes to stdout (atld opens the
//! caller's file there). The summary `screencap: WxH fnv1a64=H` goes to stderr. A refusal (`refused: SECURITY`), a missing compositor or a missing reply exits 1 with `screencap: error:`.

// the converter's rlibs, on the crate search path build.rs sets (no cargo dependency names them)
extern crate binder;
extern crate gui_aidl_types_rs;

use std::io::Write;
use std::os::fd::AsFd;
use std::process::ExitCode;
use std::sync::Mutex;
use std::sync::mpsc::{Sender, channel};
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use atl_aidl::aidl::android::gui::CaptureArgs::CaptureArgs;
use atl_aidl::aidl::android::gui::IScreenCaptureListener::{
    BnScreenCaptureListener, IScreenCaptureListener,
};
use atl_aidl::aidl::android::gui::ISurfaceComposer::ISurfaceComposer;
use atl_services::compositor_registry::{GET_COMPOSER, SERVICE};
use atl_services::registry_service::ICompositorRegistry;
use binder::binder_impl::IBinderInternal;
use binder::{BinderFeatures, ExceptionCode, ProcessState, SpIBinder, Status, Strong};
use gui_aidl_types_rs::ScreenCaptureResults;

/// How long the reply may take: the window shows the next published frame, then the readback; the harness polls its digest, this only bounds one call.
const REPLY_WAIT: Duration = Duration::from_secs(10);
/// The fence's wait: a signalled fence answers at once, a stuck one ends the call.
const FENCE_WAIT_SECS: i64 = 10;

/// The window picture as the reply carried it: rows top to bottom, no padding, RGBA bytes.
struct Picture {
    width: u32,
    height: u32,
    rgba: Vec<u8>,
}

/// Receives the reply on a binder thread: the picture (or why there is none) goes to the main thread.
struct Listener {
    tx: Mutex<Sender<Result<Picture>>>,
}

impl binder::Interface for Listener {}

impl IScreenCaptureListener for Listener {
    fn r#unimplemented(&self, _method: &'static str) -> binder::Status {
        binder::StatusCode::UNKNOWN_TRANSACTION.into()
    }

    fn r#onScreenCaptureCompleted(
        &self,
        results: Option<&ScreenCaptureResults>,
    ) -> binder::Result<()> {
        let picture = match results {
            Some(results) => read_picture(results),
            None => Err(anyhow::anyhow!("the reply carried no results")),
        };
        let tx = self.tx.lock().unwrap_or_else(|e| e.into_inner());
        let _ = tx.send(picture);
        Ok(())
    }
}

/// A reply's picture: its fence waited for, then its buffer's rows copied out.
fn read_picture(results: &ScreenCaptureResults) -> Result<Picture> {
    let fence = match &results.fence {
        Ok(fence) => fence.as_ref(),
        Err(status) => bail!("capture refused: status {status}"),
    };
    if let Some(fence) = fence {
        wait_fence(fence.as_fd())?;
    }
    let buffer = results
        .buffer
        .as_ref()
        .context("the reply carried no buffer")?;
    let words: Vec<i32> = buffer
        .payload
        .as_chunks::<4>()
        .0
        .iter()
        .map(|c| i32::from_ne_bytes(*c))
        .collect();
    // GraphicBuffer::flatten: the GB01 header (13 words), then the handle's ints; the fds are the handle's
    ensure!(
        words.len() >= 13 && words[0] == GRAPHIC_BUFFER_MAGIC,
        "the buffer is not a GB01 GraphicBuffer"
    );
    ensure!(
        words[10] as usize == buffer.fds.len(),
        "the buffer's fd count disagrees with its payload"
    );
    let (width, height) = (words[1], words[2]);
    let params = atl_gralloc::handle::Params::from_handle(buffer.fds.len(), &words[13..])
        .map_err(|e| anyhow::anyhow!("the buffer handle: {e}"))?;
    ensure!(
        params.width as i32 == width && params.height as i32 == height,
        "the buffer is {}x{}, its header says {width}x{height}",
        params.width,
        params.height
    );
    let dmabuf = buffer.fds.first().context("the buffer has no dma-buf")?;
    let row = params.width as usize * 4;
    let pitch = params.pitch as usize;
    let len = params.size as usize;
    atl_gralloc_dmabuf::begin_cpu_access(dmabuf.as_fd(), atl_gralloc_dmabuf::SYNC_READ)?;
    let mapping = atl_gralloc_dmabuf::Mapping::read_only(dmabuf.as_fd(), len)?;
    let bytes = mapping.read();
    atl_gralloc_dmabuf::end_cpu_access(dmabuf.as_fd(), atl_gralloc_dmabuf::SYNC_READ)?;
    let rgba = (0..params.height as usize)
        .flat_map(|y| bytes[y * pitch..y * pitch + row].iter().copied())
        .collect();
    Ok(Picture {
        width: params.width,
        height: params.height,
        rgba,
    })
}

/// Waits until `fence` signals (a sync_file is readable once it has).
fn wait_fence(fence: std::os::fd::BorrowedFd<'_>) -> Result<()> {
    let mut fds = [rustix::event::PollFd::new(
        &fence,
        rustix::event::PollFlags::IN,
    )];
    let timeout = rustix::event::Timespec {
        tv_sec: FENCE_WAIT_SECS,
        tv_nsec: 0,
    };
    let ready = rustix::event::poll(&mut fds, Some(&timeout))?;
    ensure!(
        ready == 1,
        "the reply's fence did not signal within {FENCE_WAIT_SECS} s"
    );
    Ok(())
}

/// libui's GraphicBuffer magic 'GB01' (GraphicBuffer.cpp flatten).
const GRAPHIC_BUFFER_MAGIC: i32 = 0x4742_3031;

/// The compositor the registry holds for `package`'s newest UI-mode launch (GET_COMPOSER). The reply starts with its exception header: a caller the registry refuses gets SECURITY.
fn composer_of(package: &str) -> Result<Strong<dyn ISurfaceComposer>> {
    let binder = binder::check_service(SERVICE).context("no compositor registry")?;
    // typed as start.rs types its registry: prepare_transact writes the interface token from the binder's class
    let registry: Strong<dyn ICompositorRegistry> = binder
        .into_interface()
        .context("the compositor registry is not ICompositorRegistry")?;
    let binder = registry.as_binder();
    let mut data = binder.prepare_transact().context("GET_COMPOSER: prepare")?;
    data.write(&package.to_owned())
        .context("GET_COMPOSER: package")?;
    let reply = binder
        .submit_transact(GET_COMPOSER, data, 0)
        .context("GET_COMPOSER: transact")?;
    let status: Status = reply.read().context("GET_COMPOSER: status")?;
    if status.exception_code() == ExceptionCode::SECURITY {
        bail!("refused: SECURITY");
    }
    ensure!(status.is_ok(), "composer: {status}");
    let composer: Option<SpIBinder> = reply.read()?;
    let composer =
        composer.with_context(|| format!("{package}: no compositor (no UI-mode launch of it)"))?;
    Ok(composer.into_interface()?)
}

/// FNV-1a 64: the digest the window gates compare (the same function as atl-composer sink.rs).
fn fnv1a64(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, &b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    })
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            let _ = writeln!(std::io::stderr(), "screencap: error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [package] = args.as_slice() else {
        bail!("usage: atl-screencap <package> (RGBA bytes to stdout)");
    };
    ProcessState::start_thread_pool();
    let composer = composer_of(package)?;
    let display = *composer
        .r#getPhysicalDisplayIds()?
        .as_deref()
        .and_then(|ids| ids.first())
        .context("the compositor reports no display")?;
    let (tx, rx) = channel();
    let listener: Strong<dyn IScreenCaptureListener> = BnScreenCaptureListener::new_binder(
        Listener { tx: Mutex::new(tx) },
        BinderFeatures::default(),
    );
    // AOSP's defaults: RGBA_8888, frame scale 1, no crop, no secure layers, the display's dataspace
    composer.r#captureDisplayById(display, Some(&CaptureArgs::default()), Some(&listener))?;
    let picture = rx
        .recv_timeout(REPLY_WAIT)
        .context("no reply from the compositor")??;
    let mut out = std::io::stdout().lock();
    out.write_all(&picture.rgba)?;
    out.flush()?;
    writeln!(
        std::io::stderr(),
        "screencap: {}x{} fnv1a64={:016x}",
        picture.width,
        picture.height,
        fnv1a64(&picture.rgba)
    )?;
    Ok(())
}
