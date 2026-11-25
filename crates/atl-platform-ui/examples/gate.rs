// SPDX-License-Identifier: MIT
//! The client of `xtask test platform-ui` (M3 slice 6): a window through the `atl-platform-ui` facade, driven only by its frame clock, as the launcher's process drives it. The module runs the window on its own display thread; this program's threads are app threads.
//! Threads (display-thread plan D2/D3): the main thread drives the phases, reaching the window only by `call` ([`on_display`]; the window lives in [`WINDOW`] on the display thread) and waiting on what the window's callbacks (run on the display thread) record; [`AppThread`] is the app's UI thread, a `poll(2)` loop ([`TestLooper`]) that drains the liveness poster ([`TestPoster`]), so the ping hold is seen apart from the display thread.
//! The module's log records go to stderr (`display: ping hold armed`, the `pings_held` / `pings_answered` counts), where the harness reads them.
//! The harness reads this program's stdout (one `KEY value...` line per event) and the raw RGBA captures it writes to `--out`; it checks them against its own implementation of the pattern below, not against this program.
//!
//! Phases, in order:
//! Buffers are real dma-bufs from gralloc's GBM backend (WSL: vgem), linear, with handle ints as the allocator writes them: the window imports them as dmabuf textures.
//! 1. `--frames` frames: each tick presents one full-window quad whose buffer holds [`pattern`] for frame `f` (rows padded with at least `STRIDE_PAD_PIXELS` extra pixels of 0xEE, so a wrong stride shows), then captures the window to `frame-<f>.rgba`.
//!    Every presented frame's presentation sample is printed (`PRESENT`, the composer's grid follows them), with the monitor's rate (`MONITOR`) before them.
//! 2. one composite scene (crop, flips, rotation, scissor, opacity, colour layers: [`composite_scene`]) captured to `composite.rgba`. Then the resize probe: `set_size` larger then smaller, each size the content gets as `RESIZED`; on a
//!    size-fixed window (maximized / tiled: `Window::is_size_fixed`) `SIZE-FIXED <granted>`, and after each request `FIXED-AFTER <size>` instead of a wait no configure would end.
//! 3. refusals + release: a handle whose pitch is shorter than a row and one with a fourcc the table does not have are refused (never a CPU fallback); a scene that replaces the frame buffer releases it at the next paint, with a read fence (`RELEASES`).
//! 4. `call` (`CALL`): a closure posted to [`AppThread`] runs there, inside its poll (`posted-on-app`); its `call` returns the window's size read on the display thread (`app-on-toolkit`, `app-size`); a call made on the display thread itself runs inline (`inline`), which the module logs as `display: call on display`.
//! 5. `READY`, then commands on stdin, read by the main thread: `block <ms>` sleeps the app's UI thread ([`AppThread`]) that long, printing `BLOCKING` when it starts (the harness's ping probe; the display thread keeps running); `ime-focus <text> <cursor-byte>` sets the window's `TextInput` content type, surrounding text and cursor rect, then focuses it (`IME-FOCUSED`), and every `TextInputEvent` is printed as an `IME-*` line; `quit` (or the end of stdin) ends.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, VecDeque};
use std::io::{BufRead, Write};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
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
    ContentHints, ContentPurpose, FrameClock, LooperPoster, PhysRect, PhysSize, PlatformInvoker,
    PlatformUi, PlatformWindow, PresentError, PresentTiming, Scale, Surrounding, TextInput,
    TextInputEvent, Ui, Window, WindowAttributes,
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

/// Two scales are the same within this: wp_fractional_scale sends n/120 (1.6 = 192/120, exact), the epsilon absorbs a float's rounding.
const SCALE_EPSILON: f64 = 1e-3;

