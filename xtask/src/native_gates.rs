// SPDX-License-Identifier: MIT
//! Native's gates: what `xtask native` judges its outputs by, each a [`Failure`] naming it. native.rs keeps the order of work (convert, ninja, packages) and calls [`run`] once after ninja, [`table`] at each exit.
//! Apart from native.rs so the build record can name the gate code a build passed (build_state.rs `GATE_CODE` hashes this file and the modules it judges with). A gate called from native.rs directly is outside that hash.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result};

use crate::native::{LICENCE_REPORT, NB_DIR};
use crate::providers::{Partial, Provided};
use crate::verdict::Failure;
use crate::{arches, licence_gate, licenses, ndk_surface, provided_gate, triage};

/// One native run's outputs and the values its gates need.
pub(crate) struct Ctx<'a> {
    pub root: &'a Path,
    pub graph_out: &'a Path,
    pub tree: &'a Path,
    pub graph: &'a triage::Graph,
    pub arches: &'a arches::Arches,
    pub host: &'a arches::Host,
    pub tiers: &'a crate::isa_tiers::Tiers,
    pub provided: &'a BTreeMap<&'a str, Provided>,
    pub partial: &'a BTreeMap<&'a str, Partial>,
    /// The guest arch of a foreign native-bridge graph; None = the host's graph.
    pub foreign: Option<&'a str>,
    /// Ninja succeeded.
    pub built: bool,
    pub verbose: bool,
}

/// Every gate over the graph's outputs, whether ninja succeeded or not: the failed ones, each with its first error line.
pub(crate) fn run(ctx: &Ctx) -> Result<Vec<Failure>> {
    let Ctx {
        root,
        graph_out,
        tree,
        graph,
        arches,
        host,
        tiers,
        provided,
        partial,
        foreign,
        built,
        verbose,
    } = *ctx;
    // a crate-source provider has no library: the converter builds the tree module, whose output the gates read as the build's own
    let provided_libs: BTreeSet<&Path> = provided
        .values()
        .chain(partial.values().filter_map(|p| p.provider.as_ref()))
        .filter(|p| p.krate.is_none())
        .map(|p| p.lib.as_path())
        .collect();
    // every check that fails the run, with its first error line: the last line names and quotes them (`verdict`)
    let mut failed: Vec<Failure> = Vec::new();
    let provided_libs: Vec<&Path> = provided_libs.into_iter().collect();
    if let Some(first) = gate(graph_out, provided_libs.iter().copied())? {
        failed.push(Failure::step("provided gate", first));
    }
    if let Some(first) = runpath_gate(graph_out)? {
        failed.push(Failure::step("runpath gate", first));
    }
    if let Some(first) = selfbind_gate(graph_out, graph)? {
        failed.push(Failure::step("selfbind gate", first));
    }
    // a foreign guest graph holds only guest-arch ELFs under nb/<arch> (no host-ISA product to check): the gate would fail on "nothing checked"
    if foreign.is_some() {
        println!("native: isa gate: foreign guest graph holds no host-ISA products: not checked");
    } else if let Some(first) = isa_floor_gate(root, graph_out, host, tiers, &provided_libs)? {
        failed.push(Failure::step("isa gate", first));
    }
    // a foreign guest graph's own outputs: the guest row's class, machine and (arm) softfp EABI5 (isa_gate `guest_check`)
    if let Some(arch) = foreign {
        let bits = arches
            .get(arch)
            .with_context(|| format!("--native-bridge {arch}: no row in {}", arches::FILE))?
            .pointer_bits;
        if let Some(first) = guest_abi_gate(graph_out, arch, bits)? {
            failed.push(Failure::step("guest abi gate", first));
        }
    }
    if let Some(first) = fronts_gate(graph_out, tree)? {
        failed.push(Failure::step("fronts gate", first));
    }
    if let Some(first) = licence_gate(graph_out, built, verbose)? {
        failed.push(Failure::step("licence gate", first));
    }
    Ok(failed)
}

/// The table check over the problems collected by the run: called after the packages, or after a failed ninja.
pub(crate) fn table(problems: &[String]) -> Option<Failure> {
    table_check(problems).map(|first| Failure::step("table check", first))
}

/// Prints the table check's problems; the first one, None when there are none.
fn table_check(problems: &[String]) -> Option<String> {
    println!("native: table check: {} problems", problems.len());
    for p in problems {
        println!("native:   table: {p}");
    }
    problems.first().cloned()
}

