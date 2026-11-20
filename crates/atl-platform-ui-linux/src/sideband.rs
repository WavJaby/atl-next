// SPDX-License-Identifier: MIT
//! Sideband widgets (webview plan S2.2 amended): toolkit widgets registered as the content of a sideband stream `[SIDEBAND_WEB_VIEW, key]` (WebKit's view), drawn by `SceneView`, and the input gate.
//! Input: Android's dispatch decides which View gets an event (OnTouchListener, onInterceptTouchEvent, per-pixel overlap), so a toolkit event aimed at the widget is held, not delivered:
//! the window's controllers (capture phase) publish it to Android as every event and record the monotonic time they stamped it with ([`stamp`]); the gate on the widget (capture phase) holds it;
//! when the provider's View receives the matching MotionEvent / KeyEvent it claims by that time ([`claim`]) and the held events are put back on the display's queue (`gdk_display_put_event`,
//! deprecated since GTK 4.10 with no replacement: the one call site), marked so the window's controllers skip them and the gate lets them through to the widget.
//! Everything here runs on the UI thread; the statics are statics, not thread-locals (module rule: no module object in the process's thread-local teardown).

use std::collections::VecDeque;
use std::sync::Mutex;

use atl_platform_ui_api::abi::Callback;
use atl_platform_ui_api::webview::{ClaimReply, InputClaim, SIDEBAND_WEB_VIEW};
use gtk4::prelude::*;
use gtk4::{gdk, glib};

/// Held events per widget at most: a bound on memory when nothing claims (the View is covered for a whole hover); 512 is ~8 s of 60 Hz motion, far past any claim's delay (one Looper trip, one frame).
const MAX_HELD: usize = 512;

/// Recent (event, stamp) pairs and replay marks kept: the events of one dispatch at most a few, so 64 only bounds a mark whose replay never reached a gate.
const MAX_MARKS: usize = 64;

/// One held event: `event`, the monotonic ns Android got it at, whether it is a key, a press (a gesture's start) or a release (its end), and whether a press came from a mouse button
/// (a toolkit button press, not a touch contact: what the page itself is given, whatever MotionEvent the desktop adaptation made of it).
#[derive(Debug)]
pub struct Held<E> {
    pub event: E,
    pub ns: i64,
    pub key: bool,
    pub press: bool,
    pub release: bool,
    pub mouse: bool,
}

impl<E> Held<E> {
    /// The counter class of this event: `key`, `press`, `release` or `motion`.
    pub fn class(&self) -> &'static str {
        match (self.key, self.press, self.release) {
            (true, ..) => "key",
            (_, true, _) => "press",
            (_, _, true) => "release",
            _ => "motion",
        }
    }
}

/// The hold and claim rules over events of type `E` (pure: unit-tested; [`Entry`] runs it on GdkEvents).
#[derive(Debug)]
pub struct Gate<E> {
    held: VecDeque<Held<E>>,
    /// The page got the press of the open gesture (a Down was claimed, no Up / Cancel yet).
    pressed: bool,
    /// Android cancelled the open gesture (a parent intercepted): its remaining events never reach the page, up to its release.
    cancelled: bool,
}

impl<E> Default for Gate<E> {
    fn default() -> Self {
        Self {
            held: VecDeque::new(),
            pressed: false,
            cancelled: false,
        }
    }
}

/// What a claim did: the events to put back, in order, and whether the page was left mid-press (Android cancelled a gesture whose press reached the page: no cancel can be sent, webview plan S2.2).
pub struct Claimed<E> {
    pub replay: Vec<E>,
    /// [`Held::class`] of each `replay` event, in order.
    pub classes: Vec<&'static str>,
    pub left_pressed: bool,
    /// A Down claim replayed a mouse-button press ([`Held::mouse`]): the provider applies Chromium's mouse-down focus rule (a touch press never focuses a WebView).
    pub mouse_press: bool,
    /// Classes of the held events the claim dropped unclaimed (before a Down, or a cancelled gesture's).
    pub dropped: Vec<&'static str>,
}

