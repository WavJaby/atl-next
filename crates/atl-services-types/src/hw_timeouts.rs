// SPDX-License-Identifier: MIT
//! `ro.hw_timeout_multiplier`: AOSP's scale of the framework's timeouts on hardware slower than a phone (Build.HW_TIMEOUT_MULTIPLIER, Build.java:1744-1745).
//! Plumbing: atld reads the property from its props (the bundled build.prop, then the host's /etc/atl/vendor.prop, atld's host_props: the later set wins) and passes the multiplier to atl-system-services as `--hw-timeout-multiplier`; each timer is `scale(BASE)`.
//! Scaled, as the pin scales them: process start, broadcast FG/BG, the cold-start abandon. Not scaled: the FGS timers (ActivityManagerConstants.java:245, :318, :320).
//! Not scaled because ATL has no such timer: BIND_APPLICATION (ActivityManagerService.java:571) and SERVICE_TIMEOUT (ActivityManagerConstants.java:327).

/// ActivityManagerService.java:568 PROC_START_TIMEOUT: a started process must attach within it, else it failed to start.
pub const PROC_START_TIMEOUT_BASE_MS: u64 = 10 * 1000;
/// ActivityManagerService.java:584 BROADCAST_FG_TIMEOUT.
pub const BROADCAST_FG_TIMEOUT_BASE_MS: u64 = 10 * 1000;
/// ActivityManagerService.java:585 BROADCAST_BG_TIMEOUT.
pub const BROADCAST_BG_TIMEOUT_BASE_MS: u64 = 60 * 1000;
/// BroadcastConstants.java:294-299 PENDING_COLD_START_ABANDON_TIMEOUT_MILLIS: a cold start not attached within it releases the broadcast queue's slot.
pub const PENDING_COLD_START_ABANDON_BASE_MS: u64 = 5 * 60 * 1000;

/// AOSP's value when the property is unset: `SystemProperties.getInt("ro.hw_timeout_multiplier", 1)`, Build.java:1745.
const UNSET: u32 = 1;

/// The multiplier in effect. [`HwTimeouts::default`] is 1, AOSP's unset value: the timers stay at their base.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HwTimeouts(u32);

impl Default for HwTimeouts {
    fn default() -> Self {
        HwTimeouts(UNSET)
    }
}

impl HwTimeouts {
    /// The property as AOSP's `getInt` reads a decimal integer. Unset, not decimal, or below 1: 1. AOSP would take 0 and negatives as they are; 0 fails every start at once, a negative timeout fires at once. Deliberate deviation.
    pub fn from_prop(value: Option<&str>) -> Self {
        HwTimeouts(
            value
                .and_then(|v| v.parse::<u32>().ok())
                .filter(|&m| m >= 1)
                .unwrap_or(UNSET),
        )
    }

    /// The `--hw-timeout-multiplier` value: a decimal integer >= 1 ([`HwTimeouts::to_arg`]'s form).
    pub fn parse_arg(s: &str) -> Result<Self, String> {
        match s.parse::<u32>() {
            Ok(m) if m >= 1 => Ok(HwTimeouts(m)),
            _ => Err(format!("{s}: not a decimal integer >= 1")),
        }
    }

    /// The `--hw-timeout-multiplier` form of this value: what atld passes on.
    pub fn to_arg(self) -> String {
        self.0.to_string()
    }

    /// `base_ms` x the multiplier, saturating.
    pub fn scale(self, base_ms: u64) -> u64 {
        base_ms.saturating_mul(u64::from(self.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Expected values are the AOSP literals times m (ActivityManagerService.java:568, :584, :585; BroadcastConstants.java:295), not this module's constants.
    #[test]
    fn scales_each_aosp_base_by_the_multiplier() {
        for m in [1u64, 2, 3] {
            let hw = HwTimeouts::from_prop(Some(&m.to_string()));
            assert_eq!(hw.scale(PROC_START_TIMEOUT_BASE_MS), 10 * 1000 * m);
            assert_eq!(hw.scale(BROADCAST_FG_TIMEOUT_BASE_MS), 10 * 1000 * m);
            assert_eq!(hw.scale(BROADCAST_BG_TIMEOUT_BASE_MS), 60 * 1000 * m);
            assert_eq!(
                hw.scale(PENDING_COLD_START_ABANDON_BASE_MS),
                5 * 60 * 1000 * m
            );
        }
    }

    #[test]
    fn unset_or_unusable_property_is_one() {
        assert_eq!(HwTimeouts::from_prop(None).to_arg(), "1");
        assert_eq!(HwTimeouts::from_prop(Some("2")).to_arg(), "2");
        assert_eq!(HwTimeouts::from_prop(Some("x")).to_arg(), "1");
        assert_eq!(HwTimeouts::from_prop(Some("-1")).to_arg(), "1");
        assert_eq!(HwTimeouts::from_prop(Some("0")).to_arg(), "1");
        // u32::MAX + 1: out of range, as AOSP's ParseInt rejects it
        assert_eq!(HwTimeouts::from_prop(Some("4294967296")).to_arg(), "1");
        assert_eq!(HwTimeouts::default(), HwTimeouts::from_prop(None));
    }

    #[test]
    fn arg_round_trips_and_rejects_below_one() {
        let hw = HwTimeouts::parse_arg("2").expect("2 is a multiplier");
        assert_eq!(hw.to_arg(), "2");
        assert_eq!(
            HwTimeouts::parse_arg("0"),
            Err("0: not a decimal integer >= 1".to_owned())
        );
        assert!(HwTimeouts::parse_arg("-1").is_err());
    }
}
