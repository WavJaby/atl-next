// SPDX-License-Identifier: MIT
//! Q6-5 gate: the QEMU engine booted through its installed paths, as a process whose ART names the arm64 ISA boots it: `engine::select(Some(GUEST_ISA))`, no path override, so the engine `.so` and the boot executable come from atl-android-paths `engine_library` / `engine_boot`.
//! Env (xtask qemu_linker.rs sets it; unset prints SKIP): ATL_NB_STAGED, set only when the run is in a mount namespace whose `/system` is the staged test image (atld_run.rs `qemu_engine_files`).
//! Check: (b) select returned Ok (the engine dlopened from `engine_library`, BOOT raised by `engine_boot`, the calling thread attached). Break (harness-level, xtask): the stage without the boot executable -> select fails, no `(b) ok`, nonzero exit.
#![allow(clippy::print_stdout, reason = "test harness output is its verdict")]

use std::process::ExitCode;

use atl_native_bridge::engine::{self, qemu};

fn main() -> ExitCode {
    if std::env::var_os("ATL_NB_STAGED").is_none() {
        println!("qemu-staged: SKIP (ATL_NB_STAGED unset: not in a staged image)");
        return ExitCode::SUCCESS;
    }
    match engine::select(Some(qemu::GUEST_ISA)) {
        Ok(_) => {
            println!("qemu-staged: (b) ok");
            ExitCode::SUCCESS
        }
        Err(e) => {
            println!("qemu-staged: (b) FAIL select: {e}");
            ExitCode::from(1)
        }
    }
}
