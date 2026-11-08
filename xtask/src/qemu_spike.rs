// SPDX-License-Identifier: MIT
//! `xtask test qemu-spike`: the QEMU engine spike (crates/atl-qemu-engine, QEMU bridge plan Q2) against its fixture, then every break.
//!
//! Library: the `--pic` build's own link line for `qemu-<target>` (`ninja -t commands`), linked `-shared` instead, `main` aliased `atl_qemu_user_main` (`--defsym`), QEMU's plugin `--dynamic-list` dropped. No QEMU file is compiled differently: this is D7 with a link-time change only.
//! Fixture: `xtask/testdata/qemu-spike/fixture.c`, static bionic by the NDK clang; QEMU's `main` loads it with `-B <guest base>`.
//! Harness: `cargo test -p atl-qemu-engine --features qemu --test spike` with `ATL_QEMU_BUILD` (layouts), `ATL_QEMU_SPIKE_SO`, `ATL_QEMU_SPIKE_FIXTURE`; its own target dir under the spike dir, so the cfg breaks rebuild only it.
//! Pass: the plain run prints `spike: (x) ok` for every gate and exits 0; each break run leaves its gate without `ok` and exits nonzero.
//! Breaks: `no-p1` relinks with the pin's unpatched `cpu_loop.c` (compiled with the build's own command: P1 removed, nothing else); `guest-base` runs `-B 4096`; the rest are atl-qemu-engine build.rs `BREAKS`.
//! Output: `<out>/qemu/<target>-linux-user-pic/spike/`: the libraries, the fixture, one log per run.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use anyhow::{Context, Result, bail, ensure};

use crate::git::git_raw;
use crate::jni_fixture::{self, NdkTarget};
use crate::qemu::{self, Build, GUEST_API};
use crate::sources;
use crate::verdict::{Failure, Verdict};

const FIXTURE_SRC: &str = "xtask/testdata/qemu-spike/fixture.c";
/// P1's call site: the one object the `no-p1` break replaces.
const CPU_LOOP: &str = "linux-user/aarch64/cpu_loop.c";
const PACKAGE: &str = "atl-qemu-engine";
const GATES: [char; 5] = ['a', 'b', 'c', 'd', 'e'];
/// The non-identity guest base of the `guest-base` break: 1 TiB, free in an x86_64 process (PIE and heap sit near 0x55.., mmap near 0x7f..) and page aligned.
/// Not 4096: with any nonzero base QEMU also maps guest page 0 (`pgb_addr_set`), and host 4096 is below `mmap_min_addr` (65536), so QEMU refuses to start and gate (c) is never reached.
const SHIFTED_BASE: &str = "0x10000000000";

#[derive(clap::Args)]
pub struct Args {
    /// arches.toml guest row whose `qemu_target` is driven (the spike engine is aarch64-only).
    #[arg(long, default_value = qemu::DEFAULT_ARCH)]
    pub arch: String,
    /// Out dir (default: as for `xtask qemu`).
    #[arg(long)]
    pub out: Option<PathBuf>,
    /// Only the plain run, no breaks.
    #[arg(long)]
    pub no_breaks: bool,
}

/// One harness run: which library, guest base and engine break, and the gate it must turn red (None = all green).
struct RunSpec {
    name: &'static str,
    unpatched: bool,
    base: &'static str,
    engine_break: Option<&'static str>,
    red: Option<char>,
}

const RUNS: &[RunSpec] = &[
    RunSpec {
        name: "plain",
        unpatched: false,
        base: "0",
        engine_break: None,
        red: None,
    },
    RunSpec {
        name: "no-p1",
        unpatched: true,
        base: "0",
        engine_break: None,
        red: Some('a'),
    },
    RunSpec {
        name: "ret-reg",
        unpatched: false,
        base: "0",
        engine_break: Some("ret-reg"),
        red: Some('a'),
    },
    RunSpec {
        name: "guest-base",
        unpatched: false,
        base: SHIFTED_BASE,
        engine_break: None,
        red: Some('c'),
    },
    RunSpec {
        name: "fault-route",
        unpatched: false,
        base: "0",
        engine_break: Some("fault-route"),
        red: Some('d'),
    },
    RunSpec {
        name: "chain-order",
        unpatched: false,
        base: "0",
        engine_break: Some("chain-order"),
        red: Some('e'),
    },
];

pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    let b = qemu::plan(root, &args.arch, args.out.as_deref(), true)?;
    qemu::execute(root, &b)?;
    let dir = b.dir.join("spike");
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let so = link(&b, &dir.join("libqemu-spike.so"), None)?;
    let fixture = fixture(root, &b, &dir)?;
    let runs: Vec<&RunSpec> = RUNS
        .iter()
        .filter(|r| !args.no_breaks || r.red.is_none())
        .collect();
    let mut fails = Vec::new();
    let mut unpatched_so = None;
    for spec in &runs {
        let lib = if spec.unpatched {
            match &unpatched_so {
                Some(p) => p,
                None => unpatched_so.insert(unpatched(root, &b, &dir)?),
            }
        } else {
            &so
        };
        if let Err(e) = harness(root, &b, &dir, spec, lib, &fixture) {
            fails.push(Failure::step(spec.name, format!("{e:#}")));
        }
    }
    let verdict = Verdict {
        cmd: "qemu-spike",
        total: Some((runs.len(), "runs")),
        skipped: Vec::new(),
        failures: fails,
    };
    println!("{}", verdict.line());
    Ok(if verdict.failures.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

/// The executable's link command, as words (meson writes it unquoted).
fn link_words(b: &Build) -> Result<Vec<String>> {
    let out = Command::new(&b.ninja)
        .arg("-C")
        .arg(b.build_dir())
        .args(["-t", "commands", &b.exe_name()])
        .output()
        .context("ninja -t commands")?;
    ensure!(
        out.status.success(),
        "ninja -t commands {} failed",
        b.exe_name()
    );
    let text = String::from_utf8(out.stdout)?;
    let line = text.lines().last().context("no link command")?;
    ensure!(
        !line.contains('\''),
        "quoted link command, not handled: {line}"
    );
    Ok(line.split_whitespace().map(str::to_owned).collect())
}

/// Links the PIC build's objects into `so`, `replace` = (object, substitute) swapped in.
fn link(b: &Build, so: &Path, replace: Option<(&str, &Path)>) -> Result<PathBuf> {
    let words = link_words(b)?;
    let mut cmd = Command::new(&words[0]);
    let mut it = words[1..].iter();
    let mut replaced = false;
    while let Some(w) = it.next() {
        match w.as_str() {
            "-o" => {
                it.next();
                cmd.arg("-shared").arg("-o").arg(so);
            }
            // QEMU's plugin export list: a shared library exports its defaults
            "-Xlinker" => {
                let next = it.next().context("-Xlinker without argument")?;
                ensure!(
                    next.starts_with("--dynamic-list="),
                    "unexpected -Xlinker {next}"
                );
            }
            "-pie" => bail!("PIE link line: the PIC build is configured with --disable-pie"),
            o if replace.is_some_and(|(obj, _)| o == obj) => {
                cmd.arg(replace.map(|(_, p)| p).context("replace")?);
                replaced = true;
            }
            o => {
                cmd.arg(o);
            }
        }
    }
    ensure!(
        replace.is_none() || replaced,
        "{:?}: not in the link line",
        replace.map(|r| r.0)
    );
    cmd.arg("-Wl,--defsym=atl_qemu_user_main=main")
        .current_dir(b.build_dir());
    let log = so.with_extension("link.log");
    let out = cmd.output().context("linking the spike library")?;
    std::fs::write(&log, [out.stdout, out.stderr].concat())?;
    ensure!(out.status.success(), "link failed, log {}", log.display());
    Ok(so.to_owned())
}

/// The library with the pin's `cpu_loop.c` (P1 removed), compiled by the build's own command for that file.
fn unpatched(root: &Path, b: &Build, dir: &Path) -> Result<PathBuf> {
    let pin = sources::load(root)?
        .git
        .get(qemu::UPSTREAM)
        .context("no [qemu] pin")?
        .commit
        .clone();
    let text = git_raw(&b.src, &["show", &format!("{pin}:{CPU_LOOP}")])?;
    let src = dir.join("cpu_loop.unpatched.c");
    std::fs::write(&src, text)?;
    let db_path = b.build_dir().join("compile_commands.json");
    let db: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&db_path)?)?;
    let entry = db
        .as_array()
        .context("compile_commands.json: not an array")?
        .iter()
        .find(|e| e["file"].as_str().is_some_and(|f| f.ends_with(CPU_LOOP)))
        .with_context(|| format!("no {CPU_LOOP} entry"))?;
    let (Some(command), Some(file), Some(output)) = (
        entry["command"].as_str(),
        entry["file"].as_str(),
        entry["output"].as_str(),
    ) else {
        bail!("{CPU_LOOP} entry lacks command/file/output");
    };
    let obj = dir.join("cpu_loop.unpatched.o");
    let cmd = command
        .replace(output, &obj.display().to_string())
        .replace(file, &src.display().to_string());
    let log = dir.join("cpu_loop.unpatched.log");
    let out = Command::new("sh")
        .args(["-c", &cmd])
        .current_dir(b.build_dir())
        .output()
        .context("compiling the unpatched cpu_loop.c")?;
    std::fs::write(&log, [out.stdout, out.stderr].concat())?;
    ensure!(
        out.status.success(),
        "unpatched cpu_loop.c failed, log {}",
        log.display()
    );
    link(b, &dir.join("libqemu-spike-no-p1.so"), Some((output, &obj)))
}

