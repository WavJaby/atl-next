// SPDX-License-Identifier: MIT
//! `cargo xtask remote`: runs WSL-built outputs on a Linux desktop over ssh (the owner's laptop, plan L1).
//!
//! The record: a gate's last run in a slot writes what its runtime read (atld_run.rs `record_inputs`, via [`write_record`]): host files, and host directories whose trees the bundle takes whole.
//! `push` bundles that record, mapped to `out/`, `java/` and `src/aosp/` names, plus `lib/`: Debian's LLVM 19 runtimes (a labelled patch until our pinned runtimes ship in an install tree, plan L1 / D1).
//! A directory is replicated with its symlinks kept as links and never followed (the ART tree's `com.android.art -> .` would loop); files are hard links, copies across filesystems.
//! Dry run by default. `--apply` stages the bundle under `<out>/remote/bundle/<gate>` (with a manifest of sha256 + size, or the link target) and runs `rsync -a --delete` to `~/atl-next-run/<slot>/` on the host.
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
use sha2::{Digest, Sha256};

use crate::{arches, gate_goals, gl_probe, provided_gate, slot, verdict};

/// The bundle's `lib/`: Debian's LLVM 19 runtimes. Arch ships libc++ 23 but no LLVM `libunwind.so.1` (plan L1 section 3). A labelled patch: the ideal is our pinned runtimes (runtimes.rs) in a packaged install tree (plan D1).
pub const LLVM_LIB_DIR: &str = "/usr/lib/llvm-19/lib";
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
/// atld's entry: `cargo_bin` (atld_run.rs) under the slot.
const ATLD: &str = "out/cargo/release/atld";
/// Bundle names of a record's roots: the slot, the java out and the AOSP tree.
const ROOT_NAMES: [(&str, &str); 3] = [("out", "out"), ("java", "java"), ("tree", "src/aosp")];
/// The target's directory for a slot's bundle, relative to the host's home: `~/atl-next-run/<slot>/`.
const REMOTE_ROOT: &str = "atl-next-run";
/// `<out>/remote/...`: the record and the staged bundle.
pub const REMOTE_DIR: &str = "remote";
/// The manifest file at the bundle's root: `sha256  size  name` per file, `link  target  name` per link.
const MANIFEST: &str = "bundle.manifest";
/// ssh options: never prompt, never hang. The alias check is the first contact with the host. ServerAlive: a link that dies mid-call fails in about 3 min instead of hanging until the caller's limit.
const SSH_OPTS: [&str; 8] = [
    "-o",
    "BatchMode=yes",
    "-o",
    "ConnectTimeout=20",
    "-o",
    "ServerAliveInterval=30",
    "-o",
    "ServerAliveCountMax=6",
];
const MIB: f64 = 1024.0 * 1024.0;

#[derive(clap::Subcommand)]
pub enum Remote {
    /// Bundles a gate's runtime inputs (its last run's record in the slot) and `lib/`, and copies them to an ssh alias or a local dir.
    ///
    /// Dry run by default: prints the bundle summary (`--list`: every entry) and the rsync it would run. `--apply` stages the bundle under `<out>/remote/bundle/<gate>` with a manifest, then runs `rsync -a --delete` to `~/atl-next-run/<slot>/` on the host.
    /// The record is written by a gate's run in the slot (`xtask test <gate>`); without it this names that command.
    Push(PushArgs),
    /// The prerequisite check on the target: `ldd` over the bundle, every `not found` named with the file that needs it. Then the gate's runtime half: its atld runs execute on the target (`gl` only).
    ///
    /// Dry run by default: prints the check it would run. `--apply` runs it over ssh (or locally for `local:`). Run `push --apply` first: a file the push did not deliver is named as missing.
    Test(TestArgs),
    /// `run-apk` on the target: pushes the bundle of the run's record (its APK, the runtime, `lib/`), checks it there, then runs the APK under atld on the target's desktop for `--hold` seconds, with run-apk's verdict.
    ///
    /// Dry run by default: prints the bundle and the plan. `--apply` pushes, checks and runs. The window is the target's, on its Wayland session; the logs come back when the run ends.
    RunApk(crate::run_apk::RemoteArgs),
}

#[derive(clap::Args)]
pub struct PushArgs {
    /// ssh alias from WSL `~/.ssh/config` (its `Host` name), or `local:<absolute dir>` for a directory standing in for the target.
    #[arg(long)]
    host: String,
    /// The gate whose record is bundled (gate_goals.rs name).
    #[arg(long)]
    gate: String,
    /// The native out dir holding the record (default: this checkout's slot, see `xtask slot`).
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
    host: String,
    /// The native out dir holding the record (default: this checkout's slot, see `xtask slot`).
    #[arg(long)]
    out: Option<PathBuf>,
    /// Runs the check on the target. Without it: prints the check.
    #[arg(long)]
    apply: bool,
    /// Deliberate break: the bundle without its copy of the named runtime (see the values).
    #[arg(long = "break", value_enum)]
    brk: Option<Break>,
}

