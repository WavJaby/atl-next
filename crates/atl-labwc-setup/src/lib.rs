// SPDX-License-Identifier: MIT
#![forbid(unsafe_code)]
//! A headless labwc's test setup over its Wayland socket, shared by the local gates and the remote test runs: the outputs (`zwlr_output_manager_v1`: mode, rate, scale, transform, position) and the seat's pointer and keyboard (`zwlr_virtual_pointer_manager_v1`, `zwp_virtual_keyboard_manager_v1`).
//! labwc 0.8 ignores rc.xml for output modes, and a seat with no input devices has no `wl_pointer` or `wl_keyboard`, so a client sends these requests. The devices live as long as the connection: [`setup`] returns the control that keeps it open.
//! The binary `atl-labwc-setup` is this setup as a process: a remote test run starts it beside its labwc on the target (remote.rs `nested_headless_script`) and waits for [`READY_LINE`].

pub mod dmabuf;

use std::collections::HashMap;
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex, MutexGuard};

use anyhow::{Context, Result, anyhow, bail, ensure};
use atl_services_types::display_model::DisplayModel;
use atl_wayland_wire::{Arg, Client, fixed, op as wire_op, string, word, word_after_string};

/// The line the helper prints once the outputs and the seat's devices are set (its run waits for it).
pub const READY_LINE: &str = "ready";

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
            // a headless output has no panel: its physical dpi is the density
            None,
        )
    }
}

/// Opcodes from wlr-output-management-unstable-v1.xml, wlr-virtual-pointer-unstable-v1.xml and virtual-keyboard-unstable-v1.xml (requests and events numbered separately, declaration order).
mod op {
    pub const VPOINTER_MANAGER_CREATE: u16 = 0;
    pub const VPOINTER_MOTION_ABSOLUTE: u16 = 1;
    pub const VPOINTER_FRAME: u16 = 4;
    pub const VKEYBOARD_MANAGER_CREATE: u16 = 0;
    pub const OUTPUT_MANAGER_CREATE_CONFIGURATION: u16 = 0;
    pub const OUTPUT_MANAGER_HEAD: u16 = 0;
    pub const OUTPUT_MANAGER_DONE: u16 = 1;
    pub const HEAD_NAME: u16 = 0;
    pub const CONFIGURATION_ENABLE_HEAD: u16 = 0;
    pub const CONFIGURATION_DISABLE_HEAD: u16 = 1;
    pub const CONFIGURATION_APPLY: u16 = 2;
    pub const CONFIGURATION_DESTROY: u16 = 4;
    pub const CONFIGURATION_SUCCEEDED: u16 = 0;
    pub const CONFIGURATION_FAILED: u16 = 1;
    pub const CONFIGURATION_CANCELLED: u16 = 2;
    pub const CONFIG_HEAD_SET_CUSTOM_MODE: u16 = 1;
    pub const CONFIG_HEAD_SET_POSITION: u16 = 2;
    pub const CONFIG_HEAD_SET_TRANSFORM: u16 = 3;
    pub const CONFIG_HEAD_SET_SCALE: u16 = 4;
}

const VPOINTER_MANAGER: &str = "zwlr_virtual_pointer_manager_v1";
const VKEYBOARD_MANAGER: &str = "zwp_virtual_keyboard_manager_v1";
const OUTPUT_MANAGER: &str = "zwlr_output_manager_v1";
/// Version 2 has `set_custom_mode` and everything used here.
const OUTPUT_MANAGER_VERSION: u32 = 2;
/// How often one output configuration is sent: a `cancelled` one is redone from the new serial. Only bounds a compositor whose outputs never settle.
const APPLY_ATTEMPTS: u32 = 3;

/// The compositor's answer to one applied configuration.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Answer {
    Succeeded,
    Failed,
    /// The configuration's serial was outdated: the outputs changed since its `done`.
    Cancelled,
}

/// The compositor answered `failed` to an output configuration: the outputs it could not set up.
#[derive(Debug)]
pub struct Refused(String);

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the compositor answered `failed` to the output configuration [{}]: see its log",
            self.0
        )
    }
}

impl std::error::Error for Refused {}

/// The helper's exit status when the compositor refused its output configuration ([`Refused`]); any other failure is 1.
pub const REFUSED_EXIT: u8 = 3;

/// The helper's exit status for `e`.
pub fn exit_status(e: &anyhow::Error) -> u8 {
    if e.is::<Refused>() { REFUSED_EXIT } else { 1 }
}

