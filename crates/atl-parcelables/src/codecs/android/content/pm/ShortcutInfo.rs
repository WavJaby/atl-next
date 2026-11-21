// SPDX-License-Identifier: MIT
//! android.content.pm.ShortcutInfo (content/pm/ShortcutInfo.java writeToParcel :2415-2470 / ShortcutInfo(Parcel) :2353-2412): the key fields (user, id, package, activity, flags, icon res id, timestamp,
//! disabled reason), then an int 0 (key fields only: FLAG_KEY_FIELDS_ONLY) or 1 and the rest: icon, title/text/disabled message (CharSequence + res id each), intents and their persistable extras
//! (writeParcelableArray), rank, extras, bitmap path, four res names, categories (count + String8s, 0 = null), persons, locus id, icon uri, starting theme, excluded surfaces, capability bindings (writeMap).
//! Nested forms: writeParcelable = class name (String16) + body, null = a null String16. Not modelled, refused (counted): Spanned text with spans, an Icon with a tint list or of TYPE_DATA, a non-empty
//! capability-binding map; a bitmap Icon is read past (Bitmap reader) and cannot be written back (ShortcutService clears it on accept, `saveIconAndFixUpShortcutLocked`).

use crate::codecs::android::content::Intent::Intent;
use crate::codecs::android::os::Bundle::Bundle;
use crate::parcel::*;
use crate::parts::*;

/// The Java class names writeParcelableCreator writes.
const INTENT: &str = "android.content.Intent";
const PERSISTABLE_BUNDLE: &str = "android.os.PersistableBundle";
const ICON: &str = "android.graphics.drawable.Icon";
const LOCUS_ID: &str = "android.content.LocusId";
const PERSON: &str = "android.app.Person";
const COMPONENT_NAME: &str = "android.content.ComponentName";

/// Icon.TYPE_* (graphics/java/android/graphics/drawable/Icon.java:90-116).
pub const TYPE_BITMAP: i32 = 1;
pub const TYPE_RESOURCE: i32 = 2;
pub const TYPE_URI: i32 = 4;
pub const TYPE_ADAPTIVE_BITMAP: i32 = 5;
pub const TYPE_URI_ADAPTIVE_BITMAP: i32 = 6;

/// android.graphics.drawable.Icon (Icon.java writeToParcel / Icon(Parcel)) of the kinds a shortcut carries; `blend_mode` is BlendMode.toValue.
#[derive(Debug, Clone, PartialEq)]
pub struct Icon {
    pub kind: IconKind,
    pub blend_mode: i32,
}

#[derive(Debug, Clone, PartialEq)]
pub enum IconKind {
    /// TYPE_BITMAP / TYPE_ADAPTIVE_BITMAP: pixels read past, not kept.
    Bitmap {
        adaptive: bool,
        width: i32,
        height: i32,
    },
    /// TYPE_RESOURCE: writeString package, writeInt id, writeBoolean useMonochrome, writeFloat insetScale.
    Resource {
        package: Option<String>,
        res_id: i32,
        monochrome: bool,
        inset_scale: f32,
    },
    /// TYPE_URI / TYPE_URI_ADAPTIVE_BITMAP: writeString uri.
    Uri { adaptive: bool, uri: Option<String> },
}

impl Part for Icon {
    fn write(&self, p: &mut In<'_>) -> Result<()> {
        match &self.kind {
            IconKind::Bitmap { .. } => {
                return Err(crate::refuse(
                    "android.graphics.drawable.Icon (bitmap write)",
                ));
            }
            IconKind::Resource {
                package,
                res_id,
                monochrome,
                inset_scale,
            } => {
                w_i32(p, TYPE_RESOURCE)?;
                w_str16(p, package)?;
                w_i32(p, *res_id)?;
                w_bool(p, *monochrome)?;
                w_f32(p, *inset_scale)?;
            }
            IconKind::Uri { adaptive, uri } => {
                w_i32(
                    p,
                    if *adaptive {
                        TYPE_URI_ADAPTIVE_BITMAP
                    } else {
                        TYPE_URI
                    },
                )?;
                w_str16(p, uri)?;
            }
        }
        w_i32(p, 0)?; // no tint list
        w_i32(p, self.blend_mode)
    }

