// SPDX-License-Identifier: MIT
//! Q5-8b gate (QEMU bridge plan section 5), harness-less: the crossing counter on the identity side (feature `crossing-count`; the counted shells of `atl-bridgegen` `x86::entries_asm`). A stand-in guest loop, run as g4 runs its own (stand-in guest TCB, no TLS), calls the host thunk pool and the JNI stub pool; each call crosses to a host function.
//! Expected values are the loop counts this file passes, never the counter's: each host function also counts its own calls, so the counter is checked against a record it does not write.
//! Checks: (s) started (FSGSBASE); (t) thunk crossings per host target exact, two slots bound to one target counted together; (j) JNI stub crossings per target and per JNI slot exact (slots 5 and 9, no other slot; the slot count is the shared `dispatch::count_jni` entry QEMU's route also takes); (w) 2^16 further crossings dump exactly once; (f) FS on the host after the calls.
//! Output: `crossing: (x) ok` or `crossing: (x) FAIL <why>` per check; exit 0 when all pass. Break `ATL_NB_BREAK=id_count`: the shells are generated plain, so the counter sees nothing and (t), (j) and (w) must fail.
#![allow(
    clippy::print_stdout,
    reason = "a harness-less gate: its verdict lines are its output"
)]

use std::process::ExitCode;
use std::sync::atomic::{AtomicU64, Ordering};

use atl_native_bridge::counters::{self, Snapshot};
use atl_native_bridge::engine::{Engine, HostThread, Tp, identity};

/// Thunk calls per host target: `A` and `B` one slot each, `D` bound to two slots (its count sums by target).
const CALLS_A: u64 = 40_000;
const CALLS_B: u64 = 25_000;
const CALLS_D: [u64; 2] = [9_000, 6_000];
/// JNI calls per host target: `E` at JNI slot 5, `F` at slot 9.
const JNI_E: usize = 5;
const JNI_F: usize = 9;
const CALLS_E: u64 = 3_000;
const CALLS_F: u64 = 1_000;
/// Crossings in one 2^16 window: the 16-bit tick wraps exactly once in it (counters.rs).
const WINDOW: u64 = 1 << 16;
/// Words of the stand-in guest TCB (as g4).
const FAKE_TCB_WORDS: usize = 64;

std::arch::global_asm!(
    ".text",
    // id_guest_loop(n, f, a): f(a) n times, a in %rdi; callee-saved only, no TLS. Three pushes leave rsp 16-aligned at each call.
    ".globl id_guest_loop",
    "id_guest_loop:",
    "    push %rbx",
    "    push %r12",
    "    push %r13",
    "    mov %rdi, %rbx",
    "    mov %rsi, %r12",
    "    mov %rdx, %r13",
    "1:  mov %r13, %rdi",
    "    call *%r12",
    "    dec %rbx",
    "    jnz 1b",
    "    pop %r13",
    "    pop %r12",
    "    pop %rbx",
    "    ret",
    options(att_syntax)
);

unsafe extern "C" {
    fn id_guest_loop(n: usize, f: usize, a: usize);
}

/// A host target: counts its own calls, the record the counter is checked against. Takes one word and ignores it (a thunk gets the guest's %rdi, a JNI stub the host object).
macro_rules! host {
    ($name:ident, $calls:ident) => {
        static $calls: AtomicU64 = AtomicU64::new(0);
        extern "C" fn $name(_: usize) {
            $calls.fetch_add(1, Ordering::Relaxed);
        }
    };
}
host!(host_a, OWN_A);
host!(host_b, OWN_B);
host!(host_d, OWN_D);
host!(host_e, OWN_E);
host!(host_f, OWN_F);

fn addr(f: extern "C" fn(usize)) -> usize {
    f as *const () as usize
}

/// Crossings each target took between two snapshots, for every target in `after`.
fn delta(before: &Snapshot, after: &Snapshot) -> Vec<(usize, u64)> {
    after
        .targets
        .iter()
        .map(|&(t, c)| {
            let b = before
                .targets
                .iter()
                .find(|(bt, _)| *bt == t)
                .map_or(0, |&(_, bc)| bc);
            (t, c.saturating_sub(b))
        })
        .collect()
}

/// Each target in `want` took exactly its count, and no other target took any.
fn check_counts(d: &[(usize, u64)], want: &[(usize, u64)]) -> Result<(), String> {
    for &(t, n) in want {
        let got = d.iter().find(|&&(x, _)| x == t).map_or(0, |&(_, c)| c);
        if got != n {
            return Err(format!(
                "host target {t:#x} took {got} crossings, the loop makes {n}"
            ));
        }
    }
    for &(t, c) in d {
        if c != 0 && !want.iter().any(|&(w, _)| w == t) {
            return Err(format!(
                "host target {t:#x} took {c} crossings the loop does not make"
            ));
        }
    }
    Ok(())
}

/// Each JNI slot in `want` gained exactly its count between the snapshots, and no other JNI slot gained any.
fn check_slots(before: &Snapshot, after: &Snapshot, want: &[(usize, u64)]) -> Result<(), String> {
    let of = |s: &Snapshot, k: usize| s.jni.iter().find(|&&(x, _)| x == k).map_or(0, |&(_, c)| c);
    for &(k, _) in &after.jni {
        let got = of(after, k) - of(before, k);
        let n = want.iter().find(|&&(w, _)| w == k).map_or(0, |&(_, n)| n);
        if got != n {
            return Err(format!(
                "JNI slot {k} counted {got} calls, the loop makes {n}"
            ));
        }
    }
    for &(k, n) in want {
        if n != 0 && !after.jni.iter().any(|&(x, _)| x == k) {
            return Err(format!("JNI slot {k} counted 0 calls, the loop makes {n}"));
        }
    }
    Ok(())
}

