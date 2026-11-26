// SPDX-License-Identifier: MIT
//! A gate's scope: the cgroup v2 dir of the transient user scope `systemd-run --user --scope` makes. The sampler reads `cpu.pressure`, `cpu.stat` and `memory.peak` there.
//! A test gate relaunches itself into `atl-gate-<pid>.scope` when its host can make one. The exec keeps the pid, so the relaunched process recognises its unit ([`relaunched`]) and never relaunches again.
//! A managed row (`xtask-harness` `RowRun::row`) is a scope `atl-row-<pid>-<n>.scope` whose main process is the supervisor ([`supervise`]): the supervisor sits in `supervisor/`, the row in the `workload/` leaf. The leaf's final counters are read after the row exited and before the leaf is drained; the row's own gate measures the leaf ([`in_managed_leaf`]) and never relaunches.
#![expect(
    clippy::disallowed_types,
    clippy::disallowed_methods,
    reason = "systemd-run moves this test-driver process into a cgroup and the exec replaces only that driver, which runs no sandboxed app; the apps it starts still go through the broker (clippy.toml)"
)]

use std::convert::Infallible;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, Write as _};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, ExitStatus};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use rustix::process::{Pid, Signal, kill_process};

/// The cgroup v2 mount.
const CGROUP_ROOT: &str = "/sys/fs/cgroup";
/// A relaunch names its unit `atl-gate-<pid>`; systemd adds `.scope`.
const UNIT_PREFIX: &str = "atl-gate-";

/// [`leftovers`]'s poll.
const LEFTOVER_POLL: Duration = Duration::from_millis(50);
/// The length of a command line [`leftovers`] shows.
const COMMAND_CHARS: usize = 100;

/// A gate's scope: its cgroup dir.
#[derive(Clone, Debug, PartialEq)]
pub struct Scope {
    pub dir: PathBuf,
}

impl Scope {
    /// `dir` as a scope: Ok when its `cpu.pressure` and `cpu.stat` read (the sampler's inputs); else the first file that does not.
    pub fn at(dir: PathBuf) -> Result<Self, String> {
        for file in ["cpu.pressure", "cpu.stat"] {
            let path = dir.join(file);
            fs::read_to_string(&path).map_err(|e| format!("{} unreadable: {e}", path.display()))?;
        }
        Ok(Self { dir })
    }

    /// The unit's name: the dir's last component, e.g. `atl-gate-1234.scope`; a managed row's leaf is `<scope>/workload`.
    pub fn unit(&self) -> String {
        if is_managed_leaf(&self.dir) {
            let scope = self.dir.parent().map_or_else(|| "?".to_owned(), unit_of);
            return format!("{scope}/{WORKLOAD_DIR}");
        }
        unit_of(&self.dir)
    }
}

/// The gate scope this process runs in (its own, or the gate's that started it, or a managed row's leaf), when it has the cpu files. Any other scope is the job's: Err.
pub fn own() -> Result<Scope, String> {
    gate_scope(cgroup_dir(std::process::id())?)
}

/// `dir` as a gate's scope: an `atl-gate-<pid>.scope` or a managed leaf, with the cpu files.
fn gate_scope(dir: PathBuf) -> Result<Scope, String> {
    let is_gate = dir
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with(UNIT_PREFIX) && n.ends_with(".scope"));
    if !is_gate && !is_managed_leaf(&dir) {
        return Err(format!("{}: not a gate's scope", dir.display()));
    }
    Scope::at(dir)
}

/// The last path component as text, `?` when none.
fn unit_of(dir: &Path) -> String {
    dir.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("?")
        .to_owned()
}

/// `pid`'s cgroup v2 dir: the `0::` line of `/proc/<pid>/cgroup`, under the cgroup mount.
pub fn cgroup_dir(pid: u32) -> Result<PathBuf, String> {
    let text = fs::read_to_string(format!("/proc/{pid}/cgroup"))
        .map_err(|e| format!("/proc/{pid}/cgroup: {e}"))?;
    parse_v2_path(&text)
        .map(v2_dir)
        .ok_or_else(|| format!("/proc/{pid}/cgroup: no cgroup v2 line"))
}

