// SPDX-License-Identifier: MIT
//! atl-egl-driver-probe: renders through the GLES driver (`libGLES_atl.so`, crate atl-egl-driver) into an ANativeWindow and checks every pixel of every queued buffer, orientation included.
//! Loads the driver as AOSP's Loader.cpp does (`initialize_api`: each egl* by dlsym, else the driver's eglGetProcAddress; each gl* the same, and the driver must export no gl* name) and calls it as libEGL's fronts would after their own window setup.
//! The window is this probe's: [`EDGE`]-square RGBA_8888 gralloc buffers (gralloc's GBM backend, LINEAR, pre-filled with [`POISON`]) behind window.h's hooks, recording the transform and fence of each queue.
//! Per frame, as hwui draws: bind framebuffer 0 and read the binding back (must read 0), invalidate GL_COLOR (an error on an FBO unless the driver maps it), then [`FILLS`] as scissored clears in GL coordinates; swap. [`FRAMES`] frames, so the swap's dequeue-and-rebind is exercised.
//! The probe sets [`APP_TRANSFORM`] on its window first, as an app's `ANativeWindow_setBuffersTransform` does. Judge: every buffer is queued with exactly that transform, and read through our own mapping as stored it equals the frame's fills top-down: buffer row r = GL row EDGE-1-r. The window starts unconnected: the driver must connect it before the first dequeue (BufferQueue refuses a dequeue with no connected API) and give the connection back after the last queue (format 0, then disconnect, egl_object.cpp's order).
//! Clear colours are c / 255 (ES 3.2 §2.3.5.2: round(f x 255) gives c back), so the bytes are exact.
//! Breaks (each must turn the verdict red): `--break flip-y` -- the surface FBO's `GL_MESA_framebuffer_flip_y` undone before drawing (a driver rendering bottom-up; meaningful on the driver's FlipY tier only); `--break fbo0` -- framebuffer entries taken from the host's EGL instead of the driver (FBO 0 unwrapped).
//! The probe implements `perform` without its variadic tail: the driver passes one integer argument, which on the Linux ABIs of every arches.toml host (SysV x86-64, AAPCS64, RISC-V LP64) travels in the register of the third named integer argument.

#![allow(non_snake_case, reason = "window.h hook and GL entry names")]

use std::collections::VecDeque;
use std::ffi::{CStr, CString, c_void};
use std::io::{self, Write};
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Mutex, OnceLock};

use anyhow::{Context as _, Result, anyhow, bail, ensure};
use atl_egl_driver::abi::egl as e;
use atl_egl_driver::abi::gles as g;
use atl_egl_driver::abi::window as w;
use atl_gralloc::descriptor::Descriptor;
use atl_gralloc::format::{DRM_FORMAT_ABGR8888, RGBA_8888};
use atl_gralloc::handle::{NUM_FDS, NUM_INTS, Params};
use atl_gralloc::node::{self, NodeSource};
use atl_gralloc::shared::PAGE_SIZE;
use atl_gralloc::usage;
use atl_gralloc_dmabuf::{Mapping, SYNC_READ, SYNC_WRITE, begin_cpu_access, end_cpu_access};
use atl_gralloc_service::backend::{Backend, DRM_DIR};
use clap::{Parser, ValueEnum};

/// Buffer edge in pixels.
const EDGE: i32 = 64;
/// Buffers in the window: one per frame plus one, so no queued buffer is recycled before it is judged.
const BUFFERS: usize = FRAMES + 1;
const FRAMES: usize = 2;
const POISON: u8 = 0xAA;
/// The transform the probe sets as an app would; a rotation, so a driver that writes any transform of its own (FLIP_V included) changes it.
const APP_TRANSFORM: u32 = w::NATIVE_WINDOW_TRANSFORM_ROT_90;
/// (x, y, w, h, rgba) in GL window coordinates (origin bottom-left), drawn in order; asymmetric in both axes, so any flip mismatches.
const FILLS: [(i32, i32, i32, i32, [u8; 4]); 4] = [
    (0, 0, EDGE, EDGE, [32, 64, 96, 255]),
    (0, 0, EDGE / 2, EDGE / 4, [255, 0, 0, 255]),
    (EDGE / 2, EDGE / 2, EDGE / 4, EDGE / 2, [0, 255, 0, 255]),
    (EDGE / 8, EDGE * 5 / 8, EDGE / 8, EDGE / 8, [0, 0, 255, 128]),
];
/// Linux errno values (asm-generic/errno-base.h).
const EINVAL: i32 = 22;
const EBUSY: i32 = 16;
const ENODEV: i32 = 19;

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Break {
    /// The surface FBO's flip-y undone before drawing: the buffer then holds GL's bottom-up rows.
    FlipY,
    /// glBindFramebuffer / glGetIntegerv / glInvalidateFramebuffer from the host's EGL, not the driver.
    Fbo0,
}

