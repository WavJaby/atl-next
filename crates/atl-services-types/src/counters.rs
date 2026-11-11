// SPDX-License-Identifier: MIT
//! Counted calls the services log, one stderr line each: `services: <kind> <interface>.<method> <n> at=<stamp>`. The kind says what the call got:
//! `unimplemented` (no implementation: UNKNOWN_TRANSACTION), `answered` (served, but worth a count: a no-op, a null by design, a drop), `refused` (a request the service cannot serve), `sent` (an outgoing call).
//! `<stamp>` is [`wall_stamp`], the app log's clock; xtask's atld_run.rs `events` reads the lines. A process killed at the end of a test prints no summary.

use std::collections::BTreeMap;
use std::io::Write;
use std::sync::Mutex;

/// What a counted call got: the label after `services: ` on its line. xtask's atld_run.rs `events` parses these spellings (its KINDS).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    Unimplemented,
    Answered,
    Refused,
    Sent,
}

impl Kind {
    fn label(self) -> &'static str {
        match self {
            Kind::Unimplemented => "unimplemented",
            Kind::Answered => "answered",
            Kind::Refused => "refused",
            Kind::Sent => "sent",
        }
    }
}

static COUNTS: Mutex<BTreeMap<(Kind, &'static str, String), u64>> = Mutex::new(BTreeMap::new());

/// One counted line, without its newline.
fn line(kind: Kind, interface: &str, method: &str, n: u64, stamp: &str) -> String {
    format!(
        "services: {} {interface}.{method} {n} at={stamp}",
        kind.label()
    )
}

/// Counts one call of `method` on `interface` under `kind`, writes its line, returns the new count.
fn emit(kind: Kind, interface: &'static str, method: &str) -> u64 {
    // a poisoned map still counts: it holds plain numbers
    let mut map = COUNTS.lock().unwrap_or_else(|e| e.into_inner());
    let n = map.entry((kind, interface, method.to_owned())).or_insert(0);
    *n += 1;
    let n = *n;
    drop(map);
    let _ = writeln!(
        std::io::stderr(),
        "{}",
        line(kind, interface, method, n, &wall_stamp())
    );
    n
}

/// A call the service does not implement. Counted; the caller answers UNKNOWN_TRANSACTION.
pub fn count_unimplemented(interface: &'static str, method: &str) -> u64 {
    emit(Kind::Unimplemented, interface, method)
}

/// A call the service served that is worth a count: a no-op, a null by design, a drop, a relaunch. Never unimplemented.
pub fn answered(interface: &'static str, method: &str) -> u64 {
    emit(Kind::Answered, interface, method)
}

/// Counts one outgoing call the services made (`call` on a client's binder, e.g. IInputMethod.bindInput), so `unimplemented` keeps meaning unimplemented.
pub fn sent(interface: &'static str, call: &str) -> u64 {
    emit(Kind::Sent, interface, call)
}

/// Counts one call of `method` on `interface` that the service refuses (a request it cannot serve: a null argument, an unknown window).
pub fn refused(interface: &'static str, method: &str) -> u64 {
    emit(Kind::Refused, interface, method)
}

/// The counts so far, in key order: (kind, interface, method, count).
pub fn counts() -> Vec<(Kind, &'static str, String, u64)> {
    let map = COUNTS.lock().unwrap_or_else(|e| e.into_inner());
    map.iter()
        .map(|((kind, interface, method), n)| (*kind, *interface, method.clone(), *n))
        .collect()
}

/// The wall clock as logcat stamps lines (`secs.mmm`): the clock of the app's log, which the window gate orders counter lines against.
pub fn wall_stamp() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    format!("{}.{:03}", now.as_secs(), now.subsec_millis())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_per_kind_interface_and_method() {
        assert_eq!(count_unimplemented("ITest", "a"), 1);
        assert_eq!(count_unimplemented("ITest", "a"), 2);
        assert_eq!(count_unimplemented("ITest", "b"), 1);
        assert_eq!(count_unimplemented("IOther", "a"), 1);
        assert_eq!(answered("ITest", "a"), 1);
        let c = counts();
        assert!(c.contains(&(Kind::Unimplemented, "ITest", "a".to_owned(), 2)));
        assert!(c.contains(&(Kind::Answered, "ITest", "a".to_owned(), 1)));
    }

    /// The spellings xtask's atld_run.rs `events` parses: one format across the two crates.
    #[test]
    fn labels_are_the_xtask_prefixes() {
        assert_eq!(Kind::Unimplemented.label(), "unimplemented");
        assert_eq!(Kind::Answered.label(), "answered");
        assert_eq!(Kind::Refused.label(), "refused");
        assert_eq!(Kind::Sent.label(), "sent");
    }

    #[test]
    fn line_is_the_xtask_format() {
        assert_eq!(
            line(
                Kind::Answered,
                "IInputMethodManager",
                "addClient",
                2,
                "100.000"
            ),
            "services: answered IInputMethodManager.addClient 2 at=100.000"
        );
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