    fn read(p: &In<'_>) -> Result<Self> {
        let kind = match r_i32(p)? {
            t @ (TYPE_BITMAP | TYPE_ADAPTIVE_BITMAP) => {
                let b = Bitmap::read(p)?;
                IconKind::Bitmap {
                    adaptive: t == TYPE_ADAPTIVE_BITMAP,
                    width: b.width,
                    height: b.height,
                }
            }
            TYPE_RESOURCE => IconKind::Resource {
                package: r_str16(p)?,
                res_id: r_i32(p)?,
                monochrome: r_bool(p)?,
                inset_scale: r_f32(p)?,
            },
            t @ (TYPE_URI | TYPE_URI_ADAPTIVE_BITMAP) => IconKind::Uri {
                adaptive: t == TYPE_URI_ADAPTIVE_BITMAP,
                uri: r_str16(p)?,
            },
            _ => return Err(crate::refuse("android.graphics.drawable.Icon (type)")),
        };
        if r_i32(p)? == 1 {
            return Err(crate::refuse("android.graphics.drawable.Icon (tint list)"));
        }
        Ok(Icon {
            kind,
            blend_mode: r_i32(p)?,
        })
    }
}

/// android.app.Person (app/Person.java writeToParcel :167-179): name CharSequence, icon presence + Icon, uri, key, important, bot.
#[derive(Debug, Clone, PartialEq)]
pub struct Person {
    pub name: Option<String>,
    pub icon: Option<Icon>,
    pub uri: Option<String>,
    pub key: Option<String>,
    pub important: bool,
    pub bot: bool,
}

impl Part for Person {
    fn write(&self, p: &mut In<'_>) -> Result<()> {
        w_char_sequence(p, &self.name)?;
        match &self.icon {
            Some(i) => {
                w_i32(p, 1)?;
                i.write(p)?;
            }
            None => w_i32(p, 0)?,
        }
        w_str16(p, &self.uri)?;
        w_str16(p, &self.key)?;
        w_bool(p, self.important)?;
        w_bool(p, self.bot)
    }

    fn read(p: &In<'_>) -> Result<Self> {
        let name = r_char_sequence(p)?;
        let icon = if r_i32(p)? != 0 {
            Some(Icon::read(p)?)
        } else {
            None
        };
        Ok(Person {
            name,
            icon,
            uri: r_str16(p)?,
            key: r_str16(p)?,
            important: r_bool(p)?,
            bot: r_bool(p)?,
        })
    }
}

/// writeParcelable of `v` as class `class`.
fn w_parcelable<T: Part>(p: &mut In<'_>, class: &str, v: &Option<T>) -> Result<()> {
    match v {
        None => w_str16(p, &None),
        Some(v) => {
            w_str16(p, &Some(class.to_owned()))?;
            v.write(p)
        }
    }
}

/// readParcelable of class `class` (another class name is refused: its body is unknown).
fn r_parcelable<T: Part>(p: &In<'_>, class: &str, what: &'static str) -> Result<Option<T>> {
    match r_str16(p)? {
        None => Ok(None),
        Some(c) if c == class => T::read(p).map(Some),
        Some(_) => Err(crate::refuse(what)),
    }
}

/// writeParcelableArray: -1 null, else the count and each element writeParcelable'd.
fn w_parcelable_array<T: Part>(
    p: &mut In<'_>,
    class: &str,
    v: &Option<Vec<Option<T>>>,
) -> Result<()> {
    match v {
        None => w_i32(p, -1),
        Some(items) => {
            w_i32(p, len_i32(items.len())?)?;
            items.iter().try_for_each(|i| w_parcelable(p, class, i))
        }
    }
}

fn r_parcelable_array<T: Part>(
    p: &In<'_>,
    class: &str,
    what: &'static str,
) -> Result<Option<Vec<Option<T>>>> {
    let n = r_i32(p)?;
    if n < 0 {
        return Ok(None);
    }
    (0..n)
        .map(|_| r_parcelable(p, class, what))
        .collect::<Result<Vec<_>>>()
        .map(Some)
}

