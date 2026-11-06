// SPDX-License-Identifier: MIT
//! android.hardware.OverlayProperties (frameworks/base/core/java/android/hardware/OverlayProperties.java writeToParcel / CREATOR), the record IDisplayManager.getOverlaySupport returns.
//! Java writes int 1 then the native gui::OverlayProperties (android_hardware_OverlayProperties.cpp nWriteOverlayPropertiesToParcel -> writeToParcel), a structured AIDL parcelable
//! (frameworks/native/libs/gui/aidl/android/gui/OverlayProperties.aidl): int32 size header, `SupportedBufferCombinations[] combinations` (count, then per element the non-null flag 1 and
//! its own sized record of four int arrays), `boolean supportMixedColorSpaces`, `@nullable LutProperties[] lutProperties`. Lut properties are modelled only null (-1); a non-null array is
//! refused. A Java object without a native one writes 0 and reads back as null: the codec refuses it (the caller's NULL flag carries null).

use binder::Parcelable;
use binder::binder_impl::BorrowedParcel;

use crate::parcel::*;
use crate::parts::Part;
use crate::refuse;

/// gui::OverlayProperties::SupportedBufferCombinations: the pixel formats (HAL_PIXEL_FORMAT_*) and dataspace standard / transfer / range parts (HAL_DATASPACE_*) composed as a hardware overlay.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SupportedBufferCombinations {
    pub pixel_formats: Vec<i32>,
    pub standards: Vec<i32>,
    pub transfers: Vec<i32>,
    pub ranges: Vec<i32>,
}

impl Parcelable for SupportedBufferCombinations {
    fn write_to_parcel(&self, p: &mut BorrowedParcel<'_>) -> Result<()> {
        p.sized_write(|s| {
            s.write(&self.pixel_formats)?;
            s.write(&self.standards)?;
            s.write(&self.transfers)?;
            s.write(&self.ranges)
        })
    }

    fn read_from_parcel(&mut self, p: &BorrowedParcel<'_>) -> Result<()> {
        p.sized_read(|s| {
            self.pixel_formats = s.read()?;
            self.standards = s.read()?;
            self.transfers = s.read()?;
            self.ranges = s.read()?;
            // fields this record does not model (a newer AIDL revision) would be skipped silently by sized_read: refuse instead
            if s.has_more_data() {
                return Err(refuse("android.gui.OverlayProperties.SupportedBufferCombinations: unmodelled trailing fields"));
            }
            Ok(())
        })
    }
}

binder::impl_serialize_for_parcelable!(SupportedBufferCombinations);
binder::impl_deserialize_for_parcelable!(SupportedBufferCombinations);

/// The non-null OverlayProperties: its native object's fields.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct OverlayProperties {
    pub combinations: Vec<SupportedBufferCombinations>,
    pub support_mixed_color_spaces: bool,
}

/// The -1 of a null array (Parcel::writeData of an empty optional vector: kNullVectorSize).
const NULL_ARRAY: i32 = -1;

impl Part for OverlayProperties {
    fn write(&self, p: &mut In<'_>) -> Result<()> {
        w_present(p, true)?; // Java's own non-null flag: mNativeObject != 0
        p.sized_write(|s| {
            s.write(&self.combinations)?;
            s.write(&self.support_mixed_color_spaces)?;
            s.write(&NULL_ARRAY) // lutProperties
        })
    }

    fn read(p: &In<'_>) -> Result<Self> {
        if !r_present(p)? {
            return Err(refuse(
                "android.hardware.OverlayProperties without a native object",
            ));
        }
        let mut v = Self::default();
        p.sized_read(|s| {
            v.combinations = s.read()?;
            v.support_mixed_color_spaces = s.read()?;
            let luts: i32 = s.read()?;
            if luts != NULL_ARRAY {
                return Err(refuse("android.gui.OverlayProperties.lutProperties"));
            }
            if s.has_more_data() {
                return Err(refuse(
                    "android.gui.OverlayProperties: unmodelled trailing fields",
                ));
            }
            Ok(())
        })?;
        Ok(v)
    }
}

crate::codec!(OverlayProperties);
