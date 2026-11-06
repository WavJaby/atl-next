// SPDX-License-Identifier: MIT
//! Transactions the services do not implement, counted per (interface, method). Each count also goes to stderr as `services: unimplemented <interface>.<method> <n> at=<stamp>`,
//! `<stamp>` being [`wall_stamp`] (the app log's clock; xtask's atld_run.rs `events` reads the lines). A process killed at the end of a test prints no summary.

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
        "services: unimplemented {interface}.{method} {n} at={}",
        wall_stamp()
    );
    n
}

/// The counts so far.
pub fn counts() -> Vec<(&'static str, String, u64)> {
    let map = COUNTS.lock().unwrap_or_else(|e| e.into_inner());
    map.iter().map(|((i, m), n)| (*i, m.clone(), *n)).collect()
}

/// The wall clock as logcat stamps lines (`secs.mmm`): the clock of the app's log, which the window gate orders counter lines against.
pub fn wall_stamp() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    format!("{}.{:03}", now.as_secs(), now.subsec_millis())
}

static SENT: Mutex<BTreeMap<(&'static str, String), u64>> = Mutex::new(BTreeMap::new());

/// Counts one outgoing call the services made (`call` on a client's binder, e.g. IInputMethod.bindInput), so `unimplemented` keeps meaning unimplemented. Written to stderr as
/// `services: sent <interface>.<call> <n> at=<stamp>`; xtask reads it as the `sent` kind (atld_run.rs `events`).
pub fn sent(interface: &'static str, call: &str) -> u64 {
    let mut map = SENT.lock().unwrap_or_else(|e| e.into_inner());
    let n = map.entry((interface, call.to_owned())).or_insert(0);
    *n += 1;
    let n = *n;
    drop(map);
    let _ = writeln!(
        std::io::stderr(),
        "services: sent {interface}.{call} {n} at={}",
        wall_stamp()
    );
    n
}

static REFUSED: Mutex<BTreeMap<(&'static str, String), u64>> = Mutex::new(BTreeMap::new());

/// Counts one call of `method` on `interface` that the service refuses (a request it cannot serve: a null argument, an unknown window). Each hit is also written to stderr as
/// `services: refused <interface>.<method> <n> at=<stamp>`; xtask reads these lines as the `refused` kind (atld_run.rs `events`), next to the parcelables' refusals.
pub fn refused(interface: &'static str, method: &str) -> u64 {
    let mut map = REFUSED.lock().unwrap_or_else(|e| e.into_inner());
    let n = map.entry((interface, method.to_owned())).or_insert(0);
    *n += 1;
    let n = *n;
    drop(map);
    let _ = writeln!(
        std::io::stderr(),
        "services: refused {interface}.{method} {n} at={}",
        wall_stamp()
    );
    n
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

    /// The stamp is logcat's shape: `secs.mmm`, three digits of milliseconds.
    #[test]
    fn wall_stamp_is_logcat_shaped() {
        let s = wall_stamp();
        let (secs, ms) = s.split_once('.').expect("secs.mmm");
        assert!(secs.parse::<u64>().is_ok());
        assert_eq!(ms.len(), 3);
    }
}
