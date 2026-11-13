// SPDX-License-Identifier: MIT
//! Gate `egl-bridge` (GS2.2 G1, identity engine): `eglGetProcAddress` through the bridge. An NDK x86_64 fixture (`xtask-harness/testdata/egl-bridge/fixture.c`, DT_NEEDED libEGL.so and libGLESv2.so -> the guest proxies) imports glVertexAttrib4f and glTexImage2D, asks eglGetProcAddress for them, for glFinish and for two names no row holds. The host side is the recorder (`examples/egl_recorder.rs`), the process's `/system/lib64/libEGL.so` and `libGLESv2.so` (the harness's bwrap root).
//! Env: tests/common/mod.rs (`ATL_NB`, `ATL_NB_LIBCXX`, `ATL_NB_TESTLIB`, the fixture dir); SKIP without them. `ATL_EGL_CASE=trap` runs check c alone.
//! Checks (line `egl-bridge: (<c>) ok`; expected values from the GL headers and the fixture's literals, never from the bridge):
//! a: the guest's import of glVertexAttrib4f equals eglGetProcAddress's answer, and the answer's call reaches the recorder with its fp arguments;
//! f: the same for glTexImage2D (nine arguments, the ninth on the stack, the pixel pointer the fixture's own);
//! b: eglGetProcAddress("glFinish"), a row the fixture never imports, answers a callable address that reaches the recorder;
//! d: a name with no row and no host answer answers NULL;
//! e: two answers for eglQueryDevicesEXT (a prototype the table describes, no row: the dedup path) are equal, and the answer's call returns EGL_TRUE from the recorder.
//! c (trap run, its own process): eglGetProcAddress("glAtlNoDescriptor") answers a named trap; calling it aborts, naming the function (the harness checks the abort and its stderr line).
//! Breaks (`ATL_NB_BREAK`, build.rs `BREAKS`): `egl_raw` -> a and f red; `egl_nodedup` -> e red.
//! Output: `egl-bridge: (<c>) ok` per check, `egl-bridge: (<c>) FAIL: <why>` otherwise; exit 0 when all pass.
#![allow(
    clippy::print_stdout,
    clippy::print_stderr,
    reason = "a harness-less gate: its verdict lines are its output"
)]

mod common;

use std::ffi::{CStr, CString, c_void};
use std::process::ExitCode;

use atl_native_bridge::engine::Guest;
use common::{RTLD_NOW, need};

const LIB: &CStr = c"libegl-fixture.so";
const PREFIX: &str = "egl-bridge";
/// GL 2.0 header literals of the calls the fixture makes (gl2.h, gl2ext.h).
const GL_TEXTURE_2D: u64 = 0x0DE1;
const GL_RGBA: u64 = 0x1908;
const GL_UNSIGNED_BYTE: u64 = 0x1401;
/// The recorder's glFinish mark word (`examples/egl_recorder.rs` FINISH_MARK).
const FINISH_MARK: u64 = 0x0f1;
/// EGL 1.5 egl.h `EGL_TRUE`: eglQueryDevicesEXT's result (the recorder answers it).
const EGL_TRUE: u32 = 1;
/// The recorder's last-call words the gate reads at most.
const WORDS: usize = 16;

/// The guest's fixture symbols, through its own dlsym.
struct Fx<'a> {
    g: &'a mut Guest,
    dlsym: usize,
    h: usize,
}

impl Fx<'_> {
    fn sym(&mut self, name: &str) -> Result<usize, String> {
        let c = CString::new(name).map_err(|e| e.to_string())?;
        // SAFETY: __loader_dlsym(void*, const char*, const void* caller).
        let f = unsafe {
            self.g
                .call(self.dlsym, self.h, c.as_ptr() as usize, self.g.exe_entry)
        };
        if f == 0 {
            return Err(format!("guest dlsym({name}) = NULL"));
        }
        Ok(f)
    }
    /// Fixture function `name()`, called with no arguments.
    fn step(&mut self, name: &str) -> Result<usize, String> {
        let f = self.sym(name)?;
        // SAFETY: a fixture function taking no arguments.
        Ok(unsafe { self.g.call(f, 0, 0, 0) })
    }
    /// Fixture function `name(arg)`: `0` records the answers only, `1` also calls the answer (the comparison runs between the two).
    fn call_with(&mut self, name: &str, arg: usize) -> Result<usize, String> {
        let f = self.sym(name)?;
        // SAFETY: a fixture function of one integer argument.
        Ok(unsafe { self.g.call(f, arg, 0, 0) })
    }
    /// Fixture global `name`, a word (identity: guest memory is ours).
    fn word(&mut self, name: &str) -> Result<usize, String> {
        let p = self.sym(name)?;
        // SAFETY: a fixture `uintptr_t` global.
        Ok(unsafe { *(p as *const usize) })
    }
    /// Fixture global `name`, a `uint32_t`.
    fn u32_global(&mut self, name: &str) -> Result<u32, String> {
        let p = self.sym(name)?;
        // SAFETY: a fixture `uint32_t` global.
        Ok(unsafe { std::ptr::read_volatile(p as *const u32) })
    }
}

