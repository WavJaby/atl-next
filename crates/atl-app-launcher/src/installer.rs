// SPDX-License-Identifier: MIT
//! The installer process (plan item 4b, slice M1): AOSP's own package parsing in a VM of its own, no Java of ours. Started as `atl-installer [VM options...] <package dir>`; the dir holds the base APK and its splits (AOSP names them `base.apk`, `split_<name>.apk`).
//! Steps: the app process's start up to the VM (lib.rs steps 0-3), plus `-Xhidden-api-policy:just-warn` (installer VM only: it calls `@hide` framework code from JNI); libandroid_runtime around the VM (step 5a: the framework's natives, ApkAssets among them);
//! then `ApkLiteParseUtils.parseClusterPackageLite` over the dir (`ffi::Vm::parse_cluster`), which also refuses a split whose package or versionCode differs from the base.
//! Output on stdout, one `key value` per line: `package`, `version-code` (PackageLite.getLongVersionCode), one `split` per split name in AOSP's order, then `time-ms vm <n> parse <n>`. A parse error: stderr `parse error <code>: <message>` (PackageManager INSTALL_PARSE_FAILED_* code), exit 1.

use std::ffi::{CString, OsString};
use std::io::Write as _;
use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};
use std::path::PathBuf;
use std::time::Instant;

use anyhow::{Context as _, Result, bail};

use crate::ffi;

/// ART's hidden-API enforcement off with a warning (art/runtime/parsed_options.cc `-Xhidden-api-policy:_`): the installer reaches `@hide` classes (ApkLiteParseUtils, PackageLite) from JNI. App VMs never get it.
const HIDDEN_API_POLICY: &std::ffi::CStr = c"-Xhidden-api-policy:just-warn";

/// argv[1..]: VM options (each one token starting with `-`), then the package dir.
fn parse(argv: Vec<OsString>) -> Result<(Vec<CString>, PathBuf)> {
    let mut options = Vec::new();
    let mut dir = None;
    for a in argv.into_iter().skip(1) {
        if dir.is_some() {
            bail!("usage: atl-installer [VM options...] <package dir>: unexpected {a:?}");
        }
        if a.as_bytes().starts_with(b"-") {
            options.push(CString::new(a.into_vec()).context("VM option")?);
        } else {
            dir = Some(PathBuf::from(a));
        }
    }
    let dir = dir.context("usage: atl-installer [VM options...] <package dir>")?;
    Ok((options, dir))
}

fn run(argv: Vec<OsString>) -> Result<()> {
    let (mut options, dir) = parse(argv)?;
    options.push(HIDDEN_API_POLICY.to_owned());
    let start = Instant::now();
    let rt = crate::load_runtime()?;
    let vm = rt.create_vm(&options)?;
    rt.install_logger();
    let android = ffi::AndroidRuntime::load()?;
    android.attach(&vm)?;
    let vm_ms = start.elapsed().as_millis();
    let parse_start = Instant::now();
    let lite = vm.parse_cluster(&dir)?;
    let parse_ms = parse_start.elapsed().as_millis();
    let mut out = std::io::stdout().lock();
    writeln!(out, "package {}", lite.package)?;
    writeln!(out, "version-code {}", lite.version_code)?;
    for s in &lite.splits {
        writeln!(out, "split {s}")?;
    }
    writeln!(out, "time-ms vm {vm_ms} parse {parse_ms}")?;
    drop(out);
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
    fn options_then_one_dir() {
        let (o, d) = parse(["x", "-Xa", "-Xb", "/p"].map(Into::into).to_vec()).unwrap();
        assert_eq!(o, [c"-Xa".to_owned(), c"-Xb".to_owned()]);
        assert_eq!(d, std::path::Path::new("/p"));
        assert!(parse(["x", "-Xa"].map(Into::into).to_vec()).is_err());
        assert!(parse(["x", "/p", "/q"].map(Into::into).to_vec()).is_err());
    }
}
