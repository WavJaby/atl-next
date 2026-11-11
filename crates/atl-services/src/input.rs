// SPDX-License-Identifier: MIT
//! The `input` service (Context.INPUT_SERVICE): the converter's IInputManager stub, serving the desktop's device list (ids: input_devices.rs): the virtual keyboard (id -1, KeyCharacterMap.VIRTUAL_KEYBOARD), the mouse, the physical keyboard, the touchscreen and the touchpad. Without this service InputManagerGlobal.getInstance() returns null and KeyCharacterMap.load NPEs at KeyCharacterMap.java:367 on the first options-panel prepare (PhoneWindow.java:843). KeyCharacterMap.load(deviceId) (:365-376) takes a device's native map from getInputDevice(deviceId): the virtual keyboard carries Virtual.kcm, the physical keyboard Generic.kcm (KeyMap::load's fallback for a keyboard with no own map; KeyEvent.getUnicodeChar of its keys reads it), both in Parcel form, loaded by AOSP's own libinput through atl-libinput (the environment supplies the files, rung 2). Devices without keys carry Virtual.kcm, as Android gives every device some map. Non-US layouts (AOSP overlay KCMs picked by the desktop's xkb layout) are not built: decided with the IME item. Read and not kept: device change listeners (no hot-plug yet). Every other method stays counted and answered UNKNOWN_TRANSACTION (the trait default).

use atl_aidl::aidl::android::hardware::input::IInputManager::{
    BnInputManager, EnforcePermission, IInputManager,
};
use atl_parcelables::android::view::{InputDevice, PointerIcon};
use atl_parcelables::codecs::android::view::InputDevice::set_key_character_map;
use binder::{BinderFeatures, ExceptionCode, Interface, SpIBinder, Status, ThreadState};

use crate::ui_agent::{IUiAgent, UiAgents};
use atl_services_types::counters;
use atl_services_types::input_devices::{
    KEYBOARD, POINTER, SOURCE_KEYBOARD, SOURCE_MOUSE, SOURCE_TOUCHPAD, SOURCE_TOUCHSCREEN,
    TOUCHPAD, TOUCHSCREEN, VIRTUAL_KEYBOARD,
};
use atl_services_types::permission::Permissions;
use std::sync::Arc;

/// The interface name counters use.
const INTERFACE: &str = "IInputManager";
/// InputDevice.KEYBOARD_TYPE_ALPHABETIC; pointer devices have no keyboard type (KEYBOARD_TYPE_NONE = 0).
const KEYBOARD_TYPE_ALPHABETIC: i32 = 2;
/// Display.INVALID_DISPLAY: the Builder default, a device not tied to a display.
const INVALID_DISPLAY: i32 = -1;

struct InputManager {
    permissions: Arc<Permissions>,
    devices: Vec<InputDevice>,
    /// The apps' UI agents: setPointerIcon goes to the caller's.
    agents: Arc<UiAgents>,
}

impl Interface for InputManager {}

/// The desktop's devices with their key maps (see the module doc): `virtual_map` Virtual.kcm's Parcel form, `keyboard_map` Generic.kcm's.
fn devices(virtual_map: &[u8], keyboard_map: &[u8]) -> Vec<InputDevice> {
    let base = InputDevice {
        key_character_map: virtual_map.to_vec(),
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
            ..base.clone()
        },
        InputDevice {
            id: KEYBOARD,
            name: Some("ATL keyboard".into()),
            descriptor: Some("atl:keyboard".into()),
            sources: SOURCE_KEYBOARD,
            keyboard_type: KEYBOARD_TYPE_ALPHABETIC,
            key_character_map: keyboard_map.to_vec(),
            ..base.clone()
        },
        InputDevice {
            id: TOUCHSCREEN,
            name: Some("ATL touchscreen".into()),
            descriptor: Some("atl:touchscreen".into()),
            sources: SOURCE_TOUCHSCREEN,
            ..base.clone()
        },
        InputDevice {
            id: TOUCHPAD,
            name: Some("ATL touchpad".into()),
            descriptor: Some("atl:touchpad".into()),
            sources: SOURCE_MOUSE | SOURCE_TOUCHPAD,
            ..base
        },
    ]
}

impl IInputManager for InputManager {
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

    fn r#unimplemented(&self, method: &'static str) -> Status {
        crate::unimplemented(INTERFACE, method)
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
        counters::answered(INTERFACE, "registerInputDevicesChangedListener (not kept)");
        Ok(())
    }

    /// ViewRootImpl.updatePointerIcon calls this when the icon resolved for a mouse event changed. Relayed (oneway) to the calling app's UI agent, which shows it on its window; true once relayed (InputManager.setPointerIcon: whether the icon was set).
    /// The token is unused: one window per app (multi-window resolves `inputToken` to its window). A caller with no agent (headless) or no icon -> false, counted.
    fn r#setPointerIcon(
        &self,
        icon: Option<&PointerIcon>,
        _display_id: i32,
        _device_id: i32,
        _pointer_id: i32,
        _input_token: Option<&SpIBinder>,
    ) -> binder::Result<bool> {
        let agent = self
            .agents
            .get(ThreadState::get_calling_uid())
            .and_then(|b| b.into_interface::<dyn IUiAgent>().ok());
        let (Some(icon), Some(agent)) = (icon, agent) else {
            counters::answered(INTERFACE, "setPointerIcon (no icon or no UI agent)");
            return Ok(false);
        };
        Ok(agent.pointer_icon(icon.kind).is_ok())
    }
}

/// A new input service binder serving the key maps `virtual_map` (Virtual.kcm) and `keyboard_map` (Generic.kcm), each the Parcel form of a KeyCharacterMap (atl-libinput `KeyCharacterMap::parcel_bytes`), relaying pointer icons to `agents` and checking @EnforcePermission against `permissions`. Fails when a map is not a Parcel-form blob.
pub fn new_binder(
    virtual_map: &[u8],
    keyboard_map: &[u8],
    agents: Arc<UiAgents>,
    permissions: Arc<Permissions>,
) -> Result<SpIBinder, String> {
    // the codec reads InputDevices against the virtual map (InputDevice.rs); the keyboard's is written only
    set_key_character_map(virtual_map).map_err(str::to_owned)?;
    if keyboard_map.is_empty() || !keyboard_map.len().is_multiple_of(4) {
        return Err("keyboard key map: not a non-empty multiple of 4 bytes".to_owned());
    }
    let service = InputManager {
        devices: devices(virtual_map, keyboard_map),
        agents,
        permissions,
    };
    Ok(BnInputManager::new_binder(service, BinderFeatures::default()).as_binder())
}
