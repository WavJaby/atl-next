// SPDX-License-Identifier: MIT
//! Stage `opening`: the window opens at its Task's dp size at its first commit. One output at scale 1.6 ([`OUTPUT`]; `--output` changes its mode) as Hyprland shows it to a sandboxed app: zxdg_output_manager_v1 withheld (move_output.rs `WITHHELD`), so GDK guesses scale 2 until the window maps,
//! and the window kept at its first buffer commit's geometry (wayland_proxy.rs `FloatAtMap::Pin`), `preferred_scale` after the map only, no `configure_bounds`. A compositor that does not resize a floating window after its map keeps a wrong first commit for good.
//! The expected content is dp = px / 1.6: the declared display's dp (`--declare-display`), else 3/4 of the output ([`super::opening_size_i32`]). On [`OUTPUT`] GDK's monitor is 1440x900 logical (2880x1800 at its guess of 2), under a declared height of 914: GTK's clamp to it must not cut the window.
//! `--start-monitor-other`: atld's `--monitor` is the output at scale 1 ([`start_monitor`]), so the Task opens at that display's dp and the window's first monitor report changes the density. REGISTER waits
//! until the desktop answered the window's first commit and the monitor it entered is adopted (task_reports.rs `wait_settled`), so the app binds on the window's display: one instance, no relaunch. Every run: the
//! enter precedes `preferred_scale` (held to the map), and the services must still take one density only (no report at GDK's integer guess, 320) and the app see one change at most (S1 review F3: WeChat exits on a density change).
//! `--hold-enter`: the proxy holds the window's enter until the app's onCreate (a window on no output at its first commit). The wait must not block on it: the app binds on the start display, then the enter
//! changes the density once (two onCreate, one relaunch, densities start -> output, sizes in the start display's px then the output's).

use std::path::Path;
use std::sync::Arc;

use atl_services_types::display_model::{DENSITY_DEFAULT, Declaration, DisplayModel};

use super::move_output::{densities, display_reports};
use super::{
    ACK_WAIT, CREATE_LINE, DRAW_LINE, HEADER_MAX, INPUT_SETTLE, LAYOUT_LINE, Liveness,
    RELAUNCH_COUNTER, Sub, counter, parse_wxh, task_bounds,
};
use crate::composer::word;
use crate::nested_wayland::{GATE_OUTPUT, Output};
use crate::wait::{Miss, Wait, until};
use crate::wayland_proxy::{AtMap, FloatAtMap, Proxy, Withheld};

/// The maintainer's laptop panel: 2880x1800 at 1.6, 1800x1125 logical.
pub const OUTPUT: Output = Output {
    width: 2880,
    height: 1800,
    scale: 1.6,
    ..GATE_OUTPUT
};

/// What the proxy recorded, read after the probe took it.
pub struct Record {
    pub at_map: Arc<AtMap>,
    pub withheld: Arc<Withheld>,
}

impl Record {
    pub fn of(proxy: &Proxy) -> Record {
        Record {
            at_map: proxy.at_map(),
            withheld: proxy.withheld(),
        }
    }
}

/// `--start-monitor-other`'s atld monitor: `output`'s mode and rate at scale 1, density [`DENSITY_DEFAULT`].
pub fn start_monitor(output: DisplayModel) -> DisplayModel {
    DisplayModel {
        dpi: DENSITY_DEFAULT,
        ..output
    }
}

/// Before atld starts.
pub fn setup(proxy: &Proxy, hold_enter: bool) {
    proxy.withhold_in_contexts(&super::move_output::WITHHELD);
    proxy.float_at_map(FloatAtMap::Pin);
    if hold_enter {
        proxy.hold_enter();
    }
}

/// The probe: keeps the proxy until the fixture drew and settled. `hold_enter`: the enter goes at the app's first onCreate, then the relaunched instance must draw.
pub fn drive(proxy: Proxy, app_log: &Path, hold_enter: bool) -> Result<Liveness, Miss> {
    if hold_enter {
        until(&Wait::log(
            "create",
            "the app never created its activity with the enter held",
            ACK_WAIT,
            app_log,
            &|t: &str| t.contains(CREATE_LINE),
        ))?;
        // a failed release leaves the enter held: the relaunch wait below names it
        let _ = proxy.release_enter();
        until(&Wait::log(
            "relaunch draw",
            "the app did not draw again after the released enter",
            ACK_WAIT,
            app_log,
            &|t: &str| {
                t.split(CREATE_LINE)
                    .nth(2)
                    .is_some_and(|after| after.contains(DRAW_LINE))
            },
        ))?;
    }
    until(&Wait::log(
        "draw",
        "the fixture never drew",
        ACK_WAIT,
        app_log,
        &|t: &str| t.contains(DRAW_LINE),
    ))?;
    std::thread::sleep(INPUT_SETTLE);
    Ok(Liveness::default())
}