#[derive(Parser)]
struct Args {
    /// Directory the RGBA captures are written to.
    #[arg(long)]
    out: PathBuf,
    /// Frames to present, one per frame-clock tick.
    #[arg(long)]
    frames: u32,
    /// The scale the window waits for before its first frame: the harness changes the output to it once MAPPED is printed, so the fractional scale arrives after the first layout (as on Hyprland). Absent: the output's scale from the start.
    #[arg(long, value_name = "SCALE")]
    await_scale: Option<f64>,
    /// The scale the content size is expressed in (`WindowAttributes::scale`).
    #[arg(long)]
    scale: f64,
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
/// One wait step: a `poll` of the app thread with nothing ready, or one look at what the display thread recorded.
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
        device: backend()?.device(),
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
        Content::Color(_) | Content::Sideband(_) => unreachable!("quad() makes buffer quads"),
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

/// The app thread's own callback for one of its fds, shared with the poll that runs it.
type OwnCallback = Rc<RefCell<dyn FnMut()>>;

/// The app thread's Looper: a `poll(2)` loop over its own fds. [`TestLooper::poll`] is its `pollOnce`: it waits for the watched fds, then runs their callbacks ([`TestLooper::watch`]).
#[derive(Clone, Default)]
struct TestLooper(Rc<RefCell<BTreeMap<RawFd, OwnCallback>>>);

/// Ends a [`TestLooper::watch`] when dropped.
struct OwnWatch {
    looper: TestLooper,
    fd: RawFd,
}

impl Drop for OwnWatch {
    fn drop(&mut self) {
        self.looper.0.borrow_mut().remove(&self.fd);
    }
}

impl TestLooper {
    /// Runs `on_readable` from [`poll`](TestLooper::poll) whenever `fd` is readable (level-triggered: the callback must drain it), until the guard drops.
    fn watch<F: FnMut() + 'static>(&self, fd: BorrowedFd<'_>, on_readable: F) -> OwnWatch {
        let raw = fd.as_raw_fd();
        let callback: OwnCallback = Rc::new(RefCell::new(on_readable));
        self.0.borrow_mut().insert(raw, callback);
        OwnWatch {
            looper: self.clone(),
            fd: raw,
        }
    }

    /// One `pollOnce`: waits up to `timeout` for a watched fd, then runs what is ready, in fd order.
    fn poll(&self, timeout: Duration) -> Result<()> {
        let registered: Vec<RawFd> = self.0.borrow().keys().copied().collect();
        let mut borrowed = Vec::with_capacity(registered.len());
        for &fd in &registered {
            // SAFETY: a fd a live watch registered; its owner keeps it open while watched
            borrowed.push(unsafe { BorrowedFd::borrow_raw(fd) });
        }
        let mut fds: Vec<PollFd<'_>> = borrowed
            .iter()
            .map(|fd| PollFd::new(fd, PollFlags::IN))
            .collect();
        let timespec = Timespec {
            tv_sec: i64::try_from(timeout.as_secs())?,
            tv_nsec: i64::from(timeout.subsec_nanos()),
        };
        poll(&mut fds, Some(&timespec))?;
        let ready: Vec<RawFd> = fds
            .iter()
            .zip(&registered)
            .filter(|(fd, _)| !fd.revents().is_empty())
            .map(|(_, &raw)| raw)
            .collect();
        drop(fds);
        for fd in ready {
            let callback = self.0.borrow().get(&fd).cloned();
            if let Some(callback) = callback {
                (callback.borrow_mut())();
            }
        }
        Ok(())
    }
}

/// One posted closure.
type Posted = Box<dyn FnOnce() + Send>;

/// The app thread's post queue: a FIFO and an eventfd its [`TestLooper`] watches (the shape of atl-looper's `Poster` over a `poll(2)` loop).
struct PostQueue {
    tasks: Mutex<VecDeque<Posted>>,
    wake: OwnedFd,
}

impl PostQueue {
    /// Runs every closure queued so far, in post order (on the app thread, inside its poll).
    fn drain(&self) {
        let mut counter = [0u8; 8];
        let _ = read(&self.wake, &mut counter);
        loop {
            let task = self
                .tasks
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .pop_front();
            match task {
                Some(task) => task(),
                None => break,
            }
        }
    }
}

/// The liveness poster the toolkit gets: posts to [`AppThread`]'s Looper.
#[derive(Clone)]
struct TestPoster(Arc<PostQueue>);