/// The scope's `memory.peak` (bytes): its high-water mark since it began, page cache included.
pub fn peak_bytes(dir: &Path) -> Option<u64> {
    fs::read_to_string(dir.join("memory.peak"))
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// The other processes in the scope at `dir`, as `<pid> <command line>`, once none is left or `settle` has passed. Zombies are not listed (cgroup v2 `cgroup.procs`).
/// This process's ancestors in the scope are not listed: a gate nested in a managed row shares the row's leaf, and the row waits on it, alive.
/// `shared` (the leaf is a managed row's, with its other workloads in it): only this process's descendants count; a sibling's process is not one this gate left.
pub fn leftovers(dir: &Path, settle: Duration, shared: bool) -> Result<Vec<String>, String> {
    let path = dir.join("cgroup.procs");
    let own_pid = std::process::id();
    let own = own_pid.to_string();
    let above = ancestors(own_pid);
    let until = Instant::now() + settle;
    loop {
        let text =
            fs::read_to_string(&path).map_err(|e| format!("{} unreadable: {e}", path.display()))?;
        let pids: Vec<&str> = text
            .lines()
            .filter(|p| *p != own && !above.iter().any(|a| a == p))
            .filter(|p| {
                !shared
                    || p.parse::<u32>()
                        .is_ok_and(|pid| ancestors(pid).contains(&own))
            })
            .collect();
        if pids.is_empty() {
            return Ok(Vec::new());
        }
        if Instant::now() >= until {
            return Ok(pids.into_iter().map(with_command).collect());
        }
        std::thread::sleep(LEFTOVER_POLL);
    }
}

/// `pid`'s ancestors as pid strings, parent first, up to init (pid 1 excluded).
fn ancestors(pid: u32) -> Vec<String> {
    let mut out = Vec::new();
    let mut at = pid;
    while let Some(parent) = ppid(at).filter(|p| *p > 1) {
        out.push(parent.to_string());
        at = parent;
    }
    out
}

/// `pid`'s parent, from its `/proc/<pid>/status` `PPid:` line.
fn ppid(pid: u32) -> Option<u32> {
    fs::read_to_string(format!("/proc/{pid}/status"))
        .ok()?
        .lines()
        .find_map(|l| l.strip_prefix("PPid:")?.trim().parse().ok())
}

/// `<pid> <command line>` on one line, cut to [`COMMAND_CHARS`].
fn with_command(pid: &str) -> String {
    let cmd = fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
    let cmd = String::from_utf8_lossy(&cmd).replace(char::is_control, " ");
    let cmd: String = cmd.trim().chars().take(COMMAND_CHARS).collect();
    format!("{pid} {cmd}")
}

/// Whether this process already is the relaunch: its unit is `atl-gate-<pid>.scope`, whether or not the scope has cpu files.
pub fn relaunched() -> bool {
    let pid = std::process::id();
    let want = format!("{UNIT_PREFIX}{pid}.scope");
    cgroup_dir(pid).is_ok_and(|dir| dir.file_name().and_then(|n| n.to_str()) == Some(want.as_str()))
}

/// Relaunches this process's binary and arguments into `atl-gate-<pid>.scope`: `systemd-run` replaces the process, runs the same command in the new scope, and the pid stays.
/// Returns only on failure. A probe runs first (a scope from `systemd-run` whose cpu files read), so a host that cannot make a scope keeps this process and says why.
pub fn relaunch_self() -> Result<Infallible, String> {
    let exe = std::env::current_exe().map_err(|e| format!("current exe: {e}"))?;
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    probe()?;
    let err = Command::new("systemd-run")
        .args(["--user", "--scope", "--quiet", "--collect"])
        .arg(format!("--unit={UNIT_PREFIX}{}", std::process::id()))
        .arg("--")
        .arg(exe)
        .args(args)
        .exec();
    Err(format!("exec systemd-run: {err}"))
}

/// One throwaway `systemd-run` scope that prints its cgroup: the proof that a scope can be made with the cpu files the sampler reads.
fn probe() -> Result<(), String> {
    let out = Command::new("systemd-run")
        .args([
            "--user",
            "--scope",
            "--quiet",
            "--collect",
            "--",
            "cat",
            "/proc/self/cgroup",
        ])
        .output()
        .map_err(|e| format!("systemd-run: {e}"))?;
    if !out.status.success() {
        let why = String::from_utf8_lossy(&out.stderr);
        return Err(format!("systemd-run probe: {}", why.trim()));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let path = parse_v2_path(&text).ok_or_else(|| "probe: no cgroup v2 line".to_owned())?;
    Scope::at(v2_dir(path)).map(drop)
}

/// The path of the `0::` (cgroup v2) line of a `/proc/<pid>/cgroup` text.
fn parse_v2_path(text: &str) -> Option<&str> {
    text.lines().find_map(|l| l.strip_prefix("0::"))
}

fn v2_dir(path: &str) -> PathBuf {
    Path::new(CGROUP_ROOT).join(path.trim_start_matches('/'))
}

// ----- managed rows (slice 3) -----

/// A managed row's scope name prefix: `atl-row-<pid>-<n>.scope`.
const MANAGED_PREFIX: &str = "atl-row-";
/// A managed scope's supervisor cgroup: the scope's main process, out of the scope's own cgroup.
const SUPERVISOR_DIR: &str = "supervisor";
/// A managed scope's leaf: the workload's cgroup, whose counters are the row's.
const WORKLOAD_DIR: &str = "workload";
/// The controllers a leaf needs: `memory` for its counters and limits, `cpu` for `cpu.stat` and the sampler's pressure.
const CONTROLLERS: &str = "+cpu +memory";
/// The harness's placer subcommand (xtask-harness main.rs `ScopeEnter`).
const ENTER_SUBCOMMAND: &str = "scope-enter";
/// Leftovers of an exited workload get this long to go before they count as leftovers.
const SETTLE: Duration = Duration::from_secs(2);
/// A drained leaf reports `populated 0` within this.
const DRAIN_WAIT: Duration = Duration::from_secs(5);
/// The poll of a cgroup's pids while the supervisor waits or drains.
const POLL: Duration = Duration::from_millis(10);
/// The record's tag in the workload's log; [`Record::read_last`] takes the last one.
pub const RECORD_TAG: &str = "scope-record:";
/// The supervisor's exit code when its record is incomplete.
pub const INCOMPLETE_EXIT: u8 = 3;
/// The placer's exit code when it cannot move into the leaf: the workload then never ran in it.
pub const PLACE_FAILED: u8 = 125;

/// A managed row's unit name, unique in this process: `atl-row-<pid>-<n>` (systemd adds `.scope`).
pub fn new_unit() -> String {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    format!(
        "{MANAGED_PREFIX}{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

/// `systemd-run` for a managed scope `unit`; the scope's main process follows `--`. The scope runs in the caller's mount namespace with its env, cwd and stdio: a transient service would run in the user manager's namespace, outside the xtask build namespace.
pub fn scoped_command(unit: &str) -> Command {
    let mut cmd = Command::new("systemd-run");
    cmd.args(["--user", "--scope", "--quiet", "--collect"])
        .arg(format!("--unit={unit}"))
        .args(["-p", "Delegate=yes", "--"]);
    cmd
}

/// Stops the managed scope `unit` and kills what it still holds; a no-op once it ended. The parent calls it after the supervisor exited, on every path.
pub fn stop(unit: &str) {
    let _ = Command::new("systemctl")
        .args(["--user", "stop"])
        .arg(format!("{unit}.scope"))
        .output();
}

/// The placer (`scope-enter`): moves this process into `leaf` and execs `argv` in place, so the workload is in its leaf from its first instruction and keeps the pid the supervisor waits on.
/// Returns only on failure: [`PLACE_FAILED`] when the move fails, 127 when `argv` does not exec.
pub fn enter(leaf: &Path, argv: &[OsString]) -> ExitCode {
    if let Err(e) = fs::write(leaf.join("cgroup.procs"), std::process::id().to_string()) {
        let _ = writeln!(
            io::stderr(),
            "scope-enter: cannot join {}: {e}",
            leaf.display()
        );
        return ExitCode::from(PLACE_FAILED);
    }
    let Some((program, args)) = argv.split_first() else {
        return ExitCode::from(PLACE_FAILED);
    };
    let e = Command::new(program).args(args).exec();
    let _ = writeln!(
        io::stderr(),
        "scope-enter: exec {}: {e}",
        program.to_string_lossy()
    );
    ExitCode::from(127)
}

/// Whether this process runs in a managed row's leaf ([`supervise`]): then its gate measures that leaf and never relaunches.
pub fn in_managed_leaf() -> bool {
    cgroup_dir(std::process::id()).is_ok_and(|dir| is_managed_leaf(&dir))
}

/// Whether `dir` is a managed row's leaf: `workload` under an `atl-row-<pid>-<n>.scope`.
fn is_managed_leaf(dir: &Path) -> bool {
    dir.file_name() == Some(OsStr::new(WORKLOAD_DIR)) && dir.parent().is_some_and(is_managed_scope)
}

/// Whether `dir` is a managed row's scope: `atl-row-<pid>-<n>.scope`.
fn is_managed_scope(dir: &Path) -> bool {
    dir.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with(MANAGED_PREFIX) && n.ends_with(".scope"))
}

/// A cgroup's `memory.events.local` counters: its own events, not its descendants'.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Events {
    pub max: u64,
    pub high: u64,
    pub oom: u64,
    pub oom_kill: u64,
    pub oom_group_kill: u64,
}

impl Events {
    /// The `<key> <count>` lines of `memory.events.local`; a key the kernel adds later is ignored, a missing one reads 0.
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut e = Self::default();
        for line in text.lines() {
            let mut words = line.split_whitespace();
            let (Some(key), Some(value)) = (words.next(), words.next()) else {
                continue;
            };
            let n: u64 = value
                .parse()
                .map_err(|_| format!("memory.events.local: {key} is not a count: {value}"))?;
            match key {
                "max" => e.max = n,
                "high" => e.high = n,
                "oom" => e.oom = n,
                "oom_kill" => e.oom_kill = n,
                "oom_group_kill" => e.oom_group_kill = n,
                _ => {}
            }
        }
        Ok(e)
    }
}

/// A limit OOM inside this leaf: its own `oom` (its limit hit and reclaim failed) and a member killed (its own `oom_kill`). A plain SIGKILL leaves both 0; an ancestor's limit kills a member too, but the leaf's own `oom` stays 0.
pub fn local_oom(events: &Events) -> bool {
    events.oom >= 1 && events.oom_kill >= 1
}

/// What the workload did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Exit {
    Code(i32),
    Signal(i32),
    /// The workload never ran in its leaf.
    NotRun,
}

