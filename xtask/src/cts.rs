// SPDX-License-Identifier: MIT
//! `xtask test cts`: the progress metric of the CTS port (owner decision 2026-11-03: our own runner drives AOSP's `am instrument`, not Tradefed). One row of cts.toml = one `android_test` module: the converter builds its APK (and the apps it names),
//! atld installs them next to the android.test.* shared libraries and runs `am instrument -w -r [-e class ..] [-e package ..] <package>/<runner>` as the shell user (instrument.rs: the same run the `test instrument` gate checks),
//! the INSTRUMENTATION_* text is parsed and classified (cts_report.rs), and `<out>/cts/report.json` holds every module's state and every test's outcome. The summary on stdout is the one-screen version: per module pass/fail/error/ignored/total, then the services' most-hit
//! unimplemented-call and refused-parcelable counters summed over the run, the list of what to implement next.
//! The runner of a module is derived: the one `<instrumentation>` of its manifest that targets the APK itself (or the one instrumentation, when none does); a manifest with several is an error naming the rows to add a column for. Its target package must be installed (the module itself, or one of `apps`).
//! Exit status: nonzero only when the harness itself fails (a table or staging error, no runtime) or, with `--baseline <report.json>`, when a module or a test that passed in that saved report is worse now; failing tests are the metric, not errors.
//! A module whose APK is missing after the build is BUILD_FAIL with the first causes of its failed dependencies (ninja.log of the build directory, triage.rs), also when an older APK is still on disk.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use atl_services::packages::{InstrumentationDecl, Manifest};

use crate::atld_run::{Run, Runtime, counters};
use crate::cts_report::{self, Module, Report, State};
use crate::cts_table::{self, Row};
use crate::instrument::{self, APP_UID};
use crate::{manifest, triage};

#[derive(clap::Args)]
pub struct Args {
    /// The host arch (an arches.toml row with `[host]`).
    #[arg(long)]
    host: String,
    /// Native build directory holding ART, servicemanager and the services (default <workspace>/out/native/<host>, as `xtask native`); the report goes to `<out>/cts/report.json`.
    #[arg(long)]
    out: Option<PathBuf>,
    /// Java pipeline build directory (default <workspace>/out/java/build): the converter builds the test APKs there, next to the boot jars, aapt2, `am` and the android.test.* libraries `xtask test instrument` needs.
    #[arg(long)]
    java_out: Option<PathBuf>,
    /// Run only these cts.toml rows (repeatable); default every row.
    #[arg(long = "module", value_name = "MODULE")]
    modules: Vec<String>,
    /// Use the APKs already in --java-out instead of running `xtask native` for the rows' modules first.
    #[arg(long)]
    no_build: bool,
    /// A saved report.json: exit nonzero when a module's state is worse or a test that passed there does not pass now (with --module, only those modules are compared).
    #[arg(long, value_name = "REPORT")]
    baseline: Option<PathBuf>,
    /// How long one module's run may take; a module still running then is stopped and reported INCOMPLETE.
    #[arg(long, default_value_t = LIMIT_SECS, value_name = "SECONDS")]
    limit_secs: u64,
    /// How many counters the summary lists.
    #[arg(long, default_value_t = TOP_COUNTERS)]
    top: usize,
    /// Print every run's whole `am` output.
    #[arg(long, short)]
    verbose: bool,
}

/// Per-module time limit: a cold ART start costs seconds and a whole CTS module hundreds of tests, so the unit-test gate's 300 s (instrument.rs LIMIT) is too short; 30 minutes bounds one row at the owner's patience and is not a measured need (no module has finished yet).
const LIMIT_SECS: u64 = 1800;
/// Counters the summary lists: one screen with the module table above.
const TOP_COUNTERS: usize = 15;
/// Lines of a log shown for a module that did not reach RAN.
const TAIL: usize = 30;
/// Where the Java pipeline puts an app's APK: `device/<module>/<module>.apk` (bp2ninja app.go).
fn apk_path(java_out: &Path, module: &str) -> PathBuf {
    java_out.join(format!("device/{module}/{module}.apk"))
}

/// The instrumentation a module's run uses, from its manifest: the one that targets the APK itself, else the only one; anything else is ambiguous.
fn pick_instrumentation(m: &Manifest) -> Result<&InstrumentationDecl, String> {
    let own: Vec<_> = m
        .instrumentation
        .iter()
        .filter(|i| i.target_package == m.name)
        .collect();
    match (own.as_slice(), m.instrumentation.as_slice()) {
        ([one], _) => Ok(*one),
        ([], [one]) => Ok(one),
        (_, all) => Err(format!(
            "{} declares {} instrumentations ({} target the package itself): {:?}; this needs a cts.toml column naming the one to run",
            m.name,
            all.len(),
            own.len(),
            all.iter().map(|i| i.name.as_str()).collect::<Vec<_>>()
        )),
    }
}

