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

/// Every `diag:` line of `log`, in log order.
pub fn events(log: &str) -> Vec<Event> {
    log.lines().filter_map(parse).collect()
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
}