#[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Break {
    /// Drops `lib/libunwind.so.1`: the prerequisite check must name it on a host with no copy of its own.
    DropLibunwind,
    /// Drops atld from the bundle: the prerequisite check names it as not delivered (`push --apply`), and a run without it stops at the executable check.
    DropAtld,
}

pub fn run(root: &Path, cmd: &Remote) -> Result<ExitCode> {
    match cmd {
        Remote::Push(a) => push(root, a),
        Remote::Test(a) => test(root, a),
        Remote::RunApk(a) => crate::run_apk::remote(root, a),
    }
}

fn push(root: &Path, a: &PushArgs) -> Result<ExitCode> {
    let out = out_of(root, a.out.as_deref())?;
    let target = Target::of(&out, &a.gate, &a.host, a.brk)?;
    if a.list {
        for e in target.entries() {
            println!("{}\t{}", e.name, e.src.display());
        }
    }
    println!("{}", target.summary());
    let stage = stage_dir(&out, &a.gate);
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
    let out = out_of(root, a.out.as_deref())?;
    let target = Target::of(&out, &a.gate, &a.host, a.brk)?;
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
        "gate `{}`: its runtime half is not remotable yet (only `gl`: its runs are one-shot; the others drive a live runtime)",
        a.gate
    );
    let arches = arches::load(root)?;
    let (arch, _) = arches::build_machine(root, &arches)?;
    let dir = out.join(REMOTE_DIR).join(target.log_dir());
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let log = dir.join(format!("{}.log", a.gate));
    std::fs::write(&log, "").with_context(|| format!("writing {}", log.display()))?;
    let site = target.run_site(gl_probe::LIMIT, log.clone(), false);
    let code = gl_probe::run_on(root, arch, out, site)?;
    println!("remote test {}: transcript {}", a.gate, log.display());
    Ok(code == ExitCode::SUCCESS)
}

/// Where a runtime's atld runs: this host, or a target that runs it from the bundle. The gate's judge reads the same log lines either way.
#[derive(Default)]
pub enum Site {
    #[default]
    Host,
    Remote(RemoteRun),
}

/// A remote site: the target, the bundle's directory there, the library search path `test` checked, atld's time limit, and the transcript the transfers append to.
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
    /// The target's desktop session supplies `WAYLAND_DISPLAY`: its one Wayland socket ([`session_script`]).
    session: bool,
}

/// The host roots a runtime names, absolute on this host: the slot, the java out and the AOSP tree. Their bundle names are [`ROOT_NAMES`].
pub struct Roots {
    out: PathBuf,
    java: PathBuf,
    tree: PathBuf,
}

impl Roots {
    pub fn of(out: &Path, java: &Path, tree: &Path) -> Roots {
        Roots {
            out: out.to_path_buf(),
            java: java.to_path_buf(),
            tree: tree.to_path_buf(),
        }
    }

