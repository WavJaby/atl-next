// SPDX-License-Identifier: MIT
//! The `account` service (Context.ACCOUNT_SERVICE, services/core/java/com/android/server/accounts/AccountManagerService.java): AccountManager's fetcher is getServiceOrThrow
//! (SystemServiceRegistry.java:349-356), so without it getSystemService(AccountManager.class) is null (LINE's SplashActivity getAccountsByType NPE). SystemServer starts it on every device (:1619).
//! Behaviour = a device with no accounts and no authenticator descriptions read (plan S8a; the account store + authenticator cache is S17):
//! - getAccountsAsUser (:4584) / getAccountsByTypeForPackage (:4782): AppOps checkPackage, the cross-user check, then the visible accounts -> always empty. No GET_ACCOUNTS check exists on
//!   this path: below targetSdk 26 the permission only widens visibility, above it visibility grants do; with no account neither changes the empty answer.
//! - getAccountsForPackage (:4769): system uid only, else SecurityException.
//! - getAuthenticatorTypes (:1685-1727): cross-user check, then the cached authenticators the caller may see. AOSP would list the caller's own authenticator (canCallerAccessPackage holds
//!   for its own package); its `<account-authenticator>` XML is not read, so the list is empty and a declaring caller is counted (`authenticator not read`).
//! - Managed types (getTypesForCaller :5975-5995): the types of authenticators signed like the caller. A caller declaring no authenticator service (action
//!   android.accounts.AccountAuthenticator) manages none: addAccountExplicitly (:495-524), removeAccountExplicitly (:2433-2454), getPassword (:1571-1589), getUserData (:1651-1667) throw
//!   AOSP's SecurityException. A declaring caller may manage the type, which is unknown without the XML: refused + counted, answered as for an account that does not exist (false / null).
//! - register/unregisterAccountListener (:940-1010): checkPackage, then per (type, package) counts; unregistering one never registered -> IllegalArgumentException. With no accounts no
//!   LOGIN_ACCOUNTS_CHANGED-style broadcast is ever due.
//!
//! Callback methods (hasFeatures, getAccountsByFeatures, getAuthToken, addAccount, ...) answer UNKNOWN_TRANSACTION (counted): their empty answer is a Bundle with a Parcelable[] (KEY_ACCOUNTS), which the Bundle codec does not carry yet.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use atl_aidl::aidl::android::accounts::IAccountManager::{BnAccountManager, IAccountManager};
use atl_parcelables::android::accounts::{Account, AuthenticatorDescription};
use atl_parcelables::android::os::Bundle;
use atl_services_types::installer_output::Kind;
use atl_services_types::permission;
use binder::{BinderFeatures, ExceptionCode, Interface, SpIBinder, Status, ThreadState};

use crate::packages::{PackageRecord, PackageTable, Permissions};

/// The name the answers are counted under (`atl_diag::answered`, the app gate's SERVICE_CALLS).
const ACCOUNT_MANAGER: &str = "IAccountManager";
/// AccountManager.ACTION_AUTHENTICATOR_INTENT: the service action RegisteredServicesCache scans for authenticators.
const AUTHENTICATOR_ACTION: &str = "android.accounts.AccountAuthenticator";
const INTERACT_ACROSS_USERS_FULL: &str = "android.permission.INTERACT_ACROSS_USERS_FULL";
/// The counter S17 drives to zero (plan S table).
const NOT_READ: &str = "(authenticator not read)";

struct AccountService {
    packages: Arc<PackageTable>,
    permissions: Arc<Permissions>,
    /// UserAccounts.mReceiversForType: account type (None = any) -> package -> registration count.
    listeners: Mutex<HashMap<Option<String>, HashMap<String, u32>>>,
}

impl Interface for AccountService {}

fn security(message: String) -> Status {
    Status::new_exception_str(ExceptionCode::SECURITY, Some(message))
}

fn illegal_argument(message: &str) -> Status {
    Status::new_exception_str(ExceptionCode::ILLEGAL_ARGUMENT, Some(message))
}

fn null_pointer(message: &str) -> Status {
    Status::new_exception_str(ExceptionCode::NULL_POINTER, Some(message))
}

