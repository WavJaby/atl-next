// SPDX-License-Identifier: MIT
//! GTK pointer and scroll events as `InputEvent`s: the toolkit speaks logical pixels, button numbers and scroll units, the platform traits physical pixels, named buttons and scroll kinds.

use atl_platform_ui_api::{
    InputEvent, PhysPoint, PointerButton, PointerEvent, PointerPhase, Scale, ScrollEvent,
    ScrollKind,
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

    #[test]
    fn button_numbers_follow_gdk() {
        assert_eq!(button(1), PointerButton::Primary);
        assert_eq!(button(2), PointerButton::Middle);
        assert_eq!(button(3), PointerButton::Secondary);
        assert_eq!(button(8), PointerButton::Other(8));
    }
}
