// SPDX-License-Identifier: MIT
//! `xtask test parcel`: atl-aidl-probe, built as `xtask native` builds converter packages (packages.rs), round-trips an AIDL parcelable through AOSP's C++ Parcel (libbinder_rs -> libbinder_ndk -> libbinder).
//! Two runs: as built (must exit 0), then told to expect one int32 more than Parcel's encoding gives (must fail naming the byte count), so the probe's check is shown to fire.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use anyhow::{Context, Result};
use atl_layout::Layout;

use crate::packages;

#[derive(clap::Args)]
pub struct Args {
    /// The host arch (an arches.toml row with `[host]`): the default --out.
    #[arg(long)]
    host: String,
    /// Native build directory whose graph holds libbinder_rs (default <workspace>/out/native/<host>, as `xtask native`).
    #[arg(long)]
    out: Option<PathBuf>,
}

const PACKAGE: &str = "atl-aidl";
const PROBE: &str = "atl-aidl-probe";
/// The probe's override of its expected parcel size (atl-aidl src/bin/probe.rs).
const WANT_BYTES: &str = "--want-bytes";
/// What the probe says when the size differs.
const SIZE_FAILURE: &str = "Parcel's encoding gives";
/// One more int32 than the probe's own expectation (48, probe.rs PARCEL_BYTES): any value but the true size makes the check fire; this one is the nearest the encoding could hold.
const WRONG_BYTES: &str = "52";

/// One probe run's result: exit success and stderr.
fn run_probe(probe: &Path, args: &[&str]) -> Result<(bool, String)> {
    let out = Command::new(probe)
        .args(args)
        .output()
        .with_context(|| format!("running {}", probe.display()))?;
    Ok((
        out.status.success(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    ))
}

/// The verdict of the two runs: the round trip passes, and the size check fires on a wrong expectation.
fn verdict(round_trip: &(bool, String), wrong: &(bool, String)) -> Vec<String> {
    let mut fails = Vec::new();
    if !round_trip.0 {
        fails.push(format!("round trip failed: {}", round_trip.1.trim()));
    }
    if wrong.0 || !wrong.1.contains(SIZE_FAILURE) {
        fails.push(format!(
            "size check did not fire with {WANT_BYTES} {WRONG_BYTES}: exit ok = {}, stderr {:?}",
            wrong.0,
            wrong.1.trim()
        ));
    }
    fails
}

pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    let out = match &args.out {
        Some(dir) => std::path::absolute(dir)?,
        None => Layout::of(root)?
            .workspace()
            .join("out/native")
            .join(&args.host),
    };
    let probe = packages::bin(root, &out, PACKAGE, PROBE)?;
    let fails = verdict(
        &run_probe(&probe, &[])?,
        &run_probe(&probe, &[WANT_BYTES, WRONG_BYTES])?,
    );
    for f in &fails {
        println!("parcel: FAIL {f}");
    }
    if fails.is_empty() {
        println!("parcel: pass (round trip, and the size check fires on a wrong size)");
        return Ok(ExitCode::SUCCESS);
    }
    Ok(ExitCode::FAILURE)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each failure is reported: a failed round trip, a wrong expectation the probe accepts, and one it refuses for another reason.
    #[test]
    fn verdicts() {
        let ok = (true, String::new());
        let fired = (
            false,
            format!("Error: parcel holds 48 bytes, {SIZE_FAILURE} 52"),
        );
        assert!(verdict(&ok, &fired).is_empty());
        assert_eq!(verdict(&(false, "boom".into()), &fired).len(), 1);
        assert_eq!(verdict(&ok, &ok).len(), 1);
        assert_eq!(verdict(&ok, &(false, "other".into())).len(), 1);
    }
}
