// SPDX-License-Identifier: MIT
//! Free-space floors of every disk a build writes, checked before `native`: a disk that fills mid-build fails writes far from the cause (ENOSPC in a link step, a torn sccache entry, a vhdx that stops growing).
//! Disks: the host drive holding the WSL vhdx (`host_disk`, WSL only), the filesystem holding the workspace (src/, cache/ with sccache, out/ logs) and the one holding the build-dir pool (out.rs).
//! Two of them on one filesystem -> one disk whose floor is the sum: both writers land there. Floors are `[disk]` keys of workspace.toml (ws_config, each default carries its derivation); under one refuses, naming every short disk.
//! The build-dir pool also has a metadata floor on btrfs (`btrfs_space`): statfs cannot see metadata, which filled while statfs still showed 6-7 GB free.
//! And when the pool's filesystem lives in a growing file (`[disk] pool_backing`, a dynamic vhdx), the host drive under that file must hold the file's worst-case growth (`backing_row`): 2026-11-20 E: hit 0 B under the pool vhdx while the guest's statfs still showed GBs free, and btrfs went read-only.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::btrfs_space::{self, Headroom};
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

/// One line per disk and metadata row; Err naming every short one. A metadata row is `(line, short)`.
fn judge(disks: &[Disk], metadata: &[(String, bool)]) -> Result<Vec<String>> {
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
    for (line, is_short) in metadata {
        if *is_short {
            short.push(line.clone());
        }
        lines.push(line.clone());
    }
    if !short.is_empty() {
        bail!(
            "disk floor: {}: short before `native` (floors: workspace.toml [disk])",
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
    let pool_rows = pool_rows(ws, cfg)?;
    for line in judge(&merge(rows), &pool_rows)? {
        println!("native: disk {line}");
    }
    Ok(())
}

/// The build-dir pool's extra rows `(line, short)` statfs cannot judge: btrfs metadata (none off btrfs), then the host drive under its backing file (`backing_row`).
pub fn pool_rows(ws: &Path, cfg: &WsConfig) -> Result<Vec<(String, bool)>> {
    let pool = crate::out::pool(ws);
    let probed =
        btrfs_space::probe(&pool).with_context(|| format!("metadata of {}", pool.display()))?;
    let mut rows = Vec::new();
    if let Some(h) = probed {
        rows.push(metadata_row(
            &pool,
            &h,
            cfg.disk.pool_metadata_min_free.value,
        ));
    }
    rows.push(match &cfg.disk.pool_backing.value {
        None => (
            format!(
                "build-dir pool {}: no [disk] pool_backing in workspace.toml, host growth check skipped",
                pool.display()
            ),
            false,
        ),
        Some(file) => {
            let fs = rustix::fs::statvfs(&pool)
                .with_context(|| format!("statvfs {}", pool.display()))?;
            let len = std::fs::metadata(file)
                .with_context(|| format!("[disk] pool_backing {}", file.display()))?
                .len();
            let dir = file.parent().context("pool_backing has no parent")?;
            let host = rustix::fs::statvfs(dir)
                .with_context(|| format!("statvfs {}", dir.display()))?;
            backing_row(&Backing {
                file,
                // the filesystem's own size, as btrfs_space reads it (block devices may be larger after a vhdx resize)
                fs_size: fs.f_blocks.saturating_mul(fs.f_bsize),
                backed: len,
                host_free: host.f_bavail * host.f_frsize,
                margin_gib: cfg.disk.pool_backing_margin.value,
            })
        }
    });
    Ok(rows)
}

/// The pool's filesystem lives in a growing file (`[disk] pool_backing`, a dynamic vhdx) on a host drive the guest's statfs cannot see.
struct Backing<'a> {
    file: &'a Path,
    /// size of the filesystem inside the file
    fs_size: u64,
    /// file length = bytes the file backs now
    backed: u64,
    /// free bytes of the host drive holding the file (drvfs statvfs)
    host_free: u64,
    margin_gib: u64,
}

/// Host-drive row: short when `host_free < (fs_size - backed) + margin`.
/// Why that bound and not btrfs unallocated: the file grows on the first write to any unbacked block, and btrfs writes unbacked blocks inside allocated chunks too (space trimmed then compacted away: 2026-11-19 allocated 49 GiB, file 44.3 GiB).
/// So only `fs_size - backed` bounds the growth; with it every admitted build can write until the pool is full without the host drive running out (incident 2026-11-20: E: hit 0 B, btrfs went read-only).
fn backing_row(b: &Backing) -> (String, bool) {
    let growth = b.fs_size.saturating_sub(b.backed);
    let need = growth.saturating_add(b.margin_gib.saturating_mul(GIB));
    let short = b.host_free < need;
    let mut line = format!(
        "build-dir pool backing {}: host drive {:.1} GiB free, file may still grow {:.1} GiB (filesystem {:.1} GiB - backed {:.1} GiB) + margin {} GiB",
        b.file.display(),
        gib(b.host_free),
        gib(growth),
        gib(b.fs_size),
        gib(b.backed),
        b.margin_gib,
    );
    if short {
        line.push_str(&format!(
            "; short {:.1} GiB: `cargo xtask out prune --execute`, then with no build running `btrfs balance start -dusage=70 -musage=70 <pool>` (root) + compact the file (scripts/env/wsl-tool.ps1 compact, then start), or free the host drive",
            gib(need - b.host_free)
        ));
    }
    (line, short)
}

/// Metadata row for the pool at `path`: headroom, its parts, and whether it is under `floor_gib`.
fn metadata_row(path: &Path, h: &Headroom, floor_gib: u64) -> (String, bool) {
    let line = format!(
        "build-dir pool {} metadata: headroom {:.2} GiB (floor {floor_gib} GiB) = {:.2} GiB free in its chunks + {:.2} GiB from {:.2} GiB unallocated at profile {:.1}; in flight (may_use) {:.2} GiB, not counted",
        path.display(),
        gib(h.bytes()),
        gib(h.free_in_chunks),
        gib(h.from_unallocated),
        gib(h.unallocated),
        h.profile,
        gib(h.in_flight),
    );
    (line, h.bytes() < floor_gib.saturating_mul(GIB))
}

/// Refuses a copy into the build-dir pool of `ws` while a `pool_rows` row is short: metadata headroom (a base copy writes ~0.14 GiB of metadata, ws_config `pool_metadata_min_free`, which statfs does not show) or the host drive under the backing file.
pub fn pool_gate(ws: &Path, cfg: &WsConfig) -> Result<()> {
    if let Some((line, _)) = pool_rows(ws, cfg)?.iter().find(|(_, short)| *short) {
        bail!("out: refusing a reflink copy into the pool: {line}");
    }
    Ok(())
}

fn gib(bytes: u64) -> f64 {
    bytes as f64 / GIB as f64
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
        let e = judge(&[at(4 * GIB, 5), at(6 * GIB, 6), at(GIB, 2)], &[]).expect_err("two short");
        let e = format!("{e}");
        assert!(
            e.contains("d5: 4 GiB free (floor 5 GiB)") && e.contains("d2: 1 GiB"),
            "{e}"
        );
        assert!(!e.contains("d6"), "{e}");
        assert_eq!(judge(&[at(5 * GIB, 5)], &[]).unwrap().len(), 1);
    }

    /// Live pool 2026-11-16: 0.31 GiB free in metadata chunks (3.76 GiB total, 3.45 GiB used or reserved), 18 GiB unallocated at DUP (x0.5): headroom 9.31 GiB.
    /// Full chunks and no unallocated space is the incident's shape: short.
    #[test]
    fn metadata_row_names_its_parts_and_trips_under_the_floor() {
        let live = Headroom {
            free_in_chunks: 328_843_264,
            in_flight: GIB / 2,
            unallocated: 19_328_401_408,
            from_unallocated: 9_663_676_416,
            profile: 2.0,
        };
        let (line, short) = metadata_row(Path::new("/p"), &live, 1);
        assert!(!short, "{line}");
        assert!(
            line.contains("headroom 9.31 GiB (floor 1 GiB) = 0.31 GiB free in its chunks + 9.00 GiB from 18.00 GiB unallocated at profile 2.0; in flight (may_use) 0.50 GiB, not counted"),
            "{line}"
        );
        let full = Headroom {
            free_in_chunks: 0,
            in_flight: 0,
            unallocated: 0,
            from_unallocated: 0,
            profile: 2.0,
        };
        assert!(metadata_row(Path::new("/p"), &full, 1).1);
        let at_floor = Headroom {
            free_in_chunks: 0,
            in_flight: 0,
            unallocated: 2 * GIB,
            from_unallocated: GIB,
            profile: 2.0,
        };
        assert!(!metadata_row(Path::new("/p"), &at_floor, 1).1);
    }

    /// A short metadata row is refused by name, next to the statfs disks.
    #[test]
    fn a_short_metadata_row_is_refused_by_name() {
        let row = (
            "build-dir pool /p metadata: headroom 0.02 GiB".to_owned(),
            true,
        );
        let e = format!("{}", judge(&[], &[row]).expect_err("short"));
        assert!(e.contains("headroom 0.02 GiB"), "{e}");
    }

    /// Pool filesystem 90 GiB (df). Numbers from the 2026-11-19/20 pool (memory M7): after balance + compact the file was 47.6 GB with E: 49.6 GB free; at the incident it was 87.8 GB with E: 0 B.
    #[test]
    fn backing_growth_must_fit_the_host_drive() {
        const GB: u64 = 1_000_000_000;
        let at = |backed, host_free, margin_gib| {
            backing_row(&Backing {
                file: Path::new("/mnt/e/WSL/atl-pool.vhdx"),
                fs_size: 90 * GIB,
                backed,
                host_free,
                margin_gib,
            })
        };
        // growth 90 GiB - 47.6 GB = 49.04 GB; 49.6 GB free leaves 0.56 GB: admitted without margin, refused with the 2 GiB default (a full pool would leave E: 0.56 GB)
        let (line, short) = at(476 * GB / 10, 496 * GB / 10, 0);
        assert!(!short, "{line}");
        let (line, short) = at(476 * GB / 10, 496 * GB / 10, 2);
        assert!(short, "{line}");
        // after the 2026-11-20 recovery: file 51.17 GB, E: ~52.6 GB free -> 45.5 GB growth + 2 GiB fits
        assert!(!at(51_174_703_104, 526 * GB / 10, 2).1);
        // incident: file 87.8 GB, E: 0 B
        let (line, short) = at(878 * GB / 10, 0, 2);
        assert!(short, "{line}");
        assert!(
            line.contains("out prune --execute") && line.contains("compact"),
            "{line}"
        );
        // margin edge: free == growth + margin admits, one byte less refuses
        let growth = 90 * GIB - 50 * GIB;
        assert!(!at(50 * GIB, growth + 2 * GIB, 2).1);
        assert!(at(50 * GIB, growth + 2 * GIB - 1, 2).1);
        // a file past the filesystem size (vhdx metadata) can grow by nothing more: margin only
        assert!(!at(91 * GIB, 2 * GIB, 2).1);
    }

    /// No `[disk] pool_backing`: one non-short row naming the skip (other machines); touches only a temp dir.
    #[test]
    fn no_backing_skips_with_one_row() {
        let tmp = std::env::temp_dir().join(format!("xtask-backing-skip-{}", std::process::id()));
        std::fs::create_dir_all(crate::out::pool(&tmp)).unwrap();
        let cfg = crate::ws_config::parse("", &tmp.join("workspace.toml")).unwrap();
        let rows = pool_rows(&tmp, &cfg);
        std::fs::remove_dir_all(&tmp).unwrap();
        let skipped: Vec<_> = rows
            .unwrap()
            .into_iter()
            .filter(|(l, _)| l.contains("host growth check skipped"))
            .collect();
        assert_eq!(skipped.len(), 1);
        assert!(!skipped[0].1);
    }

    /// The real filesystem answers: a probe that measured nothing would report 0 free.
    #[test]
    fn measure_reads_the_temp_dir() {
        let (_, free) = measure(&std::env::temp_dir()).unwrap();
        assert!(free > 0);
    }
}
