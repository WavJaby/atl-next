// SPDX-License-Identifier: MIT
//! The client of `xtask test platform-ui` (M3 slice 6): a window through the `atl-platform-ui` facade, driven only by its frame clock and its Looper's poll, as the launcher's UI thread drives it. [`TestLooper`] is this program's Looper: the platform's `Looper` seam, polled by [`TestLooper::poll`].
//! The harness reads this program's stdout (one `KEY value...` line per event) and the raw RGBA captures it writes to `--out`; it checks them against its own implementation of the pattern below, not against this program.
//!
//! Phases, in order:
//! 1. `--frames` frames: each tick presents one full-window quad whose buffer holds [`pattern`] for frame `f` (rows padded with `STRIDE_PAD_PIXELS` extra pixels of 0xEE, so a wrong stride shows), then captures the window to `frame-<f>.rgba`.
//! 2. one composite scene (crop, flips, rotation, scissor, opacity, colour layers: [`composite_scene`]) captured to `composite.rgba`. Then the resize probe: `set_size` larger then smaller, each size the content gets as `RESIZED`; on a
//!    size-fixed window (maximized / tiled: `Window::is_size_fixed`) `SIZE-FIXED <granted>`, and after each request `FIXED-AFTER <size>` instead of a wait no configure would end.
//! 3. refusals: an unsignalled acquire fence and a bad-stride buffer are refused, the signalled fence is accepted.
//! 4. an eventfd added with [`TestLooper::watch`] is written from another thread; its callback must run on this thread inside `poll`.
//! 5. `READY`, then commands on stdin, served by a `watch` callback on the stdin fd: `block <ms>` sleeps the UI thread that long (the harness's ping probe); `ime-focus <text> <cursor-byte>` sets the window's `TextInput` content type, surrounding text and cursor rect, then focuses it (`IME-FOCUSED`), and every `TextInputEvent` is printed as an `IME-*` line; `quit` ends.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::io::Write;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd, RawFd};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use atl_compositor::scene::{BufferContent, Content, Quad, Scene, SceneBuffer, SceneFd};
use atl_compositor::state::FloatRect;
use atl_gralloc::format::RGBA_8888;
use atl_platform_ui::{
    ContentHints, ContentPurpose, FdEvents, FrameClock, Looper, PhysRect, PhysSize, PlatformUi,
    PlatformWindow, PresentError, Surrounding, TextInput, TextInputEvent, Ui, UiError, Window,
    WindowAttributes,
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
use rustix::net::{AddressFamily, SocketFlags, SocketType, socketpair};
use rustix::time::Timespec;

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
/// Phase 1b: ticks asked for one at a time with nothing presented (the idle timer paces each after the first), and how long each may take: a 60 Hz grid tick is due within 17 ms, so 1 s only fails a tick that never comes.
const IDLE_TICKS: u32 = 3;
const IDLE_WAIT: Duration = Duration::from_secs(1);
/// Phase 2b on a size-fixed window: ticks after each size request before the size is read. GTK applies set_default_size at the next layout (one tick); a second covers the commit the compositor would answer with a configure.
const FIXED_TICKS: u32 = 2;
/// Pattern frame numbers of the composite scene's five buffers (distinct from the frame series).
const COMPOSITE_FRAMES: [u32; 5] = [100, 101, 102, 103, 104];
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

/// A memfd holding `width` x `height` pixels of `frame`'s pattern in rows of `width + STRIDE_PAD_PIXELS`, as a gralloc-shaped buffer. The fd must outlive the present call.
fn pattern_buffer(width: u32, height: u32, frame: u32) -> Result<(OwnedFd, Arc<SceneBuffer>)> {
    let stride = width + STRIDE_PAD_PIXELS;
    let mut bytes = Vec::with_capacity(stride as usize * height as usize * 4);
    for y in 0..height {
        for x in 0..stride {
            if x < width {
                bytes.extend_from_slice(&pattern(x, y, frame));
            } else {
                bytes.extend_from_slice(&[PAD_BYTE; 4]);
            }
        }
    }
    let fd = memfd_create("gate-buffer", MemfdFlags::CLOEXEC)?;
    ftruncate(&fd, bytes.len() as u64)?;
    let mut rest = &bytes[..];
    while !rest.is_empty() {
        let n = write(&fd, rest)?;
        rest = &rest[n..];
    }
    let buffer = SceneBuffer {
        width: width as i32,
        height: height as i32,
        stride: stride as i32,
        format: RGBA_8888,
        usage: 0,
        id: u64::from(frame),
        fds: vec![SceneFd::new(fd.as_raw_fd())],
        ints: vec![],
    };
    Ok((fd, Arc::new(buffer)))
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
fn frame_scene(width: u32, height: u32, frame: u32) -> Result<(OwnedFd, Scene)> {
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
        fds.push(fd);
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
                emit(format_args!("FRAME {f} time_ns={} {saved}", tick.time_ns));
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

    // phase 1b: ticks requested from outside a mirror pass, as the app's vsync request comes from Java on the Looper. After a tick that presented nothing the idle timer paces the next, and its timeout reaches the Looper only through the mirror's resync: without it no tick comes.
    let idle = Rc::new(Cell::new(0u32));
    {
        let idle = Rc::clone(&idle);
        window.on_tick(move |_| idle.set(idle.get() + 1));
    }
    for want in 1..=IDLE_TICKS {
        window.request_frame();
        let wait = Instant::now() + IDLE_WAIT;
        while idle.get() < want && Instant::now() < wait {
            looper.poll(PASS)?;
        }
    }
    emit(format_args!("IDLE_TICKS {}", idle.get()));

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
            let start = idle.get();
            for n in 1..=FIXED_TICKS {
                window.request_frame();
                spin(&looper, deadline, "a tick after the size request", || {
                    idle.get() >= start + n
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

    // phase 3: refusals
    let (_fd, mut scene) = frame_scene(args.width, args.height, 0)?;
    let (fence_read, fence_write) = socketpair(
        AddressFamily::UNIX,
        SocketType::STREAM,
        SocketFlags::CLOEXEC,
        None,
    )?;
    content(&mut scene.quads[0]).acquire_fence = Some(SceneFd::new(fence_read.as_raw_fd()));
    let unsignalled = matches!(
        window.present(&scene),
        Err(PresentError::FenceNotSignalled { layer_id: 0 })
    );
    write(&fence_write, b"x")?;
    let accepted = window.present(&scene);
    let signalled = accepted.is_ok();
    // step 1 copies every buffer at present: no release point outlives it
    let releases = accepted.map_or(usize::MAX, |p| p.releases.len());
    let mut bad = scene.clone();
    let b = content(&mut bad.quads[0]);
    b.acquire_fence = None;
    b.buffer = Arc::new(SceneBuffer {
        stride: 1,
        ..(*b.buffer).clone()
    });
    let bad_refused = matches!(
        window.present(&bad),
        Err(PresentError::Buffer { layer_id: 0, .. })
    );
    emit(format_args!(
        "REFUSALS unsignalled-fence={unsignalled} signalled-fence={signalled} bad-stride={bad_refused} releases={releases}"
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
    emit(format_args!("DONE"));
    Ok(())
}
