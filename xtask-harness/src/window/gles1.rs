// SPDX-License-Identifier: MIT
//! Stage `gles1` of `xtask test window` (gl-honesty H2b): the GLES1 fixture (xtask-harness/testdata/gles1, Main.java), a Java-only GLSurfaceView that never sets a client version, so its context is ES 1 (GLSurfaceView's default) and the GL driver serves it with embedded ANGLE (atl-egl-driver es1.rs). Run with routing keys empty (`Runtime::without_gl_selection`).
//! Sub-gates ([`checks`]): `probe` (the window acked, the fixture read back, the picture settled), `version` (the context is ANGLE's ES 1.1), `readback` (the app's glReadPixels at each probe is the expectation), `composed` (the picture at each probe), `present` (no failed present).
//! The expectation is Main.java's PROBES (worked out from its scene in the comment there), parsed from the fixture source: one copy.

use std::path::Path;
use std::sync::Mutex;

use atl_services_types::display_model::DisplayModel;

use super::capture;
use super::gles2::{composed_check, present_check, readback_check};
use super::{ACK_WAIT, Liveness, Sub, await_event, opening_size, wait_for_line};
use crate::gles2_fixture::java_probes;
use crate::wayland_proxy::{Event as ProxyEvent, Proxy};

pub const FIXTURE: &str = "xtask-harness/testdata/gles1";
const MAIN_JAVA: &str = "java/com/example/atlgles1/Main.java";
/// Main.java's log tag.
const TAG: &str = "AtlGles1";
/// The marker: the fixture's surface line (Main.java onSurfaceChanged), logged once the GL surface exists.
pub const MARKER: &str = "AtlGles1: surface ";
const READBACK_LINE: &str = "AtlGles1: readback";
const VERSION_LINE: &str = "AtlGles1: version ";

/// The probe: the window is acked, the fixture has read back, and its picture has settled (after the app's last present).
pub fn drive(proxy: Proxy, app_log: &Path, probe: &capture::Probe) -> Result<Liveness, String> {
    await_event(&proxy.events, ACK_WAIT, |e| {
        matches!(e, ProxyEvent::Acked { .. })
    })
    .ok_or("the window never acked a configure")?;
    wait_for_line(app_log, READBACK_LINE, ACK_WAIT)
        .then_some(())
        .ok_or("the fixture never read back")?;
    probe.record_settled();
    Ok(Liveness::default())
}

/// The sub-gates; `display` gives the window's opening size, the size the picture must have.
pub fn checks(
    root: &Path,
    app: &str,
    display: DisplayModel,
    liveness: &Result<Liveness, String>,
    seen: &Mutex<capture::Seen>,
) -> Vec<Sub> {
    let (width, height) = opening_size(display);
    let probes = std::fs::read_to_string(root.join(FIXTURE).join(MAIN_JAVA))
        .map(|java| java_probes(&java))
        .unwrap_or_default();
    vec![
        (
            "probe",
            match liveness {
                Ok(_) => Ok("acked, read back, captured".to_owned()),
                Err(e) => Err(e.clone()),
            },
        ),
        ("version", version_check(app)),
        ("readback", readback_check(app, TAG, probes.len())),
        (
            "composed",
            if probes.is_empty() {
                Err(format!("no PROBES rows in {FIXTURE}/{MAIN_JAVA}"))
            } else {
                composed_check(seen, width, height, &probes)
            },
        ),
        ("present", present_check(app)),
    ]
}

/// The fixture's context: ES 1.1 from ANGLE (GL_VERSION `OpenGL ES 1.1...` or `OpenGL ES-CM 1.1...`, naming ANGLE), not a host context.
fn version_check(app: &str) -> Result<String, String> {
    let version = app
        .lines()
        .find_map(|l| l.split_once(VERSION_LINE).map(|(_, v)| v.trim()))
        .ok_or_else(|| format!("no `{VERSION_LINE}` line"))?;
    if version.contains("1.1") && version.contains("ANGLE") {
        Ok(format!("`{version}`"))
    } else {
        Err(format!("`{version}`: not ANGLE's ES 1.1"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_must_be_angle_es11() {
        assert!(version_check("I AtlGles1: version OpenGL ES 1.1.0 (ANGLE 2.1 git hash: x)\n").is_ok());
        assert!(version_check("I AtlGles1: version OpenGL ES 3.1 Mesa 25.0.7\n").is_err());
        assert!(version_check("").is_err());
    }

    #[test]
    fn fixture_probes_parse() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let java = std::fs::read_to_string(root.join(FIXTURE).join(MAIN_JAVA)).expect("Main.java");
        // Main.java's PROBES comment: (8, 8) inside the red triangle, (56, 56) the clear colour
        assert_eq!(java_probes(&java), [(8, 8, 0xff00_00ff), (56, 56, 0x4080_c0ff)]);
    }
}
