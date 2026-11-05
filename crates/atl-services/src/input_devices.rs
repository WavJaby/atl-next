// SPDX-License-Identifier: MIT
//! The desktop's input devices as Android sees them: ids and sources shared by the `input` service's device list and the composer's MotionEvents (an event's device id must name a listed device, InputManagerGlobal.getInputDevice).
//! Static rows: the ideal is rows from the platform's seat capabilities with hot-plug; not built (no GDK seat-enumeration seam yet; Configuration.touchscreen / keyboard would derive from it).

/// InputManagerGlobal.VIRTUAL_KEYBOARD: KeyCharacterMap.load(deviceId) asks for it by this id.
pub const VIRTUAL_KEYBOARD: i32 = -1;
/// The mouse (the first id after the virtual keyboard's).
pub const POINTER: i32 = 1;
/// The physical keyboard: key events, AOSP's Generic.kl / Generic.kcm (the map KeyMap::load falls back to for a keyboard with no own files).
pub const KEYBOARD: i32 = 2;
/// The touchscreen: touch contacts.
pub const TOUCHSCREEN: i32 = 3;
/// The touchpad: its two-finger swipes (GestureConverter's fake finger); its cursor moves come as the mouse's.
pub const TOUCHPAD: i32 = 4;

/// InputDevice.SOURCE_KEYBOARD (SOURCE_CLASS_BUTTON 0x1 | 0x100).
pub const SOURCE_KEYBOARD: i32 = 0x0000_0101;
/// InputDevice.SOURCE_MOUSE (SOURCE_CLASS_POINTER 0x2 | 0x2000).
pub const SOURCE_MOUSE: i32 = 0x0000_2002;
/// InputDevice.SOURCE_TOUCHSCREEN (SOURCE_CLASS_POINTER 0x2 | 0x1000).
pub const SOURCE_TOUCHSCREEN: i32 = 0x0000_1002;
/// InputDevice.SOURCE_TOUCHPAD (SOURCE_CLASS_POSITION 0x8 | 0x100000). A touchpad device lists SOURCE_MOUSE | SOURCE_TOUCHPAD (TouchpadInputMapper::getSources).
pub const SOURCE_TOUCHPAD: i32 = 0x0010_0008;
