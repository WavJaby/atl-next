// SPDX-License-Identifier: MIT
//! `cargo xtask`: the command line lives in the library (`xtask::cli`).
#![forbid(unsafe_code)]

fn main() -> anyhow::Result<std::process::ExitCode> {
    xtask::cli::main()
}