impl<E> Gate<E> {
    /// Holds `h`. A pointer event of a cancelled gesture is dropped (its release ends the cancel). A press while an unclaimed press or release is still held: that gesture went elsewhere,
    /// its pointer events are dropped.
    /// Returns the classes ([`Held::class`]) of the events dropped unclaimed, `h` itself included when it is dropped.
    pub fn hold(&mut self, h: Held<E>) -> Vec<&'static str> {
        let mut dropped = Vec::new();
        if !h.key && self.cancelled {
            if h.release {
                self.cancelled = false;
            }
            dropped.push(h.class());
            return dropped;
        }
        if h.press && self.held.iter().any(|o| !o.key && (o.press || o.release)) {
            dropped.extend(self.held.iter().filter(|o| !o.key).map(Held::class));
            self.held.retain(|o| o.key);
        }
        if self.held.len() == MAX_HELD {
            dropped.extend(self.held.pop_front().as_ref().map(Held::class));
        }
        self.held.push_back(h);
        dropped
    }

    /// Android delivered `claim` stamped `t` (ns) to the View.
    pub fn claim(&mut self, claim: InputClaim, t: i64) -> Claimed<E> {
        let mut left_pressed = false;
        let mut dropped = Vec::new();
        match claim {
            InputClaim::Down => {
                // what came before the press went elsewhere (or was hover nobody claimed)
                dropped.extend(
                    self.held
                        .iter()
                        .filter(|h| !h.key && h.ns < t)
                        .map(Held::class),
                );
                self.held.retain(|h| h.key || h.ns >= t);
                self.pressed = true;
                self.cancelled = false;
            }
            InputClaim::Up => self.pressed = false,
            InputClaim::Cancel => {
                let released = self.held.iter().any(|h| !h.key && h.release && h.ns <= t);
                dropped.extend(
                    self.held
                        .iter()
                        .filter(|h| !h.key && h.ns <= t)
                        .map(Held::class),
                );
                self.held.retain(|h| h.key || h.ns > t);
                left_pressed = self.pressed;
                self.pressed = false;
                self.cancelled = !released;
                return Claimed {
                    replay: Vec::new(),
                    classes: Vec::new(),
                    left_pressed,
                    mouse_press: false,
                    dropped,
                };
            }
            InputClaim::Move | InputClaim::Hover | InputClaim::Key => {}
        }
        // each claim takes only its own kinds: a press only with Down, a release only with Up. Events of one click can share a stamp (ms toolkit times), and Android hands hover to a
        // WebView under a View that takes the click (an overlapping button does not handle hover), so a Hover claim must not deliver that click's press and release.
        let takes = |h: &Held<E>| match claim {
            InputClaim::Key => h.key,
            InputClaim::Down => !h.key && !h.release,
            InputClaim::Up => !h.key && !h.press,
            _ => !h.key && !h.press && !h.release,
        };
        let mut replay = Vec::new();
        let mut classes = Vec::new();
        let mut mouse_press = false;
        let mut rest = VecDeque::with_capacity(self.held.len());
        for h in self.held.drain(..) {
            if h.ns <= t && takes(&h) {
                mouse_press |= h.press && h.mouse;
                classes.push(h.class());
                replay.push(h.event);
            } else {
                rest.push_back(h);
            }
        }
        self.held = rest;
        Claimed {
            replay,
            classes,
            left_pressed,
            mouse_press,
            dropped,
        }
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.held.len()
    }
}

/// A registered widget, its gate and its unhandled-key report (UiVtable::sideband_attach).
struct Entry {
    widget: gtk4::Widget,
    gate: std::cell::RefCell<Gate<gdk::Event>>,
    unhandled_key: std::cell::RefCell<Callback<i64>>,
}

/// The registered widgets, by key.
static SIDEBANDS: Mutex<Vec<(i32, glib::thread_guard::ThreadGuard<std::rc::Rc<Entry>>)>> =
    Mutex::new(Vec::new());

/// (event address, the event, monotonic ns) of the events the window's controllers published last ([`stamp`]); the reference keeps the address from being reused while it is listed.
type Stamp = (usize, glib::thread_guard::ThreadGuard<gdk::Event>, i64);
static STAMPS: Mutex<VecDeque<Stamp>> = Mutex::new(VecDeque::new());

