// SPDX-License-Identifier: MIT
//! The run half of check step `test`: the built test binaries, each admitted by its learned demand (build_pool.rs `acquire_demand`) and run in a managed scope (xtask-harness `test-binary`); the doc tests run under Cargo admission. Compile is the step's own phase (check.rs), listing runs under Cargo admission too.
//! Args, cwd and environment of each binary are cargo's own: [`record`] stands in as cargo's target runner (<https://doc.rust-lang.org/cargo/reference/config.html#targettriplerunner>) and writes down what cargo gave it instead of running the binary. The recorded set must equal the test executables cargo's messages name, so no binary is lost on the way.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, PoisonError};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail, ensure};
use atl_layout::Layout;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::build_pool::{self, Class, Demand};
use crate::gate_need::WINDOW;
use crate::resource_ledger::{self, RunRecord};
use crate::unit_use::{self, Echo, Units};
use crate::{par, timing, verdict};

/// Test targets run alone, after the others: (package, target, why). [`split`] refuses a name no built binary has.
const SERIAL: [(&str, &str, &str); 2] = [
    (
        "xtask-harness",
        "gate_leftovers",
        "its gate holds host-load waits against the clock: red beside 14 busy cores 3 of 40, alone 0 of 40",
    ),
    (
        "xtask-harness",
        "host_load_orphans",
        "starts a burner on every CPU: starves the tests beside it",
    ),
];

/// Bytes per GiB: the pool's memory unit.
const GIB: u64 = 1 << 30;
/// A binary's reservation: its largest passed peak plus this percent, rounded up to whole GiB. Initial value; to measure.
const HEADROOM_PERCENT: u64 = 25;

/// `xtask record-test`: cargo's runner while the binaries are listed.
#[derive(clap::Args)]
pub struct RecordArgs {
    dir: PathBuf,
    #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
    argv: Vec<String>,
}

/// What cargo gave one test binary.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Recorded {
    argv: Vec<String>,
    cwd: PathBuf,
    env: BTreeMap<String, String>,
}

pub fn record(args: &RecordArgs) -> Result<()> {
    let env = std::env::vars_os()
        .map(|(k, v)| Ok((utf8(k)?, utf8(v)?)))
        .collect::<Result<_>>()?;
    let recorded = Recorded {
        argv: args.argv.clone(),
        cwd: std::env::current_dir().context("test cwd")?,
        env,
    };
    // cargo runs one runner at a time: the count names the next in cargo's order
    let n = fs::read_dir(&args.dir)?.count();
    let path = args.dir.join(format!("{n:05}.json"));
    fs::write(&path, serde_json::to_vec(&recorded)?).with_context(|| path.display().to_string())
}

fn utf8(s: std::ffi::OsString) -> Result<String> {
    s.into_string()
        .map_err(|s| anyhow::anyhow!("test environment is not UTF-8: {}", s.display()))
}

/// One built test binary.
#[derive(Debug, Clone)]
struct Binary {
    package: String,
    target: String,
    /// `<package> <kind> <target>`: its name in the ledger
    label: String,
    /// cargo's target kind: `lib`, `bin`, `test`, ...
    kind: String,
    run: Recorded,
    /// Its libtest threads: its test count, at most the pool's CPU slots (a `harness = false` binary runs as one).
    threads: usize,
}

/// Runs every test of the workspace; an error names each red binary.
pub fn run(root: &Path) -> Result<()> {
    let scratch = crate::out::cargo_target_dir(root)
        .join("check-test")
        .join(std::process::id().to_string());
    fs::create_dir_all(&scratch).with_context(|| scratch.display().to_string())?;
    let result = run_in(root, &scratch);
    let _ = fs::remove_dir_all(&scratch);
    result
}

/// The run's shared state: the pool, the harness that runs a binary in a scope, and the run's id for the ledger.
struct Ctx {
    ws: PathBuf,
    root: PathBuf,
    harness: PathBuf,
    scratch: PathBuf,
    /// the pool's memory a demand may reserve (GiB) and its CPU slots
    budget: u32,
    cpu: u32,
    /// unique per run: the ledger's attempt ids are `<run>-<index>`
    run: String,
    /// how an attempt is launched: a managed scope, or (tests only) the binary directly
    launch: Launch,
}

enum Launch {
    Scope,
    #[cfg(test)]
    Direct,
}

