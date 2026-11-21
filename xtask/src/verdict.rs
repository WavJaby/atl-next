// SPDX-License-Identifier: MIT
//! The verdict line `native`, `check` and every `test <gate>` end with.
//!
//! Success: `<cmd>: ok`, or `<cmd>: ok (<n> <unit>)` when the run counts its steps.
//! Failure: `<cmd>: FAILED[ <k> of <n> <unit>][ (<s> skipped: <step>, ...)][: <step>, ...]; first error[ (<step>)]: "<line>"`.
//! Skipped steps (`Verdict::skipped`) with nothing to check are named, not failed: `, <s> skipped: <step>, ...` in the ok parenthesis, `(<s> skipped: ...)` after the FAILED count.
//! `<line>` is the first failing check's first error line: one line, cut to `FIRST_ERROR_CHARS` chars, quoted with `{:?}` so it stays one line whatever the text holds.
//! atl-out and atl-job (scripts/.agent) read this line: keep its shape.
//! Trap: a gate that prints its own check lines names no step; its quoted line is its first failing message. The full text is in the log.

/// Cut of the quoted first error line, in chars (`...` follows a cut). 160: two 80-column rows, so one tool result shows the verdict whole; the full line is in the log.
pub const FIRST_ERROR_CHARS: usize = 160;

/// Exit code of a gate run whose property could not be judged on this host ([`Outcome::Undecided`]): distinct from 1 (failure, anyhow), 2 (clap) and land's REBASE_CONFLICT 3.
pub const UNDECIDED_EXIT: u8 = 4;
/// The gate's own line naming why it is undecided: `<cmd>: undecided <stage>: <why>`.
pub const UNDECIDED_WORD: &str = "undecided ";

/// A gate run's outcome. Undecided: a stage proved host starvation by a measured host quantity the defect under test cannot produce, so the property was not judged: neither pass nor fail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Pass,
    Fail,
    Undecided,
}

impl Outcome {
    /// From a gate process's exit code (None: killed by a signal = failure).
    pub fn of_exit(code: Option<i32>) -> Self {
        match code {
            Some(0) => Self::Pass,
            Some(c) if c == i32::from(UNDECIDED_EXIT) => Self::Undecided,
            _ => Self::Fail,
        }
    }
}

/// `<k> gate[s] undecided: <a>; <b>`; None when none.
fn undecided_note(undecided: &[String]) -> Option<String> {
    let n = undecided.len();
    (n > 0).then(|| {
        format!(
            "{n} gate{} undecided: {}",
            if n == 1 { "" } else { "s" },
            undecided.join("; ")
        )
    })
}

/// One failed check: `step` names it when the run knows the name; `text` is its error, of which the first line is quoted.
pub struct Failure {
    pub step: Option<String>,
    pub text: String,
}

impl Failure {
    pub fn step(step: &str, text: impl Into<String>) -> Self {
        Self {
            step: Some(step.to_owned()),
            text: text.into(),
        }
    }

    pub fn gate(text: &str) -> Self {
        Self {
            step: None,
            text: text.to_owned(),
        }
    }
}

pub struct Verdict<'a> {
    pub cmd: &'a str,
    /// `check` counts its steps; `native` and the gates do not.
    pub total: Option<(usize, &'a str)>,
    /// Steps with nothing to check here: named in the line, not failures.
    pub skipped: Vec<&'a str>,
    pub failures: Vec<Failure>,
}

