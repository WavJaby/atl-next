// SPDX-License-Identifier: MIT
//! The `notification` service (Context.NOTIFICATION_SERVICE): INotificationManager-lite, toasts only (maintainer 2026-11-05, D4 option A). Mirrors NotificationManagerService's toast queue
//! (enqueueToast:3969-4135, showNextToastLocked, cancelToastLocked:10367, finishToken:4192): one queue for all apps, the head shown; each shown toast hides after its duration
//! (LONG_DELAY 3.5 s / SHORT_DELAY 2 s, :461-462), and its window token is removed when the app calls finishToken, else after FINISH_TOKEN_TIMEOUT (11 s, :465).
//! The app draws the toast: text toasts take the pre-R TN path (activity.rs DISABLED_COMPAT_CHANGES), so ITransientNotification.show(windowToken) makes the app's ToastPresenter add a
//! TYPE_TOAST window with that token; the token is bound to the caller's newest Task here (WindowManagerInternal.addWindowToken), which is where window.rs places the window.
//! Channels: apps create and read back their own (notification_channels.rs), as a device answers (behaviour decision 1); nothing is displayed for them.
//! Posting: enqueueNotificationWithTag keeps a record per (pkg, tag, id, user) and cancelNotificationWithTag removes it, both checking the caller as AOSP does. Each post goes to the desktop through
//! [`DesktopNotifications`] (atld over the control socket, notifications plan N2) in its desktop form (notification_display.rs); a cancel withdraws it. The app observes AOSP's answers whatever
//! the desktop does: no notification service, or a failed call, is counted (`notification.not_shown reason=...`), never thrown.
//! Counters (atl_diag answered): `notification.posted`, `.shown`, `.updated`, `.withdrawn reason=app`, `.not_shown reason=<no-service|blocked|error>`, `.dropped_field <name>`.
//! Not modelled: rate limiting, per-user visibility, suspended packages, POST_NOTIFICATIONS, accessibility timeouts, enqueueTextToast (no system UI: answered false), channel groups,
//! getAppActiveNotifications (StatusBarNotification has no codec) (counted UNKNOWN).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use atl_aidl::aidl::android::app::INotificationManager::{
    BnNotificationManager, EnforcePermission, INotificationManager,
};
use atl_aidl::mangled::_7_android_3_app_22_ITransientNotification as ITransientNotification;
use atl_parcelables::android::app::{Notification, NotificationChannel};
use atl_parcelables::android::content::pm::ParceledListSlice;
use binder::{BinderFeatures, ExceptionCode, Interface, SpIBinder, Status, Strong, ThreadState};

use crate::notification_channels::{ChannelError, Channels};
use crate::notification_display;
use crate::packages::PackageTable;
use crate::packages::Permissions;
use crate::tasks::Tasks;
use atl_broker::control::{NotificationContent, NotifyOutcome};
use atl_services_types::permission;

/// The interface name counters use.
const INTERFACE: &str = "INotificationManager";
/// Settings.Global.ZEN_MODE_OFF (Settings.java:17663): Do Not Disturb off.
const ZEN_MODE_OFF: i32 = 0;
/// Manifest.permission.INTERACT_ACROSS_USERS: canNotifyAsPackage's check for another user's channels.
const INTERACT_ACROSS_USERS: &str = "android.permission.INTERACT_ACROSS_USERS";
/// UserHandle.USER_CURRENT: handleIncomingUser resolves it to the caller's own user.
const USER_CURRENT: i32 = -2;
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
    /// posted notifications (NotificationRecord), keyed (pkg, tag, id, user)
    posted: Vec<Posted>,
}

/// A posted notification's key. The Notification itself is not kept until a reader exists (getAppActiveNotifications).
struct Posted {
    pkg: String,
    tag: Option<String>,
    id: i32,
    user: u32,
    uid: u32,
}

impl Posted {
    /// StatusBarNotification.key (groupKey's sibling, StatusBarNotification.java key()): `user|pkg|id|tag|uid`; the desktop's key for this notification.
    fn key(&self) -> String {
        format!(
            "{}|{}|{}|{}|{}",
            self.user,
            self.pkg,
            self.id,
            self.tag.as_deref().unwrap_or("null"),
            self.uid
        )
    }
}

