// SPDX-License-Identifier: MIT
//! Tracked APK scenarios: `xtask-harness/scenarios/<row>.drive`, `<row>` a test-apks.toml key (presence derived from the file, no table column), a drive script (drive.rs) with scenario headers.
//! `run-apk <row> --scenario` runs one (run_apk.rs `scenario`); `xtask test scenarios` runs every row's as its own run-apk process, sequentially, and collects the `scenario:` lines as check_gates does.
//! `check` step `tables` runs [`check`] (static: no run): the file names a row, parses, declares a known `control`, every `unreachable` carries a reason, every fixed `wait` follows a condition and is under [`FIXED_WAIT_MAX_MS`].

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::Instant;

use anyhow::{Context, Result, bail, ensure};

use crate::drive::{self, Script, Step};

/// The scenarios' dir, relative to the checkout root.
pub const DIR: &str = "xtask-harness/scenarios";
const EXT: &str = "drive";
/// The breaks a scenario may declare as its `control` (run_apk.rs `Args::under_control` maps each to its run-apk flag).
pub const CONTROLS: &[&str] = &[
    "break-input",
    "break-isa",
    "break-render-path",
    "break-webview-engine",
    "no-app-seccomp",
];
/// A fixed `wait` is a settle after a condition, never the condition itself: design apk-scenarios S3 bounds it below 3 s, so a slow host fails a condition wait with its name instead of racing a timer.
pub const FIXED_WAIT_MAX_MS: u64 = 3000;

/// The scenario file of `row`.
pub fn path(root: &Path, row: &str) -> PathBuf {
    root.join(DIR).join(format!("{row}.{EXT}"))
}

/// The fresh data root of `row`'s scenario runs under build dir `out` (emptied before each run).
pub fn data_root(out: &Path, row: &str) -> PathBuf {
    out.join("scenario").join(row).join("data")
}

/// `row`'s scenario, parsed and linted against the table.
pub fn load(root: &Path, row: &str) -> Result<Script> {
    let rows = xtask::test_apks::load(root)?;
    let file = path(root, row);
    let text = std::fs::read_to_string(&file).with_context(|| {
        format!(
            "{}: no scenario for row {row} (scenarios: {})",
            file.display(),
            rows_with_scenario(root).unwrap_or_default().join(", ")
        )
    })?;
    lint(row, &text, &|r| rows.contains_key(r)).with_context(|| file.display().to_string())
}

/// Rows that have a scenario file, sorted.
pub fn rows_with_scenario(root: &Path) -> Result<Vec<String>> {
    let dir = root.join(DIR);
    let mut rows = Vec::new();
    for entry in std::fs::read_dir(&dir).with_context(|| dir.display().to_string())? {
        let p = entry?.path();
        if p.extension().is_some_and(|e| e == EXT)
            && let Some(stem) = p.file_stem().and_then(|s| s.to_str())
        {
            rows.push(stem.to_owned());
        }
    }
    rows.sort();
    Ok(rows)
}

/// `check` step `tables`: every file of [`DIR`] is a `<row>.drive` that passes [`lint`]. Returns the count.
pub fn check(root: &Path) -> Result<usize> {
    let rows = xtask::test_apks::load(root)?;
    let dir = root.join(DIR);
    let mut n = 0;
    for entry in std::fs::read_dir(&dir).with_context(|| dir.display().to_string())? {
        let p = entry?.path();
        let name = p.display().to_string();
        let row = match (p.extension().and_then(|e| e.to_str()), p.file_stem()) {
            (Some(EXT), Some(stem)) => stem.to_str().context("file name is not UTF-8")?,
            _ => bail!("{name}: not a <row>.{EXT} file"),
        };
        let text = std::fs::read_to_string(&p).with_context(|| name.clone())?;
        lint(row, &text, &|r| rows.contains_key(r)).with_context(|| name.clone())?;
        n += 1;
    }
    Ok(n)
}