impl Verdict<'_> {
    pub fn line(&self) -> String {
        self.line_with(&[])
    }

    /// The line naming `undecided`: gates that could not judge their property (`<gate> <why>`, [`Outcome::Undecided`]); neither passed nor failed, so they never turn ok into FAILED.
    pub fn line_with(&self, undecided: &[String]) -> String {
        let skip = self.skip_note();
        let open = undecided_note(undecided);
        let Some(first) = self.failures.first() else {
            let counts = self.total.map(|(n, unit)| format!("{n} {unit}"));
            let parts: Vec<String> = counts.into_iter().chain(skip).collect();
            let mut inner = parts.join(", ");
            if let Some(open) = open {
                inner = if inner.is_empty() {
                    open
                } else {
                    format!("{inner}; {open}")
                };
            }
            return if inner.is_empty() {
                format!("{}: ok", self.cmd)
            } else {
                format!("{}: ok ({inner})", self.cmd)
            };
        };
        let mut line = format!("{}: FAILED", self.cmd);
        if let Some((n, unit)) = self.total {
            line.push_str(&format!(" {} of {n} {unit}", self.failures.len()));
        }
        let notes: Vec<String> = skip.into_iter().chain(open).collect();
        if !notes.is_empty() {
            line.push_str(&format!(" ({})", notes.join("; ")));
        }
        let mut steps: Vec<&str> = Vec::new();
        for step in self.failures.iter().filter_map(|f| f.step.as_deref()) {
            if !steps.contains(&step) {
                steps.push(step);
            }
        }
        if !steps.is_empty() {
            line.push_str(&format!(": {}", steps.join(", ")));
        }
        line.push_str("; first error");
        if let Some(step) = &first.step {
            line.push_str(&format!(" ({step})"));
        }
        line.push_str(&format!(": {}", quote(&first.text)));
        line
    }

    /// `<s> skipped: <step>, ...`; None when no step was skipped.
    fn skip_note(&self) -> Option<String> {
        (!self.skipped.is_empty()).then(|| {
            format!(
                "{} skipped: {}",
                self.skipped.len(),
                self.skipped.join(", ")
            )
        })
    }
}

/// A progress line, printed as a long command enters a step: `step: <label>` on stderr (stdout's last line stays the verdict line), `label` without spaces (`land:3/8:build`, `check:12/29:clippy`).
/// atl-job (scripts/.agent common `log::step`) shows the newest one in `status` as `step=<label>`, with ninja's `[n/m]` appended when that is newer: keep the shape.
pub fn step(label: &str) {
    eprintln!("step: {label}");
}

/// The verdict of a gate that prints its own check lines: `fails` are its failure messages, the first one is quoted.
pub fn gate(cmd: &str, fails: &[String]) -> String {
    Verdict {
        cmd,
        total: None,
        skipped: Vec::new(),
        failures: fails.iter().map(|f| Failure::gate(f)).collect(),
    }
    .line()
}

/// The verdict of a run that ended in an error (`?`, `bail!`): one failure, no step.
pub fn error(cmd: &str, err: &anyhow::Error) -> String {
    Verdict {
        cmd,
        total: None,
        skipped: Vec::new(),
        failures: vec![Failure::gate(&format!("{err:#}"))],
    }
    .line()
}

/// The first non-blank line of `text`, trimmed, cut to `FIRST_ERROR_CHARS` chars plus `...`, in `{:?}` form.
pub fn quote(text: &str) -> String {
    let first = text
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("(no error text)");
    let shown: String = if first.chars().count() > FIRST_ERROR_CHARS {
        first
            .chars()
            .take(FIRST_ERROR_CHARS)
            .chain("...".chars())
            .collect()
    } else {
        first.to_owned()
    };
    format!("{shown:?}")
}

/// The verdict line `cmd` ended `output` with: the last line starting `<cmd>: `.
pub fn last_line<'a>(output: &'a str, cmd: &str) -> Option<&'a str> {
    let prefix = format!("{cmd}: ");
    output.lines().rev().find(|l| l.starts_with(&prefix))
}

/// The first line of a cargo or test run's output that names a failure: an `error...` line (rustc, clippy, deny), a panic, a failed libtest case, rustfmt's `Diff in`.
/// Colour codes are removed from the line returned.
pub fn first_failure_line(output: &str) -> Option<String> {
    output
        .lines()
        .map(strip_ansi)
        .find(|l| {
            let t = l.trim_start();
            t.starts_with("error")
                || t.starts_with("Diff in")
                || t.contains("panicked at")
                || t.contains(" ... FAILED")
        })
        .map(|l| l.trim().to_owned())
}

