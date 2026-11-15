// SPDX-License-Identifier: MIT
//! JNI native of com.example.atles1.Client. Mode `driver` ([`run_driver`], gl-honesty H2a): the driver's ES1 context kind as an app reaches it. Modes `spike` / `break` (H1, O1 mechanism = ANGLE external context): the same sequence done from outside the driver, below.
//! 1. ANGLE display over our driver: eglGetPlatformDisplay(OPENGLES, EGL_PLATFORM_ANGLE_EGL_HANDLE_ANGLE = the driver's dlopen handle), eglInitialize. ANGLE's init leaves its mock pbuffer + renderer context current on the driver, so this goes first.
//! 2. Host carrier: our driver's window surface on the app's Surface + an ES3 context through AOSP libEGL, made current (break: the driver's current context is cleared instead, so ANGLE finds none).
//! 3. ANGLE ES1 context with EGL_EXTERNAL_CONTEXT_ANGLE (adopts the driver's current context) + an EGL_EXTERNAL_SURFACE_ANGLE pbuffer (ANGLE's FB 0 = the native FB 0 -> the driver's FBO-0 wrapper -> the surface FBO).
//! 4. Per frame: eglAcquireExternalContextANGLE, clear + one quad through libGLESv1_CM_angle, eglReleaseExternalContextANGLE, eglSwapBuffers through libEGL.
//! Picture (two copies: here and es1_angle.rs, the gate's digest): clear [`CLEAR`] everywhere, [`QUAD`] on the top-left quadrant (GL x 0..32, y 32..64; buffer rows 0..31 when upright).
//! Glibc dl* directly; every symbol through its own handle (no global egl*/gl* binding, gl-honesty O4).

use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::fmt::Write as _;

unsafe extern "C" {
    fn dlopen(file: *const c_char, flags: c_int) -> *mut c_void;
    fn dlsym(handle: *mut c_void, name: *const c_char) -> *mut c_void;
    fn dlerror() -> *const c_char;
}

const RTLD_NOW: c_int = 2;
const RTLD_NOLOAD: c_int = 4;
/// JNINativeInterface slots (jni.h: 4 reserved, then in order).
const NEW_STRING_UTF: usize = 167;
const GET_STRING_UTF_CHARS: usize = 169;
const RELEASE_STRING_UTF_CHARS: usize = 170;

/// Buffer edge (Client.SIZE).
const EDGE: i32 = 64;
/// Frames drawn and swapped: more than one, so the swap's dequeue-and-rebind runs under ANGLE too.
const FRAMES: usize = 3;
/// Clear and quad colours as bytes; GL gets c / 255 (ES 1.1 §2.13.9 / ES 3.2 §2.3.5.2 round back to c).
const CLEAR: [u8; 4] = [32, 64, 96, 255];
const QUAD: [u8; 4] = [255, 128, 0, 255];

// EGL 1.5 / eglext_angle.h values
const EGL_NONE: i32 = 0x3038;
const EGL_RED_SIZE: i32 = 0x3024;
const EGL_GREEN_SIZE: i32 = 0x3023;
const EGL_BLUE_SIZE: i32 = 0x3022;
const EGL_ALPHA_SIZE: i32 = 0x3021;
const EGL_SURFACE_TYPE: i32 = 0x3033;
const EGL_RENDERABLE_TYPE: i32 = 0x3040;
const EGL_WINDOW_BIT: i32 = 0x4;
const EGL_PBUFFER_BIT: i32 = 0x1;
const EGL_OPENGL_ES_BIT: i32 = 0x1;
const EGL_OPENGL_ES3_BIT: i32 = 0x40;
const EGL_CONTEXT_CLIENT_VERSION: i32 = 0x3098;
const EGL_WIDTH: i32 = 0x3057;
const EGL_HEIGHT: i32 = 0x3056;
const EGL_TRUE: i32 = 1;
const EGL_PLATFORM_ANGLE_ANGLE: u32 = 0x3202;
const EGL_PLATFORM_ANGLE_TYPE_ANGLE: isize = 0x3203;
const EGL_PLATFORM_ANGLE_TYPE_OPENGLES_ANGLE: isize = 0x320E;
const EGL_PLATFORM_ANGLE_EGL_HANDLE_ANGLE: isize = 0x3480;
const EGL_EXTERNAL_CONTEXT_ANGLE: i32 = 0x348E;
const EGL_EXTERNAL_SURFACE_ANGLE: u32 = 0x348F;
// GLES 1.1
const GL_VERSION: u32 = 0x1F02;
const GL_RENDERER: u32 = 0x1F01;
const GL_EXTENSIONS: u32 = 0x1F03;
const GL_COLOR_BUFFER_BIT: u32 = 0x4000;
const GL_PROJECTION: u32 = 0x1701;
const GL_MODELVIEW: u32 = 0x1700;
const GL_VERTEX_ARRAY: u32 = 0x8074;
const GL_FLOAT: u32 = 0x1406;
const GL_TRIANGLE_STRIP: u32 = 0x0005;
const GL_RGBA: u32 = 0x1908;
const GL_UNSIGNED_BYTE: u32 = 0x1401;

