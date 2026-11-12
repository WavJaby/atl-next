// SPDX-License-Identifier: MIT
//! Gate `sles-bridge` (opensl-es plan slice O1): an NDK x86_64 fixture (`xtask-harness/testdata/sles-bridge/fixture.c`, `libsles-fixture.so`, DT_NEEDED libOpenSLES.so) calls OpenSL ES through the guest proxy `libOpenSLES.so` and the identity engine into the host recorder (`examples/sles_recorder.rs`), which must be the process's `/system/lib64/libOpenSLES.so` (the harness's bwrap root; `xtask test sles-bridge`).
//! Env: tests/common/mod.rs plus `ATL_NB_TESTLIB` (fixture dir + NDK sysroot lib dir); SKIP without them. The guest proxy's dir `<ATL_NB>/libnative_bridge_guest_libOpenSLES` joins the guest search path here.
//! Checks (line `sles-bridge: (<c>) ok`; expected values from fixture.c and OpenSLES.h, never from the bridge):
//! a: `slCreateEngine` + Realize = SL_RESULT_SUCCESS, the recorder's first engine Realize saw its own engine object as arg 0, and no recorder call in the whole run had a foreign arg 0;
//! b: `GetInterface(SL_IID_ENGINE)`'s out-param is no recorder interface (a guest proxy);
//! c: `CreateAudioPlayer`'s sink `outputMix` reached the recorder as its own output mix;
//! d: the recorder's pthread fired `sles_callbacks` callbacks; the guest saw its queue proxy as `caller` and its context each time, and re-entered `Enqueue` from each (recorder: `sles_callbacks` re-entered, no underrun); no ownerless proxy was made (engine/sles.rs `ownerless`: the caller is the queue the fixture got from GetInterface);
//! e: `sles_players` players registering one Play callback add exactly 1 down shell;
//! f: the guest's `SL_IID_PLAY` is the recorder's `SL_IID_PLAY` value;
//! g: `GetInterface` of the fixture's unknown IID = SL_RESULT_FEATURE_UNSUPPORTED (0x0C) and the refusal counter `sles.GetInterface.<its 16 bytes in hex>` is 1.
//! Breaks (`ATL_NB_BREAK`): `sles_raw_sink` -> c red; `sles_no_dedup` -> e red; `sles_var_skip` -> f red.
#![allow(
    clippy::print_stdout,
    clippy::print_stderr,
    reason = "a harness-less gate: its verdict lines are its output"
)]

mod common;

use std::ffi::{CStr, CString, c_char, c_void};
use std::process::ExitCode;

use atl_native_bridge::engine::{self, Guest, sles};
use common::{RTLD_NOW, need};

const LIB: &CStr = c"libsles-fixture.so";
/// OpenSLES.h `SL_RESULT_FEATURE_UNSUPPORTED`.
const FEATURE_UNSUPPORTED: usize = 0x0C;
/// fixture.c `unknown_iid` {0x0badc0de, 0x0001, 0x0002, 0x0003, {1..6}} as its little-endian bytes in hex.
const UNKNOWN_REFUSAL: &str = "sles.GetInterface.dec0ad0b010002000300010203040506";

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
    /// Fixture step `name()`: its SLresult (or pointer) result.
    fn step(&mut self, name: &str) -> Result<usize, String> {
        let f = self.sym(name)?;
        // SAFETY: a fixture function taking no arguments.
        Ok(unsafe { self.g.call(f, 0, 0, 0) })
    }
    /// Fixture global `name` (a word; identity: guest memory is ours).
    fn word(&mut self, name: &str) -> Result<usize, String> {
        let p = self.sym(name)?;
        // SAFETY: a fixture pointer global.
        Ok(unsafe { *(p as *const usize) })
    }
    fn int(&mut self, name: &str) -> Result<i32, String> {
        let p = self.sym(name)?;
        // SAFETY: a fixture `int` global.
        Ok(unsafe { std::ptr::read_volatile(p as *const i32) })
    }
}

/// The recorder's exports, through the copy the bridge loaded (same path, RTLD_NOLOAD).
struct Rec {
    h: *mut c_void,
}

