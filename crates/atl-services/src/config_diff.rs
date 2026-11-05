// SPDX-License-Identifier: MIT
//! Configuration.diff (Configuration.java:1919-2030) and ActivityRecord.getConfigurationChanges (ActivityRecord.java:8716-8729) over the fields the services model. ActivityRecord.shouldRelaunchLocked (:8629) is reduced to `changes & !declared`: the resources-unused, VR and desk special cases are not modelled.
//! `Config` is the plain form of the parcelable Configuration; activity.rs converts, so the default test step runs this port.
//! Undefined values are AOSP's: 0, except grammaticalGender (-1) and fontWeightAdjustment (Integer.MAX_VALUE).
//! The window part compares by plain equality. Its CONFIG_WINDOW_CONFIGURATION bit never reaches a relaunch (getConfigurationChanges clears it), so only "changed at all" reads it.

pub use crate::size_buckets::{
    CONFIG_SCREEN_LAYOUT, CONFIG_SCREEN_SIZE, CONFIG_SMALLEST_SCREEN_SIZE,
};
use crate::size_buckets::{SizeBuckets, SizeFields, filter_diff};

/// ActivityInfo.CONFIG_* (ActivityInfo.java:966-1126) not defined in size_buckets.rs.
pub const CONFIG_MCC: u32 = 0x0001;
pub const CONFIG_MNC: u32 = 0x0002;
pub const CONFIG_LOCALE: u32 = 0x0004;
pub const CONFIG_TOUCHSCREEN: u32 = 0x0008;
pub const CONFIG_KEYBOARD: u32 = 0x0010;
pub const CONFIG_KEYBOARD_HIDDEN: u32 = 0x0020;
pub const CONFIG_NAVIGATION: u32 = 0x0040;
pub const CONFIG_ORIENTATION: u32 = 0x0080;
pub const CONFIG_UI_MODE: u32 = 0x0200;
pub const CONFIG_DENSITY: u32 = 0x1000;
pub const CONFIG_LAYOUT_DIRECTION: u32 = 0x2000;
pub const CONFIG_COLOR_MODE: u32 = 0x4000;
pub const CONFIG_GRAMMATICAL_GENDER: u32 = 0x8000;
pub const CONFIG_FONT_WEIGHT_ADJUSTMENT: u32 = 0x1000_0000;
pub const CONFIG_WINDOW_CONFIGURATION: u32 = 0x2000_0000;
pub const CONFIG_FONT_SCALE: u32 = 0x4000_0000;
pub const CONFIG_ASSETS_PATHS: u32 = 0x8000_0000;

/// Configuration.SCREENLAYOUT_LAYOUTDIR_MASK (0xC0), COLOR_MODE_HDR_MASK (0xC), COLOR_MODE_WIDE_COLOR_GAMUT_MASK (0x3).
const SCREENLAYOUT_LAYOUTDIR_MASK: i32 = 0xC0;
const COLOR_MODE_HDR_MASK: i32 = 0xC;
const COLOR_MODE_WIDE_COLOR_GAMUT_MASK: i32 = 0x3;
/// Configuration.GRAMMATICAL_GENDER_UNDEFINED (-1) and FONT_WEIGHT_ADJUSTMENT_UNDEFINED (Integer.MAX_VALUE).
const GRAMMATICAL_GENDER_UNDEFINED: i32 = -1;
const FONT_WEIGHT_ADJUSTMENT_UNDEFINED: i32 = i32::MAX;

/// android.graphics.Rect as the four edges.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Rect4 {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

/// WindowConfiguration (the fields the services write). `app_bounds` None = unset.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Window {
    pub bounds: Rect4,
    pub app_bounds: Option<Rect4>,
    pub max_bounds: Rect4,
    pub windowing_mode: i32,
    pub activity_type: i32,
    pub always_on_top: i32,
    pub rotation: i32,
    pub display_rotation: i32,
}

