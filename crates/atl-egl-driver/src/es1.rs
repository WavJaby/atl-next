// SPDX-License-Identifier: MIT
//! ES1 contexts: ANGLE's GLES1 frontend embedded in the driver, its GL backend running on this driver (gl-honesty S2, M74 D1). ES2/ES3 contexts stay the host's.
//! One ANGLE display per process, made over this driver (`EGL_PLATFORM_ANGLE_EGL_HANDLE_ANGLE` = the driver's own dlopen handle; ANGLE resolves every egl* through it). An ES1 context = a host "carrier" context of the app's config + an ANGLE ES1 context in external-context mode, which adopts the carrier, + an ANGLE external surface sized as the draw surface.
//! libEGL's ES1 table points at ANGLE's libGLESv1_CM (placed as `libGLESv1_CM_atl.so`), which draws on ANGLE's current context; ANGLE's framebuffer 0 is the native one, so on a window surface it lands in the fbo0 wrapper -> the surface FBO.
//! Frame sequence: make-current(ES1) -> carrier + surface on the host, ANGLE make-current, acquire; swap -> release, the driver's swap, acquire; make-current away -> release, ANGLE make-current none, then the new binding.
//! Traps (H1, measured): with no native context current, ANGLE's external-context creation does not fail but SIGSEGVs (FunctionsGL::initialize reads a null GL_VERSION) -> guarded here, EGL_BAD_MATCH; ANGLE's display init leaves its own context current on the driver -> the thread's binding is saved and put back around every ANGLE call that may bind; no eglTerminate on the ANGLE display (it would terminate the driver's display under libEGL).
//! Refused, named and counted: share contexts across the two engines (ES1 with ES2/3, either way: nothing can be shared), and ES1 with ES1 (ANGLE's external contexts take no share context, DisplayEGL::createContext).

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ffi::{CString, c_void};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use atl_android_paths::{ANGLE_EGL, GL_DRIVER_EGL, GL_DRIVER_GLES1, VENDOR_EGL};
use libloading::os::unix::{Library, RTLD_LOCAL, RTLD_NOW};

use crate::abi::angle as a;
use crate::abi::egl as e;
use crate::driver::{self, Driver, set_error};
use crate::fbo0;

/// glibc's `RTLD_NOLOAD` (bits/dlfcn.h, the same value on every glibc arch): look a library up without loading it.
const RTLD_NOLOAD: i32 = 0x4;

/// Share-context refusals (O3): the counter of that refusal, logged with each one.
static SHARE_REFUSED: AtomicU64 = AtomicU64::new(0);

#[allow(
    clippy::print_stderr,
    reason = "one line per refusal or per failed ES1 create: the app's log names why"
)]
fn log(line: &str) {
    eprintln!("libEGL_atl: {line}");
}

fn fail<T>(err: i32, v: T) -> T {
    set_error(err);
    v
}

/// The share-context refusal: EGL_BAD_CONTEXT, named and counted.
pub fn refuse_share(why: &str) -> e::EGLContext {
    let n = SHARE_REFUSED.fetch_add(1, Ordering::Relaxed) + 1;
    log(&format!("es1 share refused ({why}) count={n}"));
    fail(e::EGL_BAD_CONTEXT, std::ptr::null_mut())
}

/// ANGLE's EGL entries, resolved through ANGLE's own handle only (gl-honesty O4: its egl* names collide with the fronts' and ours).
struct Angle {
    dpy: e::EGLDisplay,
    choose_config: a::eglChooseConfig,
    create_context: a::eglCreateContext,
    destroy_context: a::eglDestroyContext,
    create_pbuffer: a::eglCreatePbufferFromClientBuffer,
    destroy_surface: a::eglDestroySurface,
    make_current: a::eglMakeCurrent,
    get_error: a::eglGetError,
    acquire: a::eglAcquireExternalContextANGLE,
    release: a::eglReleaseExternalContextANGLE,
}

// SAFETY: the ANGLE display is a process-global EGL handle usable from any thread (EGL 1.5 §3.2); the rest are function pointers.
unsafe impl Send for Angle {}
// SAFETY: as Send.
unsafe impl Sync for Angle {}

static ANGLE: OnceLock<Result<Angle, String>> = OnceLock::new();