/// The host's own records: each function ran exactly its count.
fn check_own(own: &[(&str, &AtomicU64, u64)]) -> Result<(), String> {
    for &(name, calls, n) in own {
        let got = calls.load(Ordering::Relaxed);
        if got != n {
            return Err(format!("{name} ran {got} times, the loop makes {n}"));
        }
    }
    Ok(())
}

fn report(letter: char, result: Result<(), String>) -> bool {
    match result {
        Ok(()) => {
            println!("crossing: ({letter}) ok");
            true
        }
        Err(why) => {
            println!("crossing: ({letter}) FAIL {why}");
            false
        }
    }
}

/// `n` calls of `f(a)` from the stand-in guest loop, each crossing into the host and back.
///
/// # Safety
/// `ht` and `tcb` are the record and stand-in TCB `run` set up with `init_tcb`. The loop touches no TLS; `f` is a thunk or JNI stub of this process, whose host function runs on the guest FS with the counted shell's registers saved.
unsafe fn guest_calls(ht: &mut HostThread, tcb: usize, n: u64, f: usize, a: usize) {
    let lp = id_guest_loop as *const () as usize;
    // SAFETY: as this function's contract; the loop's arguments are plain words.
    unsafe {
        identity::Identity.call(ht, Tp::Guest(tcb), lp, &[n as usize, f, a, 0]);
    }
}

fn run() -> bool {
    if !identity::supported() {
        return report('s', Err("FSGSBASE: no (the engine cannot cross)".into()));
    }
    let host_fs = identity::fs_base();
    let tcb = Box::leak(vec![0usize; FAKE_TCB_WORDS].into_boxed_slice()).as_mut_ptr() as usize;
    let ht = HostThread::leak();
    // SAFETY: a leaked FAKE_TCB_WORDS-word stand-in TCB (bionic's: 11 words); the record is leaked.
    unsafe { identity::init_tcb(tcb, &raw mut *ht) };

    let bind = |f: extern "C" fn(usize)| identity::bind(addr(f));
    let (Some(a), Some(b), Some(d0), Some(d1)) =
        (bind(host_a), bind(host_b), bind(host_d), bind(host_d))
    else {
        return report('s', Err("no free thunk".into()));
    };
    // a host JNIEnv object: word 0 is its function table, which `atl_nb_jni_up` reads per call; the guest env: word 1 is the object, which the shell reads
    let mut table = vec![0usize; identity::JNI_STUBS];
    table[JNI_E] = addr(host_e);
    table[JNI_F] = addr(host_f);
    let table: &'static [usize] = Box::leak(table.into_boxed_slice());
    let object: &'static [usize; 1] = Box::leak(Box::new([table.as_ptr() as usize]));
    let env: &'static [usize; 2] = Box::leak(Box::new([0, object.as_ptr() as usize]));
    let env = env.as_ptr() as usize;
    let (je, jf) = (identity::jni_stub(JNI_E), identity::jni_stub(JNI_F));
    let mut ok = report('s', Ok(()));

    // (t) thunks: each target's counter delta and its own record
    let before = counters::snapshot();
    // SAFETY: the guest loop calls thunks bound above; each crosses to its host function.
    unsafe {
        guest_calls(ht, tcb, CALLS_A, a, 0);
        guest_calls(ht, tcb, CALLS_B, b, 0);
        guest_calls(ht, tcb, CALLS_D[0], d0, 0);
        guest_calls(ht, tcb, CALLS_D[1], d1, 0);
    }
    let t = delta(&before, &counters::snapshot());
    let d_total = CALLS_D[0] + CALLS_D[1];
    ok &= report(
        't',
        check_counts(
            &t,
            &[
                (addr(host_a), CALLS_A),
                (addr(host_b), CALLS_B),
                (addr(host_d), d_total),
            ],
        )
        .and_then(|()| {
            check_own(&[
                ("host_a", &OWN_A, CALLS_A),
                ("host_b", &OWN_B, CALLS_B),
                ("host_d", &OWN_D, d_total),
            ])
        }),
    );

    // (j) JNI stubs
    let before = counters::snapshot();
    // SAFETY: the guest loop calls JNI stubs of this process with the fake env built above.
    unsafe {
        guest_calls(ht, tcb, CALLS_E, je, env);
        guest_calls(ht, tcb, CALLS_F, jf, env);
    }
    let after = counters::snapshot();
    let j = delta(&before, &after);
    ok &= report(
        'j',
        check_counts(&j, &[(addr(host_e), CALLS_E), (addr(host_f), CALLS_F)])
            .and_then(|()| check_slots(&before, &after, &[(JNI_E, CALLS_E), (JNI_F, CALLS_F)]))
            .and_then(|()| check_own(&[("host_e", &OWN_E, CALLS_E), ("host_f", &OWN_F, CALLS_F)])),
    );

    // (w) one window of thunk crossings: the tick wraps once, so one dump
    let before = counters::snapshot();
    // SAFETY: as (t).
    unsafe { guest_calls(ht, tcb, WINDOW, a, 0) };
    let after = counters::snapshot();
    let crossings: u64 = delta(&before, &after).iter().map(|&(_, c)| c).sum();
    let dumps = after.dumps - before.dumps;
    ok &= report(
        'w',
        if crossings == WINDOW && dumps == 1 {
            Ok(())
        } else {
            Err(format!(
                "window made {crossings} crossings and {dumps} dumps, want {WINDOW} and 1"
            ))
        },
    );

    let fs_back = identity::fs_base() == host_fs;
    ok &= report(
        'f',
        if fs_back {
            Ok(())
        } else {
            Err("FS not on the host after the calls".into())
        },
    );
    ok
}

fn main() -> ExitCode {
    if run() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}
