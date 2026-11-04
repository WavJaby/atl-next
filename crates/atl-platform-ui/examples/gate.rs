// SPDX-License-Identifier: MIT
//! The client of `xtask test platform-ui` (M3 slice 6): a window through the `atl-platform-ui` facade, driven only by its frame clock and the `iterate` loop, as the launcher's UI thread drives it.
//! The harness reads this program's stdout (one `KEY value...` line per event) and the raw RGBA captures it writes to `--out`; it checks them against its own implementation of the pattern below, not against this program.
//!
//! Phases, in order:
//! 1. `--frames` frames: each tick presents one full-window quad whose buffer holds [`pattern`] for frame `f` (rows padded with `STRIDE_PAD_PIXELS` extra pixels of 0xEE, so a wrong stride shows), then captures the window to `frame-<f>.rgba`.
//! 2. one composite scene (crop, flips, rotation, scissor, opacity: [`composite_scene`]) captured to `composite.rgba`.
//! 3. refusals: an unsignalled acquire fence and a bad-stride buffer are refused, the signalled fence is accepted.
//! 4. an eventfd added with `Ui::watch_fd` is written from another thread; its callback must run on this thread inside `iterate`.
//! 5. `READY`, then commands on stdin, served by a `watch_fd` callback on the stdin fd: `block <ms>` sleeps the UI thread that long (the harness's ping probe), `quit` ends.

