// SPDX-License-Identifier: MIT
//! The process of `xtask test ui-module` (launcher-ui-module plan slices M1, M3): loads the platform module through the `atl-platform-ui` facade, as the launcher does, then an app library, and reports on stdout what each part saw. The harness judges; this program only reports.
//! 1. `PlatformUi::attach` with this program's Looper (a `poll(2)` loop on this thread, state in an `Rc`, not a thread-local): the facade dlopens the module `RTLD_NOW | RTLD_LOCAL` by file name, checks its build identity and routes its log records into this program's subscriber (target `atl_platform_ui_linux`). A refusal is reported (`attach_error`) and the run goes on without UI, as the launcher's "no UI mode". `--global`: the module is dlopened `RTLD_GLOBAL` first (the gate's break: the facade's later local load gets the same, global, object). `--drop-module-records`: this subscriber filters the module's records out (the other break).
//! 2. One `PlatformWindow` presenting a colour scene at each tick until the first paint or `--timeout-ms`. `--audio <socket>`: `PlatformAudioHost::connect` (loads SPA plugins inside the local group).
//! 3. `dlopen(<app>, RTLD_NOW)` as nativeloader would, and the app's `app_probe` (the stand-in's `vkCreateInstance`). Then `dlsym(RTLD_DEFAULT)` of host-library symbols: any found means the module's closure leaked into global scope.
//! 4. Drops the window and the session, then returns from `main`: the normal exit runs every thread-local destructor, which must not reach a dropped object (state note ui-module-m1, trap 1).
//!
//! Lines: module records under target `atl_platform_ui_linux`; results as `RESULT <key>=<value>`.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::ffi::{CStr, CString, c_int, c_void};
use std::os::fd::RawFd;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use atl_compositor::scene::{Content, Quad};
use atl_libgui_parcel::state::FloatRect;
use atl_platform_ui::audio::AudioHost;
use atl_platform_ui::{
    FdEvents, FrameClock, Looper, PhysSize, PlatformAudioHost, PlatformUi, PlatformWindow, Scene,
    Ui, UiError, Window, WindowAttributes, abi,
};
use clap::Parser;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
struct Args {
    /// The app library: exports `int app_probe(void)`, which returns its `vkCreateInstance(0, 0, 0)`.
    #[arg(long)]
    app: PathBuf,
    /// dlopen the module RTLD_GLOBAL before the facade loads it (deliberate break: its closure joins global scope).
    #[arg(long)]
    global: bool,
    /// Filter the module's records out of this program's subscriber (deliberate break: they reach the process only through it).
    #[arg(long)]
    drop_module_records: bool,
    /// Connect the module's audio host to this PipeWire socket.
    #[arg(long)]
    audio: Option<PathBuf>,
    /// How long the Looper runs waiting for the first paint.
    #[arg(long, default_value_t = 10_000)]
    timeout_ms: u64,
}

/// Host-library symbols no object of this process may find in global scope (one per library of the module's closure that an AOSP lib or an app also exports, plus the toolkit's own).
const HOST_SYMBOLS: &[&str] = &[
    "vkCreateInstance",
    "eglGetDisplay",
    "g_main_context_default",
    "g_type_init_with_debug_flags",
    "gtk_init",
    "gdk_display_get_default",
    "gsk_renderer_realize",
    "FT_Init_FreeType",
    "hb_buffer_create",
    "png_create_read_struct",
    "inflate",
    "XML_ParserCreate",
    "jpeg_CreateDecompress",
    "FcInit",
    "cairo_create",
    "wl_display_connect",
    "xkb_context_new",
    "pw_init",
    "selinux_check_access",
];

fn result(key: &str, value: impl std::fmt::Display) {
    tracing::info!("RESULT {key}={value}");
}

type Sink = Box<dyn FnMut(RawFd, FdEvents)>;

/// The Looper's registrations and the module's sink.
#[derive(Default)]
struct LoopState {
    sink: Option<Sink>,
    fds: BTreeMap<RawFd, FdEvents>,
}

/// This thread's Looper: registered fds polled by [`PollLooper::poll_once`], readiness handed to the sink.
#[derive(Clone, Default)]
struct PollLooper(Rc<RefCell<LoopState>>);

impl Looper for PollLooper {
    fn set_sink(&self, sink: Sink) {
        self.0.borrow_mut().sink = Some(sink);
    }

    fn add_fd(&self, fd: RawFd, events: FdEvents) -> Result<(), UiError> {
        self.0.borrow_mut().fds.insert(fd, events);
        Ok(())
    }

    fn remove_fd(&self, fd: RawFd) -> Result<(), UiError> {
        self.0.borrow_mut().fds.remove(&fd);
        Ok(())
    }
}

