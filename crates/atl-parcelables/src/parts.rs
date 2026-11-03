// SPDX-License-Identifier: MIT
//! Parcel layouts the codecs share or embed without being AIDL parcelables themselves (base classes, values written in place), each from its AOSP writeToParcel / reader.
//! Paths in docs are below frameworks/base/core/java unless stated.

use binder::SpIBinder;

use crate::codecs::android::content::Intent::Intent;
use crate::codecs::android::content::pm::ActivityInfo::ActivityInfo;
use crate::codecs::android::content::pm::ApplicationInfo::ApplicationInfo;
use crate::codecs::android::content::res::Configuration::Configuration;
use crate::parcel::*;

/// A parcel layout: written and read in the AOSP order.
pub trait Part: Sized {
    fn write(&self, p: &mut In<'_>) -> Result<()>;
    fn read(p: &In<'_>) -> Result<Self>;
}

/// Parcel.writeTypedObject: 1 + the value, or 0 for null.
pub fn w_typed<T: Part>(p: &mut In<'_>, v: &Option<T>) -> Result<()> {
    match v {
        None => w_present(p, false),
        Some(x) => {
            w_present(p, true)?;
            x.write(p)
        }
    }
}
/// Parcel.readTypedObject.
pub fn r_typed<T: Part>(p: &In<'_>) -> Result<Option<T>> {
    if r_present(p)? {
        T::read(p).map(Some)
    } else {
        Ok(None)
    }
}

/// Parcel.writeTypedList: -1 for null, else the count and each element as writeTypedObject.
pub fn w_typed_list<T: Part>(p: &mut In<'_>, v: &Option<Vec<Option<T>>>) -> Result<()> {
    match v {
        None => w_i32(p, -1),
        Some(list) => {
            w_i32(p, len_i32(list.len())?)?;
            list.iter().try_for_each(|e| w_typed(p, e))
        }
    }
}
/// Parcel.createTypedArrayList.
pub fn r_typed_list<T: Part>(p: &In<'_>) -> Result<Option<Vec<Option<T>>>> {
    let n = r_i32(p)?;
    if n < 0 {
        return Ok(None);
    }
    (0..n).map(|_| r_typed(p)).collect::<Result<_>>().map(Some)
}

/// android.content.pm.SharedLibraryInfo (content/pm/SharedLibraryInfo.java writeToParcel :491-509 / SharedLibraryInfo(Parcel) :194-214): path, package name, the code paths (a presence int, then a String8 array),
/// name, version, type, declaringPackage, dependentPackages, dependencies (a typed list), isNative, optionalDependentPackages, certDigests. Not modelled, null only: declaringPackage (a VersionedPackage),
/// dependentPackages, optionalDependentPackages, certDigests.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SharedLibraryInfo {
    pub path: Option<String>,
    pub package_name: Option<String>,
    pub code_paths: Option<Vec<Option<String>>>,
    pub name: Option<String>,
    pub version: i64,
    /// SharedLibraryInfo.TYPE_BUILTIN 0, TYPE_DYNAMIC 1, TYPE_STATIC 2, TYPE_SDK_PACKAGE 3
    pub kind: i32,
    pub dependencies: Option<Vec<Option<SharedLibraryInfo>>>,
    pub is_native: bool,
    /// optionalDependentPackages is written -1 (null) or 0 (empty): readParcelableList leaves an empty list for null, so a SharedLibraryInfo that was read from a parcel writes 0 here. No entries are modelled.
    pub empty_optional_dependents: bool,
}

impl Part for SharedLibraryInfo {
    fn write(&self, p: &mut In<'_>) -> Result<()> {
        w_str8(p, &self.path)?;
        w_str8(p, &self.package_name)?;
        match &self.code_paths {
            Some(paths) => {
                w_i32(p, 1)?;
                w_str8_array(p, &Some(paths.clone()))?;
            }
            None => w_i32(p, 0)?,
        }
        w_str8(p, &self.name)?;
        w_i64(p, self.version)?;
        w_i32(p, self.kind)?;
        w_null(p)?; // declaringPackage: writeParcelable(null) is writeString(null)
        w_null(p)?; // dependentPackages: writeList(null)
        w_typed_list(p, &self.dependencies)?;
        w_bool(p, self.is_native)?;
        // optionalDependentPackages: writeParcelableList(null) or of an empty list
        w_i32(
            p,
            if self.empty_optional_dependents {
                0
            } else {
                -1
            },
        )?;
        w_null(p) // certDigests: writeStringList(null)
    }

