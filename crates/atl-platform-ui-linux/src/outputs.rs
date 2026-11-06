// SPDX-License-Identifier: MIT
//! The compositor's output transforms (`wl_output.geometry` `transform`), by output name (`wl_output.name`, v4: the connector GDK reports as `GdkMonitor:connector`).
//! GDK reads the transform but exposes none (GdkMonitor has no transform; a half turn changes no geometry, so no GDK signal either), so this process opens a second Wayland connection that binds every
//! `wl_output` and follows its events on the GLib main context (an fd source; the context is the UI thread's, mirror.rs). One connection per process, shared by its windows.
//! No connection (no WAYLAND_DISPLAY, refused) = every monitor reports [`Rotation::Deg0`], and the window says so once.

use std::cell::RefCell;
use std::collections::HashMap;
use std::os::fd::{AsFd, AsRawFd};
use std::rc::Rc;

use atl_platform_ui_api::Rotation;
use gtk4::glib;
use wayland_client::protocol::wl_output::{self, Transform, WlOutput};
use wayland_client::protocol::wl_registry::{self, WlRegistry};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum};

/// `wl_output` version bound: 4 adds `name`; 2 adds `done` (earlier, each geometry applies at once).
const OUTPUT_VERSION: u32 = 4;
const OUTPUT_DONE_SINCE: u32 = 2;
/// `wl_output.release` exists from version 3.
const OUTPUT_RELEASE_SINCE: u32 = 3;

/// `wl_output` transform -> the drawn image's clockwise rotation (Android Surface.ROTATION_*). wayland.xml: transform 90 = "90 degrees counter-clockwise", the turn the compositor applies to content;
/// a counter-clockwise quarter turn of the image is Android's ROTATION_270. Checked against GNOME: mutter `meta_orientation_to_transform` maps iio-sensor-proxy right-up (device turned
/// counter-clockwise) to transform 270, and Display.getRotation's docs map a counter-clockwise device turn to ROTATION_90. Flipped transforms keep their rotation part.
pub(crate) fn rotation_of(t: Transform) -> Rotation {
    match t {
        Transform::_90 | Transform::Flipped90 => Rotation::Deg270,
        Transform::_180 | Transform::Flipped180 => Rotation::Deg180,
        Transform::_270 | Transform::Flipped270 => Rotation::Deg90,
        _ => Rotation::Deg0,
    }
}

#[derive(Default)]
struct Head {
    name: Option<String>,
    transform: Option<Transform>,
    pending: Option<Transform>,
}

#[derive(Default)]
struct State {
    /// By registry global name.
    heads: HashMap<u32, (WlOutput, Head)>,
    changed: bool,
}

