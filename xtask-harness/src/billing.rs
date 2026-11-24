// SPDX-License-Identifier: MIT
//! `xtask test billing`: the billing-unavailable desktop adaptation (M62 D1). Fixture xtask-harness/testdata/billing, a Play Billing client's first steps: it sees the store's billing service only
//! through its `<queries><intent>` (the responder has no forceQueryable), binds it with the implicit Intent + setPackage, and asks isBillingSupported / isBillingSupportedExtraParams.
//! - default (setting on, `Runtime::with_billing_unavailable`): the query finds the one service, the bind connects to it, both calls answer 3 (BILLING_UNAVAILABLE), an unlisted code is unanswered;
//! - `--off` (no vending package, exact AOSP without Play): the query is empty, bindService answers false and nothing connects.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use anyhow::Result;
use atl_layout::Layout;

use crate::atld_run::{Package, Run, Runtime};
use crate::wait::{LOOP_POLL, Miss, Wait, stopped_at, until_child};

#[derive(clap::Args)]
pub struct Args {
    /// The host arch (an arches.toml row with `[host]`): the default --out.
    #[arg(long)]
    host: String,
    /// Native build directory holding ART, servicemanager and the services (default: this checkout's build dir, see `cargo xtask out show`; else <workspace>/out/native/<host>).
    #[arg(long)]
    out: Option<PathBuf>,
    /// Java pipeline build directory: boot jars, d8, aapt2 (default: this checkout's build dir, see `cargo xtask out show`; else <workspace>/out/java/build).
    #[arg(long)]
    java_out: Option<PathBuf>,
    /// The setting off: the run places no vending package, so the client must see AOSP without Play (empty query, failed bind).
    #[arg(long)]
    off: bool,
    /// Print the app's whole log.
    #[arg(long, short)]
    verbose: bool,
}

const FIXTURE: &str = "xtask-harness/testdata/billing";
const PACKAGE: &str = "com.example.atlbilling";
/// A cold start (ART without a boot image interprets framework.jar's class init), as `xtask test app`'s limit.
const LIMIT: Duration = Duration::from_secs(180);
const TAIL: usize = 30;
const DONE: &str = "AtlBilling: done";
const CONNECTED: &str = "AtlBilling: connected ";
/// The head of a failed installer end (atl-broker installer_end.rs `HEAD`): a run whose installer ended first names it before any symptom.
const INSTALLER_HEAD: &str = "atl-installer exited";

/// The lines each setting must show. On: one resolved service, a connection to the responder's class, 3 = BillingResponseCode.BILLING_UNAVAILABLE for codes 1 and 10, code 2 unanswered.
/// Off: AOSP without the package -- queryIntentServices empty, bindService false (bindServiceLocked found nothing).
fn want(off: bool) -> &'static str {
    if off {
        "AtlBilling: query 0 bind false"
    } else {
        "AtlBilling: query 1 bind true"
    }
}
const ANSWER: &str = "AtlBilling: connected com.android.vending.billing.InAppBillingService supported 3 extra 3 other false";