/// Where posted notifications are shown: the desktop, through atld (the control socket, notifications plan N2). Shaped by that one implementation.
pub trait DesktopNotifications: Send + Sync {
    /// Shows `package`'s notification `key` in `content`'s form, replacing the key's earlier one.
    fn notify(
        &self,
        package: &str,
        key: &str,
        content: NotificationContent,
    ) -> std::io::Result<NotifyOutcome>;
    /// Removes notification `key`; whether one of it was shown.
    fn withdraw(&self, key: &str) -> std::io::Result<bool>;
}

/// The foreground service notifications (ServiceRecord.postNotification, ActiveServices.cancelForegroundNotificationLocked), made by the activity service as the app's own
/// (no tag, the single user). A post is enqueueNotificationWithTag's record and display; a cancel is cancelNotificationWithTag's, without the caller checks.
pub trait ForegroundNotices: Send + Sync {
    /// Posts `notification` as `package`'s notification `id` (the caller added FLAG_FOREGROUND_SERVICE).
    fn post(&self, uid: u32, package: &str, id: i32, notification: &Notification);
    /// Cancels `package`'s notification `id` (no tag), if one is posted.
    fn cancel(&self, package: &str, id: i32);
}

/// Discards the foreground notices: the activity service's tests, which run without a notification service.
pub struct NoForegroundNotices;

impl ForegroundNotices for NoForegroundNotices {
    fn post(&self, _uid: u32, _package: &str, _id: i32, _notification: &Notification) {}
    fn cancel(&self, _package: &str, _id: i32) {}
}

struct Inner {
    tasks: Arc<Tasks<SpIBinder>>,
    permissions: Arc<Permissions>,
    packages: Arc<PackageTable>,
    desktop: Arc<dyn DesktopNotifications>,
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
                .snapshot()
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

