// SPDX-License-Identifier: MIT
//! Install-time extraction of an APK's native libraries, a stand-in for AOSP's own: `NativeLibraryHelper.copyNativeBinariesForSupportedAbi` (Java) over libandroid_runtime's
//! `com_android_internal_content_NativeLibraryHelper.cpp` (`findSupportedAbi`, `copyNativeBinaries`), as `PackageAbiHelperImpl.derivePackageAbi` calls them.
//! Temporary: deleted when the installer process (AOSP Java + JNI on ART) installs packages -- it runs AOSP's NativeLibraryHelper itself, and atld's install step goes with it.
//! Reproduced: ABI choice = the device's most preferred ABI (`Build.SUPPORTED_ABIS` order) that the APK has a `lib/<abi>/` entry for; libraries go to `<app>/lib/<isa>/` (`VMRuntime.getInstructionSet`).
//! Not reproduced: the 64-bit-then-32-bit split (one list, preference order), `extractNativeLibs="false"` (AOSP then loads from `base.apk!/lib/<abi>/` without copying; here always extracted),
//! page-alignment checks, `isFilenameSafe` beyond "one path component, `.so`".

use std::io::Read;
use std::path::Path;

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

/// `NativeLibraryHelper.LIB_DIR_NAME`: the APK's library directory, and the one below the install directory.
pub const LIB_DIR_NAME: &str = "lib";

/// One device-supported ABI and its instruction set (`VMRuntime.getInstructionSet(abi)`: the arches.toml row whose `android_abis` lists it).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SupportedAbi {
    pub abi: String,
    pub isa: String,
}

/// The result of [`extract`]: what PackageManagerService records (`primaryCpuAbi`; `nativeLibraryDir` = `<root>/<isa>`, root = `<code path>/lib`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeLibs {
    pub primary_cpu_abi: Option<String>,
    pub native_library_dir: String,
}

/// Index into `supported` of the ABI to install (`findSupportedAbi`: the lowest index any `lib/<abi>/<file>` entry matches), `None` when no entry matches.
pub fn find_supported_abi<'a>(
    entries: impl IntoIterator<Item = &'a str>,
    supported: &[SupportedAbi],
) -> Option<usize> {
    entries
        .into_iter()
        .filter_map(|e| library_entry(e).map(|(abi, _)| abi))
        .filter_map(|abi| supported.iter().position(|s| s.abi == abi))
        .min()
}

/// `(abi, file)` of an APK entry `lib/<abi>/<file>.so` with one path component in `file`.
fn library_entry(entry: &str) -> Option<(&str, &str)> {
    let rest = entry.strip_prefix(LIB_DIR_NAME)?.strip_prefix('/')?;
    let (abi, file) = rest.split_once('/')?;
    (!abi.is_empty() && !file.contains('/') && file.ends_with(".so") && file != ".so")
        .then_some((abi, file))
}

/// Extracts the libraries of the chosen ABI of `apk` into `<app_dir>/lib/<isa>/`; `guest_app_dir` is the same directory as the app sees it (`/data/app/<package>`).
/// Without a supported ABI the directory is still `lib/<isa of supported[0]>` (`InstructionSets.getPrimaryInstructionSet` falls back to the preferred one), and nothing is created.
pub fn extract(
    apk: &Path,
    app_dir: &Path,
    guest_app_dir: &str,
    supported: &[SupportedAbi],
) -> Result<NativeLibs> {
    ensure!(!supported.is_empty(), "no supported ABIs");
    let file = std::fs::File::open(apk).with_context(|| format!("opening {}", apk.display()))?;
    let mut zip =
        zip::ZipArchive::new(file).with_context(|| format!("reading {}", apk.display()))?;
    let names = zip
        .file_names()
        .map(|n| n.map(|n| n.into_owned()))
        .collect::<Result<Vec<String>, _>>()
        .with_context(|| format!("{}: entry names", apk.display()))?;
    let chosen = find_supported_abi(names.iter().map(String::as_str), supported);
    let isa = &supported[chosen.unwrap_or(0)].isa;
    let native_library_dir = format!("{guest_app_dir}/{LIB_DIR_NAME}/{isa}");
    let Some(i) = chosen else {
        return Ok(NativeLibs {
            primary_cpu_abi: None,
            native_library_dir,
        });
    };
    let abi = &supported[i].abi;
    let dir = app_dir.join(LIB_DIR_NAME).join(isa);
    std::fs::create_dir_all(&dir)?;
    for name in &names {
        let Some((entry_abi, base)) = library_entry(name) else {
            continue;
        };
        if entry_abi != abi {
            continue;
        }
        let mut data = Vec::new();
        zip.by_name(name)?
            .read_to_end(&mut data)
            .with_context(|| format!("{}: {name}", apk.display()))?;
        std::fs::write(dir.join(base), data)?;
    }
    Ok(NativeLibs {
        primary_cpu_abi: Some(abi.clone()),
        native_library_dir,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn abis(list: &[(&str, &str)]) -> Vec<SupportedAbi> {
        list.iter()
            .map(|(abi, isa)| SupportedAbi {
                abi: (*abi).into(),
                isa: (*isa).into(),
            })
            .collect()
    }

    #[test]
    fn the_most_preferred_device_abi_wins_whatever_the_entry_order() {
        let s = abis(&[("x86_64", "x86_64"), ("x86", "x86"), ("arm64-v8a", "arm64")]);
        let entries = [
            "classes.dex",
            "lib/arm64-v8a/libg.so",
            "lib/x86/libg.so",
            "lib/mips/libg.so",
        ];
        assert_eq!(find_supported_abi(entries, &s), Some(1));
    }

    #[test]
    fn only_direct_so_entries_count() {
        let s = abis(&[("x86_64", "x86_64")]);
        for e in [
            "lib/x86_64/sub/libg.so",
            "lib/x86_64/readme.txt",
            "lib/x86_64/",
            "assets/lib/x86_64/libg.so",
            "lib//libg.so",
        ] {
            assert_eq!(find_supported_abi([e], &s), None, "{e}");
        }
    }
}
