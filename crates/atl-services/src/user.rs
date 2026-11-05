// SPDX-License-Identifier: MIT
//! The `user` service (Context.USER_SERVICE): the converter's IUserManager stub. ContextImpl.getSharedPreferences on a credential-protected context of a target >= O asks the
//! UserManager whether the user is unlocked (ContextImpl.java:3144-3146 for the flag, :620-632 for the check), and throws IllegalStateException when the service is missing.
//! ATL has one user, the system user (UserHandle.USER_SYSTEM = 0), always running and unlocked. The three state queries answer for it and nothing else; every other method stays counted.

use atl_aidl::aidl::android::os::IUserManager::{BnUserManager, IUserManager};
use binder::{BinderFeatures, Interface, SpIBinder, Status};

use crate::counters;

/// The interface name counters use.
const INTERFACE: &str = "IUserManager";
/// UserHandle.USER_SYSTEM (UserHandle.java): the only user this runtime has.
const USER_SYSTEM: i32 = 0;

struct UserService;

impl Interface for UserService {}

impl IUserManager for UserService {
    fn r#unimplemented(&self, method: &'static str) -> Status {
        counters::unimplemented(INTERFACE, method)
    }

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
}

/// The service's binder, for `addService`.
pub fn new_binder() -> SpIBinder {
    BnUserManager::new_binder(UserService, BinderFeatures::default()).as_binder()
}