/// A complete measurement of one managed workload: its exit, and the leaf's counters read after it exited and before the leaf was drained.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    pub unit: String,
    pub exit: Exit,
    pub wall_ms: u64,
    pub cpu_usec: u64,
    pub mem_peak: u64,
    pub events: Events,
    /// the leaf's `memory.max` the supervisor set, bytes (a test fixture only)
    pub limit: Option<u64>,
}

impl Record {
    /// Whether the leaf certifies a local OOM ([`local_oom`]).
    pub fn certified(&self) -> bool {
        local_oom(&self.events)
    }

    /// The last record of a log. Complete: Ok. Incomplete, or no record at all: Err with why.
    pub fn read_last(log: &str) -> Result<Self, String> {
        let text = log
            .lines()
            .rev()
            .find_map(|l| l.find(RECORD_TAG).map(|at| &l[at + RECORD_TAG.len()..]))
            .ok_or("no scope-record in the log: the supervisor did not finish")?;
        let (head, reason) = match text.split_once(" reason=") {
            Some((head, reason)) => (head, Some(reason)),
            None => (text, None),
        };
        let field = |key: &str| {
            head.split_whitespace()
                .find_map(|w| w.strip_prefix(key)?.strip_prefix('='))
        };
        if field("complete") != Some("1") {
            return Err(reason.unwrap_or("incomplete, no reason given").to_owned());
        }
        let count = |key: &str| {
            field(key)
                .and_then(|v| v.parse::<u64>().ok())
                .ok_or_else(|| format!("scope-record: {key} missing or not a count"))
        };
        Ok(Self {
            unit: field("unit")
                .ok_or("scope-record: unit missing")?
                .to_owned(),
            exit: field("exit")
                .and_then(parse_exit)
                .ok_or("scope-record: exit missing or unknown")?,
            wall_ms: count("wall_ms")?,
            cpu_usec: count("cpu_usec")?,
            mem_peak: count("mem_peak")?,
            events: Events {
                max: count("max")?,
                high: count("high")?,
                oom: count("oom")?,
                oom_kill: count("oom_kill")?,
                oom_group_kill: count("oom_group_kill")?,
            },
            limit: match field("limit") {
                Some("none") => None,
                _ => Some(count("limit")?),
            },
        })
    }
}

