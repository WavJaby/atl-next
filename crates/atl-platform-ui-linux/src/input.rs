// SPDX-License-Identifier: MIT
//! GTK pointer and scroll events as `InputEvent`s: the toolkit speaks logical pixels, button numbers and scroll units, the platform traits physical pixels, named buttons and scroll kinds.

use atl_platform_ui_api::{
    InputEvent, PhysPoint, PointerButton, PointerEvent, PointerPhase, Scale, ScrollEvent,
    ScrollKind, TouchEvent, TouchPhase,
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

fn physical((x, y): (f64, f64), scale: Scale) -> PhysPoint {
    PhysPoint {
        x: scale.to_physical(x),
        y: scale.to_physical(y),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn button_numbers_follow_gdk() {
        assert_eq!(button(1), PointerButton::Primary);
        assert_eq!(button(2), PointerButton::Middle);
        assert_eq!(button(3), PointerButton::Secondary);
        assert_eq!(button(8), PointerButton::Other(8));
    }
}
