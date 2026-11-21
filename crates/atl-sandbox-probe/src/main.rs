// SPDX-License-Identifier: MIT
//! atl-sandbox-probe: what an app process can reach, as `probe: <check>=<outcome>` on stdout, for `xtask test sandbox` to judge.
//! Usage: `atl-sandbox-probe HOME_FILE HOME_NEW OUTSIDE OWN_FILE PORT SOCKET OTHER_SOCKET FONTCONF FONTCONF_SIBLING SYSFS SYS_OTHER RESOLV`:
//! HOME_FILE an existing file in the real $HOME, HOME_NEW a name in it that does not exist, OUTSIDE a file the app's grants do not cover for writing, OWN_FILE a path in the app's own data dir, PORT a loopback TCP port the gate listens on;
//! SOCKET the granted compositor socket and OTHER_SOCKET a host socket beside it, FONTCONF the granted fontconfig file and FONTCONF_SIBLING a file beside it, SYSFS a file in the granted DRM device's sysfs and SYS_OTHER a sysfs file outside it, RESOLV the resolver configuration (/etc/resolv.conf).
//! Checks: `home-read`, `home-create`, `outside-write`, `own-write` + `own-read`, `net-connect`, then `socket` / `socket-other` (unix connect), `fontconf` / `fontconf-sibling`, `drm-sysfs` / `sys-other` (reads),
//! `resolv` (`ok:<first nameserver, or none>` when RESOLV reads). An outcome is `ok` or `err:<errno>`. Last, `env=<names>`: the names of its whole environment, sorted, comma-joined, no values. Exits 0 once every check ran; the gate judges the outcomes.
//! Second form `atl-sandbox-probe wayland SOCKET NAME INTERFACE VERSION`: the Wayland interfaces the compositor offers through SOCKET and a bind attempt of one global (fn `wayland`); a raw wire client, so no libwayland in the sandbox's way.
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

/// Wayland wire (wayland.xml; connection.c): header = object id, `size << 16 | opcode`, native endian; a string = u32 length with the NUL, bytes, padding to 4.
mod wire {
    use std::io::{self, Read, Write};
    use std::os::unix::net::UnixStream;

    pub const DISPLAY: u32 = 1;
    /// wl_display requests sync 0, get_registry 1; events error 0, delete_id 1. wl_registry request bind 0; event global 0. wl_callback event done 0.
    pub const SYNC: u16 = 0;
    pub const GET_REGISTRY: u16 = 1;
    pub const ERROR: u16 = 0;
    pub const BIND: u16 = 0;
    pub const GLOBAL: u16 = 0;
    pub const DONE: u16 = 0;

    pub fn word(v: u32, out: &mut Vec<u8>) {
        out.extend_from_slice(&v.to_ne_bytes());
    }

    pub fn string(s: &str, out: &mut Vec<u8>) {
        let len = s.len() + 1;
        word(len as u32, out);
        out.extend_from_slice(s.as_bytes());
        out.resize(out.len() + (len.div_ceil(4) * 4 - s.len()), 0);
    }

    pub fn send(s: &mut UnixStream, object: u32, opcode: u16, body: &[u8]) -> io::Result<()> {
        let mut m = Vec::with_capacity(8 + body.len());
        word(object, &mut m);
        word(
            (((8 + body.len()) as u32) << 16) | u32::from(opcode),
            &mut m,
        );
        m.extend_from_slice(body);
        s.write_all(&m)
    }

    pub fn read_word(b: &[u8], at: usize) -> Option<u32> {
        b.get(at..at + 4)
            .map(|w| u32::from_ne_bytes([w[0], w[1], w[2], w[3]]))
    }

    pub fn read_string(b: &[u8], at: usize) -> Option<String> {
        let len = read_word(b, at)? as usize;
        let t = b.get(at + 4..at + 4 + len)?;
        Some(String::from_utf8_lossy(t.strip_suffix(&[0]).unwrap_or(t)).into_owned())
    }