/// `text` with CSI escape sequences (`ESC [ ... <letter>`) removed.
pub fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            for d in chars.by_ref() {
                if d.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An undecided gate is named in the line and never fails it: ok stays ok (after counts and skips), a FAILED line carries it beside its skips; the exit code alone classifies the outcome.
    #[test]
    fn undecided_gates_are_named_not_failed() {
        let starved =
            ["window-slow-display host GPU starved (render max 205 ms > 100 ms)".to_owned()];
        let ok = Verdict {
            cmd: "check",
            total: Some((29, "steps")),
            skipped: vec![],
            failures: vec![],
        };
        assert_eq!(
            ok.line_with(&starved),
            "check: ok (29 steps; 1 gate undecided: window-slow-display host GPU starved (render max 205 ms > 100 ms))"
        );
        assert_eq!(ok.line_with(&[]), "check: ok (29 steps)");
        let two = ["a x".to_owned(), "b y".to_owned()];
        let skipped = Verdict {
            cmd: "check",
            total: Some((3, "steps")),
            skipped: vec!["gates"],
            failures: vec![],
        };
        assert_eq!(
            skipped.line_with(&two),
            "check: ok (3 steps, 1 skipped: gates; 2 gates undecided: a x; b y)"
        );
        let plain = Verdict {
            cmd: "gates",
            total: None,
            skipped: vec![],
            failures: vec![],
        };
        assert_eq!(
            plain.line_with(&two[..1]),
            "gates: ok (1 gate undecided: a x)"
        );
        let failed = Verdict {
            cmd: "check",
            total: Some((29, "steps")),
            skipped: vec![],
            failures: vec![Failure::step("fmt", "diff")],
        };
        assert_eq!(
            failed.line_with(&two[..1]),
            "check: FAILED 1 of 29 steps (1 gate undecided: a x): fmt; first error (fmt): \"diff\""
        );
        assert_eq!(Outcome::of_exit(Some(0)), Outcome::Pass);
        assert_eq!(
            Outcome::of_exit(Some(i32::from(UNDECIDED_EXIT))),
            Outcome::Undecided
        );
        assert_eq!(Outcome::of_exit(Some(1)), Outcome::Fail);
        assert_eq!(Outcome::of_exit(None), Outcome::Fail);
    }

    #[test]
    fn success_lines_keep_their_shape() {
        let counted = Verdict {
            cmd: "check",
            total: Some((11, "steps")),
            skipped: vec![],
            failures: vec![],
        };
        assert_eq!(counted.line(), "check: ok (11 steps)");
        let plain = Verdict {
            cmd: "native",
            total: None,
            skipped: vec![],
            failures: vec![],
        };
        assert_eq!(plain.line(), "native: ok");
    }

    #[test]
    fn counted_failure_names_step_and_quotes_first_error() {
        let v = Verdict {
            cmd: "check",
            total: Some((11, "steps")),
            skipped: vec![],
            failures: vec![Failure::step("drift", "drift.toml: 2 rows differ\nmore")],
        };
        assert_eq!(
            v.line(),
            r#"check: FAILED 1 of 11 steps: drift; first error (drift): "drift.toml: 2 rows differ""#
        );
    }

    #[test]
    fn counted_failure_lists_every_failed_step_and_quotes_the_first() {
        let v = Verdict {
            cmd: "check",
            total: Some((11, "steps")),
            skipped: vec![],
            failures: vec![
                Failure::step("fmt", "Diff in /x/src/a.rs at line 3:"),
                Failure::step("drift", "patch failed"),
            ],
        };
        assert_eq!(
            v.line(),
            r#"check: FAILED 2 of 11 steps: fmt, drift; first error (fmt): "Diff in /x/src/a.rs at line 3:""#
        );
    }

    #[test]
    fn skipped_steps_show_in_the_ok_and_failed_lines() {
        let ok = Verdict {
            cmd: "check",
            total: Some((12, "steps")),
            skipped: vec!["converter-packages"],
            failures: vec![],
        };
        assert_eq!(
            ok.line(),
            "check: ok (12 steps, 1 skipped: converter-packages)"
        );
        let failed = Verdict {
            cmd: "check",
            total: Some((12, "steps")),
            skipped: vec!["converter-packages"],
            failures: vec![
                Failure::step("fmt", "Diff in /x/src/a.rs at line 3:"),
                Failure::step("spdx", "missing SPDX header"),
            ],
        };
        assert_eq!(
            failed.line(),
            r#"check: FAILED 2 of 12 steps (1 skipped: converter-packages): fmt, spdx; first error (fmt): "Diff in /x/src/a.rs at line 3:""#
        );
    }

    #[test]
    fn run_failure_names_steps_and_quotes_the_first() {
        let v = Verdict {
            cmd: "native",
            total: None,
            skipped: vec![],
            failures: vec![
                Failure::step("ninja", "error: linker command failed with exit code 1"),
                Failure::step("provided gate", "libfoo.so exports `x`"),
            ],
        };
        assert_eq!(
            v.line(),
            r#"native: FAILED: ninja, provided gate; first error (ninja): "error: linker command failed with exit code 1""#
        );
    }

    #[test]
    fn gate_failure_quotes_its_first_message() {
        let fails = [
            "round trip failed: 3 != 4".to_owned(),
            "cases reported".to_owned(),
        ];
        assert_eq!(
            gate("parcel", &fails),
            r#"parcel: FAILED; first error: "round trip failed: 3 != 4""#
        );
    }

    #[test]
    fn error_failure_quotes_the_error_chain() {
        let err = anyhow::anyhow!("ninja failed before building a step: duplicate rule");
        assert_eq!(
            error("native", &err),
            r#"native: FAILED; first error: "ninja failed before building a step: duplicate rule""#
        );
    }

    #[test]
    fn quote_escapes_and_stays_one_line() {
        assert_eq!(quote("say \"hi\"\tnext"), r#""say \"hi\"\tnext""#);
        assert_eq!(quote("\n  \n"), r#""(no error text)""#);
    }

    #[test]
    fn quote_cuts_at_the_char_cap() {
        let at_cap = "a".repeat(FIRST_ERROR_CHARS);
        assert_eq!(quote(&at_cap), format!("\"{at_cap}\""));
        let over = "a".repeat(FIRST_ERROR_CHARS + 1);
        assert_eq!(
            quote(&over),
            format!("\"{}...\"", "a".repeat(FIRST_ERROR_CHARS))
        );
    }

    #[test]
    fn quote_counts_chars_not_bytes() {
        // 160 two-byte chars: at the cap, not over it
        let at_cap = "é".repeat(FIRST_ERROR_CHARS);
        assert_eq!(quote(&at_cap), format!("\"{at_cap}\""));
    }

    #[test]
    fn first_failure_line_finds_compiler_test_and_fmt_failures() {
        assert_eq!(
            first_failure_line("   Compiling a\nerror[E0432]: unresolved import `x`\n --> a.rs"),
            Some("error[E0432]: unresolved import `x`".to_owned())
        );
        assert_eq!(
            first_failure_line("test a::b ... ok\ntest a::c ... FAILED\n"),
            Some("test a::c ... FAILED".to_owned())
        );
        assert_eq!(
            first_failure_line("Diff in /x/src/a.rs at line 3:\n"),
            Some("Diff in /x/src/a.rs at line 3:".to_owned())
        );
        assert_eq!(first_failure_line("   Compiling a\n    Finished\n"), None);
    }

    #[test]
    fn strip_ansi_removes_colour_codes() {
        assert_eq!(
            strip_ansi("\u{1b}[1m\u{1b}[31merror\u{1b}[0m: x"),
            "error: x"
        );
        assert_eq!(strip_ansi("plain"), "plain");
    }

    #[test]
    fn last_line_is_the_final_verdict_of_the_command() {
        let out = "== check: fmt\nchecks: not a verdict\ncheck: ok (3 steps)\n";
        assert_eq!(last_line(out, "check"), Some("check: ok (3 steps)"));
        assert_eq!(last_line("== check: fmt\n", "check"), None);
    }
}