impl Dispatch<WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } if interface == WlOutput::interface().name => {
                let output =
                    registry.bind::<WlOutput, _, _>(name, version.min(OUTPUT_VERSION), qh, name);
                state.heads.insert(name, (output, Head::default()));
            }
            wl_registry::Event::GlobalRemove { name } => {
                if let Some((output, _)) = state.heads.remove(&name) {
                    if output.version() >= OUTPUT_RELEASE_SINCE {
                        output.release();
                    }
                    state.changed = true;
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<WlOutput, u32> for State {
    fn event(
        state: &mut Self,
        output: &WlOutput,
        event: wl_output::Event,
        global: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some((_, head)) = state.heads.get_mut(global) else {
            return;
        };
        match event {
            wl_output::Event::Geometry {
                transform: WEnum::Value(t),
                ..
            } => {
                head.pending = Some(t);
                if output.version() < OUTPUT_DONE_SINCE {
                    state.changed |= head.transform.replace(t) != Some(t);
                }
            }
            wl_output::Event::Name { name } => head.name = Some(name),
            wl_output::Event::Done => {
                if let Some(t) = head.pending.take() {
                    state.changed |= head.transform.replace(t) != Some(t);
                }
            }
            _ => {}
        }
    }
}

struct Outputs {
    queue: EventQueue<State>,
    state: State,
}

thread_local! {
    /// `None` until the first window asks; `Some(None)` = no connection.
    static OUTPUTS: RefCell<Option<Option<Outputs>>> = const { RefCell::new(None) };
    /// Called on the main context after a dispatch that changed a transform.
    static LISTENERS: RefCell<Vec<Rc<dyn Fn()>>> = const { RefCell::new(Vec::new()) };
}

fn connect() -> Result<Outputs, String> {
    let conn = Connection::connect_to_env().map_err(|e| e.to_string())?;
    let mut queue = conn.new_event_queue();
    let qh = queue.handle();
    conn.display().get_registry(&qh, ());
    let mut state = State::default();
    // globals, then the outputs' first geometry / name / done
    for _ in 0..2 {
        queue.roundtrip(&mut state).map_err(|e| e.to_string())?;
    }
    state.changed = false;
    let fd = conn.as_fd().as_raw_fd();
    // the source owns the connection: it lives as long as the process's main context
    glib_unix::unix_fd_add_local(fd, glib::IOCondition::IN, move |_, _| {
        dispatch(&conn);
        glib::ControlFlow::Continue
    });
    Ok(Outputs { queue, state })
}

/// The socket is readable: read, dispatch, then tell the windows when a transform changed (outside the borrow: they read [`rotation`]).
fn dispatch(conn: &Connection) {
    let changed = OUTPUTS.with_borrow_mut(|o| {
        let Some(Some(o)) = o else {
            return false;
        };
        if let Some(guard) = conn.prepare_read()
            && let Err(e) = guard.read()
        {
            tracing::warn!("atl-platform-ui: output transforms: {e}");
        }
        if let Err(e) = o.queue.dispatch_pending(&mut o.state) {
            tracing::warn!("atl-platform-ui: output transforms: {e}");
        }
        let _ = conn.flush();
        std::mem::take(&mut o.state.changed)
    });
    if changed {
        let listeners = LISTENERS.with_borrow(|l| l.clone());
        for f in listeners {
            f();
        }
    }
}

/// Starts following the outputs (once per process) and registers `on_change` for the main context at each transform change. The listener stays until the returned [`Follow`] drops.
pub(crate) fn follow(on_change: impl Fn() + 'static) -> Follow {
    OUTPUTS.with_borrow_mut(|o| {
        o.get_or_insert_with(|| match connect() {
            Ok(outputs) => Some(outputs),
            Err(e) => {
                tracing::warn!(
                    "atl-platform-ui: no output transforms ({e}): every monitor is unrotated"
                );
                None
            }
        });
    });
    listen(on_change)
}

/// A registered transform listener, removed when dropped: a closed window's callback must not stay in [`LISTENERS`].
pub(crate) struct Follow {
    listener: Rc<dyn Fn()>,
}

impl Drop for Follow {
    fn drop(&mut self) {
        LISTENERS.with_borrow_mut(|l| l.retain(|f| !Rc::ptr_eq(f, &self.listener)));
    }
}

/// Registers `on_change` without starting the connection (the part of [`follow`] a test can run without a compositor).
fn listen(on_change: impl Fn() + 'static) -> Follow {
    let listener: Rc<dyn Fn()> = Rc::new(on_change);
    LISTENERS.with_borrow_mut(|l| l.push(Rc::clone(&listener)));
    Follow { listener }
}

/// The rotation of the output named `connector`; with no name to match, the one output's when there is exactly one; else none.
pub(crate) fn rotation(connector: Option<&str>) -> Rotation {
    OUTPUTS.with_borrow(|o| {
        let Some(Some(o)) = o else {
            return Rotation::Deg0;
        };
        let heads: Vec<&Head> = o.state.heads.values().map(|(_, h)| h).collect();
        let head = match connector {
            Some(c) => heads.iter().find(|h| h.name.as_deref() == Some(c)).copied(),
            None => None,
        }
        .or(if heads.len() == 1 {
            heads.first().copied()
        } else {
            None
        });
        head.and_then(|h| h.transform)
            .map_or(Rotation::Deg0, rotation_of)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// wayland.xml's counter-clockwise transforms -> Android's clockwise rotations; flips keep the turn.
    #[test]
    fn transforms_map_to_surface_rotations() {
        let q = |t| rotation_of(t).quarter_turns();
        assert_eq!(q(Transform::Normal), 0);
        assert_eq!(q(Transform::_90), 3);
        assert_eq!(q(Transform::_180), 2);
        assert_eq!(q(Transform::_270), 1);
        assert_eq!(q(Transform::Flipped), 0);
        assert_eq!(q(Transform::Flipped90), 3);
        assert_eq!(q(Transform::Flipped180), 2);
        assert_eq!(q(Transform::Flipped270), 1);
    }

    /// Windows that open and close leave no listener behind: five listeners opened, then dropped, are back at the baseline. Without the drop the list keeps all five.
    #[test]
    fn dropped_follows_leave_no_listener() {
        let count = || LISTENERS.with_borrow(|l| l.len());
        let base = count();
        let follows: Vec<Follow> = (0..5).map(|_| listen(|| {})).collect();
        assert_eq!(count(), base + 5);
        drop(follows);
        assert_eq!(count(), base);
    }
}
