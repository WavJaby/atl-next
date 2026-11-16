// SPDX-License-Identifier: MIT
//! btrfs metadata headroom: what a metadata write can still take, from btrfs's own counters.
//! statfs counts data space only. 2026-11-16: the build pool showed 6-7 GB free while metadata was full and unallocated was 0, so every build failed (ENOSPC, then read-only).
//! Headroom = metadata free inside its allocated chunks + unallocated raw bytes at the metadata profile's ratio (a new metadata chunk is cut from unallocated space: DUP takes 2 raw bytes per logical byte).
//! Data chunks are cut from the same unallocated space, so the headroom is an upper bound.
//! Source, read unprivileged (live 2026-11-16): `/sys/fs/btrfs/<uuid>/allocation/<type>/{total_bytes,bytes_used,bytes_reserved,bytes_pinned,bytes_readonly,bytes_may_use,disk_total}`; the filesystem's size from statfs `f_blocks` x `f_bsize`, not the block devices' sizes (a vhdx grown without `btrfs filesystem resize` makes them larger).
//! Mapping: statfs `f_type` says btrfs; `/proc/self/mountinfo` names the mount's device; the `devices/<dev>` entry names the filesystem.
//! Not btrfs: no headroom, the statfs floors stay the only check.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};

/// statfs `f_type` of a btrfs filesystem (linux/magic.h).
const BTRFS_SUPER_MAGIC: u64 = 0x9123_683E;
const SYSFS_BTRFS: &str = "/sys/fs/btrfs";
const MOUNTINFO: &str = "/proc/self/mountinfo";

/// One allocation type's counters, bytes: `total` is logical space in its block groups, `disk_total` the raw bytes they take.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Space {
    total: u64,
    used: u64,
    reserved: u64,
    pinned: u64,
    readonly: u64,
    may_use: u64,
    disk_total: u64,
}

impl Space {
    /// Logical bytes a new write can still take in the allocated chunks: `total` less btrfs's used sum (used + reserved + pinned + readonly + may_use).
    fn free_in_chunks(&self) -> u64 {
        let taken = self
            .used
            .saturating_add(self.reserved)
            .saturating_add(self.pinned)
            .saturating_add(self.readonly)
            .saturating_add(self.may_use);
        self.total.saturating_sub(taken)
    }
}

/// What metadata can still take: `bytes()` = `free_in_chunks` + `from_unallocated`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Headroom {
    /// Logical metadata bytes free inside the allocated metadata chunks.
    pub free_in_chunks: u64,
    /// Raw bytes of the filesystem in no chunk, all profiles.
    pub unallocated: u64,
    /// Logical metadata bytes `unallocated` becomes at the metadata profile's ratio (DUP: half).
    pub from_unallocated: u64,
    /// Raw bytes per logical byte of metadata (DUP 2.0, single 1.0).
    pub profile: f64,
}

impl Headroom {
    /// Logical metadata bytes a write can still take.
    pub fn bytes(&self) -> u64 {
        self.free_in_chunks.saturating_add(self.from_unallocated)
    }
}

/// Headroom of the btrfs that holds `path` (its nearest existing ancestor is measured). `None`: not btrfs.
/// Err: btrfs whose counters cannot be read or mapped, or whose data is replicated; a floor must not pass on that.
pub fn probe(path: &Path) -> Result<Option<Headroom>> {
    let existing = path
        .ancestors()
        .find(|p| p.exists())
        .with_context(|| format!("no existing ancestor of {}", path.display()))?;
    let stat =
        rustix::fs::statfs(existing).with_context(|| format!("statfs {}", existing.display()))?;
    if u64::try_from(stat.f_type).ok() != Some(BTRFS_SUPER_MAGIC) {
        return Ok(None);
    }
    let real = std::fs::canonicalize(existing)
        .with_context(|| format!("canonicalizing {}", existing.display()))?;
    let mounts =
        std::fs::read_to_string(MOUNTINFO).with_context(|| format!("reading {MOUNTINFO}"))?;
    let source = mount_source(&mounts, &real)?;
    let device = std::fs::canonicalize(&source)
        .with_context(|| format!("canonicalizing device {source}"))?;
    let name = device
        .file_name()
        .and_then(|n| n.to_str())
        .with_context(|| format!("device {} has no name", device.display()))?;
    // the filesystem's own size: statfs counts `f_blocks` blocks of `f_bsize` bytes
    let block =
        u64::try_from(stat.f_bsize).with_context(|| format!("statfs f_bsize {}", stat.f_bsize))?;
    let fs_bytes = stat.f_blocks.saturating_mul(block);
    headroom_of(&fs_dir_of(Path::new(SYSFS_BTRFS), name)?, fs_bytes).map(Some)
}

