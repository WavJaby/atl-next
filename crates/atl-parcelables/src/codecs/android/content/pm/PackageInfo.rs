// SPDX-License-Identifier: MIT
//! android.content.pm.PackageInfo: content/pm/PackageInfo.java writeToParcel (:581) / PackageInfo(Parcel) (:659), squashing allowed around the whole value: every component's ApplicationInfo
//! after the first copy is a back-reference (`crate::squash`).
//! Component, permission and signature arrays: writeTypedArray (-1 = null, else the count and each element as writeTypedObject). Not modelled, null only: configPreferences,
//! reqFeatures, featureGroups, attributions (typed arrays).

use super::ActivityInfo::ActivityInfo;
use super::ApplicationInfo::ApplicationInfo;
use super::InstrumentationInfo::InstrumentationInfo;
use super::PermissionInfo::PermissionInfo;
use super::ProviderInfo::ProviderInfo;
use super::ServiceInfo::ServiceInfo;
use crate::parcel::*;
use crate::parts::*;

/// A writeTypedArray; None = null.
pub type TypedArray<T> = Option<Vec<Option<T>>>;

#[derive(Debug, Clone, Default)]
pub struct PackageInfo {
    pub package_name: Option<String>,
    pub split_names: Option<Vec<Option<String>>>,
    pub version_code: i32,
    pub version_code_major: i32,
    pub version_name: Option<String>,
    pub base_revision_code: i32,
    pub split_revision_codes: Option<Vec<i32>>,
    pub shared_user_id: Option<String>,
    pub shared_user_label: i32,
    pub application_info: Option<ApplicationInfo>,
    pub first_install_time: i64,
    pub last_update_time: i64,
    pub gids: Option<Vec<i32>>,
    pub activities: TypedArray<ActivityInfo>,
    pub receivers: TypedArray<ActivityInfo>,
    pub services: TypedArray<ServiceInfo>,
    pub providers: TypedArray<ProviderInfo>,
    pub instrumentation: TypedArray<InstrumentationInfo>,
    pub permissions: TypedArray<PermissionInfo>,
    pub requested_permissions: Option<Vec<Option<String>>>,
    pub requested_permissions_flags: Option<Vec<i32>>,
    pub signatures: TypedArray<Signature>,
    pub install_location: i32,
    pub is_stub: bool,
    pub core_app: bool,
    pub required_for_all_users: bool,
    pub restricted_account_type: Option<String>,
    pub required_account_type: Option<String>,
    pub overlay_target: Option<String>,
    pub overlay_category: Option<String>,
    pub overlay_priority: i32,
    pub overlay_is_static: bool,
    pub compile_sdk_version: i32,
    pub compile_sdk_version_codename: Option<String>,
    /// SigningInfo: written as its SigningDetails (SigningInfo.writeToParcel:231)
    pub signing_info: Option<SigningDetails>,
    pub is_apex: bool,
    pub is_active_apex: bool,
    pub archive_time_millis: i64,
    pub apex_package_name: Option<String>,
}

/// android.content.pm.Signature (content/pm/Signature.java writeToParcel:285 / Signature(Parcel):306): the certificate's DER bytes as writeByteArray.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Signature {
    pub bytes: Option<Vec<u8>>,
}

impl Part for Signature {
    fn write(&self, p: &mut In<'_>) -> Result<()> {
        // libbinder_rs Option<Vec<u8>> = Parcel::writeByteVector: int32 length or -1, the bytes padded to 4, the bytes Java's writeByteArray writes
        p.write(&self.bytes)
    }
    fn read(p: &In<'_>) -> Result<Self> {
        Ok(Signature { bytes: p.read()? })
    }
}

/// One `writeValue` element of SigningDetails.mPublicKeys (Parcel.writeArraySet:1541): Parcel.VAL_NULL, or a PublicKey as VAL_SERIALIZABLE (Parcel.java:339), length-prefixed (isLengthPrefixed:5107):
/// type, byte length, then writeString(class name) + writeByteArray(Java serialization). Kept as the raw words after the length: only the app's ART deserializes a key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PublicKeyValue {
    Null,
    Serializable(Vec<i32>),
}

const VAL_NULL: i32 = -1;
const VAL_SERIALIZABLE: i32 = 21;