/// `name` from `lib`: its export, else its eglGetProcAddress (Loader.cpp `init_api`'s order).
///
/// # Safety
/// `T` is the fn-pointer type of `name`'s C prototype.
unsafe fn sym<T: Copy>(lib: &Library, gpa: a::eglGetProcAddress, name: &str) -> Result<T, String> {
    let c = CString::new(name).map_err(|err| err.to_string())?;
    // SAFETY: a NUL-terminated name; the pointer is only reinterpreted below as the caller's type.
    let p: *mut c_void = match unsafe { lib.get::<*mut c_void>(c.as_bytes_with_nul()) } {
        Ok(s) => *s,
        Err(_) => std::ptr::null_mut(),
    };
    let p = if p.is_null() {
        // SAFETY: ANGLE's eglGetProcAddress with a NUL-terminated name.
        unsafe { gpa(c.as_ptr()) }.map_or(std::ptr::null_mut(), |f| f as *mut c_void)
    } else {
        p
    };
    if p.is_null() {
        return Err(format!("{ANGLE_EGL} has no {name}"));
    }
    // SAFETY: per the caller, `T` is the pointer-sized fn type of this symbol.
    Ok(unsafe { std::mem::transmute_copy::<*mut c_void, T>(&p) })
}

impl Angle {
    fn load() -> Result<Self, String> {
        let own = format!("{VENDOR_EGL}/{GL_DRIVER_EGL}");
        // SAFETY: NOLOAD only looks the already-loaded driver up (libEGL's Loader opened it by this path): no constructor runs.
        let driver = unsafe { Library::open(Some(&own), RTLD_NOW | RTLD_NOLOAD) }
            .map_err(|err| format!("own handle {own}: {err}"))?;
        // SAFETY: ANGLE's EGL library; its constructors only set up ANGLE's globals.
        let lib = unsafe { Library::open(Some(ANGLE_EGL), RTLD_NOW | RTLD_LOCAL) }
            .map_err(|err| format!("{ANGLE_EGL}: {err}"))?;
        // SAFETY: eglGetProcAddress's prototype is the alias.
        let gpa: a::eglGetProcAddress =
            *unsafe { lib.get::<a::eglGetProcAddress>(b"eglGetProcAddress\0") }
                .map_err(|err| format!("{ANGLE_EGL}: {err}"))?;
        // SAFETY: each field's alias is the prototype of the name it is resolved by (ANGLE's egl.h / eglext_angle.h, build.rs).
        let (platform, initialize, angle): (a::eglGetPlatformDisplay, a::eglInitialize, Angle) = unsafe {
            (
                sym(&lib, gpa, "eglGetPlatformDisplay")?,
                sym(&lib, gpa, "eglInitialize")?,
                Angle {
                    dpy: std::ptr::null_mut(),
                    choose_config: sym(&lib, gpa, "eglChooseConfig")?,
                    create_context: sym(&lib, gpa, "eglCreateContext")?,
                    destroy_context: sym(&lib, gpa, "eglDestroyContext")?,
                    create_pbuffer: sym(&lib, gpa, "eglCreatePbufferFromClientBuffer")?,
                    destroy_surface: sym(&lib, gpa, "eglDestroySurface")?,
                    make_current: sym(&lib, gpa, "eglMakeCurrent")?,
                    get_error: sym(&lib, gpa, "eglGetError")?,
                    acquire: sym(&lib, gpa, "eglAcquireExternalContextANGLE")?,
                    release: sym(&lib, gpa, "eglReleaseExternalContextANGLE")?,
                },
            )
        };
        // EGLAttrib is intptr_t: the element type comes from the generated prototype
        let attrs = [
            a::EGL_PLATFORM_ANGLE_TYPE_ANGLE as _,
            a::EGL_PLATFORM_ANGLE_TYPE_OPENGLES_ANGLE as _,
            a::EGL_PLATFORM_ANGLE_EGL_HANDLE_ANGLE as _,
            driver.into_raw() as _,
            e::EGL_NONE as _,
        ];
        // SAFETY: a NONE-terminated attribute list; the native display is unused by this platform type.
        let dpy = unsafe {
            platform(
                a::EGL_PLATFORM_ANGLE_ANGLE as u32,
                std::ptr::null_mut(),
                attrs.as_ptr(),
            )
        };
        if dpy.is_null() {
            return Err(format!("eglGetPlatformDisplay: {:#x}", angle.error()));
        }
        // SAFETY: ANGLE's display; version out-params unused.
        if unsafe { initialize(dpy, std::ptr::null_mut(), std::ptr::null_mut()) } == 0 {
            return Err(format!("eglInitialize: {:#x}", angle.error()));
        }
        // ANGLE's code stays mapped for the process: its display is never terminated
        std::mem::forget(lib);
        Ok(Angle { dpy, ..angle })
    }