use std::cell::{Cell, RefCell};
use std::io::Write;
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use atl_compositor::parcel::FdObject;
use atl_compositor::scene::{Quad, Scene};
use atl_compositor::state::{Fence, FloatRect, GraphicBuffer};
use atl_gralloc::format::RGBA_8888;
use atl_platform_ui::{
    FrameClock, PhysSize, PlatformUi, PlatformWindow, PresentError, Ui, Window, WindowAttributes,
};
use clap::Parser;
use rustix::event::{EventfdFlags, eventfd};
use rustix::fs::{MemfdFlags, ftruncate, memfd_create};
use rustix::io::{read, write};
use rustix::net::{AddressFamily, SocketFlags, SocketType, socketpair};

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
/// One `iterate` pass waits at most this long when nothing is ready.
const PASS: Duration = Duration::from_millis(20);
/// Pattern frame numbers of the composite scene's five buffers (distinct from the frame series).
const COMPOSITE_FRAMES: [u32; 5] = [100, 101, 102, 103, 104];

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
fn pattern_buffer(width: u32, height: u32, frame: u32) -> Result<(OwnedFd, Arc<GraphicBuffer>)> {
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
    let buffer = GraphicBuffer {
        width: width as i32,
        height: height as i32,
        stride: stride as i32,
        format: RGBA_8888,
        layer_count: 1,
        usage: 0,
        id: u64::from(frame),
        generation_number: 0,
        fds: vec![FdObject::new(fd.as_raw_fd(), false)],
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

fn quad(layer_id: i32, buffer: Arc<GraphicBuffer>, src: FloatRect, dst: FloatRect) -> Quad {
    Quad {
        layer_id,
        name: format!("layer-{layer_id}"),
        buffer,
        frame_number: 1,
        acquire_fence: None,
        src,
        dst,
        clip: None,
        alpha: 1.0,
        opaque: false,
        buffer_transform: 0,
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
    q.opaque = frame % 2 == 1;
    Ok((fd, scene_of(vec![q])))
}

/// Five layers, bottom first. Coordinates are physical pixels of a window of at least 256 x 192.
/// 0. frame 100 over the whole window;
/// 1. frame 101, 48 x 32 buffer, crop (8,4)-(40,28), FLIP_H, at (20,30)-(52,54);
/// 2. frame 102, 40 x 24 buffer, ROT_90 (clockwise), at (100,10)-(124,50);
/// 3. frame 103, 48 x 32 buffer at (150,60)-(198,92) scissored to (160,70)-(190,85);
/// 4. frame 104, 40 x 40 buffer at (30,100)-(70,140), alpha 0.5.
fn composite_scene(width: u32, height: u32) -> Result<(Vec<OwnedFd>, Scene)> {
    let mut fds = Vec::new();
    let mut buffer = |w, h, f| -> Result<Arc<GraphicBuffer>> {
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
    flipped.buffer_transform = 1;
    layers.push(flipped);
    let mut rotated = quad(
        2,
        buffer(40, 24, f2)?,
        rect(0.0, 0.0, 40.0, 24.0),
        rect(100.0, 10.0, 124.0, 50.0),
    );
    rotated.buffer_transform = 4;
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

/// Runs `iterate` until `done` is true or `deadline` passes.
fn spin(ui: &PlatformUi, deadline: Instant, what: &str, done: impl Fn() -> bool) -> Result<()> {
    while !done() {
        ensure!(Instant::now() < deadline, "timed out waiting for {what}");
        ui.iterate(Some(PASS))?;
    }
    Ok(())
}

fn main() -> Result<()> {
    let args = Args::parse();
    std::fs::create_dir_all(&args.out)?;
    let deadline = Instant::now() + Duration::from_secs(args.timeout_secs);
    let ui = PlatformUi::attach()?;
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
    spin(&ui, deadline, "the frame series", || {
        frame.get() >= args.frames || failure.borrow().is_some()
    })?;
    if let Some(e) = failure.borrow().as_ref() {
        bail!("{e}");
    }

    // phase 2: the composite scene
    let (_fds, scene) = composite_scene(args.width, args.height)?;
    window.present(&scene)?;
    emit(format_args!(
        "COMPOSITE {}",
        save(&args.out, "composite.rgba", &window)?
    ));

    // phase 3: refusals
    let (_fd, mut scene) = frame_scene(args.width, args.height, 0)?;
    let (fence_read, fence_write) = socketpair(
        AddressFamily::UNIX,
        SocketType::STREAM,
        SocketFlags::CLOEXEC,
        None,
    )?;
    scene.quads[0].acquire_fence = Some(Fence(Some(FdObject::new(fence_read.as_raw_fd(), false))));
    let unsignalled = matches!(
        window.present(&scene),
        Err(PresentError::FenceNotSignalled { layer_id: 0 })
    );
    write(&fence_write, b"x")?;
    let signalled = window.present(&scene).is_ok();
    let mut bad = scene.clone();
    bad.quads[0].acquire_fence = None;
    bad.quads[0].buffer = Arc::new(GraphicBuffer {
        stride: 1,
        ..(*bad.quads[0].buffer).clone()
    });
    let bad_refused = matches!(
        window.present(&bad),
        Err(PresentError::Buffer { layer_id: 0, .. })
    );
    emit(format_args!(
        "REFUSALS unsignalled-fence={unsignalled} signalled-fence={signalled} bad-stride={bad_refused}"
    ));

    // phase 4: a watched fd is serviced on this thread, inside `iterate`
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
        ui.watch_fd(wake.as_fd(), move || {
            // drain the counter: the watch is level-triggered
            let mut counter = [0u8; 8];
            let _ = read(&drain, &mut counter);
            on_ui_thread.set(thread::current().id() == ui_thread);
            fired.set(fired.get() + 1);
        })?
    };
    let writer = {
        let wake = wake.try_clone()?;
        thread::spawn(move || {
            thread::sleep(PASS);
            write(&wake, &1u64.to_ne_bytes())
        })
    };
    spin(&ui, deadline, "the watched fd", || fired.get() > 0)?;
    writer
        .join()
        .map_err(|_| anyhow::anyhow!("writer thread panicked"))??;
    emit(format_args!(
        "FDWATCH fired={} on-ui-thread={}",
        fired.get(),
        on_ui_thread.get()
    ));
    drop(watch);

    // phase 5: commands on stdin, until quit
    let quit = Rc::new(Cell::new(false));
    let pending = Rc::new(RefCell::new(Vec::<u8>::new()));
    let _stdin_watch = {
        let (quit, pending) = (Rc::clone(&quit), Rc::clone(&pending));
        ui.watch_fd(rustix::stdio::stdin(), move || {
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
                    ["quit"] => quit.set(true),
                    other => emit(format_args!("UNKNOWN-COMMAND {other:?}")),
                }
            }
        })?
    };
    emit(format_args!("READY"));
    while !quit.get() {
        ui.iterate(Some(PASS))?;
    }
    emit(format_args!("DONE"));
    Ok(())
}