fn run_in(root: &Path, scratch: &Path) -> Result<()> {
    let ws = Layout::of(root)?.workspace().to_owned();
    let (budget, cpu) = build_pool::demand_budget(&ws)?;
    let mut binaries = {
        let _compile = build_pool::acquire(&ws, root, Class::Cargo)?;
        list(root, scratch)?
    };
    test_threads(&mut binaries, cpu)?;
    let harness = crate::harness::build(root)?;
    let runs = resource_ledger::read_runs(&ws)?;
    let ctx = Ctx {
        ws,
        root: root.to_owned(),
        harness,
        scratch: scratch.to_owned(),
        budget,
        cpu,
        run: format!(
            "{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        ),
        launch: Launch::Scope,
    };
    let items = plan(&binaries, &runs, budget, cpu)?;
    let started = Instant::now();
    let stdout = Mutex::new(std::io::stdout());
    let show = |d: &Done| show_to(&stdout, d);
    let beside: Vec<&Item> = items.iter().filter(|i| !i.serial).collect();
    let alone: Vec<&Item> = items.iter().filter(|i| i.serial).collect();
    let mut done = dispatch(&ctx, &beside, cpu as usize, &show);
    done.extend(dispatch(&ctx, &alone, 1, &show));
    let done = replay(&ctx, &items, done, &show);
    let binaries_took = started.elapsed();
    // `--doc` runs the doc tests of a lib that sets `doctest = false` too: excluded, as `cargo test` leaves them out
    let started = Instant::now();
    let skip = crate::workspace::load(root)?.no_doctests;
    let mut args = vec!["test", "--doc", "--workspace", "--locked"];
    for package in &skip {
        args.extend(["--exclude", package]);
    }
    let doc = {
        let _held = build_pool::acquire(&ctx.ws, root, Class::Cargo)?;
        crate::check::cargo(root, &args, Units::Recorded)
    };
    let tests: Counts = done.iter().map(|d| d.counts).sum();
    eprintln!(
        "test: {} binaries, {} passed, {} failed, {} ignored, {} filtered out ({} CPU slots, {}); doc tests {}",
        done.len(),
        tests.passed,
        tests.failed,
        tests.ignored,
        tests.filtered,
        cpu,
        timing::secs(binaries_took),
        timing::secs(started.elapsed())
    );
    eprintln!("{}", slowest(&done));
    let mut problems: Vec<String> = done.iter().filter_map(Done::problem).collect();
    if let Err(e) = doc {
        problems.push(format!("doc tests: {e:#}"));
    }
    ensure!(problems.is_empty(), "{}", problems.join("\n"));
    Ok(())
}

/// The test binaries of the workspace as cargo would run them (compiled first where not current).
fn list(root: &Path, scratch: &Path) -> Result<Vec<Binary>> {
    let dir = scratch.join("list");
    fs::create_dir(&dir).with_context(|| dir.display().to_string())?;
    // the running image, not its path: the build may re-link target/debug/xtask
    let runner = format!(
        "target.'cfg(all())'.runner=[\"/proc/{}/exe\",\"record-test\",{:?}]",
        std::process::id(),
        dir.to_str().context("target dir is not UTF-8")?
    );
    let mut cmd = crate::cargo_command();
    // --tests: every target with `test = true`, all `cargo test` runs but the doc tests, whose runner must be real
    cmd.args(["test", "--workspace", "--locked", "--tests", "--config"])
        .arg(&runner)
        .current_dir(root);
    let cargo = unit_use::run(&mut cmd, Units::Recorded, Echo::Nothing)?;
    if !cargo.status.success() {
        let exit = format!(
            "listing the test binaries: cargo test exited with {}",
            cargo.status
        );
        // the cause leads: check's verdict quotes the first line
        match verdict::first_failure_line(&format!("{}{}", cargo.stdout, cargo.stderr)) {
            Some(line) => bail!("{line}\n{exit}"),
            None => bail!("{exit}\n{}{}", cargo.stdout, cargo.stderr),
        }
    }
    let mut paths: Vec<PathBuf> = fs::read_dir(&dir)?
        .map(|e| Ok(e?.path()))
        .collect::<Result<_>>()?;
    paths.sort();
    let recorded = paths
        .iter()
        .map(|p| Ok(serde_json::from_slice(&fs::read(p)?)?))
        .collect::<Result<Vec<Recorded>>>()?;
    join(recorded, &cargo.messages)
}

/// The recorded runs under the names of cargo's messages; a test executable on one side only is an error.
fn join(recorded: Vec<Recorded>, messages: &[Value]) -> Result<Vec<Binary>> {
    let mut named = BTreeMap::new();
    for m in messages {
        if m["reason"] != "compiler-artifact" || m["profile"]["test"] != true {
            continue;
        }
        let Some(exe) = m["executable"].as_str() else {
            continue;
        };
        let package = m["package_id"]
            .as_str()
            .and_then(unit_use::package_name)
            .context("artifact without a package")?;
        let target = m["target"]["name"]
            .as_str()
            .context("artifact without a target name")?;
        let kind = m["target"]["kind"][0].as_str().unwrap_or("?");
        named.insert(
            fs::canonicalize(exe).with_context(|| exe.to_owned())?,
            (package, target.to_owned(), kind.to_owned()),
        );
    }
    let mut binaries = Vec::new();
    for run in recorded {
        let exe = run.cwd.join(&run.argv[0]);
        let exe = fs::canonicalize(&exe).with_context(|| exe.display().to_string())?;
        let (package, target, kind) = named
            .remove(&exe)
            .with_context(|| format!("cargo ran {} but did not name it as built", exe.display()))?;
        binaries.push(Binary {
            label: format!("{package} {kind} {target}"),
            package,
            target,
            kind,
            run,
            threads: 1,
        });
    }
    let missing: Vec<String> = named.keys().map(|p| p.display().to_string()).collect();
    ensure!(
        missing.is_empty(),
        "built test binaries cargo did not run: {}",
        missing.join(", ")
    );
    ensure!(!binaries.is_empty(), "no test binaries");
    Ok(binaries)
}

/// Each binary's libtest threads: its test count, at most `cpu`; a binary without libtest (`harness = false`) runs as one. Counted in parallel, in the binaries' order.
fn test_threads(binaries: &mut [Binary], cpu: u32) -> Result<()> {
    let counts = par::map(binaries, cpu as usize, |b| -> Result<usize> {
        if !libtest(b)? {
            return Ok(1);
        }
        let out = Command::new(&b.run.argv[0])
            .args(&b.run.argv[1..])
            .args(["--list", "--format", "terse"])
            .current_dir(&b.run.cwd)
            .env_clear()
            .envs(&b.run.env)
            .stdin(Stdio::null())
            .output()
            .with_context(|| format!("listing {}", b.label))?;
        ensure!(out.status.success(), "listing {}: {}", b.label, out.status);
        Ok(String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter(|l| l.ends_with(": test"))
            .count())
    });
    for (b, n) in binaries.iter_mut().zip(counts) {
        b.threads = n?.clamp(1, cpu as usize);
    }
    Ok(())
}

/// Whether the binary runs libtest: its manifest sets no `harness = false` for the target (a `harness = false` binary would run its gate on `--list`).
fn libtest(b: &Binary) -> Result<bool> {
    let dir = b
        .run
        .env
        .get("CARGO_MANIFEST_DIR")
        .with_context(|| format!("{}: no CARGO_MANIFEST_DIR", b.label))?;
    let path = Path::new(dir).join("Cargo.toml");
    let manifest: toml::Table =
        toml::from_str(&fs::read_to_string(&path)?).with_context(|| path.display().to_string())?;
    Ok(!manifest_says_no_harness(&manifest, &b.kind, &b.target))
}

/// `harness = false` on target `name` of `kind`: `[lib]`, or the `[[bin]]` / `[[test]]` / `[[bench]]` / `[[example]]` entry of that name.
fn manifest_says_no_harness(manifest: &toml::Table, kind: &str, name: &str) -> bool {
    let off = |t: &toml::Value| t.get("harness").and_then(toml::Value::as_bool) == Some(false);
    if kind == "lib" {
        return manifest.get("lib").is_some_and(off);
    }
    manifest
        .get(kind)
        .and_then(toml::Value::as_array)
        .into_iter()
        .flatten()
        .any(|t| t.get("name").and_then(toml::Value::as_str) == Some(name) && off(t))
}

/// Every name in [`SERIAL`] must be a built binary: a name no binary has is refused.
fn check_serial_names(binaries: &[Binary]) -> Result<()> {
    let names: BTreeSet<(&str, &str)> = binaries
        .iter()
        .map(|b| (b.package.as_str(), b.target.as_str()))
        .collect();
    if let Some((package, target, _)) = SERIAL.iter().find(|(p, t, _)| !names.contains(&(*p, *t))) {
        bail!("test_run.rs SERIAL names {package} {target}: no such test binary");
    }
    Ok(())
}

fn is_serial(b: &Binary) -> bool {
    SERIAL
        .iter()
        .any(|(p, t, _)| *p == b.package && *t == b.target)
}

/// One binary's plan: its demand, the memory cap of its scope, and its place in the order (longest first).
struct Item<'a> {
    index: usize,
    binary: &'a Binary,
    demand: Demand,
    /// the leaf's `memory.max` in bytes: the reservation, the whole budget when cold
    memory_max: u64,
    /// the last passed run's wall time, ms; `u64::MAX` when cold (cold runs first)
    wall_ms: u64,
    serial: bool,
}

