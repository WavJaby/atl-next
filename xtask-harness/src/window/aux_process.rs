// SPDX-License-Identifier: MIT
//! Stage `aux-process` of `xtask test window`: two processes of one uid, as WeChat's main process and its `:push`. The fixture (xtask-harness/testdata/window-aux) starts `:aux` before its window
//! activity's first layout, kills it after the window presented, starts a new `:aux` (so one registers after the first layout too), then shows a dialog: a new window of the main process.
//! Each app process registers its own UI agent and compositor (atl-services ui_agents.rs); a window's layers must reach the compositor of the process that opened its session, whichever process of
//! the uid registered last, and a sibling's death must leave them there. Pass: the fixture logged the sequence in order; the services removed the dead `:aux` agent's entry before the new one
//! registered; the main process's compositor made the window's layers before the kill and the dialog's after it, with no new unknown layer ([`layers_check`]); and before the kill it met no unknown
//! layer at all, though AuxStart released a layer right after a transaction on it and finished ([`released_check`]; red under `--break-handle-hold`).
//! Red on the per-uid table (origin/main 65f97e24, 2026-11-21): main `layers_created=1 layers_alive=0 unknown_layer=20`, WeChat r1's symptom; no removal line.

use std::path::Path;
use std::time::Duration;

use super::{ACK_WAIT, Liveness, Sub, await_event, line_stamp, wait_for_line};
use crate::composer::word;
use crate::wayland_proxy::{Event as ProxyEvent, Proxy};

/// The fixture's dir (window-aux: the window fixture's java, its own manifest).
pub(crate) const FIXTURE: &str = "xtask-harness/testdata/window-aux";
/// The stage's watch after the marker; the probe ends the run (window.rs probe_ends_run), so this only bounds the tail after it.
pub(crate) const WINDOW_MS: u64 = 2_000;
/// The fixture's lines in the order it logs them (AuxStart, AuxMain); the first before the marker, the rest after it.
const SEQUENCE: [&str; 6] = [
    "aux bound before main",
    "aux kill sent",
    "aux died",
    "aux restarted",
    "aux dialog shown",
    "aux dialog drawn",
];
/// The fixture's last line: the probe waits for it.
const LAST: &str = "aux dialog drawn";
/// AuxStart's line after its transaction on a layer it then released ([`released_check`]).
const RELEASED: &str = "aux layer released";
/// AuxMain's line when it sent the kill: the counters before it hold the window's layers before any `:aux` death.
const KILL: &str = "aux kill sent";
/// Layers the main compositor made before the kill: AuxStart's window's and AuxMain's, one each from the window service's `window_surface`.
const MIN_LAYERS_BEFORE_KILL: u64 = 2;
/// From the marker to the dialog's draw: AuxMain's 3 s kill delay, then a cold start of the new `:aux` process (8-20 s under load, window.rs probe_ends_run), with slack.
const DIALOG_WAIT: Duration = Duration::from_secs(60);
/// After the dialog's draw: the composer logs its counters at most once per second (atl-composer host.rs COUNTERS_PERIOD_NS), so two periods hold a line that includes the dialog's layer.
const COUNTERS_SETTLE: Duration = Duration::from_secs(2);
/// The main compositor's live layers once the dialog shows: the activity's window and the dialog's window, one layer each from the window service's `window_surface` (more is fine: decor children).
const MIN_LAYERS: u64 = 2;
/// The services' line when an app process's UI agent died and its entry went (atl-services ui_agents.rs `died`).
const AGENT_DIED: &str = "died; entry removed";
const AGENT_DIED_PREFIX: &str = "services: UI agent of uid ";
/// The registry's line for a registration (atl-services registry_service.rs `register`).
const AGENT_REGISTERED_PREFIX: &str = "services: registered the UI agent and compositor of uid ";

/// The probe: the window acked a configure, then the fixture logged its last line, then the counters settle. It injects nothing.
pub fn drive(proxy: Proxy, app_log: &Path) -> Result<Liveness, String> {
    await_event(&proxy.events, ACK_WAIT, |e| {
        matches!(e, ProxyEvent::Acked { .. })
    })
    .ok_or("the window never acked a configure")?;
    if !wait_for_line(app_log, LAST, DIALOG_WAIT) {
        return Err(format!("the fixture never logged `{LAST}`"));
    }
    std::thread::sleep(COUNTERS_SETTLE);
    Ok(Liveness::default())
}

/// The stage's sub-gates over the main process's log `app`, the services' log and the probe's result.
pub fn checks(app: &str, services: &str, liveness: &Result<Liveness, String>) -> Vec<Sub> {
    vec![
        (
            "probe",
            liveness
                .as_ref()
                .map(|_| "dialog drawn".to_owned())
                .map_err(String::clone),
        ),
        ("sequence", sequence_check(app)),
        ("aux-death", death_check(services)),
        ("main-layers", layers_check(app)),
        ("released-layer", released_check(app)),
    ]
}