/// The static rules of a scenario `text` named `row`; `is_row`: whether a name is a test-apks.toml row.
pub fn lint(row: &str, text: &str, is_row: &dyn Fn(&str) -> bool) -> Result<Script> {
    ensure!(is_row(row), "{row} is no row of {}", xtask::test_apks::FILE);
    let script = drive::parse(text)?;
    ensure!(
        !script.controls.is_empty(),
        "no `control` line: a scenario names the break it must fail under ({})",
        CONTROLS.join(", ")
    );
    for c in &script.controls {
        ensure!(
            CONTROLS.contains(&c.as_str()),
            "control {c:?} is none of {}",
            CONTROLS.join(", ")
        );
    }
    lint_waits(&script.steps, false)?;
    lint_judges(&script.steps)?;
    Ok(script)
}

/// At least one unconditional expectation after the last input: a top-level `expect-*` other than `expect-fps` (judged only on its tier) after the last top-level input step or branch (a
/// branch may hold input). Without one, a run can pass having judged only waits and skipped steps.
fn lint_judges(steps: &[Step]) -> Result<()> {
    let last_input = steps
        .iter()
        .rposition(|s| drive::is_input(s) || matches!(s, Step::If { .. }));
    let after = last_input.map_or(steps, |i| &steps[i + 1..]);
    ensure!(
        after
            .iter()
            .any(|s| drive::is_expect(s) && !matches!(s, Step::ExpectFps { .. })),
        "no unconditional expect-* after the last input step: the run would judge nothing it caused (expect-fps does not count: it is skipped on another tier)"
    );
    Ok(())
}

fn is_condition(step: &Step) -> bool {
    match step {
        Step::WaitLog { .. } | Step::WaitChanged { .. } | Step::WaitStable { .. } => true,
        Step::If { cond, .. } => cond.waits(),
        _ => false,
    }
}

/// Every `wait` directly follows a condition wait (an if-log's body starts after its condition; an if-same judges at once, so its body does not) and stays under [`FIXED_WAIT_MAX_MS`].
fn lint_waits(steps: &[Step], after_condition: bool) -> Result<()> {
    let mut prev_condition = after_condition;
    for (i, step) in steps.iter().enumerate() {
        if let Step::Wait(ms) = step {
            ensure!(
                prev_condition,
                "step {}: `wait {ms}` follows no condition wait: wait on a log line or the picture instead",
                i + 1
            );
            ensure!(
                *ms < FIXED_WAIT_MAX_MS,
                "step {}: `wait {ms}` is not under {FIXED_WAIT_MAX_MS} ms",
                i + 1
            );
        }
        if let Step::If { cond, body } = step {
            lint_waits(body, cond.waits())
                .with_context(|| format!("in the branch of step {}", i + 1))?;
        }
        prev_condition = is_condition(step);
    }
    Ok(())
}

/// This machine's CPU for the verdict: `native(<model name>)` from /proc/cpuinfo (x86 `model name`, else arm64/riscv `uarch` / `CPU part`), `native(?)` when unreadable.
pub fn native_cpu() -> String {
    let info = std::fs::read_to_string("/proc/cpuinfo").unwrap_or_default();
    format!("native({})", cpu_model(&info).unwrap_or("?"))
}

fn cpu_model(info: &str) -> Option<&str> {
    ["model name", "uarch", "CPU part"].iter().find_map(|key| {
        info.lines().find_map(|l| {
            let (k, v) = l.split_once(':')?;
            (k.trim() == *key).then_some(v.trim())
        })
    })
}

/// One scenario run's verdict (design apk-scenarios S5).
pub struct Verdict<'a> {
    pub row: &'a str,
    pub host: &'a str,
    pub cpu: String,
    pub guest: String,
    /// Top-level checkpoints that judged and passed.
    pub passed: usize,
    /// Checkpoints that judged nothing, each with its reason (drive::Outcome::skipped).
    pub skipped: Vec<String>,
    pub total: usize,
    /// The script has `expect-fps` steps and the run measured none of them (every one's tier was not the run's).
    pub fps_unjudged: bool,
    pub fps: Option<f64>,
    pub tier: String,
    pub secs: u64,
    pub unreachable: Vec<&'a str>,
    /// None: ok.
    pub failure: Option<&'a str>,
}