    fn read(p: &In<'_>) -> Result<Self> {
        let path = r_str8(p)?;
        let package_name = r_str8(p)?;
        let code_paths = if r_i32(p)? != 0 {
            r_str8_array(p)?
        } else {
            None
        };
        let name = r_str8(p)?;
        let version = r_i64(p)?;
        let kind = r_i32(p)?;
        r_null(p, "android.content.pm.SharedLibraryInfo.declaringPackage")?;
        r_null(p, "android.content.pm.SharedLibraryInfo.dependentPackages")?;
        let dependencies = r_typed_list(p)?;
        let is_native = r_bool(p)?;
        let empty_optional_dependents = match r_i32(p)? {
            -1 => false,
            0 => true,
            _ => {
                return Err(crate::refuse(
                    "android.content.pm.SharedLibraryInfo.optionalDependentPackages",
                ));
            }
        };
        r_null(p, "android.content.pm.SharedLibraryInfo.certDigests")?;
        Ok(SharedLibraryInfo {
            path,
            package_name,
            code_paths,
            name,
            version,
            kind,
            dependencies,
            is_native,
            empty_optional_dependents,
        })
    }
}

/// Parcelling.BuiltIn.ForStringSet (com/android/internal/util/Parcelling.java): -1 for null, else the count and each String16. Readers turn an empty set into null, so empty = null here.
pub fn w_string_set(p: &mut In<'_>, v: &[Option<String>]) -> Result<()> {
    if v.is_empty() {
        return w_i32(p, -1);
    }
    w_i32(p, len_i32(v.len())?)?;
    v.iter().try_for_each(|s| w_str16(p, s))
}
pub fn r_string_set(p: &In<'_>) -> Result<Vec<Option<String>>> {
    let n = r_i32(p)?;
    (0..n.max(0)).map(|_| r_str16(p)).collect()
}

/// Parcelling.BuiltIn.ForBoolean: null 1, false 0, true -1.
pub fn w_opt_bool(p: &mut In<'_>, v: Option<bool>) -> Result<()> {
    w_i32(
        p,
        match v {
            None => 1,
            Some(false) => 0,
            Some(true) => -1,
        },
    )
}
pub fn r_opt_bool(p: &In<'_>) -> Result<Option<bool>> {
    match r_i32(p)? {
        1 => Ok(None),
        0 => Ok(Some(false)),
        -1 => Ok(Some(true)),
        _ => Err(binder::StatusCode::BAD_VALUE),
    }
}

/// TextUtils.writeToParcel / CHAR_SEQUENCE_CREATOR (android/text/TextUtils.java) of plain text: 1, then the String8. Spanned text (kind 0, spans) is refused.
pub fn w_char_sequence(p: &mut In<'_>, v: &Option<String>) -> Result<()> {
    w_i32(p, 1)?;
    w_str8(p, v)
}
pub fn r_char_sequence(p: &In<'_>) -> Result<Option<String>> {
    let kind = r_i32(p)?;
    let s = r_str8(p)?;
    if kind != 1 {
        return Err(crate::refuse("android.text.Spanned"));
    }
    Ok(s)
}

/// android.graphics.Rect (frameworks/base/graphics/java/android/graphics/Rect.java writeToParcel / readFromParcel).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Rect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}
impl Part for Rect {
    fn write(&self, p: &mut In<'_>) -> Result<()> {
        w_i32(p, self.left)?;
        w_i32(p, self.top)?;
        w_i32(p, self.right)?;
        w_i32(p, self.bottom)
    }
    fn read(p: &In<'_>) -> Result<Self> {
        Ok(Rect {
            left: r_i32(p)?,
            top: r_i32(p)?,
            right: r_i32(p)?,
            bottom: r_i32(p)?,
        })
    }
}

/// android.content.ComponentName (content/ComponentName.java): the static writeToParcel(ComponentName, Parcel) / readFromParcel(Parcel) pair, two String16s; a null name is a null package.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ComponentName {
    pub package: String,
    pub class: String,
}
pub fn w_component(p: &mut In<'_>, v: &Option<ComponentName>) -> Result<()> {
    match v {
        None => w_str16(p, &None),
        Some(c) => {
            w_str16(p, &Some(c.package.clone()))?;
            w_str16(p, &Some(c.class.clone()))
        }
    }
}
pub fn r_component(p: &In<'_>) -> Result<Option<ComponentName>> {
    let Some(package) = r_str16(p)? else {
        return Ok(None);
    };
    // ComponentName(Parcel) throws on a null class
    let class = r_str16(p)?.ok_or(binder::StatusCode::UNEXPECTED_NULL)?;
    Ok(Some(ComponentName { package, class }))
}

