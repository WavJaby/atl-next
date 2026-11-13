// SPDX-License-Identifier: MIT
//! The `notification` service (Context.NOTIFICATION_SERVICE): INotificationManager-lite, toasts only (owner 2026-11-05, D4 option A). Mirrors NotificationManagerService's toast queue
//! (enqueueToast:3969-4135, showNextToastLocked, cancelToastLocked:10367, finishToken:4192): one queue for all apps, the head shown; each shown toast hides after its duration
//! (LONG_DELAY 3.5 s / SHORT_DELAY 2 s, :461-462), and its window token is removed when the app calls finishToken, else after FINISH_TOKEN_TIMEOUT (11 s, :465).
//! The app draws the toast: text toasts take the pre-R TN path (activity.rs DISABLED_COMPAT_CHANGES), so ITransientNotification.show(windowToken) makes the app's ToastPresenter add a
//! TYPE_TOAST window with that token; the token is bound to the caller's newest Task here (WindowManagerInternal.addWindowToken), which is where window.rs places the window.
//! Channels: apps create and read back their own (notification_channels.rs), as a device answers (behaviour decision 1); nothing is displayed for them.
//! Not modelled: rate limiting, per-user visibility, suspended/blocked packages, accessibility timeouts, enqueueTextToast (no system UI: answered false), channel groups and posting (counted UNKNOWN).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use atl_aidl::aidl::android::app::INotificationManager::{
    BnNotificationManager, EnforcePermission, INotificationManager,
};
use atl_aidl::mangled::_7_android_3_app_22_ITransientNotification as ITransientNotification;
use atl_parcelables::android::app::NotificationChannel;
use atl_parcelables::android::content::pm::ParceledListSlice;
use binder::{BinderFeatures, ExceptionCode, Interface, SpIBinder, Status, Strong, ThreadState};

use crate::notification_channels::{ChannelError, Channels};
use crate::packages::Packages;
use crate::packages::Permissions;
use crate::tasks::Tasks;
use atl_services_types::counters;
use atl_services_types::permission;

/// The interface name counters use.
const INTERFACE: &str = "INotificationManager";
/// Manifest.permission.INTERACT_ACROSS_USERS: canNotifyAsPackage's check for another user's channels.
const INTERACT_ACROSS_USERS: &str = "android.permission.INTERACT_ACROSS_USERS";
/// NotificationManagerService.MAX_PACKAGE_TOASTS (:444).
const MAX_PACKAGE_TOASTS: usize = 5;
/// Toast.LENGTH_LONG; LONG_DELAY / SHORT_DELAY / FINISH_TOKEN_TIMEOUT (:461-465).
const LENGTH_LONG: i32 = 1;
const LONG_DELAY: Duration = Duration::from_millis(3500);
const SHORT_DELAY: Duration = Duration::from_millis(2000);
const FINISH_TOKEN_TIMEOUT: Duration = Duration::from_secs(11);

/// A queued toast (ToastRecord): who asked, the app's TN, and the window token made for it.
struct Toast {
    id: u64,
    pkg: String,
    token: SpIBinder,
    callback: Strong<dyn ITransientNotification>,
    duration: i32,
    window_token: SpIBinder,
}

#[derive(Default)]
struct State {
    queue: Vec<Toast>,
    /// hidden toasts whose window token waits for finishToken (or the timeout): (pkg, token, window token)
    finishing: Vec<(String, SpIBinder, SpIBinder)>,
    channels: Channels,
}

struct Inner {
    tasks: Arc<Tasks<SpIBinder>>,
    permissions: Arc<Permissions>,
    packages: Arc<Packages>,
    state: Mutex<State>,
    next_id: AtomicU64,
}

impl Inner {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// checkCallerIsSystemOrSameApp: `pkg` must be installed under the caller's app id (root / system pass); else SecurityException. Returns the calling uid.
    fn check_same_app(&self, method: &str, pkg: &str) -> binder::Result<u32> {
        let uid = ThreadState::get_calling_uid();
        if permission::is_system_or_root(uid)
            || self
                .packages
                .by_app_id(permission::app_id(uid))
                .any(|p| p.name() == pkg)
        {
            return Ok(uid);
        }
        Err(Status::new_exception_str(
            ExceptionCode::SECURITY,
            Some(format!("{method}: package {pkg} is not uid {uid}'s")),
        ))
    }