/// The `scope-record:` line: a complete measurement, or the incomplete one with its reason.
fn record_line(unit: &str, exit: Exit, outcome: &Result<Record, String>) -> String {
    match outcome {
        Ok(r) => format!(
            "{RECORD_TAG} unit={unit} exit={} wall_ms={} cpu_usec={} mem_peak={} max={} high={} oom={} oom_kill={} oom_group_kill={} limit={} complete=1",
            exit_text(r.exit),
            r.wall_ms,
            r.cpu_usec,
            r.mem_peak,
            r.events.max,
            r.events.high,
            r.events.oom,
            r.events.oom_kill,
            r.events.oom_group_kill,
            r.limit.map_or("none".to_owned(), |l| l.to_string()),
        ),
        Err(why) => format!(
            "{RECORD_TAG} unit={unit} exit={} complete=0 reason={}",
            exit_text(exit),
            why.replace('\n', " ")
        ),
    }
}

fn exit_text(exit: Exit) -> String {
    match exit {
        Exit::Code(c) => format!("code:{c}"),
        Exit::Signal(s) => format!("signal:{s}"),
        Exit::NotRun => "none".to_owned(),
    }
}

/// The supervisor: this process is the main of a managed scope ([`scoped_command`]). It runs `workload` in the scope's leaf through the placer ([`enter`]), reads the leaf's final counters after the workload exited and before the leaf is drained, drains what the scope still holds, and appends the `scope-record:` line to `log`.
/// Ok: a complete measurement. Err: incomplete, with why (the line says so too). `workload` is only read: its program, arguments, env and cwd are the workload's.
pub fn supervise(
    workload: &Command,
    log: &Path,
    memory_max: Option<u64>,
) -> Result<Record, String> {
    let mut exit = Exit::NotRun;
    let (unit, outcome) = match Tree::of_self() {
        Err(why) => ("?".to_owned(), Err(why)),
        Ok(tree) => {
            let measured = measure(&tree, workload, memory_max, &mut exit);
            let drained = drain(&tree);
            let outcome = match (measured, drained) {
                (Ok(record), Ok(())) => Ok(record),
                (Ok(_), Err(d)) => Err(format!("drain: {d}")),
                (Err(m), Ok(())) => Err(m),
                (Err(m), Err(d)) => Err(format!("{m}; drain: {d}")),
            };
            (tree.unit, outcome)
        }
    };
    let line = record_line(&unit, exit, &outcome);
    // a log that cannot take the line leaves no record: the reader then rejects the row
    let _ = append(log, &line);
    outcome
}

