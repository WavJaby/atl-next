// SPDX-License-Identifier: MIT
//! android.media.AudioDeviceAttributes (media/java/android/media/AudioDeviceAttributes.java writeToParcel:340-350, AudioDeviceAttributes(Parcel):352-364), field for field.
//! The device IAudioService.registerDeviceVolumeDispatcherForAbsoluteVolume names and IAudioDeviceVolumeDispatcher echoes back: role, type, address, name, native type, then the profiles and descriptors as writeParcelableArray (count, then each with its class name).
//! Only the empty profile and descriptor lists are modelled (an app-side absolute-volume device declares none); a non-empty list is refused, and so is a null one (-1: Java's Arrays.asList(null) throws).

use crate::parcel::*;
use crate::parts::*;

/// android.media.AudioDeviceAttributes's parcelled fields, the two lists left out (always empty here).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AudioDeviceAttributes {
    /// ROLE_OUTPUT (2) or ROLE_INPUT (1)
    pub role: i32,
    /// AudioDeviceInfo.TYPE_*
    pub device_type: i32,
    pub address: Option<String>,
    pub name: Option<String>,
    /// AudioSystem.DEVICE_OUT_* / DEVICE_IN_* (AudioDeviceInfo.convertDeviceTypeToInternalDevice)
    pub native_type: i32,
}

/// readParcelableArray's count for a list this codec models: only 0.
fn r_empty_array(p: &In<'_>, field: &'static str) -> Result<()> {
    if r_i32(p)? == 0 {
        Ok(())
    } else {
        Err(crate::refuse(field))
    }
}

impl Part for AudioDeviceAttributes {
    fn write(&self, p: &mut In<'_>) -> Result<()> {
        w_i32(p, self.role)?;
        w_i32(p, self.device_type)?;
        w_str16(p, &self.address)?;
        w_str16(p, &self.name)?;
        w_i32(p, self.native_type)?;
        // writeParcelableArray of the two empty lists: count 0 each
        w_i32(p, 0)?;
        w_i32(p, 0)
    }

    fn read(p: &In<'_>) -> Result<Self> {
        let v = AudioDeviceAttributes {
            role: r_i32(p)?,
            device_type: r_i32(p)?,
            address: r_str16(p)?,
            name: r_str16(p)?,
            native_type: r_i32(p)?,
        };
        r_empty_array(
            p,
            "android.media.AudioDeviceAttributes.mAudioProfiles: null or non-empty (not modelled)",
        )?;
        r_empty_array(
            p,
            "android.media.AudioDeviceAttributes.mAudioDescriptors: null or non-empty (not modelled)",
        )?;
        Ok(v)
    }
}

crate::codec!(AudioDeviceAttributes);
