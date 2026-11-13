// SPDX-License-Identifier: MIT
//! The `content` service (ContentResolver.CONTENT_SERVICE_NAME, ContentResolver.java:3925): the converter's IContentService stub over ContentService.java's observer registry. Which observers
//! hear a notification is observer_rules.rs; this keeps the registrations and dispatches. Dispatch is one onChangeEtc per observer, as ContentService.ObserverCollector.dispatch does, but at once:
//! AOSP delays it for background processes, and ATL has no process states. Sync is not kept (the SyncManager does not run): NOTIFY_SYNC_TO_NETWORK is counted, and every sync method stays
//! counted as unimplemented. Provider access (ActivityManagerInternal.checkContentProviderAccess) is not checked: the M2 services keep no provider table, so any authority is accepted.
//! A dead observer's death recipient only sets a flag, and its entry leaves on the next read of the list. A recipient must not be dropped inside its own callback: dropping it gives up the
//! callback's cookie.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use atl_aidl::aidl::android::content::IContentService::{
    BnContentService, EnforcePermission, IContentService,
};
use atl_aidl::aidl::android::database::IContentObserver::IContentObserver;
use atl_parcelables::android::net::Uri;
use binder::{
    BinderFeatures, DeathRecipient, ExceptionCode, FromIBinder, IBinder, Interface, SpIBinder,
    Status, StatusCode, Strong, ThreadState,
};

use crate::observer_rules::{Watch, segments};
use crate::packages::Permissions;
use atl_services_types::permission;

/// The interface name counters use.
const INTERFACE: &str = "IContentService";
/// ContentResolver.NOTIFY_SYNC_TO_NETWORK (ContentResolver.java:675).
const NOTIFY_SYNC_TO_NETWORK: i32 = 1 << 0;
/// UserHandle.USER_CURRENT (UserHandle.java:61): the caller's user.
const USER_CURRENT: i32 = -2;

/// One registration: the observer's binder, what it watches, and the death link that sets `dead`. The recipient is held here so the link lasts as long as the entry.
struct Entry {
    observer: SpIBinder,
    watch: Watch,
    dead: Arc<AtomicBool>,
    _death: DeathRecipient,
}

fn alive(entry: &Entry) -> bool {
    !entry.dead.load(Ordering::Acquire)
}

/// ContentService's mRootNode, flattened: the registrations in the order they were made.
struct ContentService {
    permissions: Arc<Permissions>,
    observers: Mutex<Vec<Entry>>,
}

impl Interface for ContentService {}

impl ContentService {
    fn registry(&self) -> MutexGuard<'_, Vec<Entry>> {
        self.observers.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// ContentService.handleIncomingUser for the user a call names. The caller's own user and USER_CURRENT need no permission; another user, or USER_ALL, needs a system or root caller
/// (INTERACT_ACROSS_USERS_FULL's holders here).
fn resolve_user(requested: i32, caller_uid: u32) -> binder::Result<i32> {
    let own = permission::user_id(caller_uid) as i32;
    if requested == USER_CURRENT || requested == own {
        return Ok(own);
    }
    if permission::is_system_or_root(caller_uid) {
        return Ok(requested);
    }
    Err(Status::new_exception_str(
        ExceptionCode::SECURITY,
        Some(format!("uid {caller_uid} may not act for user {requested}")),
    ))
}

impl IContentService for ContentService {
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

