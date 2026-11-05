// SPDX-License-Identifier: MIT
//! Mouse and touchpad input as Android MotionEvents: the part of AOSP's input pipeline between the device and the window (CursorInputMapper, GestureConverter, PointerChoreographer, InputDispatcher's hover synthesis) for one pointer.
//! Pure: [`Pointer::translate`] turns one platform event into the [`Motion`]s Android would deliver, in order; the publisher stamps seq and ids and writes them.
//! * Buttons: DOWN then BUTTON_PRESS per button, BUTTON_RELEASE then UP (CursorInputMapper.cpp sync); a button with no Android bit is ignored.
//! * Hover: no button held -> HOVER_MOVE, with HOVER_ENTER first and HOVER_EXIT on leave or before a press (InputDispatcher synthesizes enter/exit per window; we are the dispatcher).
//! * Wheel -> ACTION_SCROLL, AXIS_VSCROLL = -dy (positive = toward the user's "up", MotionEvent.AXIS_VSCROLL javadoc), AXIS_HSCROLL = dx.
//! * Touchpad swipe -> one fake finger (GestureConverter.cpp handleScroll): DOWN at the cursor, MOVE per delta, UP at the end; TWO_FINGER_SWIPE, IS_GENERATED_GESTURE, pressure 1, source SOURCE_MOUSE (GestureConverter.h SOURCE; mouse|touchpad is only the device's sources, and SOURCE_CLASS_POSITION on an event makes MotionEvent skip the view offset: shouldDisregardTransformation),
//!   finger and cursor offset to the cursor position (PointerChoreographer.cpp processTouchpadEventLocked). The finger moves with the content (= -scroll delta) and GESTURE_SCROLL_*_DISTANCE = the scroll delta.
//!   Android flings from the finger's own velocity (handleFling ignores the library's), so no fling is synthesized here.

use atl_platform_ui::{InputEvent, PhysPoint, PointerButton, PointerPhase, ScrollKind};
use atl_services::input_devices::{POINTER, SOURCE_MOUSE, TOUCHPAD};

/// MotionEvent actions (NDK input.h AMOTION_EVENT_ACTION_*).
pub const ACTION_DOWN: i32 = 0;
pub const ACTION_UP: i32 = 1;
pub const ACTION_MOVE: i32 = 2;
pub const ACTION_HOVER_MOVE: i32 = 7;
pub const ACTION_SCROLL: i32 = 8;
pub const ACTION_HOVER_ENTER: i32 = 9;
pub const ACTION_HOVER_EXIT: i32 = 10;
pub const ACTION_BUTTON_PRESS: i32 = 11;
pub const ACTION_BUTTON_RELEASE: i32 = 12;
/// Axes (input.h AMOTION_EVENT_AXIS_*).
pub const AXIS_PRESSURE: i32 = 2;
pub const AXIS_VSCROLL: i32 = 9;
pub const AXIS_HSCROLL: i32 = 10;
pub const AXIS_GESTURE_SCROLL_X_DISTANCE: i32 = 50;
pub const AXIS_GESTURE_SCROLL_Y_DISTANCE: i32 = 51;
/// TOOL_TYPE_FINGER 1, TOOL_TYPE_MOUSE 3 (input.h).
pub const TOOL_TYPE_FINGER: i32 = 1;
pub const TOOL_TYPE_MOUSE: i32 = 3;
/// MotionClassification TWO_FINGER_SWIPE (input.h AMOTION_EVENT_CLASSIFICATION_TWO_FINGER_SWIPE).
pub const CLASSIFICATION_TWO_FINGER_SWIPE: u8 = 3;
/// MotionEventFlag.IS_GENERATED_GESTURE (libs/input/android/os/MotionEventFlag.aidl).
pub const FLAG_IS_GENERATED_GESTURE: i32 = 0x8;

/// MotionEvent.BUTTON_PRIMARY, _SECONDARY, _TERTIARY, _BACK, _FORWARD (NDK input.h: one bit each).
const BUTTON_PRIMARY: i32 = 1 << 0;
const BUTTON_SECONDARY: i32 = 1 << 1;
const BUTTON_TERTIARY: i32 = 1 << 2;
const BUTTON_BACK: i32 = 1 << 3;
const BUTTON_FORWARD: i32 = 1 << 4;