/// android.net.Uri (net/Uri.java writeToParcel(Parcel, Uri) / CREATOR): 0 for null, StringUri = 1 + String8. OpaqueUri (2) and HierarchicalUri (3) are refused.
pub fn w_uri(p: &mut In<'_>, v: &Option<String>) -> Result<()> {
    match v {
        None => w_i32(p, 0),
        Some(_) => {
            w_i32(p, 1)?;
            w_str8(p, v)
        }
    }
}
pub fn r_uri(p: &In<'_>) -> Result<Option<String>> {
    match r_i32(p)? {
        0 => Ok(None),
        1 => r_str8(p),
        _ => Err(crate::refuse("android.net.Uri (opaque / hierarchical)")),
    }
}

/// android.content.pm.PackageItemInfo (content/pm/PackageItemInfo.java writeToParcel / PackageItemInfo(Parcel)); metaData is a Bundle: null only.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PackageItemInfo {
    pub name: Option<String>,
    pub package_name: Option<String>,
    pub label_res: i32,
    pub non_localized_label: Option<String>,
    pub icon: i32,
    pub logo: i32,
    pub banner: i32,
    pub show_user_icon: i32,
    pub is_archived: bool,
}
impl Part for PackageItemInfo {
    fn write(&self, p: &mut In<'_>) -> Result<()> {
        w_str8(p, &self.name)?;
        w_str8(p, &self.package_name)?;
        w_i32(p, self.label_res)?;
        w_char_sequence(p, &self.non_localized_label)?;
        w_i32(p, self.icon)?;
        w_i32(p, self.logo)?;
        w_null(p)?; // metaData
        w_i32(p, self.banner)?;
        w_i32(p, self.show_user_icon)?;
        w_bool(p, self.is_archived)
    }
    fn read(p: &In<'_>) -> Result<Self> {
        let name = r_str8(p)?;
        let package_name = r_str8(p)?;
        let label_res = r_i32(p)?;
        let non_localized_label = r_char_sequence(p)?;
        let icon = r_i32(p)?;
        let logo = r_i32(p)?;
        r_null(p, "android.content.pm.PackageItemInfo.metaData")?;
        Ok(PackageItemInfo {
            name,
            package_name,
            label_res,
            non_localized_label,
            icon,
            logo,
            banner: r_i32(p)?,
            show_user_icon: r_i32(p)?,
            is_archived: r_bool(p)?,
        })
    }
}

