// SPDX-License-Identifier: MIT
//! The client of `xtask test platform-ui` (M3 slice 6): a window through the `atl-platform-ui` facade, driven only by its frame clock and its Looper's poll, as the launcher's UI thread drives it. [`TestLooper`] is this program's Looper: the platform's `Looper` seam, polled by [`TestLooper::poll`].
//! The harness reads this program's stdout (one `KEY value...` line per event) and the raw RGBA captures it writes to `--out`; it checks them against its own implementation of the pattern below, not against this program.
//!
//! Phases, in order:
//! Buffers are real dma-bufs from gralloc's GBM backend (WSL: vgem), linear, with handle ints as the allocator writes them: the window imports them as dmabuf textures.
//! 1. `--frames` frames: each tick presents one full-window quad whose buffer holds [`pattern`] for frame `f` (rows padded with at least `STRIDE_PAD_PIXELS` extra pixels of 0xEE, so a wrong stride shows), then captures the window to `frame-<f>.rgba`.
//!    Every presented frame's presentation sample is printed (`PRESENT`, the composer's grid follows them), with the monitor's rate (`MONITOR`) before them.
//! 2. one composite scene (crop, flips, rotation, scissor, opacity, colour layers: [`composite_scene`]) captured to `composite.rgba`. Then the resize probe: `set_size` larger then smaller, each size the content gets as `RESIZED`; on a
//!    size-fixed window (maximized / tiled: `Window::is_size_fixed`) `SIZE-FIXED <granted>`, and after each request `FIXED-AFTER <size>` instead of a wait no configure would end.
//! 3. refusals + release: a handle whose pitch is shorter than a row and one with a fourcc the table does not have are refused (never a CPU fallback); a scene that replaces the frame buffer releases it at the next paint, with a read fence (`RELEASES`).
//! 4. an eventfd added with [`TestLooper::watch`] is written from another thread; its callback must run on this thread inside `poll`.
//! 5. `READY`, then commands on stdin, served by a `watch` callback on the stdin fd: `block <ms>` sleeps the UI thread that long (the harness's ping probe); `ime-focus <text> <cursor-byte>` sets the window's `TextInput` content type, surrounding text and cursor rect, then focuses it (`IME-FOCUSED`), and every `TextInputEvent` is printed as an `IME-*` line; `quit` ends.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::io::Write;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use atl_compositor::scene::{BufferContent, Content, Quad, Scene, SceneBuffer, SceneFd};
use atl_gralloc::descriptor::Checked;
use atl_gralloc::format::{self, DRM_FORMAT_MOD_LINEAR, RGBA_8888};
use atl_gralloc::handle::Params;
use atl_gralloc_dmabuf::{Mapping, SYNC_WRITE, begin_cpu_access, end_cpu_access};
use atl_gralloc_service::backend::Backend;
use atl_libgui_parcel::state::FloatRect;
use atl_platform_ui::{
    ContentHints, ContentPurpose, FdEvents, FrameClock, Looper, PhysRect, PhysSize, PlatformUi,
    PlatformWindow, PresentError, PresentTiming, Surrounding, TextInput, TextInputEvent, Ui,
    UiError, Window, WindowAttributes,
};

/// What `ime-focus` declares the field to be; the harness checks the input method sees it (as text-input v3 `content_type`).
const IME_PURPOSE: ContentPurpose = ContentPurpose::Email;
const IME_HINTS: ContentHints = ContentHints::LOWERCASE;
/// The cursor rect `ime-focus` reports (not observable by the harness's IME: input-method v2 has no cursor event; set for coverage of the call).
const IME_CURSOR: PhysRect = PhysRect {
    x: 10.0,
    y: 20.0,
    width: 2.0,
    height: 16.0,
};
use clap::Parser;
use rustix::event::{EventfdFlags, PollFd, PollFlags, eventfd, poll};
use rustix::fs::{MemfdFlags, ftruncate, memfd_create};
use rustix::io::{read, write};
use rustix::time::{ClockId, Timespec, clock_gettime};

