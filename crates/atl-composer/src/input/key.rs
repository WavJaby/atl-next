// SPDX-License-Identifier: MIT
//! Physical keys as Android KeyEvents: KeyboardInputMapper's part (keycode from the device's `.kl`, meta state) plus InputDispatcher's repeat numbering.
//! Pure: [`Keys::translate`] takes AOSP's key layout lookup and meta update as functions (atl-libinput's `KeyLayoutMap::map_key`, `update_meta_state` in the converter build).
//! * Keycode = the layout's for the scan code; a code it does not list -> KEYCODE_UNKNOWN 0, still delivered (KeyboardInputMapper::processKey keeps going on a failed mapKey).
//! * Meta: lock bits taken from the platform's lock states before each key (they show the state before the key), then AOSP updateMetaState for the key (modifiers set/cleared, a lock key toggles on down).
//! * Repeat: the desktop repeats (GTK synthesizes presses from the compositor's repeat_info), not a dispatcher timer. A press of a held scan code -> ACTION_DOWN with repeatCount + 1, the first down's time; repeatCount 1 adds FLAG_LONG_PRESS (InputDispatcher::synthesizeKeyRepeatLocked + the long-press flag at repeat 1).
//! * Focus loss: every held key -> ACTION_UP with FLAG_CANCELED (InputDispatcher's synthesized cancel of a window's keys), its modifier bits cleared.
//! * Source SOURCE_KEYBOARD, device KEYBOARD (input_devices), flags FLAG_FROM_SYSTEM (every reader-made key event carries it).

use atl_platform_ui::{KeyEvent, Locks};
use atl_services_types::input_devices::{KEYBOARD, SOURCE_KEYBOARD};

/// KeyEvent actions (NDK input.h AKEY_EVENT_ACTION_*).
pub const ACTION_DOWN: i32 = 0;
pub const ACTION_UP: i32 = 1;
/// KeyEvent flags (input.h AKEY_EVENT_FLAG_*).
pub const FLAG_FROM_SYSTEM: i32 = 0x8;
pub const FLAG_CANCELED: i32 = 0x20;
pub const FLAG_LONG_PRESS: i32 = 0x80;
/// KeyEvent.KEYCODE_UNKNOWN.
const KEYCODE_UNKNOWN: i32 = 0;
/// KeyEvent.META_CAPS_LOCK_ON, META_NUM_LOCK_ON, META_SCROLL_LOCK_ON.
const META_CAPS_LOCK_ON: i32 = 0x0010_0000;
const META_NUM_LOCK_ON: i32 = 0x0020_0000;
const META_SCROLL_LOCK_ON: i32 = 0x0040_0000;
const META_LOCKS: i32 = META_CAPS_LOCK_ON | META_NUM_LOCK_ON | META_SCROLL_LOCK_ON;

/// One KeyEvent for the focused window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Key {
    pub action: i32,
    pub flags: i32,
    pub key_code: i32,
    pub scan_code: i32,
    pub meta_state: i32,
    pub repeat_count: i32,
    pub down_ms: u32,
    pub time_ms: u32,
    /// The device (input_devices).
    pub device_id: i32,
    pub source: i32,
}

/// A key held down: its scan code, keycode, first down time, repeats so far.
#[derive(Debug)]
struct Held {
    scan_code: u32,
    key_code: i32,
    down_ms: u32,
    repeats: i32,
}

/// The keyboard's meta state and held keys.
#[derive(Debug, Default)]
pub struct Keys {
    meta: i32,
    held: Vec<Held>,
    /// The last key's time.
    last_ms: u32,
}

impl Keys {
    /// The meta state now: MotionEvents carry it too.
    pub fn meta_state(&self) -> i32 {
        self.meta
    }

    /// The KeyEvent of one platform key. `map` = the layout's keycode of a scan code; `update` = AOSP updateMetaState(keycode, down, old).
    pub fn translate(
        &mut self,
        e: KeyEvent,
        map: impl Fn(u32) -> Option<i32>,
        update: impl Fn(i32, bool, i32) -> i32,
    ) -> Option<Key> {
        self.last_ms = e.time_ms;
        let held = self.held.iter().position(|h| h.scan_code == e.scan_code);
        let (key_code, down_ms, repeat_count) = match (e.down, held) {
            (true, Some(i)) => {
                let h = &mut self.held[i];
                h.repeats += 1;
                (h.key_code, h.down_ms, h.repeats)
            }
            (true, None) => {
                let key_code = map(e.scan_code).unwrap_or(KEYCODE_UNKNOWN);
                self.held.push(Held {
                    scan_code: e.scan_code,
                    key_code,
                    down_ms: e.time_ms,
                    repeats: 0,
                });
                self.meta = update(
                    key_code,
                    true,
                    (self.meta & !META_LOCKS) | lock_bits(e.locks),
                );
                (key_code, e.time_ms, 0)
            }
            (false, Some(i)) => {
                let h = self.held.remove(i);
                self.meta = update(
                    h.key_code,
                    false,
                    (self.meta & !META_LOCKS) | lock_bits(e.locks),
                );
                (h.key_code, h.down_ms, 0)
            }
            // a release whose press this window never saw (pressed before focus): nothing to end
            (false, None) => return None,
        };
        let long_press = if repeat_count == 1 {
            FLAG_LONG_PRESS
        } else {
            0
        };
        Some(Key {
            action: if e.down { ACTION_DOWN } else { ACTION_UP },
            flags: FLAG_FROM_SYSTEM | long_press,
            key_code,
            scan_code: i32::try_from(e.scan_code).unwrap_or(i32::MAX),
            meta_state: self.meta,
            repeat_count,
            down_ms,
            time_ms: e.time_ms,
            device_id: KEYBOARD,
            source: SOURCE_KEYBOARD,
        })
    }