/// android.content.pm.ComponentInfo (content/pm/ComponentInfo.java writeToParcel / ComponentInfo(Parcel)): PackageItemInfo, then the ApplicationInfo in place.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ComponentInfo {
    pub item: PackageItemInfo,
    pub application_info: ApplicationInfo,
    pub process_name: Option<String>,
    pub split_name: Option<String>,
    pub attribution_tags: Option<Vec<Option<String>>>,
    pub description_res: i32,
    pub enabled: bool,
    pub exported: bool,
    pub direct_boot_aware: bool,
}
impl Part for ComponentInfo {
    fn write(&self, p: &mut In<'_>) -> Result<()> {
        self.item.write(p)?;
        self.application_info.write(p)?;
        w_str8(p, &self.process_name)?;
        w_str8(p, &self.split_name)?;
        w_str8_array(p, &self.attribution_tags)?;
        w_i32(p, self.description_res)?;
        w_bool(p, self.enabled)?;
        w_bool(p, self.exported)?;
        w_bool(p, self.direct_boot_aware)
    }
    fn read(p: &In<'_>) -> Result<Self> {
        Ok(ComponentInfo {
            item: PackageItemInfo::read(p)?,
            application_info: ApplicationInfo::read(p)?,
            process_name: r_str8(p)?,
            split_name: r_str8(p)?,
            attribution_tags: r_str8_array(p)?,
            description_res: r_i32(p)?,
            enabled: r_bool(p)?,
            exported: r_bool(p)?,
            direct_boot_aware: r_bool(p)?,
        })
    }
}

/// android.app.WindowConfiguration (app/WindowConfiguration.java writeToParcel / readFromParcel), in place inside Configuration.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct WindowConfiguration {
    pub bounds: Rect,
    pub app_bounds: Option<Rect>,
    pub max_bounds: Rect,
    pub windowing_mode: i32,
    pub activity_type: i32,
    pub always_on_top: i32,
    pub rotation: i32,
    pub display_rotation: i32,
}
impl Part for WindowConfiguration {
    fn write(&self, p: &mut In<'_>) -> Result<()> {
        self.bounds.write(p)?;
        w_typed(p, &self.app_bounds)?;
        self.max_bounds.write(p)?;
        w_i32(p, self.windowing_mode)?;
        w_i32(p, self.activity_type)?;
        w_i32(p, self.always_on_top)?;
        w_i32(p, self.rotation)?;
        w_i32(p, self.display_rotation)
    }
    fn read(p: &In<'_>) -> Result<Self> {
        Ok(WindowConfiguration {
            bounds: Rect::read(p)?,
            app_bounds: r_typed(p)?,
            max_bounds: Rect::read(p)?,
            windowing_mode: r_i32(p)?,
            activity_type: r_i32(p)?,
            always_on_top: r_i32(p)?,
            rotation: r_i32(p)?,
            display_rotation: r_i32(p)?,
        })
    }
}

/// android.window.ActivityWindowInfo (window/ActivityWindowInfo.java writeToParcel / ActivityWindowInfo(Parcel)).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ActivityWindowInfo {
    pub is_embedded: bool,
    pub task_bounds: Rect,
    pub task_fragment_bounds: Rect,
}
impl Part for ActivityWindowInfo {
    fn write(&self, p: &mut In<'_>) -> Result<()> {
        w_bool(p, self.is_embedded)?;
        self.task_bounds.write(p)?;
        self.task_fragment_bounds.write(p)
    }
    fn read(p: &In<'_>) -> Result<Self> {
        Ok(ActivityWindowInfo {
            is_embedded: r_bool(p)?,
            task_bounds: Rect::read(p)?,
            task_fragment_bounds: Rect::read(p)?,
        })
    }
}

