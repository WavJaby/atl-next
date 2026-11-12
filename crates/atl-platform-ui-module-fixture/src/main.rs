// SPDX-License-Identifier: MIT
//! The process of `xtask test ui-module` (launcher-ui-module plan slice M1): loads the platform module as the launcher will, then an app library, and reports on stdout what each part saw. The harness judges; this program only reports.
//! 1. `dlopen(<module>, RTLD_NOW | RTLD_LOCAL)` (`--global`: RTLD_GLOBAL, the gate's break), the entry with this program's [`Host`]: its Looper (a `poll(2)` loop on this thread) and its log sink (`--no-log-sink`: none, the other break).
//! 2. One colour window; the Looper runs until the module reports a paint or `--timeout-ms` passes. `--audio <socket>`: the module's PipeWire connect (loads SPA plugins from inside the local group).
//! 3. `dlopen(<app>, RTLD_NOW)` as nativeloader would, and the app's `app_probe` (the stand-in's `vkCreateInstance`). Then `dlsym(RTLD_DEFAULT)` of host-library symbols: any found means the module's closure leaked into global scope.
//!
//! Lines: module records as `MODULE <target>: <message>`; results as `RESULT <key>=<value>`.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use atl_platform_ui_api::abi::{self, Entry, FdSink, Host};
use clap::Parser;

