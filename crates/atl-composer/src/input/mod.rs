// SPDX-License-Identifier: MIT
//! The headless pointer (M3 slice 5, sub-gate c): a script (atld `--headless-pointer`, delivered as `ro.atl.headless.pointer`) injected once the first frame reached the compositor.
//! Items are `kind:x,y` separated by spaces, kind `down`, `move` or `up`, positions in physical pixels. [`HeadlessInput`] is the headless [`InputSource`] of the platform layer: the script's events go through it,
//! and the publisher on the window's input channel (the server end the UI agent's INPUT_SERVER hands over) turns them into MotionEvents (converter-only [`spawn`]).
//! The publisher is not Send: the input thread owns it and takes the channels and the first-frame signal in order.

use std::cell::RefCell;
#[cfg(feature = "converter")]
use std::os::fd::OwnedFd;

use atl_platform_ui::{InputEvent, InputSource, PhysPoint, PointerButton, PointerPhase};
#[cfg(feature = "converter")]
use binder::SpIBinder;

/// One scripted step: the phase and the position it happens at.
pub type Step = (PointerPhase, PhysPoint);

/// What the input thread takes: a window's input channel (token + server end, from the UI agent) or its removal, an event, or the first frame reaching the compositor.
#[cfg(feature = "converter")]
pub enum Message {
    Window {
        token: SpIBinder,
        fd: OwnedFd,
    },
    /// The window of this channel token was removed (WMS removeWindow): its channel is dropped.
    WindowRemoved(SpIBinder),
    FirstFrame,
    /// A pointer event of the UI-mode window (its `on_events`), published like a scripted one.
    Event(InputEvent),
}

/// The app's input windows, bottom to top, each by its channel key `K` with its channel `C`, and the open pointer gesture's window: the part of AOSP's InputDispatcher that picks a window.
/// Z-order = add order: windows of the one Task stack as they are added (a new activity's or a dialog's window comes on top). Every window is taken as touch-modal and covering the pointer
/// (activity windows fill the Task, dialogs are touch-modal: InputDispatcher `windowAcceptsTouchAt` takes a window without FLAG_NOT_TOUCH_MODAL wherever the touch is), so the topmost window
/// is the touched one (`findTouchedWindowAtLocked`) and the focused one. Re-check when windows carry frames and flags here (popups are FLAG_NOT_TOUCH_MODAL).
/// Focus (InputDispatcher's focused window, delivered as InputChannel focus events -> ViewRootImpl windowFocusChanged): the top window while the desktop toplevel is active, none otherwise.
pub struct Windows<K, C> {
    list: Vec<(K, C)>,
    gesture: Option<K>,
    /// The desktop toplevel is active (InputEvent::Focus); false until told.
    active: bool,
    /// The window last told it has focus.
    focused: Option<K>,
}

impl<K, C> Default for Windows<K, C> {
    fn default() -> Self {
        Self {
            list: Vec::new(),
            gesture: None,
            active: false,
            focused: None,
        }
    }
}

impl<K: PartialEq + Clone, C> Windows<K, C> {
    /// `key`'s channel goes on top (a key added again replaces its old channel).
    pub fn add(&mut self, key: K, channel: C) {
        self.remove(&key);
        self.list.push((key, channel));
    }

    /// Drops `key`'s channel; true when it was held. A gesture on it stays open with no window: its remaining events are dropped (InputDispatcher drops a stream whose touched window is gone).
    pub fn remove(&mut self, key: &K) -> bool {
        let before = self.list.len();
        self.list.retain(|(k, _)| k != key);
        before != self.list.len()
    }

    /// The desktop toplevel became active (`true`) or not.
    pub fn set_active(&mut self, active: bool) {
        self.active = active;
    }

    /// The focus events owed since the last call, in order: the old window loses focus, then the new one gains it. Called after every add, remove and [`Windows::set_active`], so a window added later gets the current state replayed.
    /// An event for a window whose channel is gone is still listed; [`Windows::channel`] then finds none.
    pub fn focus_changes(&mut self) -> Vec<(K, bool)> {
        let want = if self.active {
            self.list.last().map(|(k, _)| k.clone())
        } else {
            None
        };
        if want == self.focused {
            return Vec::new();
        }
        let mut out = Vec::new();
        if let Some(old) = self.focused.take() {
            out.push((old, false));
        }
        if let Some(new) = want.clone() {
            out.push((new, true));
        }
        self.focused = want;
        out
    }

