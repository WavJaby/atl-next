// SPDX-License-Identifier: MIT
//! The `sec_key_att_app_id_provider` service (keystore2's attestation application id source): the converter's IKeyAttestationApplicationIdProvider stub, reproducing AOSP's KeyAttestationApplicationIdProviderService (frameworks/base/services/core/java/com/android/server/security/KeyAttestationApplicationIdProviderService.java). Keystore asks for a challenged key's ATTESTATION_APPLICATION_ID (system/security/keystore/keystore_attestation_id.cpp gather_attestation_application_id): the packages of the caller's app id, their version codes and their signing certificates as the installer parsed them from the installed APKs (DER, what Signature.toByteArray returns). Only keystore and credstore may ask (KeyAttestationApplicationIdProviderService.java:50-54). A package whose certificate is missing fails the request: keystore's digest of an empty certificate would be an invented identity.
//! ATL has one user (system, 0): packages are looked up by app id as PackageManager.getPackagesForUid does (package.rs r#getPackagesForUid), not per user.

use std::sync::Arc;

use atl_aidl::aidl::android::security::keystore::IKeyAttestationApplicationIdProvider::{
    BnKeyAttestationApplicationIdProvider, IKeyAttestationApplicationIdProvider,
};
use atl_aidl::aidl::android::security::keystore::KeyAttestationApplicationId::KeyAttestationApplicationId;
use atl_aidl::aidl::android::security::keystore::KeyAttestationPackageInfo::KeyAttestationPackageInfo;
use atl_aidl::aidl::android::security::keystore::Signature::Signature;
use atl_services_types::permission;
use binder::{BinderFeatures, ExceptionCode, Interface, SpIBinder, Status, ThreadState};

use crate::packages::{PackageRecord, Packages};

/// AID_KEYSTORE and AID_CREDSTORE (system/core/libcutils/include/private/android_filesystem_config.h:68, :127): the only callers KeyAttestationApplicationIdProviderService admits.
const KEYSTORE_UID: u32 = 1017;
const CREDSTORE_UID: u32 = 1076;
/// IKeyAttestationApplicationIdProvider.ERROR_GET_ATTESTATION_APPLICATION_ID_FAILED (frameworks/base/keystore/aaid/aidl/android/security/keystore/IKeyAttestationApplicationIdProvider.aidl:23).
const ERROR_GET_ATTESTATION_APPLICATION_ID_FAILED: i32 = 1;
/// PackageInfo.composeLongVersionCode (PackageInfo.java:93-95): the major version code is the high word.
const VERSION_CODE_MAJOR_SHIFT: u32 = 32;

struct AttestationAppIds {
    packages: Arc<Packages>,
}

impl Interface for AttestationAppIds {}

/// PackageInfo.composeLongVersionCode: (major << 32) | (minor & 0xffffffff), the long version code keystore carries in the record.
fn long_version_code(major: i32, minor: i32) -> i64 {
    (i64::from(major) << VERSION_CODE_MAJOR_SHIFT) | (i64::from(minor) & 0xffff_ffff)
}

fn service_error(message: String) -> Status {
    Status::new_service_specific_error_str(
        ERROR_GET_ATTESTATION_APPLICATION_ID_FAILED,
        Some(message),
    )
}

/// One package's entry: its long version code and every certificate of its stored signing info, the DER bytes the installer kept (getPackageInfo(GET_SIGNATURES).signatures, package.rs:586-588).
fn package_entry(p: &PackageRecord) -> Result<KeyAttestationPackageInfo, String> {
    let name = p.name();
    let missing = || format!("package {name} has no signing certificate");
    let certs = p.info.signatures.as_ref().ok_or_else(missing)?;
    let mut signatures = Vec::with_capacity(certs.len());
    for cert in certs {
        let der = cert
            .as_ref()
            .and_then(|c| c.bytes.as_ref())
            .ok_or_else(missing)?;
        signatures.push(Some(Signature {
            data: Some(der.clone()),
        }));
    }
    Ok(KeyAttestationPackageInfo {
        packageName: Some(name.to_owned()),
        versionCode: long_version_code(p.info.version_code_major, p.info.version_code),
        signatures: Some(signatures),
    })
}

impl IKeyAttestationApplicationIdProvider for AttestationAppIds {
    fn r#getKeyAttestationApplicationId(
        &self,
        uid: i32,
    ) -> binder::Result<Option<KeyAttestationApplicationId>> {
        let caller = ThreadState::get_calling_uid();
        if caller != KEYSTORE_UID && caller != CREDSTORE_UID {
            return Err(Status::new_exception_str(
                ExceptionCode::SECURITY,
                Some("This service can only be used by Keystore or Credstore"),
            ));
        }
        // getPackagesForUid's answer (AOSP reads it with the caller cleared, so no visibility check here); none -> the service-specific error
        let packages: Vec<&PackageRecord> = match u32::try_from(uid) {
            Ok(uid) => self.packages.by_app_id(permission::app_id(uid)).collect(),
            Err(_) => Vec::new(),
        };
        if packages.is_empty() {
            return Err(service_error(format!("No package for uid: {uid}")));
        }
        let mut infos = Vec::with_capacity(packages.len());
        for p in packages {
            infos.push(Some(package_entry(p).map_err(service_error)?));
        }
        Ok(Some(KeyAttestationApplicationId {
            packageInfos: Some(infos),
        }))
    }
}

/// The service's binder, registered as `sec_key_att_app_id_provider` (system_services.rs).
pub fn new_binder(packages: Arc<Packages>) -> SpIBinder {
    BnKeyAttestationApplicationIdProvider::new_binder(
        AttestationAppIds { packages },
        BinderFeatures::default(),
    )
    .as_binder()
}

#[cfg(test)]
mod tests {
    use super::long_version_code;

    #[test]
    fn long_version_code_composes_major_high_minor_low() {
        // 1 << 32 | 2 by hand; minor -1 is the all-ones low word, not sign-extended into the high word
        assert_eq!(long_version_code(1, 2), 4_294_967_298);
        assert_eq!(long_version_code(0, -1), 0xffff_ffff);
    }
}