/// The bit of `button` in a MotionEvent's button state. GDK numbers 8 and 9 are back and forward (X11 convention); other buttons have no bit.
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

/// One pointer of a MotionEvent.
#[derive(Clone, Debug, PartialEq)]
pub struct MotionPointer {
    pub id: i32,
    pub tool_type: i32,
    /// The pointer's position.
    pub at: PhysPoint,
    pub axes: Vec<(i32, f32)>,
}

/// One MotionEvent, before the publisher numbers it. Positions are physical pixels of the window content; times are the platform's ms.
#[derive(Clone, Debug, PartialEq)]
pub struct Motion {
    /// The device (atl-services input_devices).
    pub device_id: i32,
    pub action: i32,
    pub action_button: i32,
    pub button_state: i32,
    pub flags: i32,
    pub classification: u8,
    pub source: i32,
    /// In pointer-id order (TouchInputMapper dispatches its id bits ascending); a mouse or a swipe has one, id 0.
    pub pointers: Vec<MotionPointer>,
    /// The cursor position, where a mouse-source event hit-tests (MotionEvent.getXDispatchLocation); NaN (AMOTION_EVENT_INVALID_CURSOR_POSITION) for a touchscreen, which hit-tests at its pointer.
    pub cursor: PhysPoint,
    pub down_ms: u32,
    pub time_ms: u32,
}

impl Motion {
    /// The first pointer (a mouse's only one).
    pub fn first(&self) -> &MotionPointer {
        &self.pointers[0]
    }
}

/// The fake finger of an open touchpad swipe.
#[derive(Clone, Copy, Debug)]
struct Swipe {
    down_ms: u32,
    finger: PhysPoint,
}

/// One pointer's state: held buttons, hover, the open swipe, the last position.
#[derive(Debug, Default)]
pub struct Pointer {
    buttons: i32,
    hovering: bool,
    down_ms: u32,
    at: Option<PhysPoint>,
    swipe: Option<Swipe>,
}

impl Pointer {
    /// A gesture is open (a button held or a swipe going): its events stay with the window it started on.
    pub fn in_gesture(&self) -> bool {
        self.buttons != 0 || self.swipe.is_some()
    }

    fn mouse(
        &self,
        action: i32,
        action_button: i32,
        at: PhysPoint,
        axes: Vec<(i32, f32)>,
        time_ms: u32,
    ) -> Motion {
        let in_gesture = self.buttons != 0 || action == ACTION_UP;
        Motion {
            device_id: POINTER,
            action,
            action_button,
            button_state: self.buttons,
            flags: 0,
            classification: 0,
            source: SOURCE_MOUSE,
            pointers: vec![MotionPointer {
                id: 0,
                tool_type: TOOL_TYPE_MOUSE,
                at,
                axes,
            }],
            cursor: at,
            down_ms: if in_gesture { self.down_ms } else { time_ms },
            time_ms,
        }
    }

    fn finger(
        &self,
        action: i32,
        swipe: Swipe,
        cursor: PhysPoint,
        axes: Vec<(i32, f32)>,
        time_ms: u32,
    ) -> Motion {
        Motion {
            // the touchpad's own gesture: GestureConverter reports its device's id
            device_id: TOUCHPAD,
            action,
            action_button: 0,
            button_state: 0,
            flags: FLAG_IS_GENERATED_GESTURE,
            classification: CLASSIFICATION_TWO_FINGER_SWIPE,
            source: SOURCE_MOUSE,
            pointers: vec![MotionPointer {
                id: 0,
                tool_type: TOOL_TYPE_FINGER,
                at: swipe.finger,
                axes,
            }],
            cursor,
            down_ms: swipe.down_ms,
            time_ms,
        }
    }