/// The fields of android.content.res.Configuration the diff reads. Plain values; the parcelable is converted at activity.rs.
#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub font_scale: f32,
    pub mcc: i32,
    pub mnc: i32,
    /// The LocaleList as one string; None = empty.
    pub locale_list: Option<String>,
    pub touchscreen: i32,
    pub keyboard: i32,
    pub keyboard_hidden: i32,
    pub hard_keyboard_hidden: i32,
    pub navigation: i32,
    pub navigation_hidden: i32,
    pub orientation: i32,
    pub screen_layout: i32,
    pub color_mode: i32,
    pub ui_mode: i32,
    pub screen_width_dp: i32,
    pub screen_height_dp: i32,
    pub smallest_screen_width_dp: i32,
    pub density_dpi: i32,
    pub assets_seq: i32,
    pub font_weight_adjustment: i32,
    pub grammatical_gender: i32,
    pub window: Window,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            font_scale: 0.0,
            mcc: 0,
            mnc: 0,
            locale_list: None,
            touchscreen: 0,
            keyboard: 0,
            keyboard_hidden: 0,
            hard_keyboard_hidden: 0,
            navigation: 0,
            navigation_hidden: 0,
            orientation: 0,
            screen_layout: 0,
            color_mode: 0,
            ui_mode: 0,
            screen_width_dp: 0,
            screen_height_dp: 0,
            smallest_screen_width_dp: 0,
            density_dpi: 0,
            assets_seq: 0,
            font_weight_adjustment: FONT_WEIGHT_ADJUSTMENT_UNDEFINED,
            grammatical_gender: GRAMMATICAL_GENDER_UNDEFINED,
            window: Window::default(),
        }
    }
}

/// Configuration.diff(delta, compareUndefined, publicOnly) with `this` as the receiver: the CONFIG_* bits where `delta` defines a field that differs from `this`.
/// `compare_undefined` also compares fields `delta` leaves undefined.
pub fn diff(this: &Config, delta: &Config, compare_undefined: bool, public_only: bool) -> u32 {
    let u = compare_undefined;
    let mut changed = 0;
    if (u || delta.font_scale > 0.0) && this.font_scale != delta.font_scale {
        changed |= CONFIG_FONT_SCALE;
    }
    if (u || delta.mcc != 0) && this.mcc != delta.mcc {
        changed |= CONFIG_MCC;
    }
    if (u || delta.mnc != 0) && this.mnc != delta.mnc {
        changed |= CONFIG_MNC;
    }
    if (u || delta.locale_list.is_some()) && this.locale_list != delta.locale_list {
        changed |= CONFIG_LOCALE | CONFIG_LAYOUT_DIRECTION;
    }
    let delta_dir = delta.screen_layout & SCREENLAYOUT_LAYOUTDIR_MASK;
    if (u || delta_dir != 0) && delta_dir != (this.screen_layout & SCREENLAYOUT_LAYOUTDIR_MASK) {
        changed |= CONFIG_LAYOUT_DIRECTION;
    }
    if (u || delta.touchscreen != 0) && this.touchscreen != delta.touchscreen {
        changed |= CONFIG_TOUCHSCREEN;
    }
    if (u || delta.keyboard != 0) && this.keyboard != delta.keyboard {
        changed |= CONFIG_KEYBOARD;
    }
    if (u || delta.keyboard_hidden != 0) && this.keyboard_hidden != delta.keyboard_hidden {
        changed |= CONFIG_KEYBOARD_HIDDEN;
    }
    if (u || delta.hard_keyboard_hidden != 0)
        && this.hard_keyboard_hidden != delta.hard_keyboard_hidden
    {
        changed |= CONFIG_KEYBOARD_HIDDEN;
    }
    if (u || delta.navigation != 0) && this.navigation != delta.navigation {
        changed |= CONFIG_NAVIGATION;
    }
    // AOSP sets CONFIG_KEYBOARD_HIDDEN for navigationHidden too (Configuration.java:1964); kept
    if (u || delta.navigation_hidden != 0) && this.navigation_hidden != delta.navigation_hidden {
        changed |= CONFIG_KEYBOARD_HIDDEN;
    }
    if (u || delta.orientation != 0) && this.orientation != delta.orientation {
        changed |= CONFIG_ORIENTATION;
    }
    // getScreenLayoutNoDirection: the size and long bits, the layout direction masked out
    let no_dir = |layout: i32| layout & !SCREENLAYOUT_LAYOUTDIR_MASK;
    if (u || no_dir(delta.screen_layout) != 0)
        && no_dir(this.screen_layout) != no_dir(delta.screen_layout)
    {
        changed |= CONFIG_SCREEN_LAYOUT;
    }
    if (u || (delta.color_mode & COLOR_MODE_HDR_MASK) != 0)
        && (this.color_mode & COLOR_MODE_HDR_MASK) != (delta.color_mode & COLOR_MODE_HDR_MASK)
    {
        changed |= CONFIG_COLOR_MODE;
    }
    if (u || (delta.color_mode & COLOR_MODE_WIDE_COLOR_GAMUT_MASK) != 0)
        && (this.color_mode & COLOR_MODE_WIDE_COLOR_GAMUT_MASK)
            != (delta.color_mode & COLOR_MODE_WIDE_COLOR_GAMUT_MASK)
    {
        changed |= CONFIG_COLOR_MODE;
    }
    if (u || delta.ui_mode != 0) && this.ui_mode != delta.ui_mode {
        changed |= CONFIG_UI_MODE;
    }
    if (u || delta.screen_width_dp != 0) && this.screen_width_dp != delta.screen_width_dp {
        changed |= CONFIG_SCREEN_SIZE;
    }
    if (u || delta.screen_height_dp != 0) && this.screen_height_dp != delta.screen_height_dp {
        changed |= CONFIG_SCREEN_SIZE;
    }
    if (u || delta.smallest_screen_width_dp != 0)
        && this.smallest_screen_width_dp != delta.smallest_screen_width_dp
    {
        changed |= CONFIG_SMALLEST_SCREEN_SIZE;
    }
    if (u || delta.density_dpi != 0) && this.density_dpi != delta.density_dpi {
        changed |= CONFIG_DENSITY;
    }
    if (u || delta.assets_seq != 0) && this.assets_seq != delta.assets_seq {
        changed |= CONFIG_ASSETS_PATHS;
    }
    // WindowConfiguration differences are not public (Configuration.java:2014)
    if !public_only && this.window != delta.window {
        changed |= CONFIG_WINDOW_CONFIGURATION;
    }
    if (u || delta.font_weight_adjustment != FONT_WEIGHT_ADJUSTMENT_UNDEFINED)
        && this.font_weight_adjustment != delta.font_weight_adjustment
    {
        changed |= CONFIG_FONT_WEIGHT_ADJUSTMENT;
    }
    if (u || delta.grammatical_gender != GRAMMATICAL_GENDER_UNDEFINED)
        && this.grammatical_gender != delta.grammatical_gender
    {
        changed |= CONFIG_GRAMMATICAL_GENDER;
    }
    changed
}

