// SPDX-License-Identifier: MIT
//! Free space of the Windows drive that holds this WSL distro's virtual disk (ext4.vhdx), checked before `native`.
//!
//! WSL cannot see the vhdx's own headroom: the ext4 inside reports the vhdx's virtual size, while the file grows on the host drive and a full host drive fails writes in
//! the distro mid-build. The drive is the one the distro's `Lxss` registry row names (`BasePath`), seen at `/mnt/<letter>`; free space = drvfs statvfs there (9p, no interop;
//! equals Windows' free bytes, measured 2026-11-14).
//! Locating the vhdx needs `reg.exe` interop, which drops under build load -> the BasePath is cached in `<workspace>/cache/host-disk/<distro>.basepath` and trusted only
//! while `<BasePath>/ext4.vhdx` exists through drvfs. A move (`wsl --manage --move`) removes that file -> re-query. Residual: a stale copy left at the old path keeps the old
//! drive measured until the cache file is deleted. Unknown location (no valid cache + interop down) still refuses: never a silent pass.
//! Off WSL (`WSL_DISTRO_NAME` unset, e.g. the Arch laptop) there is no host drive and the check is skipped.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

const LXSS_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Lxss";
const GIB: u64 = 1 << 30;
/// WSL2's disk file name inside BasePath (fixed by WSL; WSL1 distros have none and are unsupported here).
const VHDX: &str = "ext4.vhdx";
/// Where Windows drives are mounted in the guest (WSL's default `automount.root`).
const DRVFS_ROOT: &str = "/mnt";

/// The thresholds, in GiB (native's `--host-free-stop-gib` / `--host-free-compact-gib`).
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub stop_gib: u64,
    pub compact_gib: u64,
}

/// How the BasePath was found, for the guard's log line.
#[derive(Debug, PartialEq)]
enum Found {
    Cached,
    Queried,
}

/// The distro's BasePath: the cache while its vhdx exists under `drvfs`, else `reg.exe` (then cached). Errors only when neither yields an existing vhdx.
fn locate(
    cache: &Path,
    drvfs: &Path,
    distro: &str,
    run: impl FnMut() -> RegRun,
    attempts: u32,
    delay: std::time::Duration,
) -> Result<(String, Found)> {
    if let Ok(text) = std::fs::read_to_string(cache) {
        let base = text.trim();
        if vhdx_of(base, drvfs).is_some_and(|v| v.is_file()) {
            return Ok((base.to_owned(), Found::Cached));
        }
        println!(
            "native: cached BasePath {base:?} ({}) has no {VHDX}: re-querying",
            cache.display()
        );
    }
    let text = query_lxss(run, attempts, delay)?;
    let base = base_path(&text, distro)
        .with_context(|| format!("no Lxss BasePath for distro {distro} (reg.exe succeeded)"))?;
    let vhdx =
        vhdx_of(&base, drvfs).with_context(|| format!("BasePath {base} names no drive letter"))?;
    if !vhdx.is_file() {
        bail!(
            "Lxss BasePath {base} has no {} (WSL1 distro, or drive not mounted at {})",
            vhdx.display(),
            drvfs.display()
        );
    }
    if let Some(dir) = cache.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    std::fs::write(cache, format!("{base}\n"))
        .with_context(|| format!("writing {}", cache.display()))?;
    Ok((base, Found::Queried))
}

