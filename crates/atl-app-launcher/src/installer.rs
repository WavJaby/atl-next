// SPDX-License-Identifier: MIT
//! The installer process (plan item 4b, slices M1-M4a): AOSP's own package parsing and native-code choice in a VM of its own. Started as `atl-installer [--feature=<name>...] [VM options...] <package dir> <native library root>`;
//! the dir holds the base APK and its splits (AOSP names them `base.apk`, `split_<name>.apk`), the root receives the chosen ABI's libraries under `<root>/<isa>`. No root (a system scan: the
//! partition is read-only, PMS copies nothing there): parse only, no ABI choice, primaryCpuAbi unset. The VM options name the api-impl dex (`-Djava.class.path=`), whose ParsingPackageUtils.Callback forwarder the full parse needs.
//! `--feature=<name>`: a feature this device declares (PMS hasSystemFeature, the parse's `hasFeature`); the spawner passes the set atld's PackageManager answers with.
//! Steps: the app process's start up to the VM (lib.rs steps 0-3), plus `-Xhidden-api-policy:just-warn` (installer VM only: it calls `@hide` framework code from JNI); libandroid_runtime around the VM (step 5a: the framework's natives, ApkAssets and NativeLibraryHelper among them);
//! then `ApkLiteParseUtils.parseClusterPackageLite` over the dir (`ffi::Vm::parse_cluster`, M1), the full parse (`package::parse`, M3), then the install path's native code (`Cluster::install_natives`, M2).
//! Output on stdout: the frames of atl-services-types `installer_output` (the PackageInfo parcel with the ABI choice on its ApplicationInfo, the components' intent filters); on stderr after them
//! `time-ms vm <n> lite <n> parse <n> natives <n>`.
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

const USAGE: &str = "usage: atl-installer [--feature=<name>...] [VM options...] <package dir> [<native library root>]";
const FEATURE: &str = "--feature=";

/// The parsed command line.
struct Args {
    options: Vec<CString>,
    features: Vec<String>,
    dir: PathBuf,
    lib_root: Option<PathBuf>,
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
    let n = paths.len();
    let mut paths = paths.into_iter();
    let (Some(dir), lib_root, None) = (paths.next(), paths.next(), paths.next()) else {
        return Err(anyhow!("{USAGE}: {n} paths given"));
    };
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
    let natives = lib_root
        .map(|root| cluster.install_natives(&root))
        .transpose()?;
    let natives_ms = natives_start.elapsed().as_millis();
    let frames = parsed.frames(
        &vm,
        &package::Abi {
            primary: natives.as_ref().and_then(|n| n.primary_abi.as_deref()),
            lib_dir: natives.as_ref().and_then(|n| n.lib_dir.as_deref()),
        },
    )?;
    let mut out = std::io::stdout().lock();
    out.write_all(&frames)?;
    out.flush()?;
    drop(out);
    writeln!(
        std::io::stderr(),
        "time-ms vm {vm_ms} lite {lite_ms} parse {parse_ms} natives {natives_ms}"
    )?;
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
        assert_eq!(a.lib_root.as_deref(), Some(std::path::Path::new("/lib")));
        let scan = parse(["x", "-Xa", "/p"].map(Into::into).to_vec()).unwrap();
        assert_eq!(
            (scan.dir.as_path(), scan.lib_root),
            (std::path::Path::new("/p"), None)
        );
        assert!(parse(["x", "-Xa"].map(Into::into).to_vec()).is_err());
        assert!(parse(["x", "/p", "/q", "/r"].map(Into::into).to_vec()).is_err());
    }
}
