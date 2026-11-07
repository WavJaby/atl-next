// SPDX-License-Identifier: MIT
//! The ES1 scene's native half (xtask/testdata/gl-probe/es1-scene.toml, mode es1): the probe's program run through libGLESv1_CM.so, resolved by dlsym the way a NEEDED binding would bind it.
//! No gl* symbol is linked here, so this library never binds to AOSP's fronts at load time. Output: `row <y> <hex>`, `dladdr <name> <file>`, `error <message>` lines; Probe.java prefixes each with `native `.
//! The program is the same lines Es1Scene.java runs (same vocabulary, same ES 1.1 values); `resolve` reports where each Happy Jump name's address lies.

use std::ffi::{CStr, CString, c_char, c_int, c_void};

#[repr(C)]
struct DlInfo {
    fname: *const c_char,
    fbase: *mut c_void,
    sname: *const c_char,
    saddr: *mut c_void,
}

unsafe extern "C" {
    fn dlopen(file: *const c_char, flags: c_int) -> *mut c_void;
    fn dlsym(handle: *mut c_void, name: *const c_char) -> *mut c_void;
    fn dladdr(addr: *const c_void, info: *mut DlInfo) -> c_int;
}

/// <dlfcn.h>: the front the framework already loaded, never a second copy.
const RTLD_NOW: c_int = 2;
const RTLD_NOLOAD: c_int = 4;
/// JNINativeInterface slots (jni.h: 4 reserved entries, then the functions in order).
const NEW_STRING_UTF: usize = 167;
const GET_STRING_UTF_CHARS: usize = 169;
const RELEASE_STRING_UTF_CHARS: usize = 170;

const GL_POINTS: u32 = 0x0000;
const GL_TRIANGLES: u32 = 0x0004;
const GL_FLOAT: u32 = 0x1406;
const GL_UNSIGNED_BYTE: u32 = 0x1401;
const GL_UNSIGNED_SHORT: u32 = 0x1403;
const GL_RGBA: u32 = 0x1908;
const GL_COLOR_BUFFER_BIT: u32 = 0x4000;
const GL_TEXTURE_2D: u32 = 0x0DE1;
const GL_TEXTURE_MIN_FILTER: u32 = 0x2801;
const GL_TEXTURE_MAG_FILTER: u32 = 0x2800;

/// The entry points the scene calls, by name in libGLESv1_CM.so (ES 1.1 signatures).
macro_rules! gl_table {
    ($($name:ident : fn($($arg:ty),*) $(-> $ret:ty)?),* $(,)?) => {
        #[allow(non_snake_case)]
        struct Gl {
            $($name: unsafe extern "C" fn($($arg),*) $(-> $ret)?,)*
        }

        impl Gl {
            fn load(lib: *mut c_void) -> Result<Gl, String> {
                Ok(Gl {
                    $($name: symbol(lib, stringify!($name))?,)*
                })
            }
        }
    };
}

gl_table! {
    glClearColor: fn(f32, f32, f32, f32),
    glClear: fn(u32),
    glColor4f: fn(f32, f32, f32, f32),
    glEnable: fn(u32),
    glDisable: fn(u32),
    glEnableClientState: fn(u32),
    glDisableClientState: fn(u32),
    glScissor: fn(i32, i32, i32, i32),
    glMatrixMode: fn(u32),
    glLoadIdentity: fn(),
    glOrthof: fn(f32, f32, f32, f32, f32, f32),
    glPushMatrix: fn(),
    glPopMatrix: fn(),
    glTranslatef: fn(f32, f32, f32),
    glRotatef: fn(f32, f32, f32, f32),
    glScalef: fn(f32, f32, f32),
    glVertexPointer: fn(i32, u32, i32, *const c_void),
    glColorPointer: fn(i32, u32, i32, *const c_void),
    glTexCoordPointer: fn(i32, u32, i32, *const c_void),
    glPointSizePointerOES: fn(u32, i32, *const c_void),
    glDrawElements: fn(u32, i32, u32, *const c_void),
    glDrawArrays: fn(u32, i32, i32),
    glBlendFunc: fn(u32, u32),
    glBindTexture: fn(u32, u32),
    glTexImage2D: fn(u32, i32, i32, i32, i32, i32, u32, u32, *const c_void),
    glTexParameterf: fn(u32, u32, f32),
    glReadPixels: fn(i32, i32, i32, i32, u32, u32, *mut c_void),
}

