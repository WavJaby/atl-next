// SPDX-License-Identifier: MIT
//! Reads the `diag:` lines (crate docs) back out of a log: the one parser xtask's gates and atld's end-of-run summary use.

/// One key's count: `exact` when an orderly exit wrote its total, else the last power-of-two line's count, a lower bound within 2x.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Counter {
    pub outcome: String,
    pub api: String,
    pub count: u64,
    pub exact: bool,
}

/// One `diag:` line: the wall-clock time (ms since the epoch) it stands for (`at=`; a total's `first=`) and the key's count as of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    pub at_ms: u64,
    pub counter: Counter,
    /// what the hook took for this line (`cause=`), if anything
    pub cause: Option<String>,
}

/// Milliseconds per second: the unit of a stamp's `secs.mmm`, logcat's and the counters' wall clock.
pub const MS_PER_S: u64 = 1_000;
/// Digits after the point of a stamp: milliseconds, as logcat prints them.
const STAMP_DIGITS: usize = 3;

/// A wall-clock stamp `secs.mmm` (logcat's first field, a `diag:` line's `at=`) as milliseconds since the epoch.
pub fn parse_stamp(text: &str) -> Option<u64> {
    let (secs, frac) = text.split_once('.')?;
    if frac.len() != STAMP_DIGITS || !frac.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(secs.parse::<u64>().ok()? * MS_PER_S + frac.parse::<u64>().ok()?)
}

/// One line as an [`Event`]; anything else (other text, a line without a parsable stamp or count) is None.
pub fn parse(line: &str) -> Option<Event> {
    let rest = line.strip_prefix("diag: ")?;
    let (kind, rest) = rest.split_once(' ')?;
    let (rest, cause) = match rest.split_once(" cause=") {
        Some((head, c)) => (head, Some(c.to_owned())),
        None => (rest, None),
    };
    let (body, stamp) = match kind {
        "first" | "count" => rest.rsplit_once(" at=")?,
        "total" => rest.rsplit_once(" first=")?,
        _ => return None,
    };
    let at_ms = parse_stamp(stamp)?;
    let (key, n) = body.rsplit_once(' ')?;
    let count = n.parse::<u64>().ok()?;
    let (outcome, api) = key.split_once(' ')?;
    Some(Event {
        at_ms,
        counter: Counter {
            outcome: outcome.to_owned(),
            api: api.to_owned(),
            count,
            exact: kind == "total",
        },
        cause,
    })
}

/// The upstream log lines of a lookup nobody serves, as (text before the name, text after it; empty = the name ends the line). Each miss writes exactly one of them, by the path that gives up:
/// - Java `ServiceManager.getServiceOrThrow` (SystemServiceRegistry's manager fetchers): `ServiceNotFoundException` "No service published for: <name>" (ServiceManager.java), logged by `SystemServiceRegistry.onServiceNotFound` in the app's log. Java's `getService` itself returns null silently: its proxy's `getService2` is `checkService2` (ServiceManagerNative.java).
/// - native `getService` (libbinder `CppBackendShim::getService`, IServiceManager.cpp) after its 5 s `checkService` poll: "Service <name> didn't start. Returning NULL", in the caller's log.
/// - servicemanager's own `getService` with start-if-not-found (ServiceManager.cpp `tryStartService`), which only native `waitForService` reaches (its first `realGetService`); in servicemanager.log (its kernel log, through atld `kernel_log_to_log`). Such a wait never returns: window.rs `UNSERVED_WAIT` reports the hang.
///
/// xtask-harness `missing_service_lines_exist_at_the_pin` checks each against the pinned sources.
pub const MISSING_SERVICE_LINES: [(&str, &str); 3] = [
    (" No service published for: ", ""),
    (" Service ", " didn't start. Returning NULL"),
    (
        " Since '",
        "' could not be found trying to start it as a lazy AIDL service.",
    ),
];

/// The service name of a logcat line (atl-services-types `logcat::format`: wall stamp first) that is one of [`MISSING_SERVICE_LINES`], with the line's stamp.
fn missing_service(line: &str) -> Option<(u64, &str)> {
    let at_ms = parse_stamp(line.split(' ').next()?)?;
    MISSING_SERVICE_LINES.iter().find_map(|(before, after)| {
        let (_, rest) = line.split_once(before)?;
        let name = if after.is_empty() {
            rest.trim_end()
        } else {
            rest.split_once(after)?.0
        };
        (!name.is_empty()).then_some((at_ms, name))
    })
}