#[derive(Parser)]
struct Args {
    /// The driver library (libGLES_atl.so).
    #[arg(long)]
    driver: PathBuf,
    /// DRM node gralloc allocates on (default: gralloc's own trial); the driver is handed it as an app is by atld (trial tier).
    #[arg(long)]
    alloc: Option<PathBuf>,
    /// Break one thing the verdict depends on; the run must then fail.
    #[arg(long = "break", value_enum)]
    broken: Option<Break>,
}

fn out(line: &str) {
    let _ = writeln!(io::stdout(), "egl-driver: {line}");
}

fn main() -> ExitCode {
    let args = Args::parse();
    match run(&args) {
        Ok(()) => {
            out("PASS");
            ExitCode::SUCCESS
        }
        Err(err) => {
            out(&format!("FAIL {err:#}"));
            ExitCode::FAILURE
        }
    }
}

/// The colour of frame `frame` at GL pixel (x, y): the last fill covering it, channels rotated per frame so frames differ.
fn expected(frame: usize, x: i32, y: i32) -> [u8; 4] {
    let mut c = [POISON; 4];
    for (fx, fy, fw, fh, rgba) in FILLS {
        if x >= fx && x < fx + fw && y >= fy && y < fy + fh {
            c = fill_colour(frame, rgba);
        }
    }
    c
}

/// A fill's colour in frame `frame`: RGB rotated left by the frame number.
fn fill_colour(frame: usize, c: [u8; 4]) -> [u8; 4] {
    let mut rgb = [c[0], c[1], c[2]];
    rgb.rotate_left(frame % 3);
    [rgb[0], rgb[1], rgb[2], c[3]]
}

// ---- the window ----

struct Slot {
    buffer: Box<w::ANativeWindowBuffer>,
    /// native_handle_t storage: version, numFds, numInts, fds, ints.
    _handle: Box<[i32]>,
    dmabuf: OwnedFd,
    _page: OwnedFd,
    size: usize,
    pitch: u32,
}

struct Queued {
    slot: usize,
    transform: u32,
    fence: Option<OwnedFd>,
}

/// The window's EGL-API calls, in order (judged at the end).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Hook {
    Connect(u32),
    Format(u32),
    Disconnect(u32),
}

#[derive(Default)]
struct State {
    free: VecDeque<usize>,
    queued: Vec<Queued>,
    transform: u32,
    usage: u64,
    /// The API the window is connected to: BufferQueueProducer's mConnectedApi.
    api: Option<u32>,
    events: Vec<Hook>,
}

/// window.h's ANativeWindow first, so a hook's `ANativeWindow*` is this object's address.
#[repr(C)]
struct TestWindow {
    anw: w::ANativeWindow,
    slots: Vec<Slot>,
    state: Mutex<State>,
}

/// # Safety
/// `win` is the `anw` of a live TestWindow (the only window this probe hands out).
unsafe fn this<'a>(win: *const w::ANativeWindow) -> &'a TestWindow {
    // SAFETY: per the caller; `anw` is the first field of a #[repr(C)] struct.
    unsafe { &*win.cast::<TestWindow>() }
}

unsafe extern "C" fn no_ref(_: *mut w::android_native_base_t) {}

unsafe extern "C" fn set_swap_interval(_: *mut w::ANativeWindow, _: i32) -> i32 {
    0
}

unsafe extern "C" fn query(win: *const w::ANativeWindow, what: i32, value: *mut i32) -> i32 {
    let v = match what as u32 {
        w::NATIVE_WINDOW_WIDTH
        | w::NATIVE_WINDOW_HEIGHT
        | w::NATIVE_WINDOW_DEFAULT_WIDTH
        | w::NATIVE_WINDOW_DEFAULT_HEIGHT => EDGE,
        w::NATIVE_WINDOW_FORMAT => RGBA_8888,
        w::NATIVE_WINDOW_MIN_UNDEQUEUED_BUFFERS => 1,
        w::NATIVE_WINDOW_IS_VALID => 1,
        _ => return -EINVAL,
    };
    // SAFETY: the hook's out-pointer (window.h); `win` is ours.
    let _ = unsafe { this(win) };
    // SAFETY: as above.
    unsafe { *value = v };
    0
}

