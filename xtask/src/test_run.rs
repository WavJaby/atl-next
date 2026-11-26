// SPDX-License-Identifier: MIT
//! The run half of check step `test`: the built test binaries at once, as many as the grant has slots, where `cargo test` runs them one after another. Doc tests stay cargo's own run.
//!
//! Args, cwd and environment of each binary are cargo's own: [`record`] stands in as cargo's target runner (<https://doc.rust-lang.org/cargo/reference/config.html#targettriplerunner>) and writes down what cargo gave it instead of running the binary. The recorded set must equal the test executables cargo's messages name, so no binary is lost on the way.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::{Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::unit_use::{self, Echo, Units};
use crate::{build_pool, par, timing, verdict};

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
    /// `<package> <kind> <target>`
    label: String,
    /// cargo's target kind: `lib`, `bin`, `test`, ...
    kind: String,
    run: Recorded,
    /// Its test threads, and the slots it takes while it runs ([`weigh`]).
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

fn run_in(root: &Path, scratch: &Path) -> Result<()> {
    let binaries = list(root, scratch)?;
    let slots = build_pool::held_jobs().map_or_else(
        || std::thread::available_parallelism().map(usize::from),
        |n| Ok(n as usize),
    )?;
    let started = Instant::now();
    let binaries = weigh(binaries, slots)?;
    let (beside, alone) = split(&binaries)?;
    let stdout = Mutex::new(std::io::stdout());
    let done = run_sets(&beside, &alone, slots, scratch, &|d| show_to(&stdout, d));
    let binaries_took = started.elapsed();
    let started = Instant::now();
    // `--doc` runs the doc tests of a lib that sets `doctest = false` too: excluded, as `cargo test` leaves them out
    let skip = crate::workspace::load(root)?.no_doctests;
    let mut args = vec!["test", "--doc", "--workspace", "--locked"];
    for package in &skip {
        args.extend(["--exclude", package]);
    }
    let doc = crate::check::cargo(root, &args, Units::Recorded);
    let tests: Counts = done.iter().map(|d| d.counts).sum();
    eprintln!(
        "test: {} binaries, {} passed, {} failed, {} ignored, {} filtered out ({} at a time, {}); doc tests {}",
        done.len(),
        tests.passed,
        tests.failed,
        tests.ignored,
        tests.filtered,
        slots,
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

/// Each binary's test threads = its test count, at most `slots`; a binary without libtest (`harness = false`) runs as one. Largest first, so the long ones start at once and the small ones fill the slots they leave.
fn weigh(binaries: Vec<Binary>, slots: usize) -> Result<Vec<Binary>> {
    let counts = par::map(&binaries, slots, |b| -> Result<usize> {
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
    let mut weighed = binaries
        .into_iter()
        .zip(counts)
        .map(|(mut b, n)| {
            b.threads = n?.clamp(1, slots);
            Ok(b)
        })
        .collect::<Result<Vec<_>>>()?;
    weighed.sort_by_key(|b| std::cmp::Reverse(b.threads));
    Ok(weighed)
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

/// (run at once, run alone), by [`SERIAL`].
fn split(binaries: &[Binary]) -> Result<(Vec<&Binary>, Vec<&Binary>)> {
    let names: BTreeSet<(&str, &str)> = binaries
        .iter()
        .map(|b| (b.package.as_str(), b.target.as_str()))
        .collect();
    if let Some((package, target, _)) = SERIAL.iter().find(|(p, t, _)| !names.contains(&(*p, *t))) {
        bail!("test_run.rs SERIAL names {package} {target}: no such test binary");
    }
    Ok(binaries.iter().partition(|b| {
        !SERIAL
            .iter()
            .any(|(p, t, _)| *p == b.package && *t == b.target)
    }))
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

/// One binary that ran (or could not).
struct Done {
    label: String,
    status: Result<ExitStatus, String>,
    output: Vec<u8>,
    took: Duration,
    counts: Counts,
    /// libtest announced a run (`running N tests`) and printed no result for it
    cut_short: bool,
}

impl Done {
    /// Why the binary is red.
    fn problem(&self) -> Option<String> {
        let why = match &self.status {
            Err(e) => e.clone(),
            Ok(s) if !s.success() => s.to_string(),
            Ok(_) if self.cut_short => "ended without a test result".to_owned(),
            Ok(_) => return None,
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
        let mut block = format!("test binary {} ({took}) {state}\n", self.label).into_bytes();
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

/// `beside` within `slots` test threads at a time, then `alone` one by one.
fn run_sets(
    beside: &[&Binary],
    alone: &[&Binary],
    slots: usize,
    scratch: &Path,
    show: &(dyn Fn(&Done) + Sync),
) -> Vec<Done> {
    let mut done = run_all(beside, 0, slots, scratch, show);
    done.extend(run_all(alone, beside.len(), 1, scratch, show));
    done
}

/// Runs `binaries` in order, each started once its threads (at most `slots`) fit in the `slots` free: test threads at once never exceed `slots`. Each is shown whole as it ends; `first` numbers their output files.
fn run_all(
    binaries: &[&Binary],
    first: usize,
    slots: usize,
    scratch: &Path,
    show: &(dyn Fn(&Done) + Sync),
) -> Vec<Done> {
    let numbered: Vec<(usize, &&Binary)> =
        binaries.iter().zip(first..).map(|(b, i)| (i, b)).collect();
    let free = (Mutex::new(slots), Condvar::new());
    // a worker per slot: every binary takes one at least, so no more can run at once
    par::map(&numbered, slots, |(i, binary)| {
        let threads = binary.threads.clamp(1, slots);
        let (left, freed) = &free;
        {
            let mut left = left.lock().unwrap_or_else(PoisonError::into_inner);
            while *left < threads {
                left = freed.wait(left).unwrap_or_else(PoisonError::into_inner);
            }
            *left -= threads;
        }
        let started = Instant::now();
        // one file per binary: outputs of binaries at once never share one
        let log = scratch.join(format!("{i}.log"));
        let ran = execute(&binary.run, threads, &log);
        *left.lock().unwrap_or_else(PoisonError::into_inner) += threads;
        freed.notify_all();
        let (status, output) = match ran {
            Ok((status, output)) => (Ok(status), output),
            Err(e) => (Err(format!("{e:#}")), Vec::new()),
        };
        let (counts, cut_short) = read(&String::from_utf8_lossy(&output));
        let done = Done {
            label: binary.label.clone(),
            status,
            output,
            took: started.elapsed(),
            counts,
            cut_short,
        };
        show(&done);
        done
    })
}

/// Output to a file, not a pipe: one open file keeps the order of both streams, and a process the test leaves behind cannot hold us.
fn execute(run: &Recorded, threads: usize, log: &Path) -> Result<(ExitStatus, Vec<u8>)> {
    let file = fs::File::create(log).with_context(|| log.display().to_string())?;
    let status = Command::new(&run.argv[0])
        .args(&run.argv[1..])
        .current_dir(&run.cwd)
        .env_clear()
        .envs(&run.env)
        // the slots it holds (run_all), not the grant's count cargo's env carries: binaries at once would run slots^2 test threads
        .env("RUST_TEST_THREADS", threads.to_string())
        // no input: check runs unattended, and a test reading stdin must see EOF, not the terminal
        .stdin(Stdio::null())
        .stdout(Stdio::from(file.try_clone()?))
        .stderr(Stdio::from(file))
        .status()
        .with_context(|| format!("spawning {}", run.argv[0]))?;
    Ok((status, fs::read(log)?))
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

    /// A sink that counts its write calls: [`show_to`] gives each block one call under the lock, so blocks never mix (std's Mutex is not fair, so a mixed write shows only by chance; the count shows it always).
    #[derive(Default)]
    struct Slow(Vec<u8>, usize);

    impl Write for Slow {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            std::thread::sleep(Duration::from_millis(2));
            self.0.extend_from_slice(buf);
            self.1 += 1;
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

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

    fn run_shells(name: &str, binaries: &[Binary], at_once: usize) -> (Vec<Done>, String) {
        let dir =
            std::env::temp_dir().join(format!("xtask-test-run-{name}-{}", std::process::id()));
        fs::create_dir_all(&dir).expect("test dir");
        let log = Mutex::new(Slow::default());
        let refs: Vec<&Binary> = binaries.iter().collect();
        let done = run_all(&refs, 0, at_once, &dir, &|d| show_to(&log, d));
        fs::remove_dir_all(&dir).expect("test dir cleanup");
        let Slow(log, writes) = log.into_inner().expect("log");
        let log = String::from_utf8(log).expect("utf-8");
        assert_eq!(writes, done.len(), "one write per block:\n{log}");
        (done, log)
    }

    #[test]
    fn a_red_binary_is_named_with_its_output() {
        let (done, log) = run_shells(
            "red",
            &[
                shell(
                    "good",
                    "echo 'running 1 test'; echo 'test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s'",
                ),
                shell("bad", "echo 'test a::b ... FAILED'; exit 101"),
                shell("killed", "kill -9 $$"),
                shell("cut", "echo 'running 2 tests'; exit 0"),
            ],
            4,
        );
        let problems: Vec<String> = done.iter().filter_map(Done::problem).collect();
        assert_eq!(problems.len(), 3, "{problems:?}");
        assert_eq!(problems[0], "bad: test a::b ... FAILED (exit status: 101)");
        assert!(problems[1].starts_with("killed: signal: 9"), "{problems:?}");
        assert_eq!(problems[2], "cut: ended without a test result");
        assert!(log.contains("test a::b ... FAILED\n"), "{log}");
    }

    /// Two run together (each waits for a file the other writes), never more than `at_once` (each holds one directory of the one name).
    #[test]
    fn binaries_run_at_once_and_their_output_is_not_interleaved() {
        let dir = std::env::temp_dir().join(format!("xtask-test-run-files-{}", std::process::id()));
        fs::create_dir_all(&dir).expect("test dir");
        let at = dir.display();
        let meet = |me: &str, other: &str| {
            let wait = format!(
                "i=0; while [ ! -e {other} ]; do i=$((i+1)); [ $i -gt 500 ] && exit 3; sleep 0.02; done"
            );
            let talk = format!(
                "for i in 1 2 3 4 5; do echo {me} out $i; echo {me} err $i >&2; sleep 0.02; done"
            );
            shell(me, &format!("cd {at}; touch {me}; {wait}; {talk}"))
        };
        let (done, log) = run_shells("meet", &[meet("a", "b"), meet("b", "a")], 2);
        assert!(done.iter().all(|d| d.problem().is_none()), "{log}");
        for me in ["a", "b"] {
            let want: String = (1..=5)
                .map(|i| {
                    format!(
                        "{me} out {i}
{me} err {i}
"
                    )
                })
                .collect();
            assert!(log.contains(&want), "{log}");
        }
        let hold = shell(
            "s",
            &format!("cd {at}; mkdir one || exit 4; sleep 0.05; rmdir one"),
        );
        let (done, log) = run_shells("bound", &vec![hold; 4], 1);
        assert!(done.iter().all(|d| d.problem().is_none()), "{log}");
        fs::remove_dir_all(&dir).expect("test dir cleanup");
    }

    /// Binaries that end together: each block comes out whole through the writer, though the sink takes one slow write at a time.
    #[test]
    fn blocks_that_end_together_stay_whole() {
        let dir = std::env::temp_dir().join(format!("xtask-test-run-ends-{}", std::process::id()));
        fs::create_dir_all(&dir).expect("test dir");
        let at = dir.display();
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
        let (done, log) = run_shells("together", &binaries, 6);
        fs::remove_dir_all(&dir).expect("test dir cleanup");
        assert!(done.iter().all(|d| d.problem().is_none()), "{log}");
        for n in 0..6 {
            let want: String = (1..=8).map(|i| format!("b{n} line {i}\n")).collect();
            assert!(log.contains(&want), "{log}");
        }
    }

    /// Test threads at once stay within the slots: `slots` binaries in flight (each takes one of `slots` directories), each with one test thread.
    #[test]
    fn test_threads_stay_within_the_slots() {
        let dir =
            std::env::temp_dir().join(format!("xtask-test-run-budget-{}", std::process::id()));
        fs::create_dir_all(&dir).expect("test dir");
        let at = dir.display();
        // a binary claims one of four token directories per test thread it was given; fewer free than it needs = the budget was overrun
        let take = format!(
            "[ \"$RUST_TEST_THREADS\" = \"$WANT\" ] || exit 8; cd {at}; got=; for n in 1 2 3 4; do [ $(echo $got | wc -w) -ge $WANT ] && break; mkdir t$n 2>/dev/null && got=\"$got t$n\"; done; [ $(echo $got | wc -w) -ge $WANT ] || exit 7; sleep 0.05; rmdir $got"
        );
        let binaries: Vec<Binary> = [3, 2, 1, 4, 1, 2, 3, 1]
            .into_iter()
            .enumerate()
            .map(|(i, threads)| {
                let mut b = shell(&format!("t{i}"), &take);
                b.threads = threads;
                b.run.env.insert("WANT".to_owned(), threads.to_string());
                // the grant's count as cargo's env gives it: the runner must replace it
                b.run
                    .env
                    .insert("RUST_TEST_THREADS".to_owned(), "16".to_owned());
                b
            })
            .collect();
        let (done, log) = run_shells("slots", &binaries, 4);
        fs::remove_dir_all(&dir).expect("test dir cleanup");
        assert!(done.iter().all(|d| d.problem().is_none()), "{log}");
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

    /// The serial set runs after the rest and one at a time, though slots are free.
    #[test]
    fn the_serial_set_runs_alone() {
        let dir = std::env::temp_dir().join(format!("xtask-test-run-alone-{}", std::process::id()));
        fs::create_dir_all(&dir).expect("test dir");
        let at = dir.display();
        let rest = shell(
            "rest",
            &format!("cd {at}; [ -e alone-ran ] && exit 6; touch rest-$$; sleep 0.1; rm rest-$$"),
        );
        let alone = shell(
            "alone",
            &format!(
                "cd {at}; ls rest-* 2>/dev/null && exit 5; mkdir one || exit 4; touch alone-ran; sleep 0.05; rmdir one"
            ),
        );
        let (beside, serial) = (vec![&rest; 3], vec![&alone; 3]);
        let log = Mutex::new(Slow::default());
        let done = run_sets(&beside, &serial, 8, &dir, &|d| show_to(&log, d));
        let log = String::from_utf8(log.into_inner().expect("log").0).expect("utf-8");
        fs::remove_dir_all(&dir).expect("test dir cleanup");
        assert_eq!(done.len(), 6);
        assert!(done.iter().all(|d| d.problem().is_none()), "{log}");
    }

    #[test]
    fn a_binary_gets_the_recorded_cwd_environment_and_args() {
        let mut b = shell("env", "echo \"$(pwd) $X $1 ${HOME-unset}\"");
        b.run.argv.extend(["sh".to_owned(), "arg".to_owned()]);
        b.run.cwd = PathBuf::from("/usr");
        b.run.env.insert("X".to_owned(), "x".to_owned());
        let (_, log) = run_shells("env", &[b], 1);
        assert!(log.contains("/usr x arg unset\n"), "{log}");
    }

    /// Both sides of the list: cargo's messages and what the runner recorded.
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
        // a plain (non-test) executable is not a test binary
        assert!(join(vec![recorded(&a)], &[message(&a, true), message(&b, false)]).is_ok());
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
        let (beside, alone) = split(&binaries).expect("every name is there");
        assert_eq!((beside.len(), alone.len()), (1, SERIAL.len()));
        let err = split(&binaries[1..]).expect_err("a name is gone");
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
