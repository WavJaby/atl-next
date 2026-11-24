// SPDX-License-Identifier: MIT
//! The process whose window has input focus, as the services learn it.
//! AOSP: a focus gain of a window that may use an input method reaches the input method service from that window's process (ViewRootImpl.handleWindowFocusChanged -> InputMethodManager onPostWindowGainedFocus): startInputOrWindowGainedFocusAsync when it starts input, else the synchronous startInputOrWindowGainedFocus (WINDOW_FOCUS_GAIN_REPORT_ONLY), which ZeroJankProxy answers null and drops.
//! Ours, not AOSP (its focus owner is WMS's and InputDispatcher's): every focus gain of a window reaches here by one of those two calls, whose calling pid names the owner; the report-only one counts too. No call reports a loss: the last owner stays until another process gains focus, including while the desktop's focus is outside every app window.

use std::sync::atomic::{AtomicI32, Ordering};

/// The focus owner's pid; 0 before any window gained focus (no process has pid 0).
#[derive(Debug, Default)]
pub struct FocusedProcess(AtomicI32);

impl FocusedProcess {
    /// A window of `pid` gained focus.
    pub fn gained(&self, pid: i32) {
        self.0.store(pid, Ordering::Relaxed);
    }

    /// Whether `pid` owns the focused window.
    pub fn is(&self, pid: i32) -> bool {
        pid != 0 && self.0.load(Ordering::Relaxed) == pid
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn last_gain_owns_focus() {
        let f = FocusedProcess::default();
        assert!(!f.is(0), "nobody before a gain");
        f.gained(7);
        assert!(f.is(7));
        f.gained(9);
        assert!(!f.is(7) && f.is(9));
    }
}
