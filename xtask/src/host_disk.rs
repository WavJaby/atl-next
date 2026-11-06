// SPDX-License-Identifier: MIT
//! Free space of the Windows drive that holds this WSL distro's virtual disk (ext4.vhdx), checked before `native`.
//!
//! WSL cannot see the vhdx's own headroom: the ext4 inside reports the vhdx's virtual size, while the file grows on the host drive and a full host drive fails writes in
//! the distro mid-build. The drive is the one the distro's `Lxss` registry row names (`BasePath`, read through `reg.exe` interop), seen at `/mnt/<letter>`.
//! Off WSL (`WSL_DISTRO_NAME` unset, e.g. the Arch laptop) there is no host drive and the check is skipped.

use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, Result, bail};

const LXSS_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Lxss";
const GIB: u64 = 1 << 30;

/// The thresholds, in GiB (native's `--host-free-stop-gib` / `--host-free-compact-gib`).
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub stop_gib: u64,
    pub compact_gib: u64,
}

/// The host drive's mount and free bytes, or `None` off WSL.
fn host_free() -> Result<Option<(PathBuf, u64)>> {
    let Ok(distro) = std::env::var("WSL_DISTRO_NAME") else {
        return Ok(None);
    };
    let out = Command::new("reg.exe")
        .args(["query", LXSS_KEY, "/s"])
        .output()
        .context("running reg.exe (WSL interop) for the distro's BasePath")?;
    let text = String::from_utf8_lossy(&out.stdout);
    let base = base_path(&text, &distro)
        .with_context(|| format!("no Lxss BasePath for distro {distro}"))?;
    let mount =
        mount_of(&base).with_context(|| format!("BasePath {base} names no drive letter"))?;
    let st = rustix::fs::statvfs(&mount).with_context(|| format!("statvfs {}", mount.display()))?;
    Ok(Some((mount, st.f_bavail * st.f_frsize)))
}

/// The `BasePath` of the registry block whose `DistributionName` is `distro` (`reg query /s` output: one block per distro, `name  REG_SZ  value` lines).
fn base_path(reg: &str, distro: &str) -> Option<String> {
    let value = |line: &str, name: &str| -> Option<String> {
        let rest = line.trim().strip_prefix(name)?.trim_start();
        Some(rest.strip_prefix("REG_SZ")?.trim().to_owned())
    };
    let mut name = None;
    let mut base = None;
    for line in reg.lines().map(|l| l.trim_end_matches('\r')) {
        if line.starts_with("HKEY_") {
            name = None;
            base = None;
        }
        name = value(line, "DistributionName").or(name);
        base = value(line, "BasePath").or(base);
        if let (Some(n), Some(b)) = (&name, &base)
            && n == distro
        {
            return Some(b.clone());
        }
    }
    None
}

/// `C:\WSL\Debian` or `\\?\D:\x` -> `/mnt/c`, `/mnt/d`.
fn mount_of(base: &str) -> Option<PathBuf> {
    let p = base.strip_prefix(r"\\?\").unwrap_or(base);
    let mut chars = p.chars();
    let letter = chars.next().filter(char::is_ascii_alphabetic)?;
    (chars.next() == Some(':'))
        .then(|| PathBuf::from(format!("/mnt/{}", letter.to_ascii_lowercase())))
}

/// Refuses below `stop_gib`; a note below `compact_gib`.
fn judge(mount: &str, free: u64, limits: Limits) -> Result<Option<String>> {
    let gib = free / GIB;
    if free < limits.stop_gib * GIB {
        bail!(
            "host drive {mount} (holds the WSL vhdx) has {gib} GiB free, under {} GiB: free space or compact the vhdx before `native` (a full host drive fails the build's writes)",
            limits.stop_gib
        );
    }
    Ok((free < limits.compact_gib * GIB).then(|| {
        format!("native: host drive {mount} (holds the WSL vhdx) has {gib} GiB free, under {} GiB: compact the vhdx soon", limits.compact_gib)
    }))
}

/// `native`'s preflight.
pub fn check(limits: Limits) -> Result<()> {
    if let Some((mount, free)) = host_free()?
        && let Some(note) = judge(&mount.display().to_string(), free, limits)?
    {
        println!("{note}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIMITS: Limits = Limits {
        stop_gib: 6,
        compact_gib: 30,
    };

    /// Injected free space: under the stop refuses, between stop and compact notes, above passes quietly.
    #[test]
    fn thresholds() {
        let e = judge("/mnt/c", 5 * GIB, LIMITS).expect_err("under the stop");
        assert!(format!("{e}").contains("5 GiB free, under 6 GiB"), "{e}");
        assert!(
            judge("/mnt/c", 6 * GIB, LIMITS)
                .unwrap()
                .is_some_and(|n| n.contains("compact"))
        );
        assert!(judge("/mnt/c", 30 * GIB, LIMITS).unwrap().is_none());
    }

    /// The block of the named distro, not the first; the `\\?\` prefix and the drive letter.
    #[test]
    fn reads_the_registry() {
        let reg = "HKEY_CURRENT_USER\\Software\\Microsoft\\Windows\\CurrentVersion\\Lxss\r\n\
            HKEY_CURRENT_USER\\...\\Lxss\\{4ec2}\r\n    DistributionName    REG_SZ    docker-desktop\r\n    BasePath    REG_SZ    \\\\?\\D:\\Docker\\main\r\n\
            HKEY_CURRENT_USER\\...\\Lxss\\{c519}\r\n    BasePath    REG_SZ    C:\\WSL\\Debian\r\n    DistributionName    REG_SZ    Debian\r\n";
        assert_eq!(base_path(reg, "Debian").as_deref(), Some("C:\\WSL\\Debian"));
        assert_eq!(base_path(reg, "Ubuntu"), None);
        assert_eq!(
            mount_of(&base_path(reg, "docker-desktop").unwrap()),
            Some(PathBuf::from("/mnt/d"))
        );
        assert_eq!(mount_of("C:\\WSL\\Debian"), Some(PathBuf::from("/mnt/c")));
        assert_eq!(mount_of("\\\\server\\share"), None);
    }
}
