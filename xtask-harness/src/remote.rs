// SPDX-License-Identifier: MIT
//! `cargo xtask remote`: runs WSL-built outputs on a Linux desktop over ssh (the maintainer's laptop, plan L1).
//!
//! The record: a gate's last run in a build dir writes what its runtime read (atld_run.rs `record_inputs`, via [`write_record`]): host files, and host directories whose trees the bundle takes whole.
//! `push` bundles that record, mapped to `out/`, `java/` and `src/aosp/` names, plus `lib/`: Debian's LLVM 19 runtimes (a labelled patch until our pinned runtimes ship in an install tree, plan L1 / D1).
//! A directory is replicated with its symlinks kept as links and never followed (the ART tree's `com.android.art -> .` would loop); files are hard links, copies across filesystems.
//! Every name of one source file stays one file on the target (staged as hard links, synced with `rsync -H`): a library reached by two names (an `app-art/lib64` link and its `device/` file) would otherwise load twice with separate state -- ANGLE's EGL loader opens `libGLESv2_angle.so` beside itself while DT_NEEDED finds the other copy, so its EGL current context and its GL entries part.
//! Dry run by default. `--apply` stages the bundle under `<out>/remote/bundle/<gate>` (with a manifest of sha256 + size, or the link target) and runs `rsync -aH --delete` to `~/atl-next-run/<name>/` on the host.
//! `test` runs the prerequisite check on the target first: `ldd` over every ELF of the bundle, with the bundle's directories on `LD_LIBRARY_PATH`. Each `not found` soname is named with the file that needs it, a file the push did not deliver is named, and any of them is red. Then the runtime half ([`runtime_half`]): `gl` runs here, each of its atld runs runs on the target from the bundle ([`RemoteRun::spawn`]), and its judge reads the logs the target returns.
//! Host: an ssh alias from the operator's WSL `~/.ssh/config`. The alias is the only handle: no address, user or key goes into the repo or a flag. `local:<absolute dir>` is a directory standing in for the target (the local gate).
//! Limit: `ldd` here also searches this host's own libraries, so on WSL a missing bundle copy still resolves. The `drop-libunwind` break is red only on a host without its own `libunwind.so.1`.
//! `local:` runs the same script on this host, so it cannot show a bundle file naming a WSL path the target lacks: only a real target does.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, ExitStatus, Output, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail, ensure};
use atl_labwc_setup::{READY_LINE, REFUSED_EXIT};

use crate::test_apks::sha256_hex;
use crate::{arches, gate_goals, gl_probe, nested_wayland, out, provided_gate, verdict};

/// rsync's flags for the bundle: archive, the staged hard links kept (`-H`: one source = one file on the target, see the module doc), stale entries deleted -- except the run lock, which a
/// push holds itself ([`push_lock_sh`]) and must not unlink under a run or a later push. `--delay-updates`: an interrupted push leaves the old tree whole (new files wait in `.~tmp~`), not new files beside the old marker.
const RSYNC: [&str; 5] = [
    "-a",
    "-H",
    "--delete",
    "--delay-updates",
    "--exclude=/.run.lock",
];

/// The empty file a stage writes beside the manifest, named by the manifest's SHA-256 ([`marker_name`]): a run tests that it exists ([`stale_sh`]).
/// A different manifest is a different path, so rsync always sends it (its quick check, size + mtime, would skip a same-size manifest written within the same second) and `--delete` drops the old one.
const MARKER_PREFIX: &str = ".bundle-";

fn marker_name(manifest_sha256: &str) -> String {
    format!("{MARKER_PREFIX}{manifest_sha256}")
}

/// The bundle one push staged and sent: its manifest's SHA-256. A run takes it from its own push ([`Target::push`]), never from re-reading a stage dir another push may have refilled.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pushed(String);

/// The bundle's `lib/`: Debian's LLVM 19 runtimes. Arch ships libc++ 23 but no LLVM `libunwind.so.1` (plan L1 section 3). A labelled patch: the ideal is our pinned runtimes (runtimes.rs) in a packaged install tree (plan D1).
/// The build machine's own copy; a cross host's runs take its sysroot's ([`llvm_lib_dir`]).
pub const LLVM_LIB_DIR: &str = "/usr/lib/llvm-19/lib";

/// The LLVM runtimes' dir of a run whose out dir is `out`: the build machine's [`LLVM_LIB_DIR`]; a cross host's out dir (native.rs `cross_host_of`) takes the same path inside its sysroot (deps.toml: trixie's native llvm-toolchain-19 packages of that arch) -- the build machine's are the wrong ISA there.
pub fn llvm_lib_dir(root: &Path, out: &Path) -> Result<PathBuf> {
    let Some(host) = xtask::native::cross_host_of(root, out)? else {
        return Ok(PathBuf::from(LLVM_LIB_DIR));
    };
    let arches = arches::load(root)?;
    let sysroot = arches
        .get(&host)
        .and_then(|r| r.host.as_ref())
        .and_then(|h| h.sysroot.clone())
        .with_context(|| format!("arches.toml `{host}`: a cross host without a sysroot"))?;
    Ok(xtask::deps::sysroot_root(root, &sysroot)?.join(LLVM_LIB_DIR.trim_start_matches('/')))
}
/// (soname, file in [`LLVM_LIB_DIR`]). Each file's SONAME must equal its soname before it is bundled.
const LLVM_RUNTIMES: [(&str, &str); 3] = [
    ("libc++.so.1", "libc++.so.1.0"),
    ("libc++abi.so.1", "libc++abi.so.1.0"),
    ("libunwind.so.1", "libunwind.so.1.0"),
];
/// Bundle names of the `lib/` entries.
const LIB_PREFIX: &str = "lib/";
/// The break's entry: its name in the bundle.
const LIBUNWIND: &str = "lib/libunwind.so.1";
/// atld and the labwc setup helper: the two programs a run starts by name, out of the gate products of the run's host (gate_build.rs `BINS`, one release dir). Each is a runtime input (atld_run.rs `runtime_inputs`), so a record names it; a record without one is refused ([`Bundle::of`]).
pub const RUN_PROGRAMS: [&str; 2] = [ATLD, LABWC_SETUP];
const ATLD: &str = "atld";
/// A remote test run starts it beside its own labwc ([`nested_headless_script`]).
const LABWC_SETUP: &str = "atl-labwc-setup";
/// The helper's log in a run's scratch dir: its lines come back with the run.
const LABWC_SETUP_LOG: &str = "labwc-setup.log";
/// wayvnc's log in a run's scratch dir (`--view`).
const VIEW_LOG: &str = "wayvnc.log";
/// A nested labwc's log in a run's scratch dir.
const LABWC_LOG: &str = "labwc.log";
/// The logs a run's display writes in its scratch dir, those it has.
pub(crate) const DISPLAY_LOGS: [&str; 3] = [LABWC_LOG, LABWC_SETUP_LOG, VIEW_LOG];
/// A run's end record in its scratch dir: atld's exit code ([`ended_early`]).
const ATLD_RC: &str = "atld.rc";
/// Bundle names of a record's roots: the build dir, the java out and the AOSP tree.
const ROOT_NAMES: [(&str, &str); 3] = [("out", "out"), ("java", "java"), ("tree", "src/aosp")];
/// The target's directory for a build dir's bundle, relative to the host's home: `~/atl-next-run/<name>/`.
const REMOTE_ROOT: &str = "atl-next-run";
/// `<out>/remote/...`: the record and the staged bundle.
pub use xtask::native::REMOTE_DIR;
/// In a run's scratch on the target: its CPU possible list, online list and `nproc`, then [`ZONE_SH`]'s two lines, one per line (app.rs HostFacts).
pub const HOST_FACTS: &str = "host.facts";
/// The zone a program started from this shell runs in, as glibc `tzset` picks it (`TZ` when set, else the `/etc/localtime` link), read with the shell's own tools: line 1 the zone id (after `/zoneinfo/`, less a `posix/` / `right/` tree; empty = UTC),
/// line 2 the zone file's TZif footer (RFC 8536 3.3: the POSIX TZ string of its current rule, from which app.rs takes the standard offset). An empty `TZ` reads as UTC with no file. Run locally by `sh -c` and on a target after the run.
pub const ZONE_SH: &str = r#"if [ "${TZ+x}" ]; then z=${TZ#:}; f=$z; case $z in ''|/*) ;; *) f=/usr/share/zoneinfo/$z;; esac; else z=$(readlink /etc/localtime); f=/etc/localtime; fi; z=${z#*/zoneinfo/}; z=${z#posix/}; z=${z#right/}; printf '%s\n' "${z:-UTC}"; if [ -n "$f" ]; then tail -n 1 "$f"; else echo UTC0; fi"#;
/// In a run's scratch on the target: its /proc/cpuinfo.
pub const CPU_INFO: &str = "cpuinfo";
/// The manifest file at the bundle's root: `sha256  size  name` per file, `link  target  name` per link.
const MANIFEST: &str = "bundle.manifest";
/// ssh options: never prompt, never hang. The alias check is the first contact with the host. ServerAlive: a link that dies mid-call fails in about 3 min instead of hanging until the caller's limit.
/// `-F` the user's config: xtask runs inside the build namespace (tree.rs `enter`), a user namespace where root-owned files show as the overflow uid, and ssh refuses the system config's root-owned
/// includes there ("Bad owner or permissions on /etc/ssh/ssh_config.d/..."); the host aliases live in the user's own config.
fn ssh_opts() -> Vec<String> {
    let config = std::env::home_dir().unwrap_or_default().join(".ssh/config");
    let mut opts = vec!["-F".to_owned(), config.display().to_string()];
    for o in [
        "BatchMode=yes",
        "ConnectTimeout=20",
        "ServerAliveInterval=30",
        "ServerAliveCountMax=6",
    ] {
        opts.extend(["-o".to_owned(), o.to_owned()]);
    }
    opts
}

/// [`ssh_opts`] as shell words.
fn ssh_opts_sh() -> String {
    ssh_opts()
        .iter()
        .map(|o| sh_quote(o))
        .collect::<Vec<_>>()
        .join(" ")
}
const MIB: f64 = 1024.0 * 1024.0;

#[derive(clap::Subcommand)]
pub enum Remote {
    /// Bundles a gate's runtime inputs (its last run's record in the build dir) and `lib/`, and copies them to an ssh alias or a local dir.
    ///
    /// Dry run by default: prints the bundle summary (`--list`: every entry) and the rsync it would run. `--apply` stages the bundle under `<out>/remote/bundle/<gate>` with a manifest, then runs `rsync -a --delete` to `~/atl-next-run/<name>/` on the host.
    /// The record is written by a gate's run in the build dir (`xtask test <gate>`); without it this names that command.
    Push(PushArgs),
    /// The prerequisite check on the target: `ldd` over the bundle, every `not found` named with the file that needs it. Then the gate's runtime half: its atld runs execute on the target (`gl`, `app`).
    ///
    /// Dry run by default: prints the check it would run. `--apply` runs it over ssh (or locally for `local:`). `gl`: run `push --apply` first: a file the push did not deliver is named as missing.
    /// `app`: the headless subset (app.rs `remote`): records its run for `--arch`, pushes and checks it, runs atld headless on the target, judges the checks that need no compositor and names each one it skips.
    Test(TestArgs),
    /// `run-apk` on the target: pushes the bundle of the run's record (its APK, the runtime, `lib/`), checks it there, then runs the APK under atld on the target's display for `--hold` seconds (its session's; with no session, the run's own headless labwc, which `--view` serves), with run-apk's verdict.
    ///
    /// Dry run by default: prints the bundle and the plan. `--apply` pushes, checks and runs. The window is on the target's Wayland session, or on the run's own headless labwc when the target has no session (`--view` serves it); the logs come back when the run ends.
    RunApk(crate::run_apk::RemoteArgs),
    /// Lists the target's run dirs (`~/atl-next-run/`) and classifies each: `live` (a checkout of this repository, or `<id>-<arch>` for a host arch), `orphan` (its checkout is gone), `unknown` (not a checkout's name: never touched).
    ///
    /// Dry run by default: prints each dir with its size and the total `--execute` would remove. `--execute` removes the orphans no run or check holds (`.run.lock`), one dir at a time; a held one is skipped.
    Prune(PruneArgs),
}

#[derive(clap::Args)]
pub struct PushArgs {
    /// ssh alias from WSL `~/.ssh/config` (its `Host` name), or `local:<absolute dir>` for a directory standing in for the target.
    #[arg(long)]
    host: String,
    /// The gate whose record is bundled (gate_goals.rs name).
    #[arg(long)]
    gate: String,
    /// The native out dir holding the record (default: this checkout's build dir, see `cargo xtask out show`).
    #[arg(long)]
    out: Option<PathBuf>,
    /// Copies the bundle to the target. Without it: prints what would be copied.
    #[arg(long)]
    apply: bool,
    /// Deliberate break: the bundle without its copy of the named runtime (see the values).
    #[arg(long = "break", value_enum)]
    brk: Option<Break>,
    /// Lists every bundle entry, not the summary.
    #[arg(long)]
    list: bool,
}

#[derive(clap::Args)]
pub struct TestArgs {
    /// The gate to test (gate_goals.rs name): its record names the bundle that is checked.
    gate: String,
    /// ssh alias from WSL `~/.ssh/config`, or `local:<absolute dir>`.
    #[arg(long)]
    pub(crate) host: String,
    /// The native out dir holding the record (default: this checkout's build dir, see `cargo xtask out show`).
    #[arg(long)]
    pub(crate) out: Option<PathBuf>,
    /// Runs the check on the target. Without it: prints the check.
    #[arg(long)]
    pub(crate) apply: bool,
    /// Deliberate break: the bundle without its copy of the named runtime (see the values).
    #[arg(long = "break", value_enum)]
    pub(crate) brk: Option<Break>,
    /// `app` only: the target's arch (see [`TargetArch`]).
    #[command(flatten)]
    pub(crate) target_arch: TargetArch,
    /// `app` only: the fixture's ISA (`test app --fixture-arch`); needed where `--arch` has no identity engine.
    #[arg(long)]
    pub(crate) fixture_arch: Option<String>,
    /// `window` only: the stage (`frames`, `widgets` or `ui-block`; window.rs `REMOTE_STAGES`). Its live half runs on the target next to atld (the harness's hidden `window-live`); UI stages open their window on the target's session display.
    #[arg(long)]
    pub(crate) stage: Option<String>,
    /// `window` only: the composer's deliberate break rule (`test window --break-<rule>`, e.g. `no-backpressure` for ui-block).
    #[arg(long, value_name = "RULE")]
    pub(crate) ui_break: Option<String>,
}

/// `--arch` of a remote run (`remote test app`, `remote run-apk`): one declaration, one selection ([`TargetArch::resolve`]).
#[derive(clap::Args, Clone, Default)]
pub struct TargetArch {
    /// The target's arch (an arches.toml row with `[host]`); its bundle comes from its out dir (a cross host's under the build dir, native.rs `host_out`). Default: the build machine's.
    #[arg(long)]
    pub(crate) arch: Option<String>,
}

impl TargetArch {
    /// The target's arch: `--arch`, else the build machine's. A name without a `[host]` row is refused here, before any build or transfer.
    pub fn resolve(&self, root: &Path) -> Result<String> {
        let arches = arches::load(root)?;
        let (build, _) = arches::build_machine(root, &arches)?;
        let arch = self.arch.as_deref().unwrap_or(build);
        ensure!(
            arches.get(arch).is_some_and(|r| r.host.is_some()),
            "--arch {arch}: not a host arch (an arches.toml row with `[host]`)"
        );
        Ok(arch.to_owned())
    }
}

#[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Break {
    /// Drops `lib/libunwind.so.1`: the prerequisite check must name it on a host with no copy of its own.
    DropLibunwind,
    /// Drops atld from the bundle: the prerequisite check names it as not delivered (`push --apply`), and a run without it stops at the executable check.
    DropAtld,
    /// The run trusts the target's compositor with client dmabufs, its probe's refusal ignored ([`dmabuf_probe_sh`]): on a compositor that imports none, the app's first GTK frame ends it (Gdk Error 71) and the gate is red, not undecided.
    TrustDmabuf,
}

/// In a run's scratch: why the target's display cannot serve the run at all ([`dmabuf_probe_sh`]), one line; it comes back in the tar. A file, not an exit code: the transfer's exit is tar's.
const HOST_LIMIT_FILE: &str = "host.limit";

/// A run the target's display could not serve: neither pass nor fail (xtask-harness main: `<cmd>: undecided host: <why>`, exit [`crate::verdict::UNDECIDED_EXIT`]).
#[derive(Debug)]
pub struct HostLimit(pub String);

impl std::fmt::Display for HostLimit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for HostLimit {}

/// The [`HostLimit`] a returned run's scratch names ([`HOST_LIMIT_FILE`]), if any.
pub(crate) fn host_limit(scratch: &Path) -> Option<HostLimit> {
    let text = std::fs::read_to_string(scratch.join(HOST_LIMIT_FILE)).ok()?;
    Some(HostLimit(text.trim().to_owned()))
}

/// Asks the run's labwc, after its setup, whether it imports a client dmabuf (the helper's `--dmabuf` line, appended to its log). A refusal stops the run before atld, its why in `limit_file`:
/// GTK 4.14's GL renderer presents through Mesa's EGL, which sends dmabufs whatever GDK_DEBUG=dmabuf-disable says, and its Vulkan renderer crashed there; cairo would not render on the GPU
/// (sifive, measured 2026-11-28). `logs` sends the display's logs and `limit_file` back.
fn dmabuf_probe_sh(
    tag: &str,
    setup: &str,
    setup_log: &str,
    limit_file: &str,
    logs: &str,
) -> String {
    let refused = sh_quote(&format!("^{}", atl_labwc_setup::dmabuf::REFUSED_PREFIX));
    format!(
        "{setup} --dmabuf \"$WL\" >> {setup_log} 2>&1 < /dev/null\nif grep -q {refused} {setup_log}; then\n  echo \"remote run {tag}: host limit: the compositor imports no client dmabuf ($(grep {refused} {setup_log} | tail -n 1)); GTK's GL renderer presents only through dmabufs\" | tee {limit_file} >&2\n  {logs}\n  exit 5\nfi\n"
    )
}

#[derive(clap::Args)]
pub struct PruneArgs {
    /// ssh alias from WSL `~/.ssh/config` (its `Host` name). `local:` is refused: its dir is the caller's, not a run dir.
    #[arg(long)]
    host: String,
    /// Removes the orphan dirs no run or check holds. Without it: prints what would be removed.
    #[arg(long)]
    execute: bool,
}

pub fn run(root: &Path, cmd: &Remote) -> Result<ExitCode> {
    match cmd {
        Remote::Push(a) => push(root, a),
        Remote::Test(a) => test(root, a),
        Remote::RunApk(a) => crate::run_apk::remote(root, a),
        Remote::Prune(a) => prune(root, a),
    }
}

fn push(root: &Path, a: &PushArgs) -> Result<ExitCode> {
    let out = out_of(root, a.out.as_deref())?;
    let target = Target::of(root, &out, &a.gate, &a.host, a.brk)?;
    if a.list {
        for e in target.entries() {
            println!("{}\t{}", e.name, e.src.display());
        }
    }
    println!("{}", target.summary());
    let stage = stage_dir(&out, &a.gate, a.brk);
    if !a.apply {
        println!(
            "dry run: would stage {} and rsync -a --delete it to {}; --apply copies",
            stage.display(),
            target.target()
        );
        return Ok(ExitCode::SUCCESS);
    }
    target.push()?;
    println!(
        "pushed {} entries to {}",
        target.entries().len(),
        target.target()
    );
    Ok(ExitCode::SUCCESS)
}

