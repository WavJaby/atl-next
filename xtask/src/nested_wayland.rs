// SPDX-License-Identifier: MIT
//! The automated gates' own Wayland compositor: a headless labwc per gate run, so no test window maps on the owner's desktop and the owner's pointer cannot reach one (owner, 2026-11-05). `--hold` runs keep the desktop's compositor ($WAYLAND_DISPLAY): the owner watches those.
//! Test-environment setup only: the product reads WAYLAND_DISPLAY as it would on any desktop; nothing in a product crate knows about this.
//!
//! Why labwc (measured 2026-11-06, all headless, pixman renderer):
//! - sway tiles, and even with every window floating it configures them with an explicit size, so `gtk_window_set_default_size` on a mapped window never resizes it (`test platform-ui` phase 2b stalls); WSLg's weston leaves a floating window's size to the client.
//! - weston headless has no `wl_seat` at all: GTK binds no `wl_pointer`, so the window gates' injected pointer has nothing to go to. wlroots compositors refuse `get_pointer` on a seat that never had a pointer (protocol error), so the proxy cannot fake the capability either.
//! - labwc (wlroots, stacking) has a seat; this module gives it a pointer device through `zwlr_virtual_pointer_manager_v1` that never moves, and a keyboard through `zwp_virtual_keyboard_manager_v1` that never types (GTK's `is-active` needs a wl_keyboard enter), so the gates' input still comes only from the proxy's injections. Its outputs are configured through `zwlr_output_manager_v1` ([`Output`]): size, rate and (fractional) scale; `WLR_HEADLESS_OUTPUTS` sets the output count.
//!
//! Needs `labwc` (>= 0.8) on PATH; in WSL a wrapper supplies what wlroots needs there (environment setup, not this module).

use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use atl_services::display_model::DisplayModel;

/// One headless output: its mode in physical pixels (wlroots divides by the scale for the logical size).
#[derive(Clone, Copy, Debug)]
pub struct Output {
    pub width: i32,
    pub height: i32,
    /// mHz.
    pub refresh_mhz: i32,
    /// Physical per logical pixel; fractional scales go to clients as `wp_fractional_scale_v1`.
    pub scale: f64,
    /// `wl_output.transform` (wayland.xml: 1 = 90° counter-clockwise .. 3 = 270°), sent as `set_transform`.
    pub transform: i32,
}

/// The gates' output. It must hold the largest gate window plus its chrome, or GTK shrinks the window to the work area (a 1280 x 800 output cut the gtk stage's 1280 x 800 content to 1280 x 761, measured 2026-11-06); largest: the resize stage's 1600 x 1000 configure.
/// Rate: the test display's (DisplayModel::default, 60 Hz), so the cadence gates judge the same rate as on WSLg; scale 1, as the display model's density assumes.
pub const GATE_OUTPUT: Output = Output {
    width: 1920,
    height: 1200,
    refresh_mhz: 60_000,
    scale: 1.0,
    transform: 0,
};

/// How long labwc may take to listen and to answer a roundtrip; it takes ~30 ms unloaded (measured 2026-11-06), so this only bounds a hang.
const READY_WAIT: Duration = Duration::from_secs(10);
/// How long the compositor may take to exit after SIGTERM before it is killed.
const STOP_WAIT: Duration = Duration::from_secs(5);
const POLL: Duration = Duration::from_millis(20);
/// labwc's rc.xml: client-side decorations, as WSLg's weston and GNOME have them. labwc's default (server side, offered through `org_kde_kwin_server_decoration_manager`) makes GTK drop its header bar, and the gtk stage's chrome click (surface y 20) then lands in the content (measured 2026-11-06).
const RC_XML: &str = "<?xml version=\"1.0\"?>\n<labwc_config>\n  <core><decoration>client</decoration></core>\n</labwc_config>\n";
/// [`RC_XML`] plus a window rule maximizing every window at its first map (labwc-config(5) windowRules, action Maximize): the window gets a configure whose size is binding (state maximized) right
/// after it maps, as a tiling compositor (Hyprland) tiles a new window before the client is done starting.
const RC_XML_MAXIMIZE: &str = "<?xml version=\"1.0\"?>\n<labwc_config>\n  <core><decoration>client</decoration></core>\n  <windowRules>\n    <windowRule identifier=\"*\">\n      <action name=\"Maximize\"/>\n    </windowRule>\n  </windowRules>\n</labwc_config>\n";
/// wlroots' first free name in a fresh runtime dir.
const SOCKET: &str = "wayland-0";