impl Verdict<'_> {
    /// `scenario: ok|FAILED <row> host=<arch> cpu=<cpu> guest=<abi> <k> judged, <s> skipped (<reasons>), <n> checkpoints fps=<mean> tier=<t> <secs>s [FPS NOT JUDGED: ...]
    /// [unreachable: a, b]`, a failure's reason after `: `. Skipped checkpoints never count as judged.
    pub fn line(&self) -> String {
        let fps = self
            .fps
            .map_or_else(|| "-".to_owned(), |f| format!("{f:.1}"));
        let skipped = if self.skipped.is_empty() {
            "0 skipped".to_owned()
        } else {
            format!(
                "{} skipped ({})",
                self.skipped.len(),
                self.skipped.join("; ")
            )
        };
        let mut line = format!(
            "scenario: {} {} host={} cpu={} guest={} {} judged, {skipped}, {} checkpoints fps={fps} tier={} {}s",
            if self.failure.is_some() {
                "FAILED"
            } else {
                "ok"
            },
            self.row,
            self.host,
            self.cpu,
            self.guest,
            self.passed,
            self.total,
            self.tier,
            self.secs
        );
        if self.fps_unjudged {
            line.push_str(&format!(
                " [FPS NOT JUDGED: every expect-fps names another tier than {}]",
                self.tier
            ));
        }
        if !self.unreachable.is_empty() {
            line.push_str(&format!(" [unreachable: {}]", self.unreachable.join(", ")));
        }
        if let Some(f) = self.failure {
            line.push_str(&format!(": {f}"));
        }
        line
    }
}

#[derive(clap::Args)]
pub struct Args {
    /// The host arch (an arches.toml row with `[host]`), as for run-apk.
    #[arg(long)]
    host: String,
    /// Native build directory (default: this checkout's build dir, see `cargo xtask out show`); logs go to `<out>/scenarios/<row>.log`.
    #[arg(long)]
    out: Option<PathBuf>,
    /// Only these rows (comma-separated); each must have a scenario.
    #[arg(long, value_delimiter = ',')]
    only: Vec<String>,
    /// run-apk `--controls`: each row also runs under its declared controls, which must fail at a checkpoint.
    #[arg(long)]
    controls: bool,
}