impl LooperPoster for TestPoster {
    fn post(&self, f: Box<dyn FnOnce() + Send>) {
        self.0
            .tasks
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push_back(f);
        let _ = write(&self.0.wake, &1u64.to_ne_bytes());
    }
}

/// The app's UI thread: its own [`TestLooper`], polled until [`stop`](AppThread::stop), running what is posted to it.
struct AppThread {
    poster: TestPoster,
    quit: Arc<AtomicBool>,
    handle: thread::JoinHandle<Result<()>>,
}

impl AppThread {
    fn spawn() -> Result<Self> {
        let queue = Arc::new(PostQueue {
            tasks: Mutex::new(VecDeque::new()),
            wake: eventfd(0, EventfdFlags::CLOEXEC | EventfdFlags::NONBLOCK)?,
        });
        let quit = Arc::new(AtomicBool::new(false));
        let (q, stop) = (Arc::clone(&queue), Arc::clone(&quit));
        let handle =
            thread::Builder::new()
                .name("app-ui".to_owned())
                .spawn(move || -> Result<()> {
                    let looper = TestLooper::default();
                    let drain = Arc::clone(&q);
                    let _watch = looper.watch(q.wake.as_fd(), move || drain.drain());
                    while !stop.load(Ordering::Relaxed) {
                        looper.poll(PASS)?;
                    }
                    Ok(())
                })?;
        Ok(AppThread {
            poster: TestPoster(queue),
            quit,
            handle,
        })
    }

    /// Ends the thread after what was posted before this call has run.
    fn stop(self) -> Result<()> {
        let quit = Arc::clone(&self.quit);
        self.poster
            .post(Box::new(move || quit.store(true, Ordering::Relaxed)));
        self.handle
            .join()
            .map_err(|_| anyhow::anyhow!("app thread panicked"))?
    }
}

thread_local! {
    /// The window, on the display thread: made, used and dropped there (it is not Send); this program's threads reach it by [`on_display`].
    static WINDOW: RefCell<Option<Rc<PlatformWindow>>> = const { RefCell::new(None) };
}

/// Runs `f` with the window on the display thread and returns its result (`call`: this thread waits, the display thread keeps running).
fn on_display<R: Send + 'static>(
    invoker: &PlatformInvoker,
    f: impl FnOnce(&Rc<PlatformWindow>) -> Result<R> + Send + 'static,
) -> Result<R> {
    atl_platform_ui::call(invoker, move || {
        WINDOW.with_borrow(|w| match w {
            Some(w) => f(w),
            None => bail!("no window on the display thread"),
        })
    })
    .context("the display thread ended")?
}

/// Shared between the display thread's callbacks and this program's threads.
fn locked<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// Waits until `done` is true or `deadline` passes, looking every [`PASS`].
fn spin(deadline: Instant, what: &str, done: impl Fn() -> bool) -> Result<()> {
    while !done() {
        ensure!(Instant::now() < deadline, "timed out waiting for {what}");
        thread::sleep(PASS);
    }
    Ok(())
}