#[derive(Parser)]
struct Args {
    /// Directory the RGBA captures are written to.
    #[arg(long)]
    out: PathBuf,
    /// Frames to present, one per frame-clock tick.
    #[arg(long)]
    frames: u32,
    /// Window content width in physical pixels.
    #[arg(long)]
    width: u32,
    /// Size step of the resize probe (phase 2b): the content goes to width/height + step, then - step, per axis.
    #[arg(long)]
    resize_step: u32,
    /// Window content height in physical pixels.
    #[arg(long)]
    height: u32,
    /// How long phases 1-4 may take before the program gives up.
    #[arg(long)]
    timeout_secs: u64,
}

/// Pixels of padding after each buffer row (stride = width + this).
const STRIDE_PAD_PIXELS: u32 = 16;
/// The padding's byte value: a reader that ignores the stride shows it.
const PAD_BYTE: u8 = 0xEE;
/// One `poll` waits at most this long when nothing is ready.
const PASS: Duration = Duration::from_millis(20);
/// Phase 2b on a size-fixed window: ticks after each size request before the size is read. GTK applies set_default_size at the next layout (one tick); a second covers the commit the compositor would answer with a configure.
const FIXED_TICKS: u32 = 2;
/// Pattern frame numbers of the composite scene's five buffers (distinct from the frame series).
const COMPOSITE_FRAMES: [u32; 5] = [100, 101, 102, 103, 104];
/// Phase 3's two full-window buffers: the first is shown, then replaced by the second (distinct from every other buffer id, so a release names one buffer).
const RELEASE_FRAMES: [u32; 2] = [200, 201];
/// Where gralloc's backend looks for a DRM node that allocates.
const DRM_DIR: &str = "/dev/dri";
/// Size of the stand-in metadata page fd: one 4 KiB page, what the mapper's page is (present only checks the fd exists).
const METADATA_PAGE_BYTES: u64 = 4096;
/// The composite scene's colour layers (layer 5 opaque, layer 6 at alpha 0.5): components that are whole multiples of 1/255 (51, 102, 153; 255, 0, 0), so the harness expects exact bytes.
const COMPOSITE_COLOUR: [f32; 3] = [0.2, 0.4, 0.6];
const COMPOSITE_FADED_COLOUR: [f32; 3] = [1.0, 0.0, 0.0];

/// The pattern both sides compute: pixel (`x`, `y`) of frame `f` is R = (x + 7f) mod 256, G = (3y + f) mod 256, B = ((x xor y) + 31f) mod 256, A = 255.
fn pattern(x: u32, y: u32, f: u32) -> [u8; 4] {
    [
        (x.wrapping_add(7 * f) & 0xff) as u8,
        (y.wrapping_mul(3).wrapping_add(f) & 0xff) as u8,
        ((x ^ y).wrapping_add(31 * f) & 0xff) as u8,
        255,
    ]
}

fn emit(line: std::fmt::Arguments<'_>) {
    let mut out = std::io::stdout().lock();
    // a closed stdout means the harness is gone; nothing to report to
    let _ = writeln!(out, "{line}");
    let _ = out.flush();
}

/// CLOCK_MONOTONIC now, in nanoseconds: the clock the presentation samples' times are on.
fn monotonic_ns() -> i64 {
    let t = clock_gettime(ClockId::Monotonic);
    t.tv_sec * 1_000_000_000 + t.tv_nsec
}

/// The GBM backend of gralloc's allocator, started once on the first DRM node that allocates.
fn backend() -> Result<&'static Backend> {
    static BACKEND: OnceLock<Backend> = OnceLock::new();
    if let Some(b) = BACKEND.get() {
        return Ok(b);
    }
    let started = Backend::start(Path::new(DRM_DIR), None).context("gralloc GBM backend")?;
    Ok(BACKEND.get_or_init(|| started))
}

