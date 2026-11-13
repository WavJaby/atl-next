// SPDX-License-Identifier: MIT
//! android.app.NotificationChannel (app/NotificationChannel.java writeToParcel :439-506, NotificationChannel(Parcel) :357-437), field for field. `writeByte` is writeInt, so the presence bytes and the
//! byte booleans are int32 words. Reading trims as the Java reader does: the strings and the sound's string to MAX_TEXT_LENGTH UTF-16 units, the vibration pattern to MAX_VIBRATION_LENGTH.
//! The trailing VibrationEffect slot exists because `Flags.notificationChannelVibrationEffectApi()` is a READ_ONLY true flag in every release config (build/release/aconfig/*/android.app); VibrationEffect
//! has no codec, so a non-null one (setVibrationPattern / setVibrationEffect make one) is refused and counted, and only null is written.

use crate::codecs::android::media::AudioAttributes::AudioAttributes;
use crate::codecs::android::net::Uri::Uri;
use crate::parcel::*;
use crate::parts::*;

/// NotificationChannel.MAX_TEXT_LENGTH (:182): longer strings are cut when read.
const MAX_TEXT_LENGTH: usize = 1000;
/// NotificationChannel.MAX_VIBRATION_LENGTH (:186): longer patterns are cut when read.
const MAX_VIBRATION_LENGTH: usize = 500;

/// NotificationChannel's parcelled fields (the `m` prefix dropped). No `PartialEq`: AudioAttributes has none.
#[derive(Debug, Clone, Default)]
pub struct NotificationChannel {
    pub id: Option<String>,
    pub name: Option<String>,
    pub desc: Option<String>,
    pub importance: i32,
    pub bypass_dnd: bool,
    pub lockscreen_visibility: i32,
    pub sound: Option<Uri>,
    pub lights: bool,
    pub user_locked_fields: i32,
    pub user_visible_task_shown: bool,
    pub vibration_enabled: bool,
    pub show_badge: bool,
    pub deleted: bool,
    pub group: Option<String>,
    pub audio_attributes: Option<AudioAttributes>,
    pub light_color: i32,
    pub blockable_system: bool,
    pub allow_bubbles: i32,
    pub original_importance: i32,
    pub parent_id: Option<String>,
    pub conversation_id: Option<String>,
    pub demoted: bool,
    pub important_convo: bool,
    pub deleted_time: i64,
    pub importance_locked_default_app: bool,
    pub vibration_pattern: Option<Vec<i64>>,
}

/// getTrimmedString: Java's `substring(0, MAX_TEXT_LENGTH)` counts UTF-16 units; a surrogate pair across the cut is dropped whole here, where Java would keep its lone high half.
fn trimmed(s: Option<String>) -> Option<String> {
    s.map(|s| {
        let mut units = 0;
        let end = s
            .char_indices()
            .find(|(_, c)| {
                units += c.len_utf16();
                units > MAX_TEXT_LENGTH
            })
            .map_or(s.len(), |(i, _)| i);
        s[..end].to_owned()
    })
}

/// writeByte(1) + writeString, or writeByte(0) for null.
fn w_flagged_str(p: &mut In<'_>, v: &Option<String>) -> Result<()> {
    w_present(p, v.is_some())?;
    match v {
        Some(_) => w_str16(p, v),
        None => Ok(()),
    }
}
fn r_flagged_str(p: &In<'_>) -> Result<Option<String>> {
    if r_present(p)? {
        r_str16(p).map(trimmed)
    } else {
        Ok(None)
    }
}

impl Part for NotificationChannel {
    fn write(&self, p: &mut In<'_>) -> Result<()> {
        w_flagged_str(p, &self.id)?;
        w_flagged_str(p, &self.name)?;
        w_flagged_str(p, &self.desc)?;
        w_i32(p, self.importance)?;
        w_bool(p, self.bypass_dnd)?;
        w_i32(p, self.lockscreen_visibility)?;
        w_present(p, self.sound.is_some())?;
        if let Some(sound) = &self.sound {
            sound.write(p)?;
        }
        w_bool(p, self.lights)?;
        w_i32(p, self.user_locked_fields)?;
        w_bool(p, self.user_visible_task_shown)?;
        w_bool(p, self.vibration_enabled)?;
        w_bool(p, self.show_badge)?;
        w_bool(p, self.deleted)?;
        w_flagged_str(p, &self.group)?;
        w_typed(p, &self.audio_attributes)?;
        w_i32(p, self.light_color)?;
        w_bool(p, self.blockable_system)?;
        w_i32(p, self.allow_bubbles)?;
        w_i32(p, self.original_importance)?;
        w_str16(p, &self.parent_id)?;
        w_str16(p, &self.conversation_id)?;
        w_bool(p, self.demoted)?;
        w_bool(p, self.important_convo)?;
        w_i64(p, self.deleted_time)?;
        w_bool(p, self.importance_locked_default_app)?;
        w_i64_array(p, &self.vibration_pattern)?;
        // the VibrationEffect slot: only null is modelled
        w_present(p, false)
    }

    fn read(p: &In<'_>) -> Result<Self> {
        let id = r_flagged_str(p)?;
        let name = r_flagged_str(p)?;
        let desc = r_flagged_str(p)?;
        let importance = r_i32(p)?;
        let bypass_dnd = r_bool(p)?;
        let lockscreen_visibility = r_i32(p)?;
        // Java re-parses the sound from its trimmed string (Uri.parse), which keeps the string
        let sound = if r_present(p)? {
            Some(Uri(trimmed(Some(Uri::read(p)?.0)).unwrap_or_default()))
        } else {
            None
        };
        let channel = NotificationChannel {
            id,
            name,
            desc,
            importance,
            bypass_dnd,
            lockscreen_visibility,
            sound,
            lights: r_bool(p)?,
            user_locked_fields: r_i32(p)?,
            user_visible_task_shown: r_bool(p)?,
            vibration_enabled: r_bool(p)?,
            show_badge: r_bool(p)?,
            deleted: r_bool(p)?,
            group: r_flagged_str(p)?,
            audio_attributes: r_typed(p)?,
            light_color: r_i32(p)?,
            blockable_system: r_bool(p)?,
            allow_bubbles: r_i32(p)?,
            original_importance: r_i32(p)?,
            parent_id: r_str16(p).map(trimmed)?,
            conversation_id: r_str16(p).map(trimmed)?,
            demoted: r_bool(p)?,
            important_convo: r_bool(p)?,
            deleted_time: r_i64(p)?,
            importance_locked_default_app: r_bool(p)?,
            vibration_pattern: r_i64_array(p)?.map(|mut v| {
                v.truncate(MAX_VIBRATION_LENGTH);
                v
            }),
        };
        if r_present(p)? {
            return Err(crate::refuse(
                "android.app.NotificationChannel (VibrationEffect)",
            ));
        }
        Ok(channel)
    }
}

crate::codec!(NotificationChannel);
