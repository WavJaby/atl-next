// SPDX-License-Identifier: MIT
//! Q4-3 gate (QEMU bridge plan Q4-3), harness-less: the six NB entries are placed in place on the real arm64 `linker64` and vdso (`vdso::bind_linker`, `vdso::bind`), and their placed words are checked before QEMU starts. QEMU then runs the fixture. At BOOT the pool and both images are registered with QEMU; the fixture calls each placed site as its AOSP prototype says, and the host answers each binding trap and checks ids and arguments.
//! Env (xtask qemu_linker.rs sets it; unset prints SKIP): ATL_ENGINE_SO (the engine `.so`), ATL_QEMU_SPIKE_FIXTURE (testdata/qemu-linker/fixture.c, built static), ATL_NB_LINKER (arm64 linker64), ATL_NB_VDSO (arm64 libnative_bridge_vdso.so).
//! Expected values: the AOSP prototypes and the fixture's literals (fixture.c), never the engine. A placed id is read back from the svc immediate (ARM ARM: imm16 at bits 20:5) and must match the id of the trap that arrives.
//! Output: `qemu-linker: (x) ok` per check, `(x) FAIL <why>` otherwise. Checks: (p) placement, (r) registration, (t) trap ids and order, (a) arguments, (s) answers. Exit 0 when all pass.
#![allow(clippy::print_stdout, reason = "test harness output is its verdict")]
#![allow(
    unsafe_code,
    reason = "reads placed words and the guest's C strings at their identity addresses"
)]

use std::ffi::{CStr, c_char};
use std::path::Path;
use std::process::ExitCode;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use atl_native_bridge::engine::{active, image::Image, thread, vdso};

// ARM ARM encodings, written out here: atl-bridgegen's constants are the code under test.
/// `svc #0`: the base encoding, imm16 at bits 20:5.
const SVC_BASE: u32 = 0xD400_0001;
/// `ret`.
const RET_WORD: u32 = 0xD65F_03C0;
const IMM_SHIFT: u32 = 5;
const IMM_MASK: u32 = 0xFFFF;
/// Bit 15 of the svc immediate: a binding.
const BIND_FLAG: u32 = 0x8000;
/// `nop` (ARM ARM), the two words before the vdso's `call_guest` placeholder.
const NOP_WORD: u32 = 0xD503_201F;
/// Offset of the `call_guest` placeholder from its symbol: two `nop`s first (vdso_arm64.S:50-54).
const CALL_GUEST_PLACEHOLDER_OFFSET: usize = 8;

// The fixture's literals (fixture.c): what the host expects each site to receive.
const TRACE_FMT: &str = "qemu-linker trace %d %d\n";
const TRACE_A: u64 = 7;
const TRACE_B: u64 = 9;
const LIB_NAME: &str = "gate.library";
const SYM_NAME: &str = "gate.symbol";
const CALL_GUEST_ARGS: [u64; 3] = [0xc0de_0001, 0xc0de_0002, 0xc0de_0003];
const TLS_SIZE: usize = 0x1000;
const TLS_TPOFF: usize = 0x40;
const TLS_SLOT_THREAD_ID: i32 = 1;
const TLS_SLOT_BIONIC_TLS: i32 = 2;

// The host's own answers: the gate's choice. The intercepted stub returns STUB_ANSWER; post_init and call_guest are answered here, because production answers both with an abort (`unsupported`).
const STUB_ANSWER: u64 = 0x5717_0000_0000_0001;
const CALL_GUEST_ANSWER: u64 = 0x5ca1_1600_0000_0001;

/// Site order of fixture.c's `run_sites`: get_host_pthread, config_static_tls, trace, intercept_symbol, post_init, call_guest. The stub's trap comes between intercept_symbol and post_init.
const ORDER: [&str; 6] = [
    "get_host_pthread",
    "config_static_tls",
    "trace",
    "intercept_symbol",
    "post_init",
    "call_guest",
];
const POST_INIT: usize = 4;
const CALL_GUEST: usize = 5;

/// The placed state of the process: the mapped images, the six site addresses (fixture order), and the binding ids the placed words decode to.
struct Plan {
    linker: Image,
    vdso: Image,
    sites: [usize; 6],
    ids: [u32; 6],
}

