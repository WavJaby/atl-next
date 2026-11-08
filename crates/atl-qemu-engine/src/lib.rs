// SPDX-License-Identifier: MIT
//! The QEMU translator engine, spike shape (QEMU bridge plan Q2): QEMU's aarch64 linux-user core as a library in the host process, identity addresses (`-B 0`).
//!
//! Start: [`run`] dlopens the spike library (QEMU's PIC objects linked `-shared`, `main` aliased `atl_qemu_user_main`) and runs QEMU's own `main` on a static guest ELF: QEMU's init, loader and `cpu_loop`, nothing replayed here.
//! Hand-over: the guest's bootstrap `svc #BOOT` (x0 = payload, x1 = its `svc #RET` stub) reaches P1's hook ([`trap`]); from there the host drives guest calls ([`Guest::call`]) and answers `svc #CALL` traps, all through the one [`OnTrap`] callback.
//! Exit: the BOOT answer lands in the guest's x0 and the guest exits through QEMU's own syscall path; `run` returns only if QEMU's `main` does.
//! Faults: [`signal`] puts its handler in front of QEMU's at BOOT (QEMU's `signal_init` overwrites earlier handlers and never chains them): guest faults go to QEMU's, host-only faults to the handler that was installed before `run`; with libsigchain linked, ART's special handlers run before it.
//! Guest threads: each registers its per-thread QEMU state (`helper_retaddr`) at its first trap (BOOT, or THREAD in [`trap`]); [`signal`] classifies every fault on the thread that took it.
#![cfg_attr(
    not(feature = "qemu"),
    doc = "Built without feature `qemu`: no engine code (needs `ATL_QEMU_BUILD`, see Cargo.toml)."
)]

#[cfg(feature = "qemu")]
#[allow(
    non_snake_case,
    non_upper_case_globals,
    dead_code,
    clippy::all,
    reason = "generated: atl-bridgegen layout constants named after QEMU's records and fields"
)]
mod layout {
    include!(concat!(env!("OUT_DIR"), "/qemu_layout.rs"));
}

#[cfg(feature = "qemu")]
mod engine;
#[cfg(feature = "qemu")]
pub mod signal;
#[cfg(feature = "qemu")]
pub mod trap;

#[cfg(feature = "qemu")]
pub use engine::{Guest, OnTrap, Trap, run};