    /// The window lost focus: an ACTION_UP with FLAG_CANCELED for each held key, in press order, at the last key's time (the focus event carries none); nothing stays held.
    pub fn cancel(&mut self, update: impl Fn(i32, bool, i32) -> i32) -> Vec<Key> {
        let time_ms = self.last_ms;
        let held = std::mem::take(&mut self.held);
        held.into_iter()
            .map(|h| {
                self.meta = update(h.key_code, false, self.meta);
                Key {
                    action: ACTION_UP,
                    flags: FLAG_FROM_SYSTEM | FLAG_CANCELED,
                    key_code: h.key_code,
                    scan_code: i32::try_from(h.scan_code).unwrap_or(i32::MAX),
                    meta_state: self.meta,
                    repeat_count: 0,
                    down_ms: h.down_ms,
                    time_ms,
                    device_id: KEYBOARD,
                    source: SOURCE_KEYBOARD,
                }
            })
            .collect()
    }
}

fn lock_bits(locks: Locks) -> i32 {
    (if locks.caps { META_CAPS_LOCK_ON } else { 0 })
        | (if locks.num { META_NUM_LOCK_ON } else { 0 })
        | (if locks.scroll { META_SCROLL_LOCK_ON } else { 0 })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Generic.kl rows the tests use: `key 30 A` (KEYCODE_A 29), `key 42 SHIFT_LEFT` (KEYCODE_SHIFT_LEFT 59).
    fn map(scan: u32) -> Option<i32> {
        match scan {
            30 => Some(29),
            42 => Some(59),
            _ => None,
        }
    }

    /// updateMetaState for SHIFT_LEFT only: META_SHIFT_ON 0x1 | META_SHIFT_LEFT_ON 0x40 (KeyEvent constants), others unchanged.
    fn update(key_code: i32, down: bool, old: i32) -> i32 {
        if key_code != 59 {
            return old;
        }
        if down { old | 0x41 } else { old & !0x41 }
    }

    fn ev(down: bool, scan_code: u32, time_ms: u32) -> KeyEvent {
        KeyEvent {
            down,
            scan_code,
            locks: Locks::default(),
            time_ms,
        }
    }

    #[test]
    fn shift_a_carries_the_meta_state() {
        let mut k = Keys::default();
        let shift = k.translate(ev(true, 42, 1), map, update).unwrap();
        assert_eq!((shift.key_code, shift.meta_state), (59, 0x41));
        let a = k.translate(ev(true, 30, 2), map, update).unwrap();
        assert_eq!(
            (a.action, a.key_code, a.scan_code, a.meta_state),
            (ACTION_DOWN, 29, 30, 0x41)
        );
        assert_eq!(
            (a.device_id, a.source, a.flags),
            (KEYBOARD, SOURCE_KEYBOARD, FLAG_FROM_SYSTEM)
        );
        k.translate(ev(false, 42, 3), map, update).unwrap();
        let up = k.translate(ev(false, 30, 4), map, update).unwrap();
        assert_eq!((up.action, up.meta_state, up.down_ms), (ACTION_UP, 0, 2));
    }

    #[test]
    fn repeats_count_up_and_the_first_is_a_long_press() {
        let mut k = Keys::default();
        k.translate(ev(true, 30, 10), map, update);
        let r1 = k.translate(ev(true, 30, 610), map, update).unwrap();
        let r2 = k.translate(ev(true, 30, 650), map, update).unwrap();
        assert_eq!(
            (r1.repeat_count, r1.flags, r1.down_ms),
            (1, FLAG_FROM_SYSTEM | FLAG_LONG_PRESS, 10)
        );
        assert_eq!((r2.repeat_count, r2.flags), (2, FLAG_FROM_SYSTEM));
        let up = k.translate(ev(false, 30, 700), map, update).unwrap();
        assert_eq!(up.repeat_count, 0);
    }

    #[test]
    fn unknown_scan_codes_are_keycode_unknown_and_stray_releases_dropped() {
        let mut k = Keys::default();
        assert_eq!(
            k.translate(ev(true, 999, 1), map, update).unwrap().key_code,
            KEYCODE_UNKNOWN
        );
        assert_eq!(k.translate(ev(false, 31, 2), map, update), None);
    }

    #[test]
    fn locks_come_from_the_platform() {
        let mut k = Keys::default();
        let mut e = ev(true, 30, 1);
        e.locks.caps = true;
        assert_eq!(
            k.translate(e, map, update).unwrap().meta_state,
            META_CAPS_LOCK_ON
        );
    }

    #[test]
    fn focus_loss_cancels_held_keys() {
        let mut k = Keys::default();
        k.translate(ev(true, 42, 1), map, update);
        k.translate(ev(true, 30, 2), map, update);
        let c = k.cancel(update);
        assert_eq!(c.iter().map(|c| c.key_code).collect::<Vec<_>>(), [59, 29]);
        assert!(
            c.iter()
                .all(|c| c.action == ACTION_UP && c.flags & FLAG_CANCELED != 0 && c.time_ms == 2)
        );
        assert_eq!(c[1].meta_state, 0);
        assert!(k.cancel(update).is_empty());
    }
}
