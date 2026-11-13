// SPDX-License-Identifier: MIT
//! atl-sandbox-probe: what an app process can reach, as `probe: <check>=<outcome>` on stdout, for `xtask test sandbox` to judge.
//! Usage: `atl-sandbox-probe HOME_FILE HOME_NEW OUTSIDE OWN_FILE PORT SOCKET OTHER_SOCKET FONTCONF FONTCONF_SIBLING SYSFS SYS_OTHER RESOLV`:
//! HOME_FILE an existing file in the real $HOME, HOME_NEW a name in it that does not exist, OUTSIDE a file the app's grants do not cover for writing, OWN_FILE a path in the app's own data dir, PORT a loopback TCP port the gate listens on;
//! SOCKET the granted compositor socket and OTHER_SOCKET a host socket beside it, FONTCONF the granted fontconfig file and FONTCONF_SIBLING a file beside it, SYSFS a file in the granted DRM device's sysfs and SYS_OTHER a sysfs file outside it, RESOLV the resolver configuration (/etc/resolv.conf).
//! Checks: `home-read`, `home-create`, `outside-write`, `own-write` + `own-read`, `net-connect`, then `socket` / `socket-other` (unix connect), `fontconf` / `fontconf-sibling`, `drm-sysfs` / `sys-other` (reads),
//! `resolv` (`ok:<first nameserver, or none>` when RESOLV reads). An outcome is `ok` or `err:<errno>`. Last, `env=<names>`: the names of its whole environment, sorted, comma-joined, no values. Exits 0 once every check ran; the gate judges the outcomes.
//! Built on std, stays inside the app seccomp filter (allow-all but setpriority) and the stub's Landlock domain: no probe of its own beyond the file calls.
#![forbid(unsafe_code)]

use std::fs;
use std::io::{self, Write};
use std::net::TcpStream;
use std::os::unix::net::UnixStream;
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

fn connect(p: &Path) -> String {
    outcome(UnixStream::connect(p).map(drop))
}

/// A TCP connect to the gate's loopback listener: `ok`, or the errno of Landlock's network refusal (EACCES).
fn net(port: &str) -> String {
    match port.parse::<u16>() {
        Ok(p) => outcome(TcpStream::connect(("127.0.0.1", p)).map(drop)),
        Err(_) => format!("bad-port:{port}"),
    }
}

/// The resolver file as the sandbox shows it: `ok:<first nameserver>` (`ok:none` without one), or the read's errno (ENOENT: a link that dangles in the sandbox).
fn nameserver(p: &Path) -> String {
    match fs::read_to_string(p) {
        Ok(text) => format!("ok:{}", first_nameserver(&text).unwrap_or("none")),
        Err(e) => outcome(Err(e)),
    }
}

/// resolv.conf's first `nameserver` value (resolv_conf.c: keyword, whitespace, address).
fn first_nameserver(text: &str) -> Option<&str> {
    text.lines().find_map(|l| {
        let mut w = l.split_whitespace();
        (w.next() == Some("nameserver")).then(|| w.next()).flatten()
    })
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [
        home_file,
        home_new,
        outside,
        own,
        port,
        socket,
        other_socket,
        fontconf,
        fontconf_sibling,
        sysfs,
        sys_other,
        resolv,
    ] = args.as_slice()
    else {
        let _ = writeln!(
            io::stderr(),
            "probe: usage: HOME_FILE HOME_NEW OUTSIDE OWN_FILE PORT SOCKET OTHER_SOCKET FONTCONF FONTCONF_SIBLING SYSFS SYS_OTHER RESOLV"
        );
        return ExitCode::from(2);
    };
    let p = Path::new;
    let checks = [
        ("home-read", read(p(home_file))),
        ("home-create", create(p(home_new))),
        ("outside-write", write(p(outside))),
        ("own-write", write(p(own))),
        ("own-read", read(p(own))),
        ("net-connect", net(port)),
        ("socket", connect(p(socket))),
        ("socket-other", connect(p(other_socket))),
        ("fontconf", read(p(fontconf))),
        ("fontconf-sibling", read(p(fontconf_sibling))),
        ("drm-sysfs", read(p(sysfs))),
        ("sys-other", read(p(sys_other))),
        ("resolv", nameserver(p(resolv))),
    ];
    let mut line: Vec<String> = checks.iter().map(|(k, v)| format!("{k}={v}")).collect();
    let mut names: Vec<String> = std::env::vars_os()
        .map(|(k, _)| k.to_string_lossy().into_owned())
        .collect();
    names.sort();
    line.push(format!("env={}", names.join(",")));
    let _ = writeln!(io::stdout(), "probe: {}", line.join(" "));
    ExitCode::SUCCESS
}