type P = *mut c_void;

struct Lib(P, String);

impl Lib {
    fn open(path: &str, flags: c_int) -> Result<Lib, String> {
        let c = CString::new(path).map_err(|e| e.to_string())?;
        // SAFETY: NUL-terminated path; glibc dlopen.
        let h = unsafe { dlopen(c.as_ptr(), flags) };
        if h.is_null() {
            // SAFETY: dlerror returns a NUL-terminated message or null.
            let e = unsafe { dlerror() };
            let why = if e.is_null() {
                "?".to_owned()
            } else {
                unsafe { CStr::from_ptr(e) }.to_string_lossy().into_owned()
            };
            return Err(format!("dlopen {path}: {why}"));
        }
        Ok(Lib(h, path.to_owned()))
    }

    /// The symbol as `T` (a fn pointer type of the C prototype the caller names): dlsym, else the library's own eglGetProcAddress (Loader.cpp `init_api`'s order; our driver exports only 16 egl*).
    fn sym<T: Copy>(&self, name: &CStr) -> Result<T, String> {
        // SAFETY: valid handle and NUL-terminated name.
        let mut p = unsafe { dlsym(self.0, name.as_ptr()) };
        if p.is_null() {
            // SAFETY: as above; eglGetProcAddress(const char*) when present.
            let gpa = unsafe { dlsym(self.0, c"eglGetProcAddress".as_ptr()) };
            if !gpa.is_null() {
                p = unsafe { std::mem::transmute::<P, GetProcAddress>(gpa)(name.as_ptr()) };
            }
        }
        if p.is_null() {
            return Err(format!("{} has no {}", self.1, name.to_string_lossy()));
        }
        // SAFETY: T is a pointer-sized fn pointer type matching the symbol's C prototype.
        Ok(unsafe { std::mem::transmute_copy::<P, T>(&p) })
    }
}

type GetDisplay = unsafe extern "C" fn(P) -> P;
type GetPlatformDisplay = unsafe extern "C" fn(u32, P, *const isize) -> P;
type Initialize = unsafe extern "C" fn(P, *mut i32, *mut i32) -> u32;
type ChooseConfig = unsafe extern "C" fn(P, *const i32, *mut P, i32, *mut i32) -> u32;
type CreateWindowSurface = unsafe extern "C" fn(P, P, P, *const i32) -> P;
type CreatePbufferFromClientBuffer = unsafe extern "C" fn(P, u32, P, P, *const i32) -> P;
type CreateContext = unsafe extern "C" fn(P, P, P, *const i32) -> P;
type MakeCurrent = unsafe extern "C" fn(P, P, P, P) -> u32;
type SwapBuffers = unsafe extern "C" fn(P, P) -> u32;
type Destroy = unsafe extern "C" fn(P, P) -> u32;
type GetError = unsafe extern "C" fn() -> i32;
type GetProcAddress = unsafe extern "C" fn(*const c_char) -> P;
type Acquire = unsafe extern "C" fn(P, P);
type Release = unsafe extern "C" fn(P);
type FromSurface = unsafe extern "C" fn(*mut *const *const c_void, P) -> P;
type GetConfigAttrib = unsafe extern "C" fn(P, P, i32, *mut i32) -> u32;
type QueryContext = unsafe extern "C" fn(P, P, i32, *mut i32) -> u32;

