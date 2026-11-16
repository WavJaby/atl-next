// SPDX-License-Identifier: MIT
//! Sideband widgets (webview plan S2.2 amended): toolkit widgets registered as the content of a sideband stream `[SIDEBAND_WEB_VIEW, key]` (WebKit's view), drawn by `SceneView`, and the input gate.
//! Input: Android's dispatch decides which View gets an event (OnTouchListener, onInterceptTouchEvent, per-pixel overlap), so a toolkit event aimed at the widget is held, not delivered:
//! the window's controllers (capture phase) publish it to Android as every event and record the monotonic time they stamped it with ([`stamp`]); the gate on the widget (capture phase) holds it;
//! when the provider's View receives the matching MotionEvent / KeyEvent it claims by that time ([`claim`]) and the held events are put back on the display's queue (`gdk_display_put_event`,
//! deprecated since GTK 4.10 with no replacement: the one call site), marked so the window's controllers skip them and the gate lets them through to the widget.
//! Everything here runs on the UI thread; the statics are statics, not thread-locals (module rule: no module object in the process's thread-local teardown).

use std::collections::VecDeque;
use std::sync::Mutex;

use atl_platform_ui_api::webview::{InputClaim, SIDEBAND_WEB_VIEW};
use gtk4::prelude::*;
use gtk4::{gdk, glib};

/// Held events per widget at most: a bound on memory when nothing claims (the View is covered for a whole hover); 512 is ~8 s of 60 Hz motion, far past any claim's delay (one Looper trip, one frame).
const MAX_HELD: usize = 512;

/// Recent (event, stamp) pairs and replay marks kept: the events of one dispatch at most a few, so 64 only bounds a mark whose replay never reached a gate.
const MAX_MARKS: usize = 64;

/// One held event: `event`, the monotonic ns Android got it at, whether it is a key, a press (a gesture's start) or a release (its end).
#[derive(Debug)]
pub struct Held<E> {
    pub event: E,
    pub ns: i64,
    pub key: bool,
    pub press: bool,
    pub release: bool,
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
    pub left_pressed: bool,
}

impl<E> Gate<E> {
    /// Holds `h`. A pointer event of a cancelled gesture is dropped (its release ends the cancel). A press while an unclaimed press or release is still held: that gesture went elsewhere,
    /// its pointer events are dropped.
    pub fn hold(&mut self, h: Held<E>) {
        if !h.key && self.cancelled {
            if h.release {
                self.cancelled = false;
            }
            return;
        }
        if h.press && self.held.iter().any(|o| !o.key && (o.press || o.release)) {
            self.held.retain(|o| o.key);
        }
        if self.held.len() == MAX_HELD {
            self.held.pop_front();
        }
        self.held.push_back(h);
    }

    /// Android delivered `claim` stamped `t` (ns) to the View.
    pub fn claim(&mut self, claim: InputClaim, t: i64) -> Claimed<E> {
        let mut left_pressed = false;
        let key = matches!(claim, InputClaim::Key);
        match claim {
            InputClaim::Down => {
                // what came before the press went elsewhere (or was hover nobody claimed)
                self.held.retain(|h| h.key || h.ns >= t);
                self.pressed = true;
                self.cancelled = false;
            }
            InputClaim::Up => self.pressed = false,
            InputClaim::Cancel => {
                let released = self.held.iter().any(|h| !h.key && h.release && h.ns <= t);
                self.held.retain(|h| h.key || h.ns > t);
                left_pressed = self.pressed;
                self.pressed = false;
                self.cancelled = !released;
                return Claimed {
                    replay: Vec::new(),
                    left_pressed,
                };
            }
            InputClaim::Move | InputClaim::Hover | InputClaim::Key => {}
        }
        let mut replay = Vec::new();
        let mut rest = VecDeque::with_capacity(self.held.len());
        for h in self.held.drain(..) {
            if h.key == key && h.ns <= t {
                replay.push(h.event);
            } else {
                rest.push_back(h);
            }
        }
        self.held = rest;
        Claimed {
            replay,
            left_pressed,
        }
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.held.len()
    }
}