static PLAN: OnceLock<Plan> = OnceLock::new();

/// The six site addresses `run_sites` reads through its argument, in fixture order. One page, mapped readable before the call: the engine's call passes four arguments, so the call gets this page's address.
#[repr(C, align(4096))]
struct SitePage([AtomicU64; 512]);
static SITES: SitePage = SitePage([const { AtomicU64::new(0) }; 512]);
/// BOOT's x0: the fixture's `run_sites`, recorded by the BOOT callback before the checks run.
static RUN_SITES: AtomicU64 = AtomicU64::new(0);
/// Every binding trap the guest raised: (id, x0..x7), in arrival order.
static TRAPS: Mutex<Vec<(u32, [u64; 8])>> = Mutex::new(Vec::new());
/// Every REPORT: R0 the stub's address, R1 get_host_pthread's answer, R2 the stub's answer, R3 call_guest's answer.
static REPORTS: Mutex<Vec<u64>> = Mutex::new(Vec::new());
/// The id the intercepted stub was bound to (decoded from its placed words).
static STUB_ID: OnceLock<u32> = OnceLock::new();

fn svc_imm(word: u32) -> u32 {
    (word >> IMM_SHIFT) & IMM_MASK
}

/// The two instruction words at `addr`.
fn words_at(addr: usize) -> [u32; 2] {
    // SAFETY: `addr` is an instruction site of a mapped image or the fixture's stub: 8 readable bytes.
    unsafe { (addr as *const [u32; 2]).read_unaligned() }
}

/// Whether `words` is a binding thunk `svc #(BIND | id); ret`, with `id` below BIND; returns the id.
fn thunk_id(words: [u32; 2]) -> Result<u32, String> {
    let imm = svc_imm(words[0]);
    let base = SVC_BASE & !(IMM_MASK << IMM_SHIFT);
    if words[0] & !(IMM_MASK << IMM_SHIFT) != base || imm & BIND_FLAG == 0 || words[1] != RET_WORD {
        return Err(format!("not a binding thunk: {words:08x?}"));
    }
    Ok(imm & !BIND_FLAG)
}

/// A C string the guest passed (identity address).
fn c_str(addr: u64) -> String {
    // SAFETY: the fixture's NUL-terminated literal, at its identity address.
    unsafe { CStr::from_ptr(addr as *const c_char) }
        .to_string_lossy()
        .into_owned()
}

fn check(letter: char, result: Result<(), String>) -> bool {
    match result {
        Ok(()) => {
            println!("qemu-linker: ({letter}) ok");
            true
        }
        Err(why) => {
            println!("qemu-linker: ({letter}) FAIL {why}");
            false
        }
    }
}

/// Places the six entries on the real images, then checks the placed words: each site holds one thunk, the ids are distinct, and `call_guest`'s site is the placeholder at +8 (vdso_arm64.S:50-54) with its two nops intact.
fn place(linker_path: &Path, vdso_path: &Path) -> Result<Plan, String> {
    let linker = Image::map(linker_path)?;
    let vdso = Image::map(vdso_path)?;
    vdso::bind_linker(&linker)?;
    vdso::bind(&vdso)?;
    linker.seal()?;
    vdso.seal()?;
    let symbol = |img: &Image, name: &str| img.symbol(name).ok_or(format!("no symbol {name}"));
    let call_guest_sym = symbol(&vdso, "native_bridge_call_guest")?;
    let sites = [
        symbol(&linker, "__dl___native_bridge_get_host_pthread")?,
        symbol(&linker, "__dl___native_bridge_config_static_tls")?,
        symbol(&vdso, "native_bridge_trace")?,
        symbol(&vdso, "native_bridge_intercept_symbol")?,
        symbol(&vdso, "native_bridge_post_init")?,
        call_guest_sym + CALL_GUEST_PLACEHOLDER_OFFSET,
    ];
    if words_at(call_guest_sym) != [NOP_WORD, NOP_WORD] {
        return Err("call_guest: the two nops before its placeholder changed".into());
    }
    let mut ids = [0u32; 6];
    for (k, &site) in sites.iter().enumerate() {
        ids[k] = thunk_id(words_at(site)).map_err(|e| format!("{}: {e}", ORDER[k]))?;
        if active::is_stub(site) {
            return Err(format!("{}: still a placeholder", ORDER[k]));
        }
    }
    let distinct = ids
        .iter()
        .enumerate()
        .all(|(i, a)| ids[..i].iter().all(|b| b != a));
    if !distinct {
        return Err(format!("site ids not distinct: {ids:?}"));
    }
    Ok(Plan {
        linker,
        vdso,
        sites,
        ids,
    })
}