/// Events put back on the queue and not yet through their gate ([`replayed`]), each with a reference of its own: GTK may drop a put-back event (measured: a press put back after later
/// input never reached the widget), and a mark keyed by a freed event's address then matched the next real event allocated there, which the window's controllers skipped unpublished.
static REPLAYING: Mutex<VecDeque<(usize, glib::thread_guard::ThreadGuard<gdk::Event>)>> =
    Mutex::new(VecDeque::new());

/// Key presses a gate let through to its widget: (event address, the event, widget key, Android's stamp). WebKitGTK puts back the same GdkEvent when the page leaves a press unhandled (PageClientImpl::doneWithKeyEvent re-propagates it for the toplevel's
/// accelerators; measured with WebKitGTK 2.54: presses only, after the key's release), so a listed address coming back to the window's controllers is that report ([`unhandled_key`]). Handled presses never come back: the bound evicts them.
type Delivered = (usize, glib::thread_guard::ThreadGuard<gdk::Event>, i32, i64);
static DELIVERED: Mutex<VecDeque<Delivered>> = Mutex::new(VecDeque::new());

fn address(e: &gdk::Event) -> usize {
    use glib::translate::ToGlibPtr;
    let p: *mut gdk::ffi::GdkEvent = e.to_glib_none().0;
    p as usize
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

fn entry(key: i32) -> Option<std::rc::Rc<Entry>> {
    lock(&SIDEBANDS)
        .iter()
        .find(|(k, _)| *k == key)
        .map(|(_, e)| e.get_ref().clone())
}

/// Registers `widget` as the content of `[SIDEBAND_WEB_VIEW, key]` and puts the input gate on it; `unhandled_key` takes the stamps of the presses it leaves unhandled.
pub fn attach(key: i32, widget: gtk4::Widget, unhandled_key: Callback<i64>) {
    let gate = gtk4::EventControllerLegacy::new();
    gate.set_propagation_phase(gtk4::PropagationPhase::Capture);
    gate.connect_event(move |_, event| on_gate(key, event));
    widget.add_controller(gate);
    // the toolkit's keyboard focus follows Android's ([`focus`]) and nothing else: WebKit grabs it on a click, which left the page holding every key while Android sent them to the focused View
    // (a game's GL view: the keys stayed held in the gate, never claimed)
    widget.set_can_focus(false);
    let e = std::rc::Rc::new(Entry {
        widget,
        gate: std::cell::RefCell::default(),
        unhandled_key: std::cell::RefCell::new(unhandled_key),
    });
    let mut list = lock(&SIDEBANDS);
    list.retain(|(k, _)| *k != key);
    list.push((key, glib::thread_guard::ThreadGuard::new(e)));
}

/// Drops `key`'s widget: unparented from the view drawing it (which then draws nothing there), its held events dropped.
pub fn detach(key: i32) {
    let gone = {
        let mut list = lock(&SIDEBANDS);
        list.iter()
            .position(|(k, _)| *k == key)
            .map(|i| list.remove(i).1.into_inner())
    };
    tracing::info!(
        "atl-platform-ui: sideband key {key} detached (registered {})",
        gone.is_some()
    );
    if let Some(e) = gone
        && let Some(parent) = e.widget.parent()
    {
        e.widget.unparent();
        parent.queue_draw();
    }
}

/// The key `w` is registered under, if any (log lines name a widget by it).
pub fn key_of(w: &gtk4::Widget) -> Option<i32> {
    lock(&SIDEBANDS)
        .iter()
        .find(|(_, e)| e.get_ref().widget == *w)
        .map(|(k, _)| *k)
}

/// The widget of a sideband stream, if it names a registered one.
pub fn resolve(stream: &[i32]) -> Option<gtk4::Widget> {
    let [SIDEBAND_WEB_VIEW, key] = stream else {
        return None;
    };
    entry(*key).map(|e| e.widget.clone())
}

/// Whether `w` is a registered widget or inside one: an event aimed there is the gate's, so the window's controllers publish it but must not stop it.
pub fn routes_to_gate(w: Option<gtk4::Widget>) -> bool {
    let Some(w) = w else { return false };
    lock(&SIDEBANDS).iter().any(|(_, e)| {
        *e.get_ref().widget.upcast_ref::<gtk4::Widget>() == w || w.is_ancestor(&e.get_ref().widget)
    })
}

/// The window's controllers published `event` to Android stamped `ns`.
pub fn stamp(event: Option<gdk::Event>, ns: i64) {
    let Some(e) = event else { return };
    let evicted = {
        let mut s = lock(&STAMPS);
        let evicted = (s.len() == MAX_MARKS).then(|| s.pop_front()).flatten();
        s.push_back((address(&e), glib::thread_guard::ThreadGuard::new(e), ns));
        evicted
    };
    drop(evicted);
}

/// Whether `event` is a held event put back on the queue: the window's controllers skip it (Android has it already).
pub fn replayed(event: Option<&gdk::Event>) -> bool {
    event.is_some_and(|e| {
        let a = address(e);
        lock(&REPLAYING).iter().any(|(x, _)| *x == a)
    })
}

/// Whether `event` is a key press a widget's page left unhandled (WebKit put it back, [`DELIVERED`]): its widget's report runs with the press's stamp, from an idle callback (the report reaches the app's
/// Java code, which must not run inside this toolkit dispatch), and the window's controllers neither publish it again (Android had it) nor let it on.
pub fn unhandled_key(event: Option<&gdk::Event>) -> bool {
    let Some(e) = event else { return false };
    let a = address(e);
    let found = {
        let mut d = lock(&DELIVERED);
        d.iter()
            .position(|(x, ..)| *x == a)
            .and_then(|i| d.remove(i))
    };
    let Some((_, held, key, ns)) = found else {
        return false;
    };
    drop(held);
    glib::idle_add_local_once(move || {
        if let Some(e) = entry(key) {
            e.unhandled_key.borrow_mut().call(&ns);
        }
    });
    true
}

/// The stamp the window's controllers published the event at `address` with, now for one they did not publish.
fn stamp_of(address: usize) -> i64 {
    lock(&STAMPS)
        .iter()
        .rev()
        .find(|(x, _, _)| *x == address)
        .map(|(_, _, ns)| *ns)
        .unwrap_or_else(crate::window::monotonic_ns)
}

fn kind(t: gdk::EventType) -> Option<(bool, bool, bool)> {
    use gdk::EventType as T;
    // (key, press, release)
    Some(match t {
        T::ButtonPress | T::TouchBegin => (false, true, false),
        T::ButtonRelease | T::TouchEnd | T::TouchCancel => (false, false, true),
        T::MotionNotify | T::TouchUpdate | T::Scroll | T::EnterNotify | T::LeaveNotify => {
            (false, false, false)
        }
        T::KeyPress | T::KeyRelease => (true, false, false),
        _ => return None,
    })
}

/// The gate on `key`'s widget: a put-back event passes; a fresh one is held, stamped as the window's controllers stamped it (or now, for one they did not publish).
fn on_gate(key: i32, event: &gdk::Event) -> glib::Propagation {
    let a = address(event);
    let mark = {
        let mut r = lock(&REPLAYING);
        r.iter()
            .position(|(x, _)| *x == a)
            .and_then(|i| r.remove(i))
    };
    if mark.is_some() {
        if event.event_type() == gdk::EventType::KeyPress {
            let ns = stamp_of(a);
            let evicted = {
                let mut d = lock(&DELIVERED);
                let evicted = (d.len() == MAX_MARKS).then(|| d.pop_front()).flatten();
                d.push_back((
                    a,
                    glib::thread_guard::ThreadGuard::new(event.clone()),
                    key,
                    ns,
                ));
                evicted
            };
            drop(evicted);
        }
        return glib::Propagation::Proceed;
    }
    let Some((key_event, press, release)) = kind(event.event_type()) else {
        return glib::Propagation::Proceed;
    };
    let Some(e) = entry(key) else {
        return glib::Propagation::Proceed;
    };
    let ns = stamp_of(a);
    let h = Held {
        event: event.clone(),
        ns,
        key: key_event,
        press,
        release,
        // a touch-emulated button press is the touch's (GTK flags it), not the mouse's
        mouse: event.event_type() == gdk::EventType::ButtonPress && !event.is_pointer_emulated(),
    };
    crate::count::hit(&format!("sideband.held.{}", h.class()));
    let dropped = e.gate.borrow_mut().hold(h);
    count_dropped(&dropped);
    glib::Propagation::Stop
}

/// Counts held events dropped unclaimed (Android sent them elsewhere, or the bound evicted them), by class.
fn count_dropped(classes: &[&'static str]) {
    for c in classes {
        crate::count::hit(&format!("sideband.dropped.{c}"));
    }
}

/// The provider's View received `claim` stamped `t` (ns): `key`'s held events up to `t` go back on the queue.
pub fn claim(key: i32, claim: InputClaim, t: i64) -> ClaimReply {
    let Some(e) = entry(key) else {
        crate::count::hit("sideband.claim.no-widget");
        return ClaimReply::default();
    };
    let claimed = e.gate.borrow_mut().claim(claim, t);
    count_dropped(&claimed.dropped);
    if claimed.mouse_press {
        crate::count::hit("sideband.claim.mouse-press");
    }
    for c in &claimed.classes {
        crate::count::hit(&format!("sideband.replayed.{c}"));
    }
    for ev in claimed.replay {
        let evicted = {
            let mut r = lock(&REPLAYING);
            let evicted = (r.len() == MAX_MARKS).then(|| r.pop_front()).flatten();
            r.push_back((
                address(&ev),
                glib::thread_guard::ThreadGuard::new(ev.clone()),
            ));
            evicted
        };
        // a dropped mark's reference goes outside the lock
        drop(evicted);
        if let Some(display) = ev.display() {
            #[allow(deprecated)]
            // GTK4 has no other way to deliver an event to a widget (module doc)
            display.put_event(&ev);
        }
    }
    crate::mirror::resync();
    ClaimReply {
        left_pressed: claimed.left_pressed,
        mouse_press: claimed.mouse_press,
    }
}

/// Android focus on `key`'s View: its widget takes the toolkit's keyboard focus (WebKit's own input method context follows it), or gives it back to the window.
pub fn focus(key: i32, on: bool) {
    let Some(e) = entry(key) else { return };
    tracing::info!("atl-platform-ui: sideband {key} android focus {on}");
    if on {
        e.widget.set_can_focus(true);
        let took = e.widget.grab_focus();
        tracing::info!("atl-platform-ui: sideband {key} took toolkit focus {took}");
    } else {
        if let Some(root) = e.widget.root()
            && root
                .focus()
                .is_some_and(|f| f == e.widget || f.is_ancestor(&e.widget))
        {
            root.set_focus(None::<&gtk4::Widget>);
        }
        e.widget.set_can_focus(false);
    }
    crate::mirror::resync();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ptr(ns: i64, press: bool, release: bool) -> Held<i64> {
        Held {
            event: ns,
            ns,
            key: false,
            press,
            release,
            mouse: false,
        }
    }

    fn key(ns: i64) -> Held<i64> {
        Held {
            event: ns,
            ns,
            key: true,
            press: false,
            release: false,
            mouse: false,
        }
    }

    /// Focus on press is keyed on the physical input (Chromium: mouse-down focuses a WebView, touch ACTION_DOWN does not -- aw_field_trials disables it): only a Down claim that replays
    /// a mouse-button press reports one; a touch press, a Move / Up / Hover claim after a mouse press, and a Cancel report none.
    #[test]
    fn mouse_press_only_from_a_claimed_mouse_button_press() {
        let mouse = |ns, press, release| Held {
            mouse: press,
            ..ptr(ns, press, release)
        };
        let mut g = Gate::default();
        g.hold(mouse(10, true, false));
        g.hold(mouse(15, false, false));
        g.hold(mouse(20, false, true));
        assert!(g.claim(InputClaim::Down, 10).mouse_press);
        assert!(!g.claim(InputClaim::Move, 15).mouse_press);
        assert!(!g.claim(InputClaim::Up, 20).mouse_press);
        g.hold(ptr(30, true, false));
        assert!(!g.claim(InputClaim::Down, 30).mouse_press);
        let mut c = Gate::default();
        c.hold(mouse(40, true, false));
        assert!(!c.claim(InputClaim::Cancel, 40).mouse_press);
    }

    /// A tap the View gets: press at 10, release at 20; Android claims Down 10, Up 20; both replayed in order, nothing left.
    #[test]
    fn claimed_gesture_replays_in_order() {
        let mut g = Gate::default();
        g.hold(ptr(10, true, false));
        g.hold(ptr(15, false, false));
        g.hold(ptr(20, false, true));
        assert_eq!(g.claim(InputClaim::Down, 10).replay, vec![10]);
        assert_eq!(g.claim(InputClaim::Move, 15).replay, vec![15]);
        let up = g.claim(InputClaim::Up, 20);
        assert_eq!((up.replay, up.left_pressed), (vec![20], false));
        assert_eq!(g.len(), 0);
    }

    /// A tap Android sent elsewhere (an overlapping View) is never claimed; the next press drops it, and its own claim replays only itself.
    #[test]
    fn unclaimed_gesture_is_dropped_by_the_next_press() {
        let mut g = Gate::default();
        g.hold(ptr(10, true, false));
        g.hold(ptr(20, false, true));
        // the counters see the unclaimed tap as dropped
        assert_eq!(g.hold(ptr(30, true, false)), vec!["press", "release"]);
        assert_eq!(g.len(), 1);
        assert_eq!(g.claim(InputClaim::Down, 30).replay, vec![30]);
    }

    /// Batched moves: one Move claim at the last sample's time replays every held move up to it, not later ones.
    #[test]
    fn batched_move_claims_every_sample_up_to_its_time() {
        let mut g = Gate::default();
        g.hold(ptr(10, true, false));
        let _ = g.claim(InputClaim::Down, 10);
        for t in [11, 12, 13, 14] {
            g.hold(ptr(t, false, false));
        }
        assert_eq!(g.claim(InputClaim::Move, 13).replay, vec![11, 12, 13]);
        assert_eq!(g.len(), 1);
    }

    /// Intercept: Down reached the page, then Cancel at 12: the claim reports the page left mid-press; the gesture's later events, up to and including its release, are dropped at hold;
    /// the next gesture holds again.
    #[test]
    fn cancel_leaves_the_page_pressed_and_drops_the_rest_of_the_gesture() {
        let mut g = Gate::default();
        g.hold(ptr(10, true, false));
        let _ = g.claim(InputClaim::Down, 10);
        g.hold(ptr(12, false, false));
        let c = g.claim(InputClaim::Cancel, 12);
        assert!(c.replay.is_empty() && c.left_pressed);
        g.hold(ptr(14, false, false));
        g.hold(ptr(16, false, true));
        assert_eq!(g.len(), 0);
        g.hold(ptr(30, true, false));
        assert_eq!(g.claim(InputClaim::Down, 30).replay, vec![30]);
        // a cancel without a claimed press (the page never saw the gesture) reports nothing
        let mut fresh = Gate::<i64>::default();
        fresh.hold(ptr(40, true, false));
        assert!(!fresh.claim(InputClaim::Cancel, 41).left_pressed);
    }

    /// Keys and pointer events claim apart: a Key claim replays keys only, a Down drops no key.
    #[test]
    fn keys_and_pointer_claim_apart() {
        let mut g = Gate::default();
        g.hold(key(5));
        g.hold(ptr(10, true, false));
        assert_eq!(g.claim(InputClaim::Down, 10).replay, vec![10]);
        assert_eq!(g.claim(InputClaim::Key, 5).replay, vec![5]);
        assert_eq!(g.len(), 0);
    }

    /// A click on a View over the WebView (measured: the overlay's click reached the page): motion, press and release share one stamp; Android hands the hover to the WebView, not the click.
    /// The Hover claim replays the motion only; the next gesture's press drops the unclaimed press and release.
    #[test]
    fn hover_claim_does_not_deliver_a_click() {
        let mut g = Gate::default();
        g.hold(ptr(50, false, false));
        g.hold(ptr(50, true, false));
        g.hold(ptr(50, false, true));
        assert_eq!(g.claim(InputClaim::Hover, 50).replay, vec![50]);
        assert_eq!(g.len(), 2);
        g.hold(ptr(60, true, false));
        assert_eq!(g.len(), 1);
        assert_eq!(g.claim(InputClaim::Down, 60).replay, vec![60]);
    }

    /// The bound: past MAX_HELD the oldest goes.
    #[test]
    fn held_is_bounded() {
        let mut g = Gate::default();
        for t in 0..(MAX_HELD as i64 + 3) {
            g.hold(ptr(t, false, false));
        }
        assert_eq!(g.len(), MAX_HELD);
        assert_eq!(g.claim(InputClaim::Hover, 3).replay, vec![3]);
    }
}