/// `perform(window, op, ...)` with its one integer argument read as the third parameter (module doc).
unsafe extern "C" fn perform(win: *mut w::ANativeWindow, op: i32, arg: usize) -> i32 {
    // SAFETY: our window.
    let t = unsafe { this(win) };
    let mut s = t.state.lock().expect("probe state");
    match op as u32 {
        w::NATIVE_WINDOW_SET_BUFFERS_TRANSFORM => {
            // an int argument: its low 32 bits
            s.transform = arg as u32;
        }
        w::NATIVE_WINDOW_SET_USAGE64 => s.usage = arg as u64,
        // BufferQueueProducer::connect: BAD_VALUE while another API is connected
        w::NATIVE_WINDOW_API_CONNECT => {
            if s.api.is_some() {
                return -EINVAL;
            }
            s.api = Some(arg as u32);
            s.events.push(Hook::Connect(arg as u32));
        }
        // BufferQueueProducer::disconnect: only the connected API
        w::NATIVE_WINDOW_API_DISCONNECT => {
            if s.api != Some(arg as u32) {
                return -EINVAL;
            }
            s.api = None;
            s.events.push(Hook::Disconnect(arg as u32));
        }
        w::NATIVE_WINDOW_SET_BUFFERS_FORMAT => s.events.push(Hook::Format(arg as u32)),
        _ => return -EINVAL,
    }
    0
}

unsafe extern "C" fn dequeue(
    win: *mut w::ANativeWindow,
    buf: *mut *mut w::ANativeWindowBuffer,
    fence: *mut i32,
) -> i32 {
    // SAFETY: our window.
    let t = unsafe { this(win) };
    let mut s = t.state.lock().expect("probe state");
    // BufferQueueProducer::dequeueBuffer: no connected producer is NO_INIT
    if s.api.is_none() {
        return -ENODEV;
    }
    let Some(i) = s.free.pop_front() else {
        return -EBUSY;
    };
    drop(s);
    // SAFETY: the hook's out-pointers; the buffer lives as long as the window.
    unsafe {
        *buf = std::ptr::from_ref(&*t.slots[i].buffer).cast_mut();
        *fence = -1;
    }
    0
}

fn slot_of(t: &TestWindow, buf: *mut w::ANativeWindowBuffer) -> Option<usize> {
    t.slots.iter().position(|s| std::ptr::eq(&*s.buffer, buf))
}

unsafe extern "C" fn queue(
    win: *mut w::ANativeWindow,
    buf: *mut w::ANativeWindowBuffer,
    fence: i32,
) -> i32 {
    // SAFETY: our window.
    let t = unsafe { this(win) };
    // SAFETY: the window owns the fence from here (window.h queueBuffer).
    let fence = (fence >= 0).then(|| unsafe { OwnedFd::from_raw_fd(fence) });
    let Some(slot) = slot_of(t, buf) else {
        return -EINVAL;
    };
    let mut s = t.state.lock().expect("probe state");
    let transform = s.transform;
    s.queued.push(Queued {
        slot,
        transform,
        fence,
    });
    0
}

unsafe extern "C" fn cancel(
    win: *mut w::ANativeWindow,
    buf: *mut w::ANativeWindowBuffer,
    fence: i32,
) -> i32 {
    // SAFETY: our window.
    let t = unsafe { this(win) };
    if fence >= 0 {
        // SAFETY: as queue.
        drop(unsafe { OwnedFd::from_raw_fd(fence) });
    }
    let Some(slot) = slot_of(t, buf) else {
        return -EINVAL;
    };
    t.state.lock().expect("probe state").free.push_back(slot);
    0
}

fn base(magic: u32, size: usize) -> w::android_native_base_t {
    w::android_native_base_t {
        magic: magic as i32,
        version: size as i32,
        reserved: [std::ptr::null_mut(); 4],
        incRef: Some(no_ref),
        decRef: Some(no_ref),
    }
}

