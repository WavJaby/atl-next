// SPDX-License-Identifier: MIT
//! Initial bounds of a freeform task on the desktop display: AOSP's DesktopModeBoundsCalculator.calculateInitialBounds (`:133-225`) and its centerInScreen (`:324-334`), for our display. Pure: no binder, no I/O.
//! Owner decision 2026-11-08 (aspect option A): the display is modelled as AOSP's "ignores orientation requests" mode (DisplayContent.getIgnoreOrientationRequest, DisplayContent.java:6647-6654). The desired aspect is then the display's own (DesktopAppCompatAspectRatioPolicy.getDesiredAspectRatio branch (c)), not the freeform default 1.01 (branch (a), off in that mode). Unresizeable apps are maximised within the ideal size, keeping their aspect.
//! Not modelled: caption height (`:85-87`; maximise runs with 0), user min-aspect and compat-change overrides (DesktopAppCompatAspectRatioPolicy.getMinAspectRatio), fullscreen and windowLayout overrides, ActivityOptions launch bounds, and per-app universal resizability (ActivityRecord.isUniversalResizeable, ActivityRecord.java:3113-3126).
//! Position: the centred rect of AOSP's rule. xdg-shell gives a client no toplevel position, so the compositor places the window and this origin only records the Task's bounds.

use crate::packages::{
    ActivityDecl, SCREEN_ORIENTATION_LANDSCAPE, SCREEN_ORIENTATION_PORTRAIT,
    SCREEN_ORIENTATION_REVERSE_LANDSCAPE, SCREEN_ORIENTATION_REVERSE_PORTRAIT,
    SCREEN_ORIENTATION_SENSOR_LANDSCAPE, SCREEN_ORIENTATION_SENSOR_PORTRAIT,
    SCREEN_ORIENTATION_USER_LANDSCAPE, SCREEN_ORIENTATION_USER_PORTRAIT,
};

/// `android.graphics.Rect`: left/top inclusive, right/bottom exclusive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

impl Rect {
    pub fn width(&self) -> i32 {
        self.right - self.left
    }

    pub fn height(&self) -> i32 {
        self.bottom - self.top
    }
}

/// The activity's requested orientation, reduced to what the rule reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivityOrientation {
    /// `ActivityInfo.isFixedOrientationPortrait`.
    Portrait,
    /// `ActivityInfo.isFixedOrientationLandscape`.
    Landscape,
    /// Any other value, unspecified included: the activity takes the display's orientation (getActivityConfigurationOrientation, `:238-255`).
    Unspecified,
}

impl ActivityOrientation {
    /// From `ActivityInfo.screenOrientation`: the values AOSP's `isFixedOrientationLandscape` / `isFixedOrientationPortrait` accept (ActivityInfo.java:1927-1951). `-1`, sensor, nosensor and locked are not fixed to either side.
    pub fn from_screen_orientation(screen_orientation: i32) -> Self {
        match screen_orientation {
            SCREEN_ORIENTATION_LANDSCAPE
            | SCREEN_ORIENTATION_SENSOR_LANDSCAPE
            | SCREEN_ORIENTATION_REVERSE_LANDSCAPE
            | SCREEN_ORIENTATION_USER_LANDSCAPE => Self::Landscape,
            SCREEN_ORIENTATION_PORTRAIT
            | SCREEN_ORIENTATION_SENSOR_PORTRAIT
            | SCREEN_ORIENTATION_REVERSE_PORTRAIT
            | SCREEN_ORIENTATION_USER_PORTRAIT => Self::Portrait,
            _ => Self::Unspecified,
        }
    }

    /// getActivityConfigurationOrientation: a fixed orientation wins, otherwise the display's.
    fn resolve(self, display: Orientation) -> Orientation {
        match self {
            Self::Portrait => Orientation::Portrait,
            Self::Landscape => Orientation::Landscape,
            Self::Unspecified => display,
        }
    }
}

/// What the calculator reads of an activity. Aspect limits are None when the manifest does not declare them.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ActivityInput {
    pub orientation: ActivityOrientation,
    /// `ActivityInfo.isResizeableMode` of the activity's resize mode (Task.isResizeable).
    pub resizeable: bool,
    /// `android:minAspectRatio`.
    pub min_aspect: Option<f32>,
    /// `android:maxAspectRatio`.
    pub max_aspect: Option<f32>,
}

