// SPDX-License-Identifier: MIT
//! A posted Notification's desktop form (notifications plan N4, the baseline tier's columns): what the desktop's notification service shows for it, and which Android fields it leaves out.
//! Text: EXTRA_TITLE (EXTRA_TITLE_BIG under BigTextStyle) is the summary; EXTRA_TEXT (EXTRA_BIG_TEXT under BigTextStyle) the body, EXTRA_SUB_TEXT and EXTRA_INFO_TEXT appended on their own lines.
//! Spans are not interpreted (a span-carrying text ends the extras walk: refused, counted). Importance: the channel's (PreferencesHelper), LOW/MIN quiet, DEFAULT and above the ordinary banner;
//! NONE is never posted (blocked, NotificationManagerService checkDisqualifyingFeatures). Ongoing / no-clear / foreground service: resident (fixNotification). Sound: none for a channel without one,
//! for LOW/MIN, and for an update of an only-alert-once notification. Left out and named per post: large icon, progress, inbox lines, messages, big picture, actions, content intent, a custom sound.

use atl_broker::control::{NotificationContent, NotificationUrgency};
use atl_parcelables::android::app::{Notification, NotificationChannel};

/// NotificationManager.IMPORTANCE_NONE / IMPORTANCE_LOW (NotificationManager.java): NONE blocks the channel, LOW and MIN (1) make no sound and no banner.
pub const IMPORTANCE_NONE: i32 = 0;
const IMPORTANCE_LOW: i32 = 2;
/// Notification.FLAG_* (Notification.java): ONGOING_EVENT, ONLY_ALERT_ONCE, NO_CLEAR, FOREGROUND_SERVICE.
const FLAG_ONGOING_EVENT: i32 = 0x2;
const FLAG_ONLY_ALERT_ONCE: i32 = 0x8;
const FLAG_NO_CLEAR: i32 = 0x20;
const FLAG_FOREGROUND_SERVICE: i32 = 0x40;
/// Notification.EXTRA_* keys (Notification.java).
const EXTRA_TITLE: &str = "android.title";
const EXTRA_TITLE_BIG: &str = "android.title.big";
const EXTRA_TEXT: &str = "android.text";
const EXTRA_BIG_TEXT: &str = "android.bigText";
const EXTRA_SUB_TEXT: &str = "android.subText";
const EXTRA_INFO_TEXT: &str = "android.infoText";
const EXTRA_TEMPLATE: &str = "android.template";
/// BigTextStyle's EXTRA_TEMPLATE value: its class name (Style.addExtras).
const BIG_TEXT_STYLE: &str = "android.app.Notification$BigTextStyle";
/// Extras the baseline tier does not show, and the counter name each gets.
const LEFT_OUT_EXTRAS: [(&str, &str); 4] = [
    ("android.progressMax", "progress"),
    ("android.textLines", "inbox_lines"),
    ("android.messages", "messages"),
    ("android.picture", "picture"),
];
/// Settings.System.DEFAULT_NOTIFICATION_URI: the device's notification sound; any other channel sound is a custom one, played as the desktop's default.
const DEFAULT_NOTIFICATION_URI: &str = "content://settings/system/notification_sound";

