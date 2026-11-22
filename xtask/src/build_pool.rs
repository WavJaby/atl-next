// SPDX-License-Identifier: MIT
//! Machine-wide admission: a weighted pool of CPU slots and memory (GiB). Heavy work holds a [`Grant`] around one leaf (a ninja build, a cargo run) and releases it after.
//! A slot is an exclusive `flock` on `<workspace>/cache/build-pool/cpu-<i>` or `mem-<i>`: the kernel drops it when the holder's file closes, so exit, error, panic and SIGKILL all free it -- no stale-lock cleanup exists or is needed.
//! Waiters queue as tickets in `cache/build-pool/queue/`, served in (tier, arrival) order ([`Tier`]: the maintainer's main checkout, then land, then agents), so a large request is not starved by small ones behind it; a ticket waiting past `starve_after_s` joins the top tier at its arrival. No grant is preempted: priority decides only who is admitted next.
//! Light work takes a free CPU slot at once when the queue head could not use it (`bypass_allowed`); otherwise it queues like heavy work.
//! Reserved slot (maintainer 2026-11-18, option B): one CPU slot beyond `cpu_slots` plus the `light` class's memory carved out of `mem_gib`, taken only by interactive-tier light work ([`try_reserved`]), so the maintainer's play launch never waits behind agents' full natives. Its grant prints `granted the reserved slot` and counts in [`RESERVED_GRANTS`].
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

use crate::flock::{Flock, Mode};
use anyhow::{Context, Result, bail, ensure};

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
    /// a runtime gate whose tree peaks above the `harness` reserve (check_gates.rs `Row::class`: the `app` gate)
    HarnessHeavy,
    /// static checks: one CPU slot; bypasses the queue only when the head could not use it (`bypass_allowed`). Held while a check reads the checkouts' git dirs; the migrate quiet window (git_dirs.rs `migrate`) refuses on that hold.
    Light,
}

impl Class {
    pub const ALL: [Class; 6] = [
        Class::NativeFull,
        Class::NativeIncr,
        Class::Cargo,
        Class::Harness,
        Class::HarnessHeavy,
        Class::Light,
    ];