fn tail(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

/// The log from its first line naming a failed installer end to its end: no count can cut that line. A log without one keeps its last `n` lines.
fn tail_from_head(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let at = lines
        .iter()
        .position(|l| l.contains(INSTALLER_HEAD))
        .unwrap_or(lines.len().saturating_sub(n));
    lines[at..].join("\n")
}

/// The first line naming a failed installer end, in the first log that has one (services first, the origin; atld's exit chain repeats its text).
fn installer_head(logs: &[&str]) -> Option<String> {
    logs.iter().find_map(|log| {
        log.lines()
            .find(|l| l.contains(INSTALLER_HEAD))
            .map(str::to_owned)
    })
}

/// The run's failures in error order: the installer's head line, atld's exit, then the failed checks. The checks are symptoms (the app never started); the verdict line quotes only the first failure.
fn failures(checks: &[(&str, bool)], ended: Option<&str>, head: Option<&str>) -> Vec<String> {
    let mut whys: Vec<String> = head.map(str::to_owned).into_iter().collect();
    if let Some(status) = ended {
        whys.push(format!("atld exited {status}"));
    }
    whys.extend(
        checks
            .iter()
            .filter(|(_, ok)| !ok)
            .map(|(what, _)| (*what).to_owned()),
    );
    whys
}

pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    let _ = Layout::of(root)?;
    let rt = Runtime::new(
        root,
        "billing",
        &args.host,
        args.out.as_deref(),
        args.java_out.as_deref(),
        "billing",
        args.verbose,
    )?;
    let rt = if args.off {
        rt
    } else {
        rt.with_billing_unavailable()?
    };
    let work = rt.out.join("billing");
    let apk = rt
        .apk
        .build(&root.join(FIXTURE), &work.join("app"), &[], None, None)?;
    let run = Run {
        root: work.join("root"),
        scratch: work.join("scratch"),
        launch: Some(PACKAGE.to_owned()),
        packages: vec![Package::base(apk)],
        libraries: vec![],
        shell: None,
        log_tags: "*:i".into(),
    };
    let mut atld = rt.start_atld(&run)?;
    let read = atl_diag::logfiles::read;
    let log = run.log(PACKAGE);
    let start = Instant::now();
    let done_line = |t: &str| t.contains(DONE);
    let done = Wait::log(
        "billing-done",
        format!("no {DONE:?} line within {LIMIT:?}"),
        LIMIT,
        &log,
        &done_line,
    )
    .every(LOOP_POLL);
    let outcome = until_child(&done, &mut atld);
    let ended = match &outcome {
        Err(Miss::Exited { status, .. }) => Some(status.clone()),
        _ => None,
    };
    if ended.is_none() {
        let _ = atld.kill();
        atld.wait()?;
    }
    let text = read(&log);
    let atld_text = read(&run.atld_log());
    let services_text = read(&run.log("services"));
    if args.verbose {
        println!("---- app log\n{text}");
    }
    let setting = if args.off { "off" } else { "on" };
    let checks = [
        ("query and bind", text.contains(want(args.off))),
        (
            "the connection's answers",
            if args.off {
                !text.contains(CONNECTED)
            } else {
                text.contains(ANSWER)
            },
        ),
    ];
    let head = installer_head(&[services_text.as_str(), atld_text.as_str()]);
    let whys = match &outcome {
        Err(late @ Miss::Late { .. }) => stopped_at("billing", &checks, late),
        _ => {
            for (what, ok) in &checks {
                println!(
                    "billing: {} {what} (setting {setting})",
                    if *ok { "pass" } else { "FAIL" }
                );
            }
            failures(&checks, ended.as_deref(), head.as_deref())
        }
    };
    if whys.is_empty() {
        println!(
            "billing: pass (setting {setting}, {} checks in {:.1?})",
            checks.len(),
            start.elapsed()
        );
        return Ok(ExitCode::SUCCESS);
    }
    // the starvation rule (starvation.rs): a late done-line wait or a fired product timer the host starved; a run whose atld exited is never excused
    if ended.is_none() {
        let faults: Vec<Miss> = match &outcome {
            Err(late @ Miss::Late { .. }) => vec![late.clone()],
            _ => whys.iter().cloned().map(Miss::Wrong).collect(),
        };
        if let Some(code) =
            crate::starvation::judge_row("billing", &faults, &services_text, &atld_text)
        {
            return Ok(code);
        }
    }
    println!(
        "---- atld log (from its installer head, else last {TAIL})\n{}",
        tail_from_head(&atld_text, TAIL)
    );
    println!("---- app log (last {TAIL})\n{}", tail(&text, TAIL));
    println!(
        "---- services log (from its installer head, else last {TAIL})\n{}",
        tail_from_head(&services_text, TAIL)
    );
    println!("{}", crate::verdict::gate("billing", &whys));
    Ok(ExitCode::FAILURE)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A failed run's head line as installer_end.rs writes it for a SIGSYS end.
    const SIGSYS_HEAD: &str = "Error: scan /product/app/Killed: atl-installer exited signal: 31 (SIGSYS): seccomp: sandbox killed by its seccomp filter, the denied call is the audit record `type=1326 ... sig=31`";

    /// An atld log: `before` noise lines, the head, then `after` lines of the installer's stderr (the shape a SIGSYS end leaves).
    fn fake_atld_log(before: usize, after: usize) -> String {
        let mut lines: Vec<String> = (0..before).map(|i| format!("atld: noise {i}")).collect();
        lines.push(SIGSYS_HEAD.to_owned());
        lines.extend((0..after).map(|i| format!("    installer stderr {i}")));
        lines.join("\n")
    }

    fn texts(whys: &[String]) -> Vec<&str> {
        whys.iter().map(String::as_str).collect()
    }

    #[test]
    fn the_installer_head_leads_the_failures_before_the_exit_and_the_checks() {
        let atld = fake_atld_log(40, 3);
        let head = installer_head(&["no head in the services log", atld.as_str()])
            .expect("the head is found in atld's log");
        let checks = [
            ("query and bind", false),
            ("the connection's answers", false),
        ];
        let whys = failures(&checks, Some("exit status: 1"), Some(head.as_str()));
        assert_eq!(
            texts(&whys),
            [
                SIGSYS_HEAD,
                "atld exited exit status: 1",
                "query and bind",
                "the connection's answers",
            ]
        );
    }

    #[test]
    fn without_a_head_the_exit_leads_and_passing_checks_add_nothing() {
        let checks = [
            ("query and bind", false),
            ("the connection's answers", true),
        ];
        let whys = failures(&checks, Some("exit status: 1"), None);
        assert_eq!(
            texts(&whys),
            ["atld exited exit status: 1", "query and bind"]
        );
        let passing = [("query and bind", true), ("the connection's answers", true)];
        assert!(failures(&passing, None, None).is_empty());
    }

    #[test]
    fn the_tail_starts_at_the_head_line_not_at_a_count() {
        // the head is line 41 of 44: the last 30 lines would cut it
        let atld = fake_atld_log(40, 3);
        assert!(atld.lines().count() > TAIL);
        let shown = tail_from_head(&atld, TAIL);
        assert_eq!(shown.lines().next(), Some(SIGSYS_HEAD));
        assert_eq!(shown.lines().count(), 1 + 3);
    }

    #[test]
    fn a_log_without_a_head_keeps_its_last_lines() {
        assert_eq!(tail_from_head("a\nb\nc\nd", 2), "c\nd");
    }
}
