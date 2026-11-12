// SPDX-License-Identifier: MIT
//! The installer process (plan item 4b, slices M1-M2): AOSP's own package parsing and native-code choice in a VM of its own, no Java of ours. Started as `atl-installer [VM options...] <package dir> <native library root>`; the dir holds the base APK and its splits (AOSP names them `base.apk`, `split_<name>.apk`), the root receives the chosen ABI's libraries under `<root>/<isa>`.
//! Steps: the app process's start up to the VM (lib.rs steps 0-3), plus `-Xhidden-api-policy:just-warn` (installer VM only: it calls `@hide` framework code from JNI); libandroid_runtime around the VM (step 5a: the framework's natives, ApkAssets and NativeLibraryHelper among them);
//! then `ApkLiteParseUtils.parseClusterPackageLite` over the dir (`ffi::Vm::parse_cluster`, which also refuses a split whose package or versionCode differs from the base), then the install path's native code (`Cluster::install_natives`, M2).
//! Output on stdout, one `key value` per line: `package`, `version-code` (PackageLite.getLongVersionCode), one `split` per split name in AOSP's order, `primary-abi <abi>|none`, `lib-dir <path>|none`, then `time-ms vm <n> parse <n> natives <n>`.
//! Errors: stderr `parse error <code>: <message>` (PackageManager INSTALL_PARSE_FAILED_* code) or `native error <code>: <name>` (INSTALL_FAILED_* code), exit 1.

use std::ffi::{CString, OsString};
use std::io::Write as _;
use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};
use std::path::PathBuf;
use std::time::Instant;

use anyhow::{Context as _, Result, anyhow};

use crate::ffi;

/// ART's hidden-API enforcement off with a warning (art/runtime/parsed_options.cc `-Xhidden-api-policy:_`): the installer reaches `@hide` classes (ApkLiteParseUtils, PackageLite) from JNI. App VMs never get it.
const HIDDEN_API_POLICY: &std::ffi::CStr = c"-Xhidden-api-policy:just-warn";

const USAGE: &str = "usage: atl-installer [VM options...] <package dir> <native library root>";

/// argv[1..]: VM options (each one token starting with `-`), then the package dir and the native library root.
fn parse(argv: Vec<OsString>) -> Result<(Vec<CString>, PathBuf, PathBuf)> {
    let mut options = Vec::new();
    let mut paths = Vec::new();
    for a in argv.into_iter().skip(1) {
        if a.as_bytes().starts_with(b"-") {
            options.push(CString::new(a.into_vec()).context("VM option")?);
        } else {
            paths.push(PathBuf::from(a));
        }
    }
    let [dir, lib_root]: [PathBuf; 2] = paths
        .try_into()
        .map_err(|p: Vec<PathBuf>| anyhow!("{USAGE}: {} paths given", p.len()))?;
    Ok((options, dir, lib_root))
}

fn run(argv: Vec<OsString>) -> Result<()> {
    let (mut options, dir, lib_root) = parse(argv)?;
    options.push(HIDDEN_API_POLICY.to_owned());
    let start = Instant::now();
    let rt = crate::load_runtime()?;
    let vm = rt.create_vm(&options)?;
    rt.install_logger();
    let android = ffi::AndroidRuntime::load()?;
    android.attach(&vm)?;
    let vm_ms = start.elapsed().as_millis();
    let parse_start = Instant::now();
    let cluster = vm.parse_cluster(&dir)?;
    let parse_ms = parse_start.elapsed().as_millis();
    let natives_start = Instant::now();
    let natives = cluster.install_natives(&lib_root)?;
    let natives_ms = natives_start.elapsed().as_millis();
    let mut out = std::io::stdout().lock();
    writeln!(out, "package {}", cluster.package)?;
    writeln!(out, "version-code {}", cluster.version_code)?;
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
        "time-ms vm {vm_ms} parse {parse_ms} natives {natives_ms}"
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
    fn options_then_dir_and_root() {
        let (o, d, r) = parse(["x", "-Xa", "-Xb", "/p", "/lib"].map(Into::into).to_vec()).unwrap();
        assert_eq!(o, [c"-Xa".to_owned(), c"-Xb".to_owned()]);
        assert_eq!(d, std::path::Path::new("/p"));
        assert_eq!(r, std::path::Path::new("/lib"));
        assert!(parse(["x", "-Xa", "/p"].map(Into::into).to_vec()).is_err());
        assert!(parse(["x", "/p", "/q", "/r"].map(Into::into).to_vec()).is_err());
    }
}
