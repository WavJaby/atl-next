// SPDX-License-Identifier: MIT
//! Configuration.screenLayout's SIZE, LONG and COMPAT bits: the AOSP rule (Configuration.reduceScreenLayout, Configuration.java:428-488) and the caller that applies it to a screen's dp (WindowContainer.computeScreenLayout, WindowContainer.java:1728-1735). Pure; the default test step runs it.
//! LAYOUTDIR (0xC0) and ROUND (0x300) are not computed by the rule: the caller adds them (activity.rs global_configuration): LAYOUTDIR from the locale (`layout_direction_bits`, Configuration.setLayoutDirection:2419-2424), ROUND_NO (DisplayContent.java:2493-2496). The rule keeps the bits it is given; compute_screen_layout drops them.

/// Configuration.SCREENLAYOUT_SIZE_MASK (0x0f): SMALL 0x01, NORMAL 0x02, LARGE 0x03, XLARGE 0x04 (Configuration.java:264-299); undefined is 0.
pub const SCREENLAYOUT_SIZE_MASK: i32 = 0x0f;
pub const SCREENLAYOUT_SIZE_SMALL: i32 = 0x01;
pub const SCREENLAYOUT_SIZE_NORMAL: i32 = 0x02;
pub const SCREENLAYOUT_SIZE_LARGE: i32 = 0x03;
pub const SCREENLAYOUT_SIZE_XLARGE: i32 = 0x04;
/// SCREENLAYOUT_LONG_MASK (0x30): LONG_NO 0x10, LONG_YES 0x20 (Configuration.java:302-315).
pub const SCREENLAYOUT_LONG_MASK: i32 = 0x30;
pub const SCREENLAYOUT_LONG_NO: i32 = 0x10;
pub const SCREENLAYOUT_LONG_YES: i32 = 0x20;
/// SCREENLAYOUT_LAYOUTDIR_MASK (0xC0, Configuration.java:318) and SCREENLAYOUT_ROUND_MASK (0x300, :332): not computed here.
pub const SCREENLAYOUT_LAYOUTDIR_MASK: i32 = 0xC0;
pub const SCREENLAYOUT_ROUND_MASK: i32 = 0x300;
/// LAYOUTDIR_LTR / _RTL (Configuration.java:326, :329): View's LAYOUT_DIRECTION + 1, shifted by 6.
pub const SCREENLAYOUT_LAYOUTDIR_LTR: i32 = 0x40;
pub const SCREENLAYOUT_LAYOUTDIR_RTL: i32 = 0x80;
/// ROUND_NO (Configuration.java:344). Every ATL display is not round: DisplayModel has no round flag, and DisplayContent.java:2493-2496 sets ROUND from FLAG_ROUND.
pub const SCREENLAYOUT_ROUND_NO: i32 = 0x100;
/// SCREENLAYOUT_COMPAT_NEEDED (Configuration.java:366).
pub const SCREENLAYOUT_COMPAT_NEEDED: i32 = 0x1000_0000;
/// The start of a display's rule: what resetScreenLayout (Configuration.java:421-425) leaves in LONG and SIZE, the only bits computeScreenLayout keeps (DisplayContent.java:2469-2471).
pub const SCREENLAYOUT_RESET: i32 = SCREENLAYOUT_LONG_YES | SCREENLAYOUT_SIZE_XLARGE;

/// Configuration.reduceScreenLayout (Configuration.java:428-488): `cur` reduced for a screen whose long side is `long_dp` and short side `short_dp`.
/// Only lowers: SIZE takes the smaller of `cur`'s and the screen's; LONG becomes LONG_NO when the screen is not long (a LONG_YES in `cur` survives a long one); COMPAT is set, never cleared.
/// Bits outside SIZE, LONG and COMPAT pass through. The thresholds are AOSP's ("DO NOT CHANGE", Configuration.java:433-436): 470 below which the screen is SMALL; 960 and 720 for XLARGE; 640 and 480 for LARGE; short > 321 or long > 570 for COMPAT; (long * 3) / 5 >= short - 1 for LONG (5:3).
pub fn reduce_screen_layout(cur: i32, long_dp: i32, short_dp: i32) -> i32 {
    let (size, long, compat) = if long_dp < 470 {
        (SCREENLAYOUT_SIZE_SMALL, false, false)
    } else {
        let size = if long_dp >= 960 && short_dp >= 720 {
            SCREENLAYOUT_SIZE_XLARGE
        } else if long_dp >= 640 && short_dp >= 480 {
            SCREENLAYOUT_SIZE_LARGE
        } else {
            SCREENLAYOUT_SIZE_NORMAL
        };
        (
            size,
            (long_dp * 3) / 5 >= short_dp - 1,
            short_dp > 321 || long_dp > 570,
        )
    };
    let mut layout = cur;
    if !long {
        layout = (layout & !SCREENLAYOUT_LONG_MASK) | SCREENLAYOUT_LONG_NO;
    }
    if compat {
        layout |= SCREENLAYOUT_COMPAT_NEEDED;
    }
    if size < (layout & SCREENLAYOUT_SIZE_MASK) {
        layout = (layout & !SCREENLAYOUT_SIZE_MASK) | size;
    }
    layout
}