struct Egl {
    get_display: GetDisplay,
    initialize: Initialize,
    choose_config: ChooseConfig,
    create_context: CreateContext,
    make_current: MakeCurrent,
    swap: SwapBuffers,
    destroy_surface: Destroy,
    destroy_context: Destroy,
    get_error: GetError,
    get_proc: GetProcAddress,
}

impl Egl {
    fn load(lib: &Lib) -> Result<Egl, String> {
        Ok(Egl {
            get_display: lib.sym(c"eglGetDisplay")?,
            initialize: lib.sym(c"eglInitialize")?,
            choose_config: lib.sym(c"eglChooseConfig")?,
            create_context: lib.sym(c"eglCreateContext")?,
            make_current: lib.sym(c"eglMakeCurrent")?,
            swap: lib.sym(c"eglSwapBuffers")?,
            destroy_surface: lib.sym(c"eglDestroySurface")?,
            destroy_context: lib.sym(c"eglDestroyContext")?,
            get_error: lib.sym(c"eglGetError")?,
            get_proc: lib.sym(c"eglGetProcAddress")?,
        })
    }

    fn config(&self, dpy: P, attrs: &[i32]) -> Result<P, String> {
        let mut cfg: P = std::ptr::null_mut();
        let mut n = 0;
        // SAFETY: attrs is EGL_NONE-terminated; one config slot.
        let ok = unsafe { (self.choose_config)(dpy, attrs.as_ptr(), &mut cfg, 1, &mut n) };
        if ok == 0 || n < 1 {
            return Err(format!(
                "eglChooseConfig: {} configs, error {:#x}",
                n,
                self.error()
            ));
        }
        Ok(cfg)
    }

    fn error(&self) -> i32 {
        // SAFETY: no arguments.
        unsafe { (self.get_error)() }
    }
}

struct Gl1 {
    get_string: unsafe extern "C" fn(u32) -> *const c_char,
    get_error: unsafe extern "C" fn() -> u32,
    viewport: unsafe extern "C" fn(i32, i32, i32, i32),
    clear_color: unsafe extern "C" fn(f32, f32, f32, f32),
    clear: unsafe extern "C" fn(u32),
    matrix_mode: unsafe extern "C" fn(u32),
    load_identity: unsafe extern "C" fn(),
    orthof: unsafe extern "C" fn(f32, f32, f32, f32, f32, f32),
    enable_client_state: unsafe extern "C" fn(u32),
    vertex_pointer: unsafe extern "C" fn(i32, u32, i32, *const c_void),
    color4ub: unsafe extern "C" fn(u8, u8, u8, u8),
    draw_arrays: unsafe extern "C" fn(u32, i32, i32),
    read_pixels: unsafe extern "C" fn(i32, i32, i32, i32, u32, u32, *mut c_void),
}

impl Gl1 {
    fn load(lib: &Lib) -> Result<Gl1, String> {
        Ok(Gl1 {
            get_string: lib.sym(c"glGetString")?,
            get_error: lib.sym(c"glGetError")?,
            viewport: lib.sym(c"glViewport")?,
            clear_color: lib.sym(c"glClearColor")?,
            clear: lib.sym(c"glClear")?,
            matrix_mode: lib.sym(c"glMatrixMode")?,
            load_identity: lib.sym(c"glLoadIdentity")?,
            orthof: lib.sym(c"glOrthof")?,
            enable_client_state: lib.sym(c"glEnableClientState")?,
            vertex_pointer: lib.sym(c"glVertexPointer")?,
            color4ub: lib.sym(c"glColor4ub")?,
            draw_arrays: lib.sym(c"glDrawArrays")?,
            read_pixels: lib.sym(c"glReadPixels")?,
        })
    }

