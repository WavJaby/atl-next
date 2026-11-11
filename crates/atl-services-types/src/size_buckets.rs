// SPDX-License-Identifier: MIT
//! The size buckets an app reports and the filter that reads them: SizeConfigurationBuckets.filterDiff (SizeConfigurationBuckets.java:108-140) with its crosses* helpers (:141-222).
//! A size change counts as a configuration change only when it crosses a threshold: a resize inside one bucket is no change. An app with no size-qualified resources has no thresholds, so its resizes never count.
//! Plain Rust, so the default test step runs it. The parcelable form (atl-parcelables SizeConfigurationBuckets) builds only under the converter and converts to [`SizeBuckets`] where the services read it.

/// ActivityInfo.CONFIG_SCREEN_LAYOUT (ActivityInfo.java:1016), CONFIG_SCREEN_SIZE (:1034), CONFIG_SMALLEST_SCREEN_SIZE (:1046): the bits filter_diff may clear.
pub const CONFIG_SCREEN_LAYOUT: u32 = 0x0100;
pub const CONFIG_SCREEN_SIZE: u32 = 0x0400;
pub const CONFIG_SMALLEST_SCREEN_SIZE: u32 = 0x0800;

/// Configuration.SCREENLAYOUT_SIZE_MASK (0x0f; undefined is 0), SCREENLAYOUT_LONG_MASK (0x30) and the non-size layout bits: layout direction (0xC0), round (0x300), compat needed (0x10000000).
const SCREENLAYOUT_SIZE_MASK: i32 = 0x0f;
const SCREENLAYOUT_LONG_MASK: i32 = 0x30;
const SCREENLAYOUT_NON_SIZE_MASK: i32 = 0xC0 | 0x300 | 0x1000_0000;

/// The thresholds of one app (the fields of SizeConfigurationBuckets); None where no resource names any value.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SizeBuckets {
    pub horizontal: Option<Vec<i32>>,
    pub vertical: Option<Vec<i32>>,
    pub smallest: Option<Vec<i32>>,
    pub screen_layout_size: Option<Vec<i32>>,
    pub screen_layout_long_set: bool,
}

/// The size fields of a configuration that the filter reads: screenWidthDp, screenHeightDp, smallestScreenWidthDp, screenLayout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SizeFields {
    pub width_dp: i32,
    pub height_dp: i32,
    pub smallest_dp: i32,
    pub screen_layout: i32,
}

/// Configuration.isLayoutSizeAtLeast: the layout size of `screen_layout` is at least `size`; an undefined layout size is at least no size.
fn is_layout_size_at_least(screen_layout: i32, size: i32) -> bool {
    let current = screen_layout & SCREENLAYOUT_SIZE_MASK;
    current != 0 && current >= size
}

/// SizeConfigurationBuckets.crossesSizeThreshold: a threshold lies between `first` and `second`, in either direction (a value equal to a threshold is on its upper side). No thresholds: none crossed.
fn crosses_size_threshold(thresholds: Option<&[i32]>, first: i32, second: i32) -> bool {
    thresholds.is_some_and(|ts| {
        ts.iter()
            .any(|&t| (first < t && second >= t) || (first >= t && second < t))
    })
}

impl SizeBuckets {
    /// SizeConfigurationBuckets.crossesScreenLayoutSizeThreshold: the layout size changed and the change reaches a bucket. A layout size that got smaller always counts.
    fn crosses_screen_layout_size(&self, first: i32, second: i32) -> bool {
        if first & SCREENLAYOUT_SIZE_MASK == second & SCREENLAYOUT_SIZE_MASK {
            return false;
        }
        if !is_layout_size_at_least(second, first & SCREENLAYOUT_SIZE_MASK) {
            return true;
        }
        self.screen_layout_size.as_deref().is_some_and(|sizes| {
            sizes.iter().any(|&size| {
                is_layout_size_at_least(first, size) != is_layout_size_at_least(second, size)
            })
        })
    }

    /// SizeConfigurationBuckets.crossesScreenLayoutLongThreshold: the long bit changed, and some resource names it.
    fn crosses_screen_layout_long(&self, first: i32, second: i32) -> bool {
        self.screen_layout_long_set
            && first & SCREENLAYOUT_LONG_MASK != second & SCREENLAYOUT_LONG_MASK
    }
}