/// A gralloc dma-buf holding `width` x `height` pixels of `frame`'s pattern: allocated `STRIDE_PAD_PIXELS` wider than shown, so every row has padding of 0xEE after it, and described by handle ints as the allocator writes them. The fd must outlive the present call.
fn pattern_buffer(width: u32, height: u32, frame: u32) -> Result<([OwnedFd; 2], Arc<SceneBuffer>)> {
    let info = format::lookup(RGBA_8888).context("RGBA_8888 row")?;
    let checked = Checked {
        name: "gate".to_owned(),
        width: width + STRIDE_PAD_PIXELS,
        height,
        usage: 0,
        format: info,
        modifiers: vec![DRM_FORMAT_MOD_LINEAR],
    };
    let d = backend()?.allocate(&checked)?;
    let pitch = d.pitch as usize;
    let mut bytes = vec![PAD_BYTE; pitch * height as usize];
    for y in 0..height {
        for x in 0..width {
            let at = y as usize * pitch + x as usize * 4;
            bytes[at..at + 4].copy_from_slice(&pattern(x, y, frame));
        }
    }
    let map = Mapping::new(d.fd.as_fd(), bytes.len())?;
    begin_cpu_access(d.fd.as_fd(), SYNC_WRITE)?;
    map.write(0, &bytes);
    end_cpu_access(d.fd.as_fd(), SYNC_WRITE)?;
    // the handle's second fd is the mapper's metadata page (`handle::FD_PAGE`); present never reads it
    let page = memfd_create("gate-page", MemfdFlags::CLOEXEC)?;
    ftruncate(&page, METADATA_PAGE_BYTES)?;
    let params = Params {
        width,
        height,
        pixel_format: RGBA_8888,
        fourcc: info.fourcc,
        pitch: d.pitch,
        size: d.size,
        usage: 0,
        modifier: d.modifier,
        id: u64::from(frame),
    };
    let buffer = SceneBuffer {
        width: width as i32,
        height: height as i32,
        stride: (d.pitch / 4) as i32,
        format: RGBA_8888,
        usage: 0,
        id: u64::from(frame),
        fds: vec![
            SceneFd::new(d.fd.as_raw_fd()),
            SceneFd::new(page.as_raw_fd()),
        ],
        ints: params.to_ints().to_vec(),
    };
    Ok(([d.fd, page], Arc::new(buffer)))
}

fn rect(left: f32, top: f32, right: f32, bottom: f32) -> FloatRect {
    FloatRect {
        left,
        top,
        right,
        bottom,
    }
}

fn quad(layer_id: i32, buffer: Arc<SceneBuffer>, src: FloatRect, dst: FloatRect) -> Quad {
    Quad {
        layer_id,
        name: format!("layer-{layer_id}"),
        content: Content::Buffer(BufferContent {
            buffer,
            frame_number: 1,
            acquire_fence: None,
            src,
            opaque: false,
            buffer_transform: 0,
        }),
        dst,
        clip: None,
        alpha: 1.0,
    }
}

/// The buffer content of a quad [`quad`] made.
fn content(q: &mut Quad) -> &mut BufferContent {
    match &mut q.content {
        Content::Buffer(b) => b,
        Content::Color(_) => unreachable!("quad() makes buffer quads"),
    }
}

fn colour_quad(layer_id: i32, rgb: [f32; 3], dst: FloatRect, alpha: f32) -> Quad {
    Quad {
        layer_id,
        name: format!("colour-{layer_id}"),
        content: Content::Color(rgb),
        dst,
        clip: Some(dst),
        alpha,
    }
}

fn scene_of(quads: Vec<Quad>) -> Scene {
    Scene {
        frame: 1,
        layer_stack: 0,
        quads,
    }
}

/// The frame series' scene: one full-window quad; odd frames are flagged opaque (the `X8` texture path), even ones not.
fn frame_scene(width: u32, height: u32, frame: u32) -> Result<([OwnedFd; 2], Scene)> {
    let (fd, buffer) = pattern_buffer(width, height, frame)?;
    let mut q = quad(
        0,
        buffer,
        rect(0.0, 0.0, width as f32, height as f32),
        rect(0.0, 0.0, width as f32, height as f32),
    );
    content(&mut q).opaque = frame % 2 == 1;
    Ok((fd, scene_of(vec![q])))
}

