// SPDX-License-Identifier: MIT
//! GTK pointer and scroll events as `InputEvent`s: the toolkit speaks logical pixels, button numbers and scroll units, the platform traits physical pixels, named buttons and scroll kinds.

use atl_platform_ui_api::{
    CursorShape, InputEvent, KeyEvent, Locks, PhysPoint, PointerButton, PointerEvent, PointerPhase,
    Scale, ScrollEvent, ScrollKind, TouchEvent, TouchPhase,
};

/// GDK button numbers (`GDK_BUTTON_PRIMARY`, `_MIDDLE`, `_SECONDARY` = 1, 2, 3).
pub fn button(number: u32) -> PointerButton {
    match number {
        1 => PointerButton::Primary,
        2 => PointerButton::Middle,
        3 => PointerButton::Secondary,
        n => PointerButton::Other(n),
    }
}

/// The event for a pointer at (`x`, `y`) logical pixels of the content widget.
pub fn pointer(phase: PointerPhase, x: f64, y: f64, scale: Scale, time_ms: u32) -> InputEvent {
    InputEvent::Pointer(PointerEvent {
        phase,
        position: PhysPoint {
            x: scale.to_physical(x),
            y: scale.to_physical(y),
        },
        time_ms,
    })
}

/// One pointer position sample: (time ms, x, y), logical pixels of the content widget.
pub type Sample = (u32, f64, f64);

/// The moves of one compressed GTK motion event, oldest first: GDK coalesces motion per frame clock and keeps the dropped samples in `gdk_event_get_history`, which excludes the event's own position.
/// Each sample becomes its own MOVE, as an evdev device's are: AOSP's InputConsumer (consumeBatch) then folds them into one ACTION_MOVE with history samples per frame, so the app's VelocityTracker sees every sample.
/// Times strictly increase, after `last` (the time of the pointer event published before): an evdev stream never repeats a time, while GDK on WSLg stamps every history sample of a frame with the frame's ms time (owner drag: ~15 one-pixel samples per 15 ms, all one time).
/// AOSP keeps such duplicates as zero-dt history (InputConsumer canAddSample/resampleTouchState only skip them), so app-side per-sample velocity divides by 0; samples sharing a time collapse to the last position, and one timed at or before `last` (or after `current`) is dropped.
pub fn motion_samples(
    history: impl IntoIterator<Item = Sample>,
    current: Sample,
    last: Option<u32>,
) -> Vec<Sample> {
    let mut samples: Vec<Sample> = Vec::new();
    let fresh = |s: &Sample| s.0 <= current.0 && last.is_none_or(|t| s.0 > t);
    for s in history.into_iter().chain([current]).filter(fresh) {
        match samples.last_mut() {
            Some(prev) if prev.0 == s.0 => *prev = s,
            Some(prev) if prev.0 > s.0 => {}
            _ => samples.push(s),
        }
    }
    samples
}

/// Longest plausible delivery delay of an input event (ms): an event seeming later than this means the toolkit clock jumped (compositor restart, u32 wrap after 49.7 days) -> the offset is learned anew.
/// 1 s: far above a frame-clock-coalesced delay (one frame, ~16 ms) or a busy main loop, far below the minutes-to-hours gap of a foreign clock base.
const MAX_DELIVERY_MS: i64 = 1000;

/// Toolkit event time -> CLOCK_MONOTONIC ms, the base AOSP stamps MotionEvent/KeyEvent with (evdev timestamps, SystemClock.uptimeMillis); InputConsumer resampling and app-side deltas compare against it.
/// Wayland event times are the compositor's own ms clock with an unspecified base: WSLg's Weston ran 3,603,968 s (~41.7 days) ahead of the guest's monotonic clock (owner run tr-play-m4), a nested compositor on the same kernel matches it.
/// The offset (monotonic - toolkit) is learned per seat from receipt times: every event arrives after it happened, so the tightest bound is the smallest `now - t` seen; it only shrinks, which keeps the compositor's spacing between samples.
/// Output never lies after `now` and never goes backwards; a jump of the toolkit clock (bound exceeded by [`MAX_DELIVERY_MS`]) relearns.
#[derive(Debug, Default)]
pub struct ClockMap {
    /// monotonic - toolkit ms; None before the first event.
    offset: Option<i64>,
    /// Last time handed out (monotonic ms).
    last: i64,
}