impl ActivityInput {
    /// The activity a manifest declares: its screen orientation, resize mode and aspect limits.
    pub fn from_decl(decl: &ActivityDecl) -> Self {
        Self {
            orientation: ActivityOrientation::from_screen_orientation(decl.screen_orientation),
            resizeable: decl.resizeable,
            min_aspect: decl.min_aspect_ratio,
            max_aspect: decl.max_aspect_ratio,
        }
    }

    /// A process with no launch activity: AOSP's default activity (unspecified orientation, resizeable, no aspect limits).
    pub fn no_activity() -> Self {
        Self {
            orientation: ActivityOrientation::Unspecified,
            resizeable: true,
            min_aspect: None,
            max_aspect: None,
        }
    }
}

/// Ideal size as a share of the display: DESKTOP_MODE_INITIAL_BOUNDS_SCALE = 75 / 100f (DesktopModeBoundsCalculator.java:50-51), AOSP's default for persist.wm.debug.desktop_mode_initial_bounds_scale.
const IDEAL_SCALE: f32 = 0.75;
/// WINDOW_HEIGHT_PROPORTION (`:60`): the top offset is this share of the free height, so a centred window sits a little above middle.
const HEIGHT_PROPORTION: f32 = 0.375;
/// DESKTOP_MODE_LANDSCAPE_APP_PADDING (`:52-53`, default 25): margin on each side of a landscape app on a portrait display.
const LANDSCAPE_APP_PADDING: i32 = 25;
/// Aspect limits below this are ignored (calculateAspectRatio's `>= 1` tests, DesktopAppCompatAspectRatioPolicy.java:77-80). AOSP's "not declared" 0 falls below it too.
const MIN_ASPECT_LIMIT: f32 = 1.0;
/// Round-half-up for the `(int)(x + 0.5f)` of the resizeable mismatch sizes (`:175`, `:198`).
const ROUND_OFFSET: f32 = 0.5;

/// The display's orientation (`:154-155`) or an activity's resolved one. Two values only: the rule has no third case.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Orientation {
    Portrait,
    Landscape,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Size {
    width: i32,
    height: i32,
}

/// Initial bounds of a freeform task: AOSP's calculateInitialBounds for `activity`, placed by centerInScreen.
/// `display` is the display area: the ideal size's base and the centring rect. `stable` is the area the window fits (AOSP stable bounds, the display minus system bars). Ours has no insets, so callers pass the same rect twice.
/// An empty rect returns `display` unchanged. AOSP's zero-aspect fallbacks (`:168-170`, `:192-205`) are not ported: an empty display has no area to fit.
pub fn initial_bounds(display: Rect, stable: Rect, activity: &ActivityInput) -> Rect {
    if [display, stable]
        .iter()
        .any(|r| r.width() <= 0 || r.height() <= 0)
    {
        return display;
    }
    let ideal = Size {
        width: (display.width() as f32 * IDEAL_SCALE) as i32,
        height: (display.height() as f32 * IDEAL_SCALE) as i32,
    };
    let display_orientation = orientation_of(stable);
    let orientation = activity.orientation.resolve(display_orientation);
    let mismatch = orientation != display_orientation;
    let aspect = aspect_ratio(display, activity.min_aspect, activity.max_aspect);
    let (stable_width, stable_height) = (stable.width() as f32, stable.height() as f32);
    let landscape_app_width = display.width() - LANDSCAPE_APP_PADDING * 2;
    let size = match (display_orientation, activity.resizeable, mismatch) {
        // Landscape display, resizeable: a portrait app keeps the stable height's width at the aspect, the ideal height.
        (Orientation::Landscape, true, true) => Size {
            width: (stable_height / aspect + ROUND_OFFSET) as i32,
            height: ideal.height,
        },
        (Orientation::Landscape, true, false) => ideal,
        (Orientation::Landscape, false, _) => maximize(orientation, ideal, aspect),
        // Portrait display, resizeable: a landscape app keeps the padded width and the stable width's height at the aspect.
        (Orientation::Portrait, true, true) => Size {
            width: landscape_app_width,
            height: (stable_width / aspect + ROUND_OFFSET) as i32,
        },
        (Orientation::Portrait, true, false) => ideal,
        (Orientation::Portrait, false, true) => maximize(
            orientation,
            Size {
                width: landscape_app_width,
                height: ideal.height,
            },
            aspect,
        ),
        (Orientation::Portrait, false, false) => maximize(orientation, ideal, aspect),
    };
    centre(size, display)
}

