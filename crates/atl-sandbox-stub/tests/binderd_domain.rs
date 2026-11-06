// SPDX-License-Identifier: MIT
//! binderd runs inside the stub's Landlock domain (plan F1.2 B4 gate): `Broker::start` launches this test binary in binderd's place -- same bwrap argv, seccomp filter and stub -- and the probe reports what it can open.
//! The stub executable is bound into binderd's root (bwrap must exec it) but is not in the `--ro` grants, so only the domain can refuse it: probe `own=ok` (it answers at all) and `stub=err:EACCES`.
//! Deliberate break: the broker also grants `--ro /` -> the stub opens -> this fails.
//! Probe mode = `CARGO` unset: cargo sets it for every test run; Broker::start clears the environment (bwrap then adds only PWD). The probe stays inside binderd's seccomp allow-list: open, read, write, close, exit -- no stat (std's fs::read would statx).
//! harness = false: a libtest harness would add calls outside that list.

use std::io::{Read, Write};
use std::process::ExitCode;

const STUB: &str = env!("CARGO_BIN_EXE_atl-sandbox-stub");
/// EACCES (asm-generic/errno-base.h: 13), Landlock's refusal.
const EACCES: i32 = 13;

fn outcome(path: &std::path::Path) -> String {
    let mut buf = [0u8; 16];
    match std::fs::File::open(path).and_then(|mut f| f.read(&mut buf)) {
        Ok(_) => "ok".into(),
        Err(e) => format!("err:{}", e.raw_os_error().unwrap_or(-1)),
    }
}

fn probe() -> ExitCode {
    // argv[0]: the path the stub exec'd (current_exe would readlink /proc, absent and outside the filter)
    let own = std::env::args_os()
        .next()
        .map_or_else(|| "no-argv0".into(), |p| outcome(p.as_ref()));
    let stub = outcome(std::path::Path::new(STUB));
    let _ = writeln!(std::io::stderr(), "probe: own={own} stub={stub}");
    ExitCode::SUCCESS
}

fn gate() -> std::io::Result<ExitCode> {
    let exe = std::env::current_exe()?;
    let log_path =
        std::env::temp_dir().join(format!("atl-binderd-domain-{}.log", std::process::id()));
    let log = std::fs::File::create(&log_path)?;
    let (broker, mut process) = atl_broker::Broker::start(&exe, STUB.as_ref(), log)?;
    let status = process.child.wait()?;
    drop(broker);
    let text = std::fs::read_to_string(&log_path)?;
    std::fs::remove_file(&log_path)?;
    let want = format!("probe: own=ok stub=err:{EACCES}");
    let ok = status.success()
        && text
            .lines()
            .any(|l| l.starts_with("sandbox-stub: landlock abi="))
        && text.lines().any(|l| l == want);
    let mut err = std::io::stderr();
    if ok {
        let _ = writeln!(
            err,
            "binderd_domain: pass ({})",
            text.lines().next().unwrap_or("")
        );
        Ok(ExitCode::SUCCESS)
    } else {
        let _ = writeln!(
            err,
            "binderd_domain: FAILED: status {status}, want `{want}`, log:\n{text}"
        );
        Ok(ExitCode::FAILURE)
    }
}

fn main() -> ExitCode {
    if std::env::var_os("CARGO").is_none() {
        return probe();
    }
    gate().unwrap_or_else(|e| {
        let _ = writeln!(std::io::stderr(), "binderd_domain: FAILED: {e}");
        ExitCode::FAILURE
    })
}
