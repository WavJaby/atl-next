// SPDX-License-Identifier: MIT
//! android.content.pm.ActivityInfo: content/pm/ActivityInfo.java writeToParcel / ActivityInfo(Parcel), after ComponentInfo. windowLayout (`<layout>`) is ActivityInfo.WindowLayout's writeToParcel behind a presence int.

use crate::parcel::*;
use crate::parts::*;

/// ActivityInfo.WindowLayout (writeToParcel / WindowLayout(Parcel)): the manifest's `<layout>` element.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct WindowLayout {
    pub width: i32,
    pub width_fraction: f32,
    pub height: i32,
    pub height_fraction: f32,
    pub gravity: i32,
    pub min_width: i32,
    pub min_height: i32,
    pub window_layout_affinity: Option<String>,
}

impl Part for WindowLayout {
    fn write(&self, p: &mut In<'_>) -> Result<()> {
        w_i32(p, self.width)?;
        w_f32(p, self.width_fraction)?;
        w_i32(p, self.height)?;
        w_f32(p, self.height_fraction)?;
        w_i32(p, self.gravity)?;
        w_i32(p, self.min_width)?;
        w_i32(p, self.min_height)?;
        w_str8(p, &self.window_layout_affinity)
    }

    fn read(p: &In<'_>) -> Result<Self> {
        Ok(WindowLayout {
            width: r_i32(p)?,
            width_fraction: r_f32(p)?,
            height: r_i32(p)?,
            height_fraction: r_f32(p)?,
            gravity: r_i32(p)?,
            min_width: r_i32(p)?,
            min_height: r_i32(p)?,
            window_layout_affinity: r_str8(p)?,
        })
    }
}

#[derive(Debug, Clone, Default)]
pub struct ActivityInfo {
    pub component: ComponentInfo,
    pub theme: i32,
    pub launch_mode: i32,
    pub document_launch_mode: i32,
    pub permission: Option<String>,
    pub task_affinity: Option<String>,
    pub target_activity: Option<String>,
    pub launch_token: Option<String>,
    pub flags: i32,
    pub private_flags: i32,
    pub screen_orientation: i32,
    pub config_changes: i32,
    pub soft_input_mode: i32,
    pub ui_options: i32,
    pub parent_activity_name: Option<String>,
    pub persistable_mode: i32,
    pub max_recents: i32,
    pub lock_task_launch_mode: i32,
    pub window_layout: Option<WindowLayout>,
    pub resize_mode: i32,
    pub requested_vr_component: Option<String>,
    pub rotation_animation: i32,
    pub color_mode: i32,
    pub max_aspect_ratio: f32,
    pub min_aspect_ratio: f32,
    pub supports_size_changes: bool,
    /// mKnownActivityEmbeddingCerts; empty = null
    pub known_activity_embedding_certs: Vec<Option<String>>,
    pub required_display_category: Option<String>,
    pub require_content_uri_permission_from_caller: i32,
}

impl Part for ActivityInfo {
    fn write(&self, p: &mut In<'_>) -> Result<()> {
        self.component.write(p)?;
        w_i32(p, self.theme)?;
        w_i32(p, self.launch_mode)?;
        w_i32(p, self.document_launch_mode)?;
        w_str8(p, &self.permission)?;
        w_str8(p, &self.task_affinity)?;
        w_str8(p, &self.target_activity)?;
        w_str8(p, &self.launch_token)?;
        w_i32(p, self.flags)?;
        w_i32(p, self.private_flags)?;
        w_i32(p, self.screen_orientation)?;
        w_i32(p, self.config_changes)?;
        w_i32(p, self.soft_input_mode)?;
        w_i32(p, self.ui_options)?;
        w_str8(p, &self.parent_activity_name)?;
        w_i32(p, self.persistable_mode)?;
        w_i32(p, self.max_recents)?;
        w_i32(p, self.lock_task_launch_mode)?;
        match &self.window_layout {
            Some(l) => {
                w_i32(p, 1)?;
                l.write(p)?;
            }
            None => w_i32(p, 0)?,
        }
        w_i32(p, self.resize_mode)?;
        w_str8(p, &self.requested_vr_component)?;
        w_i32(p, self.rotation_animation)?;
        w_i32(p, self.color_mode)?;
        w_f32(p, self.max_aspect_ratio)?;
        w_f32(p, self.min_aspect_ratio)?;
        w_bool(p, self.supports_size_changes)?;
        w_string_set(p, &self.known_activity_embedding_certs)?;
        w_str8(p, &self.required_display_category)?;
        w_i32(p, self.require_content_uri_permission_from_caller)
    }

    fn read(p: &In<'_>) -> Result<Self> {
        let component = ComponentInfo::read(p)?;
        let theme = r_i32(p)?;
        let launch_mode = r_i32(p)?;
        let document_launch_mode = r_i32(p)?;
        let permission = r_str8(p)?;
        let task_affinity = r_str8(p)?;
        let target_activity = r_str8(p)?;
        let launch_token = r_str8(p)?;
        let flags = r_i32(p)?;
        let private_flags = r_i32(p)?;
        let screen_orientation = r_i32(p)?;
        let config_changes = r_i32(p)?;
        let soft_input_mode = r_i32(p)?;
        let ui_options = r_i32(p)?;
        let parent_activity_name = r_str8(p)?;
        let persistable_mode = r_i32(p)?;
        let max_recents = r_i32(p)?;
        let lock_task_launch_mode = r_i32(p)?;
        // ActivityInfo(Parcel) reads a WindowLayout only for exactly 1
        let window_layout = if r_i32(p)? == 1 {
            Some(WindowLayout::read(p)?)
        } else {
            None
        };
        Ok(ActivityInfo {
            window_layout,
            component,
            theme,
            launch_mode,
            document_launch_mode,
            permission,
            task_affinity,
            target_activity,
            launch_token,
            flags,
            private_flags,
            screen_orientation,
            config_changes,
            soft_input_mode,
            ui_options,
            parent_activity_name,
            persistable_mode,
            max_recents,
            lock_task_launch_mode,
            resize_mode: r_i32(p)?,
            requested_vr_component: r_str8(p)?,
            rotation_animation: r_i32(p)?,
            color_mode: r_i32(p)?,
            max_aspect_ratio: r_f32(p)?,
            min_aspect_ratio: r_f32(p)?,
            supports_size_changes: r_bool(p)?,
            known_activity_embedding_certs: r_string_set(p)?,
            required_display_category: r_str8(p)?,
            require_content_uri_permission_from_caller: r_i32(p)?,
        })
    }
}

crate::codec!(ActivityInfo);