/// Every `diag:` line of `log`, in log order, and each [`MISSING_SERVICE_LINES`] line as a `missing-service <name>` event whose count is the lines so far: every miss writes one, so it is exact.
pub fn events(log: &str) -> Vec<Event> {
    let mut misses = std::collections::BTreeMap::<&str, u64>::new();
    log.lines()
        .filter_map(|line| {
            parse(line).or_else(|| {
                let (at_ms, name) = missing_service(line)?;
                let n = misses.entry(name).or_default();
                *n += 1;
                Some(Event {
                    at_ms,
                    counter: Counter {
                        outcome: crate::MISSING_SERVICE.to_owned(),
                        api: name.to_owned(),
                        count: *n,
                        exact: true,
                    },
                    cause: None,
                })
            })
        })
        .collect()
}

/// Each (outcome, api) key's count among `events`: its total line's if one was written, else the highest count seen. Key order.
pub fn totals<'a>(events: impl IntoIterator<Item = &'a Event>) -> Vec<Counter> {
    let mut by_key = std::collections::BTreeMap::<(String, String), (u64, bool)>::new();
    for e in events {
        let c = &e.counter;
        let slot = by_key
            .entry((c.outcome.clone(), c.api.clone()))
            .or_default();
        if c.exact {
            *slot = (c.count, true);
        } else if !slot.1 {
            slot.0 = slot.0.max(c.count);
        }
    }
    by_key
        .into_iter()
        .map(|((outcome, api), (count, exact))| Counter {
            outcome,
            api,
            count,
            exact,
        })
        .collect()
}

/// The [`totals`] of `log`.
pub fn counters(log: &str) -> Vec<Counter> {
    totals(&events(log))
}

/// `<outcome> <api> x<n>`, `x>=<n>` for a lower bound.
pub fn show(c: &Counter) -> String {
    let bound = if c.exact { "" } else { ">=" };
    format!("{} {} x{bound}{}", c.outcome, c.api, c.count)
}

/// [`counters`] as [`show`] lines.
pub fn counter_lines(log: &str) -> Vec<String> {
    counters(log).iter().map(show).collect()
}