    /// The channel of `key`, if held.
    pub fn channel(&mut self, key: &K) -> Option<&mut C> {
        self.list.iter_mut().find(|(k, _)| k == key).map(|(_, c)| c)
    }

    /// The window of a pointer event, given whether a gesture (button held, swipe going) was open before it: without one the event starts a gesture (or is a hover or wheel) and goes to the top window,
    /// which the gesture keeps until it ends (InputDispatcher's TouchState: later windows do not take a stream already going).
    pub fn pointer_target(&mut self, in_gesture: bool) -> Option<(&K, &mut C)> {
        if !in_gesture {
            self.gesture = self.list.last().map(|(k, _)| k.clone());
        }
        let key = self.gesture.as_ref()?;
        self.list
            .iter_mut()
            .find(|(k, _)| k == key)
            .map(|(k, c)| (&*k, c))
    }
}

/// The script's steps, in order. A bad item (kind, shape, non-finite position) refuses the whole script; an empty script has no steps.
pub fn parse_script(script: &str) -> Result<Vec<Step>, String> {
    script
        .split_whitespace()
        .map(|item| {
            let (kind, at) = item
                .split_once(':')
                .ok_or_else(|| format!("script item {item:?}: want kind:x,y"))?;
            let (x, y) = at
                .split_once(',')
                .ok_or_else(|| format!("script item {item:?}: want kind:x,y"))?;
            let phase = match kind {
                "down" => PointerPhase::Down(PointerButton::Primary),
                "move" => PointerPhase::Move,
                "up" => PointerPhase::Up(PointerButton::Primary),
                _ => return Err(format!("script item {item:?}: kind is down, move or up")),
            };
            let coord = |v: &str| {
                v.parse::<f64>()
                    .ok()
                    .filter(|c| c.is_finite())
                    .ok_or_else(|| format!("script item {item:?}: {v:?} is no finite number"))
            };
            Ok((
                phase,
                PhysPoint {
                    x: coord(x)?,
                    y: coord(y)?,
                },
            ))
        })
        .collect()
}

/// The callback `on_events` sets.
type Callback = Box<dyn FnMut(InputEvent)>;

/// The headless source: `on_events` sets the callback, [`HeadlessInput::inject`] runs it per event, as a window's source does.
#[derive(Default)]
pub struct HeadlessInput {
    on_event: RefCell<Option<Callback>>,
}

impl HeadlessInput {
    /// Runs the callback on `event`; with none set the event is dropped.
    pub fn inject(&self, event: InputEvent) {
        if let Some(on_event) = self.on_event.borrow_mut().as_mut() {
            on_event(event);
        }
    }
}

impl InputSource for HeadlessInput {
    fn on_events<F: FnMut(InputEvent) + 'static>(&self, on_event: F) {
        *self.on_event.borrow_mut() = Some(Box::new(on_event));
    }
}

pub mod pointer;

#[cfg(feature = "converter")]
pub use publish::spawn;

/// Converter-only: the publisher (AOSP InputPublisher over the channel) and the thread that owns it.
#[cfg(feature = "converter")]
mod publish {
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;
    use std::sync::mpsc::{Receiver, Sender, channel};
    use std::thread;
    use std::time::Instant;

    use atl_libinput::{MotionEvent, Pointer, Publisher};
    use atl_platform_ui::{InputEvent, InputSource, PointerEvent};
    use binder::SpIBinder;

    use super::pointer::{self, Motion};
    use super::{HeadlessInput, Message, Step, Windows};

    /// The pointer device (atl-services input.rs) and the default display.
    const DEVICE_ID: i32 = 1;
    const DISPLAY_ID: i32 = 0;
    /// The one pointer's id.
    const POINTER_ID: i32 = 0;
    /// Event times are AOSP's ns, the platform's are ms.
    const NANOS_PER_MS: i64 = 1_000_000;
    /// The channel's name in AOSP's logs.
    const CHANNEL: &str = "atl-headless-pointer";

