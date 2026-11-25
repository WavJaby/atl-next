// SPDX-License-Identifier: MIT
#![forbid(unsafe_code)]
//! `atl-labwc-setup <socket>`: gives a headless labwc its gate output and seat devices (the local gates' setup, `atl_labwc_setup::setup`), prints what it set, then holds the devices until labwc closes the socket.
//! A remote test run starts it on the target beside the run's own labwc (remote.rs `nested_headless_script`); the run waits for the `ready` line, and its end stops the helper.
//! Stdout: one line per output, `seat: ...`, then `ready`, each flushed. A failure prints its cause to stderr and exits 1, or 3 when the compositor refused the output configuration.

use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::ExitCode;

use anyhow::{Context, Result, ensure};
use atl_labwc_setup::{GATE_OUTPUT, READY_LINE, exit_status, setup};

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            let _ = writeln!(std::io::stderr(), "atl-labwc-setup: {e:#}");
            ExitCode::from(exit_status(&e))
        }
    }
}

/// Sets the output and the seat's devices, then holds them until the compositor closes the socket.
fn run() -> Result<()> {
    let mut args = std::env::args_os().skip(1);
    let socket = args.next().context("usage: atl-labwc-setup <socket>")?;
    ensure!(
        args.next().is_none(),
        "usage: atl-labwc-setup <socket>: one argument"
    );
    let stream = UnixStream::connect(&socket)
        .with_context(|| format!("connecting to {}", Path::new(&socket).display()))?;
    let control = setup(stream, &[GATE_OUTPUT])?;
    for line in control.describe()? {
        say(&line)?;
    }
    say("seat: virtual pointer and keyboard")?;
    say(READY_LINE)?;
    control.hold()
}

/// One stdout line, flushed: the run reads the log while the helper holds the devices.
fn say(line: &str) -> Result<()> {
    let mut out = std::io::stdout().lock();
    writeln!(out, "{line}")?;
    out.flush()?;
    Ok(())
}
