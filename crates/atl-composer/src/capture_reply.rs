// SPDX-License-Identifier: MIT
//! The reply of `captureDisplay` / `captureDisplayById` (AOSP ISurfaceComposer, both oneway): the arguments are checked (capture.rs), the window's picture is read back on the UI thread, written into a
//! gralloc buffer the allocator service makes, and answered on the listener as ScreenCaptureResults. The picture is the window content as the toolkit renders it (`Window::capture`), so the buffer is the window's size.
//! A refusal, or a failed readback or allocation, is answered the same way, in the reply's fence status: a oneway call has no caller to return an error to (SurfaceFlinger does the same).

use std::os::fd::{AsFd, AsRawFd, OwnedFd};

use atl_aidl::aidl::android::gui::IScreenCaptureListener::IScreenCaptureListener;
use atl_aidl::graphics_allocator::aidl::android::hardware::graphics::allocator::BufferDescriptorInfo::BufferDescriptorInfo;
use atl_aidl::graphics_allocator::aidl::android::hardware::graphics::allocator::IAllocator::IAllocator;
use atl_gralloc::format::RGBA_8888;
use atl_gralloc::handle::Params;
use atl_gralloc::vintf::allocator_service_name;
use atl_gralloc_dmabuf::{Mapping, SYNC_READ, SYNC_WRITE, begin_cpu_access, end_cpu_access, export_sync_file};
use atl_libgui_parcel::parcel::FdObject;
use atl_libgui_parcel::state::GraphicBuffer;
use atl_platform_ui::{Capture, UiError};
use binder::{StatusCode, Strong};
use gui_aidl_types_rs::{Flattened, ScreenCaptureResults};

use crate::capture::{Args, check};
use crate::ui_window;

/// BufferUsage.aidl: CPU_READ_OFTEN (3) and CPU_WRITE_OFTEN (3 << 4): the readback is written by the CPU and read by the importer's CPU mapping.
const CPU_USAGE: u64 = 3 | (3 << 4);
/// The buffer's name in the allocator's log (BufferDescriptorInfo.name, NUL-terminated).
const NAME: &[u8] = b"atl-capture";

/// Answers one capture on `listener`: a refused request at once, a picture from the UI thread (`ui_window::capture`).
pub fn serve(
    listener: &Strong<dyn IScreenCaptureListener>,
    request: Result<Args, &'static str>,
    display: (i32, i32),
) {
    if let Err(why) = request.and_then(|args| check(&args, display)) {
        return refuse(listener, StatusCode::BAD_VALUE, why);
    }
    if !ui_window::available() {
        return refuse(
            listener,
            StatusCode::INVALID_OPERATION,
            "no window: the composer is headless",
        );
    }
    let listener = listener.clone();
    ui_window::capture(Box::new(move |picture| answer(&listener, picture)));
}

/// The reply that carries only a status (ScreenCaptureResults.fenceResult = the status, its other fields the header defaults), counted as that status: the oneway call has no reply for the hook to see.
fn refuse(listener: &Strong<dyn IScreenCaptureListener>, status: StatusCode, why: &str) {
    let outcome = atl_diag::outcome(atl_diag::EX_TRANSACTION_FAILED, status as i32);
    atl_diag::count(&outcome, "ISurfaceComposer.captureDisplay", Some(why));
    send(
        listener,
        ScreenCaptureResults {
            fence: Err(status as i32),
            ..Default::default()
        },
    );
}

fn send(listener: &Strong<dyn IScreenCaptureListener>, results: ScreenCaptureResults) {
    if let Err(e) = listener.r#onScreenCaptureCompleted(Some(&results)) {
        tracing::warn!("atl-composer: capture reply: {e}");
    }
}

/// Runs on the UI thread with the readback (or its failure).
fn answer(listener: &Strong<dyn IScreenCaptureListener>, picture: Result<Capture, UiError>) {
    let picture = match picture {
        Ok(picture) => picture,
        Err(e) => return refuse(listener, StatusCode::INVALID_OPERATION, &e.to_string()),
    };
    match buffer(&picture) {
        Ok(results) => send(listener, results),
        Err(status) => refuse(
            listener,
            status,
            "readback could not be written to a buffer",
        ),
    }
}

