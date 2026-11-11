// SPDX-License-Identifier: MIT
//! `xtask test nb-isa`: ART's `-Xnative-bridge-isa:<isa>` (patches/aosp/art 0004) on the converter's ART, started by atl-app-launcher with the launcher's bridge options (`-XX:NativeBridge=libatl_native_bridge.so -Xforce-nb-testing`, passed by this gate since the class is no `--app`) and libatl_native_bridge on the library path.
//!
//! Checks:
//! 1. delivery: `-Xnative-bridge-isa:arm64` with no arm64 engine staged -> the bridge's `initialize` refuses, naming `arm64` (its stderr line: the option reached it through `InitNonZygoteOrPostFork`), and libnativebridge's cpuinfo mount names `cpuinfo.arm64` (through `PreInitializeNativeBridge`).
//! 2. parse: `-Xnative-bridge-isa:<not an ISA>` -> ART refuses at parse, naming the option, and the VM is not created.
//!
//! Breaks (`--break`): `delivery` drops the option from check 1 (initialize then sees the host ISA: no refusal), `parse` passes a valid ISA to check 2 (no refusal); each must turn its check red.
//! Without the option the bridge path is the existing app gates' (`app`, `nb-identity`), unchanged.
//! Default: mutating (builds the fixture into `<out>/nb-isa`, stages the bridge into `<out>/nb-isa-lib` and ART into `<out>/nb-isa-art`).

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result};

use crate::host_art::{ArtRun, HostArt};
use crate::verdict;

const CMD: &str = "nb-isa";
const FIXTURE: &str = "xtask-harness/testdata/nb-isa/java";
/// A foreign ISA no engine is staged for in this gate's ART root: the QEMU engine's guest ISA (atl-native-bridge engine/qemu `GUEST_ISA`).
const FOREIGN: &str = "arm64";
/// Not an ART instruction set (art/libartbase/arch/instruction_set.cc `GetInstructionSetFromString`).
const NOT_AN_ISA: &str = "notanisa";
/// The patch's parse refusal, minus the value (art/runtime/parsed_options.cc).
const PARSE_REFUSAL: &str = "is not a valid instruction set";

#[derive(clap::Args)]
pub struct Args {
    /// The host arch (an arches.toml row with `[host]`).
    #[arg(long)]
    host: String,
    /// Java pipeline build directory (default: this checkout's slot, see `xtask slot`; else <workspace>/out/java/build).
    #[arg(long)]
    java_out: Option<PathBuf>,
    /// Native build directory (default: this checkout's slot; else <workspace>/out/native/<host>).
    #[arg(long)]
    out: Option<PathBuf>,
    /// Deliberate break that must turn its check red: `delivery` or `parse`.
    #[arg(long, value_parser = ["delivery", "parse"])]
    r#break: Option<String>,
    /// Print each launcher command and its output.
    #[arg(long)]
    verbose: bool,
}

pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    let out = crate::build_state::gate_out(root, args.out.as_deref(), &args.host, CMD)?;
    let mut host = HostArt::new(
        root,
        &args.host,
        args.java_out.as_deref(),
        &out,
        "nb-isa-art",
        args.verbose,
    )?;
    let nb_dir = crate::cdylib::stage(
        root,
        &out,
        "atl-native-bridge",
        // not below <out>/nb-isa: `compile` recreates that dir
        "nb-isa-lib",
        atl_native_bridge::LIBRARY,
    )?;
    // the launcher's NativeBridge option names the library by soname (atl-property-service default area): it must be on the library path
    let joined: OsString =
        std::env::join_paths([&host.lib_path, &nb_dir]).context("joining the library path")?;
    host.lib_path = PathBuf::from(joined);
    let fixture = host.compile(&root.join(FIXTURE), &out.join(CMD))?;
    let area = fixture.area.1.clone();
    // the launcher's own bridge options (atl-app-launcher `native_bridge_options`), which it adds only with `--app`: this class is no app
    let nb_load = format!("-XX:NativeBridge={}", atl_native_bridge::LIBRARY);
    let run = |extra: &[String]| -> Result<ArtRun> {
        let vm: Vec<&str> = [nb_load.as_str(), "-Xforce-nb-testing"]
            .into_iter()
            .chain(extra.iter().map(String::as_str))
            .collect();
        host.launch(&fixture, &area, &vm, "NbIsa", &[])
    };

    let mut fails = Vec::new();
    // 1. delivery
    let vm: Vec<String> = match args.r#break.as_deref() {
        Some("delivery") => Vec::new(),
        _ => vec![format!("-Xnative-bridge-isa:{FOREIGN}")],
    };
    let r = run(&vm)?;
    // both ART consumers: InitNonZygoteOrPostFork -> the bridge's `initialize` (its refusal), PreInitializeNativeBridge -> libnativebridge's cpuinfo mount (its path names the ISA; the mount itself needs privileges the gate lacks)
    let consumers = [
        ("initialize", "atl-native-bridge: initialize:".to_owned()),
        ("cpuinfo", format!("cpuinfo.{FOREIGN}")),
    ];
    for (what, marker) in consumers {
        let hit = r
            .stderr
            .lines()
            .find(|l| l.contains(&marker) && l.contains(FOREIGN));
        match hit {
            Some(line) => println!("delivery {what}: PASS {}", line.trim()),
            None => {
                let msg = format!(
                    "delivery {what}: no line with `{marker}` naming {FOREIGN} in stderr ({})",
                    r.status
                );
                println!("delivery {what}: FAIL {msg}");
                fails.push(msg);
            }
        }
    }
    // 2. parse
    let isa = if args.r#break.as_deref() == Some("parse") {
        FOREIGN
    } else {
        NOT_AN_ISA
    };
    let r = run(&[format!("-Xnative-bridge-isa:{isa}")])?;
    let line = r
        .stderr
        .lines()
        .find(|l| l.contains("-Xnative-bridge-isa:") && l.contains(PARSE_REFUSAL));
    match (line, r.ok) {
        (Some(line), false) => println!("parse: PASS {}", line.trim()),
        _ => {
            let msg = format!(
                "parse: -Xnative-bridge-isa:{isa} not refused at parse ({})",
                r.status
            );
            println!("parse: FAIL {msg}");
            fails.push(msg);
        }
    }
    println!("{}", verdict::gate(CMD, &fails));
    Ok(if fails.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}
