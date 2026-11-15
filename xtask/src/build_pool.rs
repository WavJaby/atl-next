// SPDX-License-Identifier: MIT
//! Machine-wide build tokens: `xtask native` holds one while ninja runs, so at most [`tokens`] full native builds run at once whatever the number of checkouts.
//! Only the ninja build queues; tests, `check` and cargo builds never take a token.
//! A token is an exclusive `flock` on `<workspace>/cache/build-pool/token-<i>`: the kernel drops it when the holder's file closes, so exit, error, panic and SIGKILL all free it -- no stale-lock cleanup exists or is needed.
//! The holder writes `<checkout> pid <pid>` into its token file; a waiter prints those once, then retries every [`POLL`].

use std::fs::{File, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use rustix::fs::{FlockOperation, flock};

/// Concurrent full native builds of workspace `ws`: `build.parallel_builds` (`ws_config::DEFAULT_PARALLEL_BUILDS` explains the default).
pub fn tokens(ws: &Path) -> Result<u32> {
    Ok(crate::ws_config::load(ws)?.parallel_builds.value)
}

/// Retry interval while every token is held; builds take minutes, so a second of extra wait is noise.
pub const POLL: Duration = Duration::from_secs(1);

/// The token dir under workspace `ws`.
pub fn dir(ws: &Path) -> PathBuf {
    ws.join("cache").join("build-pool")
}

/// A held token; dropping it (or the process dying) releases it.
#[derive(Debug)]
pub struct Token {
    _file: File,
    pub index: u32,
}

/// Takes one of `tokens` tokens in `dir`, recording `who` in it. Blocks while all are held: `on_wait` gets one line naming every holder, once.
pub fn acquire(
    dir: &Path,
    tokens: u32,
    who: &str,
    poll: Duration,
    on_wait: impl FnOnce(&str),
) -> Result<Token> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let mut on_wait = Some(on_wait);
    loop {
        for index in 1..=tokens {
            if let Some(token) = try_take(dir, index, who)? {
                return Ok(token);
            }
        }
        if let Some(f) = on_wait.take() {
            f(&holders(dir, tokens));
        }
        std::thread::sleep(poll);
    }
}

/// A native build's pool token for checkout `root` in workspace `ws`. Prints the wait as `<tag>: <line>`.
pub fn acquire_build(ws: &Path, root: &Path, tag: &str) -> Result<Token> {
    acquire(&dir(ws), tokens(ws)?, &this_holder(root), POLL, |line| {
        println!("{tag}: {line}")
    })
}

/// Every one of `tokens` tokens without waiting (a quiet window: no ninja build runs while they are held): Ok(all), or Err(the holders line) when any is held -- the ones taken are released again.
pub fn try_take_all(
    dir: &Path,
    tokens: u32,
    who: &str,
) -> Result<std::result::Result<Vec<Token>, String>> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let mut held = Vec::new();
    for index in 1..=tokens {
        match try_take(dir, index, who)? {
            Some(t) => held.push(t),
            None => {
                drop(held);
                return Ok(Err(holders(dir, tokens)));
            }
        }
    }
    Ok(Ok(held))
}

fn path(dir: &Path, index: u32) -> PathBuf {
    dir.join(format!("token-{index}"))
}

fn try_take(dir: &Path, index: u32, who: &str) -> Result<Option<Token>> {
    let p = path(dir, index);
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&p)
        .with_context(|| format!("opening {}", p.display()))?;
    match flock(&file, FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => {}
        Err(e) if std::io::Error::from(e).kind() == ErrorKind::WouldBlock => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("flock {}", p.display())),
    }
    // the record is written only under the lock; a reader may see it half-written, which costs a garbled wait line, nothing else
    file.set_len(0)?;
    file.write_all(who.as_bytes())?;
    Ok(Some(Token { _file: file, index }))
}

/// The wait line: each token's last recorded holder.
fn holders(dir: &Path, tokens: u32) -> String {
    let list: Vec<String> = (1..=tokens)
        .map(|i| {
            let who = std::fs::read_to_string(path(dir, i)).unwrap_or_default();
            format!("token {i}: {}", who.trim())
        })
        .collect();
    format!(
        "build pool: all {tokens} tokens held ({}), waiting",
        list.join("; ")
    )
}