/// The setup connection: it keeps the virtual pointer and keyboard alive and holds the output manager and its heads (ordered as the outputs), so the outputs can be changed later.
struct Setup {
    client: Client,
    /// The virtual pointer.
    pointer: u32,
    om: u32,
    heads: Vec<u32>,
    /// The heads' names, in head order (HEADLESS-1, ...).
    names: Vec<String>,
    outputs: Vec<Output>,
    /// Which outputs the last configuration turned on.
    on: Vec<bool>,
    /// The last `zwlr_output_manager_v1.done` serial: a configuration names it, or the compositor cancels it.
    serial: u32,
}

impl Setup {
    /// Applies one configuration ([`apply`](Self::apply)), redone while the compositor answers `cancelled`. `failed` is final: the compositor could not set the outputs up (its log has the cause), and the error names them.
    fn configure(&mut self, on: &[bool]) -> Result<()> {
        for _ in 0..APPLY_ATTEMPTS {
            match self.apply(on)? {
                Answer::Succeeded => return Ok(()),
                Answer::Failed => return Err(Refused(self.described(on)).into()),
                Answer::Cancelled => {}
            }
        }
        bail!(
            "the compositor answered `cancelled` {APPLY_ATTEMPTS} times to the output configuration [{}]",
            self.described(on)
        )
    }

    /// One line per output, as [`OutputControl::describe`] returns them.
    fn lines(&self) -> Vec<String> {
        self.names
            .iter()
            .zip(&self.outputs)
            .map(|(name, o)| {
                format!(
                    "output {name} {}x{}@{} scale {} transform {}",
                    o.width, o.height, o.refresh_mhz, o.scale, o.transform
                )
            })
            .collect()
    }

    /// The outputs of a configuration, for its error.
    fn described(&self, on: &[bool]) -> String {
        let lines: Vec<String> = self
            .lines()
            .iter()
            .zip(on)
            .map(|(line, on)| format!("{line} {}", if *on { "on" } else { "off" }))
            .collect();
        lines.join(", ")
    }