/// The cgroups of this managed scope.
struct Tree {
    unit: String,
    scope: PathBuf,
    supervisor: PathBuf,
    workload: PathBuf,
}

impl Tree {
    /// This process's scope; Err unless it is a managed row's scope.
    fn of_self() -> Result<Self, String> {
        let scope = cgroup_dir(std::process::id())?;
        if !is_managed_scope(&scope) {
            return Err(format!("{}: not a managed scope", scope.display()));
        }
        Ok(Self {
            unit: unit_of(&scope),
            supervisor: scope.join(SUPERVISOR_DIR),
            workload: scope.join(WORKLOAD_DIR),
            scope,
        })
    }
}

/// The workload's run and the leaf's final counters. Every step that cannot be read or checked is Err: the record reports it as incomplete. `exit` is set once the workload ended.
fn measure(
    tree: &Tree,
    workload: &Command,
    memory_max: Option<u64>,
    exit: &mut Exit,
) -> Result<Record, String> {
    // the supervisor leaves the scope's own cgroup: the scope then holds the supervisor's and the leaf's cgroups only
    fs::create_dir(&tree.supervisor).map_err(|e| format!("supervisor cgroup: {e}"))?;
    fs::write(
        tree.supervisor.join("cgroup.procs"),
        std::process::id().to_string(),
    )
    .map_err(|e| format!("the supervisor cannot leave the scope's cgroup: {e}"))?;
    fs::write(tree.scope.join("cgroup.subtree_control"), CONTROLLERS)
        .map_err(|e| format!("controllers {CONTROLLERS} not delegated: {e}"))?;
    fs::create_dir(&tree.workload).map_err(|e| format!("leaf cgroup: {e}"))?;
    for file in ["memory.events.local", "memory.peak", "cpu.stat"] {
        read(&tree.workload.join(file)).map_err(|e| format!("leaf {file} unreadable: {e}"))?;
    }
    // fresh counters: whatever the leaf counts before its workload ran is not the row's
    let fresh = Events::parse(&read(&tree.workload.join("memory.events.local"))?)?;
    if fresh != Events::default() {
        return Err(format!(
            "counters not fresh before the workload ran: {fresh:?}"
        ));
    }
    if let Some(max) = memory_max {
        apply_limit(&tree.workload, max)?;
    }
    let started = Instant::now();
    let mut child = placer(workload, &tree.workload)?
        .spawn()
        .map_err(|e| format!("spawn workload: {e}"))?;
    let status = child
        .wait()
        .map_err(|e| format!("wait for workload: {e}"))?;
    *exit = exit_of(status);
    if status.code() == Some(i32::from(PLACE_FAILED)) {
        *exit = Exit::NotRun;
        return Err("placement: the workload could not join its leaf".to_owned());
    }
    let leftover = settle(&tree.workload)?;
    let wall_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let events = Events::parse(&read(&tree.workload.join("memory.events.local"))?)?;
    let mem_peak = read_count(&tree.workload.join("memory.peak"))?;
    let cpu_usec = usage_usec(&tree.workload.join("cpu.stat"))?;
    let stray = strays(tree)?;
    if !stray.is_empty() {
        return Err(format!(
            "membership: {} process(es) outside the workload's leaf: {}",
            stray.len(),
            list(&stray)
        ));
    }
    if leftover > 0 {
        // the names are read now: a process may have gone since the count
        let names = leftovers(&tree.workload, Duration::ZERO, false).unwrap_or_default();
        return Err(format!(
            "leftover: {leftover} process(es) still in the workload's leaf {SETTLE:?} after it exited: {}",
            names.join("; ")
        ));
    }
    Ok(Record {
        unit: tree.unit.clone(),
        exit: *exit,
        wall_ms,
        cpu_usec,
        mem_peak,
        events,
        limit: memory_max,
    })
}