/// WindowContainer.computeScreenLayout (WindowContainer.java:1728-1735): the LONG and SIZE of `source` reduced for a screen of `width_dp` x `height_dp`, either orientation.
/// The result holds only LONG, SIZE and COMPAT: the source's COMPAT, LAYOUTDIR and ROUND are dropped (the mask at :1730-1731) and COMPAT is recomputed from this screen.
pub fn compute_screen_layout(source: i32, width_dp: i32, height_dp: i32) -> i32 {
    let source = source & (SCREENLAYOUT_LONG_MASK | SCREENLAYOUT_SIZE_MASK);
    reduce_screen_layout(source, width_dp.max(height_dp), width_dp.min(height_dp))
}

/// Configuration.setLayoutDirection (Configuration.java:2419-2424) for a locale whose direction is `rtl` (TextUtils.getLayoutDirectionFromLocale): the LAYOUTDIR bits.
pub fn layout_direction_bits(rtl: bool) -> i32 {
    if rtl {
        SCREENLAYOUT_LAYOUTDIR_RTL
    } else {
        SCREENLAYOUT_LAYOUTDIR_LTR
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Every expected value is written out by hand from Configuration.java:437-467 (the derivation sits beside it). None is computed by the code under test.

    /// 411x914 dp (a 1080x2400 px phone at 420 dpi): NORMAL (914 < 960, 411 < 480), LONG_YES (914 x 3 / 5 = 548 >= 410), COMPAT (411 > 321).
    #[test]
    fn phone_411x914_is_normal_long_compat() {
        assert_eq!(
            compute_screen_layout(SCREENLAYOUT_RESET, 411, 914),
            0x1000_0022
        );
    }

    /// 1920x1200 dp (the WSL display at 160 dpi): XLARGE (1920 >= 960, 1200 >= 720), LONG_NO (1920 x 3 / 5 = 1152 < 1199), COMPAT. Either orientation.
    #[test]
    fn tablet_1920x1200_is_xlarge_long_no_compat_in_either_orientation() {
        assert_eq!(
            compute_screen_layout(SCREENLAYOUT_RESET, 1920, 1200),
            0x1000_0014
        );
        assert_eq!(
            compute_screen_layout(SCREENLAYOUT_RESET, 1200, 1920),
            0x1000_0014
        );
    }

    /// 1800x1125 dp (the laptop's undeclared display, audit row 8): XLARGE, LONG_NO (1800 x 3 / 5 = 1080 < 1124), COMPAT.
    #[test]
    fn laptop_1800x1125_is_xlarge_long_no_compat() {
        assert_eq!(
            compute_screen_layout(SCREENLAYOUT_RESET, 1800, 1125),
            0x1000_0014
        );
    }

    /// The long side below 470 is SMALL with no COMPAT: 469 is SMALL and LONG_NO; 470 is NORMAL (470 < 470 is false) and LONG_NO (470 x 3 / 5 = 282 < 299).
    #[test]
    fn long_side_below_470_is_small_and_470_is_normal() {
        assert_eq!(compute_screen_layout(SCREENLAYOUT_RESET, 300, 469), 0x11);
        assert_eq!(compute_screen_layout(SCREENLAYOUT_RESET, 300, 470), 0x12);
    }

    /// The 320 dp smallest width: 500x320 and 500x321 are NORMAL, LONG_NO (500 x 3 / 5 = 300 < 319), no COMPAT (short > 321 is false at 321); 500x322 adds COMPAT.
    #[test]
    fn smallest_width_320_and_321_have_no_compat_and_322_has_it() {
        assert_eq!(compute_screen_layout(SCREENLAYOUT_RESET, 500, 320), 0x12);
        assert_eq!(compute_screen_layout(SCREENLAYOUT_RESET, 500, 321), 0x12);
        assert_eq!(
            compute_screen_layout(SCREENLAYOUT_RESET, 500, 322),
            0x1000_0012
        );
    }

    /// The 480 dp smallest width: LARGE needs a long side of 640 and a short side of 480. 640x480 is LARGE, LONG_NO (640 x 3 / 5 = 384 < 479), COMPAT; 640x479 is NORMAL.
    #[test]
    fn smallest_width_480_is_large_from_480() {
        assert_eq!(
            compute_screen_layout(SCREENLAYOUT_RESET, 640, 480),
            0x1000_0013
        );
        assert_eq!(
            compute_screen_layout(SCREENLAYOUT_RESET, 640, 479),
            0x1000_0012
        );
    }

    /// The 600 dp smallest width is LARGE; its long side turns LONG at 999 (999 x 3 / 5 = 599 >= 599) and not at 998 (598 < 599).
    #[test]
    fn smallest_width_600_turns_long_at_999() {
        assert_eq!(
            compute_screen_layout(SCREENLAYOUT_RESET, 999, 600),
            0x1000_0023
        );
        assert_eq!(
            compute_screen_layout(SCREENLAYOUT_RESET, 998, 600),
            0x1000_0013
        );
    }

    /// The 720 dp smallest width: XLARGE needs a long side of 960 and a short side of 720. 960x720 is XLARGE, LONG_NO (576 < 719); 960x719 and 959x720 are LARGE.
    #[test]
    fn smallest_width_720_is_xlarge_from_720_and_960() {
        assert_eq!(
            compute_screen_layout(SCREENLAYOUT_RESET, 960, 720),
            0x1000_0014
        );
        assert_eq!(
            compute_screen_layout(SCREENLAYOUT_RESET, 960, 719),
            0x1000_0013
        );
        assert_eq!(
            compute_screen_layout(SCREENLAYOUT_RESET, 959, 720),
            0x1000_0013
        );
    }

    /// The reduction only lowers SIZE: a NORMAL (0x02, LONG undefined) on a 1920x1200 screen stays NORMAL, and gets LONG_NO and COMPAT.
    #[test]
    fn reduction_never_raises_the_size() {
        assert_eq!(
            reduce_screen_layout(SCREENLAYOUT_SIZE_NORMAL, 1920, 1200),
            0x1000_0012
        );
    }

    /// A Task reduces its display's LONG and SIZE (TaskFragment.java:2583). A portrait 500x1000 dp Task of a LONG_NO display (0x10000014) is LARGE (1000 >= 640, 500 >= 480), keeps LONG_NO, and gets COMPAT.
    /// From the reset start the same size is LONG_YES (0x10000023).
    #[test]
    fn task_keeps_its_display_long_bit() {
        assert_eq!(compute_screen_layout(0x1000_0014, 500, 1000), 0x1000_0013);
        assert_eq!(
            compute_screen_layout(SCREENLAYOUT_RESET, 500, 1000),
            0x1000_0023
        );
    }

    /// The caller keeps only LONG and SIZE of its source: the source's COMPAT (0x10000000), LAYOUTDIR LTR (0x40) and ROUND_NO (0x100) are dropped; COMPAT is recomputed for the screen.
    #[test]
    fn caller_drops_the_bits_it_does_not_own() {
        assert_eq!(
            compute_screen_layout(0x1000_0064 | 0x100, 411, 914),
            0x1000_0022
        );
        assert_eq!(compute_screen_layout(0x1000_0064 | 0x100, 300, 469), 0x11);
    }

    /// The reduction passes the bits it does not own through: LAYOUTDIR LTR (0x40) survives on a 1920x1200 screen.
    #[test]
    fn reduce_passes_other_bits_through() {
        assert_eq!(
            reduce_screen_layout(0x40 | SCREENLAYOUT_RESET, 1920, 1200),
            0x1000_0054
        );
    }

    /// Configuration.java:326 and :329: LTR = 0x01 << 6, RTL = 0x02 << 6.
    #[test]
    fn layout_direction_bits_are_0x40_for_ltr_and_0x80_for_rtl() {
        assert_eq!(layout_direction_bits(false), 0x40);
        assert_eq!(layout_direction_bits(true), 0x80);
    }
}
