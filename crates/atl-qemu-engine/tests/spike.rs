// SPDX-License-Identifier: MIT
//! Spike gates (QEMU bridge plan Q2/Q3, Q5-2), harness-less: `start` boots QEMU on its helper thread, whose BOOT trap runs gates (a)-(l) there and parks; the host threads then run (n), (m), (o), (p) on their own guest CPUs, and the process exits 0 when every gate is ok.
//! Env (xtask qemu_spike.rs sets it; without it the run prints SKIP): `ATL_QEMU_SPIKE_SO` = the spike library, `ATL_QEMU_SPIKE_FIXTURE` = the static guest program, `ATL_QEMU_SIGCHAIN_OBJ` = AOSP libsigchain's object (build.rs links it and sets cfg `atl_qe_sigchain`). Args: `--guest-base <n>` (QEMU's `-B`, default 0 = identity).
//! Expected values come from the fixture's table (fixture.c), never from the engine. One line per gate: `spike: (x) ok` or `spike: (x) FAIL <why>`.
//! Gates: (a) trap arguments, (b) nested host/guest calls, (c) host memory, (d) guest store fault, (e) host fault order behind sigchain, (h) implicit null check through sigchain, (f) write into translated code, (g) second guest thread, (i) fatal guest fault on one thread, (j) the same on a second thread (both end by SIGSEGV), (k) host fault with previous action SIG_IGN (ends by SIGSEGV), (l) binding thunks from the guest pool (bit 15 of the svc immediate, id = slot); (n) helper boot: `start` returned with the BOOT answer, the helper stays parked, the creator thread attaches and calls; (m) two host threads attach at once and call concurrently, each with its own thread pointer and syscalls; (o) a write(2) on host heap memory, accepted under identity mapping (access_ok patch 0002), (p) a write straddling a QEMU-mapped page into host memory QEMU does not map.
#![allow(clippy::print_stdout, reason = "test harness output is its verdict")]

#[cfg(feature = "qemu")]
mod spike {
    use std::cell::{Cell, RefCell};
    use std::ffi::c_int;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::{Barrier, Mutex, OnceLock};

    use atl_qemu_engine::{Booted, Guest, Trap, page};

    /// Exit status of the previous handler (gates e, h): the fault reached it after the special handler declined it, in sigchain's order.
    const PREVIOUS_EXIT: c_int = 43;
    /// Exit status of the previous handler when the special handler had not run first.
    const ORDER_EXIT: c_int = 46;
    /// Exit status of the NPE stub (gate h): the special handler claimed the fault.
    const NPE_EXIT: c_int = 44;

    /// The fixture's table words (fixture.c `table`).
    mod t {
        pub const TRAP_ARGS: usize = 0;
        pub const OUTER: usize = 1;
        pub const BOX: usize = 2;
        pub const STORE_FAULT: usize = 3;
        pub const INNER: usize = 4;
        pub const A_FIRST: usize = 5;
        pub const A_ADD: usize = 6;
        pub const B_N: usize = 7;
        pub const B_SUM: usize = 8;
        pub const C_BEFORE: usize = 9;
        pub const C_AFTER: usize = 13;
        pub const D_MARK: usize = 17;
        pub const D_RET: usize = 18;
        pub const SMC_SETUP: usize = 19;
        pub const SMC_RUN: usize = 20;
        pub const SMC_OLD: usize = 21;
        pub const SMC_NEW_INSN: usize = 22;
        pub const SMC_NEW: usize = 23;
        pub const THREAD_FN: usize = 24;
        pub const THREAD_RET: usize = 25;
        pub const FATAL: usize = 26;
        pub const FATAL_THREAD: usize = 27;
        pub const BIND_FN: usize = 28;
        pub const BIND_REPORT: usize = 29;
        pub const SUM: usize = 30;
        pub const M_N1: usize = 31;
        pub const M_S1: usize = 32;
        pub const M_N2: usize = 33;
        pub const M_S2: usize = 34;
        pub const TP: usize = 35;
        pub const WRITE_MSG: usize = 36;
        pub const MSG: usize = 37;
        pub const MSG_LEN: usize = 38;
        pub const WRITE: usize = 39;
        pub const LEN: usize = 40;
    }