/// SizeConfigurationBuckets.filterDiff: `diff` (the CONFIG_* bits of old -> new) with each size bit cleared when the buckets show no threshold crossed. `None` buckets (the app
/// reported none) keep the diff whole.
pub fn filter_diff(
    buckets: Option<&SizeBuckets>,
    mut diff: u32,
    old: SizeFields,
    new: SizeFields,
) -> u32 {
    let Some(b) = buckets else {
        return diff;
    };
    if diff & CONFIG_SCREEN_SIZE != 0 {
        let crosses = crosses_size_threshold(b.horizontal.as_deref(), old.width_dp, new.width_dp)
            || crosses_size_threshold(b.vertical.as_deref(), old.height_dp, new.height_dp);
        if !crosses {
            diff &= !CONFIG_SCREEN_SIZE;
        }
    }
    if diff & CONFIG_SMALLEST_SCREEN_SIZE != 0
        && !crosses_size_threshold(b.smallest.as_deref(), old.smallest_dp, new.smallest_dp)
    {
        diff &= !CONFIG_SMALLEST_SCREEN_SIZE;
    }
    let non_size_unchanged = (old.screen_layout & SCREENLAYOUT_NON_SIZE_MASK)
        == (new.screen_layout & SCREENLAYOUT_NON_SIZE_MASK);
    if diff & CONFIG_SCREEN_LAYOUT != 0
        && non_size_unchanged
        && !b.crosses_screen_layout_size(old.screen_layout, new.screen_layout)
        && !b.crosses_screen_layout_long(old.screen_layout, new.screen_layout)
    {
        diff &= !CONFIG_SCREEN_LAYOUT;
    }
    diff
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Size fields of a configuration: the given dp values and screen layout.
    fn fields(width_dp: i32, height_dp: i32, smallest_dp: i32, screen_layout: i32) -> SizeFields {
        SizeFields {
            width_dp,
            height_dp,
            smallest_dp,
            screen_layout,
        }
    }

    /// Buckets with only the horizontal thresholds given.
    fn horizontal(thresholds: &[i32]) -> SizeBuckets {
        SizeBuckets {
            horizontal: Some(thresholds.to_vec()),
            ..SizeBuckets::default()
        }
    }

    /// No buckets reported: the diff is the one given, whatever changed.
    #[test]
    fn no_buckets_keep_every_bit() {
        let diff = CONFIG_SCREEN_SIZE | CONFIG_SMALLEST_SCREEN_SIZE;
        let (old, new) = (fields(400, 800, 400, 0), fields(500, 800, 500, 0));
        assert_eq!(filter_diff(None, diff, old, new), diff);
    }

    /// Buckets that name no size (an app with no size-qualified resources): a resize crosses nothing, so its size bits go.
    #[test]
    fn buckets_without_thresholds_clear_size_bits() {
        let b = SizeBuckets::default();
        let diff = CONFIG_SCREEN_SIZE | CONFIG_SMALLEST_SCREEN_SIZE;
        let (old, new) = (fields(400, 800, 400, 0), fields(500, 700, 500, 0));
        assert_eq!(filter_diff(Some(&b), diff, old, new), 0);
    }

    /// A width threshold at 480 dp: 400 to 500 crosses it and keeps the bit; 500 to 520 crosses nothing and clears it.
    #[test]
    fn width_threshold_crossing_keeps_screen_size() {
        let b = horizontal(&[480]);
        let crossing = (fields(400, 800, 400, 0), fields(500, 800, 500, 0));
        let inside = (fields(500, 800, 500, 0), fields(520, 800, 520, 0));
        assert_eq!(
            filter_diff(Some(&b), CONFIG_SCREEN_SIZE, crossing.0, crossing.1),
            CONFIG_SCREEN_SIZE
        );
        assert_eq!(
            filter_diff(Some(&b), CONFIG_SCREEN_SIZE, inside.0, inside.1),
            0
        );
    }

    /// A height threshold counts too: 500 to 700 dp crosses 600 with the width unchanged.
    #[test]
    fn height_threshold_crossing_keeps_screen_size() {
        let b = SizeBuckets {
            vertical: Some(vec![600]),
            ..SizeBuckets::default()
        };
        let (old, new) = (fields(900, 500, 500, 0), fields(900, 700, 700, 0));
        assert_eq!(
            filter_diff(Some(&b), CONFIG_SCREEN_SIZE, old, new),
            CONFIG_SCREEN_SIZE
        );
    }

    /// The smallest width has its own thresholds: 300 to 340 crosses 320; 330 to 340 does not.
    #[test]
    fn smallest_threshold_is_its_own_bit() {
        let b = SizeBuckets {
            smallest: Some(vec![320]),
            ..SizeBuckets::default()
        };
        let crossing = (fields(800, 800, 300, 0), fields(800, 800, 340, 0));
        let inside = (fields(800, 800, 330, 0), fields(800, 800, 340, 0));
        assert_eq!(
            filter_diff(
                Some(&b),
                CONFIG_SMALLEST_SCREEN_SIZE,
                crossing.0,
                crossing.1
            ),
            CONFIG_SMALLEST_SCREEN_SIZE
        );
        assert_eq!(
            filter_diff(Some(&b), CONFIG_SMALLEST_SCREEN_SIZE, inside.0, inside.1),
            0
        );
    }

    /// Layout size: small (1) to normal (2) crosses a normal-size threshold; with no such threshold it does not.
    #[test]
    fn layout_size_crossing_needs_a_bucket() {
        let with = SizeBuckets {
            screen_layout_size: Some(vec![2]),
            ..SizeBuckets::default()
        };
        let without = SizeBuckets::default();
        let (old, new) = (fields(400, 400, 400, 1), fields(400, 400, 400, 2));
        assert_eq!(
            filter_diff(Some(&with), CONFIG_SCREEN_LAYOUT, old, new),
            CONFIG_SCREEN_LAYOUT
        );
        assert_eq!(
            filter_diff(Some(&without), CONFIG_SCREEN_LAYOUT, old, new),
            0
        );
    }

    /// The long bit (0x10 no, 0x20 yes) counts only when some resource names it. A layout-direction change is a non-size change: its bit stays.
    #[test]
    fn long_bit_and_non_size_layout_bits() {
        let long = SizeBuckets {
            screen_layout_long_set: true,
            ..SizeBuckets::default()
        };
        let none = SizeBuckets::default();
        let (old, new) = (fields(400, 400, 400, 0x10), fields(400, 400, 400, 0x20));
        assert_eq!(
            filter_diff(Some(&long), CONFIG_SCREEN_LAYOUT, old, new),
            CONFIG_SCREEN_LAYOUT
        );
        assert_eq!(filter_diff(Some(&none), CONFIG_SCREEN_LAYOUT, old, new), 0);
        let (ltr, rtl) = (fields(400, 400, 400, 0x40), fields(400, 400, 400, 0x80));
        assert_eq!(
            filter_diff(Some(&none), CONFIG_SCREEN_LAYOUT, ltr, rtl),
            CONFIG_SCREEN_LAYOUT
        );
    }
}