impl ClockMap {
    /// Learns from an event of toolkit time `t` received at monotonic `now` (ms); returns the offset when it was (re)learned, for a log line.
    pub fn learn(&mut self, t: u32, now: i64) -> Option<i64> {
        let bound = now - i64::from(t);
        match self.offset {
            Some(o) if bound <= o => {
                self.offset = Some(bound);
                None
            }
            Some(o) if bound - o <= MAX_DELIVERY_MS => None,
            _ => {
                self.offset = Some(bound);
                Some(bound)
            }
        }
    }

    /// `t` on CLOCK_MONOTONIC, clamped to (last handed out ..= `now`); `now` itself before anything was learned.
    pub fn map(&mut self, t: u32, now: i64) -> u32 {
        let at = self
            .offset
            .map_or(now, |o| i64::from(t) + o)
            .min(now)
            .max(self.last);
        self.last = at;
        u32::try_from(at).unwrap_or(u32::MAX)
    }
}

/// How a scroll's deltas count: GTK's `GdkScrollUnit`, the toolkit type kept out of this pure mapper.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScrollUnit {
    /// Wheel notches (`GDK_SCROLL_UNIT_WHEEL`), fractional for high-resolution wheels.
    Wheel,
    /// Logical pixels of a continuous source, touchpad fingers (`GDK_SCROLL_UNIT_SURFACE`).
    Surface,
}

/// The event for a scroll of (`dx`, `dy`) in `unit` with the pointer at `at` (logical pixels): notches stay notches, surface pixels become physical pixels of a touchpad swipe.
pub fn scroll(
    unit: ScrollUnit,
    dx: f64,
    dy: f64,
    at: (f64, f64),
    scale: Scale,
    time_ms: u32,
) -> InputEvent {
    let kind = match unit {
        ScrollUnit::Wheel => ScrollKind::Wheel { dx, dy },
        ScrollUnit::Surface => ScrollKind::Swipe {
            dx: scale.to_physical(dx),
            dy: scale.to_physical(dy),
        },
    };
    InputEvent::Scroll(ScrollEvent {
        kind,
        position: physical(at, scale),
        time_ms,
    })
}

/// The end of a swipe (the fingers lifted) with the pointer at `at`.
pub fn swipe_end(at: (f64, f64), scale: Scale, time_ms: u32) -> InputEvent {
    InputEvent::Scroll(ScrollEvent {
        kind: ScrollKind::SwipeEnd,
        position: physical(at, scale),
        time_ms,
    })
}

/// Contact ids for touch sequences: a sequence (GDK's `GdkEventSequence`, keyed by its address -- stable from begin to end) takes the lowest id no live sequence holds, the id contract of [`TouchEvent`].
#[derive(Debug, Default)]
pub struct TouchIds {
    /// Index = id; the sequence holding it.
    held: Vec<Option<usize>>,
}

impl TouchIds {
    /// The id of `sequence`, taken on `Down` and released on `Up` / `Cancel` (still returned for that last event); `None` for a sequence that never went down here.
    pub fn id(&mut self, sequence: usize, phase: TouchPhase) -> Option<u32> {
        let found = self.held.iter().position(|s| *s == Some(sequence));
        let id = match (found, phase) {
            (Some(id), _) => id,
            (None, TouchPhase::Down) => match self.held.iter().position(Option::is_none) {
                Some(free) => {
                    self.held[free] = Some(sequence);
                    free
                }
                None => {
                    self.held.push(Some(sequence));
                    self.held.len() - 1
                }
            },
            (None, _) => return None,
        };
        if matches!(phase, TouchPhase::Up | TouchPhase::Cancel) {
            self.held[id] = None;
        }
        u32::try_from(id).ok()
    }
}

