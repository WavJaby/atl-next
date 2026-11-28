// SPDX-License-Identifier: MIT
//! `xtask test app-data`: an app's data outlives a run-apk launch (AOSP: until uninstall or `pm clear`). Three `run-apk` launches of the fixture (xtask-harness/testdata/app-data) on the gate's own data root (`<out>/app-data/data-root`, never the user's), made empty at the gate's start and passed to every launch as `--reuse-data-root` (the explicit not-a-clean-run opt-in this gate exists to prove):
//! 1. on empty data the app writes `save.txt` under its files dir (and its device-protected and external files dirs): `AtlData: data: wrote`;
//! 2. the next launch reads it back: `AtlData: data: read save-v1`, and the file is on the host under each of `apps/<package>/{data,data_de,external}`;
//! 3. `--clear-data` (`pm clear`) empties the package's data first, so the app writes again: `AtlData: data: wrote`.
//!
//! Shortcuts follow the same life (ShortcutService persists them per package; handlePackageDataCleared drops them on pm clear): `AtlData: shortcut:` lists the dynamic shortcuts at start, `-` at launch 1
//! (it then publishes `keep`), `keep` at launch 2 (a new atld + services read it back from the state dir), `-` at launch 3.
//!
//! Every launch: the app sees AOSP's paths (`AtlData: paths ...`: getFilesDir, the device-protected getFilesDir, getExternalFilesDir, sourceDir), whatever the host layout; and another package (a system
//! package the run scans) has its own host data dir holding none of the fixture's files.
//! Each launch is run-apk's own run (`run_apk::run`), so its window verdict must pass too. `--break-keep` empties the data before launch 2 (a device that forgets): the gate must fail.
//!
//! The code is installed once (AOSP: NativeLibraryHelper extracts native libraries at install, never at launch): the fixture's APK carries `lib/<abi>/libatldata.so` (extracted, never loaded).
//! Launches 2 and 3 run the unchanged APK: 0 bytes written under `apps/<package>/apk/lib` (every file keeps its inode and ctime) and atld logs `install skipped: unchanged`.
//! 4. control: the APK rebuilt with that library's bytes changed (same size: only AOSP's CRC check tells), `--clear-data`: the library is rewritten with the new bytes, and no `install skipped`.

use std::collections::BTreeMap;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result};
use atl_android_paths::{DATA_APP, DATA_USER, DATA_USER_DE, SHARED_STORAGE};

use crate::atld_run::{NativeLibs, Runtime, package_data_dirs};
use crate::{arches, manifest, run_apk, verdict};

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
/// The fixture's log line before the dynamic shortcut ids it found at start (`-` for none; DataMain.shortcut).
const SHORTCUT_LINE: &str = "AtlData: shortcut: ";
/// The fixture's native library: no ELF, never loaded; only its extraction into the code dir is observed.
const LIB: &str = "libatldata.so";
/// Its bytes in the APK of launches 1-3 and of launch 4: one length, so only the content (AOSP isFileDifferent's CRC) differs.
const LIB_V1: &[u8] = b"atl app-data library v1\n";
const LIB_V2: &[u8] = b"atl app-data library v2\n";
/// The manifest's minSdkVersion (testdata/app-data AndroidManifest.xml): NativeLibs' API level, which aapt2 only fills where the manifest has none.
const MIN_SDK: u32 = 21;
/// atld's log line for a session equal to the installed code (crates/atld main.rs `Host::install`).
const UNCHANGED_LINE: &str = "install skipped: unchanged";

