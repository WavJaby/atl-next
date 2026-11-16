// SPDX-License-Identifier: MIT
//! Machine-wide admission: a weighted pool of CPU slots and memory (GiB). Heavy work holds a [`Grant`] around one leaf (a ninja build, a cargo run) and releases it after.
//! A slot is an exclusive `flock` on `<workspace>/cache/build-pool/cpu-<i>` or `mem-<i>`: the kernel drops it when the holder's file closes, so exit, error, panic and SIGKILL all free it -- no stale-lock cleanup exists or is needed.
//! Waiters queue as tickets in `cache/build-pool/queue/`; the oldest live ticket is served first, so a large request is not starved by small ones behind it.
//! A grant takes CPU elastically (the free slots up to the class max, at least its min); the count is the tool's job count (ninja `-j`, cargo `CARGO_BUILD_JOBS`).
//! Nested admission is refused: a thread holding a grant, or a process with a holder among its ancestors, would wait for slots its own holder keeps.
//! Sizes and weights: `[pool]` in `workspace.toml` (ws_config.rs); the defaults and their derivation are on [`Class::default_weight`].

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{ErrorKind, Write};
use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail, ensure};
use rustix::fs::{FlockOperation, flock};

use crate::ws_config::{self, PoolConfig};

/// Retry interval while a request waits: a build takes minutes, so a second of extra wait is noise.
pub const POLL: Duration = Duration::from_secs(1);

/// Upper bound on the ancestor walk: a process tree is a few levels deep; the walk stops at init.
const MAX_DEPTH: usize = 64;

/// What heavy work takes from the pool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Class {
    /// ninja on a cold or reconfigured graph
    NativeFull,
    /// ninja on a warm graph
    NativeIncr,
    /// one cargo build, test or clippy run
    Cargo,
    /// a runtime gate: the product's processes plus the cargo and JVM work around them
    Harness,
    /// static checks
    Light,
}

impl Class {
    pub const ALL: [Class; 5] = [
        Class::NativeFull,
        Class::NativeIncr,
        Class::Cargo,
        Class::Harness,
        Class::Light,
    ];

    /// The name in `workspace.toml` (`[pool.<name>]`) and in every log line.
    pub fn name(self) -> &'static str {
        match self {
            Class::NativeFull => "native-full",
            Class::NativeIncr => "native-incr",
            Class::Cargo => "cargo",
            Class::Harness => "harness",
            Class::Light => "light",
        }
    }

    pub fn from_name(name: &str) -> Option<Class> {
        Class::ALL.into_iter().find(|c| c.name() == name)
    }

    /// Default weight; each value says what it is based on. Overridden per machine by `[pool.<name>]`.
    pub fn default_weight(self) -> ClassWeight {
        match self {
            // ninja on a cold graph: job RSS max 7.0 GB (2026-11-16, filter-data-g1). cpu 8..16: one build takes every free slot, so a second one waits for cpu
            Class::NativeFull => ClassWeight {
                cpu_min: 8,
                cpu_max: 16,
                mem_gib: 7,
            },
            // ninja on a warm graph: RSS max 1.8 GB, rounded up
            Class::NativeIncr => ClassWeight {
                cpu_min: 4,
                cpu_max: 16,
                mem_gib: 2,
            },
            // one cargo run: check's RSS max 5.4 GB over the whole check (an upper bound per step), rounded up
            Class::Cargo => ClassWeight {
                cpu_min: 4,
                cpu_max: 16,
                mem_gib: 6,
            },
            // a runtime gate: not measured yet, placeholder
            Class::Harness => ClassWeight {
                cpu_min: 4,
                cpu_max: 6,
                mem_gib: 2,
            },
            // static checks: no measurable memory, one slot
            Class::Light => ClassWeight {
                cpu_min: 1,
                cpu_max: 1,
                mem_gib: 0,
            },
        }
    }
}

/// What one grant takes: CPU slots `cpu_min..=cpu_max` (elastic) and `mem_gib` memory slots.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClassWeight {
    pub cpu_min: u32,
    pub cpu_max: u32,
    pub mem_gib: u32,
}

thread_local! {
    /// The grant this thread holds: its holder record and CPU count. Cleared when the grant drops.
    static HELD: RefCell<Option<(String, u32)>> = const { RefCell::new(None) };
}

