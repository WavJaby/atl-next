// SPDX-License-Identifier: MIT
//! Touchscreen contacts as Android MotionEvents: TouchInputMapper's dispatch (dispatchTouches) for the contacts down, one gesture from the first contact down to the last up.
//! Pure: [`Touch::translate`] turns one platform contact event into the [`Motion`]s Android would deliver.
//! * Pointer id = the platform's contact id (both the lowest free id while down, TouchEvent contract); pointers listed in id order (TouchInputMapper walks its id bits ascending).
//! * First contact -> DOWN; another -> POINTER_DOWN | index << ACTION_POINTER_INDEX_SHIFT (index of the new pointer in the list, which includes it); a move -> MOVE with every pointer;
//!   a lift with others down -> POINTER_UP | index (the list still has it), the last -> UP; a cancel voids the whole gesture -> CANCEL with FLAG_CANCELED (InputDispatcher synthesizes it so: every pointer, once).
//! * Source SOURCE_TOUCHSCREEN, TOOL_TYPE_FINGER, pressure 1, cursor NaN (a touchscreen hit-tests at its pointer: MotionEvent.getXDispatchLocation reads the cursor only for SOURCE_MOUSE).

use atl_platform_ui::{
    InputEvent, PhysPoint, PointerButton, PointerEvent, PointerPhase, TouchEvent, TouchPhase,
};
use atl_services_types::input_devices::{SOURCE_TOUCHSCREEN, TOUCHSCREEN};

use super::pointer::{
    ACTION_DOWN, ACTION_MOVE, ACTION_UP, AXIS_PRESSURE, Motion, MotionPointer, TOOL_TYPE_FINGER,
};

/// MotionEvent actions (NDK input.h AMOTION_EVENT_ACTION_*) and the pointer index shift (MotionEvent.ACTION_POINTER_INDEX_SHIFT).
pub const ACTION_CANCEL: i32 = 3;
pub const ACTION_POINTER_DOWN: i32 = 5;
pub const ACTION_POINTER_UP: i32 = 6;
const ACTION_POINTER_INDEX_SHIFT: i32 = 8;
/// MotionEvent.FLAG_CANCELED (IInputConstants INPUT_EVENT_FLAG_CANCELED).
pub const FLAG_CANCELED: i32 = 0x20;
/// AMOTION_EVENT_INVALID_CURSOR_POSITION.
const NO_CURSOR: PhysPoint = PhysPoint {
    x: f64::NAN,
    y: f64::NAN,
};

/// The contacts of the open gesture, by id ascending, and its down time.
#[derive(Debug, Default)]
pub struct Touch {
    contacts: Vec<(u32, PhysPoint)>,
    down_ms: u32,
}

impl Touch {
    /// A gesture is open (a contact is down): its events stay with the window it started on.
    pub fn in_gesture(&self) -> bool {
        !self.contacts.is_empty()
    }

    fn motion(&self, action: i32, flags: i32, time_ms: u32) -> Motion {
        Motion {
            device_id: TOUCHSCREEN,
            action,
            action_button: 0,
            button_state: 0,
            flags,
            classification: 0,
            source: SOURCE_TOUCHSCREEN,
            pointers: self
                .contacts
                .iter()
                .map(|&(id, at)| MotionPointer {
                    id: i32::try_from(id).unwrap_or(i32::MAX),
                    tool_type: TOOL_TYPE_FINGER,
                    at,
                    axes: vec![(AXIS_PRESSURE, 1.0)],
                })
                .collect(),
            cursor: NO_CURSOR,
            down_ms: self.down_ms,
            time_ms,
        }
    }

