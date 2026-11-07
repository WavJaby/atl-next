// SPDX-License-Identifier: MIT
//! The process whose window has input focus, as the services learn it: every focus gain of a window reaches InputMethodManager as startInputOrWindowGainedFocus from that window's process (ViewRootImpl.handleWindowFocusChanged -> InputMethodManager.startInputInner),
//! so its calling pid names the focus owner (InputMethodManagerService keeps mCurFocusedWindow from the same call). No call reports a loss: the last owner stays until another process gains focus, including while the desktop's focus is outside every app window.

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
