// SPDX-License-Identifier: MIT
//! The stub's Landlock domain bites (plan F1.2 B4 gate): this test binary re-runs itself under `atl-sandbox-stub`, allowed only its own ELF closure (atl_broker::elf_closure, binderd's rule) plus one control file.
//! Probe: the control file reads (the probe answers at all), `/etc/passwd` -- readable to every user, so only Landlock can refuse it -- fails with EACCES.
//! Deliberate break: a ruleset that handles no filesystem right -> /etc/passwd opens -> this fails.
#![expect(
    clippy::disallowed_types,
    reason = "a test: it runs the stub binary under test"
)]

use std::io::Write;
use std::process::Command;

const MODE_ENV: &str = "ATL_STUB_PROBE";
const CONTROL_ENV: &str = "ATL_STUB_CONTROL";
/// A file every user may read: a refusal can only be the domain's.
const FORBIDDEN: &str = "/etc/passwd";

fn outcome<T>(r: std::io::Result<T>) -> String {
    match r {
        Ok(_) => "ok".into(),
        Err(e) => format!("err:{}", e.raw_os_error().unwrap_or(-1)),
    }
}

/// Prints `probe: control=.. forbidden=..`; a no-op unless run by `domain_refuses` with MODE_ENV set.
#[test]
fn probe() {
    if std::env::var_os(MODE_ENV).is_none() {
        return;
    }
    let control = outcome(std::fs::read(std::env::var(CONTROL_ENV).unwrap()));
    let forbidden = outcome(std::fs::read(FORBIDDEN));
    let _ = writeln!(
        std::io::stdout(),
        "probe: control={control} forbidden={forbidden}"
    );
}

#[test]
fn domain_refuses() {
    let exe = std::env::current_exe().unwrap();
    let control = std::env::temp_dir().join(format!("atl-stub-control-{}", std::process::id()));
    std::fs::write(&control, "x").unwrap();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_atl-sandbox-stub"));
    for f in atl_broker::elf_closure(&exe).unwrap() {
        cmd.arg("--ro").arg(f);
    }
    let out = cmd
        .arg("--ro")
        .arg(&control)
        .arg("--")
        .arg(&exe)
        .args(["--exact", "probe", "--nocapture", "--test-threads=1"])
        .env(MODE_ENV, "1")
        .env(CONTROL_ENV, &control)
        .output()
        .unwrap();
    std::fs::remove_file(&control).unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let line = stdout
        .lines()
        .find_map(|l| l.split_once("probe: ").map(|(_, r)| r.to_owned()))
        .unwrap_or_else(|| panic!("no probe line: {stdout}{stderr}"));
    let get = |k: &str| {
        line.split_whitespace()
            .find_map(|kv| kv.strip_prefix(&format!("{k}=")).map(str::to_owned))
            .unwrap()
    };
    assert_eq!(get("control"), "ok", "{line}");
    assert_eq!(get("forbidden"), format!("err:{EACCES}"), "{line}");
}

/// EACCES (asm-generic/errno-base.h: 13), Landlock's refusal for filesystem access.
const EACCES: i32 = 13;