    /// canNotifyAsPackage (NotificationManagerService): the calling package is the caller's, another user needs INTERACT_ACROSS_USERS, and the target is the calling package itself (no delegates
    /// exist); else SecurityException as AOSP's channel getters throw. Returns the (uid, package) whose channels are read.
    fn can_notify_as<'a>(
        &self,
        calling_pkg: Option<&str>,
        user_id: i32,
        target_pkg: Option<&'a str>,
    ) -> binder::Result<(u32, &'a str)> {
        let calling_pkg =
            calling_pkg.ok_or_else(|| Status::new_exception(ExceptionCode::NULL_POINTER, None))?;
        let uid = self.check_same_app("canNotifyAsPackage", calling_pkg)?;
        if i64::from(permission::user_id(uid)) != i64::from(user_id)
            && !self.permissions.holds(uid, &[INTERACT_ACROSS_USERS], false)
        {
            return Err(Status::new_exception_str(
                ExceptionCode::SECURITY,
                Some(format!(
                    "canNotifyAsPackage for user {user_id}: caller lacks {INTERACT_ACROSS_USERS}"
                )),
            ));
        }
        match target_pkg {
            Some(t) if t == calling_pkg => Ok((uid, t)),
            t => Err(Status::new_exception_str(
                ExceptionCode::SECURITY,
                Some(format!(
                    "Pkg {calling_pkg} cannot read channels for {} in {user_id}",
                    t.unwrap_or("null")
                )),
            )),
        }
    }

    /// showNextToastLocked: the head is shown and its hide scheduled; a TN that refuses (its process died) is dropped and the next one tried.
    fn show_next(self: &Arc<Self>) {
        loop {
            let head = {
                let st = self.lock();
                st.queue
                    .first()
                    .map(|t| (t.id, t.callback.clone(), t.window_token.clone(), t.duration))
            };
            let Some((id, callback, window_token, duration)) = head else {
                return;
            };
            if let Err(e) = callback.r#show(Some(&window_token)) {
                counters::answered(INTERFACE, &format!("toast show refused: {e:?}"));
                self.remove(id);
                continue;
            }
            let delay = if duration == LENGTH_LONG {
                LONG_DELAY
            } else {
                SHORT_DELAY
            };
            let inner = Arc::clone(self);
            // scheduleDurationReachedLocked: a timer per shown toast; a toast already cancelled by then is not found by its id
            let spawned = std::thread::Builder::new()
                .name("atl-toast".to_owned())
                .spawn(move || {
                    std::thread::sleep(delay);
                    inner.cancel(id);
                });
            if spawned.is_err() {
                counters::answered(INTERFACE, "toast timer: no thread");
            }
            return;
        }
    }

    /// Takes toast `id` out of the queue, its window token with it.
    fn remove(&self, id: u64) -> Option<Toast> {
        let mut st = self.lock();
        let i = st.queue.iter().position(|t| t.id == id)?;
        let t = st.queue.remove(i);
        self.tasks.unbind_token(&t.window_token);
        Some(t)
    }

    /// cancelToastLocked: hide, keep the window token until finishToken or FINISH_TOKEN_TIMEOUT, then show the next if the head went.
    fn cancel(self: &Arc<Self>, id: u64) {
        let (toast, was_head) = {
            let mut st = self.lock();
            let Some(i) = st.queue.iter().position(|t| t.id == id) else {
                return;
            };
            let t = st.queue.remove(i);
            st.finishing
                .push((t.pkg.clone(), t.token.clone(), t.window_token.clone()));
            (t, i == 0)
        };
        if let Err(e) = toast.callback.r#hide() {
            counters::answered(INTERFACE, &format!("toast hide refused: {e:?}"));
        }
        let inner = Arc::clone(self);
        let window_token = toast.window_token.clone();
        let spawned = std::thread::Builder::new()
            .name("atl-toast-token".to_owned())
            .spawn(move || {
                std::thread::sleep(FINISH_TOKEN_TIMEOUT);
                inner.finish(|w| *w == window_token);
            });
        if spawned.is_err() {
            counters::answered(INTERFACE, "toast token timer: no thread");
        }
        if was_head {
            self.show_next();
        }
    }

    /// finishWindowTokenLocked: the hidden toast's window token goes (removeWindowToken).
    fn finish(&self, matches: impl Fn(&SpIBinder) -> bool) {
        let mut st = self.lock();
        st.finishing.retain(|(_, _, w)| {
            let gone = matches(w);
            if gone {
                self.tasks.unbind_token(w);
            }
            !gone
        });
    }
}

