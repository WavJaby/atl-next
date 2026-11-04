// SPDX-License-Identifier: MIT
//! android.view.InsetsState (frameworks/base/core/java/android/view/InsetsState.java writeToParcel:799-815, readFromParcel:825-845): the insets of a window's display, the reply of
//! addToDisplayAsUser and relayout. Order: displayFrame (Rect), displayCutout (ParcelableWrapper, writeCutoutToParcel), roundedCorners (writeTypedObject), roundedCornerFrame (Rect),
//! privacyIndicatorBounds (writeTypedObject), displayShape (writeTypedObject), seq, then the sources as a count and writeTypedObject each.
//! Modelled: displayFrame, roundedCornerFrame, seq, sources. The other four fields are modelled at the value `new InsetsState()` holds (field initialisers, InsetsState.java:78-99), which
//! is what a window gets: NO_CUTOUT (writeInt 0), NO_ROUNDED_CORNERS (presence 1, then its flag 0), `new PrivacyIndicatorBounds()` (presence 1, four null Rects, rotation 0), DisplayShape.NONE
//! (presence 1, spec "" as String8, then width, height, ratio, rotation, offsetX, offsetY, scale 1f). Any other value, null included, is refused by name.

use crate::parcel::*;
use crate::parts::*;

/// DisplayCutout.ParcelableWrapper of NO_CUTOUT: writeCutoutToParcel writes 0 (null would be -1, a cutout 1 and its geometry).
const NO_CUTOUT: i32 = 0;
/// PrivacyIndicatorBounds() allocates `new Rect[4]` for mStaticBounds, all null, and ROTATION_0 is 0.
const STATIC_BOUNDS: i32 = 4;
const ROTATION_0: i32 = 0;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct InsetsState {
    pub display_frame: Rect,
    pub rounded_corner_frame: Rect,
    pub seq: i32,
    /// In the Java SparseArray's order (ascending id), as written.
    pub sources: Vec<InsetsSource>,
}

impl Part for InsetsState {
    fn write(&self, p: &mut In<'_>) -> Result<()> {
        self.display_frame.write(p)?;
        w_i32(p, NO_CUTOUT)?;
        w_default_rounded_corners(p)?;
        self.rounded_corner_frame.write(p)?;
        w_default_privacy_bounds(p)?;
        w_default_display_shape(p)?;
        w_i32(p, self.seq)?;
        w_i32(p, len_i32(self.sources.len())?)?;
        self.sources.iter().try_for_each(|s| {
            w_present(p, true)?;
            s.write(p)
        })
    }

    fn read(p: &In<'_>) -> Result<Self> {
        let display_frame = Rect::read(p)?;
        r_want(p, NO_CUTOUT, "android.view.DisplayCutout")?;
        r_default_rounded_corners(p)?;
        let rounded_corner_frame = Rect::read(p)?;
        r_default_privacy_bounds(p)?;
        r_default_display_shape(p)?;
        let seq = r_i32(p)?;
        let n = r_i32(p)?;
        let n = usize::try_from(n).map_err(|_| binder::StatusCode::BAD_VALUE)?;
        let sources = (0..n)
            .map(|_| {
                if !r_present(p)? {
                    return Err(binder::StatusCode::BAD_VALUE);
                }
                InsetsSource::read(p)
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(InsetsState {
            display_frame,
            rounded_corner_frame,
            seq,
            sources,
        })
    }
}

/// Reads an int that must be `want`, the one value of a field this codec models; any other is refused as `what`.
fn r_want(p: &In<'_>, want: i32, what: &'static str) -> Result<()> {
    if r_i32(p)? == want {
        Ok(())
    } else {
        Err(crate::refuse(what))
    }
}

/// As `r_want` for a float, compared by bits.
fn r_want_f32(p: &In<'_>, want: f32, what: &'static str) -> Result<()> {
    if r_f32(p)?.to_bits() == want.to_bits() {
        Ok(())
    } else {
        Err(crate::refuse(what))
    }
}

/// writeTypedObject(NO_ROUNDED_CORNERS): presence 1, then RoundedCorners.writeToParcel's flag 0 (equal to NO_ROUNDED_CORNERS, so no corners follow).
fn w_default_rounded_corners(p: &mut In<'_>) -> Result<()> {
    w_present(p, true)?;
    w_i32(p, 0)
}
fn r_default_rounded_corners(p: &In<'_>) -> Result<()> {
    if !r_present(p)? {
        return Err(crate::refuse("android.view.RoundedCorners"));
    }
    r_want(p, 0, "android.view.RoundedCorners")
}

/// writeTypedObject(new PrivacyIndicatorBounds()): presence 1, writeTypedArray of four null Rects (count 4, then a presence 0 each), then the rotation.
fn w_default_privacy_bounds(p: &mut In<'_>) -> Result<()> {
    w_present(p, true)?;
    w_i32(p, STATIC_BOUNDS)?;
    (0..STATIC_BOUNDS).try_for_each(|_| w_present(p, false))?;
    w_i32(p, ROTATION_0)
}
fn r_default_privacy_bounds(p: &In<'_>) -> Result<()> {
    if !r_present(p)? {
        return Err(crate::refuse("android.view.PrivacyIndicatorBounds"));
    }
    r_want(p, STATIC_BOUNDS, "android.view.PrivacyIndicatorBounds")?;
    for _ in 0..STATIC_BOUNDS {
        if r_present(p)? {
            return Err(crate::refuse(
                "android.view.PrivacyIndicatorBounds (static bounds)",
            ));
        }
    }
    r_want(
        p,
        ROTATION_0,
        "android.view.PrivacyIndicatorBounds (rotation)",
    )
}

/// writeTypedObject(DisplayShape.NONE): presence 1, then writeString8 of "" (length 0 and its padding word), width 0, height 0, ratio 0f, rotation 0, offsetX 0, offsetY 0, scale 1f.
fn w_default_display_shape(p: &mut In<'_>) -> Result<()> {
    w_present(p, true)?;
    w_str8(p, &Some(String::new()))?;
    w_i32(p, 0)?;
    w_i32(p, 0)?;
    w_f32(p, 0.0)?;
    w_i32(p, ROTATION_0)?;
    w_i32(p, 0)?;
    w_i32(p, 0)?;
    w_f32(p, 1.0)
}
fn r_default_display_shape(p: &In<'_>) -> Result<()> {
    const WHAT: &str = "android.view.DisplayShape";
    if !r_present(p)? {
        return Err(crate::refuse(WHAT));
    }
    if r_str8(p)? != Some(String::new()) {
        return Err(crate::refuse(WHAT));
    }
    r_want(p, 0, WHAT)?;
    r_want(p, 0, WHAT)?;
    r_want_f32(p, 0.0, WHAT)?;
    r_want(p, ROTATION_0, WHAT)?;
    r_want(p, 0, WHAT)?;
    r_want(p, 0, WHAT)?;
    r_want_f32(p, 1.0, WHAT)
}

crate::codec!(InsetsState);