    /// The caller checks enqueue and cancel share: resolveNotificationUid (pkg and opPkg must be the caller's: no delegates exist, else SecurityException), then handleIncomingUser (USER_CURRENT is
    /// the caller's own user; another user needs INTERACT_ACROSS_USERS). Returns (calling uid, resolved user).
    fn resolve_caller(
        &self,
        method: &str,
        pkg: &str,
        op_pkg: Option<&str>,
        user_id: i32,
    ) -> binder::Result<(u32, u32)> {
        let uid = self.check_same_app(method, pkg)?;
        if let Some(op_pkg) = op_pkg.filter(|o| *o != pkg) {
            self.check_same_app(method, op_pkg)?;
        }
        let own = permission::user_id(uid);
        if user_id == USER_CURRENT || i64::from(own) == i64::from(user_id) {
            return Ok((uid, own));
        }
        if !self.permissions.holds(uid, &[INTERACT_ACROSS_USERS], false) {
            return Err(Status::new_exception_str(
                ExceptionCode::SECURITY,
                Some(format!(
                    "{method} for user {user_id}: caller lacks {INTERACT_ACROSS_USERS}"
                )),
            ));
        }
        u32::try_from(user_id).map(|u| (uid, u)).map_err(|_| {
            Status::new_exception_str(
                ExceptionCode::ILLEGAL_ARGUMENT,
                Some(format!("{method}: user {user_id}")),
            )
        })
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
                atl_diag::answered(INTERFACE, &format!("toast show refused: {e:?}"));
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
                atl_diag::answered(INTERFACE, "toast timer: no thread");
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
            atl_diag::answered(INTERFACE, &format!("toast hide refused: {e:?}"));
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
            atl_diag::answered(INTERFACE, "toast token timer: no thread");
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
            atl_diag::refused(INTERFACE, "enqueueToast: no package, token or callback");
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
                atl_diag::refused(INTERFACE, "enqueueToast: MAX_PACKAGE_TOASTS queued");
                return Ok(false);
            }
            let Some(task) = inner.tasks.newest_task_of(uid) else {
                atl_diag::refused(INTERFACE, "enqueueToast: the caller has no Task");
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
        atl_diag::answered(INTERFACE, "enqueueToast (queued)");
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
        atl_diag::refused(
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

    /// cancelNotificationWithTag (:4219 -> cancelNotificationInternal:8282): handleIncomingUser (another user needs INTERACT_ACROSS_USERS), resolveNotificationUid (pkg and opPkg must be the
    /// caller's: no delegates exist, else SecurityException), then cancel the (pkg, tag, id, user) record and withdraw it from the desktop. An id never posted is a no-op, as AOSP's cancelNotification is.
    fn r#cancelNotificationWithTag(
        &self,
        pkg: Option<&str>,
        op_pkg: Option<&str>,
        tag: Option<&str>,
        id: i32,
        user_id: i32,
    ) -> binder::Result<()> {
        let pkg = pkg.ok_or_else(|| Status::new_exception(ExceptionCode::NULL_POINTER, None))?;
        let (_, user) = self
            .0
            .resolve_caller("cancelNotificationWithTag", pkg, op_pkg, user_id)?;
        let mut st = self.0.lock();
        let removed = st
            .posted
            .iter()
            .position(|p| p.pkg == pkg && p.tag.as_deref() == tag && p.id == id && p.user == user)
            .map(|i| st.posted.remove(i));
        drop(st);
        let Some(removed) = removed else {
            atl_diag::answered(INTERFACE, "cancelNotificationWithTag (nothing posted)");
            return Ok(());
        };
        atl_diag::answered(INTERFACE, "cancelNotificationWithTag (cancelled)");
        match self.0.desktop.withdraw(&removed.key()) {
            Ok(true) => {
                atl_diag::answered(INTERFACE, "notification.withdrawn reason=app");
            }
            Ok(false) => {}
            Err(e) => {
                atl_diag::answered(INTERFACE, &format!("notification.withdraw failed: {e}"));
            }
        }
        Ok(())
    }

    /// enqueueNotificationWithTag (:4141 -> enqueueNotificationInternal): null pkg or notification is IllegalArgumentException; the caller checks of cancelNotificationWithTag; a channel the app
    /// never created drops the notification with a log (AOSP "No Channel found", no exception); else the (pkg, tag, id, user) record is replaced or added and shown on the desktop (a blocked channel's is not).
    fn r#enqueueNotificationWithTag(
        &self,
        pkg: Option<&str>,
        op_pkg: Option<&str>,
        tag: Option<&str>,
        id: i32,
        notification: Option<&Notification>,
        user_id: i32,
    ) -> binder::Result<()> {
        let (Some(pkg), Some(notification)) = (pkg, notification) else {
            return Err(Status::new_exception_str(
                ExceptionCode::ILLEGAL_ARGUMENT,
                Some("null not allowed"),
            ));
        };
        let (uid, user) =
            self.0
                .resolve_caller("enqueueNotificationWithTag", pkg, op_pkg, user_id)?;
        enqueue(&self.0, uid, user, pkg, tag, id, notification);
        Ok(())
    }

    /// areNotificationsEnabled: AOSP's default for an app nobody blocked (NotificationManagerService areNotificationsEnabledForPackage -> PreferencesHelper, importance unspecified -> true).
    fn r#areNotificationsEnabled(&self, _pkg: Option<&str>) -> binder::Result<bool> {
        Ok(true)
    }

    /// getZenMode (:6142 -> ZenModeHelper.getZenMode:396): no permission check; Do Not Disturb is never on here, so Settings.Global.ZEN_MODE_OFF (0, Settings.java:17663).
    fn r#getZenMode(&self) -> binder::Result<i32> {
        atl_diag::answered(INTERFACE, "getZenMode");
        Ok(ZEN_MODE_OFF)
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
        atl_diag::answered(INTERFACE, "createNotificationChannels");
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
        atl_diag::answered(INTERFACE, "getNotificationChannel");
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
        atl_diag::answered(INTERFACE, "getNotificationChannels");
        Ok(Some(ParceledListSlice::new(
            self.0.lock().channels.list(uid, pkg),
        )))
    }
}

/// The service's state: its binder ([`Notices::binder`], for `addService`) and the foreground notices of the activity service. Toast windows are bound into `tasks`,
/// posted notifications shown on `desktop`.
pub struct Notices(Arc<Inner>);

