// SPDX-License-Identifier: MIT
//! The automated gates' own Wayland compositor: a headless labwc per gate run, so no test window maps on the maintainer's desktop and the maintainer's pointer cannot reach one (maintainer, 2026-11-05); this holds for every local gate and for `check`. `--hold` runs keep the desktop's compositor ($WAYLAND_DISPLAY): the maintainer watches those.
//! One exception (maintainer, 2026-11-18): `remote run-apk` maps its window on the target's session display, on an empty workspace that the run switches to and back from, also on failure or interruption (remote.rs `workspace_script`). Every other remote test run starts its own headless labwc on the target (decision 2026-11-23, remote.rs `probe_test_display`), configured from [`RC_XML`], [`labwc_env`] and the setup helper (remote.rs `nested_headless_script`: the same `atl_labwc_setup::setup` as [`NestedCompositor`]); the window UI stages take the session where the target has no labwc and are refused where it has one, until their port lands.
//! Test-environment setup only: the product reads WAYLAND_DISPLAY as it would on any desktop; nothing in a product crate knows about this.
//!
//! Why labwc (measured 2026-11-06, all headless, pixman renderer):
//! - sway tiles, and even with every window floating it configures them with an explicit size, so `gtk_window_set_default_size` on a mapped window never resizes it (`test platform-ui` phase 2b stalls); WSLg's weston leaves a floating window's size to the client.
//! - weston headless has no `wl_seat` at all: GTK binds no `wl_pointer`, so the window gates' injected pointer has nothing to go to. wlroots compositors refuse `get_pointer` on a seat that never had a pointer (protocol error), so the proxy cannot fake the capability either.
//! - labwc (wlroots, stacking) has a seat; atl-labwc-setup gives it a pointer device through `zwlr_virtual_pointer_manager_v1` that never moves, and a keyboard through `zwp_virtual_keyboard_manager_v1` that never types (GTK's `is-active` needs a wl_keyboard enter), so the gates' input still comes only from the proxy's injections. Its outputs are configured through `zwlr_output_manager_v1` ([`Output`]): size, rate and (fractional) scale; `WLR_HEADLESS_OUTPUTS` sets the output count.
//!
//! Needs `labwc` (>= 0.8) on PATH; in WSL a wrapper supplies what wlroots needs there (environment setup, not this module).

use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use atl_services_types::display_model::DisplayModel;
use atl_wayland_wire::{Client, op as wire_op, string, word_after_string};

pub use atl_labwc_setup::{GATE_OUTPUT, Output, OutputControl};
pub(crate) use atl_wayland_wire::{Arg, word};

/// How long labwc may take to listen; it takes ~30 ms unloaded (measured 2026-11-06), so this only bounds a hang.
const READY_WAIT: Duration = Duration::from_secs(10);
/// How long the compositor may take to exit after SIGTERM before it is killed.
const STOP_WAIT: Duration = Duration::from_secs(5);
const POLL: Duration = Duration::from_millis(20);
/// labwc's rc.xml: client-side decorations, as WSLg's weston and GNOME have them. labwc's default (server side, offered through `org_kde_kwin_server_decoration_manager`) makes GTK drop its header bar, and the gtk stage's chrome click (surface y 20) then lands in the content (measured 2026-11-06).
/// Also the config of a remote test run's labwc (remote.rs `display_files`): one text for both, never a copy.
pub(crate) const RC_XML: &str = "<?xml version=\"1.0\"?>\n<labwc_config>\n  <core><decoration>client</decoration></core>\n</labwc_config>\n";
/// [`RC_XML`] plus a window rule maximizing every window at its first map (labwc-config(5) windowRules, action Maximize): the window gets a configure whose size is binding (state maximized) right
/// after it maps, as a tiling compositor (Hyprland) tiles a new window before the client is done starting.
const RC_XML_MAXIMIZE: &str = "<?xml version=\"1.0\"?>\n<labwc_config>\n  <core><decoration>client</decoration></core>\n  <windowRules>\n    <windowRule identifier=\"*\">\n      <action name=\"Maximize\"/>\n    </windowRule>\n  </windowRules>\n</labwc_config>\n";
/// [`RC_XML`] plus a window rule moving every window to the output's top-left corner at its first map: its window geometry's origin is then output (0, 0), so a capture finds the window without asking the compositor.
const RC_XML_PLACED: &str = "<?xml version=\"1.0\"?>\n<labwc_config>\n  <core><decoration>client</decoration></core>\n  <windowRules>\n    <windowRule identifier=\"*\">\n      <action name=\"MoveTo\" x=\"0\" y=\"0\"/>\n    </windowRule>\n  </windowRules>\n</labwc_config>\n";
/// wlroots' first free name in a fresh runtime dir.
const SOCKET: &str = "wayland-0";