/// Ticket counter: the same process queues several tickets from several threads in one nanosecond.
static TICKETS: AtomicU64 = AtomicU64::new(0);

/// Slots held until drop. `cpu` is the job count for the tool; `mem_gib` is reserved memory.
#[derive(Debug)]
pub struct Grant {
    /// The class name, or `quiet` for [`try_take_all`].
    pub class: &'static str,
    pub cpu: u32,
    /// CPU slots the pool has in total.
    pub cpu_of: u32,
    pub mem_gib: u32,
    /// The slot files: their locks are the reservation, released when the grant drops.
    _files: Vec<File>,
    /// Thread-bound: the grant marker is per thread, so the grant must drop on the thread that made it.
    _thread: PhantomData<*const ()>,
}

impl Drop for Grant {
    fn drop(&mut self) {
        let _ = HELD.try_with(|h| h.borrow_mut().take());
    }
}

/// The CPU count of the grant this thread holds: a tool's job count. `None` outside a grant.
pub fn held_jobs() -> Option<u32> {
    HELD.with(|h| h.borrow().as_ref().map(|(_, cpu)| *cpu))
}

/// Takes `class` for checkout `who` in workspace `ws`, waiting its turn. Prints the wait once (`pool: ...`) and the grant when it waited.
pub fn acquire(ws: &Path, who: &Path, class: Class) -> Result<Grant> {
    acquire_with(ws, who, class, POLL, &mut |line| println!("{line}"))
}

fn acquire_with(
    ws: &Path,
    who: &Path,
    class: Class,
    poll: Duration,
    report: &mut dyn FnMut(&str),
) -> Result<Grant> {
    let pool = ws_config::load(ws)?.pool;
    let w = pool.classes[&class];
    fits(&pool, class, w)?;
    let dir = dir(ws);
    let queue = dir.join("queue");
    std::fs::create_dir_all(&queue).with_context(|| format!("creating {}", queue.display()))?;
    check_nesting(&pool, &dir)?;
    let record = holder_record(who, class.name());
    let ticket = Ticket::new(&queue, std::process::id())?;
    let started = Instant::now();
    let mut waited = false;
    loop {
        let (ahead, queued) = scan(&queue, &ticket.name)?;
        // the head of the queue gathers; nobody behind it is served
        if ahead == 0
            && let Some((cpu, files)) = try_grant(&dir, &pool, w, &record)?
        {
            if waited {
                report(&format!(
                    "pool: {} granted cpu {cpu} mem {} after {:.1} s",
                    class.name(),
                    w.mem_gib,
                    started.elapsed().as_secs_f64()
                ));
            }
            drop(ticket);
            HELD.with(|h| *h.borrow_mut() = Some((record, cpu)));
            return Ok(Grant {
                class: class.name(),
                cpu,
                cpu_of: pool.cpu_slots.value,
                mem_gib: w.mem_gib,
                _files: files,
                _thread: PhantomData,
            });
        }
        if !waited {
            waited = true;
            let snap = snapshot(&dir, &pool)?;
            report(&wait_line(class, w, &pool, &snap, (ahead, queued)));
        }
        std::thread::sleep(poll);
    }
}

/// Every slot of the pool without waiting: a quiet window (no leaf build runs while it is held). `Err(line)` names the holders when any slot is taken; the slots taken are released again.
pub fn try_take_all(ws: &Path, who: &Path) -> Result<std::result::Result<Grant, String>> {
    let pool = ws_config::load(ws)?.pool;
    let dir = dir(ws);
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    check_nesting(&pool, &dir)?;
    let record = holder_record(who, "quiet");
    let mut files = Vec::new();
    for path in all_slots(&dir, &pool) {
        match try_take(&path, &record)? {
            Some(f) => files.push(f),
            None => {
                let snap = snapshot(&dir, &pool)?;
                return Ok(Err(format!(
                    "pool: quiet window refused; held: {}",
                    snap.held_list()
                )));
            }
        }
    }
    let cpu = pool.cpu_slots.value;
    HELD.with(|h| *h.borrow_mut() = Some((record, cpu)));
    Ok(Ok(Grant {
        class: "quiet",
        cpu,
        cpu_of: cpu,
        mem_gib: pool.mem_gib.value,
        _files: files,
        _thread: PhantomData,
    }))
}

