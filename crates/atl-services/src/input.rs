// SPDX-License-Identifier: MIT
//! The `input` service (Context.INPUT_SERVICE): the converter's IInputManager stub. Without it InputManagerGlobal.getInstance() returns null, and KeyCharacterMap.load NPEs at KeyCharacterMap.java:367 on the first options-panel prepare (PhoneWindow.java:843). Answered here: the four calls InputManagerGlobal makes on this path (:159, :237, :255, :265). No input device is reported yet: AOSP's InputDevice reply carries a native KeyCharacterMap that ATL does not produce, so KeyCharacterMap.load throws UnavailableException (KeyCharacterMap.java:369-372) instead of NPE-ing. Every other method stays counted and answered UNKNOWN_TRANSACTION (the trait default).

use atl_aidl::aidl::android::hardware::input::IInputManager::{
    BnInputManager, EnforcePermission, IInputManager,
};
use binder::{BinderFeatures, ExceptionCode, Interface, SpIBinder, Status, ThreadState};

use crate::{counters, permission};

/// The interface name counters use.
const INTERFACE: &str = "IInputManager";

struct InputManager;

impl Interface for InputManager {}

impl IInputManager for InputManager {
    /// Fail-closed like the other services' hooks (input_method.rs, window.rs): the permissions the stub names are held by root / system only.
    fn r#enforce_permission(
        &self,
        method: &str,
        permission: EnforcePermission<'_>,
    ) -> binder::Result<()> {
        let perms: &[&str] = match &permission {
            EnforcePermission::Single(p) => std::slice::from_ref(p),
            EnforcePermission::AllOf(p) | EnforcePermission::AnyOf(p) => p,
        };
        if permission::holds(ThreadState::get_calling_uid(), perms) {
            return Ok(());
        }
        Err(Status::new_exception_str(
            ExceptionCode::SECURITY,
            Some(format!("{method}: caller lacks {permission:?}")),
        ))
    }

    fn r#unimplemented(&self, method: &'static str) -> Status {
        counters::unimplemented(INTERFACE, method)
    }

    /// Null selects the default VelocityTracker strategy; InputManagerGlobal.java:159 keeps null as is.
    fn r#getVelocityTrackerStrategy(&self) -> binder::Result<Option<String>> {
        Ok(None)
    }

    /// An empty array, never null: InputManagerGlobal.populateInputDevicesLocked iterates it (:265).
    fn r#getInputDeviceIds(&self) -> binder::Result<Option<Vec<i32>>> {
        counters::count(
            INTERFACE,
            "getInputDeviceIds (empty: no KeyCharacterMap source)",
        );
        Ok(Some(Vec::new()))
    }

    /// Null: the caller gets no InputDevice (InputManagerGlobal.getInputDevice, :237).
    fn r#getInputDevice(
        &self,
        _device_id: i32,
    ) -> binder::Result<Option<atl_parcelables::android::view::InputDevice>> {
        counters::count(
            INTERFACE,
            "getInputDevice (null: no KeyCharacterMap source)",
        );
        Ok(None)
    }

    /// Read and not kept: no device changes are reported yet.
    fn r#registerInputDevicesChangedListener(
        &self,
        _listener: Option<
            &binder::Strong<
                dyn atl_aidl::mangled::_7_android_8_hardware_5_input_28_IInputDevicesChangedListener,
            >,
        >,
    ) -> binder::Result<()> {
        counters::count(INTERFACE, "registerInputDevicesChangedListener (not kept)");
        Ok(())
    }
}

/// A new input service binder.
pub fn new_binder() -> SpIBinder {
    BnInputManager::new_binder(InputManager, BinderFeatures::default()).as_binder()
}