/// labwc's environment in a gate run, local ([`NestedCompositor::start_rc`]) and remote (remote.rs `nested_headless_script`): wlroots' headless backend with `outputs` outputs, no input devices (the seat
/// gets its pointer from a client). The renderer is not here: the local gate adds pixman (measured 2026-11-06), a remote run keeps wlroots' default.
pub(crate) fn labwc_env(outputs: usize) -> [(&'static str, String); 3] {
    [
        ("WLR_BACKENDS", "headless".to_owned()),
        ("WLR_HEADLESS_OUTPUTS", outputs.to_string()),
        ("WLR_LIBINPUT_NO_DEVICES", "1".to_owned()),
    ]
}

/// `cmd` under `setpriv --pdeathsig <signal>`: the kernel sends it when the spawning thread dies, so spawn it from a thread that outlives it.
/// exec keeps the pid, and the signal survives `unshare -Ur` (no uid change the kernel sees). Environment and directory carry over; stdio is the caller's to set.
pub(crate) fn dying_with_parent(cmd: &Command, signal: &str) -> Command {
    let mut wrapped = Command::new("setpriv");
    wrapped
        .args(["--pdeathsig", signal, "--"])
        .arg(cmd.get_program())
        .args(cmd.get_args());
    for (k, v) in cmd.get_envs() {
        match v {
            Some(v) => wrapped.env(k, v),
            None => wrapped.env_remove(k),
        };
    }
    if let Some(dir) = cmd.get_current_dir() {
        wrapped.current_dir(dir);
    }
    wrapped
}

/// A child process that is always ended: SIGTERM, then SIGKILL after [`STOP_WAIT`], when this guard drops, so on every return path of the gate, an error or a panic included.
/// Ctrl-C reaches the child directly (same process group), and `setpriv --pdeathsig` ends it should xtask itself be killed.
pub struct ChildGuard(Child);

impl ChildGuard {
    /// Starts `cmd` with SIGTERM as its parent-death signal.
    pub fn spawn(cmd: &mut Command) -> Result<Self> {
        let child = dying_with_parent(cmd, "TERM")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| format!("starting {:?}", cmd.get_program()))?;
        Ok(Self(child))
    }

    pub fn id(&self) -> u32 {
        self.0.id()
    }

    fn exited(&mut self) -> bool {
        !matches!(self.0.try_wait(), Ok(None))
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if self.exited() {
            return;
        }
        let _ = Command::new("kill")
            .args(["-TERM", &self.0.id().to_string()])
            .status();
        let deadline = Instant::now() + STOP_WAIT;
        while !self.exited() && Instant::now() < deadline {
            std::thread::sleep(POLL);
        }
        if !self.exited() {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

/// `wl_registry.bind`, for clients of [`NestedCompositor::connect`] outside this module.
pub(crate) const REGISTRY_BIND: u16 = wire_op::REGISTRY_BIND;

/// A registry's globals: interface -> (global name, version).
pub type Globals = HashMap<String, (u32, u32)>;

/// A running headless labwc: its socket, log and private runtime dir (all under the run's work dir). Dropping it stops labwc.
pub struct NestedCompositor {
    /// Field order is drop order: the setup connection (and with it the virtual pointer) goes first, then the compositor.
    setup: OutputControl,
    _child: ChildGuard,
    outputs: Vec<Output>,
    /// The absolute socket path: atld's (or the proxy's upstream) WAYLAND_DISPLAY.
    pub socket: PathBuf,
}

impl NestedCompositor {
    /// Starts labwc headless with [`GATE_OUTPUT`], config and log under `work`.
    pub fn start(work: &Path) -> Result<Self> {
        Self::start_rc(work, &[GATE_OUTPUT], RC_XML)
    }

    /// As [`start`](Self::start), but every window opens at the output's top-left corner ([`RC_XML_PLACED`]), and the cursor waits in the opposite corner ([`OutputControl::park_pointer`]).
    pub fn start_placed(work: &Path) -> Result<Self> {
        let nested = Self::start_rc(work, &[GATE_OUTPUT], RC_XML_PLACED)?;
        nested.setup.park_pointer(GATE_OUTPUT)?;
        Ok(nested)
    }

    /// As [`start_with`](Self::start_with) on one output, but every window is maximized when it first maps ([`RC_XML_MAXIMIZE`]).
    pub fn start_maximizing(work: &Path, output: Output) -> Result<Self> {
        Self::start_rc(work, &[output], RC_XML_MAXIMIZE)
    }

    /// Starts labwc headless with `outputs` (left to right), config and log under `work`; returns once the outputs are configured and the seat has its pointer.
    pub fn start_with(work: &Path, outputs: &[Output]) -> Result<Self> {
        Self::start_rc(work, outputs, RC_XML)
    }

    fn start_rc(work: &Path, outputs: &[Output], rc_xml: &str) -> Result<Self> {
        ensure!(!outputs.is_empty(), "a nested compositor needs an output");
        // this run's config dir, not the user's ~/.config/labwc
        let config = work.join("labwc");
        std::fs::create_dir_all(&config)?;
        std::fs::write(config.join("rc.xml"), rc_xml)?;
        // a private runtime dir, emptied first: a killed run's socket left there would move wlroots to the next name (sun_path holds 108 bytes; the build dir work dirs stay well under)
        let runtime = work.join("rt");
        if runtime.exists() {
            std::fs::remove_dir_all(&runtime)?;
        }
        std::fs::create_dir_all(&runtime)?;
        std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o700))?;
        let log = work.join("labwc.log");
        // labwc logs to stderr; `sh -c` sends it to the log, and `exec` keeps the pid labwc's (the wrapper's)
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "exec labwc --config-dir \"$0\" 2>\"$1\""])
            .arg(&config)
            .arg(&log)
            .env("XDG_RUNTIME_DIR", &runtime)
            .envs(labwc_env(outputs.len()))
            // pixman: measured 2026-11-06 for the gates' frames; a remote run's labwc keeps the default, pixman has no dmabuf and the app's EGL fails on it (rvvm, 2026-11-23)
            .env("WLR_RENDERER", "pixman")
            .env_remove("WAYLAND_DISPLAY")
            .env_remove("WAYLAND_SOCKET")
            .env_remove("DISPLAY");
        let mut child = ChildGuard::spawn(&mut cmd)?;
        let socket = runtime.join(SOCKET);
        let deadline = Instant::now() + READY_WAIT;
        let stream = loop {
            if let Ok(s) = UnixStream::connect(&socket) {
                break s;
            }
            if child.exited() {
                bail!("labwc exited before listening; log {}", log.display());
            }
            if Instant::now() > deadline {
                bail!(
                    "labwc did not listen within {READY_WAIT:?}; log {}",
                    log.display()
                );
            }
            std::thread::sleep(POLL);
        };
        let setup = atl_labwc_setup::setup(stream, outputs)
            .with_context(|| format!("setting up labwc (log {})", log.display()))?;
        println!(
            "nested compositor: labwc headless pid {} on {} (log {})",
            child.id(),
            socket.display(),
            log.display()
        );
        Ok(Self {
            setup,
            _child: child,
            outputs: outputs.to_vec(),
            socket,
        })
    }

    /// The outputs' control, for a probe thread: [`OutputControl::only`].
    pub fn control(&self) -> OutputControl {
        self.setup.clone()
    }

    /// The gates' display on this compositor: the first output as Android's display (DisplayModel::from_monitor, as the app's window reports it once on that output). A different value given to atld would change the app's display when its window opens.
    pub fn display(&self) -> DisplayModel {
        self.output_display(0)
    }

    /// Output `index` as Android's display (see [`Output::display`]).
    pub fn output_display(&self, index: usize) -> DisplayModel {
        self.outputs[index].display()
    }

    /// A new raw client connection with its registry's globals (name -> (global name, version)): for gates that act as a privileged client themselves (the IME spike's input method).
    pub fn connect(&self) -> Result<(Client, u32, Globals)> {
        let mut c = Client::new(UnixStream::connect(&self.socket)?)?;
        let registry = c.new_id();
        c.send(1, wire_op::DISPLAY_GET_REGISTRY, &[Arg::U(registry)]);
        let mut globals = HashMap::new();
        for (object, opcode, body) in c.roundtrip()? {
            if object == registry && opcode == wire_op::REGISTRY_GLOBAL {
                globals.insert(
                    string(&body, 4)?,
                    (word(&body, 0)?, word_after_string(&body, 4)?),
                );
            }
        }
        Ok((c, registry, globals))
    }
}