    /// The MotionEvents of `e` (none for a contact this gesture does not hold, or a second Down of a held one).
    pub fn translate(&mut self, e: TouchEvent) -> Vec<Motion> {
        let t = e.time_ms;
        let index = self.contacts.iter().position(|c| c.0 == e.id);
        let shifted = |i: usize| (i as i32) << ACTION_POINTER_INDEX_SHIFT;
        match (e.phase, index) {
            (TouchPhase::Down, None) => {
                let i = self.contacts.partition_point(|c| c.0 < e.id);
                self.contacts.insert(i, (e.id, e.position));
                if self.contacts.len() == 1 {
                    self.down_ms = t;
                    vec![self.motion(ACTION_DOWN, 0, t)]
                } else {
                    vec![self.motion(ACTION_POINTER_DOWN | shifted(i), 0, t)]
                }
            }
            (TouchPhase::Move, Some(i)) => {
                self.contacts[i].1 = e.position;
                vec![self.motion(ACTION_MOVE, 0, t)]
            }
            (TouchPhase::Up, Some(i)) => {
                self.contacts[i].1 = e.position;
                let m = if self.contacts.len() == 1 {
                    self.motion(ACTION_UP, 0, t)
                } else {
                    self.motion(ACTION_POINTER_UP | shifted(i), 0, t)
                };
                self.contacts.remove(i);
                vec![m]
            }
            (TouchPhase::Cancel, Some(_)) => {
                let m = self.motion(ACTION_CANCEL, FLAG_CANCELED, t);
                self.contacts.clear();
                vec![m]
            }
            _ => Vec::new(),
        }
    }
}

/// The contact id the mouse finger takes: the platform's first contact id, free since the finger starts only with no contact down.
const MOUSE_FINGER_ID: u32 = 0;

/// Desktop adaptation (atld `--mouse-touch`, DesktopInput::mouse_touch): the left button is one finger. AOSP has none (CursorInputMapper.cpp sync sends buttons as SOURCE_MOUSE), so off is exact AOSP input.
/// A primary press with no contact down becomes a leave (the mouse's hover ends, as for a finger on a phone) then a contact down; moves while it is held move the contact; its release lifts it. Hover, wheel and other buttons stay the mouse's.
#[derive(Debug, Default)]
pub struct MouseFinger {
    held: bool,
}

