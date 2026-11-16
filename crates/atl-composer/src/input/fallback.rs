// SPDX-License-Identifier: MIT
//! The dispatcher's fallback keys: a key the window finished unhandled -> the fallback its device's key map names (KeyCharacterMap::getFallbackAction), published to the same window with FLAG_FALLBACK.
//! AOSP: InputDispatcher::afterKeyEventLockedInterruptable + its policy InputManagerService.dispatchUnhandledKey (the Java fallback table). Pure: the key map lookup is a function argument.
//! * A fallback key never gets a fallback (dispatchUnhandledKey returns null for FLAG_FALLBACK; the dispatcher only drops its entry).
//! * The fallback keycode is latched per window and original keycode on the initial DOWN (repeat 0): later DOWNs and the UP use the latch, never a new lookup. No fallback on the initial DOWN latches "none" (AKEYCODE_UNKNOWN). The UP removes the latch.
//! * Unhandled with a latch: the fallback carries the original's action, repeat, times, device, scan code and source; keycode + meta from the map; flags = original | FLAG_FALLBACK.
//! * Handled with a latch (the app took the original after all): the fallback key, if held, is cancelled (UP | FLAG_CANCELED) and the latch removed.
//! * A fallback key is published only where the window's key state accepts it (InputState::trackKey): an UP needs its DOWN held, else it is dropped as inconsistent. Focus loss cancels every held fallback key (synthesizeCancelationEvents).
//! * A removed window's state is dropped; nothing is sent.

use std::collections::HashMap;

use super::key::{ACTION_DOWN, ACTION_UP, FLAG_CANCELED, Key};

/// AKEY_EVENT_FLAG_FALLBACK (input.h): the key is a fallback for an unhandled one.
pub const FLAG_FALLBACK: i32 = 0x400;

/// One window's fallback state: latch per original keycode (None: the initial DOWN had no fallback), held fallback keys by keycode (the DOWN last published).
#[derive(Debug, Default)]
struct Window {
    latch: HashMap<i32, Option<(i32, i32)>>,
    held: HashMap<i32, Key>,
}

impl Window {
    /// The cancel of held fallback `code`, when held.
    fn cancel(&mut self, code: i32, time_ns: i64) -> Option<Key> {
        let down = self.held.remove(&code)?;
        Some(Key {
            action: ACTION_UP,
            flags: down.flags | FLAG_CANCELED,
            repeat_count: 0,
            time_ns,
            ..down
        })
    }

    /// `k` as the window's key state takes it: a DOWN is held, an UP releases its DOWN; an UP with no DOWN held -> None (dropped).
    fn track(&mut self, k: Key) -> Option<Key> {
        if k.action == ACTION_DOWN {
            self.held.insert(k.key_code, k);
            Some(k)
        } else {
            self.held.remove(&k.key_code).map(|_| k)
        }
    }
}

/// The fallback state of every window, keyed by the window's key, and the keys in flight by seq.
#[derive(Debug)]
pub struct Fallbacks<K> {
    pending: HashMap<u32, (K, Key)>,
    windows: Vec<(K, Window)>,
}

impl<K> Default for Fallbacks<K> {
    fn default() -> Self {
        Self {
            pending: HashMap::new(),
            windows: Vec::new(),
        }
    }
}

impl<K: PartialEq + Clone> Fallbacks<K> {
    fn window(&mut self, key: &K) -> &mut Window {
        let i = match self.windows.iter().position(|(k, _)| k == key) {
            Some(i) => i,
            None => {
                self.windows.push((key.clone(), Window::default()));
                self.windows.len() - 1
            }
        };
        &mut self.windows[i].1
    }

    /// `k` was published to `window` as `seq`; its finish comes to [`Fallbacks::finished`].
    pub fn published(&mut self, window: &K, seq: u32, k: Key) {
        self.pending.insert(seq, (window.clone(), k));
    }

    /// `window` finished `seq`: the fallback keys to publish to it, in order (each goes through [`Fallbacks::published`] too). `fallback(device, keycode, meta)`: the device map's fallback (keycode, meta), None for none.
    /// A seq not recorded here (a motion, a focus event) -> nothing.
    pub fn finished(
        &mut self,
        window: &K,
        seq: u32,
        handled: bool,
        fallback: impl FnOnce(i32, i32, i32) -> Option<(i32, i32)>,
    ) -> Vec<Key> {
        let Some((owner, k)) = self.pending.remove(&seq) else {
            return Vec::new();
        };
        if owner != *window || k.flags & FLAG_FALLBACK != 0 {
            return Vec::new();
        }
        let w = self.window(window);
        let latched = w.latch.get(&k.key_code).copied();
        if k.action == ACTION_UP {
            w.latch.remove(&k.key_code);
        }
        if handled {
            if let Some(latched) = latched {
                w.latch.remove(&k.key_code);
                if let Some(cancel) = latched.and_then(|(code, _)| w.cancel(code, k.time_ns)) {
                    return vec![cancel];
                }
            }
            return Vec::new();
        }
        let initial_down = k.action == ACTION_DOWN && k.repeat_count == 0;
        let action = match latched {
            _ if initial_down => {
                let action = fallback(k.device_id, k.key_code, k.meta_state);
                w.latch.insert(k.key_code, action);
                action
            }
            Some(action) => action,
            None => return Vec::new(),
        };
        let Some((code, meta)) = action else {
            return Vec::new();
        };
        w.track(Key {
            key_code: code,
            meta_state: meta,
            flags: k.flags | FLAG_FALLBACK,
            ..k
        })
        .into_iter()
        .collect()
    }

