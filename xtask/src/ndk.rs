// SPDX-License-Identifier: MIT
//! The NDK's clang, as an app's CMake / ndk-build drives it: what the JNI fixtures (xtask-harness) and the QEMU gate's guest programs (qemu.rs) compile with.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use atl_layout::Layout;

use crate::arches;

/// The NDK release the fixtures are built with (owner decision D1, bridge-1 plan); `--ndk` overrides.
pub const NDK_DIR: &str = "android-ndk-r30";

/// The NDK's llvm prebuilt dir for this build machine.
pub fn toolchain(ndk: &Path) -> PathBuf {
    ndk.join("toolchains/llvm/prebuilt/linux-x86_64")
}

/// The default NDK root.
pub fn default_ndk(root: &Path) -> Result<PathBuf> {
    Ok(Layout::of(root)?.workspace().join("tools").join(NDK_DIR))
}

/// NDK clang for one guest arch at one API level, as an app's CMake/ndk-build drives it: `--target=<arches.toml clang triple><api>`.
pub struct NdkTarget {
    bin: PathBuf,
    target: String,
    /// `<triple><api>-clang` in the NDK's bin: the same compiler as `cmd("clang")`, as a wrapper that names its target.
    wrapper: PathBuf,
    /// The arch's `lib/<abi>/` name in an APK: its first arches.toml `android_abis` entry.
    pub abi: String,
    /// The NDK sysroot's per-triple lib dir (libc++_shared.so).
    pub sysroot_lib: PathBuf,
}

impl NdkTarget {
    pub fn new(root: &Path, ndk: &Path, arch: &str, api: u32) -> Result<NdkTarget> {
        let arches = arches::load(root)?;
        let guest = arches
            .get(arch)
            .and_then(|a| a.guest.as_ref())
            .with_context(|| format!("arches.toml has no [{arch}.guest] row"))?;
        let tc = toolchain(ndk);
        Ok(NdkTarget {
            bin: tc.join("bin"),
            target: format!("--target={}{api}", guest.clang),
            wrapper: tc.join("bin").join(format!("{}{api}-clang", guest.clang)),
            abi: guest
                .android_abis
                .first()
                .cloned()
                .with_context(|| format!("arches.toml [{arch}.guest] has no android_abis"))?,
            sysroot_lib: tc.join("sysroot/usr/lib").join(&guest.clang),
        })
    }

    /// `tool` (`clang` / `clang++`) with the target set.
    pub fn cmd(&self, tool: &str) -> Command {
        let mut c = Command::new(self.bin.join(tool));
        c.arg(&self.target);
        c
    }

    /// The per-API clang wrapper (`<triple><api>-clang`): a compiler that needs no `--target` argument, for tools that take a bare compiler path.
    pub fn wrapper(&self) -> &Path {
        &self.wrapper
    }
}
