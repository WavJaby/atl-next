// SPDX-License-Identifier: MIT
//! The `input` service (Context.INPUT_SERVICE): the converter's IInputManager stub, serving the desktop's device list (ids: input_devices.rs): the virtual keyboard (id -1, KeyCharacterMap.VIRTUAL_KEYBOARD), the mouse, the physical keyboard, the touchscreen and the touchpad. Without this service InputManagerGlobal.getInstance() returns null and KeyCharacterMap.load NPEs at KeyCharacterMap.java:367 on the first options-panel prepare (PhoneWindow.java:843). KeyCharacterMap.load(deviceId) (:365-376) takes a device's native map from getInputDevice(deviceId): the virtual keyboard carries Virtual.kcm, the physical keyboard Generic.kcm (KeyMap::load's fallback for a keyboard with no own map; KeyEvent.getUnicodeChar of its keys reads it), both in Parcel form, loaded by AOSP's own libinput through atl-libinput (the environment supplies the files, rung 2). Devices without keys carry Virtual.kcm, as Android gives every device some map. Non-US layouts (AOSP overlay KCMs picked by the desktop's xkb layout) are not built: decided with the IME item. Read and not kept: device change listeners (no hot-plug yet). Every other method stays counted and answered UNKNOWN_TRANSACTION (the trait default).

use atl_aidl::aidl::android::hardware::input::IInputManager::{
    BnInputManager, EnforcePermission, IInputManager,
};
use atl_parcelables::android::view::{InputDevice, PointerIcon};
use atl_parcelables::codecs::android::view::InputDevice::set_key_character_map;
use binder::{BinderFeatures, ExceptionCode, Interface, SpIBinder, Status, ThreadState};

use crate::packages::Permissions;
use crate::ui_agents::{AppProcess, UiAgents};
use atl_services_types::input_devices::{
    KEYBOARD, POINTER, SOURCE_KEYBOARD, SOURCE_MOUSE, SOURCE_TOUCHPAD, SOURCE_TOUCHSCREEN,
    TOUCHPAD, TOUCHSCREEN, VIRTUAL_KEYBOARD,
};
use atl_services_wire::ui_agent::IUiAgent;
use std::sync::Arc;

/// The interface name counters use.
const INTERFACE: &str = "IInputManager";
/// InputDevice.KEYBOARD_TYPE_ALPHABETIC; pointer devices have no keyboard type (KEYBOARD_TYPE_NONE = 0).
const KEYBOARD_TYPE_ALPHABETIC: i32 = 2;
/// Display.INVALID_DISPLAY: the Builder default, a device not tied to a display.
const INVALID_DISPLAY: i32 = -1;
/// PointerIcon.TYPE_ARROW (TYPE_DEFAULT): what a custom icon shows until the image cursor lands.
const TYPE_ARROW: i32 = 1000;

struct InputManager {
    permissions: Arc<Permissions>,
    devices: Vec<InputDevice>,
    /// The app processes' UI agents: setPointerIcon goes to the calling process's.
    agents: Arc<UiAgents>,
}

impl Interface for InputManager {}

/// The desktop devices' key maps, each the Parcel form of a KeyCharacterMap (atl-libinput `KeyCharacterMap::parcel_bytes`): the one copy both getInputDevice and the compositor registry's KEY_MAP serve.
pub struct KeyMaps {
    /// Virtual.kcm: the virtual keyboard's, and every device without keys
    virtual_map: Vec<u8>,
    /// Generic.kcm (see [`keyboard_map`]): the physical keyboard's
    keyboard_map: Vec<u8>,
}

impl KeyMaps {
    /// `virtual_map` Virtual.kcm's Parcel form, `keyboard_map` Generic.kcm's. Fails when the keyboard's is not a Parcel-form blob (whole 32-bit words).
    pub fn new(virtual_map: Vec<u8>, keyboard_map: Vec<u8>) -> Result<KeyMaps, String> {
        if keyboard_map.is_empty() || !keyboard_map.len().is_multiple_of(4) {
            return Err("keyboard key map: not a non-empty multiple of 4 bytes".to_owned());
        }
        Ok(KeyMaps {
            virtual_map,
            keyboard_map,
        })
    }

    /// Device `device_id`'s map, as [`devices`] gives it; None for an id that is no device.
    pub fn of(&self, device_id: i32) -> Option<&[u8]> {
        match device_id {
            KEYBOARD => Some(&self.keyboard_map),
            VIRTUAL_KEYBOARD | POINTER | TOUCHSCREEN | TOUCHPAD => Some(&self.virtual_map),
            _ => None,
        }
    }
}