/// The items in run order: longest first. A SERIAL binary claims the whole budget (it runs alone).
fn plan<'a>(
    binaries: &'a [Binary],
    runs: &[RunRecord],
    budget: u32,
    cpu: u32,
) -> Result<Vec<Item<'a>>> {
    check_serial_names(binaries)?;
    let mut by_name: BTreeMap<&str, Vec<RunRecord>> = BTreeMap::new();
    for r in runs {
        by_name.entry(r.name.as_str()).or_default().push(r.clone());
    }
    let mut items: Vec<Item> = binaries
        .iter()
        .enumerate()
        .map(|(index, binary)| {
            let history = by_name
                .get(binary.label.as_str())
                .map_or(&[][..], Vec::as_slice);
            let (mut demand, mut wall_ms) = demand_of(history, budget, cpu);
            let serial = is_serial(binary);
            if serial {
                demand = Demand {
                    mem_gib: budget,
                    cores: f64::from(cpu),
                };
                wall_ms = u64::MAX;
            }
            Item {
                index,
                binary,
                memory_max: u64::from(demand.mem_gib) * GIB,
                demand,
                wall_ms,
                serial,
            }
        })
        .collect();
    items.sort_by_key(|i| std::cmp::Reverse(i.wall_ms));
    Ok(items)
}

/// A binary's demand from its ledger attempts, oldest first: the largest passed peak plus [`HEADROOM_PERCENT`], whole GiB (at least 1, at most `budget`); cores the largest passed average; the last passed run's wall time. Cold (no passed run), or the last attempt an OOM (an unresolved lower bound): the whole budget, alone.
fn demand_of(runs: &[RunRecord], budget: u32, cpu: u32) -> (Demand, u64) {
    let whole = (
        Demand {
            mem_gib: budget,
            cores: f64::from(cpu),
        },
        u64::MAX,
    );
    let passed: Vec<&RunRecord> = runs.iter().filter(|r| !r.oom).collect();
    if passed.is_empty() || runs.last().is_some_and(|r| r.oom) {
        return whole;
    }
    let peak = passed.iter().map(|r| r.mem_peak).max().unwrap_or(0);
    let want = (peak + peak * HEADROOM_PERCENT / 100).div_ceil(GIB).max(1);
    let mem_gib = u32::try_from(want).unwrap_or(u32::MAX).min(budget);
    let cores = passed.iter().map(|r| cores_of(r)).fold(0.0, f64::max);
    let wall_ms = passed.last().map_or(0, |r| r.wall_ms);
    (Demand { mem_gib, cores }, wall_ms)
}

/// A run's average cores: its CPU time over its wall time.
fn cores_of(r: &RunRecord) -> f64 {
    if r.wall_ms == 0 {
        0.0
    } else {
        r.cpu_usec as f64 / (r.wall_ms as f64 * 1000.0)
    }
}

/// How one attempt ended, as the managed scope recorded it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ended {
    Code(i64),
    Signal(i64),
    /// the workload never ran in its leaf
    Never,
}

/// A complete measurement of one attempt, or why it has none.
struct Measured {
    result: Result<Complete, String>,
}

struct Complete {
    ended: Ended,
    wall_ms: u64,
    cpu_usec: u64,
    mem_peak: u64,
    /// a limit OOM inside the leaf (certified by the scope's counters)
    oom: bool,
}

