// SPDX-License-Identifier: MIT
//! Spike gates (a)-(e) (QEMU bridge plan Q2), harness-less: QEMU's `main` owns this thread and the guest's exit ends the process with the BOOT answer (0 = every gate ok).
//! Env (xtask qemu_spike.rs sets it; without it the run prints SKIP): `ATL_QEMU_SPIKE_SO` = the spike library, `ATL_QEMU_SPIKE_FIXTURE` = the static guest program. Args: `--guest-base <n>` (QEMU's `-B`, default 0 = identity).
//! Expected values come from the fixture's table (fixture.c), never from the engine. One line per gate: `spike: (x) ok` or `spike: (x) FAIL <why>`.
#![allow(clippy::print_stdout, reason = "test harness output is its verdict")]

#[cfg(feature = "qemu")]
mod spike {
    use std::cell::{Cell, RefCell};
    use std::ffi::c_int;

    use atl_qemu_engine::{Guest, Trap};

    /// Exit status of the pre-QEMU SIGSEGV handler (gate e): the host fault reached it.
    const PREVIOUS_EXIT: c_int = 43;

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
        pub const LEN: usize = 19;
    }

    /// What the host answers a CALL with during gate (a): any value the guest cannot produce itself.
    const A_ANSWER: u64 = 1000;

    thread_local! {
        static TABLE: RefCell<Vec<u64>> = const { RefCell::new(Vec::new()) };
        static GATE: Cell<char> = const { Cell::new(' ') };
        static A_SEEN: Cell<Option<[u64; 8]>> = const { Cell::new(None) };
        static REPORTS: RefCell<Vec<u64>> = const { RefCell::new(Vec::new()) };
    }

    fn w(i: usize) -> u64 {
        TABLE.with(|t| t.borrow()[i])
    }

    extern "C" fn previous_handler(_: c_int) {
        // SAFETY: async-signal-safe exit, no unwinding.
        unsafe { libc::_exit(PREVIOUS_EXIT) };
    }

    pub fn main() {
        let (Some(so), Some(fixture)) = (
            std::env::var_os("ATL_QEMU_SPIKE_SO"),
            std::env::var_os("ATL_QEMU_SPIKE_FIXTURE"),
        ) else {
            println!("spike: SKIP (ATL_QEMU_SPIKE_SO / ATL_QEMU_SPIKE_FIXTURE unset)");
            return;
        };
        let args: Vec<String> = std::env::args().collect();
        let base = args
            .iter()
            .position(|a| a == "--guest-base")
            .map_or("0".to_owned(), |i| args[i + 1].clone());
        // the handler the process had before QEMU: gate (e) expects host faults to reach it
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
        let r = atl_qemu_engine::run(
            std::path::Path::new(&so),
            &["qemu-aarch64", "-B", &base, &fixture],
            on_trap,
        );
        println!("spike: FAIL QEMU main returned {r:?}");
        std::process::exit(2);
    }

    fn on_trap(g: Guest, trap: Trap) -> u64 {
        match trap {
            Trap::Boot(table) => u64::from(!gates(g, table)),
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
                REPORTS.with(|r| r.borrow_mut().push(v));
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
        let mut ok = true;
        for (gate, f) in [
            ('a', gate_a as fn(Guest) -> Result<(), String>),
            ('b', gate_b),
            ('c', gate_c),
            ('d', gate_d),
            ('e', gate_e),
        ] {
            GATE.with(|c| c.set(gate));
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

    fn gate_d(g: Guest) -> Result<(), String> {
        let (q0, _) = atl_qemu_engine::signal::routed();
        let r = g.call(w(t::STORE_FAULT), &[]);
        let (q1, _) = atl_qemu_engine::signal::routed();
        let reports = REPORTS.with(|r| r.borrow().clone());
        if reports != [w(t::D_MARK)] {
            return Err(format!(
                "guest handler reports {reports:?}, expected [{}]",
                w(t::D_MARK)
            ));
        }
        if q1 - q0 != 1 {
            return Err(format!(
                "{} faults routed to QEMU's handler, expected 1",
                q1 - q0
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

    /// A host-only fault (Rust code, not TCG) in a forked child must reach the pre-QEMU handler, not QEMU's `die_from_signal`.
    fn gate_e(_: Guest) -> Result<(), String> {
        // SAFETY: fork from the single guest thread; the child only faults.
        let pid = unsafe { libc::fork() };
        if pid == 0 {
            // SAFETY: deliberate null store, the fault under test.
            unsafe { std::ptr::null_mut::<u64>().write_volatile(1) };
            // SAFETY: not reached unless the store did not fault.
            unsafe { libc::_exit(0) };
        }
        let mut status = 0;
        // SAFETY: waits for the child just forked.
        if unsafe { libc::waitpid(pid, &mut status, 0) } != pid {
            return Err("waitpid failed".to_owned());
        }
        if libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == PREVIOUS_EXIT {
            Ok(())
        } else if libc::WIFSIGNALED(status) {
            Err(format!(
                "child killed by signal {}, expected exit {PREVIOUS_EXIT}",
                libc::WTERMSIG(status)
            ))
        } else {
            Err(format!(
                "child status {status:#x}, expected exit {PREVIOUS_EXIT}"
            ))
        }
    }
}

fn main() {
    #[cfg(feature = "qemu")]
    spike::main();
    #[cfg(not(feature = "qemu"))]
    println!("spike: SKIP (feature qemu off)");
}