/// The main compositor met no unknown layer before the kill: AuxStart's transaction on a layer it released right after apply ([`RELEASED`]) and AuxStart's finish both name layers whose last handle
/// goes while their transactions wait for a tick; SurfaceFlinger keeps such a layer until they are applied (atl-compositor `Compositor::handle_destroyed`). Red under `--break-handle-hold`.
/// `handle_deaths_deferred` is reported, not required: a tick between the apply and the release leaves nothing to defer.
fn released_check(app: &str) -> Result<String, String> {
    if !app.lines().any(|l| l.contains(RELEASED)) {
        return Err(format!("the fixture never logged `{RELEASED}`"));
    }
    let kill = app
        .lines()
        .filter(|l| l.contains(KILL))
        .find_map(line_stamp)
        .ok_or(format!("no `{KILL}` line"))?;
    let before = app
        .lines()
        .filter(|l| l.contains("composer: counters "))
        .filter_map(|l| Some((line_stamp(l)?, l)))
        .rfind(|(t, _)| *t < kill)
        .ok_or("no composer counters line before the kill")?
        .1;
    let n = |key: &str| {
        word(before, key)
            .and_then(|v| v.parse::<u64>().ok())
            .ok_or(format!("no {key} before the kill"))
    };
    let (unknown, deferred) = (n("unknown_layer")?, n("handle_deaths_deferred")?);
    let summary = format!("before the kill: unknown_layer={unknown} handle_deaths_deferred={deferred}");
    if unknown == 0 {
        Ok(summary)
    } else {
        Err(format!("want unknown_layer=0: {summary}"))
    }
}

fn sequence_check(app: &str) -> Result<String, String> {
    let mut lines = app.lines();
    for want in SEQUENCE {
        if !lines.any(|l| l.contains(want)) {
            return Err(format!(
                "`{want}` missing or out of order (want {SEQUENCE:?})"
            ));
        }
    }
    Ok(format!("{} lines in order", SEQUENCE.len()))
}

/// The first `:aux`'s entry went while the run went on: a removal line for a uid comes before that uid's last registration (the new `:aux`, a process start after the death). The services
/// log has no stamps, so order in the file is the clock; removals at the run's stop come after every registration and do not count.
fn death_check(services: &str) -> Result<String, String> {
    let lines: Vec<&str> = services.lines().collect();
    let uid_of = |line: &str, prefix: &str| -> Option<u32> {
        line.split_once(prefix)?
            .1
            .split_whitespace()
            .next()?
            .parse()
            .ok()
    };
    let removed_before_reregister = lines.iter().enumerate().find_map(|(i, l)| {
        if !l.contains(AGENT_DIED) {
            return None;
        }
        let uid = uid_of(l, AGENT_DIED_PREFIX)?;
        lines[i + 1..]
            .iter()
            .any(|later| uid_of(later, AGENT_REGISTERED_PREFIX) == Some(uid))
            .then(|| (*l).to_owned())
    });
    removed_before_reregister.ok_or(format!(
        "no `{AGENT_DIED_PREFIX}... {AGENT_DIED}` line before a later registration of its uid: the dead :aux agent's entry stayed"
    ))
}

