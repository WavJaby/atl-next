// SPDX-License-Identifier: MIT
//! Activity.requestPermissions' start (`REQUEST_PERMISSIONS`) while no permission controller is installed: answered as the user dismissing GrantPermissionsActivity's dialog (runtime-permissions plan P1).
//! Condition: `Packages::permission_controller()` is None. Once PermissionController is installed as the controller (plan S4) the start takes the normal path and this module is deleted.
//! Dismissal in AOSP (PermissionController GrantPermissionsActivity): back / outside tap -> onPermissionGrantResult(CANCELED) -> setResultAndFinish -> setResultIfNeeded(RESULT_OK): the original requested names and, for a
//! targetSdk >= M app (GrantPermissionsViewModel.shouldReturnPermissionState), checkPermission of each; grant state is not changed. Activity.dispatchRequestPermissionsResult reads the two extras into onRequestPermissionsResult.

use atl_parcelables::android::content::Intent;
use atl_parcelables::android::os::Bundle;
use atl_parcelables::codecs::android::os::Bundle::Value;

/// PackageManager.ACTION_REQUEST_PERMISSIONS, EXTRA_REQUEST_PERMISSIONS_NAMES / _RESULTS / _DEVICE_ID (PackageManager.java:5173-5222) and Intent.EXTRA_PACKAGE_NAME (Intent.java:2203).
pub const ACTION_REQUEST_PERMISSIONS: &str = "android.content.pm.action.REQUEST_PERMISSIONS";
pub const EXTRA_NAMES: &str = "android.content.pm.extra.REQUEST_PERMISSIONS_NAMES";
pub const EXTRA_RESULTS: &str = "android.content.pm.extra.REQUEST_PERMISSIONS_RESULTS";
pub const EXTRA_DEVICE_ID: &str = "android.content.pm.extra.REQUEST_PERMISSIONS_DEVICE_ID";
pub const EXTRA_PACKAGE_NAME: &str = "android.intent.extra.PACKAGE_NAME";
/// Activity.RESULT_OK: setResultAndFinish's code.
pub const RESULT_OK: i32 = -1;
/// PackageManager.PERMISSION_GRANTED / PERMISSION_DENIED.
const GRANTED: i32 = 0;
const DENIED: i32 = -1;
/// Build.VERSION_CODES.M: shouldReturnPermissionState's bound.
const M: i32 = 23;
/// Context.DEVICE_ID_DEFAULT: getIntExtra's default for the device id.
const DEVICE_ID_DEFAULT: i32 = 0;

/// The P1 condition at the start's decision point: a REQUEST_PERMISSIONS start and no permission controller to start it in.
pub fn answered_as_dismissed(intent: Option<&Intent>, controller: Option<&str>) -> bool {
    controller.is_none()
        && intent.is_some_and(|i| i.action.as_deref() == Some(ACTION_REQUEST_PERMISSIONS))
}

/// The value of `key` in `intent`'s extras; None when absent or when the extras hold a kind the Bundle codec does not read (refused and counted there).
fn extra(intent: &Intent, key: &str) -> Option<Value> {
    let entries = intent.extras.as_ref()?.entries().ok()?;
    entries.into_iter().find(|(k, _)| k == key).map(|(_, v)| v)
}

