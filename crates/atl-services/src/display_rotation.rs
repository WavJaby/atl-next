// SPDX-License-Identifier: MIT
//! The display's turn for a fixed-orientation request (plan display-model-owner S4): AOSP's DisplayRotation.rotationForOrientation (:1051-1243) as a pure function of the panel's natural
//! size, the requested orientation and the last rotation. No state here: tasks.rs keeps the rotation and asks under its own lock.
//! Modelled: the default display, a sensor-less desktop (the sensor reads the last rotation, :1077), config_supportAutoRotation true and the user rotation locked (the defaults).
//! Not modelled: the lid, dock, HDMI, demo and VR overrides (no such state on the desktop), and the below-activity walk (BEHIND is UNSPECIFIED).

use atl_services_types::display_model::Declaration;
use atl_services_types::packages::{
    SCREEN_ORIENTATION_BEHIND, SCREEN_ORIENTATION_FULL_SENSOR, SCREEN_ORIENTATION_FULL_USER,
    SCREEN_ORIENTATION_LANDSCAPE, SCREEN_ORIENTATION_LOCKED, SCREEN_ORIENTATION_PORTRAIT,
    SCREEN_ORIENTATION_REVERSE_LANDSCAPE, SCREEN_ORIENTATION_REVERSE_PORTRAIT,
    SCREEN_ORIENTATION_SENSOR, SCREEN_ORIENTATION_SENSOR_LANDSCAPE,
    SCREEN_ORIENTATION_SENSOR_PORTRAIT, SCREEN_ORIENTATION_UNSPECIFIED, SCREEN_ORIENTATION_USER,
    SCREEN_ORIENTATION_USER_LANDSCAPE, SCREEN_ORIENTATION_USER_PORTRAIT,
};

/// WindowManager.LARGE_SCREEN_SMALLEST_SCREEN_WIDTH_DP (WindowManager.java:944): a declared display whose smallest side is this many dp or more ignores fixed orientation requests
/// (AOSP universal_resizable_by_default; DisplayContent.getIgnoreOrientationRequest:6647-6654).
pub const LARGE_SCREEN_SW_DP: i32 = 600;

/// Surface.ROTATION_0..ROTATION_270: clockwise quarter turns of the drawn image from the panel's natural orientation.
const ROTATION_0: i32 = 0;
const ROTATION_90: i32 = 1;
const ROTATION_180: i32 = 2;
const ROTATION_270: i32 = 3;

/// DisplayRotation.mUserRotation under USER_ROTATION_LOCKED: the defaults (def_accelerometer_rotation false, SettingsProvider defaults.xml:33) leave it at 0.
const USER_ROTATION: i32 = ROTATION_0;

/// Whether `declared` obeys fixed orientation requests: declared, and its smallest side under [`LARGE_SCREEN_SW_DP`] dp. An undeclared display is the monitor's (maintainer option A).
pub fn obeys_requests(declared: Option<Declaration>) -> bool {
    declared.is_some_and(|d| d.width_dp.min(d.height_dp) < LARGE_SCREEN_SW_DP)
}

