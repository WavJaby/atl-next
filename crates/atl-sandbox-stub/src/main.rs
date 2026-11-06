// SPDX-License-Identifier: MIT
//! atl-sandbox-stub: the in-child launcher stage of a sandbox (plan Q5: "Landlock + seccomp are applied by our in-child launcher stub in every backend"). bwrap runs it in place of the program; it restricts itself with Landlock, then execs the program, which inherits the domain.
//! Usage: `atl-sandbox-stub [--allow PATH]... -- PROGRAM [ARGS]...`. Every filesystem right is handled and only read + execute on each `--allow` file is granted; TCP bind/connect are handled with no rule (denied, ABI >= 4); abstract unix sockets and signals are scoped to the domain (ABI >= 6).
//! Best effort over the kernel's ABI: the achieved ABI and enforcement are printed to stderr as `sandbox-stub: landlock abi=<n> ruleset=<status>`; a kernel that enforces nothing fails the start (exit 2) rather than run the program unconfined.
#![forbid(unsafe_code)]

use std::ffi::OsString;
use std::io::Write;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::ExitCode;

use landlock::{
    ABI, Access, AccessFs, AccessNet, LandlockStatus, PathBeneath, PathFd, Ruleset, RulesetAttr,
    RulesetCreatedAttr, RulesetStatus, Scope,
};

/// The newest Landlock ABI this stub asks for (landlock 0.4.7 knows up to V9; V6 adds the scopes). Older kernels give a subset (best effort), reported on stderr.
const WANT: ABI = ABI::V6;
/// Exit status when the sandbox cannot be set up (bad arguments, no Landlock, exec failed).
const SETUP_FAILED: u8 = 2;

struct Args {
    allow: Vec<PathBuf>,
    program: OsString,
    args: Vec<OsString>,
}

fn parse(mut it: impl Iterator<Item = OsString>) -> Result<Args, String> {
    let mut allow = Vec::new();
    loop {
        match it.next() {
            Some(a) if a == "--allow" => {
                allow.push(PathBuf::from(it.next().ok_or("--allow needs a path")?));
            }
            Some(a) if a == "--" => break,
            Some(a) => return Err(format!("unknown argument {a:?}")),
            None => return Err("missing `-- PROGRAM`".into()),
        }
    }
    let program = it.next().ok_or("missing PROGRAM after --")?;
    Ok(Args {
        allow,
        program,
        args: it.collect(),
    })
}

/// Restricts this process: the ruleset above, `allow` read + execute.
fn restrict(allow: &[PathBuf]) -> Result<(ABI, RulesetStatus), String> {
    let err = |e: landlock::RulesetError| e.to_string();
    let file_rights = AccessFs::Execute | AccessFs::ReadFile;
    let mut created = Ruleset::default()
        .handle_access(AccessFs::from_all(WANT))
        .map_err(err)?
        .handle_access(AccessNet::from_all(WANT))
        .map_err(err)?
        .scope(Scope::AbstractUnixSocket | Scope::Signal)
        .map_err(err)?
        .create()
        .map_err(err)?;
    for p in allow {
        let fd = PathFd::new(p).map_err(|e| format!("{}: {e}", p.display()))?;
        created = created
            .add_rule(PathBeneath::new(fd, file_rights))
            .map_err(err)?;
    }
    let status = created.restrict_self().map_err(err)?;
    let abi = match status.landlock {
        LandlockStatus::Available { effective_abi, .. } => effective_abi,
        other => return Err(format!("landlock unavailable: {other:?}")),
    };
    if status.ruleset == RulesetStatus::NotEnforced {
        return Err("landlock enforced nothing".into());
    }
    Ok((abi, status.ruleset))
}

fn fail(what: &str) -> ExitCode {
    let _ = writeln!(std::io::stderr(), "sandbox-stub: {what}");
    ExitCode::from(SETUP_FAILED)
}

fn main() -> ExitCode {
    let a = match parse(std::env::args_os().skip(1)) {
        Ok(a) => a,
        Err(e) => return fail(&e),
    };
    match restrict(&a.allow) {
        Ok((abi, ruleset)) => {
            let _ = writeln!(
                std::io::stderr(),
                "sandbox-stub: landlock abi={} ruleset={ruleset:?}",
                abi as i32
            );
        }
        Err(e) => return fail(&e),
    }
    #[expect(
        clippy::disallowed_types,
        clippy::disallowed_methods,
        reason = "the in-child stage of the broker's launch: it replaces itself with the sandboxed program, inside the sandbox bwrap built"
    )]
    let e = std::process::Command::new(&a.program).args(&a.args).exec();
    fail(&format!("exec {:?}: {e}", a.program))
}
