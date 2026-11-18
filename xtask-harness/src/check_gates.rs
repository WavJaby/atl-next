// SPDX-License-Identifier: MIT
//! Check step `gates`: runtime gates `cargo xtask check` runs on the checkout's build dir, each as `cargo xtask test` runs it (its own harness process, its own verdict line), so a regression in
//! one of them cannot land green. A row is a gate that passes on a build machine without the laptop's hardware, leaves the desktop session as it found it (no default-sink switch: not audio-pcm,
//! audio-app, webview) and takes under ~35 s; slower, load-flaky (widgets, dialog) and laptop-only gates stay `xtask test` runs. A host that cannot run a row's gate says so in the gate's own verdict
//! (`host lacks ...`), never by dropping the row.

use std::path::Path;
use std::process::Command;
use std::time::Instant;

use anyhow::{Context, Result, bail};

/// One gate run: `xtask test <args>`, plus `--host <build machine>` when `host` and `--out <build dir>` when `out` (each only where the gate's own arguments have it).
pub struct Row {
    pub args: &'static [&'static str],
    pub host: bool,
    pub out: bool,
}

impl Row {
    const fn new(args: &'static [&'static str], host: bool, out: bool) -> Self {
        Self { args, host, out }
    }

    /// The gate's verdict-line name (`window: ...` for every window stage).
    fn cmd(&self) -> &'static str {
        self.args[0]
    }

    /// The row's log name: its words joined (`window-ime`).
    fn name(&self) -> String {
        self.args
            .iter()
            .filter(|a| !a.starts_with("--"))
            .copied()
            .collect::<Vec<_>>()
            .join("-")
    }

    fn argv<'a>(&self, host: &'a str, out: &'a str) -> Vec<&'a str> {
        let mut argv: Vec<&str> = self.args.to_vec();
        if self.host {
            argv.extend(["--host", host]);
        }
        if self.out {
            argv.extend(["--out", out]);
        }
        argv
    }
}

/// The gates `check` runs, cheapest first, except where one reads what another stages.
pub const ROWS: &[Row] = &[
    Row::new(&["task-bind"], false, false),
    Row::new(&["sandbox"], true, true),
    Row::new(&["signing"], true, true),
    Row::new(&["gl-dispatch"], false, true),
    // reads the workspace's APK cache (default APKs); a missing APK is its own red verdict
    Row::new(&["app-imports"], false, false),
    // its --out is its own output dir (default under the workspace, never the build dir); runs on the host's ICD (lavapipe on WSL)
    Row::new(&["vk-driver"], false, false),
    Row::new(&["nb-isa"], true, true),
    Row::new(&["ui-module"], false, true),
    Row::new(&["platform-ui"], false, false),
    Row::new(&["compositor"], true, true),
    Row::new(&["parcel"], true, true),
    Row::new(&["provider"], true, true),
    Row::new(&["system-scan"], true, true),
    Row::new(&["broadcast"], true, true),
    Row::new(&["billing"], true, true),
    Row::new(&["gralloc"], true, true),
    Row::new(&["composer"], true, true),
    Row::new(&["instrument"], true, true),
    Row::new(&["app"], true, true),
    // reads the `app/` tree the app gate stages (its refusal otherwise: "missing <out>/app/root/system/lib64/x86_64 ... run `xtask test app`")
    Row::new(&["nb-identity"], false, true),
    // needs what nb-identity needs (the staged `app/`)
    Row::new(&["sles-bridge"], false, true),
    Row::new(&["window", "--stage", "surface-view"], true, true),
    Row::new(&["window", "--stage", "ime"], true, true),
];