struct NotificationManager(Arc<Inner>);

impl Interface for NotificationManager {}

impl INotificationManager for NotificationManager {
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
            .0
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

    fn r#unimplemented(&self, method: &'static str) -> Status {
        atl_services_wire::unimplemented(INTERFACE, method)
    }

    /// enqueueToast (:3969): queued per (pkg, token); a known one only takes the new duration (record.update). Refused (false, as AOSP): no package or TN, MAX_PACKAGE_TOASTS
    /// already queued, or a caller with no Task to show it in. A package not installed under the caller's uid -> SecurityException (checkCallerIsSameApp).
    fn r#enqueueToast(
        &self,
        pkg: Option<&str>,
        token: Option<&SpIBinder>,
        callback: Option<&Strong<dyn ITransientNotification>>,
        duration: i32,
        _is_ui_context: bool,
        _display_id: i32,
    ) -> binder::Result<bool> {
        let (Some(pkg), Some(token), Some(callback)) = (pkg, token, callback) else {
            counters::refused(INTERFACE, "enqueueToast: no package, token or callback");
            return Ok(false);
        };
        let inner = &self.0;
        let uid = inner.check_same_app("enqueueToast", pkg)?;
        let head = {
            let mut st = inner.lock();
            if let Some(t) = st
                .queue
                .iter_mut()
                .find(|t| t.pkg == pkg && t.token == *token)
            {
                t.duration = duration;
                return Ok(true);
            }
            if st.queue.iter().filter(|t| t.pkg == pkg).count() >= MAX_PACKAGE_TOASTS {
                counters::refused(INTERFACE, "enqueueToast: MAX_PACKAGE_TOASTS queued");
                return Ok(false);
            }
            let Some(task) = inner.tasks.newest_task_of(uid) else {
                counters::refused(INTERFACE, "enqueueToast: the caller has no Task");
                return Ok(false);
            };
            let window_token = crate::token::new();
            inner.tasks.bind_token(window_token.clone(), task);
            st.queue.push(Toast {
                id: inner.next_id.fetch_add(1, Ordering::Relaxed),
                pkg: pkg.to_owned(),
                token: token.clone(),
                callback: callback.clone(),
                duration,
                window_token,
            });
            st.queue.len() == 1
        };
        counters::answered(INTERFACE, "enqueueToast (queued)");
        if head {
            inner.show_next();
        }
        Ok(true)
    }

    /// enqueueTextToast: SystemUI's path (ToastUI); no system UI exists, and apps never take it (CHANGE_TEXT_TOASTS_IN_THE_SYSTEM is disabled at bindApplication). False = not enqueued.
    fn r#enqueueTextToast(
        &self,
        _pkg: Option<&str>,
        _token: Option<&SpIBinder>,
        _text: Option<&atl_parcelables::java::lang::CharSequence>,
        _duration: i32,
        _is_ui_context: bool,
        _display_id: i32,
        _callback: Option<
            &Strong<dyn atl_aidl::mangled::_7_android_3_app_30_ITransientNotificationCallback>,
        >,
    ) -> binder::Result<bool> {
        counters::refused(
            INTERFACE,
            "enqueueTextToast: no system UI renders text toasts",
        );
        Ok(false)
    }

    /// cancelToast (:4136): the (pkg, token) toast is hidden and dequeued; an unknown one is nothing to cancel (AOSP logs it).
    fn r#cancelToast(&self, pkg: Option<&str>, token: Option<&SpIBinder>) -> binder::Result<()> {
        let id = {
            let st = self.0.lock();
            st.queue
                .iter()
                .find(|t| Some(t.pkg.as_str()) == pkg && Some(&t.token) == token)
                .map(|t| t.id)
        };
        if let Some(id) = id {
            self.0.cancel(id);
        }
        Ok(())
    }

    /// finishToken (:4192): the app removed the toast's window; its window token goes now instead of at the timeout.
    fn r#finishToken(&self, pkg: Option<&str>, token: Option<&SpIBinder>) -> binder::Result<()> {
        let st = self.0.lock();
        let windows: Vec<SpIBinder> = st
            .finishing
            .iter()
            .filter(|(p, t, _)| Some(p.as_str()) == pkg && Some(t) == token)
            .map(|(_, _, w)| w.clone())
            .collect();
        drop(st);
        self.0.finish(|w| windows.contains(w));
        Ok(())
    }

    /// areNotificationsEnabled: AOSP's default for an app nobody blocked (NotificationManagerService areNotificationsEnabledForPackage -> PreferencesHelper, importance unspecified -> true).
    fn r#areNotificationsEnabled(&self, _pkg: Option<&str>) -> binder::Result<bool> {
        Ok(true)
    }

    /// createNotificationChannels (:4850 -> createNotificationChannelsImpl): each channel in turn into the caller's store; the first refused one throws, the ones before it stay (as AOSP's loop).
    fn r#createNotificationChannels(
        &self,
        pkg: Option<&str>,
        list: Option<&ParceledListSlice>,
    ) -> binder::Result<()> {
        let pkg = pkg.ok_or_else(|| Status::new_exception(ExceptionCode::NULL_POINTER, None))?;
        let uid = self.0.check_same_app("createNotificationChannels", pkg)?;
        let list = list.ok_or_else(|| Status::new_exception(ExceptionCode::NULL_POINTER, None))?;
        let mut st = self.0.lock();
        for channel in list.of::<NotificationChannel>() {
            st.channels
                .create(uid, pkg, channel.clone())
                .map_err(ChannelError::status)?;
        }
        counters::answered(INTERFACE, "createNotificationChannels");
        Ok(())
    }

    /// getNotificationChannel (:4889 -> getConversationNotificationChannel, no conversation): the target's channel `channel_id`, or null.
    fn r#getNotificationChannel(
        &self,
        calling_pkg: Option<&str>,
        user_id: i32,
        target_pkg: Option<&str>,
        channel_id: Option<&str>,
    ) -> binder::Result<Option<NotificationChannel>> {
        let (uid, pkg) = self.0.can_notify_as(calling_pkg, user_id, target_pkg)?;
        counters::answered(INTERFACE, "getNotificationChannel");
        Ok(self.0.lock().channels.get(uid, pkg, channel_id))
    }

    /// getNotificationChannels (:5124): the target's channels, deleted ones left out.
    fn r#getNotificationChannels(
        &self,
        calling_pkg: Option<&str>,
        target_pkg: Option<&str>,
        user_id: i32,
    ) -> binder::Result<Option<ParceledListSlice>> {
        let (uid, pkg) = self.0.can_notify_as(calling_pkg, user_id, target_pkg)?;
        counters::answered(INTERFACE, "getNotificationChannels");
        Ok(Some(ParceledListSlice::new(
            self.0.lock().channels.list(uid, pkg),
        )))
    }
}

/// The service's binder, for `addService`; toast windows are bound into `tasks`.
pub fn new_binder(
    tasks: Arc<Tasks<SpIBinder>>,
    permissions: Arc<Permissions>,
    packages: Arc<Packages>,
) -> SpIBinder {
    BnNotificationManager::new_binder(
        NotificationManager(Arc::new(Inner {
            tasks,
            permissions,
            packages,
            state: Mutex::new(State::default()),
            next_id: AtomicU64::new(0),
        })),
        BinderFeatures::default(),
    )
    .as_binder()
}