fn test(root: &Path, a: &TestArgs) -> Result<ExitCode> {
    // `app` records its own run here (a cross host's run cannot happen on this host to leave one), then delivers and runs it on the same Target and RemoteRun
    if a.gate == "app" {
        return crate::app::remote(root, a);
    }
    // `window` too: its record is the stage's run, written before the push (window.rs `remote`)
    if a.gate == "window" {
        return crate::window::remote(root, a);
    }
    ensure!(
        a.target_arch.arch.is_none() && a.fixture_arch.is_none(),
        "--arch and --fixture-arch are `app` options"
    );
    ensure!(
        a.stage.is_none() && a.ui_break.is_none(),
        "--stage and --ui-break are `window` options"
    );
    let out = out_of(root, a.out.as_deref())?;
    let target = Target::of(root, &out, &a.gate, &a.host, a.brk)?;
    if !a.apply {
        println!("{}", target.summary());
        println!(
            "dry run: prerequisite check on {}: ldd over {} ELF files, {} library dirs on LD_LIBRARY_PATH; --apply runs it",
            target.target(),
            target.elf_count(),
            target.dir_count()
        );
        println!(
            "dry run: the gate's runtime half would follow: `{}` runs its atld runs on the target (--apply)",
            a.gate
        );
        return Ok(ExitCode::SUCCESS);
    }
    let prereq = target.check()?;
    let mut fails = prereq.problems();
    if fails.is_empty() {
        println!(
            "remote prereq {}: PASS, {} ELF checked on {}",
            a.gate,
            prereq.checked,
            target.label()
        );
    } else {
        for f in &fails {
            println!("remote prereq {}: {f}", a.gate);
        }
    }
    if fails.is_empty() {
        match runtime_half(root, a, &target, &out) {
            Ok(true) => {}
            Ok(false) => fails.push(format!(
                "gate `{}`: a check failed, see its lines above",
                a.gate
            )),
            Err(e) => fails.push(format!("{e:#}")),
        }
    }
    println!(
        "{}",
        verdict::gate(&format!("remote test {}", a.gate), &fails)
    );
    Ok(if fails.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

/// The gate's runtime half on the target. The gate runs here; each atld run it starts runs on the target from the bundle, so its judge reads the logs the target returned and prints what the local gate prints.
/// Only `gl`: its runs are one-shot probes. The other runtime gates drive a live atld (control socket, windows, sensors), which one transfer per run cannot serve. Returns whether the gate passed.
fn runtime_half(root: &Path, a: &TestArgs, target: &Target, out: &Path) -> Result<bool> {
    ensure!(
        a.gate == "gl",
        "gate `{}`: its runtime half is not remotable yet (`gl`: one-shot runs; `app`: its headless subset; `window`: stages frames, widgets, ui-block through the target agent; the others drive a live runtime)",
        a.gate
    );
    let arches = arches::load(root)?;
    let (arch, _) = arches::build_machine(root, &arches)?;
    let dir = out.join(REMOTE_DIR).join(target.log_dir());
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let log = dir.join(format!("{}.log", a.gate));
    std::fs::write(&log, "").with_context(|| format!("writing {}", log.display()))?;
    // `remote test gl` runs what `remote push gl` (same break) staged: a push of another break to the shared dir since then makes the run stale
    let site = target.run_site(
        &target.staged()?,
        gl_probe::LIMIT,
        log.clone(),
        Display::Headless,
    )?;
    let code = gl_probe::run_on(root, arch, out, site)?;
    println!("remote test {}: transcript {}", a.gate, log.display());
    Ok(code == ExitCode::SUCCESS)
}

/// Where a runtime's atld runs: this host, or a target that runs it from the bundle. The gate's judge reads the same log lines either way.
#[derive(Default, Clone)]
pub enum Site {
    #[default]
    Host,
    Remote(RemoteRun),
}

/// A remote site: the target, the bundle's directory there, the library search path `test` checked, atld's time limit, and the transcript the transfers append to.
#[derive(Clone)]
pub struct RemoteRun {
    host: Host,
    /// Relative to the target's home dir over ssh, absolute for `local:`.
    base: String,
    /// Bundle names of the directories holding ELF files: the search path the prerequisite check ran with, and the run's `LD_LIBRARY_PATH`.
    lib_dirs: Vec<String>,
    /// atld's limit on the target (`timeout`): the run ends there even if atld does not.
    limit: Duration,
    /// Each run's header, then the stderr of its transfer.
    log: PathBuf,
    /// Where the run's `WAYLAND_DISPLAY` comes from, if anywhere.
    display: Display,
    /// The window gate's live half, run on the target beside atld ([`Agent`]); boxed: most runs have none.
    agent: Option<Box<Agent>>,
    /// SHA-256 of the manifest this gate's push staged ([`stale_sh`]).
    manifest_sha256: String,
    /// The run's audio on the target ([`RunAudio`]); None: no sink made, no route sampled.
    audio: Option<RunAudio>,
    /// `--view`'s port: wayvnc serves the run's own labwc on the target's loopback there ([`view_script`]); None: no view.
    view: Option<u16>,
    /// [`Target`]'s: the run's own labwc is asked for a client dmabuf import before atld ([`dmabuf_probe_sh`]).
    dmabuf_probe: bool,
}

/// A remote run's audio: `sink` = the run's own null sink, made on the target before atld and gone with the run (audio_pcm.rs `holder_sh`); None = an audible run (the session's routing).
/// Either way `pw-dump` samples the target's graph during the run into the scratch dir's [`PW_SAMPLES`], for the `audio-route:` check (audio_pcm.rs `stream_routes`).
#[derive(Clone, Debug, PartialEq)]
pub struct RunAudio {
    pub sink: Option<String>,
}

/// The scratch subdir holding a run's `pw-dump` samples, `<n>.json`.
pub const PW_SAMPLES: &str = "pw";
/// The file in [`PW_SAMPLES`] naming the target's atld pid, which the `audio-route:` check attributes the run's streams by (audio_pcm.rs `RunClients`).
pub const PW_ATLD_PID: &str = "atld.pid";

/// The window gate's live half on the target (window.rs `agent`, the harness's hidden `window-live`): started before atld, it proxies atld's window to the target's compositor, drives the stage and stops
/// atld through its control socket; it writes `live.json` for the judge here. Host paths, relocated by [`RemoteRun::spawn`].
#[derive(Clone)]
pub struct Agent {
    /// The harness binary (a bundle entry: atld_run.rs `runtime_inputs` lists it for the window gate).
    pub program: PathBuf,
    /// Its arguments before the proxy's sockets.
    pub args: Vec<String>,
    /// After `--`: the window args of the run (stage and flags).
    pub window: Vec<String>,
    /// Whether atld's window goes through the agent's proxy to the run's display (the UI stages): the script then gives the agent `--upstream`/`--listen` and atld the proxy's socket.
    pub proxy: bool,
}

impl Site {
    /// This site with the window gate's agent ([`Agent`]); a host site is returned as it is.
    pub fn with_agent(self, agent: Agent) -> Site {
        match self {
            Site::Remote(mut r) => {
                r.agent = Some(Box::new(agent));
                Site::Remote(r)
            }
            host => host,
        }
    }

    /// This site with `--view` on `port` ([`view_script`]); None or a host site: returned as it is.
    pub fn with_view(self, port: Option<u16>) -> Site {
        match self {
            Site::Remote(mut r) => {
                r.view = port;
                Site::Remote(r)
            }
            host => host,
        }
    }

    /// This site with the run's audio ([`RunAudio`]); a host site is returned as it is (its routing is atld_run.rs `with_audio_routing`).
    pub fn with_audio(self, audio: RunAudio) -> Site {
        match self {
            Site::Remote(mut r) => {
                r.audio = Some(audio);
                Site::Remote(r)
            }
            host => host,
        }
    }
}

/// How long the run waits for the agent's proxy socket, in 1 s polls: the harness binds it before anything else.
const AGENT_WAIT_S: u32 = 10;
/// The agent's margin over atld's limit (`timeout`): it stops atld, then joins its probe.
const AGENT_MARGIN_S: u64 = 60;
/// Files the agent run adds to the run's scratch dir: atld's pid for the agent, the agent's exit, log and record.
pub const ATLD_PID: &str = "atld.pid";
pub const AGENT_RC: &str = "agent.rc";
pub const AGENT_LOG: &str = "agent.log";
pub const LIVE_JSON: &str = "live.json";
/// The hidden `display-probe`'s answer line: atld's `--monitor` value.
pub const DISPLAY_PROBE_LINE: &str = "display-probe: ";
/// The display probe's bound: one Wayland roundtrip after the binary loads.
const DISPLAY_PROBE_LIMIT: Duration = Duration::from_secs(30);

/// Moves the target's Hyprland session to an empty workspace for a run that maps a window on the session display, and back after it (maintainer decision D1, 2026-11-18: a remote test window only on
/// an empty workspace, and the workspace that was active before comes back also on failure or interruption). Probed, never named by host: `hyprctl` on PATH and exactly one running instance
/// (`hyprctl instances`) whose Wayland socket is the run's `$WL` -- `$WL_DIR/hypr` keeps dirs of ended sessions (7 on the laptop, 2026-11-18), so a dir count is no answer. No switch possible
/// (no hyprctl, not one instance, no active workspace, or the session did not move) -> the run is refused before anything starts: `workspace: refused (...)`, exit [`WS_REFUSED_RC`]; a window
/// on the maintainer's active workspace is the one outcome D1 rules out. Empty = the lowest id no workspace has (Hyprland keeps only workspaces with windows or focus). The switch: the classic
/// dispatcher (`workspace N`), and where the session did not move, the Lua one Hyprland 0.57's Lua config takes (`hl.dsp.focus({ workspace = N })`; the classic form is a Lua syntax error there).
/// Restore, two routes, both on the target: the EXIT trap (normal end, failure, HUP/INT/TERM/PIPE) and a guardian started before the switch, detached from the ssh channel, that restores once the
/// run script is gone without the trap having run (SIGKILL, a dropped ssh the shell died of). `$WS_MARK` (`$WL_DIR/atl-ws-<pid>`) exists while the guardian lives; the trap's restore writes it
/// non-empty so the guardian does not repeat it. Lines go to stderr (the transcript): `workspace: ...`.
/// Test runs get no session where the target has labwc ([`probe_test_display`]); this switch then serves run-apk and the test runs of a target without labwc. The refusal here is for a session
/// without a switch and for a switch that went away between the probe and the run.
fn workspace_script(tag: &str) -> String {
    const SCRIPT: &str = r#"@PROBE@if [ -z "$WS_REFUSE" ]; then
  export XDG_RUNTIME_DIR="$WL_DIR"
  export HYPRLAND_INSTANCE_SIGNATURE="$WS_SIG"
  ws_id() { hyprctl activeworkspace | @ID@ | head -n 1; }
  WS_PREV=$(ws_id)
  [ -n "$WS_PREV" ] || WS_REFUSE="hyprctl named no active workspace"
fi
[ -z "$WS_REFUSE" ] || { echo "workspace: refused ($WS_REFUSE): remote run @TAG@ maps no window on the active workspace" >&2; exit @RC@; }
WS_USED=$(hyprctl workspaces | @ID@)
WS_RUN=1
while printf '%s\n' $WS_USED | grep -qx "$WS_RUN"; do WS_RUN=$((WS_RUN + 1)); done
ws_go() {
  hyprctl dispatch workspace "$1" >/dev/null 2>&1
  [ "$(ws_id)" = "$1" ] || hyprctl dispatch "hl.dsp.focus({ workspace = $1 })" >/dev/null 2>&1
}
WS_MARK="$WL_DIR/atl-ws-$$"
: > "$WS_MARK"
ws_back() {
  [ -s "$WS_MARK" ] && return 0
  ws_go "$WS_PREV"
  echo restored > "$WS_MARK"
  echo "workspace: after=$(ws_id) (restored $WS_PREV)" >&2
}
WS_OWNER=$$
(
  trap '' HUP INT TERM PIPE
  while kill -0 "$WS_OWNER" 2>/dev/null; do sleep 1; done
  [ -s "$WS_MARK" ] || ws_go "$WS_PREV"
  rm -f "$WS_MARK"
) < /dev/null > /dev/null 2>&1 &
trap ws_back EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 141' PIPE
trap 'exit 143' TERM
ws_go "$WS_RUN"
WS_NOW=$(ws_id)
[ "$WS_NOW" = "$WS_RUN" ] || { echo "workspace: refused (the session stayed on $WS_NOW, not empty $WS_RUN): remote run @TAG@ maps no window on the active workspace" >&2; exit @RC@; }
echo "workspace: before=$WS_PREV during=$WS_NOW (empty $WS_RUN)" >&2
"#;
    // `hyprctl activeworkspace` / `workspaces` text: `workspace <n> (<name>) on monitor ...` (the laptop's 2026-11-18 build), `workspace ID <n> (<name>) ...` in other releases
    SCRIPT
        .replace("@PROBE@", WS_PROBE)
        .replace(
            "@ID@",
            r"sed -n 's/^workspace \(ID \)\{0,1\}\(-\{0,1\}[0-9][0-9]*\) (.*/\2/p'",
        )
        .replace("@TAG@", &dq(tag))
        .replace("@RC@", &WS_REFUSED_RC.to_string())
}

/// The run script's exit when [`workspace_script`] refuses: the run never started (5 = no display, 4 = no program).
pub const WS_REFUSED_RC: i32 = 6;

/// Where a remote run's Wayland display comes from: the capability the target has, probed before the run ([`Target::probe_display`]), never named by host.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Display {
    /// No compositor: atld runs headless.
    Headless,
    /// The desktop session's one Wayland socket ([`session_script`]).
    Session,
    /// An X11 session without a Wayland socket: the run starts labwc nested in it ([`nested_script`]) and ends it after atld.
    NestedX11,
    /// The run's own headless labwc ([`nested_headless_script`], as the local gates do, nested_wayland.rs), given its outputs and seat by the labwc setup helper ([`nested_headless_script`]), ended after atld: a test run's answer wherever the target has labwc ([`probe_test_display`]),
    /// run-apk's where the target has no session at all, or its session has no workspace switch ([`probe_display`]). Nothing maps on the session; `--view` serves this labwc over VNC.
    NestedHeadless,
}

impl Display {
    /// The run starts labwc and ends it.
    fn nested(self) -> bool {
        matches!(self, Display::NestedX11 | Display::NestedHeadless)
    }
}

/// Whether the session on `$WL` (in `$WL_DIR`) has a workspace switch, read only: `WS_REFUSE` = why not (empty: it has), `WS_SIG` = its Hyprland instance. Shared by [`probe_script`] and [`workspace_script`].
const WS_PROBE: &str = r#"WS_REFUSE=""
if ! command -v hyprctl >/dev/null 2>&1; then
  WS_REFUSE="no hyprctl on PATH: no workspace switch on this compositor"
else
  WS_SIG=$(XDG_RUNTIME_DIR="$WL_DIR" hyprctl instances 2>/dev/null | awk -v wl="$WL" '/^instance /{sig=$2; sub(/:$/, "", sig)} $1 == "wl" && $2 == "socket:" && $3 == wl {print sig}')
  WS_N=$(printf '%s' "$WS_SIG" | grep -c .)
  [ "$WS_N" -eq 1 ] || WS_REFUSE="$WS_N Hyprland instances on $WL, want 1"
fi
"#;

/// [`probe_script`]'s answer for a target with one Wayland socket ([`WS_PROBE`]).
fn session_probe_sh() -> String {
    format!(
        "{WS_PROBE}if [ -n \"$WS_REFUSE\" ] && command -v labwc >/dev/null 2>&1; then echo \"nested-headless the session: $WS_REFUSE\"; else echo session; fi\n"
    )
}

/// The desktop session's runtime dir as the target's shell names it (`/run/user/<uid>`), for [`wl_scan`].
const SESSION_DIR: &str = "\"/run/user/$(id -u)\"";

/// Defines `wl_scan` and runs it once: `WL` = the last `wayland-*` socket (not a lock file) in `dir` (a shell word: [`SESSION_DIR`] on a target), `WL_N` = how many there are.
fn wl_scan(dir: &str) -> String {
    format!(
        "WL_DIR={dir}\nwl_scan() {{\n  WL=\"\"\n  WL_N=0\n  for s in \"$WL_DIR\"/wayland-*; do\n    case \"$s\" in *.lock) continue ;; esac\n    [ -S \"$s\" ] || continue\n    WL=${{s##*/}}\n    WL_N=$((WL_N + 1))\n  done\n}}\nwl_scan\n"
    )
}

/// The target's X11 session, read only: `XD`/`XA` = `DISPLAY`/`XAUTHORITY` of the first own process naming a DISPLAY whose socket (`@X11@/X<n>`) is live; empty when none.
/// From a session process's environment because an ssh login has neither variable. `@PROCS@` and `@X11@` are [`x_session`]'s arguments.
const X_SESSION: &str = "XD=\"\"\nXA=\"\"\nfor e in @PROCS@; do\n  [ -O \"$e\" ] && [ -r \"$e\" ] || continue\n  d=$(tr '\\0' '\\n' < \"$e\" 2>/dev/null | sed -n 's/^DISPLAY=//p' | head -n 1)\n  [ -n \"$d\" ] || continue\n  n=${d#*:}\n  n=${n%%.*}\n  [ -S \"@X11@/X$n\" ] || continue\n  XD=$d\n  XA=$(tr '\\0' '\\n' < \"$e\" 2>/dev/null | sed -n 's/^XAUTHORITY=//p' | head -n 1)\n  break\ndone\n";
/// The environ files [`X_SESSION`] reads on a target, and the X11 socket dir.
const X_PROCS: &str = "/proc/[0-9]*/environ";
const X_DIR: &str = "/tmp/.X11-unix";

/// [`X_SESSION`] over the given environ files (a glob, unquoted) and X11 socket dir: the production read is [`X_PROCS`] and [`X_DIR`]; the probe tests pass fakes.
fn x_session(procs: &str, x11: &str) -> String {
    X_SESSION.replace("@PROCS@", procs).replace("@X11@", x11)
}

/// How long the run waits for a nested labwc's Wayland socket, in 1 s polls. labwc 0.7.1 on sifive's X11 session made its socket within 4 s.
const NESTED_WAIT_S: u32 = 10;

/// The display probe ([`Target::probe_display`]): one line, `session`, `nested-headless <why>`, `nested <DISPLAY>`, or `headless <why>`.
fn probe_script() -> String {
    probe_script_at(SESSION_DIR, X_PROCS, X_DIR)
}

/// [`probe_script`] over a runtime dir (a shell word), the environ files and the X11 socket dir ([`x_session`]).
/// No socket, no X11 session, labwc on PATH: the run's own headless labwc (decision 2026-11-25); a target with a session keeps [`session_probe_sh`]'s answer.
fn probe_script_at(dir: &str, procs: &str, x11: &str) -> String {
    let session = session_probe_sh();
    let wl = wl_scan(dir);
    let x = x_session(procs, x11);
    format!(
        "{wl}if [ \"$WL_N\" -eq 1 ]; then\n{session}exit 0\nfi\n[ \"$WL_N\" -eq 0 ] || {{ echo \"headless $WL_N Wayland sockets in $WL_DIR, want 0 or 1\"; exit 0; }}\ncommand -v labwc >/dev/null 2>&1 || {{ echo \"headless no Wayland socket and no labwc to nest\"; exit 0; }}\n{x}[ -n \"$XD\" ] || {{ echo \"nested-headless no session: no Wayland socket and no X11 session\"; exit 0; }}\necho \"nested $XD\"\n"
    )
}

/// The Wayland display of a run that maps on the session (run-apk) on target `host` (ssh alias or `local:<dir>`), and why ([`probe_script`], read only): the session's one socket, its own headless labwc where that session has no workspace switch or the target has no session at all, else labwc nested in its X11 session, else none (headless; the reason says what is missing).
pub fn probe_display(host: &str) -> Result<(Display, String)> {
    parse_probe(&Host::parse(host)?.run_script(&probe_script())?)
}

/// The Wayland display a test run on target `host` gets, and why ([`test_probe_script`], read only): the run's own headless labwc wherever the target has labwc, whatever the session has (decision 2026-11-23); else the session's one socket; else headless.
pub fn probe_test_display(host: &str) -> Result<(Display, String)> {
    parse_probe(&Host::parse(host)?.run_script(&test_probe_script(SESSION_DIR))?)
}

/// The test run's probe, one line like [`probe_script`] (read only). X11 is not read: the run's labwc is headless whatever display the target has.
fn test_probe_script(dir: &str) -> String {
    let wl = wl_scan(dir);
    format!(
        "{wl}if command -v labwc >/dev/null 2>&1; then echo \"nested-headless the session: $WL_N Wayland socket(s)\"; exit 0; fi\nif [ \"$WL_N\" -eq 1 ]; then echo session; exit 0; fi\n[ \"$WL_N\" -eq 0 ] || {{ echo \"headless $WL_N Wayland sockets in $WL_DIR, want 0 or 1\"; exit 0; }}\necho \"headless no Wayland socket and no labwc to start\"\n"
    )
}

/// Reads [`probe_script`]'s line.
fn parse_probe(out: &str) -> Result<(Display, String)> {
    let line = out.lines().next().unwrap_or_default().trim();
    let (word, rest) = line.split_once(' ').unwrap_or((line, ""));
    Ok(match word {
        "session" => (
            Display::Session,
            "the desktop session's Wayland socket".to_owned(),
        ),
        "nested" => (
            Display::NestedX11,
            format!("labwc nested in X11 display {rest}"),
        ),
        "nested-headless" => (
            Display::NestedHeadless,
            format!("the run's own headless labwc ({rest})"),
        ),
        "headless" => (Display::Headless, rest.to_owned()),
        _ => bail!("display probe: unexpected answer {line:?}"),
    })
}

/// Starts labwc nested in the target's X11 session (`WLR_BACKENDS=x11`, its own empty config dir `cfg`, output to `log`) and waits for its Wayland socket: sets `WL` and `LW` (labwc's pid).
/// Exit 5 when a Wayland socket already exists (the session's must be used, not shadowed), no X11 session is found, or labwc makes no socket within [`NESTED_WAIT_S`].
/// Then `logs` ([`tar_sh`]) comes back, if labwc was started.
fn nested_script(tag: &str, cfg: &str, log: &str, logs: &str) -> String {
    let wl = wl_scan(SESSION_DIR);
    let x = x_session(X_PROCS, X_DIR);
    format!(
        "{wl}[ \"$WL_N\" -eq 0 ] || {{ echo \"remote run {tag}: $WL_N Wayland sockets in $WL_DIR, want 0 for a nested labwc\" >&2; exit 5; }}\n{x}[ -n \"$XD\" ] || {{ echo \"remote run {tag}: no X11 session to nest labwc in\" >&2; exit 5; }}\nLABWC=$(command -v labwc) || {{ echo \"remote run {tag}: no labwc on PATH\" >&2; exit 5; }}\nmkdir -p {cfg}\nenv -i PATH=/usr/local/bin:/usr/bin:/bin HOME=\"$HOME\" XDG_RUNTIME_DIR=\"$WL_DIR\" DISPLAY=\"$XD\" XAUTHORITY=\"$XA\" WLR_BACKENDS=x11 \"$LABWC\" -C {cfg} < /dev/null > {log} 2>&1 &\nLW=$!\ni=0\nwhile wl_scan; [ \"$WL_N\" -eq 0 ] && [ \"$i\" -lt {NESTED_WAIT_S} ]; do\n  kill -0 \"$LW\" 2>/dev/null || break\n  sleep 1\n  i=$((i + 1))\ndone\n[ \"$WL_N\" -eq 1 ] || {{ echo \"remote run {tag}: nested labwc gave $WL_N Wayland sockets in $WL_DIR, want 1\" >&2; kill \"$LW\" 2>/dev/null; {logs}; exit 5; }}\n"
    )
}

/// Starts the run's own labwc on wlroots' headless backend, its socket in a runtime dir of its own (`$WL_DIR/atl-nest-<pid>`: the session's scan never sees it, and every display name in the session's
/// dir stays the session's), then the labwc setup helper (crate atl-labwc-setup's binary `bin`) on that socket, which gives it the gate output and the seat's pointer and keyboard and holds them until labwc ends.
/// `WL` = the socket's absolute path (libwayland takes one as `WAYLAND_DISPLAY`), `LW` = labwc's pid, `HS` = the helper's. They go with the run script: its normal end ([`Plan::script`]) and the EXIT trap.
/// labwc is the target's `labwc` on the script's own PATH, the one the probe found (WSL has a wrapper outside the fixed PATH labwc runs with).
/// Rungs ([`LABWC_RUNGS`]): where the helper says the compositor refused the output ([`REFUSED_EXIT`]), labwc is started once more on the next rung. The transcript names the rung in use and each refusal; `log` and `setup_log` hold every rung's output, in order.
/// Exit 5 when labwc is missing, makes no socket, or the helper is not ready (each within [`NESTED_WAIT_S`]), naming the refusals before it; `logs` ([`tar_sh`]) then comes back.
fn nested_headless_script(
    tag: &str,
    dir: &str,
    cfg: &str,
    (log, setup_log): (&str, &str),
    bin: &str,
    logs: &str,
) -> String {
    const SCRIPT: &str = r#"@SCAN@LABWC=$(command -v labwc) || { echo "remote run @TAG@: no labwc on PATH" >&2; exit 5; }
NEST_DIR="$WL_DIR/atl-nest-$$"
LW=""
nest_end() {
  [ -z "$LW" ] || kill -TERM "$LW" 2>/dev/null
  rm -rf "$NEST_DIR"
}
trap nest_end EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 141' PIPE
trap 'exit 143' TERM
mkdir -m 700 "$NEST_DIR" || exit 5
mkdir -p @CFG@
: > @LOG@
: > @SETUP_LOG@
nest_up() {
  RUNG=$1
  shift
  echo "== labwc rung $RUNG" | tee -a @LOG@ >> @SETUP_LOG@
  env -i PATH=/usr/local/bin:/usr/bin:/bin HOME="$HOME" XDG_RUNTIME_DIR="$NEST_DIR"@ENV@ "$@" "$LABWC" -C @CFG@ < /dev/null >> @LOG@ 2>&1 &
  LW=$!
  WL_DIR="$NEST_DIR"
  i=0
  while wl_scan; [ "$WL_N" -eq 0 ] && [ "$i" -lt @WAIT@ ]; do
    kill -0 "$LW" 2>/dev/null || break
    sleep 1
    i=$((i + 1))
  done
  [ "$WL_N" -eq 1 ] || { echo "remote run @TAG@: headless labwc (rung $RUNG) gave $WL_N Wayland sockets, want 1" >&2; @LOGS@; exit 5; }
  WL="$NEST_DIR/$WL"
  @BIN@ "$WL" >> @SETUP_LOG@ 2>&1 < /dev/null &
  HS=$!
  i=0
  while ! grep -qx @READY@ @SETUP_LOG@ && [ "$i" -lt @WAIT@ ]; do
    kill -0 "$HS" 2>/dev/null || break
    sleep 1
    i=$((i + 1))
  done
  if grep -qx @READY@ @SETUP_LOG@; then
    echo "remote run @TAG@: labwc rung $RUNG" >&2
    return 0
  fi
  kill -TERM "$HS" 2>/dev/null
  wait "$HS"
  rc=$?
  echo "remote run @TAG@: labwc setup not ready on rung $RUNG (see @SETUP_NAME@): $(tail -n 1 @SETUP_LOG@)" >&2
  return $rc
}
nest_retry() {
  [ "$1" -eq @REFUSED@ ] || return 1
  kill -TERM "$LW" 2>/dev/null
  wait "$LW"
  rm -rf "$NEST_DIR"/*
}
@RUNGS@ || { @LOGS@; exit 5; }
"#;
    let rungs: Vec<String> = LABWC_RUNGS
        .iter()
        .map(|(name, words)| format!("nest_up {name}{words}"))
        .collect();
    SCRIPT
        .replace("@RUNGS@", &rungs.join(" || { nest_retry $? && "))
        .replace(
            " || { @LOGS@; exit 5; }\n",
            &format!(
                "{} || {{ @LOGS@; exit 5; }}\n",
                "; }".repeat(rungs.len() - 1)
            ),
        )
        .replace("@SCAN@", &wl_scan(dir))
        .replace("@CFG@", cfg)
        .replace("@SETUP_LOG@", setup_log)
        .replace("@LOG@", log)
        .replace("@ENV@", &labwc_words())
        .replace("@WAIT@", &NESTED_WAIT_S.to_string())
        .replace("@BIN@", bin)
        .replace("@READY@", &sh_quote(READY_LINE))
        .replace("@SETUP_NAME@", LABWC_SETUP_LOG)
        .replace("@REFUSED@", &REFUSED_EXIT.to_string())
        .replace("@LOGS@", logs)
        .replace("@TAG@", tag)
}

/// wayvnc on the run's own labwc socket `WL` (absolute, set by [`nested_headless_script`]), bound to the target's loopback at `port`: never another address, so the port is reached only through the
/// ssh tunnel the run prints. `VN` = its pid; the EXIT trap ends it with the run's labwc, the normal end does too ([`Plan::script`]). Exit 5 when wayvnc is gone 2 s on (its log comes back: `logs`).
fn view_script(port: u16, log: &str, logs: &str) -> String {
    format!(
        "VNC=$(command -v wayvnc) || {{ echo \"remote run-apk: wayvnc is not on PATH on the target\" >&2; {logs}; exit 5; }}\nenv -i PATH=/usr/local/bin:/usr/bin:/bin HOME=\"$HOME\" XDG_RUNTIME_DIR=\"$NEST_DIR\" WAYLAND_DISPLAY=\"$WL\" \"$VNC\" 127.0.0.1 {port} < /dev/null > {log} 2>&1 &\nVN=$!\ntrap 'kill -TERM \"$VN\" 2>/dev/null; nest_end' EXIT\nsleep 2\nkill -0 \"$VN\" 2>/dev/null || {{ echo \"remote run-apk: wayvnc exited at once (its log comes back with the run)\" >&2; {logs}; exit 5; }}\n"
    )
}

/// The `--view` check ([`Target::check_view`]), read only: `wayvnc` when it is on the target's PATH, else `missing`.
fn view_probe_script() -> String {
    "if command -v wayvnc >/dev/null 2>&1; then echo wayvnc; else echo missing; fi\n".to_owned()
}

/// Reads [`view_probe_script`]'s line: refuses by name when wayvnc is missing.
fn parse_view_probe(out: &str) -> Result<()> {
    match out.lines().next().unwrap_or_default().trim() {
        "wayvnc" => Ok(()),
        "missing" => bail!(
            "remote run-apk --view: wayvnc is not on PATH on the target (the run installs nothing there)"
        ),
        other => bail!("view probe: unexpected answer {other:?}"),
    }
}

/// The command that sends `paths` (relative to the out dir) back as the run's tar, on stdout: once per run, at its end or where it stops before atld starts (without an [`ATLD_RC`]: [`ended_early`]).
fn tar_sh(paths: &[String]) -> String {
    let paths: Vec<String> = paths.iter().map(|p| sh_quote(p)).collect();
    format!(
        "tar -c -C \"$BASE/out\" --ignore-failed-read {}",
        paths.join(" ")
    )
}

/// [`nested_wayland::labwc_env`] as the words of a run's labwc command (` NAME=value`): one output. No renderer: wlroots picks its own (pixman, the local gates', has no dmabuf, and the app's EGL then fails on rvvm, 2026-11-23).
fn labwc_words() -> String {
    nested_wayland::labwc_env([nested_wayland::GATE_OUTPUT].len())
        .iter()
        .map(|(k, v)| format!(" {k}={v}"))
        .collect()
}

/// The run's labwc, rung by rung: a name and the words added to [`labwc_words`]. The first is wlroots as it is.
/// no-modifiers: an EGL that lists every dmabuf modifier as external-only (PowerVR BXM on k3, measured 2026-11-25) leaves wlroots 0.19's GLES renderer no format to render to (egl.c `init_dmabuf_formats`), and no output can be set up; without modifiers it renders to implicit and linear buffers.
const LABWC_RUNGS: [(&str, &str); 2] =
    [("default", ""), ("no-modifiers", " WLR_EGL_NO_MODIFIERS=1")];

/// The host roots a runtime names, absolute on this host: the build dir, the java out and the AOSP tree (bundle names [`ROOT_NAMES`]), and the run's LLVM runtimes dir ([`llvm_lib_dir`], bundle name `lib`).
pub struct Roots {
    out: PathBuf,
    java: PathBuf,
    tree: PathBuf,
    lib: PathBuf,
}

impl Roots {
    pub fn of(out: &Path, java: &Path, tree: &Path, lib: &Path) -> Roots {
        Roots {
            out: out.to_path_buf(),
            java: java.to_path_buf(),
            tree: tree.to_path_buf(),
            lib: lib.to_path_buf(),
        }
    }

    /// Each root with its bundle name. The fourth is the bundle's `lib/`: the runtimes it ships stand for the run's LLVM runtimes dir.
    fn named(&self) -> [(&Path, &'static str); 4] {
        let [out, java, tree] = ROOT_NAMES.map(|(key, name)| (self.path(key), name));
        [out, java, tree, (self.lib.as_path(), "lib")]
    }

    fn path(&self, key: &str) -> &Path {
        match key {
            "out" => &self.out,
            "java" => &self.java,
            "tree" => &self.tree,
            other => unreachable!("record root `{other}`"),
        }
    }
}

/// Stands for the bundle's directory on the target: the run script expands it to `$BASE`.
const BASE_MARK: &str = "@BASE@";
/// `timeout -k`: KILL this many seconds after the limit's TERM.
const KILL_GRACE: u64 = 10;
/// How often the host's control-pipe keeper checks whether the transfer has ended ([`RemoteRun::spawn`]); only bounds how long the pipe outlives a finished run, not the run.
const CONTROL_POLL: Duration = Duration::from_secs(1);
/// The run script's name in the run's scratch dir.
const REMOTE_SCRIPT: &str = "remote-run.sh";

/// `text` with each host root that is a whole path token replaced by `@BASE@/<bundle name>`: the root is not preceded or followed by a path character (`[A-Za-z0-9._+/-]`, so `=`, `:`, quotes, `>` and spaces all bound it). The target then opens the bundle's copy, never this host's file.
pub fn relocate(text: &str, roots: &Roots) -> String {
    roots
        .named()
        .into_iter()
        .fold(text.to_owned(), |text, (root, name)| {
            replace_tokens(
                &text,
                &root.display().to_string(),
                &format!("{BASE_MARK}/{name}"),
            )
        })
}

/// `text` with each occurrence of `from` that is a whole token (see [`relocate`]) replaced by `to`.
fn replace_tokens(text: &str, from: &str, to: &str) -> String {
    let mut out = String::new();
    let mut last = 0;
    for (at, _) in text.match_indices(from) {
        let end = at + from.len();
        let before = text[..at].chars().next_back();
        let after = text[end..].chars().next();
        if at >= last && starts_token(before) && ends_token(after) {
            out.push_str(&text[last..at]);
            out.push_str(to);
            last = end;
        }
    }
    out.push_str(&text[last..]);
    out
}

/// A character a path can continue with: the root's last name must end before it.
fn path_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '+' | '-' | '/')
}

fn starts_token(before: Option<char>) -> bool {
    before.is_none_or(|c| !path_char(c))
}

/// A `/` after the root is the path going on below it, so it bounds nothing but is kept: only a name character continues the root's last component.
fn ends_token(after: Option<char>) -> bool {
    after.is_none_or(|c| c == '/' || !path_char(c))
}

/// `text` as one double-quoted shell word, `@BASE@` as `$BASE`.
fn word(text: &str) -> String {
    let parts: Vec<String> = text.split(BASE_MARK).map(dq).collect();
    format!("\"{}\"", parts.join("$BASE"))
}

/// `s` escaped for a double-quoted shell word.
fn dq(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('$', "\\$")
        .replace('`', "\\`")
}

/// The target's Wayland socket for a session run: the one `wayland-*` socket that is not a lock file, in the desktop session's `$XDG_RUNTIME_DIR` (`/run/user/<uid>`). Sets `WL` to its name. Any other count stops the run with exit 5 before atld starts, and names the count: a second compositor's socket must not be picked by chance.
pub(crate) fn session_script(tag: &str) -> String {
    let wl = wl_scan(SESSION_DIR);
    format!(
        "{wl}[ \"$WL_N\" -eq 1 ] || {{ echo \"remote run {tag}: $WL_N Wayland sockets in $WL_DIR, want 1 (the desktop session's)\" >&2; exit 5; }}\n"
    )
}

/// One atld run as the target runs it: the command, its environment, and the files this host wrote for it, all with bundle paths (`@BASE@`). `root` and `scratch` are relative to the out dir, where the logs return.
struct Plan {
    /// The run's name, its dir under the out dir, e.g. `gl/run`.
    tag: String,
    root: String,
    scratch: String,
    env: Vec<(String, String)>,
    /// Variables whose value is the target's desktop session's ([`session_env_script`]), set only when the session sets them.
    session_env: Vec<String>,
    program: String,
    args: Vec<String>,
    /// (bundle path, content): the package and permission files, the fontconfig copy.
    files: Vec<(String, String)>,
    lib_dirs: Vec<String>,
    limit: u64,
    /// Where `WAYLAND_DISPLAY` comes from ([`Display`]).
    display: Display,
    /// The window gate's agent ([`Agent`]), bundle paths.
    agent: Option<Agent>,
    /// SHA-256 of the [`MANIFEST`] this gate staged: the run refuses a dir whose manifest differs ([`stale_sh`]).
    manifest_sha256: String,
    audio: Option<RunAudio>,
    /// `--view`'s port ([`RemoteRun::view`]): only with [`Display::NestedHeadless`].
    view: Option<u16>,
    /// [`RemoteRun::dmabuf_probe`]: only with [`Display::NestedHeadless`] (its helper is the probe).
    dmabuf_probe: bool,
}

fn out_path(rel: &str) -> String {
    format!("{BASE_MARK}/out/{rel}")
}

/// A nested labwc's config dir under a run's scratch dir (`scratch` relative to the out dir): read by [`nested_script`] and [`nested_headless_script`], written by [`display_files`].
fn labwc_dir(scratch: &str) -> String {
    format!("{scratch}/labwc")
}

/// The run files a display needs before its script starts labwc: the test run's own labwc reads the local gate's config text ([`nested_wayland::RC_XML`]) from its config dir, sent here like the other run files.
fn display_files(display: Display, scratch: &str) -> Vec<(String, String)> {
    match display {
        Display::NestedHeadless => vec![(
            out_path(&format!("{}/rc.xml", labwc_dir(scratch))),
            nested_wayland::RC_XML.to_owned(),
        )],
        Display::Headless | Display::Session | Display::NestedX11 => Vec::new(),
    }
}

/// `cmd`'s explicit variables for the target, paths relocated: the target starts atld under `env -i`, so a variable `cmd` removes is simply absent there and is skipped.
fn command_env(cmd: &Command, roots: &Roots) -> Result<Vec<(String, String)>> {
    cmd.get_envs()
        .filter_map(|(key, value)| value.map(|v| (key, v)))
        .map(|(key, value)| Ok((utf8(key)?, relocate(&utf8(value)?, roots))))
        .collect()
}

/// Of `session_env`, the variables `env` does not set: an explicit value (`--env`) wins over the target session's.
fn from_session(session_env: &[&str], env: &[(String, String)]) -> Vec<String> {
    session_env
        .iter()
        .filter(|&&n| !env.iter().any(|(k, _)| k == n))
        .map(|&n| n.to_owned())
        .collect()
}

/// The shell variable holding the target session's value of `name`.
fn session_var(name: &str) -> String {
    format!("ATL_SESSION_{name}")
}

/// Reads `name` from the target user's session manager (`systemctl --user show-environment`: what an app the desktop starts inherits; the ssh shell's own env is not the desktop's) into
/// [`session_var`]; empty when the session does not set it or has no systemd user manager.
fn session_env_script(name: &str) -> String {
    format!(
        "{}=$(systemctl --user show-environment 2>/dev/null | sed -n 's/^{name}=//p' | head -n 1)\n",
        session_var(name)
    )
}

impl Plan {
    /// Whether atld maps a window on the target's session display: every session run but an agent's without a proxy (the window gate's frames stage: no `--ui`, the socket only for the
    /// renderer probe). Those runs go through [`workspace_script`] (D1).
    fn maps_window(&self) -> bool {
        self.display == Display::Session && self.agent.as_ref().is_none_or(|a| a.proxy)
    }

    /// The script the target runs on stdin (`sh -s`): the run's directories made fresh and its files written, then atld under `env -i` with `timeout` and stdin closed, in the background so its process group can be ended after it; then its logs and end record (`atld.rc`: the exit code, `end.ms`: the end's time on the target's clock) as a tar on stdout. The script's exit is the tar's. A run that stops before atld starts sends only its display's logs and no end record.
    /// Control channel: the whole script is one `{ }` group, so the shell has read all of it before running any; what stays on stdin is the host's open control pipe (`spawn`). A watchdog reads it (fd 7):
    /// EOF while atld runs = the connection or the host is gone, and it TERMs atld, so the run ends the normal way (atld's orderly stop, the group kill, the sink holder) within atld's stop grace.
    fn script(&self, base: &str) -> String {
        let mut s = format!(
            "# xtask remote run {}: written by remote.rs, do not edit\n{{\ncd {} || exit 2\nset -u\nBASE=$(pwd)\nexec 7<&0\n",
            self.tag,
            sh_quote(base)
        );
        s.push_str(&lock_sh(&format!("remote run {}", self.tag)));
        s.push_str(&stale_sh(&self.tag, &self.manifest_sha256));
        // the root and its data root (atld_run `fresh_data_root`, the only one a remote run takes): every run starts from a clean install, nothing left from the previous run on the target
        let data = crate::atld_run::fresh_data_root(Path::new(&self.root));
        s.push_str(&format!(
            "rm -rf {} {}\n",
            word(&out_path(&self.root)),
            word(&out_path(&data.to_string_lossy()))
        ));
        // the scratch dir and the dirs of the run files (a nested labwc's config dir is one of them), made before any file is written
        let mut dirs = vec![out_path(&self.scratch)];
        for (path, _) in &self.files {
            if let Some((dir, _)) = path.rsplit_once('/')
                && !dirs.iter().any(|d| d == dir)
            {
                dirs.push(dir.to_owned());
            }
        }
        let dirs: Vec<String> = dirs.iter().map(|d| word(d)).collect();
        s.push_str(&format!("mkdir -p {}\n", dirs.join(" ")));
        for (path, text) in &self.files {
            s.push_str(&format!(
                "sed \"s|{BASE_MARK}|$BASE|g\" > {} <<'ATL_EOF'\n{text}",
                word(path)
            ));
            if !text.ends_with('\n') {
                s.push('\n');
            }
            s.push_str("ATL_EOF\n");
        }
        let shown = self.program.replace(&format!("{BASE_MARK}/"), "");
        s.push_str(&format!(
            "[ -x {} ] || {{ echo \"remote run {}: {} is not an executable\" >&2; exit 4; }}\n",
            word(&self.program),
            self.tag,
            dq(&shown)
        ));
        // the logs of the display's processes started so far: they come back wherever the run stops
        let mut display_logs = Vec::new();
        if self.display.nested() {
            display_logs.push(format!("{}/{LABWC_LOG}", self.scratch));
        }
        let labwc_log = word(&out_path(&format!("{}/{LABWC_LOG}", self.scratch)));
        match self.display {
            Display::Headless => {}
            Display::Session => {
                s.push_str(&session_script(&self.tag));
                if self.maps_window() {
                    s.push_str(&workspace_script(&self.tag));
                }
            }
            Display::NestedX11 => s.push_str(&nested_script(
                &self.tag,
                &word(&out_path(&labwc_dir(&self.scratch))),
                &labwc_log,
                &tar_sh(&display_logs),
            )),
            Display::NestedHeadless => {
                display_logs.push(format!("{}/{LABWC_SETUP_LOG}", self.scratch));
                // atld's sibling: both come from one release dir ([`RUN_PROGRAMS`]), a cross host's under its triple
                let dir = self.program.rsplit_once('/').map_or("", |(d, _)| d);
                let setup = word(&format!("{dir}/{LABWC_SETUP}"));
                let setup_log = word(&out_path(&format!("{}/{LABWC_SETUP_LOG}", self.scratch)));
                s.push_str(&nested_headless_script(
                    &self.tag,
                    SESSION_DIR,
                    &word(&out_path(&labwc_dir(&self.scratch))),
                    (&labwc_log, &setup_log),
                    &setup,
                    &tar_sh(&display_logs),
                ));
                if self.dmabuf_probe {
                    let limit = format!("{}/{HOST_LIMIT_FILE}", self.scratch);
                    let mut logs = display_logs.clone();
                    logs.push(limit.clone());
                    s.push_str(&dmabuf_probe_sh(
                        &self.tag,
                        &setup,
                        &setup_log,
                        &word(&out_path(&limit)),
                        &tar_sh(&logs),
                    ));
                }
                if let Some(port) = self.view {
                    // tar --ignore-failed-read: the log may not exist yet on a failure before wayvnc starts
                    let log = format!("{}/{VIEW_LOG}", self.scratch);
                    display_logs.push(log.clone());
                    s.push_str(&view_script(
                        port,
                        &word(&out_path(&log)),
                        &tar_sh(&display_logs),
                    ));
                }
            }
        }
        for name in &self.session_env {
            s.push_str(&session_env_script(name));
        }
        let pw_dir = word(&out_path(&format!("{}/{PW_SAMPLES}", self.scratch)));
        if let Some(audio) = &self.audio {
            s.push_str(&format!(
                "rm -rf {pw_dir}
mkdir -p {pw_dir}
"
            ));
            if let Some(sink) = &audio.sink {
                // the holder outlives atld's timeout by the kill grace, then goes with the script or its own timeout
                s.push_str(&crate::audio_pcm::holder_sh(
                    sink,
                    &word(&out_path(&self.scratch)),
                    self.limit + 2 * KILL_GRACE,
                ));
            }
        }
        let lib_path: Vec<String> = self
            .lib_dirs
            .iter()
            .map(|d| format!("{BASE_MARK}/{d}"))
            .collect();
        let scratch_file = |name: &str| word(&out_path(&format!("{}/{name}", self.scratch)));
        // the agent first: its proxy socket must exist before atld connects to it
        let proxied = self.agent.as_ref().is_some_and(|a| a.proxy);
        if let Some(agent) = &self.agent {
            s.push_str("GATE_SOCK=\"/run/user/$(id -u)/atl-gate-$$.sock\"\n");
            let args: Vec<String> = agent.args.iter().map(|a| word(a)).collect();
            let window: Vec<String> = agent.window.iter().map(|a| word(a)).collect();
            let sockets = if proxied {
                " --upstream \"$WL_DIR/$WL\" --listen \"$GATE_SOCK\""
            } else {
                ""
            };
            s.push_str(&format!(
                "env -i PATH=/usr/local/bin:/usr/bin:/bin HOME=\"$HOME\" XDG_RUNTIME_DIR=\"/run/user/$(id -u)\" LD_LIBRARY_PATH={} timeout -k {KILL_GRACE} {} {} {}{sockets} -- {} < /dev/null > {} 2>&1 &\nAG=$!\n",
                word(&lib_path.join(":")),
                self.limit + AGENT_MARGIN_S,
                word(&agent.program.display().to_string()),
                args.join(" "),
                window.join(" "),
                scratch_file(AGENT_LOG)
            ));
            if proxied {
                let mut logs = display_logs.clone();
                logs.push(format!("{}/{AGENT_LOG}", self.scratch));
                s.push_str(&format!(
                    "i=0\nwhile [ ! -S \"$GATE_SOCK\" ] && [ \"$i\" -lt {AGENT_WAIT_S} ]; do\n  kill -0 \"$AG\" 2>/dev/null || break\n  sleep 1\n  i=$((i + 1))\ndone\n[ -S \"$GATE_SOCK\" ] || {{ echo \"remote run {}: the window agent made no proxy socket within {AGENT_WAIT_S} s (see {AGENT_LOG})\" >&2; kill \"$AG\" 2>/dev/null; {}; exit 5; }}\n",
                    self.tag,
                    tar_sh(&logs)
                ));
            }
        }
        s.push_str(&format!(
            "env -i PATH=/usr/local/bin:/usr/bin:/bin HOME=\"$HOME\" XDG_RUNTIME_DIR=\"/run/user/$(id -u)\" LD_LIBRARY_PATH={}",
            word(&lib_path.join(":"))
        ));
        // `${V:+"N=$V"}`: one word when the session sets it, nothing when it does not (env -i then leaves it unset, libpulse's default)
        for name in &self.session_env {
            s.push_str(&format!(
                " ${{{v}:+\"{name}=${v}\"}}",
                v = session_var(name)
            ));
        }
        if proxied {
            s.push_str(" WAYLAND_DISPLAY=\"$GATE_SOCK\"");
        } else if self.display != Display::Headless {
            s.push_str(" WAYLAND_DISPLAY=\"$WL\"");
        }
        for (key, value) in &self.env {
            s.push_str(&format!(" {key}={}", word(value)));
        }
        let args: Vec<String> = self.args.iter().map(|a| word(a)).collect();
        // background: `$!` is setsid's pid, which execs timeout as its session's and process group's leader (an async list's child is no group leader without job control, so setsid does not fork);
        // the group is ended after the run, so an app that stayed in it leaves no window behind. `--foreground`: the limit TERMs atld alone, which stops its daemons in order; without it timeout
        // TERMs the whole group and the daemons die before atld's stop, read as "system process ... died during the run" (measured: a group child of plain timeout dies, of --foreground lives)
        s.push_str(&format!(
            " setsid timeout --foreground -k {KILL_GRACE} {} {} {} < /dev/null 7<&- > {} 2>&1 &\n",
            self.limit,
            word(&self.program),
            args.join(" "),
            word(&out_path(&format!("{}/atld.log", self.scratch)))
        ));
        s.push_str("pid=$!\n");
        // the control watchdog: timeout passes the TERM on to atld
        s.push_str(
            "( cat <&7 > /dev/null 2>&1; kill -TERM \"$pid\" 2>/dev/null ) > /dev/null 2>&1 &\nWD=$!\n",
        );
        // the samples end with atld; the last one may miss a stream's final second, not its route. atld = timeout's only child (`$pid` is timeout): its pid names the run's PipeWire
        // contexts (audio_pcm.rs RunClients), written to [`PW_ATLD_PID`] once it shows
        if self.audio.is_some() {
            let every = crate::audio_pcm::ROUTE_SAMPLE.as_secs();
            s.push_str(&format!(
                "( n=0; while kill -0 \"$pid\" 2>/dev/null; do [ -s {pw_dir}/{PW_ATLD_PID} ] || pgrep -P \"$pid\" > {pw_dir}/{PW_ATLD_PID}; XDG_RUNTIME_DIR=\"/run/user/$(id -u)\" pw-dump > {pw_dir}/$n.json 2>/dev/null; n=$((n + 1)); sleep {every}; done ) &\nPWS=$!\n"
            ));
        }
        if self.agent.is_some() {
            s.push_str(&format!(
                "printf '%s\\n' \"$pid\" > {}\n",
                scratch_file(ATLD_PID)
            ));
        }
        // the watchdog's cat stays until the host closes the pipe: it reads only, so the session still ends
        s.push_str("wait \"$pid\"\nrc=$?\nkill \"$WD\" 2>/dev/null || true\n");
        // the agent stopped atld (or saw it end); it ends after joining its probe, bounded by its own timeout
        if self.agent.is_some() {
            s.push_str(&format!(
                "wait \"$AG\"\nprintf '%s\\n' \"$?\" > {}\nrm -f \"$GATE_SOCK\"\n",
                scratch_file(AGENT_RC)
            ));
        }
        s.push_str("kill -KILL -- \"-$pid\" 2>/dev/null || true\n");
        if let Some(audio) = &self.audio {
            s.push_str("wait \"$PWS\" 2>/dev/null || true\n");
            if audio.sink.is_some() {
                s.push_str(crate::audio_pcm::holder_end_sh());
            }
        }
        // the view ends with the run: wayvnc goes before the labwc whose socket it serves
        if self.view.is_some() && self.display == Display::NestedHeadless {
            s.push_str(
                "kill -TERM \"$VN\" 2>/dev/null || true\nwait \"$VN\" 2>/dev/null || true\n",
            );
        }
        // the labwc setup helper ends first: its devices go with labwc's socket, and the run's labwc log keeps labwc's own end
        if self.display == Display::NestedHeadless {
            s.push_str(
                "kill -TERM \"$HS\" 2>/dev/null || true\nwait \"$HS\" 2>/dev/null || true\n",
            );
        }
        // the nested compositor lives for the run only: its window leaves the target's desktop with it
        if self.display.nested() {
            s.push_str(
                "kill -TERM \"$LW\" 2>/dev/null || true\nwait \"$LW\" 2>/dev/null || true\n",
            );
        }
        s.push_str(&format!(
            "printf '%s\\n' \"$rc\" > {}\n",
            word(&out_path(&format!("{}/{ATLD_RC}", self.scratch)))
        ));
        s.push_str(&format!(
            "date +%s%3N > {}\n",
            word(&out_path(&format!("{}/end.ms", self.scratch)))
        ));
        s.push_str(&format!(
            "echo \"atld rc=$rc, remote run {}\" >&2\n",
            self.tag
        ));
        // the target's answers, for the rows judged against the run host (app.rs HostFacts); nproc = this shell's affinity, which atld inherited; the zone with atld's TZ (env -i above: only its own env row)
        let tz = self
            .env
            .iter()
            .find(|(k, _)| k == "TZ")
            .map_or_else(String::new, |(_, v)| format!(" TZ={}", word(v)));
        s.push_str(&format!(
            "{{ cat /sys/devices/system/cpu/possible /sys/devices/system/cpu/online; nproc; env -i PATH=/usr/bin:/bin{tz} sh -c {}; }} > {}\ncat /proc/cpuinfo > {}\n",
            sh_quote(ZONE_SH),
            word(&out_path(&format!("{}/{HOST_FACTS}", self.scratch))),
            word(&out_path(&format!("{}/{CPU_INFO}", self.scratch)))
        ));
        let mut logs = vec![format!("{}/log", self.root)];
        logs.extend(
            ["atld.log", ATLD_RC, "end.ms", HOST_FACTS, CPU_INFO]
                .map(|f| format!("{}/{f}", self.scratch)),
        );
        // labwc's log and the agent's files ride along
        logs.extend(display_logs);
        if self.audio.is_some() {
            logs.push(format!("{}/{PW_SAMPLES}", self.scratch));
        }
        if self.agent.is_some() {
            logs.extend([LIVE_JSON, AGENT_RC, AGENT_LOG].map(|f| format!("{}/{f}", self.scratch)));
        }
        s.push_str(&tar_sh(&logs));
        s.push('\n');
        // exit inside the group: after it the shell would read stdin for more, and the control pipe stays open until the transfer ends
        s.push_str("exit\n}\n");
        s
    }
}

impl RemoteRun {
    /// Whether the run has a Wayland display ([`Display`]): a UI run needs one.
    pub fn has_display(&self) -> bool {
        self.display != Display::Headless
    }

    /// Starts `cmd` (atld's command, host paths) on the target as one run. The child is the transfer, `<transport> | tar -x` (`sh -s` for `local:`) with the plan on its stdin, and its end is the run's; stdin stays open as the run's control pipe ([`keep_control`]). `root` and `scratch` lie under the out dir, where the logs return. `written` are the files the command reads that this host wrote: their content goes into the plan.
    /// `session_env`: variables atld takes from the target's desktop session ([`session_env_script`]), never from this host.
    pub fn spawn(
        &self,
        roots: &Roots,
        cmd: &Command,
        root: &Path,
        scratch: &Path,
        written: &[PathBuf],
        session_env: &[&str],
    ) -> Result<Child> {
        ensure!(
            cmd.get_current_dir().is_none(),
            "remote run: a working directory is not relocated"
        );
        let root_rel = rel_out(&roots.out, root)?;
        let scratch_rel = rel_out(&roots.out, scratch)?;
        let tag = root_rel
            .strip_suffix("/root")
            .unwrap_or(&root_rel)
            .to_owned();
        let env = command_env(cmd, roots)?;
        let session_env = from_session(session_env, &env);
        let args = cmd
            .get_args()
            .map(|a| Ok(relocate(&utf8(a)?, roots)))
            .collect::<Result<Vec<_>>>()?;
        let mut files = display_files(self.display, &scratch_rel);
        for path in written {
            let body = std::fs::read_to_string(path)
                .with_context(|| format!("reading {}", path.display()))?;
            files.push((
                relocate(&utf8(path.as_os_str())?, roots),
                relocate(&body, roots),
            ));
        }
        let plan = Plan {
            tag: tag.clone(),
            root: root_rel,
            scratch: scratch_rel,
            env,
            session_env,
            program: relocate(&utf8(cmd.get_program())?, roots),
            args,
            files,
            lib_dirs: self.lib_dirs.clone(),
            limit: self.limit.as_secs(),
            display: self.display,
            view: self.view,
            agent: self
                .agent
                .as_ref()
                .map(|a| -> Result<Agent> {
                    let rel = |s: &str| relocate(s, roots);
                    Ok(Agent {
                        program: PathBuf::from(rel(&utf8(a.program.as_os_str())?)),
                        args: a.args.iter().map(|s| rel(s)).collect(),
                        window: a.window.iter().map(|s| rel(s)).collect(),
                        proxy: a.proxy,
                    })
                })
                .transpose()?,
            manifest_sha256: self.manifest_sha256.clone(),
            audio: self.audio.clone(),
            dmabuf_probe: self.dmabuf_probe,
        };
        // the previous run's end record and host limit must not stand for this one's
        for name in [ATLD_RC, HOST_LIMIT_FILE] {
            let end = scratch.join(name);
            if end.exists() {
                std::fs::remove_file(&end)
                    .with_context(|| format!("removing {}", end.display()))?;
            }
        }
        let script = scratch.join(REMOTE_SCRIPT);
        let body = plan.script(&self.base);
        std::fs::write(&script, &body).with_context(|| format!("writing {}", script.display()))?;
        let transport = match &self.host {
            Host::Ssh(alias) => format!("ssh {} {} sh -s", ssh_opts_sh(), sh_quote(alias)),
            Host::Local(_) => "sh -s".to_owned(),
        };
        let transfer = format!(
            "{transport} | tar -x -C {}",
            sh_quote(&roots.out.display().to_string())
        );
        let mut log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log)
            .with_context(|| format!("opening {}", self.log.display()))?;
        writeln!(log, "== run {tag} on {}", self.host.label())?;
        let (control, keep) = std::io::pipe().context("making the run's control pipe")?;
        let child = Command::new("sh")
            .args(["-c", &transfer])
            .stdin(control)
            .stdout(Stdio::null())
            .stderr(log.try_clone()?)
            .spawn()
            .context("starting the transfer to the target")?;
        keep_control(keep, body.into_bytes(), child.id());
        Ok(child)
    }

    /// The error for a run whose transfer ended `end`. The cause is the transcript's lines after the run's header (the script's own message comes first, then the transfer's), up to three.
    pub fn failed(&self, root: &Path, end: ExitStatus) -> anyhow::Error {
        // a leading newline: the first run's header starts the file
        let text = format!(
            "\n{}",
            std::fs::read_to_string(&self.log).unwrap_or_default()
        );
        let after_header = text
            .rsplit_once("\n== run ")
            .map(|(_, run)| run.lines().skip(1).collect::<Vec<_>>())
            .unwrap_or_default();
        let cause: Vec<&str> = after_header
            .into_iter()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .take(3)
            .collect();
        anyhow!(
            "remote run {} ended {end}: {} (transcript {})",
            root.display(),
            if cause.is_empty() {
                "no output".to_owned()
            } else {
                cause.join(" | ")
            },
            self.log.display()
        )
    }
}

/// Whether a run's transfer ended by itself (an exit code) without the run: it failed, or it brought back no end record, so the script stopped before atld's end and sent only the logs it had.
pub(crate) fn ended_early(end: ExitStatus, scratch: &Path) -> bool {
    end.code()
        .is_some_and(|c| c != 0 || !scratch.join(ATLD_RC).is_file())
}

/// `path` relative to the out dir: the target keeps that layout. A path outside the out dir is refused.
pub(crate) fn rel_out(out: &Path, path: &Path) -> Result<String> {
    let rel = path.strip_prefix(out).with_context(|| {
        format!(
            "{} is not under the out dir {}: a remote run keeps its files there",
            path.display(),
            out.display()
        )
    })?;
    let text = rel
        .to_str()
        .with_context(|| format!("{} is not UTF-8", rel.display()))?;
    ensure!(
        safe_name(text),
        "{text}: the target path takes only [A-Za-z0-9._+/-]"
    );
    Ok(text.to_owned())
}

fn utf8(s: &std::ffi::OsStr) -> Result<String> {
    s.to_str()
        .map(str::to_owned)
        .with_context(|| format!("{s:?} is not UTF-8"))
}

/// The host a bundle goes to.
#[derive(Clone)]
enum Host {
    /// An ssh alias (WSL `~/.ssh/config`).
    Ssh(String),
    /// A directory standing in for the target.
    Local(PathBuf),
}

impl Host {
    fn parse(text: &str) -> Result<Host> {
        if let Some(dir) = text.strip_prefix("local:") {
            let dir = PathBuf::from(dir);
            ensure!(
                dir.is_absolute(),
                "`local:` needs an absolute directory, got {}",
                dir.display()
            );
            return Ok(Host::Local(dir));
        }
        ensure!(
            valid_alias(text),
            "host `{text}` is not an ssh alias: a WSL ~/.ssh/config `Host` name (letters, digits, `.`, `_`, `-`; no user@address), or local:<absolute dir>"
        );
        Ok(Host::Ssh(text.to_owned()))
    }

    fn label(&self) -> String {
        match self {
            Host::Ssh(alias) => alias.clone(),
            Host::Local(dir) => format!("local:{}", dir.display()),
        }
    }

    /// The host's transcript directory under `out/remote/`: its alias, or `local` for a `local:` directory.
    fn log_dir(&self) -> String {
        match self {
            Host::Ssh(alias) => alias.clone(),
            Host::Local(_) => "local".to_owned(),
        }
    }

    /// Where the bundle sits on the target: `~/atl-next-run/<name>` (relative to the home dir), or the local dir.
    fn base(&self, dir_name: &str) -> String {
        match self {
            Host::Ssh(_) => format!("{REMOTE_ROOT}/{dir_name}"),
            Host::Local(dir) => dir.display().to_string(),
        }
    }

    fn target(&self, dir_name: &str) -> String {
        match self {
            Host::Ssh(alias) => format!("{alias}:{REMOTE_ROOT}/{dir_name}/"),
            Host::Local(dir) => format!("{}/", dir.display()),
        }
    }

    /// Fails with the alias named unless the host answers: the first contact, before any copy.
    fn reach(&self) -> Result<()> {
        let Host::Ssh(alias) = self else {
            return Ok(());
        };
        let out = ssh(alias).arg("true").output().context("running ssh")?;
        if !out.status.success() {
            bail!(
                "ssh alias `{alias}` does not connect ({}): add a `Host {alias}` block to WSL ~/.ssh/config (maintainer; the repo holds no address)",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(())
    }

    /// Copies the staged tree to the target with rsync's [`RSYNC`] flags, the receiving rsync under the dir's [`RUN_LOCK`] ([`push_lock_sh`]): a push never deletes files under a running gate
    /// of the same checkout; it fails `busy` as a run does.
    fn sync(&self, stage: &Path, dir_name: &str) -> Result<()> {
        let src = format!("{}/", stage.display());
        let out = match self {
            Host::Ssh(alias) => Command::new("rsync")
                .args(RSYNC)
                .args(["-e", &format!("ssh {}", ssh_opts_sh())])
                .arg(format!(
                    "--rsync-path={}",
                    push_lock_sh(&format!("{REMOTE_ROOT}/{dir_name}"))
                ))
                .arg(&src)
                .arg(format!("{alias}:{REMOTE_ROOT}/{dir_name}/"))
                .stderr(Stdio::piped())
                .output(),
            Host::Local(dir) => {
                // the same lock shell as the remote side, rsync's arguments after it
                let mut c = Command::new("sh");
                c.args([
                    "-c",
                    &format!("{} \"$@\"", push_lock_sh(&dir.display().to_string())),
                    "sh",
                ])
                .args(RSYNC)
                .arg(&src)
                .arg(format!("{}/", dir.display()))
                .stderr(Stdio::piped())
                .output()
            }
        }
        .context("running rsync")?;
        let err = String::from_utf8_lossy(&out.stderr);
        eprint!("{err}");
        ensure!(
            out.status.success(),
            "rsync to {} failed: {}: {}",
            self.label(),
            out.status,
            err.trim()
        );
        Ok(())
    }

    /// Runs the prerequisite script on the target (`sh -s`, the script on stdin); its stdout. Only a zero exit is a result: a dropped ssh leaves partial stdout, which must not read as a verdict. A script that cannot reach its directory exits 2 and fails here.
    fn run_script(&self, script: &str) -> Result<String> {
        let mut cmd = match self {
            Host::Ssh(alias) => {
                let mut c = ssh(alias);
                c.args(["sh", "-s"]);
                c
            }
            Host::Local(_) => {
                let mut c = Command::new("sh");
                c.arg("-s");
                c
            }
        };
        let child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("starting the prerequisite check on {}", self.label()))?;
        let out = feed_and_wait(child, script)?;
        ensure!(
            out.status.success(),
            "prerequisite check on {} did not finish ({}): {}",
            self.label(),
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// `script` on the target (`sh -s`), its stdout as bytes, under the host's `timeout` at `limit` (killed 5 s later if it ignores it). A non-zero status is an error with the script's stderr.
    fn run_bounded(&self, script: &str, limit: Duration) -> Result<Vec<u8>> {
        let secs = limit.as_secs().max(1).to_string();
        let mut cmd = Command::new("timeout");
        cmd.args(["-k", "5", secs.as_str()]);
        match self {
            Host::Ssh(alias) => {
                cmd.arg("ssh")
                    .args(ssh_opts())
                    .arg(alias)
                    .args(["sh", "-s"]);
            }
            Host::Local(_) => {
                cmd.args(["sh", "-s"]);
            }
        }
        let child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("starting a call to {}", self.label()))?;
        let out = feed_and_wait(child, script)?;
        ensure!(
            out.status.success(),
            "{} ({}): {}",
            self.label(),
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
        Ok(out.stdout)
    }
}

/// Sends `script` to `child`'s stdin and collects its output. The script is written from its own thread: `sh -s` runs it while
/// reading it, so its stdout can fill our pipe before the whole script is sent — writing first and reading afterwards deadlocks
/// (2026-11-09: a 376-ELF ldd check stalled ssh until its ServerAlive gave up with "server not responding").
fn feed_and_wait(mut child: Child, script: &str) -> Result<Output> {
    let mut stdin = child
        .stdin
        .take()
        .context("no stdin for the target's script")?;
    let body = script.as_bytes().to_vec();
    let writer = std::thread::spawn(move || stdin.write_all(&body));
    let out = child.wait_with_output()?;
    writer
        .join()
        .map_err(|_| anyhow!("the script writer panicked"))??;
    Ok(out)
}

/// Writes a run's `plan` into its transfer's stdin from a thread, then holds the pipe open until the transfer `pid` has ended (gone or a zombie): the target's watchdog reads EOF only when the
/// connection drops or this process dies ([`Plan::script`]). Held here, not in the `Child`: `Child::wait` closes the child's stdin first, which would end a run its caller merely waits for.
fn keep_control(mut keep: std::io::PipeWriter, plan: Vec<u8>, pid: u32) {
    std::thread::spawn(move || {
        // a write error = the transport ended before reading it all; the run's end carries the cause
        if keep.write_all(&plan).is_err() {
            return;
        }
        while process_live(pid) {
            std::thread::sleep(CONTROL_POLL);
        }
    });
}

/// Whether `pid` runs: `/proc/<pid>/stat` exists and its state (after the command's closing paren) is not `Z`.
fn process_live(pid: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
        stat.rsplit_once(')')
            .is_some_and(|(_, rest)| !rest.trim_start().starts_with('Z'))
    })
}

fn ssh(alias: &str) -> Command {
    let mut c = Command::new("ssh");
    c.args(ssh_opts()).arg(alias);
    c
}

/// A gate's bundle on one host: what `push`, the prerequisite check and a remote run share.
pub struct Target {
    host: Host,
    dir_name: String,
    bundle: Bundle,
    /// Bundle names of the ELF files, and of the directories holding them: the check's search path, and a run's `LD_LIBRARY_PATH`.
    elf: Vec<String>,
    lib_dirs: Vec<String>,
    stage: PathBuf,
    /// Whether its runs ask their labwc for a client dmabuf import first ([`dmabuf_probe_sh`]); off under [`Break::TrustDmabuf`].
    dmabuf_probe: bool,
}

impl Target {
    /// The bundle of gate `gate`'s record in `out`, with `brk` applied, for the host `host` (an ssh alias or `local:<dir>`). Its ELF files and library dirs are read here.
    pub fn of(
        root: &Path,
        out: &Path,
        gate: &str,
        host: &str,
        brk: Option<Break>,
    ) -> Result<Target> {
        let host = Host::parse(host)?;
        let cross = xtask::native::cross_host_of(root, out)?;
        let dir_name = target_dir_name(out, cross.is_some())?;
        let bundle = Bundle::of(gate, out, cross.as_deref(), brk)?;
        let elf = bundle.elf_names()?;
        let lib_dirs = bundle.lib_dirs(&elf);
        Ok(Target {
            host,
            dir_name,
            bundle,
            elf,
            lib_dirs,
            stage: stage_dir(out, gate, brk),
            dmabuf_probe: brk != Some(Break::TrustDmabuf),
        })
    }

    pub fn summary(&self) -> String {
        self.bundle.summary()
    }

    pub fn entries(&self) -> &[Entry] {
        &self.bundle.entries
    }

    /// `alias:~/atl-next-run/<name>/` (or the local dir): where the bundle goes.
    pub fn target(&self) -> String {
        self.host.target(&self.dir_name)
    }

    pub fn label(&self) -> String {
        self.host.label()
    }

    /// The host's transcript dir under `out/remote/`.
    pub fn log_dir(&self) -> String {
        self.host.log_dir()
    }

    pub fn elf_count(&self) -> usize {
        self.elf.len()
    }

    pub fn dir_count(&self) -> usize {
        self.lib_dirs.len()
    }

    /// Stages the bundle and copies it to the target (`rsync -a --delete`); returns what it staged. Fails first when the host does not answer.
    /// Not closed by the marker: a non-manifest file rebuilt at the same path and size within one second of the last push (stage hard links keep build mtimes) passes rsync's quick check and
    /// stays the old bytes on the target under the new marker; only `--checksum` (hashes 164-418 MiB per push on both sides) or a target-side `sha256sum -c` of the manifest closes it.
    pub fn push(&self) -> Result<Pushed> {
        self.host.reach()?;
        let pushed = self.bundle.stage(&self.stage)?;
        self.host.sync(&self.stage, &self.dir_name)?;
        Ok(pushed)
    }

    /// What the last push of this gate and break staged (`remote push` in another process; the stage dir is per gate and break).
    pub fn staged(&self) -> Result<Pushed> {
        let manifest = self.stage.join(MANIFEST);
        let bytes = std::fs::read(&manifest)
            .with_context(|| format!("reading the staged {} (push first)", manifest.display()))?;
        Ok(Pushed(sha256_hex(&bytes)))
    }

    /// [`Target::push`], then [`Target::check`], its lines printed as `remote prereq <what>: ...`. The check's problems: empty = delivered and checked.
    pub fn deliver(&self, what: &str) -> Result<(Pushed, Vec<String>)> {
        let pushed = self.push()?;
        let prereq = self.check()?;
        let problems = prereq.problems();
        for p in &problems {
            println!("remote prereq {what}: {p}");
        }
        if problems.is_empty() {
            println!(
                "remote prereq {what}: PASS, {} ELF checked on {}",
                prereq.checked,
                self.label()
            );
        }
        Ok((pushed, problems))
    }

    /// The prerequisite check on the target: `ldd` over every ELF file of the bundle.
    pub fn check(&self) -> Result<Prereq> {
        let script = prereq_script(&self.host.base(&self.dir_name), &self.elf, &self.lib_dirs)?;
        Ok(parse_prereq(&self.host.run_script(&script)?))
    }

    /// A run's site on the target: atld runs from the bundle there, ends at `limit`, and its transcript is appended to `log`. `display`: where its `WAYLAND_DISPLAY` comes from.
    /// The run executes only if the target dir still holds `pushed`, the bundle the caller's own push staged.
    pub fn run_site(
        &self,
        pushed: &Pushed,
        limit: Duration,
        log: PathBuf,
        display: Display,
    ) -> Result<Site> {
        Ok(Site::Remote(RemoteRun {
            manifest_sha256: pushed.0.clone(),
            host: self.host.clone(),
            base: self.host.base(&self.dir_name),
            lib_dirs: self.lib_dirs.clone(),
            limit,
            log,
            display,
            agent: None,
            audio: None,
            view: None,
            dmabuf_probe: self.dmabuf_probe,
        }))
    }

    /// atld's `--monitor` value from the target's own display (the harness's hidden `display-probe`, window.rs `display_probe`, under the session's `WAYLAND_DISPLAY`): what a remote UI stage
    /// tells atld and judges with. `harness` and the bundle's lib dirs are host paths, relocated. Only the session's socket: a nested labwc lives for one run only.
    pub fn display_model(&self, roots: &Roots, harness: &Path) -> Result<String> {
        let lib_path: Vec<String> = self
            .lib_dirs
            .iter()
            .map(|d| format!("{BASE_MARK}/{d}"))
            .collect();
        let script = format!(
            "cd {} || exit 2\nBASE=$(pwd)\n{}env -i PATH=/usr/local/bin:/usr/bin:/bin HOME=\"$HOME\" XDG_RUNTIME_DIR=\"$WL_DIR\" WAYLAND_DISPLAY=\"$WL\" LD_LIBRARY_PATH={} {} display-probe < /dev/null\n",
            sh_quote(&self.base()),
            session_script("display-probe"),
            word(&lib_path.join(":")),
            word(&relocate(&utf8(harness.as_os_str())?, roots))
        );
        let out = String::from_utf8(self.run_bounded(&script, DISPLAY_PROBE_LIMIT)?)
            .context("display-probe: output is not UTF-8")?;
        out.lines()
            .find_map(|l| l.strip_prefix(DISPLAY_PROBE_LINE))
            .map(str::to_owned)
            .with_context(|| format!("display-probe: no `{DISPLAY_PROBE_LINE}` line in {out:?}"))
    }

    /// The bundle dir on the target, where a run's `out/` lives: `~/atl-next-run/<name>` relative to home, or the local dir.
    pub fn base(&self) -> String {
        self.host.base(&self.dir_name)
    }

    /// `script` on the target, its stdout as bytes, bounded by `limit` (see `Host::run_bounded`).
    pub fn run_bounded(&self, script: &str, limit: Duration) -> Result<Vec<u8>> {
        self.host.run_bounded(script, limit)
    }

    /// Refuses `--view` unless wayvnc is on the target's PATH ([`view_probe_script`], read only).
    pub fn check_view(&self) -> Result<()> {
        let out = self.run_bounded(&view_probe_script(), DISPLAY_PROBE_LIMIT)?;
        parse_view_probe(&String::from_utf8_lossy(&out))
    }
}

/// An ssh alias: a `Host` name, never `user@address` or `host:path`. It names a transcript directory under out/remote/, so dot-only names (`.`, `..`) are refused.
fn valid_alias(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= 64
        && !text.starts_with('-')
        && !text.chars().all(|c| c == '.')
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

fn out_of(root: &Path, explicit: Option<&Path>) -> Result<PathBuf> {
    out::out_dir(root, explicit)
}

/// The target's directory name for out dir `out`: its build dir's name (the checkout id, `snap-x3`), from the real dir (inside the build namespace every build dir is `out/build`, out::real). A `cross` host's out is `<dir>/<arch>` (native::host_out), so it is `<dir>-<arch>`: named by the arch alone, two checkouts pushing one arch would share (and `rsync --delete`) one dir.
fn target_dir_name(out: &Path, cross: bool) -> Result<String> {
    let out = &out::real(out);
    let name = |p: &Path| {
        p.file_name()
            .and_then(|n| n.to_str())
            .map(str::to_owned)
            .with_context(|| format!("out dir {} has no name", p.display()))
    };
    let name = if cross {
        let build = out
            .parent()
            .with_context(|| format!("cross out dir {} has no build dir", out.display()))?;
        format!("{}-{}", name(build)?, name(out)?)
    } else {
        name(out)?
    };
    ensure!(
        valid_name(&name),
        "out dir name `{name}` has characters the target path does not take"
    );
    Ok(name)
}

/// One stage dir per gate and break: a break's bundle differs, and one stage per gate let a `--break` push refill the dir a plain push's run then hashed.
fn stage_dir(out: &Path, gate: &str, brk: Option<Break>) -> PathBuf {
    let name = match brk.and_then(|b| clap::ValueEnum::to_possible_value(&b)) {
        Some(b) => format!("{gate}@{}", b.get_name()),
        None => gate.to_owned(),
    };
    out.join(REMOTE_DIR).join("bundle").join(name)
}

/// The record file of gate `gate`: `<out>/remote/inputs/<gate>.txt`, written by the gate's last run.
pub fn record_path(out: &Path, gate: &str) -> PathBuf {
    out.join(REMOTE_DIR)
        .join("inputs")
        .join(format!("{gate}.txt"))
}

/// What a run read: host files, and host directories whose trees the bundle takes whole.
#[derive(Default)]
pub struct Inputs {
    pub files: Vec<PathBuf>,
    pub dirs: Vec<PathBuf>,
}

/// Writes gate `gate`'s record: its inputs (absolute), with the roots their bundle names come from (`java`, `tree`; the build dir is `out`) and the LLVM runtimes dir the run links against (`lib`, [`llvm_lib_dir`]).
pub fn write_record(
    out: &Path,
    gate: &str,
    java: &Path,
    tree: &Path,
    lib: &Path,
    inputs: &Inputs,
) -> Result<()> {
    let file = record_path(out, gate);
    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut text =
        format!("# runtime inputs of gate {gate}, written by its last run (xtask remote)\n");
    for (name, dir) in [
        ("out", out),
        ("java", java),
        ("tree", tree),
        (LIB_ROOT, lib),
    ] {
        text.push_str(&format!("{name} {}\n", std::path::absolute(dir)?.display()));
    }
    let mut lines = BTreeSet::new();
    for f in &inputs.files {
        lines.insert(std::path::absolute(f)?.display().to_string());
    }
    for d in &inputs.dirs {
        lines.insert(format!("dir {}", std::path::absolute(d)?.display()));
    }
    for line in lines {
        ensure!(!line.contains('\n'), "record path with a newline: {line}");
        text.push_str(&line);
        text.push('\n');
    }
    std::fs::write(&file, text).with_context(|| format!("writing {}", file.display()))
}

/// A gate's record: its roots and what its run read.
pub struct Record {
    roots: BTreeMap<&'static str, PathBuf>,
    /// The run's LLVM runtimes dir: the bundle's `lib/` comes from it.
    lib: PathBuf,
    files: Vec<PathBuf>,
    dirs: Vec<PathBuf>,
}

/// The record's root line naming the run's LLVM runtimes dir.
const LIB_ROOT: &str = "lib";

/// The command that writes gate `gate`'s record again, after building what it names for that host (atld_run.rs `Runtime::new`, gate_build.rs `ensure`). A cross host's gates do not run on this host: its record comes from the remote form, whose dry run writes it too.
fn record_again(gate: &str, cross: Option<&str>) -> String {
    let arch = cross.map(|c| format!(" --arch {c}")).unwrap_or_default();
    match (gate, cross) {
        ("run-apk", _) => format!("run `cargo xtask remote run-apk <target> --host <alias>{arch}`"),
        ("app", Some(_)) => format!(
            "run `cargo xtask remote test app --host <alias>{arch}` (with `--fixture-arch <isa>` where the arch has no identity engine)"
        ),
        (_, Some(c)) => format!("a {c} out dir records only `app` and `run-apk`"),
        (_, None) => format!("run `cargo xtask test {gate}`"),
    }
}

/// The bundle name of gate product `program` (`out/cargo/release/<program>`, a cross host's `out/cargo/<triple>/release/<program>`: xtask cdylib.rs `release_build`).
fn program_entry(map: &BTreeMap<String, Entry>, program: &str) -> Option<String> {
    let end = format!("/release/{program}");
    map.keys()
        .find(|n| n.starts_with("out/cargo/") && n.ends_with(&end))
        .cloned()
}

fn read_record(out: &Path, gate: &str, cross: Option<&str>) -> Result<Record> {
    let file = record_path(out, gate);
    let text = std::fs::read_to_string(&file).with_context(|| {
        format!(
            "gate `{gate}`: no runtime-input record at {}: {} (a run records what it reads)",
            file.display(),
            record_again(gate, cross)
        )
    })?;
    parse_record(&text)
}

/// The record text: `out` / `java` / `tree` root lines, `dir <path>` lines, then one file path per line; `#` lines are comments. Paths are absolute.
fn parse_record(text: &str) -> Result<Record> {
    let mut roots = BTreeMap::new();
    let mut lib = None;
    let mut files = Vec::new();
    let mut dirs = Vec::new();
    for line in text.lines() {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(path) = line.strip_prefix(&format!("{LIB_ROOT} ")) {
            lib = Some(absolute_path(path)?);
        } else if let Some((key, path)) = ROOT_NAMES
            .iter()
            .find_map(|(key, _)| line.strip_prefix(&format!("{key} ")).map(|p| (*key, p)))
        {
            roots.insert(key, absolute_path(path)?);
        } else if let Some(path) = line.strip_prefix("dir ") {
            dirs.push(absolute_path(path)?);
        } else {
            files.push(absolute_path(line)?);
        }
    }
    for (key, _) in ROOT_NAMES {
        ensure!(roots.contains_key(key), "record has no `{key}` root line");
    }
    let lib = lib.with_context(|| format!("record has no `{LIB_ROOT}` root line"))?;
    Ok(Record {
        roots,
        lib,
        files,
        dirs,
    })
}

fn absolute_path(text: &str) -> Result<PathBuf> {
    ensure!(
        Path::new(text).is_absolute(),
        "record line is not an absolute path: {text}"
    );
    Ok(PathBuf::from(text))
}

/// A record path's bundle name: under the build dir as `out/...`, else the java out as `java/...`, else the AOSP tree as `src/aosp/...`. Anything else is refused: the bundle lays out known roots only.
fn bundle_name(path: &Path, roots: &BTreeMap<&'static str, PathBuf>) -> Result<String> {
    for (key, name) in ROOT_NAMES {
        if let Some(root) = roots.get(key)
            && let Ok(rel) = path.strip_prefix(root)
        {
            return Ok(format!("{name}/{}", rel.display()));
        }
    }
    bail!(
        "runtime input outside the build dir, the java out and the AOSP tree: {}",
        path.display()
    )
}

/// The shared libraries directly in `dir` (names with `.so`), not its subdirectories: a build dir holds its objects below.
pub fn lib_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let path = entry?.path();
        let is_lib = path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.contains(".so"));
        if is_lib && path.is_file() {
            files.push(path);
        }
    }
    files.sort();
    Ok(files)
}

/// What a bundle entry is: a regular file (its content is copied or hard-linked), or a symbolic link with its target text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Kind {
    File,
    Link(String),
}

/// One bundle entry: its name on the target and the host path it comes from (for a link, the link itself).
#[derive(Debug)]
pub struct Entry {
    pub name: String,
    pub src: PathBuf,
    pub kind: Kind,
}

/// What `push` copies: the gate's record mapped to bundle names, plus `lib/`.
pub struct Bundle {
    pub gate: String,
    pub entries: Vec<Entry>,
}

/// Adds `e` unless its name is taken: the same name from the same source is one entry, another source is an error.
fn insert(map: &mut BTreeMap<String, Entry>, e: Entry) -> Result<()> {
    if let Some(prev) = map.get(&e.name) {
        ensure!(
            prev.src == e.src && prev.kind == e.kind,
            "bundle name `{}` from two sources: {} and {}",
            e.name,
            prev.src.display(),
            e.src.display()
        );
        return Ok(());
    }
    map.insert(e.name.clone(), e);
    Ok(())
}

/// Adds the tree at `dir` under bundle name `name`. Directories recurse. A link to a regular file becomes that file's content under the link's name: its absolute target (a build dir path) would dangle on the target. A link to a directory inside the tree stays a link (the tree's own alias, `com.android.art -> .`); one outside is refused. A dangling link stays as it is.
fn add_tree(dir: &Path, name: &str, map: &mut BTreeMap<String, Entry>) -> Result<()> {
    let root =
        std::fs::canonicalize(dir).with_context(|| format!("resolving {}", dir.display()))?;
    add_entries(dir, name, &root, map)
}

fn add_entries(
    dir: &Path,
    name: &str,
    root: &Path,
    map: &mut BTreeMap<String, Entry>,
) -> Result<()> {
    for entry in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let entry = entry?;
        let path = entry.path();
        let file_name = entry
            .file_name()
            .into_string()
            .map_err(|n| anyhow::anyhow!("{} is not UTF-8: {n:?}", path.display()))?;
        let child = format!("{name}/{file_name}");
        let kind = entry.file_type()?;
        if kind.is_symlink() {
            let target = std::fs::read_link(&path)?;
            let target = target
                .to_str()
                .with_context(|| format!("link target of {} is not UTF-8", path.display()))?
                .to_owned();
            match std::fs::metadata(&path) {
                Ok(m) if m.is_file() => insert(
                    map,
                    Entry {
                        name: child,
                        src: std::fs::canonicalize(&path)?,
                        kind: Kind::File,
                    },
                )?,
                Ok(m) if m.is_dir() => {
                    let real = std::fs::canonicalize(&path)?;
                    ensure!(
                        real.starts_with(root) && Path::new(&target).is_relative(),
                        "link {} to directory {} outside its tree: not bundled",
                        path.display(),
                        real.display()
                    );
                    insert(
                        map,
                        Entry {
                            name: child,
                            src: path,
                            kind: Kind::Link(target),
                        },
                    )?;
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => insert(
                    map,
                    Entry {
                        name: child,
                        src: path,
                        kind: Kind::Link(target),
                    },
                )?,
                Err(e) => return Err(e.into()),
                Ok(_) => bail!("{} is neither a file nor a directory", path.display()),
            }
        } else if kind.is_dir() {
            add_entries(&path, &child, root, map)?;
        } else if kind.is_file() {
            insert(
                map,
                Entry {
                    name: child,
                    src: path,
                    kind: Kind::File,
                },
            )?;
        } else {
            bail!(
                "{} is neither a file, a directory nor a link",
                path.display()
            );
        }
    }
    Ok(())
}

impl Bundle {
    /// The bundle of gate `gate` from its record in `out`, with `brk` applied. The name is checked against gate_goals.rs first.
    /// `cross`: the cross host whose out dir `out` is, if any.
    fn of(gate: &str, out: &Path, cross: Option<&str>, brk: Option<Break>) -> Result<Bundle> {
        gate_goals::declared(gate)?;
        let record = read_record(out, gate, cross)?;
        let mut map = BTreeMap::new();
        for file in &record.files {
            ensure!(
                file.is_file(),
                "record names {} but it is not a file: {}",
                file.display(),
                record_again(gate, cross)
            );
            insert(
                &mut map,
                Entry {
                    name: bundle_name(file, &record.roots)?,
                    // a symlink's own inode would be hard-linked as a dangling link: the real file's
                    src: std::fs::canonicalize(file)
                        .with_context(|| format!("resolving {}", file.display()))?,
                    kind: Kind::File,
                },
            )?;
        }
        for dir in &record.dirs {
            ensure!(
                dir.is_dir(),
                "record names {} but it is not a directory: {}",
                dir.display(),
                record_again(gate, cross)
            );
            add_tree(dir, &bundle_name(dir, &record.roots)?, &mut map)?;
        }
        for (soname, file) in LLVM_RUNTIMES {
            let src = record.lib.join(file);
            ensure!(src.is_file(), "missing LLVM runtime {}", src.display());
            let got = provided_gate::exported(&src)?.soname;
            ensure!(
                got == soname,
                "{} has SONAME `{got}`, expected `{soname}`",
                src.display()
            );
            insert(
                &mut map,
                Entry {
                    name: format!("{LIB_PREFIX}{soname}"),
                    src,
                    kind: Kind::File,
                },
            )?;
        }
        for program in RUN_PROGRAMS {
            ensure!(
                program_entry(&map, program).is_some(),
                "gate `{gate}`: its record names no {program}, which a run starts on the target: {}",
                record_again(gate, cross)
            );
        }
        if brk == Some(Break::DropLibunwind) {
            ensure!(
                map.remove(LIBUNWIND).is_some(),
                "break drop-libunwind: no {LIBUNWIND} in the bundle"
            );
        }
        if brk == Some(Break::DropAtld) {
            let atld =
                program_entry(&map, ATLD).context("break drop-atld: no atld in the bundle")?;
            map.remove(&atld);
        }
        Ok(Bundle {
            gate: gate.to_owned(),
            entries: map.into_values().collect(),
        })
    }

    fn summary(&self) -> String {
        let files = self.entries.iter().filter(|e| e.kind == Kind::File);
        let bytes: u64 = files
            .filter_map(|e| std::fs::metadata(&e.src).ok())
            .map(|m| m.len())
            .sum();
        let links = self
            .entries
            .iter()
            .filter(|e| matches!(e.kind, Kind::Link(_)))
            .count();
        let libs = self
            .entries
            .iter()
            .filter(|e| e.name.starts_with(LIB_PREFIX))
            .count();
        format!(
            "bundle {}: {} entries ({} links), {:.1} MiB summed over file entries, {} in lib/: {}",
            self.gate,
            self.entries.len(),
            links,
            bytes as f64 / MIB,
            libs,
            LLVM_RUNTIMES.map(|(s, _)| s).join(" ")
        )
    }

    /// The bundle names of the regular files that are ELF, by reading each one's first four bytes.
    fn elf_names(&self) -> Result<Vec<String>> {
        let mut elf = Vec::new();
        for e in self.entries.iter().filter(|e| e.kind == Kind::File) {
            let mut magic = [0u8; 4];
            let mut f = std::fs::File::open(&e.src)
                .with_context(|| format!("opening {}", e.src.display()))?;
            if std::io::Read::read(&mut f, &mut magic)? == 4 && magic == *b"\x7fELF" {
                elf.push(e.name.clone());
            }
        }
        Ok(elf)
    }

    /// The bundle directories holding ELF files: the search path of the prerequisite check.
    fn lib_dirs(&self, elf: &[String]) -> Vec<String> {
        let dirs: BTreeSet<String> = elf
            .iter()
            .filter_map(|n| Path::new(n).parent())
            .map(|p| p.display().to_string())
            .filter(|p| !p.is_empty())
            .collect();
        dirs.into_iter().collect()
    }

    /// Stages the entries under `dir` (recreated), with the manifest and its marker. Files are hard links (a copy across filesystems), links are links. Returns the manifest's SHA-256.
    fn stage(&self, dir: &Path) -> Result<Pushed> {
        if dir.exists() {
            std::fs::remove_dir_all(dir).with_context(|| format!("clearing {}", dir.display()))?;
        }
        let mut manifest = String::new();
        for e in &self.entries {
            let dst = dir.join(&e.name);
            if let Some(parent) = dst.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("creating {}", parent.display()))?;
            }
            match &e.kind {
                Kind::Link(target) => {
                    std::os::unix::fs::symlink(target, &dst)
                        .with_context(|| format!("linking {}", dst.display()))?;
                    manifest.push_str(&format!("link  {target}  {}\n", e.name));
                }
                Kind::File => {
                    if std::fs::hard_link(&e.src, &dst).is_err() {
                        std::fs::copy(&e.src, &dst)
                            .with_context(|| format!("copying {}", e.src.display()))?;
                    }
                    let bytes = std::fs::read(&dst)
                        .with_context(|| format!("reading the staged {}", dst.display()))?;
                    let hash = sha256_hex(&bytes);
                    manifest.push_str(&format!("{hash}  {}  {}\n", bytes.len(), e.name));
                }
            }
        }
        let manifest_path = dir.join(MANIFEST);
        let sha = sha256_hex(manifest.as_bytes());
        std::fs::write(&manifest_path, manifest)
            .with_context(|| format!("writing {}", manifest_path.display()))?;
        let marker = dir.join(marker_name(&sha));
        std::fs::write(&marker, "").with_context(|| format!("writing {}", marker.display()))?;
        Ok(Pushed(sha))
    }
}

/// A path the target takes unquoted (the target's directory name, via [`valid_name`]): `[A-Za-z0-9._+/-]`, no `..` component.
fn safe_name(name: &str) -> bool {
    !name.is_empty()
        && !name.split('/').any(|c| c == "..")
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '+' | '/' | '-'))
}

/// A name the target path takes: [`safe_name`] without `/`.
fn valid_name(name: &str) -> bool {
    safe_name(name) && !name.contains('/')
}

/// The shell script the prerequisite check runs in the bundle directory `base` (relative to the home dir over ssh, absolute locally). `files` and `dirs` are bundle names, quoted as shell words (`@` in HIDL names is ordinary); each file gets `ldd`, a missing one is printed as `!missing`.
pub fn prereq_script(base: &str, files: &[String], dirs: &[String]) -> Result<String> {
    for name in files.iter().chain(dirs) {
        ensure!(
            !name.contains('\n'),
            "bundle name {name:?} has a newline: the script takes one name per word"
        );
    }
    let libpath: Vec<String> = dirs
        .iter()
        .map(|d| format!("\"$PWD\"/{}", sh_quote(d)))
        .collect();
    let mut s = format!("cd {} || exit 2\n", sh_quote(base));
    s.push_str(&lock_sh("remote check"));
    s.push_str(&format!(
        "LD_LIBRARY_PATH={}\nexport LD_LIBRARY_PATH\n",
        libpath.join(":")
    ));
    s.push_str("for f in");
    for f in files {
        s.push(' ');
        s.push_str(&sh_quote(f));
    }
    s.push_str("; do\n");
    // only the lines `parse_prereq` reads leave the target: full `ldd` output of a bundle is ~400 KB, and on the laptop -> WSL NAT path
    // one lost segment whose retransmits never arrive stalled that stream until ServerAlive gave up (2026-11-09, sshd Send-Q 400 KB, backoff 9); filtered it is ~15 KB
    s.push_str(
        "if [ -f \"$f\" ]; then echo \"== $f\"; ldd \"$f\" 2>&1 | grep -e '=> not found' -e '^ldd:'; else echo \"!missing $f\"; fi\n",
    );
    // exit 0: a failing `ldd` is a finding in the output, not a failed check (`run_script` fails on any other status)
    s.push_str("done\nexit 0\n");
    Ok(s)
}

/// `s` as one POSIX shell word.
pub(crate) fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// The prerequisite check's result: ELF files run through `ldd`, the sonames that did not resolve (with the file that needs each), the files the target lacks, and `ldd` failures.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Prereq {
    pub checked: usize,
    pub unresolved: Vec<(String, String)>,
    pub missing: Vec<String>,
    pub failures: Vec<String>,
}

impl Prereq {
    /// One line per problem, as `test` and `run-apk` print and fail on.
    pub fn problems(&self) -> Vec<String> {
        let mut p: Vec<String> = self
            .unresolved
            .iter()
            .map(|(file, soname)| format!("NEEDED {soname} of {file}: not found on the target"))
            .collect();
        p.extend(
            self.missing
                .iter()
                .map(|f| format!("{f}: not on the target (run push --apply)")),
        );
        p.extend(self.failures.clone());
        p
    }
}

/// [`prereq_script`]'s output: `== file` opens a file's `ldd` lines, `!missing file` a file the target lacks. Lines with `=> not found` are unresolved sonames. `ldd:` lines are failures, except `not a dynamic executable` (a static file has nothing to resolve) and the execute-permission warning: glibc's `ldd` (2.44 on the laptop) prints it for a bundled 0644 library and still lists its NEEDED.
pub fn parse_prereq(text: &str) -> Prereq {
    let mut p = Prereq::default();
    let mut current = String::new();
    for line in text.lines() {
        let line = line.trim();
        if let Some(f) = line.strip_prefix("!missing ") {
            p.missing.push(f.to_owned());
            current.clear();
        } else if let Some(f) = line.strip_prefix("== ") {
            p.checked += 1;
            current = f.to_owned();
        } else if let Some(soname) = line.strip_suffix("=> not found").map(str::trim_end) {
            p.unresolved.push((current.clone(), soname.to_owned()));
        } else if line.starts_with("ldd:")
            && !line.contains("not a dynamic executable")
            && !line.contains("you do not have execution permission")
        {
            p.failures.push(format!("{current}: {line}"));
        }
    }
    p
}

/// The lock a run or a check holds on its dir ([`lock_sh`]). `prune` tries the same lock before it removes a dir, so no dir is removed under a run or a check.
const RUN_LOCK: &str = ".run.lock";

/// The lock part of a script that runs in its dir: fd 9 holds [`RUN_LOCK`] until the script's processes end (children inherit fd 9, so an atld run keeps the lock while it runs). Exit 6 when another run, check or prune holds it; `what` names the caller.
fn lock_sh(what: &str) -> String {
    format!(
        "exec 9>>{RUN_LOCK}\nflock -n 9 || {{ echo \"{what}: busy, another run, check or prune holds $(pwd)\" >&2; exit 6; }}\n"
    )
}

/// A run's exit when its dir holds another push's bundle ([`stale_sh`]).
const STALE_EXIT: i32 = 7;

/// The run part that proves the dir holds this gate's bundle: one dir per checkout is shared by every gate (a dir per gate would cost 164-418 MiB per gate on the target), so another gate's push may
/// have replaced it between this gate's push and its run. Runs under the run lock ([`lock_sh`]), which a push also takes, so the bundle cannot change after the check. Exit [`STALE_EXIT`] otherwise.
/// The bundle is named by its marker file ([`MARKER_PREFIX`]), not by hashing the manifest on the target: a same-size manifest written within one second is skipped by rsync's quick check, its marker is not.
fn stale_sh(tag: &str, manifest_sha256: &str) -> String {
    format!(
        "test -f {} || {{ echo \"remote run {tag}: stale: another push replaced the bundle\" >&2; exit {STALE_EXIT}; }}\n",
        sh_quote(&marker_name(manifest_sha256))
    )
}

/// The receiving rsync's command for a push into `dir`: the dir made, fd 9 holding its [`RUN_LOCK`] as [`lock_sh`] does (rsync inherits it through `exec`), rsync's server arguments appended by
/// rsync itself after the trailing `rsync`. Exit 6 with `busy` when a run, check, prune or another push holds the lock.
fn push_lock_sh(dir: &str) -> String {
    let (d, lock) = (sh_quote(dir), sh_quote(&format!("{dir}/{RUN_LOCK}")));
    format!(
        "mkdir -p {d} && {{ exec 9>>{lock}; flock -n 9; }} || {{ echo \"remote push: busy, another run, check, prune or push holds {d}\" >&2; exit 6; }}; exec rsync"
    )
}

/// `prune`'s host: an ssh alias. A `local:` dir is refused: it is the caller's, not a run dir.
fn ssh_alias(text: &str) -> Result<String> {
    match Host::parse(text)? {
        Host::Ssh(alias) => Ok(alias),
        Host::Local(dir) => bail!(
            "remote prune: `local:{}` is refused: a local dir is the caller's, not a run dir (name an ssh alias)",
            dir.display()
        ),
    }
}

/// What `prune` makes of a run dir. `Live`: a checkout of this repository (its id, or `<id>-<arch>` for a host arch: a cross build dir, [`target_dir_name`]). `Orphan`: `push` made it (manifest present) and its checkout is gone: the only class `--execute` removes. `Unknown`: anything else, a dir without the manifest included: never touched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Class {
    Live,
    Orphan,
    Unknown,
}

/// The [`Class`] of run dir `name`. `manifest`: [`MANIFEST`] is at the dir's root. `push` writes it into every tree, so without it the dir is not ours, whatever its name. `live`: the repository's live ids ([`out::live_ids_of`]); `host_arches`: the arches with a `[host]` row. `vsync-source-tier-base` is live only as its own id: `base` is no arch.
fn classify(
    name: &str,
    manifest: bool,
    live: &BTreeSet<String>,
    host_arches: &BTreeSet<String>,
) -> Class {
    if !manifest || !id_form(name) {
        return Class::Unknown;
    }
    let cross = |arch: &String| {
        name.strip_suffix(arch.as_str())
            .and_then(|rest| rest.strip_suffix('-'))
            .is_some_and(|id| live.contains(id))
    };
    if live.contains(name) || host_arches.iter().any(cross) {
        Class::Live
    } else {
        Class::Orphan
    }
}

/// A name a checkout's build dir can have: `repo` or a worktree name (`out::checkout_id`), and a name the target path takes ([`valid_name`]). Dot-led names are not.
fn id_form(name: &str) -> bool {
    !name.starts_with('.') && valid_name(name)
}

/// The arches with a `[host]` row in arches.toml: the `<arch>` of a cross build dir's name.
fn host_arch_names(root: &Path) -> Result<BTreeSet<String>> {
    Ok(arches::load(root)?
        .into_iter()
        .filter(|(_, row)| row.host.is_some())
        .map(|(name, _)| name)
        .collect())
}

/// The target's run dirs: `dir<TAB>name<TAB>KiB<TAB>busy|free<TAB>manifest|none` per directory (KiB `?` when `du` fails), `other<TAB>name` for anything else. Read only: a lock file is tested, never created.
fn list_script(base: &str) -> String {
    format!(
        "cd {} 2>/dev/null || exit 0\nfor n in * .[!.]*; do\n  [ -e \"$n\" ] || [ -L \"$n\" ] || continue\n  if [ -L \"$n\" ] || [ ! -d \"$n\" ]; then printf 'other\\t%s\\n' \"$n\"; continue; fi\n  kb=$(du -sk -- \"$n\" 2>/dev/null | cut -f1)\n  busy=free\n  if [ -e \"$n/{RUN_LOCK}\" ] && ! flock -n \"$n/{RUN_LOCK}\" true; then busy=busy; fi\n  manifest=none\n  if [ -f \"$n/{MANIFEST}\" ]; then manifest=manifest; fi\n  printf 'dir\\t%s\\t%s\\t%s\\t%s\\n' \"$n\" \"${{kb:-?}}\" \"$busy\" \"$manifest\"\ndone\n",
        sh_quote(base)
    )
}

/// One entry of [`list_script`]'s output.
#[derive(Debug, PartialEq, Eq)]
enum Listed {
    /// A directory: its size in KiB (`None` when `du` failed), whether a run or check holds its lock, and whether [`MANIFEST`] is at its root.
    Dir {
        name: String,
        kib: Option<u64>,
        busy: bool,
        manifest: bool,
    },
    /// A file, a symlink, or a line in no shape [`list_script`] prints: never touched.
    Other(String),
}

fn parse_listing(text: &str) -> Vec<Listed> {
    text.lines()
        .filter(|line| !line.is_empty())
        .map(|line| {
            let fields: Vec<&str> = line.split('\t').collect();
            match fields.as_slice() {
                ["dir", name, kib, busy, manifest] => Listed::Dir {
                    name: (*name).to_owned(),
                    kib: kib.parse().ok(),
                    busy: *busy == "busy",
                    manifest: *manifest == "manifest",
                },
                ["other", name] => Listed::Other((*name).to_owned()),
                _ => Listed::Other(line.to_owned()),
            }
        })
        .collect()
}

/// Removes orphan dirs `names` under `base` on the target, each under its own lock: `flock -E 75` exits 75 while the lock is held, and that dir is skipped. Names pass [`id_form`] first: no `/`, no `..`, nothing dot-led. Per name: `removed`, `busy` or `failed`, tab-separated.
fn remove_script(base: &str, names: &[String]) -> Result<String> {
    let mut s = format!("cd {} || exit 2\n", sh_quote(base));
    for name in names {
        ensure!(
            id_form(name),
            "remote prune: `{name}` is not a run dir name"
        );
        let (q, lock) = (sh_quote(name), sh_quote(&format!("{name}/{RUN_LOCK}")));
        s.push_str(&format!(
            "if flock -n -E 75 {lock} rm -rf -- {q}; then printf 'removed\\t%s\\n' {q}; else rc=$?; if [ \"$rc\" -eq 75 ]; then printf 'busy\\t%s\\n' {q}; else printf 'failed\\t%s\\t%s\\n' {q} \"$rc\"; fi; fi\n"
        ));
    }
    s.push_str("exit 0\n");
    Ok(s)
}

/// What [`remove_script`] did to one name.
#[derive(Debug, PartialEq, Eq)]
enum Removal {
    Removed,
    Busy,
    Failed(String),
}

/// [`remove_script`]'s outcome per name. A name with no line has no outcome; the caller counts it failed.
fn parse_removal(text: &str) -> BTreeMap<String, Removal> {
    text.lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split('\t').collect();
            match fields.as_slice() {
                ["removed", name] => Some(((*name).to_owned(), Removal::Removed)),
                ["busy", name] => Some(((*name).to_owned(), Removal::Busy)),
                ["failed", name, rc] => {
                    Some(((*name).to_owned(), Removal::Failed(format!("exit {rc}"))))
                }
                _ => None,
            }
        })
        .collect()
}

/// A run dir's size as `prune` prints it: `12.3 MiB`, `? MiB` when `du` failed.
fn size_label(kib: Option<u64>) -> String {
    kib.map_or_else(
        || "? MiB".to_owned(),
        |k| format!("{:.1} MiB", k as f64 / 1024.0),
    )
}

/// `remote prune --host <alias> [--execute]`: classifies the target's run dirs ([`classify`]). Dry run by default; `--execute` removes the free orphans, the ones no run or check holds.
fn prune(root: &Path, a: &PruneArgs) -> Result<ExitCode> {
    let host = Host::Ssh(ssh_alias(&a.host)?);
    host.reach()?;
    let live = out::live_ids_of(root)?;
    let host_arches = host_arch_names(root)?;
    let failed = prune_dirs(&host, REMOTE_ROOT, &live, &host_arches, a.execute)?;
    Ok(if failed == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

/// One run dir as `prune` holds it after [`classify`].
struct Row {
    name: String,
    kib: Option<u64>,
    class: Class,
    busy: bool,
    manifest: bool,
}

/// [`prune`] on run dir `base` of `host`: the same code the tests run on a throwaway dir. Returns the number of failed removals.
fn prune_dirs(
    host: &Host,
    base: &str,
    live: &BTreeSet<String>,
    host_arches: &BTreeSet<String>,
    execute: bool,
) -> Result<usize> {
    let mut rows = Vec::new();
    for entry in parse_listing(&host.run_script(&list_script(base))?) {
        match entry {
            Listed::Dir {
                name,
                kib,
                busy,
                manifest,
            } => {
                let class = classify(&name, manifest, live, host_arches);
                rows.push(Row {
                    name,
                    kib,
                    class,
                    busy,
                    manifest,
                });
            }
            Listed::Other(text) => println!("unknown {text}: not a directory, never touched"),
        }
    }
    println!(
        "remote prune {}: {} run dir(s) under {base}",
        host.label(),
        rows.len()
    );
    for row in &rows {
        let label = match row.class {
            Class::Live => "live",
            Class::Orphan => "orphan",
            Class::Unknown => "unknown",
        };
        let note = match (row.class, row.busy, row.manifest) {
            (Class::Orphan, true, _) => ", busy: a run or check holds it, skipped",
            (Class::Unknown, _, false) => ", no bundle.manifest: never touched",
            (Class::Unknown, _, true) => ", not a checkout's name: never touched",
            _ => "",
        };
        println!("{label} {} {}{note}", row.name, size_label(row.kib));
    }
    let removable: Vec<&Row> = rows
        .iter()
        .filter(|row| row.class == Class::Orphan && !row.busy)
        .collect();
    let held = rows
        .iter()
        .filter(|row| row.class == Class::Orphan && row.busy)
        .count();
    let planned_kib: u64 = removable.iter().filter_map(|row| row.kib).sum();
    if !execute {
        let note = if held > 0 {
            format!("; {held} held orphan(s) skipped")
        } else {
            String::new()
        };
        println!(
            "remote prune: dry run, would remove {} tree(s), {:.1} MiB (add --execute){note}",
            removable.len(),
            planned_kib as f64 / 1024.0
        );
        return Ok(0);
    }
    let outcomes = if removable.is_empty() {
        BTreeMap::new()
    } else {
        let names: Vec<String> = removable.iter().map(|row| row.name.clone()).collect();
        parse_removal(&host.run_script(&remove_script(base, &names)?)?)
    };
    let (mut removed, mut gone_kib, mut busy, mut failed) = (0usize, 0u64, held, 0usize);
    for row in &removable {
        match outcomes.get(&row.name) {
            Some(Removal::Removed) => {
                removed += 1;
                gone_kib += row.kib.unwrap_or(0);
                println!("removed {} {}", row.name, size_label(row.kib));
            }
            Some(Removal::Busy) => {
                busy += 1;
                println!("orphan {}: busy, skipped", row.name);
            }
            Some(Removal::Failed(why)) => {
                failed += 1;
                println!("FAILED {}: {why}", row.name);
            }
            None => {
                failed += 1;
                println!("FAILED {}: no result from the target", row.name);
            }
        }
    }
    println!(
        "remote prune: removed {removed} tree(s), {:.1} MiB; {busy} busy skipped; {failed} failed",
        gone_kib as f64 / 1024.0
    );
    Ok(failed)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A plan's manifest hash where the run's stale check is not under test.
    const TEST_SHA: &str = "abc123";

    fn roots() -> BTreeMap<&'static str, PathBuf> {
        BTreeMap::from([
            ("out", PathBuf::from("/w/out/pool/ws/x9")),
            ("java", PathBuf::from("/w/out/pool/ws/x9")),
            ("tree", PathBuf::from("/w/src/aosp")),
        ])
    }

    fn names(b: &Bundle) -> Vec<String> {
        b.entries.iter().map(|e| e.name.clone()).collect()
    }

    fn temp(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("xtask-remote-{tag}-{}", std::process::id()))
    }

    #[test]
    fn alias_rejects_addresses_and_user_names() {
        assert!(matches!(Host::parse("laptop"), Ok(Host::Ssh(a)) if a == "laptop"));
        assert!(Host::parse("user@laptop").is_err());
        assert!(Host::parse("host:path").is_err());
        assert!(Host::parse("-oProxyCommand=x").is_err());
        assert!(Host::parse("").is_err());
    }

    #[test]
    fn alias_names_one_transcript_dir() {
        // `.` and `..` would name out/remote/ itself or its parent
        assert!(Host::parse(".").is_err());
        assert!(Host::parse("..").is_err());
        assert!(Host::parse("...").is_err());
        assert!(matches!(Host::parse("a.b"), Ok(Host::Ssh(a)) if a == "a.b"));
        assert_eq!(
            Host::parse("node-a").map(|h| h.log_dir()).ok().as_deref(),
            Some("node-a")
        );
        assert_eq!(
            Host::parse("local:/tmp/t")
                .map(|h| h.log_dir())
                .ok()
                .as_deref(),
            Some("local")
        );
    }

    #[test]
    fn local_host_needs_an_absolute_dir() {
        assert!(matches!(Host::parse("local:/tmp/t"), Ok(Host::Local(_))));
        assert!(Host::parse("local:relative").is_err());
    }

    #[test]
    fn bundle_names_follow_the_roots() {
        let r = roots();
        assert_eq!(
            bundle_name(Path::new("/w/out/pool/ws/x9/device/libart/libart.so"), &r).unwrap(),
            "out/device/libart/libart.so"
        );
        assert_eq!(
            bundle_name(Path::new("/w/src/aosp/frameworks/base/x.kcm"), &r).unwrap(),
            "src/aosp/frameworks/base/x.kcm"
        );
        assert!(bundle_name(Path::new("/usr/lib/libc.so.6"), &r).is_err());
    }

    #[test]
    fn record_parses_roots_dirs_and_files() {
        let text = "# c\nout /w/out/pool/ws/x9\njava /w/out/pool/ws/x9\ntree /w/src/aosp\nlib /s/usr/lib/llvm-19/lib\ndir /w/out/pool/ws/x9/gl-art\n/w/out/pool/ws/x9/a.so\n";
        let rec = parse_record(text).unwrap();
        assert_eq!(rec.files, vec![PathBuf::from("/w/out/pool/ws/x9/a.so")]);
        assert_eq!(rec.dirs, vec![PathBuf::from("/w/out/pool/ws/x9/gl-art")]);
        assert_eq!(rec.roots["tree"], PathBuf::from("/w/src/aosp"));
        assert_eq!(rec.lib, PathBuf::from("/s/usr/lib/llvm-19/lib"));
        assert!(
            parse_record("out /o\njava /o\nlib /l\n/x.so\n").is_err(),
            "no tree root"
        );
        assert!(
            parse_record("out /o\njava /o\ntree /t\n/x.so\n").is_err(),
            "no lib root"
        );
        assert!(parse_record("out /o\njava /o\ntree /t\nlib /l\nrelative.so\n").is_err());
        assert!(parse_record("out /o\njava /o\ntree /t\nlib /l\ndir relative\n").is_err());
    }

    /// Two build dirs' riscv64 outs get two target dirs; a build machine out keeps its dir name. Break: naming a cross out by its arch alone makes the first pair equal.
    #[test]
    fn cross_outs_of_two_build_dirs_get_two_target_dirs() {
        let n = |p: &str, cross| target_dir_name(Path::new(p), cross).unwrap();
        assert_eq!(n("/w/out/pool/ws/x4/riscv64", true), "x4-riscv64");
        assert_ne!(
            n("/w/out/pool/ws/x4/riscv64", true),
            n("/w/out/pool/ws/x5/riscv64", true)
        );
        assert_eq!(n("/w/out/pool/ws/x9", false), "x9");
    }

    #[test]
    fn prereq_script_quotes_and_orders_the_check() {
        let s = prereq_script(
            "atl-next-run/x9",
            &["lib/libc++.so.1".to_owned(), "out/device/x.so".to_owned()],
            &["lib".to_owned(), "out/device".to_owned()],
        )
        .unwrap();
        let want = concat!(
            "cd 'atl-next-run/x9' || exit 2\n",
            "exec 9>>.run.lock\n",
            "flock -n 9 || { echo \"remote check: busy, another run, check or prune holds $(pwd)\" >&2; exit 6; }\n",
            "LD_LIBRARY_PATH=\"$PWD\"/'lib':\"$PWD\"/'out/device'\n",
            "export LD_LIBRARY_PATH\n",
            "for f in 'lib/libc++.so.1' 'out/device/x.so'; do\n",
            "if [ -f \"$f\" ]; then echo \"== $f\"; ldd \"$f\" 2>&1 | grep -e '=> not found' -e '^ldd:'; else echo \"!missing $f\"; fi\n",
            "done\n",
            "exit 0\n",
        );
        assert_eq!(s, want);
        // names with shell metacharacters stay one word; a newline cannot be quoted onto one line
        let odd = prereq_script("b", &["a@1.0;x'y.so".to_owned()], &[]).unwrap();
        assert!(odd.contains("for f in 'a@1.0;x'\\''y.so'; do"), "{odd}");
        assert!(prereq_script("b", &["a\nb".to_owned()], &[]).is_err());
    }

    #[test]
    fn prereq_output_carries_only_parsed_lines() {
        // resolved ldd lines must not leave the target: the full listing is what stalled the laptop link
        let dir = temp("prereq-filter");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::copy("/bin/sh", dir.join("sh")).unwrap();
        let script = prereq_script(
            dir.to_str().unwrap(),
            &["sh".to_owned(), "gone.so".to_owned()],
            &[],
        )
        .unwrap();
        let out = Host::Local(PathBuf::from(".")).run_script(&script).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(out, "== sh\n!missing gone.so\n");
        let p = parse_prereq(&out);
        assert_eq!((p.checked, p.missing.len()), (1, 1));
    }

    #[test]
    fn prerequisite_check_fails_on_a_nonzero_exit_even_with_output() {
        // a dropped ssh leaves partial stdout and a non-zero status: that is no verdict
        let host = Host::Local(PathBuf::from("."));
        assert_eq!(host.run_script("echo PASS\nexit 0\n").unwrap(), "PASS\n");
        let partial = host.run_script("echo PASS, 19 ELF checked\nexit 3\n");
        assert!(partial.is_err(), "partial output with exit 3 was accepted");
    }

    #[test]
    fn ldd_output_names_each_unresolved_soname_and_its_file() {
        let text = "== lib/libc++abi.so.1\n\tlibunwind.so.1 => not found\n\tlibc.so.6 => /lib/libc.so.6 (0x1)\n== out/device/a.so\nldd: /x: not a dynamic executable\n== out/b.so\nldd: exited with unknown exit code (139)\n!missing out/c.so\n";
        let p = parse_prereq(text);
        assert_eq!(p.checked, 3);
        assert_eq!(
            p.unresolved,
            vec![("lib/libc++abi.so.1".to_owned(), "libunwind.so.1".to_owned())]
        );
        assert_eq!(p.missing, vec!["out/c.so".to_owned()]);
        assert_eq!(p.failures.len(), 1, "{:?}", p.failures);
        assert!(p.failures[0].starts_with("out/b.so: ldd: exited"));
        let problems = p.problems();
        assert!(
            problems[0].contains("NEEDED libunwind.so.1 of lib/libc++abi.so.1"),
            "{problems:?}"
        );
    }

    #[test]
    fn ldd_permission_warning_is_not_a_failure() {
        let p = parse_prereq(
            "== lib/x.so\nldd: warning: you do not have execution permission for `lib/x.so'\n\tlibfoo.so.1 => not found\n\tlibc.so.6 => /lib/libc.so.6 (0x1)\n",
        );
        assert!(p.failures.is_empty(), "{:?}", p.failures);
        assert_eq!(
            p.unresolved,
            vec![("lib/x.so".to_owned(), "libfoo.so.1".to_owned())]
        );
    }

    #[test]
    fn green_output_has_no_problems() {
        let p = parse_prereq("== a.so\n\tlibc.so.6 => /lib/x (0x1)\n\tlinux-vdso.so.1 (0x2)\n");
        assert_eq!(p.checked, 1);
        assert!(p.problems().is_empty());
    }

    #[test]
    fn record_round_trips_through_its_file() {
        let dir = temp("record");
        let out = dir.join("build");
        let tree = dir.join("tree");
        let inputs = Inputs {
            files: vec![out.join("device/a.so"), tree.join("x.kcm")],
            dirs: vec![out.join("gl-art")],
        };
        write_record(&out, "gl", &out, &tree, Path::new(LLVM_LIB_DIR), &inputs).unwrap();
        let rec = read_record(&out, "gl", None).unwrap();
        assert_eq!(rec.files, inputs.files);
        assert_eq!(rec.dirs, inputs.dirs);
        assert_eq!(rec.roots["out"], out);
        assert_eq!(rec.roots["tree"], tree);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// What [`run_rungs`]'s script did: its exit, its stderr, the rungs labwc was started on, the transfer's end and the run's scratch dir as it came back.
    struct Rungs {
        code: Option<i32>,
        err: String,
        starts: Vec<String>,
        end: ExitStatus,
        back: PathBuf,
        _home: Scratch,
    }

    /// The headless labwc's script run whole, with a fake labwc (it links a live socket and notes whether the second rung's word reached it) and a fake helper that ends with `first` on the default rung and `second` on the other, `ready` standing for a helper that stays.
    fn run_rungs(tag: &str, first: &str, second: &str) -> Rungs {
        let home = Scratch::new(tag);
        let dir = home.0.clone();
        let scratch = dir.join("out/app/scratch");
        std::fs::create_dir_all(&scratch).unwrap();
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        std::fs::create_dir_all(dir.join("rt")).unwrap();
        let d = dir.display();
        let _live = std::os::unix::net::UnixListener::bind(dir.join("sock")).unwrap();
        let tool = |name: &str, body: &str| {
            use std::os::unix::fs::PermissionsExt;
            let path = dir.join("bin").join(name);
            std::fs::write(&path, format!("#!/bin/sh\n{body}")).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        };
        tool(
            "labwc",
            &format!(
                "echo \"${{WLR_EGL_NO_MODIFIERS:-default}}\" >> {d}/starts\necho no format\nln -s {d}/sock \"$XDG_RUNTIME_DIR/wayland-0\"\nexec sleep 30\n"
            ),
        );
        let step = |what: &str| match what {
            "ready" => "echo ready; exec sleep 30".to_owned(),
            rc => format!("echo refused {rc}; exit {rc}"),
        };
        tool(
            "helper",
            &format!(
                "if [ \"$(wc -l < {d}/starts)\" -eq 1 ]; then {}; else {}; fi\n",
                step(first),
                step(second)
            ),
        );
        let logs = ["labwc.log", LABWC_SETUP_LOG].map(|f| format!("app/scratch/{f}"));
        let script = format!(
            "set -u\nBASE={}\n{}kill \"$HS\" \"$LW\"\n",
            sh_quote(&d.to_string()),
            nested_headless_script(
                "app",
                &sh_quote(&dir.join("rt").display().to_string()),
                "\"$BASE/cfg\"",
                (
                    "\"$BASE/out/app/scratch/labwc.log\"",
                    "\"$BASE/out/app/scratch/labwc-setup.log\"",
                ),
                &format!("{d}/bin/helper"),
                &tar_sh(&logs),
            )
        );
        let out = Command::new("sh")
            .args(["-c", &script])
            .env("PATH", format!("{d}/bin:/usr/bin:/bin"))
            .output()
            .unwrap();
        let back = dir.join("back");
        std::fs::create_dir_all(&back).unwrap();
        let mut tar = Command::new("tar")
            .arg("-x")
            .arg("-C")
            .arg(&back)
            .stdin(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        tar.stdin.take().unwrap().write_all(&out.stdout).unwrap();
        let end = tar.wait().unwrap();
        let starts = std::fs::read_to_string(dir.join("starts")).unwrap();
        assert_eq!(
            std::fs::read_dir(dir.join("rt")).unwrap().count(),
            0,
            "the nest dir stays"
        );
        Rungs {
            code: out.status.code(),
            err: String::from_utf8_lossy(&out.stderr).into_owned(),
            starts: starts.lines().map(str::to_owned).collect(),
            end,
            back: back.join("app/scratch"),
            _home: home,
        }
    }

    /// The first rung's output is set up: one labwc, as it is, and the transcript says so.
    #[test]
    fn a_labwc_that_takes_the_output_stays_on_the_default_rung() {
        let Rungs {
            code, err, starts, ..
        } = run_rungs("rung-default", "ready", "ready");
        assert_eq!(code, Some(0), "{err}");
        assert_eq!(starts, ["default"]);
        assert!(
            err.contains("remote run app: labwc rung default\n"),
            "{err}"
        );
    }

    /// A refused output (exit 3, atl-labwc-setup's help) restarts labwc once without modifiers.
    #[test]
    fn a_refused_output_restarts_labwc_without_modifiers() {
        let Rungs {
            code, err, starts, ..
        } = run_rungs("rung-second", "3", "ready");
        assert_eq!(code, Some(0), "{err}");
        assert_eq!(starts, ["default", "1"]);
        assert!(
            err.contains("not ready on rung default (see labwc-setup.log): refused 3\n")
                && err.contains("remote run app: labwc rung no-modifiers\n"),
            "{err}"
        );
    }

    /// Refused on both: the run stops with both answers, its logs come back whole, and without an end record it failed.
    #[test]
    fn an_output_refused_on_both_rungs_fails_with_both_answers_and_logs() {
        let Rungs {
            code,
            err,
            starts,
            end,
            back,
            _home,
        } = run_rungs("rung-none", "3", "3");
        assert_eq!(code, Some(5), "{err}");
        assert_eq!(starts, ["default", "1"]);
        assert!(
            err.contains("on rung default (see labwc-setup.log): refused 3\n")
                && err.contains("on rung no-modifiers (see labwc-setup.log): refused 3\n"),
            "{err}"
        );
        let read = |name: &str| std::fs::read_to_string(back.join(name)).unwrap();
        assert_eq!(
            read(LABWC_SETUP_LOG),
            "== labwc rung default\nrefused 3\n== labwc rung no-modifiers\nrefused 3\n"
        );
        assert_eq!(
            read("labwc.log"),
            "== labwc rung default\nno format\n== labwc rung no-modifiers\nno format\n"
        );
        assert!(end.success());
        assert!(ended_early(end, &back), "no end record, yet not a failure");
        std::fs::write(back.join(ATLD_RC), "0\n").unwrap();
        assert!(!ended_early(end, &back), "a whole run");
    }

    /// Any other failure of the helper is no answer about the renderer: no second rung.
    #[test]
    fn another_setup_failure_does_not_change_the_rung() {
        let Rungs {
            code, err, starts, ..
        } = run_rungs("rung-other", "1", "ready");
        assert_eq!(code, Some(5), "{err}");
        assert_eq!(starts, ["default"]);
    }

    /// The script's own message is the cause; the transfer's complaint follows it.
    #[test]
    fn failed_run_names_the_script_message_first() {
        let dir = temp("failed");
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("gl.log");
        std::fs::write(
            &log,
            "== run gl/run on local:/b\nremote run gl/run: out/x is not an executable\ntar: does not look like a tar archive\n",
        )
        .unwrap();
        let run = RemoteRun {
            host: Host::Local(PathBuf::from("/b")),
            base: "/b".to_owned(),
            lib_dirs: Vec::new(),
            limit: Duration::from_secs(1),
            log: log.clone(),
            display: Display::Headless,
            dmabuf_probe: false,
            view: None,
            agent: None,
            manifest_sha256: TEST_SHA.to_owned(),
            audio: None,
        };
        // a wait status: exit code 2 is `2 << 8`
        let end = std::os::unix::process::ExitStatusExt::from_raw(2 << 8);
        let msg = format!("{:#}", run.failed(Path::new("/w/out/gl/run/root"), end));
        assert!(
            msg.contains("ended exit status: 2: remote run gl/run: out/x is not an executable | tar: does not look like a tar archive"),
            "{msg}"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn host_roots() -> Roots {
        Roots::of(
            Path::new("/w/out/pool/ws/x9"),
            Path::new("/w/out/java/build"),
            Path::new("/w/src/aosp"),
            Path::new(LLVM_LIB_DIR),
        )
    }

    /// A removed variable (atld_run's gate default for PULSE_SERVER) is absent on the target, not an error (the 2026-11-18 `"PULSE_SERVER" is unset, not set` refusal); a session variable is
    /// read on the target and passed only when set there; an explicit value wins over the session's.
    #[test]
    fn removed_vars_are_absent_and_session_vars_resolve_on_the_target() {
        let r = host_roots();
        let mut cmd = Command::new("atld");
        cmd.env_remove("PULSE_SERVER")
            .env("RUST_LOG", "info")
            .env("FONTCONFIG_FILE", "/w/out/pool/ws/x9/f.conf");
        let env = command_env(&cmd, &r).expect("removal is no error");
        let mut keys: Vec<&str> = env.iter().map(|(k, _)| k.as_str()).collect();
        keys.sort_unstable();
        assert_eq!(keys, ["FONTCONFIG_FILE", "RUST_LOG"]);
        assert!(env.contains(&("FONTCONFIG_FILE".into(), "@BASE@/out/f.conf".into())));
        assert_eq!(from_session(&["PULSE_SERVER"], &env), ["PULSE_SERVER"]);
        let explicit = [("PULSE_SERVER".to_owned(), "unix:/p".to_owned())];
        assert!(from_session(&["PULSE_SERVER"], &explicit).is_empty());

        let plan = Plan {
            manifest_sha256: TEST_SHA.to_owned(),
            dmabuf_probe: false,
            audio: None,
            tag: "run-apk/x".into(),
            root: "run-apk/x/root".into(),
            scratch: "run-apk/x/scratch".into(),
            env,
            session_env: vec!["PULSE_SERVER".into()],
            program: "@BASE@/out/cargo/release/atld".into(),
            args: Vec::new(),
            files: Vec::new(),
            lib_dirs: Vec::new(),
            limit: 60,
            display: Display::Headless,
            view: None,
            agent: None,
        };
        let s = plan.script("b");
        let read = s
            .find("ATL_SESSION_PULSE_SERVER=$(systemctl --user show-environment")
            .expect("the session read");
        let env_line = s.find("env -i ").expect("env line");
        assert!(read < env_line, "{s}");
        assert!(
            s[env_line..].contains(
                " ${ATL_SESSION_PULSE_SERVER:+\"PULSE_SERVER=$ATL_SESSION_PULSE_SERVER\"}"
            ),
            "{s}"
        );
    }

    /// The `${V:+"N=$V"}` word under sh: nothing when the session has no value, one `N=value` word (spaces kept) when it has one.
    #[test]
    fn session_var_word_expands_to_nothing_or_one_word() {
        let word = format!(
            "${{{v}:+\"PULSE_SERVER=${v}\"}}",
            v = session_var("PULSE_SERVER")
        );
        let run = |value: Option<&str>| {
            let mut c = Command::new("sh");
            c.arg("-c")
                .arg(format!("set -- {word}; echo \"$#:$*\""))
                .env_remove(session_var("PULSE_SERVER"));
            if let Some(v) = value {
                c.env(session_var("PULSE_SERVER"), v);
            }
            String::from_utf8(c.output().expect("sh").stdout).expect("utf8")
        };
        assert_eq!(run(None), "0:\n");
        assert_eq!(run(Some("")), "0:\n");
        assert_eq!(run(Some("unix:/a b")), "1:PULSE_SERVER=unix:/a b\n");
    }

    #[test]
    fn relocate_maps_each_root_at_a_token_start_only() {
        let r = host_roots();
        assert_eq!(
            relocate("--root=/w/out/pool/ws/x9/gl/run/root", &r),
            "--root=@BASE@/out/gl/run/root"
        );
        assert_eq!(
            relocate("/w/out/java/build/boot.jar", &r),
            "@BASE@/java/boot.jar"
        );
        assert_eq!(relocate("/w/src/aosp/k.kcm", &r), "@BASE@/src/aosp/k.kcm");
        // `--file host=at`: the host side moves, the Android path stays
        assert_eq!(
            relocate("--file=/w/out/pool/ws/x9/a.so=/system/lib64/a.so", &r),
            "--file=@BASE@/out/a.so=/system/lib64/a.so"
        );
        // a library path joins roots with `:`
        assert_eq!(
            relocate("/w/out/pool/ws/x9/a:/w/out/pool/ws/x9/b", &r),
            "@BASE@/out/a:@BASE@/out/b"
        );
        // a JSON string value
        assert_eq!(
            relocate(r#"{"apk": "/w/out/pool/ws/x9/x.apk"}"#, &r),
            r#"{"apk": "@BASE@/out/x.apk"}"#
        );
        // element text: `>` and `<` bound a path too (fontconfig's `<dir>`)
        assert_eq!(
            relocate("<dir>/w/out/pool/ws/x9/fonts/files</dir>", &r),
            "<dir>@BASE@/out/fonts/files</dir>"
        );
        // a root followed by a name character is another path
        assert_eq!(
            relocate("/w/out/pool/ws/x9.bak", &r),
            "/w/out/pool/ws/x9.bak"
        );
        // the bundle's `lib/` stands for the host's LLVM runtimes
        assert_eq!(
            relocate("--lib-path=/w/out/pool/ws/x9/a:/usr/lib/llvm-19/lib", &r),
            "--lib-path=@BASE@/out/a:@BASE@/lib"
        );
        // a directory that only starts with the root's name is not under it
        assert_eq!(relocate("/w/out/pool/ws/x90/x", &r), "/w/out/pool/ws/x90/x");
    }

    #[test]
    fn word_quotes_for_the_shell_and_expands_the_base() {
        assert_eq!(word("@BASE@/out/a b$c\"d"), "\"$BASE/out/a b\\$c\\\"d\"");
    }

    #[test]
    fn remote_runs_keep_their_files_under_the_build_dir() {
        let out = Path::new("/w/out/pool/ws/x9");
        assert_eq!(
            rel_out(out, Path::new("/w/out/pool/ws/x9/gl/run/root")).unwrap(),
            "gl/run/root"
        );
        assert!(rel_out(out, Path::new("/w/elsewhere/x")).is_err());
    }

    /// One run's script, written out: directories fresh, the embedded file with `$BASE` expanded on the target, atld under `env -i` with stdin closed, then its logs as a tar.
    #[test]
    fn run_script_is_the_plan_in_order() {
        let plan = Plan {
            manifest_sha256: TEST_SHA.to_owned(),
            dmabuf_probe: false,
            audio: None,
            tag: "gl/run".to_owned(),
            root: "gl/run/root".to_owned(),
            scratch: "gl/run/scratch".to_owned(),
            env: vec![(
                "FONTCONFIG_FILE".to_owned(),
                "@BASE@/out/gl/run/scratch/fonts.conf".to_owned(),
            )],
            session_env: Vec::new(),
            program: "@BASE@/out/cargo/release/atld".to_owned(),
            args: vec!["--root".to_owned(), "@BASE@/out/gl/run/root".to_owned()],
            files: vec![(
                "@BASE@/out/gl/run/scratch/package-0.json".to_owned(),
                "{\"apk\": \"@BASE@/out/gl/apk/x.apk\"}\n".to_owned(),
            )],
            lib_dirs: vec!["lib".to_owned()],
            limit: 120,
            display: Display::Headless,
            view: None,
            agent: None,
        };
        let want = concat!(
            "# xtask remote run gl/run: written by remote.rs, do not edit\n",
            "{\n",
            "cd 'atl-next-run/x9' || exit 2\n",
            "set -u\n",
            "BASE=$(pwd)\n",
            "exec 7<&0\n",
            "exec 9>>.run.lock\n",
            "flock -n 9 || { echo \"remote run gl/run: busy, another run, check or prune holds $(pwd)\" >&2; exit 6; }\n",
            "test -f '.bundle-abc123' || { echo \"remote run gl/run: stale: another push replaced the bundle\" >&2; exit 7; }\n",
            "rm -rf \"$BASE/out/gl/run/root\" \"$BASE/out/gl/run/root.data\"\n",
            "mkdir -p \"$BASE/out/gl/run/scratch\"\n",
            "sed \"s|@BASE@|$BASE|g\" > \"$BASE/out/gl/run/scratch/package-0.json\" <<'ATL_EOF'\n",
            "{\"apk\": \"@BASE@/out/gl/apk/x.apk\"}\n",
            "ATL_EOF\n",
            "[ -x \"$BASE/out/cargo/release/atld\" ] || { echo \"remote run gl/run: out/cargo/release/atld is not an executable\" >&2; exit 4; }\n",
            "env -i PATH=/usr/local/bin:/usr/bin:/bin HOME=\"$HOME\" XDG_RUNTIME_DIR=\"/run/user/$(id -u)\" LD_LIBRARY_PATH=\"$BASE/lib\" FONTCONFIG_FILE=\"$BASE/out/gl/run/scratch/fonts.conf\" setsid timeout --foreground -k 10 120 \"$BASE/out/cargo/release/atld\" \"--root\" \"$BASE/out/gl/run/root\" < /dev/null 7<&- > \"$BASE/out/gl/run/scratch/atld.log\" 2>&1 &\n",
            "pid=$!\n",
            "( cat <&7 > /dev/null 2>&1; kill -TERM \"$pid\" 2>/dev/null ) > /dev/null 2>&1 &\n",
            "WD=$!\n",
            "wait \"$pid\"\n",
            "rc=$?\n",
            "kill \"$WD\" 2>/dev/null || true\n",
            "kill -KILL -- \"-$pid\" 2>/dev/null || true\n",
            "printf '%s\\n' \"$rc\" > \"$BASE/out/gl/run/scratch/atld.rc\"\n",
            "date +%s%3N > \"$BASE/out/gl/run/scratch/end.ms\"\n",
            "echo \"atld rc=$rc, remote run gl/run\" >&2\n",
            "{ cat /sys/devices/system/cpu/possible /sys/devices/system/cpu/online; nproc; env -i PATH=/usr/bin:/bin sh -c @ZONE_SH@; } > \"$BASE/out/gl/run/scratch/host.facts\"\n",
            "cat /proc/cpuinfo > \"$BASE/out/gl/run/scratch/cpuinfo\"\n",
            "tar -c -C \"$BASE/out\" --ignore-failed-read 'gl/run/root/log' 'gl/run/scratch/atld.log' 'gl/run/scratch/atld.rc' 'gl/run/scratch/end.ms' 'gl/run/scratch/host.facts' 'gl/run/scratch/cpuinfo'\n",
            "exit\n",
            "}\n",
        );
        // no TZ row in env: the zone probe runs without TZ, as atld does
        assert_eq!(
            plan.script("atl-next-run/x9"),
            want.replace("@ZONE_SH@", &sh_quote(ZONE_SH))
        );
    }

    /// The sink name of the drop tests' runs.
    const DROP_SINK: &str = "atl-run-apk-drop";

    /// A muted headless run's script for [`run_live`]: a fake atld at `out/cargo/atld` (args: its scratch dir) under `limit` s, the bundle marker [`stale_sh`] wants in `dir`.
    /// Fake atld: a daemon child in its process group that records a TERM it receives (`daemon-term`); atld's TERM handler stops it by SIGKILL, so `daemon-term` exists only when
    /// somebody else TERMed the group. Fake pw-cli: `ls Node` shows the sink; the holder reads its line and stays, as pw-cli 1.4 does after EOF.
    fn drop_fixture(dir: &Path, limit: u64) -> String {
        let _ = std::fs::remove_dir_all(dir);
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        std::fs::create_dir_all(dir.join("out/cargo")).unwrap();
        std::fs::write(dir.join(format!(".bundle-{TEST_SHA}")), "").unwrap();
        let exe = |path: PathBuf, text: &str| atl_exec_fixture::write_exe(&path, text);
        exe(
            dir.join("out/cargo/atld"),
            "#!/bin/sh\nsh -c 'trap \"echo term > $1/daemon-term; exit 0\" TERM; while :; do sleep 0.1; done' sh \"$1\" &\nd=$!\necho \"$$ $d\" > \"$1/pids\"\ntrap 'kill -KILL $d; exit 143' TERM\nwhile :; do sleep 0.1; done\n",
        );
        exe(
            dir.join("bin/pw-cli"),
            &format!(
                "#!/bin/sh\nif [ \"$1\" = ls ]; then echo '  node.name = \"{DROP_SINK}\"'; exit 0; fi\necho $$ > \"$FAKE_DIR/holder.pid\"\ncat > /dev/null\nexec sleep 600\n"
            ),
        );
        Plan {
            manifest_sha256: TEST_SHA.to_owned(),
            dmabuf_probe: false,
            audio: Some(RunAudio {
                sink: Some(DROP_SINK.to_owned()),
            }),
            tag: "run-apk/drop".to_owned(),
            root: "run-apk/root".to_owned(),
            scratch: "run-apk/scratch".to_owned(),
            env: Vec::new(),
            session_env: Vec::new(),
            program: "@BASE@/out/cargo/atld".to_owned(),
            args: vec!["@BASE@/out/run-apk/scratch".to_owned()],
            files: Vec::new(),
            lib_dirs: Vec::new(),
            limit,
            display: Display::Headless,
            view: None,
            agent: None,
        }
        .script(&dir.display().to_string())
    }

    /// Starts `script` as the `local:` transport does (`sh -s`, the script then the open control pipe on stdin) and waits until the fake atld and the holder have written their pids.
    /// Returns the child, its stdin (the control pipe: drop it to cut the connection) and the pids (atld, daemon, holder).
    fn run_live(dir: &Path, script: &str) -> (Child, std::process::ChildStdin, [u32; 3]) {
        let mut child = Command::new("sh")
            .arg("-s")
            .env(
                "PATH",
                format!("{}:/usr/bin:/bin", dir.join("bin").display()),
            )
            .env("FAKE_DIR", dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut control = child.stdin.take().unwrap();
        control.write_all(script.as_bytes()).unwrap();
        let read = |name: &str| std::fs::read_to_string(dir.join(name)).ok();
        for _ in 0..100 {
            if let (Some(p), Some(h)) = (read("out/run-apk/scratch/pids"), read("holder.pid")) {
                let mut n = p
                    .split_whitespace()
                    .chain([h.trim()])
                    .map(|w| w.parse().unwrap());
                let pids = [n.next().unwrap(), n.next().unwrap(), n.next().unwrap()];
                return (child, control, pids);
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let _ = child.kill();
        let _ = child.wait();
        panic!("the fake run did not start in {}", dir.display());
    }

    /// Waits up to `s` seconds for `child` to exit; its status.
    fn exit_within(child: &mut Child, s: u64) -> ExitStatus {
        for _ in 0..s * 10 {
            if let Some(status) = child.try_wait().unwrap() {
                return status;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let _ = child.kill();
        let _ = child.wait();
        panic!("the run script did not end within {s} s");
    }

    /// A dropped connection mid-run (the control pipe closes; here the `local:` transport's stdin): the watchdog TERMs atld and the run ends the normal way, long before its 60 s limit:
    /// atld, its daemon and the sink holder gone, the end record written. The run did not end while the pipe stayed open.
    #[test]
    fn run_ends_when_its_control_pipe_closes() {
        let dir = std::env::temp_dir().join(format!("atl-remote-drop-{}", std::process::id()));
        let script = drop_fixture(&dir, 60);
        let (mut child, control, pids) = run_live(&dir, &script);
        std::thread::sleep(Duration::from_secs(1));
        assert!(
            child.try_wait().unwrap().is_none() && pids.iter().all(|&p| process_live(p)),
            "the run ended with its control pipe open"
        );
        drop(control);
        exit_within(&mut child, 5);
        // the holder goes once its timeout passes the kill on: allow it a moment
        std::thread::sleep(Duration::from_millis(500));
        let left: Vec<u32> = pids.into_iter().filter(|&p| process_live(p)).collect();
        assert!(left.is_empty(), "left after the drop: {left:?} of {pids:?}");
        let rc = std::fs::read_to_string(dir.join("out/run-apk/scratch/atld.rc")).unwrap();
        assert_ne!(rc.trim(), "124", "the limit, not the drop, ended the run");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The planned end of a bounded run (the limit, `atld.rc` 124) with the control pipe still open: the script ends by itself (it reads no more stdin), and the limit's TERM reached atld
    /// alone, which stopped its daemon in order; a TERM to the whole group would have killed the daemon first, read as "system process ... died during the run". A daemon that really dies
    /// mid-run is still atld's own verdict line (atld_run.rs `system_deaths`).
    #[test]
    fn hold_limit_terms_atld_alone() {
        let dir = std::env::temp_dir().join(format!("atl-remote-limit-{}", std::process::id()));
        let script = drop_fixture(&dir, 2);
        let (mut child, control, pids) = run_live(&dir, &script);
        exit_within(&mut child, 8);
        let rc = std::fs::read_to_string(dir.join("out/run-apk/scratch/atld.rc")).unwrap();
        assert_eq!(rc.trim(), "124");
        assert!(
            !dir.join("out/run-apk/scratch/daemon-term").exists(),
            "the limit's TERM reached the daemon"
        );
        std::thread::sleep(Duration::from_millis(500));
        let left: Vec<u32> = pids.into_iter().filter(|&p| process_live(p)).collect();
        assert!(
            left.is_empty(),
            "left after the limit: {left:?} of {pids:?}"
        );
        drop(control);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A session run: after the launcher check, the one `wayland-*` socket of the desktop session is found, and atld's environment names it. The count check is what refuses a second compositor's socket.
    #[test]
    fn session_run_names_the_targets_one_wayland_socket() {
        let plan = Plan {
            manifest_sha256: TEST_SHA.to_owned(),
            dmabuf_probe: false,
            audio: None,
            tag: "run-apk/app".to_owned(),
            root: "run-apk/root".to_owned(),
            scratch: "run-apk/scratch".to_owned(),
            env: vec![("RUST_LOG".to_owned(), "info".to_owned())],
            session_env: Vec::new(),
            program: "@BASE@/out/cargo/release/atld".to_owned(),
            args: vec!["--ui".to_owned(), "gtk".to_owned()],
            files: Vec::new(),
            lib_dirs: vec!["lib".to_owned()],
            limit: 300,
            display: Display::Session,
            view: None,
            agent: None,
        };
        let s = plan.script("atl-next-run/x3");
        let launcher = s.find("is not an executable").expect("launcher check");
        let discover = s.find("WL_DIR=\"/run/user/$(id -u)\"").expect("discovery");
        let env_line = s.find("env -i PATH=").expect("env line");
        assert!(launcher < discover && discover < env_line, "{s}");
        assert!(s.contains("for s in \"$WL_DIR\"/wayland-*; do\n"), "{s}");
        assert!(
            s.contains("case \"$s\" in *.lock) continue ;; esac\n"),
            "{s}"
        );
        assert!(s.contains("WL=${s##*/}\n"), "{s}");
        assert!(
            s.contains(
                "[ \"$WL_N\" -eq 1 ] || { echo \"remote run run-apk/app: $WL_N Wayland sockets"
            ),
            "{s}"
        );
        assert!(s.contains(" XDG_RUNTIME_DIR=\"/run/user/$(id -u)\" LD_LIBRARY_PATH=\"$BASE/lib\" WAYLAND_DISPLAY=\"$WL\" RUST_LOG=\"info\" setsid timeout --foreground -k 10 300 "), "{s}");
    }

    /// A nested run: labwc starts on the X11 session found read-only, the run waits for its socket, atld gets it, labwc ends after atld's group and its log rides in the tar. Break: a Session-mode script has no labwc and no X11 lookup.
    #[test]
    fn nested_run_starts_and_ends_labwc_around_atld() {
        let plan = Plan {
            manifest_sha256: TEST_SHA.to_owned(),
            dmabuf_probe: false,
            audio: None,
            tag: "app".to_owned(),
            root: "app/root".to_owned(),
            scratch: "app/scratch".to_owned(),
            env: Vec::new(),
            session_env: Vec::new(),
            program: "@BASE@/out/cargo/release/atld".to_owned(),
            args: vec!["--ui".to_owned(), "gtk".to_owned()],
            files: Vec::new(),
            lib_dirs: vec!["lib".to_owned()],
            limit: 60,
            display: Display::NestedX11,
            view: None,
            agent: None,
        };
        let s = plan.script("atl-next-run/x4-riscv64");
        let at = |needle: &str| {
            s.find(needle)
                .unwrap_or_else(|| panic!("{needle:?} in\n{s}"))
        };
        let x = at("for e in /proc/[0-9]*/environ; do");
        let start = at(
            "WLR_BACKENDS=x11 \"$LABWC\" -C \"$BASE/out/app/scratch/labwc\" < /dev/null > \"$BASE/out/app/scratch/labwc.log\" 2>&1 &\nLW=$!\n",
        );
        let wait = at("while wl_scan; [ \"$WL_N\" -eq 0 ]");
        let atld = at(" WAYLAND_DISPLAY=\"$WL\" setsid timeout --foreground -k 10 60 ");
        let group = at("kill -KILL -- \"-$pid\"");
        let end = at("kill -TERM \"$LW\"");
        assert!(
            x < start && start < wait && wait < atld && atld < group && group < end,
            "{s}"
        );
        assert!(
            s.ends_with(" 'app/scratch/cpuinfo' 'app/scratch/labwc.log'\nexit\n}\n"),
            "{s}"
        );
        let session = Plan {
            display: Display::Session,
            view: None,
            ..plan
        };
        let s = session.script("b");
        assert!(!s.contains("labwc") && !s.contains("/proc/[0-9]"), "{s}");
        // a plain session run (run-apk, no agent) maps its window: the D1 switch comes before atld, like the proxied agent's
        let switch = s.find("\nws_go \"$WS_RUN\"\n").expect(&s);
        let atld = s
            .find(" WAYLAND_DISPLAY=\"$WL\" setsid timeout --foreground -k 10 60 ")
            .expect(&s);
        assert!(
            s.find("trap ws_back EXIT\n").expect(&s) < switch && switch < atld,
            "{s}"
        );
    }

    /// A fake `hyprctl` for [`workspace_script`] under `sh`: state files in `dir` (`active`, `used`, `instances`, `form` = the printf format of one workspace block), `lua` = the classic
    /// dispatcher fails as under Hyprland 0.57's Lua config, `frozen` = no dispatch moves the session. Each call is appended to `calls`.
    fn fake_hypr(tag: &str, form: &str, active: u32, used: &[u32]) -> PathBuf {
        let dir = temp(&format!("ws-{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        let bin = dir.join("bin/hyprctl");
        atl_exec_fixture::write_exe(
            &bin,
            r#"#!/bin/sh
d="$FAKE_HYPR"
echo "$*" >> "$d/calls"
block() { printf "$(cat "$d/form")\n\n" "$1" "$1"; }
case "$1" in
instances) cat "$d/instances" ;;
activeworkspace) block "$(cat "$d/active")" ;;
workspaces) for n in $(cat "$d/used") $(cat "$d/active"); do block "$n"; done ;;
dispatch)
  [ -e "$d/frozen" ] && exit 0
  case "$2" in
  workspace) [ -e "$d/lua" ] && { echo "config error: syntax"; exit 1; }; echo "$3" > "$d/active" ;;
  hl.dsp.focus*) echo "$2" | tr -cd '0-9' > "$d/active" ;;
  esac ;;