    /// Ends the hover (HOVER_EXIT) when one is going.
    fn exit_hover(&mut self, out: &mut Vec<Motion>, at: PhysPoint, time_ms: u32) {
        if std::mem::take(&mut self.hovering) {
            out.push(self.mouse(ACTION_HOVER_EXIT, 0, at, Vec::new(), time_ms));
        }
    }

    /// Lifts the swipe's fake finger (UP, pressure 0) when one is down.
    fn end_swipe(&mut self, out: &mut Vec<Motion>, time_ms: u32) {
        if let Some(swipe) = self.swipe.take() {
            let cursor = self.at.unwrap_or(swipe.finger);
            out.push(self.finger(
                ACTION_UP,
                swipe,
                cursor,
                vec![(AXIS_PRESSURE, 0.0)],
                time_ms,
            ));
        }
    }

    /// The MotionEvents of `event`, in delivery order (none for an event Android has no answer to).
    pub fn translate(&mut self, event: InputEvent) -> Vec<Motion> {
        let mut out = Vec::new();
        match event {
            InputEvent::Pointer(e) => {
                let t = e.time_ms;
                match e.phase {
                    PointerPhase::Enter | PointerPhase::Move if self.buttons != 0 => {
                        self.at = Some(e.position);
                        out.push(self.mouse(ACTION_MOVE, 0, e.position, Vec::new(), t));
                    }
                    PointerPhase::Enter | PointerPhase::Move => {
                        self.at = Some(e.position);
                        if self.swipe.is_none() {
                            if !std::mem::replace(&mut self.hovering, true) {
                                out.push(self.mouse(
                                    ACTION_HOVER_ENTER,
                                    0,
                                    e.position,
                                    Vec::new(),
                                    t,
                                ));
                            }
                            if e.phase == PointerPhase::Move {
                                out.push(self.mouse(
                                    ACTION_HOVER_MOVE,
                                    0,
                                    e.position,
                                    Vec::new(),
                                    t,
                                ));
                            }
                        }
                    }
                    PointerPhase::Leave => {
                        let at = self.at.unwrap_or(e.position);
                        self.exit_hover(&mut out, at, t);
                    }
                    PointerPhase::Down(button) => {
                        let bit = button_bit(button);
                        if bit == 0 || self.buttons & bit != 0 {
                            return out;
                        }
                        self.at = Some(e.position);
                        self.end_swipe(&mut out, t);
                        self.exit_hover(&mut out, e.position, t);
                        if self.buttons == 0 {
                            self.down_ms = t;
                            self.buttons = bit;
                            out.push(self.mouse(ACTION_DOWN, 0, e.position, Vec::new(), t));
                        } else {
                            self.buttons |= bit;
                        }
                        out.push(self.mouse(ACTION_BUTTON_PRESS, bit, e.position, Vec::new(), t));
                    }
                    PointerPhase::Up(button) => {
                        let bit = button_bit(button);
                        if self.buttons & bit == 0 {
                            return out;
                        }
                        self.at = Some(e.position);
                        self.buttons &= !bit;
                        out.push(self.mouse(ACTION_BUTTON_RELEASE, bit, e.position, Vec::new(), t));
                        if self.buttons == 0 {
                            out.push(self.mouse(ACTION_UP, 0, e.position, Vec::new(), t));
                        }
                    }
                }
            }
            InputEvent::Scroll(e) => {
                let t = e.time_ms;
                match e.kind {
                    ScrollKind::Wheel { dx, dy } => {
                        self.at = Some(e.position);
                        let axes = vec![(AXIS_VSCROLL, -dy as f32), (AXIS_HSCROLL, dx as f32)];
                        out.push(self.mouse(ACTION_SCROLL, 0, e.position, axes, t));
                    }
                    ScrollKind::Swipe { dx, dy } => {
                        if self.buttons != 0 {
                            return out;
                        }
                        let cursor = *self.at.get_or_insert(e.position);
                        let mut swipe = match self.swipe {
                            Some(swipe) => swipe,
                            None => {
                                self.exit_hover(&mut out, cursor, t);
                                let swipe = Swipe {
                                    down_ms: t,
                                    finger: cursor,
                                };
                                out.push(self.finger(
                                    ACTION_DOWN,
                                    swipe,
                                    cursor,
                                    vec![(AXIS_PRESSURE, 1.0)],
                                    t,
                                ));
                                swipe
                            }
                        };
                        swipe.finger.x -= dx;
                        swipe.finger.y -= dy;
                        self.swipe = Some(swipe);
                        let axes = vec![
                            (AXIS_PRESSURE, 1.0),
                            (AXIS_GESTURE_SCROLL_X_DISTANCE, dx as f32),
                            (AXIS_GESTURE_SCROLL_Y_DISTANCE, dy as f32),
                        ];
                        out.push(self.finger(ACTION_MOVE, swipe, cursor, axes, t));
                    }
                    ScrollKind::SwipeEnd => self.end_swipe(&mut out, t),
                }
            }
            // window focus is the dispatcher's, not the pointer's
            InputEvent::Focus(_) => {}
            // touch contacts are the touchscreen's (touch.rs), keys the keyboard's (key.rs)
            InputEvent::Touch(_) | InputEvent::Key(_) => {}
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use atl_platform_ui::{PointerEvent, ScrollEvent};

    use super::*;

    const P: PhysPoint = PhysPoint { x: 100.0, y: 200.0 };

    fn pointer(phase: PointerPhase, t: u32) -> InputEvent {
        InputEvent::Pointer(PointerEvent {
            phase,
            position: P,
            time_ms: t,
        })
    }

    fn scroll(kind: ScrollKind, t: u32) -> InputEvent {
        InputEvent::Scroll(ScrollEvent {
            kind,
            position: P,
            time_ms: t,
        })
    }

    fn actions(p: &mut Pointer, e: InputEvent) -> Vec<i32> {
        p.translate(e).iter().map(|m| m.action).collect()
    }

    /// Hover then a click: ENTER(9) MOVE(7), EXIT(10) before DOWN(0) + BUTTON_PRESS(11), BUTTON_RELEASE(12) + UP(1), then the next move enters again. Codes from input.h.
    #[test]
    fn hover_wraps_a_click() {
        let mut p = Pointer::default();
        assert_eq!(actions(&mut p, pointer(PointerPhase::Enter, 1)), [9]);
        assert_eq!(actions(&mut p, pointer(PointerPhase::Move, 2)), [7]);
        let down = p.translate(pointer(PointerPhase::Down(PointerButton::Primary), 3));
        assert_eq!(
            down.iter().map(|m| m.action).collect::<Vec<_>>(),
            [10, 0, 11]
        );
        // BUTTON_PRESS names its button; DOWN's action button is 0 (CursorInputMapper)
        assert_eq!(
            (
                down[1].action_button,
                down[2].action_button,
                down[2].button_state
            ),
            (0, 1, 1)
        );
        assert_eq!(actions(&mut p, pointer(PointerPhase::Move, 4)), [2]);
        let up = p.translate(pointer(PointerPhase::Up(PointerButton::Primary), 5));
        assert_eq!(up.iter().map(|m| m.action).collect::<Vec<_>>(), [12, 1]);
        assert_eq!(
            (up[0].action_button, up[1].button_state, up[1].down_ms),
            (1, 0, 3)
        );
        assert_eq!(actions(&mut p, pointer(PointerPhase::Move, 6)), [9, 7]);
        assert_eq!(actions(&mut p, pointer(PointerPhase::Leave, 7)), [10]);
        assert_eq!(
            actions(&mut p, pointer(PointerPhase::Leave, 8)),
            Vec::<i32>::new()
        );
        // a button with no Android bit makes no event
        assert!(
            p.translate(pointer(PointerPhase::Down(PointerButton::Other(6)), 9))
                .is_empty()
        );
    }

    /// A second button while one is held is only a BUTTON_PRESS; releasing the first keeps the gesture (no UP).
    #[test]
    fn second_button_stacks() {
        let mut p = Pointer::default();
        p.translate(pointer(PointerPhase::Down(PointerButton::Primary), 1));
        let second = p.translate(pointer(PointerPhase::Down(PointerButton::Secondary), 2));
        assert_eq!(
            (
                second.len(),
                second[0].action,
                second[0].action_button,
                second[0].button_state
            ),
            (1, 11, 2, 3)
        );
        assert_eq!(
            actions(&mut p, pointer(PointerPhase::Up(PointerButton::Primary), 3)),
            [12]
        );
        assert!(p.in_gesture());
        assert_eq!(
            actions(
                &mut p,
                pointer(PointerPhase::Up(PointerButton::Secondary), 4)
            ),
            [12, 1]
        );
        assert!(!p.in_gesture());
    }

    /// One wheel notch down (dy +1, the toolkit's "toward the end") is AXIS_VSCROLL -1: Android's positive VSCROLL scrolls up (MotionEvent.AXIS_VSCROLL javadoc).
    #[test]
    fn wheel_is_scroll_with_inverted_vscroll() {
        let mut p = Pointer::default();
        let m = p.translate(scroll(ScrollKind::Wheel { dx: 0.5, dy: 1.0 }, 1));
        assert_eq!(m.len(), 1);
        assert_eq!(
            (
                m[0].action,
                m[0].source,
                m[0].first().tool_type,
                m[0].device_id
            ),
            (8, 0x2002, 3, 1)
        );
        assert_eq!(m[0].first().axes, vec![(9, -1.0), (10, 0.5)]);
        assert_eq!(m[0].cursor, P);
    }

    /// A swipe: hover exits, the fake finger goes down at the cursor and moves against the scroll (dy +10 -> finger y 190), GESTURE_SCROLL_Y_DISTANCE = +10, then UP at the moved finger with pressure 0.
    /// Source 0x2002 (GestureConverter.h), TOOL_TYPE_FINGER 1, TWO_FINGER_SWIPE 3, IS_GENERATED_GESTURE 0x8 (input.h, MotionEventFlag.aidl).
    #[test]
    fn swipe_is_a_fake_finger() {
        let mut p = Pointer::default();
        p.translate(pointer(PointerPhase::Enter, 1));
        let first = p.translate(scroll(ScrollKind::Swipe { dx: 0.0, dy: 10.0 }, 2));
        assert_eq!(
            first.iter().map(|m| m.action).collect::<Vec<_>>(),
            [10, 0, 2]
        );
        let (down, moved) = (&first[1], &first[2]);
        assert_eq!(
            (
                down.source,
                down.first().tool_type,
                down.classification,
                down.flags,
                down.device_id
            ),
            (0x2002, 1, 3, 0x8, TOUCHPAD)
        );
        assert_eq!(down.first().at, P);
        assert_eq!(down.first().axes, vec![(2, 1.0)]);
        assert_eq!(moved.first().at, PhysPoint { x: 100.0, y: 190.0 });
        assert_eq!(moved.cursor, P);
        assert_eq!(moved.first().axes, vec![(2, 1.0), (50, 0.0), (51, 10.0)]);
        assert!(p.in_gesture());
        // a pointer move during the swipe is no hover
        assert!(p.translate(pointer(PointerPhase::Move, 3)).is_empty());
        let up = p.translate(scroll(ScrollKind::SwipeEnd, 4));
        assert_eq!((up.len(), up[0].action, up[0].down_ms), (1, 1, 2));
        assert_eq!(up[0].first().at, PhysPoint { x: 100.0, y: 190.0 });
        assert_eq!(up[0].first().axes, vec![(2, 0.0)]);
        assert!(!p.in_gesture());
        assert!(p.translate(scroll(ScrollKind::SwipeEnd, 5)).is_empty());
    }

    /// A press during a swipe lifts the fake finger first.
    #[test]
    fn press_ends_a_swipe() {
        let mut p = Pointer::default();
        p.translate(scroll(ScrollKind::Swipe { dx: 0.0, dy: 1.0 }, 1));
        assert_eq!(
            actions(
                &mut p,
                pointer(PointerPhase::Down(PointerButton::Primary), 2)
            ),
            [1, 0, 11]
        );
    }
}
