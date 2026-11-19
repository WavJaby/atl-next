// SPDX-License-Identifier: MIT
//! arm32 engine spike gates (phase 4 P1), harness-less, on an arm-linux-user PIC build: `start` boots QEMU on its helper thread, whose BOOT trap runs gates (a)-(e) there and parks; the main thread then runs (f). Exit 0 when every gate is ok.
//! Env (xtask qemu_spike.rs `--arch arm` sets it; without it the run prints SKIP): `ATL_QEMU_SPIKE_SO` = the spike library, `ATL_QEMU_SPIKE_FIXTURE` = the static armv7 guest program (fixture_arm.c), `ATL_QEMU_SIGCHAIN_OBJ` = AOSP libsigchain's object (build.rs links it, cfg `atl_qe_sigchain`).
//! Expected values come from the fixture's table (fixture_arm.c), never from the engine. One line per gate: `spike-arm: (x) ok` or `spike-arm: (x) FAIL <why>`.
//! Gates: (B) the boot reaches BOOT with the ART stand-in mapped; (a) a CALL trap from a Thumb and from an Arm caller sees r0..r3 and four stack words; (b) host -> guest -> host -> guest nesting sums right; (c) a host pointer below 4 GiB is read and written raw (plain + exclusive); (d) a guest store fault goes through the engine's special handler to the guest's SIGSEGV handler; (e) a mapping made before QEMU at [`STANDIN`] (ART's low-4G heap stand-in) survives the boot and guest `mmap` (any address, and hinted at it) avoids it; (w) the window allocator (`gate_w`); (p) an arm pool thunk traps as its binding id (`gate_p`); (f) a host thread attaches a CPU, calls, detaches, then the process forks and the child exits (QEMU's RCU prefork handler would block on a broken registry).
#![allow(clippy::print_stdout, reason = "test harness output is its verdict")]