    fn error(&self) -> i32 {
        // SAFETY: no arguments.
        unsafe { (self.get_error)() }
    }

    /// An ANGLE config for an ES1 context over host `config`: the same channel, depth and stencil sizes, else any ES1 pbuffer config.
    fn config(&self, d: &Driver, config: e::EGLConfig) -> Result<e::EGLConfig, String> {
        let mut exact = Vec::new();
        for k in [
            e::EGL_RED_SIZE,
            e::EGL_GREEN_SIZE,
            e::EGL_BLUE_SIZE,
            e::EGL_ALPHA_SIZE,
            e::EGL_DEPTH_SIZE,
            e::EGL_STENCIL_SIZE,
        ] {
            if let Some(v) = d.host_attrib(config, k) {
                exact.extend([k, v]);
            }
        }
        let base = [
            e::EGL_SURFACE_TYPE,
            e::EGL_PBUFFER_BIT,
            e::EGL_RENDERABLE_TYPE,
            e::EGL_OPENGL_ES_BIT,
        ];
        for attrs in [[exact.as_slice(), &base].concat(), base.to_vec()] {
            let mut list = attrs;
            list.push(e::EGL_NONE);
            let (mut cfg, mut n) = (std::ptr::null_mut(), 0);
            // SAFETY: a NONE-terminated list, one config slot.
            let ok = unsafe { (self.choose_config)(self.dpy, list.as_ptr(), &mut cfg, 1, &mut n) };
            if ok != 0 && n == 1 {
                return Ok(cfg);
            }
        }
        Err(format!("no ES1 pbuffer config: {:#x}", self.error()))
    }
}

/// The ANGLE display, made on first use with this thread's binding put back afterwards.
fn angle(d: &Driver) -> Result<&'static Angle, String> {
    ANGLE
        .get_or_init(|| inside_angle(d, Angle::load))
        .as_ref()
        .map_err(Clone::clone)
}

/// This thread's host binding and fbo0 state.
struct Binding {
    ctx: e::EGLContext,
    draw: e::EGLSurface,
    read: e::EGLSurface,
    fbo0: (Option<(usize, usize)>, u32),
}

impl Binding {
    fn save(d: &Driver) -> Self {
        // SAFETY: current-binding queries, no arguments beyond the readdraw enum.
        unsafe {
            Binding {
                ctx: (d.egl.eglGetCurrentContext)(),
                draw: (d.egl.eglGetCurrentSurface)(e::EGL_DRAW),
                read: (d.egl.eglGetCurrentSurface)(e::EGL_READ),
                fbo0: fbo0::state(),
            }
        }
    }

    fn restore(&self, d: &Driver) {
        // SAFETY: the binding this thread had (or none); its handles are still live (nothing destroyed them in between).
        unsafe { (d.egl.eglMakeCurrent)(d.display.raw(), self.draw, self.read, self.ctx) };
        fbo0::set_current(self.fbo0.0, self.fbo0.1);
    }
}

/// The ANGLE external surface standing for the draw surface: its size follows the draw surface's.
struct External {
    raw: e::EGLSurface,
    size: (i32, i32),
}

pub struct Es1Context {
    /// The app's (host) config.
    pub config: e::EGLConfig,
    /// The host context ANGLE renders through.
    pub carrier: e::EGLContext,
    angle_ctx: e::EGLContext,
    angle_config: e::EGLConfig,
    surface: Mutex<Option<External>>,
}

// SAFETY: EGL handles are process-global (EGL 1.5 §3.2); the surface slot is behind a mutex.
unsafe impl Send for Es1Context {}
// SAFETY: as Send.
unsafe impl Sync for Es1Context {}

