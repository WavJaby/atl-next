// SPDX-License-Identifier: MIT
//! cargo's blocking file locks, read from its stderr. cargo prints `Blocking waiting for file lock on <what>` before it blocks, so that line starts a wait; the next output, or cargo's exit, ends it. A wait past [`ANNOUNCE_AFTER`] is announced once with its holder (pid and command line, found in `/proc/locks` by the lock file's device and inode). `unit_use::run` is the only user; the totals go to `timing`.

use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use crate::timing;

/// A wait this long is announced; shorter ones count only in the phase report. 2 s is chosen, not measured: cargo's no-op runs sit well below it. Check the report's wait counts before changing it.
pub const ANNOUNCE_AFTER: Duration = Duration::from_secs(2);

/// How often the announcer checks the open wait: an announcement comes at most this late.
pub const TICK: Duration = Duration::from_millis(250);

const BLOCKED: &str = "Blocking waiting for file lock on ";

/// The lock a cargo stderr line says it blocks on (`package cache`, `build directory`), when the line starts a wait.
pub fn blocked_on(line: &str) -> Option<&str> {
    line.trim_start().strip_prefix(BLOCKED).map(str::trim)
}

/// Target dir of cargo run `cmd`: its `--target-dir` (either form), else `CARGO_TARGET_DIR`, else `<cwd>/target`.
pub fn target_dir(cmd: &Command) -> Option<PathBuf> {
    let mut args = cmd.get_args();
    while let Some(arg) = args.next() {
        if arg == "--target-dir" {
            return args.next().map(PathBuf::from);
        }
        if let Some(dir) = arg.to_str().and_then(|a| a.strip_prefix("--target-dir=")) {
            return Some(PathBuf::from(dir));
        }
    }
    std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .or_else(|| cmd.get_current_dir().map(|dir| dir.join("target")))
}

/// The lock file cargo takes for `what`, when this module knows it: `<CARGO_HOME>/.package-cache`, `<target>/.cargo-lock`.
pub fn lock_file(what: &str, target: Option<&Path>) -> Option<PathBuf> {
    match what {
        "package cache" => cargo_home().map(|home| home.join(".package-cache")),
        "build directory" => target.map(|dir| dir.join(".cargo-lock")),
        _ => None,
    }
}

fn cargo_home() -> Option<PathBuf> {
    std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cargo")))
}

/// The holder of lock `what` (see [`lock_file`]): `pid <n> <command line>`; None when nothing holds it or /proc says nothing.
pub fn holder_of(what: &str, target: Option<&Path>) -> Option<String> {
    holder(&lock_file(what, target)?)
}

/// Who holds an exclusive flock on `file`: `pid <n> <command line>`, from `/proc/locks` (matched by device and inode) and `/proc/<n>/cmdline`.
pub fn holder(file: &Path) -> Option<String> {
    let meta = std::fs::metadata(file).ok()?;
    let key = format!(
        "{:02x}:{:02x}:{}",
        rustix::fs::major(meta.dev()),
        rustix::fs::minor(meta.dev()),
        meta.ino()
    );
    let pid = holder_pid(&std::fs::read_to_string("/proc/locks").ok()?, &key)?;
    let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    let command = String::from_utf8_lossy(&cmdline).replace('\0', " ");
    Some(format!("pid {pid} {}", command.trim()))
}

/// The pid of the exclusive holder of `key` (`maj:min:inode`) in `/proc/locks` text: `3: FLOCK  ADVISORY  WRITE 3553140 00:57:17161404 0 EOF`.
fn holder_pid(locks: &str, key: &str) -> Option<u32> {
    locks.lines().find_map(|line| {
        let f: Vec<&str> = line.split_whitespace().collect();
        let held =
            f.get(1) == Some(&"FLOCK") && f.get(3) == Some(&"WRITE") && f.get(5) == Some(&key);
        if held { f.get(4)?.parse().ok() } else { None }
    })
}

/// cargo's stderr as it arrives, with the wait open now (if any).
#[derive(Default)]
pub struct Watch {
    /// Bytes of the line not complete yet.
    partial: Vec<u8>,
    open: Option<Open>,
}

struct Open {
    what: String,
    since: Instant,
    holder: Option<String>,
    announced: bool,
}

/// A closed wait: its lock, its length, and its holder when it was announced.
pub struct Ended {
    pub what: String,
    pub took: Duration,
    pub holder: Option<String>,
}

impl Watch {
    /// Reads `bytes` that arrived at `at`; returns the waits they closed. A `Blocking` line opens a wait, any later output closes it.
    pub fn feed(&mut self, bytes: &[u8], at: Instant) -> Vec<Ended> {
        let mut data = std::mem::take(&mut self.partial);
        data.extend_from_slice(bytes);
        let mut ended = Vec::new();
        let mut start = 0;
        while let Some(off) = data[start..].iter().position(|&b| b == b'\n') {
            let end = start + off;
            let line = String::from_utf8_lossy(&data[start..end]).into_owned();
            ended.extend(self.line(&line, at));
            start = end + 1;
        }
        // a fragment is output too: cargo writes whole lines, so a fragment means it went on
        if start < data.len() {
            ended.extend(self.close(at));
        }
        self.partial = data[start..].to_vec();
        ended
    }