#[cfg(all(feature = "qemu", atl_qe_guest = "arm"))]
mod spike {
    use std::ffi::c_void;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU8, AtomicU32, Ordering};

    use atl_qemu_engine::{Guest, Trap, map_window, page, unmap_window};

    /// The ART heap stand-in of the phase 4 plan's gate (e): a mapping in the low 4 GiB made before QEMU's `main`, as ART's `low_4gb` heap spaces are when the bridge loads (art/runtime/gc/heap.cc). Any pre-existing low mapping serves; this is the address the plan names.
    const STANDIN: usize = 0x12c0_0000;
    const STANDIN_LEN: usize = 16 << 20;
    /// Written into the stand-in before the boot; must read back after (e).
    const STANDIN_MAGIC: u32 = 0xa7_12c0;
    /// What the host answers gate (a)'s CALL: any value the guest cannot produce itself.
    const A_ANSWER: u32 = 0x5a5a;
    /// Gate (w): windows made, each of `W_LEN` bytes (several, so one lands after the late mapping; any page multiple serves).
    const W_COUNT: usize = 8;
    const W_LEN: usize = 64 << 10;
    /// Gate (w)'s late host mapping, made where the next window search starts.
    const W_LATE_LEN: usize = 1 << 20;
    /// Seconds the parent of (f)'s fork waits for the child before calling it hung.
    const F_DEADLINE_SECS: u64 = 20;
    /// Gate (p): the binding id of the one thunk (any id below BIND; not 0, so a decode that drops the id shows), its argument, and the host's answer (any value the guest cannot produce itself).
    const P_ID: u32 = 0x1234;
    const P_ARG: u64 = 0x77;
    const P_ANSWER: u64 = 0x6b6b;

    /// atl-bridgegen's thunk constants as the engine's build script wrote them (the decode reads the same file): `ARM_SVC`, `ARM_RET`, `ARM_BIND`.
    #[allow(
        dead_code,
        reason = "generated: aarch64's constants and the placeholders are unused here"
    )]
    mod thunk {
        include!(concat!(env!("OUT_DIR"), "/thunk_constants.rs"));
    }
    /// Gate (p)'s binding traps seen: (id, r0).
    static P_SEEN: Mutex<Vec<(u32, u64)>> = Mutex::new(Vec::new());

    /// The fixture's table words (fixture_arm.c `table`).
    mod t {
        pub const TRAP_ARGS_THUMB: usize = 0;
        pub const TRAP_ARGS_ARM: usize = 1;
        pub const OUTER: usize = 2;
        pub const INNER: usize = 3;
        pub const BOX: usize = 4;
        pub const STORE_FAULT: usize = 5;
        pub const MMAP: usize = 6;
        pub const A_FIRST: usize = 7;
        pub const A_ADD: usize = 8;
        pub const B_N: usize = 9;
        pub const B_SUM: usize = 10;
        pub const C_BEFORE: usize = 11;
        pub const C_AFTER: usize = 15;
        pub const D_MARK: usize = 19;
        pub const D_RET: usize = 20;
        pub const E_LEN: usize = 21;
        pub const F_ARG: usize = 22;
        pub const F_RET: usize = 23;
    }

    /// The table's guest address (BOOT's r0); identity, so also its host address.
    static TABLE: AtomicU32 = AtomicU32::new(0);
    /// What the CALL trap does now: `a` check gate (a)'s arguments, `b` call fx_inner.
    static PHASE: AtomicU8 = AtomicU8::new(0);
    /// Gate (a)'s trap observations: (r0..r3, four stack words, thumb).
    static A_SEEN: Mutex<Vec<([u64; 4], [Option<u64>; 4], bool)>> = Mutex::new(Vec::new());
    static REPORTS: Mutex<Vec<u64>> = Mutex::new(Vec::new());
    static RESULTS: Mutex<Vec<(char, Result<(), String>)>> = Mutex::new(Vec::new());

    fn word(i: usize) -> u32 {
        let table = TABLE.load(Ordering::Acquire) as usize;
        // SAFETY: the fixture's static table, mapped for the life of the process (identity: guest = host address); i is one of `t`.
        unsafe { (table as *const u32).add(i).read() }
    }

    fn record(gate: char, r: Result<(), String>) {
        match &r {
            Ok(()) => println!("spike-arm: ({gate}) ok"),
            Err(e) => println!("spike-arm: ({gate}) FAIL {e}"),
        }
        RESULTS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push((gate, r));
    }

    fn on_trap(g: Guest, t: Trap) -> u64 {
        match t {
            Trap::Boot(table) => {
                TABLE.store(table as u32, Ordering::Release);
                println!("spike-arm: (B) ok");
                boot_gates(g);
                0
            }
            Trap::Call(args) => match PHASE.load(Ordering::Acquire) {
                b'a' => {
                    let regs = [args[0], args[1], args[2], args[3]];
                    let stack = std::array::from_fn(|i| g.stack_word(i));
                    A_SEEN
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push((regs, stack, g.thumb()));
                    u64::from(A_ANSWER)
                }
                b'b' => g.call(u64::from(word(t::INNER)), &[args[0]]),
                p => panic!("CALL in phase {p}"),
            },
            Trap::Report(v) => {
                REPORTS
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(v);
                0
            }
            Trap::Bind { id, args } => {
                P_SEEN
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push((id, args[0]));
                P_ANSWER
            }
        }
    }

    fn gate_a(g: Guest) -> Result<(), String> {
        PHASE.store(b'a', Ordering::Release);
        let first = u64::from(word(t::A_FIRST));
        let want_regs: [u64; 4] = std::array::from_fn(|i| first + i as u64);
        let want_stack: [Option<u64>; 4] = std::array::from_fn(|i| Some(first + 4 + i as u64));
        let want_ret = u64::from(A_ANSWER + word(t::A_ADD));
        for (entry, thumb) in [(t::TRAP_ARGS_THUMB, true), (t::TRAP_ARGS_ARM, false)] {
            let f = u64::from(word(entry));
            if (f & 1 == 1) != thumb {
                return Err(format!("{f:#x}: the fixture's mode bit is not {thumb}"));
            }
            A_SEEN
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clear();
            let got = g.call(f, &[]);
            let seen = A_SEEN
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            let mode = if thumb { "thumb" } else { "arm" };
            if seen != [(want_regs, want_stack, thumb)] {
                return Err(format!(
                    "{mode} caller: traps saw {seen:?}, expected [({want_regs:?}, {want_stack:?}, {thumb})]"
                ));
            }
            if got != want_ret {
                return Err(format!(
                    "{mode} caller: returned {got:#x}, expected {want_ret:#x}"
                ));
            }
        }
        Ok(())
    }

    fn gate_b(g: Guest) -> Result<(), String> {
        PHASE.store(b'b', Ordering::Release);
        let got = g.call(u64::from(word(t::OUTER)), &[u64::from(word(t::B_N))]);
        let want = u64::from(word(t::B_SUM));
        if got == want {
            Ok(())
        } else {
            Err(format!("fx_outer returned {got}, expected {want}"))
        }
    }

    /// Gate (c)'s host memory: a window (the engine's window allocator), which comes back marked for the guest, so no `map_pages`.
    fn gate_c(g: Guest) -> Result<(), String> {
        let p = map_window(4096, page::READ | page::WRITE)? as *mut u32;
        // SAFETY: p is a fresh RW window of 1024 u32.
        let bx = unsafe { std::slice::from_raw_parts_mut(p, 4) };
        for (i, w) in bx.iter_mut().enumerate() {
            *w = word(t::C_BEFORE + i);
        }
        g.call(u64::from(word(t::BOX)), &[p as u64]);
        let want: Vec<u32> = (0..4).map(|i| word(t::C_AFTER + i)).collect();
        if bx == want.as_slice() {
            Ok(())
        } else {
            Err(format!("box {bx:?} at {p:?}, expected {want:?}"))
        }
    }

    fn gate_d(g: Guest) -> Result<(), String> {
        REPORTS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
        let before = atl_qemu_engine::signal::routed().code;
        let got = g.call(u64::from(word(t::STORE_FAULT)), &[]);
        let routed = atl_qemu_engine::signal::routed().code - before;
        let reports = REPORTS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let (mark, ret) = (u64::from(word(t::D_MARK)), u64::from(word(t::D_RET)));
        if reports != [mark] || got != ret || routed != 1 {
            return Err(format!(
                "reports {reports:?} (want [{mark}]), returned {got} (want {ret}), engine-routed TCG faults {routed} (want 1)"
            ));
        }
        Ok(())
    }

    fn standin() -> Result<(), String> {
        // SAFETY: MAP_FIXED_NOREPLACE never replaces a mapping; a refusal is an error return.
        let p = unsafe {
            libc::mmap(
                STANDIN as *mut c_void,
                STANDIN_LEN,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_FIXED_NOREPLACE,
                -1,
                0,
            )
        };
        if p as usize != STANDIN {
            return Err(format!("stand-in at {STANDIN:#x}: mmap gave {p:?}"));
        }
        // SAFETY: the RW mapping just made.
        unsafe { (STANDIN as *mut u32).write(STANDIN_MAGIC) };
        Ok(())
    }

    fn gate_e(g: Guest) -> Result<(), String> {
        let len = word(t::E_LEN) as usize;
        let overlaps = |a: usize| a < STANDIN + STANDIN_LEN && STANDIN < a + len;
        for hint in [0, STANDIN as u64] {
            let a = g.call(u64::from(word(t::MMAP)), &[hint]) as usize;
            if a == u32::MAX as usize {
                return Err(format!("guest mmap(hint {hint:#x}) failed"));
            }
            if overlaps(a) {
                return Err(format!(
                    "guest mmap(hint {hint:#x}) = {a:#x} overlaps the stand-in {STANDIN:#x}+{STANDIN_LEN:#x}"
                ));
            }
        }
        // SAFETY: the stand-in mapping, alive for the process.
        let magic = unsafe { (STANDIN as *const u32).read() };
        if magic != STANDIN_MAGIC {
            return Err(format!(
                "stand-in word {magic:#x}, expected {STANDIN_MAGIC:#x}"
            ));
        }
        Ok(())
    }

    /// The host mapping (w) makes between two windows, `[at, at + W_LATE_LEN)`, magic written; Err when the site is taken.
    fn late_mapping(at: usize) -> Result<(), String> {
        // SAFETY: MAP_FIXED_NOREPLACE never replaces a mapping; a refusal is an error return.
        let p = unsafe {
            libc::mmap(
                at as *mut c_void,
                W_LATE_LEN,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_FIXED_NOREPLACE,
                -1,
                0,
            )
        };
        if p as usize != at {
            return Err(format!("late host mapping at {at:#x}: mmap gave {p:?}"));
        }
        // SAFETY: the RW mapping just made.
        unsafe { (at as *mut u32).write(STANDIN_MAGIC) };
        Ok(())
    }

    /// Gate (w), the window allocator (plan D8 obstacle 2): [`W_COUNT`] windows, each below 4 GiB, overlapping neither the ART stand-in nor each other nor a host mapping made after QEMU booted (ART's heap grows later) right where the next window search starts (the end of the last window: linux-user's `mmap_next_start`); each comes back guest-mapped, and the engine counted every one. The host-side views stay intact (magic words).
    fn gate_w(g: Guest) -> Result<(), String> {
        let before = atl_qemu_engine::live_windows();
        let mut taken: Vec<(usize, usize, &str)> = vec![(STANDIN, STANDIN_LEN, "stand-in")];
        for i in 0..W_COUNT {
            let a = map_window(W_LEN, page::READ | page::WRITE)?;
            if a + W_LEN > 1 << 32 {
                return Err(format!("window {i} at {a:#x}+{W_LEN:#x} not below 4 GiB"));
            }
            if let Some((s, l, what)) = taken.iter().find(|(s, l, _)| a < s + l && *s < a + W_LEN) {
                return Err(format!(
                    "window {i} at {a:#x}+{W_LEN:#x} overlaps the {what} {s:#x}+{l:#x}"
                ));
            }
            if g.page_flags(a as u64) & page::VALID == 0 {
                return Err(format!("window {i} at {a:#x} not marked for the guest"));
            }
            taken.push((a, W_LEN, "window"));
            if i == W_COUNT / 2 {
                late_mapping(a + W_LEN)?;
                taken.push((a + W_LEN, W_LATE_LEN, "late host mapping"));
            }
        }
        let made = atl_qemu_engine::live_windows() - before;
        if made != W_COUNT as u64 {
            return Err(format!("engine counted {made} windows, made {W_COUNT}"));
        }
        // give every window back (unmap_window, ABI 4.2): marks cleared, the engine's count back, and the range free for a host mapping again
        for &(a, _, _) in taken.iter().filter(|(_, _, w)| *w == "window") {
            unmap_window(a, W_LEN)?;
            if g.page_flags(a as u64) & page::VALID != 0 {
                return Err(format!(
                    "window {a:#x} still marked for the guest after unmap_window"
                ));
            }
            // SAFETY: MAP_FIXED_NOREPLACE never replaces a mapping; a refusal (window still mapped) is the error return.
            let p = unsafe {
                libc::mmap(
                    a as *mut c_void,
                    W_LEN,
                    libc::PROT_NONE,
                    libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_FIXED_NOREPLACE,
                    -1,
                    0,
                )
            };
            if p as usize != a {
                return Err(format!(
                    "window {a:#x} still host-mapped after unmap_window: mmap gave {p:?}"
                ));
            }
            // SAFETY: the probe mapping just made at a.
            unsafe { libc::munmap(p, W_LEN) };
        }
        let live = atl_qemu_engine::live_windows();
        if live != before {
            return Err(format!(
                "engine counts {live} live windows after unmapping, {before} before"
            ));
        }
        for (s, _, what) in taken.iter().filter(|(_, _, w)| *w != "window") {
            // SAFETY: the stand-in and the late mapping, alive for the process.
            let magic = unsafe { (*s as *const u32).read() };
            if magic != STANDIN_MAGIC {
                return Err(format!(
                    "{what} word {magic:#x}, expected {STANDIN_MAGIC:#x}"
                ));
            }
        }
        println!("spike-arm: (w) {W_COUNT} windows below 4 GiB, none overlapping, all given back");
        Ok(())
    }

    /// Gate (p), the arm pool thunk (bridgegen arm.rs): its two words `svc #(BIND | P_ID); bx lr` in a window, called from the host as a guest function, reach the trap callback as binding `P_ID` with r0, and the host's answer comes back.
    fn gate_p(g: Guest) -> Result<(), String> {
        let at = map_window(8, page::READ | page::WRITE | page::EXEC)?;
        let words = [thunk::ARM_SVC | thunk::ARM_BIND | P_ID, thunk::ARM_RET];
        // SAFETY: a fresh RW window of at least 8 bytes.
        unsafe { (at as *mut [u32; 2]).write(words) };
        P_SEEN
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
        let got = g.call(at as u64, &[P_ARG]);
        let seen = P_SEEN
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if seen != [(P_ID, P_ARG)] || got != P_ANSWER {
            return Err(format!(
                "binding traps {seen:?} (want [({P_ID:#x}, {P_ARG:#x})]), returned {got:#x} (want {P_ANSWER:#x})"
            ));
        }
        Ok(())
    }

    /// Gates (a)-(e), (w) and (p) on the boot CPU, inside BOOT. Each prints `(x) run` first: a gate the process dies in (QEMU's fatal guest signal) leaves it as the last line.
    fn boot_gates(g: Guest) {
        let gates: [(char, fn(Guest) -> Result<(), String>); 7] = [
            ('a', gate_a),
            ('b', gate_b),
            ('c', gate_c),
            ('d', gate_d),
            ('e', gate_e),
            ('w', gate_w),
            ('p', gate_p),
        ];
        for (name, gate) in gates {
            println!("spike-arm: ({name}) run");
            record(name, gate(g));
        }
    }

    fn gate_f(booted: &atl_qemu_engine::Booted) -> Result<(), String> {
        PHASE.store(b'b', Ordering::Release);
        let r = std::thread::scope(|s| {
            s.spawn(|| -> Result<u64, String> {
                let cpu = booted.attach()?;
                let got = cpu
                    .guest()
                    .call(u64::from(word(t::INNER)), &[u64::from(word(t::F_ARG))]);
                drop(cpu);
                Ok(got)
            })
            .join()
            .map_err(|_| "attached thread panicked".to_owned())?
        })?;
        let want = u64::from(word(t::F_RET));
        if r != want {
            return Err(format!("fx_inner returned {r}, expected {want}"));
        }
        // SAFETY: fork in a multi-threaded process; the child only calls _exit.
        let pid = unsafe { libc::fork() };
        if pid == 0 {
            // SAFETY: async-signal-safe exit of the child.
            unsafe { libc::_exit(0) };
        }
        if pid < 0 {
            return Err("fork failed".to_owned());
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(F_DEADLINE_SECS);
        loop {
            let mut status = 0;
            // SAFETY: our own child; WNOHANG polls.
            let w = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
            if w == pid {
                return if libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0 {
                    Ok(())
                } else {
                    Err(format!("fork child ended with status {status:#x}"))
                };
            }
            if std::time::Instant::now() > deadline {
                // SAFETY: our own child.
                unsafe { libc::kill(pid, libc::SIGKILL) };
                return Err(format!("fork child hung for {F_DEADLINE_SECS} s"));
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }

    #[cfg(atl_qe_sigchain)]
    mod sigchain {
        use std::ffi::{c_int, c_void};

        /// `art::SigchainAction` (art/sigchainlib/sigchain.h): one special handler, its mask and flags.
        #[repr(C)]
        struct SigchainAction {
            sc_sigaction: extern "C" fn(c_int, *mut libc::siginfo_t, *mut c_void) -> bool,
            sc_mask: libc::sigset_t,
            sc_flags: u64,
        }

        unsafe extern "C" {
            fn AddSpecialSignalHandlerFn(signal: c_int, sa: *mut SigchainAction);
        }

        /// `SIGCHAIN_ALLOW_NORETURN` (art/sigchainlib/sigchain.h).
        const ALLOW_NORETURN: u64 = 0x1;

        /// What `InitializeNativeBridge` registers: the engine's special, full mask, `SIGCHAIN_ALLOW_NORETURN` (native_bridge_art_interface.cc).
        pub fn register() {
            for sig in [libc::SIGSEGV, libc::SIGBUS] {
                // SAFETY: a zeroed sigset_t is a valid buffer for sigfillset.
                let mut full: libc::sigset_t = unsafe { std::mem::zeroed() };
                // SAFETY: full is a live sigset_t.
                unsafe { libc::sigfillset(&mut full) };
                let mut engine = SigchainAction {
                    sc_sigaction: atl_qemu_engine::signal::special,
                    sc_mask: full,
                    sc_flags: ALLOW_NORETURN,
                };
                // SAFETY: sigchain copies the action; the engine's special has the special-handler signature.
                unsafe { AddSpecialSignalHandlerFn(sig, &mut engine) };
            }
        }
    }

    pub fn main() -> i32 {
        let (Some(so), Some(fixture)) = (
            std::env::var_os("ATL_QEMU_SPIKE_SO"),
            std::env::var_os("ATL_QEMU_SPIKE_FIXTURE"),
        ) else {
            println!("spike-arm: SKIP (ATL_QEMU_SPIKE_SO / ATL_QEMU_SPIKE_FIXTURE unset)");
            return 0;
        };
        #[cfg(atl_qe_sigchain)]
        sigchain::register();
        #[cfg(not(atl_qe_sigchain))]
        {
            println!("spike-arm: FAIL no sigchain (ATL_QEMU_SIGCHAIN_OBJ unset)");
            return 1;
        }
        if let Err(e) = standin() {
            println!("spike-arm: (e) FAIL {e}");
            return 1;
        }
        // printed before the boot: a boot QEMU ends (patch 0006 dropped: "address space in use") leaves this as the last line
        println!("spike-arm: boot");
        let fixture = fixture.to_string_lossy().into_owned();
        let booted = match atl_qemu_engine::start(
            std::path::Path::new(&so),
            &["qemu-arm", "-B", "0", &fixture],
            on_trap,
        ) {
            Ok(b) => b,
            Err(e) => {
                println!("spike-arm: (B) FAIL {e}");
                return 1;
            }
        };
        record('f', gate_f(&booted));
        let results = RESULTS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let gates: String = results.iter().map(|(g, _)| *g).collect();
        if gates != "abcdewpf" || results.iter().any(|(_, r)| r.is_err()) {
            println!("spike-arm: FAIL gates run {gates:?}");
            return 1;
        }
        println!("spike-arm: ok");
        0
    }
}

fn main() {
    #[cfg(all(feature = "qemu", atl_qe_guest = "arm"))]
    std::process::exit(spike::main());
    #[cfg(not(all(feature = "qemu", atl_qe_guest = "arm")))]
    println!("spike-arm: SKIP (feature qemu off, or not an arm build)");
}