/// The recorder's exports, through the copy the bridge loaded (same path, RTLD_NOLOAD).
struct Rec {
    h: *mut c_void,
}

impl Rec {
    fn open() -> Result<Rec, String> {
        let path = CString::new(format!("{}/libGLESv2.so", atl_android_paths::SYSTEM_LIB))
            .map_err(|e| e.to_string())?;
        // SAFETY: dlopen by path, RTLD_NOLOAD: no load, no constructors.
        let h = unsafe { libc::dlopen(path.as_ptr(), libc::RTLD_NOW | libc::RTLD_NOLOAD) };
        if h.is_null() {
            return Err(format!(
                "{path:?} is not loaded: the bridge did not bind the host library"
            ));
        }
        Ok(Rec { h })
    }
    /// The words of the recorder's last call.
    fn last(&self) -> Result<Vec<u64>, String> {
        // SAFETY: dlsym on a live handle with a NUL-terminated name.
        let p = unsafe { libc::dlsym(self.h, c"rec_take".as_ptr()) };
        if p.is_null() {
            return Err("recorder has no rec_take: not the recorder".to_owned());
        }
        let mut words = [0u64; WORDS];
        // SAFETY: recorder `rec_take(u64 *out, size_t cap) -> size_t`; `words` holds `WORDS` entries.
        let n = unsafe {
            std::mem::transmute::<*mut c_void, unsafe extern "C" fn(*mut u64, usize) -> usize>(p)(
                words.as_mut_ptr(),
                WORDS,
            )
        };
        if n > WORDS {
            return Err(format!(
                "the recorder's last call has {n} words, the gate reads {WORDS}"
            ));
        }
        Ok(words[..n].to_vec())
    }
}

/// One check's verdict line; false when red.
fn report(c: char, r: Result<String, String>) -> bool {
    match r {
        Ok(what) => {
            println!("{PREFIX}: ({c}) ok: {what}");
            println!("{PREFIX}: ({c}) ok");
            true
        }
        Err(e) => {
            println!("{PREFIX}: ({c}) FAIL: {e}");
            false
        }
    }
}

fn check_a(fx: &mut Fx, rec: &Rec) -> Result<String, String> {
    fx.call_with("qe_a", 0)?;
    let import = fx.word("qe_a_import")?;
    let answer = fx.word("qe_a_answer")?;
    if import == 0 {
        return Err("the guest's import of glVertexAttrib4f is NULL".to_owned());
    }
    if answer != import {
        return Err(format!(
            "eglGetProcAddress(glVertexAttrib4f) = {answer:#x}, the guest's own import = {import:#x}"
        ));
    }
    fx.call_with("qe_a", 1)?;
    let want = [
        5,
        u64::from(2.0f32.to_bits()),
        u64::from((-3.0f32).to_bits()),
        u64::from(4.5f32.to_bits()),
        u64::from((-5.25f32).to_bits()),
    ];
    let got = rec.last()?;
    if got != want {
        return Err(format!("the recorder saw {got:x?}, want {want:x?}"));
    }
    Ok(format!("answer {answer:#x} = import"))
}

fn check_f(fx: &mut Fx, rec: &Rec) -> Result<String, String> {
    fx.call_with("qe_f", 0)?;
    let import = fx.word("qe_f_import")?;
    let answer = fx.word("qe_f_answer")?;
    if import == 0 {
        return Err("the guest's import of glTexImage2D is NULL".to_owned());
    }
    if answer != import {
        return Err(format!(
            "eglGetProcAddress(glTexImage2D) = {answer:#x}, the guest's own import = {import:#x}"
        ));
    }
    fx.call_with("qe_f", 1)?;
    let pixels = fx.sym("qe_pixels")?;
    let want = [
        GL_TEXTURE_2D,
        2,
        GL_RGBA,
        64,
        32,
        0,
        GL_RGBA,
        GL_UNSIGNED_BYTE,
        pixels as u64,
    ];
    let got = rec.last()?;
    if got != want {
        return Err(format!("the recorder saw {got:x?}, want {want:x?}"));
    }
    Ok(format!("answer {answer:#x} = import"))
}