/// `C:\WSL\Debian` -> `<drvfs>/c/WSL/Debian/ext4.vhdx`.
fn vhdx_of(base: &str, drvfs: &Path) -> Option<PathBuf> {
    let mount = mount_of(base, drvfs)?;
    let p = base.strip_prefix(r"\\?\").unwrap_or(base);
    let mut v = mount;
    v.extend(p[2..].split('\\').filter(|s| !s.is_empty()));
    Some(v.join(VHDX))
}

/// The host drive's mount, free bytes and how the BasePath was found, or `None` off WSL.
fn host_free(workspace: &Path) -> Result<Option<(PathBuf, u64, String, Found)>> {
    let Ok(distro) = std::env::var("WSL_DISTRO_NAME") else {
        return Ok(None);
    };
    let cache = workspace
        .join("cache")
        .join("host-disk")
        .join(format!("{distro}.basepath"));
    let drvfs = Path::new(DRVFS_ROOT);
    let (base, found) = locate(
        &cache,
        drvfs,
        &distro,
        run_reg,
        REG_ATTEMPTS,
        REG_RETRY_DELAY,
    )?;
    let mount = mount_of(&base, drvfs).context("BasePath lost its drive letter")?;
    let st = rustix::fs::statvfs(&mount).with_context(|| format!("statvfs {}", mount.display()))?;
    Ok(Some((mount, st.f_bavail * st.f_frsize, base, found)))
}

/// Interop under heavy build load fails transiently (same moments as `Wsl/Service/0x8007274c`) and a rerun seconds later works: 3 tries 2 s apart span ~4 s of
/// outage. Runs only without a valid cache (first build, or after a move).
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

/// `C:\WSL\Debian` or `\\?\D:\x` -> `<drvfs>/c`, `<drvfs>/d`.
fn mount_of(base: &str, drvfs: &Path) -> Option<PathBuf> {
    let p = base.strip_prefix(r"\\?\").unwrap_or(base);
    let mut chars = p.chars();
    let letter = chars.next().filter(char::is_ascii_alphabetic)?;
    (chars.next() == Some(':')).then(|| drvfs.join(letter.to_ascii_lowercase().to_string()))
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

/// `native`'s preflight; `workspace` holds the BasePath cache.
pub fn check(workspace: &Path, limits: Limits) -> Result<()> {
    if let Some((mount, free, base, found)) = host_free(workspace)? {
        let m = mount.display().to_string();
        let note = judge(&m, free, limits)?;
        println!(
            "native: host drive {m} has {} GiB free (floor {} GiB; vhdx at {base}, {found:?})",
            free / GIB,
            limits.stop_gib
        );
        if let Some(note) = note {
            println!("{note}");
        }
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
        let mnt = Path::new("/mnt");
        assert_eq!(
            mount_of(&base_path(reg, "docker-desktop").unwrap(), mnt),
            Some(PathBuf::from("/mnt/d"))
        );
        assert_eq!(
            mount_of("C:\\WSL\\Debian", mnt),
            Some(PathBuf::from("/mnt/c"))
        );
        assert_eq!(mount_of("\\\\server\\share", mnt), None);
        assert_eq!(
            vhdx_of("\\\\?\\C:\\WSL\\Debian\\", mnt),
            Some(PathBuf::from("/mnt/c/WSL/Debian/ext4.vhdx"))
        );
    }

    /// A fake drvfs root with `c/WSL/Debian/ext4.vhdx`; returns (root, cache path).
    fn fake_drvfs(tag: &str) -> (PathBuf, PathBuf) {
        let root =
            std::env::temp_dir().join(format!("xtask-host-disk-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let dir = root.join("c").join("WSL").join("Debian");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(VHDX), b"").unwrap();
        (root.clone(), root.join("cache").join("Debian.basepath"))
    }

    const REG: &str = "HKEY_CURRENT_USER\\...\\Lxss\\{c519}\r\n    BasePath    REG_SZ    C:\\WSL\\Debian\r\n    DistributionName    REG_SZ    Debian\r\n";

    /// Break test: interop down. With a valid cache -> located without reg.exe; without one -> refuses (unknown is never treated as enough).
    #[test]
    fn interop_down_uses_cache_or_refuses() {
        let (root, cache) = fake_drvfs("down");
        let mut calls = 0;
        let mut down = || {
            calls += 1;
            fake(false, "")
        };
        let e = locate(
            &cache,
            &root,
            "Debian",
            &mut down,
            2,
            std::time::Duration::ZERO,
        )
        .expect_err("no cache");
        assert!(format!("{e}").contains("host disk floor unchecked"), "{e}");
        assert_eq!(calls, 2);
        // first build with interop up writes the cache
        let got = locate(
            &cache,
            &root,
            "Debian",
            || fake(true, REG),
            1,
            std::time::Duration::ZERO,
        )
        .unwrap();
        assert_eq!(got, ("C:\\WSL\\Debian".to_owned(), Found::Queried));
        let mut calls = 0;
        let got = locate(
            &cache,
            &root,
            "Debian",
            || {
                calls += 1;
                fake(false, "")
            },
            2,
            std::time::Duration::ZERO,
        )
        .unwrap();
        assert_eq!((got.1, calls), (Found::Cached, 0));
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// Distro moved: the cached path's vhdx is gone -> re-query; if interop is then down, refuse rather than measure the old drive.
    #[test]
    fn moved_distro_invalidates_cache() {
        let (root, cache) = fake_drvfs("moved");
        std::fs::create_dir_all(cache.parent().unwrap()).unwrap();
        std::fs::write(&cache, "D:\\Old\\Debian\n").unwrap();
        let got = locate(
            &cache,
            &root,
            "Debian",
            || fake(true, REG),
            1,
            std::time::Duration::ZERO,
        )
        .unwrap();
        assert_eq!(got.1, Found::Queried);
        assert_eq!(
            std::fs::read_to_string(&cache).unwrap().trim(),
            "C:\\WSL\\Debian"
        );
        std::fs::write(&cache, "D:\\Old\\Debian\n").unwrap();
        assert!(
            locate(
                &cache,
                &root,
                "Debian",
                || fake(false, ""),
                1,
                std::time::Duration::ZERO
            )
            .is_err()
        );
        std::fs::remove_dir_all(&root).unwrap();
    }
}