/// The desktop's devices with their key maps (see the module doc and [`KeyMaps::of`]).
fn devices(maps: &KeyMaps) -> Vec<InputDevice> {
    let base = InputDevice {
        key_character_map: maps.virtual_map.clone(),
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
            key_character_map: maps.keyboard_map.clone(),
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
        atl_diag::answered(INTERFACE, "registerInputDevicesChangedListener (not kept)");
        Ok(())
    }

    /// ViewRootImpl.updatePointerIcon calls this when the icon resolved for a mouse event changed. Relayed (oneway) to the calling process's UI agent, which shows it on its window; true once relayed (InputManager.setPointerIcon: whether the icon was set).
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
            .agent(AppProcess::calling())
            .and_then(|b| b.into_interface::<dyn IUiAgent>().ok());
        let (Some(icon), Some(agent)) = (icon, agent) else {
            atl_diag::answered(INTERFACE, "setPointerIcon (no icon or no UI agent)");
            return Ok(false);
        };
        // A custom icon's image is not shown yet, so the default arrow stands in rather than the previous view's cursor.
        let kind = if icon.custom.is_some() {
            atl_diag::answered(INTERFACE, "setPointerIcon (custom icon shown as default)");
            TYPE_ARROW
        } else {
            icon.kind
        };
        Ok(agent.pointer_icon(kind).is_ok())
    }
}

/// ATL's desktop key-map overlay (`type OVERLAY`): ESCAPE `base: fallback BACK`, the Esc -> Back adaptation (DesktopInput::esc_back). Its own file, never a patch to Generic.kcm, so off is AOSP's map.
pub const ESC_BACK_OVERLAY: &str = include_str!("../keychars/atl-desktop.kcm");
/// The overlay's load name (KeyCharacterMap::loadContents); combine keeps the base's, so the served map stays `Generic.kcm`.
const ESC_BACK_OVERLAY_NAME: &str = "atl-desktop.kcm";

/// The physical keyboard's map in Parcel form: Generic.kcm's `contents` loaded under `name`, combined with [`ESC_BACK_OVERLAY`] when `esc_back` (AOSP's keyboard-layout overlay route, KeyCharacterMap::combine).
pub fn keyboard_map(
    name: &str,
    contents: &[u8],
    esc_back: bool,
) -> Result<Vec<u8>, atl_libinput::Error> {
    let mut map = atl_libinput::KeyCharacterMap::load(name, contents)?;
    if esc_back {
        let overlay = atl_libinput::KeyCharacterMap::load_overlay(
            ESC_BACK_OVERLAY_NAME,
            ESC_BACK_OVERLAY.as_bytes(),
        )?;
        map.combine(&overlay);
    }
    Ok(map.parcel_bytes())
}

/// A new input service binder serving `maps` in its devices, relaying pointer icons to `agents` and checking @EnforcePermission against `permissions`. Fails when the virtual map is not a Parcel-form blob.
pub fn new_binder(
    maps: &KeyMaps,
    agents: Arc<UiAgents>,
    permissions: Arc<Permissions>,
) -> Result<SpIBinder, String> {
    // the codec reads InputDevices against the virtual map (InputDevice.rs); the keyboard's is written only
    set_key_character_map(&maps.virtual_map).map_err(str::to_owned)?;
    let service = InputManager {
        devices: devices(maps),
        agents,
        permissions,
    };
    Ok(BnInputManager::new_binder(service, BinderFeatures::default()).as_binder())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// android/keycodes.h; android/input.h AMETA_ALT_ON, AMETA_CTRL_ON.
    const ESCAPE: i32 = 111;
    const BACK: i32 = 4;
    const HOME: i32 = 3;
    const MENU: i32 = 82;
    const ALT_ON: i32 = 0x02;
    const CTRL_ON: i32 = 0x1000;
    /// Pinned Generic.kcm's ESCAPE block (frameworks/base/data/keyboards/Generic.kcm:501-505) in a minimal base map.
    const GENERIC_ESCAPE: &str = "type FULL\nkey ESCAPE {\n    base: none\n    alt: fallback HOME\n    ctrl: fallback MENU\n}\n";

    fn fallbacks(esc_back: bool) -> [Option<(i32, i32)>; 3] {
        let bytes =
            keyboard_map("Generic.kcm", GENERIC_ESCAPE.as_bytes(), esc_back).expect("map builds");
        // read back as an app's InputDevice does
        let map = atl_libinput::KeyCharacterMap::from_parcel(&bytes).expect("parcel reads");
        [0, ALT_ON, CTRL_ON].map(|meta| map.fallback_action(ESCAPE, meta))
    }

    /// On: plain Esc falls back to BACK; Generic.kcm's alt / ctrl fallbacks stay (the overlay restates them, as combine replaces the key whole).
    #[test]
    fn esc_back_on_falls_back_to_back() {
        assert_eq!(
            fallbacks(true),
            [Some((BACK, 0)), Some((HOME, 0)), Some((MENU, 0))]
        );
    }

    /// Off: exact AOSP, plain Esc has no fallback.
    #[test]
    fn esc_back_off_is_aosp() {
        assert_eq!(fallbacks(false), [None, Some((HOME, 0)), Some((MENU, 0))]);
    }
}
