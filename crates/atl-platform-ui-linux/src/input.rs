// SPDX-License-Identifier: MIT
//! GTK pointer events as `InputEvent`s: the toolkit speaks logical pixels and button numbers, the platform traits physical pixels and named buttons.

use atl_platform_ui_api::{
    InputEvent, PhysPoint, PointerButton, PointerEvent, PointerPhase, Scale,
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

    #[test]
    fn button_numbers_follow_gdk() {
        assert_eq!(button(1), PointerButton::Primary);
        assert_eq!(button(2), PointerButton::Middle);
        assert_eq!(button(3), PointerButton::Secondary);
        assert_eq!(button(8), PointerButton::Other(8));
    }
}