/// The failed modules of a build (`device/<name>` keys of triage's table) that `module` depends on, transitively (itself included), and the first cause line of each one that has no failed direct dependency of its own.
fn build_blockers(
    graph: &triage::Graph,
    failures: &std::collections::BTreeMap<String, triage::Failed>,
    module: &str,
) -> Vec<String> {
    let mut seen = BTreeSet::new();
    let mut todo = vec![format!("device/{module}")];
    while let Some(m) = todo.pop() {
        if seen.insert(m.clone()) {
            todo.extend(graph.deps.get(&m).cloned().unwrap_or_default());
        }
    }
    let mut out = Vec::new();
    for (name, f) in failures.iter().filter(|(n, _)| seen.contains(*n)) {
        let blocked_by_another = graph
            .deps
            .get(name)
            .is_some_and(|ds| ds.iter().any(|d| failures.contains_key(d)));
        if blocked_by_another {
            continue;
        }
        let cause = f
            .errors
            .first()
            .map_or_else(|| "(no cause line)".to_owned(), |c| c.line.clone());
        out.push(format!("{name}: {cause}"));
    }
    out
}

/// `xtask native` for the rows' modules into the Java build directory (this binary again, output in `<work>/build.log`); its exit status is not the result: a module is built when its APK is, and a failed one is told from ninja.log.
fn build(args: &Args, java_out: &Path, goals: &BTreeSet<String>, work: &Path) -> Result<()> {
    let log = work.join("build.log");
    let file = std::fs::File::create(&log)?;
    println!(
        "cts: building {} modules into {} (log {})",
        goals.len(),
        java_out.display(),
        log.display()
    );
    let status = Command::new(std::env::current_exe().context("locating xtask")?)
        .args(["native", "--host", &args.host, "--out"])
        .arg(java_out)
        .args(goals)
        .stdout(file.try_clone()?)
        .stderr(file)
        .status()
        .context("running xtask native")?;
    println!("cts: build {status}");
    Ok(())
}

/// What the build left of one module: its APK and manifest, or why it has none.
struct Built {
    apk: PathBuf,
    manifest: Manifest,
}

fn built(rt: &Runtime, module: &str, blockers: &[String], log: &Path) -> Result<Built, String> {
    let apk = apk_path(&rt.java_out, module);
    if !blockers.is_empty() {
        return Err(format!(
            "{module} waits on failed modules: {}",
            blockers.join("; ")
        ));
    }
    if !apk.is_file() {
        return Err(format!(
            "{} was not built (no failed module in its closure; see {})",
            apk.display(),
            log.display()
        ));
    }
    let manifest = manifest::read(&rt.aapt2, &apk).map_err(|e| format!("{module}: {e:#}"))?;
    Ok(Built { apk, manifest })
}