/// The address of `name` in `lib` as the function type `F`; a missing name is an error.
fn symbol<F: Copy>(lib: *mut c_void, name: &str) -> Result<F, String> {
    let c = CString::new(name).map_err(|e| e.to_string())?;
    // SAFETY: lib is a dlopen handle; c is NUL-terminated.
    let p = unsafe { dlsym(lib, c.as_ptr()) };
    if p.is_null() {
        return Err(format!("dlsym {name} in libGLESv1_CM.so"));
    }
    // SAFETY: F is a C function pointer type of the same size; the symbol has that ES 1.1 signature.
    Ok(unsafe { std::mem::transmute_copy::<*mut c_void, F>(&p) })
}

/// The front library (libGLESv1_CM.so, already loaded by the framework) and its entry points.
fn load_front() -> Result<(Gl, *mut c_void), String> {
    // SAFETY: NUL-terminated name; NOLOAD never runs constructors.
    let lib = unsafe { dlopen(c"libGLESv1_CM.so".as_ptr(), RTLD_NOW | RTLD_NOLOAD) };
    if lib.is_null() {
        return Err("libGLESv1_CM.so is not loaded".to_owned());
    }
    Ok((Gl::load(lib)?, lib))
}

/// The arrays the scene set most recently. The GL reads a client pointer at every draw until it is replaced, so each replacement is kept alive here.
#[derive(Default)]
struct State {
    vertices: Vec<f32>,
    colours: Vec<u8>,
    texcoords: Vec<f32>,
    point_sizes: Vec<f32>,
}

/// GL enum by name (ES 1.1 gl.h values; the same table as Es1Scene.java).
fn cap(name: &str) -> Result<u32, String> {
    Ok(match name {
        "SCISSOR_TEST" => 0x0C11,
        "BLEND" => 0x0BE2,
        "TEXTURE_2D" => 0x0DE1,
        "MODELVIEW" => 0x1700,
        "PROJECTION" => 0x1701,
        "SRC_ALPHA" => 0x0302,
        "ONE_MINUS_SRC_ALPHA" => 0x0303,
        "NEAREST" => 0x2600,
        "VERTEX_ARRAY" => 0x8074,
        "COLOR_ARRAY" => 0x8076,
        "TEXTURE_COORD_ARRAY" => 0x8078,
        "POINT_SIZE_ARRAY_OES" => 0x8B9C,
        _ => return Err(format!("GL name {name}")),
    })
}

fn num<T: std::str::FromStr>(s: &str) -> Result<T, String> {
    s.parse().map_err(|_| format!("bad number `{s}`"))
}

fn nums<T: std::str::FromStr>(a: &[&str], n: usize) -> Result<Vec<T>, String> {
    if a.len() != n {
        return Err(format!("takes {n} numbers, got {}", a.len()));
    }
    a.iter().map(|s| num(s)).collect()
}

fn floats(a: &[&str]) -> Result<Vec<f32>, String> {
    a.iter().map(|s| num(s)).collect()
}

fn bytes(a: &[&str]) -> Result<Vec<u8>, String> {
    a.iter().map(|s| num(s)).collect()
}

/// Four 0-255 channels as GL floats: k / 255.
fn channels(a: &[&str]) -> Result<[f32; 4], String> {
    let v: Vec<f32> = nums(a, 4)?;
    Ok(std::array::from_fn(|i| v[i] / 255.0))
}

fn one<'a>(a: &[&'a str]) -> Result<&'a str, String> {
    match a {
        [x] => Ok(*x),
        _ => Err(format!("takes one name, got {a:?}")),
    }
}

