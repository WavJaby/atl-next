// SPDX-License-Identifier: MIT
//! atl-sandbox-stub: the in-child launcher stage of a sandbox (plan Q5: "Landlock + seccomp are applied by our in-child launcher stub in every backend"). bwrap runs it in place of the program; it restricts itself with Landlock, then execs the program, which inherits the domain.
//! Usage: `atl-sandbox-stub [--ro PATH | --rw PATH | --device PATH]... -- PROGRAM [ARGS]...`. Every filesystem right is handled; each path gets only its grant, and a path under no grant is refused. `--ro`: read, list, execute. `--rw`: also write, create, remove, rename. `--device`: rw plus the device ioctls (ABI >= 5).
//! TCP bind/connect are handled with no rule (denied, ABI >= 4); abstract unix sockets and signals are scoped to the domain (ABI >= 6).
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

/// What a path is granted, one flag each (atl-broker's `bwrap::Access`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Grant {
    Ro,
    Rw,
    Device,
}

struct Args {
    grants: Vec<(Grant, PathBuf)>,
    program: OsString,
    args: Vec<OsString>,
}

fn parse(mut it: impl Iterator<Item = OsString>) -> Result<Args, String> {
    let mut grants = Vec::new();
    loop {
        let flag = match it.next() {
            Some(a) if a == "--" => break,
            Some(a) => a,
            None => return Err("missing `-- PROGRAM`".into()),
        };
        let grant = match flag.to_str() {
            Some("--ro") => Grant::Ro,
            Some("--rw") => Grant::Rw,
            Some("--device") => Grant::Device,
            _ => return Err(format!("unknown argument {flag:?}")),
        };
        let path = it.next().ok_or_else(|| format!("{flag:?} needs a path"))?;
        grants.push((grant, PathBuf::from(path)));
    }
    let program = it.next().ok_or("missing PROGRAM after --")?;
    Ok(Args {
        grants,
        program,
        args: it.collect(),
    })
}

/// Restricts this process: every filesystem right handled, each grant's rights on its path only.
fn restrict(grants: &[(Grant, PathBuf)]) -> Result<(ABI, RulesetStatus), String> {
    let err = |e: landlock::RulesetError| e.to_string();
    let read = AccessFs::Execute | AccessFs::ReadFile | AccessFs::ReadDir;
    let write = read
        | AccessFs::WriteFile
        | AccessFs::Truncate
        | AccessFs::MakeReg
        | AccessFs::MakeDir
        | AccessFs::MakeSym
        | AccessFs::RemoveFile
        | AccessFs::RemoveDir
        | AccessFs::Refer;
    let mut created = Ruleset::default()
        .handle_access(AccessFs::from_all(WANT))
        .map_err(err)?
        .handle_access(AccessNet::from_all(WANT))
        .map_err(err)?
        .scope(Scope::AbstractUnixSocket | Scope::Signal)
        .map_err(err)?
        .create()
        .map_err(err)?;
    for (grant, p) in grants {
        let rights = match grant {
            Grant::Ro => read,
            Grant::Rw => write,
            Grant::Device => write | AccessFs::IoctlDev,
        };
        let fd = PathFd::new(p).map_err(|e| format!("{}: {e}", p.display()))?;
        created = created
            .add_rule(PathBeneath::new(fd, rights))
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
    match restrict(&a.grants) {
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