    /// What the host answers a CALL with during gate (a): any value the guest cannot produce itself.
    const A_ANSWER: u64 = 1000;
    /// What the host answers a binding trap during gate (l); the fixture reports it + 1 (table `BIND_REPORT`).
    const BIND_ANSWER: u64 = 0x1234;
    /// Gate (l)'s pool slots: `fx_bind(k)` must reach binding k. Literals here, as the fixture's table is; the pool's layout (slot k = id k) is checked on the built fixture by xtask.
    const BIND_SLOTS: [u32; 3] = [3, 0, 8191];
    /// Gate (m): calls per host thread, each fx_sum + fx_tp + fx_write_msg. Enough rounds that the two threads' guest code overlaps.
    const M_ROUNDS: usize = 20;
    /// Gate (o): bytes of the host heap buffer handed to write(2).
    const O_LEN: usize = 64;
    /// Gate (p): bytes of the straddling buffer handed to write(2), half in each page.
    const P_LEN: usize = 64;

    /// The fixture's table, from its BOOT trap; read by every thread.
    static TABLE: OnceLock<Vec<u64>> = OnceLock::new();

    thread_local! {
        static GATE: Cell<char> = const { Cell::new(' ') };
        static A_SEEN: Cell<Option<[u64; 8]>> = const { Cell::new(None) };
        /// The binding traps of the current gate-(l) call: (id, x0..x7).
        static BINDS: RefCell<Vec<(u32, [u64; 8])>> = const { RefCell::new(Vec::new()) };
    }

    /// The guest's reports on any thread: the callback runs on the faulting thread, so this is process-wide.
    static REPORTS: Mutex<Vec<u64>> = Mutex::new(Vec::new());
    /// Calls of the special handler so far: every SIGSEGV of the process reaches it first.
    static SPECIAL_SEEN: AtomicU64 = AtomicU64::new(0);
    /// `SPECIAL_SEEN` at the fork of gates (e) and (h): the child's previous handler checks that exactly one call, its own fault, came since.
    static SEEN_AT_FORK: AtomicU64 = AtomicU64::new(0);
    /// Gate (k)'s verdict, from its child before QEMU starts (`run_gate_k`); printed with the others.
    static K_RESULT: OnceLock<Result<(), String>> = OnceLock::new();
    /// Set in gate (k)'s child: its BOOT trap runs gate (k) instead of the table gates.
    static GATE_K_CHILD: AtomicBool = AtomicBool::new(false);

    // ART-style managed code: an implicit null check is a load through a register. A fault at a pc in [atl_managed_probe, atl_managed_probe_end) is claimed by the special handler, which moves the pc to atl_npe_stub.
    core::arch::global_asm!(
        ".globl atl_managed_probe",
        ".globl atl_managed_probe_end",
        ".globl atl_npe_stub",
        "atl_managed_probe:",
        "    mov rax, qword ptr [rdi]",
        "    ret",
        "atl_managed_probe_end:",
        "atl_npe_stub:",
        "    and rsp, -16",
        "    mov edi, {exit}",
        "    call {_exit}",
        exit = const NPE_EXIT,
        _exit = sym libc::_exit,
    );

    unsafe extern "C" {
        fn atl_managed_probe(p: *mut u64) -> u64;
        fn atl_managed_probe_end();
        fn atl_npe_stub();
    }

    /// AOSP's libsigchain (art/sigchainlib), linked into this executable: the chain ART installs its fault handlers on.
    #[cfg(atl_qe_sigchain)]
    mod chain {
        use std::ffi::{c_int, c_void};
        use std::sync::atomic::Ordering;

        use super::{SPECIAL_SEEN, atl_managed_probe, atl_managed_probe_end, atl_npe_stub};

        /// `art::SigchainAction` (art/sigchainlib/sigchain.h): one special handler, its mask and flags.
        #[repr(C)]
        pub struct SigchainAction {
            pub sc_sigaction: extern "C" fn(c_int, *mut libc::siginfo_t, *mut c_void) -> bool,
            pub sc_mask: libc::sigset_t,
            pub sc_flags: u64,
        }

        unsafe extern "C" {
            /// Registers a special handler; sigchain claims the signal (its kernel action is the chain's from then on).
            pub fn AddSpecialSignalHandlerFn(signal: c_int, sa: *mut SigchainAction);
        }

        /// The ART FaultManager stand-in (art/runtime/fault_handler.cc registers the same way): claims a fault at a pc in the managed probe, by moving the pc to the NPE stub.
        extern "C" fn art_special(_: c_int, _: *mut libc::siginfo_t, uc: *mut c_void) -> bool {
            SPECIAL_SEEN.fetch_add(1, Ordering::Relaxed);
            // SAFETY: the kernel's ucontext of this delivery (SA_SIGINFO third argument); the gregs writes resume the thread at the stub on sigreturn.
            let gregs = unsafe { &mut (*uc.cast::<libc::ucontext_t>()).uc_mcontext.gregs };
            let pc = gregs[libc::REG_RIP as usize] as usize;
            let probe = atl_managed_probe as *const () as usize;
            let end = atl_managed_probe_end as *const () as usize;
            if !(probe..end).contains(&pc) {
                return false;
            }
            gregs[libc::REG_RIP as usize] = atl_npe_stub as *const () as usize as i64;
            true
        }

