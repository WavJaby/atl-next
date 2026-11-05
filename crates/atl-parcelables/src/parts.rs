// SPDX-License-Identifier: MIT
//! Parcel layouts the codecs share or embed without being AIDL parcelables themselves (base classes, values written in place), each from its AOSP writeToParcel / reader.
//! Paths in docs are below frameworks/base/core/java unless stated.

use binder::SpIBinder;

use crate::codecs::android::content::Intent::Intent;
use crate::codecs::android::content::pm::ActivityInfo::ActivityInfo;
use crate::codecs::android::content::pm::ApplicationInfo::ApplicationInfo;
use crate::codecs::android::content::res::Configuration::Configuration;
use crate::codecs::android::util::MergedConfiguration::MergedConfiguration;
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

/// TextUtils.writeToParcel / CHAR_SEQUENCE_CREATOR (android/text/TextUtils.java) of plain text: 1, then the String8. Read also takes kind 0 (Spanned: String8, then per ParcelableSpan its type id, payload, where; id 0 ends) when it carries no span -- an EditText's
/// text, whose selection / watcher spans are not ParcelableSpan -- as its text; a span's payload layout depends on its type, so a Spanned with any span is refused.
pub fn w_char_sequence(p: &mut In<'_>, v: &Option<String>) -> Result<()> {
    w_i32(p, 1)?;
    w_str8(p, v)
}
pub fn r_char_sequence(p: &In<'_>) -> Result<Option<String>> {
    let kind = r_i32(p)?;
    let s = r_str8(p)?;
    match kind {
        1 => Ok(s),
        0 if r_i32(p)? == 0 => Ok(s),
        _ => Err(crate::refuse(
            "android.text.Spanned (with a ParcelableSpan)",
        )),
    }
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

/// android.os.storage.StorageVolume (os/storage/StorageVolume.java writeToParcel:546-565, StorageVolume(Parcel):158-175), field for field.
/// `owner` is a UserHandle written by writeParcelable: class name (String16), then the int handle; null is String16 -1. `uuid` is StorageManager.convert's string behind an int flag, kept as read.
/// The paths are non-null: `new File(null)` throws on the Java side, so a null path is refused and counted.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StorageVolume {
    pub id: Option<String>,
    pub path: String,
    pub internal_path: String,
    pub description: Option<String>,
    pub primary: bool,
    pub removable: bool,
    pub emulated: bool,
    pub externally_managed: bool,
    pub allow_mass_storage: bool,
    pub max_file_size: i64,
    pub owner: Option<i32>,
    pub uuid: Option<String>,
    pub fs_uuid: Option<String>,
    pub state: Option<String>,
}
/// The class name writeParcelable writes for a UserHandle (android.os.UserHandle).
const USER_HANDLE: &str = "android.os.UserHandle";
impl Part for StorageVolume {
    fn write(&self, p: &mut In<'_>) -> Result<()> {
        w_str8(p, &self.id)?;
        w_str8(p, &Some(self.path.clone()))?;
        w_str8(p, &Some(self.internal_path.clone()))?;
        w_str8(p, &self.description)?;
        w_bool(p, self.primary)?;
        w_bool(p, self.removable)?;
        w_bool(p, self.emulated)?;
        w_bool(p, self.externally_managed)?;
        w_bool(p, self.allow_mass_storage)?;
        w_i64(p, self.max_file_size)?;
        match self.owner {
            None => w_null(p)?,
            Some(user) => {
                w_str16(p, &Some(USER_HANDLE.to_owned()))?;
                w_i32(p, user)?;
            }
        }
        match &self.uuid {
            None => w_bool(p, false)?,
            Some(uuid) => {
                w_bool(p, true)?;
                w_str8(p, &Some(uuid.clone()))?;
            }
        }
        w_str8(p, &self.fs_uuid)?;
        w_str8(p, &self.state)
    }
    fn read(p: &In<'_>) -> Result<Self> {
        let id = r_str8(p)?;
        let path = r_str8(p)?.ok_or_else(|| crate::refuse("StorageVolume.path: null"))?;
        let internal_path =
            r_str8(p)?.ok_or_else(|| crate::refuse("StorageVolume.internalPath: null"))?;
        let description = r_str8(p)?;
        let primary = r_bool(p)?;
        let removable = r_bool(p)?;
        let emulated = r_bool(p)?;
        let externally_managed = r_bool(p)?;
        let allow_mass_storage = r_bool(p)?;
        let max_file_size = r_i64(p)?;
        let owner = match r_str16(p)? {
            None => None,
            Some(class) if class == USER_HANDLE => Some(r_i32(p)?),
            Some(_) => return Err(crate::refuse("StorageVolume.owner: not a UserHandle")),
        };
        let uuid = if r_bool(p)? {
            Some(r_str8(p)?.ok_or_else(|| crate::refuse("StorageVolume.uuid: null"))?)
        } else {
            None
        };
        let fs_uuid = r_str8(p)?;
        let state = r_str8(p)?;
        Ok(StorageVolume {
            id,
            path,
            internal_path,
            description,
            primary,
            removable,
            emulated,
            externally_managed,
            allow_mass_storage,
            max_file_size,
            owner,
            uuid,
            fs_uuid,
            state,
        })
    }
}