    /// The stream ended at `at`: closes the open wait, if any.
    pub fn finish(&mut self, at: Instant) -> Option<Ended> {
        self.close(at)
    }

    /// The announcement for the open wait once it has lasted [`ANNOUNCE_AFTER`] at `at`, once per wait; `holder_of` names the holder.
    pub fn announce(
        &mut self,
        at: Instant,
        holder_of: impl FnOnce(&str) -> Option<String>,
    ) -> Option<String> {
        let open = self.open.as_mut().filter(|open| !open.announced)?;
        let waited = at.saturating_duration_since(open.since);
        if waited < ANNOUNCE_AFTER {
            return None;
        }
        open.announced = true;
        open.holder = holder_of(&open.what);
        Some(match &open.holder {
            Some(holder) => format!(
                "waiting for {} held by {holder} (blocked {})",
                open.what,
                timing::secs(waited)
            ),
            None => format!(
                "waiting for {} (holder unknown; blocked {})",
                open.what,
                timing::secs(waited)
            ),
        })
    }

    fn line(&mut self, line: &str, at: Instant) -> Option<Ended> {
        let ended = self.close(at);
        if let Some(what) = blocked_on(line) {
            self.open = Some(Open {
                what: what.to_owned(),
                since: at,
                holder: None,
                announced: false,
            });
        }
        ended
    }

    fn close(&mut self, at: Instant) -> Option<Ended> {
        self.open.take().map(|open| Ended {
            what: open.what,
            took: at.saturating_duration_since(open.since),
            holder: open.holder,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_blocked_line_names_its_lock() {
        assert_eq!(
            blocked_on("    Blocking waiting for file lock on package cache"),
            Some("package cache")
        );
        assert_eq!(blocked_on("   Compiling xtask v0.1.0"), None);
    }

    #[test]
    fn the_holder_is_the_flock_writer_of_the_key() {
        let locks = "1: POSIX  ADVISORY  READ 3553375 00:57:17161884 128 128\n3: FLOCK  ADVISORY  WRITE 3553140 00:57:17161404 0 EOF\n";
        assert_eq!(holder_pid(locks, "00:57:17161404"), Some(3_553_140));
        // a POSIX record on the same file is not the flock holder
        assert_eq!(holder_pid(locks, "00:57:17161884"), None);
    }

    #[test]
    fn a_flock_held_here_is_found_as_this_process() {
        let dir = std::env::temp_dir().join(format!("cargo-lock-holder-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let lock = dir.join("lock");
        let file = std::fs::File::create(&lock).expect("lock file");
        rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive)
            .expect("flock");
        let held = holder(&lock);
        drop(file);
        let _ = std::fs::remove_dir_all(&dir);
        let held = held.expect("the holder of a flock held here");
        assert!(
            held.starts_with(&format!("pid {} ", std::process::id())),
            "{held}"
        );
    }

    #[test]
    fn a_wait_runs_from_its_line_to_the_next_output() {
        let t0 = Instant::now();
        let mut w = Watch::default();
        assert!(
            w.feed(b"    Blocking waiting for file lock on package cache\n", t0)
                .is_empty()
        );
        let ended = w.feed(b"   Compiling xtask\n", t0 + Duration::from_secs(3));
        assert_eq!(ended.len(), 1);
        assert_eq!(ended[0].what, "package cache");
        assert_eq!(ended[0].took, Duration::from_secs(3));
        assert!(w.finish(t0 + Duration::from_secs(4)).is_none());
    }

    #[test]
    fn a_line_split_across_reads_opens_its_wait_once_complete() {
        let t0 = Instant::now();
        let mut w = Watch::default();
        assert!(w.feed(b"    Blocking waiting for file lo", t0).is_empty());
        assert!(
            w.feed(b"ck on build directory\n", t0 + Duration::from_secs(1))
                .is_empty()
        );
        let ended = w.finish(t0 + Duration::from_secs(4)).expect("open wait");
        assert_eq!(ended.what, "build directory");
        assert_eq!(ended.took, Duration::from_secs(3));
    }

    #[test]
    fn a_wait_is_announced_once_past_the_threshold() {
        let t0 = Instant::now();
        let mut w = Watch::default();
        w.feed(b"    Blocking waiting for file lock on package cache\n", t0);
        let holder = || Some("pid 7 cargo build".to_owned());
        assert_eq!(w.announce(t0 + Duration::from_secs(1), |_| holder()), None);
        assert_eq!(
            w.announce(t0 + Duration::from_secs(2), |_| holder())
                .as_deref(),
            Some("waiting for package cache held by pid 7 cargo build (blocked 2.0s)")
        );
        assert_eq!(w.announce(t0 + Duration::from_secs(3), |_| holder()), None);
        let ended = w.finish(t0 + Duration::from_secs(5)).expect("open wait");
        assert_eq!(ended.holder.as_deref(), Some("pid 7 cargo build"));
    }

    #[test]
    fn the_target_dir_is_the_flag_in_either_form() {
        let mut cmd = Command::new("cargo");
        cmd.args(["build", "--target-dir", "/x/out"]);
        assert_eq!(target_dir(&cmd), Some(PathBuf::from("/x/out")));
        let mut cmd = Command::new("cargo");
        cmd.arg("--target-dir=/y");
        assert_eq!(target_dir(&cmd), Some(PathBuf::from("/y")));
    }
}