impl Part for PublicKeyValue {
    fn write(&self, p: &mut In<'_>) -> Result<()> {
        match self {
            PublicKeyValue::Null => w_i32(p, VAL_NULL),
            PublicKeyValue::Serializable(words) => {
                w_i32(p, VAL_SERIALIZABLE)?;
                w_i32(p, len_i32(words.len() * 4)?)?;
                words.iter().try_for_each(|w| w_i32(p, *w))
            }
        }
    }
    fn read(p: &In<'_>) -> Result<Self> {
        match r_i32(p)? {
            VAL_NULL => Ok(PublicKeyValue::Null),
            VAL_SERIALIZABLE => {
                let len = r_i32(p)?;
                // every part of a serializable value (String16, byte array) is padded to 4, so the length is whole words
                if len < 0 || len % 4 != 0 {
                    return Err(crate::refuse(
                        "android.content.pm.SigningDetails.mPublicKeys length",
                    ));
                }
                (0..len / 4)
                    .map(|_| r_i32(p))
                    .collect::<Result<_>>()
                    .map(PublicKeyValue::Serializable)
            }
            _ => Err(crate::refuse(
                "android.content.pm.SigningDetails.mPublicKeys value type",
            )),
        }
    }
}

/// android.content.pm.SigningDetails (content/pm/SigningDetails.java writeToParcel:815 / CREATOR:839): writeBoolean(this == UNKNOWN), then for a known one the signatures (typed array), the scheme
/// version, the public keys (writeArraySet, -1 = null) and the lineage (typed array).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SigningDetails {
    Unknown,
    Known {
        signatures: TypedArray<Signature>,
        scheme_version: i32,
        public_keys: Option<Vec<PublicKeyValue>>,
        past_signing_certificates: TypedArray<Signature>,
    },
}

impl Part for SigningDetails {
    fn write(&self, p: &mut In<'_>) -> Result<()> {
        match self {
            SigningDetails::Unknown => w_bool(p, true),
            SigningDetails::Known {
                signatures,
                scheme_version,
                public_keys,
                past_signing_certificates,
            } => {
                w_bool(p, false)?;
                w_typed_list(p, signatures)?;
                w_i32(p, *scheme_version)?;
                match public_keys {
                    None => w_i32(p, -1)?,
                    Some(keys) => {
                        w_i32(p, len_i32(keys.len())?)?;
                        keys.iter().try_for_each(|k| k.write(p))?;
                    }
                }
                w_typed_list(p, past_signing_certificates)
            }
        }
    }
    fn read(p: &In<'_>) -> Result<Self> {
        if r_bool(p)? {
            return Ok(SigningDetails::Unknown);
        }
        let signatures = r_typed_list(p)?;
        let scheme_version = r_i32(p)?;
        let n = r_i32(p)?;
        let public_keys = if n < 0 {
            None
        } else {
            Some(
                (0..n)
                    .map(|_| PublicKeyValue::read(p))
                    .collect::<Result<_>>()?,
            )
        };
        let past_signing_certificates = r_typed_list(p)?;
        Ok(SigningDetails::Known {
            signatures,
            scheme_version,
            public_keys,
            past_signing_certificates,
        })
    }
}

/// The unmodelled typed arrays after signatures, refused when non-null.
const ARRAYS_B: [&str; 4] = [
    "android.content.pm.PackageInfo.configPreferences",
    "android.content.pm.PackageInfo.reqFeatures",
    "android.content.pm.PackageInfo.featureGroups",
    "android.content.pm.PackageInfo.attributions",
];

impl Part for PackageInfo {
    fn write(&self, p: &mut In<'_>) -> Result<()> {
        crate::squash::allow(crate::squash::key(p), || self.write_fields(p))
    }

    fn read(p: &In<'_>) -> Result<Self> {
        crate::squash::allow(crate::squash::key(p), || Self::read_fields(p))
    }
}