/// Seven layers, bottom first (5 and 6 are colour layers, listed after 4). Coordinates are physical pixels of a window of at least 256 x 192.
/// 0. frame 100 over the whole window;
/// 1. frame 101, 48 x 32 buffer, crop (8,4)-(40,28), FLIP_H, at (20,30)-(52,54);
/// 2. frame 102, 40 x 24 buffer, ROT_90 (clockwise), at (100,10)-(124,50);
/// 3. frame 103, 48 x 32 buffer at (150,60)-(198,92) scissored to (160,70)-(190,85);
/// 4. frame 104, 40 x 40 buffer at (30,100)-(70,140), alpha 0.5;
/// 5. colour [`COMPOSITE_COLOUR`] at (200,100)-(240,140);
/// 6. colour [`COMPOSITE_FADED_COLOUR`] at (80,150)-(120,180), alpha 0.5.
fn composite_scene(width: u32, height: u32) -> Result<(Vec<OwnedFd>, Scene)> {
    let mut fds = Vec::new();
    let mut buffer = |w, h, f| -> Result<Arc<SceneBuffer>> {
        let (fd, b) = pattern_buffer(w, h, f)?;
        fds.extend(fd);
        Ok(b)
    };
    let [f0, f1, f2, f3, f4] = COMPOSITE_FRAMES;
    let mut layers = vec![quad(
        0,
        buffer(width, height, f0)?,
        rect(0.0, 0.0, width as f32, height as f32),
        rect(0.0, 0.0, width as f32, height as f32),
    )];
    let mut flipped = quad(
        1,
        buffer(48, 32, f1)?,
        rect(8.0, 4.0, 40.0, 28.0),
        rect(20.0, 30.0, 52.0, 54.0),
    );
    content(&mut flipped).buffer_transform = 1;
    layers.push(flipped);
    let mut rotated = quad(
        2,
        buffer(40, 24, f2)?,
        rect(0.0, 0.0, 40.0, 24.0),
        rect(100.0, 10.0, 124.0, 50.0),
    );
    content(&mut rotated).buffer_transform = 4;
    layers.push(rotated);
    let mut scissored = quad(
        3,
        buffer(48, 32, f3)?,
        rect(0.0, 0.0, 48.0, 32.0),
        rect(150.0, 60.0, 198.0, 92.0),
    );
    scissored.clip = Some(rect(160.0, 70.0, 190.0, 85.0));
    layers.push(scissored);
    let mut faded = quad(
        4,
        buffer(40, 40, f4)?,
        rect(0.0, 0.0, 40.0, 40.0),
        rect(30.0, 100.0, 70.0, 140.0),
    );
    faded.alpha = 0.5;
    layers.push(faded);
    layers.push(colour_quad(
        5,
        COMPOSITE_COLOUR,
        rect(200.0, 100.0, 240.0, 140.0),
        1.0,
    ));
    layers.push(colour_quad(
        6,
        COMPOSITE_FADED_COLOUR,
        rect(80.0, 150.0, 120.0, 180.0),
        0.5,
    ));
    Ok((fds, scene_of(layers)))
}

fn save(out: &std::path::Path, name: &str, window: &PlatformWindow) -> Result<String> {
    let capture = window.capture()?;
    let expect = capture.size.width as usize * capture.size.height as usize * 4;
    ensure!(capture.rgba.len() == expect, "capture size mismatch");
    let path = out.join(name);
    std::fs::write(&path, &capture.rgba).with_context(|| path.display().to_string())?;
    Ok(format!(
        "{name} {}x{}",
        capture.size.width, capture.size.height
    ))
}

/// The example's own callback for one of its fds, shared with the poll that runs it.
type OwnCallback = Rc<RefCell<dyn FnMut()>>;

/// The platform's sink for the toolkit's readiness.
type ToolkitSink = Box<dyn FnMut(RawFd, FdEvents)>;

/// This program's Looper: the platform's [`Looper`] seam over a `poll(2)` loop. [`TestLooper::poll`] is its `pollOnce`: it waits for the registered fds, then runs the example's own callbacks ([`TestLooper::watch`]) and hands the toolkit's readiness to the platform's sink.
#[derive(Clone, Default)]
struct TestLooper(Rc<LooperState>);

