// SPDX-License-Identifier: MIT
//! android.app.Notification (app/Notification.java Notification(Parcel) :2823-2829 -> readFromParcelImpl :2831-2950; writeToParcel :3454-3504 -> writeToParcelImpl :3506-3680).
//! Wire: writeToParcelImpl's fields, then the allPendingIntents ArraySet (writeArraySet: int32 count, -1 = null, each writeValue = VAL_PARCELABLE, int32 length, class name, the PendingIntent).
//! There is no length prefix, so the reader walks every field to find the end; the range it walked is kept as a sub-Parcel (bytes + binder/fd objects, Bundle's `Body`) and written back verbatim:
//! byte- and object-exact without a writer per nested type. The class cookies (allowlist token, PendingIntent cookie) are Java-side state only; the token itself is the strong binder in the range.
//! Nested types without a codec are refused (counted) when present, since the end of the parcel cannot be found past them: RemoteViews (tickerView, contentView, bigContentView, headsUpContentView),
//! BubbleMetadata, Action.RemoteInput, Icon's ColorStateList tint. A Notification.Builder notification for targetSdk >= N carries none of them (Builder.build makes RemoteViews only below N).

use binder::ParcelFileDescriptor;
use binder::binder_impl::Parcel;

use crate::codecs::android::app::PendingIntent::PendingIntent;
use crate::codecs::android::media::AudioAttributes::AudioAttributes;
use crate::codecs::android::net::Uri::Uri;
use crate::codecs::android::os::Bundle::{Body, Bundle, r_bundle};
use crate::parcel::*;
use crate::parts::*;

/// The fields a service reads; the whole parcelled form is `raw`.
#[derive(Debug, Clone, Default)]
pub struct Notification {
    pub when: i64,
    pub flags: i32,
    pub category: Option<String>,
    pub group_key: Option<String>,
    pub channel_id: Option<String>,
    pub timeout: i64,
    /// contentIntent is set
    pub has_content_intent: bool,
    /// actions.length (0 for null)
    pub actions: usize,
    /// extras (title, text, style fields: Notification.EXTRA_*)
    pub extras: Option<Bundle>,
    /// mLargeIcon is set
    pub has_large_icon: bool,
    /// the parcelled range as read
    raw: Body,
}

/// Icon.TYPE_* (graphics/drawable/Icon.java :90-116).
const TYPE_BITMAP: i32 = 1;
const TYPE_RESOURCE: i32 = 2;
const TYPE_DATA: i32 = 3;
const TYPE_URI: i32 = 4;
const TYPE_ADAPTIVE_BITMAP: i32 = 5;
const TYPE_URI_ADAPTIVE_BITMAP: i32 = 6;
/// Parcel.BLOB_INPLACE (libbinder Parcel.cpp): the blob's bytes follow; else a descriptor.
const BLOB_INPLACE: i32 = 0;
/// Parcel.VAL_PARCELABLE (Parcel.java): writeValue's type code for a Parcelable, length-prefixed (isLengthPrefixed).
const VAL_PARCELABLE: i32 = 4;

/// Icon(Parcel) (Icon.java :1050-1089): type, its payload, tint flag (+ ColorStateList), blend mode.
fn skip_icon(p: &In<'_>) -> Result<()> {
    match r_i32(p)? {
        TYPE_BITMAP | TYPE_ADAPTIVE_BITMAP => {
            Bitmap::read(p)?;
        }
        TYPE_RESOURCE => {
            r_str16(p)?; // package
            r_i32(p)?; // resId
            r_bool(p)?; // mUseMonochrome
            r_f32(p)?; // mInsetScale
        }
        TYPE_DATA => {
            let len = r_i32(p)?;
            // Parcel.readBlob (android_os_Parcel_readBlob): its own int32 length, then Parcel::readBlob: blob type, then in place (padded to 4) or a descriptor
            if r_i32(p)? != len || len < 0 {
                return Err(crate::refuse(
                    "android.graphics.drawable.Icon (data length)",
                ));
            }
            if r_i32(p)? == BLOB_INPLACE {
                let end = p.get_data_position() + ((len + 3) & !3);
                p.set_data_position_checked(end)?;
            } else {
                let _fd: ParcelFileDescriptor = p.read()?;
            }
        }
        TYPE_URI | TYPE_URI_ADAPTIVE_BITMAP => {
            r_str16(p)?;
        }
        _ => return Err(crate::refuse("android.graphics.drawable.Icon (type)")),
    }
    if r_i32(p)? == 1 {
        return Err(crate::refuse("android.content.res.ColorStateList"));
    }
    r_i32(p)?; // blend mode
    Ok(())
}

/// A presence word then the nested value, for the `if (parcel.readInt() != 0)` fields.
fn present(p: &In<'_>) -> Result<bool> {
    Ok(r_i32(p)? != 0)
}

/// RemoteViews has no codec: only its absence is accepted.
fn no_remote_views(p: &In<'_>) -> Result<()> {
    if present(p)? {
        return Err(crate::refuse("android.widget.RemoteViews"));
    }
    Ok(())
}