    /// The name in `workspace.toml` (`[pool.<name>]`) and in every log line.
    pub fn name(self) -> &'static str {
        match self {
            Class::NativeFull => "native-full",
            Class::NativeIncr => "native-incr",
            Class::Cargo => "cargo",
            Class::Harness => "harness",
            Class::HarnessHeavy => "harness-heavy",
            Class::Light => "light",
        }
    }

    pub fn from_name(name: &str) -> Option<Class> {
        Class::ALL.into_iter().find(|c| c.name() == name)
    }

    /// Default weight on a pool of `cpu_slots`; each value says what it is based on. Overridden per machine by `[pool.<name>]`.
    pub fn default_weight(self, cpu_slots: u32) -> ClassWeight {
        // half the pool, fixed: two full natives run side by side (memory fits two at 7 GiB); a cold build (25-40 min) never holds the whole pool while near-no-op natives queue behind it
        let half = (cpu_slots / 2).max(1);
        // cpu_slots - 1: one slot stays free, so a queued light check is never stuck behind a whole-pool grant (`may_bypass`)
        let all_but_one = cpu_slots.saturating_sub(1).max(1);
        // elastic classes start at 4 slots, or the whole elastic range on a pool smaller than that
        let elastic_min = 4.min(all_but_one);
        match self {
            // ninja on a cold graph: job RSS max 7.0 GB (2026-11-16, filter-data-g1)
            Class::NativeFull => ClassWeight {
                cpu_min: half,
                cpu_max: half,
                mem_gib: 7,
            },
            // ninja on a warm graph: RSS max 1.8 GB, rounded up
            Class::NativeIncr => ClassWeight {
                cpu_min: elastic_min,
                cpu_max: all_but_one,
                mem_gib: 2,
            },
            // one cargo run: check's RSS max 5.4 GB over the whole check (an upper bound per step), rounded up
            Class::Cargo => ClassWeight {
                cpu_min: elastic_min,
                cpu_max: all_but_one,
                mem_gib: 6,
            },
            // a runtime gate: tree VmRSS peak (check_gates.rs `peak=`, sampled lower bound) <= 1.83 GiB over every row but `app` (webview-overlay 1830 MiB, window rows ~1 GiB, atld rows < 0.8 GiB; 2026-11-22, land-speed S0), rounded up
            Class::Harness => ClassWeight {
                cpu_min: 4,
                cpu_max: 6,
                mem_gib: 2,
            },
            // the `app` gate: tree VmRSS peak 3976 MiB (same sample), rounded up; CPU as `harness`
            Class::HarnessHeavy => ClassWeight {
                cpu_min: 4,
                cpu_max: 6,
                mem_gib: 4,
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
    /// Time spent queued before the grant: zero when the slots were free (`check times` wait column).
    pub waited: Duration,
    /// The slot files: their locks are the reservation, released when the grant drops.
    _files: Vec<Flock>,
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

/// Admission tier of a queue ticket: served in (tier, arrival) order. Like a scheduler's nice level, but at admission: the pool admits by slots, so priority decides who is admitted next, never what a running grant gets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Tier {
    /// The maintainer at the main checkout (a game launch, a native the maintainer waits on).
    Interactive,
    /// The land worktree: its verify gates main for everyone.
    Land,
    /// Every other worktree: agents' builds and checks.
    Agent,
}

impl Tier {
    /// Derived from the checkout, no flag or environment: the main checkout is the maintainer's (agents never build there, project rule), the land worktree is land's, every other checkout an agent's. A path that is no checkout (tests, a broken git dir) is an agent's: no tier is gained by failing.
    pub fn of(who: &Path) -> Tier {
        match crate::out::checkout_id(who).as_deref() {
            Ok(crate::out::MAIN_ID) => Tier::Interactive,
            Ok(crate::out::LAND_ID) => Tier::Land,
            _ => Tier::Agent,
        }
    }

    /// The ticket name's prefix: names sort as (tier, arrival) for every reader. An agent's name is the pre-tier format (`<nanos:020>-...`, today `01...`) unchanged, so agent tickets of xtask binaries older than tiers interleave with new ones by arrival; `0-` and `0.` sort before any `01...`. ! Never give agents a prefix that sorts after digits: old binaries' later tickets would then pass every new agent ticket until promotion.
    fn prefix(self) -> &'static str {
        match self {
            Tier::Interactive => "0-",
            Tier::Land => "0.",
            Tier::Agent => "",
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Tier::Interactive => "interactive",
            Tier::Land => "land",
            Tier::Agent => "agent",
        }
    }
}

/// Grants admitted ahead of an earlier-arrived live ticket (by tier or by promotion), and tickets promoted for waiting past `starve_after_s`, in this process: the counters the log lines carry.
pub static ADMITTED_AHEAD: AtomicU64 = AtomicU64::new(0);
pub static PROMOTED: AtomicU64 = AtomicU64::new(0);
/// Grants of the reserved interactive slot in this process ([`try_reserved`]).
pub static RESERVED_GRANTS: AtomicU64 = AtomicU64::new(0);

/// Takes `class` for checkout `who` in workspace `ws`, waiting its turn ([`Tier::of`] `who`). Prints the wait once (`pool: ...`) and the grant when it waited.
pub fn acquire(ws: &Path, who: &Path, class: Class) -> Result<Grant> {
    acquire_with(ws, who, class, Tier::of(who), POLL, &mut |line| {
        println!("{line}")
    })
}

fn acquire_with(
    ws: &Path,
    who: &Path,
    class: Class,
    tier: Tier,
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
    // heavy work queues at once; light queues only when it would take a slot the head needs (`bypass_allowed`)
    let mut ticket = match class {
        Class::Light => None,
        _ => Some(Ticket::new(&queue, std::process::id(), class, tier)?),
    };
    let started = Instant::now();
    let starve_after = Duration::from_secs(pool.starve_after_s.value.into());
    let mut waited = false;
    let grant = |cpu: u32, mem_gib: u32, waited: Duration, files: Vec<Flock>, record: String| {
        HELD.with(|h| *h.borrow_mut() = Some((record, cpu)));
        Grant {
            class: class.name(),
            cpu,
            cpu_of: pool.cpu_slots.value,
            mem_gib,
            waited,
            _files: files,
            _thread: PhantomData,
        }
    };
    loop {
        // the reserved slot first: it exists for this request, and taking it leaves the shared slots to the queue
        if class == Class::Light
            && tier == Tier::Interactive
            && let Some(files) = try_reserved(&dir, &pool, &record)?
        {
            let n = RESERVED_GRANTS.fetch_add(1, Ordering::Relaxed) + 1;
            let took = started.elapsed();
            report(&format!(
                "pool: light tier interactive granted the reserved slot cpu 1 mem {} after {:.1} s (reserved_grants={n})",
                w.mem_gib,
                took.as_secs_f64()
            ));
            drop(ticket);
            let waited = if waited { took } else { Duration::ZERO };
            return Ok(grant(1, w.mem_gib, waited, files, record));
        }
        // starvation bound: a ticket waiting past it joins the top tier at its arrival, so later interactive tickets queue behind it
        if let Some(t) = &mut ticket
            && t.tier != Tier::Interactive
            && started.elapsed() >= starve_after
        {
            t.promote(&queue)?;
            PROMOTED.fetch_add(1, Ordering::Relaxed);
            report(&format!(
                "pool: {} ticket promoted from tier {} after {:.1} s (starve_after_s {})",
                class.name(),
                tier.name(),
                started.elapsed().as_secs_f64(),
                pool.starve_after_s.value
            ));
        }
        let tickets = live_tickets(&queue)?;
        let queue_state = if let Some(t) = &ticket {
            Some(queue_position(&tickets, &t.name))
        } else if bypass_allowed(&dir, &pool, &tickets, report)? {
            None
        } else {
            ticket = Some(Ticket::new(&queue, std::process::id(), class, tier)?);
            continue;
        };
        // the head of the queue gathers; nobody behind it is served
        if queue_state.is_none_or(|(ahead, _)| ahead == 0)
            && let Some((cpu, files)) = try_grant(&dir, &pool, w, &record)?
        {
            if let Some(t) = &ticket {
                let passed = passed_earlier(&tickets, &t.name);
                if passed > 0 {
                    ADMITTED_AHEAD.fetch_add(1, Ordering::Relaxed);
                    report(&format!(
                        "pool: {} tier {} admitted ahead of {passed} earlier ticket(s)",
                        class.name(),
                        t.tier.name()
                    ));
                }
            }
            let waited = if waited {
                let took = started.elapsed();
                report(&format!(
                    "pool: {} granted cpu {cpu} mem {} after {:.1} s",
                    class.name(),
                    w.mem_gib,
                    took.as_secs_f64()
                ));
                took
            } else {
                Duration::ZERO
            };
            drop(ticket);
            return Ok(grant(cpu, w.mem_gib, waited, files, record));
        }
        if !waited {
            waited = true;
            let snap = snapshot(&dir, &pool)?;
            report(&wait_line(class, w, &pool, &snap, queue_state));
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
        waited: Duration::ZERO,
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

/// Every slot file, the reserved ones included (nesting check, quiet window).
fn all_slots(dir: &Path, pool: &PoolConfig) -> Vec<PathBuf> {
    (1..=pool.cpu_slots.value)
        .map(|i| slot(dir, "cpu", i))
        .chain((1..=shared_mem(pool)).map(|i| slot(dir, "mem", i)))
        .chain(reserved_slots(dir, pool))
        .collect()
}

/// The reserved interactive slot's files: one CPU slot beyond `cpu_slots`, and the `light` class's `mem_gib` memory slots, carved out of `mem_gib` (the memory budget is the WSL cap, so it is not raised).
fn reserved_slots(dir: &Path, pool: &PoolConfig) -> Vec<PathBuf> {
    std::iter::once(slot(dir, "cpu-interactive", 1))
        .chain((1..=reserved_mem(pool)).map(|i| slot(dir, "mem-interactive", i)))
        .collect()
}

fn reserved_mem(pool: &PoolConfig) -> u32 {
    pool.classes[&Class::Light].mem_gib
}

/// Memory slots every class shares: `mem_gib` less the reserved slot's.
fn shared_mem(pool: &PoolConfig) -> u32 {
    pool.mem_gib.value.saturating_sub(reserved_mem(pool))
}

/// Every reserved file at once, or none; only interactive light asks (`acquire_with`).
fn try_reserved(dir: &Path, pool: &PoolConfig, record: &str) -> Result<Option<Vec<Flock>>> {
    let mut files = Vec::new();
    for path in reserved_slots(dir, pool) {
        match try_take(&path, record)? {
            Some(f) => files.push(f),
            None => return Ok(None),
        }
    }
    Ok(Some(files))
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
        w.mem_gib <= shared_mem(pool),
        "pool: class {} reserves {} GiB but the pool shares {} ([pool] mem_gib less the reserved slot's light mem_gib)",
        class.name(),
        w.mem_gib,
        shared_mem(pool)
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
    tier: Tier,
    _lock: Flock,
}

impl Ticket {
    /// Named by tier, arrival time, pid and a counter, so names sort in (tier, arrival) order. Written under `tmp-` with the class name, locked, then renamed.
    fn new(queue: &Path, pid: u32, class: Class, tier: Tier) -> Result<Ticket> {
        let seq = TICKETS.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let name = format!("{}{nanos:020}-{pid:010}-{seq:010}", tier.prefix());
        let tmp = queue.join(format!("tmp-{name}"));
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&tmp)
            .with_context(|| format!("creating {}", tmp.display()))?;
        let mut lock = Flock::try_take(lock, Mode::Exclusive)
            .with_context(|| format!("flock {}", tmp.display()))?
            .with_context(|| format!("{} is held: a fresh ticket", tmp.display()))?;
        lock.file()
            .write_all(class.name().as_bytes())
            .with_context(|| format!("writing {}", tmp.display()))?;
        let path = queue.join(&name);
        std::fs::rename(&tmp, &path).with_context(|| format!("queueing {}", path.display()))?;
        Ok(Ticket {
            path,
            name,
            tier,
            _lock: lock,
        })
    }

    /// Moves the ticket into the top tier, keeping its arrival: a rename, so the waiter's lock (on the open file) stays and readers see one name or the other.
    fn promote(&mut self, queue: &Path) -> Result<()> {
        let name = format!("{}{}", Tier::Interactive.prefix(), arrival(&self.name));
        let path = queue.join(&name);
        std::fs::rename(&self.path, &path)
            .with_context(|| format!("promoting {}", self.path.display()))?;
        self.path = path;
        self.name = name;
        self.tier = Tier::Interactive;
        Ok(())
    }
}

/// A ticket name without its tier prefix: `<nanos>-<pid>-<seq>`, the arrival order. An agent's name is all arrival.
fn arrival(name: &str) -> &str {
    [Tier::Interactive, Tier::Land]
        .into_iter()
        .find_map(|t| name.strip_prefix(t.prefix()))
        .unwrap_or(name)
}

/// Live tickets that arrived before `me` but sort after it: the ones a grant to `me` passes.
fn passed_earlier(tickets: &[(String, Option<Class>)], me: &str) -> usize {
    tickets
        .iter()
        .filter(|(name, _)| name.as_str() > me && arrival(name) < arrival(me))
        .count()
}

impl Drop for Ticket {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// The live tickets, oldest first, each with the class its waiter wrote (`None` when unreadable or unknown). A ticket whose lock is free has no waiter left: it is removed.
fn live_tickets(queue: &Path) -> Result<Vec<(String, Option<Class>)>> {
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
                let class = std::fs::read_to_string(&path)
                    .ok()
                    .and_then(|text| Class::from_name(text.trim()));
                live.push((name, class));
            }
        }
    }
    // names sort in arrival order: fixed-width time, then pid, then counter
    live.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(live)
}

/// (live tickets ordered before `me`, all live tickets); `me` is a ticket's file name.
fn queue_position(tickets: &[(String, Option<Class>)], me: &str) -> (usize, usize) {
    let ahead = tickets
        .iter()
        .filter(|(name, _)| name.as_str() < me)
        .count();
    (ahead, tickets.len())
}

/// Takes one slot without waiting. The holder record is written under the lock; a reader may see it half-written, which costs a garbled wait line, nothing else.
fn try_take(path: &Path, record: &str) -> Result<Option<Flock>> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    let Some(mut held) = Flock::try_take(file, Mode::Exclusive)
        .with_context(|| format!("flock {}", path.display()))?
    else {
        return Ok(None);
    };
    held.file().set_len(0)?;
    held.file().write_all(record.as_bytes())?;
    Ok(Some(held))
}