/// The pool directory of workspace `ws`.
pub fn dir(ws: &Path) -> PathBuf {
    ws.join("cache").join("build-pool")
}

fn slot(dir: &Path, kind: &str, index: u32) -> PathBuf {
    dir.join(format!("{kind}-{index}"))
}

fn all_slots(dir: &Path, pool: &PoolConfig) -> Vec<PathBuf> {
    (1..=pool.cpu_slots.value)
        .map(|i| slot(dir, "cpu", i))
        .chain((1..=pool.mem_gib.value).map(|i| slot(dir, "mem", i)))
        .collect()
}

fn holder_record(who: &Path, class: &str) -> String {
    format!("{} pid {} class {class}", who.display(), std::process::id())
}

/// The pid a holder record names: `<who> pid <pid> class <class>`.
fn record_pid(record: &str) -> Option<u32> {
    record
        .rsplit_once(" class ")?
        .0
        .rsplit_once(" pid ")?
        .1
        .parse()
        .ok()
}

fn fits(pool: &PoolConfig, class: Class, w: ClassWeight) -> Result<()> {
    ensure!(
        w.cpu_min <= pool.cpu_slots.value,
        "pool: class {} needs cpu_min {} but the pool has {} CPU slots ([pool] cpu_slots)",
        class.name(),
        w.cpu_min,
        pool.cpu_slots.value
    );
    ensure!(
        w.mem_gib <= pool.mem_gib.value,
        "pool: class {} reserves {} GiB but the pool holds {} ([pool] mem_gib)",
        class.name(),
        w.mem_gib,
        pool.mem_gib.value
    );
    Ok(())
}

/// Refuses an acquire made by a thread that holds a grant, or by a process with a holder among its ancestors: that waiter would wait for slots its own holder keeps.
fn check_nesting(pool: &PoolConfig, dir: &Path) -> Result<()> {
    if let Some((rec, _)) = HELD.with(|h| h.borrow().clone()) {
        bail!("pool: nested admission refused (held by {rec})");
    }
    let ancestors = ancestors();
    if ancestors.is_empty() {
        return Ok(());
    }
    for path in all_slots(dir, pool) {
        if let Some(rec) = probe(&path)?
            && record_pid(&rec).is_some_and(|pid| ancestors.contains(&pid))
        {
            bail!("pool: nested admission refused (held by {rec})");
        }
    }
    Ok(())
}

/// Parent pids up to init, from `/proc` (Linux).
fn ancestors() -> Vec<u32> {
    let mut out = Vec::new();
    let mut pid = std::process::id();
    while out.len() < MAX_DEPTH {
        let Some(ppid) = std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .ok()
            .and_then(|s| parent_pid(&s))
        else {
            break;
        };
        if ppid <= 1 {
            break;
        }
        out.push(ppid);
        pid = ppid;
    }
    out
}

/// The ppid field of a `/proc/<pid>/stat` line; the command name is in parentheses and may hold spaces or `)`.
fn parent_pid(stat: &str) -> Option<u32> {
    let after = stat.get(stat.rfind(')')? + 2..)?;
    after.split_whitespace().nth(1)?.parse().ok()
}

/// One queue entry. Its file is locked by its waiter for as long as the waiter lives; the lock is taken before the file is visible.
struct Ticket {
    path: PathBuf,
    name: String,
    _lock: File,
}

impl Ticket {
    /// Named by arrival time, pid and a counter, so names sort in arrival order. Written under `tmp-`, locked, then renamed.
    fn new(queue: &Path, pid: u32) -> Result<Ticket> {
        let seq = TICKETS.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let name = format!("{nanos:020}-{pid:010}-{seq:010}");
        let tmp = queue.join(format!("tmp-{name}"));
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&tmp)
            .with_context(|| format!("creating {}", tmp.display()))?;
        match flock(&lock, FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => {}
            Err(e) => return Err(e).with_context(|| format!("flock {}", tmp.display())),
        }
        let path = queue.join(&name);
        std::fs::rename(&tmp, &path).with_context(|| format!("queueing {}", path.display()))?;
        Ok(Ticket {
            path,
            name,
            _lock: lock,
        })
    }
}