static CONTEXTS: OnceLock<Mutex<HashMap<usize, Arc<Es1Context>>>> = OnceLock::new();

thread_local! {
    /// The ES1 context current on this thread (ANGLE holds the native state: acquired).
    static CURRENT: RefCell<Option<Arc<Es1Context>>> = const { RefCell::new(None) };
    /// Inside an ANGLE display or context creation: ANGLE's own calls into the driver's eglMakeCurrent are its native binding, not an app's, so they leave [`CURRENT`] alone.
    static IN_ANGLE: Cell<bool> = const { Cell::new(false) };
}

/// `f` with [`IN_ANGLE`] set and this thread's host binding put back afterwards.
fn inside_angle<R>(d: &Driver, f: impl FnOnce() -> R) -> R {
    let saved = Binding::save(d);
    let outer = IN_ANGLE.with(|b| b.replace(true));
    let r = f();
    IN_ANGLE.with(|b| b.set(outer));
    saved.restore(d);
    r
}

fn contexts() -> &'static Mutex<HashMap<usize, Arc<Es1Context>>> {
    CONTEXTS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The ES1 context of a handle the driver gave out.
pub fn lookup(ctx: e::EGLContext) -> Option<Arc<Es1Context>> {
    if ctx.is_null() {
        return None;
    }
    contexts().lock().ok()?.get(&(ctx as usize)).cloned()
}

/// The ES1 lib must be the one libEGL loaded from the driver dir: without it Loader.cpp fills the ES1 table by `dlsym(NULL, ...)`, which finds the fronts' own gl* (they would call themselves).
fn es1_lib_placed() -> Result<(), String> {
    let path = format!("{VENDOR_EGL}/{GL_DRIVER_GLES1}");
    // SAFETY: NOLOAD only looks the library up: nothing is loaded, no constructor runs.
    unsafe { Library::open(Some(&path), RTLD_NOW | RTLD_NOLOAD) }
        .map(drop)
        .map_err(|_| {
            format!("{path} not loaded (libEGL's ES1 table would call the fronts themselves)")
        })
}

/// eglCreateContext for client version 1 (or none): a carrier on the host with the app's config, then ANGLE's external context adopting it.
pub fn create(d: &'static Driver, config: e::EGLConfig, share: e::EGLContext) -> e::EGLContext {
    if !share.is_null() {
        return refuse_share(if lookup(share).is_some() {
            "ES1 with ES1: ANGLE's external contexts take no share context"
        } else {
            "ES1 with an ES2/3 context: the two engines share no objects"
        });
    }
    if let Err(why) = es1_lib_placed() {
        log(&format!("es1 context refused: {why}"));
        return fail(e::EGL_BAD_ALLOC, std::ptr::null_mut());
    }
    let angle = match angle(d) {
        Ok(a) => a,
        Err(why) => {
            log(&format!("es1 context refused: ANGLE display: {why}"));
            return fail(e::EGL_BAD_ALLOC, std::ptr::null_mut());
        }
    };
    let dpy = d.display.raw();
    let mut carrier = std::ptr::null_mut();
    for version in [3, 2] {
        let attrs = [e::EGL_CONTEXT_CLIENT_VERSION, version, e::EGL_NONE];
        // SAFETY: the host display, a host config the app chose, NONE-terminated attributes, no share context.
        carrier =
            unsafe { (d.egl.eglCreateContext)(dpy, config, std::ptr::null_mut(), attrs.as_ptr()) };
        if !carrier.is_null() {
            break;
        }
    }
    if carrier.is_null() {
        // SAFETY: no arguments; the host's error of the failed create.
        return fail(unsafe { (d.egl.eglGetError)() }, std::ptr::null_mut());
    }
    match inside_angle(d, || adopt(d, angle, config, carrier)) {
        Ok((angle_ctx, angle_config)) => {
            let c = Arc::new(Es1Context {
                config,
                carrier,
                angle_ctx,
                angle_config,
                surface: Mutex::new(None),
            });
            let handle = Arc::as_ptr(&c) as usize;
            match contexts().lock() {
                Ok(mut m) => {
                    m.insert(handle, c);
                    handle as e::EGLContext
                }
                Err(_) => fail(e::EGL_BAD_ALLOC, std::ptr::null_mut()),
            }
        }
        Err((err, why)) => {
            log(&format!("es1 context refused: {why}"));
            // SAFETY: the carrier made above, current nowhere (the binding was put back).
            unsafe { (d.egl.eglDestroyContext)(dpy, carrier) };
            fail(err, std::ptr::null_mut())
        }
    }
}