impl Measured {
    /// The launcher's result file (xtask-harness test_binary.rs).
    fn read(path: &Path) -> Result<Self> {
        let v: Value =
            serde_json::from_slice(&fs::read(path).with_context(|| path.display().to_string())?)?;
        if v["complete"] != true {
            return Ok(Self {
                result: Err(v["reason"].as_str().unwrap_or("no reason given").to_owned()),
            });
        }
        let ended = match (v["exit"]["code"].as_i64(), v["exit"]["signal"].as_i64()) {
            (Some(c), _) => Ended::Code(c),
            (None, Some(s)) => Ended::Signal(s),
            (None, None) => Ended::Never,
        };
        let count = |key: &str| v[key].as_u64().unwrap_or(0);
        Ok(Self {
            result: Ok(Complete {
                ended,
                wall_ms: count("wall_ms"),
                cpu_usec: count("cpu_usec"),
                mem_peak: count("mem_peak"),
                oom: v["certified"] == true,
            }),
        })
    }
}

/// One binary's attempt that ended (or could not run).
struct Done {
    index: usize,
    label: String,
    status: Result<Ended, String>,
    output: Vec<u8>,
    took: std::time::Duration,
    counts: Counts,
    /// libtest announced a run (`running N tests`) and printed no result for it
    cut_short: bool,
    /// the attempt was an OOM kill inside its leaf: a lower bound on the memory, and a retry candidate
    oom: bool,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct Counts {
    passed: u64,
    failed: u64,
    ignored: u64,
    filtered: u64,
}

impl std::iter::Sum for Counts {
    fn sum<I: Iterator<Item = Self>>(iter: I) -> Self {
        iter.fold(Self::default(), |a, b| Self {
            passed: a.passed + b.passed,
            failed: a.failed + b.failed,
            ignored: a.ignored + b.ignored,
            filtered: a.filtered + b.filtered,
        })
    }
}

impl Done {
    /// Why the binary is red.
    fn problem(&self) -> Option<String> {
        let why = match &self.status {
            Err(e) => e.clone(),
            Ok(Ended::Code(0)) if self.cut_short => "ended without a test result".to_owned(),
            Ok(Ended::Code(0)) => return None,
            Ok(Ended::Code(c)) => format!("exit status: {c}"),
            Ok(Ended::Signal(s)) => format!("signal: {s}"),
            Ok(Ended::Never) => "did not run".to_owned(),
        };
        let line = verdict::first_failure_line(&String::from_utf8_lossy(&self.output));
        // the failing line leads: check's verdict quotes the first line
        Some(match line {
            Some(line) => format!("{}: {line} ({why})", self.label),
            None => format!("{}: {why}", self.label),
        })
    }

