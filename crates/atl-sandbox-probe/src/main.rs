// SPDX-License-Identifier: MIT
//! atl-sandbox-probe: what an app process can reach, as `probe: <check>=<outcome>` on stdout, for `xtask test sandbox` to judge.
//! Usage: `atl-sandbox-probe HOME_FILE HOME_NEW OUTSIDE OWN_FILE PORT`: HOME_FILE an existing file in the real $HOME, HOME_NEW a name in it that does not exist, OUTSIDE a file the app's grants do not cover for writing, OWN_FILE a path in the app's own data dir, PORT a loopback TCP port the gate listens on.
//! Checks: `home-read` reads HOME_FILE, `home-create` creates HOME_NEW, `outside-write` writes OUTSIDE, `own-write` and `own-read` write and read OWN_FILE. An outcome is `ok` or `err:<errno>`. Exits 0 once every check ran; the gate judges the outcomes.
//! Built on std, stays inside the app seccomp filter (allow-all but setpriority) and the stub's Landlock domain: no probe of its own beyond the file calls.
#![forbid(unsafe_code)]

use std::fs;
use std::io::{self, Write};
use std::net::TcpStream;
use std::path::Path;
use std::process::ExitCode;

/// `ok`, or `err:<raw OS error>`: EACCES (Landlock or a mode bit), ENOENT (a path the sandbox does not show), EROFS (a read-only mount).
fn outcome(r: io::Result<()>) -> String {
    match r {
        Ok(()) => "ok".to_owned(),
        Err(e) => format!("err:{}", e.raw_os_error().unwrap_or(0)),
    }
}

fn read(p: &Path) -> String {
    outcome(fs::read(p).map(drop))
}

fn create(p: &Path) -> String {
    outcome(fs::File::create_new(p).map(drop))
}

fn write(p: &Path) -> String {
    outcome(fs::write(p, b"probe"))
}

/// A TCP connect to the gate's loopback listener: `ok`, or the errno of Landlock's network refusal (EACCES).
fn net(port: &str) -> String {
    match port.parse::<u16>() {
        Ok(p) => outcome(TcpStream::connect(("127.0.0.1", p)).map(drop)),
        Err(_) => format!("bad-port:{port}"),
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [home_file, home_new, outside, own, port] = args.as_slice() else {
        let _ = writeln!(
            io::stderr(),
            "probe: usage: HOME_FILE HOME_NEW OUTSIDE OWN_FILE PORT"
        );
        return ExitCode::from(2);
    };
    let checks = [
        ("home-read", read(Path::new(home_file))),
        ("home-create", create(Path::new(home_new))),
        ("outside-write", write(Path::new(outside))),
        ("own-write", write(Path::new(own))),
        ("own-read", read(Path::new(own))),
        ("net-connect", net(port)),
    ];
    let line: Vec<String> = checks.iter().map(|(k, v)| format!("{k}={v}")).collect();
    let _ = writeln!(io::stdout(), "probe: {}", line.join(" "));
    ExitCode::SUCCESS
}