    /// Sends one configuration and reads its answer: output i on (its mode, rate, scale, position) where `on[i]`, else off. Positions are left to right over all outputs, on or off, so an output keeps its place.
    fn apply(&mut self, on: &[bool]) -> Result<Answer> {
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
        let answer = events
            .iter()
            .filter(|(o, ..)| *o == config)
            .find_map(|(_, opcode, _)| match *opcode {
                op::CONFIGURATION_SUCCEEDED => Some(Answer::Succeeded),
                op::CONFIGURATION_FAILED => Some(Answer::Failed),
                op::CONFIGURATION_CANCELLED => Some(Answer::Cancelled),
                _ => None,
            })
            .with_context(|| {
                format!(
                    "the compositor did not answer the output configuration [{}]",
                    self.described(on)
                )
            })?;
        self.client.send(config, op::CONFIGURATION_DESTROY, &[]);
        Ok(answer)
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

/// The outputs of a running headless labwc, shareable with a probe thread. The setup connection lives while any clone does.
#[derive(Clone)]
pub struct OutputControl(Arc<Mutex<Setup>>);

impl OutputControl {
    fn lock(&self) -> Result<MutexGuard<'_, Setup>> {
        self.0
            .lock()
            .map_err(|_| anyhow!("the output control's lock is poisoned"))
    }

    /// Output `index` on, every other one off, in one configuration: the compositor moves the windows of an output going off to the one left on (labwc output_disable -> views to another output). Before a window opens, it makes that output the window's: labwc opens a view on the output under the cursor or the last added, not the first (measured 2026-11-06: HEADLESS-2).
    pub fn only(&self, index: usize) -> Result<()> {
        let mut s = self.lock()?;
        ensure!(index < s.heads.len(), "no output {index}");
        let on: Vec<bool> = (0..s.heads.len()).map(|i| i == index).collect();
        s.configure(&on)?;
        s.on = on;
        Ok(())
    }

    /// Output `index` takes `scale` in place: the outputs on stay on; a window on it gets the new preferred scale (wp_fractional_scale_v1) after the change, not before.
    pub fn set_scale(&self, index: usize, scale: f64) -> Result<()> {
        let mut s = self.lock()?;
        ensure!(index < s.heads.len(), "no output {index}");
        s.outputs[index].scale = scale;
        let on = s.on.clone();
        s.configure(&on)
    }

    /// Output `index` takes `transform` in place: the outputs on stay on, so windows stay where they are; the compositor sends the new `wl_output.geometry` + `done`.
    pub fn set_transform(&self, index: usize, transform: i32) -> Result<()> {
        let mut s = self.lock()?;
        ensure!(index < s.heads.len(), "no output {index}");
        s.outputs[index].transform = transform;
        let on = s.on.clone();
        s.configure(&on)
    }

    /// The virtual pointer waits at the output's bottom-right pixel, where no window opens. wlroots draws the cursor in software into the output, so a capture holds it whatever `overlay_cursor` says (measured 2026-11-07: an arrow over the fixture's quadrants).
    pub fn park_pointer(&self, output: Output) -> Result<()> {
        let mut s = self.lock()?;
        let (pointer, w, h) = (s.pointer, output.width as u32, output.height as u32);
        let args = [
            Arg::U(0),
            Arg::U(w - 1),
            Arg::U(h - 1),
            Arg::U(w),
            Arg::U(h),
        ];
        s.client.send(pointer, op::VPOINTER_MOTION_ABSOLUTE, &args);
        s.client.send(pointer, op::VPOINTER_FRAME, &[]);
        s.client.roundtrip()?;
        Ok(())
    }

    /// One line per output, as the helper prints it: `output <head> <w>x<h>@<mHz> scale <s> transform <t>`.
    pub fn describe(&self) -> Result<Vec<String>> {
        Ok(self.lock()?.lines())
    }

    /// Reads and drops the compositor's events until it closes the socket: the devices stay up for that long. The remote helper's whole life after setup.
    pub fn hold(&self) -> Result<()> {
        self.lock()?.client.drain_until_closed()
    }
}

/// Configures `outputs` (all on, left to right) and gives the seat its pointer and keyboard, over `stream`. The returned control keeps the connection, and with it the virtual devices, alive.
pub fn setup(stream: UnixStream, outputs: &[Output]) -> Result<OutputControl> {
    let mut c = Client::new(stream)?;
    let registry = c.new_id();
    c.send(1, wire_op::DISPLAY_GET_REGISTRY, &[Arg::U(registry)]);
    let mut globals: HashMap<String, (u32, u32)> = HashMap::new();
    for (object, opcode, body) in c.roundtrip()? {
        if object == registry && opcode == wire_op::REGISTRY_GLOBAL {
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
            wire_op::REGISTRY_BIND,
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
    let names = heads.iter().map(|(_, name)| name.clone()).collect();
    let mut s = Setup {
        client: c,
        pointer,
        om,
        heads: heads.into_iter().map(|(h, _)| h).collect(),
        names,
        outputs: outputs.to_vec(),
        on: vec![true; outputs.len()],
        serial,
    };
    s.configure(&vec![true; outputs.len()])?;
    Ok(OutputControl(Arc::new(Mutex::new(s))))
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::thread::JoinHandle;

    use super::*;

    /// The fake compositor's one head: a server-side object id (wayland.xml: 0xff000000 up).
    const HEAD: u32 = 0xff00_0000;
    const FIRST_SERIAL: u32 = 7;
    /// How far a change of the outputs moves the serial.
    const SERIAL_STEP: u32 = 10;

    /// What the fake does with an `apply` whose serial is current.
    #[derive(Clone, Copy)]
    enum Then {
        Succeed,
        Fail,
        Silent,
    }

    /// A compositor with one output: the globals [`setup`] binds, the head, and an answer to each `apply` by the protocol's rule (an outdated serial is `cancelled`), else `then`. Its outputs change at the first `changes` applies: a new `done`, so that configuration is outdated.
    /// Ends when the client closes; the applies' serials.
    fn compositor(mut s: UnixStream, changes: usize, then: Then) -> JoinHandle<Vec<u32>> {
        std::thread::spawn(move || {
            let mut serial = FIRST_SERIAL;
            let mut applied = Vec::new();
            let (mut om, mut configs) = (0, HashMap::new());
            let mut head = [0u8; 8];
            while s.read_exact(&mut head).is_ok() {
                let object = word(&head, 0).expect("object");
                let second = word(&head, 4).expect("size");
                let mut body = vec![0u8; (second >> 16) as usize - 8];
                s.read_exact(&mut body).expect("body");
                let mut send = |object: u32, opcode: u16, body: &[u8]| {
                    let size = 8 + body.len() as u32;
                    let mut m = object.to_ne_bytes().to_vec();
                    m.extend(((size << 16) | u32::from(opcode)).to_ne_bytes());
                    m.extend(body);
                    s.write_all(&m).expect("event");
                };
                let opcode = (second & 0xffff) as u16;
                match (object, opcode) {
                    // wl_display.sync -> wl_callback.done
                    (1, 0) => send(word(&body, 0).expect("id"), 0, &[0; 4]),
                    // wl_display.get_registry -> wl_registry.global
                    (1, 1) => {
                        let globals = [
                            "wl_seat",
                            VPOINTER_MANAGER,
                            VKEYBOARD_MANAGER,
                            OUTPUT_MANAGER,
                        ];
                        for (name, iface) in (1u32..).zip(globals) {
                            let mut g = name.to_ne_bytes().to_vec();
                            g.extend(text(iface));
                            g.extend(OUTPUT_MANAGER_VERSION.to_ne_bytes());
                            send(word(&body, 0).expect("id"), 0, &g);
                        }
                    }
                    // wl_registry.bind: the registry is the client's first object, 2
                    (2, 0) if string(&body, 4).expect("iface") == OUTPUT_MANAGER => {
                        om = word(&body, body.len() - 4).expect("id");
                        send(om, 0, &HEAD.to_ne_bytes());
                        send(HEAD, 0, &text("HEADLESS-1"));
                        send(om, 1, &serial.to_ne_bytes());
                    }
                    // zwlr_output_manager_v1.create_configuration
                    (o, 0) if om != 0 && o == om => {
                        let word = |at| word(&body, at).expect("argument");
                        configs.insert(word(0), word(4));
                    }
                    // zwlr_output_configuration_v1.apply -> succeeded 0, failed 1, cancelled 2
                    (o, 2) if configs.contains_key(&o) => {
                        if applied.len() < changes {
                            serial += SERIAL_STEP;
                            send(om, 1, &serial.to_ne_bytes());
                        }
                        applied.push(configs[&o]);
                        match then {
                            _ if configs[&o] != serial => send(o, 2, &[]),
                            Then::Succeed => send(o, 0, &[]),
                            Then::Fail => send(o, 1, &[]),
                            Then::Silent => {}
                        }
                    }
                    _ => {}
                }
            }
            applied
        })
    }

    /// A string argument: length with the NUL, bytes, padding to 4.
    fn text(s: &str) -> Vec<u8> {
        let mut out = (s.len() as u32 + 1).to_ne_bytes().to_vec();
        out.extend(s.as_bytes());
        out.push(0);
        out.resize(out.len().next_multiple_of(4), 0);
        out
    }

    /// The setup against [`compositor`]: its result (the connection closed) and the applies' serials.
    fn run(changes: usize, then: Then) -> (Result<()>, Vec<u32>) {
        let (client, server) = UnixStream::pair().expect("pair");
        let fake = compositor(server, changes, then);
        let result = setup(client, &[GATE_OUTPUT]).map(drop);
        (result, fake.join().expect("the fake compositor"))
    }

    /// The outputs changed under the first configuration: `cancelled`, redone with the new serial (7 + 10), `succeeded`.
    #[test]
    fn a_cancelled_configuration_is_redone_from_the_new_serial() {
        let (result, applied) = run(1, Then::Succeed);
        result.expect("the second configuration succeeds");
        assert_eq!(applied, [7, 17]);
    }

    /// Outputs that never settle end the setup after 3 tries.
    #[test]
    fn cancelled_again_and_again_is_an_error() {
        let (result, applied) = run(usize::MAX, Then::Succeed);
        let e = result.expect_err("never applied");
        assert_eq!(exit_status(&e), 1, "not a refusal");
        let text = format!("{e:#}");
        assert!(text.contains("`cancelled` 3 times"), "{text}");
        assert_eq!(applied, [7, 17, 27]);
    }

    /// `failed` is final: one try, the error names the output, and the helper's status says refused (main.rs: 3).
    #[test]
    fn a_failed_configuration_names_its_output() {
        let (result, applied) = run(0, Then::Fail);
        let e = result.expect_err("failed");
        assert_eq!(exit_status(&e), 3);
        let text = format!("{e:#}");
        assert!(
            text.contains("`failed`")
                && text.contains("output HEADLESS-1 1920x1200@60000 scale 1 transform 0 on"),
            "{text}"
        );
        assert_eq!(applied, [7]);
    }

    /// No answer is neither: it says so.
    #[test]
    fn no_answer_is_named() {
        let (result, _) = run(0, Then::Silent);
        let text = format!("{:#}", result.expect_err("no answer"));
        assert!(
            text.contains("did not answer") && text.contains("HEADLESS-1"),
            "{text}"
        );
    }
}
