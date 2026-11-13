// SPDX-License-Identifier: MIT
//! The process's own view of servicemanager (owner decision 2026-11-06): libbinder's default service manager becomes an in-process `android.os.IServiceManager` that answers the names this process
//! serves itself ([`serve`]) and forwards every other call to the real servicemanager (the binder context object, through binderd). One mechanism for every service an app process must find by a fixed
//! name it cannot register: its SurfaceFlinger (atl-composer), the Codec2 store, the media resource manager, the media extractor service (atl-media-extractors) and the media player service (made at first lookup, [`serve_lazy`]). Other processes never see these names: servicemanager stays one namespace of system services.
//! Native lookups only (`defaultServiceManager()`: libbinder, libbinder_ndk, libbinder_rs); Java's ServiceManager reads the context object itself and sees the real one.
//! [`install`] must run before anything calls `defaultServiceManager()` (atl-app-launcher: before `JNI_CreateJavaVM`). The real servicemanager is looked up at the first forwarded call, so a process
//! without a binder driver installs the view and never opens one unless it asks for a service.
//! Boundary crate (cxx, raw AIBinder): `unsafe` only at the two bridge calls.
#![cfg(feature = "converter")]
#![deny(unsafe_code)]

// the converter's rlibs, on the crate search path build.rs sets (no cargo dependency names them)
extern crate atl_parcelables;
extern crate binder;
extern crate gui_aidl_types_rs;
extern crate rustutils;

#[allow(unsafe_code)]
mod bridge;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, OnceLock};

use atl_aidl::aidl::android::os::ConnectionInfo::ConnectionInfo;
use atl_aidl::aidl::android::os::IClientCallback::IClientCallback;
use atl_aidl::aidl::android::os::IServiceCallback::IServiceCallback;
use atl_aidl::aidl::android::os::IServiceManager::{BnServiceManager, IServiceManager};
use atl_aidl::aidl::android::os::Service::Service;
use atl_aidl::aidl::android::os::ServiceDebugInfo::ServiceDebugInfo;
use atl_aidl::aidl::android::os::ServiceWithMetadata::ServiceWithMetadata;
use binder::unstable_api::{AsNative, new_spibinder};
use binder::{BinderFeatures, ExceptionCode, FromIBinder, SpIBinder, Status, Strong};

/// The installed view; set once by [`install`].
static VIEW: OnceLock<Strong<dyn IServiceManager>> = OnceLock::new();
/// The names this process serves.
static LOCAL: Mutex<BTreeMap<String, Arc<Entry>>> = Mutex::new(BTreeMap::new());

/// One served name: the binder, made at the first lookup for a [`serve_lazy`] name. Made outside the table lock, since a maker may itself look services up (ResourceManagerService's constructor asks for `batterystats`).
struct Entry {
    made: OnceLock<Option<SpIBinder>>,
    make: fn() -> Option<SpIBinder>,
}

impl Entry {
    fn binder(&self) -> Option<SpIBinder> {
        self.made.get_or_init(self.make).clone()
    }
}

/// Makes this process's view libbinder's default service manager. Call once, before any `defaultServiceManager()`: libbinder aborts the process if one already ran (IServiceManager.cpp `setDefaultServiceManager`).
pub fn install() -> Result<(), String> {
    let view = BnServiceManager::new_binder(View, BinderFeatures::default());
    let mut binder = view.as_binder();
    VIEW.set(view)
        .map_err(|_| "the service view is already installed".to_owned())?;
    // SAFETY: the pointer is the live AIBinder of `binder`; install only reads it (AIBinder_toPlatformBinder takes its own reference), and VIEW keeps the object alive for the process.
    #[allow(unsafe_code)]
    unsafe {
        bridge::ffi::install(binder.as_native_mut().cast());
    }
    Ok(())
}

/// Serves `service` as `name` to this process's lookups; a later `serve` of the same name replaces it. Refused before [`install`]: nothing would see it.
pub fn serve(name: &str, service: SpIBinder) -> Result<(), String> {
    let made = OnceLock::new();
    let _ = made.set(Some(service));
    insert(
        name,
        Entry {
            made,
            make: || None,
        },
    )
}

/// Serves `name` with the binder `make` returns at the first lookup that needs it (getService / checkService); a None from `make` answers that and every later lookup as absent. For a service whose construction itself talks to the real servicemanager, so a process that never asks for it (or has no binder driver) never pays for it. isDeclared and getDeclaredInstances answer without making it. Refused before [`install`].
pub fn serve_lazy(name: &str, make: fn() -> Option<SpIBinder>) -> Result<(), String> {
    insert(
        name,
        Entry {
            made: OnceLock::new(),
            make,
        },
    )
}

fn insert(name: &str, entry: Entry) -> Result<(), String> {
    if VIEW.get().is_none() {
        return Err(format!(
            "{name}: the service view is not installed (atl-app-launcher installs it)"
        ));
    }
    LOCAL
        .lock()
        .map_err(|_| "service view table poisoned".to_owned())?
        .insert(name.to_owned(), Arc::new(entry));
    Ok(())
}

fn entry(name: Option<&str>) -> Option<Arc<Entry>> {
    LOCAL.lock().ok()?.get(name?).cloned()
}

/// A served name's binder, made now for a lazy one (outside the table lock).
fn local(name: Option<&str>) -> Option<SpIBinder> {
    entry(name)?.binder()
}