fn fixture(root: &Path, b: &Build, dir: &Path) -> Result<PathBuf> {
    let ndk = jni_fixture::default_ndk(root)?;
    let bin = dir.join("fixture");
    let src = root.join(FIXTURE_SRC);
    let status = NdkTarget::new(root, &ndk, &b.arch, GUEST_API)?
        .cmd("clang")
        .args(["-static", "-O1", "-Wall", "-Werror", "-o"])
        .arg(&bin)
        .arg(&src)
        .status()
        .context("running the NDK clang")?;
    ensure!(status.success(), "NDK clang failed on {}", src.display());
    Ok(bin)
}

/// One harness run; Ok when it ends as `spec` says.
fn harness(
    root: &Path,
    b: &Build,
    dir: &Path,
    spec: &RunSpec,
    so: &Path,
    fixture: &Path,
) -> Result<()> {
    let mut cmd = crate::cargo_command();
    cmd.args([
        "test",
        "--locked",
        "-p",
        PACKAGE,
        "--features",
        "qemu",
        "--test",
        "spike",
        "--target-dir",
    ])
    .arg(dir.join("cargo"))
    .args(["--", "--guest-base", spec.base])
    .env("ATL_QEMU_BUILD", b.build_dir())
    .env("ATL_QEMU_SPIKE_SO", so)
    .env("ATL_QEMU_SPIKE_FIXTURE", fixture)
    .current_dir(root);
    match spec.engine_break {
        Some(x) => cmd.env("ATL_QEMU_ENGINE_BREAK", x),
        None => cmd.env_remove("ATL_QEMU_ENGINE_BREAK"),
    };
    let out = cmd.output().context("running cargo test")?;
    let log = dir.join(format!("{}.log", spec.name));
    std::fs::write(
        &log,
        [out.stdout.as_slice(), out.stderr.as_slice()].concat(),
    )?;
    let text = String::from_utf8_lossy(&out.stdout);
    let ok = |g: char| text.lines().any(|l| l == format!("spike: ({g}) ok"));
    match spec.red {
        None => {
            let red: Vec<char> = GATES.into_iter().filter(|g| !ok(*g)).collect();
            ensure!(
                red.is_empty() && out.status.success(),
                "gates {red:?} not ok ({}), log {}",
                out.status,
                log.display()
            );
        }
        Some(g) => {
            ensure!(!ok(g), "break left gate ({g}) ok, log {}", log.display());
            ensure!(
                !out.status.success(),
                "break run exited 0, log {}",
                log.display()
            );
        }
    }
    Ok(())
}
