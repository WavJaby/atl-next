// SPDX-License-Identifier: MIT
//! android.media.VolumeInfo (media/java/android/media/VolumeInfo.java writeToParcel:356-367, VolumeInfo(Parcel):369-382), field for field.
//! The volume an absolute-volume device's dispatcher receives (IAudioDeviceVolumeDispatcher) and registers with (registerDeviceVolumeDispatcherForAbsoluteVolume's list): stream form only.
//! The volume-group form (mUsesStreamType false) appends an AudioVolumeGroup, which is not modelled: refused on read and unrepresentable on write.

use crate::parcel::*;
use crate::parts::*;

/// VolumeInfo.INDEX_NOT_SET: an index the sender left unset.
pub const INDEX_NOT_SET: i32 = -100;

/// android.media.VolumeInfo's parcelled fields in the stream form (mUsesStreamType true).
#[derive(Debug, Clone, PartialEq)]
pub struct VolumeInfo {
    /// AudioManager.STREAM_*
    pub stream_type: i32,
    /// true when the sender set a mute state (Builder.setMuted)
    pub has_mute_command: bool,
    pub muted: bool,
    pub index: i32,
    pub min_index: i32,
    pub max_index: i32,
}

/// Java's Builder(streamType) defaults: no mute command, every index INDEX_NOT_SET.
impl Default for VolumeInfo {
    fn default() -> Self {
        VolumeInfo {
            stream_type: 0,
            has_mute_command: false,
            muted: false,
            index: INDEX_NOT_SET,
            min_index: INDEX_NOT_SET,
            max_index: INDEX_NOT_SET,
        }
    }
}

impl Part for VolumeInfo {
    fn write(&self, p: &mut In<'_>) -> Result<()> {
        w_bool(p, true)?;
        w_i32(p, self.stream_type)?;
        w_bool(p, self.has_mute_command)?;
        w_bool(p, self.muted)?;
        w_i32(p, self.index)?;
        w_i32(p, self.min_index)?;
        w_i32(p, self.max_index)
    }

    fn read(p: &In<'_>) -> Result<Self> {
        if !r_bool(p)? {
            return Err(crate::refuse(
                "android.media.VolumeInfo.mVolGroup: volume-group form (not modelled)",
            ));
        }
        Ok(VolumeInfo {
            stream_type: r_i32(p)?,
            has_mute_command: r_bool(p)?,
            muted: r_bool(p)?,
            index: r_i32(p)?,
            min_index: r_i32(p)?,
            max_index: r_i32(p)?,
        })
    }
}

crate::codec!(VolumeInfo);
