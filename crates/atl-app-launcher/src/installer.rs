// SPDX-License-Identifier: MIT
//! The installer process (plan item 4b, slices M1-M3): AOSP's own package parsing and native-code choice in a VM of its own. Started as `atl-installer [--feature=<name>...] [VM options...] <package dir> <native library root>`;
//! the dir holds the base APK and its splits (AOSP names them `base.apk`, `split_<name>.apk`), the root receives the chosen ABI's libraries under `<root>/<isa>`. The VM options name the api-impl dex (`-Djava.class.path=`), whose ParsingPackageUtils.Callback forwarder the full parse needs.
//! `--feature=<name>`: a feature this device declares (PMS hasSystemFeature, the parse's `hasFeature`); the spawner passes the set atld's PackageManager answers with.
//! Steps: the app process's start up to the VM (lib.rs steps 0-3), plus `-Xhidden-api-policy:just-warn` (installer VM only: it calls `@hide` framework code from JNI); libandroid_runtime around the VM (step 5a: the framework's natives, ApkAssets and NativeLibraryHelper among them);
//! then `ApkLiteParseUtils.parseClusterPackageLite` over the dir (`ffi::Vm::parse_cluster`, M1), the full parse (`package::parse`, M3), then the install path's native code (`Cluster::install_natives`, M2).
//! Output on stdout, one `key value` per line: the full parse's lines (`package`, `version-code`, `version-name`, `min-sdk`, `target-sdk`, `app-class`, `app-flags`, `extract-native-libs`, `app-process`, `app-theme`,
//! `uses-permission`*, `permission`*, `activity`/`receiver`/`service`/`provider` <name> key=value...*, `filter <kind> <component> actions= categories= schemes= types=`*), one `split` per split name in AOSP's order,
//! `primary-abi <abi>|none`, `lib-dir <path>|none`, then `time-ms vm <n> lite <n> parse <n> natives <n>`.
//! Errors: stderr `parse error <code>: <message>` (PackageManager INSTALL_PARSE_FAILED_* code) or `native error <code>: <name>` (INSTALL_FAILED_* code), exit 1.

use std::ffi::{CString, OsString};
use std::io::Write as _;
use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};
use std::path::PathBuf;
use std::time::Instant;

use anyhow::{Context as _, Result, anyhow};

use crate::{ffi, package};

/// ART's hidden-API enforcement off with a warning (art/runtime/parsed_options.cc `-Xhidden-api-policy:_`): the installer reaches `@hide` classes (ApkLiteParseUtils, PackageLite) from JNI. App VMs never get it.
const HIDDEN_API_POLICY: &std::ffi::CStr = c"-Xhidden-api-policy:just-warn";

const USAGE: &str = "usage: atl-installer [--feature=<name>...] [VM options...] <package dir> <native library root>";
const FEATURE: &str = "--feature=";

/// The parsed command line.
struct Args {
    options: Vec<CString>,
    features: Vec<String>,
    dir: PathBuf,
    lib_root: PathBuf,
}

/// argv[1..]: `--feature=` options, VM options (each one token starting with `-`), then the package dir and the native library root.
fn parse(argv: Vec<OsString>) -> Result<Args> {
    let mut options = Vec::new();
    let mut features = Vec::new();
    let mut paths = Vec::new();
    for a in argv.into_iter().skip(1) {
        if let Some(f) = a.to_str().and_then(|s| s.strip_prefix(FEATURE)) {
            features.push(f.to_owned());
        } else if a.as_bytes().starts_with(b"-") {
            options.push(CString::new(a.into_vec()).context("VM option")?);
        } else {
            paths.push(PathBuf::from(a));
        }
    }
    let [dir, lib_root]: [PathBuf; 2] = paths
        .try_into()
        .map_err(|p: Vec<PathBuf>| anyhow!("{USAGE}: {} paths given", p.len()))?;
    Ok(Args {
        options,
        features,
        dir,
        lib_root,
    })
}

fn run(argv: Vec<OsString>) -> Result<()> {
    let Args {
        mut options,
        features,
        dir,
        lib_root,
    } = parse(argv)?;
    options.push(HIDDEN_API_POLICY.to_owned());
    let start = Instant::now();
    let rt = crate::load_runtime()?;
    let vm = rt.create_vm(&options)?;
    rt.install_logger();
    let android = ffi::AndroidRuntime::load()?;
    android.attach(&vm)?;
    let vm_ms = start.elapsed().as_millis();
    let lite_start = Instant::now();
    let cluster = vm.parse_cluster(&dir)?;
    let lite_ms = lite_start.elapsed().as_millis();
    let parse_start = Instant::now();
    let parsed = package::parse(&vm, &dir, &package::Device { features })?;
    let parse_ms = parse_start.elapsed().as_millis();
    let natives_start = Instant::now();
    let natives = cluster.install_natives(&lib_root)?;
    let natives_ms = natives_start.elapsed().as_millis();
    let mut out = std::io::stdout().lock();
    for line in &parsed {
        writeln!(out, "{line}")?;
    }
    for s in &cluster.splits {
        writeln!(out, "split {s}")?;
    }
    writeln!(
        out,
        "primary-abi {}",
        natives.primary_abi.as_deref().unwrap_or("none")
    )?;
    match &natives.lib_dir {
        Some(d) => writeln!(out, "lib-dir {}", d.display())?,
        None => writeln!(out, "lib-dir none")?,
    }
    writeln!(
        out,
        "time-ms vm {vm_ms} lite {lite_ms} parse {parse_ms} natives {natives_ms}"
    )?;
    drop(out);
    drop(cluster);
    vm.shut_down()
}

/// The installer's whole run, argv -> exit status (`ffi::atl_installer_main`, the C entry).
pub fn main(argv: Vec<OsString>) -> i32 {
    crate::trace::install();
    match run(argv) {
        Ok(()) => 0,
        Err(e) => {
            let _ = writeln!(std::io::stderr(), "atl-installer: {e:#}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::parse;

    #[test]
    fn features_options_then_dir_and_root() {
        let a = parse(
            [
                "x",
                "--feature=android.hardware.camera",
                "-Xa",
                "-Xb",
                "/p",
                "/lib",
            ]
            .map(Into::into)
            .to_vec(),
        )
        .unwrap();
        assert_eq!(a.features, ["android.hardware.camera"]);
        assert_eq!(a.options, [c"-Xa".to_owned(), c"-Xb".to_owned()]);
        assert_eq!(a.dir, std::path::Path::new("/p"));
        assert_eq!(a.lib_root, std::path::Path::new("/lib"));
        assert!(parse(["x", "-Xa", "/p"].map(Into::into).to_vec()).is_err());
        assert!(parse(["x", "/p", "/q", "/r"].map(Into::into).to_vec()).is_err());
    }
}