/// A registered widget and its gate.
struct Entry {
    widget: gtk4::Widget,
    gate: std::cell::RefCell<Gate<gdk::Event>>,
}

/// The registered widgets, by key.
static SIDEBANDS: Mutex<Vec<(i32, glib::thread_guard::ThreadGuard<std::rc::Rc<Entry>>)>> =
    Mutex::new(Vec::new());

/// (event address, monotonic ns) of the events the window's controllers published last ([`stamp`]).
static STAMPS: Mutex<VecDeque<(usize, i64)>> = Mutex::new(VecDeque::new());

/// Addresses of events put back on the queue and not yet through their gate ([`replayed`]).
static REPLAYING: Mutex<VecDeque<usize>> = Mutex::new(VecDeque::new());

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

/// Registers `widget` as the content of `[SIDEBAND_WEB_VIEW, key]` and puts the input gate on it.
pub fn attach(key: i32, widget: gtk4::Widget) {
    let gate = gtk4::EventControllerLegacy::new();
    gate.set_propagation_phase(gtk4::PropagationPhase::Capture);
    gate.connect_event(move |_, event| on_gate(key, event));
    widget.add_controller(gate);
    let e = std::rc::Rc::new(Entry {
        widget,
        gate: std::cell::RefCell::default(),
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
    if let Some(e) = gone
        && let Some(parent) = e.widget.parent()
    {
        e.widget.unparent();
        parent.queue_draw();
    }
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
    let mut s = lock(&STAMPS);
    if s.len() == MAX_MARKS {
        s.pop_front();
    }
    s.push_back((address(&e), ns));
}

/// Whether `event` is a held event put back on the queue: the window's controllers skip it (Android has it already).
pub fn replayed(event: Option<&gdk::Event>) -> bool {
    event.is_some_and(|e| lock(&REPLAYING).contains(&address(e)))
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
    {
        let mut r = lock(&REPLAYING);
        if let Some(i) = r.iter().position(|x| *x == a) {
            r.remove(i);
            return glib::Propagation::Proceed;
        }
    }
    let Some((key_event, press, release)) = kind(event.event_type()) else {
        return glib::Propagation::Proceed;
    };
    let Some(e) = entry(key) else {
        return glib::Propagation::Proceed;
    };
    let ns = lock(&STAMPS)
        .iter()
        .rev()
        .find(|(x, _)| *x == a)
        .map_or_else(crate::window::monotonic_ns, |(_, ns)| *ns);
    e.gate.borrow_mut().hold(Held {
        event: event.clone(),
        ns,
        key: key_event,
        press,
        release,
    });
    glib::Propagation::Stop
}

/// The provider's View received `claim` stamped `t` (ns): `key`'s held events up to `t` go back on the queue. True when the page was left mid-press (a cancel it cannot be told of).
pub fn claim(key: i32, claim: InputClaim, t: i64) -> bool {
    let Some(e) = entry(key) else { return false };
    let claimed = e.gate.borrow_mut().claim(claim, t);
    for ev in claimed.replay {
        {
            let mut r = lock(&REPLAYING);
            if r.len() == MAX_MARKS {
                r.pop_front();
            }
            r.push_back(address(&ev));
        }
        if let Some(display) = ev.display() {
            #[allow(deprecated)]
            // GTK4 has no other way to deliver an event to a widget (module doc)
            display.put_event(&ev);
        }
    }
    crate::mirror::resync();
    claimed.left_pressed
}

/// Android focus on `key`'s View: its widget takes the toolkit's keyboard focus (WebKit's own input method context follows it), or gives it back to the window.
pub fn focus(key: i32, on: bool) {
    let Some(e) = entry(key) else { return };
    if on {
        e.widget.grab_focus();
    } else if let Some(root) = e.widget.root()
        && root
            .focus()
            .is_some_and(|f| f == e.widget || f.is_ancestor(&e.widget))
    {
        root.set_focus(None::<&gtk4::Widget>);
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
        }
    }

    fn key(ns: i64) -> Held<i64> {
        Held {
            event: ns,
            ns,
            key: true,
            press: false,
            release: false,
        }
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
        g.hold(ptr(30, true, false));
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