/// GrantPermissionsActivity.setResultIfNeeded's data for a dismissed request from `target` (the caller package) of `target_sdk`; `granted` is checkPermission against the caller.
/// Names: removeNullOrEmptyPermissions of the request's names (none -> empty). Device id: isDeviceAwarePermissionSupported holds (V+, not a TV / car / watch), so it is the request's, default DEVICE_ID_DEFAULT.
pub fn dismissed_result(
    intent: &Intent,
    target: &str,
    target_sdk: i32,
    granted: impl Fn(&str) -> bool,
) -> Intent {
    let requested: Vec<String> = match extra(intent, EXTRA_NAMES) {
        Some(Value::StringArray(Some(names))) => names
            .into_iter()
            .flatten()
            .filter(|n| !n.is_empty())
            .collect(),
        _ => Vec::new(),
    };
    let (names, results) = if target_sdk >= M {
        let results = requested
            .iter()
            .map(|n| if granted(n) { GRANTED } else { DENIED })
            .collect();
        (requested, results)
    } else {
        (Vec::new(), Vec::new())
    };
    let device = match extra(intent, EXTRA_DEVICE_ID) {
        Some(Value::Int(d)) => d,
        _ => DEVICE_ID_DEFAULT,
    };
    Intent {
        action: Some(ACTION_REQUEST_PERMISSIONS.to_owned()),
        extras: Some(Bundle::from_entries([
            (
                EXTRA_NAMES.to_owned(),
                Value::StringArray(Some(names.into_iter().map(Some).collect())),
            ),
            (EXTRA_RESULTS.to_owned(), Value::IntArray(Some(results))),
            (EXTRA_DEVICE_ID.to_owned(), Value::Int(device)),
            (
                EXTRA_PACKAGE_NAME.to_owned(),
                Value::String(Some(target.to_owned())),
            ),
        ])),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atl_parcelables::parts::Part;
    use binder::binder_impl::Parcel;

    const POST_NOTIFICATIONS: &str = "android.permission.POST_NOTIFICATIONS";
    const INTERNET: &str = "android.permission.INTERNET";

    /// The Bundle codec's array values (the result's extras), word by word as Java writes {"k": int[]{1, -1}} and {"s": String[]{"ab", null}}: BaseBundle.writeToParcelInner's length, 'BNDL', the
    /// ArrayMap in key-hash order ("k" 107 before "s" 115) with Parcel.writeValue's VAL_INTARRAY 18 / VAL_STRINGARRAY 14, writeIntArray / writeStringArray / writeString16, then mHasIntent 0.
    #[test]
    fn bundle_arrays_are_written_as_java_writes_them_and_read_back() {
        let unit = |c: char| u32::from(c) as i32;
        let body = [
            2,
            1,
            unit('k'), // "k": count 1, 'k' + NUL in one word
            18,
            2,
            1,
            -1,
            1,
            unit('s'),
            14,
            2,
            2,
            unit('a') | unit('b') << 16, // "ab": count 2, two units, NUL padded to a word
            0,
            -1, // null element
        ];
        let mut want = vec![body.len() as i32 * 4, 0x4C44_4E42];
        want.extend(body);
        want.push(0);
        let entries = vec![
            (
                "s".to_owned(),
                Value::StringArray(Some(vec![Some("ab".into()), None])),
            ),
            ("k".to_owned(), Value::IntArray(Some(vec![1, -1]))),
        ];
        let b = Bundle::from_entries(entries.clone());
        let mut p = Parcel::new();
        b.write(&mut p.borrowed()).expect("encode");
        let bytes = p.marshal().expect("marshal");
        let words: Vec<i32> = bytes
            .as_chunks::<4>()
            .0
            .iter()
            .map(|w| i32::from_ne_bytes(*w))
            .collect();
        assert_eq!(words, want);
        let mut back = b.entries().expect("entries");
        back.sort_by(|a, b| b.0.cmp(&a.0));
        assert_eq!(back, entries);
    }

    /// PackageManager.buildRequestPermissionsIntent: the action and the names extra (setPackage(null) without a controller).
    fn request(names: &[&str]) -> Intent {
        Intent {
            action: Some(ACTION_REQUEST_PERMISSIONS.into()),
            extras: Some(Bundle::from_entries([(
                EXTRA_NAMES.to_owned(),
                Value::StringArray(Some(names.iter().map(|n| Some((*n).to_owned())).collect())),
            )])),
            ..Default::default()
        }
    }

    fn extras(i: &Intent) -> Vec<(String, Value)> {
        let mut e = i
            .extras
            .as_ref()
            .expect("extras")
            .entries()
            .expect("entries");
        e.sort_by(|a, b| a.0.cmp(&b.0));
        e
    }

    /// The condition fires only for REQUEST_PERMISSIONS with no controller; a resolvable controller (S4) leaves the start to the normal path.
    #[test]
    fn fires_only_without_a_controller() {
        let r = request(&[POST_NOTIFICATIONS]);
        assert!(answered_as_dismissed(Some(&r), None));
        assert!(!answered_as_dismissed(
            Some(&r),
            Some("com.android.permissioncontroller")
        ));
        let other = Intent {
            action: Some("android.intent.action.VIEW".into()),
            ..Default::default()
        };
        assert!(!answered_as_dismissed(Some(&other), None));
        assert!(!answered_as_dismissed(None, None));
    }

    /// setResultIfNeeded for a targetSdk >= M app: the original names (null / empty dropped), checkPermission each (-1 denied, 0 granted), device id 0, the caller package; RESULT_OK is the caller's.
    #[test]
    fn dismissal_returns_names_and_current_state() {
        let r = request(&[POST_NOTIFICATIONS, "", INTERNET]);
        let got = dismissed_result(&r, "org.x", 36, |p| p == INTERNET);
        assert_eq!(got.action.as_deref(), Some(ACTION_REQUEST_PERMISSIONS));
        assert_eq!(
            extras(&got),
            [
                (EXTRA_DEVICE_ID.to_owned(), Value::Int(0)),
                (
                    EXTRA_NAMES.to_owned(),
                    Value::StringArray(Some(vec![
                        Some(POST_NOTIFICATIONS.into()),
                        Some(INTERNET.into())
                    ]))
                ),
                (EXTRA_RESULTS.to_owned(), Value::IntArray(Some(vec![-1, 0]))),
                (
                    EXTRA_PACKAGE_NAME.to_owned(),
                    Value::String(Some("org.x".into()))
                ),
            ]
        );
    }

    /// shouldReturnPermissionState is false below M, and a request without names returns none: both give empty arrays.
    #[test]
    fn legacy_or_nameless_requests_return_empty_arrays() {
        let empty = |i: Intent| {
            let e = extras(&i);
            // sorted keys: DEVICE_ID, NAMES, RESULTS, PACKAGE_NAME
            (e[1].1.clone(), e[2].1.clone())
        };
        let none = (
            Value::StringArray(Some(Vec::new())),
            Value::IntArray(Some(Vec::new())),
        );
        assert_eq!(
            empty(dismissed_result(
                &request(&[POST_NOTIFICATIONS]),
                "org.x",
                22,
                |_| false
            )),
            none
        );
        let nameless = Intent {
            action: Some(ACTION_REQUEST_PERMISSIONS.into()),
            ..Default::default()
        };
        assert_eq!(
            empty(dismissed_result(&nameless, "org.x", 36, |_| false)),
            none
        );
    }
}
