// SPDX-License-Identifier: MIT
//! android.view.WindowRelayoutResult (frameworks/base/core/java/android/view/WindowRelayoutResult.java writeToParcel:112-119, readFromParcel:101-110): the out parameter of
//! IWindowSession.relayout. Order: frames, mergedConfiguration, surfaceControl, insetsState, activeControls, syncSeqId, activityWindowInfo (writeTypedObject).
//! Every field is modelled; the SurfaceControl and the activity window info are modelled as the codecs of those types model them.

use crate::codecs::android::util::MergedConfiguration::MergedConfiguration;
use crate::codecs::android::view::InsetsSourceControl_Array::InsetsSourceControl_Array;
use crate::codecs::android::view::InsetsState::InsetsState;
use crate::codecs::android::view::SurfaceControl::SurfaceControl;
use crate::codecs::android::window::ClientWindowFrames::ClientWindowFrames;
use crate::parcel::*;
use crate::parts::*;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct WindowRelayoutResult {
    pub frames: ClientWindowFrames,
    pub merged_configuration: MergedConfiguration,
    pub surface_control: SurfaceControl,
    pub insets_state: InsetsState,
    pub active_controls: InsetsSourceControl_Array,
    pub sync_seq_id: i32,
    pub activity_window_info: Option<ActivityWindowInfo>,
}

impl Part for WindowRelayoutResult {
    fn write(&self, p: &mut In<'_>) -> Result<()> {
        self.frames.write(p)?;
        self.merged_configuration.write(p)?;
        self.surface_control.write(p)?;
        self.insets_state.write(p)?;
        self.active_controls.write(p)?;
        w_i32(p, self.sync_seq_id)?;
        w_typed(p, &self.activity_window_info)
    }

    fn read(p: &In<'_>) -> Result<Self> {
        Ok(WindowRelayoutResult {
            frames: ClientWindowFrames::read(p)?,
            merged_configuration: MergedConfiguration::read(p)?,
            surface_control: SurfaceControl::read(p)?,
            insets_state: InsetsState::read(p)?,
            active_controls: InsetsSourceControl_Array::read(p)?,
            sync_seq_id: r_i32(p)?,
            activity_window_info: r_typed(p)?,
        })
    }
}

crate::codec!(WindowRelayoutResult);