#[derive(Default)]
struct LooperState {
    /// The toolkit's registrations, with the events each asked for.
    toolkit: RefCell<BTreeMap<RawFd, FdEvents>>,
    /// The example's own fds, with their callbacks.
    own: RefCell<BTreeMap<RawFd, OwnCallback>>,
    /// The platform's sink, set by `Ui::attach`.
    sink: RefCell<Option<ToolkitSink>>,
}

/// Ends a [`TestLooper::watch`] when dropped.
struct OwnWatch {
    looper: Rc<LooperState>,
    fd: RawFd,
}

impl Drop for OwnWatch {
    fn drop(&mut self) {
        self.looper.own.borrow_mut().remove(&self.fd);
    }
}

impl TestLooper {
    /// Runs `on_readable` from [`poll`](TestLooper::poll) whenever `fd` is readable (level-triggered: the callback must drain it), until the guard drops.
    fn watch<F: FnMut() + 'static>(&self, fd: BorrowedFd<'_>, on_readable: F) -> OwnWatch {
        let raw = fd.as_raw_fd();
        let callback: OwnCallback = Rc::new(RefCell::new(on_readable));
        self.0.own.borrow_mut().insert(raw, callback);
        OwnWatch {
            looper: Rc::clone(&self.0),
            fd: raw,
        }
    }

    /// One `pollOnce`: waits up to `timeout` for a registered fd, then runs what is ready, in fd order.
    fn poll(&self, timeout: Duration) -> Result<()> {
        let registered: Vec<(RawFd, PollFlags)> = self
            .0
            .toolkit
            .borrow()
            .iter()
            .map(|(&fd, events)| (fd, poll_flags(*events)))
            .chain(self.0.own.borrow().keys().map(|&fd| (fd, PollFlags::IN)))
            .collect();
        let mut borrowed = Vec::with_capacity(registered.len());
        for &(fd, _) in &registered {
            // SAFETY: a fd this looper registered and has not removed; the toolkit's fds stay open while registered (the mirror syncs its registrations after every pass)
            borrowed.push(unsafe { BorrowedFd::borrow_raw(fd) });
        }
        let mut fds: Vec<PollFd<'_>> = borrowed
            .iter()
            .zip(&registered)
            .map(|(fd, &(_, flags))| PollFd::new(fd, flags))
            .collect();
        let timespec = Timespec {
            tv_sec: i64::try_from(timeout.as_secs())?,
            tv_nsec: i64::from(timeout.subsec_nanos()),
        };
        poll(&mut fds, Some(&timespec))?;
        let ready: Vec<(RawFd, PollFlags)> = fds
            .iter()
            .zip(&registered)
            .filter(|(fd, _)| !fd.revents().is_empty())
            .map(|(fd, &(raw, _))| (raw, fd.revents()))
            .collect();
        drop(fds);
        for (fd, revents) in ready {
            let own = self.0.own.borrow().get(&fd).cloned();
            if let Some(callback) = own {
                (callback.borrow_mut())();
            } else if let Some(sink) = self.0.sink.borrow_mut().as_mut() {
                sink(fd, events_of(revents));
            }
        }
        Ok(())
    }
}

impl Looper for TestLooper {
    fn set_sink(&self, sink: Box<dyn FnMut(RawFd, FdEvents)>) {
        *self.0.sink.borrow_mut() = Some(sink);
    }

    fn add_fd(&self, fd: RawFd, events: FdEvents) -> Result<(), UiError> {
        self.0.toolkit.borrow_mut().insert(fd, events);
        Ok(())
    }

    fn remove_fd(&self, fd: RawFd) -> Result<(), UiError> {
        self.0.toolkit.borrow_mut().remove(&fd);
        Ok(())
    }
}

/// The `poll(2)` events a registration asks for.
fn poll_flags(events: FdEvents) -> PollFlags {
    let mut flags = PollFlags::empty();
    if events.input {
        flags |= PollFlags::IN;
    }
    if events.output {
        flags |= PollFlags::OUT;
    }
    flags
}

