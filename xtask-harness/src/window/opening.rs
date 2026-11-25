// SPDX-License-Identifier: MIT
//! Stage `opening`: the window opens at its Task's dp size at its first commit. One output at scale 1.6 ([`OUTPUT`]; `--output` changes its mode) as Hyprland shows it to a sandboxed app: zxdg_output_manager_v1 withheld (move_output.rs `WITHHELD`), so GDK guesses scale 2 until the window maps,
//! and the window kept at its first buffer commit's geometry (wayland_proxy.rs `FloatAtMap::Pin`), `preferred_scale` after the map only, no `configure_bounds`. A compositor that does not resize a floating window after its map keeps a wrong first commit for good.
//! The expected content is dp = px / 1.6: the declared display's dp (`--declare-display`), else 3/4 of the output ([`super::opening_size_i32`]). On [`OUTPUT`] GDK's monitor is 1440x900 logical (2880x1800 at its guess of 2), under a declared height of 914: GTK's clamp to it must not cut the window.

use std::path::Path;
use std::sync::Arc;

use atl_services_types::display_model::{DENSITY_DEFAULT, Declaration, DisplayModel};

use super::{
    ACK_WAIT, CREATE_LINE, DRAW_LINE, HEADER_MAX, INPUT_SETTLE, LAYOUT_LINE, Liveness, Sub,
    parse_wxh, task_bounds,
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

/// Before atld starts.
pub fn setup(proxy: &Proxy) {
    proxy.withhold_in_contexts(&super::move_output::WITHHELD);
    proxy.float_at_map(FloatAtMap::Pin);
}

/// The probe: keeps the proxy until the fixture drew and settled.
pub fn drive(_proxy: Proxy, app_log: &Path) -> Result<Liveness, Miss> {
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

/// The sub-gates: policy (the proxy was Hyprland), geometry (the first buffer commit's window is the content at 1.6 plus the header, and stays), bounds (the window told the services no other size), layout (every layout at that size, one instance).
pub fn checks(
    app: &str,
    services: &str,
    display: DisplayModel,
    declared: Option<Declaration>,
    record: &Record,
) -> Vec<Sub> {
    let (pinned, held) = record.at_map.counts();
    let withheld = record.withheld.dropped();
    let policy = format!(
        "{pinned} configures pinned, {held} preferred_scale held, {withheld} globals withheld"
    );
    let to_dp = |px: i32| {
        (f64::from(px) * f64::from(DENSITY_DEFAULT) / f64::from(display.dpi)).round() as i32
    };
    let to_px = |dp: i32| {
        (f64::from(dp) * f64::from(display.dpi) / f64::from(DENSITY_DEFAULT)).round() as i32
    };
    let dp = match declared {
        Some(d) => (d.width_dp, d.height_dp),
        None => {
            let (w, h) = super::opening_size_i32(display);
            (to_dp(w), to_dp(h))
        }
    };
    let px = (to_px(dp.0), to_px(dp.1));
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
    let laid_out = format!("layouts {layouts:?}, {creates} onCreate, want {px:?} px, 1 onCreate");
    let judged = |ok: bool, text: String| if ok { Ok(text) } else { Err(text) };
    vec![
        ("policy", judged(pinned > 0 && withheld > 0, policy)),
        ("geometry", judged(sized && last == first, shown)),
        (
            "bounds",
            judged(
                bounds.is_empty(),
                format!("bounds reported {bounds:?}, want none"),
            ),
        ),
        (
            "layout",
            judged(
                creates == 1 && !layouts.is_empty() && layouts.iter().all(|l| *l == Some(px)),
                laid_out,
            ),
        ),
    ]
}