impl PackageInfo {
    fn write_fields(&self, p: &mut In<'_>) -> Result<()> {
        w_str8(p, &self.package_name)?;
        w_str8_array(p, &self.split_names)?;
        w_i32(p, self.version_code)?;
        w_i32(p, self.version_code_major)?;
        w_str8(p, &self.version_name)?;
        w_i32(p, self.base_revision_code)?;
        w_i32_array(p, &self.split_revision_codes)?;
        w_str8(p, &self.shared_user_id)?;
        w_i32(p, self.shared_user_label)?;
        // `hasApp` int, then the ApplicationInfo in place (not writeTypedObject: same bytes)
        w_typed(p, &self.application_info)?;
        w_i64(p, self.first_install_time)?;
        w_i64(p, self.last_update_time)?;
        w_i32_array(p, &self.gids)?;
        w_typed_list(p, &self.activities)?;
        w_typed_list(p, &self.receivers)?;
        w_typed_list(p, &self.services)?;
        w_typed_list(p, &self.providers)?;
        w_typed_list(p, &self.instrumentation)?;
        w_typed_list(p, &self.permissions)?;
        w_str8_array(p, &self.requested_permissions)?;
        w_i32_array(p, &self.requested_permissions_flags)?;
        w_typed_list(p, &self.signatures)?;
        ARRAYS_B.iter().try_for_each(|_| w_null(p))?;
        w_i32(p, self.install_location)?;
        w_bool(p, self.is_stub)?;
        w_bool(p, self.core_app)?;
        w_bool(p, self.required_for_all_users)?;
        w_str8(p, &self.restricted_account_type)?;
        w_str8(p, &self.required_account_type)?;
        w_str8(p, &self.overlay_target)?;
        w_str8(p, &self.overlay_category)?;
        w_i32(p, self.overlay_priority)?;
        w_bool(p, self.overlay_is_static)?;
        w_i32(p, self.compile_sdk_version)?;
        w_str8(p, &self.compile_sdk_version_codename)?;
        // signingInfo: an int presence flag, then SigningInfo.writeToParcel (PackageInfo.java:627)
        w_typed(p, &self.signing_info)?;
        w_bool(p, self.is_apex)?;
        w_bool(p, self.is_active_apex)?;
        w_i64(p, self.archive_time_millis)?;
        match &self.apex_package_name {
            None => w_i32(p, 0),
            Some(_) => {
                w_i32(p, 1)?;
                w_str8(p, &self.apex_package_name)
            }
        }
    }

    fn read_fields(p: &In<'_>) -> Result<Self> {
        let package_name = r_str8(p)?;
        let split_names = r_str8_array(p)?;
        let version_code = r_i32(p)?;
        let version_code_major = r_i32(p)?;
        let version_name = r_str8(p)?;
        let base_revision_code = r_i32(p)?;
        let split_revision_codes = r_i32_array(p)?;
        let shared_user_id = r_str8(p)?;
        let shared_user_label = r_i32(p)?;
        let application_info = r_typed(p)?;
        let first_install_time = r_i64(p)?;
        let last_update_time = r_i64(p)?;
        let gids = r_i32_array(p)?;
        let activities = r_typed_list(p)?;
        let receivers = r_typed_list(p)?;
        let services = r_typed_list(p)?;
        let providers = r_typed_list(p)?;
        let instrumentation = r_typed_list(p)?;
        let permissions = r_typed_list(p)?;
        let requested_permissions = r_str8_array(p)?;
        let requested_permissions_flags = r_i32_array(p)?;
        let signatures = r_typed_list(p)?;
        for what in ARRAYS_B {
            r_null(p, what)?;
        }
        let install_location = r_i32(p)?;
        let is_stub = r_bool(p)?;
        let core_app = r_bool(p)?;
        let required_for_all_users = r_bool(p)?;
        let restricted_account_type = r_str8(p)?;
        let required_account_type = r_str8(p)?;
        let overlay_target = r_str8(p)?;
        let overlay_category = r_str8(p)?;
        let overlay_priority = r_i32(p)?;
        let overlay_is_static = r_bool(p)?;
        let compile_sdk_version = r_i32(p)?;
        let compile_sdk_version_codename = r_str8(p)?;
        let signing_info = r_typed(p)?;
        let is_apex = r_bool(p)?;
        let is_active_apex = r_bool(p)?;
        let archive_time_millis = r_i64(p)?;
        let apex_package_name = if r_i32(p)? != 0 { r_str8(p)? } else { None };
        Ok(PackageInfo {
            package_name,
            split_names,
            version_code,
            version_code_major,
            version_name,
            base_revision_code,
            split_revision_codes,
            shared_user_id,
            shared_user_label,
            application_info,
            first_install_time,
            last_update_time,
            gids,
            activities,
            receivers,
            services,
            providers,
            instrumentation,
            permissions,
            requested_permissions,
            requested_permissions_flags,
            signatures,
            install_location,
            is_stub,
            core_app,
            required_for_all_users,
            restricted_account_type,
            required_account_type,
            overlay_target,
            overlay_category,
            overlay_priority,
            overlay_is_static,
            compile_sdk_version,
            compile_sdk_version_codename,
            signing_info,
            is_apex,
            is_active_apex,
            archive_time_millis,
            apex_package_name,
        })
    }
}

crate::codec!(PackageInfo);
