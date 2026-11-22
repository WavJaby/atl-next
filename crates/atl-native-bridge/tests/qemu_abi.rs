// SPDX-License-Identifier: MIT
//! Q6-1 gate, harness-less (QEMU bridge plan Q6-1): the engine ABI in both directions. The trap side: a guest host call's integer lanes, FP lanes and stack words reach the host, and the host's FP answer returns in the guest's v0. The call side: `call_ex` puts the host's lanes in the guest's registers and stack words, and returns the callee's FP result.
//! The fixture (`xtask-harness/testdata/qemu-abi/fixture.c`, static) raises BOOT with a table of three entries, which the host calls: `qa_run` (checks x, v, k, r), `qa_sum` (d), `qa_edge` (e).
//! Env (xtask qemu_abi.rs sets it; unset prints SKIP): ATL_ENGINE_SO (the engine `.so`), ATL_QEMU_ABI_FIXTURE (fixture.c, built static).
//! Expected values are the fixture's literals, written out here again (never read from the engine): x0..x7 = 0x101 + i, v0..v7 = 1.5 + i and the stack words s0, s1 (x, v, k); the answer 3.25 is no input lane, so `qa_run` returns its bits (r); `qa_sum` returns [`SUM_EXPECT`] (d); `qa_edge` stores [`EDGE_S0`], [`EDGE_S1`] and reads its last six stack words past the mapped page as 0 (e).
//! Checks: (s) started (BOOT delivered, the thread attached); (x) integer lanes; (v) FP lanes; (k) stack words; (r) the value `qa_run` returned; (d) `call_ex` lanes into `qa_sum`; (e) stack words past the mapped page; (z) `attach_ex` stack size: default 1 MiB, a requested 256 KiB round-trips and holds the guest's sp; (f) after z's detach, a fork returns (QEMU's grace period ends).
//! Output: `qemu-abi: (x) ok` per check, `(x) FAIL <why>` otherwise, exit 0 when all pass.
#![allow(clippy::print_stdout, reason = "test harness output is its verdict")]

use std::path::Path;
use std::process::ExitCode;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use atl_engine_api::{AtlLanes, AtlStack, PROT_READ, PROT_WRITE};
use atl_native_bridge::engine::{self, Engine as _, qemu};

/// fixture.c `qa_run`: x0..x7 = 0x101 + i.
const X_EXPECT: [u64; 8] = [0x101, 0x102, 0x103, 0x104, 0x105, 0x106, 0x107, 0x108];
/// fixture.c `qa_run`: v0..v7 = 1.5 + i (decimal literals; the bits are compared).
const V_EXPECT: [f64; 8] = [1.5, 2.5, 3.5, 4.5, 5.5, 6.5, 7.5, 8.5];
/// fixture.c `qa_run`: the stack words s0 and s1 at sp.
const K_EXPECT: [u64; 2] = [0x0123_4567_89ab_cdef, 0xfedc_ba98_7654_3210];
/// The host's FP answer: v0 after `qa_run`'s call, which `qa_run` returns as bits.
const ANSWER: f64 = 3.25;

/// Check d, the arguments of fixture.c `qa_sum`: ints 1..=9 (x0..x7, then the ninth as stack word 0), doubles (i + 1) / 2 (v0..v7, then the ninth, 4.5, as stack word 1).
const SUM_INTS: [u64; 9] = [1, 2, 3, 4, 5, 6, 7, 8, 9];
const SUM_DOUBLES: [f64; 9] = [0.5, 1.0, 1.5, 2.0, 2.5, 3.0, 3.5, 4.0, 4.5];
/// `qa_sum`'s result. Derivation: the ints read as decimal digits give 123456789; the doubles weighted 2^i give sum (i + 1) / 2 * 2^i over i = 0..=8 = (1/2) * sum k * 2^(k-1) for k = 1..=9 = 4097 / 2 = 2048.5. Total 123458837.5, exact in binary64, so the bits compare.
const SUM_EXPECT: f64 = 123_458_837.5;

/// Check e, fixture.c `qa_edge`: the two words it stores at its sp.
const EDGE_S0: u64 = 0x0f0e_0d0c_0b0a_0908;
const EDGE_S1: u64 = 0x1716_1514_1312_1110;
/// The host page the guest maps. The page after it is host memory the guest does not map.
const PAGE: usize = 4096;
/// The first word of the unmapped page: a stack read that reached it would return this.
const MARKER: u64 = 0xdead_beef_dead_beef;

/// Check z: `attach_ex` with size 0 gets the default guest stack, 1 MiB (maintainer D2, bionic's default order).
const DEFAULT_STACK: u64 = 1 << 20;
/// Check z: the requested stack, 256 KiB (plan G3), page-aligned so no rounding applies.
const SIZED_STACK: u64 = 256 << 10;

