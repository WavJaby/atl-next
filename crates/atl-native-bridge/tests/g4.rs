// SPDX-License-Identifier: MIT
//! Spike gate g4 (bridge-1 route N): crossing cost. RUNS runs x CALLS calls each of
//! - guest->host: a guest loop (`g4_guest_loop`, run on a stand-in guest TCB) calls an engine thunk (`arch::bind`) bound to an empty host function: shadow-stack push, FS switch both ways;
//! - host->guest: a host loop calls `arch::call_guest` on an empty guest function: FS switch both ways;
//! - baseline: the same guest loop calling the empty host function directly (loop + indirect call, no crossing), subtracted from guest->host.
//!
//! Reports median and min-max ns per call over the runs, and whether the CPU has FSGSBASE (without it the engine refuses to start).
//! Gate: median per crossing (minus baseline) < BAND_NS for both directions, and the host target ran exactly RUNS x CALLS times.
//! Starvation: the host's CPU stall share over the whole run (atl-cpu-pressure, the measure the cadence window is judged with); a band failure while it is over the threshold is `undecidable`, still a failure. A pass stands.
//! Deliberate break: `ATL_NB_BREAK=g4_break` adds SYSCALLS_IN_BREAK `getppid` syscalls to the crossed path only -> guest->host must leave the band.
//! Timing is wall clock on an uncontrolled host (WSL2 here): a band, not a measurement of record; run in release (`--release`) for numbers worth reporting.
#![allow(
    clippy::print_stdout,
    clippy::print_stderr,
    reason = "a harness-less gate: its verdict lines are its output"
)]

mod common;

use std::process::ExitCode;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use atl_cpu_pressure::{Sampler, judge_window};
use atl_native_bridge::engine::{HostThread, arch};

/// Plan bridge1-plan step 3 g4: target < 50 ns per crossing.
const BAND_NS: f64 = 50.0;
/// Plan: 10 runs x 1e7 calls.
const RUNS: usize = 10;
const CALLS: usize = 10_000_000;
/// Words of the stand-in guest TCB (as g1 stage 0).
const FAKE_TCB_WORDS: usize = 64;
/// The break's syscalls per host call: one syscall is ~50-100 ns on x86_64 with mitigations, so 4 put guest->host well past BAND_NS.
#[cfg(atl_g4_break)]
const SYSCALLS_IN_BREAK: usize = 4;

std::arch::global_asm!(
    ".text",
    // g4_guest_loop(n, f): calls f() n times; callee-saved only, no TLS
    ".globl g4_guest_loop",
    "g4_guest_loop:",
    "    push %rbx",
    "    push %r12",
    "    sub $8, %rsp",
    "    mov %rdi, %rbx",
    "    mov %rsi, %r12",
    "1:  call *%r12",
    "    dec %rbx",
    "    jnz 1b",
    "    add $8, %rsp",
    "    pop %r12",
    "    pop %rbx",
    "    ret",
    ".globl g4_guest_empty",
    "g4_guest_empty:",
    "    ret",
    options(att_syntax)
);

unsafe extern "C" {
    fn g4_guest_loop(n: usize, f: usize);
    fn g4_guest_empty();
}

static HOST_CALLS: AtomicU64 = AtomicU64::new(0);

extern "C" fn host_target() {
    HOST_CALLS.fetch_add(1, Ordering::Relaxed);
}

/// What the thunk reaches: host_target, plus the break's syscalls (only on the crossed path, so the baseline does not cancel them).
extern "C" fn crossed_target() {
    #[cfg(atl_g4_break)]
    for _ in 0..SYSCALLS_IN_BREAK {
        // SAFETY: getppid has no preconditions.
        unsafe { libc::syscall(libc::SYS_getppid) };
    }
    host_target();
}

/// ns per call of `f()` running `CALLS` calls, per run.
fn runs(mut f: impl FnMut()) -> Vec<f64> {
    (0..RUNS)
        .map(|_| {
            let t = Instant::now();
            f();
            t.elapsed().as_nanos() as f64 / CALLS as f64
        })
        .collect()
}

/// (median, min, max).
fn band(mut v: Vec<f64>) -> (f64, f64, f64) {
    v.sort_by(f64::total_cmp);
    let m = (v[RUNS / 2 - 1] + v[RUNS / 2]) / 2.0;
    (m, v[0], v[RUNS - 1])
}

fn run() -> Result<String, String> {
    if !arch::supported() {
        return Err("FSGSBASE: no (the engine cannot cross)".into());
    }
    let host = arch::fs_base();
    let tcb = Box::leak(vec![0usize; FAKE_TCB_WORDS].into_boxed_slice()).as_mut_ptr() as usize;
    let ht = HostThread::leak();
    // SAFETY: a leaked FAKE_TCB_WORDS-word stand-in TCB (bionic's: 11 words); the record is leaked.
    unsafe { arch::init_tcb(tcb, &raw mut *ht) };
    ht.set_guest_fs(tcb);
    let thunk = arch::bind(crossed_target as *const () as usize).ok_or("no free thunk")?;
    let target = host_target as *const () as usize;
    let lp = g4_guest_loop as *const () as usize;
    let empty = g4_guest_empty as *const () as usize;
    let sampler = Sampler::start();
    let base = band(runs(|| {
        // SAFETY: the guest loop touches no TLS; host_target, called on the guest FS, only does an atomic add.
        unsafe { arch::call_guest(ht, lp, CALLS, target, 0, 0) };
    }));
    HOST_CALLS.store(0, Ordering::Relaxed);
    let g2h = band(runs(|| {
        // SAFETY: as above; the thunk crosses back to crossed_target on the host FS.
        unsafe { arch::call_guest(ht, lp, CALLS, thunk, 0, 0) };
    }));
    let h2g = band(runs(|| {
        for _ in 0..CALLS {
            // SAFETY: the empty guest fn touches nothing.
            unsafe { arch::call_guest(ht, empty, 0, 0, 0, 0) };
        }
    }));
    let pressure = sampler.stop();
    if arch::fs_base() != host {
        // SAFETY: this thread's own thread pointer, read above.
        unsafe { arch::set_fs_base(host) };
        return Err("FS not host after the runs".into());
    }
    let calls = HOST_CALLS.load(Ordering::Relaxed);
    if calls != (RUNS * CALLS) as u64 {
        return Err(format!(
            "host target ran {calls} times, want {}",
            RUNS * CALLS
        ));
    }
    let g2h_net = g2h.0 - base.0;
    println!(
        "g4: FSGSBASE yes; {RUNS} runs x {CALLS}: baseline loop+call {:.2} ns ({:.2}-{:.2}); guest->host {:.2} ns ({:.2}-{:.2}), net {g2h_net:.2}; host->guest {:.2} ns ({:.2}-{:.2})",
        base.0, base.1, base.2, g2h.0, g2h.1, g2h.2, h2g.0, h2g.1, h2g.2
    );
    let band_judgement = if g2h_net >= BAND_NS || h2g.0 >= BAND_NS {
        Err(format!(
            "outside the band: guest->host net {g2h_net:.2} ns, host->guest {:.2} ns, band < {BAND_NS} ns",
            h2g.0
        ))
    } else {
        Ok(format!(
            "net {g2h_net:.2} ns, host->guest {:.2} ns, band < {BAND_NS} ns",
            h2g.0
        ))
    };
    judge_window(band_judgement, pressure.span_share(), "the run")
}

fn main() -> ExitCode {
    common::verdict("g4", [Some(run().map(|m| println!("g4: {m}")))])
}
