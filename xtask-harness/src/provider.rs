// SPDX-License-Identifier: MIT
//! `xtask test provider`: an app's ContentProviders in an atld run (isolated runtime tier). Fixture xtask-harness/testdata/provider; checks are lines of its log (tag AtlProvider):
//! - order: High (initOrder 9), then Low (1), then Application.onCreate (bindApplication's list, ActivityThread.installContentProviders);
//! - filter: the disabled provider and the one of process `:other` do not start here;
//! - lookups: getProviderInfo carries `<meta-data>`, resolveContentProvider finds the second authority of a `;` list, ContentResolver reaches the process's own provider.
//!
//! `--break-provider-list` empties the provider table given to the services: every order and lookup check must fail.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use crate::manifest::MetaValue;
use anyhow::{Result, ensure};
use atl_layout::Layout;

use crate::atld_run::{Package, Run, Runtime};
use crate::manifest;
use crate::wait::{Miss, Wait, judge_checks, stopped_at, until_child};

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
    /// Hand the services the package with no providers (deliberate break): the order and lookup checks must fail.
    #[arg(long)]
    break_provider_list: bool,
    /// Read the manifest without resolving `android:value` references (deliberate break): the `@string` meta-data check must fail.
    #[arg(long)]
    break_meta_ref: bool,
    /// Print the app's whole log.
    #[arg(long, short)]
    verbose: bool,
}

const FIXTURE: &str = "xtask-harness/testdata/provider";
/// A cold start (ART without a boot image interprets framework.jar's class init), as `xtask test app`'s limit.
const LIMIT: Duration = Duration::from_secs(180);
const TAIL: usize = 30;

const HIGH: &str = "AtlProvider: provider created High";
const LOW: &str = "AtlProvider: provider created Low";
const OFF: &str = "AtlProvider: provider created Off";
const ELSEWHERE: &str = "AtlProvider: provider created Elsewhere";
const APPLICATION: &str = "AtlProvider: application created";
const META_DATA: &str = "AtlProvider: meta-data startup";
const RESOLVED: &str = "AtlProvider: resolved com.example.atlprovider.High";
const OWN_TYPE: &str = "AtlProvider: own type High";
const DONE: &str = "AtlProvider: done";

fn tail(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    let _ = Layout::of(root)?;
    let rt = Runtime::new(
        root,
        "provider",
        &args.host,
        args.out.as_deref(),
        args.java_out.as_deref(),
        "provider",
        args.verbose,
    )?;
    let work = rt.out.join("provider");
    let apk = rt
        .apk
        .build(&root.join(FIXTURE), &work.join("app"), &[], None, None)?;
    let read = if args.break_meta_ref {
        manifest::read_unresolved
    } else {
        manifest::read
    };
    let mut m = read(&rt.apk.aapt2, &apk)?;
    // the fixture's res/values/strings.xml `meta_ref`, through aapt2's real dump
    let wanted = (
        "com.example.atlprovider.REF".to_owned(),
        MetaValue::String("from-resource".to_owned()),
    );
    ensure!(
        m.application_meta_data.contains(&wanted),
        "provider: FAIL @string meta-data not resolved: {:?}",
        m.application_meta_data
    );
    println!(
        "provider: built {} ({} providers)",
        apk.display(),
        m.providers.len()
    );
    if args.break_provider_list {
        m.providers.clear();
    }
    let package = m.name.clone();
    let run = Run {
        root: work.join("root"),
        scratch: work.join("scratch"),
        launch: Some(package.clone()),
        packages: vec![Package::base(apk)],
        libraries: vec![],
        shell: None,
        log_tags: "*:i".into(),
    };
    let mut atld = rt.start_atld(&run)?;
    let read = atl_diag::logfiles::read;
    let log = run.log(&package);
    let start = Instant::now();
    let done_line = |t: &str| t.contains(DONE);
    let done = Wait::log(
        "provider-done",
        format!("no {DONE:?} line within {LIMIT:?}"),
        LIMIT,
        &log,
        &done_line,
    );
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
    if args.verbose {
        println!("---- app log\n{text}");
    }
    let (high, low, app) = (text.find(HIGH), text.find(LOW), text.find(APPLICATION));
    let checks = [
        (
            "providers start by initOrder, before Application.onCreate",
            matches!((high, low, app), (Some(h), Some(l), Some(a)) if h < l && l < a),
        ),
        (
            "a disabled provider and another process's do not start",
            app.is_some() && !text.contains(OFF) && !text.contains(ELSEWHERE),
        ),
        (
            "getProviderInfo carries meta-data",
            text.contains(META_DATA),
        ),
        (
            "resolveContentProvider finds a listed authority",
            text.contains(RESOLVED),
        ),
        (
            "ContentResolver reaches the process's own provider",
            text.contains(OWN_TYPE),
        ),
    ];
    let mut whys = match &outcome {
        Err(late @ Miss::Late { .. }) => stopped_at("provider", &checks, late),
        _ => judge_checks("provider", &checks),
    };
    if whys.is_empty() {
        println!(
            "provider: pass ({} checks in {:.1?})",
            checks.len(),
            start.elapsed()
        );
        return Ok(ExitCode::SUCCESS);
    }
    if let Some(e) = ended {
        whys.push(format!("atld exited {e}"));
    }
    println!("---- atld log\n{}", tail(&read(&run.atld_log()), TAIL));
    println!("---- app log (last {TAIL})\n{}", tail(&text, TAIL));
    println!(
        "---- services log (last {TAIL})\n{}",
        tail(&read(&run.log("services")), TAIL)
    );
    println!("{}", crate::verdict::gate("provider", &whys));
    Ok(ExitCode::FAILURE)
}
