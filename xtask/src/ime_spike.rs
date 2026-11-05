// SPDX-License-Identifier: MIT
//! `xtask test ime-spike`: can a gate act as the desktop's input method on the gates' headless labwc (nested_wayland.rs)? Spike for the IME bridge (route R1); no product code involved.
//! This side binds `zwp_input_method_manager_v2` as a fake IME before the client starts; the client (xtask/testdata/ime-entry, a focused GtkText on GTK's text-input-v3 path) prints its `preedit-changed` and buffer changes.
//! Gates, each on the client's own output:
//! 1. **activate**: labwc sends the IME `activate` + `done` once the client's text input is enabled on the focused surface.
//! 2. **preedit**: `set_preedit_string(PREEDIT)` + `commit(serial)` -> the client reports `PREEDIT <PREEDIT>`.
//! 3. **commit**: `commit_string(COMMIT)` with an empty preedit + `commit(serial)` -> the buffer reads COMMIT. `--break-commit` skips the commit_string, so this gate must fail (the probe is shown to respond).
//!
//! Evidence tier: isolated runtime, real compositor routing (wlroots input-method relay) and GTK's own IM context; no Android side.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use crate::nested_wayland::{Arg, ChildGuard, Client, NestedCompositor};

#[derive(clap::Args)]
pub struct Args {
    /// Skip the commit_string request (deliberate break): the commit gate must fail.
    #[arg(long)]
    break_commit: bool,
}

const FIXTURE: &str = "xtask/testdata/ime-entry";
/// Preedit and commit text: multi-byte UTF-8, so a byte/char mix-up in the path shows.
const PREEDIT: &str = "かな";
const COMMIT: &str = "仮名";
/// How long one gate may wait for its event: GTK maps and focuses in well under a second unloaded; this bounds a hang.
const WAIT: Duration = Duration::from_secs(10);
const POLL: Duration = Duration::from_millis(20);
/// No new `done` for this long = the client has answered; a GTK round trip takes one main-loop pass (~ms), so 200 ms is a wide margin.
const SETTLE: Duration = Duration::from_millis(200);

/// Opcodes from input-method-unstable-v2.xml (requests and events numbered separately, declaration order) and wayland.xml.
mod op {
    pub const REGISTRY_BIND: u16 = 0;
    pub const MANAGER_GET_INPUT_METHOD: u16 = 0;
    pub const IM_COMMIT_STRING: u16 = 0;
    pub const IM_SET_PREEDIT_STRING: u16 = 1;
    pub const IM_COMMIT: u16 = 3;
    pub const IM_ACTIVATE: u16 = 0;
    pub const IM_DEACTIVATE: u16 = 1;
    pub const IM_DONE: u16 = 5;
    pub const IM_UNAVAILABLE: u16 = 6;
}

/// The fake IME's state: the object id and the `done` count, which is the serial `commit` must carry.
struct Ime {
    c: Client,
    im: u32,
    dones: u32,
    active: bool,
}

impl Ime {
    fn bind(nested: &NestedCompositor) -> Result<Self> {
        let (mut c, registry, globals) = nested.connect()?;
        let bind = |c: &mut Client, iface: &str| -> Result<u32> {
            let &(name, _) = globals
                .get(iface)
                .with_context(|| format!("labwc has no {iface}"))?;
            let id = c.new_id();
            c.send(
                registry,
                op::REGISTRY_BIND,
                &[Arg::U(name), Arg::S(iface), Arg::U(1), Arg::U(id)],
            );
            Ok(id)
        };
        let seat = bind(&mut c, "wl_seat")?;
        let manager = bind(&mut c, "zwp_input_method_manager_v2")?;
        let im = c.new_id();
        c.send(
            manager,
            op::MANAGER_GET_INPUT_METHOD,
            &[Arg::U(seat), Arg::U(im)],
        );
        let mut ime = Self {
            c,
            im,
            dones: 0,
            active: false,
        };
        ime.pump()?;
        Ok(ime)
    }

    /// Reads the events that have arrived; `unavailable` (another IME holds the seat) fails.
    fn pump(&mut self) -> Result<()> {
        for (object, opcode, _) in self.c.roundtrip()? {
            if object != self.im {
                continue;
            }
            match opcode {
                op::IM_ACTIVATE => self.active = true,
                op::IM_DEACTIVATE => self.active = false,
                op::IM_DONE => self.dones += 1,
                op::IM_UNAVAILABLE => anyhow::bail!("input method unavailable"),
                _ => {}
            }
        }
        Ok(())
    }

