// SPDX-License-Identifier: MIT
//! The `input` service (Context.INPUT_SERVICE): the converter's IInputManager stub, serving the desktop's device list: the virtual keyboard (id -1, KeyCharacterMap.VIRTUAL_KEYBOARD) and one pointer. Without this service InputManagerGlobal.getInstance() returns null and KeyCharacterMap.load NPEs at KeyCharacterMap.java:367 on the first options-panel prepare (PhoneWindow.java:843). KeyCharacterMap.load(VIRTUAL_KEYBOARD) (:365-376) takes the keyboard's native map from getInputDevice(-1), so the keyboard carries the staged Virtual.kcm (`validatekeymaps --parcel-out`, built by AOSP's own loader: the environment supplies the file, rung 2). The pointer carries the same bytes: it has no keys, and a second map would be a second file. Read and not kept: device change listeners (no hot-plug yet). Every other method stays counted and answered UNKNOWN_TRANSACTION (the trait default).

use atl_aidl::aidl::android::hardware::input::IInputManager::{
    BnInputManager, EnforcePermission, IInputManager,
};
use atl_parcelables::android::view::InputDevice;
use atl_parcelables::codecs::android::view::InputDevice::set_key_character_map;
use binder::{BinderFeatures, ExceptionCode, Interface, SpIBinder, Status, ThreadState};

use crate::{counters, permission};

/// The interface name counters use.
const INTERFACE: &str = "IInputManager";
/// InputManagerGlobal.VIRTUAL_KEYBOARD: KeyCharacterMap.load(deviceId) asks for it by this id.
const VIRTUAL_KEYBOARD: i32 = -1;
/// The pointer's id (the first id after the virtual keyboard's).
const POINTER: i32 = 1;
/// InputDevice.SOURCE_KEYBOARD (SOURCE_CLASS_BUTTON | 0x100) and SOURCE_MOUSE (SOURCE_CLASS_POINTER | 0x2000).
const SOURCE_KEYBOARD: i32 = 0x0000_0101;
const SOURCE_MOUSE: i32 = 0x0000_2002;
/// InputDevice.KEYBOARD_TYPE_ALPHABETIC; the pointer has no keyboard type (KEYBOARD_TYPE_NONE = 0).
const KEYBOARD_TYPE_ALPHABETIC: i32 = 2;
/// Display.INVALID_DISPLAY: the Builder default, a device not tied to a display.
const INVALID_DISPLAY: i32 = -1;

struct InputManager {
    devices: Vec<InputDevice>,
}

impl Interface for InputManager {}

/// The desktop's devices; both carry the staged keyboard map (see the module doc).
fn devices(map: &[u8]) -> Vec<InputDevice> {
    let base = InputDevice {
        key_character_map: map.to_vec(),
        generation: 1,
        usi_version_major: -1,
        usi_version_minor: -1,
        associated_display_id: INVALID_DISPLAY,
        enabled: true,
        ..InputDevice::default()
    };
    vec![
        InputDevice {
            id: VIRTUAL_KEYBOARD,
            name: Some("Virtual".into()),
            descriptor: Some("atl:virtual-keyboard".into()),
            sources: SOURCE_KEYBOARD,
            keyboard_type: KEYBOARD_TYPE_ALPHABETIC,
            ..base.clone()
        },
        InputDevice {
            id: POINTER,
            name: Some("ATL pointer".into()),
            descriptor: Some("atl:pointer".into()),
            sources: SOURCE_MOUSE,
            ..base
        },
    ]
}

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

    /// Never null: InputManagerGlobal.populateInputDevicesLocked iterates the array (:265).
    fn r#getInputDeviceIds(&self) -> binder::Result<Option<Vec<i32>>> {
        Ok(Some(self.devices.iter().map(|d| d.id).collect()))
    }

    /// The device with that id, or null for an unknown one (InputManagerGlobal.getInputDevice, :237).
    fn r#getInputDevice(&self, device_id: i32) -> binder::Result<Option<InputDevice>> {
        Ok(self.devices.iter().find(|d| d.id == device_id).cloned())
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

/// A new input service binder serving the staged keyboard map `map` (the bytes of `validatekeymaps --parcel-out`). Fails when `map` is not a Parcel-form blob.
pub fn new_binder(map: &[u8]) -> Result<SpIBinder, String> {
    set_key_character_map(map).map_err(str::to_owned)?;
    let service = InputManager {
        devices: devices(map),
    };
    Ok(BnInputManager::new_binder(service, BinderFeatures::default()).as_binder())
}