/// android.content.LocusId: one writeString.
#[derive(Debug, Clone, PartialEq)]
pub struct LocusId(pub Option<String>);

impl Part for LocusId {
    fn write(&self, p: &mut In<'_>) -> Result<()> {
        w_str16(p, &self.0)
    }
    fn read(p: &In<'_>) -> Result<Self> {
        Ok(LocusId(r_str16(p)?))
    }
}

/// The ComponentName body (writeParcelable's form, unlike the bare `w_component`): two String16s.
#[derive(Debug, Clone, PartialEq)]
struct Component(ComponentName);

impl Part for Component {
    fn write(&self, p: &mut In<'_>) -> Result<()> {
        w_str16(p, &Some(self.0.package.clone()))?;
        w_str16(p, &Some(self.0.class.clone()))
    }
    fn read(p: &In<'_>) -> Result<Self> {
        let package = r_str16(p)?.unwrap_or_default();
        let class = r_str16(p)?.unwrap_or_default();
        Ok(Component(ComponentName { package, class }))
    }
}

/// Everything after the key fields (absent when FLAG_KEY_FIELDS_ONLY). No `PartialEq`: Intent and Bundle have none.
#[derive(Debug, Clone, Default)]
pub struct ShortcutDetails {
    pub icon: Option<Icon>,
    pub title: Option<String>,
    pub title_res_id: i32,
    pub text: Option<String>,
    pub text_res_id: i32,
    pub disabled_message: Option<String>,
    pub disabled_message_res_id: i32,
    pub intents: Option<Vec<Option<Intent>>>,
    pub intent_persistable_extras: Option<Vec<Option<Bundle>>>,
    pub rank: i32,
    pub extras: Option<Bundle>,
    pub bitmap_path: Option<String>,
    pub icon_res_name: Option<String>,
    pub title_res_name: Option<String>,
    pub text_res_name: Option<String>,
    pub disabled_message_res_name: Option<String>,
    /// None = mCategories null (written as count 0).
    pub categories: Option<Vec<Option<String>>>,
    pub persons: Option<Vec<Option<Person>>>,
    pub locus_id: Option<LocusId>,
    pub icon_uri: Option<String>,
    pub starting_theme_res_name: Option<String>,
    pub excluded_surfaces: i32,
}

#[derive(Debug, Clone, Default)]
pub struct ShortcutInfo {
    pub user_id: i32,
    pub id: Option<String>,
    pub package: Option<String>,
    pub activity: Option<ComponentName>,
    pub flags: i32,
    pub icon_res_id: i32,
    pub last_changed_timestamp: i64,
    pub disabled_reason: i32,
    /// None: key fields only (the int 0 after the key fields).
    pub details: Option<ShortcutDetails>,
}

impl Part for ShortcutInfo {
    fn write(&self, p: &mut In<'_>) -> Result<()> {
        w_i32(p, self.user_id)?;
        w_str8(p, &self.id)?;
        w_str8(p, &self.package)?;
        w_parcelable(p, COMPONENT_NAME, &self.activity.clone().map(Component))?;
        w_i32(p, self.flags)?;
        w_i32(p, self.icon_res_id)?;
        w_i64(p, self.last_changed_timestamp)?;
        w_i32(p, self.disabled_reason)?;
        let Some(d) = &self.details else {
            return w_i32(p, 0);
        };
        w_i32(p, 1)?;
        w_parcelable(p, ICON, &d.icon)?;
        w_char_sequence(p, &d.title)?;
        w_i32(p, d.title_res_id)?;
        w_char_sequence(p, &d.text)?;
        w_i32(p, d.text_res_id)?;
        w_char_sequence(p, &d.disabled_message)?;
        w_i32(p, d.disabled_message_res_id)?;
        w_parcelable_array(p, INTENT, &d.intents)?;
        w_parcelable_array(p, PERSISTABLE_BUNDLE, &d.intent_persistable_extras)?;
        w_i32(p, d.rank)?;
        w_parcelable(p, PERSISTABLE_BUNDLE, &d.extras)?;
        w_str8(p, &d.bitmap_path)?;
        w_str8(p, &d.icon_res_name)?;
        w_str8(p, &d.title_res_name)?;
        w_str8(p, &d.text_res_name)?;
        w_str8(p, &d.disabled_message_res_name)?;
        match &d.categories {
            Some(c) if !c.is_empty() => {
                w_i32(p, len_i32(c.len())?)?;
                c.iter().try_for_each(|s| w_str8(p, s))?;
            }
            _ => w_i32(p, 0)?,
        }
        w_parcelable_array(p, PERSON, &d.persons)?;
        w_parcelable(p, LOCUS_ID, &d.locus_id)?;
        w_str8(p, &d.icon_uri)?;
        w_str8(p, &d.starting_theme_res_name)?;
        w_i32(p, d.excluded_surfaces)?;
        w_i32(p, -1) // capability bindings: null map
    }