        /// ART claims SIGSEGV and SIGBUS through sigchain before QEMU's `main`, as its FaultManager does.
        pub fn register() {
            for sig in [libc::SIGSEGV, libc::SIGBUS] {
                let mut sa = SigchainAction {
                    sc_sigaction: art_special,
                    // SAFETY: a zeroed sigset_t is the empty set.
                    sc_mask: unsafe { std::mem::zeroed() },
                    sc_flags: 0,
                };
                // SAFETY: sigchain copies sa into its table; art_special has the special-handler signature.
                unsafe { AddSpecialSignalHandlerFn(sig, &mut sa) };
            }
        }
    }

    pub fn main() {
        let (Some(so), Some(fixture)) = (
            std::env::var_os("ATL_QEMU_SPIKE_SO"),
            std::env::var_os("ATL_QEMU_SPIKE_FIXTURE"),
        ) else {
            println!("spike: SKIP (ATL_QEMU_SPIKE_SO / ATL_QEMU_SPIKE_FIXTURE unset)");
            return;
        };
        #[cfg(not(atl_qe_sigchain))]
        {
            println!("spike: SKIP (ATL_QEMU_SIGCHAIN_OBJ unset: no libsigchain)");
            return;
        }
        let args: Vec<String> = std::env::args().collect();
        let base = args
            .iter()
            .position(|a| a == "--guest-base")
            .map_or("0".to_owned(), |i| args[i + 1].clone());
        #[cfg(atl_qe_sigchain)]
        chain::register();
        // the handler the process had before QEMU: host faults reach it after the special handler (gate e)
        // SAFETY: a plain handler for SIGSEGV; zeroed mask/flags are valid.
        unsafe {
            let mut act: libc::sigaction = std::mem::zeroed();
            act.sa_sigaction = previous_handler as *const () as usize;
            assert_eq!(
                libc::sigaction(libc::SIGSEGV, &act, std::ptr::null_mut()),
                0
            );
        }
        let fixture = fixture.to_string_lossy().into_owned();
        run_gate_k(&so, &fixture);
        let booted = match atl_qemu_engine::start(
            std::path::Path::new(&so),
            &["qemu-aarch64", "-B", &base, &fixture],
            on_trap,
        ) {
            Ok(b) => b,
            Err(e) => {
                println!("spike: FAIL boot: {e}");
                std::process::exit(2);
            }
        };
        // the BOOT answer: 0 = gates (a)-(l) ok on the boot thread
        let mut ok = booted.answer() == 0;
        for (gate, f) in [
            ('n', gate_n as fn(&Booted) -> Result<(), String>),
            ('m', gate_m),
            ('o', gate_o),
            ('p', gate_p),
        ] {
            // a panic (an engine assert on this thread) is the gate's failure, not the harness's
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(&booted)))
                .unwrap_or_else(|_| Err("panicked".to_owned()));
            ok &= verdict(gate, r);
        }
        std::process::exit(i32::from(!ok));
    }

    /// The process's pre-QEMU handler. Gates (e) and (h) expect host faults here, after the special handler declined them.
    extern "C" fn previous_handler(_: c_int) {
        // exactly one special-handler call since the fork: the fault itself
        let ordered =
            SPECIAL_SEEN.load(Ordering::Relaxed) == SEEN_AT_FORK.load(Ordering::Relaxed) + 1;
        let code = if ordered { PREVIOUS_EXIT } else { ORDER_EXIT };
        // SAFETY: async-signal-safe exit, no unwinding.
        unsafe { libc::_exit(code) };
    }

    fn w(i: usize) -> u64 {
        TABLE.get().expect("table from BOOT")[i]
    }

    fn reports() -> Vec<u64> {
        REPORTS.lock().expect("reports").clone()
    }

    fn on_trap(g: Guest, trap: Trap) -> u64 {
        match trap {
            Trap::Boot(table) => {
                if GATE_K_CHILD.load(Ordering::Relaxed) {
                    gate_k();
                }
                u64::from(!gates(g, table))
            }
            Trap::Call(regs) => match GATE.with(Cell::get) {
                'a' => {
                    A_SEEN.with(|s| s.set(Some(regs)));
                    A_ANSWER
                }
                // fx_outer's CALL(i): answer with the guest's own fx_inner(i), a nested host->guest call
                'b' => g.call(w(t::INNER), &[regs[0]]),
                other => panic!("CALL during gate {other:?}"),
            },
            Trap::Report(v) => {
                REPORTS.lock().expect("reports").push(v);
                0
            }
            Trap::Bind { id, args } => {
                BINDS.with(|b| b.borrow_mut().push((id, args)));
                BIND_ANSWER
            }
        }
    }

    fn verdict(gate: char, r: Result<(), String>) -> bool {
        match &r {
            Ok(()) => println!("spike: ({gate}) ok"),
            Err(e) => println!("spike: ({gate}) FAIL {e}"),
        }
        r.is_ok()
    }

    /// All gates in order; true when every one passed.
    fn gates(g: Guest, table: u64) -> bool {
        let words = g.host_addr(table) as *const u64;
        // SAFETY: the fixture's static table of t::LEN words, at its host address.
        let words = unsafe { std::slice::from_raw_parts(words, t::LEN) }.to_vec();
        let _ = TABLE.set(words);
        let mut ok = verdict(
            'k',
            K_RESULT
                .get()
                .cloned()
                .unwrap_or_else(|| Err("gate (k) did not run".to_owned())),
        );
        for (gate, f) in [
            ('a', gate_a as fn(Guest) -> Result<(), String>),
            ('b', gate_b),
            ('c', gate_c),
            ('d', gate_d),
            ('e', gate_e),
            ('h', gate_h),
            ('f', gate_f),
            ('g', gate_g),
            ('i', gate_i),
            ('j', gate_j),
            ('l', gate_l),
        ] {
            GATE.with(|c| c.set(gate));
            REPORTS.lock().expect("reports").clear();
            ok &= verdict(gate, f(g));
        }
        ok
    }

    fn gate_a(g: Guest) -> Result<(), String> {
        let r = g.call(w(t::TRAP_ARGS), &[]);
        let expected: [u64; 8] = std::array::from_fn(|i| w(t::A_FIRST) + i as u64);
        let seen = A_SEEN.with(Cell::get).ok_or("no CALL reached the host")?;
        if seen != expected {
            return Err(format!("x0..x7 {seen:?}, expected {expected:?}"));
        }
        let want = A_ANSWER + w(t::A_ADD);
        if r != want {
            return Err(format!("guest returned {r}, expected {want}"));
        }
        Ok(())
    }

    fn gate_b(g: Guest) -> Result<(), String> {
        let r = g.call(w(t::OUTER), &[w(t::B_N)]);
        let want = w(t::B_SUM);
        if r != want {
            return Err(format!("fx_outer({}) = {r}, expected {want}", w(t::B_N)));
        }
        Ok(())
    }

    fn gate_c(g: Guest) -> Result<(), String> {
        let mut host: Box<[u64; 4]> = Box::new(std::array::from_fn(|i| w(t::C_BEFORE + i)));
        let p = host.as_mut_ptr() as u64;
        g.call(w(t::BOX), &[p]);
        let want: [u64; 4] = std::array::from_fn(|i| w(t::C_AFTER + i));
        if *host != want {
            return Err(format!("box {:?}, expected {want:?}", *host));
        }
        Ok(())
    }

    /// A store fault in translated code (address 0): QEMU delivers it (the code route) and the guest's own SIGSEGV handler runs and reports D_MARK.
    fn gate_d(g: Guest) -> Result<(), String> {
        let before = atl_qemu_engine::signal::routed().code;
        let r = g.call(w(t::STORE_FAULT), &[]);
        let faults = atl_qemu_engine::signal::routed().code - before;
        let reports = reports();
        if reports != [w(t::D_MARK)] {
            return Err(format!(
                "guest handler reports {reports:?}, expected [{}]",
                w(t::D_MARK)
            ));
        }
        if faults != 1 {
            return Err(format!(
                "{faults} translated-code faults routed to QEMU's handler, expected 1"
            ));
        }
        if r != w(t::D_RET) {
            return Err(format!(
                "fx_store_fault returned {r}, expected {}",
                w(t::D_RET)
            ));
        }
        Ok(())
    }

    /// Forks a child for a fault under test. The parent records the special handler's count first.
    fn fork_child() -> Result<libc::pid_t, String> {
        SEEN_AT_FORK.store(SPECIAL_SEEN.load(Ordering::Relaxed), Ordering::Relaxed);
        // SAFETY: fork from the single guest thread (before gate g starts one); the child only faults and exits.
        let pid = unsafe { libc::fork() };
        if pid < 0 {
            return Err("fork failed".to_owned());
        }
        Ok(pid)
    }

    /// The forked child's exit status; a child killed by a signal is an error.
    fn wait_child(pid: libc::pid_t) -> Result<c_int, String> {
        let mut status = 0;
        // SAFETY: waits for the child just forked.
        if unsafe { libc::waitpid(pid, &mut status, 0) } != pid {
            return Err("waitpid failed".to_owned());
        }
        if libc::WIFEXITED(status) {
            Ok(libc::WEXITSTATUS(status))
        } else {
            Err(format!("child killed by signal {}", libc::WTERMSIG(status)))
        }
    }

    /// A host-only fault (Rust code, not TCG) in a forked child: the special handler declines it, then the previous handler runs, not QEMU's `die_from_signal`.
    fn gate_e(_: Guest) -> Result<(), String> {
        let pid = fork_child()?;
        if pid == 0 {
            // SAFETY: deliberate null store, the fault under test.
            unsafe { std::ptr::null_mut::<u64>().write_volatile(1) };
            // SAFETY: not reached unless the store did not fault.
            unsafe { libc::_exit(0) };
        }
        match wait_child(pid)? {
            PREVIOUS_EXIT => Ok(()),
            ORDER_EXIT => Err("the previous handler ran before the special handler".to_owned()),
            code => Err(format!("child exit {code}, expected {PREVIOUS_EXIT}")),
        }
    }

    /// ART's implicit null check through sigchain: the probe's load faults, the special handler claims it and the pc moves to the NPE stub. Only the claim reaches that stub; a fall-through exits PREVIOUS_EXIT.
    fn gate_h(_: Guest) -> Result<(), String> {
        let pid = fork_child()?;
        if pid == 0 {
            // SAFETY: deliberate null load in the probe, the fault under test; a claim does not return here.
            unsafe { atl_managed_probe(std::ptr::null_mut()) };
            // SAFETY: not reached unless the load did not fault.
            unsafe { libc::_exit(0) };
        }
        match wait_child(pid)? {
            NPE_EXIT => Ok(()),
            PREVIOUS_EXIT => {
                Err("the fault fell through to the previous handler: not claimed".to_owned())
            }
            ORDER_EXIT => Err("the previous handler ran before the special handler".to_owned()),
            code => Err(format!("child exit {code}, expected {NPE_EXIT}")),
        }
    }

    /// A write into translated code: QEMU write-protects a writable page that holds translated code, so the host's write faults with SEGV_ACCERR; QEMU's handler un-protects the page and the guest runs the new code.
    fn gate_f(g: Guest) -> Result<(), String> {
        let page = g.call(w(t::SMC_SETUP), &[]);
        let first = g.call(w(t::SMC_RUN), &[page]);
        if first != w(t::SMC_OLD) {
            return Err(format!(
                "first run returned {first}, expected {}",
                w(t::SMC_OLD)
            ));
        }
        let before = atl_qemu_engine::signal::routed().accerr;
        // SAFETY: the fixture's writable, executable page; g.host_addr is its host address (identity at -B 0).
        unsafe { (g.host_addr(page) as *mut u32).write_volatile(w(t::SMC_NEW_INSN) as u32) };
        let faults = atl_qemu_engine::signal::routed().accerr - before;
        if faults != 1 {
            return Err(format!(
                "{faults} write faults routed to QEMU's handler, expected 1"
            ));
        }
        let second = g.call(w(t::SMC_RUN), &[page]);
        if second != w(t::SMC_NEW) {
            return Err(format!(
                "second run returned {second}, expected {}",
                w(t::SMC_NEW)
            ));
        }
        Ok(())
    }

    /// A second guest thread faults inside a helper (an atomic on address 0): its helper_retaddr classifies the fault, QEMU's handler delivers it, and the guest handler runs on that thread and reports D_MARK.
    fn gate_g(g: Guest) -> Result<(), String> {
        let before = atl_qemu_engine::signal::routed().helper;
        let r = g.call(w(t::THREAD_FN), &[]);
        let faults = atl_qemu_engine::signal::routed().helper - before;
        let reports = reports();
        if reports != [w(t::D_MARK)] {
            return Err(format!(
                "guest handler reports {reports:?}, expected [{}]",
                w(t::D_MARK)
            ));
        }
        if faults != 1 {
            return Err(format!(
                "{faults} helper faults routed to QEMU's handler, expected 1"
            ));
        }
        if r != w(t::THREAD_RET) {
            return Err(format!(
                "thread gate returned {r}, expected {}",
                w(t::THREAD_RET)
            ));
        }
        Ok(())
    }

    /// The forked child's death signal; a child that exits is an error.
    fn wait_signal(pid: libc::pid_t) -> Result<c_int, String> {
        let mut status = 0;
        // SAFETY: waits for the child just forked.
        if unsafe { libc::waitpid(pid, &mut status, 0) } != pid {
            return Err("waitpid failed".to_owned());
        }
        if libc::WIFSIGNALED(status) {
            Ok(libc::WTERMSIG(status))
        } else {
            Err(format!(
                "child exited {}, expected death by signal",
                libc::WEXITSTATUS(status)
            ))
        }
    }

    /// QEMU's fatal path (die_with_signal): the fixture function `entry` faults with no guest SIGSEGV handler; the child must end by SIGSEGV, not exit (sigchain's revert) or sleep.
    fn fatal_gate(g: Guest, entry: u64) -> Result<(), String> {
        let pid = fork_child()?;
        if pid == 0 {
            g.call(entry, &[]);
            // SAFETY: not reached unless the guest survived its fatal fault.
            unsafe { libc::_exit(0) };
        }
        match wait_signal(pid)? {
            libc::SIGSEGV => Ok(()),
            sig => Err(format!("child died by signal {sig}, expected SIGSEGV")),
        }
    }

    /// Gate (i): the fatal fault on the main guest thread.
    fn gate_i(g: Guest) -> Result<(), String> {
        fatal_gate(g, w(t::FATAL))
    }

    /// Gate (j): the fatal fault on a second guest thread.
    fn gate_j(g: Guest) -> Result<(), String> {
        fatal_gate(g, w(t::FATAL_THREAD))
    }

    /// Gate (l): binding thunks from the guest pool. `fx_bind(k)` calls slot k with x0..x7 = 1..8; the trap must name binding k with those eight values, and the result and the report are the fixture's (BIND_ANSWER + 1).
    fn gate_l(g: Guest) -> Result<(), String> {
        let want_args: [u64; 8] = std::array::from_fn(|i| w(t::A_FIRST) + i as u64);
        let want = w(t::BIND_REPORT);
        for slot in BIND_SLOTS {
            BINDS.with(|b| b.borrow_mut().clear());
            REPORTS.lock().expect("reports").clear();
            let r = g.call(w(t::BIND_FN), &[u64::from(slot)]);
            let binds = BINDS.with(|b| std::mem::take(&mut *b.borrow_mut()));
            if binds != [(slot, want_args)] {
                return Err(format!(
                    "slot {slot}: traps {binds:?}, expected [({slot}, {want_args:?})]"
                ));
            }
            let reports = reports();
            if reports != [want] || r != want {
                return Err(format!(
                    "slot {slot}: returned {r}, reports {reports:?}, expected {want}"
                ));
            }
        }
        Ok(())
    }

    /// A pipe for the guest's write(2) calls: (read end, write end), both non-blocking.
    fn pipe() -> Result<(c_int, c_int), String> {
        let mut fds = [0; 2];
        // SAFETY: fds is a valid out array of two ints.
        if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) } != 0 {
            return Err("pipe2 failed".to_owned());
        }
        Ok((fds[0], fds[1]))
    }

    /// Everything the pipe holds now.
    fn drain(fd: c_int) -> Vec<u8> {
        let mut out = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            // SAFETY: buf is a writable buffer of its length.
            let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
            let Ok(n) = usize::try_from(n) else { break };
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n]);
        }
        out
    }

    fn close(fds: (c_int, c_int)) {
        // SAFETY: the two descriptors pipe() opened.
        unsafe {
            libc::close(fds.0);
            libc::close(fds.1);
        }
    }

    /// The fixture's message bytes (table MSG, MSG_LEN), read at the guest's static data, identity addresses.
    fn message() -> Vec<u8> {
        let len = usize::try_from(w(t::MSG_LEN)).expect("message length");
        // SAFETY: the fixture's static message of MSG_LEN bytes, mapped for the life of the process.
        unsafe { std::slice::from_raw_parts(w(t::MSG) as *const u8, len) }.to_vec()
    }

    /// One host thread's share of gates (m)/(n): attach, then `rounds` × (fx_sum(n) = sum, fx_tp() = its own thread pointer, fx_write_msg(fd) = MSG_LEN), detach.
    fn attached_calls(
        booted: &Booted,
        n: u64,
        sum: u64,
        fd: c_int,
        rounds: usize,
        start: Option<&Barrier>,
    ) -> Result<(), String> {
        let cpu = booted.attach()?;
        let g = cpu.guest();
        // the thread pointer: a host block of this thread, never dereferenced by fx_tp
        let tcb: Box<[u64; 8]> = Box::new([0; 8]);
        let tp = tcb.as_ptr() as u64;
        if let Some(b) = start {
            b.wait();
        }
        for round in 0..rounds {
            let (got, _) = g.call_on(tp, w(t::SUM), &[n]);
            if got != sum {
                return Err(format!(
                    "round {round}: fx_sum({n}) = {got}, expected {sum}"
                ));
            }
            let (seen, _) = g.call_on(tp, w(t::TP), &[]);
            if seen != tp {
                return Err(format!(
                    "round {round}: TPIDR_EL0 {seen:#x}, expected this thread's {tp:#x}"
                ));
            }
            let (wrote, _) = g.call_on(tp, w(t::WRITE_MSG), &[fd as u64]);
            if wrote != w(t::MSG_LEN) {
                return Err(format!(
                    "round {round}: write = {:#x}, expected {}",
                    wrote,
                    w(t::MSG_LEN)
                ));
            }
        }
        drop(cpu);
        Ok(())
    }

    /// The pipe holds exactly `writes` copies of the fixture message.
    fn check_pipe(fd: c_int, writes: usize) -> Result<(), String> {
        let got = drain(fd);
        let want = message().repeat(writes);
        if got != want {
            return Err(format!(
                "pipe holds {} bytes, expected {writes} messages ({} bytes)",
                got.len(),
                want.len()
            ));
        }
        Ok(())
    }

    /// Gate (n): the helper boot. `start` returned on this (creator) thread with BOOT's answer; the helper thread is still there, parked; this thread attaches, calls, detaches, and attaches again (a detach leaves the engine usable).
    fn gate_n(booted: &Booted) -> Result<(), String> {
        if !booted.helper_parked() {
            return Err("the boot helper thread ended".to_owned());
        }
        let fds = pipe()?;
        let r = (|| {
            attached_calls(booted, w(t::M_N1), w(t::M_S1), fds.1, 1, None)?;
            attached_calls(booted, w(t::M_N1), w(t::M_S1), fds.1, 1, None)?;
            check_pipe(fds.0, 2)
        })();
        close(fds);
        r?;
        if !booted.helper_parked() {
            return Err("the boot helper thread ended after the calls".to_owned());
        }
        Ok(())
    }

    /// Gate (m): two host threads attach and call at once, each on its own CPU, thread pointer and guest stack: fx_sum with its own n, fx_tp, fx_write_msg. Every result is the fixture's.
    fn gate_m(booted: &Booted) -> Result<(), String> {
        let fds = pipe()?;
        let start = Barrier::new(2);
        let jobs = [(w(t::M_N1), w(t::M_S1)), (w(t::M_N2), w(t::M_S2))];
        let r = std::thread::scope(|s| {
            let handles: Vec<_> = jobs
                .iter()
                .map(|&(n, sum)| {
                    let start = &start;
                    s.spawn(move || attached_calls(booted, n, sum, fds.1, M_ROUNDS, Some(start)))
                })
                .collect();
            handles
                .into_iter()
                .enumerate()
                .map(|(i, h)| {
                    h.join()
                        .unwrap_or_else(|_| Err("panicked".to_owned()))
                        .map_err(|e| format!("thread {i}: {e}"))
                })
                .collect::<Result<Vec<()>, String>>()
        })
        .and_then(|_| check_pipe(fds.0, jobs.len() * M_ROUNDS));
        close(fds);
        r
    }

    /// Gate (o): the guest's write(2) on a host heap buffer (glibc malloc, never mapped for the guest). Under identity mapping QEMU's `lock_user` accepts a range no QEMU page covers (linux-user/qemu.h, patch 0002), so the host does the copy: the write returns the length and the bytes reach the pipe.
    fn gate_o(booted: &Booted) -> Result<(), String> {
        let fds = pipe()?;
        // a Box, not an array on the stack: the buffer must be glibc heap
        let heap = Box::new([0xa5u8; O_LEN]);
        let at = heap.as_ptr() as u64;
        let r = (|| {
            let cpu = booted.attach()?;
            let g = cpu.guest();
            let tcb: Box<[u64; 8]> = Box::new([0; 8]);
            let flags = g.page_flags(at);
            let (r, _) = g.call_on(
                tcb.as_ptr() as u64,
                w(t::WRITE),
                &[fds.1 as u64, at, O_LEN as u64],
            );
            drop(cpu);
            let got = drain(fds.0);
            println!(
                "spike: (o) observed write(pipe, heap {at:#x}, {O_LEN}) = {} with QEMU page flags {flags:#x}; {} bytes reached the pipe",
                r as i64,
                got.len()
            );
            let bytes_ok = got.len() == O_LEN && got.iter().all(|&b| b == 0xa5);
            if r != O_LEN as u64 || !bytes_ok {
                return Err(format!(
                    "write returned {}, expected {O_LEN} bytes of 0xa5 ({} reached the pipe)",
                    r as i64,
                    got.len()
                ));
            }
            Ok(())
        })();
        close(fds);
        r
    }

    /// Gate (p): a write(2) whose buffer starts in a page QEMU maps (registered here, read-write) and runs into the next page, which no QEMU page covers (host memory, mapped). The per-page access_ok accepts the range; the whole-range form (break `whole-range`) refuses it with EFAULT.
    fn gate_p(booted: &Booted) -> Result<(), String> {
        let fds = pipe()?;
        // SAFETY: sysconf has no memory effects
        let psize = unsafe { libc::sysconf(libc::_SC_PAGESIZE) as usize };
        // SAFETY: a fresh anonymous mapping of two pages; it stays mapped, since QEMU's page table still names its first page
        let region = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                2 * psize,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        if region == libc::MAP_FAILED {
            close(fds);
            return Err("mmap of two pages failed".to_owned());
        }
        let region = region as usize;
        let r = (|| {
            let cpu = booted.attach()?;
            let g = cpu.guest();
            // the first page is QEMU's; the second is host memory QEMU never maps
            g.map_pages(region, psize, page::READ | page::WRITE)?;
            let at = region + psize - P_LEN / 2;
            // SAFETY: the mapping covers [at, at + P_LEN)
            unsafe { std::ptr::write_bytes(at as *mut u8, 0x3c, P_LEN) };
            let flags = (
                g.page_flags(at as u64),
                g.page_flags((region + psize) as u64),
            );
            let tcb: Box<[u64; 8]> = Box::new([0; 8]);
            let (r, _) = g.call_on(
                tcb.as_ptr() as u64,
                w(t::WRITE),
                &[fds.1 as u64, at as u64, P_LEN as u64],
            );
            drop(cpu);
            let got = drain(fds.0);
            println!(
                "spike: (p) observed write(pipe, straddle {at:#x}, {P_LEN}) = {} with QEMU page flags {:#x} then {:#x}; {} bytes reached the pipe",
                r as i64,
                flags.0,
                flags.1,
                got.len()
            );
            let bytes_ok = got.len() == P_LEN && got.iter().all(|&b| b == 0x3c);
            if r != P_LEN as u64 || !bytes_ok {
                return Err(format!(
                    "write returned {}, expected {P_LEN} bytes of 0x3c ({} reached the pipe)",
                    r as i64,
                    got.len()
                ));
            }
            Ok(())
        })();
        close(fds);
        r
    }

    /// Gate (k) in its child: a host-only fault while the previous SIGSEGV action is SIG_IGN. A synchronous fault cannot be ignored, so the child must end by SIGSEGV; surviving the store exits 3.
    fn gate_k() -> ! {
        // SAFETY: deliberate null store, the fault under test.
        unsafe { std::ptr::null_mut::<u64>().write_volatile(1) };
        // SAFETY: not reached unless the store did not fault.
        unsafe { libc::_exit(3) }
    }

    /// Gate (k), first: a child sets SIG_IGN before QEMU captures the previous action, so its BOOT trap faults with that action. The verdict waits in `K_RESULT` for `gates`; the parent then runs its own gates.
    fn run_gate_k(so: &std::ffi::OsStr, fixture: &str) {
        // SAFETY: fork before QEMU starts, so the child has one thread.
        let pid = unsafe { libc::fork() };
        let result = if pid < 0 {
            Err("fork failed".to_owned())
        } else if pid == 0 {
            // SAFETY: a zeroed sigaction with SIG_IGN; the chain records it as the user action.
            unsafe {
                let mut act: libc::sigaction = std::mem::zeroed();
                act.sa_sigaction = libc::SIG_IGN;
                libc::sigaction(libc::SIGSEGV, &act, std::ptr::null_mut());
            }
            GATE_K_CHILD.store(true, Ordering::Relaxed);
            let r = atl_qemu_engine::start(
                std::path::Path::new(so),
                &["qemu-aarch64", "-B", "0", fixture],
                on_trap,
            )
            .map(|b| b.answer());
            println!("spike: FAIL gate (k) child: the boot ended {r:?}");
            // SAFETY: async-signal-safe exit, no unwinding.
            unsafe { libc::_exit(2) }
        } else {
            match wait_signal(pid) {
                Ok(libc::SIGSEGV) => Ok(()),
                Ok(sig) => Err(format!("child died by signal {sig}, expected SIGSEGV")),
                Err(e) => Err(e),
            }
        };
        let _ = K_RESULT.set(result);
    }
}

fn main() {
    #[cfg(feature = "qemu")]
    spike::main();
    #[cfg(not(feature = "qemu"))]
    println!("spike: SKIP (feature qemu off)");
}
