// SPDX-License-Identifier: MIT
//! Spike gates (QEMU bridge plan Q2/Q3, Q5-2), harness-less: `start` boots QEMU on its helper thread, whose BOOT trap runs gates (a)-(l) there and parks; the host threads then run (n), (u), (m), (o), (p), (s), (t), (v) on their own guest CPUs, and the process exits 0 when every gate is ok.
//! Env (xtask qemu_spike.rs sets it; without it the run prints SKIP): `ATL_QEMU_SPIKE_SO` = the spike library, `ATL_QEMU_SPIKE_FIXTURE` = the static guest program, `ATL_QEMU_SIGCHAIN_OBJ` = AOSP libsigchain's object (build.rs links it and sets cfg `atl_qe_sigchain`). Args: `--guest-base <n>` (QEMU's `-B`, default 0 = identity).
//! Expected values come from the fixture's table (fixture.c), never from the engine. One line per gate: `spike: (x) ok` or `spike: (x) FAIL <why>`.
//! Gates: (a) trap arguments, (b) nested host/guest calls, (c) host memory, (d) guest store fault, (e) host fault order behind sigchain, (h) implicit null check through sigchain, (f) write into translated code, (g) second guest thread, (i) fatal guest fault on one thread, (j) the same on a second thread (both end by SIGSEGV), (k) host fault with previous action SIG_IGN (ends by SIGSEGV), (l) binding thunks from the guest pool (bit 15 of the svc immediate, id = slot); (n) helper boot: `start` returned with the BOOT answer, the helper stays parked, the creator thread attaches and calls; (m) two host threads attach at once and call concurrently, each with its own thread pointer and syscalls; (o) a write(2) on host heap memory, accepted under identity mapping (access_ok patch 0002), (p) a write straddling a QEMU-mapped page into host memory QEMU does not map; (s) four attached threads fill their guest stacks across one barrier and find their own words intact; (t) a guest store into the guard page below an attached CPU's stack is a guest SIGSEGV at that address (forked child); (u) 50 host threads attach, call and detach while another attached thread's guest loop churns guest mmap/munmap, translation and host calls (deadline: a hang prints FAIL); (v) two attached threads, both alive, each overflow their guest stack into its guard: an SA_ONSTACK SIGSEGV handler runs on the thread's own alternate stack (32 KiB, disjoint from the other's), sees SS_ONSTACK and EPERM there, siglongjmps out (forked child: with no alternate stack QEMU kills it); (x) a guest SA_SIGINFO handler leaving by siglongjmp gets its si_addr through the engine's special handler, the host mask survives the no-return exit, and later host faults on that thread still reach the previous action and ART's special; (y) a host `sigaction(SIGSEGV)` after boot does not take guest faults; (w) a host `ud2` / `abort()` on a thread with no CPU ends by SIGILL / SIGABRT, and the guest's own SIGABRT (handled and unhandled) still works.
//! Signals: the harness registers on the linked sigchain what ART does -- an ART FaultManager stand-in, then the engine's special (`signal::special`) with a full mask and `SIGCHAIN_ALLOW_NORETURN` -- and sets the pre-QEMU user action.
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
        pub const STACK_FILL: usize = 40;
        pub const S_WORDS: usize = 41;
        pub const GUARD_STORE: usize = 42;
        pub const T_SURVIVED: usize = 43;
        pub const CHURN: usize = 44;
        pub const SP: usize = 45;
        pub const X_FN: usize = 46;
        pub const X_ADDR: usize = 47;
        pub const OVERFLOW: usize = 48;
        pub const V_LONGJMP: usize = 49;
        pub const V_ALT_SIZE: usize = 50;
        pub const V_ONSTACK: usize = 51;
        pub const V_EPERM: usize = 52;
        pub const V_ENOMEM: usize = 53;
        pub const ABORT_CATCH: usize = 54;
        pub const W_CAUGHT: usize = 55;
        pub const ABORT: usize = 56;
        pub const LEN: usize = 57;
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

    /// Gate (s): host threads, each on its own CPU and guest stack. Four, so a shared stack has three losers rather than one coin flip.
    const S_THREADS: usize = 4;
    /// Gate (t) child: exit status when the guard fault reported an address other than the store's.
    const T_ADDR_EXIT: c_int = 47;
    /// Gate (y) child: the late host handler took the host fault, after the guest fault went to the guest (the pass).
    const Y_OK: c_int = 48;
    /// Gate (y) child: the late host handler took the guest fault.
    const Y_STOLE: c_int = 49;
    /// Gate (y) child: fx_sig_recover answered another si_addr (the guest handler did not run as expected).
    const Y_MISSED: c_int = 50;
    /// Gate (y) child: its host store did not fault.
    const Y_SURVIVED: c_int = 51;

    /// Gate (y) child: set before its host fault, so the late handler can tell the host fault from a stolen guest one.
    static Y_HOST_PHASE: AtomicBool = AtomicBool::new(false);

    /// Gate (u): host threads that attach, call and detach while the parent's guest loop runs. 50: the slice's count, enough that attaches land across many parent iterations.
    const U_CHILDREN: usize = 50;
    /// Gate (u): wall-clock bound on the whole gate; past it the watchdog prints the FAIL line and ends the process (a hang). Measured gate time is far below (log line `(u) observed`).
    const U_DEADLINE: std::time::Duration = std::time::Duration::from_secs(60);
    /// Gate (u): the parent's next expected CALL(i); its CALL answers 0 (stop) once `U_STOP` is set.
    static U_NEXT: AtomicU64 = AtomicU64::new(0);
    static U_SEQ_BAD: AtomicU64 = AtomicU64::new(0);
    static U_STOP: AtomicBool = AtomicBool::new(false);

    /// Gate (s): every thread's fx_stack_fill holds here (its CALL) until all have filled.
    static S_BARRIER: Barrier = Barrier::new(S_THREADS);
    /// Gate (t) child: the address the guest store must fault at (stack base - 8).
    static T_ADDR: AtomicU64 = AtomicU64::new(0);

    /// The fixture's table, from its BOOT trap; read by every thread.
    static TABLE: OnceLock<Vec<u64>> = OnceLock::new();

    thread_local! {
        static GATE: Cell<char> = const { Cell::new(' ') };
        static A_SEEN: Cell<Option<[u64; 8]>> = const { Cell::new(None) };
        /// Gate (s): the guest buffer address fx_stack_fill passed in its CALL.
        static S_BUF: Cell<u64> = const { Cell::new(0) };
        /// Gate (v): this thread's fx_overflow reports, in the fixture's order.
        static V_REPORTS: RefCell<Vec<u64>> = const { RefCell::new(Vec::new()) };
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

        /// `SIGCHAIN_ALLOW_NORETURN` (art/sigchainlib/sigchain.h): the handler may leave by longjmp. Break `no-noreturn` (gate x) drops it.
        const ALLOW_NORETURN: u64 = if cfg!(atl_qe_no_noreturn) { 0 } else { 0x1 };

        /// ART's two registrations before QEMU's `main`, in ART's order: its FaultManager claims SIGSEGV and SIGBUS (fault_handler.cc, empty mask, no flags), then `InitializeNativeBridge` adds the bridge's `getSignalHandler` answer -- the engine's special -- with a full mask and `SIGCHAIN_ALLOW_NORETURN` (native_bridge_art_interface.cc).
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
                // SAFETY: a zeroed sigset_t is a valid buffer for sigfillset.
                let mut full: libc::sigset_t = unsafe { std::mem::zeroed() };
                // SAFETY: full is a live sigset_t.
                unsafe { libc::sigfillset(&mut full) };
                let mut engine = SigchainAction {
                    sc_sigaction: atl_qemu_engine::signal::special,
                    sc_mask: full,
                    sc_flags: ALLOW_NORETURN,
                };
                // SAFETY: as above; the engine's special has the special-handler signature.
                unsafe { AddSpecialSignalHandlerFn(sig, &mut engine) };
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
            // (u) before (m): break `copy-caller` also breaks (m) by a crash, and (u) must print its own check line first
            ('u', gate_u),
            ('m', gate_m),
            ('o', gate_o),
            ('p', gate_p),
            ('s', gate_s),
            ('t', gate_t),
            ('v', gate_v),
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
                's' => {
                    S_BUF.with(|b| b.set(regs[0]));
                    S_BARRIER.wait();
                    0
                }
                // gate (u) parent: fx_churn's CALL(i) must count 0, 1, 2, ... (an uninterrupted loop); answer go on until the children are done
                'u' => {
                    if U_NEXT.fetch_add(1, Ordering::SeqCst) != regs[0] {
                        U_SEQ_BAD.fetch_add(1, Ordering::SeqCst);
                    }
                    u64::from(!U_STOP.load(Ordering::SeqCst))
                }
                // gate (u) child: fx_stack_fill's buffer, no barrier
                'v' => {
                    S_BUF.with(|b| b.set(regs[0]));
                    0
                }
                other => panic!("CALL during gate {other:?}"),
            },
            Trap::Report(v) if GATE.with(Cell::get) == 't' => {
                let want = T_ADDR.load(Ordering::Relaxed);
                println!("spike: (t) child: guest SIGSEGV at {v:#x}, store at {want:#x}");
                if v != want {
                    // SAFETY: the forked child ends here; the parent reads the status.
                    unsafe { libc::_exit(T_ADDR_EXIT) };
                }
                0
            }
            Trap::Report(v) if GATE.with(Cell::get) == 'V' => {
                V_REPORTS.with(|r| r.borrow_mut().push(v));
                0
            }
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
            ('x', gate_x),
            ('y', gate_y),
            ('w', gate_w),
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

    /// This thread's blocked-signal mask, read with the raw syscall (one kernel sigset word), so sigchain's filter does not touch the answer.
    fn host_mask() -> u64 {
        let mut old = 0u64;
        // SAFETY: rt_sigprocmask(SIG_BLOCK, NULL, &old, 8): a query; old is one kernel sigset word.
        unsafe {
            libc::syscall(
                libc::SYS_rt_sigprocmask,
                libc::c_long::from(libc::SIG_BLOCK),
                std::ptr::null::<u64>(),
                &raw mut old,
                8usize,
            )
        };
        old
    }

    /// fx_sig_recover's answer must be the fault address (table X_ADDR); its X_NONE / X_FAILED answers are named.
    fn check_recover(seen: u64) -> Result<(), String> {
        if seen == w(t::X_ADDR) {
            return Ok(());
        }
        Err(format!(
            "guest handler saw si_addr {seen:#x}, expected {:#x} (fixture: 1 = no fault, 2 = sigaction failed)",
            w(t::X_ADDR)
        ))
    }

    /// Gate (x), the special route (Q7-1): a guest SA_SIGINFO handler that leaves by siglongjmp gets si_addr = the store's address, through one code-route fault. After QEMU's no-return exit from the special (sigchain set a full `sc_mask` for it) SIGSEGV and SIGBUS are unblocked and the host mask gained no standard signal; it may gain the guest's own blocked realtime signals, which QEMU mirrors. Then, on this thread in forked children: a host fault still reaches the previous action after both specials declined it (the restored user action), and ART's special still claims an implicit null check (sigchain's per-thread handling flag was not left set by the longjmp).
    fn gate_x(g: Guest) -> Result<(), String> {
        let mask_before = host_mask();
        let before = atl_qemu_engine::signal::routed().code;
        let seen = g.call(w(t::X_FN), &[w(t::X_ADDR)]);
        let faults = atl_qemu_engine::signal::routed().code - before;
        let mask_after = host_mask();
        let guest_mask = reports()
            .first()
            .map_or_else(|| "not reported".to_owned(), |m| format!("{m:#x}"));
        println!(
            "spike: (x) guest handler saw si_addr {seen:#x}; host mask {mask_before:#x} before, {mask_after:#x} after; guest blocked set after {guest_mask}"
        );
        check_recover(seen)?;
        if faults != 1 {
            return Err(format!(
                "{faults} translated-code faults routed to QEMU's handler, expected 1"
            ));
        }
        // a leaked sc_mask (sigchain's full set, set raw) blocks every signal, SIGSEGV and SIGBUS included; QEMU itself sets the host mask to the guest's blocked set minus SIGSEGV/SIGBUS (process_pending_signals), and guest realtime signals map to host realtime ones (signal_table_init)
        let fault_bits = (1u64 << (libc::SIGSEGV - 1)) | (1u64 << (libc::SIGBUS - 1));
        let realtime = !0u64 << (libc::SIGRTMIN() - 1);
        let gained = mask_after & !mask_before;
        if mask_after & fault_bits != 0 || gained & !realtime != 0 {
            return Err(format!(
                "host signal mask {mask_after:#x} after the guest's no-return exit, {mask_before:#x} before: SIGSEGV/SIGBUS blocked or a standard signal gained"
            ));
        }
        let pid = fork_child()?;
        if pid == 0 {
            // SAFETY: deliberate null store, the fault under test.
            unsafe { std::ptr::null_mut::<u64>().write_volatile(1) };
            // SAFETY: not reached unless the store did not fault.
            unsafe { libc::_exit(0) };
        }
        match wait_child(pid) {
            Ok(PREVIOUS_EXIT) => {}
            Ok(ORDER_EXIT) => {
                return Err(
                    "host fault after the guest fault skipped the special handlers (sigchain's handling flag left set)"
                        .to_owned(),
                );
            }
            Ok(code) => {
                return Err(format!(
                    "host fault after the guest fault: child exit {code}, expected the previous action's {PREVIOUS_EXIT}"
                ));
            }
            Err(e) => {
                return Err(format!(
                    "host fault after the guest fault did not reach the previous action: {e}"
                ));
            }
        }
        let pid = fork_child()?;
        if pid == 0 {
            // SAFETY: deliberate null load in the probe; a claim does not return here.
            unsafe { atl_managed_probe(std::ptr::null_mut()) };
            // SAFETY: not reached unless the load did not fault.
            unsafe { libc::_exit(0) };
        }
        match wait_child(pid)? {
            NPE_EXIT => Ok(()),
            code => Err(format!(
                "implicit null check after the guest fault: child exit {code}, expected ART's claim ({NPE_EXIT})"
            )),
        }
    }

    /// Gate (y) child: the host's own SIGSEGV handler, installed after boot.
    extern "C" fn late_handler(_: c_int) {
        let code = if Y_HOST_PHASE.load(Ordering::Relaxed) {
            Y_OK
        } else {
            Y_STOLE
        };
        // SAFETY: async-signal-safe exit, no unwinding.
        unsafe { libc::_exit(code) };
    }

    /// Gate (y): a host `sigaction(SIGSEGV)` after boot (sigchain records it as the user action, as a game engine's or crash reporter's call is) does not take guest faults. In a forked child: the late handler is installed, the guest's fault still reaches the guest handler with its address, and a host fault then reaches the late handler.
    fn gate_y(g: Guest) -> Result<(), String> {
        let pid = fork_child()?;
        if pid == 0 {
            // SAFETY: a plain handler for SIGSEGV; zeroed mask and flags are valid.
            unsafe {
                let mut act: libc::sigaction = std::mem::zeroed();
                act.sa_sigaction = late_handler as *const () as usize;
                libc::sigaction(libc::SIGSEGV, &act, std::ptr::null_mut());
            }
            if g.call(w(t::X_FN), &[w(t::X_ADDR)]) != w(t::X_ADDR) {
                // SAFETY: the forked child ends here.
                unsafe { libc::_exit(Y_MISSED) };
            }
            Y_HOST_PHASE.store(true, Ordering::Relaxed);
            // SAFETY: deliberate null store, the host fault under test.
            unsafe { std::ptr::null_mut::<u64>().write_volatile(1) };
            // SAFETY: not reached unless the store did not fault.
            unsafe { libc::_exit(Y_SURVIVED) };
        }
        match wait_child(pid)? {
            Y_OK => Ok(()),
            Y_STOLE => Err("the host's sigaction after boot took the guest fault".to_owned()),
            Y_MISSED => Err("the guest handler did not see its fault address".to_owned()),
            Y_SURVIVED => Err("the host store did not fault".to_owned()),
            code => Err(format!("child exit {code}, expected {Y_OK}")),
        }
    }

    /// Gate (w) child: a host thread that never attached a CPU runs `f`; the child dies by whatever `f` raises.
    fn host_thread_child(f: fn()) -> Result<c_int, String> {
        let pid = fork_child()?;
        if pid == 0 {
            // the child's only thread is this one (fork); the new thread is a plain host thread
            let _ = std::thread::spawn(f).join();
            // SAFETY: not reached unless f returned.
            unsafe { libc::_exit(0) };
        }
        wait_signal(pid)
    }

    /// Gate (w), host traps (Q7-3), in forked children on a host thread with no CPU: a `ud2` (Rust's abort intrinsic, C `__builtin_trap`) must end by SIGILL under the pre-QEMU action (here the default); QEMU's handler, if still installed, reads a NULL `thread_cpu` and dies by SIGSEGV. A host `abort()` must end by SIGABRT (QEMU never takes host SIGABRT: it remaps the guest's). Then the guest's own SIGABRT still works: a guest handler catches its raise(SIGABRT), and a guest abort() with no handler ends a forked child by SIGABRT.
    fn gate_w(g: Guest) -> Result<(), String> {
        let cases: [(&str, fn(), c_int); 2] = [
            // SAFETY: the trap under test.
            ("ud2", || unsafe { std::arch::asm!("ud2") }, libc::SIGILL),
            // SAFETY: the abort under test.
            ("abort()", || unsafe { libc::abort() }, libc::SIGABRT),
        ];
        for (what, f, want) in cases {
            match host_thread_child(f) {
                Ok(sig) if sig == want => {}
                Ok(sig) => {
                    return Err(format!(
                        "host thread {what}: child died by signal {sig}, expected {want}"
                    ));
                }
                Err(e) => return Err(format!("host thread {what}: {e}")),
            }
        }
        let caught = g.call(w(t::ABORT_CATCH), &[]);
        if caught != w(t::W_CAUGHT) {
            return Err(format!(
                "guest raise(SIGABRT): fx_abort_catch returned {caught:#x}, expected {:#x} (fixture: 1 = handler did not run, 2 = sigaction failed)",
                w(t::W_CAUGHT)
            ));
        }
        let pid = fork_child()?;
        if pid == 0 {
            g.call(w(t::ABORT), &[]);
            // SAFETY: not reached unless the guest survived abort().
            unsafe { libc::_exit(0) };
        }
        match wait_signal(pid) {
            Ok(libc::SIGABRT) => Ok(()),
            Ok(sig) => Err(format!(
                "guest abort(): child died by signal {sig}, expected SIGABRT"
            )),
            Err(e) => Err(format!("guest abort(): {e}")),
        }
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

    /// One gate-(s) thread: attach, fx_stack_fill(pattern) -> (stack base, top, guest buffer, mismatched words).
    fn stack_fill(booted: &Booted, pattern: u64) -> Result<(usize, usize, u64, u64), String> {
        let cpu = booted.attach()?;
        GATE.with(|c| c.set('s'));
        let tcb: Box<[u64; 8]> = Box::new([0; 8]);
        let (bad, _) = cpu
            .guest()
            .call_on(tcb.as_ptr() as u64, w(t::STACK_FILL), &[pattern]);
        let r = (
            cpu.stack_base(),
            cpu.stack_top(),
            S_BUF.with(Cell::get),
            bad,
        );
        drop(cpu);
        Ok(r)
    }

    /// Gate (s): S_THREADS host threads attach; each fills S_WORDS words of its guest stack with its own pattern ((i + 1) << 32, so no two threads' words coincide), all wait at one barrier inside the call, then each counts its words that changed. Expected: 0 mismatches each, every buffer inside its own CPU's [base, top), the stacks disjoint.
    fn gate_s(booted: &Booted) -> Result<(), String> {
        let words = w(t::S_WORDS);
        let seen = std::thread::scope(|s| {
            let handles: Vec<_> = (0..S_THREADS)
                .map(|i| s.spawn(move || stack_fill(booted, (i as u64 + 1) << 32)))
                .collect();
            handles
                .into_iter()
                .enumerate()
                .map(|(i, h)| {
                    h.join()
                        .unwrap_or_else(|_| Err("panicked".to_owned()))
                        .map_err(|e| format!("thread {i}: {e}"))
                })
                .collect::<Result<Vec<_>, String>>()
        })?;
        for (i, &(base, top, buf, bad)) in seen.iter().enumerate() {
            println!(
                "spike: (s) thread {i}: stack [{base:#x}, {top:#x}), buffer {buf:#x}, {bad} of {words} words changed"
            );
        }
        for (i, &(base, top, buf, bad)) in seen.iter().enumerate() {
            if bad != 0 {
                return Err(format!(
                    "thread {i}: {bad} of {words} stack words changed under it (expected 0)"
                ));
            }
            let end = buf + words * 8;
            if buf < base as u64 || end > top as u64 {
                return Err(format!(
                    "thread {i}: buffer [{buf:#x}, {end:#x}) outside its stack [{base:#x}, {top:#x})"
                ));
            }
            for (j, &(b2, t2, ..)) in seen.iter().enumerate().skip(i + 1) {
                if base < t2 && b2 < top {
                    return Err(format!(
                        "threads {i} and {j} share stack memory: [{base:#x}, {top:#x}) and [{b2:#x}, {t2:#x})"
                    ));
                }
            }
        }
        Ok(())
    }

    /// Gate (t): in a forked child, an attached CPU stores at its stack base - 8, inside the guard page. Expected: a guest SIGSEGV whose si_addr is that address (the child's REPORT check), then death by SIGSEGV once the handler restored SIG_DFL. A child that exits T_SURVIVED's status 0 stored into the guard.
    fn gate_t(booted: &Booted) -> Result<(), String> {
        let pid = fork_child()?;
        if pid == 0 {
            GATE.with(|c| c.set('t'));
            let code = match booted.attach() {
                Ok(cpu) => {
                    let at = cpu.stack_base() as u64 - 8;
                    T_ADDR.store(at, Ordering::Relaxed);
                    let tcb: Box<[u64; 8]> = Box::new([0; 8]);
                    let (r, _) = cpu
                        .guest()
                        .call_on(tcb.as_ptr() as u64, w(t::GUARD_STORE), &[at]);
                    println!(
                        "spike: (t) child: store at {at:#x} returned {r} (T_SURVIVED {})",
                        w(t::T_SURVIVED)
                    );
                    0
                }
                Err(e) => {
                    println!("spike: (t) child: attach failed: {e}");
                    1
                }
            };
            // SAFETY: the child ends without running the parent's exit path.
            unsafe { libc::_exit(code) };
        }
        let mut status = 0;
        // SAFETY: waits for the child just forked.
        if unsafe { libc::waitpid(pid, &mut status, 0) } != pid {
            return Err("waitpid failed".to_owned());
        }
        if libc::WIFSIGNALED(status) && libc::WTERMSIG(status) == libc::SIGSEGV {
            return Ok(());
        }
        Err(
            match (libc::WIFSIGNALED(status), libc::WEXITSTATUS(status)) {
                (true, _) => format!(
                    "child died by signal {}, expected SIGSEGV",
                    libc::WTERMSIG(status)
                ),
                (false, T_ADDR_EXIT) => "the guard fault reported another address".to_owned(),
                (false, 0) => "the store below the stack succeeded: no guard page".to_owned(),
                (false, c) => format!("child exited {c}, expected death by SIGSEGV"),
            },
        )
    }

    /// Gate (v): attached threads, alive together, so a shared or reused alternate stack shows as overlapping ranges.
    const V_THREADS: usize = 2;
    /// Gate (v): fx_overflow's REPORT count (fixture.c order: ENOMEM, ss_sp, ss_size, ss_flags; then in the handler si_addr, sp, ss_flags, EPERM).
    const V_REPORT_COUNT: usize = 8;
    /// Gate (v) child: exit status when a check line printed FAIL.
    const V_CHECK_EXIT: c_int = 48;

    /// One gate-(v) thread's facts: guest stack (base, guard), engine alt stack (base, len), fx_overflow's return and reports.
    struct VRun {
        stack: (usize, usize),
        alt: (usize, usize),
        ret: u64,
        reports: Vec<u64>,
    }

    /// One gate-(v) thread: attach, wait until every thread is attached, run fx_overflow one thread at a time (the guest's jmp_buf is a global), wait again so no CPU detaches (and frees its stacks) before all ran.
    fn overflow_thread(
        booted: &Booted,
        attached: &Barrier,
        turn: &Mutex<()>,
    ) -> Result<VRun, String> {
        GATE.with(|c| c.set('V'));
        let cpu = booted.attach();
        attached.wait();
        // both waits happen on every path, so a failed attach does not strand the other thread at the second
        let ret = cpu.as_ref().ok().map(|cpu| {
            let _turn = turn
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let tcb: Box<[u64; 8]> = Box::new([0; 8]);
            cpu.guest()
                .call_on(tcb.as_ptr() as u64, w(t::OVERFLOW), &[])
                .0
        });
        attached.wait();
        let (cpu, ret) = (cpu?, ret.unwrap_or_default());
        Ok(VRun {
            stack: (cpu.stack_base(), cpu.stack_guard()),
            alt: cpu.alt_stack(),
            ret,
            reports: V_REPORTS.with(|r| r.borrow().clone()),
        })
    }

    /// Gate (v) checks for thread `i` against the fixture's table; one FAIL line per miss.
    fn check_overflow(i: usize, r: &VRun) -> bool {
        let (base, guard) = r.stack;
        let (alt, alt_len) = r.alt;
        let size = w(t::V_ALT_SIZE);
        if r.reports.len() != V_REPORT_COUNT {
            println!(
                "spike: (v) child FAIL thread {i}: {} reports, expected {V_REPORT_COUNT}; fx_overflow = {}",
                r.reports.len(),
                r.ret
            );
            return false;
        }
        let p = &r.reports;
        let checks = [
            (
                r.ret == w(t::V_LONGJMP),
                format!(
                    "fx_overflow = {}, expected the longjmp return {}",
                    r.ret,
                    w(t::V_LONGJMP)
                ),
            ),
            (
                p[0] == w(t::V_ENOMEM),
                format!(
                    "sigaltstack below MINSIGSTKSZ = {}, expected -ENOMEM",
                    p[0] as i64
                ),
            ),
            (
                p[2] == size,
                format!(
                    "alternate stack ss_size = {}, expected bionic's {size}",
                    p[2]
                ),
            ),
            (
                p[1] == alt as u64 && alt_len as u64 == p[2],
                format!(
                    "guest ss_sp {:#x} size {} != engine's [{alt:#x}, +{alt_len})",
                    p[1], p[2]
                ),
            ),
            (
                p[3] == 0,
                format!("ss_flags outside the handler = {}, expected 0", p[3]),
            ),
            (
                p[4] >= (base - guard) as u64 && p[4] < base as u64,
                format!(
                    "si_addr {:#x} outside the guard [{:#x}, {base:#x})",
                    p[4],
                    base - guard
                ),
            ),
            (
                p[5] >= p[1] && p[5] < p[1] + p[2],
                format!(
                    "handler sp {:#x} outside the alternate stack [{:#x}, {:#x})",
                    p[5],
                    p[1],
                    p[1] + p[2]
                ),
            ),
            (
                p[6] == w(t::V_ONSTACK),
                format!("ss_flags in the handler = {}, expected SS_ONSTACK", p[6]),
            ),
            (
                p[7] == w(t::V_EPERM),
                format!(
                    "sigaltstack on the alternate stack = {}, expected -EPERM",
                    p[7] as i64
                ),
            ),
        ];
        let mut ok = true;
        for (pass, why) in checks {
            if !pass {
                println!("spike: (v) child FAIL thread {i}: {why}");
                ok = false;
            }
        }
        if ok {
            println!(
                "spike: (v) child: thread {i} fault {:#x} in guard below {base:#x}, handler sp {:#x} on alt [{alt:#x}, +{alt_len:#x})",
                p[4], p[5]
            );
        }
        ok
    }

    /// Gate (v), Q7-2: in a forked child, V_THREADS attached threads overflow their guest stacks into the guard; each SA_ONSTACK SIGSEGV handler must run on its own thread's alternate stack (attach registers one per CPU, D-Q7-2) and siglongjmp out. Without an alternate stack QEMU cannot build the frame on the overflowed stack and kills the child by SIGSEGV (force_sigsegv).
    fn gate_v(booted: &Booted) -> Result<(), String> {
        let pid = fork_child()?;
        if pid == 0 {
            let attached = Barrier::new(V_THREADS);
            let turn = Mutex::new(());
            let runs: Vec<Result<VRun, String>> = std::thread::scope(|s| {
                let hs: Vec<_> = (0..V_THREADS)
                    .map(|_| s.spawn(|| overflow_thread(booted, &attached, &turn)))
                    .collect();
                hs.into_iter()
                    .map(|h| h.join().unwrap_or_else(|_| Err("panicked".to_owned())))
                    .collect()
            });
            let mut ok = true;
            let mut ranges = Vec::new();
            for (i, r) in runs.iter().enumerate() {
                match r {
                    Ok(r) => {
                        ok &= check_overflow(i, r);
                        ranges.push((i, "alt", r.alt.0, r.alt.0 + r.alt.1));
                        ranges.push((i, "stack", r.stack.0 - r.stack.1, r.stack.0));
                    }
                    Err(e) => {
                        println!("spike: (v) child FAIL thread {i}: {e}");
                        ok = false;
                    }
                }
            }
            for (k, a) in ranges.iter().enumerate() {
                for b in &ranges[k + 1..] {
                    if a.2 < b.3 && b.2 < a.3 {
                        println!(
                            "spike: (v) child FAIL thread {} {} [{:#x}, {:#x}) overlaps thread {} {} [{:#x}, {:#x})",
                            a.0, a.1, a.2, a.3, b.0, b.1, b.2, b.3
                        );
                        ok = false;
                    }
                }
            }
            // SAFETY: the child ends without running the parent's exit path; its CPUs are already detached.
            unsafe { libc::_exit(if ok { 0 } else { V_CHECK_EXIT }) };
        }
        let mut status = 0;
        // SAFETY: waits for the child just forked.
        if unsafe { libc::waitpid(pid, &mut status, 0) } != pid {
            return Err("waitpid failed".to_owned());
        }
        match (libc::WIFSIGNALED(status), libc::WEXITSTATUS(status)) {
            (false, 0) => Ok(()),
            (true, _) => Err(format!(
                "child killed by signal {}: the SA_ONSTACK handler had no alternate stack to run on",
                libc::WTERMSIG(status)
            )),
            // QEMU's fatal path re-raises the host SIGSEGV, which reaches the harness's pre-QEMU handler (gate e's) and exits with its code
            (false, PREVIOUS_EXIT | ORDER_EXIT) => Err(
                "child killed: QEMU's fatal guest SIGSEGV reached the pre-QEMU handler; the SA_ONSTACK handler had no alternate stack to run on".to_owned(),
            ),
            (false, V_CHECK_EXIT) => Err("child check lines failed (above)".to_owned()),
            (false, c) => Err(format!("child exited {c}")),
        }
    }

    /// Gate (k) in its child: a host-only fault while the previous SIGSEGV action is SIG_IGN. A synchronous fault cannot be ignored, so the child must end by SIGSEGV; surviving the store exits 3.
    fn gate_k() -> ! {
        // SAFETY: deliberate null store, the fault under test.
        unsafe { std::ptr::null_mut::<u64>().write_volatile(1) };
        // SAFETY: not reached unless the store did not fault.
        unsafe { libc::_exit(3) }
    }

    /// One gate-(u) child: attach while the parent runs, fx_sp (entry sp inside its own stack, checked before any large write, so a wrong stack fails here instead of crashing on another thread's frames), fx_stack_fill(pattern), fx_sum(M_N1), detach -> (stack base, top, guest buffer, changed words, sum, parent iteration at attach).
    fn churn_child(
        booted: &Booted,
        pattern: u64,
    ) -> Result<(usize, usize, u64, u64, u64, u64), String> {
        let cpu = booted.attach()?;
        let at = U_NEXT.load(Ordering::SeqCst);
        GATE.with(|c| c.set('v'));
        let tcb: Box<[u64; 8]> = Box::new([0; 8]);
        let g = cpu.guest();
        let (base, top) = (cpu.stack_base(), cpu.stack_top());
        let (sp, _) = g.call_on(tcb.as_ptr() as u64, w(t::SP), &[]);
        if sp < base as u64 || sp >= top as u64 {
            // the CPU stays attached (never dropped): its stack may hold another thread's live frames
            std::mem::forget(cpu);
            return Err(format!(
                "entry sp {sp:#x} outside its own stack [{base:#x}, {top:#x})"
            ));
        }
        let (bad, _) = g.call_on(tcb.as_ptr() as u64, w(t::STACK_FILL), &[pattern]);
        let (sum, _) = g.call_on(tcb.as_ptr() as u64, w(t::SUM), &[w(t::M_N1)]);
        let r = (
            cpu.stack_base(),
            cpu.stack_top(),
            S_BUF.with(Cell::get),
            bad,
            sum,
            at,
        );
        drop(cpu);
        Ok(r)
    }

    /// Gate (u), G5 (R10 S4): a parent host thread attaches and runs fx_churn (guest mmap/munmap of RWX pages, a translation on each, guest calls, one host CALL per iteration); once it runs, U_CHILDREN host threads attach at once, each runs fx_stack_fill with its own pattern and fx_sum, and detaches. Attach copies the parked boot CPU under ATTACH + qemu_cpu_list_lock + mmap_lock (attach.rs); this measures that argument.
    /// Expected: every child 0 changed words, buffer inside its own stack, fx_sum = M_S1; the parent's CALLs counted 0, 1, 2, ... with no gap, it iterated while children attached and after the last detach, and fx_churn returned 0. A watchdog bounds the gate: a hang prints the FAIL line and exits.
    fn gate_u(booted: &Booted) -> Result<(), String> {
        let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
        std::thread::spawn(move || {
            if done_rx.recv_timeout(U_DEADLINE) == Err(std::sync::mpsc::RecvTimeoutError::Timeout) {
                println!(
                    "spike: (u) FAIL hang: not done after {} s (parent at iteration {})",
                    U_DEADLINE.as_secs(),
                    U_NEXT.load(Ordering::SeqCst)
                );
                // SAFETY: exit without unwinding or running the stuck threads' destructors.
                unsafe { libc::_exit(1) };
            }
        });
        let words = w(t::S_WORDS);
        let t0 = std::time::Instant::now();
        let (start, end, seen, parent) = std::thread::scope(|s| {
            let parent = s.spawn(|| -> Result<(usize, usize, u64), String> {
                let cpu = booted.attach()?;
                GATE.with(|c| c.set('u'));
                let tcb: Box<[u64; 8]> = Box::new([0; 8]);
                let (bad, _) = cpu.guest().call_on(tcb.as_ptr() as u64, w(t::CHURN), &[]);
                let r = (cpu.stack_base(), cpu.stack_top(), bad);
                drop(cpu);
                Ok(r)
            });
            // the children start only once the parent's loop runs (bounded by the watchdog)
            while U_NEXT.load(Ordering::SeqCst) == 0 && !parent.is_finished() {
                std::thread::yield_now();
            }
            let start = U_NEXT.load(Ordering::SeqCst);
            let children: Vec<_> = (0..U_CHILDREN)
                .map(|i| s.spawn(move || churn_child(booted, (i as u64 + 1) << 40)))
                .collect();
            let seen: Vec<_> = children
                .into_iter()
                .map(|h| h.join().unwrap_or_else(|_| Err("panicked".to_owned())))
                .collect();
            let end = U_NEXT.load(Ordering::SeqCst);
            // the parent must still loop after the last detach: wait for two more iterations, then stop it
            while U_NEXT.load(Ordering::SeqCst) <= end + 1 && !parent.is_finished() {
                std::thread::yield_now();
            }
            U_STOP.store(true, Ordering::SeqCst);
            let p = parent.join().unwrap_or_else(|_| Err("panicked".to_owned()));
            (start, end, seen, p)
        });
        let _ = done_tx.send(());
        let (pbase, ptop, pbad) = parent.map_err(|e| format!("parent: {e}"))?;
        let iters = U_NEXT.load(Ordering::SeqCst);
        let seq_bad = U_SEQ_BAD.load(Ordering::SeqCst);
        let ats: Vec<u64> = seen
            .iter()
            .filter_map(|c| c.as_ref().ok().map(|c| c.5))
            .collect();
        let (lo, hi) = (
            ats.iter().min().copied().unwrap_or(0),
            ats.iter().max().copied().unwrap_or(0),
        );
        println!(
            "spike: (u) observed {U_CHILDREN} children in {:?}: parent iteration {start} at the first spawn, attaches at {lo}..={hi}, {end} at the last join, {iters} in all; parent stack [{pbase:#x}, {ptop:#x}), fx_churn = {pbad}, {seq_bad} CALLs out of order",
            t0.elapsed()
        );
        for (i, c) in seen.iter().enumerate() {
            let (base, top, buf, bad, sum, _) = c.clone().map_err(|e| format!("child {i}: {e}"))?;
            let buf_end = buf + words * 8;
            if bad != 0 || sum != w(t::M_S1) || buf < base as u64 || buf_end > top as u64 {
                return Err(format!(
                    "child {i}: buffer [{buf:#x}, {buf_end:#x}) vs own stack [{base:#x}, {top:#x}), {bad} of {words} words changed, fx_sum = {sum} (expected {})",
                    w(t::M_S1)
                ));
            }
        }
        if pbad != 0 || seq_bad != 0 {
            return Err(format!(
                "parent: fx_churn counted {pbad} wrong values, {seq_bad} CALLs out of order"
            ));
        }
        if end <= start || iters <= end + 1 {
            return Err(format!(
                "no overlap: parent iteration {start} at the first spawn, {end} at the last join, {iters} in all"
            ));
        }
        Ok(())
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
    if std::env::args().any(|a| a == "--engine-so") {
        #[cfg(feature = "qemu")]
        engine_so::main();
        return;
    }
    #[cfg(feature = "qemu")]
    spike::main();
    #[cfg(not(feature = "qemu"))]
    println!("spike: SKIP (feature qemu off)");
}

/// Gate (q), the engine `.so` through its C ABI (QEMU bridge plan Q5-1a), in a process of its own: dlopen the library, refuse it unless its table speaks the host's major and fits its size, resolve the TLS exports, create the engine on the boot executable, and read the auxv QEMU built for it at BOOT. Its AT_ENTRY must equal `ATL_ENGINE_ENTRY` (xtask: llvm-readelf's e_entry of that executable).
/// Env (xtask qemu_spike.rs sets it): `ATL_ENGINE_SO` the library, `ATL_ENGINE_BOOT` the static boot executable, `ATL_ENGINE_ENTRY` the expected entry in hex.
/// BOOT also probes D9: `map` of a host page and `attach` from a host thread both succeed inside the BOOT trap.
/// Gate (r), after (q) in the same process: `attach` then `detach` of a CPU on a thread of its own.
#[cfg(feature = "qemu")]
mod engine_so {
    use std::ffi::{CStr, CString, c_void};
    use std::sync::OnceLock;
    use std::sync::atomic::{AtomicPtr, AtomicU64, Ordering};

    use atl_engine_api::{
        ABI_MAJOR, ABI_MINOR, AtlCpu, AtlEngine, AtlEngineHandle, AtlHost, AtlLanes, PROT_READ,
        TrapKind, check_engine_table,
    };

    /// The auxv pointer BOOT delivered (x0), written by the host trap on the boot helper.
    static AUXV: AtomicU64 = AtomicU64::new(0);
    /// The BOOT probe's result, written by the host trap on the boot helper.
    static BOOT_PROBE: OnceLock<Result<(), String>> = OnceLock::new();

    /// The host context: the table, and the engine handle `create` writes before BOOT.
    struct Probe {
        table: &'static AtlEngine,
        engine: AtomicPtr<AtlEngineHandle>,
    }

    /// A host page for the probe's `map`, page-aligned.
    #[repr(align(4096))]
    struct Page([u8; 4096]);
    static PAGE: Page = Page([0; 4096]);

    /// The host's trap: BOOT records x0 and runs the probe; nothing else is expected in this gate.
    unsafe extern "C" fn trap(ctx: *mut c_void, kind: u32, _id: u32, regs: *const u64) -> u64 {
        if kind == TrapKind::Boot as u32 {
            // SAFETY: regs points at the eight registers the ABI passes (atl_engine.h).
            AUXV.store(unsafe { *regs }, Ordering::SeqCst);
            // SAFETY: ctx is the leaked Probe that `run` passed as the host context.
            let _ = BOOT_PROBE.set(boot_probe(unsafe { &*ctx.cast::<Probe>() }));
        }
        0
    }

    /// The host's trap for CALL and BIND. Nothing but BOOT is expected in this probe, so it answers what `trap` answers: x0 = 0, v0 left as the guest had it.
    unsafe extern "C" fn trap_ex(ctx: *mut c_void, kind: u32, id: u32, lanes: *mut AtlLanes) {
        // SAFETY: lanes is the engine's live AtlLanes for this call (the ABI).
        let lanes = unsafe { &mut *lanes };
        // SAFETY: the same trap as `trap`, with the x lanes it reads.
        lanes.x[0] = unsafe { trap(ctx, kind, id, lanes.x.as_ptr()) };
    }

    /// D9 at BOOT: `map` a host page, then `attach` a CPU from a thread of its own, both inside the trap. Attach runs on another thread: a boot thread attaching itself would register its RCU state twice. That thread parks with its CPU, since a thread that exits while registered with RCU leaves a dangling registry node; gate (r) detaches its own.
    fn boot_probe(p: &Probe) -> Result<(), String> {
        let engine = p.engine.load(Ordering::SeqCst);
        if engine.is_null() {
            return Err("create wrote no handle before BOOT".to_owned());
        }
        let map = p.table.map.ok_or("engine table has no map")?;
        let attach = p.table.attach.ok_or("engine table has no attach")?;
        // SAFETY: engine is the handle create wrote; PAGE is a live host page of this process (the ABI's map).
        let rc = unsafe {
            map(
                engine,
                PAGE.0.as_ptr() as u64,
                PAGE.0.len() as u64,
                PROT_READ,
            )
        };
        if rc != 0 {
            return Err(format!("map inside BOOT: {rc}"));
        }
        let engine = engine as usize;
        let (tx, rx) = std::sync::mpsc::sync_channel::<i32>(1);
        std::thread::spawn(move || {
            let engine = engine as *mut AtlEngineHandle;
            let mut cpu: *mut AtlCpu = std::ptr::null_mut();
            // SAFETY: engine is live; cpu is an out parameter.
            let rc = unsafe { attach(engine, &mut cpu) };
            let _ = tx.send(rc);
            if rc == 0 {
                loop {
                    std::thread::park();
                }
            }
        });
        match rx.recv() {
            Ok(0) => Ok(()),
            Ok(rc) => Err(format!("attach inside BOOT: {rc}")),
            Err(_) => Err("attach thread ended without an answer".to_owned()),
        }
    }

    fn env(name: &str) -> Result<String, String> {
        std::env::var(name).map_err(|_| format!("{name} unset"))
    }

    fn dlsym_in(handle: *mut c_void, name: &CStr) -> Result<*mut c_void, String> {
        // SAFETY: handle is a dlopen handle that is never closed; name is NUL-terminated.
        let p = unsafe { libc::dlsym(handle, name.as_ptr()) };
        if p.is_null() {
            Err(format!("dlsym {}: not exported", name.to_string_lossy()))
        } else {
            Ok(p)
        }
    }

    /// AT_ENTRY from the auxv at `auxv` (identity addresses: the host address is the guest's).
    fn at_entry(auxv: u64) -> Result<u64, String> {
        let mut p = auxv as *const u64;
        loop {
            // SAFETY: auxv is QEMU's (type, value) array for the boot executable, ended by AT_NULL; its stack stays mapped (the boot thread is parked in BOOT).
            let (t, v) = unsafe { (*p, *p.add(1)) };
            if t == libc::AT_NULL as u64 {
                return Err("auxv has no AT_ENTRY".to_owned());
            }
            if t == libc::AT_ENTRY as u64 {
                return Ok(v);
            }
            // SAFETY: as above; the next pair is inside the array, which AT_NULL ends.
            p = unsafe { p.add(2) };
        }
    }

    fn run() -> Result<(&'static AtlEngine, *mut AtlEngineHandle), String> {
        let so = CString::new(env("ATL_ENGINE_SO")?).map_err(|e| e.to_string())?;
        let boot = CString::new(env("ATL_ENGINE_BOOT")?).map_err(|e| e.to_string())?;
        let want = u64::from_str_radix(env("ATL_ENGINE_ENTRY")?.trim_start_matches("0x"), 16)
            .map_err(|e| e.to_string())?;
        // SAFETY: so is NUL-terminated; RTLD_LOCAL keeps the library's symbols out of the global scope.
        let handle = unsafe { libc::dlopen(so.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
        if handle.is_null() {
            return Err("dlopen the engine library failed".to_owned());
        }
        let table = dlsym_in(handle, c"atl_engine")?.cast::<AtlEngine>();
        // SAFETY: the table is a static of the loaded library, read-only.
        let table = unsafe { &*table };
        check_engine_table(table)?;
        for tls in [c"thread_cpu", c"helper_retaddr"] {
            dlsym_in(handle, tls)?;
        }
        // leaked: the boot helper reads it inside BOOT and never returns from there
        let probe: &'static Probe = Box::leak(Box::new(Probe {
            table,
            engine: AtomicPtr::new(std::ptr::null_mut()),
        }));
        let host = AtlHost {
            major: ABI_MAJOR,
            minor: ABI_MINOR,
            size: std::mem::size_of::<AtlHost>() as u32,
            ctx: std::ptr::from_ref(probe).cast_mut().cast(),
            trap: Some(trap),
            trap_ex: Some(trap_ex),
        };
        let create = table.create.ok_or("engine table has no create")?;
        // SAFETY: host, boot (NUL-terminated) and the handle out parameter (in the leaked probe) outlive the call; the ABI's create.
        let rc = unsafe { create(&host, boot.as_ptr(), probe.engine.as_ptr()) };
        if rc != 0 {
            return Err(format!("create returned {rc}"));
        }
        let auxv = AUXV.load(Ordering::SeqCst);
        if auxv == 0 {
            return Err("create returned without BOOT".to_owned());
        }
        match BOOT_PROBE.get() {
            Some(Ok(())) => {}
            Some(Err(e)) => return Err(e.clone()),
            None => return Err("the BOOT probe did not run".to_owned()),
        }
        let entry = at_entry(auxv)?;
        if entry != want {
            return Err(format!(
                "AT_ENTRY {entry:#x} != llvm-readelf e_entry {want:#x}"
            ));
        }
        Ok((table, probe.engine.load(Ordering::SeqCst)))
    }

    /// Gate (r): `attach` then `detach` on a thread of its own, the thread that attached (the ABI's rule), with no guest call between. The copied CPU has not run its queued work, so detach must enter it in `plugin.cpu_ht` itself: without that, plugins/core.c:293 aborts.
    fn detach_probe(table: &AtlEngine, engine: *mut AtlEngineHandle) -> Result<(), String> {
        let attach = table.attach.ok_or("engine table has no attach")?;
        let detach = table.detach.ok_or("engine table has no detach")?;
        let engine = engine as usize;
        let (tx, rx) = std::sync::mpsc::sync_channel::<Result<(), String>>(1);
        std::thread::spawn(move || {
            let engine = engine as *mut AtlEngineHandle;
            let mut cpu: *mut AtlCpu = std::ptr::null_mut();
            // SAFETY: engine is live; cpu is an out parameter.
            let rc = unsafe { attach(engine, &mut cpu) };
            if rc != 0 {
                let _ = tx.send(Err(format!("attach for detach: {rc}")));
                return;
            }
            // SAFETY: engine is live, and cpu is the CPU this thread attached: detach runs on the attaching thread.
            unsafe { detach(engine, cpu) };
            let _ = tx.send(Ok(()));
        });
        rx.recv()
            .map_err(|_| "detach thread ended without an answer".to_owned())?
    }

    pub(super) fn main() {
        match run() {
            Ok((table, engine)) => {
                println!("spike: (q) ok");
                match detach_probe(table, engine) {
                    Ok(()) => println!("spike: (r) ok"),
                    Err(e) => {
                        println!("spike: FAIL (r) {e}");
                        std::process::exit(1);
                    }
                }
            }
            Err(e) => {
                println!("spike: FAIL (q) {e}");
                std::process::exit(1);
            }
        }
    }
}