/// The properties the driver reads in an app process (atl_gralloc::node), as atld would write them for this probe's gralloc node: the trial tier, the probe's node being gralloc's own trial or `--alloc`.
static PROPERTIES: OnceLock<Vec<(CString, CString)>> = OnceLock::new();

/// bionic's `__system_property_get`, exported from this executable (build.rs) so the driver finds it in the global scope as it does in an app process: copies the value and returns its length, 0 when unset.
///
/// # Safety
/// The caller passes a NUL-terminated name and a buffer of PROP_VALUE_MAX (92) bytes, bionic's contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __system_property_get(
    name: *const std::ffi::c_char,
    value: *mut std::ffi::c_char,
) -> i32 {
    // SAFETY: the caller's.
    let name = unsafe { CStr::from_ptr(name) };
    let found = PROPERTIES
        .get()
        .and_then(|p| p.iter().find(|(k, _)| k.as_c_str() == name));
    let bytes = found.map_or(&[0u8][..], |(_, v)| v.as_bytes_with_nul());
    // SAFETY: the caller's buffer holds PROP_VALUE_MAX bytes; a node path is shorter.
    unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr().cast(), value, bytes.len()) };
    i32::try_from(bytes.len() - 1).unwrap_or(0)
}

fn window(alloc: Option<&Path>) -> Result<Box<TestWindow>> {
    let backend =
        Backend::start(Path::new(DRM_DIR), alloc).map_err(|e| anyhow!("gralloc backend: {e}"))?;
    out(&format!("gralloc node {}", backend.node().display()));
    let props = vec![
        (
            CString::new(node::RENDER_NODE_PROPERTY)?,
            CString::new(backend.node().as_os_str().as_encoded_bytes())?,
        ),
        (
            CString::new(node::NODE_SOURCE_PROPERTY)?,
            CString::new(NodeSource::Trial.name())?,
        ),
    ];
    ensure!(
        PROPERTIES.set(props).is_ok(),
        "one window per probe run (the properties are set once)"
    );
    let checked = Descriptor {
        name: b"egl-driver".to_vec(),
        width: EDGE,
        height: EDGE,
        layer_count: 1,
        format: RGBA_8888,
        usage: usage::CPU_MASK | usage::GPU_RENDER_TARGET | usage::GPU_TEXTURE,
        reserved_size: 0,
        option_names: vec![],
        // CPU usage -> LINEAR (gralloc's policy, policy.rs `modifiers_for`)
        modifiers: vec![atl_gralloc::format::DRM_FORMAT_MOD_LINEAR],
    }
    .check()
    .map_err(|e| anyhow!("descriptor refused: {e:?}"))?;
    let mut slots = Vec::new();
    for i in 0..BUFFERS {
        let buf = backend
            .allocate(&checked)
            .map_err(|e| anyhow!("allocate: {e}"))?;
        let size = usize::try_from(buf.size)?;
        let map = Mapping::new(buf.fd.as_fd(), size)?;
        begin_cpu_access(buf.fd.as_fd(), SYNC_WRITE)?;
        map.write(0, &vec![POISON; size]);
        end_cpu_access(buf.fd.as_fd(), SYNC_WRITE)?;
        let page = rustix::fs::memfd_create("egl-driver-page", rustix::fs::MemfdFlags::CLOEXEC)?;
        rustix::fs::ftruncate(&page, PAGE_SIZE as u64)?;
        let params = Params {
            width: EDGE as u32,
            height: EDGE as u32,
            pixel_format: RGBA_8888,
            fourcc: DRM_FORMAT_ABGR8888,
            pitch: buf.pitch,
            size: buf.size,
            usage: checked.usage,
            modifier: buf.modifier,
            id: (u64::from(std::process::id()) << 32) | (i as u64 + 1),
        };
        // native_handle_t: version = sizeof(native_handle_t) (3 ints), numFds, numInts, then data
        let mut h = vec![
            12,
            NUM_FDS as i32,
            NUM_INTS as i32,
            buf.fd.as_raw_fd(),
            page.as_raw_fd(),
        ];
        h.extend(params.to_ints());
        let handle = h.into_boxed_slice();
        let buffer = Box::new(w::ANativeWindowBuffer {
            common: base(
                w::ANDROID_NATIVE_BUFFER_MAGIC,
                size_of::<w::ANativeWindowBuffer>(),
            ),
            width: EDGE,
            height: EDGE,
            stride: i32::try_from(buf.pitch / 4)?,
            format: RGBA_8888,
            usage_deprecated: 0,
            layerCount: 1,
            reserved: [std::ptr::null_mut(); 1],
            handle: handle.as_ptr().cast(),
            usage: checked.usage,
            reserved_proc: [std::ptr::null_mut(); 7],
        });
        slots.push(Slot {
            buffer,
            _handle: handle,
            dmabuf: buf.fd,
            _page: page,
            size,
            pitch: buf.pitch,
        });
    }
    // SAFETY: the declared hook type is variadic; `perform` takes the one integer argument the driver passes in the register the variadic call uses (module doc).
    let perform_hook = unsafe {
        std::mem::transmute::<
            unsafe extern "C" fn(*mut w::ANativeWindow, i32, usize) -> i32,
            unsafe extern "C" fn(*mut w::ANativeWindow, i32, ...) -> i32,
        >(perform)
    };
    let state = State {
        free: (0..BUFFERS).collect(),
        ..State::default()
    };
    Ok(Box::new(TestWindow {
        anw: w::ANativeWindow {
            common: base(
                w::ANDROID_NATIVE_WINDOW_MAGIC,
                size_of::<w::ANativeWindow>(),
            ),
            flags: 0,
            minSwapInterval: 0,
            maxSwapInterval: 1,
            xdpi: 160.0,
            ydpi: 160.0,
            oem: [0; 4],
            setSwapInterval: Some(set_swap_interval),
            dequeueBuffer_DEPRECATED: None,
            lockBuffer_DEPRECATED: None,
            queueBuffer_DEPRECATED: None,
            query: Some(query),
            perform: Some(perform_hook),
            cancelBuffer_DEPRECATED: None,
            dequeueBuffer: Some(dequeue),
            queueBuffer: Some(queue),
            cancelBuffer: Some(cancel),
        },
        slots,
        state: Mutex::new(state),
    }))
}

