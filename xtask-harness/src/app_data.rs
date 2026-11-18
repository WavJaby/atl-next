// SPDX-License-Identifier: MIT
//! `xtask test app-data`: an app's data outlives a run-apk launch (AOSP: until uninstall or `pm clear`). Three `run-apk` launches of the fixture (xtask-harness/testdata/app-data) on the gate's own data root (`<out>/app-data/data-root`, never the user's):
//! 1. on empty data the app writes `save.txt` under its files dir (and its device-protected and external files dirs): `AtlData: data: wrote`;
//! 2. the next launch reads it back: `AtlData: data: read save-v1`, and the file is on the host under each of `apps/<package>/{data,data_de,external}`;
//! 3. `--clear-data` (`pm clear`) empties the package's data first, so the app writes again: `AtlData: data: wrote`.
//!
//! Every launch: the app sees AOSP's paths (`AtlData: paths ...`: getFilesDir, the device-protected getFilesDir, getExternalFilesDir, sourceDir), whatever the host layout; and another package (a system
//! package the run scans) has its own host data dir holding none of the fixture's files.
//! Each launch is run-apk's own run (`run_apk::run`), so its window verdict must pass too. `--break-keep` empties the data before launch 2 (a device that forgets): the gate must fail.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::Result;
use atl_android_paths::{DATA_APP, DATA_USER, DATA_USER_DE, SHARED_STORAGE};

use crate::atld_run::{Runtime, clear_package_data, package_data_dirs};
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
/// The fixture's file under its files dirs and the value it holds (testdata/app-data DataMain.java).
const FILE: &str = "save.txt";
const VALUE: &str = "save-v1";
/// The fixture's log line before its state: `AtlData: data: <state> dir=<files dir>`.
const DATA_LINE: &str = "AtlData: data: ";
/// The fixture's log line before the dirs it sees: `AtlData: paths files=... de=... external=... source=...`.
const PATHS_LINE: &str = "AtlData: paths ";

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
    let data_root = out.join("app-data").join("data-root");
    let log = out
        .join("run-apk")
        .join("root")
        .join("log")
        .join(format!("{package}.log"));
    // the fixture's file in each of its host data dirs: data/files, data_de/files, external/data/files (Android/data/<package>/files)
    let [data, data_de, external] = package_data_dirs(&data_root, &package);
    let saved = [
        data.join("files").join(FILE),
        data_de.join("files").join(FILE),
        external.join("data/files").join(FILE),
    ];
    let files_dir = format!("{DATA_USER}/0/{package}/files");
    // AOSP's paths (Environment / ContextImpl), the ones the app saw before the per-package host layout: the host layout must not show through
    let want_paths = format!(
        "files={files_dir} de={DATA_USER_DE}/0/{package}/files external={SHARED_STORAGE}/Android/data/{package}/files source={DATA_APP}/{package}/base.apk"
    );
    // the gate starts from empty data: this package's data from an earlier run is cleared
    clear_package_data(&data_root, &package)?;
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
            false,
        )
        .with_clear_data(clear)
        .with_data_root(&data_root);
        let code = run_apk::run(root, &launch)?;
        let text = std::fs::read_to_string(&log).unwrap_or_default();
        let state = data_state(&text, &files_dir);
        let paths = line_after(&text, PATHS_LINE);
        let missing: Vec<&PathBuf> = saved
            .iter()
            .filter(|f| std::fs::read_to_string(f).ok().as_deref() != Some(VALUE))
            .collect();
        let others = other_packages_with_data(&data_root, &package, FILE)?;
        println!(
            "app-data: launch {n} ({what}): data {} (want {want:?}), {}/{} host files present, paths {}, {} other packages' data dirs ({} holding the file)",
            state.as_deref().unwrap_or("no data line"),
            saved.len() - missing.len(),
            saved.len(),
            if paths.as_deref() == Some(want_paths.as_str()) {
                "AOSP's"
            } else {
                "differ"
            },
            others.0,
            others.1.len()
        );
        if code != ExitCode::SUCCESS {
            whys.push(format!("launch {n}: run-apk's verdict failed"));
        }
        if state.as_deref() != Some(want.as_str()) {
            whys.push(format!(
                "launch {n} ({what}): data {state:?}, want {want:?}"
            ));
        }
        for f in missing {
            whys.push(format!(
                "launch {n}: {} is not {VALUE:?} on the host",
                f.display()
            ));
        }
        if paths.as_deref() != Some(want_paths.as_str()) {
            whys.push(format!(
                "launch {n}: the app saw {paths:?}, want {want_paths:?}"
            ));
        }
        if others.0 == 0 {
            whys.push(format!(
                "launch {n}: no other package has a data dir under {}: separation not shown",
                data_root.join("apps").display()
            ));
        }
        for o in others.1 {
            whys.push(format!(
                "launch {n}: {o}'s data dir holds the fixture's {FILE}: data dirs not separate"
            ));
        }
    }
    // the data root is left as found: this package's data is removed again
    clear_package_data(&data_root, &package)?;
    if whys.is_empty() {
        println!(
            "app-data: pass (3 launches: empty data, kept, --clear-data; AOSP paths; own host dirs)"
        );
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

/// The rest of the first line holding `marker`, after it.
fn line_after(log: &str, marker: &str) -> Option<String> {
    log.lines()
        .find_map(|line| Some(line.split_once(marker)?.1.trim_end().to_owned()))
}

/// The packages other than `package` with a host data dir under `data_root`'s `apps/`: their count, and those whose `data/files` holds `file`.
fn other_packages_with_data(
    data_root: &Path,
    package: &str,
    file: &str,
) -> Result<(usize, Vec<String>)> {
    let mut count = 0;
    let mut holding = Vec::new();
    let Ok(entries) = std::fs::read_dir(data_root.join("apps")) else {
        return Ok((0, holding));
    };
    for e in entries {
        let e = e?;
        let name = e.file_name().to_string_lossy().into_owned();
        if name == package || !e.path().join("data").is_dir() {
            continue;
        }
        count += 1;
        if e.path().join("data/files").join(file).exists() {
            holding.push(name);
        }
    }
    Ok((count, holding))
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

    #[test]
    fn line_after_takes_the_first_marked_line() {
        let log = "x\nI AtlData: paths files=/a de=/b \nI AtlData: paths files=/c\n";
        assert_eq!(
            line_after(log, PATHS_LINE).as_deref(),
            Some("files=/a de=/b")
        );
        assert_eq!(line_after("", PATHS_LINE), None);
    }

    /// Only other packages with a `data` dir count; the one holding the file is named.
    #[test]
    fn other_packages_are_counted_and_checked() {
        let r = std::env::temp_dir().join(format!("atl-app-data-others-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&r);
        for d in [
            "apps/me/data/files",
            "apps/a/data",
            "apps/b/data/files",
            "apps/c/apk",
        ] {
            std::fs::create_dir_all(r.join(d)).unwrap();
        }
        std::fs::write(r.join("apps/b/data/files/save.txt"), "x").unwrap();
        let (n, holding) = other_packages_with_data(&r, "me", "save.txt").unwrap();
        assert_eq!(n, 2);
        assert_eq!(holding, vec!["b".to_owned()]);
        std::fs::remove_dir_all(&r).unwrap();
    }
}
