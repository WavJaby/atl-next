// SPDX-License-Identifier: MIT
//! The notification channels apps create, as NotificationManagerService keeps them for an app caller: PreferencesHelper.createNotificationChannel (:1076-1225, fromTargetApp, no DND access),
//! getConversationNotificationChannel (no conversation id) and getNotificationChannels (:1490-1515). Per (uid, package), in the ArrayMap order getNotificationChannels lists them: by the id's Java hashCode,
//! equal hashes in insertion order. Nothing is displayed (D4: toasts only); the store answers what an app reads back.
//! Not modelled: channel groups (none exist, so a channel naming one is refused as AOSP refuses an unknown group), deletion and user edits (userLockedFields stay 0), pre-O apps' default
//! `miscellaneous` channel, notification classification's SYSTEM_RESERVED_IDS, delegates, persistence across atld runs (AOSP keeps the policy file).

use std::collections::HashMap;

use atl_parcelables::android::app::NotificationChannel;
use binder::{ExceptionCode, Status};

/// NotificationChannel.DEFAULT_CHANNEL_ID (:82): reserved for pre-O apps' default channel.
const DEFAULT_CHANNEL_ID: &str = "miscellaneous";
/// NotificationManager.IMPORTANCE_NONE / IMPORTANCE_MAX / IMPORTANCE_UNSPECIFIED (:593, :626, :588).
const IMPORTANCE_NONE: i32 = 0;
const IMPORTANCE_MAX: i32 = 5;
const IMPORTANCE_UNSPECIFIED: i32 = -1000;
/// NotificationManager.VISIBILITY_NO_OVERRIDE (:580): PackagePreferences.visibility's default (PreferencesHelper DEFAULT_VISIBILITY :179), which a new app channel takes.
const VISIBILITY_NO_OVERRIDE: i32 = -1000;
/// NotificationChannel.DEFAULT_ALLOW_BUBBLE (:280).
const DEFAULT_ALLOW_BUBBLE: i32 = -1;
/// PreferencesHelper.NOTIFICATION_CHANNEL_COUNT_LIMIT (:138).
const COUNT_LIMIT: usize = 5000;

/// Why a channel was not created: the exception AOSP throws, with its message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelError {
    /// Objects.requireNonNull(channel.getId())
    NoId,
    /// Preconditions.checkArgument(!TextUtils.isEmpty(channel.getName()))
    NoName,
    Importance,
    UnknownGroup,
    ReservedId,
    NoParent,
    Limit,
}

impl ChannelError {
    /// The Java exception: NullPointerException, IllegalArgumentException or IllegalStateException, with AOSP's message (none where Preconditions gives none).
    pub fn status(self) -> Status {
        let (code, msg) = match self {
            ChannelError::NoId => (ExceptionCode::NULL_POINTER, None),
            ChannelError::NoName => (ExceptionCode::ILLEGAL_ARGUMENT, None),
            ChannelError::Importance => (
                ExceptionCode::ILLEGAL_ARGUMENT,
                Some("Invalid importance level"),
            ),
            ChannelError::UnknownGroup => (
                ExceptionCode::ILLEGAL_ARGUMENT,
                Some("NotificationChannelGroup doesn't exist"),
            ),
            ChannelError::ReservedId => (ExceptionCode::ILLEGAL_ARGUMENT, Some("Reserved id")),
            ChannelError::NoParent => (
                ExceptionCode::ILLEGAL_ARGUMENT,
                Some("Tried to create a conversation channel without a preexisting parent"),
            ),
            ChannelError::Limit => (
                ExceptionCode::ILLEGAL_STATE,
                Some("Limit exceed; cannot create more channels"),
            ),
        };
        Status::new_exception_str(code, msg)
    }
}

/// Java's String.hashCode over UTF-16 units: ArrayMap's key order.
fn java_hash(s: &str) -> i32 {
    s.encode_utf16()
        .fold(0i32, |h, u| h.wrapping_mul(31).wrapping_add(i32::from(u)))
}

/// Every app's channels, keyed by (uid, package).
#[derive(Default)]
pub struct Channels {
    by_app: HashMap<(u32, String), Vec<NotificationChannel>>,
}