    /// `window` lost the focus: the cancel of each fallback key it holds (synthesizeCancelationEvents of its keys), at `time_ns`. Latches stay: a later finish sends no fallback UP for a key no longer held.
    pub fn focus_lost(&mut self, window: &K, time_ns: i64) -> Vec<Key> {
        let w = self.window(window);
        let mut codes: Vec<i32> = w.held.keys().copied().collect();
        codes.sort_unstable();
        codes
            .into_iter()
            .filter_map(|code| w.cancel(code, time_ns))
            .collect()
    }

    /// Keeps the windows `held` says are still there: a gone window's state and keys in flight are dropped; nothing is sent.
    pub fn retain(&mut self, held: impl Fn(&K) -> bool) {
        self.windows.retain(|(k, _)| held(k));
        self.pending.retain(|_, (k, _)| held(k));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::key::FLAG_FROM_SYSTEM;

    // Expected values from AOSP, not from this module: keycodes.h ESCAPE 111, BACK 4, HOME 3, A 29; KeyEvent META_ALT_ON 0x02; input.h FLAG_FROM_SYSTEM 0x8, FLAG_FALLBACK 0x400, FLAG_CANCELED 0x20.
    // The map stands in for Generic.kcm + ATL's overlay: ESCAPE base -> BACK meta 0, alt -> HOME meta 0 (atl-desktop.kcm).
    const ESCAPE: i32 = 111;
    const BACK: i32 = 4;
    const HOME: i32 = 3;
    const A: i32 = 29;
    const META_ALT_ON: i32 = 0x02;
    const KEYBOARD: i32 = 2;

    fn map(_device: i32, code: i32, meta: i32) -> Option<(i32, i32)> {
        match (code, meta) {
            (ESCAPE, 0) => Some((BACK, 0)),
            (ESCAPE, META_ALT_ON) => Some((HOME, 0)),
            _ => None,
        }
    }

    fn key(action: i32, code: i32, repeat: i32, time_ns: i64) -> Key {
        Key {
            action,
            flags: FLAG_FROM_SYSTEM,
            key_code: code,
            scan_code: 1,
            meta_state: 0,
            repeat_count: repeat,
            down_ns: 10,
            time_ns,
            device_id: KEYBOARD,
            source: 0x101,
        }
    }

    /// Publishes `k` as `seq` to window 1 and finishes it.
    fn run(f: &mut Fallbacks<u8>, seq: u32, k: Key, handled: bool) -> Vec<Key> {
        f.published(&1, seq, k);
        let out = f.finished(&1, seq, handled, map);
        for (i, o) in out.iter().enumerate() {
            f.published(&1, 1000 + seq * 10 + i as u32, *o);
        }
        out
    }

    #[test]
    fn fallback_unhandled_esc_down_and_up_send_back_down_and_up() {
        let mut f = Fallbacks::default();
        let down = run(&mut f, 1, key(ACTION_DOWN, ESCAPE, 0, 10), false);
        assert_eq!(down.len(), 1);
        let d = down[0];
        assert_eq!(
            (
                d.action,
                d.key_code,
                d.meta_state,
                d.flags,
                d.scan_code,
                d.device_id,
                d.down_ns,
                d.time_ns
            ),
            (ACTION_DOWN, BACK, 0, 0x408, 1, KEYBOARD, 10, 10)
        );
        let up = run(&mut f, 2, key(ACTION_UP, ESCAPE, 0, 20), false);
        assert_eq!(
            up.iter()
                .map(|k| (k.action, k.key_code, k.flags, k.time_ns))
                .collect::<Vec<_>>(),
            [(ACTION_UP, BACK, 0x408, 20)]
        );
        // the latch is gone with the UP: a new press looks the map up again
        assert!(f.windows[0].1.latch.is_empty());
    }

    #[test]
    fn fallback_never_falls_back_from_a_fallback() {
        let mut f = Fallbacks::default();
        let back = run(&mut f, 1, key(ACTION_DOWN, ESCAPE, 0, 10), false);
        // the BACK DOWN itself finished unhandled: nothing more
        assert!(f.finished(&1, 1010, false, map).is_empty());
        assert_eq!(back.len(), 1);
    }

    #[test]
    fn fallback_none_for_a_key_without_one_and_not_on_a_later_lookup() {
        let mut f = Fallbacks::default();
        assert!(run(&mut f, 1, key(ACTION_DOWN, A, 0, 10), false).is_empty());
        // the latch says none: the UP sends nothing even with a map that would answer now
        f.published(&1, 2, key(ACTION_UP, A, 0, 20));
        assert!(
            f.finished(&1, 2, false, |_, _, _| Some((BACK, 0)))
                .is_empty()
        );
    }

    #[test]
    fn fallback_non_initial_down_without_latch_sends_nothing() {
        let mut f = Fallbacks::default();
        // a repeat whose initial DOWN this module never saw finish: no lookup (AOSP: "not an initial down")
        f.published(&1, 1, key(ACTION_DOWN, ESCAPE, 2, 10));
        assert!(
            f.finished(&1, 1, false, |_, _, _| panic!("looked up"))
                .is_empty()
        );
    }

    #[test]
    fn fallback_repeats_follow_the_latch() {
        let mut f = Fallbacks::default();
        run(&mut f, 1, key(ACTION_DOWN, ESCAPE, 0, 10), false);
        let r = run(&mut f, 2, key(ACTION_DOWN, ESCAPE, 1, 15), false);
        assert_eq!(
            r.iter()
                .map(|k| (k.action, k.key_code, k.repeat_count))
                .collect::<Vec<_>>(),
            [(ACTION_DOWN, BACK, 1)]
        );
    }

    #[test]
    fn fallback_alt_esc_is_home_with_the_maps_meta() {
        let mut f = Fallbacks::default();
        let mut k = key(ACTION_DOWN, ESCAPE, 0, 10);
        k.meta_state = META_ALT_ON;
        let out = run(&mut f, 1, k, false);
        assert_eq!(
            out.iter()
                .map(|k| (k.key_code, k.meta_state))
                .collect::<Vec<_>>(),
            [(HOME, 0)]
        );
    }

    #[test]
    fn fallback_handled_original_cancels_the_held_fallback() {
        let mut f = Fallbacks::default();
        run(&mut f, 1, key(ACTION_DOWN, ESCAPE, 0, 10), false);
        // a repeat the app now handles: BACK is cancelled, the latch removed
        let out = run(&mut f, 2, key(ACTION_DOWN, ESCAPE, 1, 15), true);
        assert_eq!(
            out.iter()
                .map(|k| (k.action, k.key_code, k.flags, k.time_ns))
                .collect::<Vec<_>>(),
            [(ACTION_UP, BACK, 0x428, 15)]
        );
        // the UP then finds no latch: nothing
        assert!(run(&mut f, 3, key(ACTION_UP, ESCAPE, 0, 20), false).is_empty());
    }

    #[test]
    fn fallback_handled_up_cancels_and_handled_down_without_latch_sends_nothing() {
        let mut f = Fallbacks::default();
        assert!(run(&mut f, 1, key(ACTION_DOWN, ESCAPE, 0, 10), true).is_empty());
        let mut f = Fallbacks::default();
        run(&mut f, 1, key(ACTION_DOWN, ESCAPE, 0, 10), false);
        let out = run(&mut f, 2, key(ACTION_UP, ESCAPE, 0, 20), true);
        assert_eq!(
            out.iter().map(|k| (k.action, k.flags)).collect::<Vec<_>>(),
            [(ACTION_UP, 0x428)]
        );
    }

    #[test]
    fn fallback_focus_loss_cancels_held_and_the_late_up_is_dropped() {
        let mut f = Fallbacks::default();
        run(&mut f, 1, key(ACTION_DOWN, ESCAPE, 0, 10), false);
        let cancels = f.focus_lost(&1, 15);
        assert_eq!(
            cancels
                .iter()
                .map(|k| (k.action, k.key_code, k.flags, k.time_ns))
                .collect::<Vec<_>>(),
            [(ACTION_UP, BACK, 0x428, 15)]
        );
        // the original's (cancelled) UP finishes unhandled later: BACK is no longer held, so no second UP
        let mut up = key(ACTION_UP, ESCAPE, 0, 20);
        up.flags |= FLAG_CANCELED;
        assert!(run(&mut f, 2, up, false).is_empty());
        assert!(f.focus_lost(&1, 30).is_empty());
    }

    #[test]
    fn fallback_removed_window_drops_its_keys_in_flight() {
        let mut f = Fallbacks::default();
        f.published(&1, 1, key(ACTION_DOWN, ESCAPE, 0, 10));
        f.retain(|k| *k != 1);
        assert!(f.finished(&1, 1, false, map).is_empty());
        assert!(f.windows.is_empty());
    }

    #[test]
    fn fallback_state_is_per_window() {
        let mut f = Fallbacks::default();
        run(&mut f, 1, key(ACTION_DOWN, ESCAPE, 0, 10), false);
        // window 2's ESC UP (its DOWN never seen) has no latch there
        f.published(&2, 2, key(ACTION_UP, ESCAPE, 0, 20));
        assert!(f.finished(&2, 2, false, map).is_empty());
        // a finish reported by another window than the one published to is not this seq's
        f.published(&1, 3, key(ACTION_UP, ESCAPE, 0, 20));
        assert!(f.finished(&2, 3, false, map).is_empty());
    }
}