/// `xtask test scenarios`: each selected row's scenario as its own `run-apk <row> --scenario` process, sequential; one `scenarios:` summary line.
pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    let all = rows_with_scenario(root)?;
    let rows: Vec<String> = if args.only.is_empty() {
        all
    } else {
        for r in &args.only {
            ensure!(
                all.contains(r),
                "--only {r}: no scenario (scenarios: {})",
                all.join(", ")
            );
        }
        args.only.clone()
    };
    let out = crate::build_state::gate_out(root, args.out.as_deref(), &args.host, "run-apk")?;
    let logs = out.join("scenarios");
    std::fs::create_dir_all(&logs).with_context(|| logs.display().to_string())?;
    let exe = std::env::current_exe().context("the harness's own path")?;
    let mut results = BTreeMap::new();
    for row in &rows {
        let log = logs.join(format!("{row}.log"));
        let file = std::fs::File::create(&log).with_context(|| log.display().to_string())?;
        let mut cmd = Command::new(&exe);
        cmd.current_dir(root)
            .args(["run-apk", row, "--host", &args.host, "--scenario", "--out"])
            .arg(&out);
        if args.controls {
            cmd.arg("--controls");
        }
        let started = Instant::now();
        let status = cmd
            .stdout(file.try_clone()?)
            .stderr(file)
            .status()
            .with_context(|| format!("spawning xtask-harness run-apk {row} --scenario"))?;
        let text = std::fs::read_to_string(&log).unwrap_or_default();
        let line = crate::check_gates::verdict_line(&text, "scenario").unwrap_or("no verdict line");
        // the control runs' lines, so both show without opening the log
        for control in text.lines().filter(|l| l.starts_with("control: ")) {
            println!("scenarios: {row} {control}");
        }
        println!(
            "scenarios: {row} ({:.0}s) {line}",
            started.elapsed().as_secs_f64()
        );
        results.insert(row.clone(), (status.success(), log));
    }
    let failed: Vec<String> = results
        .iter()
        .filter(|(_, (ok, _))| !ok)
        .map(|(row, (_, log))| format!("{row} (log {})", log.display()))
        .collect();
    if failed.is_empty() {
        println!(
            "scenarios: ok {} of {} rows{}",
            rows.len(),
            rows.len(),
            if args.controls { ", controls red" } else { "" }
        );
        Ok(ExitCode::SUCCESS)
    } else {
        println!(
            "scenarios: FAILED {} of {} rows: {}",
            failed.len(),
            rows.len(),
            failed.join("; ")
        );
        Ok(ExitCode::FAILURE)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = "size 1440 900\ncontrol break-input\nunreachable ad end: filled from the network\nwait-stable 0 0 10 10 1000 200\nwait 500\nmark a\ntap 1 1\nexpect-changed a\n";

    fn lint_row(text: &str) -> Result<Script> {
        lint("fossify-math", text, &|r| r == "fossify-math")
    }

    fn red(text: &str, why: &str) {
        let e = format!("{:#}", lint_row(text).expect_err(why));
        assert!(e.contains(why), "{e}");
    }

    #[test]
    fn a_good_scenario_passes() {
        let s = lint_row(GOOD).expect("good scenario");
        assert_eq!(s.controls, ["break-input"]);
        assert_eq!(
            s.unreachable,
            [("ad end".to_owned(), "filled from the network".to_owned())]
        );
        // wait-stable and expect-changed; the wait, the mark (judges nothing) and the tap are no checkpoints
        assert_eq!(s.checkpoints(), 2);
    }

    /// Review-2 G4: a scenario must judge what its input caused, unconditionally.
    #[test]
    fn a_scenario_without_a_final_unconditional_expect_is_red() {
        const WHY: &str = "no unconditional expect-* after the last input step";
        // nothing after the last input
        red(&GOOD.replace("expect-changed a\n", ""), WHY);
        // only marks and waits after it
        red(
            &GOOD.replace(
                "expect-changed a\n",
                "mark b\nwait-stable 0 0 10 10 1000 200\n",
            ),
            WHY,
        );
        // only an expect-fps: skipped on another tier
        red(
            &GOOD.replace(
                "expect-changed a\n",
                "expect-fps 10 tier=readback over 1000\n",
            ),
            WHY,
        );
        // the expectation sits in a branch that may not be taken
        red(
            &GOOD.replace("expect-changed a\n", "if-same a\nexpect-changed a\nend\n"),
            WHY,
        );
        // a branch after the expectation may hold input: it ends the run unjudged
        red(&format!("{GOOD}if-same a\ntap 1 1\nend\n"), WHY);
    }

    #[test]
    fn the_file_names_a_row() {
        let e = lint("no-such-row", GOOD, &|r| r == "fossify-math").expect_err("unknown row");
        assert!(format!("{e:#}").contains("no row"));
    }

    #[test]
    fn the_script_parses() {
        red(&format!("{GOOD}tap 1\n"), "script line 8");
    }

    #[test]
    fn a_control_is_declared_and_known() {
        red(
            &GOOD.replace("control break-input\n", ""),
            "no `control` line",
        );
        red(&GOOD.replace("break-input", "break-nothing"), "is none of");
    }

    #[test]
    fn unreachable_carries_a_reason() {
        red(
            &GOOD.replace(": filled from the network", ""),
            "unreachable needs `STATE: REASON`",
        );
        red(
            &GOOD.replace("filled from the network", " "),
            "unreachable needs `STATE: REASON`",
        );
    }

    #[test]
    fn a_fixed_wait_follows_a_condition_and_is_short() {
        red(&format!("{GOOD}wait 100\n"), "follows no condition wait");
        red(&GOOD.replace("wait 500", "wait 3000"), "not under 3000 ms");
        // an if-log's body starts after its condition
        lint_row(&format!(
            "{GOOD}if-log app x 100\nwait 200\nend\nexpect-changed a\n"
        ))
        .expect("wait opens an if-log body");
        // an if-same judges at once: nothing to wait after
        red(
            &format!("{GOOD}mark m\nif-same m\nwait 200\nend\n"),
            "follows no condition wait",
        );
    }

    /// The design's S5 example shape, written out by hand.
    #[test]
    fn verdict_line_shape() {
        let v = Verdict {
            row: "fossify-math",
            host: "x86_64",
            cpu: "native(AMD Ryzen 7)".into(),
            guest: "x86_64".into(),
            passed: 5,
            skipped: vec![],
            total: 5,
            fps_unjudged: false,
            fps: Some(16.25),
            tier: "readback".into(),
            secs: 61,
            unreachable: vec!["ad end", "sign-in"],
            failure: None,
        };
        assert_eq!(
            v.line(),
            "scenario: ok fossify-math host=x86_64 cpu=native(AMD Ryzen 7) guest=x86_64 5 judged, 0 skipped, 5 checkpoints fps=16.2 tier=readback 61s [unreachable: ad end, sign-in]"
        );
        let red = Verdict {
            fps: None,
            unreachable: vec![],
            passed: 3,
            failure: Some("drive: step 9 failed"),
            ..v
        };
        assert_eq!(
            red.line(),
            "scenario: FAILED fossify-math host=x86_64 cpu=native(AMD Ryzen 7) guest=x86_64 3 judged, 0 skipped, 5 checkpoints fps=- tier=readback 61s: drive: step 9 failed"
        );
        // Review-2 G4: skipped checkpoints are apart from judged ones, with their reasons; a run that judged no fps says so
        let skipped = Verdict {
            row: "angry-birds-2",
            host: "x86_64",
            cpu: "native(AMD Ryzen 7)".into(),
            guest: "x86_64".into(),
            passed: 11,
            skipped: vec![
                "expect-fps: tier import != readback".into(),
                "if-same panel: not taken".into(),
            ],
            total: 13,
            fps_unjudged: true,
            fps: Some(30.0),
            tier: "import".into(),
            secs: 90,
            unreachable: vec![],
            failure: None,
        };
        assert_eq!(
            skipped.line(),
            "scenario: ok angry-birds-2 host=x86_64 cpu=native(AMD Ryzen 7) guest=x86_64 11 judged, 2 skipped (expect-fps: tier import != readback; if-same panel: not taken), 13 checkpoints fps=30.0 tier=import 90s [FPS NOT JUDGED: every expect-fps names another tier than import]"
        );
    }

    #[test]
    fn cpu_model_per_arch() {
        assert_eq!(
            cpu_model("processor\t: 0\nmodel name\t: AMD Ryzen 7 5800H\n"),
            Some("AMD Ryzen 7 5800H")
        );
        assert_eq!(
            cpu_model("hart\t: 0\nisa\t: rv64imafdc\nuarch\t: sifive,p550\n"),
            Some("sifive,p550")
        );
        assert_eq!(cpu_model(""), None);
    }

    /// Every tracked scenario passes the lint against the real table (also what step `tables` checks).
    #[test]
    fn tracked_scenarios_lint() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("repo root");
        assert!(check(root).expect("tracked scenarios") >= 1);
    }

    /// A deliberately broken copy of the tracked fossify-math scenario is red.
    #[test]
    fn broken_copy_of_a_tracked_scenario_is_red() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("repo root");
        let text = std::fs::read_to_string(path(root, "fossify-math")).expect("tracked file");
        let broken: String = text
            .lines()
            .filter(|l| !l.starts_with("control "))
            .map(|l| format!("{l}\n"))
            .collect();
        assert!(lint_row(&text).is_ok());
        assert!(lint_row(&broken).is_err());
    }
}