fn check_b(fx: &mut Fx, rec: &Rec) -> Result<String, String> {
    fx.call_with("qe_b", 0)?;
    let answer = fx.word("qe_b_answer")?;
    if answer == 0 {
        return Err("eglGetProcAddress(glFinish) = NULL, a row holds it".to_owned());
    }
    // the guest's own resolution of glFinish (dlsym on the fixture, whose DT_NEEDED proxy exports it): the row's exported entry
    let resolved = fx.sym("glFinish")?;
    if answer != resolved {
        return Err(format!(
            "eglGetProcAddress(glFinish) = {answer:#x}, the guest's own resolution = {resolved:#x}"
        ));
    }
    fx.call_with("qe_b", 1)?;
    let got = rec.last()?;
    if got != [FINISH_MARK] {
        return Err(format!(
            "the recorder saw {got:x?}, want [{FINISH_MARK:#x}]"
        ));
    }
    Ok(format!("answer {answer:#x}"))
}

fn check_d(fx: &mut Fx) -> Result<String, String> {
    let answer = fx.step("qe_d")?;
    if answer != 0 {
        return Err(format!(
            "eglGetProcAddress(glNoSuchFunctionAtl) = {answer:#x}, want NULL"
        ));
    }
    Ok("NULL".to_owned())
}

fn check_e(fx: &mut Fx) -> Result<String, String> {
    fx.step("qe_e")?;
    let first = fx.word("qe_e_first")?;
    let second = fx.word("qe_e_second")?;
    if first == 0 {
        return Err(
            "eglGetProcAddress(eglQueryDevicesEXT) = NULL, the host describes it".to_owned(),
        );
    }
    if first != second {
        return Err(format!(
            "two answers for one name: {first:#x} and {second:#x} (one binding per host address)"
        ));
    }
    let result = fx.u32_global("qe_e_result")?;
    if result != EGL_TRUE {
        return Err(format!(
            "eglQueryDevicesEXT through the answer returned {result:#x}, want EGL_TRUE {EGL_TRUE:#x}"
        ));
    }
    Ok(format!("answer {first:#x} twice"))
}

/// The plain run: checks a, f, b, d, e.
fn plain(g: &mut Guest, dlsym: usize, h: usize) -> Result<bool, String> {
    let rec = Rec::open()?;
    let mut fx = Fx { g, dlsym, h };
    let mut ok = true;
    ok &= report('a', check_a(&mut fx, &rec));
    ok &= report('f', check_f(&mut fx, &rec));
    ok &= report('b', check_b(&mut fx, &rec));
    ok &= report('d', check_d(&mut fx));
    ok &= report('e', check_e(&mut fx));
    Ok(ok)
}

/// The guest started with the fixture loaded: the libraries the fixture needs are its own proxies, the recorder the host side.
fn start(env: &common::Nb) -> Result<(Guest, usize, usize), String> {
    let mut env2 = common::Nb {
        nb: env.nb.clone(),
        libcxx: env.libcxx.clone(),
        testlib: env.testlib.clone(),
    };
    env2.testlib
        .push(env.nb.join("libnative_bridge_guest_libEGL"));
    env2.testlib
        .push(env.nb.join("libnative_bridge_guest_libGLESv2"));
    let mut g = common::start(&env2)?;
    let dlopen = need(&g.linker, "__dl___loader_dlopen")?;
    let dlsym = need(&g.linker, "__dl___loader_dlsym")?;
    // SAFETY: __loader_dlopen(const char*, int, const void* caller); the main executable's namespace searches ATL_NB_TESTLIB.
    let h = unsafe { g.call(dlopen, LIB.as_ptr() as usize, RTLD_NOW, g.exe_entry) };
    if h == 0 {
        return Err(format!(
            "guest dlopen({LIB:?}) = NULL: {}",
            common::dlerror(&mut g)
        ));
    }
    Ok((g, dlsym, h))
}

fn run(env: &common::Nb) -> Result<(), String> {
    let (mut g, dlsym, h) = start(env)?;
    if plain(&mut g, dlsym, h)? {
        Ok(())
    } else {
        Err("a check is red (see the lines above)".to_owned())
    }
}

/// The trap run: calls the named trap; a bridge that returns from the call did not trap.
fn trap(env: &common::Nb) -> Result<(), String> {
    let (mut g, dlsym, h) = start(env)?;
    let mut fx = Fx {
        g: &mut g,
        dlsym,
        h,
    };
    fx.step("qe_c")?;
    Err("the named trap for glAtlNoDescriptor returned: it did not abort".to_owned())
}

fn main() -> ExitCode {
    let Some(env) = common::nb_env().filter(|e| !e.testlib.is_empty()) else {
        println!("{PREFIX}: SKIP (ATL_NB / ATL_NB_LIBCXX / ATL_NB_TESTLIB unset)");
        return ExitCode::SUCCESS;
    };
    if std::env::var("ATL_EGL_CASE").is_ok_and(|c| c == "trap") {
        // reached only if the trap did not abort: the harness reads the line and the nonzero exit
        if let Err(e) = trap(&env) {
            println!("{PREFIX}: (c) FAIL: {e}");
        }
        return ExitCode::FAILURE;
    }
    match run(&env) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            println!("{PREFIX}: FAIL: {e}");
            ExitCode::FAILURE
        }
    }
}