fn size_fields(c: &Config) -> SizeFields {
    SizeFields {
        width_dp: c.screen_width_dp,
        height_dp: c.screen_height_dp,
        smallest_dp: c.smallest_screen_width_dp,
        screen_layout: c.screen_layout,
    }
}

/// ActivityRecord.getConfigurationChanges: the diff from the last reported configuration, with each size bit kept only where the app's buckets show a crossing (filterDiff), and the window bit cleared.
/// `buckets` None = the app reported none: the size bits stay.
pub fn configuration_changes(last: &Config, new: &Config, buckets: Option<&SizeBuckets>) -> u32 {
    let changes = diff(last, new, false, false);
    let changes = filter_diff(buckets, changes, size_fields(last), size_fields(new));
    changes & !CONFIG_WINDOW_CONFIGURATION
}

/// ActivityRecord.shouldRelaunchLocked, reduced: a change outside the bits the app declares (android:configChanges) relaunches it.
pub fn needs_relaunch(changes: u32, declared: u32) -> bool {
    changes & !declared != 0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A phone-sized configuration: 1280x800 dp at 160 dpi, landscape, the window at the display size.
    fn base() -> Config {
        let full = Rect4 {
            left: 0,
            top: 0,
            right: 1280,
            bottom: 800,
        };
        Config {
            font_scale: 1.0,
            locale_list: Some("en-US".to_owned()),
            density_dpi: 160,
            screen_width_dp: 1280,
            screen_height_dp: 800,
            smallest_screen_width_dp: 800,
            orientation: 2,
            window: Window {
                bounds: full,
                max_bounds: full,
                windowing_mode: 5,
                ..Window::default()
            },
            ..Config::default()
        }
    }

    /// The configuration after a Task of `width` x `height` px at 160 dpi (task_override's dp; window bounds follow).
    fn resized(width: i32, height: i32) -> Config {
        let mut c = base();
        c.screen_width_dp = width;
        c.screen_height_dp = height;
        c.smallest_screen_width_dp = width.min(height);
        c.orientation = if width <= height { 1 } else { 2 };
        c.window.bounds = Rect4 {
            left: 0,
            top: 0,
            right: width,
            bottom: height,
        };
        c
    }

    /// Buckets with only the smallest-width thresholds given (sw600dp: [600]).
    fn smallest(thresholds: &[i32]) -> SizeBuckets {
        SizeBuckets {
            smallest: Some(thresholds.to_vec()),
            ..SizeBuckets::default()
        }
    }

    /// Equal configurations differ in no bit.
    #[test]
    fn identical_configurations_have_no_changes() {
        assert_eq!(diff(&base(), &base(), false, false), 0);
        assert_eq!(configuration_changes(&base(), &base(), None), 0);
    }

    /// A screen size change sets CONFIG_SCREEN_SIZE, and the smallest width and orientation bits where they moved.
    #[test]
    fn grow_sets_size_bits_and_orientation() {
        let bits = diff(&base(), &resized(1600, 961), false, false);
        assert_eq!(bits & CONFIG_SCREEN_SIZE, CONFIG_SCREEN_SIZE);
        assert_eq!(
            bits & CONFIG_SMALLEST_SCREEN_SIZE,
            CONFIG_SMALLEST_SCREEN_SIZE
        );
        assert_eq!(bits & CONFIG_ORIENTATION, 0, "landscape stays landscape");
        assert_eq!(bits & CONFIG_LOCALE, 0);
    }

    /// A field the delta leaves undefined (0) is skipped unless compareUndefined; a changed window is not a public change.
    #[test]
    fn undefined_fields_are_skipped_unless_compared() {
        let mut delta = base();
        delta.mcc = 0;
        let mut this = base();
        this.mcc = 310;
        assert_eq!(diff(&this, &delta, false, false) & CONFIG_MCC, 0);
        assert_eq!(diff(&this, &delta, true, false) & CONFIG_MCC, CONFIG_MCC);
        let mut moved = base();
        moved.window.bounds.right = 900;
        assert_eq!(
            diff(&base(), &moved, false, false) & CONFIG_WINDOW_CONFIGURATION,
            CONFIG_WINDOW_CONFIGURATION
        );
        assert_eq!(
            diff(&base(), &moved, false, true) & CONFIG_WINDOW_CONFIGURATION,
            0
        );
    }

    /// The layout direction bits (0xC0) are not size bits: a direction-only change is CONFIG_LAYOUT_DIRECTION alone; a size-bit change is CONFIG_SCREEN_LAYOUT alone.
    #[test]
    fn layout_direction_is_not_a_size_change() {
        let mut rtl = base();
        rtl.screen_layout = 0x80;
        assert_eq!(diff(&base(), &rtl, false, false), CONFIG_LAYOUT_DIRECTION);
        let mut long = base();
        long.screen_layout = 0x03;
        assert_eq!(diff(&base(), &long, false, false), CONFIG_SCREEN_LAYOUT);
    }

    /// The window bit is cleared by getConfigurationChanges: a window-only change is no change.
    #[test]
    fn window_only_change_is_no_change() {
        let mut moved = base();
        moved.window.bounds.right = 900;
        assert_eq!(
            configuration_changes(&base(), &moved, None) & CONFIG_WINDOW_CONFIGURATION,
            0
        );
        assert_eq!(configuration_changes(&base(), &moved, None), 0);
    }

    /// No buckets reported: the size bits stay, so a resize changes the configuration.
    #[test]
    fn no_buckets_keep_the_size_bits() {
        let changes = configuration_changes(&base(), &resized(960, 561), None);
        assert_eq!(changes & CONFIG_SCREEN_SIZE, CONFIG_SCREEN_SIZE);
        assert_eq!(
            changes & CONFIG_SMALLEST_SCREEN_SIZE,
            CONFIG_SMALLEST_SCREEN_SIZE
        );
    }

    /// sw600dp buckets: a smallest width moving 800 -> 961 crosses no threshold; 961 -> 561 crosses 600.
    #[test]
    fn a_threshold_crossing_keeps_the_smallest_bit() {
        let b = smallest(&[600]);
        let grow = configuration_changes(&base(), &resized(1600, 961), Some(&b));
        assert_eq!(
            grow & CONFIG_SMALLEST_SCREEN_SIZE,
            0,
            "800 -> 961 crosses nothing"
        );
        assert_eq!(
            grow, 0,
            "the screen size bit is cleared too: no horizontal buckets"
        );
        let shrink = configuration_changes(&resized(1600, 961), &resized(960, 561), Some(&b));
        assert_eq!(
            shrink & CONFIG_SMALLEST_SCREEN_SIZE,
            CONFIG_SMALLEST_SCREEN_SIZE,
            "961 -> 561 crosses 600"
        );
    }

    /// A change the app does not declare relaunches it; a declared bit does not.
    #[test]
    fn declared_bits_avoid_the_relaunch() {
        let shrink = configuration_changes(
            &resized(1600, 961),
            &resized(960, 561),
            Some(&smallest(&[600])),
        );
        assert!(needs_relaunch(shrink, 0));
        assert!(!needs_relaunch(
            shrink,
            CONFIG_SMALLEST_SCREEN_SIZE | CONFIG_SCREEN_SIZE
        ));
        assert!(!needs_relaunch(0, 0));
    }

    /// The undefined grammatical gender is -1 (AOSP): a defined value that differs is a change, the default against itself is none.
    #[test]
    fn grammatical_gender_is_undefined_at_minus_one() {
        assert_eq!(
            diff(&Config::default(), &Config::default(), false, false),
            0
        );
        let mut gendered = Config::default();
        gendered.grammatical_gender = 1;
        assert_eq!(
            diff(&Config::default(), &gendered, false, false),
            CONFIG_GRAMMATICAL_GENDER
        );
    }
}