    /// `m` as the MotionEvent numbered `seq`: one pointer, identity transform.
    fn motion(m: &Motion, seq: u32) -> MotionEvent {
        MotionEvent {
            seq,
            event_id: i32::try_from(seq).unwrap_or(i32::MAX),
            device_id: DEVICE_ID,
            source: m.source,
            display_id: DISPLAY_ID,
            action: m.action,
            action_button: m.action_button,
            flags: m.flags,
            edge_flags: 0,
            // no keyboard yet: no modifier can be down (the keys step publishes the meta state)
            meta_state: 0,
            button_state: m.button_state,
            classification: m.classification,
            transform: [1.0, 0.0, 0.0, 1.0, 0.0, 0.0],
            x_precision: 1.0,
            y_precision: 1.0,
            // a mouse hit-tests at the cursor (MotionEvent.getXDispatchLocation); its NaN check is `!=`, always true, so NaN hits no child
            x_cursor: m.cursor.x as f32,
            y_cursor: m.cursor.y as f32,
            down_time: i64::from(m.down_ms) * NANOS_PER_MS,
            event_time: i64::from(m.time_ms) * NANOS_PER_MS,
            pointers: vec![Pointer {
                id: POINTER_ID,
                tool_type: m.tool_type,
                x: m.at.x as f32,
                y: m.at.y as f32,
                axes: m.axes.clone(),
            }],
        }
    }

    /// The seq after `last`. InputPublisher refuses seq 0 (BAD_VALUE, InputTransport.cpp publishMotionEvent), which lost the gesture's DOWN: 0 is skipped, first and on wrap.
    fn next_seq(last: u32) -> u32 {
        last.wrapping_add(1).max(1)
    }

    /// AOSP's `DEAD_OBJECT` (= `-EPIPE`, utils/Errors.h): InputPublisher's status when the window's end of the channel is closed.
    const DEAD_OBJECT: i32 = -32;

    /// The dispatch state: the windows' publishers by channel token, the pointer, the last seq.
    #[derive(Default)]
    struct State {
        windows: RefCell<Windows<SpIBinder, Publisher>>,
        pointer: RefCell<pointer::Pointer>,
        seq: Cell<u32>,
    }

    /// Publishes the MotionEvents of one event to its window ([`Windows::pointer_target`]), then drains the finished signals the channel holds (no ANR path: no system dispatcher waits on them).
    /// A channel whose window end is closed is dropped (InputDispatcher::handleReceiveCallback: "unrecoverably broken" -> removeInputChannelLocked).
    fn publish(state: &State, event: InputEvent) {
        if let InputEvent::Focus(active) = event {
            state.windows.borrow_mut().set_active(active);
            publish_focus(state);
            return;
        }
        let mut pointer = state.pointer.borrow_mut();
        let in_gesture = pointer.in_gesture();
        let motions = pointer.translate(event);
        if motions.is_empty() {
            return;
        }
        let mut windows = state.windows.borrow_mut();
        let Some((token, publisher)) = windows.pointer_target(in_gesture) else {
            tracing::warn!("input: no window takes {} event(s), dropped", motions.len());
            return;
        };
        for m in &motions {
            let seq = next_seq(state.seq.get());
            state.seq.set(seq);
            if let Err(err) = publisher.publish_motion(&motion(m, seq)) {
                tracing::warn!("input: publishing event {seq}: {err:?}");
                if matches!(
                    err,
                    atl_libinput::Error::Status {
                        status: DEAD_OBJECT,
                        ..
                    }
                ) {
                    let token = token.clone();
                    windows.remove(&token);
                }
                return;
            }
        }
        loop {
            match publisher.receive_finished() {
                Ok(Some(finished)) => tracing::debug!(
                    "input: finished seq {} handled {}",
                    finished.seq,
                    finished.handled
                ),
                Ok(None) => break,
                Err(err) => {
                    tracing::warn!("input: finished signal: {err:?}");
                    break;
                }
            }
        }
    }