    fn send(&mut self, opcode: u16, args: &[Arg<'_>]) {
        self.c.send(self.im, opcode, args);
    }

    /// Pumps until no `done` arrived for [`SETTLE`]: the client answers a preedit with its own text-input commit (cursor, surrounding), which the compositor relays as a new `done`; an IME commit carrying the older serial is then stale.
    fn settle(&mut self) -> Result<()> {
        let mut seen = self.dones;
        let mut quiet = Instant::now();
        while quiet.elapsed() < SETTLE {
            self.pump()?;
            if self.dones != seen {
                seen = self.dones;
                quiet = Instant::now();
            }
            std::thread::sleep(POLL);
        }
        Ok(())
    }

    fn commit(&mut self) -> Result<()> {
        let serial = self.dones;
        self.send(op::IM_COMMIT, &[Arg::U(serial)]);
        self.pump()
    }
}

/// Waits for a client line equal to `want`, pumping the IME meanwhile.
fn expect(lines: &Receiver<String>, ime: &mut Ime, want: &str) -> Result<bool> {
    let deadline = Instant::now() + WAIT;
    while Instant::now() < deadline {
        while let Ok(l) = lines.try_recv() {
            if l == want {
                return Ok(true);
            }
        }
        ime.pump()?;
        std::thread::sleep(POLL);
    }
    Ok(false)
}

fn build(root: &Path) -> Result<PathBuf> {
    let dir = root.join(FIXTURE);
    let target = root.join("target/ime-entry");
    let ok = Command::new("cargo")
        .args(["build", "--quiet", "--manifest-path"])
        .arg(dir.join("Cargo.toml"))
        .arg("--target-dir")
        .arg(&target)
        .status()?
        .success();
    anyhow::ensure!(ok, "building {FIXTURE} failed");
    Ok(target.join("debug/ime-entry"))
}

pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    let bin = build(root)?;
    let work = root.join("target/ime-spike");
    let nested = NestedCompositor::start(&work)?;
    let mut ime = Ime::bind(&nested)?;
    let mut fails = Vec::new();

    let (tx, lines) = channel();
    let mut cmd = Command::new(&bin);
    cmd.env("WAYLAND_DISPLAY", &nested.socket)
        .env("GSK_RENDERER", "cairo")
        .env("GDK_BACKEND", "wayland")
        .env_remove("GTK_IM_MODULE")
        .env_remove("DISPLAY")
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let (_client, stdout) = ChildGuard::spawn_piped(&mut cmd)?;
    std::thread::spawn(move || {
        for l in BufReader::new(stdout).lines().map_while(Result::ok) {
            if tx.send(l).is_err() {
                break;
            }
        }
    });

    if !expect(&lines, &mut ime, "FOCUS")? {
        fails.push("client never reported focus".to_owned());
    }
    let deadline = Instant::now() + WAIT;
    while !(ime.active && ime.dones > 0) && Instant::now() < deadline {
        ime.pump()?;
        std::thread::sleep(POLL);
    }
    if ime.active {
        println!(
            "pass activate: labwc activated the input method (done #{})",
            ime.dones
        );
    } else {
        fails.push("activate: labwc never sent zwp_input_method_v2.activate".to_owned());
    }

    if ime.active {
        let len = PREEDIT.len() as i32;
        ime.send(
            op::IM_SET_PREEDIT_STRING,
            &[Arg::S(PREEDIT), Arg::I(len), Arg::I(len)],
        );
        ime.commit()?;
        if expect(&lines, &mut ime, &format!("PREEDIT {PREEDIT}"))? {
            println!("pass preedit: client reported preedit-changed {PREEDIT:?}");
        } else {
            fails.push(format!("preedit: no preedit-changed {PREEDIT:?}"));
        }
        ime.settle()?;
        if !args.break_commit {
            ime.send(op::IM_COMMIT_STRING, &[Arg::S(COMMIT)]);
        }
        ime.send(
            op::IM_SET_PREEDIT_STRING,
            &[Arg::S(""), Arg::I(0), Arg::I(0)],
        );
        ime.commit()?;
        if expect(&lines, &mut ime, &format!("TEXT {COMMIT}"))? {
            println!("pass commit: client buffer reads {COMMIT:?}");
        } else {
            fails.push(format!("commit: client buffer never read {COMMIT:?}"));
        }
    }
    println!("{}", crate::verdict::gate("ime-spike", &fails));
    Ok(if fails.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}