/// Notification.Action(Parcel) (:2016-2033).
fn skip_action(p: &In<'_>) -> Result<()> {
    if present(p)? {
        skip_icon(p)?;
    }
    r_char_sequence(p)?; // title
    if r_i32(p)? == 1 {
        PendingIntent::read(p)?;
    }
    r_bundle(p)?;
    // createTypedArray(RemoteInput.CREATOR): -1 null, else count + a presence word each
    let n = r_i32(p)?;
    for _ in 0..n.max(0) {
        if present(p)? {
            return Err(crate::refuse("android.app.RemoteInput"));
        }
    }
    r_i32(p)?; // mAllowGeneratedReplies
    r_i32(p)?; // mSemanticAction
    r_i32(p)?; // mIsContextual
    r_i32(p)?; // mAuthenticationRequired
    Ok(())
}

/// Notification(Parcel): readFromParcelImpl's fields in order, then allPendingIntents.
fn read_fields(p: &In<'_>) -> Result<Notification> {
    let mut n = Notification::default();
    r_i32(p)?; // version
    r_binder(p)?; // mAllowlistToken
    n.when = r_i64(p)?;
    r_i64(p)?; // creationTime
    if present(p)? {
        skip_icon(p)?; // mSmallIcon
    }
    r_i32(p)?; // number
    if present(p)? {
        PendingIntent::read(p)?;
        n.has_content_intent = true;
    }
    if present(p)? {
        PendingIntent::read(p)?; // deleteIntent
    }
    if present(p)? {
        r_char_sequence(p)?; // tickerText
    }
    no_remote_views(p)?; // tickerView
    no_remote_views(p)?; // contentView
    if present(p)? {
        skip_icon(p)?; // mLargeIcon
        n.has_large_icon = true;
    }
    r_i32(p)?; // defaults
    n.flags = r_i32(p)?;
    if present(p)? {
        Uri::read(p)?; // sound
    }
    r_i32(p)?; // audioStreamType
    if present(p)? {
        AudioAttributes::read(p)?;
    }
    r_i64_array(p)?; // vibrate
    for _ in 0..4 {
        r_i32(p)?; // ledARGB, ledOnMS, ledOffMS, iconLevel
    }
    if present(p)? {
        PendingIntent::read(p)?; // fullScreenIntent
    }
    r_i32(p)?; // priority
    n.category = r_str8(p)?;
    n.group_key = r_str8(p)?;
    r_str8(p)?; // mSortKey
    n.extras = r_bundle(p)?;
    let actions = r_i32(p)?;
    for _ in 0..actions.max(0) {
        if present(p)? {
            skip_action(p)?;
            n.actions += 1;
        }
    }
    no_remote_views(p)?; // bigContentView
    no_remote_views(p)?; // headsUpContentView
    r_i32(p)?; // visibility
    if present(p)? {
        read_fields(p)?; // publicVersion: a whole Notification, its own ArraySet included
    }
    r_i32(p)?; // color
    if present(p)? {
        n.channel_id = r_str8(p)?;
    }
    n.timeout = r_i64(p)?;
    if present(p)? {
        r_str8(p)?; // mShortcutId
    }
    if present(p)? {
        r_str16(p)?; // LocusId.writeToParcel: writeString(mId)
    }
    r_i32(p)?; // mBadgeIcon
    if present(p)? {
        r_char_sequence(p)?; // mSettingsText
    }
    r_i32(p)?; // mGroupAlertBehavior
    if present(p)? {
        return Err(crate::refuse("android.app.Notification$BubbleMetadata"));
    }
    r_i32(p)?; // mAllowSystemGeneratedContextualActions (writeBoolean)
    r_i32(p)?; // mFgsDeferBehavior
    let pending = r_i32(p)?;
    for _ in 0..pending.max(0) {
        if r_i32(p)? != VAL_PARCELABLE {
            return Err(crate::refuse(
                "android.app.Notification.allPendingIntents (value type)",
            ));
        }
        let length = r_i32(p)?;
        let start = p.get_data_position();
        r_str16(p)?; // writeParcelableCreator: the class name
        PendingIntent::read(p)?;
        if p.get_data_position() != start + length {
            return Err(crate::refuse(
                "android.app.Notification.allPendingIntents (length)",
            ));
        }
    }
    Ok(n)
}

impl Part for Notification {
    fn write(&self, p: &mut In<'_>) -> Result<()> {
        let raw = self.raw.lock();
        let length = raw.get_data_size();
        if length == 0 {
            // only a decoded Notification has a range; Default is no notification at all
            return Err(crate::refuse(
                "android.app.Notification.write (not decoded)",
            ));
        }
        p.append_from(&*raw, 0, length)
    }

    fn read(p: &In<'_>) -> Result<Self> {
        let start = p.get_data_position();
        let mut n = read_fields(p)?;
        let end = p.get_data_position();
        let mut raw = Parcel::new();
        raw.append_from(p, start, end - start)?;
        p.set_data_position_checked(end)?;
        n.raw = Body::new(raw);
        Ok(n)
    }
}

crate::codec!(Notification);
