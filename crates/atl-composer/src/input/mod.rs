// SPDX-License-Identifier: MIT
//! The headless pointer (M3 slice 5, sub-gate c): a script (atld `--headless-pointer`, delivered as `ro.atl.headless.pointer`) injected once the first frame reached the compositor.
//! Items are `kind:x,y` separated by spaces, kind `down`, `move` or `up`, positions in physical pixels, or `wait:MS` (the rest goes in that much later). UI mode runs the script from its first present (run-apk `--pointer`). [`HeadlessInput`] is the headless [`InputSource`] of the platform layer: the script's events go through it,
//! and the publisher on the window's input channel (the server end the UI agent's INPUT_SERVER hands over) turns them into MotionEvents (converter-only [`spawn`]).
//! The publisher is not Send: the input thread owns it and takes the channels and the first-frame signal in order.

use std::cell::RefCell;
#[cfg(feature = "converter")]
use std::os::fd::OwnedFd;
use std::os::fd::{AsFd, BorrowedFd};

use atl_platform_ui::{InputEvent, InputSource, PhysPoint, PointerButton, PointerPhase};
#[cfg(feature = "converter")]
use binder::SpIBinder;
use rustix::event::PollFlags;

/// One scripted step: the phase and the position it happens at.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Step {
    Pointer(PointerPhase, PhysPoint),
    /// `wait:MS`: the steps after it go in this much later (run-apk interactions: a tap, then the next once the UI answered).
    Wait(std::time::Duration),
    /// `focus:in` / `focus:out`: the desktop toplevel becomes active or not (headless has no toplevel, so no window has focus and keys are dropped until this).
    Focus(bool),
    /// `key-down:CODE` / `key-up:CODE`: a physical key, Linux input event code (KEY_LEFT = 105).
    Key(bool, u32),
}

/// What the input thread takes: a window's input channel (token + server end, from the UI agent) or its removal, an event, or the first frame reaching the compositor.
#[cfg(feature = "converter")]
pub enum Message {
    Window {
        token: SpIBinder,
        fd: OwnedFd,
        /// the window's LayoutParams flags (Windows::add)
        flags: i32,
    },
    /// The window of this channel token was removed (WMS removeWindow): its channel is dropped.
    WindowRemoved(SpIBinder),
    /// The frame of this channel token's window, in its Task (WindowInfo.frame + transform).
    WindowFrame {
        token: SpIBinder,
        frame: Frame,
    },
    FirstFrame,
    /// A pointer event of the UI-mode window (its `on_events`), published like a scripted one.
    Event(InputEvent),
}

/// WindowManager.LayoutParams flags InputDispatcher acts on (WMS hands them over as WindowInfo inputConfig NOT_FOCUSABLE / NOT_TOUCHABLE).
pub const FLAG_NOT_FOCUSABLE: i32 = 0x8;
pub const FLAG_NOT_TOUCHABLE: i32 = 0x10;
pub const FLAG_NOT_TOUCH_MODAL: i32 = 0x20;

/// A window's frame in its Task, `[left, right)` x `[top, bottom)` (WindowInfo.frame).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Frame {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

impl Frame {
    /// InputDispatcher windowAcceptsTouchAt: floor the point, right and bottom edges outside.
    fn contains(&self, (x, y): (f32, f32)) -> bool {
        let (x, y) = (x.floor() as i32, y.floor() as i32);
        (self.left..self.right).contains(&x) && (self.top..self.bottom).contains(&y)
    }
}

/// The app's input windows, bottom to top, each by its channel key `K` with its channel `C`, its LayoutParams flags and its frame (None until the first relayout), and the open pointer gesture's window:
/// the part of AOSP's InputDispatcher that picks a window. Z-order = add order: windows of the one Task stack as they are added (a new activity's or a dialog's window comes on top).
/// Touched window = topmost without FLAG_NOT_TOUCHABLE that is touch-modal or holds the point: a modal window's touchable region is the Task, any other's its frame (WindowState.getEffectiveTouchableRegion,
/// InputDispatcher windowAcceptsTouchAt). A window with no frame yet counts as covering. The topmost without FLAG_NOT_FOCUSABLE is the focused one.
/// Focus (InputDispatcher's focused window, delivered as InputChannel focus events -> ViewRootImpl windowFocusChanged): the top window while the desktop toplevel is active, none otherwise.
pub struct Windows<K, C> {
    list: Vec<(K, C, i32, Option<Frame>)>,
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
    /// `key`'s channel goes on top (a key added again replaces its old channel); `flags` = its window's LayoutParams flags.
    pub fn add(&mut self, key: K, channel: C, flags: i32) {
        self.remove(&key);
        self.list.push((key, channel, flags, None));
    }

