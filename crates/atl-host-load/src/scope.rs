// SPDX-License-Identifier: MIT
//! A gate's scope: the cgroup v2 dir of the transient user scope `systemd-run --user --scope` makes. The sampler reads `cpu.pressure`, `cpu.stat` and `memory.peak` there.
//! A test gate relaunches itself into `atl-gate-<pid>.scope` when its host can make one. The exec keeps the pid, so the relaunched process recognises its unit ([`relaunched`]) and never relaunches again.
#![expect(
    clippy::disallowed_types,
    clippy::disallowed_methods,
    reason = "systemd-run moves this test-driver process into a cgroup and the exec replaces only that driver, which runs no sandboxed app; the apps it starts still go through the broker (clippy.toml)"
)]

use std::convert::Infallible;
use std::ffi::OsString;
use std::fs;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

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

    /// The unit's name: the dir's last component, e.g. `atl-gate-1234.scope`.
    pub fn unit(&self) -> &str {
        self.dir.file_name().and_then(|n| n.to_str()).unwrap_or("?")
    }
}

/// The gate scope this process runs in (its own, or the gate's that started it), when it has the cpu files. Any other scope is the job's: Err.
pub fn own() -> Result<Scope, String> {
    gate_scope(cgroup_dir(std::process::id())?)
}

/// `dir` as a gate's scope: an `atl-gate-<pid>.scope` with the cpu files.
fn gate_scope(dir: PathBuf) -> Result<Scope, String> {
    let is_gate = dir
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with(UNIT_PREFIX) && n.ends_with(".scope"));
    if !is_gate {
        return Err(format!("{}: not a gate's scope", dir.display()));
    }
    Scope::at(dir)
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
pub fn leftovers(dir: &Path, settle: Duration) -> Result<Vec<String>, String> {
    let path = dir.join("cgroup.procs");
    let own = std::process::id().to_string();
    let until = Instant::now() + settle;
    loop {
        let text =
            fs::read_to_string(&path).map_err(|e| format!("{} unreadable: {e}", path.display()))?;
        let pids: Vec<&str> = text.lines().filter(|p| *p != own).collect();
        if pids.is_empty() {
            return Ok(Vec::new());
        }
        if Instant::now() >= until {
            return Ok(pids.into_iter().map(with_command).collect());
        }
        std::thread::sleep(LEFTOVER_POLL);
    }
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
}