// ---- the driver, loaded as Loader.cpp loads it ----

type EglInitialize = unsafe extern "C" fn(e::EGLDisplay, *mut i32, *mut i32) -> u32;
type EglBindApi = unsafe extern "C" fn(u32) -> u32;
type EglCreateContext =
    unsafe extern "C" fn(e::EGLDisplay, e::EGLConfig, e::EGLContext, *const i32) -> e::EGLContext;
type GlViewport = unsafe extern "C" fn(i32, i32, i32, i32);
type GlCap = unsafe extern "C" fn(u32);
type GlClearColor = unsafe extern "C" fn(f32, f32, f32, f32);
type GlClear = unsafe extern "C" fn(u32);
type GlGetError = unsafe extern "C" fn() -> u32;
/// egl.h values the generated module does not carry (EGL_OPENGL_ES_API, EGL_CONTEXT_CLIENT_VERSION, EGL_OPENGL_ES2_BIT, EGL_RENDERABLE_TYPE) and gl2.h's GL_SCISSOR_TEST / GL_COLOR_BUFFER_BIT.
const EGL_OPENGL_ES_API: u32 = 0x30A0;
const EGL_CONTEXT_CLIENT_VERSION: i32 = 0x3098;
const EGL_RENDERABLE_TYPE: i32 = 0x3040;
const EGL_OPENGL_ES2_BIT: i32 = 0x0004;
const GL_SCISSOR_TEST: u32 = 0x0C11;
const GL_COLOR_BUFFER_BIT: u32 = 0x4000;

struct Loaded {
    _lib: libloading::Library,
    gpa: e::eglGetProcAddress,
}

impl Loaded {
    fn load(path: &Path) -> Result<Self> {
        // SAFETY: loading the driver runs only its initialisers (none of ours do work).
        let lib = unsafe { libloading::Library::new(path) }
            .with_context(|| format!("dlopen {}", path.display()))?;
        // SAFETY: eglGetProcAddress has the header's signature (exports.rs checks it).
        let gpa = *unsafe { lib.get::<e::eglGetProcAddress>(b"eglGetProcAddress\0") }
            .context("driver lacks eglGetProcAddress")?;
        Ok(Self { _lib: lib, gpa })
    }