extern "C" fn stub_answer(_: u64, _: u64, _: u64, _: u64, _: u64, _: u64, _: u64, _: u64) -> u64 {
    STUB_ANSWER
}

/// Binds the fixture's intercept stub (its address arrives as R0) to `stub_answer` and writes the thunk over it, the write `intercept_symbol` makes.
fn patch_stub(stub: usize) -> Result<(), String> {
    if !active::is_stub(stub) {
        return Err("the fixture's stub is not the 8-byte placeholder".into());
    }
    let thunk = active::bind(stub_answer as *const () as usize).ok_or("out of thunks")?;
    if !active::patch_site(stub, thunk) {
        return Err("patch_site refused the stub".into());
    }
    let id = thunk_id(words_at(stub))?;
    let _ = STUB_ID.set(id);
    Ok(())
}

/// The engine's trap callback for this gate. BOOT only records the fixture's `run_sites`: the engine answers BOOT at once, and [`checks`] runs on the attached thread after it.
fn on_trap(trap: active::Trap) -> u64 {
    match trap {
        active::Trap::Boot(run_sites) => {
            RUN_SITES.store(run_sites, Ordering::Relaxed);
            0
        }
        active::Trap::Report(v) => {
            let first = {
                let mut reports = REPORTS.lock().expect("reports");
                reports.push(v);
                reports.len() == 1
            };
            if first && let Err(e) = patch_stub(v as usize) {
                println!("qemu-linker: (s) FAIL stub: {e}");
            }
            0
        }
        active::Trap::Bind { id, args } => {
            TRAPS.lock().expect("traps").push((id, args));
            match PLAN.get() {
                Some(plan) if id == plan.ids[POST_INIT] => 0,
                Some(plan) if id == plan.ids[CALL_GUEST] => CALL_GUEST_ANSWER,
                _ => active::on_trap(trap),
            }
        }
        other => active::on_trap(other),
    }
}

/// Runs on the attached main thread after BOOT answered: registers the pool, the images and the site page with QEMU, runs the fixture's `run_sites` on the page, then checks what came back. Its answer is the exit status (0 = every check passed). Identity addresses (guest = host) are the engine's contract, not checked here.
fn checks(run_sites: u64) -> u64 {
    let Some(plan) = PLAN.get() else {
        println!("qemu-linker: (r) FAIL no plan");
        return 1;
    };
    let page = (&raw const SITES) as usize;
    let registered = active::register(&[&plan.linker, &plan.vdso])
        .and_then(|()| active::register_data(page, std::mem::size_of::<SitePage>()));
    if !check('r', registered) {
        return 1;
    }
    active::call(run_sites, &[page as u64]);
    u64::from(!verify(plan))
}