/// The package declares a service with the authenticator action (RegisteredServicesCache's query); its `<account-authenticator>` meta-data, and so the type, is not read.
pub fn declares_authenticator(p: &PackageRecord) -> bool {
    p.intents
        .iter()
        .filter(|c| c.kind == Kind::ServiceIntents)
        .flat_map(|c| c.intents.iter())
        .filter_map(|i| i.intent_filter.as_ref())
        .any(|f| {
            f.actions
                .iter()
                .flatten()
                .any(|a| a == AUTHENTICATOR_ACTION)
        })
}

impl AccountService {
    /// AppOpsManager.checkPackage (AppOpsManager.java :10010-10018).
    fn check_package(&self, uid: u32, package: Option<&str>) -> binder::Result<()> {
        let package = package.unwrap_or_default();
        if self.packages.snapshot().is_same_app(package, uid) {
            return Ok(());
        }
        Err(security(format!(
            "Package {package} does not belong to {uid}"
        )))
    }

    /// `userId != UserHandle.getCallingUserId() && callingUid != SYSTEM_UID && !INTERACT_ACROSS_USERS_FULL` (isCrossUser :1729-1735 and the inline copies).
    fn cross_user(&self, uid: u32, user_id: i32) -> bool {
        i64::from(user_id) != i64::from(permission::user_id(uid))
            && uid != permission::SYSTEM_UID
            && !self
                .permissions
                .holds(uid, &[INTERACT_ACROSS_USERS_FULL], false)
    }

    /// Whether the caller's app declares an authenticator (any package of its app id).
    fn caller_declares(&self, uid: u32) -> bool {
        let packages = self.packages.snapshot();
        packages
            .by_app_id(permission::app_id(uid))
            .any(declares_authenticator)
    }

    /// isAccountManagedByCaller (:5958-5964) where it is decidable: Some(false) for a caller with no authenticator, None (refused + counted as `what`) when the type is unread.
    fn managed(&self, uid: u32, account_type: Option<&str>, what: &str) -> Option<bool> {
        if account_type.is_none() || !self.caller_declares(uid) {
            return Some(false);
        }
        atl_diag::refused(ACCOUNT_MANAGER, &format!("{what} {NOT_READ}"));
        None
    }

    fn empty_accounts(&self, what: &str) -> binder::Result<Option<Vec<Option<Account>>>> {
        atl_diag::answered(ACCOUNT_MANAGER, &format!("{what} (no accounts)"));
        Ok(Some(Vec::new()))
    }
}

