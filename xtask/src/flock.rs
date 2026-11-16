// SPDX-License-Identifier: MIT
//! A held `flock`, released by an explicit unlock on drop.
//!
//! A flock belongs to the open file description. A child that any thread of this process forks shares that description until the child's exec (`O_CLOEXEC` acts only at exec), so closing our fd alone leaves the lock held for that window: a lock this process just "released" reads as busy to the next taker. `LOCK_UN` frees the description's lock whatever fds still share it, so [`Flock`] unlocks before its file closes. Every flock xtask takes goes through this type -- a bare `File` + `flock` reopens the race.
//! OFD (`F_OFD_SETLK`) locks share the description the same way; POSIX record locks are not inherited but do not exclude threads of one process, which the pool and dir locks rely on.

use rustix::fs::{FlockOperation, flock};
use std::fs::File;
use std::io::{ErrorKind, Result};

/// Lock mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Shared,
    Exclusive,
}

/// `file` with its flock held; dropping it unlocks, then closes.
#[derive(Debug)]
pub struct Flock {
    file: File,
}

impl Flock {
    /// Takes `mode` on `file` without waiting: `None` when another open file holds a conflicting lock.
    pub fn try_take(file: File, mode: Mode) -> Result<Option<Flock>> {
        let op = match mode {
            Mode::Shared => FlockOperation::NonBlockingLockShared,
            Mode::Exclusive => FlockOperation::NonBlockingLockExclusive,
        };
        match flock(&file, op) {
            Ok(()) => Ok(Some(Flock { file })),
            Err(e) if std::io::Error::from(e).kind() == ErrorKind::WouldBlock => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Takes `mode` on `file`, waiting for conflicting holders.
    pub fn take(file: File, mode: Mode) -> Result<Flock> {
        let op = match mode {
            Mode::Shared => FlockOperation::LockShared,
            Mode::Exclusive => FlockOperation::LockExclusive,
        };
        flock(&file, op)?;
        Ok(Flock { file })
    }

    pub fn file(&mut self) -> &mut File {
        &mut self.file
    }
}

impl Drop for Flock {
    fn drop(&mut self) {
        let _ = flock(&self.file, FlockOperation::Unlock);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::OpenOptions;
    use std::os::fd::OwnedFd;
    use std::path::Path;

    fn open(p: &Path) -> File {
        OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(p)
            .expect("open")
    }

    /// The fd a child forked meanwhile holds until its exec: fork copies the fd table, i.e. a dup of the same open file description. (A real fork needs `pre_exec`, which is unsafe and forbidden here; the stress run exercises the real one.)
    fn pending_fork(file: &File) -> OwnedFd {
        rustix::io::dup(file).expect("dup")
    }

    fn free(p: &Path) -> bool {
        Flock::try_take(open(p), Mode::Exclusive).unwrap().is_some()
    }

    /// The race itself: a bare file's flock, closed while a forked child still shares its description, reads as held.
    #[test]
    fn a_closed_file_lock_stays_held_by_a_pending_fork() {
        let p = std::env::temp_dir().join(format!("xtask-flock-bare-{}", std::process::id()));
        let file = open(&p);
        flock(&file, FlockOperation::NonBlockingLockExclusive).expect("free");
        let child = pending_fork(&file);
        drop(file);
        assert!(
            !free(&p),
            "the lock no longer outlives the close: Flock's unlock is moot"
        );
        // no "free once the dup closes" check: a real fork by another test thread may hold a copy of the dup by then -- the race itself
        drop(child);
        let _ = std::fs::remove_file(&p);
    }

    /// The fix: a dropped Flock is free at once, a pending fork or not.
    #[test]
    fn a_dropped_flock_is_free_despite_a_pending_fork() {
        let p = std::env::temp_dir().join(format!("xtask-flock-{}", std::process::id()));
        let mut held = Flock::try_take(open(&p), Mode::Exclusive)
            .unwrap()
            .expect("free");
        let _child = pending_fork(held.file());
        drop(held);
        assert!(free(&p), "the pending fork kept the dropped lock");
        let _ = std::fs::remove_file(&p);
    }
}