/// The workload as the placer runs it: `scope-enter --leaf <leaf> -- <program> <args>` with the workload's env and cwd. Its stdio is the supervisor's, inherited.
fn placer(workload: &Command, leaf: &Path) -> Result<Command, String> {
    let exe = std::env::current_exe().map_err(|e| format!("current exe: {e}"))?;
    let mut cmd = Command::new(exe);
    cmd.arg(ENTER_SUBCOMMAND)
        .arg("--leaf")
        .arg(leaf)
        .arg("--")
        .arg(workload.get_program())
        .args(workload.get_args());
    for (key, value) in workload.get_envs() {
        match value {
            Some(value) => cmd.env(key, value),
            None => cmd.env_remove(key),
        };
    }
    if let Some(dir) = workload.get_current_dir() {
        cmd.current_dir(dir);
    }
    Ok(cmd)
}

/// The leaf's memory limit, a test fixture: `memory.max`, swap off (`memory.swap.max`: else the limit reclaims to swap and never kills) and the group killed together (`memory.oom.group`). Each value is read back.
fn apply_limit(leaf: &Path, max: u64) -> Result<(), String> {
    for (file, value) in [
        ("memory.max", max.to_string()),
        ("memory.swap.max", "0".to_owned()),
        ("memory.oom.group", "1".to_owned()),
    ] {
        fs::write(leaf.join(file), &value).map_err(|e| format!("limit {file}: {e}"))?;
        let back = read(&leaf.join(file))?;
        if back.trim() != value {
            return Err(format!("limit {file} reads {}, not {value}", back.trim()));
        }
    }
    Ok(())
}

/// The leaf's processes still there after the workload exited: once none is left or [`SETTLE`] has passed.
fn settle(leaf: &Path) -> Result<u64, String> {
    let until = Instant::now() + SETTLE;
    loop {
        let left = procs_of(leaf)?.len();
        if left == 0 || Instant::now() >= until {
            return Ok(u64::try_from(left).unwrap_or(u64::MAX));
        }
        std::thread::sleep(POLL);
    }
}

/// Releases what the scope holds: the strays, then the leaf's processes (`cgroup.kill`), then the leaf dir. The counters were read before this. A leaf still populated after [`DRAIN_WAIT`] is an error.
fn drain(tree: &Tree) -> Result<(), String> {
    for pid in strays(tree)? {
        kill(pid);
    }
    if !tree.workload.exists() {
        return Ok(());
    }
    let kill_file = tree.workload.join("cgroup.kill");
    if kill_file.exists() {
        fs::write(&kill_file, "1").map_err(|e| format!("cgroup.kill: {e}"))?;
    } else {
        for pid in procs_of(&tree.workload)? {
            kill(pid);
        }
    }
    let until = Instant::now() + DRAIN_WAIT;
    while populated(&tree.workload)? {
        if Instant::now() >= until {
            return Err(format!(
                "leaf still populated {DRAIN_WAIT:?} after the kill"
            ));
        }
        std::thread::sleep(POLL);
    }
    fs::remove_dir(&tree.workload).map_err(|e| format!("leaf not released: {e}"))
}