/// `who` for this process: the checkout and the pid.
pub fn this_holder(root: &Path) -> String {
    format!("{} pid {}", root.display(), std::process::id())
}

/// Per-build-dir lock `<workspace>/cache/build-pool/ws-<id>.lock`. Every process that resolves build dir `id` (`out::out_dir`) holds it shared until it exits; making, deleting and pruning the dir take it exclusive, so a dir is never changed under a build. Freed by the kernel on exit or SIGKILL, like the tokens.
/// Covers only xtask processes: cargo runs that xtask does not wrap are invisible to it.
pub fn dir_lock_path(ws: &Path, id: &str) -> PathBuf {
    dir(ws).join(format!("ws-{id}.lock"))
}

/// A held dir lock; dropping it (or the process dying) releases it.
#[derive(Debug)]
pub struct DirLock {
    _file: File,
}

fn open_dir_lock(ws: &Path, id: &str) -> Result<(PathBuf, File)> {
    let p = dir_lock_path(ws, id);
    std::fs::create_dir_all(dir(ws)).with_context(|| format!("creating {}", dir(ws).display()))?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&p)
        .with_context(|| format!("opening {}", p.display()))?;
    Ok((p, file))
}

/// Takes `op` on `id`'s lock; when it would block, `on_wait` runs once and the call waits.
fn take_dir(ws: &Path, id: &str, shared: bool, on_wait: impl FnOnce()) -> Result<DirLock> {
    let (p, file) = open_dir_lock(ws, id)?;
    let (try_op, op) = if shared {
        (
            FlockOperation::NonBlockingLockShared,
            FlockOperation::LockShared,
        )
    } else {
        (
            FlockOperation::NonBlockingLockExclusive,
            FlockOperation::LockExclusive,
        )
    };
    match flock(&file, try_op) {
        Ok(()) => {}
        Err(e) if std::io::Error::from(e).kind() == ErrorKind::WouldBlock => {
            on_wait();
            flock(&file, op).with_context(|| format!("flock {}", p.display()))?;
        }
        Err(e) => return Err(e).with_context(|| format!("flock {}", p.display())),
    }
    Ok(DirLock { _file: file })
}

/// Shared hold on build dir `id`: any number of builds at once; waits (after `on_wait`) only while the dir is being made, deleted or pruned.
pub fn hold_dir(ws: &Path, id: &str, on_wait: impl FnOnce()) -> Result<DirLock> {
    take_dir(ws, id, true, on_wait)
}

/// Exclusive hold on build dir `id` for making it: waits (after `on_wait`) for every holder.
pub fn lock_dir(ws: &Path, id: &str, on_wait: impl FnOnce()) -> Result<DirLock> {
    take_dir(ws, id, false, on_wait)
}

