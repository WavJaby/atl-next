// SPDX-License-Identifier: MIT
//! Free space of the Windows drive that holds this WSL distro's virtual disk (ext4.vhdx), checked before `native`.
//!
//! WSL cannot see the vhdx's own headroom: the ext4 inside reports the vhdx's virtual size, while the file grows on the host drive and a full host drive fails writes in
//! the distro mid-build. The drive is the one the distro's `Lxss` registry row names (`BasePath`, read through `reg.exe` interop, retried on transient interop failure), seen at `/mnt/<letter>`.
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
    let text = query_lxss(run_reg, REG_ATTEMPTS, REG_RETRY_DELAY)?;
    let base = base_path(&text, &distro)
        .with_context(|| format!("no Lxss BasePath for distro {distro} (reg.exe succeeded)"))?;
    let mount =
        mount_of(&base).with_context(|| format!("BasePath {base} names no drive letter"))?;
    let st = rustix::fs::statvfs(&mount).with_context(|| format!("statvfs {}", mount.display()))?;
    Ok(Some((mount, st.f_bavail * st.f_frsize)))
}

/// Interop under heavy build load fails transiently (same moments as `Wsl/Service/0x8007274c`) and a rerun seconds later works: 3 tries 2 s apart span ~4 s of
/// outage. Not cached: the distro can be moved (`wsl --manage --move`) and nothing inside the guest names the vhdx's drive (`/proc/mounts` shows only `/dev/sdX`).
const REG_ATTEMPTS: u32 = 3;
const REG_RETRY_DELAY: std::time::Duration = std::time::Duration::from_secs(2);

/// One `reg.exe` run: exit code (`None` = killed by signal / spawn failed), stdout, stderr.
struct RegRun {
    ok: bool,
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

fn run_reg() -> RegRun {
    match Command::new("reg.exe")
        .args(["query", LXSS_KEY, "/s"])
        .output()
    {
        Ok(o) => RegRun {
            ok: o.status.success(),
            code: o.status.code(),
            stdout: String::from_utf8_lossy(&o.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&o.stderr).into_owned(),
        },
        Err(e) => RegRun {
            ok: false,
            code: None,
            stdout: String::new(),
            stderr: format!("spawn: {e}"),
        },
    }
}

/// `reg query` stdout, retried while interop fails (non-zero exit or empty stdout); the error names the last exit code and stderr, never "no BasePath".
fn query_lxss(
    mut run: impl FnMut() -> RegRun,
    attempts: u32,
    delay: std::time::Duration,
) -> Result<String> {
    let mut last = None;
    for i in 1..=attempts {
        let r = run();
        if r.ok && !r.stdout.trim().is_empty() {
            if i > 1 {
                println!("native: reg.exe (WSL interop) succeeded on attempt {i}/{attempts}");
            }
            return Ok(r.stdout);
        }
        println!(
            "native: reg.exe (WSL interop) failed, attempt {i}/{attempts}: exit {:?}, stderr {:?}",
            r.code,
            r.stderr.trim()
        );
        last = Some(r);
        if i < attempts {
            std::thread::sleep(delay);
        }
    }
    let r = last.expect("attempts >= 1");
    bail!(
        "WSL interop failed: reg.exe query {LXSS_KEY} gave exit {:?}, stdout {} bytes, stderr {:?} after {attempts} attempts; host disk floor unchecked, refusing `native`",
        r.code,
        r.stdout.len(),
        r.stderr.trim()
    )
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

    fn fake(ok: bool, stdout: &str) -> RegRun {
        RegRun {
            ok,
            code: Some(if ok { 0 } else { 1 }),
            stdout: stdout.into(),
            stderr: if ok {
                String::new()
            } else {
                "Wsl/Service/0x8007274c".into()
            },
        }
    }

    /// Transient failures (bad exit, empty stdout) are retried; success on the last attempt returns its stdout.
    #[test]
    fn retries_transient_interop() {
        let mut seq = vec![
            fake(false, ""),
            fake(true, "  \r\n"),
            fake(true, "BasePath"),
        ]
        .into_iter();
        let mut calls = 0;
        let out = query_lxss(
            || {
                calls += 1;
                seq.next().unwrap()
            },
            3,
            std::time::Duration::ZERO,
        )
        .unwrap();
        assert_eq!((out.as_str(), calls), ("BasePath", 3));
    }

    /// Break test: interop never works -> bounded tries, error names the interop failure + stderr, not "no BasePath".
    #[test]
    fn persistent_interop_failure_refuses() {
        let mut calls = 0;
        let e = query_lxss(
            || {
                calls += 1;
                fake(false, "")
            },
            3,
            std::time::Duration::ZERO,
        )
        .expect_err("must refuse");
        let msg = format!("{e}");
        assert_eq!(calls, 3);
        assert!(
            msg.contains("WSL interop failed")
                && msg.contains("0x8007274c")
                && msg.contains("exit Some(1)"),
            "{msg}"
        );
        assert!(!msg.contains("BasePath for distro"), "{msg}");
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