/// The event for touch contact `id` at (`x`, `y`) logical pixels of the content widget.
pub fn touch(phase: TouchPhase, id: u32, x: f64, y: f64, scale: Scale, time_ms: u32) -> InputEvent {
    InputEvent::Touch(TouchEvent {
        phase,
        id,
        position: physical((x, y), scale),
        time_ms,
    })
}

/// The xkb keycode offset: a Wayland/X11 hardware keycode is the linux input code + 8 (xkbcommon's evdev rules).
const XKB_EVDEV_OFFSET: u32 = 8;

/// The key event of GDK hardware keycode `keycode` (xkb numbering); None for a code below the evdev offset (no linux key).
pub fn key(down: bool, keycode: u32, locks: Locks, time_ms: u32) -> Option<InputEvent> {
    let scan_code = keycode.checked_sub(XKB_EVDEV_OFFSET)?;
    Some(InputEvent::Key(KeyEvent {
        down,
        scan_code,
        locks,
        time_ms,
    }))
}

/// The CSS cursor name GDK takes (`gdk_cursor_new_from_name`) for `shape`.
pub fn cursor_name(shape: CursorShape) -> &'static str {
    match shape {
        CursorShape::Default => "default",
        CursorShape::None => "none",
        CursorShape::ContextMenu => "context-menu",
        CursorShape::Pointer => "pointer",
        CursorShape::Help => "help",
        CursorShape::Wait => "wait",
        CursorShape::Cell => "cell",
        CursorShape::Crosshair => "crosshair",
        CursorShape::Text => "text",
        CursorShape::VerticalText => "vertical-text",
        CursorShape::Alias => "alias",
        CursorShape::Copy => "copy",
        CursorShape::NoDrop => "no-drop",
        CursorShape::AllScroll => "all-scroll",
        CursorShape::EwResize => "ew-resize",
        CursorShape::NsResize => "ns-resize",
        CursorShape::NeswResize => "nesw-resize",
        CursorShape::NwseResize => "nwse-resize",
        CursorShape::ZoomIn => "zoom-in",
        CursorShape::ZoomOut => "zoom-out",
        CursorShape::Grab => "grab",
        CursorShape::Grabbing => "grabbing",
    }
}

