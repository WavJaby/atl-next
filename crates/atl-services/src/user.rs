// SPDX-License-Identifier: MIT
//! The `user` service (Context.USER_SERVICE): the converter's IUserManager stub. ContextImpl.getSharedPreferences on a credential-protected context of a target >= O asks the
//! UserManager whether the user is unlocked (ContextImpl.java:3144-3146 for the flag, :620-632 for the check), and throws IllegalStateException when the service is missing.
//! ATL has one user, the system user (UserHandle.USER_SYSTEM = 0), always running and unlocked. The three state queries answer for it and nothing else; getUsers / getUserInfo describe it
//! (SettingsProvider builds its per-user settings from them); every other method stays counted.

use atl_aidl::aidl::android::os::IUserManager::{BnUserManager, IUserManager};
use atl_parcelables::android::content::pm::UserInfo;
use atl_services_types::permission;
use binder::{BinderFeatures, ExceptionCode, Interface, SpIBinder, Status, ThreadState};

/// UserHandle.USER_SYSTEM (UserHandle.java): the only user this runtime has.
const USER_SYSTEM: i32 = 0;
/// UserManager.USER_TYPE_FULL_SYSTEM: the system user's type on a device without headless system user mode (UserManagerService.fallbackToSingleUserLP).
const USER_TYPE_FULL_SYSTEM: &str = "android.os.usertype.full.SYSTEM";
/// UserInfo flags (UserInfo.java): FLAG_PRIMARY 0x1, FLAG_ADMIN 0x2, FLAG_INITIALIZED 0x10, FLAG_FULL 0x400, FLAG_SYSTEM 0x800, FLAG_MAIN 0x4000.
/// fallbackToSingleUserLP: the type's default flags (UserTypeFactory USER_TYPE_FULL_SYSTEM: base FLAG_SYSTEM | FLAG_FULL, defaults FLAG_PRIMARY | FLAG_ADMIN | FLAG_MAIN) | FLAG_INITIALIZED.
const SYSTEM_USER_FLAGS: i32 = 0x1 | 0x2 | 0x10 | 0x400 | 0x800 | 0x4000;
/// framework-res `owner_name` in the default locale (core/res/res/values/strings.xml): userWithName gives the unnamed system user this name. AOSP reads the localized resource.
const OWNER_NAME: &str = "Owner";

struct UserService;

impl Interface for UserService {}

/// UserManagerService.checkCreateUsersPermission / checkQueryOrCreateUsersPermission: the system and root uids pass; MANAGE_USERS, CREATE_USERS and QUERY_USERS are signature / role
/// permissions no app here holds, so every other caller is refused with AOSP's message.
fn check_query_users(what: &str) -> binder::Result<()> {
    if permission::is_system_or_root(ThreadState::get_calling_uid()) {
        return Ok(());
    }
    Err(Status::new_exception_str(
        ExceptionCode::SECURITY,
        Some(format!(
            "You either need MANAGE_USERS, CREATE_USERS, or QUERY_USERS permission to: {what}"
        )),
    ))
}

/// The system user as fallbackToSingleUserLP creates it and userWithName answers it: id 0, the owner name, no icon, its flags and type; serial number, times and the profile fields at
/// UserInfo's defaults (profileGroupId and restrictedProfileParentId = UserInfo.NO_PROFILE_GROUP_ID -10000).
fn system_user() -> UserInfo {
    UserInfo {
        id: USER_SYSTEM,
        name: Some(OWNER_NAME.into()),
        flags: SYSTEM_USER_FLAGS,
        user_type: Some(USER_TYPE_FULL_SYSTEM.into()),
        profile_group_id: NO_PROFILE_GROUP_ID,
        restricted_profile_parent_id: NO_PROFILE_GROUP_ID,
        ..Default::default()
    }
}

/// UserInfo.NO_PROFILE_GROUP_ID = UserHandle.USER_NULL (-10000).
const NO_PROFILE_GROUP_ID: i32 = -10_000;

impl IUserManager for UserService {
    /// UserManagerService.isUserRunning:8005: a user with a state (mUserStates) is running; the system user has one, no other id does.
    fn r#isUserRunning(&self, user_id: i32) -> binder::Result<bool> {
        Ok(user_id == USER_SYSTEM)
    }

    /// UserManagerService.isUserUnlocked:8115: state RUNNING_UNLOCKED. The stopping and shutdown special case (CE storage) does not apply to the running system user.
    fn r#isUserUnlocked(&self, user_id: i32) -> binder::Result<bool> {
        Ok(user_id == USER_SYSTEM)
    }

    /// UserManagerService.isUserUnlockingOrUnlocked:8096: state RUNNING_UNLOCKING or RUNNING_UNLOCKED.
    fn r#isUserUnlockingOrUnlocked(&self, user_id: i32) -> binder::Result<bool> {
        Ok(user_id == USER_SYSTEM)
    }

    /// UserManagerService.getUsers:1553: the permission check, then every user not excluded (getUsersInternal); the system user is never partial, dying or pre-created.
    fn r#getUsers(
        &self,
        _exclude_partial: bool,
        _exclude_dying: bool,
        _exclude_pre_created: bool,
    ) -> binder::Result<Option<Vec<Option<UserInfo>>>> {
        check_query_users("query users")?;
        Ok(Some(vec![Some(system_user())]))
    }

    /// UserManagerService.getUserInfo:2366: the permission check, then the user of `user_id` or null.
    fn r#getUserInfo(&self, user_id: i32) -> binder::Result<Option<UserInfo>> {
        check_query_users("query user")?;
        Ok((user_id == USER_SYSTEM).then(system_user))
    }
}

/// The service's binder, for `addService`.
pub fn new_binder() -> SpIBinder {
    BnUserManager::new_binder(UserService, BinderFeatures::default()).as_binder()
}