/// The provided gate (provided_gate.rs) over the device artifacts ninja produced; false on a violation.
/// Every provided shared object, whatever its row kind (`srcs` rows' included): the converter links each one first, and an in-tree strong definition of a provided symbol is a second implementation wherever it comes from.
/// Archives are not checked: their members are copied into the dependent, so there is no DT_NEEDED order or cross-object lookup to get wrong.
/// The provided gate's first violation, None when it passes.
fn gate<'a>(out: &Path, libs: impl Iterator<Item = &'a Path>) -> Result<Option<String>> {
    let exported = libs
        .filter(|lib| lib.extension().is_some_and(|e| e == "so"))
        .map(provided_gate::exported)
        .collect::<Result<Vec<_>>>()?;
    if exported.is_empty() {
        return Ok(None);
    }
    let current = provided_gate::current_outputs(out, &["device"])?;
    let report = provided_gate::check(&out.join("device"), &exported, &current)?;
    println!(
        "native: provided gate: {} libraries, {} artifacts, {} weak fallbacks lose, {} violations",
        exported.len(),
        report.artifacts,
        report.fallbacks,
        report.violations.len()
    );
    for v in &report.violations {
        println!("native:   gate: {v}");
    }
    Ok(report.violations.first().map(ToString::to_string))
}

/// The runpath gate (runpath_gate.rs) over this graph's fresh link outputs: the device ones from the licence edge's list (`device_links`, rustc binaries too) and `<out>/nb/` by rule link (not in that list); a stale one's failed link is ninja's to report. Plus the converter-built Rust packages' libraries (`<out>/crates/*/lib`, packages.rs: cargo links them, not ninja); its first violation, None when it passes.
fn runpath_gate(out: &Path) -> Result<Option<String>> {
    let mut all = crate::runpath_gate::device_links(out)?;
    all.extend(crate::runpath_gate::link_outputs(out, &[out.join(NB_DIR)])?);
    let (mut artifacts, _) = crate::runpath_gate::fresh(out, all);
    artifacts.extend(crate::runpath_gate::package_libs(&out.join("crates"))?);
    let (read, violations) = crate::runpath_gate::check(out, &artifacts)?;
    println!(
        "native: runpath gate: {read} artifacts, {} runpath entries",
        violations.len()
    );
    for v in violations.iter().take(5) {
        println!("native:   runpath: {v}");
    }
    Ok(violations.first().cloned())
}

/// The selfbind gate (selfbind_gate.rs) over this graph's fresh link outputs under `<out>/device`, the glibc-side libraries app processes load from glibc's scope; its first violation, None when it passes.
/// Expected = graph.json's glibc-side shared artifacts (selfbind_gate.rs [`crate::selfbind_gate::expected`]), so the set comes from the goal set, not from the probe: each one not read as a shared object (and not a failed build's stale output, which fails elsewhere) fails the gate.
/// A graph with none (a foreign guest graph: nb/<arch> bionic guests only) says so with its guest count, not a silent 0.
fn selfbind_gate(out: &Path, graph: &triage::Graph) -> Result<Option<String>> {
    let (expected, guests) = crate::selfbind_gate::expected(graph, out);
    if expected.is_empty() {
        println!(
            "native: selfbind gate: no glibc-side shared library in this graph's goals ({guests} bionic guest artifacts, not in the class): nothing to check"
        );
        return Ok(None);
    }
    let all = crate::runpath_gate::device_links(out)?;
    let (artifacts, stale) = crate::runpath_gate::fresh(out, all);
    let (read, violations) = crate::selfbind_gate::check(out, &artifacts)?;
    let unread = crate::selfbind_gate::unread(&expected, &read, &stale);
    println!(
        "native: selfbind gate: {} of {} expected glibc-side shared objects read ({} stale), {} preemptible self-references",
        read.len(),
        expected.len(),
        stale.len(),
        violations.len()
    );
    for v in violations.iter().take(5) {
        println!("native:   selfbind: {v}");
    }
    if let Some(first) = unread.first() {
        return Ok(Some(format!(
            "{} expected glibc-side shared libraries not read as shared objects, first {}",
            unread.len(),
            first.display()
        )));
    }
    Ok(violations.first().cloned())
}

/// The NDK fronts gate: each built `device/<m>/<m>.so` of ndk_surface.rs `FRONT_SCRIPTS` exports exactly its version script's symbols under their nodes, the surface an NDK app links against; its first mismatch, None when it passes. A front this graph did not build is not checked; the count says how many were.
fn fronts_gate(out: &Path, tree: &Path) -> Result<Option<String>> {
    let mut checked = 0;
    let mut first = None;
    for (module, map) in ndk_surface::FRONT_SCRIPTS {
        let lib = out.join("device").join(module).join(format!("{module}.so"));
        if !lib.is_file() {
            continue;
        }
        checked += 1;
        let bad = ndk_surface::front_mismatch(&lib, &tree.join(map))?;
        for b in bad.iter().take(5) {
            println!("native:   fronts: {module}: {b}");
        }
        if first.is_none()
            && let Some(b) = bad.first()
        {
            first = Some(format!(
                "{module}: {b} ({} differences from {map})",
                bad.len()
            ));
        }
    }
    println!(
        "native: fronts gate: {checked} of {} NDK fronts checked against their version scripts",
        ndk_surface::FRONT_SCRIPTS.len()
    );
    Ok(first)
}