    /// ContentService.registerContentObserver (:365-400). A null URI or observer is an IllegalArgumentException. The death link is set before the registration is kept, so a dead
    /// observer is refused rather than kept.
    fn r#registerContentObserver(
        &self,
        uri: Option<&Uri>,
        notify_for_descendants: bool,
        observer: Option<&Strong<dyn IContentObserver>>,
        user_handle: i32,
        _target_sdk_version: i32,
    ) -> binder::Result<()> {
        let (Some(uri), Some(observer)) = (uri, observer) else {
            atl_diag::refused(INTERFACE, "registerContentObserver (null uri or observer)");
            return Err(Status::new_exception_str(
                ExceptionCode::ILLEGAL_ARGUMENT,
                Some("You must pass a valid uri and observer".to_owned()),
            ));
        };
        let user = resolve_user(user_handle, ThreadState::get_calling_uid())?;
        let dead = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&dead);
        let mut death = DeathRecipient::new(move || flag.store(true, Ordering::Release));
        let mut binder = observer.as_binder();
        binder
            .link_to_death(&mut death)
            .map_err(|_| Status::from(StatusCode::DEAD_OBJECT))?;
        let entry = Entry {
            observer: binder,
            watch: Watch::new(&uri.0, notify_for_descendants, user),
            dead,
            _death: death,
        };
        let mut list = self.registry();
        list.retain(alive);
        list.push(entry);
        Ok(())
    }

    /// ContentService.unregisterContentObserver (:409): every registration of this observer goes.
    fn r#unregisterContentObserver(
        &self,
        observer: Option<&Strong<dyn IContentObserver>>,
    ) -> binder::Result<()> {
        let Some(observer) = observer else {
            atl_diag::refused(INTERFACE, "unregisterContentObserver (null observer)");
            return Err(Status::new_exception_str(
                ExceptionCode::ILLEGAL_ARGUMENT,
                Some("You must pass a valid observer".to_owned()),
            ));
        };
        let binder = observer.as_binder();
        self.registry().retain(|e| alive(e) && e.observer != binder);
        Ok(())
    }

    /// ContentService.notifyChange (:428-...). Each URI reaches the observers registered at it or above it, for the target user, and each observer gets one onChangeEtc with the URIs it
    /// heard. The notifier hears its own changes only when it asked to. NOTIFY_SYNC_TO_NETWORK would schedule a sync: counted, not kept.
    fn r#notifyChange(
        &self,
        uris: Option<&[Option<Uri>]>,
        observer: Option<&Strong<dyn IContentObserver>>,
        observer_wants_self_notifications: bool,
        flags: i32,
        user_handle: i32,
        _target_sdk_version: i32,
        _calling_package: Option<&str>,
    ) -> binder::Result<()> {
        let target = resolve_user(user_handle, ThreadState::get_calling_uid())?;
        if (flags & NOTIFY_SYNC_TO_NETWORK) != 0 {
            atl_diag::answered(
                INTERFACE,
                "notifyChange sync to network (not kept: no SyncManager)",
            );
        }
        let notifier = observer.map(|o| o.as_binder());
        // (observer, whether it is the notifier, the URIs it hears), in the order each first heard one
        let mut heard: Vec<(SpIBinder, bool, Vec<Uri>)> = Vec::new();
        {
            let mut list = self.registry();
            list.retain(alive);
            for uri in uris.unwrap_or_default() {
                let Some(uri) = uri else {
                    atl_diag::refused(INTERFACE, "notifyChange (null uri)");
                    continue;
                };
                let notified = segments(&uri.0);
                for entry in list.iter() {
                    let self_change = notifier.as_ref() == Some(&entry.observer);
                    if self_change && !observer_wants_self_notifications {
                        continue;
                    }
                    if !entry.watch.hears(&notified, flags, target) {
                        continue;
                    }
                    match heard
                        .iter_mut()
                        .find(|(o, s, _)| *o == entry.observer && *s == self_change)
                    {
                        Some((_, _, got)) => got.push(uri.clone()),
                        None => {
                            heard.push((entry.observer.clone(), self_change, vec![uri.clone()]))
                        }
                    }
                }
            }
        }
        for (observer, self_change, got) in heard {
            let Ok(proxy) = <dyn IContentObserver as FromIBinder>::try_from(observer) else {
                atl_diag::refused(INTERFACE, "notifyChange (not an IContentObserver)");
                continue;
            };
            let got: Vec<Option<Uri>> = got.into_iter().map(Some).collect();
            // oneway: only a dead observer fails here, and its entry leaves on the next read of the list
            let _ = proxy.r#onChangeEtc(self_change, Some(got.as_slice()), flags, target);
        }
        Ok(())
    }
}

/// A new content service binder.
pub fn new_binder(permissions: Arc<Permissions>) -> SpIBinder {
    BnContentService::new_binder(
        ContentService {
            permissions,
            observers: Mutex::new(Vec::new()),
        },
        BinderFeatures::default(),
    )
    .as_binder()
}
