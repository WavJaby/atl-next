// SPDX-License-Identifier: MIT
//! android.os.Bundle: os/Bundle.java writeToParcel -> BaseBundle.writeToParcelInner (BaseBundle.java:1838-1882) / Bundle(Parcel) -> BaseBundle.readFromParcelInner (BaseBundle.java:1889-1945).
//! Wire: int32 length; 0 is the empty Bundle and nothing more. Else int32 magic (BUNDLE_MAGIC 'BNDL', or BUNDLE_MAGIC_NATIVE 'BNDN' when native code parcelled it, BaseBundle.java:60-61), `length` bytes of body (the ArrayMap of entries), int32 mHasIntent (writeBoolean: 0 or 1).
//! The body is a blob, copied verbatim and never parsed. A body that holds a binder or file descriptor is refused and counted: the object would be lost from the bytes. The copy is made as a parcel and marshalled, and Parcel::marshal refuses objects (patches/aosp/frameworks/native/0004).
//! AMS refuses the same in instrumentation results: ActivityManagerService.java:14917-14923 (finishInstrumentation, results.hasFileDescriptors()).

use binder::StatusCode;
use binder::binder_impl::Parcel;

use crate::parcel::*;
use crate::parts::*;

/// BaseBundle.BUNDLE_MAGIC and BUNDLE_MAGIC_NATIVE (BaseBundle.java:60-61): the reader takes either, the writer writes back the one it read.
const BUNDLE_MAGIC: i32 = 0x4C44_4E42;
const BUNDLE_MAGIC_NATIVE: i32 = 0x4C44_4E44;

/// android.os.Bundle as its blob. `Default` is the empty Bundle: length 0 and nothing else.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Bundle {
    /// BUNDLE_MAGIC or BUNDLE_MAGIC_NATIVE; 0 when the body is empty.
    pub magic: i32,
    /// The map's bytes, verbatim; a multiple of 4; empty = the empty Bundle.
    pub body: Vec<u8>,
    /// mHasIntent. Java writes 0 or 1 only, so the codec refuses any other value.
    pub has_intent: bool,
}

impl Part for Bundle {
    fn write(&self, p: &mut In<'_>) -> Result<()> {
        if self.body.is_empty() {
            return w_i32(p, 0);
        }
        if !matches!(self.magic, BUNDLE_MAGIC | BUNDLE_MAGIC_NATIVE)
            || !self.body.len().is_multiple_of(4)
        {
            return Err(StatusCode::BAD_VALUE);
        }
        w_i32(p, len_i32(self.body.len())?)?;
        w_i32(p, self.magic)?;
        for w in self.body.as_chunks::<4>().0 {
            w_i32(p, i32::from_ne_bytes(*w))?;
        }
        w_bool(p, self.has_intent)
    }

    fn read(p: &In<'_>) -> Result<Self> {
        let length = r_i32(p)?;
        if length == 0 {
            return Ok(Bundle::default());
        }
        if length < 0 || length % 4 != 0 {
            return Err(crate::refuse("android.os.Bundle (length)"));
        }
        let magic = r_i32(p)?;
        if magic != BUNDLE_MAGIC && magic != BUNDLE_MAGIC_NATIVE {
            return Err(crate::refuse("android.os.Bundle (magic)"));
        }
        let body = read_body(p, length)?;
        let has_intent = match r_i32(p)? {
            0 => false,
            1 => true,
            _ => return Err(crate::refuse("android.os.Bundle.mHasIntent")),
        };
        Ok(Bundle {
            magic,
            body,
            has_intent,
        })
    }
}

/// The body's `length` bytes, copied out and skipped. The copy is a parcel, marshalled: a binder or file descriptor in the range is in its object table, and marshal refuses it (counted).
fn read_body(p: &In<'_>, length: i32) -> Result<Vec<u8>> {
    let start = p.get_data_position();
    let mut copy = Parcel::new();
    copy.append_from(p, start, length)?;
    let body = copy.marshal().map_err(|e| {
        if e == StatusCode::INVALID_OPERATION {
            crate::refuse("android.os.Bundle (file descriptor or binder)")
        } else {
            e
        }
    })?;
    p.set_data_position_checked(start + length)?;
    Ok(body)
}

crate::codec!(Bundle);