impl PollLooper {
    /// One `poll(2)` pass of at most `timeout`, then the sink for each ready fd.
    fn poll_once(&self, timeout: Duration) -> Result<()> {
        let mut pfds: Vec<libc::pollfd> = self
            .0
            .borrow()
            .fds
            .iter()
            .map(|(&fd, ev)| libc::pollfd {
                fd,
                events: (if ev.input { libc::POLLIN } else { 0 })
                    | (if ev.output { libc::POLLOUT } else { 0 }),
                revents: 0,
            })
            .collect();
        let ms = c_int::try_from(timeout.as_millis()).unwrap_or(c_int::MAX);
        // SAFETY: `pfds` is a valid array of its length for the call.
        let n = unsafe { libc::poll(pfds.as_mut_ptr(), pfds.len() as libc::nfds_t, ms) };
        if n < 0 {
            let e = std::io::Error::last_os_error();
            if e.kind() == std::io::ErrorKind::Interrupted {
                return Ok(());
            }
            bail!("poll: {e}");
        }
        for p in pfds.iter().filter(|p| p.revents != 0) {
            let r = p.revents;
            let events = FdEvents {
                input: r & libc::POLLIN != 0,
                output: r & libc::POLLOUT != 0,
                error: r & libc::POLLERR != 0,
                hangup: r & libc::POLLHUP != 0,
            };
            // the sink adds and removes fds: run it with the state unborrowed, then put it back
            let sink = self.0.borrow_mut().sink.take();
            if let Some(mut sink) = sink {
                sink(p.fd, events);
                self.0.borrow_mut().sink.get_or_insert(sink);
            }
        }
        Ok(())
    }
}

fn dlerror() -> String {
    // SAFETY: dlerror returns null or a NUL-terminated string valid until the next dl* call on this thread.
    let e = unsafe { libc::dlerror() };
    if e.is_null() {
        "unknown".into()
    } else {
        // SAFETY: non-null, NUL-terminated (above).
        unsafe { CStr::from_ptr(e) }.to_string_lossy().into_owned()
    }
}

fn dlopen(path: &std::path::Path, flags: c_int) -> Result<*mut c_void> {
    let c = CString::new(path.as_os_str().as_encoded_bytes())?;
    // SAFETY: `c` is NUL-terminated; loading runs the object's constructors, which is the point.
    let h = unsafe { libc::dlopen(c.as_ptr(), flags) };
    ensure!(!h.is_null(), "dlopen {}: {}", path.display(), dlerror());
    Ok(h)
}

fn dlsym(handle: *mut c_void, name: &str) -> Option<*mut c_void> {
    let c = CString::new(name).ok()?;
    // SAFETY: `handle` is a live dlopen handle or RTLD_DEFAULT, `c` NUL-terminated.
    let p = unsafe { libc::dlsym(handle, c.as_ptr()) };
    (!p.is_null()).then_some(p)
}

/// The file of the object global lookup finds `name` in, or `none`.
fn provider(name: &str) -> String {
    let Some(p) = dlsym(libc::RTLD_DEFAULT, name) else {
        return "none".into();
    };
    let mut info = std::mem::MaybeUninit::<libc::Dl_info>::zeroed();
    // SAFETY: `p` is an address dlsym returned; `info` is a zeroed out-parameter of the right type.
    if unsafe { libc::dladdr(p, info.as_mut_ptr()) } == 0 {
        return "unknown".into();
    }
    // SAFETY: dladdr succeeded, so `info` is filled; dli_fname is null or a NUL-terminated path owned by the loader.
    let fname = unsafe { info.assume_init() }.dli_fname;
    if fname.is_null() {
        return "unknown".into();
    }
    // SAFETY: non-null, NUL-terminated (above).
    unsafe { CStr::from_ptr(fname) }
        .to_string_lossy()
        .into_owned()
}

/// One opaque colour quad covering `width` x `height`.
fn color_scene(width: u32, height: u32, rgb: [f32; 3]) -> Scene {
    let dst = FloatRect {
        left: 0.0,
        top: 0.0,
        right: width as f32,
        bottom: height as f32,
    };
    Scene {
        frame: 1,
        layer_stack: 0,
        quads: vec![Quad {
            layer_id: 0,
            name: "color".into(),
            content: Content::Color(rgb),
            dst,
            clip: Some(dst),
            alpha: 1.0,
        }],
    }
}

