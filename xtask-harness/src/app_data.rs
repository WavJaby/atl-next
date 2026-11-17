// SPDX-License-Identifier: MIT
//! `xtask test app-data`: an app's data outlives a run-apk launch (AOSP: until uninstall or `pm clear`). Three `run-apk` launches of the fixture (xtask-harness/testdata/app-data) on the gate's own device (`<out>/app-data/device`, never the user's):
//! 1. on empty data the app writes `save.txt` under its files dir: `AtlData: data: wrote`;
//! 2. the next launch reads it back: `AtlData: data: read save-v1`, and the file is on the host under the device dir;
//! 3. `--clear-data` (`pm clear`) empties the package's data first, so the app writes again: `AtlData: data: wrote`.
//!
//! Each launch is run-apk's own run (`run_apk::run`), so its window verdict must pass too. `--break-keep` empties the data before launch 2 (a device that forgets): the gate must fail.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::Result;
use atl_android_paths::DATA_USER;

use crate::atld_run::{Runtime, clear_package_data};
use crate::{manifest, run_apk, verdict};

#[derive(clap::Args)]
pub struct Args {
    /// The host arch (an arches.toml row with `[host]`): the default --out.
    #[arg(long)]
    host: String,
    /// Native build directory (default: this checkout's build dir, see `cargo xtask out show`; else <workspace>/out/native/<host>).
    #[arg(long)]
    out: Option<PathBuf>,
    /// Java pipeline build directory: boot jars, aapt2, d8 (default: this checkout's build dir, see `cargo xtask out show`; else <workspace>/out/java/build).
    #[arg(long)]
    java_out: Option<PathBuf>,
    /// Deliberate failure: the data is emptied before launch 2 (a device that forgets), so its read check must fail.
    #[arg(long)]
    break_keep: bool,
}

const FIXTURE: &str = "xtask-harness/testdata/app-data";
/// The fixture's file under its files dir and the value it holds (testdata/app-data DataMain.java).
const FILE: &str = "save.txt";
const VALUE: &str = "save-v1";
/// The fixture's log line before its state: `AtlData: data: <state> dir=<files dir>`.
const DATA_LINE: &str = "AtlData: data: ";

pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    let rt = Runtime::new(
        root,
        "run-apk",
        &args.host,
        args.out.as_deref(),
        args.java_out.as_deref(),
        run_apk::STAGE,
        false,
    )?;
    let out = rt.out.clone();
    let apk = rt.apk.build(
        &root.join(FIXTURE),
        &out.join("app-data").join("apk"),
        &[],
        None,
        None,
    )?;
    let package = manifest::read(&rt.apk.aapt2, &apk)?.name;
    let device = out.join("app-data").join("device");
    let log = out
        .join("run-apk")
        .join("root")
        .join("log")
        .join(format!("{package}.log"));
    let saved = device
        .join("data/user/0")
        .join(&package)
        .join("files")
        .join(FILE);
    let files_dir = format!("{DATA_USER}/0/{package}/files");
    // the gate starts from empty data: this package's data from an earlier run is cleared
    clear_package_data(&device, &package)?;
    let mut whys = Vec::new();
    let launches = [
        (1, false, "wrote".to_owned(), "empty data"),
        (2, args.break_keep, format!("read {VALUE}"), "data kept"),
        (3, true, "wrote".to_owned(), "--clear-data"),
    ];
    for (n, clear, want, what) in launches {
        let launch = run_apk::Args::driven(
            &apk,
            &args.host,
            Some(out.clone()),
            args.java_out.clone(),
            false,
        )
        .with_clear_data(clear)
        .with_device(&device);
        let code = run_apk::run(root, &launch)?;
        let state = data_state(
            &std::fs::read_to_string(&log).unwrap_or_default(),
            &files_dir,
        );
        let on_device = std::fs::read_to_string(&saved).ok();
        println!(
            "app-data: launch {n} ({what}): data {} (want {want:?}), {} on the device",
            state.as_deref().unwrap_or("no data line"),
            if on_device.as_deref() == Some(VALUE) {
                "file present"
            } else {
                "file missing"
            }
        );
        if code != ExitCode::SUCCESS {
            whys.push(format!("launch {n}: run-apk's verdict failed"));
        }
        if state.as_deref() != Some(want.as_str()) {
            whys.push(format!(
                "launch {n} ({what}): data {state:?}, want {want:?}"
            ));
        }
        if on_device.as_deref() != Some(VALUE) {
            whys.push(format!(
                "launch {n}: {} is not {VALUE:?} on the device",
                saved.display()
            ));
        }
    }
    // the device is left as found: this package's data is removed again
    clear_package_data(&device, &package)?;
    if whys.is_empty() {
        println!("app-data: pass (3 launches: empty data, kept, --clear-data)");
        return Ok(ExitCode::SUCCESS);
    }
    println!("{}", verdict::gate("app-data", &whys));
    Ok(ExitCode::FAILURE)
}

/// The state of the fixture's data line for `files_dir` (`AtlData: data: <state> dir=<files_dir>`), or None when the log has no such line.
fn data_state(log: &str, files_dir: &str) -> Option<String> {
    log.lines().find_map(|line| {
        let (_, rest) = line.split_once(DATA_LINE)?;
        let (state, dir) = rest.rsplit_once(" dir=")?;
        (dir.trim_end() == files_dir).then(|| state.to_owned())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_state_reads_the_line_of_that_files_dir() {
        let files = |p: &str| format!("{DATA_USER}/0/{p}/files");
        let log = format!(
            "I AtlData: data: wrote dir={}\nI AtlData: data: read save-v1 dir={}\n",
            files("p"),
            files("q")
        );
        assert_eq!(
            data_state(&log, &files("q")).as_deref(),
            Some("read save-v1")
        );
        assert_eq!(data_state(&log, &files("r")), None);
        assert_eq!(data_state("", &files("p")), None);
    }
}
