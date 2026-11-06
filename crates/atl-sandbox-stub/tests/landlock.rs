// SPDX-License-Identifier: MIT
//! The stub's Landlock domain bites (plan F1.2 B4 gate): this test binary re-runs itself under `atl-sandbox-stub`, allowed only its own ELF closure (atl_broker::elf_closure, binderd's rule) plus one control file.
//! Probe: the control file reads (the probe answers at all), `/etc/passwd` -- readable to every user, so only Landlock can refuse it -- fails with EACCES; with ABI >= 4 a TCP connect to a listener of the parent is refused too, below that it is reported as not enforceable.
//! Deliberate break: a ruleset that handles no filesystem right -> /etc/passwd opens -> this fails.
#![expect(
    clippy::disallowed_types,
    reason = "a test: it runs the stub binary under test"
)]

use std::io::Write;
use std::net::{TcpListener, TcpStream};
use std::process::Command;

const MODE_ENV: &str = "ATL_STUB_PROBE";
const CONTROL_ENV: &str = "ATL_STUB_CONTROL";
const PORT_ENV: &str = "ATL_STUB_PORT";
/// A file every user may read: a refusal can only be the domain's.
const FORBIDDEN: &str = "/etc/passwd";

fn outcome<T>(r: std::io::Result<T>) -> String {
    match r {
        Ok(_) => "ok".into(),
        Err(e) => format!("err:{}", e.raw_os_error().unwrap_or(-1)),
    }
}

/// Prints `probe: control=.. forbidden=.. tcp=..`; a no-op unless run by `domain_refuses` with MODE_ENV set.
#[test]
fn probe() {
    if std::env::var_os(MODE_ENV).is_none() {
        return;
    }
    let control = outcome(std::fs::read(std::env::var(CONTROL_ENV).unwrap()));
    let forbidden = outcome(std::fs::read(FORBIDDEN));
    let tcp = outcome(TcpStream::connect((
        "127.0.0.1",
        std::env::var(PORT_ENV).unwrap().parse::<u16>().unwrap(),
    )));
    let _ = writeln!(
        std::io::stdout(),
        "probe: control={control} forbidden={forbidden} tcp={tcp}"
    );
}

#[test]
fn domain_refuses() {
    let exe = std::env::current_exe().unwrap();
    let control = std::env::temp_dir().join(format!("atl-stub-control-{}", std::process::id()));
    std::fs::write(&control, "x").unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_atl-sandbox-stub"));
    for f in atl_broker::elf_closure(&exe).unwrap() {
        cmd.arg("--allow").arg(f);
    }
    let out = cmd
        .arg("--allow")
        .arg(&control)
        .arg("--")
        .arg(&exe)
        .args(["--exact", "probe", "--nocapture", "--test-threads=1"])
        .env(MODE_ENV, "1")
        .env(CONTROL_ENV, &control)
        .env(PORT_ENV, port.to_string())
        .output()
        .unwrap();
    std::fs::remove_file(&control).unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let line = stdout
        .lines()
        .find_map(|l| l.split_once("probe: ").map(|(_, r)| r.to_owned()))
        .unwrap_or_else(|| panic!("no probe line: {stdout}{stderr}"));
    let abi: i32 = stderr
        .lines()
        .find_map(|l| l.strip_prefix("sandbox-stub: landlock abi="))
        .and_then(|r| r.split_whitespace().next()?.parse().ok())
        .unwrap_or_else(|| panic!("no abi line: {stderr}"));
    let get = |k: &str| {
        line.split_whitespace()
            .find_map(|kv| kv.strip_prefix(&format!("{k}=")).map(str::to_owned))
            .unwrap()
    };
    assert_eq!(get("control"), "ok", "{line}");
    assert_eq!(get("forbidden"), format!("err:{EACCES}"), "{line}");
    // Landlock ABI 4 (Linux 6.7) handles TCP connect; below it the kernel cannot refuse it here
    if abi >= 4 {
        assert_eq!(get("tcp"), format!("err:{EACCES}"), "{line}");
    } else {
        let _ = writeln!(
            std::io::stderr(),
            "landlock abi={abi}: TCP connect not enforceable on this kernel (tcp={})",
            get("tcp")
        );
    }
}

/// EACCES (asm-generic/errno-base.h: 13), Landlock's refusal for both filesystem and TCP access.
const EACCES: i32 = 13;