/// The rotation a request of `screen_orientation` turns a display of natural size `natural` (width, height) to, from `last` (its rotation now). The four rotations are mapped by the
/// natural orientation (DisplayRotation.configure:383-405): a landscape panel has portrait 3, seascape 2, upside-down 1; a portrait panel has landscape 1, seascape 3, upside-down 2.
pub fn rotation_for_orientation(natural: (i32, i32), screen_orientation: i32, last: i32) -> i32 {
    let (landscape, seascape, portrait, upside_down) = if natural.0 > natural.1 {
        (ROTATION_0, ROTATION_180, ROTATION_270, ROTATION_90)
    } else {
        (ROTATION_90, ROTATION_270, ROTATION_0, ROTATION_180)
    };
    let is_landscape = |r: i32| r == landscape || r == seascape;
    let is_portrait = |r: i32| r == portrait || r == upside_down;
    // the preferred rotation (:1133-1183): the lock, the sensor or the user rotation; -1 when none
    let preferred = match screen_orientation {
        SCREEN_ORIENTATION_LOCKED => last,
        SCREEN_ORIENTATION_SENSOR
        | SCREEN_ORIENTATION_FULL_SENSOR
        | SCREEN_ORIENTATION_SENSOR_LANDSCAPE
        | SCREEN_ORIENTATION_SENSOR_PORTRAIT => last,
        SCREEN_ORIENTATION_UNSPECIFIED
        | SCREEN_ORIENTATION_BEHIND
        | SCREEN_ORIENTATION_USER
        | SCREEN_ORIENTATION_FULL_USER
        | SCREEN_ORIENTATION_USER_LANDSCAPE
        | SCREEN_ORIENTATION_USER_PORTRAIT => USER_ROTATION,
        // the fixed requests and NOSENSOR: no preference (:1183)
        _ => -1,
    };
    match screen_orientation {
        SCREEN_ORIENTATION_PORTRAIT => {
            if is_portrait(preferred) {
                preferred
            } else {
                portrait
            }
        }
        SCREEN_ORIENTATION_LANDSCAPE => {
            if is_landscape(preferred) {
                preferred
            } else {
                landscape
            }
        }
        SCREEN_ORIENTATION_REVERSE_PORTRAIT => {
            if is_portrait(preferred) {
                preferred
            } else {
                upside_down
            }
        }
        SCREEN_ORIENTATION_REVERSE_LANDSCAPE => {
            if is_landscape(preferred) {
                preferred
            } else {
                seascape
            }
        }
        SCREEN_ORIENTATION_SENSOR_LANDSCAPE | SCREEN_ORIENTATION_USER_LANDSCAPE => {
            if is_landscape(preferred) {
                preferred
            } else if is_landscape(last) {
                last
            } else {
                landscape
            }
        }
        SCREEN_ORIENTATION_SENSOR_PORTRAIT | SCREEN_ORIENTATION_USER_PORTRAIT => {
            if is_portrait(preferred) {
                preferred
            } else if is_portrait(last) {
                last
            } else {
                portrait
            }
        }
        _ => {
            if preferred >= 0 {
                preferred
            } else {
                ROTATION_0
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atl_services_types::packages::SCREEN_ORIENTATION_NOSENSOR;

    /// The phone of the gates, 411x914 dp at mdpi: portrait natural. The landscape panel is its quarter turn.
    const PORTRAIT_NATURAL: (i32, i32) = (411, 914);
    const LANDSCAPE_NATURAL: (i32, i32) = (914, 411);

    /// The expected values are AOSP's switch worked out by hand for each natural orientation (configure:383-405), not computed here.
    #[test]
    fn fixed_requests_map_through_the_natural_orientation() {
        assert_eq!(
            rotation_for_orientation(PORTRAIT_NATURAL, SCREEN_ORIENTATION_LANDSCAPE, 0),
            1
        );
        assert_eq!(
            rotation_for_orientation(PORTRAIT_NATURAL, SCREEN_ORIENTATION_PORTRAIT, 1),
            0
        );
        assert_eq!(
            rotation_for_orientation(PORTRAIT_NATURAL, SCREEN_ORIENTATION_REVERSE_LANDSCAPE, 1),
            3
        );
        assert_eq!(
            rotation_for_orientation(PORTRAIT_NATURAL, SCREEN_ORIENTATION_REVERSE_PORTRAIT, 3),
            2
        );
        assert_eq!(
            rotation_for_orientation(LANDSCAPE_NATURAL, SCREEN_ORIENTATION_LANDSCAPE, 1),
            0
        );
        assert_eq!(
            rotation_for_orientation(LANDSCAPE_NATURAL, SCREEN_ORIENTATION_PORTRAIT, 0),
            3
        );
        assert_eq!(
            rotation_for_orientation(LANDSCAPE_NATURAL, SCREEN_ORIENTATION_REVERSE_LANDSCAPE, 0),
            2
        );
        assert_eq!(
            rotation_for_orientation(LANDSCAPE_NATURAL, SCREEN_ORIENTATION_REVERSE_PORTRAIT, 0),
            1
        );
    }

    /// A sensor-landscape request keeps the last rotation when it is a landscape of the panel, else takes the natural landscape (AOSP :1186-1242 for SENSOR_LANDSCAPE / USER_LANDSCAPE).
    #[test]
    fn sensor_landscape_keeps_a_landscape_last_rotation() {
        assert_eq!(
            rotation_for_orientation(PORTRAIT_NATURAL, SCREEN_ORIENTATION_SENSOR_LANDSCAPE, 3),
            3
        );
        assert_eq!(
            rotation_for_orientation(PORTRAIT_NATURAL, SCREEN_ORIENTATION_SENSOR_LANDSCAPE, 0),
            1
        );
        assert_eq!(
            rotation_for_orientation(PORTRAIT_NATURAL, SCREEN_ORIENTATION_USER_LANDSCAPE, 1),
            1
        );
    }

    /// Under the locked user rotation the requests that name no fixed orientation take the user's rotation, 0; NOSENSOR and BEHIND (as UNSPECIFIED) do too (AOSP :1173-1179, :1218-1220).
    #[test]
    fn unspecified_takes_the_user_rotation() {
        assert_eq!(
            rotation_for_orientation(PORTRAIT_NATURAL, SCREEN_ORIENTATION_UNSPECIFIED, 1),
            0
        );
        assert_eq!(
            rotation_for_orientation(PORTRAIT_NATURAL, SCREEN_ORIENTATION_NOSENSOR, 1),
            0
        );
        assert_eq!(
            rotation_for_orientation(PORTRAIT_NATURAL, SCREEN_ORIENTATION_BEHIND, 3),
            0
        );
    }

    /// LOCKED keeps the last rotation (:1133-1134); SENSOR and FULL_SENSOR read the sensor, which is absent here, so they keep it too (:1077, :1144-1164).
    #[test]
    fn locked_and_sensor_keep_the_last_rotation() {
        assert_eq!(
            rotation_for_orientation(PORTRAIT_NATURAL, SCREEN_ORIENTATION_LOCKED, 1),
            1
        );
        assert_eq!(
            rotation_for_orientation(PORTRAIT_NATURAL, SCREEN_ORIENTATION_SENSOR, 2),
            2
        );
        assert_eq!(
            rotation_for_orientation(LANDSCAPE_NATURAL, SCREEN_ORIENTATION_FULL_SENSOR, 3),
            3
        );
    }

    /// A declared display of smallest side under 600 dp obeys requests (599 yes, 600 no); an undeclared display never does here.
    #[test]
    fn a_declared_phone_display_obeys_requests() {
        let at = |width_dp, height_dp| {
            Some(Declaration {
                width_dp,
                height_dp,
            })
        };
        assert!(obeys_requests(at(411, 914)));
        assert!(obeys_requests(at(599, 914)));
        assert!(!obeys_requests(at(600, 914)));
        assert!(!obeys_requests(at(914, 600)));
        assert!(!obeys_requests(None));
    }
}
