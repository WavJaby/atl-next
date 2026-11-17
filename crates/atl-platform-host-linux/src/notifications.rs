// SPDX-License-Identifier: MIT
//! The desktop's notification service over the session bus: the freedesktop Desktop Notifications 1.3 service (`org.freedesktop.Notifications`), the baseline tier (notifications plan N1).
//! One shared connection; each key's desktop id is kept, so a re-post passes it as `replaces_id` and a withdraw closes it. The app's identity travels per call (`app_name`, hint `desktop-entry`).
//! Tier probe: at connect the name has an owner or is activatable (a desktop that starts its daemon on demand); after that every call re-decides: a call refused because nobody owns the name
//! (the bus's `org.freedesktop.DBus.Error.ServiceUnknown` / `NameHasNoOwner`) is tier none and sends nothing more for that post, a call answered is tier fdo again. No session bus at connect: tier none for the process's life.
//! zbus's own runtime (`default-rt`) runs the connection; each call blocks the calling thread (`zbus::block_on`), as atld's control thread expects.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use atl_platform_host_api::{
    DesktopNotification, NotificationError, NotificationTier, Notifications, Posted, Urgency,
};
use zbus::{Connection, Variant};

const NAME: &str = "org.freedesktop.Notifications";
const PATH: &str = "/org/freedesktop/Notifications";
const BUS: &str = "org.freedesktop.DBus";
const BUS_PATH: &str = "/org/freedesktop/DBus";
/// `expire_timeout` -1: the server's default; 0: never expires (spec "Notify").
const EXPIRE_DEFAULT: i32 = -1;
const EXPIRE_NEVER: i32 = 0;

/// The freedesktop notification service on the user's session bus.
pub struct FdoNotifications {
    bus: Option<Connection>,
    fdo: AtomicBool,
    /// key -> the id the service gave its notification
    ids: Mutex<HashMap<String, u32>>,
}

/// A call's outcome: answered, or refused because the name has no owner.
enum Call<T> {
    Answered(T),
    NoOwner,
}

impl FdoNotifications {
    fn ids(&self) -> std::sync::MutexGuard<'_, HashMap<String, u32>> {
        self.ids.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn call<B, R>(&self, method: &str, body: &B) -> Result<Call<R>, NotificationError>
    where
        B: zbus::export::serde::Serialize + zbus::Type,
        R: for<'d> zbus::export::serde::Deserialize<'d> + zbus::Type,
    {
        let Some(bus) = &self.bus else {
            return Ok(Call::NoOwner);
        };
        let reply = zbus::block_on(bus.call_method(Some(NAME), PATH, Some(NAME), method, body));
        match reply {
            Ok(m) => {
                self.fdo.store(true, Ordering::Relaxed);
                m.body()
                    .deserialize::<R>()
                    .map(Call::Answered)
                    .map_err(|e| NotificationError(format!("{method} reply: {e}")))
            }
            // zbus reports the bus's own error names as `Error::FDO`
            Err(zbus::Error::FDO(e))
                if matches!(
                    *e,
                    zbus::fdo::Error::ServiceUnknown(_) | zbus::fdo::Error::NameHasNoOwner(_)
                ) =>
            {
                self.fdo.store(false, Ordering::Relaxed);
                Ok(Call::NoOwner)
            }
            Err(e) => Err(NotificationError(format!("{method}: {e}"))),
        }
    }
}

/// Whether `bus` has a notification service now: an owner, or a service file that starts one.
fn served(bus: &Connection) -> bool {
    let has_owner =
        zbus::block_on(bus.call_method(Some(BUS), BUS_PATH, Some(BUS), "NameHasOwner", &(NAME,)))
            .and_then(|m| m.body().deserialize::<bool>());
    if matches!(has_owner, Ok(true)) {
        return true;
    }
    zbus::block_on(bus.call_method(Some(BUS), BUS_PATH, Some(BUS), "ListActivatableNames", &()))
        .and_then(|m| m.body().deserialize::<Vec<String>>())
        .is_ok_and(|names| names.iter().any(|n| n == NAME))
}

impl Notifications for FdoNotifications {
    fn connect() -> Self {
        let bus = zbus::block_on(Connection::session()).ok();
        let fdo = bus.as_ref().is_some_and(served);
        FdoNotifications {
            bus,
            fdo: AtomicBool::new(fdo),
            ids: Mutex::new(HashMap::new()),
        }
    }

    fn tier(&self) -> NotificationTier {
        if self.fdo.load(Ordering::Relaxed) {
            NotificationTier::Fdo
        } else {
            NotificationTier::None
        }
    }

    fn post(&self, key: &str, n: &DesktopNotification) -> Result<Posted, NotificationError> {
        let replaces = self.ids().get(key).copied();
        let mut hints: HashMap<&str, Variant<'_>> = HashMap::new();
        hints.insert(
            "urgency",
            Variant::from(match n.urgency {
                Urgency::Low => 0u8,
                Urgency::Normal => 1u8,
            }),
        );
        hints.insert("desktop-entry", Variant::from(n.app_id.as_str()));
        if n.silent {
            hints.insert("suppress-sound", Variant::from(true));
        }
        if n.resident {
            hints.insert("resident", Variant::from(true));
        }
        let actions: Vec<&str> = Vec::new();
        let body = (
            n.app_name.as_str(),
            replaces.unwrap_or(0),
            "",
            n.summary.as_str(),
            n.body.as_str(),
            actions,
            hints,
            if n.resident {
                EXPIRE_NEVER
            } else {
                EXPIRE_DEFAULT
            },
        );
        match self.call::<_, u32>("Notify", &body)? {
            Call::Answered(id) => {
                self.ids().insert(key.to_owned(), id);
                Ok(if replaces.is_some() {
                    Posted::Updated
                } else {
                    Posted::Shown
                })
            }
            Call::NoOwner => {
                // the service went: its ids mean nothing to the next one
                self.ids().clear();
                Ok(Posted::NoService)
            }
        }
    }

    fn withdraw(&self, key: &str) -> Result<bool, NotificationError> {
        let Some(id) = self.ids().remove(key) else {
            return Ok(false);
        };
        match self.call::<_, ()>("CloseNotification", &(id,))? {
            Call::Answered(()) => Ok(true),
            Call::NoOwner => {
                self.ids().clear();
                Ok(false)
            }
        }
    }
}