    /// Publishes the focus events owed ([`Windows::focus_changes`]) to the windows still held.
    fn publish_focus(state: &State) {
        let mut windows = state.windows.borrow_mut();
        for (token, has_focus) in windows.focus_changes() {
            let Some(publisher) = windows.channel(&token) else {
                continue;
            };
            let seq = next_seq(state.seq.get());
            state.seq.set(seq);
            let event_id = i32::try_from(seq).unwrap_or(i32::MAX);
            match publisher.publish_focus(seq, event_id, has_focus) {
                Ok(()) => tracing::info!("input: focus {has_focus} published (seq {seq})"),
                Err(err) => tracing::warn!("input: publishing focus {has_focus}: {err:?}"),
            }
        }
    }

    /// The input thread: takes each window's channel, and at the first frame injects the script through the headless source. The publisher stays on this thread (it is not Send).
    fn dispatch(rx: Receiver<Message>, script: Vec<Step>) {
        let state = Rc::new(State::default());
        let source = HeadlessInput::default();
        let publish_state = Rc::clone(&state);
        source.on_events(move |event| publish(&publish_state, event));
        let started = Instant::now();
        for message in rx {
            match message {
                Message::Window { token, fd } => match Publisher::new(fd, CHANNEL) {
                    Ok(publisher) => {
                        state.windows.borrow_mut().add(token, publisher);
                        publish_focus(&state);
                    }
                    Err(err) => tracing::warn!("input: input channel refused: {err:?}"),
                },
                Message::WindowRemoved(token) => {
                    state.windows.borrow_mut().remove(&token);
                    publish_focus(&state);
                }
                Message::Event(event) => source.inject(event),
                Message::FirstFrame => {
                    for &(phase, position) in &script {
                        let time_ms =
                            u32::try_from(started.elapsed().as_millis()).unwrap_or(u32::MAX);
                        source.inject(InputEvent::Pointer(PointerEvent {
                            phase,
                            position,
                            time_ms,
                        }));
                    }
                }
            }
        }
    }

    /// Starts the input thread with `script` (empty: nothing injected, channels still taken). The sender takes each window's channel and the first frame.
    pub fn spawn(script: Vec<Step>) -> Result<Sender<Message>, String> {
        let (tx, rx) = channel();
        thread::Builder::new()
            .name("atl-input".to_owned())
            .spawn(move || dispatch(rx, script))
            .map_err(|e| format!("input thread: {e}"))?;
        Ok(tx)
    }

    #[cfg(test)]
    mod tests {
        use atl_platform_ui::PhysPoint;

        use super::*;

        /// 0 is InputPublisher's refused seq: the first event and the wrap both land on 1.
        #[test]
        fn seq_is_never_zero() {
            assert_eq!((next_seq(0), next_seq(1), next_seq(u32::MAX)), (1, 2, 1));
        }