impl Drop for Ticket {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// The queue as seen by `me` (its file name): (live tickets ordered before `me`, all live tickets). A ticket whose lock is free has no waiter left: it is removed.
fn scan(queue: &Path, me: &str) -> Result<(usize, usize)> {
    let (mut ahead, mut live) = (0, 0);
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
        match flock(&file, FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => {
                let _ = std::fs::remove_file(&path);
            }
            Err(e) if std::io::Error::from(e).kind() == ErrorKind::WouldBlock => {
                live += 1;
                if name.as_str() < me {
                    ahead += 1;
                }
            }
            Err(e) => return Err(e).with_context(|| format!("flock {}", path.display())),
        }
    }
    Ok((ahead, live))
}

/// Takes one slot without waiting. The holder record is written under the lock; a reader may see it half-written, which costs a garbled wait line, nothing else.
fn try_take(path: &Path, record: &str) -> Result<Option<File>> {
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    match flock(&file, FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => {}
        Err(e) if std::io::Error::from(e).kind() == ErrorKind::WouldBlock => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("flock {}", path.display())),
    }
    file.set_len(0)?;
    file.write_all(record.as_bytes())?;
    Ok(Some(file))
}

/// `Some(holder record)` when a holder has the slot, `None` when it is free. Takes nothing.
fn probe(path: &Path) -> Result<Option<String>> {
    let file = match OpenOptions::new().read(true).open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("opening {}", path.display())),
    };
    match flock(&file, FlockOperation::NonBlockingLockShared) {
        Ok(()) => Ok(None),
        Err(e) if std::io::Error::from(e).kind() == ErrorKind::WouldBlock => Ok(Some(
            std::fs::read_to_string(path)
                .unwrap_or_default()
                .trim()
                .to_owned(),
        )),
        Err(e) => Err(e).with_context(|| format!("flock {}", path.display())),
    }
}

/// The whole request at once, or nothing: `mem_gib` memory slots, then CPU slots up to `cpu_max`, at least `cpu_min`. A partial take is released.
fn try_grant(
    dir: &Path,
    pool: &PoolConfig,
    w: ClassWeight,
    record: &str,
) -> Result<Option<(u32, Vec<File>)>> {
    let mut files = Vec::new();
    let mut mem = 0;
    for i in 1..=pool.mem_gib.value {
        if mem == w.mem_gib {
            break;
        }
        if let Some(f) = try_take(&slot(dir, "mem", i), record)? {
            files.push(f);
            mem += 1;
        }
    }
    if mem < w.mem_gib {
        return Ok(None);
    }
    let mut cpu = 0;
    for i in 1..=pool.cpu_slots.value {
        if cpu == w.cpu_max {
            break;
        }
        if let Some(f) = try_take(&slot(dir, "cpu", i), record)? {
            files.push(f);
            cpu += 1;
        }
    }
    if cpu < w.cpu_min {
        return Ok(None);
    }
    Ok(Some((cpu, files)))
}

/// What the pool holds right now: free slots per kind, and holders by record.
#[derive(Debug, Default)]
struct Snapshot {
    free_cpu: u32,
    free_mem: u32,
    /// holder record -> (CPU slots, memory slots)
    held: BTreeMap<String, (u32, u32)>,
}

impl Snapshot {
    fn held_list(&self) -> String {
        if self.held.is_empty() {
            return "none".to_owned();
        }
        self.held
            .iter()
            .map(|(rec, (cpu, mem))| format!("{rec} cpu {cpu} mem {mem}"))
            .collect::<Vec<_>>()
            .join("; ")
    }
}

fn snapshot(dir: &Path, pool: &PoolConfig) -> Result<Snapshot> {
    let mut s = Snapshot::default();
    for i in 1..=pool.cpu_slots.value {
        match probe(&slot(dir, "cpu", i))? {
            None => s.free_cpu += 1,
            Some(rec) => s.held.entry(rec).or_default().0 += 1,
        }
    }
    for i in 1..=pool.mem_gib.value {
        match probe(&slot(dir, "mem", i))? {
            None => s.free_mem += 1,
            Some(rec) => s.held.entry(rec).or_default().1 += 1,
        }
    }
    Ok(s)
}

