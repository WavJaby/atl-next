// SPDX-License-Identifier: MIT
//! Stage `suspended` of `xtask test window` (window lifecycle S0): the probe injects xdg-shell states into the window's configures, through the proxy: `maximized` (the control, so the notify path is shown to respond), then `suspended`, each cleared by the next configure.
//! The platform layer logs each change of the toplevel's state (`atl-platform-ui: state maximized=M suspended=S`, crates/atl-platform-ui-linux/src/window.rs). Pass: every configure is acked, and the logged states after the marker hold [`WANT`] in order (other states may sit between).
//! `--break-suspended` never sends the suspended configure: the states check must then fail. The stage proves what GTK maps; what the window does when suspended is S2's.

use std::path::Path;

use super::{
    ACK_WAIT, DRAW_LINE, INPUT_SETTLE, Liveness, PONG_WAIT, RESIZE_SETTLE, Sub, await_event,
    wait_for_line,
};
use crate::wayland_proxy::{Event as ProxyEvent, Proxy, XDG_STATE_MAXIMIZED, XDG_STATE_SUSPENDED};

/// The configures in order, each with its serial: maximized, cleared, suspended, cleared. A configure carries no size (0 x 0: the client keeps its own).
const STEPS: [(&[u32], u32); 4] = [
    (&[XDG_STATE_MAXIMIZED], 0xA7C0_0301),
    (&[], 0xA7C0_0302),
    (&[XDG_STATE_SUSPENDED], 0xA7C0_0303),
    (&[], 0xA7C0_0304),
];

/// The `(maximized, suspended)` states the platform layer must log, in this order: the control on and off, then suspended on and off. The start state is not listed: it is not a step.
const WANT: [(bool, bool); 4] = [(true, false), (false, false), (false, true), (false, false)];

/// The suspended stage's probe, from the proxy's side: once the window has drawn, each of [`STEPS`] is injected and must be acked, then the probe leaves the client [`RESIZE_SETTLE`] to report it.
/// `skip_suspended` is the break: the suspended configure is never sent, so the states check fails.
pub fn drive(proxy: Proxy, app_log: &Path, skip_suspended: bool) -> Result<Liveness, String> {
    await_event(&proxy.events, ACK_WAIT, |e| {
        matches!(e, ProxyEvent::Acked { .. })
    })
    .ok_or("the window never acked a configure")?;
    wait_for_line(app_log, DRAW_LINE, ACK_WAIT)
        .then_some(())
        .ok_or("the fixture never drew")?;
    std::thread::sleep(INPUT_SETTLE);
    let mut configured = Vec::new();
    for (states, serial) in STEPS {
        if skip_suspended && states.contains(&XDG_STATE_SUSPENDED) {
            continue;
        }
        proxy
            .inject_configure(0, 0, serial, states)
            .map_err(|e| e.to_string())?;
        let acked = await_event(&proxy.events, PONG_WAIT, |e| {
            *e == ProxyEvent::Acked { serial }
        })
        .is_some();
        std::thread::sleep(RESIZE_SETTLE);
        configured.push(acked);
    }
    Ok(Liveness {
        configured,
        ..Liveness::default()
    })
}

/// The suspended stage's sub-gates: every injected configure was acked, and the platform layer logged [`WANT`] in order after the marker (`after`: the app log lines after it).
pub fn checks(after: &[&str], liveness: &Result<Liveness, String>) -> Vec<Sub> {
    vec![
        ("acked", acked_check(liveness)),
        ("states", states_check(after)),
    ]
}

fn acked_check(liveness: &Result<Liveness, String>) -> Result<String, String> {
    let probe = liveness.as_ref().map_err(String::clone)?;
    if probe.configured.is_empty() {
        return Err("the probe injected no configure".to_owned());
    }
    if probe.configured.iter().all(|acked| *acked) {
        Ok(format!("{} configures acked", probe.configured.len()))
    } else {
        Err(format!("configures acked in order: {:?}", probe.configured))
    }
}

fn states_check(after: &[&str]) -> Result<String, String> {
    let seen: Vec<(bool, bool)> = after.iter().filter_map(|line| parse_state(line)).collect();
    if contains_in_order(&seen, &WANT) {
        Ok(format!(
            "{} state lines; {WANT:?} in order (maximized, suspended)",
            seen.len()
        ))
    } else {
        Err(format!(
            "want {WANT:?} in order, the platform layer logged {seen:?}"
        ))
    }
}

/// The `(maximized, suspended)` of an `atl-platform-ui: state` line; None for any other line.
fn parse_state(line: &str) -> Option<(bool, bool)> {
    let rest = line.split_once("atl-platform-ui: state ")?.1;
    let (mut maximized, mut suspended) = (None, None);
    for field in rest.split_whitespace() {
        match field.split_once('=') {
            Some(("maximized", value)) => maximized = value.parse().ok(),
            Some(("suspended", value)) => suspended = value.parse().ok(),
            _ => {}
        }
    }
    Some((maximized?, suspended?))
}

/// Whether `want` occurs in `seen` in this order; entries between its items are fine.
fn contains_in_order(seen: &[(bool, bool)], want: &[(bool, bool)]) -> bool {
    let mut seen = seen.iter();
    want.iter().all(|w| seen.any(|s| s == w))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The line shape the app's process log writes (tracing's fmt layer, targets off): a wall stamp, the level, the message.
    #[test]
    fn parse_state_reads_the_two_flags_and_ignores_other_lines() {
        let on = "1762500000.123  INFO atl-platform-ui: state maximized=true suspended=false";
        assert_eq!(parse_state(on), Some((true, false)));
        let sus = "1762500001.001  INFO atl-platform-ui: state maximized=false suspended=true";
        assert_eq!(parse_state(sus), Some((false, true)));
        assert_eq!(
            parse_state("1762500002.0  INFO atl-platform-ui: focus true"),
            None
        );
        assert_eq!(
            parse_state("atl-platform-ui: state maximized=maybe suspended=false"),
            None
        );
    }

    /// The expected sequence is the spec's (the four steps in order); a break that drops the suspended-on entry leaves it unmatched.
    #[test]
    fn states_must_hold_the_steps_in_order() {
        let full = [
            (false, false),
            (true, false),
            (false, false),
            (false, true),
            (false, false),
        ];
        assert!(contains_in_order(&full, &WANT));
        let no_suspend = [
            (false, false),
            (true, false),
            (false, false),
            (false, false),
        ];
        assert!(!contains_in_order(&no_suspend, &WANT));
        let reversed = [(false, true), (false, false), (true, false), (false, false)];
        assert!(!contains_in_order(&reversed, &WANT));
    }

    /// Every configure acked is a pass; one unacked, or none sent, fails.
    #[test]
    fn acks_must_cover_every_configure() {
        let all = Liveness {
            configured: vec![true; STEPS.len()],
            ..Liveness::default()
        };
        assert!(acked_check(&Ok(all)).is_ok());
        let one_missing = Liveness {
            configured: vec![true, false, true, true],
            ..Liveness::default()
        };
        assert!(acked_check(&Ok(one_missing)).is_err());
        assert!(acked_check(&Ok(Liveness::default())).is_err());
        assert!(acked_check(&Err("no probe".to_owned())).is_err());
    }
}