impl Notices {
    pub fn new(
        tasks: Arc<Tasks<SpIBinder>>,
        permissions: Arc<Permissions>,
        packages: Arc<PackageTable>,
        desktop: Arc<dyn DesktopNotifications>,
    ) -> Self {
        Notices(Arc::new(Inner {
            tasks,
            permissions,
            packages,
            desktop,
            state: Mutex::new(State::default()),
            next_id: AtomicU64::new(0),
        }))
    }

    /// The INotificationManager binder over this state.
    pub fn binder(&self) -> SpIBinder {
        BnNotificationManager::new_binder(
            NotificationManager(Arc::clone(&self.0)),
            BinderFeatures::default(),
        )
        .as_binder()
    }
}

impl ForegroundNotices for Notices {
    fn post(&self, uid: u32, package: &str, id: i32, notification: &Notification) {
        enqueue(
            &self.0,
            uid,
            permission::user_id(uid),
            package,
            None,
            id,
            notification,
        );
    }

    fn cancel(&self, package: &str, id: i32) {
        cancel_foreground(&self.0, package, id);
    }
}

/// enqueueNotificationWithTag's record and display, shared by the binder and the foreground notices: the (pkg, tag, id, user) record is replaced or added, then shown
/// on the desktop unless its channel is blocked or was never created.
fn enqueue(
    inner: &Inner,
    uid: u32,
    user: u32,
    pkg: &str,
    tag: Option<&str>,
    id: i32,
    notification: &Notification,
) {
    let mut st = inner.lock();
    let channel_id = notification.channel_id.as_deref();
    let Some(channel) = st.channels.get(uid, pkg, channel_id) else {
        drop(st);
        atl_diag::answered(
            INTERFACE,
            &format!(
                "notification: No Channel found for pkg={pkg}, channelId={}: dropped",
                channel_id.unwrap_or("null")
            ),
        );
        return;
    };
    let before = st.posted.len();
    st.posted
        .retain(|p| !(p.pkg == pkg && p.tag.as_deref() == tag && p.id == id && p.user == user));
    let update = st.posted.len() != before;
    let posted = Posted {
        pkg: pkg.to_owned(),
        tag: tag.map(str::to_owned),
        id,
        user,
        uid,
    };
    let key = posted.key();
    st.posted.push(posted);
    drop(st);
    atl_diag::answered(INTERFACE, "notification.posted");
    // checkDisqualifyingFeatures (:9144): a blocked channel's post is recorded by the app's call and never shown
    if channel.importance == notification_display::IMPORTANCE_NONE {
        atl_diag::answered(INTERFACE, "notification.not_shown reason=blocked");
        return;
    }
    let (content, left_out) = notification_display::content(notification, &channel, update);
    for name in left_out {
        atl_diag::answered(INTERFACE, &format!("notification.dropped_field {name}"));
    }
    let outcome = match inner.desktop.notify(pkg, &key, content) {
        Ok(NotifyOutcome::Shown) => "notification.shown",
        Ok(NotifyOutcome::Updated) => "notification.updated",
        Ok(NotifyOutcome::NoService) => "notification.not_shown reason=no-service",
        Err(e) => {
            atl_diag::answered(INTERFACE, &format!("notification.notify failed: {e}"));
            "notification.not_shown reason=error"
        }
    };
    atl_diag::answered(INTERFACE, outcome);
}

/// cancelNotificationWithTag for a foreground service's notification (no tag): the record goes and the desktop withdraws it. Called under the activity service's lock:
/// the withdraw is one write to atld, and this path never takes the activity lock, so no cycle forms.
fn cancel_foreground(inner: &Inner, package: &str, id: i32) {
    let removed = {
        let mut st = inner.lock();
        let at = st
            .posted
            .iter()
            .position(|p| p.pkg == package && p.tag.is_none() && p.id == id);
        at.map(|i| st.posted.remove(i))
    };
    let Some(removed) = removed else {
        atl_diag::answered(INTERFACE, "foreground notification cancel (nothing posted)");
        return;
    };
    match inner.desktop.withdraw(&removed.key()) {
        Ok(true) => {
            atl_diag::answered(INTERFACE, "notification.withdrawn reason=foreground");
        }
        Ok(false) => {}
        Err(e) => {
            atl_diag::answered(INTERFACE, &format!("notification.withdraw failed: {e}"));
        }
    }
}
