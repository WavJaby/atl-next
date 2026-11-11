// SPDX-License-Identifier: MIT
//! The environment a sandboxed process starts with: [`APP_ENV`] names copied verbatim from the launcher's (atld's) own environment, then the spawn's explicit entries; nothing else of the host's.
//! Forwarding, not a switch: no code here reads a value. A variable the runtime itself decides (LD_LIBRARY_PATH, ANDROID_*, a harness's debug or break variable) is an explicit `Spawn::env` entry, never a name here.
//! Left out on purpose: HOME, USER, PATH (the sandbox shows no home; glibc's exec falls back to its default path), DBUS_SESSION_BUS_ADDRESS (no bus socket is shown; desktop access is portals), PIPEWIRE_* (the launcher gets the socket as `--pipewire-socket`),
//! GTK_IM_MODULE (a desktop IM module such as ibus would bypass the app's text-input v3 path to the IME), GDK_BACKEND (GTK picks Wayland from the shown socket).

use std::ffi::OsString;

/// Every name a sandboxed process may take from the launcher's environment, each with who reads it in the sandbox.
pub const APP_ENV: &[&str] = &[
    // libwayland-client wl_display_connect (GTK's Wayland backend, atl-platform-ui-linux): the socket atld shows (atl_platform_host::app_access::wayland_socket)
    "WAYLAND_DISPLAY",
    // libwayland-client: a relative WAYLAND_DISPLAY resolves under it, the same path inside (only the socket is shown); GLib g_get_user_runtime_dir
    "XDG_RUNTIME_DIR",
    // fontconfig (Pango under GTK): the config atld shows read-only at its own path (app_access::fontconfig_paths)
    "FONTCONFIG_FILE",
    // glibc setlocale(LC_ALL, "") in gtk_init + gettext: the desktop's locale; the locale data is under the shown /usr
    "LANG",
    "LANGUAGE",
    "LC_ALL",
    "LC_CTYPE",
    "LC_NUMERIC",
    "LC_TIME",
    "LC_COLLATE",
    "LC_MONETARY",
    "LC_MESSAGES",
    // GTK 4 Wayland backend: the desktop's cursor theme and size (themes under the shown /usr/share/icons)
    "XCURSOR_THEME",
    "XCURSOR_SIZE",
    // GTK: the desktop's theme choice
    "GTK_THEME",
    // GPU offload, the desktop's choice for this launch, passed through unread (switcheroo-control / .desktop PrefersNonDefaultGPU): Mesa's GL + Vulkan device pick
    "DRI_PRIME",
    // Mesa Vulkan device-select layer (GSK's Vulkan renderer)
    "MESA_VK_DEVICE_SELECT",
    // NVIDIA README primerenderoffload: EGL/Vulkan/GLX offload to the NVIDIA GPU, and which NVIDIA screen
    "__NV_PRIME_RENDER_OFFLOAD",
    "__NV_PRIME_RENDER_OFFLOAD_PROVIDER",
    // glvnd: the GLX vendor library (NVIDIA README sets it with the offload variable)
    "__GLX_VENDOR_LIBRARY_NAME",
    // glvnd libEGL: the EGL vendor ICDs, the app's GLES driver and GSK
    "__EGL_VENDOR_LIBRARY_FILENAMES",
    "__EGL_VENDOR_LIBRARY_DIRS",
    // NVIDIA Vulkan layer: orders the Vulkan devices (GSK's Vulkan renderer on the offload GPU)
    "__VK_LAYER_NV_optimus",
];

/// The [`APP_ENV`] names `lookup` has, in list order, verbatim.
pub fn forwarded(lookup: impl Fn(&str) -> Option<OsString>) -> Vec<(OsString, OsString)> {
    APP_ENV
        .iter()
        .filter_map(|k| lookup(k).map(|v| (OsString::from(*k), v)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Only listed names pass, a set non-listed one (HOME) does not, and an unset listed one adds nothing.
    #[test]
    fn forwards_listed_names_only() {
        let host = |k: &str| match k {
            "HOME" => Some(OsString::from("/home/u")),
            "WAYLAND_DISPLAY" => Some(OsString::from("wayland-1")),
            "DRI_PRIME" => Some(OsString::from("1")),
            _ => None,
        };
        assert_eq!(
            forwarded(host),
            vec![
                ("WAYLAND_DISPLAY".into(), "wayland-1".into()),
                ("DRI_PRIME".into(), "1".into()),
            ]
        );
    }

    /// Each name once: a duplicate would be a second reason recorded for one variable.
    #[test]
    fn names_are_unique() {
        let set: std::collections::BTreeSet<_> = APP_ENV.iter().collect();
        assert_eq!(set.len(), APP_ENV.len());
    }
}
