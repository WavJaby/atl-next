// SPDX-License-Identifier: MIT
//! android.view.InsetsState (frameworks/base/core/java/android/view/InsetsState.java writeToParcel:799-815, readFromParcel:825-845): the insets of a window's display, the reply of
//! addToDisplayAsUser and relayout. Order: displayFrame (Rect), displayCutout (ParcelableWrapper, writeCutoutToParcel), roundedCorners (presence), roundedCornerFrame (Rect),
//! privacyIndicatorBounds (presence), displayShape (presence), seq, then the sources as a count and writeTypedObject each.
//! Modelled: displayFrame, roundedCornerFrame, seq, sources. Null only (refused otherwise): displayCutout, roundedCorners, privacyIndicatorBounds, displayShape.

use crate::parcel::*;
use crate::parts::*;

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
        // DisplayCutout.ParcelableWrapper of null (writeCutoutToParcel(null): -1)
        w_null(p)?;
        // roundedCorners, privacyIndicatorBounds, displayShape: writeTypedObject(null)
        w_present(p, false)?;
        self.rounded_corner_frame.write(p)?;
        w_present(p, false)?;
        w_present(p, false)?;
        w_i32(p, self.seq)?;
        w_i32(p, len_i32(self.sources.len())?)?;
        self.sources.iter().try_for_each(|s| {
            w_present(p, true)?;
            s.write(p)
        })
    }

    fn read(p: &In<'_>) -> Result<Self> {
        let display_frame = Rect::read(p)?;
        r_null(p, "android.view.DisplayCutout")?;
        r_absent(p, "android.view.RoundedCorners")?;
        let rounded_corner_frame = Rect::read(p)?;
        r_absent(p, "android.view.PrivacyIndicatorBounds")?;
        r_absent(p, "android.view.DisplayShape")?;
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

crate::codec!(InsetsState);