/// Runs one program line on the current context. Output lines go to `out`.
fn step(gl: &Gl, lib: *mut c_void, st: &mut State, line: &str, out: &mut Vec<String>) -> Result<(), String> {
    let t: Vec<&str> = line.split_whitespace().collect();
    let Some((&op, a)) = t.split_first() else {
        return Ok(());
    };
    // SAFETY (each call): the entry points are libGLESv1_CM.so's, which the current context dispatches through; arguments follow the ES 1.1 signatures; client pointers outlive their draws (State).
    match op {
        "clear" => {
            let c = channels(a)?;
            unsafe {
                (gl.glClearColor)(c[0], c[1], c[2], c[3]);
                (gl.glClear)(GL_COLOR_BUFFER_BIT);
            }
        }
        "color" => {
            let c = channels(a)?;
            unsafe { (gl.glColor4f)(c[0], c[1], c[2], c[3]) }
        }
        "enable" => unsafe { (gl.glEnable)(cap(one(a)?)?) },
        "disable" => unsafe { (gl.glDisable)(cap(one(a)?)?) },
        "client_enable" => unsafe { (gl.glEnableClientState)(cap(one(a)?)?) },
        "client_disable" => unsafe { (gl.glDisableClientState)(cap(one(a)?)?) },
        "scissor" => {
            let v: Vec<i32> = nums(a, 4)?;
            unsafe { (gl.glScissor)(v[0], v[1], v[2], v[3]) }
        }
        "matrix" => unsafe { (gl.glMatrixMode)(cap(one(a)?)?) },
        "load_identity" => unsafe { (gl.glLoadIdentity)() },
        "ortho" => {
            let v: Vec<f32> = nums(a, 6)?;
            unsafe { (gl.glOrthof)(v[0], v[1], v[2], v[3], v[4], v[5]) }
        }
        "push" => unsafe { (gl.glPushMatrix)() },
        "pop" => unsafe { (gl.glPopMatrix)() },
        "translate" => {
            let v: Vec<f32> = nums(a, 3)?;
            unsafe { (gl.glTranslatef)(v[0], v[1], v[2]) }
        }
        "rotate" => {
            let v: Vec<f32> = nums(a, 4)?;
            unsafe { (gl.glRotatef)(v[0], v[1], v[2], v[3]) }
        }
        "scale" => {
            let v: Vec<f32> = nums(a, 3)?;
            unsafe { (gl.glScalef)(v[0], v[1], v[2]) }
        }
        "vertices" => {
            let v = floats(a)?;
            unsafe { (gl.glVertexPointer)(2, GL_FLOAT, 0, v.as_ptr().cast()) }
            st.vertices = v;
        }
        "colors" => {
            let v = bytes(a)?;
            unsafe { (gl.glColorPointer)(4, GL_UNSIGNED_BYTE, 0, v.as_ptr().cast()) }
            st.colours = v;
        }
        "texcoords" => {
            let v = floats(a)?;
            unsafe { (gl.glTexCoordPointer)(2, GL_FLOAT, 0, v.as_ptr().cast()) }
            st.texcoords = v;
        }
        "point_sizes" => {
            let v = floats(a)?;
            unsafe { (gl.glPointSizePointerOES)(GL_FLOAT, 0, v.as_ptr().cast()) }
            st.point_sizes = v;
        }
        "elements" => {
            let v: Vec<u16> = a.iter().map(|s| num(s)).collect::<Result<_, String>>()?;
            let n = i32::try_from(v.len()).map_err(|e| e.to_string())?;
            unsafe { (gl.glDrawElements)(GL_TRIANGLES, n, GL_UNSIGNED_SHORT, v.as_ptr().cast()) }
        }
        "points" => {
            let v: Vec<i32> = nums(a, 2)?;
            unsafe { (gl.glDrawArrays)(GL_POINTS, v[0], v[1]) }
        }
        "blend_func" => {
            let [src, dst] = a else {
                return Err(format!("blend_func takes two names, got {a:?}"));
            };
            unsafe { (gl.glBlendFunc)(cap(src)?, cap(dst)?) }
        }
        "bind_texture" => {
            let n: u32 = num(one(a)?)?;
            unsafe { (gl.glBindTexture)(GL_TEXTURE_2D, n) }
        }
        "texture" => {
            let v: Vec<i32> = a.iter().map(|s| num(s)).collect::<Result<_, String>>()?;
            let (w, h) = (*v.first().ok_or("texture takes W H")?, *v.get(1).ok_or("texture takes W H")?);
            let px: Vec<u8> = v[2..]
                .iter()
                .map(|&c| u8::try_from(c).map_err(|e| e.to_string()))
                .collect::<Result<_, String>>()?;
            if px.len() != (4 * w * h) as usize {
                return Err(format!("texture {w}x{h} needs {} channels", 4 * w * h));
            }
            unsafe {
                (gl.glTexImage2D)(GL_TEXTURE_2D, 0, GL_RGBA as i32, w, h, 0, GL_RGBA, GL_UNSIGNED_BYTE, px.as_ptr().cast())
            }
        }
        "texture_filter" => {
            let f = cap(one(a)?)? as f32;
            unsafe {
                (gl.glTexParameterf)(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, f);
                (gl.glTexParameterf)(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, f);
            }
        }
        "read" => {
            let v: Vec<i32> = nums(a, 2)?;
            let (w, h) = (v[0], v[1]);
            let mut buf = vec![0u8; (4 * w * h) as usize];
            unsafe { (gl.glReadPixels)(0, 0, w, h, GL_RGBA, GL_UNSIGNED_BYTE, buf.as_mut_ptr().cast()) }
            for (y, row) in buf.chunks((4 * w) as usize).enumerate() {
                let hex: String = row.iter().map(|b| format!("{b:02x}")).collect();
                out.push(format!("row {y} {hex}"));
            }
        }
        "resolve" => {
            for name in a {
                let c = CString::new(*name).map_err(|e| e.to_string())?;
                // SAFETY: lib is a dlopen handle; c is NUL-terminated.
                let addr = unsafe { dlsym(lib, c.as_ptr()) };
                if addr.is_null() {
                    return Err(format!("dlsym {name} in libGLESv1_CM.so"));
                }
                let mut info = DlInfo {
                    fname: std::ptr::null(),
                    fbase: std::ptr::null_mut(),
                    sname: std::ptr::null(),
                    saddr: std::ptr::null_mut(),
                };
                // SAFETY: info is a valid out struct; dladdr only reads the address.
                if unsafe { dladdr(addr, &mut info) } == 0 || info.fname.is_null() {
                    return Err(format!("dladdr {name}"));
                }
                // SAFETY: dladdr's fname is a NUL-terminated path that stays valid while the object is loaded.
                let file = unsafe { CStr::from_ptr(info.fname) }.to_string_lossy();
                out.push(format!("dladdr {name} {file}"));
            }
        }
        _ => return Err(format!("unknown step {op}")),
    }
    Ok(())
}

