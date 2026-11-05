// SPDX-License-Identifier: MIT
//! The desktop's input devices as Android sees them: ids and sources shared by the `input` service's device list and the composer's MotionEvents (an event's device id must name a listed device, InputManagerGlobal.getInputDevice).
//! Static rows: the ideal is rows from the platform's seat capabilities with hot-plug; not built (no GDK seat-enumeration seam yet; Configuration.touchscreen / keyboard would derive from it).

/// InputManagerGlobal.VIRTUAL_KEYBOARD: KeyCharacterMap.load(deviceId) asks for it by this id.
pub const VIRTUAL_KEYBOARD: i32 = -1;
/// The mouse (the first id after the virtual keyboard's).
pub const POINTER: i32 = 1;
/// The touchscreen: touch contacts.
pub const TOUCHSCREEN: i32 = 3;

/// InputDevice.SOURCE_KEYBOARD (SOURCE_CLASS_BUTTON 0x1 | 0x100).
pub const SOURCE_KEYBOARD: i32 = 0x0000_0101;
/// InputDevice.SOURCE_MOUSE (SOURCE_CLASS_POINTER 0x2 | 0x2000).
pub const SOURCE_MOUSE: i32 = 0x0000_2002;
/// InputDevice.SOURCE_TOUCHSCREEN (SOURCE_CLASS_POINTER 0x2 | 0x1000).
pub const SOURCE_TOUCHSCREEN: i32 = 0x0000_1002;
