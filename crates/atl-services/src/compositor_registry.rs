// SPDX-License-Identifier: MIT
//! The compositor registry (M3 slice 4, plan C3 "Amended 2026-11-04"): how an app process's in-process SurfaceFlinger reaches servicemanager without addService rights.
//! The app creates the two service objects (`android.ui.ISurfaceComposer` "SurfaceFlinger", `android.gui.ISurfaceComposer` "SurfaceFlingerAIDL") and hands them to this service in the system process, which registers them as
//! `SurfaceFlinger<suffix>` / `SurfaceFlingerAIDL<suffix>`; libgui's lookup appends the same suffix, read from [`SUFFIX_PROPERTY`] in the process's own property area (patches/aosp/frameworks/native, libgui).
//!
//! The suffix is `-<uid>-<process name>` ([`suffix`]): atld writes it into the property area it gives the process (it knows both from the start request), the registry checks that its caller's uid is the one named,
//! so a process can register only names carrying its own uid. A pid would need to be known before the process exists, and the app sees a pid of its own sandbox, not the one binderd reports.
//! Processes of one uid trust each other (one app); a restarted process re-registers the same names and replaces the dead entries.
//!
//! This module is the protocol (names, code, checks): plain Rust, shared by the app side (atl-composer) and the registry service (`registry_service`, feature `converter`).

/// The registry's name in servicemanager.
pub const SERVICE: &str = "atl.compositor_registry";
/// Its interface descriptor.
pub const DESCRIPTOR: &str = "atl.ICompositorRegistry";
/// `IBinder.FIRST_CALL_TRANSACTION`: the one transaction. Arguments: the suffix as a String16, then the two service binders, "SurfaceFlinger" first, then the app's UI agent (ui_agent.rs); reply: a status only.
pub const REGISTER: u32 = 1;
/// The transaction a UI-mode app process sends when its window's content size changes: the Task's number (the one its UI agent was named, ui_agent.rs TASK), then width and height (i32, physical pixels). The caller's uid must own that Task (tasks.rs). Reply: a status only.
pub const SET_BOUNDS: u32 = 2;
/// The transaction a UI-mode app process sends when the monitor its window is on changes (another monitor, or its scale or mode): the monitor as Android's display, a String16 in the `--display` form (display_model.rs `DisplayModel::to_arg`). The caller's uid must own a Task (an app with a window). Reply: a status only.
pub const SET_DISPLAY: u32 = 3;
/// The system property libgui appends to both service names (patches/aosp/frameworks/native libgui: `composerServiceName`).
pub const SUFFIX_PROPERTY: &str = "ro.atl.surfaceflinger.suffix";
/// The system property holding the display the in-app compositor answers with (`DisplayModel::to_arg`): the same model atld forwards to the services, delivered to the app the way its other environment is.
pub const DISPLAY_PROPERTY: &str = "ro.atl.display";
/// The system property holding the headless pointer script (atld `--headless-pointer`, parsed by atl-composer `input::parse_script`), delivered like [`DISPLAY_PROPERTY`]. Unset: no input thread; the window handoff still works.
pub const POINTER_PROPERTY: &str = "ro.atl.headless.pointer";
/// The system property selecting an app process's UI mode (atld `--ui`): `gtk` presents the compositor's scenes into a GTK window of the process, on its main thread (atl-app-launcher lib.rs `start_ui`, atl-composer). Unset: headless.
pub const UI_PROPERTY: &str = "ro.atl.ui";
/// AOSP's service names, libgui's defaults.
pub const SURFACE_FLINGER: &str = "SurfaceFlinger";
pub const SURFACE_FLINGER_AIDL: &str = "SurfaceFlingerAIDL";
/// servicemanager's limit on a service name (isValidServiceName: 127 characters).
const MAX_NAME: usize = 127;

/// The suffix of the process `process_name` of uid `uid`. A name's characters are those servicemanager accepts (`[A-Za-z0-9_.-/]`): a process name's `:` (`pkg:remote`) becomes `/`.
pub fn suffix(uid: u32, process_name: &str) -> String {
    format!("-{uid}-{}", process_name.replace(':', "/"))
}

/// The two service names for `suffix`: ("SurfaceFlinger<suffix>", "SurfaceFlingerAIDL<suffix>").
pub fn service_names(suffix: &str) -> (String, String) {
    (
        format!("{SURFACE_FLINGER}{suffix}"),
        format!("{SURFACE_FLINGER_AIDL}{suffix}"),
    )
}

/// Whether `caller_uid` may register under `suffix`: it must be `-<caller_uid>-` followed by a process name, and both service names must be names servicemanager accepts.
pub fn check(caller_uid: u32, suffix: &str) -> Result<(), String> {
    let prefix = format!("-{caller_uid}-");
    let name = suffix
        .strip_prefix(&prefix)
        .ok_or_else(|| format!("suffix {suffix:?} is not under uid {caller_uid}"))?;
    if name.is_empty() {
        return Err(format!("suffix {suffix:?} names no process"));
    }
    let (sf, aidl) = service_names(suffix);
    for n in [&sf, &aidl] {
        if n.len() > MAX_NAME
            || !n
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '/'))
        {
            return Err(format!("{n:?} is not a valid service name"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A process name's colon becomes a slash; the rest is kept. The expected text is the rule above spelled out.
    #[test]
    fn suffix_replaces_the_colon() {
        assert_eq!(suffix(10_123, "com.example.app"), "-10123-com.example.app");
        assert_eq!(
            suffix(10_123, "com.example.app:remote"),
            "-10123-com.example.app/remote"
        );
        assert_eq!(
            service_names("-1-p").1,
            "SurfaceFlingerAIDL-1-p",
            "AOSP's name, then the suffix"
        );
    }

    /// A process registers only under its own uid; an empty process name, a name past servicemanager's limit and characters it refuses are rejected.
    #[test]
    fn check_binds_the_suffix_to_the_caller() {
        assert!(check(10_123, &suffix(10_123, "com.example.app:remote")).is_ok());
        assert!(check(10_124, &suffix(10_123, "com.example.app")).is_err());
        // a uid that is a prefix of another must not match
        assert!(check(10_12, &suffix(10_123, "p")).is_err());
        assert!(check(10_123, "-10123-").is_err());
        assert!(check(10_123, "-10123-p q").is_err());
        let long = "p".repeat(MAX_NAME);
        assert!(check(10_123, &suffix(10_123, &long)).is_err());
    }
}