/// The display's orientation: portrait when the stable height is at least its width (`:154-155`).
fn orientation_of(stable: Rect) -> Orientation {
    if stable.height() >= stable.width() {
        Orientation::Portrait
    } else {
        Orientation::Landscape
    }
}

/// AOSP computeAspectRatio (AppCompatUtils.java:75-82): long side over short side, in f32.
fn display_aspect(display: Rect) -> f32 {
    let (width, height) = (display.width(), display.height());
    width.max(height) as f32 / width.min(height) as f32
}

/// The display's aspect clamped to the declared limits: a max below it wins, else a min above it (calculateAspectRatio, DesktopAppCompatAspectRatioPolicy.java:72-83).
/// Under option A the desired aspect is the display's (getDesiredAspectRatio branch (c)): the 1.01 freeform default is off in the ignore-orientation mode.
fn aspect_ratio(display: Rect, min: Option<f32>, max: Option<f32>) -> f32 {
    let desired = display_aspect(display);
    let limit = |v: Option<f32>| v.filter(|v| *v >= MIN_ASPECT_LIMIT);
    match (limit(max), limit(min)) {
        (Some(max), _) if desired > max => max,
        (_, Some(min)) if desired < min => min,
        _ => desired,
    }
}

/// AOSP maximizeSizeGivenAspectRatio (`:263-307`) with caption height 0: the largest size of `aspect` within `target`, its long side along `orientation`.
fn maximize(orientation: Orientation, target: Size, aspect: f32) -> Size {
    match orientation {
        Orientation::Portrait => {
            let width = (target.height as f32 / aspect) as i32;
            if width <= target.width {
                Size {
                    width,
                    height: target.height,
                }
            } else {
                Size {
                    width: target.width,
                    height: (target.width as f32 * aspect) as i32,
                }
            }
        }
        Orientation::Landscape => {
            let width = (target.height as f32 * aspect) as i32;
            if width <= target.width {
                Size {
                    width,
                    height: target.height,
                }
            } else {
                Size {
                    width: target.width,
                    height: (target.width as f32 / aspect) as i32,
                }
            }
        }
    }
}

/// AOSP centerInScreen (`:324-334`): width centred; top at 37.5% of the free height (`HEIGHT_PROPORTION`); both offsets from the display's origin.
fn centre(size: Size, display: Rect) -> Rect {
    let top_offset = ((display.height() - size.height) as f32 * HEIGHT_PROPORTION) as i32;
    let left_offset = (display.width() - size.width) / 2;
    let left = display.left + left_offset;
    let top = display.top + top_offset;
    Rect {
        left,
        top,
        right: left + size.width,
        bottom: top + size.height,
    }
}

#[cfg(test)]
mod tests {
    // Expected values are derived by hand from the AOSP formulas in each test's comment, with f32 semantics where AOSP does int/float math:
    // 1.6f = 1.6000000238..., 16:9 as f32 = 1.7777777910... (above 16/9 by 1.3e-8), so a quotient that is exact in decimal can land a hair off and truncate one lower.
    // Derivations use decimal arithmetic, then check the f32 rounding of the one value where it matters.
    use super::ActivityOrientation::{Landscape, Portrait, Unspecified};
    use super::*;

    const DISPLAY_2560X1440: Rect = Rect {
        left: 0,
        top: 0,
        right: 2560,
        bottom: 1440,
    };
    const DISPLAY_1280X800: Rect = Rect {
        left: 0,
        top: 0,
        right: 1280,
        bottom: 800,
    };
    const DISPLAY_800X1280: Rect = Rect {
        left: 0,
        top: 0,
        right: 800,
        bottom: 1280,
    };

    fn act(orientation: ActivityOrientation, resizeable: bool) -> ActivityInput {
        ActivityInput {
            orientation,
            resizeable,
            min_aspect: None,
            max_aspect: None,
        }
    }