fn verify(plan: &Plan) -> bool {
    let traps = TRAPS.lock().expect("traps").clone();
    let reports = REPORTS.lock().expect("reports").clone();
    let stub_id = STUB_ID.get().copied();
    // fixture order: the six sites, with the stub's trap between intercept_symbol and post_init
    let expected: Vec<u32> = [0, 1, 2, 3]
        .map(|k| plan.ids[k])
        .into_iter()
        .chain(stub_id)
        .chain([plan.ids[POST_INIT], plan.ids[CALL_GUEST]])
        .collect();
    let got: Vec<u32> = traps.iter().map(|t| t.0).collect();
    let mut ok = check(
        't',
        if got == expected {
            Ok(())
        } else {
            Err(format!(
                "trap ids {got:?}, expected {expected:?} (fixture order)"
            ))
        },
    );
    let by_site = |k: usize| traps.iter().find(|t| t.0 == plan.ids[k]).map(|t| t.1);
    let args_ok = (|| -> Result<(), String> {
        let stub_addr = *reports.first().ok_or("no R0 report")?;
        let trace = by_site(2).ok_or("trace: no trap")?;
        if c_str(trace[0]) != TRACE_FMT || trace[1] != TRACE_A || trace[2] != TRACE_B {
            return Err(format!("trace args {trace:x?}"));
        }
        let intercept = by_site(3).ok_or("intercept_symbol: no trap")?;
        if intercept[0] != stub_addr
            || c_str(intercept[1]) != LIB_NAME
            || c_str(intercept[2]) != SYM_NAME
        {
            return Err(format!("intercept_symbol args {intercept:x?}"));
        }
        let call_guest = by_site(CALL_GUEST).ok_or("call_guest: no trap")?;
        if call_guest[..3] != CALL_GUEST_ARGS {
            return Err(format!("call_guest args {:x?}", &call_guest[..3]));
        }
        Ok(())
    })();
    ok &= check('a', args_ok);
    // get_host_pthread answers a record, config_static_tls leaves the static TLS the prototype describes
    let answers = (|| -> Result<(), String> {
        if reports.len() != 4 {
            return Err(format!("{} reports, expected 4", reports.len()));
        }
        if reports[1] == 0 {
            return Err("get_host_pthread answered 0".into());
        }
        if reports[2] != STUB_ANSWER || reports[3] != CALL_GUEST_ANSWER {
            return Err(format!("answers {:x?}", &reports[2..]));
        }
        let tls = thread::static_tls().ok_or("config_static_tls recorded nothing")?;
        if tls.size != TLS_SIZE
            || tls.tpoff != TLS_TPOFF
            || tls.slot_thread_id != TLS_SLOT_THREAD_ID
            || tls.slot_bionic_tls != TLS_SLOT_BIONIC_TLS
        {
            return Err(format!(
                "static TLS size {:#x} tpoff {:#x} slots {}/{}",
                tls.size, tls.tpoff, tls.slot_thread_id, tls.slot_bionic_tls
            ));
        }
        Ok(())
    })();
    ok &= check('s', answers);
    ok
}

fn main() -> ExitCode {
    let (Some(so), Some(fixture), Some(linker), Some(vdso)) = (
        std::env::var_os("ATL_ENGINE_SO"),
        std::env::var_os("ATL_QEMU_SPIKE_FIXTURE"),
        std::env::var_os("ATL_NB_LINKER"),
        std::env::var_os("ATL_NB_VDSO"),
    ) else {
        println!(
            "qemu-linker: SKIP (ATL_ENGINE_SO / ATL_QEMU_SPIKE_FIXTURE / ATL_NB_LINKER / ATL_NB_VDSO unset)"
        );
        return ExitCode::SUCCESS;
    };
    let plan = match place(Path::new(&linker), Path::new(&vdso)) {
        Ok(plan) => plan,
        Err(e) => {
            println!("qemu-linker: (p) FAIL {e}");
            return ExitCode::from(2);
        }
    };
    println!("qemu-linker: (p) ok");
    // the sites go to the page `run_sites` reads, mapped readable by `checks`
    for (k, &site) in plan.sites.iter().enumerate() {
        SITES.0[k].store(site as u64, Ordering::Relaxed);
    }
    if PLAN.set(plan).is_err() {
        println!("qemu-linker: FAIL plan set twice");
        return ExitCode::from(2);
    }
    // BOOT answers at once; the engine is then up, and the checks run on this thread
    if let Err(e) = active::boot(Path::new(&so), Path::new(&fixture), on_trap) {
        println!("qemu-linker: FAIL boot: {e}");
        return ExitCode::from(2);
    }
    if let Err(e) = active::attach_current() {
        println!("qemu-linker: FAIL attach: {e}");
        return ExitCode::from(2);
    }
    // 0 = every check passed
    ExitCode::from(u8::try_from(checks(RUN_SITES.load(Ordering::Relaxed))).unwrap_or(u8::MAX))
}