/// Exclusive hold on build dir `id`, without waiting: `None` when a build holds it or another prune runs.
pub fn try_lock_dir(ws: &Path, id: &str) -> Result<Option<DirLock>> {
    let (p, file) = open_dir_lock(ws, id)?;
    match flock(&file, FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => Ok(Some(DirLock { _file: file })),
        Err(e) if std::io::Error::from(e).kind() == ErrorKind::WouldBlock => Ok(None),
        Err(e) => Err(e).with_context(|| format!("flock {}", p.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};
    use std::sync::mpsc;

    const FAST: Duration = Duration::from_millis(20);
    /// Long enough that a waiter which should block has polled many times (FAST x 25).
    const STILL_WAITING: Duration = Duration::from_millis(500);

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("xtask-pool-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    /// Tries before a lock released a moment ago reads as free. The flock stays held by a child another test thread forked before the close and has not exec'd yet (git_dirs.rs hits the same race): FREE_TRIES x FREE_POLL covers that window.
    const FREE_TRIES: u32 = 100;
    const FREE_POLL: Duration = Duration::from_millis(10);

    fn retry_until_free<T>(mut attempt: impl FnMut() -> Option<T>) -> Option<T> {
        for _ in 0..FREE_TRIES {
            if let Some(got) = attempt() {
                return Some(got);
            }
            std::thread::sleep(FREE_POLL);
        }
        None
    }

    /// Every build's shared hold keeps prune out of its dir and no other: prune gets dir a only after the last hold is gone; dir b was never held.
    #[test]
    fn prune_waits_for_every_build_hold() {
        let dir = scratch("dir-lock");
        let first = hold_dir(&dir, "a", || panic!("uncontended hold waited")).unwrap();
        let second = hold_dir(&dir, "a", || panic!("a second build waited")).unwrap();
        assert!(
            try_lock_dir(&dir, "a").unwrap().is_none(),
            "builds hold dir a, prune got it"
        );
        drop(first);
        assert!(
            try_lock_dir(&dir, "a").unwrap().is_none(),
            "one build still holds dir a"
        );
        drop(second);
        assert!(
            retry_until_free(|| try_lock_dir(&dir, "a").expect("lock file")).is_some(),
            "no build left, prune must get dir a"
        );
        assert!(
            try_lock_dir(&dir, "b").unwrap().is_some(),
            "dir b has no holder"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A build that starts while prune holds its dir reports the wait once, then runs after prune.
    #[test]
    fn build_waits_for_a_running_prune() {
        let dir = scratch("dir-prune-first");
        let prune = try_lock_dir(&dir, "a").unwrap().expect("free dir");
        let (wtx, wrx) = mpsc::channel();
        let (tx, rx) = mpsc::channel();
        let d = dir.clone();
        let build = std::thread::spawn(move || {
            let hold = hold_dir(&d, "a", || wtx.send(()).unwrap()).unwrap();
            tx.send(()).unwrap();
            drop(hold);
        });
        wrx.recv_timeout(STILL_WAITING)
            .expect("build did not report the wait");
        assert!(
            rx.recv_timeout(STILL_WAITING).is_err(),
            "build ran while prune held the dir"
        );
        drop(prune);
        rx.recv_timeout(STILL_WAITING * 4)
            .expect("build did not get the dir after prune");
        build.join().unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// flock is per open file, so two tokens taken in one process exclude each other as two processes would.
    #[test]
    fn third_holder_waits_until_one_releases() {
        let dir = scratch("third");
        let a = acquire(&dir, 2, "a", FAST, |_| panic!("a waited")).unwrap();
        let b = acquire(&dir, 2, "b", FAST, |_| panic!("b waited")).unwrap();
        assert_ne!(a.index, b.index);
        let (tx, rx) = mpsc::channel();
        let (wtx, wrx) = mpsc::channel();
        let d = dir.clone();
        let waiter = std::thread::spawn(move || {
            let t = acquire(&d, 2, "c", FAST, |line| wtx.send(line.to_owned()).unwrap()).unwrap();
            tx.send(t.index).unwrap();
        });
        let line = wrx.recv_timeout(STILL_WAITING).expect("no wait line");
        assert!(
            line.contains("token 1: a") && line.contains("token 2: b"),
            "{line}"
        );
        assert!(
            rx.recv_timeout(STILL_WAITING).is_err(),
            "third holder did not wait"
        );
        let freed = a.index;
        drop(a);
        assert_eq!(rx.recv_timeout(STILL_WAITING * 4).unwrap(), freed);
        waiter.join().unwrap();
        drop(b);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A holder killed with SIGKILL frees its token: the lock goes with its file. The holder is flock(1) (util-linux), another process.
    #[test]
    fn killed_holder_frees_its_token() {
        let dir = scratch("killed");
        std::fs::create_dir_all(&dir).unwrap();
        // -o: the lock fd is not passed to sleep, so flock(1) alone holds it
        let mut child = Command::new("flock")
            .arg("-o")
            .arg(path(&dir, 1))
            .args(["sleep", "60"])
            .stdout(Stdio::null())
            // the orphaned sleep must not hold the test binary's stderr open (a piped `cargo test` would wait 60 s)
            .stderr(Stdio::null())
            .spawn()
            .expect("flock(1) from util-linux");
        // wait until the child holds it
        let held = (0..250).any(|_| {
            std::thread::sleep(FAST);
            try_take(&dir, 1, "probe").unwrap().is_none()
        });
        assert!(held, "flock(1) never took the token");
        assert!(try_take(&dir, 1, "probe").unwrap().is_none());
        child.kill().unwrap();
        child.wait().unwrap();
        let freed = (0..250).any(|_| {
            std::thread::sleep(FAST);
            try_take(&dir, 1, "after").unwrap().is_some()
        });
        assert!(freed, "the killed holder's token stayed held");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
