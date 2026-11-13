// SPDX-License-Identifier: MIT
//! The native-library facts PackageManagerService records around an install: the device's ABI list and the instruction set an app's `primaryCpuAbi` selects. The copy itself is AOSP's
//! NativeLibraryHelper, run by the installer process (atl-app-launcher installer.rs).

use serde::{Deserialize, Serialize};

/// `NativeLibraryHelper.LIB_DIR_NAME`: the APK's library directory, and the one below the install directory.
pub const LIB_DIR_NAME: &str = "lib";

/// One device-supported ABI and its instruction set (`VMRuntime.getInstructionSet(abi)`: the arches.toml row whose `android_abis` lists it).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SupportedAbi {
    pub abi: String,
    pub isa: String,
}

/// The app's instruction set from its `primaryCpuAbi` (`VMRuntime.getInstructionSet`, as ProcessList passes `--instruction-set` to zygote): the row of `supported` naming that ABI; none (no native code) or
/// an ABI not in `supported` -> `supported[0]`'s, the preferred one (`InstructionSets.getPrimaryInstructionSet`). `supported` is non-empty.
pub fn instruction_set<'a>(
    primary_cpu_abi: Option<&str>,
    supported: &'a [SupportedAbi],
) -> &'a str {
    primary_cpu_abi
        .and_then(|abi| supported.iter().find(|s| s.abi == abi))
        .unwrap_or(&supported[0])
        .isa
        .as_str()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instruction_set_follows_the_primary_abi_else_the_preferred() {
        let s = [("x86_64", "x86_64"), ("arm64-v8a", "arm64")].map(|(abi, isa)| SupportedAbi {
            abi: abi.into(),
            isa: isa.into(),
        });
        assert_eq!(instruction_set(Some("arm64-v8a"), &s), "arm64");
        assert_eq!(instruction_set(Some("x86_64"), &s), "x86_64");
        // no native code, or an ABI the device does not list: the preferred row's
        assert_eq!(instruction_set(None, &s), "x86_64");
        assert_eq!(instruction_set(Some("mips"), &s), "x86_64");
    }
}