    fn string(&self, name: u32) -> String {
        // SAFETY: an ES1 context is current; the string is static per context.
        let s = unsafe { (self.get_string)(name) };
        if s.is_null() {
            String::new()
        } else {
            unsafe { CStr::from_ptr(s) }.to_string_lossy().into_owned()
        }
    }

    /// One frame: clear, the quad; returns the GL error after it (0 = none).
    fn frame(&self) -> u32 {
        let f = |c: u8| f32::from(c) / 255.0;
        let e = EDGE as f32;
        let quad: [f32; 8] = [0.0, e / 2.0, e / 2.0, e / 2.0, 0.0, e, e / 2.0, e];
        // SAFETY: an ES1 context is current; quad outlives the draw.
        unsafe {
            (self.viewport)(0, 0, EDGE, EDGE);
            (self.clear_color)(f(CLEAR[0]), f(CLEAR[1]), f(CLEAR[2]), f(CLEAR[3]));
            (self.clear)(GL_COLOR_BUFFER_BIT);
            (self.matrix_mode)(GL_PROJECTION);
            (self.load_identity)();
            (self.orthof)(0.0, e, 0.0, e, -1.0, 1.0);
            (self.matrix_mode)(GL_MODELVIEW);
            (self.load_identity)();
            (self.enable_client_state)(GL_VERTEX_ARRAY);
            (self.vertex_pointer)(2, GL_FLOAT, 0, quad.as_ptr().cast());
            (self.color4ub)(QUAD[0], QUAD[1], QUAD[2], QUAD[3]);
            (self.draw_arrays)(GL_TRIANGLE_STRIP, 0, 4);
            (self.get_error)()
        }
    }

    fn pixel(&self, x: i32, y: i32) -> String {
        let mut px = [0u8; 4];
        // SAFETY: an ES1 context is current; 1x1 RGBA into a 4-byte buffer.
        unsafe {
            (self.read_pixels)(
                x,
                y,
                1,
                1,
                GL_RGBA,
                GL_UNSIGNED_BYTE,
                px.as_mut_ptr().cast(),
            )
        };
        format!("{:02x}{:02x}{:02x}{:02x}", px[0], px[1], px[2], px[3])
    }
}

/// `draw(surface, angleEgl, angleGles1, driver, mode)`: newline-separated result lines; the last is `ok` or `error <step>: <why>`. Modes: `spike`, `break` ([`run`]), `driver` ([`run_driver`]).
#[unsafe(no_mangle)]
pub extern "C" fn Java_com_example_atles1_Client_draw(
    env: *mut *const *const c_void,
    _class: P,
    surface: P,
    angle_egl: P,
    angle_gles1: P,
    driver: P,
    mode: P,
) -> P {
    let mut out = String::new();
    let args = (
        java_text(env, angle_egl),
        java_text(env, angle_gles1),
        java_text(env, driver),
        java_text(env, mode),
    );
    let r = match args {
        (_, _, _, Some(m)) if m == "driver" => run_driver(env, surface, &mut out),
        (Some(a), Some(g), Some(d), m) => run(
            env,
            surface,
            &a,
            &g,
            &d,
            m.as_deref() == Some("break"),
            &mut out,
        ),
        _ => Err("args: a path is null".to_owned()),
    };
    match r {
        Ok(()) => out.push_str("ok"),
        Err(e) => {
            let _ = write!(out, "error {e}");
        }
    }
    new_string(env, &out)
}

