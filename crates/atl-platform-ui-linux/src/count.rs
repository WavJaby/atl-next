// SPDX-License-Identifier: MIT
//! Hand-off counters of the input path (sideband gate, window key controller) and the ping hold (`ping`): `atl-platform-ui: count <what> <n>` at the first hit and each power of two (atl-diag's log policy), so a run's log says how
//! many events took each branch without a line per event. Not atl-diag itself: this crate is a dlopened module, and a cargo-linked atl-diag would be a second map beside the converter's one copy.
//! Toolkit thread only in practice; a Mutex keeps it sound anyway.

use std::collections::HashMap;
use std::sync::Mutex;

static COUNTS: Mutex<Option<HashMap<String, u64>>> = Mutex::new(None);

/// Counts one hit of `what` (a short space-free key, e.g. `sideband.held.press`); logs at 1, 2, 4, ...; returns the count so far.
pub fn hit(what: &str) -> u64 {
    let n = bump(what);
    if n.is_power_of_two() {
        tracing::info!("atl-platform-ui: count {what} {n}");
    }
    n
}

/// [`hit`] logging every hit: for rare events whose final count a run is judged on (the ping hold's `pings_held` / `pings_answered`: a few per minute at most).
pub fn each(what: &str) -> u64 {
    let n = bump(what);
    tracing::info!("atl-platform-ui: count {what} {n}");
    n
}

fn bump(what: &str) -> u64 {
    let mut m = COUNTS.lock().unwrap_or_else(|p| p.into_inner());
    let n = m
        .get_or_insert_with(HashMap::new)
        .entry(what.to_owned())
        .or_insert(0);
    *n += 1;
    *n
}

/// The count of `what` so far.
#[cfg(test)]
pub fn get(what: &str) -> u64 {
    COUNTS
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .as_ref()
        .and_then(|m| m.get(what).copied())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    #[test]
    fn counts_each_key_apart() {
        for _ in 0..3 {
            super::hit("test.a");
        }
        super::hit("test.b");
        assert_eq!((super::get("test.a"), super::get("test.b")), (3, 1));
    }
}