/// The sub-gates: policy (the proxy was Hyprland), geometry (the first buffer commit's window is the content at 1.6 plus the header, and stays), bounds (the window told the services no other size),
/// layout (every layout at that size, one instance), instances (no relaunch), densities (the services took `display`'s density only, the app saw one change at most). `start`: atld's monitor, whose dp the Task opens at.
pub fn checks(
    app: &str,
    services: &str,
    display: DisplayModel,
    start: DisplayModel,
    declared: Option<Declaration>,
    record: &Record,
    hold_enter: bool,
) -> Vec<Sub> {
    let (pinned, held) = record.at_map.counts();
    let withheld = record.withheld.dropped();
    let policy = format!(
        "{pinned} configures pinned, {held} preferred_scale held, {withheld} globals withheld"
    );
    let to_dp = |px: i32| {
        (f64::from(px) * f64::from(DENSITY_DEFAULT) / f64::from(start.dpi)).round() as i32
    };
    let to_px = |dp: i32| {
        (f64::from(dp) * f64::from(display.dpi) / f64::from(DENSITY_DEFAULT)).round() as i32
    };
    let dp = match declared {
        Some(d) => (d.width_dp, d.height_dp),
        None => {
            let (w, h) = super::opening_size_i32(start);
            (to_dp(w), to_dp(h))
        }
    };
    let px = (to_px(dp.0), to_px(dp.1));
    // `--hold-enter`: the app binds on the start display first, so its sizes are in that display's px until the enter
    let start_px = {
        let at = |dp: i32| {
            (f64::from(dp) * f64::from(start.dpi) / f64::from(DENSITY_DEFAULT)).round() as i32
        };
        (at(dp.0), at(dp.1))
    };
    let held_enters = record.at_map.held_enters();
    let (want_creates, want_relaunches) = if hold_enter { (2, 1) } else { (1, 0) };
    let (first, last) = (record.at_map.first(), record.at_map.last());
    let sized = first.is_some_and(|(w, h)| w == dp.0 && (dp.1..dp.1 + HEADER_MAX).contains(&h));
    let shown = format!("first {first:?}, last {last:?}, content wanted {dp:?} logical");
    let bounds = task_bounds(services);
    let layouts: Vec<Option<(i32, i32)>> = app
        .lines()
        .filter(|l| l.contains(LAYOUT_LINE))
        .map(|l| word(l, "size").and_then(parse_wxh))
        .collect();
    let creates = app.matches(CREATE_LINE).count();
    let laid_out = if hold_enter {
        format!(
            "layouts {layouts:?}, {creates} onCreate, want {start_px:?} then {px:?} px, 2 onCreate"
        )
    } else {
        format!("layouts {layouts:?}, {creates} onCreate, want {px:?} px, 1 onCreate")
    };
    let relaunches = counter(services, RELAUNCH_COUNTER);
    let instances = format!(
        "{creates} onCreate, {relaunches} relaunches, want {want_creates} and {want_relaunches}; {held_enters} enters held"
    );
    let taken: Vec<i32> = display_reports(services)
        .iter()
        .filter(|r| r.changed)
        .map(|r| r.display.dpi)
        .collect();
    let seen: Vec<i32> = densities(app).iter().map(|(config, _)| *config).collect();
    let changes = seen.windows(2).filter(|w| w[0] != w[1]).count();
    let density = if hold_enter {
        format!(
            "services took dpi {taken:?}, want only {}; app densities {seen:?}, {changes} changes, want {} then {}, 1 change",
            display.dpi, start.dpi, display.dpi
        )
    } else {
        format!(
            "services took dpi {taken:?}, want only {}; app densities {seen:?}, {changes} changes, want at most 1",
            display.dpi
        )
    };
    let seen_ok = if hold_enter {
        changes == 1 && seen.first() == Some(&start.dpi) && seen.last() == Some(&display.dpi)
    } else {
        changes <= 1
    };
    // a size is the start display's (before the enter, `--hold-enter` only) or the output's, and the last is the output's
    let size_ok = |s: (i32, i32)| s == px || (hold_enter && s == start_px);
    let bounds_ok = bounds.iter().all(|b| size_ok(*b)) && bounds.last().is_none_or(|b| *b == px);
    let bounds_text = if hold_enter {
        format!("bounds reported {bounds:?}, want {start_px:?} then {px:?}, the last {px:?}")
    } else {
        format!("bounds reported {bounds:?}, want none or {px:?}")
    };
    let judged = |ok: bool, text: String| if ok { Ok(text) } else { Err(text) };
    vec![
        ("policy", judged(pinned > 0 && withheld > 0, policy)),
        ("geometry", judged(sized && last == first, shown)),
        ("bounds", judged(bounds_ok, bounds_text)),
        (
            "instances",
            judged(
                creates == want_creates
                    && relaunches == want_relaunches
                    && (held_enters > 0) == hold_enter,
                instances,
            ),
        ),
        (
            "densities",
            judged(taken.iter().all(|d| *d == display.dpi) && seen_ok, density),
        ),
        (
            "layout",
            judged(
                creates == want_creates
                    && layouts.last() == Some(&Some(px))
                    && layouts.iter().all(|l| l.is_some_and(size_ok)),
                laid_out,
            ),
        ),
    ]
}