        /// A finger at (260, 340), cursor (250, 300), at 7 ms, the gesture down at 5 ms, as event 3: every field of the translation passes through, times become ns.
        #[test]
        fn motion_carries_the_translation() {
            let m = Motion {
                action: 1,
                action_button: 0,
                button_state: 0,
                flags: 0x8,
                classification: 3,
                source: 0x2002,
                tool_type: 1,
                at: PhysPoint { x: 260.0, y: 340.0 },
                cursor: PhysPoint { x: 250.0, y: 300.0 },
                axes: vec![(2, 0.0)],
                down_ms: 5,
                time_ms: 7,
            };
            let m = motion(&m, 3);
            assert_eq!(
                (m.seq, m.action, m.source, m.flags, m.classification),
                (3, 1, 0x2002, 0x8, 3)
            );
            assert_eq!((m.x_cursor, m.y_cursor), (250.0, 300.0));
            assert_eq!(
                m.pointers,
                vec![Pointer {
                    id: 0,
                    tool_type: 1,
                    x: 260.0,
                    y: 340.0,
                    axes: vec![(2, 0.0)]
                }]
            );
            assert_eq!(m.transform, [1.0, 0.0, 0.0, 1.0, 0.0, 0.0]);
            assert_eq!((m.down_time, m.event_time), (5_000_000, 7_000_000));
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::rc::Rc;

    use atl_platform_ui::PointerEvent;

    use super::*;

    /// The script's items in order: kinds, positions and phases as written; a bad item refuses the whole script; an empty script has no steps.
    #[test]
    fn parse_script_reads_the_items_in_order() {
        let want = vec![
            (
                PointerPhase::Down(PointerButton::Primary),
                PhysPoint { x: 200.0, y: 300.0 },
            ),
            (PointerPhase::Move, PhysPoint { x: 260.0, y: 340.0 }),
            (
                PointerPhase::Up(PointerButton::Primary),
                PhysPoint { x: 260.0, y: 340.0 },
            ),
        ];
        assert_eq!(
            parse_script("down:200,300 move:260,340 up:260,340"),
            Ok(want)
        );
        assert_eq!(parse_script(""), Ok(Vec::new()));
        for bad in ["tap:1,2", "down:1", "down:1,nan", "down:a,2"] {
            assert!(parse_script(bad).is_err(), "{bad}");
        }
    }

    /// Main (1) opens a second window (2) and closes it: events go to 2 while it is on top, to 1 again after its removal (the Calculator History bug: the last-added channel kept every event).
    /// A gesture keeps the window it started on when another window comes up mid-gesture, and is dropped (no window) when its window goes.
    #[test]
    fn windows_route_to_the_top_and_keep_a_gesture() {
        let mut w = Windows::<u32, &str>::default();
        assert!(w.pointer_target(false).is_none());
        w.add(1, "main");
        w.add(2, "history");
        assert_eq!(w.pointer_target(false).map(|(k, _)| *k), Some(2));
        assert!(w.remove(&2));
        assert!(!w.remove(&2));
        assert_eq!(w.pointer_target(false).map(|(k, _)| *k), Some(1));
        // press on 1 (no gesture before), 3 comes up, the move with the button held stays on 1
        assert_eq!(w.pointer_target(false).map(|(k, _)| *k), Some(1));
        w.add(3, "dialog");
        assert_eq!(w.pointer_target(true).map(|(k, _)| *k), Some(1));
        // the gesture's window goes: the rest of the gesture has no window; the next gesture starts on the top
        w.remove(&1);
        assert!(w.pointer_target(true).is_none());
        assert_eq!(w.pointer_target(false).map(|(k, _)| *k), Some(3));
    }

    /// Focus goes to the top window only while the desktop window is active: replayed to a window added later, moved when the top changes, taken (false) on deactivation.
    #[test]
    fn focus_follows_the_top_window_while_active() {
        let mut w = Windows::<u32, &str>::default();
        w.add(1, "main");
        assert!(w.focus_changes().is_empty());
        w.set_active(true);
        assert_eq!(w.focus_changes(), [(1, true)]);
        assert!(w.focus_changes().is_empty());
        w.add(2, "second");
        assert_eq!(w.focus_changes(), [(1, false), (2, true)]);
        w.remove(&2);
        assert_eq!(w.focus_changes(), [(2, false), (1, true)]);
        w.set_active(false);
        assert_eq!(w.focus_changes(), [(1, false)]);
        w.remove(&1);
        w.set_active(true);
        assert!(w.focus_changes().is_empty());
        w.add(3, "late");
        assert_eq!(w.focus_changes(), [(3, true)]);
    }

    /// The headless source runs the callback set by `on_events` once per injected event; with none set an event is dropped.
    #[test]
    fn headless_input_runs_its_callback_per_event() {
        let event = InputEvent::Pointer(PointerEvent {
            phase: PointerPhase::Move,
            position: PhysPoint { x: 1.0, y: 2.0 },
            time_ms: 0,
        });
        let source = HeadlessInput::default();
        source.inject(event);
        let seen = Rc::new(Cell::new(0_u32));
        let counter = Rc::clone(&seen);
        source.on_events(move |_| counter.set(counter.get() + 1));
        source.inject(event);
        source.inject(event);
        assert_eq!(seen.get(), 2);
    }
}