/// The main process's compositor, at the last counters line before the kill (`before`) and the last after the dialog's draw (`after`):
/// - before: at least [`MIN_LAYERS_BEFORE_KILL`] layers created: AuxStart's window and AuxMain's, whose first layout came after `:aux` registered;
/// - after: the dialog's layer created since `before`, at least [`MIN_LAYERS`] alive, and `unknown_layer` unchanged since `before`.
///
/// `unknown_layer` is judged as growth across the sibling's death and re-registration; the count before the kill is [`released_check`]'s.
fn layers_check(app: &str) -> Result<String, String> {
    let stamp_of = |needle: &str| {
        app.lines()
            .filter(|l| l.contains(needle))
            .find_map(line_stamp)
            .ok_or(format!("no `{needle}` line"))
    };
    let (kill, drawn) = (stamp_of(KILL)?, stamp_of(LAST)?);
    let counters: Vec<(u64, &str)> = app
        .lines()
        .filter(|l| l.contains("composer: counters "))
        .filter_map(|l| Some((line_stamp(l)?, l)))
        .collect();
    let before = counters
        .iter()
        .rfind(|(t, _)| *t < kill)
        .ok_or("no composer counters line before the kill")?
        .1;
    let after = counters
        .iter()
        .rfind(|(t, _)| *t >= drawn)
        .ok_or("no composer counters line after the dialog's draw")?
        .1;
    let n = |line: &str, key: &str| word(line, key).and_then(|v| v.parse::<u64>().ok());
    let created_before = n(before, "layers_created").ok_or("no layers_created before the kill")?;
    let unknown_before = n(before, "unknown_layer").ok_or("no unknown_layer before the kill")?;
    let created = n(after, "layers_created").ok_or("no layers_created after the dialog")?;
    let alive = n(after, "layers_alive").ok_or("no layers_alive after the dialog")?;
    let unknown = n(after, "unknown_layer").ok_or("no unknown_layer after the dialog")?;
    let summary = format!(
        "before the kill: layers_created={created_before} unknown_layer={unknown_before}; after the dialog: layers_created={created} layers_alive={alive} unknown_layer={unknown}"
    );
    if created_before >= MIN_LAYERS_BEFORE_KILL
        && created > created_before
        && alive >= MIN_LAYERS
        && unknown == unknown_before
    {
        Ok(summary)
    } else {
        Err(format!(
            "want layers_created >= {MIN_LAYERS_BEFORE_KILL} before the kill, more after the dialog, layers_alive >= {MIN_LAYERS} and unknown_layer unchanged: {summary}"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Lines as the app's log writes them (tracing fmt: wall stamp, level, message; logcat lines as `<tag>: <message>`); `before` and `after` are the counters before the kill and after the dialog.
    fn app(before: &str, after: &str) -> String {
        format!(
            "1762500000.000  INFO AtlWindow: aux bound before main\n\
             1762500000.001  INFO AtlWindow: aux layer released\n\
             1762500001.000  INFO AtlWindow: onResume reached\n\
             1762500002.000  INFO composer: counters ticks=5 {before} ignored=\n\
             1762500004.000  INFO AtlWindow: aux kill sent\n\
             1762500004.100  INFO AtlWindow: aux died\n\
             1762500010.000  INFO AtlWindow: aux restarted\n\
             1762500010.100  INFO AtlWindow: aux dialog shown\n\
             1762500010.300  INFO AtlWindow: aux dialog drawn\n\
             1762500011.000  INFO composer: counters ticks=9 {after} ignored=\n"
        )
    }

    /// The fixed run's numbers (2026-11-21: 2 created and the launcher's finish leaving 5 unknown before the kill; dialog + its dim after) pass; each misrouting fails.
    #[test]
    fn layers_need_both_phases_in_the_main_compositor() {
        let before = "layers_created=2 layers_alive=1 unknown_layer=5";
        assert!(
            layers_check(&app(
                before,
                "layers_created=4 layers_alive=3 unknown_layer=5"
            ))
            .is_ok()
        );
        // the per-uid table: the window's layer in :aux's compositor, the main one sees transactions on layers it never made
        assert!(
            layers_check(&app(
                "layers_created=1 layers_alive=0 unknown_layer=8",
                "layers_created=1 layers_alive=0 unknown_layer=20"
            ))
            .is_err()
        );
        // the dialog's layer elsewhere: nothing created after the kill
        assert!(
            layers_check(&app(
                before,
                "layers_created=2 layers_alive=1 unknown_layer=5"
            ))
            .is_err()
        );
        // a transaction on a layer this compositor does not hold, after the kill
        assert!(
            layers_check(&app(
                before,
                "layers_created=4 layers_alive=3 unknown_layer=6"
            ))
            .is_err()
        );
    }

    /// Green: no unknown layer before the kill. Red: the 5 that AuxStart's finish left on origin/main (2026-11-21), or the fixture's line missing.
    #[test]
    fn released_layer_needs_no_unknown_layer_before_the_kill() {
        let after = "layers_created=4 layers_alive=3 unknown_layer=0 handle_deaths_deferred=1";
        let good = app(
            "layers_created=3 layers_alive=2 unknown_layer=0 handle_deaths_deferred=1",
            after,
        );
        assert!(released_check(&good).is_ok());
        let leaked = app(
            "layers_created=3 layers_alive=2 unknown_layer=5 handle_deaths_deferred=0",
            after,
        );
        assert!(released_check(&leaked).is_err());
        assert!(released_check(&good.replace("aux layer released", "aux layer kept")).is_err());
    }

    #[test]
    fn counters_before_the_dialog_draw_do_not_count_as_after() {
        let early = app(
            "layers_created=2 layers_alive=1 unknown_layer=0",
            "layers_created=4 layers_alive=3 unknown_layer=0",
        )
        .replace("1762500011.000", "1762500009.000");
        assert!(layers_check(&early).is_err());
    }

    #[test]
    fn sequence_must_be_complete_and_ordered() {
        let good = app("layers_created=2", "layers_created=4");
        assert!(sequence_check(&good).is_ok());
        assert!(sequence_check(&good.replace("aux died", "aux gone")).is_err());
        let swapped = good
            .replace("aux kill sent", "TMP")
            .replace("aux restarted", "aux kill sent")
            .replace("TMP", "aux restarted");
        assert!(sequence_check(&swapped).is_err());
    }

    /// The shape of a passing run's services log (2026-11-21, this stage): main, aux, aux's death, the new aux, then the stop's deaths.
    #[test]
    fn death_needs_the_removal_before_the_new_registration() {
        let reg = |pid: u32| {
            format!("services: registered the UI agent and compositor of uid 10002 pid {pid}\n")
        };
        let died = |pid: u32| {
            format!("services: UI agent of uid 10002 pid {pid} died; entry removed, 1 left\n")
        };
        let good = [reg(10), reg(20), died(20), reg(30), died(30), died(10)].concat();
        assert!(death_check(&good).is_ok());
        // the old table: no removal at all
        assert!(death_check(&[reg(10), reg(20), reg(30)].concat()).is_err());
        // removals only at the stop, after every registration
        assert!(death_check(&[reg(10), reg(20), reg(30), died(30), died(10)].concat()).is_err());
    }
}