fn tail(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

/// The `am instrument` arguments of a row: wait, raw text, the filters, the component.
fn am_args(row: &Row, component: &str) -> Vec<String> {
    let mut args: Vec<String> = ["instrument", "-w", "-r"].map(str::to_owned).to_vec();
    args.extend(row.filter_args());
    args.push(component.to_owned());
    args
}

pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    let table = cts_table::load(root)?;
    for m in &args.modules {
        ensure!(
            table.contains_key(m),
            "--module {m}: no such row in {}",
            cts_table::FILE
        );
    }
    let rows: Vec<(&String, &Row)> = table
        .iter()
        .filter(|(m, _)| args.modules.is_empty() || args.modules.contains(m))
        .collect();
    let base: Option<Report> = match &args.baseline {
        Some(p) => {
            let text = std::fs::read_to_string(p)
                .with_context(|| format!("reading baseline {}", p.display()))?;
            let r: Report = serde_json::from_str(&text)
                .with_context(|| format!("parsing baseline {}", p.display()))?;
            ensure!(
                r.schema == cts_report::SCHEMA && r.host == args.host,
                "baseline {} is schema {} for host {}, this run is schema {} for {}",
                p.display(),
                r.schema,
                r.host,
                cts_report::SCHEMA,
                args.host
            );
            Some(r)
        }
        None => None,
    };
    let rt = Runtime::new(
        root,
        &args.host,
        args.out.as_deref(),
        args.java_out.as_deref(),
        "cts-art",
        args.verbose,
    )?;
    let work = rt.out.join("cts");
    std::fs::create_dir_all(&work)?;
    let goals: BTreeSet<String> = rows
        .iter()
        .flat_map(|(m, r)| std::iter::once((*m).clone()).chain(r.apps.iter().cloned()))
        .collect();
    if !args.no_build {
        build(args, &rt.java_out, &goals, &work)?;
    }
    // ninja.log of the build directory names the failed modules; with --no-build it is whatever the last build there left
    let ninja_log = rt.java_out.join("ninja.log");
    let failures = triage::failures(
        &std::fs::read_to_string(&ninja_log).unwrap_or_default(),
        &[&format!("{}/", rt.java_out.display())],
    );
    // the graph of that build: it must parse, or a failed dependency would go unseen and a stale APK be run
    let graph_file = rt.java_out.join("graph.json");
    let graph: triage::Graph = serde_json::from_str(
        &std::fs::read_to_string(&graph_file)
            .with_context(|| format!("reading {}", graph_file.display()))?,
    )
    .with_context(|| format!("parsing {}", graph_file.display()))?;
    let libraries = instrument::libraries(&rt)?;
    let limit = Duration::from_secs(args.limit_secs);
    let mut modules = Vec::new();
    let mut all_counters = Vec::new();
    for (module, row) in &rows {
        let module = module.as_str();
        println!("cts: {module}: staging");
        let blockers = |m: &str| build_blockers(&graph, &failures, m);
        let test = match built(&rt, module, &blockers(module), &work.join("build.log")) {
            Ok(b) => b,
            Err(why) => {
                println!("cts: {module}: BUILD_FAIL {why}");
                modules.push(Module::without_run(module, State::BuildFail, why));
                continue;
            }
        };
        let mut packages = vec![(test.apk.clone(), test.manifest.clone())];
        let mut missing = None;
        for app in &row.apps {
            match built(&rt, app, &blockers(app), &work.join("build.log")) {
                Ok(b) => packages.push((b.apk, b.manifest)),
                Err(why) => {
                    missing = Some(why);
                    break;
                }
            }
        }
        if let Some(why) = missing {
            println!("cts: {module}: BUILD_FAIL {why}");
            modules.push(Module::without_run(module, State::BuildFail, why));
            continue;
        }
        let instrumentation =
            pick_instrumentation(&test.manifest).map_err(|e| anyhow::anyhow!("{module}: {e}"))?;
        ensure!(
            packages
                .iter()
                .any(|(_, m)| m.name == instrumentation.target_package),
            "{module}: instrumentation {} targets package {}, which no staged package is: add the module that builds it to `apps` in {}",
            instrumentation.name,
            instrumentation.target_package,
            cts_table::FILE
        );
        let component = format!("{}/{}", test.manifest.name, instrumentation.name);
        let run = Run {
            root: work.join(format!("root-{module}")),
            scratch: work.join(format!("scratch-{module}")),
            packages,
            first_uid: APP_UID,
            launch: None,
            libraries: libraries.clone(),
            shell: Some(instrument::am_shell(&rt, am_args(row, &component))),
            log_tags: "*:i".to_owned(),
        };
        println!("cts: {module}: running {component}");
        let o = instrument::am_instrument(&rt, &run, limit, args.verbose)?;
        let result = cts_report::classify(module, &o.session, o.timed_out, args.limit_secs);
        let services_log = std::fs::read_to_string(run.log("services")).unwrap_or_default();
        all_counters.push(counters(&services_log));
        println!(
            "cts: {module}: {} in {:.1?}: {} pass, {} fail, {} error, {} ignored of {}{}",
            result.state.name(),
            o.elapsed,
            result.pass,
            result.fail,
            result.error,
            result.ignored,
            result.total,
            if result.detail.is_empty() {
                String::new()
            } else {
                format!(" ({})", result.detail)
            }
        );
        if result.state != State::Ran {
            println!(
                "---- am output ({module}, last {TAIL} lines)\n{}",
                tail(&o.lines.join("\n"), TAIL)
            );
            println!("---- atld log ({module})\n{}", tail(&o.atld_log, TAIL));
            println!(
                "---- services log ({module})\n{}",
                tail(&services_log, TAIL)
            );
            println!("---- logs: {}", run.root.join("log").display());
        }
        modules.push(result);
    }
    let report = Report {
        schema: cts_report::SCHEMA,
        host: args.host.clone(),
        modules,
        counters: cts_report::sum_counters(&all_counters),
    };
    let file = work.join("report.json");
    std::fs::write(&file, serde_json::to_string_pretty(&report)? + "\n")
        .with_context(|| format!("writing {}", file.display()))?;
    println!("cts: report {}", file.display());
    for line in cts_report::summary(&report, args.top) {
        println!("{line}");
    }
    let Some(base) = base else {
        return Ok(ExitCode::SUCCESS);
    };
    let worse = cts_report::regressions(&base, &report, args.modules.is_empty());
    if worse.is_empty() {
        println!("cts: no regression against the baseline");
        return Ok(ExitCode::SUCCESS);
    }
    const SHOWN: usize = 40;
    println!("cts: {} regressions against the baseline:", worse.len());
    for w in worse.iter().take(SHOWN) {
        println!("cts:   {w}");
    }
    if worse.len() > SHOWN {
        println!("cts:   ... and {} more", worse.len() - SHOWN);
    }
    Ok(ExitCode::FAILURE)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decl(name: &str, target: &str) -> InstrumentationDecl {
        InstrumentationDecl {
            name: name.into(),
            target_package: target.into(),
            target_processes: None,
            handle_profiling: false,
            functional_test: false,
        }
    }

    fn manifest(instrumentation: Vec<InstrumentationDecl>) -> Manifest {
        Manifest {
            name: "p.t".into(),
            version_code: 1,
            version_name: None,
            min_sdk: 1,
            target_sdk: 1,
            process_name: None,
            launch_activity: None,
            requested_permissions: Vec::new(),
            libraries: Vec::new(),
            instrumentation,
        }
    }

    /// The self-targeting instrumentation wins over one aimed at an app, the only one is taken whatever it targets, and several candidates are refused.
    #[test]
    fn the_instrumentation_is_derived_from_the_manifest() {
        let own = decl("a.Runner", "p.t");
        let other = decl("b.Runner", "p.app");
        let pick = |v: Vec<InstrumentationDecl>| {
            pick_instrumentation(&manifest(v)).map(|i| i.name.clone())
        };
        assert_eq!(pick(vec![own.clone()]), Ok("a.Runner".into()));
        assert_eq!(
            pick(vec![other.clone(), own.clone()]),
            Ok("a.Runner".into())
        );
        assert_eq!(pick(vec![other.clone()]), Ok("b.Runner".into()));
        assert!(
            pick(vec![])
                .unwrap_err()
                .contains("declares 0 instrumentations")
        );
        assert!(
            pick(vec![own.clone(), decl("c.Runner", "p.t")])
                .unwrap_err()
                .contains("2 target the package itself")
        );
        assert!(
            pick(vec![other.clone(), decl("c.Runner", "p.other")])
                .unwrap_err()
                .contains("0 target the package itself")
        );
    }

    #[test]
    fn filters_come_before_the_component() {
        let none = Row::default();
        assert_eq!(
            am_args(&none, "p/R"),
            ["instrument", "-w", "-r", "p/R"].map(String::from)
        );
        let row = Row {
            classes: vec!["a.B".into(), "a.C#t".into()],
            packages: vec!["a".into()],
            reason: Some("r".into()),
            ..Row::default()
        };
        assert_eq!(
            am_args(&row, "p/R"),
            [
                "instrument",
                "-w",
                "-r",
                "-e",
                "class",
                "a.B,a.C#t",
                "-e",
                "package",
                "a",
                "p/R"
            ]
            .map(String::from)
        );
    }

    /// A failure in a dependency of the module is its blocker, named by its first cause; a failed module that waits on another failed one is not (the root cause is), and an unrelated failure is not.
    #[test]
    fn blockers_are_the_root_failures_in_the_closure() {
        let graph = triage::Graph {
            deps: [
                ("device/T", vec!["device/L1", "device/L2"]),
                ("device/L1", vec!["device/Base"]),
                ("device/L2", vec![]),
                ("device/Other", vec![]),
            ]
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v.into_iter().map(str::to_owned).collect()))
            .collect(),
            ..triage::Graph::default()
        };
        let log = "FAILED: device/Base/x.o\ncmd\nbase.c:1:1: error: no such header\n[1/9] next\n\
                   FAILED: device/L1/y.o\ncmd\ny.c:1:1: error: undeclared\n[2/9] next\n\
                   FAILED: device/Other/z.o\ncmd\nz.c:1:1: error: unrelated\n[3/9] next\n";
        let failures = triage::failures(log, &[]);
        assert_eq!(
            build_blockers(&graph, &failures, "T"),
            ["device/Base: base.c:1:1: error: no such header"]
        );
        assert!(build_blockers(&graph, &failures, "Other").len() == 1);
        assert!(build_blockers(&graph, &triage::failures("", &[]), "T").is_empty());
    }
}