    fn at(left: i32, top: i32, width: i32, height: i32) -> Rect {
        Rect {
            left,
            top,
            right: left + width,
            bottom: top + height,
        }
    }

    fn sz(width: i32, height: i32) -> Size {
        Size { width, height }
    }

    /// Each activity's bounds on `display` (stable = display) against its hand-derived rect.
    fn check(display: Rect, cases: &[(ActivityInput, Rect)]) {
        for (activity, expected) in cases {
            assert_eq!(
                initial_bounds(display, display, activity),
                *expected,
                "{activity:?}"
            );
        }
    }

    #[test]
    fn landscape_display_2560x1440() {
        // Ideal 2560 x 0.75 = 1920, 1440 x 0.75 = 1080; top (1440-1080) x 0.375 = 135; left (2560-w)/2. Aspect 2560/1440 = 16:9.
        // Portrait resizeable: 1440 / (16/9) + 0.5 = 810.5 -> 810; left 875.
        // Portrait unresizeable: maximise 1920x1080 portrait: 1080 / (16/9) = 607.5 -> 607 (f32 rounds 607.4999956 to 607.5; the int is 607 either way); left (2560-607)/2 = 976.
        // Landscape: match, so ideal. Landscape unresizeable: maximise 1080 x 16/9 = 1920 (f32 1920.0000143 -> 1920.0) <= 1920, so ideal.
        check(
            DISPLAY_2560X1440,
            &[
                (act(Unspecified, true), at(320, 135, 1920, 1080)),
                (act(Unspecified, false), at(320, 135, 1920, 1080)),
                (act(Landscape, true), at(320, 135, 1920, 1080)),
                (act(Landscape, false), at(320, 135, 1920, 1080)),
                (act(Portrait, true), at(875, 135, 810, 1080)),
                (act(Portrait, false), at(976, 135, 607, 1080)),
            ],
        );
    }

    #[test]
    fn landscape_display_1280x800() {
        // Ideal 960x600; top (800-600) x 0.375 = 75. Aspect 1280/800 = 1.6f.
        // Portrait resizeable: 800 / 1.6 + 0.5 = 500.5 -> 500 (f32 800/1.6f = 500.0); left (1280-500)/2 = 390.
        // Portrait unresizeable: 600 / 1.6 = 375 -> 375 (f32 375.0; an f64 quotient 374.99999 would give 374); left (1280-375)/2 = 452.
        // Landscape unresizeable: 600 x 1.6 = 960 (f32 960.0000143 -> 960.0) <= 960, so ideal.
        check(
            DISPLAY_1280X800,
            &[
                (act(Unspecified, true), at(160, 75, 960, 600)),
                (act(Unspecified, false), at(160, 75, 960, 600)),
                (act(Landscape, true), at(160, 75, 960, 600)),
                (act(Landscape, false), at(160, 75, 960, 600)),
                (act(Portrait, true), at(390, 75, 500, 600)),
                (act(Portrait, false), at(452, 75, 375, 600)),
            ],
        );
    }

    #[test]
    fn portrait_display_800x1280() {
        // Ideal 600x960; top (1280-960) x 0.375 = 120; left (800-600)/2 = 100. Aspect 1280/800 = 1.6f.
        // Landscape resizeable (mismatch): width 800 - 2 x 25 = 750; height 800 / 1.6 + 0.5 = 500.5 -> 500; top (1280-500) x 0.375 = 292.5 -> 292; left (800-750)/2 = 25.
        // Landscape unresizeable: maximise 750x960 landscape: 960 x 1.6 = 1536 > 750, so width 750, height 750 / 1.6 = 468.75 -> 468; top (1280-468) x 0.375 = 304.5 -> 304.
        // Portrait unresizeable: 960 / 1.6 = 600 <= 600, so ideal.
        check(
            DISPLAY_800X1280,
            &[
                (act(Unspecified, true), at(100, 120, 600, 960)),
                (act(Unspecified, false), at(100, 120, 600, 960)),
                (act(Portrait, true), at(100, 120, 600, 960)),
                (act(Portrait, false), at(100, 120, 600, 960)),
                (act(Landscape, true), at(25, 292, 750, 500)),
                (act(Landscape, false), at(25, 304, 750, 468)),
            ],
        );
    }

