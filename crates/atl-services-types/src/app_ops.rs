// SPDX-License-Identifier: MIT
//! App-op modes as AppOpsService answers them for an app nobody changed (the `appops` service, appops.rs): no per-uid or per-package mode was ever set, so an op's mode is its switch op's
//! default (AppOpsManager.opToDefaultMode(opToSwitch(code)), checkOperationUnchecked's fall-through), except where PermissionPolicyService sets the uid mode from a runtime permission's grant
//! (synchronizePackagePermissionsAndAppOpsForUser, over the package's requested permissions only: granted -> MODE_ALLOWED, denied -> MODE_IGNORED; an unrequested one keeps the default). Foreground-only grants, restrictions, suspension and user-set modes are not modelled.
//! Plain module: the table and the rules carry no binder, so their tests run in workspace builds.

/// AppOpsManager.MODE_ALLOWED / MODE_IGNORED / MODE_ERRORED / MODE_DEFAULT / MODE_FOREGROUND.
pub const ALLOWED: i32 = 0;
pub const IGNORED: i32 = 1;
pub const ERRORED: i32 = 2;
pub const DEFAULT: i32 = 3;
pub const FOREGROUND: i32 = 4;

/// One op's AppOpInfo: default mode, switch code, permission.
pub type Op = (i32, usize, Option<&'static str>);

/// AppOpsManager.sAppOpInfos at the pin, index = op code (`_NUM_OP` = the length). Checked row by row against the pin by `cargo xtask check` step `tables` (xtask/src/app_ops_table.rs).
/// Absent `setDefaultMode` -> MODE_DEFAULT (AppOpInfo.Builder's default); absent `setSwitchCode` -> the op itself. SYSTEM_ALERT_WINDOW's getSystemAlertWindowDefault() = MODE_DEFAULT on a desktop, which is not low-RAM.
/// READ_HEART_RATE, READ_SKIN_TEMPERATURE, READ_OXYGEN_SATURATION, RANGING, WRITE_SYSTEM_PREFERENCES and the XR tracking ops take a permission only behind flags, taken flag-off -> none.
/// READ_SYSTEM_GRAMMATICAL_GENDER: its setPermission is commented out at the pin -> none.
#[rustfmt::skip]
pub const OPS: [Op; 163] = [
    (ALLOWED, 0, Some("android.permission.ACCESS_COARSE_LOCATION")), // 0 COARSE_LOCATION
    (ALLOWED, 1, Some("android.permission.ACCESS_FINE_LOCATION")), // 1 FINE_LOCATION
    (ALLOWED, 0, None), // 2 GPS
    (ALLOWED, 3, Some("android.permission.VIBRATE")), // 3 VIBRATE
    (ALLOWED, 4, Some("android.permission.READ_CONTACTS")), // 4 READ_CONTACTS
    (ALLOWED, 5, Some("android.permission.WRITE_CONTACTS")), // 5 WRITE_CONTACTS
    (ALLOWED, 6, Some("android.permission.READ_CALL_LOG")), // 6 READ_CALL_LOG
    (ALLOWED, 7, Some("android.permission.WRITE_CALL_LOG")), // 7 WRITE_CALL_LOG
    (ALLOWED, 8, Some("android.permission.READ_CALENDAR")), // 8 READ_CALENDAR
    (ALLOWED, 9, Some("android.permission.WRITE_CALENDAR")), // 9 WRITE_CALENDAR
    (ALLOWED, 0, Some("android.permission.ACCESS_WIFI_STATE")), // 10 WIFI_SCAN
    (ALLOWED, 11, Some("android.permission.POST_NOTIFICATIONS")), // 11 POST_NOTIFICATION
    (ALLOWED, 0, None), // 12 NEIGHBORING_CELLS
    (ALLOWED, 13, Some("android.permission.CALL_PHONE")), // 13 CALL_PHONE
    (ALLOWED, 14, Some("android.permission.READ_SMS")), // 14 READ_SMS
    (IGNORED, 15, None), // 15 WRITE_SMS
    (ALLOWED, 16, Some("android.permission.RECEIVE_SMS")), // 16 RECEIVE_SMS
    (ALLOWED, 16, Some("android.permission.RECEIVE_EMERGENCY_BROADCAST")), // 17 RECEIVE_EMERGECY_SMS
    (ALLOWED, 18, Some("android.permission.RECEIVE_MMS")), // 18 RECEIVE_MMS
    (ALLOWED, 19, Some("android.permission.RECEIVE_WAP_PUSH")), // 19 RECEIVE_WAP_PUSH
    (ALLOWED, 20, Some("android.permission.SEND_SMS")), // 20 SEND_SMS
    (ALLOWED, 14, Some("android.permission.READ_SMS")), // 21 READ_ICC_SMS
    (ALLOWED, 15, None), // 22 WRITE_ICC_SMS
    (DEFAULT, 23, Some("android.permission.WRITE_SETTINGS")), // 23 WRITE_SETTINGS
    (DEFAULT, 24, Some("android.permission.SYSTEM_ALERT_WINDOW")), // 24 SYSTEM_ALERT_WINDOW
    (DEFAULT, 25, Some("android.permission.ACCESS_NOTIFICATIONS")), // 25 ACCESS_NOTIFICATIONS
    (ALLOWED, 26, Some("android.permission.CAMERA")), // 26 CAMERA
    (ALLOWED, 27, Some("android.permission.RECORD_AUDIO")), // 27 RECORD_AUDIO
    (ALLOWED, 28, None), // 28 PLAY_AUDIO
    (ALLOWED, 29, None), // 29 READ_CLIPBOARD
    (ALLOWED, 30, None), // 30 WRITE_CLIPBOARD
    (ALLOWED, 31, None), // 31 TAKE_MEDIA_BUTTONS
    (FOREGROUND, 32, None), // 32 TAKE_AUDIO_FOCUS
    (ALLOWED, 33, None), // 33 AUDIO_MASTER_VOLUME
    (ALLOWED, 34, None), // 34 AUDIO_VOICE_VOLUME
    (ALLOWED, 35, None), // 35 AUDIO_RING_VOLUME
    (ALLOWED, 36, None), // 36 AUDIO_MEDIA_VOLUME
    (ALLOWED, 37, None), // 37 AUDIO_ALARM_VOLUME
    (ALLOWED, 38, None), // 38 AUDIO_NOTIFICATION_VOLUME
    (ALLOWED, 39, None), // 39 AUDIO_BLUETOOTH_VOLUME
    (ALLOWED, 40, Some("android.permission.WAKE_LOCK")), // 40 WAKE_LOCK
    (ALLOWED, 0, None), // 41 MONITOR_LOCATION
    (ALLOWED, 0, None), // 42 MONITOR_HIGH_POWER_LOCATION
    (DEFAULT, 43, Some("android.permission.PACKAGE_USAGE_STATS")), // 43 GET_USAGE_STATS
    (ALLOWED, 44, None), // 44 MUTE_MICROPHONE
    (ALLOWED, 45, None), // 45 TOAST_WINDOW
    (IGNORED, 46, None), // 46 PROJECT_MEDIA
    (IGNORED, 47, None), // 47 ACTIVATE_VPN
    (ALLOWED, 48, None), // 48 WRITE_WALLPAPER
    (ALLOWED, 49, None), // 49 ASSIST_STRUCTURE
    (ALLOWED, 50, None), // 50 ASSIST_SCREENSHOT
    (ALLOWED, 51, Some("android.permission.READ_PHONE_STATE")), // 51 READ_PHONE_STATE
    (ALLOWED, 52, Some("android.permission.ADD_VOICEMAIL")), // 52 ADD_VOICEMAIL
    (ALLOWED, 53, Some("android.permission.USE_SIP")), // 53 USE_SIP
    (ALLOWED, 54, Some("android.permission.PROCESS_OUTGOING_CALLS")), // 54 PROCESS_OUTGOING_CALLS
    (ALLOWED, 55, Some("android.permission.USE_FINGERPRINT")), // 55 USE_FINGERPRINT
    (ALLOWED, 56, Some("android.permission.BODY_SENSORS")), // 56 BODY_SENSORS
    (ALLOWED, 57, Some("android.permission.READ_CELL_BROADCASTS")), // 57 READ_CELL_BROADCASTS
    (ERRORED, 58, None), // 58 MOCK_LOCATION
    (ALLOWED, 59, Some("android.permission.READ_EXTERNAL_STORAGE")), // 59 READ_EXTERNAL_STORAGE
    (ALLOWED, 60, Some("android.permission.WRITE_EXTERNAL_STORAGE")), // 60 WRITE_EXTERNAL_STORAGE
    (DEFAULT, 61, Some("android.permission.TURN_SCREEN_ON")), // 61 TURN_SCREEN_ON
    (ALLOWED, 62, Some("android.permission.GET_ACCOUNTS")), // 62 GET_ACCOUNTS
    (ALLOWED, 63, None), // 63 RUN_IN_BACKGROUND
    (ALLOWED, 64, None), // 64 AUDIO_ACCESSIBILITY_VOLUME
    (ALLOWED, 65, Some("android.permission.READ_PHONE_NUMBERS")), // 65 READ_PHONE_NUMBERS
    (DEFAULT, 66, Some("android.permission.REQUEST_INSTALL_PACKAGES")), // 66 REQUEST_INSTALL_PACKAGES
    (ALLOWED, 67, None), // 67 PICTURE_IN_PICTURE
    (DEFAULT, 68, Some("android.permission.INSTANT_APP_FOREGROUND_SERVICE")), // 68 INSTANT_APP_START_FOREGROUND
    (ALLOWED, 69, Some("android.permission.ANSWER_PHONE_CALLS")), // 69 ANSWER_PHONE_CALLS
    (ALLOWED, 70, None), // 70 RUN_ANY_IN_BACKGROUND
    (ALLOWED, 71, Some("android.permission.CHANGE_WIFI_STATE")), // 71 CHANGE_WIFI_STATE
    (ALLOWED, 72, Some("android.permission.REQUEST_DELETE_PACKAGES")), // 72 REQUEST_DELETE_PACKAGES
    (ALLOWED, 73, Some("android.permission.BIND_ACCESSIBILITY_SERVICE")), // 73 BIND_ACCESSIBILITY_SERVICE
    (ALLOWED, 74, Some("android.permission.ACCEPT_HANDOVER")), // 74 ACCEPT_HANDOVER
    (ERRORED, 75, Some("android.permission.MANAGE_IPSEC_TUNNELS")), // 75 MANAGE_IPSEC_TUNNELS
    (ALLOWED, 76, Some("android.permission.FOREGROUND_SERVICE")), // 76 START_FOREGROUND
    (ALLOWED, 77, Some("android.permission.BLUETOOTH_SCAN")), // 77 BLUETOOTH_SCAN
    (ALLOWED, 78, Some("android.permission.USE_BIOMETRIC")), // 78 USE_BIOMETRIC
    (ALLOWED, 79, Some("android.permission.ACTIVITY_RECOGNITION")), // 79 ACTIVITY_RECOGNITION
    (DEFAULT, 80, Some("android.permission.SMS_FINANCIAL_TRANSACTIONS")), // 80 SMS_FINANCIAL_TRANSACTIONS
    (ALLOWED, 81, Some("android.permission.READ_MEDIA_AUDIO")), // 81 READ_MEDIA_AUDIO
    (ERRORED, 82, None), // 82 WRITE_MEDIA_AUDIO
    (ALLOWED, 83, Some("android.permission.READ_MEDIA_VIDEO")), // 83 READ_MEDIA_VIDEO
    (ERRORED, 84, None), // 84 WRITE_MEDIA_VIDEO
    (ALLOWED, 85, Some("android.permission.READ_MEDIA_IMAGES")), // 85 READ_MEDIA_IMAGES
    (ERRORED, 86, None), // 86 WRITE_MEDIA_IMAGES
    (DEFAULT, 87, None), // 87 LEGACY_STORAGE
    (ALLOWED, 88, None), // 88 ACCESS_ACCESSIBILITY
    (ERRORED, 89, None), // 89 READ_DEVICE_IDENTIFIERS
    (ALLOWED, 90, Some("android.permission.ACCESS_MEDIA_LOCATION")), // 90 ACCESS_MEDIA_LOCATION
    (DEFAULT, 91, None), // 91 QUERY_ALL_PACKAGES
    (DEFAULT, 92, Some("android.permission.MANAGE_EXTERNAL_STORAGE")), // 92 MANAGE_EXTERNAL_STORAGE
    (DEFAULT, 93, Some("android.permission.INTERACT_ACROSS_PROFILES")), // 93 INTERACT_ACROSS_PROFILES
    (IGNORED, 94, None), // 94 ACTIVATE_PLATFORM_VPN
    (DEFAULT, 95, Some("android.permission.LOADER_USAGE_STATS")), // 95 LOADER_USAGE_STATS
    (IGNORED, 96, None), // 96 NONE
    (DEFAULT, 97, None), // 97 AUTO_REVOKE_PERMISSIONS_IF_UNUSED
    (ALLOWED, 98, None), // 98 AUTO_REVOKE_MANAGED_BY_INSTALLER
    (ERRORED, 99, None), // 99 NO_ISOLATED_STORAGE
    (ALLOWED, 100, None), // 100 PHONE_CALL_MICROPHONE
    (ALLOWED, 101, None), // 101 PHONE_CALL_CAMERA
    (ALLOWED, 102, None), // 102 RECORD_AUDIO_HOTWORD
    (DEFAULT, 103, Some("android.permission.MANAGE_ONGOING_CALLS")), // 103 MANAGE_ONGOING_CALLS
    (DEFAULT, 104, None), // 104 MANAGE_CREDENTIALS
    (DEFAULT, 105, Some("android.permission.USE_ICC_AUTH_WITH_DEVICE_IDENTIFIER")), // 105 USE_ICC_AUTH_WITH_DEVICE_IDENTIFIER
    (ALLOWED, 106, None), // 106 RECORD_AUDIO_OUTPUT
    (DEFAULT, 107, Some("android.permission.SCHEDULE_EXACT_ALARM")), // 107 SCHEDULE_EXACT_ALARM
    (ALLOWED, 1, None), // 108 FINE_LOCATION_SOURCE
    (ALLOWED, 0, None), // 109 COARSE_LOCATION_SOURCE
    (DEFAULT, 110, Some("android.permission.MANAGE_MEDIA")), // 110 MANAGE_MEDIA
    (ALLOWED, 111, Some("android.permission.BLUETOOTH_CONNECT")), // 111 BLUETOOTH_CONNECT
    (ALLOWED, 112, Some("android.permission.UWB_RANGING")), // 112 UWB_RANGING
    (ALLOWED, 79, None), // 113 ACTIVITY_RECOGNITION_SOURCE
    (ALLOWED, 114, Some("android.permission.BLUETOOTH_ADVERTISE")), // 114 BLUETOOTH_ADVERTISE
    (ALLOWED, 115, None), // 115 RECORD_INCOMING_PHONE_AUDIO
    (ALLOWED, 116, Some("android.permission.NEARBY_WIFI_DEVICES")), // 116 NEARBY_WIFI_DEVICES
    (ALLOWED, 117, None), // 117 ESTABLISH_VPN_SERVICE
    (ALLOWED, 118, None), // 118 ESTABLISH_VPN_MANAGER
    (DEFAULT, 119, None), // 119 ACCESS_RESTRICTED_SETTINGS
    (ALLOWED, 120, None), // 120 RECEIVE_AMBIENT_TRIGGER_AUDIO
    (ALLOWED, 121, None), // 121 RECEIVE_EXPLICIT_USER_INTERACTION_AUDIO
    (ALLOWED, 122, None), // 122 RUN_USER_INITIATED_JOBS
    (ALLOWED, 123, Some("android.permission.READ_MEDIA_VISUAL_USER_SELECTED")), // 123 READ_MEDIA_VISUAL_USER_SELECTED
    (DEFAULT, 124, None), // 124 SYSTEM_EXEMPT_FROM_SUSPENSION
    (DEFAULT, 125, None), // 125 SYSTEM_EXEMPT_FROM_DISMISSIBLE_NOTIFICATIONS
    (ALLOWED, 126, None), // 126 READ_WRITE_HEALTH_DATA
    (DEFAULT, 127, Some("android.permission.FOREGROUND_SERVICE_SPECIAL_USE")), // 127 FOREGROUND_SERVICE_SPECIAL_USE
    (DEFAULT, 128, None), // 128 SYSTEM_EXEMPT_FROM_POWER_RESTRICTIONS
    (DEFAULT, 129, None), // 129 SYSTEM_EXEMPT_FROM_HIBERNATION
    (DEFAULT, 130, None), // 130 SYSTEM_EXEMPT_FROM_ACTIVITY_BG_START_RESTRICTION
    (DEFAULT, 131, Some("android.permission.CAPTURE_CONSENTLESS_BUGREPORT_ON_USERDEBUG_BUILD")), // 131 CAPTURE_CONSENTLESS_BUGREPORT_ON_USERDEBUG_BUILD
    (IGNORED, 132, None), // 132 DEPRECATED_2
    (DEFAULT, 133, Some("android.permission.USE_FULL_SCREEN_INTENT")), // 133 USE_FULL_SCREEN_INTENT
    (ALLOWED, 134, None), // 134 CAMERA_SANDBOXED
    (ALLOWED, 135, None), // 135 RECORD_AUDIO_SANDBOXED
    (DEFAULT, 136, Some("android.permission.RECEIVE_SANDBOX_TRIGGER_AUDIO")), // 136 RECEIVE_SANDBOX_TRIGGER_AUDIO
    (IGNORED, 137, None), // 137 DEPRECATED_3
    (ALLOWED, 138, None), // 138 CREATE_ACCESSIBILITY_OVERLAY
    (DEFAULT, 139, Some("android.permission.MEDIA_ROUTING_CONTROL")), // 139 MEDIA_ROUTING_CONTROL
    (ALLOWED, 140, None), // 140 ENABLE_MOBILE_DATA_BY_USER
    (ALLOWED, 141, None), // 141 RESERVED_FOR_TESTING
    (ALLOWED, 142, None), // 142 RAPID_CLEAR_NOTIFICATIONS_BY_LISTENER
    (DEFAULT, 143, None), // 143 READ_SYSTEM_GRAMMATICAL_GENDER
    (IGNORED, 144, None), // 144 DEPRECATED_4
    (ALLOWED, 145, None), // 145 ARCHIVE_ICON_OVERLAY
    (ALLOWED, 146, None), // 146 UNARCHIVAL_CONFIRMATION
    (ALLOWED, 147, Some("android.permission.LOCATION_BYPASS")), // 147 EMERGENCY_LOCATION
    (IGNORED, 148, None), // 148 RECEIVE_SENSITIVE_NOTIFICATIONS
    (ALLOWED, 149, None), // 149 READ_HEART_RATE
    (ALLOWED, 150, None), // 150 READ_SKIN_TEMPERATURE
    (ALLOWED, 151, None), // 151 RANGING
    (ALLOWED, 152, None), // 152 READ_OXYGEN_SATURATION
    (DEFAULT, 153, None), // 153 WRITE_SYSTEM_PREFERENCES
    (FOREGROUND, 154, None), // 154 CONTROL_AUDIO
    (FOREGROUND, 155, None), // 155 CONTROL_AUDIO_PARTIAL
    (DEFAULT, 156, None), // 156 EYE_TRACKING_COARSE
    (DEFAULT, 157, None), // 157 EYE_TRACKING_FINE
    (DEFAULT, 158, None), // 158 FACE_TRACKING
    (DEFAULT, 159, None), // 159 HAND_TRACKING
    (DEFAULT, 160, None), // 160 HEAD_TRACKING
    (DEFAULT, 161, None), // 161 SCENE_UNDERSTANDING_COARSE
    (DEFAULT, 162, None), // 162 SCENE_UNDERSTANDING_FINE
];

/// AppOpsManager._NUM_OP.
pub const NUM_OP: usize = OPS.len();

/// The op's mode for a uid whose runtime-permission state `runtime_grant` reports (Some(granted) for a requested runtime permission, else None); None = `code` is no op
/// (AOSP's verifyIncomingOp throws IllegalArgumentException).
pub fn mode(code: i32, runtime_grant: impl Fn(&str) -> Option<bool>) -> Option<i32> {
    let (_, switch, _) = *OPS.get(usize::try_from(code).ok()?)?;
    let (default, _, permission) = OPS[switch];
    Some(match permission.and_then(runtime_grant) {
        Some(true) => ALLOWED,
        Some(false) => IGNORED,
        None => default,
    })
}

/// The one op whose AppOpInfo sets forceCollectNotes (setForceCollectNotes(true) at the pin): OP_RECEIVE_AMBIENT_TRIGGER_AUDIO.
const FORCE_COLLECT_NOTES: usize = 120;

/// AppOpsService.shouldCollectNotes for a uid-independent permission test `runtime_or_app_op`: forced ops yes; else the op's permission is runtime or an app-op one. None = no op.
pub fn should_collect_notes(code: i32, runtime_or_app_op: impl Fn(&str) -> bool) -> Option<bool> {
    let i = usize::try_from(code).ok()?;
    let (_, _, permission) = *OPS.get(i)?;
    Some(i == FORCE_COLLECT_NOTES || permission.is_some_and(runtime_or_app_op))
}

/// AppOpsManager.opToDefaultMode: what checkOperation answers for a package it cannot verify.
pub fn default_mode(code: i32) -> Option<i32> {
    OPS.get(usize::try_from(code).ok()?).map(|op| op.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Op codes from AppOpsManager (OP_COARSE_LOCATION 0, OP_GPS 2, OP_SYSTEM_ALERT_WINDOW 24, OP_RUN_IN_BACKGROUND 63, OP_COARSE_LOCATION_SOURCE 109 = AppOpEnums.APP_OP_COARSE_LOCATION_SOURCE), their Builder rows read at the pin.
    #[test]
    fn modes_follow_the_switch_and_the_runtime_grant() {
        let none = |_: &str| None;
        // RUN_IN_BACKGROUND: no permission, setDefaultMode(MODE_ALLOWED)
        assert_eq!(mode(63, none), Some(ALLOWED));
        // SYSTEM_ALERT_WINDOW: appop permission (not runtime) -> its default
        assert_eq!(mode(24, none), Some(DEFAULT));
        // GPS switches to COARSE_LOCATION, whose ACCESS_COARSE_LOCATION is runtime
        let coarse = |granted| {
            move |p: &str| (p == "android.permission.ACCESS_COARSE_LOCATION").then_some(granted)
        };
        assert_eq!(mode(2, coarse(false)), Some(IGNORED));
        assert_eq!(mode(2, coarse(true)), Some(ALLOWED));
        assert_eq!(mode(109, coarse(false)), Some(IGNORED));
        assert_eq!(mode(-1, none), None);
        assert_eq!(mode(163, none), None);
        assert_eq!(default_mode(24), Some(DEFAULT));
    }

    /// shouldCollectNotes: GPS has no permission; COARSE_LOCATION's is runtime; RECEIVE_AMBIENT_TRIGGER_AUDIO (120) is forced.
    #[test]
    fn notes_are_collected_for_runtime_permissions_and_the_forced_op() {
        let runtime = |p: &str| p == "android.permission.ACCESS_COARSE_LOCATION";
        assert_eq!(should_collect_notes(2, runtime), Some(false));
        assert_eq!(should_collect_notes(0, runtime), Some(true));
        assert_eq!(should_collect_notes(120, |_| false), Some(true));
        assert_eq!(should_collect_notes(163, runtime), None);
    }

    /// AppOpsManager._NUM_OP at the pin is 163; every switch names an op.
    #[test]
    fn table_is_complete() {
        assert_eq!(NUM_OP, 163);
        assert!(OPS.iter().all(|op| op.1 < NUM_OP));
    }
}