    /// Loader.cpp: dlsym, else the driver's eglGetProcAddress.
    fn entry(&self, name: &CStr) -> Result<*const c_void> {
        // SAFETY: a lookup only; the caller types the result.
        if let Ok(f) = unsafe { self._lib.get::<*const c_void>(name.to_bytes_with_nul()) } {
            return Ok(*f);
        }
        // SAFETY: a NUL-terminated name.
        let f = unsafe { (self.gpa)(name.as_ptr()) }
            .with_context(|| format!("driver has no {name:?}"))?;
        Ok(f as *const c_void)
    }

    fn exports(&self, name: &CStr) -> bool {
        // SAFETY: a lookup only.
        unsafe { self._lib.get::<*const c_void>(name.to_bytes_with_nul()) }.is_ok()
    }
}

/// Casts an entry to its type.
macro_rules! entry {
    ($src:expr, $name:literal, $ty:ty) => {{
        let p = $src($name)?;
        // SAFETY: the address of the registry entry `$name`, whose C signature is `$ty` (Khronos headers).
        unsafe { std::mem::transmute::<*const c_void, $ty>(p) }
    }};
}

fn run(args: &Args) -> Result<()> {
    // the window first: its gralloc node is the property the driver reads when it loads (as atld writes an app's properties before the app starts)
    let win = window(args.alloc.as_deref())?;
    let drv = Loaded::load(&args.driver)?;
    for name in [c"glBindFramebuffer", c"glGetIntegerv", c"glClear"] {
        // Loader.cpp dlsyms gl* first: an exported gl* name would bind before eglGetProcAddress and bypass the FBO-0 wrappers
        ensure!(!drv.exports(name), "driver exports {name:?}");
    }
    let host = atl_egl_host::Host::load()?;
    let from_host = |name: &CStr| -> Result<*const c_void> {
        let p = host.proc_address(name);
        ensure!(!p.is_null(), "host has no {name:?}");
        Ok(p)
    };
    let from_drv = |name: &CStr| drv.entry(name);
    let fbo0_src: &dyn Fn(&CStr) -> Result<*const c_void> = if args.broken == Some(Break::Fbo0) {
        &from_host
    } else {
        &from_drv
    };

    let get_display = entry!(from_drv, c"eglGetDisplay", e::eglGetDisplay);
    let initialize = entry!(from_drv, c"eglInitialize", EglInitialize);
    let bind_api = entry!(from_drv, c"eglBindAPI", EglBindApi);
    let choose = entry!(from_drv, c"eglChooseConfig", e::eglChooseConfig);
    let attrib = entry!(from_drv, c"eglGetConfigAttrib", e::eglGetConfigAttrib);
    let create_ctx = entry!(from_drv, c"eglCreateContext", EglCreateContext);
    let create_surface = entry!(
        from_drv,
        c"eglCreateWindowSurface",
        e::eglCreateWindowSurface
    );
    let make_current = entry!(from_drv, c"eglMakeCurrent", e::eglMakeCurrent);
    let swap = entry!(from_drv, c"eglSwapBuffers", e::eglSwapBuffers);
    let destroy = entry!(from_drv, c"eglDestroySurface", e::eglDestroySurface);
    let egl_error = entry!(from_drv, c"eglGetError", e::eglGetError);
    let bind_fb = entry!(fbo0_src, c"glBindFramebuffer", g::glBindFramebuffer);
    let get_integer = entry!(fbo0_src, c"glGetIntegerv", g::glGetIntegerv);
    let invalidate = entry!(
        fbo0_src,
        c"glInvalidateFramebuffer",
        g::glInvalidateFramebuffer
    );
    let viewport = entry!(from_drv, c"glViewport", GlViewport);
    let enable = entry!(from_drv, c"glEnable", GlCap);
    let disable = entry!(from_drv, c"glDisable", GlCap);
    let scissor = entry!(from_drv, c"glScissor", GlViewport);
    let clear_color = entry!(from_drv, c"glClearColor", GlClearColor);
    let clear = entry!(from_drv, c"glClear", GlClear);
    let gl_error = entry!(from_drv, c"glGetError", GlGetError);
    // SAFETY: no arguments.
    let egl_fail = |what: &str| anyhow!("{what}: EGL error {:#x}", unsafe { egl_error() });

    // the host's own entry, not wrapped by the driver: the flip-y break undoes the driver's setting on the bound FBO; resolved only for it (a host without flip-y has no entry)
    let fb_param = if args.broken == Some(Break::FlipY) {
        Some(entry!(
            from_host,
            c"glFramebufferParameteriMESA",
            g::glFramebufferParameteriMESA
        ))
    } else {
        None
    };
    // as an app does before drawing (ANativeWindow_setBuffersTransform -> perform): the driver must queue every buffer with it unchanged
    // SAFETY: our own window and hook; the op takes one int.
    unsafe {
        perform(
            std::ptr::from_ref(&win.anw).cast_mut(),
            w::NATIVE_WINDOW_SET_BUFFERS_TRANSFORM as i32,
            APP_TRANSFORM as usize,
        )
    };
    // SAFETY: every call below passes handles the driver returned, NONE-terminated attribute lists and valid out-pointers; GL calls run with the context current.
    unsafe {
        let dpy = get_display(std::ptr::null_mut());
        ensure!(!dpy.is_null(), egl_fail("eglGetDisplay"));
        let (mut major, mut minor) = (0, 0);
        ensure!(
            initialize(dpy, &mut major, &mut minor) != 0,
            egl_fail("eglInitialize")
        );
        ensure!(bind_api(EGL_OPENGL_ES_API) != 0, egl_fail("eglBindAPI"));
        // hwui's EglManager config: RGBA8888, stencil 8, window + pbuffer, ES2
        let want = [
            e::EGL_RED_SIZE,
            8,
            e::EGL_GREEN_SIZE,
            8,
            e::EGL_BLUE_SIZE,
            8,
            e::EGL_ALPHA_SIZE,
            8,
            e::EGL_STENCIL_SIZE,
            8,
            EGL_RENDERABLE_TYPE,
            EGL_OPENGL_ES2_BIT,
            e::EGL_SURFACE_TYPE,
            e::EGL_WINDOW_BIT | e::EGL_PBUFFER_BIT,
            e::EGL_NONE,
        ];
        let (mut config, mut n) = (std::ptr::null_mut(), 0);
        ensure!(
            choose(dpy, want.as_ptr(), &mut config, 1, &mut n) != 0 && n == 1,
            egl_fail("eglChooseConfig(window)")
        );
        let mut visual = 0;
        ensure!(
            attrib(dpy, config, e::EGL_NATIVE_VISUAL_ID, &mut visual) != 0,
            egl_fail("EGL_NATIVE_VISUAL_ID")
        );
        ensure!(
            visual == RGBA_8888,
            "config's native visual {visual}, want RGBA_8888 ({RGBA_8888})"
        );
        let ctx_attribs = [EGL_CONTEXT_CLIENT_VERSION, 3, e::EGL_NONE];
        let ctx = create_ctx(dpy, config, std::ptr::null_mut(), ctx_attribs.as_ptr());
        ensure!(!ctx.is_null(), egl_fail("eglCreateContext(ES3)"));
        let anw = std::ptr::from_ref(&win.anw).cast_mut();
        let surface = create_surface(dpy, config, anw.cast(), std::ptr::null());
        ensure!(!surface.is_null(), egl_fail("eglCreateWindowSurface"));
        ensure!(
            make_current(dpy, surface, surface, ctx) != 0,
            egl_fail("eglMakeCurrent")
        );
        for frame in 0..FRAMES {
            bind_fb(g::GL_FRAMEBUFFER as u32, 0);
            let mut binding = -1;
            get_integer(g::GL_FRAMEBUFFER_BINDING as u32, &mut binding);
            let color = g::GL_COLOR as u32;
            invalidate(g::GL_FRAMEBUFFER as u32, 1, &color);
            let err = gl_error();
            ensure!(
                binding == 0 && err == 0,
                "frame {frame}: framebuffer 0 reads back as {binding}, invalidate(GL_COLOR) GL error {err:#x}"
            );
            if let Some(fb_param) = fb_param {
                fb_param(
                    g::GL_FRAMEBUFFER as u32,
                    g::GL_FRAMEBUFFER_FLIP_Y_MESA as u32,
                    0,
                );
            }
            viewport(0, 0, EDGE, EDGE);
            enable(GL_SCISSOR_TEST);
            for (x, y, fw, fh, rgba) in FILLS {
                let c = fill_colour(frame, rgba);
                scissor(x, y, fw, fh);
                clear_color(
                    f32::from(c[0]) / 255.0,
                    f32::from(c[1]) / 255.0,
                    f32::from(c[2]) / 255.0,
                    f32::from(c[3]) / 255.0,
                );
                clear(GL_COLOR_BUFFER_BIT);
            }
            disable(GL_SCISSOR_TEST);
            let err = gl_error();
            ensure!(err == 0, "frame {frame}: GL error {err:#x}");
            ensure!(swap(dpy, surface) != 0, egl_fail("eglSwapBuffers"));
        }
        ensure!(
            make_current(
                dpy,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut()
            ) != 0,
            egl_fail("eglMakeCurrent(none)")
        );
        ensure!(destroy(dpy, surface) != 0, egl_fail("eglDestroySurface"));
    }
    judge(&win)
}

