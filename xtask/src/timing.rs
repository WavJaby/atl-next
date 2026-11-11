// SPDX-License-Identifier: MIT
//! Wall time per step of a multi-step command (`check` steps, `land` stages): shown on each step's own line and as a closing table, so a build or check optimisation is chosen by measured share, not by guess.

use std::time::Duration;

/// A duration as `<s>.<tenths>s`: tenths, since steps run from milliseconds to many minutes and the table ranks them, not profiles them.
pub fn secs(d: Duration) -> String {
    format!("{:.1}s", d.as_secs_f64())
}

/// Each step's wall time, in run order.
#[derive(Default)]
pub struct Timings(Vec<(String, Duration)>);

impl Timings {
    pub fn push(&mut self, step: &str, took: Duration) {
        self.0.push((step.to_owned(), took));
    }

    /// `<cmd> times:` then one row per step, longest first (ties keep run order): name, seconds, share of the total; then the total.
    pub fn table(&self, cmd: &str) -> String {
        let total: Duration = self.0.iter().map(|(_, d)| *d).sum();
        let mut rows: Vec<&(String, Duration)> = self.0.iter().collect();
        rows.sort_by_key(|r| std::cmp::Reverse(r.1));
        let width = rows
            .iter()
            .map(|(n, _)| n.len())
            .max()
            .unwrap_or(0)
            .max("total".len());
        let mut out = format!("{cmd} times:\n");
        for (name, took) in rows {
            let share = if total.is_zero() {
                0.0
            } else {
                100.0 * took.as_secs_f64() / total.as_secs_f64()
            };
            out.push_str(&format!(
                "  {name:<width$} {:>8} {share:>5.1}%\n",
                secs(*took)
            ));
        }
        out.push_str(&format!("  {:<width$} {:>8}\n", "total", secs(total)));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_are_longest_first_with_their_share() {
        let mut t = Timings::default();
        t.push("fmt", Duration::from_millis(1_000));
        t.push("clippy", Duration::from_millis(3_000));
        t.push("spdx", Duration::from_millis(0));
        // shares by hand: 3/4 = 75 %, 1/4 = 25 %, 0
        let want = "check times:\n  clippy     3.0s  75.0%\n  fmt        1.0s  25.0%\n  spdx       0.0s   0.0%\n  total      4.0s\n";
        assert_eq!(t.table("check"), want);
    }

    #[test]
    fn an_empty_run_has_a_zero_total() {
        assert_eq!(
            Timings::default().table("land"),
            "land times:\n  total     0.0s\n"
        );
    }
}
