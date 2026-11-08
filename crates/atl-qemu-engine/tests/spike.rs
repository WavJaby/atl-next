// SPDX-License-Identifier: MIT
//! Spike gates (a)-(h) (QEMU bridge plan Q2/Q3), harness-less: QEMU's `main` owns this thread and the guest's exit ends the process with the BOOT answer (0 = every gate ok).
//! Env (xtask qemu_spike.rs sets it; without it the run prints SKIP): `ATL_QEMU_SPIKE_SO` = the spike library, `ATL_QEMU_SPIKE_FIXTURE` = the static guest program, `ATL_QEMU_SIGCHAIN_OBJ` = AOSP libsigchain's object (build.rs links it and sets cfg `atl_qe_sigchain`). Args: `--guest-base <n>` (QEMU's `-B`, default 0 = identity).
//! Expected values come from the fixture's table (fixture.c), never from the engine. One line per gate: `spike: (x) ok` or `spike: (x) FAIL <why>`.
//! Gates: (a) trap arguments, (b) nested host/guest calls, (c) host memory, (d) guest store fault, (e) host fault order behind sigchain, (h) implicit null check through sigchain, (f) write into translated code, (g) second guest thread, (i) fatal guest fault on one thread, (j) the same on a second thread (both end by SIGSEGV), (k) host fault with previous action SIG_IGN (ends by SIGSEGV).
#![allow(clippy::print_stdout, reason = "test harness output is its verdict")]

#[cfg(feature = "qemu")]
mod spike {
    use std::cell::{Cell, RefCell};
    use std::ffi::c_int;
    use std::sync::Mutex;
    use std::sync::OnceLock;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    use atl_qemu_engine::{Guest, Trap};

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
        pub const LEN: usize = 28;
    }

    /// What the host answers a CALL with during gate (a): any value the guest cannot produce itself.
    const A_ANSWER: u64 = 1000;

    thread_local! {
        static TABLE: RefCell<Vec<u64>> = const { RefCell::new(Vec::new()) };
        static GATE: Cell<char> = const { Cell::new(' ') };
        static A_SEEN: Cell<Option<[u64; 8]>> = const { Cell::new(None) };
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
        let r = atl_qemu_engine::run(
            std::path::Path::new(&so),
            &["qemu-aarch64", "-B", &base, &fixture],
            on_trap,
        );
        println!("spike: FAIL QEMU main returned {r:?}");
        std::process::exit(2);
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
        TABLE.with(|t| t.borrow()[i])
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
        TABLE.with(|t| *t.borrow_mut() = words);
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
            let r = atl_qemu_engine::run(
                std::path::Path::new(so),
                &["qemu-aarch64", "-B", "0", fixture],
                on_trap,
            );
            println!("spike: FAIL gate (k) child: QEMU main returned {r:?}");
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
