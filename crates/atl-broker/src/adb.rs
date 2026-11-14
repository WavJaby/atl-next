// SPDX-License-Identifier: MIT
//! [`Profile::Adb`](crate::Profile::Adb): AOSP's adbd, unpatched (plan adbd D1 rung 2). adbd starts with the rights its `drop_privileges` needs (setgroups to its 14 AID groups, then uid/gid AID_SHELL and a capability drop: minijail aborts on any failure),
//! so its user namespace maps more than one id: the setuid `newuidmap`/`newgidmap` write the maps from the user's `/etc/subuid` / `/etc/subgid` range (rootless podman's model), bwrap waiting on `--userns-block-fd` until they are written.
//! Map: namespace AID_SHELL -> the desktop user (adbd after its drop and every `adb shell` process: their files are the user's); every other namespace id up to the range's end -> a subordinate id, so namespace root and the AID groups own nothing of the user's.
//! The sandbox's first process runs as namespace AID_SHELL (bwrap's block mode keeps the caller's ids, mapped) with only [`ADBD_CAPS`]; drop_privileges then sets the groups and drops them.

use std::ffi::OsString;
use std::io;
use std::path::Path;

use crate::other;

/// Android's shell uid and gid (android_filesystem_config.h AID_SHELL): adbd's identity after drop_privileges, mapped to the desktop user.
pub const AID_SHELL: u32 = 2000;

/// The highest namespace id adbd sets: its supplementary groups (daemon/main.cpp `drop_privileges`, android_filesystem_config.h) top out at AID_UHID = 3011. A subordinate range too short to map it is refused.
const HIGHEST_ADBD_ID: u32 = 3011;

/// What the sandbox's first process keeps: minijail's set_supplementary_gids needs CAP_SETGID, change_uid CAP_SETUID, its bounding-set drop CAP_SETPCAP, and drop_caps raises the caps adbd keeps (daemon/main.cpp `minijail_use_caps`: SETUID | SETGID | SYS_NICE) into permitted, so one missing = "can't apply initial cleaned capset" abort.
/// All are namespace capabilities: none reaches the host (CAP_SYS_NICE over the init namespace's tasks is checked against the init user namespace).
pub const ADBD_CAPS: &[&str] = &["CAP_SETUID", "CAP_SETGID", "CAP_SETPCAP", "CAP_SYS_NICE"];

/// The setuid helpers that write a child's maps from the caller's subordinate ranges (shadow-utils / uidmap package).
const NEWUIDMAP: &str = "newuidmap";
const NEWGIDMAP: &str = "newgidmap";

/// One `/proc/<pid>/{uid,gid}_map` line: namespace id, host id, count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MapRow {
    pub inside: u32,
    pub outside: u32,
    pub count: u32,
}

/// The uid and gid maps of the adb sandbox ([`rows`] of each range).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdMaps {
    pub uid: Vec<MapRow>,
    pub gid: Vec<MapRow>,
}

/// The map for desktop id `me` and subordinate range (`start`, `count`): namespace 0..AID_SHELL -> the range's head, AID_SHELL -> `me`, the rest of the range after it. Error when the range cannot reach [`HIGHEST_ADBD_ID`].
pub fn rows(me: u32, start: u32, count: u32) -> io::Result<Vec<MapRow>> {
    // namespace ids AID_SHELL+1 ..= count take the range past its first AID_SHELL ids: the range is used whole, the map covers 0..=count
    if count < HIGHEST_ADBD_ID {
        return Err(other(format!(
            "subordinate range of {count} ids: adbd's groups need namespace ids up to {HIGHEST_ADBD_ID}"
        )));
    }
    Ok(vec![
        MapRow {
            inside: 0,
            outside: start,
            count: AID_SHELL,
        },
        MapRow {
            inside: AID_SHELL,
            outside: me,
            count: 1,
        },
        MapRow {
            inside: AID_SHELL + 1,
            outside: start + AID_SHELL,
            count: count - AID_SHELL,
        },
    ])
}

/// The first `/etc/subuid`-format line (`owner:start:count`, subuid(5)) whose owner is `name` or the number `id`: (start, count).
pub fn subordinate(text: &str, name: &str, id: u32) -> Option<(u32, u32)> {
    text.lines().find_map(|l| {
        let mut f = l.trim().split(':');
        let (owner, start, count) = (f.next()?, f.next()?, f.next()?);
        (owner == name || owner.parse() == Ok(id))
            .then(|| Some((start.parse().ok()?, count.parse().ok()?)))?
    })
}

/// The `/etc/passwd`-format entry name of `uid` (passwd(5)); None when it has none there (an NSS-only user: subordinate entries by number still match).
pub fn user_name(passwd: &str, uid: u32) -> Option<String> {
    passwd.lines().find_map(|l| {
        let mut f = l.split(':');
        let name = f.next()?;
        (f.nth(1)?.parse() == Ok(uid)).then(|| name.to_owned())
    })
}