/// The ANGLE ES1 context over `carrier`, made current surfaceless for ANGLE to adopt.
fn adopt(
    d: &Driver,
    angle: &Angle,
    config: e::EGLConfig,
    carrier: e::EGLContext,
) -> Result<(e::EGLContext, e::EGLConfig), (i32, String)> {
    let none = std::ptr::null_mut();
    // SAFETY: surfaceless make-current of the carrier (EGL_KHR_surfaceless_context, both target hosts).
    let made = unsafe { (d.egl.eglMakeCurrent)(d.display.raw(), none, none, carrier) };
    // SAFETY: no arguments.
    if made == 0 || unsafe { (d.egl.eglGetCurrentContext)() } != carrier {
        return Err((e::EGL_BAD_MATCH, "no native context current for ANGLE to adopt (ANGLE would SIGSEGV in FunctionsGL::initialize)".to_owned()));
    }
    let angle_config = angle
        .config(d, config)
        .map_err(|why| (e::EGL_BAD_CONFIG, format!("ANGLE {why}")))?;
    let attrs = [
        e::EGL_CONTEXT_CLIENT_VERSION,
        1,
        a::EGL_EXTERNAL_CONTEXT_ANGLE,
        e::EGL_TRUE,
        e::EGL_NONE,
    ];
    // SAFETY: ANGLE's display and config, NONE-terminated attributes, no share context.
    let ctx = unsafe { (angle.create_context)(angle.dpy, angle_config, none, attrs.as_ptr()) };
    if ctx.is_null() {
        return Err((
            e::EGL_BAD_ALLOC,
            format!(
                "ANGLE eglCreateContext(ES1, external): {:#x}",
                angle.error()
            ),
        ));
    }
    Ok((ctx, angle_config))
}

impl Es1Context {
    /// After the carrier and the draw surface (of `size`) went current on the host: ANGLE's context on an external surface of that size, acquired.
    pub fn enter(self: &Arc<Self>, size: (i32, i32)) -> Result<(), i32> {
        let Some(Ok(angle)) = ANGLE.get() else {
            return Err(e::EGL_BAD_CONTEXT);
        };
        let mut slot = self.surface.lock().map_err(|_| e::EGL_BAD_CONTEXT)?;
        if slot.as_ref().map(|s| s.size) != Some(size) {
            if let Some(old) = slot.take() {
                // SAFETY: ANGLE's surface, current nowhere (this context left ANGLE before its re-entry).
                unsafe { (angle.destroy_surface)(angle.dpy, old.raw) };
            }
            let attrs = [e::EGL_WIDTH, size.0, e::EGL_HEIGHT, size.1, e::EGL_NONE];
            // SAFETY: ANGLE's display and config, NONE-terminated attributes; an external surface takes no client buffer.
            let raw = unsafe {
                (angle.create_pbuffer)(
                    angle.dpy,
                    a::EGL_EXTERNAL_SURFACE_ANGLE as u32,
                    std::ptr::null_mut(),
                    self.angle_config,
                    attrs.as_ptr(),
                )
            };
            if raw.is_null() {
                log(&format!(
                    "es1 make-current: ANGLE external surface {}x{}: {:#x}",
                    size.0,
                    size.1,
                    angle.error()
                ));
                return Err(e::EGL_BAD_ALLOC);
            }
            *slot = Some(External { raw, size });
        }
        let raw = slot.as_ref().map_or(std::ptr::null_mut(), |s| s.raw);
        // SAFETY: ANGLE's display, surface and context.
        if unsafe { (angle.make_current)(angle.dpy, raw, raw, self.angle_ctx) } == 0 {
            log(&format!(
                "es1 make-current: ANGLE eglMakeCurrent: {:#x}",
                angle.error()
            ));
            return Err(e::EGL_BAD_MATCH);
        }
        // SAFETY: ANGLE's current external context and its surface.
        unsafe { (angle.acquire)(angle.dpy, raw) };
        drop(slot);
        CURRENT.with(|c| *c.borrow_mut() = Some(Arc::clone(self)));
        Ok(())
    }
}

