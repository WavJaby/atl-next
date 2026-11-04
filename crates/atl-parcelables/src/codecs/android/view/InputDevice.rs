// SPDX-License-Identifier: MIT
//! android.view.InputDevice (frameworks/base/core/java/android/view/InputDevice.java writeToParcel:1604-1644; the reading constructor :540-560). The first field is the KeyCharacterMap as Java frames it (frameworks/base/core/jni/android_view_KeyCharacterMap.cpp nativeWriteToParcel): the map's deviceId, a present flag (1), then libinput's KeyCharacterMap::writeToParcel bytes. Rust never parses or writes libinput's part: those bytes are the map atl-libinput writes (`KeyCharacterMap::writeToParcel` of Virtual.kcm), passed through verbatim. The map's length is not in the stream, so a read needs those bytes first ([`set_key_character_map`]) and compares its words against them. The map's deviceId is the device's id (the map is loaded for that device). Motion ranges are modelled as none (a device with ranges is refused by name). HostUsiVersion is two int32 (mMajorVersion, mMinorVersion), always present.

use std::sync::OnceLock;

use crate::parcel::*;
use crate::parts::*;

/// The KeyCharacterMap Parcel form, set once per process by the process that reads or serves devices.
static KEY_CHARACTER_MAP: OnceLock<Vec<u8>> = OnceLock::new();

/// Sets the map that every InputDevice carries. Parcel data is 4-byte words, so the length must be a non-empty multiple of 4; a second set must give the same bytes.
pub fn set_key_character_map(bytes: &[u8]) -> std::result::Result<(), &'static str> {
    if bytes.is_empty() || !bytes.len().is_multiple_of(4) {
        return Err("key character map: not a non-empty multiple of 4 bytes");
    }
    if KEY_CHARACTER_MAP.get_or_init(|| bytes.to_vec()).as_slice() != bytes {
        return Err("key character map: already set to other bytes");
    }
    Ok(())
}

/// The map, once set.
pub fn key_character_map() -> Option<&'static [u8]> {
    KEY_CHARACTER_MAP.get().map(Vec::as_slice)
}

/// The little-endian int32 words of a Parcel-form blob.
fn words(bytes: &[u8]) -> impl Iterator<Item = i32> + '_ {
    bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|w| i32::from_le_bytes(*w))
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct InputDevice {
    /// The KeyCharacterMap's Parcel form, verbatim (empty until set).
    pub key_character_map: Vec<u8>,
    pub id: i32,
    pub generation: i32,
    pub controller_number: i32,
    /// mName (String16).
    pub name: Option<String>,
    pub vendor_id: i32,
    pub product_id: i32,
    pub device_bus: i32,
    /// mDescriptor (String16).
    pub descriptor: Option<String>,
    pub is_external: bool,
    pub sources: i32,
    pub keyboard_type: i32,
    /// mKeyboardLanguageTag and mKeyboardLayoutType (String8).
    pub keyboard_language_tag: Option<String>,
    pub keyboard_layout_type: Option<String>,
    pub has_vibrator: bool,
    pub has_microphone: bool,
    pub has_button_under_pad: bool,
    pub has_sensor: bool,
    pub has_battery: bool,
    /// HostUsiVersion's major and minor (-1, -1: no USI).
    pub usi_version_major: i32,
    pub usi_version_minor: i32,
    pub associated_display_id: i32,
    pub enabled: bool,
    /// ViewBehavior.mShouldSmoothScroll.
    pub should_smooth_scroll: bool,
}

impl Part for InputDevice {
    fn write(&self, p: &mut In<'_>) -> Result<()> {
        // KeyCharacterMap framing: deviceId, present, then libinput's form (verbatim)
        w_i32(p, self.id)?;
        w_bool(p, true)?;
        for word in words(&self.key_character_map) {
            w_i32(p, word)?;
        }
        w_i32(p, self.id)?;
        w_i32(p, self.generation)?;
        w_i32(p, self.controller_number)?;
        w_str16(p, &self.name)?;
        w_i32(p, self.vendor_id)?;
        w_i32(p, self.product_id)?;
        w_i32(p, self.device_bus)?;
        w_str16(p, &self.descriptor)?;
        w_bool(p, self.is_external)?;
        w_i32(p, self.sources)?;
        w_i32(p, self.keyboard_type)?;
        w_str8(p, &self.keyboard_language_tag)?;
        w_str8(p, &self.keyboard_layout_type)?;
        w_bool(p, self.has_vibrator)?;
        w_bool(p, self.has_microphone)?;
        w_bool(p, self.has_button_under_pad)?;
        w_bool(p, self.has_sensor)?;
        w_bool(p, self.has_battery)?;
        w_i32(p, self.usi_version_major)?;
        w_i32(p, self.usi_version_minor)?;
        w_i32(p, self.associated_display_id)?;
        w_bool(p, self.enabled)?;
        // numRanges: no motion ranges modelled
        w_i32(p, 0)?;
        w_bool(p, self.should_smooth_scroll)
    }

    fn read(p: &In<'_>) -> Result<Self> {
        let Some(map) = key_character_map() else {
            return Err(crate::refuse(
                "android.view.InputDevice (no KeyCharacterMap set to read against)",
            ));
        };
        let map_id = r_i32(p)?;
        if !r_bool(p)? {
            return Err(crate::refuse(
                "android.view.InputDevice (absent KeyCharacterMap)",
            ));
        }
        for want in words(map) {
            if r_i32(p)? != want {
                return Err(crate::refuse(
                    "android.view.InputDevice (KeyCharacterMap differs from the map set)",
                ));
            }
        }
        let id = r_i32(p)?;
        if id != map_id {
            return Err(crate::refuse(
                "android.view.InputDevice (KeyCharacterMap's deviceId differs from the device's)",
            ));
        }
        let generation = r_i32(p)?;
        let controller_number = r_i32(p)?;
        let name = r_str16(p)?;
        let vendor_id = r_i32(p)?;
        let product_id = r_i32(p)?;
        let device_bus = r_i32(p)?;
        let descriptor = r_str16(p)?;
        let is_external = r_bool(p)?;
        let sources = r_i32(p)?;
        let keyboard_type = r_i32(p)?;
        let keyboard_language_tag = r_str8(p)?;
        let keyboard_layout_type = r_str8(p)?;
        let has_vibrator = r_bool(p)?;
        let has_microphone = r_bool(p)?;
        let has_button_under_pad = r_bool(p)?;
        let has_sensor = r_bool(p)?;
        let has_battery = r_bool(p)?;
        let usi_version_major = r_i32(p)?;
        let usi_version_minor = r_i32(p)?;
        let associated_display_id = r_i32(p)?;
        let enabled = r_bool(p)?;
        if r_i32(p)? != 0 {
            return Err(crate::refuse("android.view.InputDevice (motion ranges)"));
        }
        let should_smooth_scroll = r_bool(p)?;
        Ok(InputDevice {
            key_character_map: map.to_vec(),
            id,
            generation,
            controller_number,
            name,
            vendor_id,
            product_id,
            device_bus,
            descriptor,
            is_external,
            sources,
            keyboard_type,
            keyboard_language_tag,
            keyboard_layout_type,
            has_vibrator,
            has_microphone,
            has_button_under_pad,
            has_sensor,
            has_battery,
            usi_version_major,
            usi_version_minor,
            associated_display_id,
            enabled,
            should_smooth_scroll,
        })
    }
}

crate::codec!(InputDevice);