impl Rec {
    fn open() -> Result<Rec, String> {
        let path = CString::new(format!("{}/libOpenSLES.so", atl_android_paths::SYSTEM_LIB))
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
    fn sym(&self, name: &CStr) -> Result<usize, String> {
        // SAFETY: dlsym on a live handle.
        let p = unsafe { libc::dlsym(self.h, name.as_ptr()) };
        if p.is_null() {
            return Err(format!("recorder has no {name:?}: not the recorder"));
        }
        Ok(p as usize)
    }
    fn get(&self, what: &CStr) -> Result<u64, String> {
        let f = self.sym(c"rec_get")?;
        // SAFETY: recorder `rec_get(const char*) -> u64`.
        Ok(unsafe {
            std::mem::transmute::<usize, unsafe extern "C" fn(*const c_char) -> u64>(f)(
                what.as_ptr(),
            )
        })
    }
    fn is_own(&self, itf: usize) -> Result<bool, String> {
        let f = self.sym(c"rec_is_own")?;
        // SAFETY: recorder `rec_is_own(const void*) -> u32`.
        Ok(
            unsafe { std::mem::transmute::<usize, unsafe extern "C" fn(usize) -> u32>(f)(itf) }
                != 0,
        )
    }
    fn call0(&self, name: &CStr) -> Result<(), String> {
        let f = self.sym(name)?;
        // SAFETY: recorder `void name(void)`.
        unsafe { std::mem::transmute::<usize, unsafe extern "C" fn()>(f)() };
        Ok(())
    }
    fn set_callbacks(&self, n: u32) -> Result<(), String> {
        let f = self.sym(c"rec_set_callbacks")?;
        // SAFETY: recorder `rec_set_callbacks(u32)`.
        unsafe { std::mem::transmute::<usize, unsafe extern "C" fn(u32)>(f)(n) };
        Ok(())
    }
}

/// One check's verdict line; false when red.
fn report(c: char, r: Result<String, String>) -> bool {
    match r {
        Ok(what) => {
            println!("sles-bridge: ({c}) ok: {what}");
            println!("sles-bridge: ({c}) ok");
            true
        }
        Err(e) => {
            println!("sles-bridge: ({c}) FAIL: {e}");
            false
        }
    }
}

fn sl_ok(step: &str, rc: usize) -> Result<(), String> {
    match rc as u32 {
        0 => Ok(()),
        r => Err(format!("{step} = {r:#x}, want SL_RESULT_SUCCESS")),
    }
}

fn run(env: &common::Nb) -> Result<(), String> {
    let mut env2 = common::Nb {
        nb: env.nb.clone(),
        libcxx: env.libcxx.clone(),
        testlib: env.testlib.clone(),
    };
    env2.testlib
        .push(env.nb.join("libnative_bridge_guest_libOpenSLES"));
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
    let mut fx = Fx {
        g: &mut g,
        dlsym,
        h,
    };
    let mut red = Vec::new();
    macro_rules! check {
        ($c:expr, $r:expr $(,)?) => {
            if !report($c, $r) {
                red.push($c);
            }
        };
    }

    // a: engine
    let rc = fx.step("sles_a_engine")?;
    let rec = Rec::open()?;
    let a = sl_ok("slCreateEngine + Realize", rc).and_then(|()| {
        let (engine, arg0) = (rec.get(c"engine")?, rec.get(c"realize_engine_arg0")?);
        if engine == 0 || arg0 != engine {
            return Err(format!(
                "recorder engine {engine:#x}, Realize arg 0 {arg0:#x}"
            ));
        }
        Ok(format!("Realize arg 0 = recorder engine {engine:#x}"))
    });
    // reported at the end, with the whole run's foreign arg 0 count
    let a = match a {
        Ok(what) => what,
        Err(e) => {
            check!('a', Err(e));
            return Err("check a red; later checks need the engine".into());
        }
    };

    // f: the IID variable row
    let iid = fx.step("sles_f_iid_play")?;
    let rec_iid = rec.sym(c"SL_IID_PLAY")?;
    // SAFETY: the recorder's `SL_IID_PLAY` is a pointer-sized static.
    let rec_iid = unsafe { *(rec_iid as *const usize) };
    check!(
        'f',
        if iid != 0 && iid == rec_iid {
            Ok(format!("guest SL_IID_PLAY = recorder's {rec_iid:#x}"))
        } else {
            Err(format!(
                "guest SL_IID_PLAY = {iid:#x}, recorder's {rec_iid:#x}"
            ))
        },
    );

    // g: unknown IID refused and counted
    let rc = fx.step("sles_g_unknown")? as u32 as usize;
    let count = sles::refusals()
        .into_iter()
        .find(|(k, _)| k == UNKNOWN_REFUSAL)
        .map_or(0, |(_, n)| n);
    check!(
        'g',
        if rc == FEATURE_UNSUPPORTED && count == 1 {
            Ok(format!("{rc:#x}, {UNKNOWN_REFUSAL} = 1"))
        } else {
            Err(format!(
                "GetInterface = {rc:#x} (want {FEATURE_UNSUPPORTED:#x}), {UNKNOWN_REFUSAL} = {count} (want 1)"
            ))
        },
    );

    // b: Engine interface is a proxy
    let rc = fx.step("sles_b_interface")?;
    let b = sl_ok("GetInterface(SL_IID_ENGINE)", rc).and_then(|()| {
        let itf = fx.word("sles_engine")?;
        if itf == 0 || rec.is_own(itf)? {
            return Err(format!(
                "out-param {itf:#x} is a recorder interface (or null)"
            ));
        }
        Ok(format!("out-param {itf:#x} is no recorder interface"))
    });
    let has_engine = b.is_ok();
    check!('b', b);
    if !has_engine {
        return Err(format!("checks {red:?} red; c-e need the Engine interface"));
    }

    // c: the sink's output mix
    let rc = fx.step("sles_c_player")?;
    let c = sl_ok("CreateOutputMix + CreateAudioPlayer + GetInterface", rc).and_then(|()| {
        let (mix, sink) = (rec.get(c"mix")?, rec.get(c"sink_mix")?);
        if mix == 0 || sink != mix {
            return Err(format!(
                "sink outputMix {sink:#x}, recorder's output mix {mix:#x}"
            ));
        }
        Ok(format!("sink outputMix = recorder's output mix {mix:#x}"))
    });
    let player = rc as u32 == 0;
    check!('c', c);

    if player {
        // d: callbacks from a host pthread
        let n = fx.int("sles_callbacks")?;
        rec.set_callbacks(u32::try_from(n).map_err(|e| e.to_string())?)?;
        let rc = fx.step("sles_d_start")?;
        rec.call0(c"rec_join")?;
        let d = sl_ok("RegisterCallback + Enqueue + SetPlayState", rc).and_then(|()| {
            let seen = (
                fx.int("sles_cb_count")?,
                fx.int("sles_cb_bad_caller")?,
                fx.int("sles_cb_bad_ctx")?,
                fx.int("sles_cb_bad_enqueue")?,
            );
            let host = (rec.get(c"fired")?, rec.get(c"reentered")?, rec.get(c"underruns")?);
            let n64 = u64::try_from(n).map_err(|e| e.to_string())?;
            if seen != (n, 0, 0, 0) || host != (n64, n64, 0) {
                return Err(format!(
                    "guest (callbacks, bad caller, bad context, bad enqueue) = {seen:?}, want ({n}, 0, 0, 0); recorder (fired, re-entered, underruns) = {host:?}, want ({n}, {n}, 0)"
                ));
            }
            // the caller is the queue the fixture got from GetInterface, so its proxy is owned: none ownerless
            let ownerless = sles::ownerless();
            if ownerless != 0 {
                return Err(format!("{ownerless} ownerless proxies after the callbacks, want 0"));
            }
            Ok(format!("{n} callbacks on the recorder thread, caller + context intact, {n} Enqueue re-entries, 0 ownerless proxies"))
        });
        check!('d', d);

        // e: one shell for one callback
        let players = fx.int("sles_players")?;
        let before = engine::active().down_shells();
        let rc = fx.step("sles_e_players")?;
        let added = engine::active().down_shells() - before;
        let e = sl_ok("50 x CreateAudioPlayer + RegisterCallback", rc).and_then(|()| {
            if added != 1 {
                return Err(format!(
                    "{players} players with one callback added {added} down shells, want 1"
                ));
            }
            Ok(format!("{players} players, 1 down shell"))
        });
        check!('e', e);
    } else {
        check!('d', Err("no player (step c failed)".into()));
        check!('e', Err("no player (step c failed)".into()));
    }

    // a, second half: every recorder call saw one of its own interfaces as arg 0
    let foreign = rec.get(c"foreign")?;
    let calls = rec.get(c"calls")?;
    check!(
        'a',
        if foreign == 0 {
            Ok(format!("{a}; all {calls} recorder calls had an own arg 0"))
        } else {
            Err(format!(
                "{foreign} of {calls} recorder calls had a foreign arg 0"
            ))
        }
    );
    if red.is_empty() {
        println!("sles-bridge: ok ({calls} recorder calls, all arg 0 own)");
        Ok(())
    } else {
        Err(format!("checks {red:?} red"))
    }
}

fn main() -> ExitCode {
    let stage = match common::nb_env() {
        Some(env) if !env.testlib.is_empty() => Some(run(&env)),
        _ => {
            println!("sles-bridge: SKIP (ATL_NB / ATL_NB_LIBCXX / ATL_NB_TESTLIB unset)");
            None
        }
    };
    let mut v = common::Verdict::new("sles-bridge");
    v.stage(stage);
    v.finish()
}
