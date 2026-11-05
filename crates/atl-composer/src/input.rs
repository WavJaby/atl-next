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
pub struct Windows<K, C> {
    list: Vec<(K, C)>,
    gesture: Option<K>,
}

impl<K, C> Default for Windows<K, C> {
    fn default() -> Self {
        Self {
            list: Vec::new(),
            gesture: None,
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

    /// The window of a pointer event, given the buttons `held` before it: with none held the event starts a gesture (or is a hover) and goes to the top window, which the gesture keeps
    /// until its last button is released (InputDispatcher's TouchState: later windows do not take a stream already going).
    pub fn pointer_target(&mut self, held: i32) -> Option<(&K, &mut C)> {
        if held == 0 {
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

/// MotionEvent.BUTTON_PRIMARY, _SECONDARY, _TERTIARY, _BACK, _FORWARD (NDK input.h: one bit each).
#[cfg(any(feature = "converter", test))]
const BUTTON_PRIMARY: i32 = 1 << 0;
#[cfg(any(feature = "converter", test))]
const BUTTON_SECONDARY: i32 = 1 << 1;
#[cfg(any(feature = "converter", test))]
const BUTTON_TERTIARY: i32 = 1 << 2;
#[cfg(any(feature = "converter", test))]
const BUTTON_BACK: i32 = 1 << 3;
#[cfg(any(feature = "converter", test))]
const BUTTON_FORWARD: i32 = 1 << 4;

/// The bit of `button` in a MotionEvent's button state. GDK numbers 8 and 9 are back and forward (X11 convention); other buttons have no bit.
#[cfg(any(feature = "converter", test))]
fn button_bit(button: PointerButton) -> i32 {
    match button {
        PointerButton::Primary => BUTTON_PRIMARY,
        PointerButton::Secondary => BUTTON_SECONDARY,
        PointerButton::Middle => BUTTON_TERTIARY,
        PointerButton::Other(8) => BUTTON_BACK,
        PointerButton::Other(9) => BUTTON_FORWARD,
        PointerButton::Other(_) => 0,
    }
}

/// The buttons held after `phase`: a press adds its button, a release removes it, a move keeps the set.
#[cfg(any(feature = "converter", test))]
fn held_after(held: i32, phase: PointerPhase) -> i32 {
    match phase {
        PointerPhase::Down(button) => held | button_bit(button),
        PointerPhase::Up(button) => held & !button_bit(button),
        PointerPhase::Move => held,
    }
}

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
    use atl_platform_ui::{InputEvent, InputSource, PointerEvent, PointerPhase};
    use binder::SpIBinder;

    use super::{HeadlessInput, Message, Step, Windows, held_after};

    /// MotionEvent.ACTION_DOWN, ACTION_UP, ACTION_MOVE (MotionEvent.java).
    const ACTION_DOWN: i32 = 0;
    const ACTION_UP: i32 = 1;
    const ACTION_MOVE: i32 = 2;
    /// InputDevice.SOURCE_MOUSE: a pointer device. Its DOWN/MOVE/UP reach View.onTouchEvent through View.dispatchPointerEvent.
    const SOURCE_MOUSE: i32 = 0x0000_2002;
    /// MotionEvent.TOOL_TYPE_MOUSE (NDK input.h AMOTION_EVENT_TOOL_TYPE_MOUSE = 3).
    const TOOL_TYPE_MOUSE: i32 = 3;
    /// The headless pointer device and the default display.
    const DEVICE_ID: i32 = 1;
    const DISPLAY_ID: i32 = 0;
    /// The script's one pointer.
    const POINTER_ID: i32 = 0;
    /// Event times are AOSP's ns, the script's are ms.
    const NANOS_PER_MS: i64 = 1_000_000;
    /// The channel's name in AOSP's logs.
    const CHANNEL: &str = "atl-headless-pointer";

    /// The MotionEvent of `e` as event `seq`: one pointer at its position, identity transform. `down_ms` is the gesture's down time, carried by every event of it; `buttons` the buttons held after `e`.
    fn motion(e: &PointerEvent, seq: u32, down_ms: u32, buttons: i32) -> MotionEvent {
        let action = match e.phase {
            PointerPhase::Down(_) => ACTION_DOWN,
            PointerPhase::Up(_) => ACTION_UP,
            PointerPhase::Move => ACTION_MOVE,
        };
        MotionEvent {
            seq,
            event_id: i32::try_from(seq).unwrap_or(i32::MAX),
            device_id: DEVICE_ID,
            source: SOURCE_MOUSE,
            display_id: DISPLAY_ID,
            action,
            action_button: 0,
            flags: 0,
            edge_flags: 0,
            meta_state: 0,
            button_state: buttons,
            classification: 0,
            transform: [1.0, 0.0, 0.0, 1.0, 0.0, 0.0],
            x_precision: 1.0,
            y_precision: 1.0,
            // a mouse hit-tests at the cursor (MotionEvent.getXDispatchLocation); its NaN check is `!=`, always true, so NaN hits no child
            x_cursor: e.position.x as f32,
            y_cursor: e.position.y as f32,
            down_time: i64::from(down_ms) * NANOS_PER_MS,
            event_time: i64::from(e.time_ms) * NANOS_PER_MS,
            pointers: vec![Pointer {
                id: POINTER_ID,
                tool_type: TOOL_TYPE_MOUSE,
                x: e.position.x as f32,
                y: e.position.y as f32,
                axes: Vec::new(),
            }],
        }
    }

    /// The seq after `last`. InputPublisher refuses seq 0 (BAD_VALUE, InputTransport.cpp publishMotionEvent), which lost the gesture's DOWN: 0 is skipped, first and on wrap.
    fn next_seq(last: u32) -> u32 {
        last.wrapping_add(1).max(1)
    }

    /// AOSP's `DEAD_OBJECT` (= `-EPIPE`, utils/Errors.h): InputPublisher's status when the window's end of the channel is closed.
    const DEAD_OBJECT: i32 = -32;

    /// The dispatch state: the windows' publishers by channel token, the gesture's down time, the last seq, the held buttons.
    #[derive(Default)]
    struct State {
        windows: RefCell<Windows<SpIBinder, Publisher>>,
        down_ms: Cell<u32>,
        seq: Cell<u32>,
        buttons: Cell<i32>,
    }

    /// Publishes one injected event to its window ([`Windows::pointer_target`]), then drains the finished signals the channel holds (no ANR path: no system dispatcher waits on them).
    /// A channel whose window end is closed is dropped (InputDispatcher::handleReceiveCallback: "unrecoverably broken" -> removeInputChannelLocked).
    fn publish(state: &State, event: InputEvent) {
        let InputEvent::Pointer(e) = event;
        if matches!(e.phase, PointerPhase::Down(_)) {
            state.down_ms.set(e.time_ms);
        }
        let seq = next_seq(state.seq.get());
        state.seq.set(seq);
        let held = state.buttons.get();
        let buttons = held_after(held, e.phase);
        state.buttons.set(buttons);
        let event_motion = motion(&e, seq, state.down_ms.get(), buttons);
        let mut windows = state.windows.borrow_mut();
        let Some((token, publisher)) = windows.pointer_target(held) else {
            tracing::warn!("input: no window takes event {seq}, dropped");
            return;
        };
        if let Err(err) = publisher.publish_motion(&event_motion) {
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
        loop {
            match publisher.receive_finished() {
                Ok(Some(finished)) => tracing::debug!(
                    "headless pointer: finished seq {} handled {}",
                    finished.seq,
                    finished.handled
                ),
                Ok(None) => break,
                Err(err) => {
                    tracing::warn!("headless pointer: finished signal: {err:?}");
                    break;
                }
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
                    Ok(publisher) => state.windows.borrow_mut().add(token, publisher),
                    Err(err) => tracing::warn!("input: input channel refused: {err:?}"),
                },
                Message::WindowRemoved(token) => {
                    state.windows.borrow_mut().remove(&token);
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
        use atl_platform_ui::{PhysPoint, PointerButton};

        use super::*;

        /// 0 is InputPublisher's refused seq: the first event and the wrap both land on 1.
        #[test]
        fn seq_is_never_zero() {
            assert_eq!((next_seq(0), next_seq(1), next_seq(u32::MAX)), (1, 2, 1));
        }

        /// An up at (260, 340) at 7 ms, the gesture down at 5 ms, as event 3. The expected action (ACTION_UP 1) and source (0x2002) are written out, not taken from the code.
        #[test]
        fn motion_is_one_mouse_pointer_at_the_position() {
            let e = PointerEvent {
                phase: PointerPhase::Up(PointerButton::Primary),
                position: PhysPoint { x: 260.0, y: 340.0 },
                time_ms: 7,
            };
            let m = motion(&e, 3, 5, 0);
            assert_eq!((m.seq, m.action, m.source), (3, 1, 0x0000_2002));
            assert_eq!(
                m.pointers,
                vec![Pointer {
                    id: 0,
                    tool_type: 3,
                    x: 260.0,
                    y: 340.0,
                    axes: Vec::new()
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

    /// Press and release move the held set by their button, a move keeps it, a second button stacks: primary 1, plus secondary 2 = 3, release primary = 2. Bits as NDK input.h (AMOTION_EVENT_BUTTON_*).
    #[test]
    fn held_buttons_follow_press_and_release() {
        let held = held_after(0, PointerPhase::Down(PointerButton::Primary));
        assert_eq!(held, 1);
        assert_eq!(held_after(held, PointerPhase::Move), 1);
        let both = held_after(held, PointerPhase::Down(PointerButton::Secondary));
        assert_eq!(both, 3);
        assert_eq!(
            held_after(both, PointerPhase::Up(PointerButton::Primary)),
            2
        );
        assert_eq!(held_after(2, PointerPhase::Up(PointerButton::Secondary)), 0);
        assert_eq!(
            held_after(0, PointerPhase::Down(PointerButton::Other(8))),
            8
        );
        assert_eq!(
            held_after(0, PointerPhase::Down(PointerButton::Other(3))),
            0
        );
    }

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
        assert!(w.pointer_target(0).is_none());
        w.add(1, "main");
        w.add(2, "history");
        assert_eq!(w.pointer_target(0).map(|(k, _)| *k), Some(2));
        assert!(w.remove(&2));
        assert!(!w.remove(&2));
        assert_eq!(w.pointer_target(0).map(|(k, _)| *k), Some(1));
        // press on 1 (held 0 before), 3 comes up, the move with the button held stays on 1
        assert_eq!(w.pointer_target(0).map(|(k, _)| *k), Some(1));
        w.add(3, "dialog");
        assert_eq!(w.pointer_target(1).map(|(k, _)| *k), Some(1));
        // the gesture's window goes: the rest of the gesture has no window; the next gesture starts on the top
        w.remove(&1);
        assert!(w.pointer_target(1).is_none());
        assert_eq!(w.pointer_target(0).map(|(k, _)| *k), Some(3));
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