    /// The binary's whole block of the log.
    fn block(&self) -> Vec<u8> {
        let state = if self.problem().is_some() {
            "RED"
        } else {
            "ok"
        };
        let took = timing::secs(self.took);
        let note = if self.oom { " OOM in its leaf" } else { "" };
        let mut block = format!("test binary {} ({took}){note} {state}\n", self.label).into_bytes();
        block.extend_from_slice(&self.output);
        if !block.ends_with(b"\n") {
            block.push(b'\n');
        }
        block
    }
}

/// One binary's block as one write under `out`'s lock, so blocks of binaries at once never mix; a closed stdout loses the echo, not the verdict.
fn show_to<W: Write>(out: &Mutex<W>, done: &Done) {
    let mut out = out.lock().unwrap_or_else(PoisonError::into_inner);
    let _ = out.write_all(&done.block()).and_then(|()| out.flush());
}

/// Runs `items` with `workers` at once, in order: each takes its demand grant (waiting its turn), runs, and is shown whole as it ends.
fn dispatch(
    ctx: &Ctx,
    items: &[&Item],
    workers: usize,
    show: &(dyn Fn(&Done) + Sync),
) -> Vec<Done> {
    par::map(items, workers, |item| {
        let log = ctx.scratch.join(format!("{}.log", item.index));
        let done = attempt(ctx, item, item.demand, item.memory_max, &log, "");
        show(&done);
        done
    })
}

/// Whether an attempt is replayed alone: only a certified OOM kill, whose leaf counters are the evidence. A SIGKILL without that evidence is one attempt, and red.
fn retries(d: &Done) -> bool {
    d.oom
}

/// The OOM replay: each binary whose attempt was an OOM kill runs once more, alone, with the whole budget in a fresh scope. Its attempt replaces the first; a second OOM is red. Runs after the first wave has drained.
fn replay(ctx: &Ctx, items: &[Item], done: Vec<Done>, show: &(dyn Fn(&Done) + Sync)) -> Vec<Done> {
    let retry: Vec<&Item> = done
        .iter()
        .filter(|d| retries(d))
        .filter_map(|d| items.iter().find(|i| i.index == d.index))
        .collect();
    let mut again = Vec::new();
    for item in retry {
        let whole = Demand {
            mem_gib: ctx.budget,
            cores: f64::from(ctx.cpu),
        };
        let log = ctx.scratch.join(format!("{}-retry.log", item.index));
        let retried = attempt(
            ctx,
            item,
            whole,
            u64::from(ctx.budget) * GIB,
            &log,
            "-retry",
        );
        show(&retried);
        again.push(retried);
    }
    let mut out: Vec<Done> = done
        .into_iter()
        .map(|d| match again.iter().position(|r| r.index == d.index) {
            Some(at) => again.swap_remove(at),
            None => d,
        })
        .collect();
    out.sort_by_key(|d| d.index);
    out
}

/// One attempt: waits for its grant, runs the binary in a managed scope capped at `memory_max`, releases the grant, and records a passed or OOM attempt in the ledger (a failed one is not demand).
fn attempt(
    ctx: &Ctx,
    item: &Item,
    demand: Demand,
    memory_max: u64,
    log: &Path,
    suffix: &str,
) -> Done {
    let binary = item.binary;
    let grant = match build_pool::acquire_demand(&ctx.ws, &ctx.root, demand) {
        Ok(grant) => grant,
        Err(e) => return refused(item, format!("{e:#}")),
    };
    let started = Instant::now();
    // the unit is named here so that a launcher lost abruptly can still be stopped (see `launch`)
    let unit = format!("atl-row-{}-{}{suffix}", ctx.run, item.index);
    let measured = launch(ctx, binary, binary.threads, memory_max, log, &unit);
    let took = started.elapsed();
    drop(grant);
    let output = fs::read(log).unwrap_or_default();
    let (counts, cut_short) = read(&String::from_utf8_lossy(&output));
    let (status, oom) = match measured {
        Ok(Measured { result: Ok(c) }) => {
            let record = RunRecord {
                name: binary.label.clone(),
                run: format!("{}-{}{suffix}", ctx.run, item.index),
                mem_peak: c.mem_peak,
                oom: c.oom,
                cpu_usec: c.cpu_usec,
                wall_ms: c.wall_ms,
            };
            let passed = c.ended == Ended::Code(0);
            if (passed || c.oom)
                && let Err(e) = resource_ledger::append_runs(&ctx.ws, &[record], WINDOW)
            {
                eprintln!("test: ledger not written for {}: {e:#}", binary.label);
            }
            (Ok(c.ended), c.oom)
        }
        Ok(Measured { result: Err(why) }) => (Err(format!("scope record: {why}")), false),
        Err(e) => (Err(format!("{e:#}")), false),
    };
    Done {
        index: item.index,
        label: binary.label.clone(),
        status,
        output,
        took,
        counts,
        cut_short,
        oom,
    }
}

/// A demand that could not be admitted: the binary is red without running.
fn refused(item: &Item, why: String) -> Done {
    Done {
        index: item.index,
        label: item.binary.label.clone(),
        status: Err(why),
        output: Vec::new(),
        took: std::time::Duration::ZERO,
        counts: Counts::default(),
        cut_short: false,
        oom: false,
    }
}

/// Stops the scope's unit, synchronously: its processes end whatever became of the launcher (a launcher killed abruptly leaves its workload running otherwise).
fn stop_unit(unit: &str) {
    let _ = Command::new("systemctl")
        .args(["--user", "stop"])
        .arg(format!("{unit}.scope"))
        .output();
}

/// The binary under its scope: the launcher (xtask-harness `test-binary`) runs it with cargo's cwd, environment and arguments, its output to `log`, and writes the measurement to a JSON file beside it.
fn launch(
    ctx: &Ctx,
    binary: &Binary,
    threads: usize,
    memory_max: u64,
    log: &Path,
    unit: &str,
) -> Result<Measured> {
    let run = &binary.run;
    let file = fs::File::create(log).with_context(|| log.display().to_string())?;
    let json = log.with_extension("json");
    let mut cmd = match ctx.launch {
        Launch::Scope => {
            let mut cmd = Command::new(&ctx.harness);
            cmd.args(["test-binary", "--result"])
                .arg(&json)
                .arg("--unit")
                .arg(unit)
                .arg("--memory-max")
                .arg(memory_max.to_string())
                .arg("--")
                .args(&run.argv);
            cmd
        }
        #[cfg(test)]
        Launch::Direct => {
            let mut cmd = Command::new(&run.argv[0]);
            cmd.args(&run.argv[1..]);
            cmd
        }
    };
    let status = cmd
        .current_dir(&run.cwd)
        .env_clear()
        .envs(&run.env)
        // the slots it holds (the libtest threads), not the grant's count cargo's env carries
        .env("RUST_TEST_THREADS", threads.to_string())
        // no input: check runs unattended, and a test reading stdin must see EOF, not the terminal
        .stdin(Stdio::null())
        .stdout(Stdio::from(file.try_clone()?))
        .stderr(Stdio::from(file))
        .status()
        .with_context(|| format!("spawning {}", binary.label))?;
    match ctx.launch {
        Launch::Scope => {
            // however the launcher ended (killed included), its unit ends here: a workload left running would hold memory its grant no longer covers
            stop_unit(unit);
            ensure!(status.success(), "the scope launcher exited {status}");
            Measured::read(&json)
        }
        #[cfg(test)]
        Launch::Direct => Ok(Measured {
            result: Ok(Complete {
                ended: exit_of(status),
                wall_ms: 0,
                cpu_usec: 0,
                mem_peak: 0,
                oom: false,
            }),
        }),
    }
}

#[cfg(test)]
fn exit_of(status: std::process::ExitStatus) -> Ended {
    use std::os::unix::process::ExitStatusExt;
    match (status.code(), status.signal()) {
        (Some(c), _) => Ended::Code(i64::from(c)),
        (None, Some(s)) => Ended::Signal(i64::from(s)),
        (None, None) => Ended::Never,
    }
}

/// The counts of libtest's result lines, and whether a run it announced has none. A binary without libtest (`harness = false`) announces none: its exit code alone counts, as for cargo.
fn read(output: &str) -> (Counts, bool) {
    let mut announced = 0usize;
    let mut ended = 0usize;
    let mut counts = Counts::default();
    for line in output.lines() {
        if let Some(n) = line.strip_prefix("running ") {
            let n = n.strip_suffix(" tests").or(n.strip_suffix(" test"));
            announced += usize::from(n.is_some_and(|n| n.parse::<u64>().is_ok()));
        }
        let Some(result) = line.strip_prefix("test result: ") else {
            continue;
        };
        ended += 1;
        let count = |what: &str| -> u64 {
            result
                .split([';', '.'])
                .find_map(|part| part.trim().strip_suffix(what)?.trim().parse().ok())
                .unwrap_or(0)
        };
        counts = [
            counts,
            Counts {
                passed: count("passed"),
                failed: count("failed"),
                ignored: count("ignored"),
                filtered: count("filtered out"),
            },
        ]
        .into_iter()
        .sum();
    }
    (counts, ended < announced)
}

/// The five slowest binaries: the run lasts at least as long as the first.
fn slowest(done: &[Done]) -> String {
    let mut by_time: Vec<&Done> = done.iter().collect();
    by_time.sort_by_key(|d| std::cmp::Reverse(d.took));
    let rows: Vec<String> = by_time
        .iter()
        .take(5)
        .map(|d| format!("{} {}", d.label, timing::secs(d.took)))
        .collect();
    format!("test: slowest: {}", rows.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shell(label: &str, script: &str) -> Binary {
        Binary {
            package: "p".to_owned(),
            target: label.to_owned(),
            label: label.to_owned(),
            kind: "test".to_owned(),
            threads: 1,
            run: Recorded {
                argv: ["sh", "-c", script].map(str::to_owned).to_vec(),
                cwd: std::env::temp_dir(),
                env: BTreeMap::from([("PATH".to_owned(), std::env::var("PATH").expect("PATH"))]),
            },
        }
    }

    /// A test workspace (pool of `cpu` slots and `mem` GiB) and a context that runs binaries directly.
    fn ctx(name: &str, cpu: u32, mem: u32) -> (Ctx, PathBuf) {
        let ws = std::env::temp_dir().join(format!("xtask-test-run-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&ws);
        fs::create_dir_all(build_pool::dir(&ws)).expect("pool dir");
        fs::write(
            ws.join("workspace.toml"),
            format!("[pool]\ncpu_slots = {cpu}\nmem_gib = {mem}\n"),
        )
        .expect("workspace.toml");
        let scratch = ws.join("scratch");
        fs::create_dir_all(&scratch).expect("scratch");
        let ctx = Ctx {
            ws: ws.clone(),
            root: ws.clone(),
            harness: PathBuf::new(),
            scratch,
            budget: mem,
            cpu,
            run: format!("test-{name}"),
            launch: Launch::Direct,
        };
        (ctx, ws)
    }

    /// Items of `binaries` in order, each with the given demand (a test sets it, not the ledger).
    fn items_with(binaries: &[Binary], mem_gib: u32, serial: bool) -> Vec<Item<'_>> {
        binaries
            .iter()
            .enumerate()
            .map(|(index, binary)| Item {
                index,
                binary,
                demand: Demand {
                    mem_gib,
                    cores: 1.0,
                },
                memory_max: u64::from(mem_gib) * GIB,
                wall_ms: 0,
                serial,
            })
            .collect()
    }

    fn run_items(ctx: &Ctx, items: &[Item], workers: usize) -> (Vec<Done>, String) {
        let log = Mutex::new(Vec::<u8>::new());
        let refs: Vec<&Item> = items.iter().collect();
        let done = dispatch(ctx, &refs, workers, &|d| {
            let mut l = log.lock().expect("log");
            l.extend_from_slice(&d.block());
        });
        let text = String::from_utf8(log.into_inner().expect("log")).expect("utf-8");
        (done, text)
    }

    fn done_of(status: Result<Ended, String>, oom: bool) -> Done {
        Done {
            index: 0,
            label: "p lib b".to_owned(),
            status,
            output: Vec::new(),
            took: std::time::Duration::ZERO,
            counts: Counts::default(),
            cut_short: false,
            oom,
        }
    }

    /// A certified OOM kill is replayed alone; its solo pass stands, so the binary is green.
    #[test]
    fn oom_then_solo_pass_is_green() {
        let first = done_of(Ok(Ended::Signal(9)), true);
        assert!(retries(&first), "a certified OOM is replayed alone");
        let retry = done_of(Ok(Ended::Code(0)), false);
        assert!(retry.problem().is_none(), "the solo pass is green");
    }

    /// The solo replay OOMs too: the binary is red.
    #[test]
    fn second_oom_is_red() {
        let retry = done_of(Ok(Ended::Signal(9)), true);
        assert!(retry.problem().is_some(), "a second OOM is red");
    }

    /// A SIGKILL without cgroup evidence is one attempt: not replayed, and red.
    #[test]
    fn a_sigkill_without_cgroup_evidence_gets_one_attempt() {
        let killed = done_of(Ok(Ended::Signal(9)), false);
        assert!(!retries(&killed), "no evidence, no replay");
        assert!(killed.problem().is_some(), "one attempt, red");
    }

    /// Items run longest first: a cold binary (no history, its wall unknown) leads, then the slower of two passed binaries.
    #[test]
    fn items_run_longest_first_and_a_cold_binary_leads() {
        let serial = |target: &str| {
            let mut b = shell(target, "true");
            b.package = "xtask-harness".to_owned();
            b
        };
        // the SERIAL names must be built (check_serial_names); they run alone and are left out of the order
        let binaries = [
            serial("gate_leftovers"),
            serial("host_load_orphans"),
            shell("fast", "true"),
            shell("cold", "true"),
            shell("slow", "true"),
        ];
        let run = |name: &str, wall_ms: u64| RunRecord {
            name: name.to_owned(),
            run: "r".to_owned(),
            mem_peak: 1 << 20,
            oom: false,
            cpu_usec: wall_ms * 1000,
            wall_ms,
        };
        let runs = [run("fast", 100), run("slow", 900)];
        let items = plan(&binaries, &runs, 20, 16).expect("plan");
        let order: Vec<&str> = items
            .iter()
            .filter(|i| !i.serial)
            .map(|i| i.binary.label.as_str())
            .collect();
        assert_eq!(order, ["cold", "slow", "fast"]);
    }

    #[test]
    fn a_red_binary_is_named_with_its_output() {
        let (ctx, ws) = ctx("red", 4, 10);
        let binaries = [
            shell(
                "good",
                "echo 'running 1 test'; echo 'test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s'",
            ),
            shell("bad", "echo 'test a::b ... FAILED'; exit 101"),
            shell("killed", "kill -9 $$"),
            shell("cut", "echo 'running 2 tests'; exit 0"),
        ];
        let (done, log) = run_items(&ctx, &items_with(&binaries, 1, false), 4);
        let problems: Vec<String> = done.iter().filter_map(Done::problem).collect();
        assert_eq!(problems.len(), 3, "{problems:?}");
        assert_eq!(problems[0], "bad: test a::b ... FAILED (exit status: 101)");
        assert!(problems[1].starts_with("killed: signal: 9"), "{problems:?}");
        assert_eq!(problems[2], "cut: ended without a test result");
        assert!(log.contains("test a::b ... FAILED\n"), "{log}");
        fs::remove_dir_all(&ws).expect("cleanup");
    }

    /// Two demands of 1 GiB in a pool of 2 run together (each waits for a file the other writes); a whole-budget demand runs alone.
    #[test]
    fn binaries_run_at_once_and_their_output_is_not_interleaved() {
        let (ctx, ws) = ctx("meet", 4, 2);
        let dir = ws.display().to_string();
        let meet = |me: &str, other: &str| {
            let wait = format!(
                "i=0; while [ ! -e {dir}/{other} ]; do i=$((i+1)); [ $i -gt 500 ] && exit 3; sleep 0.02; done"
            );
            let talk = format!(
                "for i in 1 2 3 4 5; do echo {me} out $i; echo {me} err $i >&2; sleep 0.02; done"
            );
            shell(me, &format!("cd {dir}; touch {me}; {wait}; {talk}"))
        };
        let (done, log) = run_items(
            &ctx,
            &items_with(&[meet("a", "b"), meet("b", "a")], 1, false),
            2,
        );
        assert!(done.iter().all(|d| d.problem().is_none()), "{log}");
        for me in ["a", "b"] {
            let want: String = (1..=5)
                .map(|i| format!("{me} out {i}\n{me} err {i}\n"))
                .collect();
            assert!(log.contains(&want), "{log}");
        }
        fs::remove_dir_all(&ws).expect("cleanup");
    }

    /// The SERIAL set runs after the rest and alone: its whole-budget demand holds every memory slot while it runs.
    #[test]
    fn the_serial_set_runs_alone() {
        let (ctx, ws) = ctx("alone", 8, 4);
        let dir = ws.display().to_string();
        let rest = shell(
            "rest",
            &format!("cd {dir}; [ -e alone-ran ] && exit 6; touch rest-$$; sleep 0.1; rm rest-$$"),
        );
        let alone = shell(
            "alone",
            &format!(
                "cd {dir}; ls rest-* 2>/dev/null && exit 5; mkdir one || exit 4; touch alone-ran; sleep 0.05; rmdir one"
            ),
        );
        let rest_bins = vec![rest; 3];
        let alone_bins = vec![alone; 3];
        let rest_items = items_with(&rest_bins, 1, false);
        let alone_items = items_with(&alone_bins, 4, true);
        let (mut done, _) = run_items(&ctx, &rest_items, 3);
        let (serial, log) = run_items(&ctx, &alone_items, 1);
        done.extend(serial);
        fs::remove_dir_all(&ws).expect("cleanup");
        assert_eq!(done.len(), 6);
        assert!(done.iter().all(|d| d.problem().is_none()), "{log}");
    }

    #[test]
    fn a_binary_gets_the_recorded_cwd_environment_and_args() {
        let (ctx, ws) = ctx("env", 2, 2);
        let mut b = shell("env", "echo \"$(pwd) $X $1 ${HOME-unset}\"");
        b.run.argv.extend(["sh".to_owned(), "arg".to_owned()]);
        b.run.cwd = PathBuf::from("/usr");
        b.run.env.insert("X".to_owned(), "x".to_owned());
        let (_, log) = run_items(&ctx, &items_with(&[b], 1, false), 1);
        assert!(log.contains("/usr x arg unset\n"), "{log}");
        fs::remove_dir_all(&ws).expect("cleanup");
    }

    /// Demand history: no passed run is cold (the whole budget, alone); a passed peak of 2.1 GiB reserves 3 GiB with headroom; a last OOM is unresolved (the whole budget).
    #[test]
    fn a_binary_s_demand_comes_from_its_ledger_history() {
        let run = |mem_peak: u64, oom: bool, wall_ms: u64| RunRecord {
            name: "p lib b".to_owned(),
            run: "r".to_owned(),
            mem_peak,
            oom,
            cpu_usec: wall_ms * 1000 * 2,
            wall_ms,
        };
        let gib = |g: f64| (g * GIB as f64) as u64;
        let (cold, _) = demand_of(&[], 20, 16);
        assert_eq!(cold.mem_gib, 20, "cold: the whole budget");
        let (warm, wall) = demand_of(&[run(gib(2.1), false, 900)], 20, 16);
        assert_eq!((warm.mem_gib, wall), (3, 900), "2.1 GiB + 25 % -> 3 GiB");
        assert!(
            (warm.cores - 2.0).abs() < 1e-9,
            "cpu over wall: {}",
            warm.cores
        );
        let (unresolved, _) = demand_of(
            &[run(gib(2.1), false, 900), run(gib(3.0), true, 500)],
            20,
            16,
        );
        assert_eq!(
            unresolved.mem_gib, 20,
            "an OOM as the last attempt: the whole budget"
        );
        let (resolved, _) = demand_of(
            &[run(gib(3.0), true, 500), run(gib(2.1), false, 900)],
            20,
            16,
        );
        assert_eq!(resolved.mem_gib, 3, "a passed run after an OOM resolves it");
    }

    /// Binaries that end together: each block comes out whole, one write each under the lock, though the sink takes one slow write at a time.
    #[test]
    fn blocks_that_end_together_stay_whole() {
        struct Slow(Vec<u8>, usize);
        impl Write for Slow {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                std::thread::sleep(std::time::Duration::from_millis(2));
                self.0.extend_from_slice(buf);
                self.1 += 1;
                Ok(buf.len())
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let (ctx, ws) = ctx("together", 6, 6);
        let meet = ws.join("meet");
        fs::create_dir_all(&meet).expect("meet dir");
        let at = meet.display().to_string();
        let binaries: Vec<Binary> = (0..6)
            .map(|n| {
                shell(
                    &format!("b{n}"),
                    &format!(
                        "for i in 1 2 3 4 5 6 7 8; do echo b{n} line $i; done; cd {at}; touch {n}; i=0; while [ $(ls | wc -l) -lt 6 ]; do i=$((i+1)); [ $i -gt 500 ] && exit 3; sleep 0.01; done"
                    ),
                )
            })
            .collect();
        let items = items_with(&binaries, 1, false);
        let refs: Vec<&Item> = items.iter().collect();
        let log = Mutex::new(Slow(Vec::new(), 0));
        let done = dispatch(&ctx, &refs, 6, &|d| show_to(&log, d));
        let Slow(text, writes) = log.into_inner().expect("log");
        let text = String::from_utf8(text).expect("utf-8");
        fs::remove_dir_all(&ws).expect("cleanup");
        assert_eq!(writes, done.len(), "one write per block:\n{text}");
        assert!(done.iter().all(|d| d.problem().is_none()), "{text}");
        for n in 0..6 {
            let want: String = (1..=8).map(|i| format!("b{n} line {i}\n")).collect();
            assert!(text.contains(&want), "{text}");
        }
    }

    #[test]
    fn harness_false_is_read_from_the_manifest() {
        let manifest: toml::Table = toml::from_str(
            "[lib]\nharness = false\n[[test]]\nname = \"g1\"\nharness = false\n[[test]]\nname = \"plain\"\n[[bin]]\nname = \"g1\"\n",
        )
        .expect("manifest");
        assert!(manifest_says_no_harness(&manifest, "lib", "x"));
        assert!(manifest_says_no_harness(&manifest, "test", "g1"));
        assert!(!manifest_says_no_harness(&manifest, "test", "plain"));
        assert!(!manifest_says_no_harness(&manifest, "test", "unlisted"));
        assert!(!manifest_says_no_harness(&manifest, "bin", "g1"));
    }

    /// A binary without a name in cargo's messages, or one cargo did not run, is refused.
    #[test]
    fn a_binary_missing_on_either_side_is_refused() {
        let dir = std::env::temp_dir().join(format!("xtask-test-run-join-{}", std::process::id()));
        fs::create_dir_all(&dir).expect("test dir");
        let exe = |name: &str| {
            let path = dir.join(name);
            fs::write(&path, "").expect("exe");
            path
        };
        let message = |exe: &Path, test: bool| serde_json::json!({"reason": "compiler-artifact", "package_id": "path+file:///w/crate-a#0.1.0", "target": {"name": "t", "kind": ["test"]}, "profile": {"test": test}, "executable": exe});
        let recorded = |exe: &Path| Recorded {
            argv: vec![exe.to_str().expect("utf-8").to_owned()],
            cwd: dir.clone(),
            env: BTreeMap::new(),
        };
        let (a, b) = (exe("a"), exe("b"));
        let both = [message(&a, true), message(&b, true)];
        let list = join(vec![recorded(&a), recorded(&b)], &both).expect("both sides");
        assert_eq!(list[0].label, "crate-a test t");
        let err = join(vec![recorded(&a)], &both).expect_err("b was not run");
        assert!(
            err.to_string()
                .ends_with(&format!("did not run: {}", b.display())),
            "{err}"
        );
        let err = join(vec![recorded(&a), recorded(&b)], &both[..1]).expect_err("b has no name");
        assert!(err.to_string().contains("did not name it"), "{err}");
        assert!(join(Vec::new(), &[]).is_err());
        fs::remove_dir_all(&dir).expect("test dir cleanup");
    }

    #[test]
    fn the_serial_set_names_real_tests_and_runs_apart() {
        let root = crate::test_root();
        for (package, target, _) in SERIAL {
            let source = root
                .join(package)
                .join("tests")
                .join(format!("{target}.rs"));
            assert!(source.is_file(), "SERIAL: no {}", source.display());
        }
        let mut binaries: Vec<Binary> = SERIAL
            .iter()
            .map(|(p, t, _)| Binary {
                package: (*p).to_owned(),
                ..shell(t, "")
            })
            .collect();
        binaries.push(shell("other", ""));
        check_serial_names(&binaries).expect("every name is there");
        assert_eq!(
            binaries.iter().filter(|b| is_serial(b)).count(),
            SERIAL.len()
        );
        let err = check_serial_names(&binaries[1..]).expect_err("a name is gone");
        assert!(err.to_string().contains(SERIAL[0].1), "{err}");
    }

    #[test]
    fn counts_come_from_the_result_lines() {
        let text = "running 3 tests\ntest result: ok. 2 passed; 0 failed; 1 ignored; 0 measured; 4 filtered out; finished in 0.50s\n";
        let want = Counts {
            passed: 2,
            failed: 0,
            ignored: 1,
            filtered: 4,
        };
        assert_eq!(read(text), (want, false));
        assert_eq!(read("running the gate\n"), (Counts::default(), false));
    }
}