/// Check f: wait before the fork so QEMU's grace period for z's detach has started. QEMU's call_rcu thread (util/rcu.c `call_rcu_thread`) batches a lone callback for at most 5 x 10 ms before `synchronize_rcu`; 4x that for scheduling.
const GRACE_START: std::time::Duration = std::time::Duration::from_millis(200);
/// Check f: how long the fork may take. A sound grace period ends within `wait_for_readers`' forced phase (5 polls x 10 ms, then the readers' unlock); a broken one never ends, so any bound far above 50 ms separates them; 5 s keeps a loaded host green.
const FORK_LIMIT: std::time::Duration = std::time::Duration::from_secs(5);

/// BOOT's payload: the address of the fixture's entry table (`qa_run`, `qa_sum`, `qa_edge`); identity, so the host reads it directly.
static TABLE: AtomicU64 = AtomicU64::new(0);
/// The lanes of the gate's last host call (checks x, v, k, e), as the host received them.
static LAST: Mutex<Option<AtlLanes>> = Mutex::new(None);

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

/// The engine's trap callback for this gate. BOOT records the table; CALL records its lanes and answers v0 = the FP answer, x0 = 0; the rest is the production route.
fn on_trap(trap: qemu::Trap) -> qemu::Answer {
    match trap {
        qemu::Trap::Boot(table) => {
            TABLE.store(table, Ordering::Relaxed);
            qemu::Answer::default()
        }
        qemu::Trap::Call(lanes) => {
            *LAST.lock().expect("last") = Some(lanes);
            qemu::Answer {
                v0: ANSWER.to_bits(),
                ..qemu::Answer::default()
            }
        }
        other => qemu::on_trap(other),
    }
}

/// The fixture's entries `[qa_run, qa_sum, qa_edge]` from its table at `table` (BOOT's x0).
fn entries(table: u64) -> Result<[u64; 3], String> {
    if table == 0 {
        return Err("BOOT carried no entry table".to_owned());
    }
    let p = table as *const u64;
    // SAFETY: the table is the fixture's static array of three pointers, mapped in this process (identity).
    Ok(std::array::from_fn(|i| unsafe { p.add(i).read() }))
}

/// Two host pages of anonymous memory. The first is the page the guest maps; the second holds [`MARKER`]. Left mapped: the gate is the process.
fn two_pages() -> Result<usize, String> {
    // SAFETY: an anonymous private mapping of two pages; the result is checked against MAP_FAILED below.
    let p = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            2 * PAGE,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    if p == libc::MAP_FAILED {
        return Err("mmap of two pages failed".to_owned());
    }
    let base = p as usize;
    // SAFETY: base + PAGE is the second page of the mapping, page-aligned, so a u64 there is in bounds and aligned.
    unsafe { ((base + PAGE) as *mut u64).write(MARKER) };
    Ok(base)
}

/// Check d: `qa_sum` through `call_ex`, its ninth int and ninth double as stack arguments. The double result is the answer lane v0.
fn check_sum(sum: u64) -> Result<(), String> {
    let mut lanes = AtlLanes::default();
    lanes.regs[..8].copy_from_slice(&SUM_INTS[..8]);
    lanes.stack[0] = SUM_INTS[8];
    for (v, d) in lanes.fp.iter_mut().zip(SUM_DOUBLES) {
        *v = d.to_bits();
    }
    lanes.stack[1] = SUM_DOUBLES[8].to_bits();
    qemu::call_ex(sum, &mut lanes);
    same("qa_sum v0 bits", lanes.fp[0], SUM_EXPECT.to_bits())
}

/// Check e: `qa_edge` with its sp 16 bytes below the end of the first host page, only that page mapped. Words 0 and 1 are the stored pair; words 2..16 lie past the mapped page and must read 0, not [`MARKER`].
fn check_edge(edge: u64) -> Result<(), String> {
    let base = two_pages()?;
    qemu::CROSSING.map(base, PAGE, PROT_READ | PROT_WRITE)?;
    let mut lanes = AtlLanes::default();
    lanes.regs[0] = (base + PAGE - 16) as u64;
    qemu::call_ex(edge, &mut lanes);
    let seen = LAST.lock().expect("last").take().unwrap_or_default().stack;
    same("stack words 0..2", [seen[0], seen[1]], [EDGE_S0, EDGE_S1])?;
    // the aarch64 engine reads all sixteen words of the window; every one past the page reads 0
    same(
        "stack words 2..16 past the mapped page",
        seen[2..].to_vec(),
        vec![0; atl_engine_api::LANE_STACK - 2],
    )
}