esac
"#,
        );
        std::fs::write(dir.join("form"), form).unwrap();
        std::fs::write(dir.join("active"), format!("{active}\n")).unwrap();
        let used: Vec<String> = used.iter().map(u32::to_string).collect();
        std::fs::write(dir.join("used"), used.join("\n")).unwrap();
        // `hyprctl instances` as Hyprland prints it: one block per live instance; the run's socket is wayland-1, the other instance is not this session's
        std::fs::write(
            dir.join("instances"),
            "instance 9958d297641b5c84dcff93f9b1d6c5dd2c3a7a19_1763590000_1234567890:\n\ttime: 1763590000\n\tpid: 1234\n\twl socket: wayland-1\n\ninstance 0aa1_1763500000_42:\n\ttime: 1763500000\n\tpid: 999\n\twl socket: wayland-2\n\n",
        )
        .unwrap();
        dir
    }

    /// `hyprctl activeworkspace`/`workspaces` block forms: Hyprland 0.57.2 (the laptop, recorded 2026-11-22; title replaced) prints `workspace <n> (<name>) on monitor ...`, other releases
    /// `workspace ID <n> (<name>) ...`. The title line holds a decoy: a window title is free text.
    const WS_FORMS: [&str; 2] = [
        "workspace %s (%s) on monitor eDP-1:\n\tmonitorID: 0\n\twindows: 2\n\thasfullscreen: 0\n\tlastwindow: 0x55c231a7a420\n\tlastwindowtitle: workspace 9 (9) notes\n\tispersistent: 0\n\ttiledLayout: dwindle\n",
        "workspace ID %s (%s) on monitor eDP-1:\n\tmonitorID: 0\n\twindows: 2\n\thasfullscreen: 0\n\tlastwindow: 0x55d0c0a1b2c0\n\tlastwindowtitle: foot\n\n",
    ];

    /// [`workspace_script`] then `body` under `sh`, against the fake in `dir`; the run's socket is `wayland-1`.
    fn run_ws(dir: &Path, body: &str) -> Output {
        let script = format!(
            "WL_DIR={}\nWL=wayland-1\n{}{body}\n",
            sh_quote(&dir.display().to_string()),
            workspace_script("t")
        );
        let mut child = Command::new("sh")
            .arg("-s")
            .env(
                "PATH",
                format!("{}:/usr/bin:/bin", dir.join("bin").display()),
            )
            .env("FAKE_HYPR", dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(script.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    }

    fn active(dir: &Path) -> String {
        std::fs::read_to_string(dir.join("active"))
            .unwrap()
            .trim()
            .to_owned()
    }

    /// Waits up to 5 s for the guardian (1 s polls) to have finished: its marker `atl-ws-<pid>` lives until it has restored (or seen the trap's restore).
    fn guardian_done(dir: &Path) {
        for _ in 0..50 {
            let left = std::fs::read_dir(dir)
                .unwrap()
                .filter_map(|e| e.ok())
                .any(|e| e.file_name().to_string_lossy().starts_with("atl-ws-"));
            if !left {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        panic!(
            "the workspace guardian left its marker in {}",
            dir.display()
        );
    }

    /// Failure after the switch: the run was on the lowest unused workspace (used 1, 2; active 2 -> 3) and the trap restores 2; in both recorded forms.
    #[test]
    fn workspace_switch_restores_after_failure() {
        for (i, form) in WS_FORMS.iter().enumerate() {
            let dir = fake_hypr(&format!("fail{i}"), form, 2, &[1, 2]);
            let out = run_ws(
                &dir,
                "echo \"body on $(cat \"$FAKE_HYPR/active\")\" >&2\nexit 3",
            );
            let err = String::from_utf8_lossy(&out.stderr);
            assert_eq!(out.status.code(), Some(3), "{err}");
            assert!(
                err.contains("workspace: before=2 during=3 (empty 3)\nbody on 3\n"),
                "{err}"
            );
            assert!(err.contains("workspace: after=2 (restored 2)"), "{err}");
            assert_eq!(active(&dir), "2");
            guardian_done(&dir);
            assert_eq!(
                active(&dir),
                "2",
                "the guardian must not move the session again"
            );
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    /// SIGKILL of the run script (no trap runs; a dropped ssh the shell died of looks the same): the guardian restores.
    #[test]
    fn workspace_guardian_restores_after_kill() {
        let dir = fake_hypr("kill", WS_FORMS[0], 1, &[1]);
        let out = run_ws(&dir, "kill -KILL $$");
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(
            err.contains("during=2 (empty 2)") && !err.contains("after="),
            "{err}"
        );
        guardian_done(&dir);
        assert_eq!(active(&dir), "1");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Hyprland 0.57's Lua config: the classic dispatcher fails, the Lua one moves the session, both ways.
    #[test]
    fn workspace_switch_takes_the_lua_dispatcher() {
        let dir = fake_hypr("lua", WS_FORMS[0], 1, &[1, 2]);
        std::fs::write(dir.join("lua"), "").unwrap();
        let out = run_ws(&dir, "true");
        let err = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(0), "{err}");
        assert!(
            err.contains("during=3 (empty 3)") && err.contains("after=1 (restored 1)"),
            "{err}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// No switch possible -> refused before the body (atld) runs, exit [`WS_REFUSED_RC`], the session untouched: no Hyprland instance on the run's socket; a session that does not move.
    #[test]
    fn workspace_switch_refuses_rather_than_run_in_place() {
        let body = "touch \"$FAKE_HYPR/ran\"";
        let dir = fake_hypr("none", WS_FORMS[0], 2, &[2]);
        std::fs::write(
            dir.join("instances"),
            "instance x_1_2:\n\ttime: 1\n\tpid: 2\n\twl socket: wayland-2\n\n",
        )
        .unwrap();
        let out = run_ws(&dir, body);
        let err = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(WS_REFUSED_RC), "{err}");
        assert!(
            err.contains("workspace: refused (0 Hyprland instances on wayland-1, want 1)"),
            "{err}"
        );
        assert!(!dir.join("ran").exists() && active(&dir) == "2");
        let calls = std::fs::read_to_string(dir.join("calls")).unwrap();
        assert!(!calls.contains("dispatch"), "{calls}");
        let _ = std::fs::remove_dir_all(&dir);

        let dir = fake_hypr("frozen", WS_FORMS[1], 2, &[2]);
        std::fs::write(dir.join("frozen"), "").unwrap();
        let out = run_ws(&dir, body);
        let err = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(WS_REFUSED_RC), "{err}");
        assert!(
            err.contains("workspace: refused (the session stayed on 2, not empty 1)"),
            "{err}"
        );
        assert!(!dir.join("ran").exists() && active(&dir) == "2");
        guardian_done(&dir);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A window agent run on the session display: the workspace switch (its restore trapped on exit) and the agent come before atld, the run waits for the agent's socket, atld's display is that
    /// socket (never the session's), its pid goes to the agent, the agent's exit is recorded after atld's, and its files ride in the tar. Headless agent (frames): no proxy, no switch, no socket wait.
    #[test]
    fn agent_run_switches_workspace_and_proxies_atld() {
        let plan = Plan {
            manifest_sha256: TEST_SHA.to_owned(),
            dmabuf_probe: false,
            audio: None,
            tag: "window".to_owned(),
            root: "window/root".to_owned(),
            scratch: "window/scratch".to_owned(),
            env: Vec::new(),
            session_env: Vec::new(),
            program: "@BASE@/out/cargo/release/atld".to_owned(),
            args: vec!["--ui".to_owned(), "gtk".to_owned()],
            files: Vec::new(),
            lib_dirs: vec!["lib".to_owned()],
            limit: 240,
            display: Display::Session,
            view: None,
            agent: Some(Agent {
                program: PathBuf::from("@BASE@/out/cargo/debug/xtask-harness"),
                args: vec!["window-live".to_owned()],
                window: vec!["--stage".to_owned(), "widgets".to_owned()],
                proxy: true,
            }),
        };
        let s = plan.script("atl-next-run/x5");
        let at = |needle: &str| {
            s.find(needle)
                .unwrap_or_else(|| panic!("{needle:?} in\n{s}"))
        };
        let session = at("[ \"$WL_N\" -eq 1 ] ||");
        let trap = at("trap ws_back EXIT\n");
        let switch = at("\nws_go \"$WS_RUN\"\n");
        let agent = at(
            " timeout -k 10 300 \"$BASE/out/cargo/debug/xtask-harness\" \"window-live\" --upstream \"$WL_DIR/$WL\" --listen \"$GATE_SOCK\" -- \"--stage\" \"widgets\" < /dev/null > \"$BASE/out/window/scratch/agent.log\" 2>&1 &\nAG=$!\n",
        );
        let wait = at("while [ ! -S \"$GATE_SOCK\" ]");
        let atld = at(" WAYLAND_DISPLAY=\"$GATE_SOCK\" setsid timeout --foreground -k 10 240 ");
        let pid = at("printf '%s\\n' \"$pid\" > \"$BASE/out/window/scratch/atld.pid\"\n");
        let agent_rc =
            at("wait \"$AG\"\nprintf '%s\\n' \"$?\" > \"$BASE/out/window/scratch/agent.rc\"\n");
        assert!(
            session < trap
                && trap < switch
                && switch < agent
                && agent < wait
                && wait < atld
                && atld < pid
                && pid < agent_rc,
            "{s}"
        );
        assert!(!s.contains("WAYLAND_DISPLAY=\"$WL\""), "{s}");
        assert!(
            s.ends_with(
                " 'window/scratch/live.json' 'window/scratch/agent.rc' 'window/scratch/agent.log'\nexit\n}\n"
            ),
            "{s}"
        );
        let headless = Plan {
            display: Display::Headless,
            view: None,
            agent: plan.agent.clone().map(|a| Agent { proxy: false, ..a }),
            ..plan
        };
        let s = headless.script("b");
        assert!(
            !s.contains("hyprctl") && !s.contains("--upstream") && !s.contains("WAYLAND_DISPLAY"),
            "{s}"
        );
        assert!(s.contains("AG=$!\n") && s.contains("agent.rc"), "{s}");
    }

    /// One Wayland socket: a Hyprland instance on it -> the session (its workspace switches); without one, labwc -> the run's own headless compositor, never the session; neither -> the session, which the run
    /// script refuses ([`workspace_script`]). PATH holds only the fake's dir and the two tools the probe pipes through.
    #[test]
    fn session_without_a_workspace_switch_gets_its_own_compositor() {
        let dir = fake_hypr("probe", WS_FORMS[0], 1, &[1]);
        let bin = dir.join("bin");
        for tool in ["awk", "grep", "cat"] {
            std::os::unix::fs::symlink(format!("/usr/bin/{tool}"), bin.join(tool)).unwrap();
        }
        let ask = |wl: &str| {
            let script = format!(
                "WL_DIR={}\nWL={wl}\n{}",
                sh_quote(&dir.display().to_string()),
                session_probe_sh()
            );
            let out = Command::new("/bin/sh")
                .args(["-c", &script])
                .env("PATH", &bin)
                .env("FAKE_HYPR", &dir)
                .output()
                .unwrap();
            parse_probe(&String::from_utf8(out.stdout).unwrap()).unwrap()
        };
        assert_eq!(ask("wayland-1").0, Display::Session);
        assert_eq!(ask("wayland-0").0, Display::Session, "no labwc to start");
        std::fs::copy(bin.join("hyprctl"), bin.join("labwc")).unwrap();
        assert_eq!(ask("wayland-1").0, Display::Session);
        let (display, why) = ask("wayland-0");
        assert_eq!(display, Display::NestedHeadless);
        assert!(why.contains("0 Hyprland instances on wayland-0"), "{why}");
        std::fs::remove_file(bin.join("hyprctl")).unwrap();
        let (display, why) = ask("wayland-0");
        assert_eq!(display, Display::NestedHeadless);
        assert!(why.contains("no hyprctl on PATH"), "{why}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A run with its own headless compositor: labwc starts in a runtime dir of its own after its cleanup is trapped, atld gets that socket's absolute path, labwc ends after atld and its log rides in the tar; no session lookup.
    #[test]
    fn nested_headless_run_owns_its_compositor() {
        let plan = Plan {
            manifest_sha256: TEST_SHA.to_owned(),
            dmabuf_probe: false,
            audio: None,
            tag: "app".to_owned(),
            root: "app/root".to_owned(),
            scratch: "app/scratch".to_owned(),
            env: Vec::new(),
            session_env: Vec::new(),
            program: "@BASE@/out/cargo/release/atld".to_owned(),
            args: vec!["--ui".to_owned(), "gtk".to_owned()],
            files: Vec::new(),
            lib_dirs: vec!["lib".to_owned()],
            limit: 60,
            display: Display::NestedHeadless,
            view: None,
            agent: None,
        };
        let s = plan.script("atl-next-run/x");
        let at = |needle: &str| {
            s.find(needle)
                .unwrap_or_else(|| panic!("{needle:?} in\n{s}"))
        };
        assert!(
            !s.contains("hyprctl") && !s.contains("DISPLAY=\"$XD\""),
            "{s}"
        );
        // wlroots picks the renderer on the target: pixman there broke the app's EGL (rvvm, 2026-11-23)
        assert!(!s.contains("WLR_RENDERER"), "{s}");
        assert!(
            at("trap nest_end EXIT\n")
                < at(
                    "XDG_RUNTIME_DIR=\"$NEST_DIR\" WLR_BACKENDS=headless WLR_HEADLESS_OUTPUTS=1 WLR_LIBINPUT_NO_DEVICES=1 \"$@\" \"$LABWC\" -C"
                )
                && at("\"$LABWC\" -C") < at("\n  WL=\"$NEST_DIR/$WL\"\n")
                && at("\n  WL=\"$NEST_DIR/$WL\"\n")
                    < at(" WAYLAND_DISPLAY=\"$WL\" setsid timeout ")
                && at(" WAYLAND_DISPLAY=\"$WL\" setsid timeout ")
                    < at("kill -TERM \"$LW\" 2>/dev/null || true"),
            "{s}"
        );
        assert!(s.contains(" 'app/scratch/labwc.log'"), "{s}");
        // the labwc setup helper: started on the run's socket once labwc listens, ready before atld, stopped before labwc ends; its log comes back with the run
        assert!(
            at("\n  WL=\"$NEST_DIR/$WL\"\n")
                < at(
                    "\"$BASE/out/cargo/release/atl-labwc-setup\" \"$WL\" >> \"$BASE/out/app/scratch/labwc-setup.log\" 2>&1 < /dev/null &\n  HS=$!\n"
                )
                && at("HS=$!\n") < at("grep -qx ")
                && at("grep -qx ") < at(" WAYLAND_DISPLAY=\"$WL\" setsid timeout ")
                && at("kill -TERM \"$HS\" 2>/dev/null || true")
                    < at("kill -TERM \"$LW\" 2>/dev/null || true"),
            "{s}"
        );
        assert!(s.contains(" 'app/scratch/labwc-setup.log'"), "{s}");
        let cross = Plan {
            program: "@BASE@/out/cargo/riscv64gc-unknown-linux-gnu/release/atld".to_owned(),
            ..plan
        };
        assert!(
            cross.script("atl-next-run/x").contains(
                "\"$BASE/out/cargo/riscv64gc-unknown-linux-gnu/release/atl-labwc-setup\" \"$WL\" >> "
            ),
            "the helper is not beside the cross host's atld"
        );
    }

    /// The dmabuf probe: asked after the labwc setup is ready and before atld; a refusal line stops the run, its why in [`HOST_LIMIT_FILE`] (sent back with the display's logs, read as [`HostLimit`]);
    /// any other line goes on. Under [`Break::TrustDmabuf`] (`dmabuf_probe` false) the run never asks.
    #[test]
    fn dmabuf_refusal_is_a_host_limit_before_atld() {
        let plan = Plan {
            manifest_sha256: TEST_SHA.to_owned(),
            dmabuf_probe: true,
            audio: None,
            tag: "app".to_owned(),
            root: "app/root".to_owned(),
            scratch: "app/scratch".to_owned(),
            env: Vec::new(),
            session_env: Vec::new(),
            program: "@BASE@/out/cargo/release/atld".to_owned(),
            args: Vec::new(),
            files: Vec::new(),
            lib_dirs: vec!["lib".to_owned()],
            limit: 60,
            display: Display::NestedHeadless,
            view: None,
            agent: None,
        };
        let s = plan.script("atl-next-run/x");
        let at = |needle: &str| {
            s.find(needle)
                .unwrap_or_else(|| panic!("{needle:?} in\n{s}"))
        };
        assert!(
            at("grep -qx ")
                < at("\"$BASE/out/cargo/release/atl-labwc-setup\" --dmabuf \"$WL\" >> ")
                && at(" --dmabuf ") < at("| tee \"$BASE/out/app/scratch/host.limit\" >&2\n")
                && at("tee \"$BASE") < at(" 'app/scratch/host.limit'\n  exit 5\n")
                && at("host.limit'\n  exit 5\n") < at(" setsid timeout "),
            "{s}"
        );
        let trusted = Plan {
            dmabuf_probe: false,
            ..plan
        };
        assert!(!trusted.script("atl-next-run/x").contains("--dmabuf"));

        let dir = temp("dmabuf");
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("setup.log");
        let run = |line: &str| {
            let setup = dir.join("setup");
            std::fs::write(&setup, format!("#!/bin/sh\necho '{line}'\n")).unwrap();
            std::fs::set_permissions(&setup, std::os::unix::fs::PermissionsExt::from_mode(0o755))
                .unwrap();
            let _ = std::fs::remove_file(&log);
            let _ = std::fs::remove_file(dir.join(HOST_LIMIT_FILE));
            let script = format!(
                "WL=/x\n{}echo went on\n",
                dmabuf_probe_sh(
                    "app",
                    &sh_quote(&setup.display().to_string()),
                    &sh_quote(&log.display().to_string()),
                    &sh_quote(&dir.join(HOST_LIMIT_FILE).display().to_string()),
                    "echo logs"
                )
            );
            Command::new("/bin/sh")
                .args(["-c", &script])
                .output()
                .unwrap()
        };
        let refused = run("dmabuf: refused 64x64 fourcc 0x34325241 stride 256 modifier 0x0");
        assert_eq!(refused.status.code(), Some(5));
        let stderr = String::from_utf8_lossy(&refused.stderr).into_owned();
        let want = "remote run app: host limit: the compositor imports no client dmabuf (dmabuf: refused 64x64";
        assert!(stderr.contains(want), "{stderr}");
        assert_eq!(String::from_utf8_lossy(&refused.stdout), "logs\n");
        // the file the tar brings back is what the host reads
        let limit = host_limit(&dir).expect("a host limit");
        assert!(limit.0.starts_with(want), "{limit}");
        for line in [
            "dmabuf: imported 64x64",
            "dmabuf: not probed: no render node in /dev/dri",
        ] {
            let out = run(line);
            assert!(out.status.success(), "{line}");
            assert_eq!(String::from_utf8_lossy(&out.stdout), "went on\n", "{line}");
            assert!(host_limit(&dir).is_none(), "{line}");
        }

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// `--view` serves the run's own labwc on the target's loopback only: wayvnc on its socket after labwc is up and before atld, ended before the labwc it shows and by the EXIT trap on any other exit; its log rides in the tar.
    #[test]
    fn view_serves_the_run_labwc_on_loopback_and_ends_with_it() {
        let plan = |view: Option<u16>| Plan {
            manifest_sha256: TEST_SHA.to_owned(),
            dmabuf_probe: false,
            audio: None,
            tag: "app".to_owned(),
            root: "app/root".to_owned(),
            scratch: "app/scratch".to_owned(),
            env: Vec::new(),
            session_env: Vec::new(),
            program: "@BASE@/out/cargo/release/atld".to_owned(),
            args: vec!["--ui".to_owned(), "gtk".to_owned()],
            files: Vec::new(),
            lib_dirs: vec!["lib".to_owned()],
            limit: 60,
            display: Display::NestedHeadless,
            view,
            agent: None,
        };
        let s = plan(Some(5900)).script("atl-next-run/x");
        let at = |needle: &str| {
            s.find(needle)
                .unwrap_or_else(|| panic!("{needle:?} in\n{s}"))
        };
        // the loopback only: another address would reach the port without the ssh tunnel
        assert!(s.contains("\"$VNC\" 127.0.0.1 5900 < /dev/null > "), "{s}");
        assert!(!s.contains("0.0.0.0"), "{s}");
        assert!(
            at("\n  WL=\"$NEST_DIR/$WL\"\n") < at("\"$VNC\" 127.0.0.1 5900")
                && at("\"$VNC\" 127.0.0.1 5900") < at(" WAYLAND_DISPLAY=\"$WL\" setsid timeout "),
            "{s}"
        );
        // ended with the run: before the labwc it shows goes, and by the EXIT trap on every other exit
        assert!(
            at("wait \"$VN\" 2>/dev/null || true") < at("kill -TERM \"$LW\" 2>/dev/null || true"),
            "{s}"
        );
        assert!(
            s.contains("trap 'kill -TERM \"$VN\" 2>/dev/null; nest_end' EXIT\n"),
            "{s}"
        );
        assert!(s.contains(" 'app/scratch/wayvnc.log'"), "{s}");
        let none = plan(None).script("atl-next-run/x");
        assert!(!none.contains("wayvnc") && !none.contains("$VN"), "{none}");
    }

    /// `--view` refuses by name when the target has no wayvnc on its PATH; with one, the check passes. The probe runs here with a PATH of the test's own.
    #[test]
    fn view_check_refuses_a_target_without_wayvnc() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp("view-check");
        let _ = std::fs::remove_dir_all(&dir);
        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let ask = || {
            let out = Command::new("/bin/sh")
                .args(["-c", &view_probe_script()])
                .env("PATH", &bin)
                .output()
                .unwrap();
            parse_view_probe(&String::from_utf8_lossy(&out.stdout))
        };
        let err = format!("{:#}", ask().unwrap_err());
        assert!(err.contains("wayvnc is not on PATH"), "{err}");
        let fake = bin.join("wayvnc");
        std::fs::write(&fake, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(ask().is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The run-apk probe by capability ([`probe_script_at`] over a fake runtime dir with real sockets, fake environ files and an X11 socket dir; PATH holds a labwc or nothing). Expected values
    /// from the decision text (2026-11-25), not from the script: no session and labwc -> the run's own headless labwc; no labwc -> headless; an X11 session -> labwc nested in it; a session socket keeps its answer.
    #[test]
    fn run_probe_maps_on_no_session_to_its_own_labwc() {
        let case =
            |tag: &str, sockets: &[u32], labwc: bool, x11: Option<u32>| -> (Display, String) {
                let dir = temp(&format!("run-probe-{tag}"));
                let _ = std::fs::remove_dir_all(&dir);
                let (run, bin, procs, xdir) = (
                    dir.join("run"),
                    dir.join("bin"),
                    dir.join("proc"),
                    dir.join("x11"),
                );
                for d in [&run, &bin, &procs, &xdir] {
                    std::fs::create_dir_all(d).unwrap();
                }
                // the X11 read pipes through these; PATH holds no other tool, so a labwc on this host cannot answer
                for tool in ["tr", "sed", "head"] {
                    std::os::unix::fs::symlink(format!("/usr/bin/{tool}"), bin.join(tool)).unwrap();
                }
                // bound for the probe: `[ -S ]` sees a socket file
                let _sockets: Vec<std::os::unix::net::UnixListener> = sockets
                    .iter()
                    .map(|n| {
                        std::os::unix::net::UnixListener::bind(run.join(format!("wayland-{n}")))
                            .unwrap()
                    })
                    .collect();
                if labwc {
                    use std::os::unix::fs::PermissionsExt;
                    let fake = bin.join("labwc");
                    std::fs::write(&fake, "#!/bin/sh\nexit 0\n").unwrap();
                    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755))
                        .unwrap();
                }
                // one process naming DISPLAY=:n, and the socket that display names
                let _x11: Option<std::os::unix::net::UnixListener> = x11.map(|n| {
                    let p = procs.join("1");
                    std::fs::create_dir_all(&p).unwrap();
                    std::fs::write(p.join("environ"), format!("DISPLAY=:{n}\0")).unwrap();
                    std::os::unix::net::UnixListener::bind(xdir.join(format!("X{n}"))).unwrap()
                });
                let script = probe_script_at(
                    &sh_quote(&run.display().to_string()),
                    &format!("{}/*/environ", procs.display()),
                    &xdir.display().to_string(),
                );
                let out = Command::new("/bin/sh")
                    .args(["-c", &script])
                    .env("PATH", &bin)
                    .output()
                    .unwrap();
                let _ = std::fs::remove_dir_all(&dir);
                parse_probe(&String::from_utf8(out.stdout).unwrap()).unwrap()
            };
        // no session, labwc on PATH: the run's own headless labwc (decision 2026-11-25)
        let (display, why) = case("bare", &[], true, None);
        assert_eq!(display, Display::NestedHeadless);
        assert!(why.contains("no session"), "{why}");
        // no session, no labwc: headless, the reason names the missing labwc
        let (display, why) = case("none", &[], false, None);
        assert_eq!(display, Display::Headless);
        assert!(why.contains("no labwc"), "{why}");
        // an X11 session: labwc nested in it, as before
        assert_eq!(
            case("x11", &[], true, Some(7)),
            (
                Display::NestedX11,
                "labwc nested in X11 display :7".to_owned()
            )
        );
        assert_eq!(
            case("x11-no-labwc", &[], false, Some(7)).0,
            Display::Headless
        );
        // a session socket without labwc: the session, as before
        assert_eq!(case("session", &[1], false, None).0, Display::Session);
        // two sockets: refused
        assert_eq!(case("two", &[1, 2], true, None).0, Display::Headless);
    }

    /// The probe's answers; anything else is an error, not a mode.
    #[test]
    fn display_probe_answers() {
        assert_eq!(parse_probe("session\n").unwrap().0, Display::Session);
        let (d, why) = parse_probe("nested :1\n").unwrap();
        assert_eq!(
            (d, why.as_str()),
            (Display::NestedX11, "labwc nested in X11 display :1")
        );
        let (d, why) = parse_probe("headless no labwc\n").unwrap();
        assert_eq!((d, why.as_str()), (Display::Headless, "no labwc"));
        assert!(parse_probe("").is_err());
    }

    /// The probe on this host (`local:`) answers one of the three modes: the script runs as a whole under sh.
    #[test]
    fn display_probe_runs_under_sh() {
        let host = Host::Local(std::env::temp_dir());
        parse_probe(&host.run_script(&probe_script()).unwrap()).unwrap();
    }

    /// The test run's probe by capability ([`test_probe_script`] on a fake runtime dir with real sockets; PATH holds a labwc or nothing; DISPLAY is set, and X11 is not read). Expected values from the
    /// decision text (2026-11-23), not from the script: labwc present -> the run's own headless labwc whatever the session; no labwc -> the session's one socket, else headless.
    #[test]
    fn test_run_probe_answers_by_capability() {
        let case = |tag: &str, sockets: &[u32], labwc: bool| -> (Display, String) {
            let dir = temp(&format!("test-probe-{tag}"));
            let _ = std::fs::remove_dir_all(&dir);
            let (run, bin) = (dir.join("run"), dir.join("bin"));
            std::fs::create_dir_all(&run).unwrap();
            std::fs::create_dir_all(&bin).unwrap();
            // bound for the probe: `[ -S ]` sees a socket file
            let _sockets: Vec<std::os::unix::net::UnixListener> = sockets
                .iter()
                .map(|n| {
                    std::os::unix::net::UnixListener::bind(run.join(format!("wayland-{n}")))
                        .unwrap()
                })
                .collect();
            if labwc {
                use std::os::unix::fs::PermissionsExt;
                let fake = bin.join("labwc");
                std::fs::write(&fake, "#!/bin/sh\nexit 0\n").unwrap();
                std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
            let out = Command::new("/bin/sh")
                .args([
                    "-c",
                    &test_probe_script(&sh_quote(&run.display().to_string())),
                ])
                .env("PATH", &bin)
                .env("DISPLAY", ":1")
                .output()
                .unwrap();
            let _ = std::fs::remove_dir_all(&dir);
            parse_probe(&String::from_utf8(out.stdout).unwrap()).unwrap()
        };
        let (display, why) = case("session-labwc", &[1], true);
        assert_eq!(display, Display::NestedHeadless);
        assert!(why.contains("1 Wayland socket"), "{why}");
        assert_eq!(case("session", &[1], false).0, Display::Session);
        let (display, why) = case("labwc", &[], true);
        assert_eq!(display, Display::NestedHeadless);
        assert!(why.contains("0 Wayland socket"), "{why}");
        assert_eq!(case("nothing", &[], false).0, Display::Headless);
        assert_eq!(case("two", &[1, 2], false).0, Display::Headless);
        assert_eq!(case("two-labwc", &[1, 2], true).0, Display::NestedHeadless);
    }

    /// The test run's probe on this host (`local:`) answers one of the modes, as [`display_probe_runs_under_sh`] does for run-apk's.
    #[test]
    fn test_probe_runs_under_sh() {
        let host = Host::Local(std::env::temp_dir());
        parse_probe(&host.run_script(&test_probe_script(SESSION_DIR)).unwrap()).unwrap();
    }

    /// A nested labwc's config is a run file: its dir made, then rc.xml written (the local gate's text, [`nested_wayland::RC_XML`]), both before the script starts labwc. No other display sends one.
    #[test]
    fn nested_labwc_config_is_a_run_file() {
        let files = display_files(Display::NestedHeadless, "app/scratch");
        assert_eq!(
            files,
            vec![(
                "@BASE@/out/app/scratch/labwc/rc.xml".to_owned(),
                nested_wayland::RC_XML.to_owned()
            )]
        );
        assert!(display_files(Display::Session, "app/scratch").is_empty());
        assert!(display_files(Display::Headless, "app/scratch").is_empty());
        let plan = Plan {
            manifest_sha256: TEST_SHA.to_owned(),
            dmabuf_probe: false,
            audio: None,
            tag: "app".to_owned(),
            root: "app/root".to_owned(),
            scratch: "app/scratch".to_owned(),
            env: Vec::new(),
            session_env: Vec::new(),
            program: "@BASE@/out/cargo/release/atld".to_owned(),
            args: Vec::new(),
            files,
            lib_dirs: Vec::new(),
            limit: 60,
            display: Display::NestedHeadless,
            view: None,
            agent: None,
        };
        let s = plan.script("atl-next-run/x");
        let at = |needle: &str| {
            s.find(needle)
                .unwrap_or_else(|| panic!("{needle:?} in\n{s}"))
        };
        assert!(
            at("mkdir -p \"$BASE/out/app/scratch\" \"$BASE/out/app/scratch/labwc\"\n")
                < at("> \"$BASE/out/app/scratch/labwc/rc.xml\""),
            "{s}"
        );
        assert!(
            at("> \"$BASE/out/app/scratch/labwc/rc.xml\"")
                < at("\"$LABWC\" -C \"$BASE/out/app/scratch/labwc\""),
            "{s}"
        );
    }

    /// A run without a session has no discovery: its environment names no Wayland socket.
    #[test]
    fn plain_run_has_no_session_discovery() {
        let plan = Plan {
            manifest_sha256: TEST_SHA.to_owned(),
            dmabuf_probe: false,
            audio: None,
            tag: "gl/run".to_owned(),
            root: "gl/run/root".to_owned(),
            scratch: "gl/run/scratch".to_owned(),
            env: Vec::new(),
            session_env: Vec::new(),
            program: "@BASE@/out/cargo/release/atld".to_owned(),
            args: Vec::new(),
            files: Vec::new(),
            lib_dirs: Vec::new(),
            limit: 120,
            display: Display::Headless,
            view: None,
            agent: None,
        };
        let s = plan.script("b");
        assert!(!s.contains("WL_DIR"), "{s}");
        assert!(!s.contains("WAYLAND_DISPLAY"), "{s}");
    }

    /// A record of one out dir file, one out dir tree holding a link to itself (`com.android.art -> .`, the ART staging's alias) and one AOSP file: the names, `lib/` from the LLVM runtimes, the link kept as a link in the stage, and the break removing exactly `lib/libunwind.so.1`.
    #[test]
    fn bundle_lists_record_entries_then_lib_and_the_break_drops_libunwind() {
        let dir = temp("bundle");
        let out = dir.join("build");
        let tree = dir.join("tree");
        let art = out.join("gl-art");
        std::fs::create_dir_all(art.join("lib64")).unwrap();
        std::fs::write(art.join("lib64/a.so"), b"x").unwrap();
        std::os::unix::fs::symlink(".", art.join("com.android.art")).unwrap();
        std::fs::create_dir_all(out.join("device")).unwrap();
        std::fs::write(out.join("device/a.so"), b"x").unwrap();
        let programs = run_programs(&out.join("cargo/release"));
        // a file link to an out dir file: bundled as that file's content, its absolute target not kept
        std::os::unix::fs::symlink(out.join("device/a.so"), art.join("lib64/b.so")).unwrap();
        std::fs::create_dir_all(&tree).unwrap();
        std::fs::write(tree.join("x.kcm"), b"x").unwrap();
        // a record file that is a symlink (an out dir's libglapi.so.0 is one): bundled from the real file
        std::os::unix::fs::symlink("a.so", out.join("device/link.so")).unwrap();
        let mut files = vec![
            out.join("device/a.so"),
            out.join("device/link.so"),
            tree.join("x.kcm"),
        ];
        files.extend(programs);
        let inputs = Inputs {
            files,
            dirs: vec![art],
        };
        write_record(&out, "gl", &out, &tree, Path::new(LLVM_LIB_DIR), &inputs).unwrap();
        let full = Bundle::of("gl", &out, None, None).unwrap();
        assert_eq!(
            names(&full),
            [
                "lib/libc++.so.1",
                "lib/libc++abi.so.1",
                "lib/libunwind.so.1",
                "out/cargo/release/atl-labwc-setup",
                "out/cargo/release/atld",
                "out/device/a.so",
                "out/device/link.so",
                "out/gl-art/com.android.art",
                "out/gl-art/lib64/a.so",
                "out/gl-art/lib64/b.so",
                "src/aosp/x.kcm",
            ]
        );
        let alias = full
            .entries
            .iter()
            .find(|e| e.name == "out/gl-art/com.android.art")
            .unwrap();
        assert_eq!(alias.kind, Kind::Link(".".to_owned()));
        let file_link = full
            .entries
            .iter()
            .find(|e| e.name == "out/gl-art/lib64/b.so")
            .unwrap();
        assert_eq!(file_link.kind, Kind::File);
        assert_eq!(
            file_link.src,
            std::fs::canonicalize(out.join("device/a.so")).unwrap()
        );
        let record_link = full
            .entries
            .iter()
            .find(|e| e.name == "out/device/link.so")
            .unwrap();
        assert_eq!(
            record_link.src,
            std::fs::canonicalize(out.join("device/a.so")).unwrap()
        );
        let broken = Bundle::of("gl", &out, None, Some(Break::DropLibunwind)).unwrap();
        assert_eq!(names(&broken).len(), names(&full).len() - 1);
        assert!(!names(&broken).contains(&LIBUNWIND.to_owned()));
        let stage = dir.join("stage");
        full.stage(&stage).unwrap();
        assert_eq!(
            std::fs::read_link(stage.join("out/gl-art/com.android.art")).unwrap(),
            PathBuf::from(".")
        );
        let manifest = std::fs::read_to_string(stage.join(MANIFEST)).unwrap();
        assert!(manifest.contains("link  .  out/gl-art/com.android.art\n"));
        let staged = stage.join("out/gl-art/lib64/b.so");
        assert!(std::fs::symlink_metadata(&staged).unwrap().is_file());
        assert_eq!(std::fs::read(&staged).unwrap(), b"x");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// [`RUN_PROGRAMS`] as files of the release dir `release`.
    fn run_programs(release: &Path) -> Vec<PathBuf> {
        std::fs::create_dir_all(release).unwrap();
        RUN_PROGRAMS
            .iter()
            .map(|p| {
                let f = release.join(p);
                std::fs::write(&f, b"x").unwrap();
                f
            })
            .collect()
    }

    /// A cross out dir's record without a program the run starts is refused by name, with the command that records again for that host, even when the file is there: the record names the products. Break: a bundle that adds the file itself passes the second record.
    #[test]
    fn a_cross_record_without_a_run_program_is_refused() {
        let dir = temp("no-program");
        let out = dir.join("build/riscv64");
        let tree = dir.join("tree");
        std::fs::create_dir_all(&tree).unwrap();
        let release = out.join("cargo/riscv64gc-unknown-linux-gnu/release");
        let programs = run_programs(&release);
        let bundle = |files: &[PathBuf], brk| {
            let inputs = Inputs {
                files: files.to_vec(),
                dirs: vec![],
            };
            write_record(&out, "app", &out, &tree, Path::new(LLVM_LIB_DIR), &inputs).unwrap();
            Bundle::of("app", &out, Some("riscv64"), brk)
        };
        let full = bundle(&programs, None).unwrap();
        let entry = |p: &str| format!("out/cargo/riscv64gc-unknown-linux-gnu/release/{p}");
        assert!(names(&full).contains(&entry("atl-labwc-setup")));
        // drop-atld takes the cross host's atld
        let dropped = bundle(&programs, Some(Break::DropAtld)).unwrap();
        assert!(!names(&dropped).contains(&entry("atld")));
        for missing in RUN_PROGRAMS {
            let without: Vec<PathBuf> = programs
                .iter()
                .filter(|p| !p.ends_with(missing))
                .cloned()
                .collect();
            let refused = bundle(&without, None).err().expect("refused").to_string();
            assert!(
                refused.contains(&format!("its record names no {missing},")),
                "{refused}"
            );
            assert!(
                refused.contains("cargo xtask remote test app --host <alias> --arch riscv64"),
                "{refused}"
            );
        }
        // named, but gone from the build dir
        std::fs::remove_file(release.join("atl-labwc-setup")).unwrap();
        let gone = bundle(&programs, None).err().expect("refused").to_string();
        assert!(
            gone.contains("atl-labwc-setup but it is not a file"),
            "{gone}"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The dry run's first line: entry and link counts, the `lib/` count, and the runtimes it names.
    #[test]
    fn summary_counts_entries_links_and_lib() {
        let bundle = Bundle {
            gate: "gl".to_owned(),
            entries: vec![
                Entry {
                    name: "lib/libc++.so.1".to_owned(),
                    src: PathBuf::from("/nonexistent/lib"),
                    kind: Kind::File,
                },
                Entry {
                    name: "out/gl-art/com.android.art".to_owned(),
                    src: PathBuf::from("/nonexistent/link"),
                    kind: Kind::Link(".".to_owned()),
                },
            ],
        };
        assert_eq!(
            bundle.summary(),
            "bundle gl: 2 entries (1 links), 0.0 MiB summed over file entries, 1 in lib/: libc++.so.1 libc++abi.so.1 libunwind.so.1"
        );
    }

    #[test]
    fn manifest_stages_hard_links_with_sha256_and_size() {
        let dir = temp("manifest");
        let src = dir.join("src");
        std::fs::create_dir_all(&src).unwrap();
        let file = src.join("a.so");
        std::fs::write(&file, b"abc").unwrap();
        let bundle = Bundle {
            gate: "t".to_owned(),
            entries: vec![Entry {
                name: "out/device/a.so".to_owned(),
                src: file,
                kind: Kind::File,
            }],
        };
        let stage = dir.join("stage");
        bundle.stage(&stage).unwrap();
        assert_eq!(
            std::fs::read(stage.join("out/device/a.so")).unwrap(),
            b"abc"
        );
        let manifest = std::fs::read_to_string(stage.join(MANIFEST)).unwrap();
        // sha256("abc") from FIPS 180-2
        assert_eq!(
            manifest,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad  3  out/device/a.so\n"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Two names of one source arrive as one file (one inode) on the target, as they are one file here: a library loaded by both names must not map twice.
    #[test]
    fn sync_keeps_one_file_per_source() {
        use std::os::unix::fs::MetadataExt;
        let dir = temp("one-file");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("a.so");
        std::fs::write(&file, b"abc").unwrap();
        let entry = |name: &str| Entry {
            name: name.to_owned(),
            src: file.clone(),
            kind: Kind::File,
        };
        let bundle = Bundle {
            gate: "t".to_owned(),
            entries: vec![entry("out/app-art/lib64/a.so"), entry("out/device/a/a.so")],
        };
        let stage = dir.join("stage");
        bundle.stage(&stage).unwrap();
        let target = dir.join("target");
        Host::Local(target.clone()).sync(&stage, "t").unwrap();
        let ino = |name: &str| std::fs::metadata(target.join(name)).unwrap().ino();
        assert_eq!(ino("out/app-art/lib64/a.so"), ino("out/device/a/a.so"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A test Target on `local:<target>` staging one file `lib/a.so` with `body`, for gate `g` and break `brk` (stage dir per gate and break under `out`).
    fn local_target(
        out: &Path,
        target: &Path,
        src: &Path,
        body: &str,
        brk: Option<Break>,
    ) -> Target {
        std::fs::create_dir_all(src.parent().unwrap()).unwrap();
        std::fs::write(src, body).unwrap();
        Target {
            host: Host::Local(target.to_path_buf()),
            dir_name: "t".to_owned(),
            bundle: Bundle {
                gate: "g".to_owned(),
                entries: vec![Entry {
                    name: "lib/a.so".to_owned(),
                    src: src.to_path_buf(),
                    kind: Kind::File,
                }],
            },
            elf: Vec::new(),
            lib_dirs: Vec::new(),
            dmabuf_probe: false,
            stage: stage_dir(out, "g", brk),
        }
    }

    /// The run's staleness check in `dir`, as a run executes it: its exit code (7 = [`STALE_EXIT`]).
    fn stale_exit(dir: &Path, pushed: &Pushed) -> i32 {
        Command::new("sh")
            .arg("-c")
            .arg(stale_sh("g/run", &pushed.0))
            .current_dir(dir)
            .status()
            .unwrap()
            .code()
            .unwrap()
    }

    /// R5-1: `remote push g` (A), then `remote push g --break drop-atld` (B, another bundle) into the same target dir; `remote test g` then runs what A staged, which the dir no longer holds: stale (exit 7).
    /// Before: one stage dir per gate, so the run re-read B's manifest and ran B's bundle as A's.
    #[test]
    fn a_push_of_another_break_makes_the_first_pushs_run_stale() {
        let s = Scratch::new("brk");
        let (out, target) = (s.0.join("out"), s.0.join("target"));
        let a = local_target(&out, &target, &s.0.join("src/a/a.so"), "plain", None);
        let b = local_target(
            &out,
            &target,
            &s.0.join("src/b/a.so"),
            "broken",
            Some(Break::DropAtld),
        );
        let pa = a.push().unwrap();
        assert_eq!(a.staged().unwrap(), pa);
        assert_eq!(stale_exit(&target, &pa), 0, "A's own bundle runs");
        let pb = b.push().unwrap();
        assert_ne!(pa, pb);
        assert_eq!(a.staged().unwrap(), pa, "B's push leaves A's stage alone");
        assert_eq!(stale_exit(&target, &a.staged().unwrap()), STALE_EXIT);
        assert_eq!(stale_exit(&target, &pb), 0);
    }

    /// R5-2: two bundles whose manifests have the same size and the same mtime second, pushed A then B: rsync's quick check (size + mtime, rsync(1) `--size-only`/`--checksum`) skips B's manifest,
    /// so a check that hashes the target's manifest still finds A's and runs A. The marker file is a new path for B: A's run is stale.
    #[test]
    fn a_same_second_same_size_manifest_still_makes_the_run_stale() {
        let s = Scratch::new("same-second");
        let (out, target) = (s.0.join("out"), s.0.join("target"));
        // same length bodies -> manifests of equal size (`<64 hex>  <len>  lib/a.so`)
        let a = local_target(&out, &target, &s.0.join("src/a/a.so"), "abc", None);
        let pa = a.bundle.stage(&a.stage).unwrap();
        let b = local_target(
            &out,
            &target,
            &s.0.join("src/b/a.so"),
            "abd",
            Some(Break::DropAtld),
        );
        let pb = b.bundle.stage(&b.stage).unwrap();
        let (ma, mb) = (a.stage.join(MANIFEST), b.stage.join(MANIFEST));
        assert_eq!(
            std::fs::metadata(&ma).unwrap().len(),
            std::fs::metadata(&mb).unwrap().len()
        );
        // one fixed second for every staged file and dir, both stages
        let touch = Command::new("sh")
            .arg("-c")
            .arg("find \"$1\" \"$2\" -exec touch -h -d @1700000000 {} +")
            .args([
                "sh",
                &a.stage.display().to_string(),
                &b.stage.display().to_string(),
            ])
            .status()
            .unwrap();
        assert!(touch.success());
        a.host.sync(&a.stage, &a.dir_name).unwrap();
        assert_eq!(stale_exit(&target, &pa), 0);
        b.host.sync(&b.stage, &b.dir_name).unwrap();
        assert_eq!(
            std::fs::read(target.join(MANIFEST)).unwrap(),
            std::fs::read(&ma).unwrap(),
            "the fixture must hit the quick check: B's manifest skipped"
        );
        assert_eq!(stale_exit(&target, &pa), STALE_EXIT);
        assert_eq!(stale_exit(&target, &pb), 0);
    }

    #[test]
    fn classify_follows_the_checkout_rule() {
        let live = BTreeSet::from([
            "repo".to_owned(),
            "vsync-source-tier".to_owned(),
            "vsync-source-tier-base".to_owned(),
        ]);
        let arches = BTreeSet::from(["riscv64".to_owned(), "x86_64".to_owned()]);
        for (name, want) in [
            ("repo", Class::Live),
            ("repo-riscv64", Class::Live),
            ("vsync-source-tier", Class::Live),
            ("vsync-source-tier-base", Class::Live),
            ("vsync-source-tier-x86_64", Class::Live),
            ("vsync-source-tier-arm64", Class::Orphan),
            ("slot-1", Class::Orphan),
            ("slot-1-riscv64", Class::Orphan),
            ("gl-honesty-h4-x86_64", Class::Orphan),
            (".cache", Class::Unknown),
            ("..", Class::Unknown),
            ("a b", Class::Unknown),
            ("a/b", Class::Unknown),
        ] {
            assert_eq!(classify(name, true, &live, &arches), want, "{name}");
        }
        // `base` is no arch: with only `vsync-source-tier` live, `vsync-source-tier-base` is an orphan
        let only = BTreeSet::from(["vsync-source-tier".to_owned()]);
        assert_eq!(
            classify("vsync-source-tier-base", true, &only, &arches),
            Class::Orphan
        );
    }

    #[test]
    fn a_dir_without_the_manifest_is_never_an_orphan() {
        // `push` writes `bundle.manifest` into every tree: a dir without it is not ours, whatever its name
        let live = BTreeSet::from(["repo".to_owned()]);
        let arches = BTreeSet::from(["x86_64".to_owned()]);
        for name in ["slot-1", "egl-probe", "es1probe", "repo", "repo-x86_64"] {
            assert_eq!(
                classify(name, false, &live, &arches),
                Class::Unknown,
                "{name}"
            );
        }
    }

    #[test]
    fn listing_lines_parse_and_anything_else_stays_unknown() {
        let text = "dir\tslot-1\t556\tbusy\tmanifest\ndir\tx\t?\tfree\tnone\nother\tREADME\ndir\tbroken line\n";
        assert_eq!(
            parse_listing(text),
            [
                Listed::Dir {
                    name: "slot-1".to_owned(),
                    kib: Some(556),
                    busy: true,
                    manifest: true,
                },
                Listed::Dir {
                    name: "x".to_owned(),
                    kib: None,
                    busy: false,
                    manifest: false,
                },
                Listed::Other("README".to_owned()),
                Listed::Other("dir\tbroken line".to_owned()),
            ]
        );
    }

    #[test]
    fn remove_script_takes_run_dir_names_only() {
        for bad in ["../x", "a/b", ".cache", "a b", ""] {
            assert!(
                remove_script("atl-next-run", &[bad.to_owned()]).is_err(),
                "{bad:?} is refused"
            );
        }
        let s = remove_script("atl-next-run", &["slot-2".to_owned()]).expect("script");
        assert!(s.starts_with("cd 'atl-next-run' || exit 2\n"), "{s}");
        assert!(
            s.contains("if flock -n -E 75 'slot-2/.run.lock' rm -rf -- 'slot-2'; then"),
            "{s}"
        );
    }

    #[test]
    fn prune_takes_an_ssh_alias_not_a_local_dir() {
        assert_eq!(ssh_alias("laptop_local").expect("alias"), "laptop_local");
        assert!(ssh_alias("local:/tmp/run").is_err());
    }

    /// A throwaway dir under the temp dir, removed on drop.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(tag: &str) -> Scratch {
            let dir = std::env::temp_dir().join(format!("atl-prune-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("scratch dir");
            Scratch(dir)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Takes `dir/.run.lock` exclusively in a child process, as a run does, and returns once it holds it. Killing the child releases it.
    fn hold_lock(dir: &Path) -> std::process::Child {
        use std::io::{BufRead, BufReader};
        let mut child = Command::new("sh")
            .args([
                "-c",
                "exec 8>>\"$1\" && flock -x 8 && echo locked && exec sleep 60",
                "sh",
            ])
            .arg(dir.join(RUN_LOCK))
            .stdout(Stdio::piped())
            .spawn()
            .expect("sh");
        let mut line = String::new();
        BufReader::new(child.stdout.as_mut().expect("stdout"))
            .read_line(&mut line)
            .expect("read the lock holder");
        assert_eq!(line.trim(), "locked", "the holder takes the lock");
        child
    }

    #[test]
    fn prune_removes_a_free_orphan_and_keeps_held_and_live_dirs() {
        let scratch = Scratch::new("list");
        let root = scratch.0.clone();
        for name in ["vsync-source-tier", "slot-1", "slot-2"] {
            std::fs::create_dir_all(root.join(name)).expect("run dir");
            std::fs::write(root.join(name).join("file"), [0u8; 4096]).expect("file");
            std::fs::write(root.join(name).join(MANIFEST), "").expect("manifest");
        }
        // another tool's dir: a checkout's name, no manifest
        std::fs::create_dir(root.join("egl-probe")).expect("probe dir");
        std::fs::write(root.join("egl-probe").join("probe.so"), [0u8; 64]).expect("file");
        std::fs::write(root.join("slot-2").join(RUN_LOCK), "").expect("free lock file");
        std::fs::write(root.join("README"), "not a dir").expect("file");
        std::os::unix::fs::symlink(root.join("slot-2"), root.join("link")).expect("symlink");
        std::fs::create_dir(root.join(".hidden")).expect("dot dir");
        let mut holder = hold_lock(&root.join("slot-1"));

        let host = Host::Local(root.clone());
        let base = root.display().to_string();
        let listed = parse_listing(&host.run_script(&list_script(&base)).expect("listing"));
        // name -> (busy, manifest)
        let dirs: BTreeMap<String, (bool, bool)> = listed
            .iter()
            .filter_map(|entry| match entry {
                Listed::Dir {
                    name,
                    busy,
                    manifest,
                    ..
                } => Some((name.clone(), (*busy, *manifest))),
                Listed::Other(_) => None,
            })
            .collect();
        assert_eq!(
            dirs,
            BTreeMap::from([
                (".hidden".to_owned(), (false, false)),
                ("egl-probe".to_owned(), (false, false)),
                ("slot-1".to_owned(), (true, true)),
                ("slot-2".to_owned(), (false, true)),
                ("vsync-source-tier".to_owned(), (false, true)),
            ])
        );
        assert!(listed.contains(&Listed::Other("README".to_owned())));
        assert!(listed.contains(&Listed::Other("link".to_owned())));

        let live = BTreeSet::from(["vsync-source-tier".to_owned()]);
        let no_arches = BTreeSet::new();
        let free_orphans: Vec<String> = dirs
            .iter()
            .filter(|&(name, &(busy, manifest))| {
                classify(name, manifest, &live, &no_arches) == Class::Orphan && !busy
            })
            .map(|(name, _)| name.clone())
            .collect();
        assert_eq!(free_orphans, vec!["slot-2".to_owned()]);

        let removal =
            remove_script(&base, &["slot-1".to_owned(), "slot-2".to_owned()]).expect("script");
        let out = host.run_script(&removal).expect("removal");
        assert_eq!(
            parse_removal(&out),
            BTreeMap::from([
                ("slot-1".to_owned(), Removal::Busy),
                ("slot-2".to_owned(), Removal::Removed),
            ])
        );
        assert!(root.join("slot-1").is_dir(), "a held dir stays");
        assert!(!root.join("slot-2").exists(), "a free orphan is removed");
        assert!(
            root.join("egl-probe").is_dir(),
            "a dir without the manifest stays"
        );
        assert!(
            root.join("vsync-source-tier").is_dir(),
            "a live dir is not in the removal set"
        );
        holder.kill().expect("release the lock");
        holder.wait().expect("reap the holder");
    }

    #[test]
    fn a_held_lock_refuses_a_check_and_a_run_before_they_touch_the_dir() {
        let scratch = Scratch::new("busy");
        let dir = scratch.0.join("x");
        let run_root = dir.join("out/gl/run/root");
        std::fs::create_dir_all(&run_root).expect("run root");
        std::fs::write(run_root.join("keep"), "k").expect("file");
        let mut holder = hold_lock(&dir);
        let host = Host::Local(dir.clone());
        let base = dir.display().to_string();

        let check = prereq_script(&base, &[], &[]).expect("check script");
        let err = host
            .run_script(&check)
            .expect_err("a held lock refuses the check");
        assert!(format!("{err:#}").contains("busy"), "{err:#}");

        let plan = Plan {
            manifest_sha256: TEST_SHA.to_owned(),
            dmabuf_probe: false,
            audio: None,
            tag: "gl/run".to_owned(),
            root: "gl/run/root".to_owned(),
            scratch: "gl/run/scratch".to_owned(),
            env: Vec::new(),
            session_env: Vec::new(),
            program: "@BASE@/out/cargo/release/atld".to_owned(),
            args: Vec::new(),
            files: Vec::new(),
            lib_dirs: Vec::new(),
            limit: 120,
            display: Display::Headless,
            view: None,
            agent: None,
        };
        let err = host
            .run_script(&plan.script(&base))
            .expect_err("a held lock refuses the run");
        assert!(format!("{err:#}").contains("busy"), "{err:#}");
        assert!(
            run_root.join("keep").is_file(),
            "a refused run removes nothing"
        );
        holder.kill().expect("release the lock");
        holder.wait().expect("reap the holder");
    }

    /// Review-2 G2: a push into a dir a run holds fails `busy` and deletes nothing (the running gate's files and its lock file stay); once free, the push deletes stale files but not the lock.
    #[test]
    fn a_push_takes_the_run_lock_and_keeps_the_lock_file() {
        let scratch = Scratch::new("push");
        let dir = scratch.0.join("x");
        std::fs::create_dir_all(&dir).expect("dir");
        std::fs::write(dir.join("running-gate.so"), "r").expect("file");
        let stage = scratch.0.join("stage");
        std::fs::create_dir_all(&stage).expect("stage");
        std::fs::write(stage.join("new.so"), "n").expect("staged file");
        let host = Host::Local(dir.clone());

        let mut holder = hold_lock(&dir);
        let err = host
            .sync(&stage, "x")
            .expect_err("a held lock refuses the push");
        assert!(format!("{err:#}").contains("busy"), "{err:#}");
        assert!(
            dir.join("running-gate.so").is_file(),
            "a refused push deletes nothing"
        );
        assert!(
            !dir.join("new.so").exists(),
            "a refused push copies nothing"
        );
        holder.kill().expect("release the lock");
        holder.wait().expect("reap the holder");

        host.sync(&stage, "x").expect("a free dir takes the push");
        assert!(dir.join("new.so").is_file());
        assert!(
            !dir.join("running-gate.so").exists(),
            "--delete still removes stale files"
        );
        assert!(
            dir.join(RUN_LOCK).is_file(),
            "the lock file is excluded from --delete"
        );
    }

    /// Review-4 H4: gate A pushes, gate B pushes another file set into the same dir, then A's run starts: it must refuse (`stale`, exit 7) before touching the dir, not run B's bundle. Once A's bundle
    /// is back, the same script passes the check and reaches the launcher check (exit 4 here: the fixture has no atld), which shows the check responds both ways.
    #[test]
    fn a_run_refuses_a_bundle_another_push_replaced() {
        let scratch = Scratch::new("replaced");
        let dir = scratch.0.join("x");
        let stage = |name: &str, file: &str| {
            let s = scratch.0.join(name);
            std::fs::create_dir_all(&s).expect("stage");
            std::fs::write(s.join(file), name).expect("staged file");
            let manifest = format!("{file}\n");
            std::fs::write(s.join(MANIFEST), &manifest).expect("manifest");
            // as Bundle::stage writes it
            std::fs::write(s.join(marker_name(&sha256_hex(manifest.as_bytes()))), "")
                .expect("marker");
            (s, manifest)
        };
        let (a, a_manifest) = stage("a", "a.so");
        let (b, b_manifest) = stage("b", "bundle-b.so");
        assert_ne!(a_manifest.len(), b_manifest.len());
        let host = Host::Local(dir.clone());
        host.sync(&a, "x").expect("push A");
        host.sync(&b, "x").expect("push B");
        let run_root = dir.join("out/gl/run/root");
        std::fs::create_dir_all(&run_root).expect("run root");
        std::fs::write(run_root.join("keep"), "k").expect("file");
        let plan = Plan {
            tag: "gl/run".to_owned(),
            root: "gl/run/root".to_owned(),
            scratch: "gl/run/scratch".to_owned(),
            env: Vec::new(),
            session_env: Vec::new(),
            program: "@BASE@/out/cargo/release/atld".to_owned(),
            args: Vec::new(),
            files: Vec::new(),
            lib_dirs: Vec::new(),
            limit: 120,
            display: Display::Headless,
            dmabuf_probe: false,
            view: None,
            agent: None,
            manifest_sha256: sha256_hex(a_manifest.as_bytes()),
            audio: None,
        };
        let script = plan.script(&dir.display().to_string());
        let err = host
            .run_script(&script)
            .expect_err("B's bundle refuses A's run");
        let text = format!("{err:#}");
        assert!(text.contains("stale: another push"), "{text}");
        assert!(
            run_root.join("keep").is_file(),
            "a stale run touches nothing: {text}"
        );

        host.sync(&a, "x").expect("push A again");
        let err = host
            .run_script(&script)
            .expect_err("the fixture has no atld");
        let text = format!("{err:#}");
        assert!(
            !text.contains("stale: another push") && text.contains("not an executable"),
            "{text}"
        );
    }

    #[test]
    fn prune_dirs_executes_on_a_throwaway_dir() {
        let scratch = Scratch::new("exec");
        let root = scratch.0.clone();
        let base = root.display().to_string();
        for name in ["repo", "slot-1", "slot-2"] {
            std::fs::create_dir_all(root.join(name)).expect("run dir");
            std::fs::write(root.join(name).join("file"), [0u8; 4096]).expect("file");
            std::fs::write(root.join(name).join(MANIFEST), "").expect("manifest");
        }
        std::fs::create_dir(root.join("egl-probe")).expect("probe dir");
        let mut holder = hold_lock(&root.join("slot-1"));
        let host = Host::Local(root.clone());
        let live = BTreeSet::from(["repo".to_owned()]);
        let no_arches = BTreeSet::new();

        let dry = prune_dirs(&host, &base, &live, &no_arches, false).expect("dry run");
        assert_eq!(dry, 0, "a dry run has no failures");
        assert!(root.join("slot-2").is_dir(), "a dry run removes nothing");

        let failed = prune_dirs(&host, &base, &live, &no_arches, true).expect("execute");
        assert_eq!(failed, 0, "a held orphan is a skip, not a failure");
        assert!(!root.join("slot-2").exists(), "a free orphan is removed");
        assert!(root.join("slot-1").is_dir(), "a held orphan is skipped");
        assert!(root.join("repo").is_dir(), "a live dir stays");
        assert!(
            root.join("egl-probe").is_dir(),
            "a dir without the manifest stays"
        );
        holder.kill().expect("release the lock");
        holder.wait().expect("reap the holder");
    }
}
