// SPDX-License-Identifier: MIT
//! Touchscreen contacts as Android MotionEvents: TouchInputMapper's dispatch (dispatchTouches) for the contacts down, one gesture from the first contact down to the last up.
//! Pure: [`Touch::translate`] turns one platform contact event into the [`Motion`]s Android would deliver.
//! * Pointer id = the platform's contact id (both the lowest free id while down, TouchEvent contract); pointers listed in id order (TouchInputMapper walks its id bits ascending).
//! * First contact -> DOWN; another -> POINTER_DOWN | index << ACTION_POINTER_INDEX_SHIFT (index of the new pointer in the list, which includes it); a move -> MOVE with every pointer;
//!   a lift with others down -> POINTER_UP | index (the list still has it), the last -> UP; a cancel voids the whole gesture -> CANCEL with FLAG_CANCELED (InputDispatcher synthesizes it so: every pointer, once).
//! * Source SOURCE_TOUCHSCREEN, TOOL_TYPE_FINGER, pressure 1, cursor NaN (a touchscreen hit-tests at its pointer: MotionEvent.getXDispatchLocation reads the cursor only for SOURCE_MOUSE).

use atl_platform_ui::{PhysPoint, TouchEvent, TouchPhase};
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

#[cfg(test)]
mod tests {
    use super::*;

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
