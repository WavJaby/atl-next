// SPDX-License-Identifier: MIT
//! Free-space floors of every disk a build writes, checked before `native`: a disk that fills mid-build fails writes far from the cause (ENOSPC in a link step, a torn sccache entry, a vhdx that stops growing).
//! Disks: the host drive holding the WSL vhdx (`host_disk`, WSL only), the filesystem holding the workspace (src/, cache/ with sccache, out/ logs) and the one holding the build-dir pool (out.rs).
//! Two of them on one filesystem -> one disk whose floor is the sum: both writers land there. Floors are `[disk]` keys of workspace.toml (ws_config, each default carries its derivation); under one refuses, naming every short disk.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::ws_config::WsConfig;

const GIB: u64 = 1 << 30;

/// One disk as judged: what it holds, free bytes, the floor in GiB.
#[derive(Debug, PartialEq, Eq)]
struct Disk {
    holds: String,
    free: u64,
    floor_gib: u64,
}

/// A filesystem path, its device and its free bytes (statvfs `f_bavail`: what an unprivileged build may write).
fn measure(path: &Path) -> Result<(u64, u64)> {
    let st = rustix::fs::stat(path).with_context(|| format!("stat {}", path.display()))?;
    let vfs = rustix::fs::statvfs(path).with_context(|| format!("statvfs {}", path.display()))?;
    Ok((st.st_dev, vfs.f_bavail * vfs.f_frsize))
}

/// `(device, free, holds, floor)` rows merged per device: holds joined, floors summed (both writers land on that filesystem).
fn merge(rows: Vec<(u64, u64, String, u64)>) -> Vec<Disk> {
    let mut out: Vec<(u64, Disk)> = Vec::new();
    for (dev, free, holds, floor_gib) in rows {
        match out.iter_mut().find(|(d, _)| *d == dev) {
            Some((_, disk)) => {
                disk.holds = format!("{} + {holds}", disk.holds);
                disk.floor_gib += floor_gib;
            }
            None => out.push((
                dev,
                Disk {
                    holds,
                    free,
                    floor_gib,
                },
            )),
        }
    }
    out.into_iter().map(|(_, d)| d).collect()
}

/// One line per disk; Err naming every disk under its floor.
fn judge(disks: &[Disk]) -> Result<Vec<String>> {
    let mut lines = Vec::new();
    let mut short = Vec::new();
    for d in disks {
        let line = format!(
            "{}: {} GiB free (floor {} GiB)",
            d.holds,
            d.free / GIB,
            d.floor_gib
        );
        if d.free < d.floor_gib * GIB {
            short.push(line.clone());
        }
        lines.push(line);
    }
    if !short.is_empty() {
        bail!(
            "disk floor: {}: free space there before `native` (floors: workspace.toml [disk])",
            short.join("; ")
        );
    }
    Ok(lines)
}

/// `native`'s preflight for workspace `ws` with `cfg`'s floors: prints one line per disk, refuses when one is short.
pub fn check(ws: &Path, cfg: &WsConfig) -> Result<()> {
    let mut rows = Vec::new();
    if let Some(h) = crate::host_disk::host_free(ws)? {
        // its own filesystem (drvfs); no guest path shares the device id space, so it gets a device of its own
        rows.push((
            u64::MAX,
            h.free,
            format!(
                "host drive {} (WSL vhdx at {}, {:?})",
                h.mount.display(),
                h.base,
                h.found
            ),
            cfg.disk.host_min_free.value,
        ));
        if h.free < cfg.disk.host_compact_below.value * GIB {
            println!(
                "native: host drive {} has {} GiB free, under {} GiB: compact the vhdx soon",
                h.mount.display(),
                h.free / GIB,
                cfg.disk.host_compact_below.value
            );
        }
    }
    let pool: PathBuf = crate::out::pool(ws);
    for (path, holds, floor) in [
        (
            ws.to_path_buf(),
            "workspace",
            cfg.disk.workspace_min_free.value,
        ),
        (pool, "build-dir pool", cfg.disk.pool_min_free.value),
    ] {
        let (dev, free) = measure(&path)?;
        rows.push((dev, free, format!("{holds} {}", path.display()), floor));
    }
    for line in judge(&merge(rows))? {
        println!("native: disk {line}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Same device: one disk, floors summed, holds joined; another device stays apart.
    #[test]
    fn one_filesystem_sums_its_floors() {
        let disks = merge(vec![
            (1, 10 * GIB, "workspace /w".into(), 6),
            (1, 10 * GIB, "build-dir pool /w/out/pool".into(), 5),
            (2, 3 * GIB, "host drive /mnt/c".into(), 6),
        ]);
        assert_eq!(
            disks,
            [
                Disk {
                    holds: "workspace /w + build-dir pool /w/out/pool".into(),
                    free: 10 * GIB,
                    floor_gib: 11
                },
                Disk {
                    holds: "host drive /mnt/c".into(),
                    free: 3 * GIB,
                    floor_gib: 6
                },
            ]
        );
    }

    /// Under the floor refuses naming every short disk; at the floor passes.
    #[test]
    fn short_disks_are_named() {
        let at = |free, floor_gib| Disk {
            holds: format!("d{floor_gib}"),
            free,
            floor_gib,
        };
        let e = judge(&[at(4 * GIB, 5), at(6 * GIB, 6), at(GIB, 2)]).expect_err("two short");
        let e = format!("{e}");
        assert!(
            e.contains("d5: 4 GiB free (floor 5 GiB)") && e.contains("d2: 1 GiB"),
            "{e}"
        );
        assert!(!e.contains("d6"), "{e}");
        assert_eq!(judge(&[at(5 * GIB, 5)]).unwrap().len(), 1);
    }

    /// The real filesystem answers: a probe that measured nothing would report 0 free.
    #[test]
    fn measure_reads_the_temp_dir() {
        let (_, free) = measure(&std::env::temp_dir()).unwrap();
        assert!(free > 0);
    }
}