/// The isa gate (isa_gate.rs: ISA, glibc floor, machine per dir, DT_NEEDED) over this cross host's outputs; its first violation, None when it passes. The build machine's row has no isa: not checked.
fn isa_floor_gate(
    root: &Path,
    out: &Path,
    host: &arches::Host,
    tiers: &crate::isa_tiers::Tiers,
    provided: &[&Path],
) -> Result<Option<String>> {
    if host.isa.is_none() {
        return Ok(None);
    }
    let sysroot = crate::isa_gate::sysroot_dirs(root, host)?;
    // a provided archive (bssl-sys's librust_wrapper.a) is no ELF of its own: its members are copied into the outputs the gate reads
    let shared: Vec<&Path> = provided
        .iter()
        .copied()
        .filter(|p| p.extension().is_none_or(|e| e != "a"))
        .collect();
    let report = crate::isa_gate::check(
        out,
        host,
        tiers,
        &crate::isa_tiers::current_objects(out)?,
        &shared,
        &sysroot,
    )?;
    for line in crate::isa_gate::summary(&report) {
        println!("native: isa gate: {line}");
    }
    for (_, v) in report.violations.iter().take(5) {
        println!("native:   isa: {v}");
    }
    Ok(crate::isa_gate::first_violation(&report).map(str::to_owned))
}

/// The guest gate (isa_gate.rs `guest_check`) over a foreign native-bridge graph: every ELF of guest `arch` (pointer width `bits`) has the guest row's ABI. Its first violation, None when it passes.
fn guest_abi_gate(out: &Path, arch: &str, bits: u32) -> Result<Option<String>> {
    let report = crate::isa_gate::guest_check(out, arch, bits)?;
    println!(
        "native: guest abi gate: {arch}: {} ELF files, {} violations",
        report.files,
        report.violations.len()
    );
    for v in report.violations.iter().take(5) {
        println!("native:   guest abi: {v}");
    }
    Ok(report.violations.first().cloned())
}

/// The licence gate's first problem from its edge's report, None when it passes. After a failed build the report counts only when ninja would not rerun the edge (`ninja -n`): otherwise an artifact it reads failed to build, so it did not run.
fn licence_gate(out: &Path, built: bool, verbose: bool) -> Result<Option<String>> {
    let file = out.join(LICENCE_REPORT);
    if !built {
        let dry = Command::new("ninja")
            .arg("-C")
            .arg(out)
            .arg("-n")
            .arg(&file)
            .output()
            .context("running ninja -n for the licence gate")?;
        if !String::from_utf8_lossy(&dry.stdout).contains("no work to do") {
            println!("native: licence gate: not run (an artifact it checks did not build)");
            return Ok(Some(
                "not run: an artifact it checks did not build".to_owned(),
            ));
        }
    }
    let report: licence_gate::Report = serde_json::from_str(
        &std::fs::read_to_string(&file).with_context(|| format!("reading {}", file.display()))?,
    )
    .with_context(|| file.display().to_string())?;
    Ok(licence_report("licence gate", &report, verbose))
}

/// Prints a licence gate report under `label`; its first problem, None when it passes.
fn licence_report(label: &str, report: &licence_gate::Report, verbose: bool) -> Option<String> {
    println!(
        "native: {label}: {} artifacts ({} test-only skipped), {} files checked, not checked: {} generated, {} system, {} crate archive members; {} licences denied, {} files unresolved",
        report.artifacts,
        report.test_only,
        report.checked,
        report.generated,
        report.system,
        report.crates,
        report.denied.len(),
        report.unresolved.len()
    );
    let shown = |n: usize| if verbose { n } else { n.min(5) };
    for (expr, files) in &report.denied {
        println!(
            "native:   licence: `{expr}` is not allowed in [artifact.{}]: {} files",
            licenses::RUNTIME,
            files.len()
        );
        for (file, artifact) in files.iter().take(shown(files.len())) {
            println!("native:     {file} (in {artifact})");
        }
    }
    for u in report
        .unresolved
        .iter()
        .take(shown(report.unresolved.len()))
    {
        println!("native:   licence: unresolved {u}");
    }
    if report.ok() {
        return None;
    }
    let first = report
        .denied
        .iter()
        .flat_map(|(expr, files)| {
            files.iter().map(move |(file, artifact)| {
                format!(
                    "{file} (in {artifact}): `{expr}` is not allowed in [artifact.{}]",
                    licenses::RUNTIME
                )
            })
        })
        .chain(report.unresolved.iter().map(|u| format!("unresolved {u}")))
        .next();
    Some(first.unwrap_or_else(|| format!("{label} failed")))
}