fn run(
    env: *mut *const *const c_void,
    surface: P,
    angle_egl: &str,
    angle_gles1: &str,
    driver: &str,
    broken: bool,
    out: &mut String,
) -> Result<(), String> {
    let none = std::ptr::null_mut::<c_void>();
    let libegl = Lib::open("libEGL.so", RTLD_NOW)?;
    let host = Egl::load(&libegl)?;
    let android = Lib::open("libandroid.so", RTLD_NOW)?;
    let from_surface: FromSurface = android.sym(c"ANativeWindow_fromSurface")?;
    // libEGL's Loader loads the driver at eglInitialize; the handle is that copy (same inode)
    // SAFETY: EGL_DEFAULT_DISPLAY; out-params unused.
    let dpy = unsafe { (host.get_display)(none) };
    if unsafe { (host.initialize)(dpy, std::ptr::null_mut(), std::ptr::null_mut()) } == 0 {
        return Err(format!("host eglInitialize: {:#x}", host.error()));
    }
    let drv_lib = Lib::open(driver, RTLD_NOW | RTLD_NOLOAD)?;
    let drv = Egl::load(&drv_lib)?;

    // 1. ANGLE display over the driver
    let angle_lib = Lib::open(angle_egl, RTLD_NOW)?;
    let angle = Egl::load(&angle_lib)?;
    let platform: GetPlatformDisplay = angle_lib.sym(c"eglGetPlatformDisplay")?;
    let attrs = [
        EGL_PLATFORM_ANGLE_TYPE_ANGLE,
        EGL_PLATFORM_ANGLE_TYPE_OPENGLES_ANGLE,
        EGL_PLATFORM_ANGLE_EGL_HANDLE_ANGLE,
        drv_lib.0 as isize,
        EGL_NONE as isize,
    ];
    // SAFETY: attribute list EGL_NONE-terminated.
    let adpy = unsafe { platform(EGL_PLATFORM_ANGLE_ANGLE, none, attrs.as_ptr()) };
    if adpy.is_null() {
        return Err(format!("angle eglGetPlatformDisplay: {:#x}", angle.error()));
    }
    if unsafe { (angle.initialize)(adpy, std::ptr::null_mut(), std::ptr::null_mut()) } == 0 {
        return Err(format!("angle eglInitialize: {:#x}", angle.error()));
    }
    // ANGLE's init left its own context current on the driver: clear it, as the driver will before handing ANGLE its carrier
    // SAFETY: the driver's own display; releasing is always valid.
    let ddpy = unsafe { (drv.get_display)(none) };
    unsafe { (drv.make_current)(ddpy, none, none, none) };

    // 2. host carrier: the driver's window surface + an ES3 context, through libEGL
    let hcfg = host.config(
        dpy,
        &[
            EGL_RED_SIZE,
            8,
            EGL_GREEN_SIZE,
            8,
            EGL_BLUE_SIZE,
            8,
            EGL_ALPHA_SIZE,
            8,
            EGL_SURFACE_TYPE,
            EGL_WINDOW_BIT,
            EGL_RENDERABLE_TYPE,
            EGL_OPENGL_ES3_BIT,
            EGL_NONE,
        ],
    )?;
    // SAFETY: env and surface are the JNI arguments.
    let window = unsafe { from_surface(env, surface) };
    if window.is_null() {
        return Err("ANativeWindow_fromSurface: null".to_owned());
    }
    let cws: CreateWindowSurface = libegl.sym(c"eglCreateWindowSurface")?;
    let hsurf = unsafe { cws(dpy, hcfg, window, [EGL_NONE].as_ptr()) };
    if hsurf.is_null() {
        return Err(format!("host eglCreateWindowSurface: {:#x}", host.error()));
    }
    let hctx = unsafe {
        (host.create_context)(
            dpy,
            hcfg,
            none,
            [EGL_CONTEXT_CLIENT_VERSION, 3, EGL_NONE].as_ptr(),
        )
    };
    if hctx.is_null() {
        return Err(format!("host eglCreateContext: {:#x}", host.error()));
    }
    if broken {
        out.push_str("break no native context current\n");
    } else if unsafe { (host.make_current)(dpy, hsurf, hsurf, hctx) } == 0 {
        return Err(format!("host eglMakeCurrent: {:#x}", host.error()));
    }

    // 3. ANGLE ES1 external context + external surface. Guard first: with no native context current ANGLE does not fail, it SIGSEGVs (release build, ASSERT off: DisplayEGL::createRenderer adopts EGL_NO_CONTEXT,
    // FunctionsGL::initialize reads a null GL_VERSION; measured H1 2026-11-16) -- so the caller (the driver, H2) names the error itself.
    let current: unsafe extern "C" fn() -> P = drv_lib.sym(c"eglGetCurrentContext")?;
    if unsafe { current() }.is_null() {
        return Err("angle external context: no native context current on the driver (EGL_BAD_MATCH; ANGLE would SIGSEGV in FunctionsGL::initialize)".to_owned());
    }
    let acfg = angle
        .config(
            adpy,
            &[
                EGL_RED_SIZE,
                8,
                EGL_GREEN_SIZE,
                8,
                EGL_BLUE_SIZE,
                8,
                EGL_ALPHA_SIZE,
                8,
                EGL_SURFACE_TYPE,
                EGL_PBUFFER_BIT,
                EGL_RENDERABLE_TYPE,
                EGL_OPENGL_ES_BIT,
                EGL_NONE,
            ],
        )
        .map_err(|e| format!("angle {e}"))?;
    let actx = unsafe {
        (angle.create_context)(
            adpy,
            acfg,
            none,
            [
                EGL_CONTEXT_CLIENT_VERSION,
                1,
                EGL_EXTERNAL_CONTEXT_ANGLE,
                EGL_TRUE,
                EGL_NONE,
            ]
            .as_ptr(),
        )
    };
    if actx.is_null() {
        return Err(format!(
            "angle eglCreateContext(ES1, external): {:#x}",
            angle.error()
        ));
    }
    let pfcb: CreatePbufferFromClientBuffer = angle_lib.sym(c"eglCreatePbufferFromClientBuffer")?;
    let asurf = unsafe {
        pfcb(
            adpy,
            EGL_EXTERNAL_SURFACE_ANGLE,
            none,
            acfg,
            [EGL_WIDTH, EDGE, EGL_HEIGHT, EDGE, EGL_NONE].as_ptr(),
        )
    };
    if asurf.is_null() {
        return Err(format!(
            "angle eglCreatePbufferFromClientBuffer(external): {:#x}",
            angle.error()
        ));
    }
    if unsafe { (angle.make_current)(adpy, asurf, asurf, actx) } == 0 {
        return Err(format!("angle eglMakeCurrent: {:#x}", angle.error()));
    }
    // SAFETY: NUL-terminated names; the results are ANGLE's entry points of eglext_angle.h's prototypes, or null.
    let acq = unsafe { (angle.get_proc)(c"eglAcquireExternalContextANGLE".as_ptr()) };
    let rel = unsafe { (angle.get_proc)(c"eglReleaseExternalContextANGLE".as_ptr()) };
    if acq.is_null() || rel.is_null() {
        return Err("angle: no eglAcquire/ReleaseExternalContextANGLE".to_owned());
    }
    let acquire: Acquire = unsafe { std::mem::transmute::<P, Acquire>(acq) };
    let release: Release = unsafe { std::mem::transmute::<P, Release>(rel) };
    let gl = Gl1::load(&Lib::open(angle_gles1, RTLD_NOW)?)?;
    let _ = writeln!(out, "version {}", gl.string(GL_VERSION));
    let _ = writeln!(out, "renderer {}", gl.string(GL_RENDERER));
    let _ = writeln!(
        out,
        "extensions {}",
        gl.string(GL_EXTENSIONS).split_whitespace().count()
    );

    // 4. frames
    for n in 0..FRAMES {
        unsafe { acquire(adpy, asurf) };
        let err = gl.frame();
        if n == 0 {
            // GL coordinates: inside the quad (top-left quadrant), and the clear (bottom-right)
            let _ = writeln!(
                out,
                "px quad {} clear {}",
                gl.pixel(8, EDGE - 8),
                gl.pixel(EDGE - 8, 8)
            );
        }
        unsafe { release(adpy) };
        if err != 0 {
            return Err(format!("frame {n}: gl error {err:#x}"));
        }
        if unsafe { (host.swap)(dpy, hsurf) } == 0 {
            return Err(format!(
                "frame {n}: host eglSwapBuffers: {:#x}",
                host.error()
            ));
        }
    }
    let _ = writeln!(out, "frames {FRAMES}");

    // teardown: ANGLE first (no eglTerminate: it would terminate the driver's display under libEGL), then the host
    unsafe {
        (angle.make_current)(adpy, none, none, none);
        (angle.destroy_surface)(adpy, asurf);
        (angle.destroy_context)(adpy, actx);
        (host.make_current)(dpy, none, none, none);
        (host.destroy_surface)(dpy, hsurf);
        (host.destroy_context)(dpy, hctx);
    }
    Ok(())
}