/// The processes that are not in the workload's leaf and are not this supervisor: none by construction, so any is a membership failure.
fn strays(tree: &Tree) -> Result<Vec<u32>, String> {
    let me = std::process::id();
    let mut out = procs_of(&tree.scope)?;
    out.extend(procs_of(&tree.supervisor)?.into_iter().filter(|p| *p != me));
    Ok(out)
}

/// The pids in a cgroup dir; none when the dir does not exist.
fn procs_of(dir: &Path) -> Result<Vec<u32>, String> {
    if !dir.exists() {
        return Ok(Vec::new());
    }
    pids(&dir.join("cgroup.procs"))
}

/// The pids a `cgroup.procs` lists.
fn pids(procs: &Path) -> Result<Vec<u32>, String> {
    read(procs)?
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| {
            l.parse()
                .map_err(|_| format!("{}: not a pid: {l}", procs.display()))
        })
        .collect()
}

/// Whether a leaf still has processes (`cgroup.events` `populated 1`).
fn populated(leaf: &Path) -> Result<bool, String> {
    Ok(read(&leaf.join("cgroup.events"))?
        .lines()
        .any(|l| l == "populated 1"))
}

/// SIGKILL to `pid`; one that already ended is no error.
fn kill(pid: u32) {
    if let Some(pid) = i32::try_from(pid).ok().and_then(Pid::from_raw) {
        let _ = kill_process(pid, Signal::KILL);
    }
}

/// A cgroup file's text; its path leads the error.
fn read(path: &Path) -> Result<String, String> {
    fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))
}

/// A cgroup file holding one count (`memory.peak`).
fn read_count(path: &Path) -> Result<u64, String> {
    let text = read(path)?;
    text.trim()
        .parse()
        .map_err(|_| format!("{}: not a count: {}", path.display(), text.trim()))
}

/// The `usage_usec` of a `cpu.stat`.
fn usage_usec(stat: &Path) -> Result<u64, String> {
    read(stat)?
        .lines()
        .find_map(|l| l.strip_prefix("usage_usec ")?.parse().ok())
        .ok_or_else(|| format!("{}: no usage_usec", stat.display()))
}

fn exit_of(status: ExitStatus) -> Exit {
    match (status.code(), status.signal()) {
        (Some(code), _) => Exit::Code(code),
        (None, Some(signal)) => Exit::Signal(signal),
        (None, None) => Exit::NotRun,
    }
}

fn parse_exit(text: &str) -> Option<Exit> {
    if text == "none" {
        return Some(Exit::NotRun);
    }
    if let Some(code) = text.strip_prefix("code:") {
        return code.parse().ok().map(Exit::Code);
    }
    text.strip_prefix("signal:")?.parse().ok().map(Exit::Signal)
}

fn list(pids: &[u32]) -> String {
    pids.iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(" ")
}