// ---- the verdict ----

fn judge(win: &TestWindow) -> Result<()> {
    let s = win.state.lock().expect("probe state");
    out(&format!("usage {:#x}, {} queued", s.usage, s.queued.len()));
    // the driver's connection: connected once before the first dequeue, given back last (format 0, then disconnect), nothing left connected
    ensure!(
        s.events.first() == Some(&Hook::Connect(w::NATIVE_WINDOW_API_EGL)),
        "window events {:?}: want the driver's connect first",
        s.events
    );
    ensure!(
        s.events
            .ends_with(&[Hook::Format(0), Hook::Disconnect(w::NATIVE_WINDOW_API_EGL)]),
        "window events {:?}: want format 0, then disconnect, last",
        s.events
    );
    ensure!(
        s.api.is_none(),
        "window still connected to API {:?} after destroy",
        s.api
    );
    out(&format!("window events {:?}", s.events));
    ensure!(
        s.queued.len() == FRAMES,
        "{} buffers queued, want {FRAMES}",
        s.queued.len()
    );
    for (frame, q) in s.queued.iter().enumerate() {
        if let Some(f) = &q.fence {
            let mut fds = [rustix::event::PollFd::new(f, rustix::event::PollFlags::IN)];
            rustix::event::poll(&mut fds, None)?;
        }
        let slot = &win.slots[q.slot];
        let map = Mapping::read_only(slot.dmabuf.as_fd(), slot.size)?;
        begin_cpu_access(slot.dmabuf.as_fd(), SYNC_READ)?;
        let px = map.read();
        end_cpu_access(slot.dmabuf.as_fd(), SYNC_READ)?;
        ensure!(
            q.transform == APP_TRANSFORM,
            "frame {frame}: queued with transform {:#x}, the app set {APP_TRANSFORM:#x}: the driver clobbered it",
            q.transform
        );
        let mut bad = 0;
        let mut first = None;
        for r in 0..EDGE {
            for x in 0..EDGE {
                // the buffer as stored, row 0 the window's top (the app's transform applies after, to the whole upright image)
                let o = usize::try_from(r)? * slot.pitch as usize + usize::try_from(x)? * 4;
                let got = [px[o], px[o + 1], px[o + 2], px[o + 3]];
                // buffer row r is GL row EDGE-1-r
                let want = expected(frame, x, EDGE - 1 - r);
                if got != want {
                    bad += 1;
                    first.get_or_insert((x, r, got, want));
                }
            }
        }
        let verdict = if bad == 0 { "ok" } else { "red" };
        out(&format!(
            "frame {frame}: transform {:#x}, {bad} pixels differ: orientation {verdict}",
            q.transform
        ));
        if let Some((x, r, got, want)) = first {
            // the buffer as stored, row 0 first, one letter per 8x8 block's top-left pixel: what a failure looks like
            for by in (0..EDGE).step_by(8) {
                let row: String = (0..EDGE)
                    .step_by(8)
                    .map(|bx| {
                        let o = by as usize * slot.pitch as usize + bx as usize * 4;
                        match [px[o], px[o + 1], px[o + 2]] {
                            [255, 0, 0] => 'R',
                            [0, 255, 0] => 'G',
                            [0, 0, 255] => 'B',
                            [32, 64, 96] => '.',
                            [POISON, POISON, POISON] => 'P',
                            _ => '?',
                        }
                    })
                    .collect();
                out(&format!("  buffer row {by:2}: {row}"));
            }
            bail!("frame {frame}: display ({x}, {r}) is {got:?}, want {want:?}");
        }
    }
    Ok(())
}