impl IAccountManager for AccountService {
    fn r#getAccountsAsUser(
        &self,
        _account_type: Option<&str>,
        user_id: i32,
        op_package_name: Option<&str>,
    ) -> binder::Result<Option<Vec<Option<Account>>>> {
        let uid = ThreadState::get_calling_uid();
        self.check_package(uid, op_package_name)?;
        if self.cross_user(uid, user_id) {
            return Err(security(format!(
                "User {} trying to get account for {user_id}",
                permission::user_id(uid)
            )));
        }
        self.empty_accounts("getAccountsAsUser")
    }

    fn r#getAccountsByTypeForPackage(
        &self,
        _account_type: Option<&str>,
        _package_name: Option<&str>,
        op_package_name: Option<&str>,
    ) -> binder::Result<Option<Vec<Option<Account>>>> {
        self.check_package(ThreadState::get_calling_uid(), op_package_name)?;
        self.empty_accounts("getAccountsByTypeForPackage")
    }

    fn r#getAccountsForPackage(
        &self,
        _package_name: Option<&str>,
        uid: i32,
        _op_package_name: Option<&str>,
    ) -> binder::Result<Option<Vec<Option<Account>>>> {
        let caller = ThreadState::get_calling_uid();
        if permission::app_id(caller) != permission::SYSTEM_UID {
            return Err(security(format!(
                "getAccountsForPackage() called from unauthorized uid {caller} with uid={uid}"
            )));
        }
        self.empty_accounts("getAccountsForPackage")
    }

    fn r#getAuthenticatorTypes(
        &self,
        user_id: i32,
    ) -> binder::Result<Option<Vec<Option<AuthenticatorDescription>>>> {
        let uid = ThreadState::get_calling_uid();
        if self.cross_user(uid, user_id) {
            return Err(security(format!(
                "User {} tying to get authenticator types for {user_id}",
                permission::user_id(uid)
            )));
        }
        if self.caller_declares(uid) {
            atl_diag::refused(
                ACCOUNT_MANAGER,
                &format!("getAuthenticatorTypes {NOT_READ}"),
            );
        } else {
            atl_diag::answered(ACCOUNT_MANAGER, "getAuthenticatorTypes (none)");
        }
        Ok(Some(Vec::new()))
    }

    fn r#addAccountExplicitly(
        &self,
        account: Option<&Account>,
        _password: Option<&str>,
        _extras: Option<&Bundle>,
        op_package_name: Option<&str>,
    ) -> binder::Result<bool> {
        let account = account.ok_or_else(|| null_pointer("account cannot be null"))?;
        let uid = ThreadState::get_calling_uid();
        match self.managed(uid, Some(&account.r#type), "addAccountExplicitly") {
            Some(_) => Err(security(format!(
                "uid={uid}, package={} cannot explicitly add accounts of type: {}",
                op_package_name.unwrap_or("null"),
                account.r#type
            ))),
            // addAccountInternal's failure answer: no account is stored
            None => Ok(false),
        }
    }

    fn r#removeAccountExplicitly(&self, account: Option<&Account>) -> binder::Result<bool> {
        let Some(account) = account else {
            return Ok(false);
        };
        let uid = ThreadState::get_calling_uid();
        match self.managed(uid, Some(&account.r#type), "removeAccountExplicitly") {
            Some(_) => Err(security(format!(
                "uid {uid} cannot explicitly remove accounts of type: {}",
                account.r#type
            ))),
            None => Ok(false),
        }
    }

    fn r#getPassword(&self, account: Option<&Account>) -> binder::Result<Option<String>> {
        let account = account.ok_or_else(|| illegal_argument("account is null"))?;
        let uid = ThreadState::get_calling_uid();
        match self.managed(uid, Some(&account.r#type), "getPassword") {
            Some(_) => Err(security(format!(
                "uid {uid} cannot get secrets for accounts of type: {}",
                account.r#type
            ))),
            None => Ok(None),
        }
    }

    fn r#getUserData(
        &self,
        account: Option<&Account>,
        key: Option<&str>,
    ) -> binder::Result<Option<String>> {
        let account = account.ok_or_else(|| null_pointer("account cannot be null"))?;
        key.ok_or_else(|| null_pointer("key cannot be null"))?;
        let uid = ThreadState::get_calling_uid();
        match self.managed(uid, Some(&account.r#type), "getUserData") {
            Some(_) => Err(security(format!(
                "uid {uid} cannot get user data for accounts of type: {}",
                account.r#type
            ))),
            None => Ok(None),
        }
    }

    fn r#registerAccountListener(
        &self,
        account_types: Option<&[Option<String>]>,
        op_package_name: Option<&str>,
    ) -> binder::Result<()> {
        self.check_package(ThreadState::get_calling_uid(), op_package_name)?;
        let package = op_package_name.unwrap_or_default().to_owned();
        let types: Vec<Option<String>> = account_types.map_or_else(|| vec![None], <[_]>::to_vec);
        let mut listeners = self.listeners.lock().unwrap_or_else(|e| e.into_inner());
        for t in types {
            *listeners
                .entry(t)
                .or_default()
                .entry(package.clone())
                .or_default() += 1;
        }
        atl_diag::answered(ACCOUNT_MANAGER, "registerAccountListener (never notified)");
        Ok(())
    }

    fn r#unregisterAccountListener(
        &self,
        account_types: Option<&[Option<String>]>,
        op_package_name: Option<&str>,
    ) -> binder::Result<()> {
        self.check_package(ThreadState::get_calling_uid(), op_package_name)?;
        let package = op_package_name.unwrap_or_default();
        let types: Vec<Option<String>> = account_types.map_or_else(|| vec![None], <[_]>::to_vec);
        let mut listeners = self.listeners.lock().unwrap_or_else(|e| e.into_inner());
        for t in types {
            let receivers = listeners.get_mut(&t);
            let Some(count) = receivers.and_then(|r| r.get_mut(package)) else {
                return Err(illegal_argument("attempt to unregister wrong receiver"));
            };
            *count -= 1;
            if *count == 0 {
                listeners.get_mut(&t).map(|r| r.remove(package));
            }
        }
        Ok(())
    }
}

/// The `account` binder, for `addService`.
pub fn new_binder(packages: Arc<PackageTable>, permissions: Arc<Permissions>) -> SpIBinder {
    BnAccountManager::new_binder(
        AccountService {
            packages,
            permissions,
            listeners: Mutex::new(HashMap::new()),
        },
        BinderFeatures::default(),
    )
    .as_binder()
}