/// The source device of the mount that holds `path` (the longest mount point that prefixes it); that mount must be btrfs.
fn mount_source(mountinfo: &str, path: &Path) -> Result<String> {
    // fields: id parent maj:min root mountpoint opts [optional...] - fstype source superopts
    let mut held = Vec::new();
    for line in mountinfo.lines() {
        let Some((head, tail)) = line.split_once(" - ") else {
            continue;
        };
        let Some(mount) = head.split_whitespace().nth(4) else {
            continue;
        };
        let mount = PathBuf::from(unescape(mount));
        let mut tail = tail.split_whitespace();
        let (Some(fstype), Some(source)) = (tail.next(), tail.next()) else {
            continue;
        };
        if path.starts_with(&mount) {
            held.push((mount, fstype.to_owned(), unescape(source)));
        }
    }
    let (mount, fstype, source) = held
        .into_iter()
        .max_by_key(|(mount, _, _)| mount.as_os_str().len())
        .with_context(|| format!("no mount in {MOUNTINFO} holds {}", path.display()))?;
    if fstype != "btrfs" {
        bail!(
            "{} is btrfs (statfs) but its mount {} is {fstype}",
            path.display(),
            mount.display()
        );
    }
    Ok(source)
}

/// mountinfo octal-escapes space, tab, newline and backslash: `\040`, `\011`, `\012`, `\134`.
fn unescape(field: &str) -> String {
    field
        .replace(r"\040", " ")
        .replace(r"\011", "\t")
        .replace(r"\012", "\n")
        .replace(r"\134", r"\")
}

/// The `<uuid>` dir under `root` whose `devices/` holds `device`, its kernel name (`sde`).
fn fs_dir_of(root: &Path, device: &str) -> Result<PathBuf> {
    for entry in std::fs::read_dir(root).with_context(|| format!("reading {}", root.display()))? {
        let dir = entry?.path();
        if dir.join("devices").join(device).symlink_metadata().is_ok() {
            return Ok(dir);
        }
    }
    bail!(
        "no btrfs filesystem under {} has device {device}",
        root.display()
    )
}

/// Headroom of the filesystem at `fs_dir` (its `<uuid>` dir in sysfs), whose size is `fs_bytes` (statfs).
fn headroom_of(fs_dir: &Path, fs_bytes: u64) -> Result<Headroom> {
    let data = space_of(fs_dir, "data")?;
    // statfs reports logical bytes: the raw size only with one copy per byte. Replicated data is refused, not converted.
    ensure!(
        data.disk_total == data.total,
        "data is replicated ({} raw bytes for {} logical): statfs does not give the raw size",
        data.disk_total,
        data.total
    );
    let meta = space_of(fs_dir, "metadata")?;
    let system = space_of(fs_dir, "system")?;
    let allocated = data
        .disk_total
        .saturating_add(meta.disk_total)
        .saturating_add(system.disk_total);
    headroom(&meta, fs_bytes.saturating_sub(allocated))
}

/// Metadata headroom from its counters and the unallocated raw bytes. The ratio is `total / disk_total`, so no profile name is read.
fn headroom(meta: &Space, unallocated: u64) -> Result<Headroom> {
    if meta.total == 0 || meta.disk_total == 0 {
        bail!(
            "metadata has no chunks (total {}, disk_total {})",
            meta.total,
            meta.disk_total
        );
    }
    let logical = u128::from(unallocated) * u128::from(meta.total) / u128::from(meta.disk_total);
    Ok(Headroom {
        free_in_chunks: meta.free_in_chunks(),
        unallocated,
        from_unallocated: u64::try_from(logical)
            .expect("total <= disk_total, so logical <= unallocated"),
        profile: meta.disk_total as f64 / meta.total as f64,
    })
}

fn space_of(fs_dir: &Path, kind: &str) -> Result<Space> {
    let dir = fs_dir.join("allocation").join(kind);
    Ok(Space {
        total: counter(&dir, "total_bytes")?,
        used: counter(&dir, "bytes_used")?,
        reserved: counter(&dir, "bytes_reserved")?,
        pinned: counter(&dir, "bytes_pinned")?,
        readonly: counter(&dir, "bytes_readonly")?,
        may_use: counter(&dir, "bytes_may_use")?,
        disk_total: counter(&dir, "disk_total")?,
    })
}

/// One sysfs counter: a decimal number and a newline.
fn counter(dir: &Path, name: &str) -> Result<u64> {
    let path = dir.join(name);
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    text.trim()
        .parse()
        .with_context(|| format!("{}: not a byte count: {:?}", path.display(), text.trim()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: u64 = 1 << 20;
    const GIB: u64 = 1 << 30;
    /// The fixtures write a block device's `size` file in sysfs; the headroom must not read it.
    const SECTOR_BYTES: u64 = 512;
    const NAMES: [&str; 7] = [
        "total_bytes",
        "bytes_used",
        "bytes_reserved",
        "bytes_pinned",
        "bytes_readonly",
        "bytes_may_use",
        "disk_total",
    ];

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("xtask-btrfs-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    fn put(dir: &Path, file: &str, value: u64) {
        std::fs::create_dir_all(dir).expect("counter dir");
        std::fs::write(dir.join(file), format!("{value}\n")).expect("counter file");
    }

    /// A sysfs-shaped tree: `uuid-1/allocation/<kind>/` from `types` (values in `NAMES` order), and device `sde` of `dev_bytes` (a size the headroom ignores: statfs gives it).
    fn fs_tree(root: &Path, dev_bytes: u64, types: &[(&str, [u64; 7])]) -> PathBuf {
        let fs = root.join("uuid-1");
        for (kind, values) in types {
            let dir = fs.join("allocation").join(kind);
            for (name, value) in NAMES.iter().zip(values) {
                put(&dir, name, *value);
            }
        }
        let block = root.join("block").join("sde");
        put(&block, "size", dev_bytes / SECTOR_BYTES);
        std::fs::create_dir_all(fs.join("devices")).expect("devices dir");
        std::os::unix::fs::symlink(&block, fs.join("devices").join("sde")).expect("device link");
        fs
    }

    /// 8 GiB device: DUP metadata 1 GiB logical (900 MiB used, 100 MiB may_use, 2 GiB raw), data 4 GiB (3 GiB used, raw 4 GiB), system 8 MiB (16 MiB raw).
    fn dup_tree(root: &Path, dev_bytes: u64) -> PathBuf {
        fs_tree(
            root,
            dev_bytes,
            &[
                ("metadata", [GIB, 900 * MIB, 0, 0, 0, 100 * MIB, 2 * GIB]),
                ("data", [4 * GIB, 3 * GIB, 0, 0, 0, 0, 4 * GIB]),
                ("system", [8 * MIB, 0, 0, 0, 0, 0, 16 * MIB]),
            ],
        )
    }

    /// Independent: unallocated = 8 GiB - (2 GiB + 4 GiB + 16 MiB) = 2 GiB - 16 MiB; DUP halves it; chunk free = 1 GiB - (900 + 100) MiB.
    #[test]
    fn headroom_adds_unallocated_at_the_profile_ratio() {
        let root = scratch("dup");
        let h = headroom_of(&dup_tree(&root, 8 * GIB), 8 * GIB).expect("headroom");
        assert_eq!(h.unallocated, 2 * GIB - 16 * MIB);
        assert_eq!(h.from_unallocated, GIB - 8 * MIB);
        assert_eq!(h.free_in_chunks, 24 * MIB);
        assert_eq!(h.bytes(), GIB + 16 * MIB);
        assert_eq!(h.profile, 2.0);
    }

    /// The incident's shape: no unallocated raw space, metadata chunks full.
    #[test]
    fn no_unallocated_leaves_only_the_chunk_free() {
        let root = scratch("full");
        let h = headroom_of(&dup_tree(&root, 6 * GIB + 16 * MIB), 6 * GIB + 16 * MIB)
            .expect("headroom");
        assert_eq!((h.unallocated, h.from_unallocated), (0, 0));
        assert_eq!(h.bytes(), 24 * MIB);
    }

    /// Single profile: raw = logical, so unallocated counts one-to-one. Independent: 8 GiB - (1 GiB + 4 GiB + 8 MiB) = 3 GiB - 8 MiB.
    #[test]
    fn single_profile_counts_unallocated_one_to_one() {
        let root = scratch("single");
        let fs = fs_tree(
            &root,
            8 * GIB,
            &[
                ("metadata", [GIB, 0, 0, 0, 0, 0, GIB]),
                ("data", [4 * GIB, 0, 0, 0, 0, 0, 4 * GIB]),
                ("system", [8 * MIB, 0, 0, 0, 0, 0, 8 * MIB]),
            ],
        );
        let h = headroom_of(&fs, 8 * GIB).expect("headroom");
        assert_eq!(h.from_unallocated, 3 * GIB - 8 * MIB);
        assert_eq!(h.free_in_chunks, GIB);
        assert_eq!(h.profile, 1.0);
    }

    #[test]
    fn a_missing_counter_is_refused_by_name() {
        let root = scratch("missing");
        let fs = dup_tree(&root, 8 * GIB);
        std::fs::remove_file(fs.join("allocation").join("metadata").join("bytes_may_use"))
            .expect("remove counter");
        let e = format!("{:#}", headroom_of(&fs, 8 * GIB).unwrap_err());
        assert!(e.contains("bytes_may_use"), "{e}");
    }

    #[test]
    fn metadata_without_chunks_is_an_error() {
        let root = scratch("no-chunks");
        let fs = fs_tree(
            &root,
            8 * GIB,
            &[("metadata", [0; 7]), ("data", [0; 7]), ("system", [0; 7])],
        );
        assert!(headroom_of(&fs, 8 * GIB).is_err());
    }

    /// Block device 10 GiB, filesystem 8 GiB (a vhdx grown without `btrfs filesystem resize`): unallocated must follow the filesystem.
    /// Independent: 8 GiB - (2 GiB + 4 GiB + 16 MiB) = 2 GiB - 16 MiB.
    #[test]
    fn a_block_device_larger_than_the_fs_is_not_counted() {
        let root = scratch("grown");
        let h = headroom_of(&dup_tree(&root, 10 * GIB), 8 * GIB).expect("headroom");
        assert_eq!(h.unallocated, 2 * GIB - 16 * MIB);
    }

    /// Replicated data (two raw bytes per logical byte here) is refused: statfs would report logical bytes, not the raw size.
    #[test]
    fn replicated_data_is_refused() {
        let root = scratch("replicated");
        let fs = fs_tree(
            &root,
            8 * GIB,
            &[
                ("metadata", [GIB, 0, 0, 0, 0, 0, GIB]),
                ("data", [4 * GIB, 0, 0, 0, 0, 0, 8 * GIB]),
                ("system", [8 * MIB, 0, 0, 0, 0, 0, 8 * MIB]),
            ],
        );
        let e = format!("{:#}", headroom_of(&fs, 8 * GIB).unwrap_err());
        assert!(e.contains("replicated"), "{e}");
    }

    #[test]
    fn the_filesystem_is_the_one_whose_devices_name_the_device() {
        let root = scratch("uuid");
        for (uuid, dev) in [("uuid-1", "sda"), ("uuid-2", "sde")] {
            let block = root.join("block").join(dev);
            put(&block, "size", 1);
            let devs = root.join(uuid).join("devices");
            std::fs::create_dir_all(&devs).expect("devices dir");
            std::os::unix::fs::symlink(&block, devs.join(dev)).expect("device link");
        }
        assert_eq!(fs_dir_of(&root, "sde").expect("found"), root.join("uuid-2"));
        assert!(fs_dir_of(&root, "sdz").is_err());
    }

    const MOUNTS: &str = "\
22 1 0:21 / /dev rw,nosuid shared:2 - tmpfs tmpfs rw
30 1 8:64 / /home/wavjaby/atl-next/out/pool rw,noatime shared:5 - btrfs /dev/sde rw,compress=zstd:1
31 1 8:1 / / rw,relatime shared:1 - ext4 /dev/sda1 rw
40 1 8:2 / /home/wavjaby/atl-next/out rw shared:6 - ext4 /dev/sdb2 rw
";

    #[test]
    fn the_longest_mount_holding_the_path_names_the_device() {
        assert_eq!(
            mount_source(MOUNTS, Path::new("/home/wavjaby/atl-next/out/pool/ws/repo"))
                .expect("btrfs"),
            "/dev/sde"
        );
        // the ext4 mount above the pool holds this path: not btrfs, refused
        assert!(mount_source(MOUNTS, Path::new("/home/wavjaby/atl-next/out/logs")).is_err());
        assert!(mount_source(MOUNTS, Path::new("/etc/hosts")).is_err());
    }

    #[test]
    fn mountinfo_escapes_come_back() {
        assert_eq!(unescape(r"/srv/a\040b\134c"), "/srv/a b\\c");
    }

    /// tmpfs, not btrfs: out.rs's copy tests use /dev/shm for the same reason.
    #[test]
    fn probe_is_none_off_btrfs() {
        assert!(probe(Path::new("/dev/shm")).expect("statfs").is_none());
    }
}