/// Runs every row on build dir `out` for host `host`, logs under `<out>/check-gates/`; fails naming each red gate's verdict line.
pub fn run(root: &Path, host: &str, out: &Path) -> Result<()> {
    let exe = std::env::current_exe().context("the harness's own path")?;
    let logs = out.join("check-gates");
    std::fs::create_dir_all(&logs).with_context(|| logs.display().to_string())?;
    let out_arg = out.to_str().context("build dir path is not UTF-8")?;
    let mut failed = Vec::new();
    for row in ROWS {
        let log = logs.join(format!("{}.log", row.name()));
        let file = std::fs::File::create(&log).with_context(|| log.display().to_string())?;
        let argv = row.argv(host, out_arg);
        let started = Instant::now();
        let status = Command::new(&exe)
            .current_dir(root)
            .arg("test")
            .args(&argv)
            .stdout(file.try_clone()?)
            .stderr(file)
            .status()
            .with_context(|| format!("spawning xtask-harness test {}", argv.join(" ")))?;
        let took = started.elapsed().as_secs_f64();
        let text = std::fs::read_to_string(&log).unwrap_or_default();
        let line = verdict_line(&text, row.cmd()).unwrap_or("no verdict line");
        eprintln!(
            "gates: {} {} ({took:.1}s) {line}",
            row.name(),
            if status.success() { "ok" } else { "FAILED" }
        );
        if !status.success() {
            failed.push(format!("{}: {line} (log {})", row.name(), log.display()));
        }
    }
    if failed.is_empty() {
        return Ok(());
    }
    let summary = format!(
        "{} of {} gates failed: {}",
        failed.len(),
        ROWS.len(),
        failed.join("; ")
    );
    // stdout leads the step's failure text (xtask harness.rs check_step), so check's first-error line is this one, not the first gate's progress line
    println!("gates: FAILED {summary}");
    bail!("{summary}")
}

/// The gate's verdict among its `<cmd>: ` lines (a gate also prints counters and log tails under its name): its `FAILED` line, else the last pass / ok line.
fn verdict_line<'a>(text: &'a str, cmd: &str) -> Option<&'a str> {
    let prefix = format!("{cmd}: ");
    let named = || text.lines().filter(|l| l.starts_with(&prefix));
    named().find(|l| l.contains("FAILED")).or_else(|| {
        named().rev().find(|l| {
            let rest = &l[prefix.len()..];
            rest.starts_with("ok") || rest.starts_with("pass") || rest.starts_with("all checks")
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    /// Lines as the window and composer gates printed them (job rgm-done1).
    #[test]
    fn the_verdict_not_the_last_named_line() {
        let pass = "window: pass ime (`AtlWindow: onResume reached` logged 11521 ms)\nwindow: +227 ms after onResume: unimplemented IWindowSession.x (total 7)\n";
        assert_eq!(
            verdict_line(pass, "window"),
            Some("window: pass ime (`AtlWindow: onResume reached` logged 11521 ms)")
        );
        let red = "composer: FAIL quads readable: 0 quads\ncomposer: FAILED: quads readable; first error\ncomposer: services counted x\n";
        assert_eq!(
            verdict_line(red, "composer"),
            Some("composer: FAILED: quads readable; first error")
        );
        assert_eq!(verdict_line("other: ok\n", "composer"), None);
    }

    /// Every row is a command line `xtask test` accepts, as `run` builds it: a renamed gate, stage or flag, or a host/out the gate does not take, fails here rather than in a check run.
    #[test]
    fn every_row_parses_as_xtask_test() {
        for row in ROWS {
            let mut argv = vec!["xtask", "test"];
            argv.extend(row.argv("x86_64", "/tmp/out"));
            if let Err(e) = crate::Cli::try_parse_from(&argv) {
                panic!("{}: {e}", argv.join(" "));
            }
        }
    }

    #[test]
    fn row_names_and_argv() {
        let ime = Row::new(&["window", "--stage", "ime"], true, true);
        assert_eq!((ime.cmd(), ime.name().as_str()), ("window", "window-ime"));
        assert_eq!(
            ime.argv("h", "o"),
            ["window", "--stage", "ime", "--host", "h", "--out", "o"]
        );
        assert_eq!(
            Row::new(&["platform-ui"], false, false).argv("h", "o"),
            ["platform-ui"]
        );
    }
}