/// android.app.servertransaction.LaunchActivityItem (app/servertransaction/LaunchActivityItem.java writeToParcel / LaunchActivityItem(Parcel)), the body after ClientTransaction's class name.
/// Not modelled, null only: state (Bundle), persistentState (PersistableBundle), pendingResults, pendingNewIntents, sceneTransitionInfo, profilerInfo.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LaunchActivityItem {
    pub activity_token: Option<SpIBinder>,
    pub ident: i32,
    pub cur_config: Option<Configuration>,
    pub override_config: Option<Configuration>,
    pub device_id: i32,
    pub referrer: Option<String>,
    pub voice_interactor: Option<SpIBinder>,
    pub proc_state: i32,
    pub is_forward: bool,
    pub assist_token: Option<SpIBinder>,
    pub activity_client_controller: Option<SpIBinder>,
    pub shareable_activity_token: Option<SpIBinder>,
    pub launched_from_bubble: bool,
    pub task_fragment_token: Option<SpIBinder>,
    pub initial_caller_info_access_token: Option<SpIBinder>,
    pub activity_window_info: Option<ActivityWindowInfo>,
    pub intent: Option<Intent>,
    pub info: Option<ActivityInfo>,
}
impl Part for LaunchActivityItem {
    fn write(&self, p: &mut In<'_>) -> Result<()> {
        w_binder(p, &self.activity_token)?;
        w_i32(p, self.ident)?;
        w_typed(p, &self.cur_config)?;
        w_typed(p, &self.override_config)?;
        w_i32(p, self.device_id)?;
        w_str16(p, &self.referrer)?;
        w_binder(p, &self.voice_interactor)?;
        w_i32(p, self.proc_state)?;
        w_null(p)?; // state
        w_null(p)?; // persistentState
        w_null(p)?; // pendingResults
        w_null(p)?; // pendingNewIntents
        w_present(p, false)?; // sceneTransitionInfo
        w_bool(p, self.is_forward)?;
        w_present(p, false)?; // profilerInfo
        w_binder(p, &self.assist_token)?;
        w_binder(p, &self.activity_client_controller)?;
        w_binder(p, &self.shareable_activity_token)?;
        w_bool(p, self.launched_from_bubble)?;
        w_binder(p, &self.task_fragment_token)?;
        w_binder(p, &self.initial_caller_info_access_token)?;
        w_typed(p, &self.activity_window_info)?;
        w_typed(p, &self.intent)?;
        w_typed(p, &self.info)
    }
    fn read(p: &In<'_>) -> Result<Self> {
        let activity_token = r_binder(p)?;
        let ident = r_i32(p)?;
        let cur_config = r_typed(p)?;
        let override_config = r_typed(p)?;
        let device_id = r_i32(p)?;
        let referrer = r_str16(p)?;
        let voice_interactor = r_binder(p)?;
        let proc_state = r_i32(p)?;
        r_null(p, "android.app.servertransaction.LaunchActivityItem.state")?;
        r_null(
            p,
            "android.app.servertransaction.LaunchActivityItem.persistentState",
        )?;
        r_null(
            p,
            "android.app.servertransaction.LaunchActivityItem.pendingResults",
        )?;
        r_null(
            p,
            "android.app.servertransaction.LaunchActivityItem.pendingNewIntents",
        )?;
        r_absent(
            p,
            "android.app.servertransaction.LaunchActivityItem.sceneTransitionInfo",
        )?;
        let is_forward = r_bool(p)?;
        r_absent(
            p,
            "android.app.servertransaction.LaunchActivityItem.profilerInfo",
        )?;
        Ok(LaunchActivityItem {
            activity_token,
            ident,
            cur_config,
            override_config,
            device_id,
            referrer,
            voice_interactor,
            proc_state,
            is_forward,
            assist_token: r_binder(p)?,
            activity_client_controller: r_binder(p)?,
            shareable_activity_token: r_binder(p)?,
            launched_from_bubble: r_bool(p)?,
            task_fragment_token: r_binder(p)?,
            initial_caller_info_access_token: r_binder(p)?,
            activity_window_info: r_typed(p)?,
            intent: r_typed(p)?,
            info: r_typed(p)?,
        })
    }
}