/// H2a: the driver's own ES1 context kind as an app reaches it. A config with the ES1 bit (GLSurfaceView's default chooser), an ES1 context (no routing: libEGL -> the driver), frames through AOSP's libGLESv1_CM front;
/// then in the same process an ES3 context (must be the host's, not ANGLE's) and the two cross-kind share contexts (must fail EGL_BAD_CONTEXT).
fn run_driver(env: *mut *const *const c_void, surface: P, out: &mut String) -> Result<(), String> {
    let none = std::ptr::null_mut::<c_void>();
    let libegl = Lib::open("libEGL.so", RTLD_NOW)?;
    let host = Egl::load(&libegl)?;
    let attrib: GetConfigAttrib = libegl.sym(c"eglGetConfigAttrib")?;
    let query: QueryContext = libegl.sym(c"eglQueryContext")?;
    let android = Lib::open("libandroid.so", RTLD_NOW)?;
    let from_surface: FromSurface = android.sym(c"ANativeWindow_fromSurface")?;
    // SAFETY (this function): EGL / GL calls through their own libraries with the arguments their prototypes take; attribute lists EGL_NONE-terminated.
    let dpy = unsafe { (host.get_display)(none) };
    if unsafe { (host.initialize)(dpy, std::ptr::null_mut(), std::ptr::null_mut()) } == 0 {
        return Err(format!("eglInitialize: {:#x}", host.error()));
    }
    let cfg = host.config(
        dpy,
        &[
            EGL_RED_SIZE,
            8,
            EGL_GREEN_SIZE,
            8,
            EGL_BLUE_SIZE,
            8,
            EGL_ALPHA_SIZE,
            8,
            EGL_SURFACE_TYPE,
            EGL_WINDOW_BIT,
            EGL_RENDERABLE_TYPE,
            EGL_OPENGL_ES_BIT,
            EGL_NONE,
        ],
    )?;
    let mut rt = 0;
    unsafe { attrib(dpy, cfg, EGL_RENDERABLE_TYPE, &mut rt) };
    let _ = writeln!(out, "config renderable {rt:#x}");
    let window = unsafe { from_surface(env, surface) };
    if window.is_null() {
        return Err("ANativeWindow_fromSurface: null".to_owned());
    }
    let cws: CreateWindowSurface = libegl.sym(c"eglCreateWindowSurface")?;
    let hsurf = unsafe { cws(dpy, cfg, window, [EGL_NONE].as_ptr()) };
    if hsurf.is_null() {
        return Err(format!("eglCreateWindowSurface: {:#x}", host.error()));
    }
    let ctx = unsafe {
        (host.create_context)(
            dpy,
            cfg,
            none,
            [EGL_CONTEXT_CLIENT_VERSION, 1, EGL_NONE].as_ptr(),
        )
    };
    if ctx.is_null() {
        return Err(format!("es1 eglCreateContext: {:#x}", host.error()));
    }
    let mut v = 0;
    unsafe { query(dpy, ctx, EGL_CONTEXT_CLIENT_VERSION, &mut v) };
    let _ = writeln!(out, "query version {v}");
    if unsafe { (host.make_current)(dpy, hsurf, hsurf, ctx) } == 0 {
        return Err(format!("es1 eglMakeCurrent: {:#x}", host.error()));
    }
    let gl = Gl1::load(&Lib::open("libGLESv1_CM.so", RTLD_NOW)?)?;
    let _ = writeln!(out, "version {}", gl.string(GL_VERSION));
    let _ = writeln!(out, "renderer {}", gl.string(GL_RENDERER));
    let _ = writeln!(
        out,
        "extensions {}",
        gl.string(GL_EXTENSIONS).split_whitespace().count()
    );
    for n in 0..FRAMES {
        let err = gl.frame();
        if n == 0 {
            let _ = writeln!(
                out,
                "px quad {} clear {}",
                gl.pixel(8, EDGE - 8),
                gl.pixel(EDGE - 8, 8)
            );
        }
        if err != 0 {
            return Err(format!("frame {n}: gl error {err:#x}"));
        }
        if unsafe { (host.swap)(dpy, hsurf) } == 0 {
            return Err(format!("frame {n}: eglSwapBuffers: {:#x}", host.error()));
        }
    }
    let _ = writeln!(out, "frames {FRAMES}");

    // an ES3 context in this ES1 process: the host's GLES, surfaceless
    let ctx3 = unsafe {
        (host.create_context)(
            dpy,
            cfg,
            none,
            [EGL_CONTEXT_CLIENT_VERSION, 3, EGL_NONE].as_ptr(),
        )
    };
    if ctx3.is_null() {
        return Err(format!("es3 eglCreateContext: {:#x}", host.error()));
    }
    if unsafe { (host.make_current)(dpy, none, none, ctx3) } == 0 {
        return Err(format!("es3 eglMakeCurrent: {:#x}", host.error()));
    }
    let gles2 = Lib::open("libGLESv2.so", RTLD_NOW)?;
    let get_string: unsafe extern "C" fn(u32) -> *const c_char = gles2.sym(c"glGetString")?;
    let s = unsafe { get_string(GL_VERSION) };
    let es3 = if s.is_null() {
        String::new()
    } else {
        unsafe { CStr::from_ptr(s) }.to_string_lossy().into_owned()
    };
    let _ = writeln!(out, "es3 version {es3}");

    // cross-kind share contexts: refused by the driver
    for (what, version, share) in [("es1-on-es3", 1, ctx3), ("es3-on-es1", 3, ctx)] {
        let c = unsafe {
            (host.create_context)(
                dpy,
                cfg,
                share,
                [EGL_CONTEXT_CLIENT_VERSION, version, EGL_NONE].as_ptr(),
            )
        };
        let _ = writeln!(
            out,
            "share {what} {} {:#x}",
            if c.is_null() { "refused" } else { "made" },
            host.error()
        );
        if !c.is_null() {
            unsafe { (host.destroy_context)(dpy, c) };
        }
    }

    unsafe {
        (host.make_current)(dpy, none, none, none);
        (host.destroy_context)(dpy, ctx3);
        (host.destroy_context)(dpy, ctx);
        (host.destroy_surface)(dpy, hsurf);
    }
    Ok(())
}

