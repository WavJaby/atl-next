// SPDX-License-Identifier: MIT
//! KeyCharacterMap combine / fallback / Parcel round trip through atl-libinput, against AOSP's documented semantics.
//! Expected values: android/keycodes.h (ESCAPE 111, BACK 4, HOME 3, MENU 82, A 29), android/input.h meta bits (ALT_ON 0x02, ALT_LEFT_ON 0x10, CTRL_ON 0x1000);
//! KeyCharacterMap.cpp getFallbackAction (fallback meta = event meta minus the behaviour's own modifiers), combine (an overlay key replaces the base key whole: `mKeys.insert_or_assign`).

use atl_libinput::KeyCharacterMap;

const ESCAPE: i32 = 111;
const BACK: i32 = 4;
const HOME: i32 = 3;
const MENU: i32 = 82;
const A: i32 = 29;
const ALT_ON: i32 = 0x02;
const ALT_LEFT_ON: i32 = 0x10;
const CTRL_ON: i32 = 0x1000;

/// A base map whose ESCAPE block is pinned Generic.kcm's (frameworks/base/data/keyboards/Generic.kcm:501-505), plus one printing key.
const BASE: &str = "type FULL\n\
key A {\n    label: 'A'\n    base: 'a'\n    shift, capslock: 'A'\n}\n\
key ESCAPE {\n    base: none\n    alt: fallback HOME\n    ctrl: fallback MENU\n}\n";

/// An overlay listing ESCAPE with only a base behaviour: combine must drop the base map's alt / ctrl ones.
const BARE_OVERLAY: &str = "type OVERLAY\nkey ESCAPE {\n    base: fallback BACK\n}\n";

fn base() -> KeyCharacterMap {
    KeyCharacterMap::load("Generic.kcm", BASE.as_bytes()).expect("base map loads")
}

#[test]
fn base_map_has_no_plain_escape_fallback() {
    let map = base();
    assert_eq!(map.fallback_action(ESCAPE, 0), None);
    assert_eq!(map.fallback_action(ESCAPE, ALT_ON), Some((HOME, 0)));
    assert_eq!(
        map.fallback_action(ESCAPE, ALT_ON | ALT_LEFT_ON),
        Some((HOME, ALT_LEFT_ON))
    );
    assert_eq!(map.fallback_action(ESCAPE, CTRL_ON), Some((MENU, 0)));
    assert_eq!(map.fallback_action(A, 0), None);
}

#[test]
fn overlay_key_replaces_the_base_key_whole() {
    let mut map = base();
    let overlay =
        KeyCharacterMap::load_overlay("o.kcm", BARE_OVERLAY.as_bytes()).expect("overlay loads");
    map.combine(&overlay);
    assert_eq!(map.fallback_action(ESCAPE, 0), Some((BACK, 0)));
    // the base's alt behaviour went with its key, and `base` does not match an event with ALT (matchesMetaState: CTRL / ALT / META must match exactly)
    assert_eq!(map.fallback_action(ESCAPE, ALT_ON), None);
    // keys the overlay does not list stay the base's
    assert_eq!(map.fallback_action(A, 0), None);
}

#[test]
fn overlay_format_requires_type_overlay() {
    assert!(KeyCharacterMap::load_overlay("o.kcm", BASE.as_bytes()).is_err());
}

#[test]
fn parcel_round_trip_keeps_the_combined_map() {
    let mut map = base();
    let overlay =
        KeyCharacterMap::load_overlay("o.kcm", BARE_OVERLAY.as_bytes()).expect("overlay loads");
    map.combine(&overlay);
    let bytes = map.parcel_bytes();
    let read = KeyCharacterMap::from_parcel(&bytes).expect("parcel reads");
    assert_eq!(read.parcel_bytes(), bytes);
    assert_eq!(read.fallback_action(ESCAPE, 0), Some((BACK, 0)));
    assert!(KeyCharacterMap::from_parcel(&[]).is_err());
}
