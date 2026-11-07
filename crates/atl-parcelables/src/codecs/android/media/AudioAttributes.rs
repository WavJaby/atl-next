// SPDX-License-Identifier: MIT
//! android.media.AudioAttributes (media/java/android/media/AudioAttributes.java writeToParcel:1364-1383, AudioAttributes(Parcel):1385-1412), field for field.
//! Holders write it inline as `writeToParcel(dest, 0)` (PlayerBase.PlayerIdCard, IAudioService.playerAttributes): usage, content type, source, flags, the parcel flags, the tags, the bundle.
//! The tags are either a String16 array (writeStringArray; the set's order, kept as read) or, with FLATTEN_TAGS, one String16. The bundle is a marker (-1977 null, 1980 valid), then writeBundle.
//! Java's writer puts only 0 or FLATTEN_TAGS in the parcel flags and never a null flattened string, so any other word is refused.

use crate::codecs::android::os::Bundle::{Bundle, r_bundle, w_bundle};
use crate::parcel::*;
use crate::parts::*;

/// AudioAttributes.FLATTEN_TAGS: the parcel flag that writes the tags as one string.
const FLATTEN_TAGS: i32 = 0x1;
/// AudioAttributes.ATTR_PARCEL_IS_NULL_BUNDLE and ATTR_PARCEL_IS_VALID_BUNDLE: the marker before the bundle.
const ATTR_PARCEL_IS_NULL_BUNDLE: i32 = -1977;
const ATTR_PARCEL_IS_VALID_BUNDLE: i32 = 1980;

/// The tags as the parcel carries them: the array form (mTags, in the order Java's HashSet gave), or the flattened string (mFormattedTags).
#[derive(Debug, Clone, PartialEq)]
pub enum Tags {
    Array(Vec<Option<String>>),
    Flattened(String),
}

/// Java's default: no tags, in array form.
impl Default for Tags {
    fn default() -> Self {
        Tags::Array(Vec::new())
    }
}

/// android.media.AudioAttributes's parcelled fields. No `PartialEq`: the bundle is a Bundle, which has none.
#[derive(Debug, Clone, Default)]
pub struct AudioAttributes {
    pub usage: i32,
    pub content_type: i32,
    pub source: i32,
    pub flags: i32,
    pub tags: Tags,
    pub bundle: Option<Bundle>,
}

impl Part for AudioAttributes {
    fn write(&self, p: &mut In<'_>) -> Result<()> {
        w_i32(p, self.usage)?;
        w_i32(p, self.content_type)?;
        w_i32(p, self.source)?;
        w_i32(p, self.flags)?;
        match &self.tags {
            Tags::Array(tags) => {
                w_i32(p, 0)?;
                w_str16_array(p, &Some(tags.clone()))?;
            }
            Tags::Flattened(tags) => {
                w_i32(p, FLATTEN_TAGS)?;
                w_str16(p, &Some(tags.clone()))?;
            }
        }
        match &self.bundle {
            None => w_i32(p, ATTR_PARCEL_IS_NULL_BUNDLE),
            Some(_) => {
                w_i32(p, ATTR_PARCEL_IS_VALID_BUNDLE)?;
                w_bundle(p, &self.bundle)
            }
        }
    }

    fn read(p: &In<'_>) -> Result<Self> {
        let usage = r_i32(p)?;
        let content_type = r_i32(p)?;
        let source = r_i32(p)?;
        let flags = r_i32(p)?;
        let tags = match r_i32(p)? {
            0 => Tags::Array(r_str16_array(p)?.ok_or_else(|| {
                crate::refuse("android.media.AudioAttributes.mTags: null array (Java NPE)")
            })?),
            FLATTEN_TAGS => Tags::Flattened(r_str16(p)?.ok_or_else(|| {
                crate::refuse("android.media.AudioAttributes.mFormattedTags: null (Java NPE)")
            })?),
            _ => {
                return Err(crate::refuse(
                    "android.media.AudioAttributes.parcel flags: not 0 or FLATTEN_TAGS",
                ));
            }
        };
        let bundle = match r_i32(p)? {
            ATTR_PARCEL_IS_NULL_BUNDLE => None,
            ATTR_PARCEL_IS_VALID_BUNDLE => Some(r_bundle(p)?.ok_or_else(|| {
                crate::refuse("android.media.AudioAttributes.mBundle: null body (Java NPE)")
            })?),
            _ => {
                return Err(crate::refuse(
                    "android.media.AudioAttributes.mBundle: unknown marker (Java logs it and goes on)",
                ));
            }
        };
        Ok(AudioAttributes {
            usage,
            content_type,
            source,
            flags,
            tags,
            bundle,
        })
    }
}

crate::codec!(AudioAttributes);