#[derive(Parser)]
struct Args {
    /// The platform module; a bare file name is found by the dynamic loader's search path.
    #[arg(long, default_value = abi::MODULE_FILE)]
    module: PathBuf,
    /// The app library: exports `int app_probe(void)`, which returns its `vkCreateInstance(0, 0, 0)`.
    #[arg(long)]
    app: PathBuf,
    /// Load the module RTLD_GLOBAL (deliberate break: its closure joins global scope).
    #[arg(long)]
    global: bool,
    /// Give the module no log sink (deliberate break: its records go nowhere).
    #[arg(long)]
    no_log_sink: bool,
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

/// The Looper: registered fds polled on this thread, readiness handed to the module's sink.
#[derive(Default)]
struct LooperState {
    sink: Option<FdSink>,
    fds: BTreeMap<c_int, u32>,
}

thread_local! {
    static LOOPER: RefCell<LooperState> = RefCell::new(LooperState::default());
}

unsafe extern "C" fn host_log(
    _data: *mut c_void,
    level: abi::Level,
    target: *const c_char,
    target_len: usize,
    message: *const c_char,
    message_len: usize,
) {
    // SAFETY: the ABI contract: both are UTF-8 byte ranges valid for this call.
    let (target, message) = unsafe {
        (
            std::slice::from_raw_parts(target.cast::<u8>(), target_len),
            std::slice::from_raw_parts(message.cast::<u8>(), message_len),
        )
    };
    let (target, message) = (
        String::from_utf8_lossy(target),
        String::from_utf8_lossy(message),
    );
    tracing::info!("MODULE level={level} {target}: {message}");
}

unsafe extern "C" fn host_set_sink(_data: *mut c_void, sink: FdSink) {
    let old = LOOPER.with_borrow_mut(|l| l.sink.replace(sink));
    if let Some(old) = old {
        // SAFETY: the sink's contract: dropped once, after its last call.
        unsafe { (old.drop)(old.data) };
    }
}

unsafe extern "C" fn host_add_fd(_data: *mut c_void, fd: c_int, events: u32) -> c_int {
    LOOPER.with_borrow_mut(|l| l.fds.insert(fd, events));
    0
}

unsafe extern "C" fn host_remove_fd(_data: *mut c_void, fd: c_int) -> c_int {
    LOOPER.with_borrow_mut(|l| l.fds.remove(&fd));
    0
}

/// One `poll(2)` pass of at most `timeout`, then the sink for each ready fd.
fn poll_once(timeout: Duration) -> Result<()> {
    let mut pfds: Vec<libc::pollfd> = LOOPER.with_borrow(|l| {
        l.fds
            .iter()
            .map(|(&fd, &ev)| libc::pollfd {
                fd,
                events: (if ev & abi::FD_INPUT != 0 {
                    libc::POLLIN
                } else {
                    0
                }) | (if ev & abi::FD_OUTPUT != 0 {
                    libc::POLLOUT
                } else {
                    0
                }),
                revents: 0,
            })
            .collect()
    });
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
        let bits = (if r & libc::POLLIN != 0 {
            abi::FD_INPUT
        } else {
            0
        }) | (if r & libc::POLLOUT != 0 {
            abi::FD_OUTPUT
        } else {
            0
        }) | (if r & libc::POLLERR != 0 {
            abi::FD_ERROR
        } else {
            0
        }) | (if r & libc::POLLHUP != 0 {
            abi::FD_HANGUP
        } else {
            0
        });
        // the sink may add or remove fds: call it without the borrow held
        let sink = LOOPER.with_borrow(|l| l.sink.as_ref().map(|s| (s.call, s.data)));
        if let Some((call, data)) = sink {
            // SAFETY: the sink's contract: called on this (the UI) thread while it is set.
            unsafe { call(data, p.fd, bits) };
        }
    }
    Ok(())
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

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stdout)
        .with_ansi(false)
        .without_time()
        .with_target(false)
        .init();
    let args = Args::parse();
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
    // what global lookup finds before the module: the process's own closure (libsigchain's pulls AOSP libz.so) is not the module's leak
    let before: Vec<String> = HOST_SYMBOLS.iter().map(|s| provider(s)).collect();
    let flags = libc::RTLD_NOW
        | if args.global {
            libc::RTLD_GLOBAL
        } else {
            libc::RTLD_LOCAL
        };
    result("module_flags", if args.global { "global" } else { "local" });
    let module = dlopen(&args.module, flags)?;
    let entry = dlsym(module, abi::ENTRY_SYMBOL)
        .with_context(|| format!("{}: {}", abi::ENTRY_SYMBOL, dlerror()))?;
    // SAFETY: the symbol is the module's `abi::Entry` (one definition compiled into both sides).
    let entry: Entry = unsafe { std::mem::transmute::<*mut c_void, Entry>(entry) };
    let host: &'static Host = Box::leak(Box::new(Host {
        data: std::ptr::null_mut(),
        log: if args.no_log_sink {
            None
        } else {
            Some(host_log)
        },
        set_sink: host_set_sink,
        add_fd: host_add_fd,
        remove_fd: host_remove_fd,
    }));
    // SAFETY: `host` is leaked, so it outlives the process; this thread is the UI thread from here on.
    let m = unsafe { entry(host) };
    // SAFETY: null or the module's static `Module`.
    let m =
        unsafe { m.as_ref() }.context("module entry failed (its reason is in the MODULE lines)")?;
    let title = CString::new("atl ui-module fixture")?;
    // the colour the harness may look for in a capture; any opaque colour shows a paint
    let rgb = [0.2f32, 0.6, 0.9];
    // SAFETY: the ABI contract: NUL-terminated title, three floats, UI thread.
    let rc = unsafe { (m.open_color_window)(title.as_ptr(), 320, 240, &rgb) };
    ensure!(rc == 0, "open_color_window: {rc}");
    let deadline = Instant::now() + Duration::from_millis(args.timeout_ms);
    let painted = || {
        // SAFETY: UI thread.
        unsafe { (m.painted)() }
    };
    while painted() == 0 && Instant::now() < deadline {
        poll_once(
            deadline
                .saturating_duration_since(Instant::now())
                .min(Duration::from_millis(100)),
        )?;
    }
    result("painted", painted());
    if let Some(socket) = &args.audio {
        let c = CString::new(socket.as_os_str().as_encoded_bytes())?;
        // SAFETY: NUL-terminated, UI thread.
        result("audio_connect", unsafe { (m.audio_connect)(c.as_ptr()) });
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
        poll_once(Duration::from_millis(20))?;
    }
    result("done", 1);
    // the launcher's exit (ART's FastExit): no thread-local destructors. Running them would drop the module's window, whose fd removal calls this program's Looper after its thread-local is gone (measured: "cannot access a Thread Local Storage value during or after destruction", abort).
    std::io::Write::flush(&mut std::io::stdout())?;
    // SAFETY: _exit has no preconditions; stdout is flushed above.
    unsafe { libc::_exit(0) }
}
