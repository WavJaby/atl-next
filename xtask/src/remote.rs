// SPDX-License-Identifier: MIT
//! `cargo xtask remote`: runs WSL-built outputs on a Linux desktop over ssh (the owner's laptop, plan L1).
//!
//! The record: a gate's last run in a slot writes what its runtime read (atld_run.rs `record_inputs`, via [`write_record`]): host files, and host directories whose trees the bundle takes whole.
//! `push` bundles that record, mapped to `out/`, `java/` and `src/aosp/` names, plus `lib/`: Debian's LLVM 19 runtimes (a labelled patch until our pinned runtimes ship in an install tree, plan L1 / D1).
//! A directory is replicated with its symlinks kept as links and never followed (the ART tree's `com.android.art -> .` would loop); files are hard links, copies across filesystems.
//! Dry run by default. `--apply` stages the bundle under `<out>/remote/bundle/<gate>` (with a manifest of sha256 + size, or the link target) and runs `rsync -a --delete` to `~/atl-next-run/<slot>/` on the host.
//! `test` runs the prerequisite check on the target first: `ldd` over every ELF of the bundle, with the bundle's directories on `LD_LIBRARY_PATH`. Each `not found` soname is named with the file that needs it, a file the push did not deliver is named, and any of them is red. The gate's runtime half on the target is not implemented ([`runtime_half`]), so `test` is red after a green prerequisite.
//! Host: an ssh alias from the operator's WSL `~/.ssh/config`. The alias is the only handle: no address, user or key goes into the repo or a flag. `local:<absolute dir>` is a directory standing in for the target (the local gate).
//! Limit: `ldd` here also searches this host's own libraries, so on WSL a missing bundle copy still resolves. The `drop-libunwind` break is red only on a host without its own `libunwind.so.1`.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Output, Stdio};

use anyhow::{Context, Result, bail, ensure};
use sha2::{Digest, Sha256};

use crate::{gate_goals, provided_gate, slot, verdict};

/// The bundle's `lib/`: Debian's LLVM 19 runtimes. Arch ships libc++ 23 but no LLVM `libunwind.so.1` (plan L1 section 3). A labelled patch: the ideal is our pinned runtimes (runtimes.rs) in a packaged install tree (plan D1).
const LLVM_LIB_DIR: &str = "/usr/lib/llvm-19/lib";
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
/// Bundle names of a record's roots: the slot, the java out and the AOSP tree.
const ROOT_NAMES: [(&str, &str); 3] = [("out", "out"), ("java", "java"), ("tree", "src/aosp")];
/// The target's directory for a slot's bundle, relative to the host's home: `~/atl-next-run/<slot>/`.
const REMOTE_ROOT: &str = "atl-next-run";
/// `<out>/remote/...`: the record and the staged bundle.
const REMOTE_DIR: &str = "remote";
/// The manifest file at the bundle's root: `sha256  size  name` per file, `link  target  name` per link.
const MANIFEST: &str = "bundle.manifest";
/// ssh options: never prompt, never hang. The alias check is the first contact with the host.
const SSH_OPTS: [&str; 4] = ["-o", "BatchMode=yes", "-o", "ConnectTimeout=20"];
const MIB: f64 = 1024.0 * 1024.0;

#[derive(clap::Subcommand)]
pub enum Remote {
    /// Bundles a gate's runtime inputs (its last run's record in the slot) and `lib/`, and copies them to an ssh alias or a local dir.
    ///
    /// Dry run by default: prints the bundle summary (`--list`: every entry) and the rsync it would run. `--apply` stages the bundle under `<out>/remote/bundle/<gate>` with a manifest, then runs `rsync -a --delete` to `~/atl-next-run/<slot>/` on the host.
    /// The record is written by a gate's run in the slot (`xtask test <gate>`); without it this names that command.
    Push(PushArgs),
    /// The prerequisite check on the target: `ldd` over the bundle, every `not found` named with the file that needs it. The gate's runtime half then runs there: not implemented, so red.
    ///
    /// Dry run by default: prints the check it would run. `--apply` runs it over ssh (or locally for `local:`). Run `push --apply` first: a file the push did not deliver is named as missing.
    Test(TestArgs),
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
}

pub fn run(root: &Path, cmd: &Remote) -> Result<ExitCode> {
    match cmd {
        Remote::Push(a) => push(root, a),
        Remote::Test(a) => test(root, a),
    }
}