/// What a launch must do to the installed code's libraries.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Code {
    /// installed now into the empty root: the library holds these bytes
    Installed(&'static [u8]),
    /// unchanged APK: nothing under lib/ written, atld's unchanged line
    Kept,
    /// changed APK: the library rewritten with these bytes, no unchanged line
    Rewritten(&'static [u8]),
}

pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    let rt = Runtime::new(
        root,
        "run-apk",
        &args.host,
        args.out.as_deref(),
        args.java_out.as_deref(),
        "app-data",
        false,
    )?;
    let out = rt.out.clone();
    // the host arch's first Android ABI: the device's primary, so the installer extracts the fixture's library
    let arches = arches::load(root)?;
    let abi = arches::find_host(&arches, &args.host)
        .and_then(|h| h.arch.guest.as_ref())
        .and_then(|g| g.android_abis.first().cloned())
        .with_context(|| format!("arches.toml has no [{}.guest] android_abis", args.host))?;
    let build = |lib: &[u8]| -> Result<PathBuf> {
        let src = out.join("app-data").join("lib");
        std::fs::create_dir_all(&src)?;
        std::fs::write(src.join(LIB), lib)?;
        let native = NativeLibs {
            abi: abi.clone(),
            libs: vec![src.join(LIB)],
            min_sdk: MIN_SDK,
        };
        rt.apk.build(
            &root.join(FIXTURE),
            &out.join("app-data").join("apk"),
            &[],
            Some(&native),
            None,
        )
    };
    let mut apk = build(LIB_V1)?;
    let package = manifest::read(&rt.apk.aapt2, &apk)?.name;
    let data_root = out.join("app-data").join("data-root");
    let code_dir = data_root.join("apps").join(&package).join("apk");
    let atld_log = out.join("run-apk").join("scratch").join("atld.log");
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
    // the gate starts from an empty root: nothing from an earlier gate run (data or installed code)
    if data_root.exists() {
        xtask::out_run::remove(&data_root)?;
    }
    std::fs::create_dir_all(&data_root).with_context(|| data_root.display().to_string())?;
    let mut whys = Vec::new();
    let launches = [
        (
            1,
            false,
            "wrote".to_owned(),
            "-",
            "empty data",
            Code::Installed(LIB_V1),
        ),
        (
            2,
            args.break_keep,
            format!("read {VALUE}"),
            "keep",
            "data kept",
            Code::Kept,
        ),
        (3, true, "wrote".to_owned(), "-", "--clear-data", Code::Kept),
        (
            4,
            true,
            "wrote".to_owned(),
            "-",
            "changed APK, --clear-data",
            Code::Rewritten(LIB_V2),
        ),
    ];
    for (n, clear, want, want_shortcut, what, want_code) in launches {
        if let Code::Rewritten(lib) = want_code {
            apk = build(lib)?;
        }
        let libs_before = lib_files(&code_dir)?;
        let launch = run_apk::Args::driven(
            &apk,
            &args.host,
            Some(out.clone()),
            args.java_out.clone(),
            false,
            false,
        )
        .with_reused_data_root(&data_root)
        .with_clear_data(clear);
        let code = run_apk::run(root, &launch)?;
        let libs_after = lib_files(&code_dir)?;
        let unchanged = std::fs::read_to_string(&atld_log)
            .unwrap_or_default()
            .contains(UNCHANGED_LINE);
        let lib_now = libs_after
            .iter()
            .find(|(p, _)| p.file_name().is_some_and(|f| f == LIB))
            .and_then(|(p, _)| std::fs::read(p).ok());
        let written = rewritten_bytes(&libs_before, &libs_after);
        println!(
            "app-data: launch {n} ({what}): code: {written} bytes written under lib/ ({} files), {LIB} {}, atld {}",
            libs_after.len(),
            match &lib_now {
                Some(b) => String::from_utf8_lossy(b).trim_end().to_owned(),
                None => "missing".to_owned(),
            },
            if unchanged {
                UNCHANGED_LINE
            } else {
                "installed"
            }
        );
        whys.extend(code_whys(
            n,
            want_code,
            written,
            lib_now.as_deref(),
            unchanged,
        ));
        let text = std::fs::read_to_string(&log).unwrap_or_default();
        let state = data_state(&text, &files_dir);
        let paths = line_after(&text, PATHS_LINE);
        let shortcut = line_after(&text, SHORTCUT_LINE);
        println!(
            "app-data: launch {n} ({what}): shortcuts at start {shortcut:?} (want {want_shortcut:?})"
        );
        if shortcut.as_deref() != Some(want_shortcut) {
            whys.push(format!(
                "launch {n} ({what}): shortcuts at start {shortcut:?}, want {want_shortcut:?}"
            ));
        }
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
    if whys.is_empty() {
        println!(
            "app-data: pass (4 launches: empty data, kept, --clear-data, changed APK; AOSP paths; own host dirs; code installed once, libs rewritten only for a changed APK)"
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

/// Every file under the code dir's `lib/` -> (inode, ctime ns, size); empty when there is none.
fn lib_files(code_dir: &Path) -> Result<BTreeMap<PathBuf, (u64, i128, u64)>> {
    let mut out = BTreeMap::new();
    let mut dirs = vec![code_dir.join("lib")];
    while let Some(d) = dirs.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in entries {
            let e = e?;
            let m = std::fs::symlink_metadata(e.path())?;
            if m.is_dir() {
                dirs.push(e.path());
            } else {
                let ctime = i128::from(m.ctime()) * 1_000_000_000 + i128::from(m.ctime_nsec());
                out.insert(e.path(), (m.ino(), ctime, m.len()));
            }
        }
    }
    Ok(out)
}

/// Bytes of the files in `after` that are new or whose inode or ctime moved since `before` (a write in place moves ctime; AOSP's tmp + rename makes a new inode).
fn rewritten_bytes(
    before: &BTreeMap<PathBuf, (u64, i128, u64)>,
    after: &BTreeMap<PathBuf, (u64, i128, u64)>,
) -> u64 {
    after
        .iter()
        .filter(|(p, (ino, ctime, _))| {
            before
                .get(*p)
                .is_none_or(|(i, c, _)| (i, c) != (ino, ctime))
        })
        .map(|(_, (_, _, size))| size)
        .sum()
}

/// Launch `n`'s failures against `want`: `written` bytes under lib/, the library's bytes now, whether atld logged the unchanged line.
fn code_whys(n: u32, want: Code, written: u64, lib: Option<&[u8]>, unchanged: bool) -> Vec<String> {
    let mut whys = Vec::new();
    let content = |bytes: &[u8], whys: &mut Vec<String>| {
        if lib != Some(bytes) {
            whys.push(format!(
                "launch {n}: {LIB} holds {:?}, want {:?}",
                lib.map(String::from_utf8_lossy),
                String::from_utf8_lossy(bytes)
            ));
        }
    };
    match want {
        Code::Installed(bytes) => content(bytes, &mut whys),
        Code::Kept => {
            if written != 0 {
                whys.push(format!(
                    "launch {n}: unchanged APK, {written} bytes written under lib/ (want 0: installed once)"
                ));
            }
            if !unchanged {
                whys.push(format!(
                    "launch {n}: unchanged APK, atld has no {UNCHANGED_LINE:?}"
                ));
            }
        }
        Code::Rewritten(bytes) => {
            content(bytes, &mut whys);
            if written == 0 {
                whys.push(format!(
                    "launch {n}: changed APK, nothing written under lib/"
                ));
            }
            if unchanged {
                whys.push(format!(
                    "launch {n}: changed APK, atld logged {UNCHANGED_LINE:?}"
                ));
            }
        }
    }
    whys
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

    /// A new inode or a moved ctime counts the file's size; an untouched file and a removed one count nothing.
    #[test]
    fn rewritten_bytes_counts_new_inodes_and_ctimes() {
        let p = |s: &str| PathBuf::from(s);
        let before = BTreeMap::from([
            (p("a"), (1, 10, 5)),
            (p("b"), (2, 20, 7)),
            (p("c"), (3, 30, 11)),
            (p("gone"), (4, 40, 13)),
        ]);
        let after = BTreeMap::from([
            (p("a"), (1, 10, 5)),
            (p("b"), (9, 20, 7)),
            (p("c"), (3, 31, 11)),
            (p("new"), (5, 50, 17)),
        ]);
        // b (7, new inode) + c (11, ctime) + new (17)
        assert_eq!(rewritten_bytes(&before, &after), 7 + 11 + 17);
    }

    /// Kept wants 0 bytes and the unchanged line; Rewritten wants the new bytes, some write and no unchanged line.
    #[test]
    fn code_whys_judge_each_want() {
        assert!(code_whys(2, Code::Kept, 0, Some(LIB_V1), true).is_empty());
        assert_eq!(code_whys(2, Code::Kept, 24, Some(LIB_V1), false).len(), 2);
        assert!(code_whys(4, Code::Rewritten(LIB_V2), 24, Some(LIB_V2), false).is_empty());
        assert_eq!(
            code_whys(4, Code::Rewritten(LIB_V2), 0, Some(LIB_V1), true).len(),
            3
        );
        assert_eq!(
            code_whys(1, Code::Installed(LIB_V1), 0, None, false).len(),
            1
        );
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