/// A child process that is always ended: SIGTERM, then SIGKILL after [`STOP_WAIT`], when this guard drops, so on every return path of the gate, an error or a panic included.
/// Ctrl-C reaches the child directly (same process group), and `setpriv --pdeathsig` ends it should xtask itself be killed.
pub struct ChildGuard(Child);

impl ChildGuard {
    /// Starts `cmd` with SIGTERM as its parent-death signal.
    pub fn spawn(cmd: &mut Command) -> Result<Self> {
        let mut wrapped = Command::new("setpriv");
        wrapped
            .args(["--pdeathsig", "TERM", "--"])
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
        let child = wrapped
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
        // a private runtime dir, emptied first: a killed run's socket left there would move wlroots to the next name (sun_path holds 108 bytes; the slot work dirs stay well under)
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
            .env("WLR_BACKENDS", "headless")
            .env("WLR_HEADLESS_OUTPUTS", outputs.len().to_string())
            .env("WLR_LIBINPUT_NO_DEVICES", "1")
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
        let setup = setup(stream, outputs)
            .with_context(|| format!("setting up labwc (log {})", log.display()))?;
        println!(
            "nested compositor: labwc headless pid {} on {} (log {})",
            child.id(),
            socket.display(),
            log.display()
        );
        Ok(Self {
            setup: OutputControl(Arc::new(Mutex::new(setup))),
            _child: child,
            outputs: outputs.to_vec(),
            socket,
        })
    }

    /// The outputs' control, for a probe thread: [`OutputControl::disable`].
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
        c.send(1, op::DISPLAY_GET_REGISTRY, &[Arg::U(registry)]);
        let mut globals = HashMap::new();
        for (object, opcode, body) in c.roundtrip()? {
            if object == registry && opcode == op::REGISTRY_GLOBAL {
                globals.insert(
                    string(&body, 4)?,
                    (word(&body, 0)?, word_after_string(&body, 4)?),
                );
            }
        }
        Ok((c, registry, globals))
    }
}

impl Output {
    /// This output as Android's display: the mode this module sets (`set_custom_mode`: physical pixels), its rate and scale. The headless window stages (no Wayland window) take [`GATE_OUTPUT`]'s, so they judge the same display as the stages on a compositor, whatever the desktop's monitor runs at.
    /// The transform: wayland.xml turns are counter-clockwise, Android's Surface.ROTATION_* clockwise (Display.getRotation docs), so n counter-clockwise quarter turns are (4 - n) mod 4 clockwise;
    /// a quarter-turned output shows its mode with the sides swapped (wlroots' transformed output size, GDK's geometry).
    pub fn display(&self) -> DisplayModel {
        let physical = |px: i32| u32::try_from(px).unwrap_or(0);
        let ccw = self.transform.rem_euclid(4);
        let (w, h) = if ccw % 2 == 1 {
            (self.height, self.width)
        } else {
            (self.width, self.height)
        };
        DisplayModel::from_monitor(
            physical(w),
            physical(h),
            Some(self.refresh_mhz as f32 / 1000.0),
            self.scale,
            ((4 - ccw) % 4) as u8,
        )
    }
}