fn push(root: &Path, a: &PushArgs) -> Result<ExitCode> {
    let host = Host::parse(&a.host)?;
    let out = out_of(root, a.out.as_deref())?;
    let slot = slot_name(&out)?;
    let bundle = Bundle::of(&a.gate, &out, a.brk)?;
    if a.list {
        for e in &bundle.entries {
            println!("{}\t{}", e.name, e.src.display());
        }
    }
    println!("{}", bundle.summary());
    let stage = stage_dir(&out, &a.gate);
    if !a.apply {
        println!(
            "dry run: would stage {} and rsync -a --delete it to {}; --apply copies",
            stage.display(),
            host.target(&slot)
        );
        return Ok(ExitCode::SUCCESS);
    }
    host.reach()?;
    bundle.stage(&stage)?;
    host.sync(&stage, &slot)?;
    println!(
        "pushed {} entries to {}",
        bundle.entries.len(),
        host.target(&slot)
    );
    Ok(ExitCode::SUCCESS)
}

fn test(root: &Path, a: &TestArgs) -> Result<ExitCode> {
    let host = Host::parse(&a.host)?;
    let out = out_of(root, a.out.as_deref())?;
    let slot = slot_name(&out)?;
    let bundle = Bundle::of(&a.gate, &out, a.brk)?;
    let elf = bundle.elf_names()?;
    let dirs = bundle.lib_dirs(&elf);
    let script = prereq_script(&host.base(&slot), &elf, &dirs)?;
    if !a.apply {
        println!("{}", bundle.summary());
        println!(
            "dry run: prerequisite check on {}: ldd over {} ELF files, {} library dirs on LD_LIBRARY_PATH; --apply runs it",
            host.target(&slot),
            elf.len(),
            dirs.len()
        );
        println!("dry run: the gate's runtime half would follow: not implemented (red)");
        return Ok(ExitCode::SUCCESS);
    }
    let text = host.run_script(&script)?;
    let prereq = parse_prereq(&text);
    let mut fails = prereq.problems();
    if fails.is_empty() {
        println!(
            "remote prereq {}: PASS, {} ELF checked on {}",
            a.gate,
            prereq.checked,
            host.label()
        );
    } else {
        for f in &fails {
            println!("remote prereq {}: {f}", a.gate);
        }
    }
    if let Err(e) = runtime_half(&a.gate) {
        fails.push(format!("{e:#}"));
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

/// The gate's runtime half on the target: not implemented. The gate's run builds inside itself (cargo for the probe library, the APK with aapt2, javac and d8, its manifest read with aapt2) and starts atld from host paths. Running it on the target needs a serialized runtime and a target runner: a plan L1 open decision.
fn runtime_half(gate: &str) -> Result<()> {
    bail!(
        "gate `{gate}`: its runtime half is not remotable yet (no target runner; the gate builds its inputs inside its run, plan L1 open decision)"
    )
}

/// The host a bundle goes to.
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

    /// Runs the prerequisite script on the target (`sh -s`, the script on stdin); its stdout. A script that cannot reach its directory fails here.
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
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("starting the prerequisite check on {}", self.label()))?;
        child
            .stdin
            .take()
            .context("no stdin for the prerequisite script")?
            .write_all(script.as_bytes())?;
        let out: Output = child.wait_with_output()?;
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        if text.is_empty() && !out.status.success() {
            bail!(
                "prerequisite check on {} did not run: {}",
                self.label(),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(text)
    }
}

fn ssh(alias: &str) -> Command {
    let mut c = Command::new("ssh");
    c.args(SSH_OPTS).arg(alias);
    c
}

/// An ssh alias: a `Host` name, never `user@address` or `host:path`.
fn valid_alias(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= 64
        && !text.starts_with('-')
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
        gate_goals::of(gate)?;
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
    s.push_str(
        "if [ -f \"$f\" ]; then echo \"== $f\"; ldd \"$f\" 2>&1; else echo \"!missing $f\"; fi\n",
    );
    s.push_str("done\n");
    Ok(s)
}

/// `s` as one POSIX shell word.
fn sh_quote(s: &str) -> String {
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
    /// One line per problem, as `test` prints and fails on.
    fn problems(&self) -> Vec<String> {
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

/// [`prereq_script`]'s output: `== file` opens a file's `ldd` lines, `!missing file` a file the target lacks. Lines with `=> not found` are unresolved sonames. `ldd:` lines are failures, except `not a dynamic executable` (a static file has nothing to resolve).
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
        } else if line.starts_with("ldd:") && !line.contains("not a dynamic executable") {
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
            "if [ -f \"$f\" ]; then echo \"== $f\"; ldd \"$f\" 2>&1; else echo \"!missing $f\"; fi\n",
            "done\n",
        );
        assert_eq!(s, want);
        // names with shell metacharacters stay one word; a newline cannot be quoted onto one line
        let odd = prereq_script("b", &["a@1.0;x'y.so".to_owned()], &[]).unwrap();
        assert!(odd.contains("for f in 'a@1.0;x'\\''y.so'; do"), "{odd}");
        assert!(prereq_script("b", &["a\nb".to_owned()], &[]).is_err());
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
