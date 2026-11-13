// SPDX-License-Identifier: MIT
//! The `mount` service (`ServiceManager.getService("mount")`, StorageManager.java:326): ATL's one storage volume, the emulated primary, at [`atl_android_paths::SHARED_STORAGE`].
//! StorageManager.getVolumeList (its PropertyInvalidatedCache query, StorageManager.java:322-331) asks for a user's volumes. ATL has one user, the system user (user.rs): it gets the primary, other users none.
//! The flags (FLAG_FOR_WRITE, FLAG_REAL_STATE, ...) are not modelled: the primary is always mounted and writable. Environment.getExternalDirs builds every external path from these volumes (Environment.java:186-194).
//! Every other method keeps the stub's default body: counted by the reply hook (atl-diag) and answered UNKNOWN_TRANSACTION. Not modelled: the shared storage root, the permission-derived mounts and MediaStore; the broker mounts only each app's own Android/data and Android/obb (atld app_spawn).

use atl_aidl::aidl::android::os::storage::IStorageManager::{
    BnStorageManager, EnforcePermission, IStorageManager,
};
use atl_parcelables::android::os::storage::StorageVolume;
use binder::{BinderFeatures, ExceptionCode, Interface, SpIBinder, Status, ThreadState};

use crate::packages::Permissions;
use std::sync::Arc;

/// VolumeInfo.ID_EMULATED_INTERNAL: the id of the emulated primary.
const PRIMARY_ID: &str = "emulated";
/// R.string.storage_internal (frameworks/base/core/res/res/values/strings.xml:4540).
const PRIMARY_DESCRIPTION: &str = "Internal shared storage";
/// Environment.MEDIA_MOUNTED.
const STATE_MOUNTED: &str = "mounted";
/// UserHandle.USER_SYSTEM: the only user ATL has.
const USER_SYSTEM: i32 = 0;

struct StorageService {
    permissions: Arc<Permissions>,
}

impl Interface for StorageService {}

impl IStorageManager for StorageService {
    /// @EnforcePermission: root / system, or a caller whose install-time grants hold the permissions (Permissions::holds, rule A).
    fn r#enforce_permission(
        &self,
        method: &str,
        permission: EnforcePermission<'_>,
    ) -> binder::Result<()> {
        let (perms, any): (&[&str], bool) = match &permission {
            EnforcePermission::Single(p) => (std::slice::from_ref(p), false),
            EnforcePermission::AllOf(p) => (p, false),
            EnforcePermission::AnyOf(p) => (p, true),
        };
        if self
            .permissions
            .holds(ThreadState::get_calling_uid(), perms, any)
        {
            return Ok(());
        }
        Err(Status::new_exception_str(
            ExceptionCode::SECURITY,
            Some(format!("{method}: caller lacks {permission:?}")),
        ))
    }

    /// StorageManager.getVolumeList (StorageManager.java:330): the volumes of `user_id`; the calling package and the flags are not modelled.
    fn r#getVolumeList(
        &self,
        user_id: i32,
        _calling_package: Option<&str>,
        _flags: i32,
    ) -> binder::Result<Option<Vec<Option<StorageVolume>>>> {
        Ok(Some(volumes(user_id).into_iter().map(Some).collect()))
    }
}

/// The volumes of `user_id`: the primary for the system user, none for any other.
fn volumes(user_id: i32) -> Vec<StorageVolume> {
    if user_id == USER_SYSTEM {
        vec![primary()]
    } else {
        Vec::new()
    }
}

/// The emulated primary as StorageVolume.Builder.build makes it (StorageVolume.java: the internal path is the path, no FUSE backing `/data/media/<user>` here).
fn primary() -> StorageVolume {
    StorageVolume {
        id: Some(PRIMARY_ID.to_owned()),
        path: atl_android_paths::SHARED_STORAGE.to_owned(),
        internal_path: atl_android_paths::SHARED_STORAGE.to_owned(),
        description: Some(PRIMARY_DESCRIPTION.to_owned()),
        primary: true,
        removable: false,
        emulated: true,
        externally_managed: false,
        allow_mass_storage: false,
        max_file_size: 0,
        owner: Some(USER_SYSTEM),
        uuid: None,
        fs_uuid: None,
        state: Some(STATE_MOUNTED.to_owned()),
    }
}

/// The service's binder, for `addService`.
pub fn new_binder(permissions: Arc<Permissions>) -> SpIBinder {
    BnStorageManager::new_binder(StorageService { permissions }, BinderFeatures::default())
        .as_binder()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_user_gets_the_emulated_primary_and_no_other_user_any() {
        let found = volumes(USER_SYSTEM);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].path, "/storage/emulated/0");
        assert!(found[0].primary && found[0].emulated);
        assert_eq!(found[0].state.as_deref(), Some("mounted"));
        assert!(volumes(10).is_empty());
    }
}