impl Channels {
    /// createNotificationChannel with fromTargetApp: a new id is stored with the fields an app may not set reset; a known id takes the app's name, description, blockability, a first group,
    /// an importance downgrade and a first original importance.
    pub fn create(
        &mut self,
        uid: u32,
        pkg: &str,
        mut channel: NotificationChannel,
    ) -> Result<(), ChannelError> {
        let id = channel.id.clone().ok_or(ChannelError::NoId)?;
        if channel.name.as_deref().is_none_or(str::is_empty) {
            return Err(ChannelError::NoName);
        }
        if !(IMPORTANCE_NONE..=IMPORTANCE_MAX).contains(&channel.importance) {
            return Err(ChannelError::Importance);
        }
        if channel.group.is_some() {
            return Err(ChannelError::UnknownGroup);
        }
        if id == DEFAULT_CHANNEL_ID {
            return Err(ChannelError::ReservedId);
        }
        let list = self.by_app.entry((uid, pkg.to_owned())).or_default();
        if let Some(existing) = list
            .iter_mut()
            .find(|c| c.id.as_deref() == Some(id.as_str()))
        {
            existing.name = channel.name;
            existing.desc = channel.desc;
            existing.blockable_system = channel.blockable_system;
            if existing.user_locked_fields == 0 && channel.importance < existing.importance {
                existing.importance = channel.importance;
            }
            if existing.original_importance == IMPORTANCE_UNSPECIFIED {
                existing.original_importance = channel.importance;
            }
            return Ok(());
        }
        if list.len() >= COUNT_LIMIT {
            return Err(ChannelError::Limit);
        }
        // r.priority is PRIORITY_DEFAULT, not PRIORITY_MAX, so no DND bypass
        channel.bypass_dnd = false;
        channel.lockscreen_visibility = VISIBILITY_NO_OVERRIDE;
        channel.allow_bubbles = DEFAULT_ALLOW_BUBBLE;
        channel.important_convo = false;
        channel.user_locked_fields = 0;
        channel.importance_locked_default_app = false;
        channel.original_importance = channel.importance;
        if let Some(parent) = &channel.parent_id
            && !list.iter().any(|c| c.id.as_ref() == Some(parent))
        {
            return Err(ChannelError::NoParent);
        }
        let hash = java_hash(&id);
        let at = list.partition_point(|c| c.id.as_deref().map_or(0, java_hash) <= hash);
        list.insert(at, channel);
        Ok(())
    }

    /// getConversationNotificationChannel without a conversation: the channel `id` (null = DEFAULT_CHANNEL_ID, never created here), or None.
    pub fn get(&self, uid: u32, pkg: &str, id: Option<&str>) -> Option<NotificationChannel> {
        let id = id.unwrap_or(DEFAULT_CHANNEL_ID);
        self.by_app
            .get(&(uid, pkg.to_owned()))?
            .iter()
            .find(|c| c.id.as_deref() == Some(id) && !c.deleted)
            .cloned()
    }

    /// getNotificationChannels without deleted ones, in ArrayMap order.
    pub fn list(&self, uid: u32, pkg: &str) -> Vec<NotificationChannel> {
        self.by_app
            .get(&(uid, pkg.to_owned()))
            .map(|l| l.iter().filter(|c| !c.deleted).cloned().collect())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const UID: u32 = 10_000;
    const PKG: &str = "com.example.app";

    fn channel(id: &str, importance: i32) -> NotificationChannel {
        NotificationChannel {
            id: Some(id.to_owned()),
            name: Some(format!("name {id}")),
            importance,
            ..Default::default()
        }
    }

    #[test]
    fn new_channel_resets_app_fields() {
        let mut ch = Channels::default();
        let mut c = channel("a", 3);
        c.bypass_dnd = true;
        c.lockscreen_visibility = 1;
        c.user_locked_fields = 4;
        ch.create(UID, PKG, c).expect("create");
        let got = ch.get(UID, PKG, Some("a")).expect("stored");
        // PreferencesHelper.createNotificationChannel's new-channel branch
        assert_eq!(
            (
                got.bypass_dnd,
                got.lockscreen_visibility,
                got.user_locked_fields
            ),
            (false, VISIBILITY_NO_OVERRIDE, 0)
        );
        assert_eq!(
            (got.allow_bubbles, got.original_importance),
            (DEFAULT_ALLOW_BUBBLE, 3)
        );
        assert!(ch.get(UID, "other", Some("a")).is_none());
    }

    #[test]
    fn existing_channel_only_downgrades() {
        let mut ch = Channels::default();
        ch.create(UID, PKG, channel("a", 3)).expect("create");
        ch.create(UID, PKG, channel("a", 4)).expect("raise");
        assert_eq!(ch.get(UID, PKG, Some("a")).map(|c| c.importance), Some(3));
        ch.create(UID, PKG, channel("a", 2)).expect("lower");
        assert_eq!(ch.get(UID, PKG, Some("a")).map(|c| c.importance), Some(2));
        assert_eq!(ch.list(UID, PKG).len(), 1);
    }

    #[test]
    fn refusals() {
        let mut ch = Channels::default();
        let mut no_name = channel("a", 3);
        no_name.name = Some(String::new());
        assert_eq!(ch.create(UID, PKG, no_name), Err(ChannelError::NoName));
        assert_eq!(
            ch.create(UID, PKG, channel("a", 6)),
            Err(ChannelError::Importance)
        );
        assert_eq!(
            ch.create(UID, PKG, channel(DEFAULT_CHANNEL_ID, 3)),
            Err(ChannelError::ReservedId)
        );
        let mut child = channel("c", 3);
        child.parent_id = Some("p".to_owned());
        assert_eq!(ch.create(UID, PKG, child), Err(ChannelError::NoParent));
        assert!(ch.list(UID, PKG).is_empty());
    }

    #[test]
    fn listed_in_array_map_order() {
        // Java "b".hashCode() = 98 > "a".hashCode() = 97: ArrayMap lists a first whatever the creation order
        let mut ch = Channels::default();
        ch.create(UID, PKG, channel("b", 3)).expect("b");
        ch.create(UID, PKG, channel("a", 3)).expect("a");
        let ids: Vec<_> = ch.list(UID, PKG).into_iter().filter_map(|c| c.id).collect();
        assert_eq!(ids, ["a", "b"]);
    }
}