/// Opcodes from input-method-unstable-v2.xml (requests and events numbered separately, declaration order).
pub(crate) mod im_op {
    pub const MANAGER_GET_INPUT_METHOD: u16 = 0;
    pub const COMMIT_STRING: u16 = 0;
    pub const SET_PREEDIT_STRING: u16 = 1;
    pub const DELETE_SURROUNDING_TEXT: u16 = 2;
    pub const COMMIT: u16 = 3;
    pub const ACTIVATE: u16 = 0;
    pub const DEACTIVATE: u16 = 1;
    pub const SURROUNDING_TEXT: u16 = 2;
    pub const CONTENT_TYPE: u16 = 4;
    pub const DONE: u16 = 5;
    pub const UNAVAILABLE: u16 = 6;
}

/// No new `done` for this long = the text-input client has answered; a GTK round trip takes one main-loop pass (~ms), so 200 ms is a wide margin.
const IME_SETTLE: Duration = Duration::from_millis(200);
const IME_POLL: Duration = Duration::from_millis(20);

/// A fake desktop input method on the labwc seat (`zwp_input_method_v2`), for the gates of a client's text input. It records what the compositor relays from the focused client's text-input v3.
/// Serial trap (measured 2026-11-06, ime spike): the client answers a preedit with its own text-input commit, relayed as a new `done`; an IME `commit` carrying the older serial is dropped silently. [`Ime::settle`] before each commit that follows client traffic.
pub(crate) struct Ime {
    c: Client,
    im: u32,
    /// `done` count: the serial `commit` carries.
    pub dones: u32,
    pub active: bool,
    /// Last `surrounding_text`: text, cursor and anchor bytes.
    pub surrounding: Option<(String, u32, u32)>,
    /// Last `content_type`: hint bits, purpose (text-input v3 enum values).
    pub content_type: Option<(u32, u32)>,
    /// Events received and requests sent since [`Ime::restart_journal`], as [`journal_line`] formats them: the probe journal the red dump prints.
    journal: Vec<String>,
    /// The journal's clock: the probe's start ([`Ime::restart_journal`]).
    origin: Instant,
}

