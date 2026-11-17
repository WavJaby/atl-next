// SPDX-License-Identifier: MIT
//! What an app process reads from the desktop, so the broker can show exactly that in the app sandbox.
//! - the compositor's socket, for the UI side (atl-platform-ui-linux: GTK 4 on Wayland): `WAYLAND_DISPLAY` absolute, else under `XDG_RUNTIME_DIR`, `wayland-0` when unset (libwayland wl_display_connect); the broker forwards both variables (atl-broker app_env::APP_ENV), so the same path resolves inside.
//! - the PipeWire server's socket, for audio (every app, UI or not): atld names it to the launcher (`--pipewire-socket`), which connects to that path and reads no environment for it.
//! - fontconfig's config: `FONTCONFIG_FILE` when set (absolute; else the distro's under /etc, which the app root already shows) and the dirs of every face it lists. No cache dir: read-only it saves nothing, and fontconfig rescans the dirs without it.
//!
//! The `_of` forms take the variables as values: `xtask test sandbox` proves the grants on paths it makes.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::path::PathBuf;

use atl_platform_host_api::{FontError, FontQuery, FontSource};

use crate::FcFonts;

/// The compositor socket the app's GTK connects to, from this process's environment; `None` when it names none or it does not exist.
pub fn wayland_socket() -> Option<PathBuf> {
    socket_of(
        std::env::var_os("WAYLAND_DISPLAY"),
        std::env::var_os("XDG_RUNTIME_DIR"),
    )
}

/// [`wayland_socket`] for the values `display` (WAYLAND_DISPLAY) and `runtime` (XDG_RUNTIME_DIR).
pub fn socket_of(display: Option<OsString>, runtime: Option<OsString>) -> Option<PathBuf> {
    named_socket_of(display, "wayland-0", runtime)
}

/// The PipeWire server's socket as libpipewire's default connect resolves it here: `PIPEWIRE_REMOTE` (absolute, or a name under the runtime dir), `pipewire-0` when unset; the runtime dir is `PIPEWIRE_RUNTIME_DIR`, else `XDG_RUNTIME_DIR` (libpipewire's order). `None` when it does not exist.
pub fn pipewire_socket() -> Option<PathBuf> {
    let runtime =
        std::env::var_os("PIPEWIRE_RUNTIME_DIR").or_else(|| std::env::var_os("XDG_RUNTIME_DIR"));
    pipewire_socket_of(std::env::var_os("PIPEWIRE_REMOTE"), runtime)
}

/// [`pipewire_socket`] for the values `remote` (PIPEWIRE_REMOTE) and `runtime` (the runtime dir, as above).
pub fn pipewire_socket_of(remote: Option<OsString>, runtime: Option<OsString>) -> Option<PathBuf> {
    named_socket_of(remote, "pipewire-0", runtime)
}

/// The PulseAudio-protocol server's socket as libpulse's default connect finds a local one: the first `unix:` entry of `PULSE_SERVER` (a space-separated list; a bare absolute path is a unix one too),
/// else `pulse/native` under `XDG_RUNTIME_DIR` (pa_runtime_path). Network entries (`tcp:`, a host name) are no socket to show: skipped, as Flatpak's `--socket=pulseaudio` skips them.
/// client.conf's `default-server` is not read (a per-user file, not the environment). `None` when it does not exist. For the web process's `share-sound` (atl-broker webspawn).
pub fn pulse_socket() -> Option<PathBuf> {
    pulse_socket_of(
        std::env::var_os("PULSE_SERVER"),
        std::env::var_os("XDG_RUNTIME_DIR"),
    )
}

/// Where [`pulse_socket`] looks: `PULSE_SERVER` when set, else the runtime dir's default.
pub fn pulse_socket_source() -> &'static str {
    if std::env::var_os("PULSE_SERVER").is_some() {
        "PULSE_SERVER"
    } else {
        "XDG_RUNTIME_DIR default"
    }
}

/// [`pulse_socket`] for the values `server` (PULSE_SERVER) and `runtime` (XDG_RUNTIME_DIR).
pub fn pulse_socket_of(server: Option<OsString>, runtime: Option<OsString>) -> Option<PathBuf> {
    let p = match server {
        // a PULSE_SERVER naming only network servers: libpulse never tries the local default either
        Some(s) => s.into_string().ok()?.split_whitespace().find_map(|e| {
            let path = e.strip_prefix("unix:").unwrap_or(e);
            path.starts_with('/').then(|| PathBuf::from(path))
        })?,
        None => PathBuf::from(runtime?).join("pulse/native"),
    };
    p.exists().then_some(p)
}

/// The socket `name` (absolute, else under `runtime`; `default` when unset) if it exists.
fn named_socket_of(
    name: Option<OsString>,
    default: &str,
    runtime: Option<OsString>,
) -> Option<PathBuf> {
    let p = PathBuf::from(name.unwrap_or_else(|| default.into()));
    let p = if p.is_absolute() {
        p
    } else {
        PathBuf::from(runtime?).join(p)
    };
    p.exists().then_some(p)
}

/// The config file for the value `file` of FONTCONFIG_FILE: itself when absolute and present; `None` = fontconfig's default under /etc (already shown).
pub fn config_file_of(file: Option<OsString>) -> Option<PathBuf> {
    let f = PathBuf::from(file?);
    (f.is_absolute() && f.exists()).then_some(f)
}

/// [`config_file_of`] this process's FONTCONFIG_FILE, and the dir of every face fontconfig lists, sorted, each once.
pub fn fontconfig_paths() -> Result<Vec<PathBuf>, FontError> {
    let mut v: BTreeSet<PathBuf> = config_file_of(std::env::var_os("FONTCONFIG_FILE"))
        .into_iter()
        .collect();
    for face in FcFonts::new()?.list(&FontQuery::default())? {
        if let Some(d) = face.path.parent() {
            v.insert(d.to_path_buf());
        }
    }
    Ok(v.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// libpulse's order on a made runtime dir: PULSE_SERVER's first unix entry wins over the default, network entries are skipped, a network-only value finds none, unset = `<runtime>/pulse/native`.
    #[test]
    fn pulse_socket_follows_libpulse_order() {
        let dir = std::env::temp_dir().join(format!("atl-pulse-of-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("pulse")).unwrap();
        let default = dir.join("pulse/native");
        let other = dir.join("other");
        std::fs::write(&default, "").unwrap();
        std::fs::write(&other, "").unwrap();
        let of = |s: Option<String>| pulse_socket_of(s.map(Into::into), Some(dir.clone().into()));
        assert_eq!(of(None), Some(default.clone()));
        let o = other.display();
        assert_eq!(of(Some(format!("unix:{o}"))), Some(other.clone()));
        assert_eq!(of(Some(format!("tcp:host:4713 {o}"))), Some(other.clone()));
        assert_eq!(of(Some("tcp:host:4713".into())), None);
        assert_eq!(of(Some(format!("unix:{}/missing", dir.display()))), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