/// JNI NewStringUTF of `text`.
fn new_string(env: *mut *const *const c_void, text: &str) -> P {
    type New = unsafe extern "C" fn(*mut *const *const c_void, *const c_char) -> P;
    let c = CString::new(text.replace('\0', " ")).unwrap_or_default();
    // SAFETY: env is the JNIEnv* JNI passes; slot NEW_STRING_UTF is NewStringUTF.
    unsafe {
        let new: New = std::mem::transmute(*(*env).add(NEW_STRING_UTF));
        new(env, c.as_ptr())
    }
}

/// JNI GetStringUTFChars, copied out; None for null.
fn java_text(env: *mut *const *const c_void, s: P) -> Option<String> {
    type Get = unsafe extern "C" fn(*mut *const *const c_void, P, *mut u8) -> *const c_char;
    type Rel = unsafe extern "C" fn(*mut *const *const c_void, P, *const c_char);
    if s.is_null() {
        return None;
    }
    // SAFETY: env/s from JNI; the chars are read before their release.
    unsafe {
        let get: Get = std::mem::transmute(*(*env).add(GET_STRING_UTF_CHARS));
        let release: Rel = std::mem::transmute(*(*env).add(RELEASE_STRING_UTF_CHARS));
        let chars = get(env, s, std::ptr::null_mut());
        if chars.is_null() {
            return None;
        }
        let text = CStr::from_ptr(chars).to_string_lossy().into_owned();
        release(env, s, chars);
        Some(text)
    }
}