    #[test]
    fn declared_aspect_limits_clamp_the_display_aspect() {
        let d = DISPLAY_2560X1440;
        // max 1.5 < 16:9: aspect 1.5. Portrait resizeable: 1440 / 1.5 + 0.5 = 960.5 -> 960; left (2560-960)/2 = 800.
        let max = ActivityInput {
            max_aspect: Some(1.5),
            ..act(Portrait, true)
        };
        assert_eq!(initial_bounds(d, d, &max), at(800, 135, 960, 1080));
        // min 2.0 > 16:9: aspect 2.0. Portrait resizeable: 1440 / 2 + 0.5 = 720.5 -> 720; left (2560-720)/2 = 920.
        let min = ActivityInput {
            min_aspect: Some(2.0),
            ..act(Portrait, true)
        };
        assert_eq!(initial_bounds(d, d, &min), at(920, 135, 720, 1080));
        // Portrait unresizeable with max 1.5: maximise 1920x1080 portrait: 1080 / 1.5 = 720 <= 1920; left (2560-720)/2 = 920.
        let max_unresizeable = ActivityInput {
            max_aspect: Some(1.5),
            ..act(Portrait, false)
        };
        assert_eq!(
            initial_bounds(d, d, &max_unresizeable),
            at(920, 135, 720, 1080)
        );
        // A limit below 1 is not a limit (`>= 1` tests): max 0.5 leaves 810 as before.
        let below_one = ActivityInput {
            max_aspect: Some(0.5),
            ..act(Portrait, true)
        };
        assert_eq!(initial_bounds(d, d, &below_one), at(875, 135, 810, 1080));
    }

    #[test]
    fn stable_bounds_set_the_fit_area() {
        // Stable 2560x1000 (a 440 px bar at the bottom): still landscape. Portrait resizeable: width 1000 / (16/9) + 0.5 = 562.5 + 0.5 -> 563.
        // Height is the display's ideal 1080 (the ideal comes from the display). Centred on the display: left (2560-563)/2 = 998; top 135.
        let d = DISPLAY_2560X1440;
        let stable = Rect { bottom: 1000, ..d };
        assert_eq!(
            initial_bounds(d, stable, &act(Portrait, true)),
            at(998, 135, 563, 1080)
        );
    }

    #[test]
    fn float_edges_follow_aosp_f32_math() {
        // 600 / 1.6 = 375 exactly in decimal; f32 gives 375.0 (see the 1280x800 test). An f64 quotient truncates to 374.
        assert_eq!(
            maximize(Orientation::Portrait, sz(960, 600), 1.6),
            sz(375, 600)
        );
        // 600 x 1.01 = 606 exactly in decimal; f32 gives 606.0 (exact product 605.99999428). An f64 product truncates to 605.
        assert_eq!(
            maximize(Orientation::Landscape, sz(1000, 600), 1.01),
            sz(606, 600)
        );
    }

    #[test]
    fn screen_orientation_classes() {
        // ActivityInfo.java:1927-1951: landscape family 0, 6, 8, 11; portrait family 1, 7, 9, 12. Unspecified -1, sensor 4, nosensor 5 and locked 14 take the display's orientation.
        assert_eq!(ActivityOrientation::from_screen_orientation(0), Landscape);
        assert_eq!(ActivityOrientation::from_screen_orientation(6), Landscape);
        assert_eq!(ActivityOrientation::from_screen_orientation(8), Landscape);
        assert_eq!(ActivityOrientation::from_screen_orientation(11), Landscape);
        assert_eq!(ActivityOrientation::from_screen_orientation(1), Portrait);
        assert_eq!(ActivityOrientation::from_screen_orientation(7), Portrait);
        assert_eq!(ActivityOrientation::from_screen_orientation(9), Portrait);
        assert_eq!(ActivityOrientation::from_screen_orientation(12), Portrait);
        for other in [-1, 2, 3, 4, 5, 10, 13, 14] {
            assert_eq!(
                ActivityOrientation::from_screen_orientation(other),
                Unspecified
            );
        }
    }

    #[test]
    fn empty_rect_is_returned_unchanged() {
        let empty = at(0, 0, 0, 0);
        assert_eq!(initial_bounds(empty, empty, &act(Unspecified, true)), empty);
    }
}
