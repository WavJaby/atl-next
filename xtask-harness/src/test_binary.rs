// SPDX-License-Identifier: MIT
//! One test binary under a managed scope: `xtask` (test_run.rs) runs each test binary through this. The scope's supervisor runs the binary in its leaf (atl-host-load `supervise`); this launcher stays outside the scope, stops the scope on every path, and writes the measured record to `--result` as JSON. No checkout, no gate.

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result};
use atl_host_load::{Exit, Record};
use serde_json::{Value, json};

#[derive(clap::Args)]
pub struct Args {
    /// The result: the measured record as JSON (`complete`; when complete `exit`, `wall_ms`, `cpu_usec`, `mem_peak`, `certified`, `oom_kill`; else `reason`).
    #[arg(long)]
    result: PathBuf,
    /// The scope's unit name, `atl-row-` prefixed (a managed leaf for nested gates). The caller names it so it can stop the unit when this launcher is lost abruptly.
    #[arg(long)]
    unit: String,
    /// The leaf's `memory.max` in bytes (swap off, group kill on): an overrun is an OOM kill inside the leaf, which `certified` reports.
    #[arg(long)]
    memory_max: Option<u64>,
    /// The binary and its arguments, after `--`.
    #[arg(last = true, required = true)]
    argv: Vec<OsString>,
}

/// Runs the binary in a managed scope and writes its record to `--result`. Exits 0 whatever the binary did: the result says how it ended; an incomplete measurement is `complete: false`.
pub fn run(args: &Args) -> Result<ExitCode> {
    let exe = std::env::current_exe().context("locating the harness")?;
    let unit = args.unit.as_str();
    anyhow::ensure!(
        unit.starts_with("atl-row-"),
        "--unit {unit}: not a managed row's unit name"
    );
    // the supervisor appends its record here; the workload's output goes to the launcher's stdio (the caller's log)
    let record_log = args.result.with_extension("record");
    let mut cmd = atl_host_load::scoped_command(unit);
    cmd.arg(&exe)
        .args(["scope-supervise", "--record"])
        .arg(&record_log);
    if let Some(max) = args.memory_max {
        cmd.arg("--memory-max").arg(max.to_string());
    }
    cmd.arg("--").args(&args.argv);
    let supervisor = cmd.status().context("spawning the managed scope")?;
    atl_host_load::stop(unit);
    let text = std::fs::read_to_string(&record_log).unwrap_or_default();
    let result = match Record::read_last(&text) {
        Ok(r) => json!({
            "complete": true,
            "exit": match r.exit {
                Exit::Code(c) => json!({"code": c}),
                Exit::Signal(s) => json!({"signal": s}),
                Exit::NotRun => Value::Null,
            },
            "wall_ms": r.wall_ms,
            "cpu_usec": r.cpu_usec,
            "mem_peak": r.mem_peak,
            "certified": r.certified(),
            "oom_kill": r.events.oom_kill,
        }),
        Err(why) => json!({"complete": false, "reason": why, "supervisor": supervisor.code()}),
    };
    std::fs::write(&args.result, result.to_string())
        .with_context(|| args.result.display().to_string())?;
    Ok(ExitCode::SUCCESS)
}