/// The readiness `poll(2)` reported, in the Looper's terms.
fn events_of(revents: PollFlags) -> FdEvents {
    FdEvents {
        input: revents.contains(PollFlags::IN),
        output: revents.contains(PollFlags::OUT),
        error: revents.contains(PollFlags::ERR),
        hangup: revents.contains(PollFlags::HUP),
    }
}

/// Polls `looper` until `done` is true or `deadline` passes.
fn spin(looper: &TestLooper, deadline: Instant, what: &str, done: impl Fn() -> bool) -> Result<()> {
    while !done() {
        ensure!(Instant::now() < deadline, "timed out waiting for {what}");
        looper.poll(PASS)?;
    }
    Ok(())
}

fn main() -> Result<()> {
    let args = Args::parse();
    std::fs::create_dir_all(&args.out)?;
    let deadline = Instant::now() + Duration::from_secs(args.timeout_secs);
    let looper = TestLooper::default();
    let ui = PlatformUi::attach(Box::new(looper.clone()))?;
    let window = Rc::new(PlatformWindow::create(
        &ui,
        &WindowAttributes {
            title: "atl gate".to_owned(),
            size: PhysSize {
                width: args.width,
                height: args.height,
            },
        },
    )?);

    // the monitor's rate, and every frame the display showed (its presentation time and period), in order: printed before DONE
    let monitor_hz: Rc<Cell<Option<f32>>> = Rc::default();
    {
        let monitor_hz = Rc::clone(&monitor_hz);
        window.on_monitor(move |m| monitor_hz.set(m.refresh_hz));
    }
    let presents: Rc<RefCell<Vec<(PresentTiming, i64)>>> = Rc::default();
    {
        let presents = Rc::clone(&presents);
        window.on_presented(move |p| presents.borrow_mut().push((p, monotonic_ns())));
    }

    // phase 1: the frame series, one frame per tick
    let frame = Rc::new(Cell::new(0u32));
    let failure: Rc<RefCell<Option<String>>> = Rc::default();
    {
        let (window2, frame, failure) =
            (Rc::clone(&window), Rc::clone(&frame), Rc::clone(&failure));
        let (frames, width, height, out) = (args.frames, args.width, args.height, args.out.clone());
        window.on_tick(move |tick| {
            let f = frame.get();
            let step = || -> Result<()> {
                if f == 0 {
                    emit(format_args!(
                        "MAPPED mapped={} scale={}",
                        window2.is_mapped(),
                        window2.scale().get()
                    ));
                }
                let (_fd, scene) = frame_scene(width, height, f)?;
                window2.present(&scene)?;
                let saved = save(&out, &format!("frame-{f}.rgba"), &window2)?;
                emit(format_args!(
                    "FRAME {f} time_ns={} scale={} {saved}",
                    tick.time_ns,
                    window2.scale().get()
                ));
                frame.set(f + 1);
                if f + 1 < frames {
                    window2.request_frame();
                }
                Ok(())
            };
            if let Err(e) = step() {
                *failure.borrow_mut() = Some(format!("frame {f}: {e:#}"));
            }
        });
    }
    window.request_frame();
    spin(&looper, deadline, "the frame series", || {
        frame.get() >= args.frames || failure.borrow().is_some()
    })?;
    if let Some(e) = failure.borrow().as_ref() {
        bail!("{e}");
    }

    // the ticks from here on are counted: phase 2b waits for them
    let ticks = Rc::new(Cell::new(0u32));
    {
        let ticks = Rc::clone(&ticks);
        window.on_tick(move |_| ticks.set(ticks.get() + 1));
    }

    // phase 2: the composite scene
    let (_fds, scene) = composite_scene(args.width, args.height)?;
    window.present(&scene)?;
    emit(format_args!(
        "COMPOSITE {}",
        save(&args.out, "composite.rgba", &window)?
    ));

    // phase 2b: the content follows the window: set_size asks the platform for a larger, then a smaller size; each size the content gets is reported
    let sizes: Rc<RefCell<Vec<PhysSize>>> = Rc::default();
    {
        let sizes = Rc::clone(&sizes);
        window.on_resize(move |size| sizes.borrow_mut().push(size));
    }
    let step = args.resize_step;
    // a maximized / tiled / fullscreen window keeps the configured size (xdg-shell: that configure's size is binding), so no request can be waited for: each is followed by
    // FIXED_TICKS frame-clock ticks (each a commit + frame callback round trip), then the content size is reported, which must still be the granted one
    let fixed = window.is_size_fixed();
    if fixed {
        let granted = window.size();
        emit(format_args!(
            "SIZE-FIXED {}x{}",
            granted.width, granted.height
        ));
    }
    for (width, height) in [
        (args.width + step, args.height + step),
        (args.width - step, args.height - step),
    ] {
        let want = PhysSize { width, height };
        window.set_size(want);
        if fixed {
            let start = ticks.get();
            for n in 1..=FIXED_TICKS {
                window.request_frame();
                spin(&looper, deadline, "a tick after the size request", || {
                    ticks.get() >= start + n
                })?;
            }
            let now = window.size();
            emit(format_args!("FIXED-AFTER {}x{}", now.width, now.height));
        } else {
            spin(&looper, deadline, "the resized content", || {
                sizes.borrow().last() == Some(&want)
            })?;
        }
    }
    for size in sizes.borrow().iter() {
        emit(format_args!("RESIZED {}x{}", size.width, size.height));
    }

    // phase 3: refusals, then the release of a replaced buffer at the paint after its replacement
    let (_fd, scene) = frame_scene(args.width, args.height, RELEASE_FRAMES[0])?;
    let refused = |ints: &dyn Fn(&mut Params)| -> Result<bool> {
        let mut bad = scene.clone();
        let b = content(&mut bad.quads[0]);
        let mut p = Params::from_handle(b.buffer.fds.len(), &b.buffer.ints)
            .map_err(|e| anyhow::anyhow!("{e:?}"))?;
        ints(&mut p);
        b.buffer = Arc::new(SceneBuffer {
            ints: p.to_ints().to_vec(),
            ..(*b.buffer).clone()
        });
        Ok(matches!(
            window.present(&bad),
            Err(PresentError::Buffer { layer_id: 0, .. })
        ))
    };
    let short_pitch = refused(&|p| p.pitch = p.width * 4 - 4)?;
    let bad_fourcc = refused(&|p| p.fourcc = u32::from_le_bytes(*b"ZZ99"))?;
    let painted: Rc<RefCell<Vec<(u64, bool)>>> = Rc::default();
    let seen = Rc::clone(&painted);
    window.on_painted(move |p| {
        seen.borrow_mut().extend(
            p.releases
                .into_iter()
                .map(|r| (r.buffer_key, r.release_fence.is_some())),
        )
    });
    window.present(&scene)?;
    spin(&looper, deadline, "the first release scene's paint", || {
        // the frame series' last buffer goes back once this scene is painted
        !painted.borrow().is_empty()
    })?;
    painted.borrow_mut().clear();
    let (_fd2, replacing) = frame_scene(args.width, args.height, RELEASE_FRAMES[1])?;
    window.present(&replacing)?;
    let early = painted.borrow().len();
    spin(
        &looper,
        deadline,
        "the release of the replaced buffer",
        || !painted.borrow().is_empty(),
    )?;
    let released = painted.borrow().clone();
    let keys: Vec<String> = released.iter().map(|(k, _)| k.to_string()).collect();
    emit(format_args!(
        "REFUSALS short-pitch={short_pitch} bad-fourcc={bad_fourcc}"
    ));
    emit(format_args!(
        "RELEASES before-paint={early} released={} fenced={} keys={}",
        released.len(),
        released.iter().filter(|(_, f)| *f).count(),
        keys.join(",")
    ));

    // phase 4: a watched fd is serviced on this thread, inside the Looper's poll
    let wake = eventfd(0, EventfdFlags::CLOEXEC | EventfdFlags::NONBLOCK)?;
    let fired = Rc::new(Cell::new(0u32));
    let ui_thread = thread::current().id();
    let on_ui_thread = Rc::new(Cell::new(false));
    let watch = {
        let (fired, on_ui_thread, drain) = (
            Rc::clone(&fired),
            Rc::clone(&on_ui_thread),
            wake.try_clone()?,
        );
        looper.watch(wake.as_fd(), move || {
            // drain the counter: the watch is level-triggered
            let mut counter = [0u8; 8];
            let _ = read(&drain, &mut counter);
            on_ui_thread.set(thread::current().id() == ui_thread);
            fired.set(fired.get() + 1);
        })
    };
    let writer = {
        let wake = wake.try_clone()?;
        thread::spawn(move || {
            thread::sleep(PASS);
            write(&wake, &1u64.to_ne_bytes())
        })
    };
    spin(&looper, deadline, "the watched fd", || fired.get() > 0)?;
    writer
        .join()
        .map_err(|_| anyhow::anyhow!("writer thread panicked"))??;
    emit(format_args!(
        "FDWATCH fired={} on-ui-thread={}",
        fired.get(),
        on_ui_thread.get()
    ));
    drop(watch);

    // the text input's events, one line each (phase 5 `ime-focus` enables it)
    window.text_input().on_events(|event| match event {
        TextInputEvent::Preedit { text, cursor_chars } => emit(format_args!(
            "IME-PREEDIT cursor={cursor_chars} text={text}"
        )),
        TextInputEvent::PreeditEnd => emit(format_args!("IME-PREEDIT-END")),
        TextInputEvent::Commit(text) => emit(format_args!("IME-COMMIT text={text}")),
        TextInputEvent::DeleteSurrounding {
            offset_chars,
            n_chars,
        } => emit(format_args!("IME-DELETE offset={offset_chars} n={n_chars}")),
    });

    // phase 5: commands on stdin, until quit
    let quit = Rc::new(Cell::new(false));
    let pending = Rc::new(RefCell::new(Vec::<u8>::new()));
    let _stdin_watch = {
        let (quit, pending, window) = (Rc::clone(&quit), Rc::clone(&pending), Rc::clone(&window));
        looper.watch(rustix::stdio::stdin(), move || {
            let mut chunk = [0u8; 256];
            let n = read(rustix::stdio::stdin(), &mut chunk).unwrap_or(0);
            if n == 0 {
                quit.set(true);
                return;
            }
            let mut pending = pending.borrow_mut();
            pending.extend_from_slice(&chunk[..n]);
            while let Some(end) = pending.iter().position(|&b| b == b'\n') {
                let line = String::from_utf8_lossy(&pending[..end]).into_owned();
                pending.drain(..=end);
                match line.split_whitespace().collect::<Vec<_>>().as_slice() {
                    ["block", ms] => {
                        let ms: u64 = ms.parse().unwrap_or(0);
                        emit(format_args!("BLOCKING {ms}"));
                        thread::sleep(Duration::from_millis(ms));
                        emit(format_args!("UNBLOCKED"));
                    }
                    ["ime-focus", text, cursor] => {
                        let cursor_byte = cursor.parse().unwrap_or(0);
                        let input = window.text_input();
                        input.set_content(IME_PURPOSE, IME_HINTS);
                        input.set_surrounding(&Surrounding {
                            text: (*text).to_owned(),
                            cursor_byte,
                            anchor_byte: cursor_byte,
                        });
                        input.set_cursor_rect(IME_CURSOR);
                        input.focus_in();
                        emit(format_args!("IME-FOCUSED"));
                    }
                    ["quit"] => quit.set(true),
                    other => emit(format_args!("UNKNOWN-COMMAND {other:?}")),
                }
            }
        })
    };
    emit(format_args!("READY"));
    while !quit.get() {
        looper.poll(PASS)?;
    }
    // `xtask test platform-ui` checks these: the monitor's rate (0 = none reported), then every presentation sample in order, with the monotonic time its report arrived
    emit(format_args!(
        "MONITOR refresh_hz={}",
        monitor_hz.get().unwrap_or(0.0)
    ));
    for (p, seen_ns) in presents.borrow().iter() {
        emit(format_args!(
            "PRESENT time_ns={} refresh_ns={} seen_ns={seen_ns}",
            p.time_ns, p.refresh_ns
        ));
    }
    emit(format_args!("DONE"));
    Ok(())
}