/// android.view.InsetsSource (view/InsetsSource.java writeToParcel:689-700, InsetsSource(Parcel):668-680): one inset source of an InsetsState, written as writeTypedObject inside it. Order:
/// id, type, frame, visibleFrame (presence, Rect), visible, flags, sideHint, boundingRects (writeTypedArray: -1 null or count, each element with its own presence). Every field is modelled.
/// A layout, not a codec: no .aidl declares InsetsSource (the converter takes codecs only for declared parcelables).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct InsetsSource {
    pub id: i32,
    pub r#type: i32,
    pub frame: Rect,
    pub visible_frame: Option<Rect>,
    pub visible: bool,
    pub flags: i32,
    pub side_hint: i32,
    pub bounding_rects: Option<Vec<Option<Rect>>>,
}
impl Part for InsetsSource {
    fn write(&self, p: &mut In<'_>) -> Result<()> {
        w_i32(p, self.id)?;
        w_i32(p, self.r#type)?;
        self.frame.write(p)?;
        w_typed(p, &self.visible_frame)?;
        w_bool(p, self.visible)?;
        w_i32(p, self.flags)?;
        w_i32(p, self.side_hint)?;
        w_typed_list(p, &self.bounding_rects)
    }
    fn read(p: &In<'_>) -> Result<Self> {
        Ok(InsetsSource {
            id: r_i32(p)?,
            r#type: r_i32(p)?,
            frame: Rect::read(p)?,
            visible_frame: r_typed(p)?,
            visible: r_bool(p)?,
            flags: r_i32(p)?,
            side_hint: r_i32(p)?,
            bounding_rects: r_typed_list(p)?,
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

/// android.net.Uri (net/Uri.java writeToParcel(Parcel, Uri) / CREATOR): 0 for null, else the type int and String8. StringUri (1), OpaqueUri (2) and HierarchicalUri (3) all write toString() that way (Uri.java:520, :899, :1225); writing uses StringUri's type.
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
        1..=3 => r_str8(p),
        _ => Err(crate::refuse("android.net.Uri (unknown type)")),
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
/// Not modelled, null only: state (Bundle), persistentState (PersistableBundle), pendingResults, pendingNewIntents, sceneTransitionInfo, profilerInfo. No `PartialEq` (its Intent has none).
#[derive(Debug, Clone, Default)]
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

/// android.app.servertransaction.ResumeActivityItem (ResumeActivityItem.java writeToParcel: ActivityTransactionItem's activity token, then mProcState, mIsForward and
/// mShouldSendCompatFakeFocus), the body after ClientTransaction's class name. Its token is a strong binder; the item is not a codec of its own (ClientTransaction holds it).
#[derive(Debug, Clone, Default)]
pub struct ResumeActivityItem {
    pub activity_token: Option<SpIBinder>,
    /// ActivityManager.ProcessState; -1 (PROCESS_STATE_UNKNOWN) when the item does not update the process state.
    pub proc_state: i32,
    pub is_forward: bool,
    pub should_send_compat_fake_focus: bool,
}
impl Part for ResumeActivityItem {
    fn write(&self, p: &mut In<'_>) -> Result<()> {
        w_binder(p, &self.activity_token)?;
        w_i32(p, self.proc_state)?;
        w_bool(p, self.is_forward)?;
        w_bool(p, self.should_send_compat_fake_focus)
    }
    fn read(p: &In<'_>) -> Result<Self> {
        Ok(ResumeActivityItem {
            activity_token: r_binder(p)?,
            proc_state: r_i32(p)?,
            is_forward: r_bool(p)?,
            should_send_compat_fake_focus: r_bool(p)?,
        })
    }
}

/// android.app.servertransaction.ActivityRelaunchItem (ActivityRelaunchItem.java writeToParcel): the token, the pending results and new intents (typed lists; only null modelled: the services hold neither), the
/// MergedConfiguration and ActivityWindowInfo as typed objects, the config changes, preserveWindow. The body after ClientTransaction's class name.
#[derive(Debug, Clone, Default)]
pub struct ActivityRelaunchItem {
    pub activity_token: Option<SpIBinder>,
    pub config: Option<MergedConfiguration>,
    pub activity_window_info: Option<ActivityWindowInfo>,
    pub config_changes: i32,
    pub preserve_window: bool,
}

impl Part for ActivityRelaunchItem {
    fn write(&self, p: &mut In<'_>) -> Result<()> {
        w_binder(p, &self.activity_token)?;
        w_null(p)?;
        w_null(p)?;
        w_typed(p, &self.config)?;
        w_typed(p, &self.activity_window_info)?;
        w_i32(p, self.config_changes)?;
        w_bool(p, self.preserve_window)
    }

    fn read(p: &In<'_>) -> Result<Self> {
        let activity_token = r_binder(p)?;
        r_null(p, "ActivityRelaunchItem.mPendingResults")?;
        r_null(p, "ActivityRelaunchItem.mPendingNewIntents")?;
        Ok(ActivityRelaunchItem {
            activity_token,
            config: r_typed(p)?,
            activity_window_info: r_typed(p)?,
            config_changes: r_i32(p)?,
            preserve_window: r_bool(p)?,
        })
    }
}

/// android.app.servertransaction.ConfigurationChangeItem (ConfigurationChangeItem.java:63-66 writeToParcel): the process's new global Configuration as a typed object (non-null on read), then the device id.
/// The body after ClientTransaction's class name; the item is not a codec of its own.
#[derive(Debug, Clone, Default)]
pub struct ConfigurationChangeItem {
    pub configuration: Configuration,
    /// mDeviceId: Context.DEVICE_ID_DEFAULT (0) for the default device
    pub device_id: i32,
}
impl Part for ConfigurationChangeItem {
    fn write(&self, p: &mut In<'_>) -> Result<()> {
        w_typed(p, &Some(self.configuration.clone()))?;
        w_i32(p, self.device_id)
    }
    fn read(p: &In<'_>) -> Result<Self> {
        let configuration = r_typed(p)?.ok_or_else(|| {
            crate::refuse(
                "android.app.servertransaction.ConfigurationChangeItem.mConfiguration (null)",
            )
        })?;
        Ok(ConfigurationChangeItem {
            configuration,
            device_id: r_i32(p)?,
        })
    }
}

/// android.app.servertransaction.ActivityConfigurationChangeItem (ActivityConfigurationChangeItem.java:79-89 writeToParcel): the ActivityTransactionItem's token, then the
/// Configuration and the ActivityWindowInfo as typed objects. The body after ClientTransaction's class name; the item is not a codec of its own.
#[derive(Debug, Clone, Default)]
pub struct ActivityConfigurationChangeItem {
    pub activity_token: Option<SpIBinder>,
    /// the activity's override configuration (AOSP: mergedOverrideConfiguration)
    pub configuration: Option<Configuration>,
    pub activity_window_info: Option<ActivityWindowInfo>,
}
impl Part for ActivityConfigurationChangeItem {
    fn write(&self, p: &mut In<'_>) -> Result<()> {
        w_binder(p, &self.activity_token)?;
        w_typed(p, &self.configuration)?;
        w_typed(p, &self.activity_window_info)
    }
    fn read(p: &In<'_>) -> Result<Self> {
        Ok(ActivityConfigurationChangeItem {
            activity_token: r_binder(p)?,
            configuration: r_typed(p)?,
            activity_window_info: r_typed(p)?,
        })
    }
}

/// android.app.servertransaction.PauseActivityItem (PauseActivityItem.java:84-89 writeToParcel): the ActivityTransactionItem's token, then finished, userLeaving, dontReport and autoEnteringPip as booleans.
/// The body after ClientTransaction's class name; the item is not a codec of its own.
#[derive(Debug, Clone, Default)]
pub struct PauseActivityItem {
    pub activity_token: Option<SpIBinder>,
    pub finished: bool,
    pub user_leaving: bool,
    pub dont_report: bool,
    pub auto_entering_pip: bool,
}
impl Part for PauseActivityItem {
    fn write(&self, p: &mut In<'_>) -> Result<()> {
        w_binder(p, &self.activity_token)?;
        w_bool(p, self.finished)?;
        w_bool(p, self.user_leaving)?;
        w_bool(p, self.dont_report)?;
        w_bool(p, self.auto_entering_pip)
    }
    fn read(p: &In<'_>) -> Result<Self> {
        Ok(PauseActivityItem {
            activity_token: r_binder(p)?,
            finished: r_bool(p)?,
            user_leaving: r_bool(p)?,
            dont_report: r_bool(p)?,
            auto_entering_pip: r_bool(p)?,
        })
    }
}

/// android.app.servertransaction.StopActivityItem (StopActivityItem.java:62-63: no writeToParcel of its own): the ActivityTransactionItem's token alone.
/// The body after ClientTransaction's class name; the item is not a codec of its own.
#[derive(Debug, Clone, Default)]
pub struct StopActivityItem {
    pub activity_token: Option<SpIBinder>,
}
impl Part for StopActivityItem {
    fn write(&self, p: &mut In<'_>) -> Result<()> {
        w_binder(p, &self.activity_token)
    }
    fn read(p: &In<'_>) -> Result<Self> {
        Ok(StopActivityItem {
            activity_token: r_binder(p)?,
        })
    }
}

/// android.app.servertransaction.DestroyActivityItem (DestroyActivityItem.java:73-75 writeToParcel): the token, then finished.
/// The body after ClientTransaction's class name; the item is not a codec of its own.
#[derive(Debug, Clone, Default)]
pub struct DestroyActivityItem {
    pub activity_token: Option<SpIBinder>,
    pub finished: bool,
}
impl Part for DestroyActivityItem {
    fn write(&self, p: &mut In<'_>) -> Result<()> {
        w_binder(p, &self.activity_token)?;
        w_bool(p, self.finished)
    }
    fn read(p: &In<'_>) -> Result<Self> {
        Ok(DestroyActivityItem {
            activity_token: r_binder(p)?,
            finished: r_bool(p)?,
        })
    }
}

/// android.app.servertransaction.StartActivityItem (StartActivityItem.java:63-65 writeToParcel): the token, then the SceneTransitionInfo as a typed object. Only null is modelled
/// (flag 0), as LaunchActivityItem does. The body after ClientTransaction's class name; the item is not a codec of its own.
#[derive(Debug, Clone, Default)]
pub struct StartActivityItem {
    pub activity_token: Option<SpIBinder>,
}
impl Part for StartActivityItem {
    fn write(&self, p: &mut In<'_>) -> Result<()> {
        w_binder(p, &self.activity_token)?;
        w_present(p, false) // sceneTransitionInfo
    }
    fn read(p: &In<'_>) -> Result<Self> {
        let activity_token = r_binder(p)?;
        r_absent(
            p,
            "android.app.servertransaction.StartActivityItem.sceneTransitionInfo",
        )?;
        Ok(StartActivityItem { activity_token })
    }
}

/// android.view.Display.Mode (Display.java:2695 writeToParcel / Mode(Parcel)): a nested class of DisplayInfo, not an AIDL parcelable, so a Part here and no codec file.
/// Its arrays are the Java ones (`createFloatArray` / `createIntArray`): null is -1, which Mode's constructor never produces.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DisplayMode {
    pub mode_id: i32,
    pub width: i32,
    pub height: i32,
    pub peak_refresh_rate: f32,
    pub vsync_rate: f32,
    pub is_synthetic: bool,
    pub alternative_refresh_rates: Option<Vec<f32>>,
    pub supported_hdr_types: Option<Vec<i32>>,
}
impl Part for DisplayMode {
    fn write(&self, p: &mut In<'_>) -> Result<()> {
        w_i32(p, self.mode_id)?;
        w_i32(p, self.width)?;
        w_i32(p, self.height)?;
        w_f32(p, self.peak_refresh_rate)?;
        w_f32(p, self.vsync_rate)?;
        w_bool(p, self.is_synthetic)?;
        w_f32_array(p, &self.alternative_refresh_rates)?;
        w_i32_array(p, &self.supported_hdr_types)
    }
    fn read(p: &In<'_>) -> Result<Self> {
        Ok(DisplayMode {
            mode_id: r_i32(p)?,
            width: r_i32(p)?,
            height: r_i32(p)?,
            peak_refresh_rate: r_f32(p)?,
            vsync_rate: r_f32(p)?,
            is_synthetic: r_bool(p)?,
            alternative_refresh_rates: r_f32_array(p)?,
            supported_hdr_types: r_i32_array(p)?,
        })
    }
}