/// Opens the colour window and runs the Looper until its first paint or the deadline; returns the window and the paint count.
fn paint_window(
    ui: &PlatformUi,
    looper: &PollLooper,
    timeout: Duration,
) -> Result<(Rc<PlatformWindow>, u64)> {
    let (width, height) = (320, 240);
    let window = Rc::new(
        PlatformWindow::create(
            ui,
            &WindowAttributes {
                title: "atl ui-module fixture".into(),
                app_id: None,
                size: PhysSize { width, height },
            },
        )
        .map_err(|e| anyhow::anyhow!("window: {e}"))?,
    );
    let painted = Rc::new(Cell::new(0u64));
    // the colour the harness may look for in a capture; any opaque colour shows a paint
    let scene = color_scene(width, height, [0.2, 0.6, 0.9]);
    let weak = Rc::downgrade(&window);
    let count = Rc::clone(&painted);
    window.on_tick(move |_| {
        let Some(w) = weak.upgrade() else { return };
        if let Err(e) = w.present(&scene) {
            tracing::error!("present: {e}");
        }
        if count.get() == 0 {
            w.request_frame();
        }
    });
    let count = Rc::clone(&painted);
    window.on_painted(move |_| count.set(count.get() + 1));
    window.request_frame();
    let deadline = Instant::now() + timeout;
    while painted.get() == 0 && Instant::now() < deadline {
        looper.poll_once(
            deadline
                .saturating_duration_since(Instant::now())
                .min(Duration::from_millis(100)),
        )?;
    }
    let n = painted.get();
    Ok((window, n))
}

fn main() -> Result<()> {
    let args = Args::parse();
    let filter = if args.drop_module_records {
        "info,atl_platform_ui_linux=off"
    } else {
        "info"
    };
    tracing_subscriber::fmt()
        .with_writer(std::io::stdout)
        .with_ansi(false)
        .without_time()
        .with_env_filter(EnvFilter::new(filter))
        .init();
    // the launcher's crash handler queries and sets actions through sigchain (libsigchain leads DT_NEEDED); the same call here is the reference that keeps libsigchain linked, and where it resolves is reported
    let mut old = std::mem::MaybeUninit::<libc::sigaction>::zeroed();
    // SAFETY: a query (null new action) into a zeroed out-parameter of the right type.
    let rc = unsafe { libc::sigaction(libc::SIGSEGV, std::ptr::null(), old.as_mut_ptr()) };
    ensure!(
        rc == 0,
        "sigaction query: {}",
        std::io::Error::last_os_error()
    );
    result("sigaction_from", provider("sigaction"));
    result("abi_id", abi::ABI_ID);
    // what global lookup finds before the module: the process's own closure (libsigchain's pulls AOSP libz.so) is not the module's leak
    let before: Vec<String> = HOST_SYMBOLS.iter().map(|s| provider(s)).collect();
    result("module_flags", if args.global { "global" } else { "local" });
    if args.global {
        dlopen(
            std::path::Path::new(abi::MODULE_FILE),
            libc::RTLD_NOW | libc::RTLD_GLOBAL,
        )?;
    }
    let looper = PollLooper::default();
    let mut session = None;
    match PlatformUi::attach(Box::new(looper.clone()), None) {
        Ok(ui) => {
            let (window, painted) =
                paint_window(&ui, &looper, Duration::from_millis(args.timeout_ms))?;
            result("painted", painted);
            session = Some((ui, window));
        }
        Err(e) => result("attach_error", e),
    }
    if let Some(socket) = &args.audio {
        match PlatformAudioHost::connect(socket) {
            Ok(_) => result("audio_connect", 1),
            Err(e) => {
                result("audio_connect", 0);
                result("audio_error", e);
            }
        }
    }
    let app = dlopen(&args.app, libc::RTLD_NOW)?;
    let probe = dlsym(app, "app_probe").context("app_probe")?;
    // SAFETY: the app's contract: `int app_probe(void)`.
    let probe: extern "C" fn() -> c_int =
        unsafe { std::mem::transmute::<*mut c_void, extern "C" fn() -> c_int>(probe) };
    result("app_vkCreateInstance", probe());
    // a leak: global lookup now finds the symbol in a different object than before the module was loaded
    let leaks: Vec<String> = HOST_SYMBOLS
        .iter()
        .zip(&before)
        .filter_map(|(s, b)| {
            let now = provider(s);
            (now != *b).then(|| format!("{s}@{now}"))
        })
        .collect();
    result("global_leaks", leaks.len());
    result("global_leak_names", leaks.join(","));
    let preexisting: Vec<String> = HOST_SYMBOLS
        .iter()
        .zip(&before)
        .filter(|(_, b)| b.as_str() != "none")
        .map(|(s, b)| format!("{s}@{b}"))
        .collect();
    result("global_before_module", preexisting.join(","));
    // a few more passes after the app load: the module keeps running in the same loop
    for _ in 0..5 {
        looper.poll_once(Duration::from_millis(20))?;
    }
    // window first, then the session: the order the trait objects' owners use
    if let Some((ui, window)) = session.take() {
        drop(window);
        drop(ui);
    }
    result("done", 1);
    // a normal exit: the thread-local destructors run (M1 used _exit to skip a module thread-local that called a destroyed Looper; M3 keeps no state there)
    Ok(())
}