/// Opcodes from wayland.xml, wlr-output-management-unstable-v1.xml and wlr-virtual-pointer-unstable-v1.xml (requests and events numbered separately, in declaration order).
mod op {
    pub const DISPLAY_SYNC: u16 = 0;
    pub const DISPLAY_GET_REGISTRY: u16 = 1;
    pub const DISPLAY_ERROR: u16 = 0;
    pub const REGISTRY_BIND: u16 = 0;
    pub const REGISTRY_GLOBAL: u16 = 0;
    pub const CALLBACK_DONE: u16 = 0;
    pub const VPOINTER_MANAGER_CREATE: u16 = 0;
    pub const VKEYBOARD_MANAGER_CREATE: u16 = 0;
    pub const OUTPUT_MANAGER_CREATE_CONFIGURATION: u16 = 0;
    pub const OUTPUT_MANAGER_HEAD: u16 = 0;
    pub const OUTPUT_MANAGER_DONE: u16 = 1;
    pub const HEAD_NAME: u16 = 0;
    pub const CONFIGURATION_ENABLE_HEAD: u16 = 0;
    pub const CONFIGURATION_DISABLE_HEAD: u16 = 1;
    pub const CONFIGURATION_APPLY: u16 = 2;
    pub const CONFIGURATION_SUCCEEDED: u16 = 0;
    pub const CONFIG_HEAD_SET_CUSTOM_MODE: u16 = 1;
    pub const CONFIG_HEAD_SET_POSITION: u16 = 2;
    pub const CONFIG_HEAD_SET_TRANSFORM: u16 = 3;
    pub const CONFIG_HEAD_SET_SCALE: u16 = 4;
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
                op::REGISTRY_BIND,
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
        };
        ime.pump()?;
        Ok(ime)
    }

    /// Reads the events that have arrived.
    pub(crate) fn pump(&mut self) -> Result<()> {
        for (object, opcode, body) in self.c.roundtrip()? {
            if object != self.im {
                continue;
            }
            match opcode {
                im_op::ACTIVATE => self.active = true,
                im_op::DEACTIVATE => self.active = false,
                im_op::DONE => self.dones += 1,
                im_op::SURROUNDING_TEXT => {
                    let text = string(&body, 0)?;
                    let cursor = word_after_string(&body, 0)?;
                    let anchor = word(&body, 4 + (word(&body, 0)? as usize).div_ceil(4) * 4 + 4)?;
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
        self.send(im_op::COMMIT, &[Arg::U(serial)]);
        self.pump()
    }
}

const VPOINTER_MANAGER: &str = "zwlr_virtual_pointer_manager_v1";
const VKEYBOARD_MANAGER: &str = "zwp_virtual_keyboard_manager_v1";
const OUTPUT_MANAGER: &str = "zwlr_output_manager_v1";
/// Version 2 has `set_custom_mode` and everything used here.
const OUTPUT_MANAGER_VERSION: u32 = 2;

/// The setup connection: it keeps the virtual pointer alive and holds the output manager and its heads (ordered as the outputs), so the outputs can be changed later.
struct Setup {
    client: Client,
    om: u32,
    heads: Vec<u32>,
    outputs: Vec<Output>,
    /// Which outputs the last configuration turned on.
    on: Vec<bool>,
    /// The last `zwlr_output_manager_v1.done` serial: a configuration names it, or the compositor cancels it.
    serial: u32,
}

impl Setup {
    /// Applies one configuration: output i on (its mode, rate, scale, position) where `on[i]`, else off. Positions are left to right over all outputs, on or off, so an output keeps its place.
    fn configure(&mut self, on: &[bool]) -> Result<()> {
        ensure!(
            on.len() == self.heads.len(),
            "{} heads, {} switches",
            self.heads.len(),
            on.len()
        );
        // events since the last configuration (a later done) come in with this roundtrip
        let events = self.client.roundtrip()?;
        self.note_serial(&events)?;
        let c = &mut self.client;
        let config = c.new_id();
        c.send(
            self.om,
            op::OUTPUT_MANAGER_CREATE_CONFIGURATION,
            &[Arg::U(config), Arg::U(self.serial)],
        );
        let mut x = 0;
        for ((head, o), &enable) in self.heads.iter().zip(&self.outputs).zip(on) {
            if enable {
                let ch = c.new_id();
                c.send(
                    config,
                    op::CONFIGURATION_ENABLE_HEAD,
                    &[Arg::U(ch), Arg::U(*head)],
                );
                c.send(
                    ch,
                    op::CONFIG_HEAD_SET_CUSTOM_MODE,
                    &[Arg::I(o.width), Arg::I(o.height), Arg::I(o.refresh_mhz)],
                );
                c.send(ch, op::CONFIG_HEAD_SET_POSITION, &[Arg::I(x), Arg::I(0)]);
                c.send(ch, op::CONFIG_HEAD_SET_SCALE, &[Arg::F(o.scale)]);
                c.send(ch, op::CONFIG_HEAD_SET_TRANSFORM, &[Arg::I(o.transform)]);
            } else {
                c.send(config, op::CONFIGURATION_DISABLE_HEAD, &[Arg::U(*head)]);
            }
            // the next output starts where this one ends: its logical width at the scale as sent (wl_fixed: 1.6 goes as 410/256, 1920 px -> 1198 logical, as wlroots computes it)
            x += (f64::from(o.width) / fixed(o.scale)).floor() as i32;
        }
        c.send(config, op::CONFIGURATION_APPLY, &[]);
        let events = c.roundtrip()?;
        self.note_serial(&events)?;
        ensure!(
            events
                .iter()
                .any(|(o, opcode, _)| *o == config && *opcode == op::CONFIGURATION_SUCCEEDED),
            "the compositor did not apply the output configuration {:?} on {on:?}",
            self.outputs
        );
        Ok(())
    }

    fn note_serial(&mut self, events: &[(u32, u16, Vec<u8>)]) -> Result<()> {
        for (object, opcode, body) in events {
            if *object == self.om && *opcode == op::OUTPUT_MANAGER_DONE {
                self.serial = word(body, 0)?;
            }
        }
        Ok(())
    }
}

/// The outputs of a running [`NestedCompositor`], shareable with a probe thread.
#[derive(Clone)]
pub struct OutputControl(Arc<Mutex<Setup>>);

impl OutputControl {
    /// Output `index` on, every other one off, in one configuration: the compositor moves the windows of an output going off to the one left on (labwc output_disable -> views to another output). Before a window opens, it makes that output the window's: labwc opens a view on the output under the cursor or the last added, not the first (measured 2026-11-06: HEADLESS-2).
    pub fn only(&self, index: usize) -> Result<()> {
        let mut s = self
            .0
            .lock()
            .map_err(|_| anyhow::anyhow!("the output control's lock is poisoned"))?;
        ensure!(index < s.heads.len(), "no output {index}");
        let on: Vec<bool> = (0..s.heads.len()).map(|i| i == index).collect();
        s.configure(&on)?;
        s.on = on;
        Ok(())
    }

    /// Output `index` takes `transform` in place: the outputs on stay on, so windows stay where they are; the compositor sends the new `wl_output.geometry` + `done`.
    pub fn set_transform(&self, index: usize, transform: i32) -> Result<()> {
        let mut s = self
            .0
            .lock()
            .map_err(|_| anyhow::anyhow!("the output control's lock is poisoned"))?;
        ensure!(index < s.heads.len(), "no output {index}");
        s.outputs[index].transform = transform;
        let on = s.on.clone();
        s.configure(&on)
    }
}

/// Gives the seat its pointer device and configures the outputs, over `stream`; the returned connection keeps the virtual pointer alive.
fn setup(stream: UnixStream, outputs: &[Output]) -> Result<Setup> {
    let mut c = Client::new(stream)?;
    let registry = c.new_id();
    c.send(1, op::DISPLAY_GET_REGISTRY, &[Arg::U(registry)]);
    let mut globals: HashMap<String, (u32, u32)> = HashMap::new();
    for (object, opcode, body) in c.roundtrip()? {
        if object == registry && opcode == op::REGISTRY_GLOBAL {
            globals.insert(
                string(&body, 4)?,
                (word(&body, 0)?, word_after_string(&body, 4)?),
            );
        }
    }
    let bind = |c: &mut Client, iface: &str, max: u32| -> Result<u32> {
        let &(name, version) = globals
            .get(iface)
            .with_context(|| format!("the compositor has no {iface}"))?;
        let id = c.new_id();
        c.send(
            registry,
            op::REGISTRY_BIND,
            &[
                Arg::U(name),
                Arg::S(iface),
                Arg::U(version.min(max)),
                Arg::U(id),
            ],
        );
        Ok(id)
    };
    let vpm = bind(&mut c, VPOINTER_MANAGER, 1)?;
    let pointer = c.new_id();
    // seat 0 (null): the compositor's default seat
    c.send(
        vpm,
        op::VPOINTER_MANAGER_CREATE,
        &[Arg::U(0), Arg::U(pointer)],
    );
    // keyboard device: GTK's window `is-active` follows wl_keyboard enter/leave, not xdg `activated`, and a seat without a keyboard has no wl_keyboard to enter. No keymap sent and no key ever pressed: clients get the seat's (labwc's keyboard group) keymap, gates' keys come from the proxy.
    let seat = bind(&mut c, "wl_seat", 1)?;
    let vkm = bind(&mut c, VKEYBOARD_MANAGER, 1)?;
    let keyboard = c.new_id();
    c.send(
        vkm,
        op::VKEYBOARD_MANAGER_CREATE,
        &[Arg::U(seat), Arg::U(keyboard)],
    );
    let om = bind(&mut c, OUTPUT_MANAGER, OUTPUT_MANAGER_VERSION)?;
    let mut heads: Vec<(u32, String)> = Vec::new();
    let mut serial = None;
    for (object, opcode, body) in c.roundtrip()? {
        if object == om && opcode == op::OUTPUT_MANAGER_HEAD {
            heads.push((word(&body, 0)?, String::new()));
        } else if object == om && opcode == op::OUTPUT_MANAGER_DONE {
            serial = Some(word(&body, 0)?);
        } else if opcode == op::HEAD_NAME
            && let Some(h) = heads.iter_mut().find(|h| h.0 == object)
        {
            h.1 = string(&body, 0)?;
        }
    }
    let serial = serial.context("no zwlr_output_manager_v1.done")?;
    // order by name (HEADLESS-1, -2, ...) so outputs[i] is the i-th
    heads.sort_by(|a, b| a.1.cmp(&b.1));
    ensure!(
        heads.len() == outputs.len(),
        "{} outputs asked for, the compositor has {}",
        outputs.len(),
        heads.len()
    );
    let mut s = Setup {
        client: c,
        om,
        heads: heads.into_iter().map(|(h, _)| h).collect(),
        outputs: outputs.to_vec(),
        on: vec![true; outputs.len()],
        serial,
    };
    s.configure(&vec![true; outputs.len()])?;
    Ok(s)
}

pub(crate) enum Arg<'a> {
    U(u32),
    I(i32),
    /// wl_fixed: 24.8.
    F(f64),
    S(&'a str),
}

/// `v` as wl_fixed (24.8) carries it.
fn fixed(v: f64) -> f64 {
    (v * 256.0).round() / 256.0
}

pub(crate) fn word(body: &[u8], at: usize) -> Result<u32> {
    body.get(at..at + 4)
        .map(|b| u32::from_ne_bytes([b[0], b[1], b[2], b[3]]))
        .context("short Wayland message")
}

/// A string argument at `at`: length (with the NUL), bytes, padding to 4.
pub(crate) fn string(body: &[u8], at: usize) -> Result<String> {
    let len = word(body, at)? as usize;
    let text = body
        .get(at + 4..at + 4 + len)
        .context("short Wayland string")?;
    Ok(String::from_utf8_lossy(text.strip_suffix(&[0]).unwrap_or(text)).into_owned())
}

/// The word after the string argument at `at`.
fn word_after_string(body: &[u8], at: usize) -> Result<u32> {
    let len = word(body, at)? as usize;
    word(body, at + 4 + len.div_ceil(4) * 4)
}

/// A minimal Wayland client: requests out, events back by roundtrip. No fds: nothing it binds sends any.
pub struct Client {
    stream: UnixStream,
    next: u32,
    pending: Vec<u8>,
    /// Bytes read past the last roundtrip's `done`: events the compositor sent on its own (an input method's `done`), kept for the next roundtrip. Dropping them lost IME serials (measured 2026-11-06: 4 of 5 ime gate runs).
    inbox: Vec<u8>,
}

impl Client {
    fn new(stream: UnixStream) -> Result<Self> {
        stream.set_read_timeout(Some(READY_WAIT))?;
        // id 1 is wl_display
        Ok(Self {
            stream,
            next: 2,
            pending: Vec::new(),
            inbox: Vec::new(),
        })
    }

    pub(crate) fn new_id(&mut self) -> u32 {
        let id = self.next;
        self.next += 1;
        id
    }

    pub(crate) fn send(&mut self, object: u32, opcode: u16, args: &[Arg<'_>]) {
        let mut body = Vec::new();
        for a in args {
            match a {
                Arg::U(v) => body.extend(v.to_ne_bytes()),
                Arg::I(v) => body.extend(v.to_ne_bytes()),
                Arg::F(v) => body.extend(((fixed(*v) * 256.0) as i32).to_ne_bytes()),
                Arg::S(s) => {
                    let len = s.len() + 1;
                    body.extend((len as u32).to_ne_bytes());
                    body.extend(s.as_bytes());
                    body.resize(body.len() + len.div_ceil(4) * 4 - s.len(), 0);
                }
            }
        }
        let size = (8 + body.len()) as u32;
        self.pending.extend(object.to_ne_bytes());
        self.pending
            .extend(((size << 16) | u32::from(opcode)).to_ne_bytes());
        self.pending.extend(body);
    }

    /// Flushes the requests, then reads events until a `wl_display.sync` sent after them is done; a `wl_display.error` fails.
    pub(crate) fn roundtrip(&mut self) -> Result<Vec<(u32, u16, Vec<u8>)>> {
        let done = self.new_id();
        self.send(1, op::DISPLAY_SYNC, &[Arg::U(done)]);
        self.stream.write_all(&std::mem::take(&mut self.pending))?;
        let mut events = Vec::new();
        let mut buf = std::mem::take(&mut self.inbox);
        loop {
            let mut chunk = [0u8; 4096];
            let n = self
                .stream
                .read(&mut chunk)
                .context("reading from the compositor")?;
            ensure!(n > 0, "the compositor closed the connection");
            buf.extend(&chunk[..n]);
            while buf.len() >= 8 {
                let object = word(&buf, 0)?;
                let second = word(&buf, 4)?;
                let size = (second >> 16) as usize;
                if buf.len() < size {
                    break;
                }
                let opcode = (second & 0xffff) as u16;
                let body = buf[8..size].to_vec();
                buf.drain(..size);
                if object == 1 && opcode == op::DISPLAY_ERROR {
                    bail!(
                        "protocol error on object {}: code {}, {}",
                        word(&body, 0)?,
                        word(&body, 4)?,
                        string(&body, 8)?
                    );
                }
                if object == done && opcode == op::CALLBACK_DONE {
                    self.inbox = buf;
                    return Ok(events);
                }
                events.push((object, opcode, body));
            }
        }
    }
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

    /// wayland.xml's wire format, written out by hand: `wl_registry.bind(7, "wl_seat", 5, 9)` on object 2 is 8 header bytes, name, string length 8 ("wl_seat" + NUL), 8 string bytes, version, id = 32 bytes.
    #[test]
    fn bind_follows_the_wire_format() {
        let (a, _b) = UnixStream::pair().expect("pair");
        let mut c = Client::new(a).expect("client");
        c.send(
            2,
            op::REGISTRY_BIND,
            &[Arg::U(7), Arg::S("wl_seat"), Arg::U(5), Arg::U(9)],
        );
        let mut want = Vec::new();
        for w in [2u32, 32 << 16, 7, 8] {
            want.extend(w.to_ne_bytes());
        }
        want.extend(b"wl_seat\0");
        for w in [5u32, 9] {
            want.extend(w.to_ne_bytes());
        }
        assert_eq!(c.pending, want);
        assert_eq!(string(&want[8..], 4).expect("string"), "wl_seat");
        assert_eq!(word_after_string(&want[8..], 4).expect("version"), 5);
    }
}