/// The instances of `iface` this process serves (`iface/<instance>` names): servicemanager's declared list does not carry them (no VINTF entry), so getDeclaredInstances adds them.
fn local_instances(iface: Option<&str>) -> Vec<String> {
    let (Some(iface), Ok(served)) = (iface, LOCAL.lock()) else {
        return Vec::new();
    };
    served
        .keys()
        .filter_map(|name| name.strip_prefix(iface)?.strip_prefix('/'))
        .map(str::to_owned)
        .collect()
}

/// The real servicemanager, looked up at the first forwarded call; None (the call fails) when the process has no binder context object.
fn real() -> binder::Result<&'static Strong<dyn IServiceManager>> {
    static REAL: OnceLock<Option<Strong<dyn IServiceManager>>> = OnceLock::new();
    REAL.get_or_init(|| {
        let raw = bridge::ffi::context_object();
        // SAFETY: context_object returns null or an AIBinder with one reference owned by the caller, which new_spibinder takes over.
        #[allow(unsafe_code)]
        let binder = unsafe { new_spibinder(raw.cast()) }?;
        <dyn IServiceManager as FromIBinder>::try_from(binder).ok()
    })
    .as_ref()
    .ok_or_else(|| Status::new_exception(ExceptionCode::ILLEGAL_STATE, None))
}

/// The view's binder object. Every method not answered locally forwards with the same arguments; the deprecated getService/checkService too, since older callers still use them.
struct View;

impl binder::Interface for View {}

impl IServiceManager for View {
    fn r#getService(&self, name: Option<&str>) -> binder::Result<Option<SpIBinder>> {
        match local(name) {
            Some(b) => Ok(Some(b)),
            #[allow(deprecated)]
            None => real()?.r#getService(name),
        }
    }

    fn r#getService2(&self, name: Option<&str>) -> binder::Result<Option<Service>> {
        match local(name) {
            Some(b) => Ok(Some(with_metadata(b))),
            None => real()?.r#getService2(name),
        }
    }

    fn r#checkService(&self, name: Option<&str>) -> binder::Result<Option<SpIBinder>> {
        match local(name) {
            Some(b) => Ok(Some(b)),
            #[allow(deprecated)]
            None => real()?.r#checkService(name),
        }
    }

    fn r#checkService2(&self, name: Option<&str>) -> binder::Result<Option<Service>> {
        match local(name) {
            Some(b) => Ok(Some(with_metadata(b))),
            None => real()?.r#checkService2(name),
        }
    }

    fn r#addService(
        &self,
        name: Option<&str>,
        service: Option<&SpIBinder>,
        allow_isolated: bool,
        dump_priority: i32,
    ) -> binder::Result<()> {
        real()?.r#addService(name, service, allow_isolated, dump_priority)
    }

    fn r#listServices(&self, dump_priority: i32) -> binder::Result<Option<Vec<Option<String>>>> {
        real()?.r#listServices(dump_priority)
    }

    fn r#registerForNotifications(
        &self,
        name: Option<&str>,
        callback: Option<&Strong<dyn IServiceCallback>>,
    ) -> binder::Result<()> {
        real()?.r#registerForNotifications(name, callback)
    }

    fn r#unregisterForNotifications(
        &self,
        name: Option<&str>,
        callback: Option<&Strong<dyn IServiceCallback>>,
    ) -> binder::Result<()> {
        real()?.r#unregisterForNotifications(name, callback)
    }

    fn r#isDeclared(&self, name: Option<&str>) -> binder::Result<bool> {
        match entry(name) {
            Some(_) => Ok(true),
            None => real()?.r#isDeclared(name),
        }
    }

    fn r#getDeclaredInstances(
        &self,
        iface: Option<&str>,
    ) -> binder::Result<Option<Vec<Option<String>>>> {
        let served = local_instances(iface);
        if served.is_empty() {
            return real()?.r#getDeclaredInstances(iface);
        }
        let mut found = real()?.r#getDeclaredInstances(iface)?.unwrap_or_default();
        for instance in served {
            if !found
                .iter()
                .any(|f| f.as_deref() == Some(instance.as_str()))
            {
                found.push(Some(instance));
            }
        }
        Ok(Some(found))
    }

    fn r#updatableViaApex(&self, name: Option<&str>) -> binder::Result<Option<String>> {
        real()?.r#updatableViaApex(name)
    }

    fn r#getUpdatableNames(
        &self,
        apex_name: Option<&str>,
    ) -> binder::Result<Option<Vec<Option<String>>>> {
        real()?.r#getUpdatableNames(apex_name)
    }

    fn r#getConnectionInfo(&self, name: Option<&str>) -> binder::Result<Option<ConnectionInfo>> {
        real()?.r#getConnectionInfo(name)
    }

    fn r#registerClientCallback(
        &self,
        name: Option<&str>,
        service: Option<&SpIBinder>,
        callback: Option<&Strong<dyn IClientCallback>>,
    ) -> binder::Result<()> {
        real()?.r#registerClientCallback(name, service, callback)
    }

    fn r#tryUnregisterService(
        &self,
        name: Option<&str>,
        service: Option<&SpIBinder>,
    ) -> binder::Result<()> {
        real()?.r#tryUnregisterService(name, service)
    }

    fn r#getServiceDebugInfo(&self) -> binder::Result<Option<Vec<Option<ServiceDebugInfo>>>> {
        real()?.r#getServiceDebugInfo()
    }
}

/// getService2's answer for a local service: the binder, not lazy (servicemanager's own form for a registered service).
fn with_metadata(service: SpIBinder) -> Service {
    Service::ServiceWithMetadata(Some(ServiceWithMetadata {
        service: Some(service),
        isLazyService: false,
    }))
}