/// Check z, `attach_ex`: the main thread's default stack is [`DEFAULT_STACK`]; a second thread asks for [`SIZED_STACK`], gets exactly that, and `qa_run` on it stores its stack words (`K_EXPECT`) inside `[base, base + len)`, so the guest's sp lives in the reported range.
fn check_stack(default: AtlStack, run: u64) -> Result<(), String> {
    same("default stack len", default.len, DEFAULT_STACK)?;
    std::thread::spawn(move || -> Result<(), String> {
        let request = AtlStack {
            size: SIZED_STACK,
            ..AtlStack::default()
        };
        let stack = qemu::attach_current_ex(request)?.ok_or("fresh thread had a CPU already")?;
        qemu::call(run, &[]);
        // the scan runs before detach, which unmaps the stack
        let found = holds_pair(stack, K_EXPECT);
        qemu::detach_current();
        same("256 KiB stack len", stack.len, SIZED_STACK)?;
        if found {
            Ok(())
        } else {
            Err(format!(
                "qa_run's stack words not in [{:#x}, +{:#x}): sp outside the reported stack",
                stack.base, stack.len
            ))
        }
    })
    .join()
    .map_err(|_| "sized-stack thread panicked".to_owned())?
}

/// Check f, fork after a detach: z's detach frees the CPU's jump cache through RCU (`tcg_exec_unrealizefn` -> `g_free_rcu`), so QEMU's call_rcu thread runs a grace period, and QEMU's prefork handler (`rcu_init_lock`) waits for it. The main thread loaded the engine and attached: if it is in QEMU's RCU registry twice (the constructor's registration plus attach's), the registry is a cycle, the grace period never ends, and every fork in the process (WebKit's process spawn) blocks.
fn check_fork() -> Result<(), String> {
    std::thread::sleep(GRACE_START);
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        // SAFETY: the child calls only _exit (async-signal-safe).
        let pid = unsafe { libc::fork() };
        if pid == 0 {
            // SAFETY: as above.
            unsafe { libc::_exit(0) };
        }
        let _ = tx.send(pid);
    });
    let pid = rx.recv_timeout(FORK_LIMIT).map_err(|_| {
        format!("fork did not return within {FORK_LIMIT:?}: QEMU's prefork handler waits on a grace period that does not end")
    })?;
    if pid < 0 {
        return Err(format!("fork: {}", std::io::Error::last_os_error()));
    }
    let mut status = 0;
    // SAFETY: pid is this process's child; status is a valid out-parameter.
    unsafe { libc::waitpid(pid, &mut status, 0) };
    same("child exit", status, 0)
}

/// Whether two adjacent words `pair` lie inside `stack`'s usable range (identity: guest memory is host memory).
fn holds_pair(stack: AtlStack, pair: [u64; 2]) -> bool {
    let words = stack.len as usize / 8;
    // SAFETY: [base, base + len) is the calling thread's attached guest stack, mapped read/write until detach.
    let s = unsafe { std::slice::from_raw_parts(stack.base as *const u64, words) };
    s.windows(2).any(|w| w == pair)
}

/// Runs the checks in order against the fixture's entries. Returns true when every check passed.
fn checks([run, sum, edge]: [u64; 3]) -> bool {
    let returned = qemu::call(run, &[]);
    let lanes = LAST.lock().expect("last").take().unwrap_or_default();
    let mut ok = true;
    ok &= check(
        'x',
        same("x0..x7", lanes.regs[..8].to_vec(), X_EXPECT.to_vec()),
    );
    ok &= check(
        'v',
        same(
            "v0..v7 bits",
            lanes.fp[..8].to_vec(),
            V_EXPECT.map(f64::to_bits).to_vec(),
        ),
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
    ok &= check('d', check_sum(sum));
    ok &= check('e', check_edge(edge));
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
    if let Err(e) = engine::claim(Some(qemu::Arch::Aarch64.isa())) {
        println!("qemu-abi: (s) FAIL engine: {e}");
        return ExitCode::from(2);
    }
    // BOOT answers at once, so the engine is up when this returns
    if let Err(e) = qemu::boot(
        qemu::Arch::Aarch64,
        Path::new(&so),
        Path::new(&fixture),
        on_trap,
    ) {
        println!("qemu-abi: (s) FAIL boot: {e}");
        return ExitCode::from(2);
    }
    let default_stack = match qemu::attach_current_ex(AtlStack::default()) {
        Ok(Some(stack)) => stack,
        Ok(None) => {
            println!("qemu-abi: (s) FAIL attach: the main thread had a CPU already");
            return ExitCode::from(2);
        }
        Err(e) => {
            println!("qemu-abi: (s) FAIL attach: {e}");
            return ExitCode::from(2);
        }
    };
    let entries = match entries(TABLE.load(Ordering::Relaxed)) {
        Ok(entries) => entries,
        Err(why) => {
            check('s', Err(why));
            return ExitCode::FAILURE;
        }
    };
    if !check('s', Ok(())) {
        return ExitCode::FAILURE;
    }
    if checks(entries)
        & check('z', check_stack(default_stack, entries[0]))
        & check('f', check_fork())
    {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