/// The ranked list (plan diagnostics-f4) of a run, `logs` one per process: every key but the served-by-design ones (`answered`, `sent`), most hits first, then first seen; each with its first-seen
/// offset from `since_ms` (an app's spawn) and the last cause it carried. A key several processes count sums their [`totals`] (each process has its own map), exact only when every one is.
pub fn summary(logs: &[impl AsRef<str>], since_ms: u64) -> Vec<String> {
    let mut rows: Vec<(Counter, u64, Option<String>)> = Vec::new();
    for log in logs {
        let all = events(log.as_ref());
        for c in totals(&all) {
            if c.outcome == crate::ANSWERED || c.outcome == crate::SENT {
                continue;
            }
            let same = |e: &&Event| e.counter.outcome == c.outcome && e.counter.api == c.api;
            let first = all.iter().filter(same).map(|e| e.at_ms).min().unwrap_or(0);
            let cause = all
                .iter()
                .filter(same)
                .filter_map(|e| e.cause.clone())
                .next_back();
            match rows
                .iter_mut()
                .find(|r| r.0.outcome == c.outcome && r.0.api == c.api)
            {
                Some(r) => {
                    r.0.count += c.count;
                    r.0.exact &= c.exact;
                    r.1 = r.1.min(first);
                    r.2 = cause.or(r.2.take());
                }
                None => rows.push((c, first, cause)),
            }
        }
    }
    rows.sort_by(|a, b| b.0.count.cmp(&a.0.count).then(a.1.cmp(&b.1)));
    rows.into_iter()
        .map(|(c, first, cause)| {
            let cause = cause.map_or_else(String::new, |c| format!(" cause: {c}"));
            let secs = first.saturating_sub(since_ms) as f64 / MS_PER_S as f64;
            format!("{} +{secs:.3}s{cause}", show(&c))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every line kind; an api with spaces keeps them; a total wins over the power-of-two lines; other lines and a line without a stamp are not counted.
    #[test]
    fn totals_prefer_the_exit_total_and_else_the_highest_count() {
        let log = "services: ready\n\
                   diag: first unimplemented IActivityManager.getProcessLimit 1 at=100.000\n\
                   diag: first refused-parcel android.os.Bundle (value type) 1 at=100.001 cause=x\n\
                   diag: count unimplemented IActivityManager.getProcessLimit 2 at=100.002\n\
                   diag: first answered IInputMethodManager.addClient 1 at=100.006\n\
                   diag: count answered IInputMethodManager.addClient 4 at=100.007\n\
                   diag: total answered IInputMethodManager.addClient 7 first=100.006\n\
                   diag: count unimplemented IPackageManager.getMoveStatus 8\n";
        assert_eq!(
            counter_lines(log),
            [
                "answered IInputMethodManager.addClient x7",
                "refused-parcel android.os.Bundle (value type) x>=1",
                "unimplemented IActivityManager.getProcessLimit x>=2",
            ]
        );
        assert!(counter_lines("nothing\n").is_empty());
    }

    /// Ranked by count, then first seen; served-by-design keys left out; offsets from the spawn stamp, the cause carried along.
    #[test]
    fn summary_ranks_by_count_then_first_seen() {
        let log = "diag: first exception:SECURITY I.b 1 at=100.500\n\
                   diag: first status:BAD_VALUE I.a 1 at=100.250 cause=android.os.Message\n\
                   diag: first answered I.c 1 at=100.000\n\
                   diag: count answered I.c 2 at=100.000\n\
                   diag: first unimplemented I.d 1 at=101.000\n\
                   diag: count unimplemented I.d 2 at=101.000\n";
        assert_eq!(
            summary(&[log], 100_000),
            [
                "unimplemented I.d x>=2 +1.000s",
                "status:BAD_VALUE I.a x>=1 +0.250s cause: android.os.Message",
                "exception:SECURITY I.b x>=1 +0.500s",
            ]
        );
    }

    /// One key in two processes' logs: the counts add (3 exact + 2 exact = 5 exact; 3 exact + a lower bound 2 = >=5), first seen is the earlier one; another process's key ranks on its own.
    #[test]
    fn summary_sums_a_key_across_processes() {
        let services = "diag: first unimplemented I.a 1 at=100.200\n\
                        diag: total unimplemented I.a 3 first=100.200\n";
        let other_exact = "diag: total unimplemented I.a 2 first=100.100\n";
        let app = "diag: first unimplemented I.a 1 at=100.100\n\
                   diag: count unimplemented I.a 2 at=100.300\n\
                   diag: first refused-parcel P 1 at=100.400\n";
        assert_eq!(
            summary(&[services, other_exact], 100_000),
            ["unimplemented I.a x5 +0.100s"]
        );
        assert_eq!(
            summary(&[services, app], 100_000),
            [
                "unimplemented I.a x>=5 +0.100s",
                "refused-parcel P x>=1 +0.400s"
            ]
        );
    }

    /// The shapes lib.rs writes parse back (one format across writer and reader).
    #[test]
    fn writer_lines_parse_back() {
        let e = parse(
            "diag: first status:BAD_VALUE IMessenger.send 1 at=100.000 cause=android.os.Message",
        )
        .expect("a first line");
        assert_eq!(
            (
                e.counter.outcome.as_str(),
                e.counter.api.as_str(),
                e.counter.count
            ),
            ("status:BAD_VALUE", "IMessenger.send", 1)
        );
        assert_eq!(e.cause.as_deref(), Some("android.os.Message"));
        assert!(parse("diag: total sent I.x 3 first=1.000").is_some_and(|e| e.counter.exact));
    }

    /// Each upstream line (Java's ServiceNotFoundException message, servicemanager's with its calling-context prefix, libbinder's after the wait) becomes an exact `missing-service` count, one
    /// per line; libbinder's wait-start line and the found case are not misses.
    #[test]
    fn missing_service_lines_count_exactly() {
        let log = "\
1700000000.100 5.000   7   7 I kmsg/servicemanager: Caller(pid=9,uid=10000,sid=) Since 'atl.none' could not be found trying to start it as a lazy AIDL service. (if it's not configured\n\
1700000000.200 5.100   7   7 I kmsg/servicemanager: Caller(pid=9,uid=10000,sid=) Since 'atl.none' could not be found trying to start it as a lazy AIDL service. (if it's not configured\n\
1700000001.000 6.000   9   9 I liblog/ServiceManagerCppClient: Waiting for service 'atl.n' on '/dev/binder'...\n\
1700000006.000 11.00   9   9 W liblog/ServiceManagerCppClient: Service atl.n didn't start. Returning NULL\n\
1700000007.000 12.00   9   9 I liblog/ServiceManagerCppClient: Waiting for service 'x' on '/dev/binder' successful after waiting 3ms\n\
1700000008.000 13.00   9   9 W liblog/SystemServiceRegistry: No service published for: legacy_permission\n";
        let mut c = counters(log);
        c.sort_by(|a, b| a.api.cmp(&b.api));
        let shown: Vec<String> = c.iter().map(show).collect();
        assert_eq!(
            shown,
            [
                "missing-service atl.n x1",
                "missing-service atl.none x2",
                "missing-service legacy_permission x1"
            ]
        );
        assert_eq!(
            summary(&[log], 1_700_000_000_000),
            [
                "missing-service atl.none x2 +0.100s",
                "missing-service atl.n x1 +6.000s",
                "missing-service legacy_permission x1 +8.000s"
            ]
        );
    }
}