/// `Some(holder record)` when a holder has the slot, `None` when it is free. Takes nothing.
fn probe(path: &Path) -> Result<Option<String>> {
    let file = match OpenOptions::new().read(true).open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("opening {}", path.display())),
    };
    match Flock::try_take(file, Mode::Shared)
        .with_context(|| format!("flock {}", path.display()))?
    {
        Some(_free) => Ok(None),
        None => Ok(Some(
            std::fs::read_to_string(path)
                .unwrap_or_default()
                .trim()
                .to_owned(),
        )),
    }
}

/// The whole request at once, or nothing: `mem_gib` memory slots, then CPU slots up to `cpu_max`, at least `cpu_min`. A partial take is released.
fn try_grant(
    dir: &Path,
    pool: &PoolConfig,
    w: ClassWeight,
    record: &str,
) -> Result<Option<(u32, Vec<Flock>)>> {
    let mut files = Vec::new();
    let mut mem = 0;
    for i in 1..=shared_mem(pool) {
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

/// Light may take a free CPU slot past the queue head only when the head could not use it: its memory is short, or it still keeps `cpu_min` free after the take.
fn may_bypass(head: ClassWeight, snap: &Snapshot) -> bool {
    head.mem_gib > snap.free_mem || snap.free_cpu > head.cpu_min
}

/// Times this process queued light behind a head ticket whose class it could not read (`bypass_allowed`); printed in that line.
static UNKNOWN_HEAD_FIFO: AtomicU64 = AtomicU64::new(0);

/// Light may bypass the queue with no queue, or when `may_bypass` holds against the head's class.
/// An unknown head class makes light queue (FIFO is always safe), logged and counted. Not unrepresentable: tickets are files shared by every checkout's xtask, so a head written by another version may name a class this one lacks.
fn bypass_allowed(
    dir: &Path,
    pool: &PoolConfig,
    tickets: &[(String, Option<Class>)],
    report: &mut dyn FnMut(&str),
) -> Result<bool> {
    let Some((name, head)) = tickets.first() else {
        return Ok(true);
    };
    let Some(class) = head else {
        let n = UNKNOWN_HEAD_FIFO.fetch_add(1, Ordering::Relaxed) + 1;
        report(&format!(
            "pool: light queues FIFO: head ticket {name} has a class this xtask cannot read (unknown_head_fifo={n})"
        ));
        return Ok(false);
    };
    Ok(may_bypass(pool.classes[class], &snapshot(dir, pool)?))
}

fn snapshot(dir: &Path, pool: &PoolConfig) -> Result<Snapshot> {
    let mut s = Snapshot::default();
    for i in 1..=pool.cpu_slots.value {
        match probe(&slot(dir, "cpu", i))? {
            None => s.free_cpu += 1,
            Some(rec) => s.held.entry(rec).or_default().0 += 1,
        }
    }
    for i in 1..=shared_mem(pool) {
        match probe(&slot(dir, "mem", i))? {
            None => s.free_mem += 1,
            Some(rec) => s.held.entry(rec).or_default().1 += 1,
        }
    }
    // the reserved slot's holder is listed (`reserved`), its files never counted free: no shared request can use them
    if let Some(rec) = probe(&slot(dir, "cpu-interactive", 1))? {
        s.held.entry(format!("{rec} reserved")).or_default().0 += 1;
    }
    for i in 1..=reserved_mem(pool) {
        if let Some(rec) = probe(&slot(dir, "mem-interactive", i))? {
            s.held.entry(format!("{rec} reserved")).or_default().1 += 1;
        }
    }
    Ok(s)
}

fn wait_line(
    class: Class,
    w: ClassWeight,
    pool: &PoolConfig,
    snap: &Snapshot,
    queue: Option<(usize, usize)>,
) -> String {
    let queue = match queue {
        Some((ahead, queued)) => format!("queue {queued} ({ahead} ahead)"),
        None => "queue bypassed".to_owned(),
    };
    format!(
        "pool: {} wants cpu >={} mem {} GiB, free cpu {}/{} mem {}/{}, {queue}; held: {}",
        class.name(),
        w.cpu_min,
        w.mem_gib,
        snap.free_cpu,
        pool.cpu_slots.value,
        snap.free_mem,
        shared_mem(pool),
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
    _held: Flock,
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
    let mode = if shared {
        Mode::Shared
    } else {
        Mode::Exclusive
    };
    // a probe that fails leaves the lock untaken, so the file is reopened for the wait
    if let Some(held) =
        Flock::try_take(file, mode).with_context(|| format!("flock {}", p.display()))?
    {
        return Ok(DirLock { _held: held });
    }
    on_wait();
    let (p, file) = open_dir_lock(ws, id)?;
    let held = Flock::take(file, mode).with_context(|| format!("flock {}", p.display()))?;
    Ok(DirLock { _held: held })
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
    Ok(Flock::try_take(file, Mode::Exclusive)
        .with_context(|| format!("flock {}", p.display()))?
        .map(|held| DirLock { _held: held }))
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

    /// A thread that takes `class` as `who`, reports each wait line and then its grant, and holds the grant until `release` is sent (or dropped).
    struct Contender {
        waits: mpsc::Receiver<String>,
        granted: mpsc::Receiver<String>,
        release: mpsc::Sender<()>,
    }

    fn contender(ws: &Path, who: &'static str, class: Class) -> Contender {
        contender_at(ws, who, class, Tier::Agent)
    }

    fn contender_at(ws: &Path, who: &'static str, class: Class, tier: Tier) -> Contender {
        let (wtx, waits) = mpsc::channel();
        let (gtx, granted) = mpsc::channel();
        let (release, rrx) = mpsc::channel::<()>();
        let ws = ws.to_owned();
        std::thread::spawn(move || {
            let grant = acquire_with(&ws, Path::new(who), class, tier, FAST, &mut |line| {
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
            try_lock_dir(&dir, "a").unwrap().is_some(),
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

    /// Heavy waiters are granted in arrival order: each one's wait line means its ticket is queued before the next one arrives, and no later waiter is granted before an earlier one released. Cargo stands in for the heavy classes; light queues only when it would take the head's slot (the next tests).
    #[test]
    fn waiters_are_served_in_arrival_order() {
        let ws = scratch_ws(
            "fifo",
            Some(
                "[pool]\ncpu_slots = 1\nmem_gib = 1\n[pool.cargo]\ncpu_min = 1\ncpu_max = 1\nmem_gib = 0\n",
            ),
        );
        let holder = acquire_with(
            &ws,
            Path::new("holder"),
            Class::Cargo,
            Tier::Agent,
            FAST,
            &mut |_| panic!("holder waited"),
        )
        .unwrap();
        let mut queued = Vec::new();
        for who in ["a", "b", "c"] {
            let c = contender(&ws, who, Class::Cargo);
            let line = c.waits.recv_timeout(WAIT).expect("no wait line");
            assert!(line.contains("cargo wants cpu >=1 mem 0 GiB"), "{line}");
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

    /// Light takes a free CPU slot while a heavy request queues ahead of it: the heavy head waits for memory the holder keeps, and light gets the slot the head cannot use. With no slot free, light waits (queue bypassed) and runs when one frees, still ahead of the heavy head.
    #[test]
    fn light_takes_a_free_slot_past_a_queued_heavy_request() {
        let ws = scratch_ws(
            "light-bypass",
            Some(
                "[pool]\ncpu_slots = 2\nmem_gib = 1\n[pool.cargo]\ncpu_min = 1\ncpu_max = 1\nmem_gib = 1\n",
            ),
        );
        let holder = contender(&ws, "holder", Class::Cargo);
        assert_eq!(holder.granted.recv_timeout(WAIT).unwrap(), "cpu 1 mem 1");
        let heavy = contender(&ws, "heavy", Class::Cargo);
        let line = heavy
            .waits
            .recv_timeout(WAIT)
            .expect("the heavy request did not wait");
        assert!(line.contains("cargo wants cpu >=1 mem 1 GiB"), "{line}");
        // a CPU slot is free and the queued heavy request cannot use it (no memory)
        let light = contender(&ws, "light", Class::Light);
        assert_eq!(light.granted.recv_timeout(WAIT).unwrap(), "cpu 1 mem 0");
        assert!(
            light.waits.try_recv().is_err(),
            "light waited with a slot free"
        );
        // no slot free now: light waits, its line names no queue position
        let light2 = contender(&ws, "light2", Class::Light);
        let line = light2
            .waits
            .recv_timeout(WAIT)
            .expect("light2 did not wait with no slot free");
        assert!(
            line.contains("light wants cpu >=1 mem 0 GiB") && line.contains("queue bypassed"),
            "{line}"
        );
        light.release.send(()).unwrap();
        assert_eq!(light2.granted.recv_timeout(WAIT).unwrap(), "cpu 1 mem 0");
        light2.release.send(()).unwrap();
        holder.release.send(()).unwrap();
        assert_eq!(heavy.granted.recv_timeout(WAIT).unwrap(), "cpu 1 mem 1");
        heavy.release.send(()).unwrap();
    }

    /// Pool of 1 CPU + 2 GiB, light weighted 1 GiB: an agent's cargo holds the only shared CPU and the 1 shared GiB, an agent cargo queues. Interactive light is still granted at once, from the reserved slot (cpu 1 + light's 1 GiB); a second interactive light finds it held and waits.
    #[test]
    fn interactive_light_takes_the_reserved_slot_on_a_full_pool() {
        let ws = scratch_ws(
            "reserved-fires",
            Some(
                "[pool]\ncpu_slots = 1\nmem_gib = 2\n[pool.cargo]\ncpu_min = 1\ncpu_max = 1\nmem_gib = 1\n[pool.light]\nmem_gib = 1\n",
            ),
        );
        let holder = contender(&ws, "holder", Class::Cargo);
        assert_eq!(holder.granted.recv_timeout(WAIT).unwrap(), "cpu 1 mem 1");
        let heavy = contender(&ws, "heavy", Class::Cargo);
        heavy.waits.recv_timeout(WAIT).expect("heavy did not queue");
        let before = RESERVED_GRANTS.load(Ordering::Relaxed);
        let play = contender_at(&ws, "play", Class::Light, Tier::Interactive);
        assert_eq!(play.granted.recv_timeout(WAIT).unwrap(), "cpu 1 mem 1");
        let line = play.waits.recv_timeout(WAIT).expect("no slot-kind line");
        assert!(
            line.contains("granted the reserved slot cpu 1 mem 1"),
            "{line}"
        );
        assert!(RESERVED_GRANTS.load(Ordering::Relaxed) > before);
        let play2 = contender_at(&ws, "play2", Class::Light, Tier::Interactive);
        let line = play2.waits.recv_timeout(WAIT).expect("play2 did not wait");
        assert!(
            line.contains("light wants cpu >=1") && line.contains("play pid"),
            "{line}"
        );
        play.release.send(()).unwrap();
        assert_eq!(play2.granted.recv_timeout(WAIT).unwrap(), "cpu 1 mem 1");
        for c in [play2, holder, heavy] {
            c.release.send(()).unwrap();
        }
    }

    /// The reserved slot is free and the shared CPU held: agent-tier light and interactive-tier cargo both wait (neither takes the reserved slot), and both run once the holder releases.
    #[test]
    fn only_interactive_light_may_take_the_reserved_slot() {
        let ws = scratch_ws(
            "reserved-refused",
            Some(
                "[pool]\ncpu_slots = 1\nmem_gib = 1\n[pool.cargo]\ncpu_min = 1\ncpu_max = 1\nmem_gib = 0\n",
            ),
        );
        let holder = contender(&ws, "holder", Class::Cargo);
        assert_eq!(holder.granted.recv_timeout(WAIT).unwrap(), "cpu 1 mem 0");
        let agent_light = contender(&ws, "agent-light", Class::Light);
        let line = agent_light
            .waits
            .recv_timeout(WAIT)
            .expect("agent light did not wait");
        assert!(line.contains("light wants"), "{line}");
        let owner_cargo = contender_at(&ws, "owner-cargo", Class::Cargo, Tier::Interactive);
        let line = owner_cargo
            .waits
            .recv_timeout(WAIT)
            .expect("interactive cargo did not wait");
        assert!(line.contains("cargo wants"), "{line}");
        std::thread::sleep(STILL_WAITING);
        assert!(
            agent_light.granted.try_recv().is_err(),
            "agent light took the reserved slot"
        );
        assert!(
            owner_cargo.granted.try_recv().is_err(),
            "interactive cargo took the reserved slot"
        );
        assert!(
            probe(&slot(&dir(&ws), "cpu-interactive", 1))
                .unwrap()
                .is_none(),
            "reserved slot held"
        );
        holder.release.send(()).unwrap();
        // one CPU: the two run one after the other
        let first = [&agent_light, &owner_cargo]
            .into_iter()
            .position(|c| c.granted.recv_timeout(STILL_WAITING).is_ok());
        let first = first.expect("neither ran after the release");
        let (done, next) = if first == 0 {
            (agent_light, owner_cargo)
        } else {
            (owner_cargo, agent_light)
        };
        done.release.send(()).unwrap();
        next.granted
            .recv_timeout(WAIT)
            .expect("the second did not run");
        next.release.send(()).unwrap();
    }

    /// Three light streams keep churning while a heavy head needs both CPU slots: the head must be granted within HEAD_BOUND.
    /// Light taking every freed slot starves it, since the head needs two free at one poll.
    #[test]
    fn light_stream_does_not_starve_a_heavy_head() {
        use std::sync::{Arc, atomic::AtomicBool};
        /// One slot hold per light step, long against a stream's re-entry gap (a few ms in a debug build: config load, probes), so a freed slot is almost never still free at the head's poll.
        const HOLD: Duration = Duration::from_millis(500);
        /// One hold plus a poll is the head's real need; the bound is generous, so the test fails on starvation, not on a slow host.
        const HEAD_BOUND: Duration = Duration::from_secs(5);
        let ws = scratch_ws(
            "light-stream",
            Some(
                "[pool]\ncpu_slots = 2\nmem_gib = 2\n[pool.cargo]\ncpu_min = 2\ncpu_max = 2\nmem_gib = 1\n",
            ),
        );
        let stop = Arc::new(AtomicBool::new(false));
        let streams: Vec<_> = (0..3)
            .map(|i| {
                let (ws, stop) = (ws.clone(), Arc::clone(&stop));
                let who = format!("stream-{i}");
                std::thread::spawn(move || {
                    while !stop.load(Ordering::Relaxed) {
                        let grant = acquire_with(
                            &ws,
                            Path::new(&who),
                            Class::Light,
                            Tier::Agent,
                            FAST,
                            &mut |_| {},
                        )
                        .expect("light acquire");
                        std::thread::sleep(HOLD);
                        drop(grant);
                    }
                })
            })
            .collect();
        std::thread::sleep(STILL_WAITING);
        let head = contender(&ws, "head", Class::Cargo);
        let granted = head.granted.recv_timeout(HEAD_BOUND);
        stop.store(true, Ordering::Relaxed);
        // a release sent before the grant makes the head drop its grant at once, so no stream waits on it
        let _ = head.release.send(());
        for s in streams {
            s.join().unwrap();
        }
        assert!(
            granted.is_ok(),
            "the heavy head was not granted within {HEAD_BOUND:?} while light streams ran"
        );
    }

    /// The bypass rule: light may take a slot past a head only while the head keeps its `cpu_min` free, or the head lacks memory anyway. Expected values follow the rule.
    #[test]
    fn light_bypass_keeps_the_head_grantable() {
        let head = ClassWeight {
            cpu_min: 4,
            cpu_max: 8,
            mem_gib: 7,
        };
        let snap = |free_cpu, free_mem| Snapshot {
            free_cpu,
            free_mem,
            held: BTreeMap::new(),
        };
        // memory ready: one slot may go only while 4 stay free
        assert!(may_bypass(head, &snap(5, 7)));
        assert!(!may_bypass(head, &snap(4, 7)));
        // memory short: the head cannot be granted anyway
        assert!(may_bypass(head, &snap(1, 6)));
    }

    /// Ticket names sort as (tier, arrival): a later interactive ticket before an earlier land one, both before agents; agent tickets (the pre-tier format, which old binaries still write) interleave by arrival whichever binary wrote them.
    #[test]
    fn tickets_order_by_tier_then_arrival() {
        let agent_new_early = "01794900000000000001-0000000001-0000000000";
        let land_mid = "0.01794900000000000002-0000000002-0000000000";
        let interactive_late = "0-01794900000000000003-0000000003-0000000000";
        let agent_old_later = "01794900000000000004-0000000004-0000000000";
        let mut names = vec![agent_old_later, agent_new_early, land_mid, interactive_late];
        names.sort();
        assert_eq!(
            names,
            [interactive_late, land_mid, agent_new_early, agent_old_later]
        );
        let tickets: Vec<(String, Option<Class>)> =
            names.iter().map(|n| ((*n).to_owned(), None)).collect();
        assert_eq!(queue_position(&tickets, interactive_late), (0, 4));
        assert_eq!(queue_position(&tickets, agent_old_later), (3, 4));
        // interactive passes the two that arrived before it (land, early agent); land passes the early agent; agents pass none
        assert_eq!(passed_earlier(&tickets, interactive_late), 2);
        assert_eq!(passed_earlier(&tickets, land_mid), 1);
        assert_eq!(passed_earlier(&tickets, agent_new_early), 0);
        assert_eq!(arrival(interactive_late), &interactive_late[2..]);
        assert_eq!(arrival(land_mid), &land_mid[2..]);
        assert_eq!(arrival(agent_new_early), agent_new_early);
    }

    /// Mixed classes: an interactive cargo request that arrives after an agent's native-full ticket is admitted first once slots free, says so, and the agent's full native waits on.
    #[test]
    fn interactive_ticket_is_admitted_ahead_of_an_earlier_agent() {
        let ws = scratch_ws(
            "tier-ahead",
            Some(
                "[pool]\ncpu_slots = 8\nmem_gib = 8\n[pool.native-full]\ncpu_min = 8\ncpu_max = 8\nmem_gib = 7\n[pool.cargo]\ncpu_min = 4\ncpu_max = 4\nmem_gib = 1\n",
            ),
        );
        let blocker = contender(&ws, "blocker", Class::NativeFull);
        assert_eq!(blocker.granted.recv_timeout(WAIT).unwrap(), "cpu 8 mem 7");
        let agent = contender(&ws, "agent", Class::NativeFull);
        agent
            .waits
            .recv_timeout(WAIT)
            .expect("the agent did not queue");
        let owner = contender_at(&ws, "owner", Class::Cargo, Tier::Interactive);
        owner
            .waits
            .recv_timeout(WAIT)
            .expect("the owner did not wait for the blocker");
        blocker.release.send(()).unwrap();
        assert_eq!(owner.granted.recv_timeout(WAIT).unwrap(), "cpu 4 mem 1");
        let lines: Vec<String> = owner.waits.try_iter().collect();
        assert!(
            lines
                .iter()
                .any(|l| l == "pool: cargo tier interactive admitted ahead of 1 earlier ticket(s)"),
            "{lines:?}"
        );
        assert!(
            agent.granted.recv_timeout(STILL_WAITING).is_err(),
            "the agent took cpu the owner holds"
        );
        owner.release.send(()).unwrap();
        assert_eq!(agent.granted.recv_timeout(WAIT).unwrap(), "cpu 8 mem 7");
    }

    /// Starvation bound: an agent ticket stuck behind an interactive ticket that never leaves (a waiter that gathers nothing) is promoted after `starve_after_s`, then heads the queue by its earlier arrival and is granted.
    #[test]
    fn a_starved_agent_ticket_is_promoted_and_granted() {
        let ws = scratch_ws(
            "tier-starve",
            Some(
                "[pool]\ncpu_slots = 4\nmem_gib = 4\nstarve_after_s = 1\n[pool.cargo]\ncpu_min = 1\ncpu_max = 4\nmem_gib = 1\n",
            ),
        );
        let queue = dir(&ws).join("queue");
        // all slots held so the agent queues first, then a later interactive ticket heads the queue
        let pool = ws_config::load(&ws).unwrap().pool;
        let all = ClassWeight {
            cpu_min: 4,
            cpu_max: 4,
            mem_gib: 4,
        };
        std::fs::create_dir_all(&queue).unwrap();
        let (_, held) = try_grant(&dir(&ws), &pool, all, "holder pid 1 class test")
            .unwrap()
            .expect("empty pool");
        let agent = contender(&ws, "agent", Class::Cargo);
        agent
            .waits
            .recv_timeout(WAIT)
            .expect("the agent did not queue");
        let stuck = Ticket::new(&queue, 1, Class::NativeFull, Tier::Interactive).unwrap();
        drop(held);
        let lines: Vec<String> = std::iter::from_fn(|| agent.waits.recv_timeout(WAIT).ok())
            .take_while(|l| !l.contains("promoted"))
            .collect();
        assert!(
            !lines.iter().any(|l| l.contains("granted")),
            "granted before promotion: {lines:?}"
        );
        agent
            .granted
            .recv_timeout(WAIT)
            .expect("promoted ticket not granted");
        drop(stuck);
    }

    /// A live head ticket naming a class this xtask lacks (another version's): light queues FIFO, and the fallback logs and counts.
    #[test]
    fn unknown_head_class_queues_light_and_is_counted() {
        let ws = scratch_ws("unknown-head", Some("[pool]\ncpu_slots = 2\nmem_gib = 2\n"));
        let pool = ws_config::load(&ws).expect("config").pool;
        let dir = dir(&ws);
        let queue = dir.join("queue");
        std::fs::create_dir_all(&queue).expect("queue dir");
        let path = queue.join("00000000000000000001-0000000001-0000000000");
        std::fs::write(&path, "retired-class").expect("ticket");
        let file = OpenOptions::new().read(true).open(&path).expect("open");
        let _waiter = Flock::try_take(file, Mode::Exclusive)
            .expect("flock")
            .expect("free");
        let tickets = live_tickets(&queue).expect("tickets");
        assert_eq!(tickets.len(), 1);
        assert_eq!(tickets[0].1, None);
        let before = UNKNOWN_HEAD_FIFO.load(Ordering::Relaxed);
        let mut lines = Vec::new();
        let allowed = bypass_allowed(&dir, &pool, &tickets, &mut |l| lines.push(l.to_owned()))
            .expect("bypass_allowed");
        assert!(!allowed, "light bypassed an unknown head");
        assert_eq!(UNKNOWN_HEAD_FIFO.load(Ordering::Relaxed), before + 1);
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].contains("light queues FIFO"), "{}", lines[0]);
    }

    /// Memory admits two full natives (7 GiB each of 20) and not a third; the third's wait line names the memory.
    #[test]
    fn memory_admits_two_full_natives_and_the_third_waits() {
        // 32 slots, native-full pinned at 8 so that CPU is not the limit (the default would be 16 each)
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
            line.contains("native-full wants cpu >=8 mem 7 GiB"),
            "{line}"
        );
        assert_eq!(
            line.matches("class native-full cpu 8 mem 7").count(),
            2,
            "{line}"
        );
        assert!(
            c.granted.recv_timeout(STILL_WAITING).is_err(),
            "the third native ran past the memory"
        );
        a.release.send(()).unwrap();
        assert_eq!(c.granted.recv_timeout(WAIT).unwrap(), "cpu 8 mem 7");
        b.release.send(()).unwrap();
        c.release.send(()).unwrap();
    }

    /// Default weights on 16 slots: native-full is 8..8 (half), so two full natives run side by side and a third waits on CPU with every slot held.
    #[test]
    fn default_weights_run_two_full_natives_side_by_side() {
        let ws = scratch_ws("halves", Some("[pool]\ncpu_slots = 16\nmem_gib = 20\n"));
        let a = contender(&ws, "a", Class::NativeFull);
        assert_eq!(a.granted.recv_timeout(WAIT).unwrap(), "cpu 8 mem 7");
        let b = contender(&ws, "b", Class::NativeFull);
        assert_eq!(b.granted.recv_timeout(WAIT).unwrap(), "cpu 8 mem 7");
        let c = contender(&ws, "c", Class::NativeFull);
        let line = c
            .waits
            .recv_timeout(WAIT)
            .expect("the third native did not wait");
        assert!(line.contains("free cpu 0/16"), "{line}");
        a.release.send(()).unwrap();
        assert!(
            c.granted.recv_timeout(WAIT).is_ok(),
            "third native never ran"
        );
        b.release.send(()).unwrap();
        c.release.send(()).unwrap();
    }

    /// Each class takes its weight: cargo gets every free slot up to its default max (16 - 1 = 15) and 6 GiB; harness is capped at its max of 6 CPU slots.
    #[test]
    fn class_weights_size_the_grant() {
        let ws = scratch_ws("weights", Some("[pool]\ncpu_slots = 16\nmem_gib = 20\n"));
        let cargo = contender(&ws, "cargo", Class::Cargo);
        assert_eq!(cargo.granted.recv_timeout(WAIT).unwrap(), "cpu 15 mem 6");
        cargo.release.send(()).unwrap();
        let harness = contender(&ws, "harness", Class::Harness);
        assert_eq!(harness.granted.recv_timeout(WAIT).unwrap(), "cpu 6 mem 2");
        harness.release.send(()).unwrap();
    }

    #[test]
    fn same_thread_acquire_under_a_grant_is_refused() {
        let ws = scratch_ws("nest-thread", Some(CONFIG_ONE_CPU));
        let who = Path::new("nest");
        let grant = acquire_with(&ws, who, Class::Light, Tier::Agent, FAST, &mut |_| {
            panic!("waited")
        })
        .unwrap();
        assert_eq!(held_jobs(), Some(1));
        let e = acquire_with(&ws, who, Class::Light, Tier::Agent, FAST, &mut |_| {
            panic!("waited")
        })
        .expect_err("a nested acquire must be refused");
        assert!(
            format!("{e:#}").contains("nested admission refused"),
            "{e:#}"
        );
        drop(grant);
        assert_eq!(held_jobs(), None);
        acquire_with(&ws, who, Class::Light, Tier::Agent, FAST, &mut |_| {
            panic!("waited")
        })
        .expect("admitted again after the release");
    }

    const CONFIG_ONE_CPU: &str = "[pool]\ncpu_slots = 1\nmem_gib = 20\n";

    /// Holder process: takes a grant, then runs `helper_nested` as its child while holding it. The child's acquire must be refused.
    #[test]
    #[ignore = "run by ancestor_holder_refuses_a_nested_acquire"]
    fn helper_holder() {
        let ws = PathBuf::from(std::env::var_os("POOL_TEST_WS").expect("set by the test"));
        let _grant = acquire_with(
            &ws,
            Path::new("holder"),
            Class::Light,
            Tier::Agent,
            FAST,
            &mut |_| panic!("holder waited"),
        )
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
        let e = acquire_with(
            &ws,
            Path::new("nested"),
            Class::Light,
            Tier::Agent,
            FAST,
            &mut |_| panic!("nested waited"),
        )
        .expect_err("a child of the holder must be refused");
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
            .expect_err("a held slot must refuse the quiet window");
        assert!(
            refused.contains("quiet window refused") && refused.contains("held pid"),
            "{refused}"
        );
        c.release.send(()).unwrap();
    }

    #[test]
    fn class_larger_than_the_pool_is_refused_not_waited_for() {
        // defaults always fit cpu_slots (half the slots): an override asks for more than the pool has
        let ws = scratch_ws(
            "fits",
            Some(
                "[pool]\ncpu_slots = 4\nmem_gib = 20\n[pool.native-full]\ncpu_min = 8\ncpu_max = 8\n",
            ),
        );
        let e = acquire_with(
            &ws,
            Path::new("big"),
            Class::NativeFull,
            Tier::Agent,
            FAST,
            &mut |_| panic!("waited"),
        )
        .expect_err("must refuse");
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