    /// The next whole message: (object, opcode, body); None at end of stream.
    pub fn next(s: &mut UnixStream) -> io::Result<Option<(u32, u16, Vec<u8>)>> {
        let mut h = [0u8; 8];
        match s.read_exact(&mut h) {
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
            r => r?,
        }
        let object = u32::from_ne_bytes([h[0], h[1], h[2], h[3]]);
        let second = u32::from_ne_bytes([h[4], h[5], h[6], h[7]]);
        let mut body = vec![0u8; ((second >> 16) as usize).saturating_sub(8)];
        s.read_exact(&mut body)?;
        Ok(Some((object, (second & 0xffff) as u16, body)))
    }
}

/// What a sync on `callback` ends in: `ok` (its done), `err:protocol:<interface error code>` (wl_display.error first), `err:closed` (the compositor hung up).
/// `global` sees every wl_registry.global on the way.
fn until_done(
    s: &mut UnixStream,
    registry: u32,
    callback: u32,
    mut global: impl FnMut(String),
) -> io::Result<String> {
    while let Some((object, opcode, body)) = wire::next(s)? {
        if object == wire::DISPLAY && opcode == wire::ERROR {
            return Ok(format!(
                "err:protocol:{}",
                wire::read_word(&body, 4).unwrap_or(0)
            ));
        }
        if object == registry && opcode == wire::GLOBAL {
            if let Some(i) = wire::read_string(&body, 4) {
                global(i);
            }
        }
        if object == callback && opcode == wire::DONE {
            return Ok("ok".to_owned());
        }
    }
    Ok("err:closed".to_owned())
}

/// `wayland SOCKET NAME INTERFACE VERSION`: the interfaces the compositor at SOCKET offers this client, then a `wl_registry.bind` of global NAME as INTERFACE at VERSION (a global the gate saw offered outside the sandbox).
/// Prints `probe: wayland interfaces=<sorted, comma-joined> bind=<outcome>`; bind is `ok`, `err:protocol:<code>` or `err:closed` (until_done), a connect failure `err:<errno>` for both.
fn wayland(socket: &str, name: &str, interface: &str, version: &str) -> String {
    let run = || -> io::Result<(Vec<String>, String)> {
        let mut s = UnixStream::connect(socket)?;
        let (registry, sync1, bound, sync2) = (2, 3, 4, 5);
        let mut b = Vec::new();
        wire::word(registry, &mut b);
        wire::send(&mut s, wire::DISPLAY, wire::GET_REGISTRY, &b)?;
        b.clear();
        wire::word(sync1, &mut b);
        wire::send(&mut s, wire::DISPLAY, wire::SYNC, &b)?;
        let mut interfaces = Vec::new();
        let listed = until_done(&mut s, registry, sync1, |i| interfaces.push(i))?;
        if listed != "ok" {
            return Ok((interfaces, format!("not-tried:{listed}")));
        }
        b.clear();
        wire::word(name.parse().unwrap_or(0), &mut b);
        wire::string(interface, &mut b);
        wire::word(version.parse().unwrap_or(1), &mut b);
        wire::word(bound, &mut b);
        wire::send(&mut s, registry, wire::BIND, &b)?;
        b.clear();
        wire::word(sync2, &mut b);
        wire::send(&mut s, wire::DISPLAY, wire::SYNC, &b)?;
        Ok((interfaces, until_done(&mut s, registry, sync2, drop)?))
    };
    match run() {
        Ok((mut interfaces, bind)) => {
            interfaces.sort();
            interfaces.dedup();
            format!("wayland interfaces={} bind={bind}", interfaces.join(","))
        }
        Err(e) => {
            let o = outcome(Err(e));
            format!("wayland interfaces={o} bind={o}")
        }
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("wayland") {
        let [_, socket, name, interface, version] = args.as_slice() else {
            let _ = writeln!(
                io::stderr(),
                "probe: usage: wayland SOCKET NAME INTERFACE VERSION"
            );
            return ExitCode::from(2);
        };
        let _ = writeln!(
            io::stdout(),
            "probe: {}",
            wayland(socket, name, interface, version)
        );
        return ExitCode::SUCCESS;
    }
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