    /// Each root with its bundle name. The fourth is the bundle's `lib/`: the runtimes it ships stand for [`LLVM_LIB_DIR`] on this host.
    fn named(&self) -> [(&Path, &'static str); 4] {
        let [out, java, tree] = ROOT_NAMES.map(|(key, name)| (self.path(key), name));
        [out, java, tree, (Path::new(LLVM_LIB_DIR), "lib")]
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
    format!(
        "WL_DIR=\"/run/user/$(id -u)\"\nWL=\"\"\nWL_N=0\nfor s in \"$WL_DIR\"/wayland-*; do\n  case \"$s\" in *.lock) continue ;; esac\n  [ -S \"$s\" ] || continue\n  WL=${{s##*/}}\n  WL_N=$((WL_N + 1))\ndone\n[ \"$WL_N\" -eq 1 ] || {{ echo \"remote run {tag}: $WL_N Wayland sockets in $WL_DIR, want 1 (the desktop session's)\" >&2; exit 5; }}\n"
    )
}

/// One atld run as the target runs it: the command, its environment, and the files this host wrote for it, all with bundle paths (`@BASE@`). `root` and `scratch` are relative to the slot's out dir, where the logs return.
struct Plan {
    /// The run's name, its dir under the slot, e.g. `gl/run`.
    tag: String,
    root: String,
    scratch: String,
    env: Vec<(String, String)>,
    program: String,
    args: Vec<String>,
    /// (bundle path, content): the package and permission files, the fontconfig copy.
    files: Vec<(String, String)>,
    lib_dirs: Vec<String>,
    limit: u64,
    /// `WAYLAND_DISPLAY` comes from the target's session: [`session_script`] finds the socket.
    session: bool,
}

fn out_path(rel: &str) -> String {
    format!("{BASE_MARK}/out/{rel}")
}

impl Plan {
    /// The script the target runs on stdin (`sh -s`): the run's directories made fresh and its files written, then atld under `env -i` with `timeout` and stdin closed, in the background so its process group can be ended after it; then its logs and end record (`atld.rc`: the exit code, `end.ms`: the end's time on the target's clock) as a tar on stdout. The script's exit is the tar's.
    fn script(&self, base: &str) -> String {
        let mut s = format!(
            "# xtask remote run {}: written by remote.rs, do not edit\ncd {} || exit 2\nset -u\nBASE=$(pwd)\n",
            self.tag,
            sh_quote(base)
        );
        s.push_str(&format!("rm -rf {}\n", word(&out_path(&self.root))));
        s.push_str(&format!("mkdir -p {}\n", word(&out_path(&self.scratch))));
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
        if self.session {
            s.push_str(&session_script(&self.tag));
        }
        let lib_path: Vec<String> = self
            .lib_dirs
            .iter()
            .map(|d| format!("{BASE_MARK}/{d}"))
            .collect();
        s.push_str(&format!(
            "env -i PATH=/usr/local/bin:/usr/bin:/bin HOME=\"$HOME\" XDG_RUNTIME_DIR=\"/run/user/$(id -u)\" LD_LIBRARY_PATH={}",
            word(&lib_path.join(":"))
        ));
        if self.session {
            s.push_str(" WAYLAND_DISPLAY=\"$WL\"");
        }
        for (key, value) in &self.env {
            s.push_str(&format!(" {key}={}", word(value)));
        }
        let args: Vec<String> = self.args.iter().map(|a| word(a)).collect();
        // background: `$!` is timeout's pid, which timeout makes its process group's id; the group is ended after the run, so an app that stayed in it leaves no window behind
        s.push_str(&format!(
            " timeout -k {KILL_GRACE} {} {} {} < /dev/null > {} 2>&1 &\n",
            self.limit,
            word(&self.program),
            args.join(" "),
            word(&out_path(&format!("{}/atld.log", self.scratch)))
        ));
        s.push_str("pid=$!\nwait \"$pid\"\nrc=$?\n");
        s.push_str("kill -KILL -- \"-$pid\" 2>/dev/null || true\n");
        s.push_str(&format!(
            "printf '%s\\n' \"$rc\" > {}\n",
            word(&out_path(&format!("{}/atld.rc", self.scratch)))
        ));
        s.push_str(&format!(
            "date +%s%3N > {}\n",
            word(&out_path(&format!("{}/end.ms", self.scratch)))
        ));
        s.push_str(&format!(
            "echo \"atld rc=$rc, remote run {}\" >&2\n",
            self.tag
        ));
        s.push_str(&format!(
            "tar -c -C \"$BASE/out\" --ignore-failed-read {} {} {} {}\n",
            sh_quote(&format!("{}/log", self.root)),
            sh_quote(&format!("{}/atld.log", self.scratch)),
            sh_quote(&format!("{}/atld.rc", self.scratch)),
            sh_quote(&format!("{}/end.ms", self.scratch))
        ));
        s
    }
}

impl RemoteRun {
    /// Whether the target's desktop session supplies `WAYLAND_DISPLAY` (`session`).
    pub fn has_session(&self) -> bool {
        self.session
    }

    /// Starts `cmd` (atld's command, host paths) on the target as one run. The child is the transfer, `<transport> < plan | tar -x` (`sh -s` for `local:`), and its end is the run's. `root` and `scratch` lie under the slot's out dir, where the logs return. `written` are the files the command reads that this host wrote: their content goes into the plan.
    pub fn spawn(
        &self,
        roots: &Roots,
        cmd: &Command,
        root: &Path,
        scratch: &Path,
        written: &[PathBuf],
    ) -> Result<Child> {
        ensure!(
            cmd.get_current_dir().is_none(),
            "remote run: a working directory is not relocated"
        );
        let root_rel = rel_out(&roots.out, root)?;
        let tag = root_rel
            .strip_suffix("/root")
            .unwrap_or(&root_rel)
            .to_owned();
        let env = cmd
            .get_envs()
            .map(|(key, value)| -> Result<(String, String)> {
                let value = value.with_context(|| format!("{key:?} is unset, not set"))?;
                Ok((utf8(key)?, relocate(&utf8(value)?, roots)))
            })
            .collect::<Result<Vec<_>>>()?;
        let args = cmd
            .get_args()
            .map(|a| Ok(relocate(&utf8(a)?, roots)))
            .collect::<Result<Vec<_>>>()?;
        let files = written
            .iter()
            .map(|path| -> Result<(String, String)> {
                let body = std::fs::read_to_string(path)
                    .with_context(|| format!("reading {}", path.display()))?;
                Ok((
                    relocate(&utf8(path.as_os_str())?, roots),
                    relocate(&body, roots),
                ))
            })
            .collect::<Result<Vec<_>>>()?;
        let plan = Plan {
            tag: tag.clone(),
            root: root_rel,
            scratch: rel_out(&roots.out, scratch)?,
            env,
            program: relocate(&utf8(cmd.get_program())?, roots),
            args,
            files,
            lib_dirs: self.lib_dirs.clone(),
            limit: self.limit.as_secs(),
            session: self.session,
        };
        let script = scratch.join(REMOTE_SCRIPT);
        std::fs::write(&script, plan.script(&self.base))
            .with_context(|| format!("writing {}", script.display()))?;
        let transport = match &self.host {
            Host::Ssh(alias) => format!("ssh {} {} sh -s", SSH_OPTS.join(" "), sh_quote(alias)),
            Host::Local(_) => "sh -s".to_owned(),
        };
        let transfer = format!(
            "{transport} < {} | tar -x -C {}",
            sh_quote(&script.display().to_string()),
            sh_quote(&roots.out.display().to_string())
        );
        let mut log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log)
            .with_context(|| format!("opening {}", self.log.display()))?;
        writeln!(log, "== run {tag} on {}", self.host.label())?;
        Command::new("sh")
            .args(["-c", &transfer])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(log.try_clone()?)
            .spawn()
            .context("starting the transfer to the target")
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

/// `path` relative to the slot's out dir: the target keeps that layout. A path outside the slot is refused.
pub(crate) fn rel_out(out: &Path, path: &Path) -> Result<String> {
    let rel = path.strip_prefix(out).with_context(|| {
        format!(
            "{} is not under the slot {}: a remote run keeps its files there",
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

    /// Where the bundle sits on the target: `~/atl-next-run/<slot>` (relative to the home dir), or the local dir.
    fn base(&self, slot: &str) -> String {
        match self {
            Host::Ssh(_) => format!("{REMOTE_ROOT}/{slot}"),
            Host::Local(dir) => dir.display().to_string(),
        }
    }

    fn target(&self, slot: &str) -> String {
        match self {
            Host::Ssh(alias) => format!("{alias}:{REMOTE_ROOT}/{slot}/"),
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
                "ssh alias `{alias}` does not connect ({}): add a `Host {alias}` block to WSL ~/.ssh/config (owner; the repo holds no address)",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(())
    }

    fn sync(&self, stage: &Path, slot: &str) -> Result<()> {
        let src = format!("{}/", stage.display());
        let status = match self {
            Host::Ssh(alias) => Command::new("rsync")
                .args(["-a", "--delete", "--mkpath", "-e"])
                .arg(format!("ssh {}", SSH_OPTS.join(" ")))
                .arg(&src)
                .arg(format!("{alias}:{REMOTE_ROOT}/{slot}/"))
                .status(),
            Host::Local(dir) => {
                std::fs::create_dir_all(dir)?;
                Command::new("rsync")
                    .args(["-a", "--delete"])
                    .arg(&src)
                    .arg(format!("{}/", dir.display()))
                    .status()
            }
        }
        .context("running rsync")?;
        ensure!(
            status.success(),
            "rsync to {} failed: {status}",
            self.label()
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
                cmd.arg("ssh").args(SSH_OPTS).arg(alias).args(["sh", "-s"]);
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

fn ssh(alias: &str) -> Command {
    let mut c = Command::new("ssh");
    c.args(SSH_OPTS).arg(alias);
    c
}

/// A gate's bundle on one host: what `push`, the prerequisite check and a remote run share.
pub struct Target {
    host: Host,
    slot: String,
    bundle: Bundle,
    /// Bundle names of the ELF files, and of the directories holding them: the check's search path, and a run's `LD_LIBRARY_PATH`.
    elf: Vec<String>,
    lib_dirs: Vec<String>,
    stage: PathBuf,
}

impl Target {
    /// The bundle of gate `gate`'s record in `out`, with `brk` applied, for the host `host` (an ssh alias or `local:<dir>`). Its ELF files and library dirs are read here.
    pub fn of(out: &Path, gate: &str, host: &str, brk: Option<Break>) -> Result<Target> {
        let host = Host::parse(host)?;
        let slot = slot_name(out)?;
        let bundle = Bundle::of(gate, out, brk)?;
        let elf = bundle.elf_names()?;
        let lib_dirs = bundle.lib_dirs(&elf);
        Ok(Target {
            host,
            slot,
            bundle,
            elf,
            lib_dirs,
            stage: stage_dir(out, gate),
        })
    }

    pub fn summary(&self) -> String {
        self.bundle.summary()
    }

    pub fn entries(&self) -> &[Entry] {
        &self.bundle.entries
    }

    /// `alias:~/atl-next-run/<slot>/` (or the local dir): where the bundle goes.
    pub fn target(&self) -> String {
        self.host.target(&self.slot)
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

    /// Stages the bundle and copies it to the target (`rsync -a --delete`). Fails first when the host does not answer.
    pub fn push(&self) -> Result<()> {
        self.host.reach()?;
        self.bundle.stage(&self.stage)?;
        self.host.sync(&self.stage, &self.slot)
    }

    /// The prerequisite check on the target: `ldd` over every ELF file of the bundle.
    pub fn check(&self) -> Result<Prereq> {
        let script = prereq_script(&self.host.base(&self.slot), &self.elf, &self.lib_dirs)?;
        Ok(parse_prereq(&self.host.run_script(&script)?))
    }

    /// A run's site on the target: atld runs from the bundle there, ends at `limit`, and its transcript is appended to `log`. `session`: the target's desktop session supplies `WAYLAND_DISPLAY`.
    pub fn run_site(&self, limit: Duration, log: PathBuf, session: bool) -> Site {
        Site::Remote(RemoteRun {
            host: self.host.clone(),
            base: self.host.base(&self.slot),
            lib_dirs: self.lib_dirs.clone(),
            limit,
            log,
            session,
        })
    }

    /// The bundle dir on the target, where a run's `out/` lives: `~/atl-next-run/<slot>` relative to home, or the local dir.
    pub fn base(&self) -> String {
        self.host.base(&self.slot)
    }

    /// `script` on the target, its stdout as bytes, bounded by `limit` (see `Host::run_bounded`).
    pub fn run_bounded(&self, script: &str, limit: Duration) -> Result<Vec<u8>> {
        self.host.run_bounded(script, limit)
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
    slot::out_dir(root, explicit, "--out", |ws| ws.join("out/native"))
}

/// The slot's directory name: the target's directory is named after it.
fn slot_name(out: &Path) -> Result<String> {
    let name = out
        .file_name()
        .and_then(|n| n.to_str())
        .with_context(|| format!("out dir {} has no name", out.display()))?;
    ensure!(
        valid_name(name),
        "out dir name `{name}` has characters the target path does not take"
    );
    Ok(name.to_owned())
}

fn stage_dir(out: &Path, gate: &str) -> PathBuf {
    out.join(REMOTE_DIR).join("bundle").join(gate)
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

/// Writes gate `gate`'s record: its inputs (absolute), with the roots their bundle names come from (`java`, `tree`; the slot is `out`).
pub fn write_record(
    out: &Path,
    gate: &str,
    java: &Path,
    tree: &Path,
    inputs: &Inputs,
) -> Result<()> {
    let file = record_path(out, gate);
    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut text =
        format!("# runtime inputs of gate {gate}, written by its last run (xtask remote)\n");
    for (name, dir) in [("out", out), ("java", java), ("tree", tree)] {
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
    files: Vec<PathBuf>,
    dirs: Vec<PathBuf>,
}

fn read_record(out: &Path, gate: &str) -> Result<Record> {
    let file = record_path(out, gate);
    let text = std::fs::read_to_string(&file).with_context(|| {
        format!(
            "gate `{gate}`: no runtime-input record at {}: run `cargo xtask test {gate}` in this slot once (its run records what it reads)",
            file.display()
        )
    })?;
    parse_record(&text)
}

/// The record text: `out` / `java` / `tree` root lines, `dir <path>` lines, then one file path per line; `#` lines are comments. Paths are absolute.
fn parse_record(text: &str) -> Result<Record> {
    let mut roots = BTreeMap::new();
    let mut files = Vec::new();
    let mut dirs = Vec::new();
    for line in text.lines() {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((key, path)) = ROOT_NAMES
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
    Ok(Record { roots, files, dirs })
}

fn absolute_path(text: &str) -> Result<PathBuf> {
    ensure!(
        Path::new(text).is_absolute(),
        "record line is not an absolute path: {text}"
    );
    Ok(PathBuf::from(text))
}

/// A record path's bundle name: under the slot as `out/...`, else the java out as `java/...`, else the AOSP tree as `src/aosp/...`. Anything else is refused: the bundle lays out known roots only.
fn bundle_name(path: &Path, roots: &BTreeMap<&'static str, PathBuf>) -> Result<String> {
    for (key, name) in ROOT_NAMES {
        if let Some(root) = roots.get(key)
            && let Ok(rel) = path.strip_prefix(root)
        {
            return Ok(format!("{name}/{}", rel.display()));
        }
    }
    bail!(
        "runtime input outside the slot, the java out and the AOSP tree: {}",
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

/// Adds the tree at `dir` under bundle name `name`. Directories recurse. A link to a regular file becomes that file's content under the link's name: its absolute target (a slot path) would dangle on the target. A link to a directory inside the tree stays a link (the tree's own alias, `com.android.art -> .`); one outside is refused. A dangling link stays as it is.
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
    fn of(gate: &str, out: &Path, brk: Option<Break>) -> Result<Bundle> {
        gate_goals::declared(gate)?;
        let record = read_record(out, gate)?;
        let mut map = BTreeMap::new();
        for file in &record.files {
            ensure!(
                file.is_file(),
                "record names {} but it is not a file: rebuild the slot or rerun `cargo xtask test {gate}`",
                file.display()
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
                "record names {} but it is not a directory: rebuild the slot or rerun `cargo xtask test {gate}`",
                dir.display()
            );
            add_tree(dir, &bundle_name(dir, &record.roots)?, &mut map)?;
        }
        for (soname, file) in LLVM_RUNTIMES {
            let src = Path::new(LLVM_LIB_DIR).join(file);
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
        if brk == Some(Break::DropLibunwind) {
            ensure!(
                map.remove(LIBUNWIND).is_some(),
                "break drop-libunwind: no {LIBUNWIND} in the bundle"
            );
        }
        if brk == Some(Break::DropAtld) {
            ensure!(
                map.remove(ATLD).is_some(),
                "break drop-atld: no {ATLD} in the bundle"
            );
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

    /// Stages the entries under `dir` (recreated), with the manifest. Files are hard links (a copy across filesystems), links are links.
    fn stage(&self, dir: &Path) -> Result<()> {
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
                    let hash: String = Sha256::digest(&bytes)
                        .iter()
                        .map(|b| format!("{b:02x}"))
                        .collect();
                    manifest.push_str(&format!("{hash}  {}  {}\n", bytes.len(), e.name));
                }
            }
        }
        let manifest_path = dir.join(MANIFEST);
        std::fs::write(&manifest_path, manifest)
            .with_context(|| format!("writing {}", manifest_path.display()))?;
        Ok(())
    }
}

/// A path the target takes unquoted (the slot's directory name, via [`valid_name`]): `[A-Za-z0-9._+/-]`, no `..` component.
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

#[cfg(test)]
mod tests {
    use super::*;

    fn roots() -> BTreeMap<&'static str, PathBuf> {
        BTreeMap::from([
            ("out", PathBuf::from("/w/out/slot-9")),
            ("java", PathBuf::from("/w/out/slot-9")),
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
            bundle_name(Path::new("/w/out/slot-9/device/libart/libart.so"), &r).unwrap(),
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
        let text = "# c\nout /w/out/slot-9\njava /w/out/slot-9\ntree /w/src/aosp\ndir /w/out/slot-9/gl-art\n/w/out/slot-9/a.so\n";
        let rec = parse_record(text).unwrap();
        assert_eq!(rec.files, vec![PathBuf::from("/w/out/slot-9/a.so")]);
        assert_eq!(rec.dirs, vec![PathBuf::from("/w/out/slot-9/gl-art")]);
        assert_eq!(rec.roots["tree"], PathBuf::from("/w/src/aosp"));
        assert!(
            parse_record("out /o\njava /o\n/x.so\n").is_err(),
            "no tree root"
        );
        assert!(parse_record("out /o\njava /o\ntree /t\nrelative.so\n").is_err());
        assert!(parse_record("out /o\njava /o\ntree /t\ndir relative\n").is_err());
    }

    #[test]
    fn prereq_script_quotes_and_orders_the_check() {
        let s = prereq_script(
            "atl-next-run/slot-9",
            &["lib/libc++.so.1".to_owned(), "out/device/x.so".to_owned()],
            &["lib".to_owned(), "out/device".to_owned()],
        )
        .unwrap();
        let want = concat!(
            "cd 'atl-next-run/slot-9' || exit 2\n",
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
        let out = dir.join("slot");
        let tree = dir.join("tree");
        let inputs = Inputs {
            files: vec![out.join("device/a.so"), tree.join("x.kcm")],
            dirs: vec![out.join("gl-art")],
        };
        write_record(&out, "gl", &out, &tree, &inputs).unwrap();
        let rec = read_record(&out, "gl").unwrap();
        assert_eq!(rec.files, inputs.files);
        assert_eq!(rec.dirs, inputs.dirs);
        assert_eq!(rec.roots["out"], out);
        assert_eq!(rec.roots["tree"], tree);
        std::fs::remove_dir_all(&dir).unwrap();
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
            session: false,
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
            Path::new("/w/out/slot-9"),
            Path::new("/w/out/java/build"),
            Path::new("/w/src/aosp"),
        )
    }

    #[test]
    fn relocate_maps_each_root_at_a_token_start_only() {
        let r = host_roots();
        assert_eq!(
            relocate("--root=/w/out/slot-9/gl/run/root", &r),
            "--root=@BASE@/out/gl/run/root"
        );
        assert_eq!(
            relocate("/w/out/java/build/boot.jar", &r),
            "@BASE@/java/boot.jar"
        );
        assert_eq!(relocate("/w/src/aosp/k.kcm", &r), "@BASE@/src/aosp/k.kcm");
        // `--file host=at`: the host side moves, the Android path stays
        assert_eq!(
            relocate("--file=/w/out/slot-9/a.so=/system/lib64/a.so", &r),
            "--file=@BASE@/out/a.so=/system/lib64/a.so"
        );
        // a library path joins roots with `:`
        assert_eq!(
            relocate("/w/out/slot-9/a:/w/out/slot-9/b", &r),
            "@BASE@/out/a:@BASE@/out/b"
        );
        // a JSON string value
        assert_eq!(
            relocate(r#"{"apk": "/w/out/slot-9/x.apk"}"#, &r),
            r#"{"apk": "@BASE@/out/x.apk"}"#
        );
        // element text: `>` and `<` bound a path too (fontconfig's `<dir>`)
        assert_eq!(
            relocate("<dir>/w/out/slot-9/fonts/files</dir>", &r),
            "<dir>@BASE@/out/fonts/files</dir>"
        );
        // a root followed by a name character is another path
        assert_eq!(relocate("/w/out/slot-9.bak", &r), "/w/out/slot-9.bak");
        // the bundle's `lib/` stands for the host's LLVM runtimes
        assert_eq!(
            relocate("--lib-path=/w/out/slot-9/a:/usr/lib/llvm-19/lib", &r),
            "--lib-path=@BASE@/out/a:@BASE@/lib"
        );
        // a directory that only starts with the root's name is not under it
        assert_eq!(relocate("/w/out/slot-90/x", &r), "/w/out/slot-90/x");
    }

    #[test]
    fn word_quotes_for_the_shell_and_expands_the_base() {
        assert_eq!(word("@BASE@/out/a b$c\"d"), "\"$BASE/out/a b\\$c\\\"d\"");
    }

    #[test]
    fn remote_runs_keep_their_files_under_the_slot() {
        let out = Path::new("/w/out/slot-9");
        assert_eq!(
            rel_out(out, Path::new("/w/out/slot-9/gl/run/root")).unwrap(),
            "gl/run/root"
        );
        assert!(rel_out(out, Path::new("/w/elsewhere/x")).is_err());
    }

    /// One run's script, written out: directories fresh, the embedded file with `$BASE` expanded on the target, atld under `env -i` with stdin closed, then its logs as a tar.
    #[test]
    fn run_script_is_the_plan_in_order() {
        let plan = Plan {
            tag: "gl/run".to_owned(),
            root: "gl/run/root".to_owned(),
            scratch: "gl/run/scratch".to_owned(),
            env: vec![(
                "FONTCONFIG_FILE".to_owned(),
                "@BASE@/out/gl/run/scratch/fonts.conf".to_owned(),
            )],
            program: "@BASE@/out/cargo/release/atld".to_owned(),
            args: vec!["--root".to_owned(), "@BASE@/out/gl/run/root".to_owned()],
            files: vec![(
                "@BASE@/out/gl/run/scratch/package-0.json".to_owned(),
                "{\"apk\": \"@BASE@/out/gl/apk/x.apk\"}\n".to_owned(),
            )],
            lib_dirs: vec!["lib".to_owned()],
            limit: 120,
            session: false,
        };
        let want = concat!(
            "# xtask remote run gl/run: written by remote.rs, do not edit\n",
            "cd 'atl-next-run/slot-9' || exit 2\n",
            "set -u\n",
            "BASE=$(pwd)\n",
            "rm -rf \"$BASE/out/gl/run/root\"\n",
            "mkdir -p \"$BASE/out/gl/run/scratch\"\n",
            "sed \"s|@BASE@|$BASE|g\" > \"$BASE/out/gl/run/scratch/package-0.json\" <<'ATL_EOF'\n",
            "{\"apk\": \"@BASE@/out/gl/apk/x.apk\"}\n",
            "ATL_EOF\n",
            "[ -x \"$BASE/out/cargo/release/atld\" ] || { echo \"remote run gl/run: out/cargo/release/atld is not an executable\" >&2; exit 4; }\n",
            "env -i PATH=/usr/local/bin:/usr/bin:/bin HOME=\"$HOME\" XDG_RUNTIME_DIR=\"/run/user/$(id -u)\" LD_LIBRARY_PATH=\"$BASE/lib\" FONTCONFIG_FILE=\"$BASE/out/gl/run/scratch/fonts.conf\" timeout -k 10 120 \"$BASE/out/cargo/release/atld\" \"--root\" \"$BASE/out/gl/run/root\" < /dev/null > \"$BASE/out/gl/run/scratch/atld.log\" 2>&1 &\n",
            "pid=$!\n",
            "wait \"$pid\"\n",
            "rc=$?\n",
            "kill -KILL -- \"-$pid\" 2>/dev/null || true\n",
            "printf '%s\\n' \"$rc\" > \"$BASE/out/gl/run/scratch/atld.rc\"\n",
            "date +%s%3N > \"$BASE/out/gl/run/scratch/end.ms\"\n",
            "echo \"atld rc=$rc, remote run gl/run\" >&2\n",
            "tar -c -C \"$BASE/out\" --ignore-failed-read 'gl/run/root/log' 'gl/run/scratch/atld.log' 'gl/run/scratch/atld.rc' 'gl/run/scratch/end.ms'\n",
        );
        assert_eq!(plan.script("atl-next-run/slot-9"), want);
    }

    /// A session run: after the launcher check, the one `wayland-*` socket of the desktop session is found, and atld's environment names it. The count check is what refuses a second compositor's socket.
    #[test]
    fn session_run_names_the_targets_one_wayland_socket() {
        let plan = Plan {
            tag: "run-apk/app".to_owned(),
            root: "run-apk/root".to_owned(),
            scratch: "run-apk/scratch".to_owned(),
            env: vec![("RUST_LOG".to_owned(), "info".to_owned())],
            program: "@BASE@/out/cargo/release/atld".to_owned(),
            args: vec!["--ui".to_owned(), "gtk".to_owned()],
            files: Vec::new(),
            lib_dirs: vec!["lib".to_owned()],
            limit: 300,
            session: true,
        };
        let s = plan.script("atl-next-run/slot-3");
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
        assert!(s.contains(" XDG_RUNTIME_DIR=\"/run/user/$(id -u)\" LD_LIBRARY_PATH=\"$BASE/lib\" WAYLAND_DISPLAY=\"$WL\" RUST_LOG=\"info\" timeout -k 10 300 "), "{s}");
    }

    /// A run without a session has no discovery: its environment names no Wayland socket.
    #[test]
    fn plain_run_has_no_session_discovery() {
        let plan = Plan {
            tag: "gl/run".to_owned(),
            root: "gl/run/root".to_owned(),
            scratch: "gl/run/scratch".to_owned(),
            env: Vec::new(),
            program: "@BASE@/out/cargo/release/atld".to_owned(),
            args: Vec::new(),
            files: Vec::new(),
            lib_dirs: Vec::new(),
            limit: 120,
            session: false,
        };
        let s = plan.script("b");
        assert!(!s.contains("WL_DIR"), "{s}");
        assert!(!s.contains("WAYLAND_DISPLAY"), "{s}");
    }

    /// A record of one slot file, one slot tree holding a link to itself (`com.android.art -> .`, the ART staging's alias) and one AOSP file: the names, `lib/` from the LLVM runtimes, the link kept as a link in the stage, and the break removing exactly `lib/libunwind.so.1`.
    #[test]
    fn bundle_lists_record_entries_then_lib_and_the_break_drops_libunwind() {
        let dir = temp("bundle");
        let out = dir.join("slot");
        let tree = dir.join("tree");
        let art = out.join("gl-art");
        std::fs::create_dir_all(art.join("lib64")).unwrap();
        std::fs::write(art.join("lib64/a.so"), b"x").unwrap();
        std::os::unix::fs::symlink(".", art.join("com.android.art")).unwrap();
        std::fs::create_dir_all(out.join("device")).unwrap();
        std::fs::write(out.join("device/a.so"), b"x").unwrap();
        // a file link to a slot file: bundled as that file's content, its absolute target not kept
        std::os::unix::fs::symlink(out.join("device/a.so"), art.join("lib64/b.so")).unwrap();
        std::fs::create_dir_all(&tree).unwrap();
        std::fs::write(tree.join("x.kcm"), b"x").unwrap();
        // a record file that is a symlink (slot-4's libglapi.so.0 is one): bundled from the real file
        std::os::unix::fs::symlink("a.so", out.join("device/link.so")).unwrap();
        let inputs = Inputs {
            files: vec![
                out.join("device/a.so"),
                out.join("device/link.so"),
                tree.join("x.kcm"),
            ],
            dirs: vec![art],
        };
        write_record(&out, "gl", &out, &tree, &inputs).unwrap();
        let full = Bundle::of("gl", &out, None).unwrap();
        assert_eq!(
            names(&full),
            [
                "lib/libc++.so.1",
                "lib/libc++abi.so.1",
                "lib/libunwind.so.1",
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
        let broken = Bundle::of("gl", &out, Some(Break::DropLibunwind)).unwrap();
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
}