/// An ES1 context is current on this thread (es1_ext.rs's shells answer for ES1 then).
pub fn is_current() -> bool {
    CURRENT.with(|c| c.borrow().is_some())
}

/// Inside ANGLE's display or context creation: ANGLE resolving its native GL through this driver must get the host's entries, never es1_ext's shells (an ES1 context may be current while ANGLE's backend calls them).
pub fn in_angle() -> bool {
    IN_ANGLE.with(Cell::get)
}

/// The ES1 context current on this thread, if any, leaves ANGLE: released, ANGLE's binding cleared. Before any other make-current on the thread.
pub fn leave_current() {
    if IN_ANGLE.with(Cell::get) {
        return;
    }
    let Some(_c) = CURRENT.with(|c| c.borrow_mut().take()) else {
        return;
    };
    if let Some(Ok(angle)) = ANGLE.get() {
        let none = std::ptr::null_mut();
        // SAFETY: ANGLE's display; the context being left is current and acquired on this thread.
        unsafe {
            (angle.release)(angle.dpy);
            (angle.make_current)(angle.dpy, none, none, none);
        }
    }
}

/// `swap` with ANGLE's hold on the native state released around it, when an ES1 context is current on this thread.
pub fn around_swap<T>(swap: impl FnOnce() -> T) -> T {
    let current = CURRENT.with(|c| c.borrow().clone());
    let angle = ANGLE.get().and_then(|r| r.as_ref().ok());
    let (Some(c), Some(angle)) = (current, angle) else {
        return swap();
    };
    // SAFETY: ANGLE's display; the current external context is acquired.
    unsafe { (angle.release)(angle.dpy) };
    let r = swap();
    let raw = c
        .surface
        .lock()
        .ok()
        .and_then(|s| s.as_ref().map(|s| s.raw))
        .unwrap_or(std::ptr::null_mut());
    // SAFETY: ANGLE's current external context and its surface.
    unsafe { (angle.acquire)(angle.dpy, raw) };
    r
}

/// eglDestroyContext of an ES1 handle: forgotten now, freed when no thread still has it current (EGL 1.5 §3.7.1.4). `None`: not an ES1 handle.
pub fn destroy(ctx: e::EGLContext) -> Option<u32> {
    let c = contexts().lock().ok()?.remove(&(ctx as usize))?;
    drop(c);
    Some(e::EGL_TRUE as u32)
}

/// eglQueryContext of an ES1 handle; `None`: not an ES1 handle.
pub fn query(d: &Driver, ctx: e::EGLContext, attr: i32, value: *mut i32) -> Option<u32> {
    let c = lookup(ctx)?;
    if value.is_null() {
        return Some(fail(e::EGL_BAD_PARAMETER, 0));
    }
    let v = match attr {
        e::EGL_CONTEXT_CLIENT_VERSION => 1,
        e::EGL_CONTEXT_CLIENT_TYPE => e::EGL_OPENGL_ES_API,
        e::EGL_RENDER_BUFFER => e::EGL_BACK_BUFFER,
        e::EGL_CONFIG_ID => match d.host_attrib(c.config, e::EGL_CONFIG_ID) {
            Some(v) => v,
            None => return Some(fail(e::EGL_BAD_CONTEXT, 0)),
        },
        _ => return Some(fail(e::EGL_BAD_ATTRIBUTE, 0)),
    };
    // SAFETY: checked non-null.
    unsafe { *value = v };
    Some(e::EGL_TRUE as u32)
}

impl Drop for Es1Context {
    fn drop(&mut self) {
        if let Some(Ok(angle)) = ANGLE.get() {
            // SAFETY: ANGLE's handles of this context; current nowhere (the last holder left it).
            unsafe {
                if let Ok(Some(s)) = self.surface.get_mut().map(Option::take) {
                    (angle.destroy_surface)(angle.dpy, s.raw);
                }
                (angle.destroy_context)(angle.dpy, self.angle_ctx);
            }
        }
        if let Some(d) = driver::get() {
            // SAFETY: the host carrier; the host defers its deletion while it is current.
            unsafe { (d.egl.eglDestroyContext)(d.display.raw(), self.carrier) };
        }
    }
}
