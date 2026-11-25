// SPDX-License-Identifier: MIT
//! The process of `xtask test ui-module` (launcher-ui-module plan slices M1, M3): loads the platform module through the `atl-platform-ui` facade, as the launcher does, then an app library, and reports on stdout what each part saw. The harness judges; this program only reports.
//! 1. `PlatformUi::attach` (the module starts its display thread, which runs GTK's loop): the facade dlopens the module `RTLD_NOW | RTLD_LOCAL` by file name, checks its build identity and routes its log records into this program's subscriber (target `atl_platform_ui_linux`). A refusal is reported (`attach_error`) and the run goes on without UI, as the launcher's "no UI mode". `--global`: the module is dlopened `RTLD_GLOBAL` first (the gate's break: the facade's later local load gets the same, global, object). `--drop-module-records`: this subscriber filters the module's records out (the other break).
//! 2. One `PlatformWindow`, made on the display thread by `call`, presenting a colour scene at each tick until the first paint or `--timeout-ms`. `--panic-on-display`: then a panicking closure invoked there, which must abort the process. `--audio <socket>`: `PlatformAudioHost::connect` (loads SPA plugins inside the local group).
//! 3. `dlopen(<app>, RTLD_NOW)` as nativeloader would, and the app's `app_probe` (the stand-in's `vkCreateInstance`). Then `dlsym(RTLD_DEFAULT)` of host-library symbols: any found means the module's closure leaked into global scope.
//! 4. Drops the window and the session, then returns from `main`: the normal exit runs every thread-local destructor, which must not reach a dropped object (state note ui-module-m1, trap 1).
//!
//! Lines: module records under target `atl_platform_ui_linux`; results as `RESULT <key>=<value>`.

use std::cell::RefCell;
use std::ffi::{CStr, CString, c_int, c_void};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, mpsc};
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use atl_compositor::scene::{Content, Quad};
use atl_libgui_parcel::state::FloatRect;
use atl_platform_ui::audio::AudioHost;
use atl_platform_ui::{
    DisplayInvoker, FrameClock, LooperPoster, PhysSize, PlatformAudioHost, PlatformUi,
    PlatformWindow, Scale, Scene, Ui, UiError, Window, WindowAttributes, abi,
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
    /// How long to wait for the first paint.
    #[arg(long, default_value_t = 10_000)]
    timeout_ms: u64,
    /// After the paint, run a panicking closure on the display thread: the process must end by SIGABRT, its panic report naming the closure's message.
    #[arg(long)]
    panic_on_display: bool,
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

/// The liveness poster: runs each post at once. This program has no Looper thread, so a ping is answered as soon as it arrives; what it gates is module loading, not liveness (`xtask test platform-ui` gate 4 is that).
struct InlinePoster;

impl LooperPoster for InlinePoster {
    fn post(&self, f: Box<dyn FnOnce() + Send>) {
        f();
    }
}

thread_local! {
    /// The colour window, on the module's display thread (a window is not Send: it is made, kept and dropped there).
    static WINDOW: RefCell<Option<Rc<PlatformWindow>>> = const { RefCell::new(None) };
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

/// Opens the colour window on the display thread ([`WINDOW`], by `call` from this thread) and waits until its first paint or `timeout`; returns the paint count.
fn paint_window(ui: &Arc<PlatformUi>, timeout: Duration) -> Result<u64> {
    let (width, height) = (320, 240);
    let painted = Arc::new(AtomicU64::new(0));
    let (first, first_paint) = mpsc::channel();
    let session = Arc::clone(ui);
    let count = Arc::clone(&painted);
    atl_platform_ui::call(&ui.invoker(), move || -> Result<(), UiError> {
        let window = Rc::new(PlatformWindow::create(
            &session,
            &WindowAttributes {
                title: "atl ui-module fixture".into(),
                app_id: None,
                size: PhysSize { width, height },
                scale: Scale::ONE,
            },
        )?);
        // the colour the harness may look for in a capture; any opaque colour shows a paint
        let scene = color_scene(width, height, [0.2, 0.6, 0.9]);
        let weak = Rc::downgrade(&window);
        let ticks = Arc::clone(&count);
        window.on_tick(move |_| {
            let Some(w) = weak.upgrade() else { return };
            if let Err(e) = w.present(&scene) {
                tracing::error!("present: {e}");
            }
            if ticks.load(Ordering::Acquire) == 0 {
                w.request_frame();
            }
        });
        window.on_painted(move |_| {
            if count.fetch_add(1, Ordering::AcqRel) == 0 {
                let _ = first.send(());
            }
        });
        window.request_frame();
        WINDOW.set(Some(window));
        Ok(())
    })
    .context("the display thread ended")?
    .map_err(|e| anyhow::anyhow!("window: {e}"))?;
    // a timeout is the gate's `painted >= 1` failure, not this program's
    let _ = first_paint.recv_timeout(timeout);
    Ok(painted.load(Ordering::Acquire))
}

/// `--panic-on-display`: a closure that panics on the display thread, then a wait the process must not survive (the panic aborts it: no display thread ends quietly while calls wait on it).
fn panic_on_display(ui: &PlatformUi) {
    ui.invoker().invoke(Box::new(|| {
        panic!("{PANIC_MESSAGE}");
    }));
    std::thread::sleep(PANIC_WAIT);
    result("panic_survived", 1);
}

/// The panic's message, which the harness finds in the panic report (xtask-harness ui_module.rs `DISPLAY_PANIC`).
const PANIC_MESSAGE: &str = "module fixture: panic on the display thread";
/// How long `--panic-on-display` waits for the abort: the display thread runs the closure at its next pass, a paint at most (llvmpipe paints under load reach 500 ms, display-thread S4 note); 10 such passes.
const PANIC_WAIT: Duration = Duration::from_secs(5);

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
    let mut session = None;
    match PlatformUi::attach(Box::new(InlinePoster), None) {
        Ok(ui) => {
            let ui = Arc::new(ui);
            let painted = paint_window(&ui, Duration::from_millis(args.timeout_ms))?;
            result("painted", painted);
            if args.panic_on_display {
                panic_on_display(&ui);
            }
            session = Some(ui);
        }
        Err(e) => result("attach_error", e),
    }
    if let Some(socket) = &args.audio {
        match PlatformAudioHost::connect(socket, None) {
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
    // window first (on the display thread, where it lives), then the session: the order the trait objects' owners use
    if let Some(ui) = session.take() {
        atl_platform_ui::call(&ui.invoker(), || drop(WINDOW.take()));
        drop(ui);
    }
    result("done", 1);
    // a normal exit: the thread-local destructors run (M1 used _exit to skip a module thread-local that called a destroyed Looper; M3 keeps no state there)
    Ok(())
}