/// Appends `line` to `log` on a line of its own: the workload's output may end without a newline.
fn append(log: &Path, line: &str) -> io::Result<()> {
    let mut file = fs::OpenOptions::new().append(true).create(true).open(log)?;
    writeln!(file, "\n{line}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The v2 line is `0::`, not a v1 controller line.
    #[test]
    fn finds_the_v2_line_only() {
        let text = "1:name=systemd:/init\n0::/user.slice/app.slice/a.scope\n";
        assert_eq!(parse_v2_path(text), Some("/user.slice/app.slice/a.scope"));
        assert_eq!(parse_v2_path("1:cpu,cpuacct:/x\n"), None);
    }

    /// A job's scope with the same cpu files is not measured as the gate's.
    #[test]
    fn only_a_gate_unit_is_a_gates_scope() {
        let root = std::env::temp_dir().join(format!("atl-host-load-scope-{}", std::process::id()));
        for unit in ["atl-gate-7.scope", "job-7.scope"] {
            let dir = root.join(unit);
            fs::create_dir_all(&dir).expect("unit dir");
            for file in ["cpu.pressure", "cpu.stat"] {
                fs::write(dir.join(file), "").expect("cpu file");
            }
        }
        let gate = gate_scope(root.join("atl-gate-7.scope"));
        let job = gate_scope(root.join("job-7.scope"));
        fs::remove_dir_all(&root).expect("cleanup");
        assert_eq!(
            gate.map(|s| s.unit().to_owned()),
            Ok("atl-gate-7.scope".to_owned())
        );
        assert!(job.is_err());
    }

    #[test]
    fn joins_the_path_under_the_mount() {
        assert_eq!(
            v2_dir("/user.slice/a.scope"),
            PathBuf::from("/sys/fs/cgroup/user.slice/a.scope")
        );
        assert_eq!(v2_dir("/"), PathBuf::from("/sys/fs/cgroup"));
    }

    /// Fresh leaf counters read 0 and a plain SIGKILL leaves them there; a leaf-local OOM kill reads `oom 1` and `oom_kill 1`; a kill with the leaf's own `oom` at 0 (an ancestor's limit) does not certify.
    #[test]
    fn local_oom_needs_the_leafs_own_oom_and_oom_kill() {
        let fresh = Events::parse("low 0\nhigh 0\nmax 0\noom 0\noom_kill 0\noom_group_kill 0\n")
            .expect("counters");
        assert!(!local_oom(&fresh), "a plain SIGKILL certifies nothing");
        let limit_kill =
            Events::parse("low 0\nhigh 0\nmax 1\noom 1\noom_kill 1\noom_group_kill 1\n")
                .expect("counters");
        assert!(
            local_oom(&limit_kill),
            "the leaf's own limit killed a member"
        );
        let ancestor_kill =
            Events::parse("low 0\nhigh 0\nmax 0\noom 0\noom_kill 1\noom_group_kill 0\n")
                .expect("counters");
        assert!(
            !local_oom(&ancestor_kill),
            "an ancestor's limit kill is not the leaf's"
        );
    }

    /// A line written by the supervisor reads back as the same record; the reader takes the last record of a log; an incomplete record is an error with its reason, and a log with no record is an error too.
    #[test]
    fn the_record_line_reads_back_and_an_incomplete_one_is_rejected() {
        let record = Record {
            unit: "atl-row-7-1.scope".to_owned(),
            exit: Exit::Signal(9),
            wall_ms: 5,
            cpu_usec: 6,
            mem_peak: 7,
            events: Events {
                max: 1,
                high: 0,
                oom: 1,
                oom_kill: 1,
                oom_group_kill: 1,
            },
            limit: Some(67_108_864),
        };
        let line = record_line(&record.unit, record.exit, &Ok(record.clone()));
        assert_eq!(Record::read_last(&line), Ok(record));
        let old = "scope-record: unit=atl-row-7-1.scope exit=code:0 wall_ms=1 cpu_usec=2 mem_peak=3 max=0 high=0 oom=0 oom_kill=0 oom_group_kill=0 limit=none complete=1\n";
        let later = "scope-record: unit=atl-row-7-1.scope exit=code:4 wall_ms=9 cpu_usec=8 mem_peak=7 max=0 high=0 oom=0 oom_kill=0 oom_group_kill=0 limit=none complete=1\n";
        assert_eq!(
            Record::read_last(&format!("{old}gates: row\n{later}")).map(|r| r.exit),
            Ok(Exit::Code(4))
        );
        let incomplete = record_line(
            "atl-row-7-1.scope",
            Exit::Code(0),
            &Err("membership: 2 process(es) outside the workload's leaf: 41 42".to_owned()),
        );
        let why =
            Record::read_last(&incomplete).expect_err("an incomplete record is not a measurement");
        assert!(why.starts_with("membership:"), "{why}");
        assert!(
            Record::read_last("gates: row ok\n").is_err(),
            "no record: the supervisor did not finish"
        );
    }

    /// A managed leaf is `workload` under a managed scope; a gate's scope, a managed scope's supervisor, and a service are not.
    #[test]
    fn a_managed_leaf_is_named_by_its_scope() {
        assert!(is_managed_leaf(Path::new(
            "/sys/fs/cgroup/user.slice/app.slice/atl-row-7-1.scope/workload"
        )));
        assert!(!is_managed_leaf(Path::new(
            "/sys/fs/cgroup/user.slice/atl-gate-7.scope/workload"
        )));
        assert!(!is_managed_leaf(Path::new(
            "/sys/fs/cgroup/atl-row-7-1.scope/supervisor"
        )));
        assert!(!is_managed_leaf(Path::new(
            "/sys/fs/cgroup/atl-row-7-1.service/workload"
        )));
    }
}