/// The reply for one picture: the picture written into a new gralloc buffer, with its fence (the CPU write is finished, so the fence is already signalled).
fn buffer(picture: &Capture) -> Result<ScreenCaptureResults, StatusCode> {
    let width = i32::try_from(picture.size.width).map_err(|_| StatusCode::BAD_VALUE)?;
    let height = i32::try_from(picture.size.height).map_err(|_| StatusCode::BAD_VALUE)?;
    let Allocated {
        params,
        ints,
        dmabuf,
        page,
    } = allocate(width, height)?;
    let row = picture.size.width as usize * 4;
    let rows = picture.size.height as usize;
    let pitch = params.pitch as usize;
    let len = params.size as usize;
    if pitch < row || len < pitch * rows || picture.rgba.len() != row * rows {
        tracing::warn!(
            "atl-composer: capture: the allocation ({pitch} byte rows, {len} bytes) does not hold the picture"
        );
        return Err(StatusCode::NO_MEMORY);
    }
    begin_cpu_access(dmabuf.as_fd(), SYNC_WRITE).map_err(|_| StatusCode::UNKNOWN_ERROR)?;
    let mapping = Mapping::new(dmabuf.as_fd(), len).map_err(|_| StatusCode::NO_MEMORY)?;
    for (y, src) in picture.rgba.chunks_exact(row).enumerate() {
        mapping.write(y * pitch, src);
    }
    drop(mapping);
    end_cpu_access(dmabuf.as_fd(), SYNC_WRITE).map_err(|_| StatusCode::UNKNOWN_ERROR)?;
    // the flattened GraphicBuffer: the GB01 words and the handle's ints; its fds are the handle's, in order (dma-buf, metadata page)
    let graphic = GraphicBuffer {
        width,
        height,
        stride: (pitch / 4) as i32,
        format: params.pixel_format,
        layer_count: 1,
        usage: params.usage,
        id: params.id,
        generation_number: 0,
        fds: vec![
            FdObject::new(dmabuf.as_raw_fd(), false),
            FdObject::new(page.as_raw_fd(), false),
        ],
        ints,
    };
    let payload = graphic.flatten_words().map_err(|_| StatusCode::BAD_VALUE)?;
    let fence =
        export_sync_file(dmabuf.as_fd(), SYNC_READ).map_err(|_| StatusCode::UNKNOWN_ERROR)?;
    Ok(ScreenCaptureResults {
        buffer: Some(Flattened {
            payload,
            fds: vec![dmabuf, page],
        }),
        fence: Ok(Some(fence)),
        ..Default::default()
    })
}

/// One buffer the allocator made: its decoded handle, the handle's ints, its dma-buf and metadata page.
struct Allocated {
    params: Params,
    ints: Vec<i32>,
    dmabuf: OwnedFd,
    page: OwnedFd,
}

/// Asks the allocator service for one RGBA_8888 buffer of `width` x `height` (IAllocator.allocate2).
fn allocate(width: i32, height: i32) -> Result<Allocated, StatusCode> {
    // check_interface, not get_interface: the allocator is up before any app starts, and a capture must not poll for it
    let allocator: Strong<dyn IAllocator> = binder::check_interface(&allocator_service_name())
        .map_err(|_| StatusCode::NAME_NOT_FOUND)?;
    let mut info = BufferDescriptorInfo {
        width,
        height,
        layerCount: 1,
        ..Default::default()
    };
    info.name[..NAME.len()].copy_from_slice(NAME);
    info.format.0 = RGBA_8888;
    info.usage.0 = CPU_USAGE as i64;
    let result = allocator
        .r#allocate2(&info, 1)
        .map_err(|_| StatusCode::NO_MEMORY)?;
    let handle = result
        .buffers
        .into_iter()
        .next()
        .ok_or(StatusCode::NO_MEMORY)?;
    let fds: Vec<OwnedFd> = handle.fds.into_iter().map(OwnedFd::from).collect();
    let [dmabuf, page]: [OwnedFd; 2] = fds.try_into().map_err(|_| StatusCode::NO_MEMORY)?;
    let params = Params::from_handle(2, &handle.ints).map_err(|_| StatusCode::NO_MEMORY)?;
    Ok(Allocated {
        params,
        ints: handle.ints,
        dmabuf,
        page,
    })
}
