// SPDX-License-Identifier: MIT
//! android.window.ClientWindowFrames (frameworks/base/core/java/android/window/ClientWindowFrames.java writeToParcel:97-106, readFromParcel:86-95): the frames of a window the
//! client lays out with, part of WindowRelayoutResult. Order: frame, displayFrame, parentFrame (Rects), attachedFrame (writeTypedObject), isParentFrameClippedByDisplayCutout,
//! compatScale, seq. Every field is modelled.

use crate::parcel::*;
use crate::parts::*;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ClientWindowFrames {
    pub frame: Rect,
    pub display_frame: Rect,
    pub parent_frame: Rect,
    pub attached_frame: Option<Rect>,
    pub is_parent_frame_clipped_by_display_cutout: bool,
    pub compat_scale: f32,
    pub seq: i32,
}

impl Part for ClientWindowFrames {
    fn write(&self, p: &mut In<'_>) -> Result<()> {
        self.frame.write(p)?;
        self.display_frame.write(p)?;
        self.parent_frame.write(p)?;
        w_typed(p, &self.attached_frame)?;
        w_bool(p, self.is_parent_frame_clipped_by_display_cutout)?;
        w_f32(p, self.compat_scale)?;
        w_i32(p, self.seq)
    }

    fn read(p: &In<'_>) -> Result<Self> {
        Ok(ClientWindowFrames {
            frame: Rect::read(p)?,
            display_frame: Rect::read(p)?,
            parent_frame: Rect::read(p)?,
            attached_frame: r_typed(p)?,
            is_parent_frame_clipped_by_display_cutout: r_bool(p)?,
            compat_scale: r_f32(p)?,
            seq: r_i32(p)?,
        })
    }
}

crate::codec!(ClientWindowFrames);