impl MouseFinger {
    /// The events `e` stands for: itself, or the finger's. `touch_down`: a real contact is down, so a press stays the mouse's (one gesture per touchscreen).
    pub fn adapt(&mut self, e: InputEvent, touch_down: bool) -> Vec<InputEvent> {
        let InputEvent::Pointer(p) = e else {
            return vec![e];
        };
        let contact = |phase| {
            InputEvent::Touch(TouchEvent {
                phase,
                id: MOUSE_FINGER_ID,
                position: p.position,
                time_ms: p.time_ms,
            })
        };
        match p.phase {
            PointerPhase::Down(PointerButton::Primary) if !self.held && !touch_down => {
                self.held = true;
                let leave = PointerEvent {
                    phase: PointerPhase::Leave,
                    ..p
                };
                vec![InputEvent::Pointer(leave), contact(TouchPhase::Down)]
            }
            PointerPhase::Move | PointerPhase::Enter if self.held => {
                vec![contact(TouchPhase::Move)]
            }
            PointerPhase::Up(PointerButton::Primary) if self.held => {
                self.held = false;
                vec![contact(TouchPhase::Up)]
            }
            _ => vec![InputEvent::Pointer(p)],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mouse(phase: PointerPhase, t: u32) -> InputEvent {
        InputEvent::Pointer(PointerEvent {
            phase,
            position: PhysPoint { x: 7.0, y: 9.0 },
            time_ms: t,
        })
    }

    /// The left button through MouseFinger then Touch: a hover move stays the mouse's; press -> pointer Leave + DOWN(0) from the touchscreen (source 0x1002, device 3, TOOL_TYPE_FINGER 1, input.h / InputDevice);
    /// a drag -> MOVE(2); release -> UP(1); a right press stays the mouse's. Pressed while a real contact is down, the left button stays the mouse's.
    #[test]
    fn left_button_is_one_finger() {
        let mut f = MouseFinger::default();
        let mut t = Touch::default();
        assert!(matches!(
            f.adapt(mouse(PointerPhase::Move, 1), false)[..],
            [InputEvent::Pointer(_)]
        ));
        let press = f.adapt(mouse(PointerPhase::Down(PointerButton::Primary), 2), false);
        assert!(matches!(&press[0], InputEvent::Pointer(p) if p.phase == PointerPhase::Leave));
        let InputEvent::Touch(down) = press[1] else {
            panic!("no contact: {press:?}")
        };
        let d = t.translate(down);
        assert_eq!(
            (
                d[0].action,
                d[0].source,
                d[0].device_id,
                d[0].pointers[0].tool_type
            ),
            (0, 0x1002, 3, 1)
        );
        for (e, action) in [
            (mouse(PointerPhase::Move, 3), 2),
            (mouse(PointerPhase::Up(PointerButton::Primary), 4), 1),
        ] {
            let [InputEvent::Touch(c)] = f.adapt(e, t.in_gesture())[..] else {
                panic!("not a contact")
            };
            assert_eq!(t.translate(c)[0].action, action);
        }
        assert!(matches!(
            f.adapt(
                mouse(PointerPhase::Down(PointerButton::Secondary), 5),
                false
            )[..],
            [InputEvent::Pointer(_)]
        ));
        assert!(matches!(
            f.adapt(mouse(PointerPhase::Down(PointerButton::Primary), 6), true)[..],
            [InputEvent::Pointer(_)]
        ));
    }

    fn ev(phase: TouchPhase, id: u32, x: f64, t: u32) -> TouchEvent {
        TouchEvent {
            phase,
            id,
            position: PhysPoint { x, y: 50.0 },
            time_ms: t,
        }
    }

    fn ids(m: &Motion) -> Vec<i32> {
        m.pointers.iter().map(|p| p.id).collect()
    }

    /// Two fingers: DOWN(0) [0]; id 1 down -> POINTER_DOWN index 1 = 5 | 1 << 8 = 261 [0, 1]; MOVE(2) with both; id 0 lifts -> POINTER_UP index 0 = 6 [0, 1]; id 1 lifts -> UP(1) [1].
    /// Codes from input.h; source 0x1002 (InputDevice.SOURCE_TOUCHSCREEN), device 3 (input_devices), down time = the first contact's.
    #[test]
    fn two_fingers_follow_touch_input_mapper() {
        let mut t = Touch::default();
        let d = t.translate(ev(TouchPhase::Down, 0, 10.0, 1));
        assert_eq!(
            (d[0].action, ids(&d[0]), d[0].source, d[0].device_id),
            (0, vec![0], 0x1002, 3)
        );
        assert!(d[0].cursor.x.is_nan());
        let pd = t.translate(ev(TouchPhase::Down, 1, 20.0, 2));
        assert_eq!(
            (pd[0].action, ids(&pd[0]), pd[0].down_ms),
            (261, vec![0, 1], 1)
        );
        let mv = t.translate(ev(TouchPhase::Move, 1, 25.0, 3));
        assert_eq!((mv[0].action, mv[0].pointers[1].at.x), (2, 25.0));
        let pu = t.translate(ev(TouchPhase::Up, 0, 10.0, 4));
        assert_eq!((pu[0].action, ids(&pu[0])), (6, vec![0, 1]));
        assert!(t.in_gesture());
        let up = t.translate(ev(TouchPhase::Up, 1, 25.0, 5));
        assert_eq!((up[0].action, ids(&up[0])), (1, vec![1]));
        assert!(!t.in_gesture());
        // a move of a lifted contact makes no event
        assert!(t.translate(ev(TouchPhase::Move, 1, 30.0, 6)).is_empty());
    }

    /// A new contact with a lower id than one down goes first: POINTER_DOWN index 0 = 5 (input.h); a cancel is one CANCEL(3) of every pointer with FLAG_CANCELED 0x20, and ends the gesture.
    #[test]
    fn lower_id_sorts_first_and_cancel_voids_all() {
        let mut t = Touch::default();
        t.translate(ev(TouchPhase::Down, 2, 10.0, 1));
        let pd = t.translate(ev(TouchPhase::Down, 0, 20.0, 2));
        assert_eq!((pd[0].action, ids(&pd[0])), (5, vec![0, 2]));
        let c = t.translate(ev(TouchPhase::Cancel, 2, 10.0, 3));
        assert_eq!(
            (c.len(), c[0].action, c[0].flags, ids(&c[0])),
            (1, 3, 0x20, vec![0, 2])
        );
        assert!(!t.in_gesture());
        assert!(t.translate(ev(TouchPhase::Up, 0, 20.0, 4)).is_empty());
    }
}
