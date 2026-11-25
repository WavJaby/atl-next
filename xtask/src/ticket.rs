// SPDX-License-Identifier: MIT
//! FIFO queue tickets in a directory: one file per waiter, exclusively flocked by it for as long as it lives (the lock is taken before the file is visible), so a killed waiter's ticket is skipped and removed by the next reader -- no stale-ticket cleanup exists or is needed.
//! Names sort in arrival order (`<prefix><nanos:020>-<pid:010>-<seq:010>`); the prefix lets a user order tiers before arrival (build_pool.rs `Tier`). The file holds a short payload its waiter wrote (a class name, a state).
//! Users: the build pool's queue (`cache/build-pool/queue/`) and the RAM store's admission queue (delta.rs, `cache/build-pool/delta/queue/`).

use std::fs::OpenOptions;
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};

use crate::flock::{Flock, Mode};

/// Ticket counter: the same process queues several tickets from several threads in one nanosecond.
static TICKETS: AtomicU64 = AtomicU64::new(0);

/// One queue entry, removed when dropped.
pub struct Ticket {
    pub path: PathBuf,
    pub name: String,
    lock: Flock,
}

impl Ticket {
    /// Queues a ticket in `queue` named `<prefix><arrival>` for waiter `pid`, holding `payload`. Written under `tmp-`, locked, then renamed.
    pub fn new(queue: &Path, prefix: &str, pid: u32, payload: &str) -> Result<Ticket> {
        let seq = TICKETS.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let name = format!("{prefix}{nanos:020}-{pid:010}-{seq:010}");
        let tmp = queue.join(format!("tmp-{name}"));
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&tmp)
            .with_context(|| format!("creating {}", tmp.display()))?;
        let mut lock = Flock::try_take(file, Mode::Exclusive)
            .with_context(|| format!("flock {}", tmp.display()))?
            .with_context(|| format!("{} is held: a fresh ticket", tmp.display()))?;
        lock.file()
            .write_all(payload.as_bytes())
            .with_context(|| format!("writing {}", tmp.display()))?;
        let path = queue.join(&name);
        std::fs::rename(&tmp, &path).with_context(|| format!("queueing {}", path.display()))?;
        Ok(Ticket { path, name, lock })
    }

    /// Renames the ticket to `name` in `queue`: the waiter's lock (on the open file) stays and readers see one name or the other.
    pub fn rename(&mut self, queue: &Path, name: String) -> Result<()> {
        let path = queue.join(&name);
        std::fs::rename(&self.path, &path)
            .with_context(|| format!("renaming {}", self.path.display()))?;
        self.path = path;
        self.name = name;
        Ok(())
    }

    /// Replaces the payload. A reader may see it half-written: readers treat an unknown payload as the most restrictive state.
    pub fn set_payload(&mut self, payload: &str) -> Result<()> {
        let f = self.lock.file();
        f.set_len(0)?;
        use std::io::Seek;
        f.seek(std::io::SeekFrom::Start(0))?;
        f.write_all(payload.as_bytes())
            .with_context(|| format!("writing {}", self.path.display()))
    }
}

impl Drop for Ticket {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// The live tickets of `queue`, sorted by name, each with its payload (empty when unreadable). A ticket whose lock is free has no waiter left: it is removed.
pub fn live(queue: &Path) -> Result<Vec<(String, String)>> {
    let mut live = Vec::new();
    for entry in std::fs::read_dir(queue).with_context(|| format!("reading {}", queue.display()))? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        // a `tmp-` entry is still being written by its waiter
        if name.starts_with("tmp-") {
            continue;
        }
        let path = entry.path();
        let file = match OpenOptions::new().read(true).open(&path) {
            Ok(f) => f,
            Err(e) if e.kind() == ErrorKind::NotFound => continue,
            Err(e) => return Err(e).with_context(|| format!("opening {}", path.display())),
        };
        match Flock::try_take(file, Mode::Exclusive)
            .with_context(|| format!("flock {}", path.display()))?
        {
            Some(_dead) => {
                let _ = std::fs::remove_file(&path);
            }
            None => {
                let payload = std::fs::read_to_string(&path).unwrap_or_default();
                live.push((name, payload.trim().to_owned()));
            }
        }
    }
    // names sort in arrival order: fixed-width time, then pid, then counter
    live.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(live)
}

/// (live tickets ordered before `me`, all live tickets); `me` is a ticket's file name.
pub fn position<T>(tickets: &[(String, T)], me: &str) -> (usize, usize) {
    let ahead = tickets
        .iter()
        .filter(|(name, _)| name.as_str() < me)
        .count();
    (ahead, tickets.len())
}