/// The program's output lines; the first error ends the run and is the last line (`error <message> in <line>`).
fn run_program(gl: &Gl, lib: *mut c_void, program: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut st = State::default();
    for line in program.lines() {
        if let Err(e) = step(gl, lib, &mut st, line, &mut out) {
            out.push(format!("error {e} in `{line}`"));
            break;
        }
    }
    out
}

/// The JNIEnv function table slot `index` as the function type `F`.
///
/// # Safety
/// `env` is the JNIEnv* the JVM passed to a native method; slot `index` of its table holds a function of type `F`.
unsafe fn jni<F: Copy>(env: *mut *const *const c_void, index: usize) -> F {
    // SAFETY: per the contract above; the slot is one pointer wide.
    unsafe { std::mem::transmute_copy(&*(*env).add(index)) }
}

fn jni_string(env: *mut *const *const c_void, s: *mut c_void) -> String {
    type Get = unsafe extern "C" fn(*mut *const *const c_void, *mut c_void, *mut u8) -> *const c_char;
    type Release = unsafe extern "C" fn(*mut *const *const c_void, *mut c_void, *const c_char);
    // SAFETY: env is the JNIEnv* of this call; s is a jstring the JVM passed; the chars are released after the copy.
    unsafe {
        let get: Get = jni(env, GET_STRING_UTF_CHARS);
        let release: Release = jni(env, RELEASE_STRING_UTF_CHARS);
        let chars = get(env, s, std::ptr::null_mut());
        let text = CStr::from_ptr(chars).to_string_lossy().into_owned();
        release(env, s, chars);
        text
    }
}

fn new_string(env: *mut *const *const c_void, text: &str) -> *mut c_void {
    type New = unsafe extern "C" fn(*mut *const *const c_void, *const c_char) -> *mut c_void;
    let c = CString::new(text).unwrap_or_default();
    // SAFETY: env is the JNIEnv* of this call; c is NUL-terminated and outlives the call.
    unsafe {
        let new: New = jni(env, NEW_STRING_UTF);
        new(env, c.as_ptr())
    }
}

/// Probe.es1Scene: runs the program on the current context through libGLESv1_CM.so; returns the output lines joined by `\n`.
#[unsafe(no_mangle)]
pub extern "C" fn Java_com_example_atlgl_Probe_es1Scene(
    env: *mut *const *const c_void,
    _class: *mut c_void,
    program: *mut c_void,
) -> *mut c_void {
    let text = jni_string(env, program);
    let lines = match load_front() {
        Ok((gl, lib)) => run_program(&gl, lib, &text),
        Err(e) => vec![format!("error {e}")],
    };
    new_string(env, &lines.join("\n"))
}
