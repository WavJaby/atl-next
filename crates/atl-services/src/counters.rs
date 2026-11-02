// SPDX-License-Identifier: MIT
//! Transactions the services do not implement, counted per (interface, method). Each count also goes to stderr as `services: unimplemented <interface>.<method> <n>`,
//! the line `xtask test binder` reads (a process killed at the end of a test prints no summary).

use std::collections::BTreeMap;
use std::io::Write;
use std::sync::Mutex;

static COUNTS: Mutex<BTreeMap<(&'static str, String), u64>> = Mutex::new(BTreeMap::new());

/// Counts one call of `method` on `interface` and returns the new count.
pub fn count(interface: &'static str, method: &str) -> u64 {
    // a poisoned map still counts: it holds plain numbers
    let mut map = COUNTS.lock().unwrap_or_else(|e| e.into_inner());
    let n = map.entry((interface, method.to_owned())).or_insert(0);
    *n += 1;
    let n = *n;
    drop(map);
    let _ = writeln!(
        std::io::stderr(),
        "services: unimplemented {interface}.{method} {n}"
    );
    n
}

/// The counts so far.
pub fn counts() -> Vec<(&'static str, String, u64)> {
    let map = COUNTS.lock().unwrap_or_else(|e| e.into_inner());
    map.iter().map(|((i, m), n)| (*i, m.clone(), *n)).collect()
}

/// The stubs' `unimplemented` hook: counted, then UNKNOWN_TRANSACTION (frameworks/base core/java/android/os/Binder.java onTransact returns false for a code it does not know,
/// which the caller sees as UNKNOWN_TRANSACTION; a Java proxy then reads an empty reply: no exception, a null / zero result).
#[cfg(feature = "converter")]
pub fn unimplemented(interface: &'static str, method: &str) -> binder::Status {
    count(interface, method);
    binder::StatusCode::UNKNOWN_TRANSACTION.into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_per_interface_and_method() {
        assert_eq!(count("ITest", "a"), 1);
        assert_eq!(count("ITest", "a"), 2);
        assert_eq!(count("ITest", "b"), 1);
        assert_eq!(count("IOther", "a"), 1);
        let c = counts();
        assert!(c.contains(&("ITest", "a".to_owned(), 2)));
        assert!(c.contains(&("IOther", "a".to_owned(), 1)));
    }
}
