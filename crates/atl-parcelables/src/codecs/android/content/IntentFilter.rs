// SPDX-License-Identifier: MIT
//! android.content.IntentFilter: content/IntentFilter.java writeToParcel (:2952) / IntentFilter(Parcel) (:3084).
//! Wire: the actions as a String array (the ArraySet's iteration order, kept as read), then for categories, data schemes, static data types, data types and MIME groups an int presence flag and a String list
//! (count, strings), the scheme-specific parts / authorities / paths as a count and entries, priority, the two partial-type flags, autoVerify, the instant-app visibility, order, an extras presence flag + PersistableBundle,
//! and the UriRelativeFilterGroup count (always written, 0 unless aconfig flag `relative_reference_intent_filters` is on).
//! Not modelled (refused and counted when present): scheme-specific parts, authorities, paths (PatternMatcher / AuthorityEntry), extras, relative filter groups.

use crate::parcel::*;
use crate::parts::*;

/// android.content.IntentFilter. Optional lists are `None` where Java's field is null (a present empty list is `Some`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct IntentFilter {
    /// mActions in ArraySet order
    pub actions: Vec<Option<String>>,
    pub categories: Option<Vec<Option<String>>>,
    pub data_schemes: Option<Vec<Option<String>>>,
    pub static_data_types: Option<Vec<Option<String>>>,
    pub data_types: Option<Vec<Option<String>>>,
    pub mime_groups: Option<Vec<Option<String>>>,
    pub priority: i32,
    pub has_static_partial_types: bool,
    pub has_dynamic_partial_types: bool,
    /// getAutoVerify()
    pub auto_verify: bool,
    /// mInstantAppVisibility
    pub instant_app_visibility: i32,
    pub order: i32,
}

/// Parcel.writeStringList: the count, then each String16.
fn w_list(p: &mut In<'_>, v: &[Option<String>]) -> Result<()> {
    w_i32(p, len_i32(v.len())?)?;
    v.iter().try_for_each(|s| w_str16(p, s))
}

/// Parcel.readStringList into a new list.
fn r_list(p: &In<'_>) -> Result<Vec<Option<String>>> {
    let n = r_i32(p)?;
    (0..n.max(0)).map(|_| r_str16(p)).collect()
}

/// `if (list != null) { writeInt(1); writeStringList(list); } else writeInt(0)`.
fn w_opt_list(p: &mut In<'_>, v: &Option<Vec<Option<String>>>) -> Result<()> {
    match v {
        Some(list) => {
            w_i32(p, 1)?;
            w_list(p, list)
        }
        None => w_i32(p, 0),
    }
}

/// `if (readInt() != 0) { list = new ArrayList; readStringList(list); }`.
fn r_opt_list(p: &In<'_>) -> Result<Option<Vec<Option<String>>>> {
    if r_i32(p)? != 0 {
        r_list(p).map(Some)
    } else {
        Ok(None)
    }
}

/// A count written for a list of entries no codec models: only 0.
fn r_no_entries(p: &In<'_>, what: &'static str) -> Result<()> {
    if r_i32(p)? > 0 {
        Err(crate::refuse(what))
    } else {
        Ok(())
    }
}

impl Part for IntentFilter {
    fn write(&self, p: &mut In<'_>) -> Result<()> {
        w_list(p, &self.actions)?;
        w_opt_list(p, &self.categories)?;
        w_opt_list(p, &self.data_schemes)?;
        w_opt_list(p, &self.static_data_types)?;
        w_opt_list(p, &self.data_types)?;
        w_opt_list(p, &self.mime_groups)?;
        w_i32(p, 0)?; // mDataSchemeSpecificParts
        w_i32(p, 0)?; // mDataAuthorities
        w_i32(p, 0)?; // mDataPaths
        w_i32(p, self.priority)?;
        w_bool(p, self.has_static_partial_types)?;
        w_bool(p, self.has_dynamic_partial_types)?;
        w_bool(p, self.auto_verify)?;
        w_i32(p, self.instant_app_visibility)?;
        w_i32(p, self.order)?;
        w_i32(p, 0)?; // mExtras
        w_i32(p, 0) // mUriRelativeFilterGroups
    }

    fn read(p: &In<'_>) -> Result<Self> {
        let actions = r_list(p)?;
        let categories = r_opt_list(p)?;
        let data_schemes = r_opt_list(p)?;
        let static_data_types = r_opt_list(p)?;
        let data_types = r_opt_list(p)?;
        let mime_groups = r_opt_list(p)?;
        r_no_entries(p, "android.content.IntentFilter.mDataSchemeSpecificParts")?;
        r_no_entries(p, "android.content.IntentFilter.mDataAuthorities")?;
        r_no_entries(p, "android.content.IntentFilter.mDataPaths")?;
        let filter = IntentFilter {
            actions,
            categories,
            data_schemes,
            static_data_types,
            data_types,
            mime_groups,
            priority: r_i32(p)?,
            // `readInt() > 0`
            has_static_partial_types: r_i32(p)? > 0,
            has_dynamic_partial_types: r_i32(p)? > 0,
            auto_verify: r_i32(p)? > 0,
            instant_app_visibility: r_i32(p)?,
            order: r_i32(p)?,
        };
        if r_i32(p)? != 0 {
            return Err(crate::refuse("android.content.IntentFilter.mExtras"));
        }
        r_no_entries(p, "android.content.IntentFilter.mUriRelativeFilterGroups")?;
        Ok(filter)
    }
}

crate::codec!(IntentFilter);