fn main() -> Result<()> {
    let args = Args::parse();
    // the module's records (the ping hold's line and counts) on stderr, which the harness keeps in a file
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .with_max_level(tracing_subscriber::filter::LevelFilter::INFO)
        .init();
    std::fs::create_dir_all(&args.out)?;
    let deadline = Instant::now() + Duration::from_secs(args.timeout_secs);
    let app = AppThread::spawn()?;
    // the window imports what this gate's own GBM backend allocates
    let ui = Arc::new(PlatformUi::attach(
        Box::new(app.poster.clone()),
        Some(backend()?.device()),
    )?);
    let invoker = Arc::new(ui.invoker());

    // the monitor's rate, and every frame the display showed (its presentation time and period), in order: printed before DONE
    let monitor_hz: Arc<Mutex<Option<f32>>> = Arc::default();
    let presents: Arc<Mutex<Vec<(PresentTiming, i64)>>> = Arc::default();
    {
        let (session, monitor_hz, presents) = (
            Arc::clone(&ui),
            Arc::clone(&monitor_hz),
            Arc::clone(&presents),
        );
        let attributes = WindowAttributes {
            title: "atl gate".to_owned(),
            app_id: None,
            size: PhysSize {
                width: args.width,
                height: args.height,
            },
            scale: Scale::new(args.scale).context("--scale")?,
        };
        atl_platform_ui::call(&*invoker, move || -> Result<()> {
            let window = Rc::new(PlatformWindow::create(&session, &attributes)?);
            window.on_monitor(move |m| *locked(&monitor_hz) = m.refresh_hz);
            window.on_presented(move |p| locked(&presents).push((p, monotonic_ns())));
            WINDOW.set(Some(window));
            Ok(())
        })
        .context("the display thread ended")??;
    }

    // phase 1: the frame series, one frame per tick
    let frame = Arc::new(AtomicU32::new(0));
    let failure: Arc<Mutex<Option<String>>> = Arc::default();
    {
        let (frame, failure) = (Arc::clone(&frame), Arc::clone(&failure));
        let (frames, width, height, out) = (args.frames, args.width, args.height, args.out.clone());
        let (await_scale, scale_deadline) = (args.await_scale, deadline);
        on_display(&invoker, move |window| {
            let window2 = Rc::downgrade(window);
            let mapped_printed = Cell::new(false);
            window.on_tick(move |tick| {
                let Some(window2) = window2.upgrade() else {
                    return;
                };
                let f = frame.load(Ordering::Acquire);
                let step = || -> Result<()> {
                    if f == 0 && !mapped_printed.replace(true) {
                        emit(format_args!(
                            "MAPPED mapped={} scale={}",
                            window2.is_mapped(),
                            window2.scale().get()
                        ));
                    }
                    // no frame until the window is at the target scale with the requested size: the frames then all show the state it ends at
                    let settled = |target: f64| {
                        let size = window2.size();
                        (window2.scale().get() - target).abs() <= SCALE_EPSILON
                            && (size.width, size.height) == (width, height)
                    };
                    if let Some(target) = await_scale.filter(|t| !settled(*t)) {
                        let size = window2.size();
                        ensure!(
                            Instant::now() < scale_deadline,
                            "the window never settled at scale {target} and {width}x{height}: scale {}, size {}x{}",
                            window2.scale().get(),
                            size.width,
                            size.height
                        );
                        window2.request_frame();
                        return Ok(());
                    }
                    let (_fd, scene) = frame_scene(width, height, f)?;
                    window2.present(&scene)?;
                    let saved = save(&out, &format!("frame-{f}.rgba"), &window2)?;
                    emit(format_args!(
                        "FRAME {f} time_ns={} scale={} {saved}",
                        tick.time_ns,
                        window2.scale().get()
                    ));
                    frame.store(f + 1, Ordering::Release);
                    if f + 1 < frames {
                        window2.request_frame();
                    }
                    Ok(())
                };
                if let Err(e) = step() {
                    *locked(&failure) = Some(format!("frame {f}: {e:#}"));
                }
            });
            window.request_frame();
            Ok(())
        })?;
    }
    spin(deadline, "the frame series", || {
        frame.load(Ordering::Acquire) >= args.frames || locked(&failure).is_some()
    })?;
    if let Some(e) = locked(&failure).as_ref() {
        bail!("{e}");
    }

    // the ticks from here on are counted: phase 2b waits for them
    let ticks = Arc::new(AtomicU32::new(0));
    {
        let ticks = Arc::clone(&ticks);
        on_display(&invoker, move |window| {
            window.on_tick(move |_| {
                ticks.fetch_add(1, Ordering::AcqRel);
            });
            Ok(())
        })?;
    }

    // phase 2: the composite scene
    let (_fds, scene) = composite_scene(args.width, args.height)?;
    let out = args.out.clone();
    let composite = on_display(&invoker, move |window| {
        window.present(&scene)?;
        save(&out, "composite.rgba", window)
    })?;
    emit(format_args!("COMPOSITE {composite}"));

    // phase 2b: the content follows the window: set_size asks the platform for a larger, then a smaller size; each size the content gets is reported
    let sizes: Arc<Mutex<Vec<PhysSize>>> = Arc::default();
    {
        let sizes = Arc::clone(&sizes);
        on_display(&invoker, move |window| {
            window.on_resize(move |size| locked(&sizes).push(size));
            Ok(())
        })?;
    }
    let step = args.resize_step;
    // a maximized / tiled / fullscreen window keeps the configured size (xdg-shell: that configure's size is binding), so no request can be waited for: each is followed by
    // FIXED_TICKS frame-clock ticks (each a commit + frame callback round trip), then the content size is reported, which must still be the granted one
    let (fixed, granted) = on_display(&invoker, |window| {
        Ok((window.is_size_fixed(), window.size()))
    })?;
    if fixed {
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
        on_display(&invoker, move |window| {
            window.set_size(want);
            Ok(())
        })?;
        if fixed {
            let start = ticks.load(Ordering::Acquire);
            for n in 1..=FIXED_TICKS {
                on_display(&invoker, |window| {
                    window.request_frame();
                    Ok(())
                })?;
                spin(deadline, "a tick after the size request", || {
                    ticks.load(Ordering::Acquire) >= start + n
                })?;
            }
            let now = on_display(&invoker, |window| Ok(window.size()))?;
            emit(format_args!("FIXED-AFTER {}x{}", now.width, now.height));
        } else {
            spin(deadline, "the resized content", || {
                locked(&sizes).last() == Some(&want)
            })?;
        }
    }
    for size in locked(&sizes).iter() {
        emit(format_args!("RESIZED {}x{}", size.width, size.height));
    }

    // phase 3: refusals, then the release of a replaced buffer at the paint after its replacement
    let (_fd, scene) = frame_scene(args.width, args.height, RELEASE_FRAMES[0])?;
    let bad = |ints: &dyn Fn(&mut Params)| -> Result<Scene> {
        let mut bad = scene.clone();
        let b = content(&mut bad.quads[0]);
        let mut p = Params::from_handle(b.buffer.fds.len(), &b.buffer.ints)
            .map_err(|e| anyhow::anyhow!("{e:?}"))?;
        ints(&mut p);
        b.buffer = Arc::new(SceneBuffer {
            ints: p.to_ints().to_vec(),
            ..(*b.buffer).clone()
        });
        Ok(bad)
    };
    let refused = |bad: Scene| -> Result<bool> {
        on_display(&invoker, move |window| {
            Ok(matches!(
                window.present(&bad),
                Err(PresentError::Buffer { layer_id: 0, .. })
            ))
        })
    };
    let short_pitch = refused(bad(&|p| p.pitch = p.width * 4 - 4)?)?;
    let bad_fourcc = refused(bad(&|p| p.fourcc = u32::from_le_bytes(*b"ZZ99"))?)?;
    let painted: Arc<Mutex<Vec<(u64, bool)>>> = Arc::default();
    {
        let seen = Arc::clone(&painted);
        on_display(&invoker, move |window| {
            window.on_painted(move |p| {
                locked(&seen).extend(
                    p.releases
                        .into_iter()
                        .map(|r| (r.buffer_key, r.release_fence.is_some())),
                )
            });
            window.present(&scene)?;
            Ok(())
        })?;
    }
    spin(deadline, "the first release scene's paint", || {
        // the frame series' last buffer goes back once this scene is painted
        !locked(&painted).is_empty()
    })?;
    let (_fd2, replacing) = frame_scene(args.width, args.height, RELEASE_FRAMES[1])?;
    // cleared and presented in one display-thread step: no paint can report between them
    let early = {
        let seen = Arc::clone(&painted);
        on_display(&invoker, move |window| {
            locked(&seen).clear();
            window.present(&replacing)?;
            Ok(locked(&seen).len())
        })?
    };
    spin(deadline, "the release of the replaced buffer", || {
        !locked(&painted).is_empty()
    })?;
    let released = locked(&painted).clone();
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

    // phase 4: `call` (display-thread plan D2). From the app thread it returns a value computed on the display thread; on the display thread itself it runs inline (a queued call would wait on itself)
    let display = atl_platform_ui::call(&*invoker, || thread::current().id())
        .context("the display thread ended")?;
    let on_toolkit = move || {
        let size = WINDOW.with_borrow(|w| w.as_ref().map(|w| w.size()));
        (thread::current().id() == display, size)
    };
    // set once by the posted closure: whether it ran on the app thread (inside its poll), and what its call returned (None: dropped unrun)
    type AppCall = (bool, Option<(bool, Option<PhysSize>)>);
    let from_app: Arc<Mutex<Option<AppCall>>> = Arc::default();
    let app_ui = app.handle.thread().id();
    {
        let (invoker, slot) = (Arc::clone(&invoker), Arc::clone(&from_app));
        app.poster.post(Box::new(move || {
            let on_app = thread::current().id() == app_ui;
            let got = atl_platform_ui::call(&*invoker, on_toolkit);
            *locked(&slot) = Some((on_app, got));
        }));
    }
    spin(deadline, "the app thread's call", || {
        locked(&from_app).is_some()
    })?;
    let (posted_on_app, app_call) = locked(&from_app).take().unwrap_or((false, None));
    // a call made on the display thread (inside one queued from here): inline
    let nested = Arc::clone(&invoker);
    let inline = atl_platform_ui::call(&*invoker, move || {
        atl_platform_ui::call(&*nested, move || thread::current().id() == display)
    })
    .flatten();
    let app_size = app_call
        .and_then(|(_, s)| s)
        .map_or("none".to_owned(), |s| format!("{}x{}", s.width, s.height));
    emit(format_args!(
        "CALL posted-on-app={posted_on_app} app-on-toolkit={} app-size={app_size} inline={}",
        app_call.is_some_and(|(on, _)| on),
        inline == Some(true)
    ));

    // the text input's events, one line each (phase 5 `ime-focus` enables it)
    on_display(&invoker, |window| {
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
        Ok(())
    })?;

    // phase 5: commands on stdin, until quit or its end
    emit(format_args!("READY"));
    for line in std::io::stdin().lock().lines() {
        let line = line?;
        match line.split_whitespace().collect::<Vec<_>>().as_slice() {
            ["block", ms] => {
                let ms: u64 = ms.parse().unwrap_or(0);
                // the app's UI thread only: the display thread keeps running
                app.poster.post(Box::new(move || {
                    emit(format_args!("BLOCKING {ms}"));
                    thread::sleep(Duration::from_millis(ms));
                    emit(format_args!("UNBLOCKED"));
                }));
            }
            ["ime-focus", text, cursor] => {
                let cursor_byte = cursor.parse().unwrap_or(0);
                let text = (*text).to_owned();
                on_display(&invoker, move |window| {
                    let input = window.text_input();
                    input.set_content(IME_PURPOSE, IME_HINTS);
                    input.set_surrounding(&Surrounding {
                        text,
                        cursor_byte,
                        anchor_byte: cursor_byte,
                    });
                    input.set_cursor_rect(IME_CURSOR);
                    input.focus_in();
                    Ok(())
                })?;
                emit(format_args!("IME-FOCUSED"));
            }
            ["quit"] => break,
            other => emit(format_args!("UNKNOWN-COMMAND {other:?}")),
        }
    }
    app.stop()?;
    // `xtask test platform-ui` checks these: the monitor's rate (0 = none reported), then every presentation sample in order, with the monotonic time its report arrived
    emit(format_args!(
        "MONITOR refresh_hz={}",
        locked(&monitor_hz).unwrap_or(0.0)
    ));
    for (p, seen_ns) in locked(&presents).iter() {
        emit(format_args!(
            "PRESENT time_ns={} refresh_ns={} source={:?} seen_ns={seen_ns}",
            p.time_ns, p.refresh_ns, p.source
        ));
    }
    emit(format_args!("DONE"));
    Ok(())
}