fn wait_line(
    class: Class,
    w: ClassWeight,
    pool: &PoolConfig,
    snap: &Snapshot,
    queue: (usize, usize),
) -> String {
    format!(
        "pool: {} wants cpu >={} mem {} GiB, free cpu {}/{} mem {}/{}, queue {} ({} ahead); held: {}",
        class.name(),
        w.cpu_min,
        w.mem_gib,
        snap.free_cpu,
        pool.cpu_slots.value,
        snap.free_mem,
        pool.mem_gib.value,
        queue.1,
        queue.0,
        snap.held_list()
    )
}

/// Per-build-dir lock `<workspace>/cache/build-pool/ws-<id>.lock`. Every process that resolves build dir `id` (`out::out_dir`) holds it shared until it exits; making, deleting and pruning the dir take it exclusive, so a dir is never changed under a build. Freed by the kernel on exit or SIGKILL, like the slots.
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
    /// Upper bound on any wait a test expects to end.
    const WAIT: Duration = Duration::from_secs(10);

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("xtask-pool-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    /// A workspace dir with `workspace.toml` = `toml` (absent when None), so the weights come from the file.
    fn scratch_ws(name: &str, toml: Option<&str>) -> PathBuf {
        let d = scratch(name);
        std::fs::create_dir_all(&d).unwrap();
        if let Some(t) = toml {
            std::fs::write(d.join(ws_config::FILE), t).unwrap();
        }
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

    /// A thread that takes `class` as `who`, reports each wait line and then its grant, and holds the grant until `release` is sent (or dropped).
    struct Contender {
        waits: mpsc::Receiver<String>,
        granted: mpsc::Receiver<String>,
        release: mpsc::Sender<()>,
    }

    fn contender(ws: &Path, who: &'static str, class: Class) -> Contender {
        let (wtx, waits) = mpsc::channel();
        let (gtx, granted) = mpsc::channel();
        let (release, rrx) = mpsc::channel::<()>();
        let ws = ws.to_owned();
        std::thread::spawn(move || {
            let grant = acquire_with(&ws, Path::new(who), class, FAST, &mut |line| {
                let _ = wtx.send(line.to_owned());
            })
            .expect("acquire");
            let _ = gtx.send(format!("cpu {} mem {}", grant.cpu, grant.mem_gib));
            let _ = rrx.recv();
            drop(grant);
        });
        Contender {
            waits,
            granted,
            release,
        }
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

    /// Waiters are granted in arrival order: each one's wait line means its ticket is queued before the next one arrives, and no later waiter is granted before an earlier one released.
    #[test]
    fn waiters_are_served_in_arrival_order() {
        let ws = scratch_ws("fifo", Some("[pool]\ncpu_slots = 1\nmem_gib = 1\n"));
        let holder = acquire_with(&ws, Path::new("holder"), Class::Light, FAST, &mut |_| {
            panic!("holder waited")
        })
        .unwrap();
        let mut queued = Vec::new();
        for who in ["a", "b", "c"] {
            let c = contender(&ws, who, Class::Light);
            let line = c.waits.recv_timeout(WAIT).expect("no wait line");
            assert!(line.contains("wants cpu >=1 mem 0 GiB"), "{line}");
            queued.push(c);
        }
        drop(holder);
        for (i, c) in queued.iter().enumerate() {
            c.granted
                .recv_timeout(WAIT)
                .unwrap_or_else(|_| panic!("waiter {i} was not granted in turn"));
            for later in &queued[i + 1..] {
                assert!(
                    later.granted.try_recv().is_err(),
                    "a later waiter got a slot before waiter {i} released"
                );
            }
            c.release.send(()).unwrap();
        }
    }

    /// Memory admits two full natives (7 GiB each of 20) and not a third; the third's wait line names the memory.
    #[test]
    fn memory_admits_two_full_natives_and_the_third_waits() {
        // cpu_max 8 so that CPU is not the limit: the shipped weights give one full native the whole pool
        let ws = scratch_ws(
            "mem-two",
            Some(
                "[pool]\ncpu_slots = 32\nmem_gib = 20\n[pool.native-full]\ncpu_min = 8\ncpu_max = 8\nmem_gib = 7\n",
            ),
        );
        let a = contender(&ws, "a", Class::NativeFull);
        assert_eq!(a.granted.recv_timeout(WAIT).unwrap(), "cpu 8 mem 7");
        let b = contender(&ws, "b", Class::NativeFull);
        assert_eq!(b.granted.recv_timeout(WAIT).unwrap(), "cpu 8 mem 7");
        let c = contender(&ws, "c", Class::NativeFull);
        let line = c
            .waits
            .recv_timeout(WAIT)
            .expect("the third native did not wait");
        assert!(
            line.contains("native-full wants cpu >=8 mem 7 GiB, free cpu 16/32 mem 6/20"),
            "{line}"
        );
        assert_eq!(line.matches("cpu 8 mem 7").count(), 2, "{line}");
        assert!(
            c.granted.recv_timeout(STILL_WAITING).is_err(),
            "the third native ran past the memory"
        );
        a.release.send(()).unwrap();
        assert_eq!(c.granted.recv_timeout(WAIT).unwrap(), "cpu 8 mem 7");
        b.release.send(()).unwrap();
        c.release.send(()).unwrap();
    }

    /// The shipped weights (cpu 8..16, mem 7): one full native takes all 16 CPU slots, so a second waits on CPU although memory fits two.
    #[test]
    fn elastic_cpu_lets_one_full_native_take_the_pool() {
        let ws = scratch_ws("elastic", Some("[pool]\ncpu_slots = 16\nmem_gib = 20\n"));
        let a = contender(&ws, "a", Class::NativeFull);
        assert_eq!(a.granted.recv_timeout(WAIT).unwrap(), "cpu 16 mem 7");
        let b = contender(&ws, "b", Class::NativeFull);
        let line = b
            .waits
            .recv_timeout(WAIT)
            .expect("the second native did not wait");
        assert!(line.contains("free cpu 0/16 mem 13/20"), "{line}");
        a.release.send(()).unwrap();
        assert!(
            b.granted.recv_timeout(WAIT).is_ok(),
            "second native never ran"
        );
        b.release.send(()).unwrap();
    }

    /// Each class takes its weight: cargo gets every free slot up to its max (16) and 6 GiB; harness is capped at its max of 6 CPU slots.
    #[test]
    fn class_weights_size_the_grant() {
        let ws = scratch_ws("weights", Some("[pool]\ncpu_slots = 16\nmem_gib = 20\n"));
        let cargo = contender(&ws, "cargo", Class::Cargo);
        assert_eq!(cargo.granted.recv_timeout(WAIT).unwrap(), "cpu 16 mem 6");
        cargo.release.send(()).unwrap();
        let harness = contender(&ws, "harness", Class::Harness);
        assert_eq!(harness.granted.recv_timeout(WAIT).unwrap(), "cpu 6 mem 2");
        harness.release.send(()).unwrap();
    }

    #[test]
    fn same_thread_acquire_under_a_grant_is_refused() {
        let ws = scratch_ws("nest-thread", Some(CONFIG_ONE_CPU));
        let who = Path::new("nest");
        let grant = acquire_with(&ws, who, Class::Light, FAST, &mut |_| panic!("waited")).unwrap();
        assert_eq!(held_jobs(), Some(1));
        let e = acquire_with(&ws, who, Class::Light, FAST, &mut |_| panic!("waited"))
            .err()
            .expect("a nested acquire must be refused");
        assert!(
            format!("{e:#}").contains("nested admission refused"),
            "{e:#}"
        );
        drop(grant);
        assert_eq!(held_jobs(), None);
        acquire_with(&ws, who, Class::Light, FAST, &mut |_| panic!("waited"))
            .expect("admitted again after the release");
    }

    const CONFIG_ONE_CPU: &str = "[pool]\ncpu_slots = 1\nmem_gib = 20\n";

    /// Holder process: takes a grant, then runs `helper_nested` as its child while holding it. The child's acquire must be refused.
    #[test]
    #[ignore = "run by ancestor_holder_refuses_a_nested_acquire"]
    fn helper_holder() {
        let ws = PathBuf::from(std::env::var_os("POOL_TEST_WS").expect("set by the test"));
        let _grant = acquire_with(&ws, Path::new("holder"), Class::Light, FAST, &mut |_| {
            panic!("holder waited")
        })
        .unwrap();
        let status = Command::new(std::env::current_exe().unwrap())
            .args([
                "--ignored",
                "--exact",
                "build_pool::tests::helper_nested",
                "--nocapture",
            ])
            .env("POOL_TEST_WS", &ws)
            .stdout(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "the nested acquire was not refused");
    }

    #[test]
    #[ignore = "run by helper_holder"]
    fn helper_nested() {
        let ws = PathBuf::from(std::env::var_os("POOL_TEST_WS").expect("set by the test"));
        let e = acquire_with(&ws, Path::new("nested"), Class::Light, FAST, &mut |_| {
            panic!("nested waited")
        })
        .err()
        .expect("a child of the holder must be refused");
        assert!(
            format!("{e:#}").contains("nested admission refused"),
            "{e:#}"
        );
    }

    #[test]
    fn ancestor_holder_refuses_a_nested_acquire() {
        let ws = scratch_ws("nest-ancestor", Some(CONFIG_ONE_CPU));
        let status = Command::new(std::env::current_exe().unwrap())
            .args([
                "--ignored",
                "--exact",
                "build_pool::tests::helper_holder",
                "--nocapture",
            ])
            .env("POOL_TEST_WS", &ws)
            .stdout(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "helper_holder failed");
    }

    /// The quiet window takes every slot, and is refused (naming the holder) while one is held.
    #[test]
    fn quiet_window_needs_every_slot() {
        let ws = scratch_ws("quiet", Some("[pool]\ncpu_slots = 2\nmem_gib = 2\n"));
        let who = Path::new("quiet-test");
        let quiet = try_take_all(&ws, who)
            .unwrap()
            .expect("an idle pool is quiet");
        assert_eq!((quiet.cpu, quiet.mem_gib), (2, 2));
        drop(quiet);
        let c = contender(&ws, "held", Class::Light);
        assert_eq!(c.granted.recv_timeout(WAIT).unwrap(), "cpu 1 mem 0");
        let refused = try_take_all(&ws, who)
            .unwrap()
            .err()
            .expect("a held slot must refuse the quiet window");
        assert!(
            refused.contains("quiet window refused") && refused.contains("held pid"),
            "{refused}"
        );
        c.release.send(()).unwrap();
    }

    #[test]
    fn class_larger_than_the_pool_is_refused_not_waited_for() {
        let ws = scratch_ws("fits", Some("[pool]\ncpu_slots = 4\nmem_gib = 20\n"));
        let e = acquire_with(&ws, Path::new("big"), Class::NativeFull, FAST, &mut |_| {
            panic!("waited")
        })
        .err()
        .expect("must refuse");
        assert!(
            format!("{e:#}").contains("needs cpu_min 8 but the pool has 4"),
            "{e:#}"
        );
    }

    #[test]
    fn parent_pid_reads_the_field_after_the_command_name() {
        assert_eq!(parent_pid("123 (a) b) S 45 0 0"), Some(45));
        assert_eq!(parent_pid("7 (xtask) R 1 7 7"), Some(1));
        assert_eq!(record_pid("/ws pid 42 class native-full"), Some(42));
        assert_eq!(
            record_pid("/ws/with pid 9 in it pid 42 class cargo"),
            Some(42)
        );
    }

    /// A holder killed with SIGKILL frees its slot: the lock goes with its file. The holder is flock(1) (util-linux), another process.
    #[test]
    fn killed_holder_frees_its_slot() {
        let dir = scratch("killed");
        std::fs::create_dir_all(&dir).unwrap();
        let cpu1 = slot(&dir, "cpu", 1);
        // -o: the lock fd is not passed to sleep, so flock(1) alone holds it
        let mut child = Command::new("flock")
            .arg("-o")
            .arg(&cpu1)
            .args(["sleep", "60"])
            .stdout(Stdio::null())
            // the orphaned sleep must not hold the test binary's stderr open (a piped `cargo test` would wait 60 s)
            .stderr(Stdio::null())
            .spawn()
            .expect("flock(1) from util-linux");
        // wait until the child holds it
        let held = (0..250).any(|_| {
            std::thread::sleep(FAST);
            try_take(&cpu1, "probe").unwrap().is_none()
        });
        assert!(held, "flock(1) never took the slot");
        assert!(try_take(&cpu1, "probe").unwrap().is_none());
        child.kill().unwrap();
        child.wait().unwrap();
        let freed = (0..250).any(|_| {
            std::thread::sleep(FAST);
            try_take(&cpu1, "after").unwrap().is_some()
        });
        assert!(freed, "the killed holder's slot stayed held");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