/// This user's maps from the host's `/etc/passwd`, `/etc/subuid`, `/etc/subgid`. Error names the missing entry: a host requirement of this off-by-default profile (M69 D1), set up by the administrator.
pub fn host_maps() -> io::Result<IdMaps> {
    let (uid, gid) = (
        rustix::process::getuid().as_raw(),
        rustix::process::getgid().as_raw(),
    );
    let name = user_name(&std::fs::read_to_string("/etc/passwd")?, uid).unwrap_or_default();
    let range = |file: &str, id: u32| -> io::Result<Vec<MapRow>> {
        let text = std::fs::read_to_string(file)
            .map_err(|e| other(format!("{file}: {e} (adb needs a subordinate range there)")))?;
        let (start, count) = subordinate(&text, &name, uid).ok_or_else(|| {
            other(format!(
                "{file}: no entry for {name:?} (uid {uid}): adb needs a subordinate range"
            ))
        })?;
        rows(id, start, count)
    };
    Ok(IdMaps {
        uid: range("/etc/subuid", uid)?,
        gid: range("/etc/subgid", gid)?,
    })
}

/// `newuidmap`/`newgidmap` argv tail for `pid`: the pid, then each row's three numbers.
pub fn map_args(pid: i32, rows: &[MapRow]) -> Vec<OsString> {
    let mut v = vec![OsString::from(pid.to_string())];
    for r in rows {
        v.extend([r.inside, r.outside, r.count].map(|n| OsString::from(n.to_string())));
    }
    v
}

/// Writes `maps` into the user namespace of `pid` (bwrap's waiting first process) with the setuid helpers.
pub fn write_maps(pid: i32, maps: &IdMaps) -> io::Result<()> {
    for (helper, rows) in [(NEWUIDMAP, &maps.uid), (NEWGIDMAP, &maps.gid)] {
        #[expect(
            clippy::disallowed_types,
            reason = "the setuid map helpers: no sandbox, they write another process's namespace maps"
        )]
        let out = std::process::Command::new(helper)
            .args(map_args(pid, rows))
            .env_clear()
            .current_dir(Path::new("/"))
            .output()
            .map_err(|e| {
                other(format!(
                    "running {helper}: {e} (adb needs the uidmap/shadow helpers)"
                ))
            })?;
        if !out.status.success() {
            return Err(other(format!(
                "{helper} {pid}: {} {}",
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// WSL's entry (`wavjaby:100000:65536`, the uidmap default for the first user): ns 0..2000 and 2001..=65536 from the range, 2000 the user; 2000 + 63536 + 1 rows' counts = 65537 ids, the range used whole (65536) plus the user.
    #[test]
    fn rows_cover_the_range_and_map_shell_to_the_user() {
        let r = rows(1000, 100_000, 65_536).unwrap();
        assert_eq!(
            r,
            vec![
                MapRow {
                    inside: 0,
                    outside: 100_000,
                    count: 2000
                },
                MapRow {
                    inside: 2000,
                    outside: 1000,
                    count: 1
                },
                MapRow {
                    inside: 2001,
                    outside: 102_000,
                    count: 63_536
                },
            ]
        );
        let sub: u32 = r
            .iter()
            .filter(|m| m.outside != 1000)
            .map(|m| m.count)
            .sum();
        assert_eq!(sub, 65_536);
        assert!(r[2].inside + r[2].count > HIGHEST_ADBD_ID);
    }

    /// Break: a range that ends below AID_UHID cannot map drop_privileges' groups.
    #[test]
    fn short_range_is_refused() {
        assert!(rows(1000, 100_000, HIGHEST_ADBD_ID - 1).is_err());
    }

    #[test]
    fn subordinate_by_name_or_number() {
        let t = "root:1:2\nwavjaby:100000:65536\n1001:200000:65536\n";
        assert_eq!(subordinate(t, "wavjaby", 1000), Some((100_000, 65_536)));
        assert_eq!(subordinate(t, "", 1001), Some((200_000, 65_536)));
        assert_eq!(subordinate(t, "nobody", 1002), None);
        assert_eq!(
            user_name(
                "root:x:0:0::/root:/bin/sh\nwavjaby:x:1000:1000::/home/w:/bin/bash\n",
                1000
            )
            .as_deref(),
            Some("wavjaby")
        );
    }

    #[test]
    fn map_args_flatten_rows() {
        let a = map_args(42, &rows(1000, 100_000, 65_536).unwrap());
        let a: Vec<_> = a.iter().map(|s| s.to_str().unwrap()).collect();
        assert_eq!(
            a,
            [
                "42", "0", "100000", "2000", "2000", "1000", "1", "2001", "102000", "63536"
            ]
        );
    }
}