impl Ime {
    /// Binds the seat's input method; fails if labwc has no `zwp_input_method_manager_v2` or another IME holds the seat.
    pub(crate) fn bind(nested: &NestedCompositor) -> Result<Self> {
        let (mut c, registry, globals) = nested.connect()?;
        let mut bind = |iface: &str| -> Result<u32> {
            let &(name, _) = globals
                .get(iface)
                .with_context(|| format!("labwc has no {iface}"))?;
            let id = c.new_id();
            c.send(
                registry,
                wire_op::REGISTRY_BIND,
                &[Arg::U(name), Arg::S(iface), Arg::U(1), Arg::U(id)],
            );
            Ok(id)
        };
        let seat = bind("wl_seat")?;
        let manager = bind("zwp_input_method_manager_v2")?;
        let im = c.new_id();
        c.send(
            manager,
            im_op::MANAGER_GET_INPUT_METHOD,
            &[Arg::U(seat), Arg::U(im)],
        );
        let mut ime = Self {
            c,
            im,
            dones: 0,
            active: false,
            surrounding: None,
            content_type: None,
            journal: Vec::new(),
            origin: Instant::now(),
        };
        ime.pump()?;
        Ok(ime)
    }

    /// Starts the probe journal: its times count from now, and the events before are dropped.
    pub(crate) fn restart_journal(&mut self) {
        self.journal.clear();
        self.origin = Instant::now();
    }

    /// The journal's lines, oldest first.
    pub(crate) fn journal(&self) -> &[String] {
        &self.journal
    }

    fn record(&mut self, what: &str) {
        self.journal.push(journal_line(self.origin.elapsed(), what));
    }

