// SPDX-License-Identifier: MIT
//! Q6-1 gate, trap side (QEMU bridge plan Q6-1), harness-less: a guest host call (`svc #CALL`) delivers its integer lanes, FP lanes and eight stack words to the host, and the host's FP answer comes back in the guest's v0. The fixture (`xtask/testdata/qemu-abi/fixture.c`, static) raises BOOT with `qa_run`; the host calls `qa_run`, which returns the bits of v0 after the call.
//! Env (xtask qemu_abi.rs sets it; unset prints SKIP): ATL_ENGINE_SO (the engine `.so`), ATL_QEMU_ABI_FIXTURE (fixture.c, built static).
//! Expected values are the fixture's literals, written out here again (never read from the engine): x0..x7 = 0x101 + i; v0..v7 = 1.5 + i, as the bits `f64::to_bits` gives (an independent calculation of the binary64 pattern); stack words 0 and 1 = the fixture's s0 and s1; the answer 3.25 (not any input lane), so `qa_run` returns `3.25f64.to_bits()`.
//! Checks: (s) started (BOOT delivered, the thread attached); (x) integer lanes; (v) FP lanes; (k) stack words; (r) the value `qa_run` returned, the host's FP answer.
//! Output: `qemu-abi: (x) ok` per check, `(x) FAIL <why>` otherwise, exit 0 when all pass.
#![allow(clippy::print_stdout, reason = "test harness output is its verdict")]

use std::path::Path;
use std::process::ExitCode;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use atl_engine_api::AtlLanesV1;
use atl_native_bridge::engine::{self, qemu};

/// fixture.c: x0..x7 = 0x101 + i.
const X_EXPECT: [u64; 8] = [0x101, 0x102, 0x103, 0x104, 0x105, 0x106, 0x107, 0x108];
/// fixture.c: v0..v7 = 1.5 + i (decimal literals; the bits are compared).
const V_EXPECT: [f64; 8] = [1.5, 2.5, 3.5, 4.5, 5.5, 6.5, 7.5, 8.5];
/// fixture.c: the stack words s0 and s1 at sp.
const K_EXPECT: [u64; 2] = [0x0123_4567_89ab_cdef, 0xfedc_ba98_7654_3210];
/// The host's FP answer: v0 after the call, which `qa_run` returns as bits.
const ANSWER: f64 = 3.25;

/// BOOT's payload: `qa_run`'s address, the entry the host calls.
static RUN: AtomicU64 = AtomicU64::new(0);
/// The lanes of the gate's host call, as the host received them.
static LAST: Mutex<Option<AtlLanesV1>> = Mutex::new(None);

fn check(letter: char, result: Result<(), String>) -> bool {
    match result {
        Ok(()) => {
            println!("qemu-abi: ({letter}) ok");
            true
        }
        Err(why) => {
            println!("qemu-abi: ({letter}) FAIL {why}");
            false
        }
    }
}

fn same<T: PartialEq + std::fmt::Debug>(what: &str, got: T, want: T) -> Result<(), String> {
    if got == want {
        Ok(())
    } else {
        Err(format!("{what}: got {got:x?}, want {want:x?}"))
    }
}

/// The engine's trap callback for this gate. BOOT records `qa_run`; CALL records its lanes and answers v0 = the FP answer, x0 = 0; the rest is the production route.
fn on_trap(trap: qemu::Trap) -> qemu::Answer {
    match trap {
        qemu::Trap::Boot(run) => {
            RUN.store(run, Ordering::Relaxed);
            qemu::Answer::default()
        }
        qemu::Trap::Call(lanes) => {
            *LAST.lock().expect("last") = Some(lanes);
            qemu::Answer {
                x0: 0,
                v0: ANSWER.to_bits(),
            }
        }
        other => qemu::on_trap(other),
    }
}

/// Runs `qa_run` on the attached thread (its one host call records the lanes), then checks what the host saw and what came back. Returns true when every check passed.
fn checks(run: u64) -> bool {
    let returned = qemu::call(run, &[]);
    let lanes = LAST.lock().expect("last").take().unwrap_or_default();
    let mut ok = true;
    ok &= check('x', same("x0..x7", lanes.x, X_EXPECT));
    ok &= check(
        'v',
        same("v0..v7 bits", lanes.v, V_EXPECT.map(f64::to_bits)),
    );
    ok &= check(
        'k',
        same(
            "stack words 0..2",
            [lanes.stack[0], lanes.stack[1]],
            K_EXPECT,
        ),
    );
    ok &= check('r', same("qa_run returned", returned, ANSWER.to_bits()));
    ok
}

fn main() -> ExitCode {
    let (Some(so), Some(fixture)) = (
        std::env::var_os("ATL_ENGINE_SO"),
        std::env::var_os("ATL_QEMU_ABI_FIXTURE"),
    ) else {
        println!("qemu-abi: SKIP (ATL_ENGINE_SO / ATL_QEMU_ABI_FIXTURE unset)");
        return ExitCode::SUCCESS;
    };
    // the process's engine is QEMU before `boot` claims it
    if let Err(e) = engine::claim(Some(qemu::GUEST_ISA)) {
        println!("qemu-abi: (s) FAIL engine: {e}");
        return ExitCode::from(2);
    }
    // BOOT answers at once, so the engine is up when this returns
    if let Err(e) = qemu::boot(Path::new(&so), Path::new(&fixture), on_trap) {
        println!("qemu-abi: (s) FAIL boot: {e}");
        return ExitCode::from(2);
    }
    if let Err(e) = qemu::attach_current() {
        println!("qemu-abi: (s) FAIL attach: {e}");
        return ExitCode::from(2);
    }
    let run = RUN.load(Ordering::Relaxed);
    if !check(
        's',
        if run != 0 {
            Ok(())
        } else {
            Err("BOOT carried no entry".to_owned())
        },
    ) {
        return ExitCode::FAILURE;
    }
    if checks(run) {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