    /// `key`'s window frame, from its relayout.
    pub fn set_frame(&mut self, key: &K, frame: Frame) {
        if let Some(window) = self.list.iter_mut().find(|(k, ..)| k == key) {
            window.3 = Some(frame);
        }
    }

    /// Drops `key`'s channel; true when it was held. A gesture on it stays open with no window: its remaining events are dropped (InputDispatcher drops a stream whose touched window is gone).
    pub fn remove(&mut self, key: &K) -> bool {
        let before = self.list.len();
        self.list.retain(|(k, ..)| k != key);
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
            self.list
                .iter()
                .rev()
                .find(|(_, _, f, _)| f & FLAG_NOT_FOCUSABLE == 0)
                .map(|(k, ..)| k.clone())
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

    /// A window has focus.
    pub fn has_focus(&self) -> bool {
        self.focused.is_some()
    }

    /// The focused window's channel (keys go there), if one is focused and still held.
    pub fn focused_channel(&mut self) -> Option<&mut C> {
        let key = self.focused.clone()?;
        self.channel(&key)
    }

    /// The channel of `key`, if held.
    pub fn channel(&mut self, key: &K) -> Option<&mut C> {
        self.list
            .iter_mut()
            .find(|(k, ..)| k == key)
            .map(|(_, c, ..)| c)
    }

    /// The window of a pointer event, given whether a gesture (button held, swipe going) was open before it: without one the event starts a gesture (or is a hover or wheel) and goes to the top window,
    /// which the gesture keeps until it ends (InputDispatcher's TouchState: later windows do not take a stream already going). `at` = the event's point in the Task. Also returns the window's origin, what its events are translated by.
    pub fn pointer_target(
        &mut self,
        in_gesture: bool,
        at: (f32, f32),
    ) -> Option<(&K, &mut C, (i32, i32))> {
        if !in_gesture {
            self.gesture = self
                .list
                .iter()
                .rev()
                .find(|(_, _, f, frame)| {
                    f & FLAG_NOT_TOUCHABLE == 0
                        && (f & FLAG_NOT_TOUCH_MODAL == 0 || frame.is_none_or(|r| r.contains(at)))
                })
                .map(|(k, ..)| k.clone());
        }
        let key = self.gesture.as_ref()?;
        self.list
            .iter_mut()
            .find(|(k, ..)| k == key)
            .map(|(k, c, _, frame)| (&*k, c, frame.map_or((0, 0), |r| (r.left, r.top))))
    }
}

impl<K, C: AsFd> Windows<K, C> {
    /// Each held channel's key and fd, in add order: the input loop polls them ([`wake::InputReceiver::wait`]).
    pub fn channel_fds(&self) -> impl Iterator<Item = (&K, BorrowedFd<'_>)> {
        self.list.iter().map(|(k, c, ..)| (k, c.as_fd()))
    }
}

/// The channels of one poll round: a readable one is drained by `finished` (false: the channel is broken), and one that hung up, failed or broke is removed.
/// A key the windows no longer hold is skipped. True when a window was removed, so the focus may have moved.
pub fn settle<K: PartialEq + Clone, C>(
    windows: &mut Windows<K, C>,
    channels: &[(K, PollFlags)],
    mut finished: impl FnMut(&mut C) -> bool,
) -> bool {
    let mut removed = false;
    for (key, revents) in channels {
        let drained = if revents.contains(PollFlags::IN) {
            match windows.channel(key) {
                Some(channel) => finished(channel),
                None => continue,
            }
        } else {
            true
        };
        if !drained || revents.intersects(PollFlags::ERR | PollFlags::HUP | PollFlags::NVAL) {
            removed |= windows.remove(key);
        }
    }
    removed
}

/// The script's steps, in order. A bad item (kind, shape, non-finite position) refuses the whole script; an empty script has no steps.
pub fn parse_script(script: &str) -> Result<Vec<Step>, String> {
    script
        .split_whitespace()
        .map(|item| {
            let (kind, at) = item
                .split_once(':')
                .ok_or_else(|| format!("script item {item:?}: want kind:x,y"))?;
            if kind == "wait" {
                let ms = at
                    .parse::<u64>()
                    .map_err(|_| format!("script item {item:?}: want wait:MS"))?;
                return Ok(Step::Wait(std::time::Duration::from_millis(ms)));
            }
            if kind == "focus" {
                return match at {
                    "in" => Ok(Step::Focus(true)),
                    "out" => Ok(Step::Focus(false)),
                    _ => Err(format!("script item {item:?}: want focus:in or focus:out")),
                };
            }
            if kind == "key-down" || kind == "key-up" {
                let code = at
                    .parse::<u32>()
                    .map_err(|_| format!("script item {item:?}: want {kind}:CODE"))?;
                return Ok(Step::Key(kind == "key-down", code));
            }
            let (x, y) = at
                .split_once(',')
                .ok_or_else(|| format!("script item {item:?}: want kind:x,y"))?;
            let phase = match kind {
                "down" => PointerPhase::Down(PointerButton::Primary),
                "move" => PointerPhase::Move,
                "up" => PointerPhase::Up(PointerButton::Primary),
                _ => {
                    return Err(format!(
                        "script item {item:?}: kind is down, move, up, wait, focus, key-down or key-up"
                    ));
                }
            };
            let coord = |v: &str| {
                v.parse::<f64>()
                    .ok()
                    .filter(|c| c.is_finite())
                    .ok_or_else(|| format!("script item {item:?}: {v:?} is no finite number"))
            };
            Ok(Step::Pointer(
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

pub mod key;
pub mod pointer;
pub mod touch;
pub mod wake;

pub use wake::InputSender;

#[cfg(feature = "converter")]
pub use publish::spawn;

/// Converter-only: the publisher (AOSP InputPublisher over the channel) and the thread that owns it.
#[cfg(feature = "converter")]
mod publish {
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::Instant;

    use crate::tilt::Tilt;
    use atl_libinput::{KeyEvent, KeyLayoutMap, MotionEvent, Pointer, Publisher};
    use atl_platform_ui::{InputEvent, InputSource, PointerEvent};
    use binder::SpIBinder;

    use super::key::{self, Key};
    use super::pointer::{self, Motion};
    use super::touch;
    use super::wake::{self, InputReceiver, InputSender};
    use super::{HeadlessInput, Message, Step, Windows, settle};

    /// The default display.
    const DISPLAY_ID: i32 = 0;
    /// Event times are AOSP's ns, the platform's are ms.
    const NANOS_PER_MS: i64 = 1_000_000;
    /// The channel's name in AOSP's logs.
    const CHANNEL: &str = "atl-headless-pointer";

    /// AOSP's key layout of a keyboard with no own files (KeyMap::load's Generic fallback), where the environment links it.
    const KEY_LAYOUT: &str = atl_android_paths::SYSTEM_GENERIC_KEY_LAYOUT;

    /// `k` as the KeyEvent numbered `seq`.
    fn key_event(k: &Key, seq: u32) -> KeyEvent {
        KeyEvent {
            seq,
            event_id: i32::try_from(seq).unwrap_or(i32::MAX),
            device_id: k.device_id,
            source: k.source,
            display_id: DISPLAY_ID,
            action: k.action,
            flags: k.flags,
            key_code: k.key_code,
            scan_code: k.scan_code,
            meta_state: k.meta_state,
            repeat_count: k.repeat_count,
            down_time: i64::from(k.down_ms) * NANOS_PER_MS,
            event_time: i64::from(k.time_ms) * NANOS_PER_MS,
        }
    }

    /// `m` as the MotionEvent numbered `seq` with the keyboard's `meta_state`. Coordinates stay the Task's; the transform takes them to the window at `origin` (WindowInfo.transform), getRawX keeps the Task's.
    fn motion(m: &Motion, seq: u32, meta_state: i32, origin: (i32, i32)) -> MotionEvent {
        MotionEvent {
            seq,
            event_id: i32::try_from(seq).unwrap_or(i32::MAX),
            device_id: m.device_id,
            source: m.source,
            display_id: DISPLAY_ID,
            action: m.action,
            action_button: m.action_button,
            flags: m.flags,
            edge_flags: 0,
            meta_state,
            button_state: m.button_state,
            classification: m.classification,
            transform: [1.0, 0.0, 0.0, 1.0, -(origin.0 as f32), -(origin.1 as f32)],
            raw_transform: [1.0, 0.0, 0.0, 1.0, 0.0, 0.0],
            x_precision: 1.0,
            y_precision: 1.0,
            // a mouse hit-tests at the cursor (MotionEvent.getXDispatchLocation); its NaN check is `!=`, always true, so NaN hits no child
            x_cursor: m.cursor.x as f32,
            y_cursor: m.cursor.y as f32,
            down_time: i64::from(m.down_ms) * NANOS_PER_MS,
            event_time: i64::from(m.time_ms) * NANOS_PER_MS,
            pointers: m
                .pointers
                .iter()
                .map(|p| Pointer {
                    id: p.id,
                    tool_type: p.tool_type,
                    x: p.at.x as f32,
                    y: p.at.y as f32,
                    axes: p.axes.clone(),
                })
                .collect(),
        }
    }

    /// The seq after `last`. InputPublisher refuses seq 0 (BAD_VALUE, InputTransport.cpp publishMotionEvent), which lost the gesture's DOWN: 0 is skipped, first and on wrap.
    fn next_seq(last: u32) -> u32 {
        last.wrapping_add(1).max(1)
    }

    /// AOSP's `DEAD_OBJECT` (= `-EPIPE`, utils/Errors.h): InputPublisher's status when the window's end of the channel is closed.
    const DEAD_OBJECT: i32 = -32;

    /// The dispatch state: the windows' publishers by channel token, the pointer, the keyboard and its layout (None: Generic.kl not loadable, keys dropped), the last seq.
    #[derive(Default)]
    struct State {
        windows: RefCell<Windows<SpIBinder, Publisher>>,
        pointer: RefCell<pointer::Pointer>,
        touch: RefCell<touch::Touch>,
        keys: RefCell<key::Keys>,
        layout: Option<KeyLayoutMap>,
        seq: Cell<u32>,
        /// The key tilt (`ro.atl.tilt`), shared with its injector thread; None: off.
        tilt: Option<Arc<Mutex<Tilt>>>,
    }

    /// Runs `f` on the tilt, when on.
    fn with_tilt(state: &State, f: impl FnOnce(&mut Tilt)) {
        if let Some(t) = &state.tilt {
            f(&mut t.lock().unwrap_or_else(|e| e.into_inner()));
        }
    }

    /// Publishes `keys` to the focused window (InputDispatcher sends keys to the focused window, none while the desktop window is inactive).
    fn publish_keys(state: &State, keys: &[Key]) {
        if keys.is_empty() {
            return;
        }
        let mut windows = state.windows.borrow_mut();
        let Some(publisher) = windows.focused_channel() else {
            tracing::warn!(
                "input: no focused window takes {} key(s), dropped",
                keys.len()
            );
            return;
        };
        for k in keys {
            let seq = next_seq(state.seq.get());
            state.seq.set(seq);
            if let Err(err) = publisher.publish_key(&key_event(k, seq)) {
                tracing::warn!("input: publishing key {seq}: {err:?}");
                return;
            }
        }
    }

    /// Publishes the events of one platform event: keys to the focused window, MotionEvents to the pointer's window ([`Windows::pointer_target`]).
    /// A channel whose window end is closed is dropped (InputDispatcher::handleReceiveCallback: "unrecoverably broken" -> removeInputChannelLocked).
    fn publish(state: &State, event: InputEvent) {
        if let InputEvent::Focus(active) = event {
            if !active {
                // the window keeps its focus until its held keys are cancelled
                let cancels = state
                    .keys
                    .borrow_mut()
                    .cancel(atl_libinput::update_meta_state);
                publish_keys(state, &cancels);
            }
            state.windows.borrow_mut().set_active(active);
            publish_focus(state);
            return;
        }
        if let InputEvent::Key(e) = event {
            let Some(layout) = &state.layout else {
                return;
            };
            let map = |scan: u32| {
                layout
                    .map_key(i32::try_from(scan).ok()?)
                    .map(|(code, _)| code)
            };
            let k = state
                .keys
                .borrow_mut()
                .translate(e, map, atl_libinput::update_meta_state);
            // keys go both ways: the app gets them, and the tilt follows those its focused window got
            if state.windows.borrow().has_focus() {
                with_tilt(state, |t| {
                    if let Some(k) = &k {
                        t.key(k.key_code, k.action == key::ACTION_DOWN);
                    }
                });
            }
            publish_keys(state, k.as_slice());
            return;
        }
        let meta_state = state.keys.borrow().meta_state();
        let (in_gesture, motions) = {
            let mut pointer_state = state.pointer.borrow_mut();
            let mut touch = state.touch.borrow_mut();
            // one window holds every open gesture: InputDispatcher keeps a touched window per device, and all devices here start on the same top window
            let in_gesture = pointer_state.in_gesture() || touch.in_gesture();
            let motions = match event {
                InputEvent::Touch(e) => touch.translate(e),
                other => pointer_state.translate(other),
            };
            (in_gesture, motions)
        };
        // AOSP synthesizeButtonKeys: a back or forward button's DOWN goes before the event's motions, its UP after them
        let (downs, ups) = pointer::button_keys(&motions, meta_state);
        publish_keys(state, &downs);
        publish_motions(state, &motions, in_gesture, meta_state);
        publish_keys(state, &ups);
    }

    /// Publishes `motions` to the pointer's window. Finished signals are read by the loop, when the channel is readable.
    fn publish_motions(state: &State, motions: &[Motion], in_gesture: bool, meta_state: i32) {
        if motions.is_empty() {
            return;
        }
        let mut windows = state.windows.borrow_mut();
        let at = motions[0]
            .pointers
            .first()
            .map_or((0.0, 0.0), |p| (p.at.x as f32, p.at.y as f32));
        let Some((token, publisher, origin)) = windows.pointer_target(in_gesture, at) else {
            tracing::warn!("input: no window takes {} event(s), dropped", motions.len());
            return;
        };
        for m in motions {
            let seq = next_seq(state.seq.get());
            state.seq.set(seq);
            if let Err(err) = publisher.publish_motion(&motion(m, seq, meta_state, origin)) {
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
    }

    /// Reads the finished signals a channel holds (receiveConsumerResponse until none). False when a read fails: the loop then removes the channel, since a channel left readable with every read failing spins the loop.
    /// `Ok(None)` also covers a non-finished message; the next poll reports whatever is left.
    fn drain_finished(publisher: &mut Publisher) -> bool {
        loop {
            match publisher.receive_finished() {
                Ok(Some(finished)) => tracing::debug!(
                    "input: finished seq {} handled {}",
                    finished.seq,
                    finished.handled
                ),
                Ok(None) => return true,
                Err(err) => {
                    tracing::warn!("input: finished signal: {err:?}");
                    return false;
                }
            }
        }
    }

    /// Publishes the focus events owed ([`Windows::focus_changes`]) to the windows still held.
    fn publish_focus(state: &State) {
        let mut windows = state.windows.borrow_mut();
        let changes = windows.focus_changes();
        if !changes.is_empty() {
            let focused = windows.has_focus();
            with_tilt(state, |t| t.focus(focused));
        }
        for (token, has_focus) in changes {
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
    /// A turn waits on the wake end and every window's channel, with no timeout. It settles the channels first: the round polled them as they were, and a Window message handled before them could replace a hung-up channel under its key.
    fn dispatch(
        rx: InputReceiver<Message>,
        tx: InputSender<Message>,
        mut script: Vec<Step>,
        tilt: Option<Arc<Mutex<Tilt>>>,
    ) {
        let layout = KeyLayoutMap::load(KEY_LAYOUT)
            .inspect_err(|err| {
                tracing::warn!("input: key layout {KEY_LAYOUT}: {err:?}; keys dropped")
            })
            .ok();
        let state = Rc::new(State {
            layout,
            tilt,
            ..State::default()
        });
        let source = HeadlessInput::default();
        let publish_state = Rc::clone(&state);
        source.on_events(move |event| publish(&publish_state, event));
        let started = Instant::now();
        loop {
            let round = {
                let windows = state.windows.borrow();
                let channels: Vec<_> = windows
                    .channel_fds()
                    .map(|(key, fd)| (key.clone(), fd))
                    .collect();
                rx.wait(&channels, None)
            };
            let round = match round {
                Ok(round) => round,
                Err(err) => {
                    tracing::error!("input: wait: {err}; the input thread ends");
                    return;
                }
            };
            if settle(
                &mut state.windows.borrow_mut(),
                &round.channels,
                drain_finished,
            ) {
                publish_focus(&state);
            }
            if !round.wake {
                continue;
            }
            rx.ack();
            for message in rx.try_iter() {
                match message {
                    Message::Window { token, fd, flags } => match Publisher::new(fd, CHANNEL) {
                        Ok(publisher) => {
                            state.windows.borrow_mut().add(token, publisher, flags);
                            publish_focus(&state);
                        }
                        Err(err) => tracing::warn!("input: input channel refused: {err:?}"),
                    },
                    Message::WindowRemoved(token) => {
                        state.windows.borrow_mut().remove(&token);
                        publish_focus(&state);
                    }
                    Message::WindowFrame { token, frame } => {
                        state.windows.borrow_mut().set_frame(&token, frame);
                    }
                    Message::Event(event) => source.inject(event),
                    // once: the script's events come back as Message::Event from its own thread, so a wait never holds up the channels a later window brings
                    Message::FirstFrame if !script.is_empty() => {
                        let steps = std::mem::take(&mut script);
                        let tx = tx.clone();
                        let run = move || {
                            for step in steps {
                                let time_ms = u32::try_from(started.elapsed().as_millis())
                                    .unwrap_or(u32::MAX);
                                let event = match step {
                                    Step::Wait(d) => {
                                        thread::sleep(d);
                                        continue;
                                    }
                                    Step::Pointer(phase, position) => {
                                        InputEvent::Pointer(PointerEvent {
                                            phase,
                                            position,
                                            time_ms,
                                        })
                                    }
                                    Step::Focus(active) => InputEvent::Focus(active),
                                    Step::Key(down, scan_code) => {
                                        InputEvent::Key(atl_platform_ui::KeyEvent {
                                            down,
                                            scan_code,
                                            locks: Default::default(),
                                            time_ms,
                                        })
                                    }
                                };
                                if tx.send(Message::Event(event)).is_err() {
                                    return;
                                }
                            }
                        };
                        if let Err(err) = thread::Builder::new()
                            .name("atl-input-script".to_owned())
                            .spawn(run)
                        {
                            tracing::warn!("input: script thread: {err}");
                        }
                    }
                    Message::FirstFrame => {}
                }
            }
        }
    }

    /// Starts the input thread with `script` (empty: nothing injected, channels still taken). The sender takes each window's channel and the first frame.
    /// `wake_enabled` false is the deliberate-failure break (start.rs BREAK_INPUT_NO_WAKE): sends queue but never wake the loop.
    /// `tilt`: the key tilt the keys and focus drive (its injector runs on its own thread).
    pub fn spawn(
        script: Vec<Step>,
        wake_enabled: bool,
        tilt: Option<Arc<Mutex<Tilt>>>,
    ) -> Result<InputSender<Message>, String> {
        let (tx, rx) = wake::channel(wake_enabled).map_err(|e| format!("input wake pair: {e}"))?;
        // the thread's own sender (for the script's events) keeps its loop open for the process's life; it ends with the process, as before
        let own = tx.clone();
        thread::Builder::new()
            .name("atl-input".to_owned())
            .spawn(move || dispatch(rx, own, script, tilt))
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
                device_id: 4,
                action: 1,
                action_button: 0,
                button_state: 0,
                flags: 0x8,
                classification: 3,
                source: 0x2002,
                pointers: vec![pointer::MotionPointer {
                    id: 0,
                    tool_type: 1,
                    at: PhysPoint { x: 260.0, y: 340.0 },
                    axes: vec![(2, 0.0)],
                }],
                cursor: PhysPoint { x: 250.0, y: 300.0 },
                down_ms: 5,
                time_ms: 7,
            };
            // AMETA_SHIFT_ON | AMETA_SHIFT_LEFT_ON (android/input.h): the keyboard's meta state rides on the motion
            let m = motion(&m, 3, 0x41, (600, 390));
            assert_eq!(m.meta_state, 0x41);
            assert_eq!(
                (
                    m.seq,
                    m.device_id,
                    m.action,
                    m.source,
                    m.flags,
                    m.classification
                ),
                (3, 4, 1, 0x2002, 0x8, 3)
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
            // a window at (600, 390): getX is the point less the origin, getRawX the point
            assert_eq!(m.transform, [1.0, 0.0, 0.0, 1.0, -600.0, -390.0]);
            assert_eq!(m.raw_transform, [1.0, 0.0, 0.0, 1.0, 0.0, 0.0]);
            assert_eq!((m.down_time, m.event_time), (5_000_000, 7_000_000));
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::os::fd::OwnedFd;
    use std::rc::Rc;
    use std::thread;
    use std::time::Duration;

    use atl_platform_ui::PointerEvent;
    use rustix::net::{AddressFamily, SocketFlags, SocketType, socketpair};
    use rustix::time::Timespec;

    use super::*;

    /// A non-modal popup takes only points in its frame, right and bottom edges out (windowAcceptsTouchAt); a modal window takes any point; origins come with the target.
    #[test]
    fn touch_hit_test_follows_frames() {
        let frame = |left, top, right, bottom| Frame {
            left,
            top,
            right,
            bottom,
        };
        let mut w: Windows<u32, ()> = Windows::default();
        w.add(1, (), 0);
        w.set_frame(&1, frame(0, 0, 400, 300));
        w.add(2, (), FLAG_NOT_TOUCH_MODAL);
        w.set_frame(&2, frame(100, 50, 200, 150));
        let hit =
            |w: &mut Windows<u32, ()>, at| w.pointer_target(false, at).map(|(k, _, o)| (*k, o));
        assert_eq!(hit(&mut w, (100.0, 50.0)), Some((2, (100, 50))));
        assert_eq!(hit(&mut w, (199.9, 149.9)), Some((2, (100, 50))));
        assert_eq!(hit(&mut w, (200.0, 100.0)), Some((1, (0, 0))));
        assert_eq!(hit(&mut w, (150.0, 150.0)), Some((1, (0, 0))));
        w.add(3, (), 0);
        w.set_frame(&3, frame(100, 50, 200, 150));
        assert_eq!(hit(&mut w, (10.0, 10.0)), Some((3, (100, 50))));
    }

    /// The script's items in order: kinds, positions and phases as written; a bad item refuses the whole script; an empty script has no steps.
    #[test]
    fn parse_script_reads_the_items_in_order() {
        let want = vec![
            Step::Pointer(
                PointerPhase::Down(PointerButton::Primary),
                PhysPoint { x: 200.0, y: 300.0 },
            ),
            Step::Pointer(PointerPhase::Move, PhysPoint { x: 260.0, y: 340.0 }),
            Step::Wait(std::time::Duration::from_millis(1500)),
            Step::Pointer(
                PointerPhase::Up(PointerButton::Primary),
                PhysPoint { x: 260.0, y: 340.0 },
            ),
        ];
        assert_eq!(
            parse_script("down:200,300 move:260,340 wait:1500 up:260,340"),
            Ok(want)
        );
        assert_eq!(parse_script(""), Ok(Vec::new()));
        for bad in [
            "tap:1,2",
            "down:1",
            "down:1,nan",
            "down:a,2",
            "wait:-1",
            "wait:1.5",
        ] {
            assert!(parse_script(bad).is_err(), "{bad}");
        }
    }

    /// Main (1) opens a second window (2) and closes it: events go to 2 while it is on top, to 1 again after its removal (the Calculator History bug: the last-added channel kept every event).
    /// A gesture keeps the window it started on when another window comes up mid-gesture, and is dropped (no window) when its window goes.
    #[test]
    fn windows_route_to_the_top_and_keep_a_gesture() {
        let mut w = Windows::<u32, &str>::default();
        assert!(w.pointer_target(false, (0.0, 0.0)).is_none());
        w.add(1, "main", 0);
        w.add(2, "history", 0);
        assert_eq!(
            w.pointer_target(false, (0.0, 0.0)).map(|(k, ..)| *k),
            Some(2)
        );
        assert!(w.remove(&2));
        assert!(!w.remove(&2));
        assert_eq!(
            w.pointer_target(false, (0.0, 0.0)).map(|(k, ..)| *k),
            Some(1)
        );
        // press on 1 (no gesture before), 3 comes up, the move with the button held stays on 1
        assert_eq!(
            w.pointer_target(false, (0.0, 0.0)).map(|(k, ..)| *k),
            Some(1)
        );
        w.add(3, "dialog", 0);
        assert_eq!(
            w.pointer_target(true, (0.0, 0.0)).map(|(k, ..)| *k),
            Some(1)
        );
        // the gesture's window goes: the rest of the gesture has no window; the next gesture starts on the top
        w.remove(&1);
        assert!(w.pointer_target(true, (0.0, 0.0)).is_none());
        assert_eq!(
            w.pointer_target(false, (0.0, 0.0)).map(|(k, ..)| *k),
            Some(3)
        );
    }

    /// Focus goes to the top window only while the desktop window is active: replayed to a window added later, moved when the top changes, taken (false) on deactivation.
    #[test]
    fn focus_follows_the_top_window_while_active() {
        let mut w = Windows::<u32, &str>::default();
        w.add(1, "main", 0);
        assert!(w.focus_changes().is_empty());
        w.set_active(true);
        assert_eq!(w.focus_changes(), [(1, true)]);
        assert!(w.focus_changes().is_empty());
        w.add(2, "second", 0);
        assert_eq!(w.focus_changes(), [(1, false), (2, true)]);
        w.remove(&2);
        assert_eq!(w.focus_changes(), [(2, false), (1, true)]);
        w.set_active(false);
        assert_eq!(w.focus_changes(), [(1, false)]);
        w.remove(&1);
        w.set_active(true);
        assert!(w.focus_changes().is_empty());
        w.add(3, "late", 0);
        assert_eq!(w.focus_changes(), [(3, true)]);
    }

    /// A toast (FLAG_NOT_TOUCHABLE | FLAG_NOT_FOCUSABLE, as Toast's TN lays it out) on top of the activity window takes neither the pointer nor the focus; they stay with the window below.
    #[test]
    fn a_toast_takes_neither_touch_nor_focus() {
        let mut w = Windows::<u32, &str>::default();
        w.set_active(true);
        w.add(1, "main", 0);
        assert_eq!(w.focus_changes(), [(1, true)]);
        w.add(2, "toast", FLAG_NOT_TOUCHABLE | FLAG_NOT_FOCUSABLE);
        assert!(w.focus_changes().is_empty());
        assert_eq!(
            w.pointer_target(false, (0.0, 0.0)).map(|(k, ..)| *k),
            Some(1)
        );
        w.remove(&1);
        assert_eq!(w.focus_changes(), [(1, false)]);
        assert!(w.pointer_target(false, (0.0, 0.0)).is_none());
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

    /// A channel hangs up while the loop waits: the round reports only that channel, settle removes its window and keeps the other, and a message sent afterwards still wakes the loop.
    #[test]
    fn a_hung_up_channel_is_removed_mid_wait() {
        let pair = || {
            socketpair(
                AddressFamily::UNIX,
                SocketType::SEQPACKET,
                SocketFlags::CLOEXEC | SocketFlags::NONBLOCK,
                None,
            )
            .expect("socket pair")
        };
        let (kept, _kept_peer) = pair();
        let (gone, gone_peer) = pair();
        let mut windows = Windows::<u32, OwnedFd>::default();
        windows.add(1, kept, 0);
        windows.add(2, gone, 0);
        let (tx, rx) = wake::channel::<u32>(true).expect("wake pair");
        let closer = thread::spawn(move || {
            thread::sleep(Duration::from_millis(20));
            drop(gone_peer);
        });
        let channels: Vec<_> = windows.channel_fds().map(|(key, fd)| (*key, fd)).collect();
        let round = rx.wait(&channels, None).expect("wait");
        drop(channels);
        assert!(!round.wake);
        assert_eq!(round.channels.len(), 1);
        assert_eq!(round.channels[0].0, 2);
        assert!(round.channels[0].1.contains(PollFlags::HUP));
        assert!(settle(&mut windows, &round.channels, |_| true));
        assert!(windows.channel(&1).is_some());
        assert!(windows.channel(&2).is_none());
        tx.send(7).expect("queue");
        let bound = Timespec {
            tv_sec: 5,
            tv_nsec: 0,
        };
        let round = rx.wait::<u32>(&[], Some(&bound)).expect("wait");
        assert!(round.wake);
        rx.ack();
        assert_eq!(rx.try_iter().collect::<Vec<_>>(), [7]);
        closer.join().expect("closer");
    }
}