fn physical((x, y): (f64, f64), scale: Scale) -> PhysPoint {
    PhysPoint {
        x: scale.to_physical(x),
        y: scale.to_physical(y),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// xkb keycode 38 is KEY_A 30 (xkbcommon evdev rules: `<AC01> = 38`, input-event-codes.h KEY_A 30); codes below 8 have no linux key.
    #[test]
    fn key_is_the_linux_code() {
        let locks = Locks {
            caps: true,
            ..Locks::default()
        };
        assert_eq!(
            key(true, 38, locks, 5),
            Some(InputEvent::Key(KeyEvent {
                down: true,
                scan_code: 30,
                locks,
                time_ms: 5
            }))
        );
        assert_eq!(key(false, 7, locks, 5), None);
    }

    /// At 1.5x a pointer 10 logical pixels in is 15 physical pixels in (arithmetic); the phase and time pass through.
    #[test]
    fn positions_become_physical() {
        let scale = Scale::new(1.5).expect("valid");
        let e = pointer(PointerPhase::Move, 10.0, 20.0, scale, 42);
        assert_eq!(
            e,
            InputEvent::Pointer(PointerEvent {
                phase: PointerPhase::Move,
                position: PhysPoint { x: 15.0, y: 30.0 },
                time_ms: 42,
            })
        );
    }

    /// At 2x: wheel notches pass unscaled, a 3 x -4 logical-pixel swipe is 6 x -8 physical pixels, the pointer at (5, 6) is at (10, 12) (arithmetic).
    #[test]
    fn scroll_units_become_kinds() {
        let scale = Scale::new(2.0).expect("valid");
        let at = PhysPoint { x: 10.0, y: 12.0 };
        let event = |kind| {
            InputEvent::Scroll(ScrollEvent {
                kind,
                position: at,
                time_ms: 9,
            })
        };
        assert_eq!(
            scroll(ScrollUnit::Wheel, 0.0, 1.5, (5.0, 6.0), scale, 9),
            event(ScrollKind::Wheel { dx: 0.0, dy: 1.5 })
        );
        assert_eq!(
            scroll(ScrollUnit::Surface, 3.0, -4.0, (5.0, 6.0), scale, 9),
            event(ScrollKind::Swipe { dx: 6.0, dy: -8.0 })
        );
        assert_eq!(swipe_end((5.0, 6.0), scale, 9), event(ScrollKind::SwipeEnd));
    }

    /// Sequences A, B down -> 0, 1; A up frees 0; C down takes 0 (lowest free), B keeps 1; moves of an unknown sequence have no id; a cancelled id is free again.
    #[test]
    fn touch_ids_take_the_lowest_free() {
        let (a, b, c) = (0x10, 0x20, 0x30);
        let mut ids = TouchIds::default();
        assert_eq!(ids.id(a, TouchPhase::Down), Some(0));
        assert_eq!(ids.id(b, TouchPhase::Down), Some(1));
        assert_eq!(ids.id(a, TouchPhase::Move), Some(0));
        assert_eq!(ids.id(a, TouchPhase::Up), Some(0));
        assert_eq!(ids.id(a, TouchPhase::Move), None);
        assert_eq!(ids.id(c, TouchPhase::Down), Some(0));
        assert_eq!(ids.id(b, TouchPhase::Cancel), Some(1));
        assert_eq!(ids.id(a, TouchPhase::Down), Some(1));
        // at 2x a contact 4 logical px in is 8 physical px in (arithmetic)
        let scale = Scale::new(2.0).expect("valid");
        assert_eq!(
            touch(TouchPhase::Move, 1, 4.0, 5.0, scale, 3),
            InputEvent::Touch(TouchEvent {
                phase: TouchPhase::Move,
                id: 1,
                position: PhysPoint { x: 8.0, y: 10.0 },
                time_ms: 3,
            })
        );
    }

    /// A frame that coalesced 3 motions: the 2 history samples come first with their own times, then the event's position; no history -> just the event; a stray later sample is dropped.
    #[test]
    fn compressed_motion_becomes_every_sample() {
        let current = (40, 9.0, 9.0);
        assert_eq!(
            motion_samples([(8, 1.0, 2.0), (24, 5.0, 6.0)], current, None),
            vec![(8, 1.0, 2.0), (24, 5.0, 6.0), current]
        );
        assert_eq!(motion_samples([], current, None), vec![current]);
        assert_eq!(
            motion_samples([(41, 0.0, 0.0)], current, None),
            vec![current]
        );
    }

    /// Owner's WSLg drag (tr-play-m1 log, DOWN at 7600 ms (x, y) (141, 458)): frames at 7613/7631/7643 ms each published 3-5 one-pixel moves sharing the frame's time.
    /// Each frame collapses to its last position -> one move per distinct time, increasing; a frame at or before the previous event's time (DOWN 7600) publishes nothing.
    #[test]
    fn shared_times_collapse_to_last_position() {
        let frames: [(&[Sample], Sample); 3] = [
            (
                &[(7613, 141.0, 457.0), (7613, 142.0, 457.0)],
                (7613, 142.0, 456.0),
            ),
            (
                &[(7631, 142.0, 455.0), (7631, 142.0, 454.0)],
                (7631, 142.0, 453.0),
            ),
            (
                &[
                    (7643, 142.0, 452.0),
                    (7643, 142.0, 451.0),
                    (7643, 142.0, 450.0),
                    (7643, 142.0, 449.0),
                ],
                (7643, 142.0, 448.0),
            ),
        ];
        let mut last = Some(7600);
        let mut published = Vec::new();
        for (history, current) in frames {
            let moves = motion_samples(history.iter().copied(), current, last);
            last = moves.last().map(|s| s.0).or(last);
            published.extend(moves);
        }
        assert_eq!(
            published,
            vec![
                (7613, 142.0, 456.0),
                (7631, 142.0, 453.0),
                (7643, 142.0, 448.0)
            ]
        );
        assert_eq!(motion_samples([], (7600, 1.0, 1.0), Some(7600)), vec![]);
        assert_eq!(
            motion_samples([(30, 1.0, 1.0)], (40, 2.0, 2.0), Some(30)),
            vec![(40, 2.0, 2.0)]
        );
    }

    /// Owner run tr-play-m4: Weston stamped a touch at 3643489863 ms while the guest's CLOCK_MONOTONIC read ~39522 s (app log 39442.483 s at wall 1794644772.739, the event logged 79.557 s later) -> toolkit time 3,603,968 s (~41.7 days) ahead of monotonic.
    /// A drag's samples 15 ms apart keep their spacing on the monotonic clock and end at or before receipt; a later event received sooner tightens the offset; a toolkit-clock jump relearns; nothing goes backwards.
    #[test]
    fn toolkit_time_maps_onto_monotonic() {
        // receipt of the logged event, monotonic ms: 39442483 + (1794644852296 - 1794644772739)
        let now = 39_442_483 + (1_794_644_852_296 - 1_794_644_772_739);
        let t: u32 = 3_643_489_863;
        let mut clock = ClockMap::default();
        assert_eq!(clock.learn(t, now), Some(now - i64::from(t)));
        // history 30 and 15 ms before, then the event itself: spacing kept, the event lands on receipt
        let mapped: Vec<u32> = [t - 30, t - 15, t]
            .iter()
            .map(|&s| clock.map(s, now))
            .collect();
        let n = u32::try_from(now).expect("fits");
        assert_eq!(mapped, vec![n - 30, n - 15, n]);
        // the toolkit base the old code passed through: 3643489863 - 39522040 = 3603967823 ms ahead
        assert_eq!(i64::from(t) - now, 3_603_967_823);
        // next frame: 16 ms of toolkit time, received 20 ms later (4 ms delay) -> the 0-delay bound stays; event at n + 16
        assert_eq!(clock.learn(t + 16, now + 20), None);
        assert_eq!(clock.map(t + 16, now + 20), n + 16);
        // received 10 ms after a 16 ms step would be in the future of an over-estimated offset: clamped to now, and the offset tightens
        let mut late_first = ClockMap::default();
        late_first.learn(t, now + 8);
        assert_eq!(late_first.map(t, now + 8), n + 8);
        assert_eq!(late_first.learn(t + 16, now + 16), None);
        assert_eq!(late_first.map(t + 16, now + 16), n + 16);
        // a sample older than one already handed out does not go backwards
        assert_eq!(late_first.map(t, now + 16), n + 16);
        // toolkit clock jumps back 1 h (compositor restart): bound grows by 3.6e6 > MAX_DELIVERY_MS -> relearned
        let jumped = t - 3_600_000;
        assert_eq!(
            clock.learn(jumped, now + 40),
            Some(now + 40 - i64::from(jumped))
        );
        assert_eq!(clock.map(jumped, now + 40), n + 40);
        // nothing learned -> receipt time
        assert_eq!(ClockMap::default().map(5, 1234), 1234);
    }

    #[test]
    fn button_numbers_follow_gdk() {
        assert_eq!(button(1), PointerButton::Primary);
        assert_eq!(button(2), PointerButton::Middle);
        assert_eq!(button(3), PointerButton::Secondary);
        assert_eq!(button(8), PointerButton::Other(8));
    }
}