/// The desktop form of `n` posted on `channel`; `update`: the key is shown already. Also the names of the fields left out.
pub fn content(
    n: &Notification,
    channel: &NotificationChannel,
    update: bool,
) -> (NotificationContent, Vec<&'static str>) {
    let mut left_out = Vec::new();
    let texts = match n.extras.as_ref().map(|e| e.texts()) {
        None => Vec::new(),
        Some(Ok(t)) => t,
        Some(Err(_)) => {
            left_out.push("extras");
            Vec::new()
        }
    };
    let text = |key: &str| {
        texts
            .iter()
            .find(|(k, _)| k == key)
            .and_then(|(_, v)| v.clone())
    };
    let big_text = text(EXTRA_TEMPLATE).as_deref() == Some(BIG_TEXT_STYLE);
    let summary = big_text
        .then(|| text(EXTRA_TITLE_BIG))
        .flatten()
        .or_else(|| text(EXTRA_TITLE))
        .unwrap_or_default();
    let mut body = big_text
        .then(|| text(EXTRA_BIG_TEXT))
        .flatten()
        .or_else(|| text(EXTRA_TEXT))
        .unwrap_or_default();
    for extra in [EXTRA_SUB_TEXT, EXTRA_INFO_TEXT]
        .into_iter()
        .filter_map(text)
    {
        if !body.is_empty() {
            body.push('\n');
        }
        body.push_str(&extra);
    }
    for (key, name) in LEFT_OUT_EXTRAS {
        if texts.iter().any(|(k, _)| k == key) {
            left_out.push(name);
        }
    }
    if n.has_large_icon {
        left_out.push("large_icon");
    }
    if n.actions > 0 {
        left_out.push("actions");
    }
    if n.has_content_intent {
        left_out.push("content_intent");
    }
    let quiet = channel.importance <= IMPORTANCE_LOW;
    let custom_sound = channel
        .sound
        .as_ref()
        .is_some_and(|u| u.0 != DEFAULT_NOTIFICATION_URI);
    if custom_sound && !quiet {
        left_out.push("custom_sound");
    }
    let content = NotificationContent {
        summary,
        body,
        urgency: if quiet {
            NotificationUrgency::Low
        } else {
            NotificationUrgency::Normal
        },
        resident: n.flags & (FLAG_ONGOING_EVENT | FLAG_NO_CLEAR | FLAG_FOREGROUND_SERVICE) != 0,
        silent: quiet || channel.sound.is_none() || (update && n.flags & FLAG_ONLY_ALERT_ONCE != 0),
    };
    (content, left_out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use atl_parcelables::android::net::Uri;
    use atl_parcelables::android::os::Bundle;

    fn notification(extras: &[(&str, &str)], flags: i32) -> Notification {
        let mut n = Notification::default();
        n.extras = Some(Bundle::from_strings(extras));
        n.flags = flags;
        n
    }

    /// A channel at `importance` with the device's default sound (what NotificationChannel's constructor sets).
    fn channel(importance: i32) -> NotificationChannel {
        NotificationChannel {
            importance,
            sound: Some(Uri(DEFAULT_NOTIFICATION_URI.to_owned())),
            ..NotificationChannel::default()
        }
    }

    /// N4: title -> summary, text -> body; IMPORTANCE_DEFAULT (3) -> the ordinary banner with sound; nothing left out.
    #[test]
    fn plain_post_on_a_default_channel() {
        let (c, left) = content(
            &notification(&[(EXTRA_TITLE, "T"), (EXTRA_TEXT, "x")], 0),
            &channel(3),
            false,
        );
        assert_eq!(
            c,
            NotificationContent {
                summary: "T".into(),
                body: "x".into(),
                urgency: NotificationUrgency::Normal,
                resident: false,
                silent: false,
            }
        );
        assert!(left.is_empty(), "{left:?}");
    }

    /// N4: BigTextStyle's big title and big text replace title and text; sub text and info text follow on their own lines.
    #[test]
    fn big_text_style_and_appended_texts() {
        let n = notification(
            &[
                (EXTRA_TITLE, "T"),
                (EXTRA_TITLE_BIG, "Big"),
                (EXTRA_TEXT, "x"),
                (EXTRA_BIG_TEXT, "long"),
                (EXTRA_SUB_TEXT, "sub"),
                (EXTRA_INFO_TEXT, "info"),
                (EXTRA_TEMPLATE, BIG_TEXT_STYLE),
            ],
            0,
        );
        let (c, _) = content(&n, &channel(3), false);
        assert_eq!(
            (c.summary.as_str(), c.body.as_str()),
            ("Big", "long\nsub\ninfo")
        );
    }

    /// N4: LOW (2) and MIN (1) are quiet; HIGH (4) is the banner; a channel with no sound is silent; ongoing (0x2) is resident.
    #[test]
    fn importance_sound_and_ongoing() {
        let n = notification(&[], 0);
        for (importance, urgency, silent) in [
            (1, NotificationUrgency::Low, true),
            (2, NotificationUrgency::Low, true),
            (4, NotificationUrgency::Normal, false),
        ] {
            let (c, _) = content(&n, &channel(importance), false);
            assert_eq!(
                (c.urgency, c.silent),
                (urgency, silent),
                "importance {importance}"
            );
        }
        let mut quiet = channel(4);
        quiet.sound = None;
        assert!(content(&n, &quiet, false).0.silent);
        assert!(
            content(&notification(&[], FLAG_ONGOING_EVENT), &channel(3), false)
                .0
                .resident
        );
    }

    /// N4 onlyAlertOnce: silent on an update only.
    #[test]
    fn only_alert_once_silences_updates() {
        let n = notification(&[], FLAG_ONLY_ALERT_ONCE);
        assert!(!content(&n, &channel(3), false).0.silent);
        assert!(content(&n, &channel(3), true).0.silent);
    }

    /// What the baseline tier does not show is named: progress, actions, content intent, a custom sound.
    #[test]
    fn left_out_fields_are_named() {
        let mut n = notification(&[("android.progressMax", "100")], 0);
        n.actions = 2;
        n.has_content_intent = true;
        let mut ch = channel(3);
        ch.sound = Some(Uri("content://media/x".into()));
        let (_, left) = content(&n, &ch, false);
        assert_eq!(
            left,
            vec!["progress", "actions", "content_intent", "custom_sound"]
        );
    }
}