    fn read(p: &In<'_>) -> Result<Self> {
        let user_id = r_i32(p)?;
        let id = r_str8(p)?;
        let package = r_str8(p)?;
        let activity = r_parcelable::<Component>(
            p,
            COMPONENT_NAME,
            "android.content.pm.ShortcutInfo (activity class)",
        )?
        .map(|c| c.0);
        let flags = r_i32(p)?;
        let icon_res_id = r_i32(p)?;
        let last_changed_timestamp = r_i64(p)?;
        let disabled_reason = r_i32(p)?;
        let mut s = ShortcutInfo {
            user_id,
            id,
            package,
            activity,
            flags,
            icon_res_id,
            last_changed_timestamp,
            disabled_reason,
            details: None,
        };
        if r_i32(p)? == 0 {
            return Ok(s);
        }
        let icon = r_parcelable(p, ICON, "android.content.pm.ShortcutInfo (icon class)")?;
        let title = r_char_sequence(p)?;
        let title_res_id = r_i32(p)?;
        let text = r_char_sequence(p)?;
        let text_res_id = r_i32(p)?;
        let disabled_message = r_char_sequence(p)?;
        let disabled_message_res_id = r_i32(p)?;
        let intents =
            r_parcelable_array(p, INTENT, "android.content.pm.ShortcutInfo (intent class)")?;
        let intent_persistable_extras = r_parcelable_array(
            p,
            PERSISTABLE_BUNDLE,
            "android.content.pm.ShortcutInfo (extras class)",
        )?;
        let rank = r_i32(p)?;
        let extras = r_parcelable(
            p,
            PERSISTABLE_BUNDLE,
            "android.content.pm.ShortcutInfo (extras class)",
        )?;
        let bitmap_path = r_str8(p)?;
        let icon_res_name = r_str8(p)?;
        let title_res_name = r_str8(p)?;
        let text_res_name = r_str8(p)?;
        let disabled_message_res_name = r_str8(p)?;
        let n = r_i32(p)?;
        let categories = if n <= 0 {
            None
        } else {
            Some((0..n).map(|_| r_str8(p)).collect::<Result<Vec<_>>>()?)
        };
        let persons =
            r_parcelable_array(p, PERSON, "android.content.pm.ShortcutInfo (person class)")?;
        let locus_id = r_parcelable(p, LOCUS_ID, "android.content.pm.ShortcutInfo (locus class)")?;
        let icon_uri = r_str8(p)?;
        let starting_theme_res_name = r_str8(p)?;
        let excluded_surfaces = r_i32(p)?;
        // writeMap: -1 null, else a count of (key, writeValue) pairs; only the empty map is modelled
        if r_i32(p)? > 0 {
            return Err(crate::refuse(
                "android.content.pm.ShortcutInfo (capability bindings)",
            ));
        }
        s.details = Some(ShortcutDetails {
            icon,
            title,
            title_res_id,
            text,
            text_res_id,
            disabled_message,
            disabled_message_res_id,
            intents,
            intent_persistable_extras,
            rank,
            extras,
            bitmap_path,
            icon_res_name,
            title_res_name,
            text_res_name,
            disabled_message_res_name,
            categories,
            persons,
            locus_id,
            icon_uri,
            starting_theme_res_name,
            excluded_surfaces,
        });
        Ok(s)
    }
}

crate::codec!(ShortcutInfo);