    /// Reads the events that have arrived.
    pub(crate) fn pump(&mut self) -> Result<()> {
        for (object, opcode, body) in self.c.roundtrip()? {
            if object != self.im {
                continue;
            }
            match opcode {
                im_op::ACTIVATE => {
                    self.active = true;
                    self.record("recv activate");
                }
                im_op::DEACTIVATE => {
                    self.active = false;
                    self.record("recv deactivate");
                }
                im_op::DONE => {
                    self.dones += 1;
                    let serial = self.dones;
                    self.record(&format!("recv done serial={serial}"));
                }
                im_op::SURROUNDING_TEXT => {
                    let text = string(&body, 0)?;
                    let cursor = word_after_string(&body, 0)?;
                    let anchor = word(&body, 4 + (word(&body, 0)? as usize).div_ceil(4) * 4 + 4)?;
                    self.record(&format!(
                        "recv surrounding_text {text:?} cursor={cursor} anchor={anchor}"
                    ));
                    self.surrounding = Some((text, cursor, anchor));
                }
                im_op::CONTENT_TYPE => {
                    self.content_type = Some((word(&body, 0)?, word(&body, 4)?));
                }
                im_op::UNAVAILABLE => {
                    bail!("input method unavailable (another IME holds the seat)")
                }
                _ => {}
            }
        }
        Ok(())
    }

    pub(crate) fn send(&mut self, opcode: u16, args: &[Arg<'_>]) {
        self.record(&format!("send {} {args:?}", request_name(opcode)));
        self.c.send(self.im, opcode, args);
    }

    /// Pumps until no `done` arrived for [`IME_SETTLE`].
    pub(crate) fn settle(&mut self) -> Result<()> {
        let mut seen = self.dones;
        let mut quiet = Instant::now();
        while quiet.elapsed() < IME_SETTLE {
            self.pump()?;
            if self.dones != seen {
                seen = self.dones;
                quiet = Instant::now();
            }
            std::thread::sleep(IME_POLL);
        }
        Ok(())
    }

    /// Applies the pending requests (`commit` with the current serial).
    pub(crate) fn commit(&mut self) -> Result<()> {
        let serial = self.dones;
        self.record(&format!("send commit serial={serial}"));
        self.c.send(self.im, im_op::COMMIT, &[Arg::U(serial)]);
        self.pump()
    }
}

/// An input-method request's name, for the journal.
fn request_name(opcode: u16) -> &'static str {
    match opcode {
        im_op::COMMIT_STRING => "commit_string",
        im_op::SET_PREEDIT_STRING => "set_preedit_string",
        im_op::DELETE_SURROUNDING_TEXT => "delete_surrounding_text",
        im_op::COMMIT => "commit",
        _ => "request",
    }
}

/// A journal line: the event's time since the probe started, then `recv` or `send` and what it was.
fn journal_line(at: Duration, what: &str) -> String {
    format!("+{:>6} ms {what}", at.as_millis())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn alive(pid: u32) -> bool {
        // a zombie still has /proc/<pid>; state Z counts as gone
        std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|s| {
            s.rsplit_once(')')
                .is_some_and(|(_, r)| !r.trim_start().starts_with('Z'))
        })
    }

    /// The journal's shape (ime.rs `red_dump` prints it): ms since the probe started in a 6-wide field, then the event's name.
    #[test]
    fn journal_lines_carry_the_time_since_the_probe_started() {
        assert_eq!(
            journal_line(Duration::from_millis(1234), "recv done serial=2"),
            "+  1234 ms recv done serial=2"
        );
        assert_eq!(
            journal_line(Duration::from_millis(7), "send commit serial=0"),
            "+     7 ms send commit serial=0"
        );
        assert_eq!(request_name(im_op::COMMIT), "commit");
        assert_eq!(
            request_name(im_op::SET_PREEDIT_STRING),
            "set_preedit_string"
        );
    }

    /// A stage that fails with the guarded process running: the error return drops the guard, which ends the process.
    #[test]
    fn guard_ends_child_on_failing_stage() {
        let mut pid = 0;
        let mut stage = || -> Result<()> {
            let g = ChildGuard::spawn(Command::new("sleep").arg("600"))?;
            pid = g.id();
            assert!(alive(pid), "the child runs inside the stage");
            bail!("stage failed")
        };
        assert!(stage().is_err());
        assert!(
            pid != 0 && !alive(pid),
            "pid {pid} outlived the failing stage"
        );
    }

    /// A child that ignores SIGTERM is killed after STOP_WAIT.
    #[test]
    fn guard_kills_child_ignoring_term() {
        let g = ChildGuard::spawn(Command::new("sh").args(["-c", "trap '' TERM; exec sleep 600"]))
            .expect("spawn");
        let pid = g.id();
        std::thread::sleep(Duration::from_millis(100));
        drop(g);
        assert!(!alive(pid), "pid {pid} outlived the guard");
    }
}
