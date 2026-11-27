// SPDX-License-Identifier: MIT
//! Stages `landscape` and `orientation` with `--declare-display WxH` (plan display-model-owner S4): on a declared display whose smallest side is under 600 dp, a fixed-orientation
//! activity turns the display as a phone does (sides swapped, rotation 1), the window takes the turned shape, and the activity is not relaunched. Expectations are worked out here from
//! AOSP's rule (DisplayRotation.rotationForOrientation: a portrait natural panel, a landscape request -> ROTATION_90 = 1) and the declaration, never read from the services.

use atl_services_types::display_model::{Declaration, DisplayModel};

use super::{
    CHANGED_LINE, CREATE_LINE, LAYOUT_LINE, PROCESS_CREATED_LINE, Sub, parse_wxh,
    process_config_check,
};
use crate::composer::word;

/// The fixture's display line (testdata Main.java logDisplay): `AtlWindow: display <created|changed|settled> info=WxH@hz,dpi,rotation sf=WxH@hz,dpi`.
const DISPLAY_LINE: &str = "AtlWindow: display ";
/// The services' turn line (tasks.rs request_orientation): `services: rotation <from> -> <to> (<why>): display <d>`, once per rotation.
const ROTATION_LINE: &str = "services: rotation ";
/// The composer's start read of the display (atl-composer task_reports.rs): `atl-composer: ui: display WxH@hz,dpi,rotation`.
const COMPOSER_DISPLAY_LINE: &str = "atl-composer: ui: display ";
/// Configuration.ORIENTATION_LANDSCAPE (Configuration.java:714).
const CONFIG_LANDSCAPE: i32 = 2;
/// The fixture's request (window-request-landscape's meta-data atl.requestOrientation = 0): SCREEN_ORIENTATION_LANDSCAPE (ActivityInfo.java:824-909), named in messages only.
const REQUESTED_LANDSCAPE: &str = "landscape";
/// The turned display's rotation: Surface.ROTATION_90, the landscape of a portrait natural panel (AOSP DisplayRotation.configure: portrait natural -> landscape 1).
const ROTATION_90: i32 = 1;

/// The panel's sides in px at `dpi`, from the declaration in dp: dp x dpi / 160, rounded half up (AOSP density = dpi / DENSITY_DEFAULT).
fn px(dp: i32, dpi: i32) -> i32 {
    (dp * dpi + 80) / 160
}

/// The turned display the request must produce: the declared panel is portrait (its height the longer side), so the landscape request shows it with its sides swapped.
/// `None` for a declaration that is not portrait: the expectation is then not the one this module states.
fn turned_expected(declared: Declaration, dpi: i32) -> Option<(i32, i32)> {
    (declared.width_dp < declared.height_dp)
        .then(|| (px(declared.height_dp, dpi), px(declared.width_dp, dpi)))
}

/// The fixture's display lines in order: `(state, info)`; `info` None when it does not parse.
fn app_displays(app: &str) -> Vec<(String, Option<DisplayModel>)> {
    app.lines()
        .filter_map(|l| {
            let rest = l.split_once(DISPLAY_LINE)?.1;
            let state = rest.split_whitespace().next()?.to_owned();
            let info = field(rest, "info=").and_then(|t| DisplayModel::parse_arg(t).ok());
            Some((state, info))
        })
        .collect()
}

/// The value of `key` (`info=`, `sf=`, `size=`, ...) in a whitespace-separated log line.
fn field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    line.split_whitespace().find_map(|w| w.strip_prefix(key))
}

/// The displays the in-app compositor logged taking (`atl-composer: ui: display WxH@hz,dpi,rotation`: its start read, then each change pushed or answered), in order: size and rotation.
fn composer_displays(app: &str) -> Vec<(i32, i32, i32)> {
    app.lines()
        .filter_map(|l| {
            let rest = l.split_once(COMPOSER_DISPLAY_LINE)?.1;
            let model = DisplayModel::parse_arg(rest.split_whitespace().next()?).ok()?;
            Some((model.width, model.height, model.rotation))
        })
        .collect()
}

/// The display the in-app compositor's start read logged.
fn composer_display(app: &str) -> Option<(i32, i32, i32)> {
    composer_displays(app).first().copied()
}

/// Sub-gate `composer` of stage `orientation` with `--declare-display`: the in-app compositor's last display is the turned one with its rotation. The turn happens while the app runs,
/// so only the services' push (UI agent DISPLAY) brings it: the window reports no new monitor. Tilt reads the same value (atl-composer start.rs, `composer.display().rotation`).
fn composer_turn_check(app: &str, shown: (i32, i32)) -> Result<String, String> {
    let all = composer_displays(app);
    match all.last() {
        Some(&(w, h, rot)) if (w, h, rot) == (shown.0, shown.1, ROTATION_90) => Ok(format!(
            "composer took {}x{} rotation {ROTATION_90} after {} display line(s)",
            shown.0,
            shown.1,
            all.len()
        )),
        other => Err(format!(
            "composer displays {all:?}, last {other:?}; want {}x{} rotation {ROTATION_90} after the request",
            shown.0, shown.1
        )),
    }
}

/// Whether the services logged the turn `from -> to` with `why` (tasks.rs request_orientation): one line, from the rotation it leaves.
fn turn_logged(services: &str, why: &str) -> bool {
    services
        .lines()
        .any(|l| l.contains(&format!("{ROTATION_LINE}0 -> {ROTATION_90} ({why})")))
}

/// Layout lines whose `size` and `window` are not `want` (px, the content and the window frame), and the count of layouts.
fn layouts_off(app: &str, want: (i32, i32)) -> (usize, Vec<String>) {
    let layouts: Vec<&str> = app.lines().filter(|l| l.contains(LAYOUT_LINE)).collect();
    let off = layouts
        .iter()
        .filter(|l| {
            field(l, "size=").and_then(parse_wxh) != Some(want)
                || field(l, "window=").and_then(parse_wxh) != Some(want)
        })
        .map(|l| l.trim().to_owned())
        .collect();
    (layouts.len(), off)
}

/// Sub-gate `rotation` of stage `landscape`: the services turned the display once at the launch, and the fixture's first display line (created, after the process bound) shows the
/// turned display (info) with the panel's natural size as sf. The turn is before the bind, so the activity never sees the portrait panel.
fn launch_rotation_check(
    app: &str,
    services: &str,
    shown: (i32, i32),
    dpi: i32,
    natural: (i32, i32),
) -> Result<String, String> {
    if !turn_logged(services, "launch") {
        return Err(format!(
            "the services never logged `{ROTATION_LINE}0 -> {ROTATION_90} (launch)`"
        ));
    }
    let created = app.lines().find_map(|l| {
        let rest = l.split_once(DISPLAY_LINE)?.1;
        (rest.split_whitespace().next() == Some("created")).then_some(rest)
    });
    let Some(rest) = created else {
        return Err("the fixture never logged its created display".to_owned());
    };
    let info = field(rest, "info=").and_then(|t| DisplayModel::parse_arg(t).ok());
    let sf = field(rest, "sf=")
        .and_then(|t| t.split_once('@'))
        .and_then(|(size, _)| parse_wxh(size));
    let want_sf = natural;
    match info {
        Some(m)
            if (m.width, m.height, m.rotation, m.dpi) == (shown.0, shown.1, ROTATION_90, dpi)
                && sf == Some(want_sf) =>
        {
            Ok(format!(
                "launch turn 0 -> {ROTATION_90}: created info {}x{} rotation {ROTATION_90}, sf {}x{}",
                shown.0, shown.1, want_sf.0, want_sf.1
            ))
        }
        _ => Err(format!(
            "created {:?} sf {sf:?}; want info {}x{}@{dpi},rotation {ROTATION_90} and sf {}x{}",
            info.map(|m| (m.width, m.height, m.rotation)),
            shown.0,
            shown.1,
            want_sf.0,
            want_sf.1
        )),
    }
}

/// Sub-gate `instances` of stage `landscape` and `orientation`: one onCreate, so the turn (launch or request) did not relaunch the activity.
fn one_create(app: &str) -> Result<String, String> {
    let creates = app.matches(CREATE_LINE).count();
    if creates == 1 {
        Ok("1 onCreate, 0 relaunches".to_owned())
    } else {
        Err(format!(
            "{creates} onCreate (want 1: a turn relaunches an activity without configChanges)"
        ))
    }
}

/// Sub-gate `window` after the request (stage orientation): the last layout's content and window frame are `shown`. The layouts before the turn (the portrait opening) are not judged.
fn last_window_check(app: &str, shown: (i32, i32)) -> Result<String, String> {
    let layouts: Vec<&str> = app.lines().filter(|l| l.contains(LAYOUT_LINE)).collect();
    let Some(last) = layouts.last() else {
        return Err("the fixture never laid out".to_owned());
    };
    let got = (
        field(last, "size=").and_then(parse_wxh),
        field(last, "window=").and_then(parse_wxh),
    );
    if got == (Some(shown), Some(shown)) {
        Ok(format!(
            "last of {} layouts {}x{} (content and window)",
            layouts.len(),
            shown.0,
            shown.1
        ))
    } else {
        Err(format!(
            "last of {} layouts {got:?}, want {}x{} after the request",
            layouts.len(),
            shown.0,
            shown.1
        ))
    }
}

/// Sub-gate `window` of stage landscape: every layout's content and window frame are `shown` (the turned display), and the activity laid out at least once.
fn window_check(app: &str, shown: (i32, i32)) -> Result<String, String> {
    match layouts_off(app, shown) {
        (0, _) => Err("the fixture never laid out".to_owned()),
        (n, off) if off.is_empty() => Ok(format!(
            "{n} layouts {}x{} (content and window)",
            shown.0, shown.1
        )),
        (n, off) => Err(format!(
            "{} of {n} layouts off {}x{}: {:?}",
            off.len(),
            shown.0,
            shown.1,
            off.first()
        )),
    }
}

/// Sub-gate `config` of stage `landscape` (and of `orientation` through [`request_config_check`]): the first process configuration is the turned display, landscape.
fn launch_config_check(
    app: &str,
    shown: (i32, i32),
    display: DisplayModel,
) -> Result<String, String> {
    let line = app.lines().find(|l| l.contains(PROCESS_CREATED_LINE));
    process_config_check(line, shown, display)
}

/// Sub-gate `composer` of stage `landscape`: the in-app compositor's start read is the turned display with its rotation (S2a: the composer reads the services' display at start).
fn composer_check(app: &str, shown: (i32, i32), dpi: i32) -> Result<String, String> {
    match composer_display(app) {
        Some((w, h, rot)) if (w, h, rot) == (shown.0, shown.1, ROTATION_90) => Ok(format!(
            "ui: display {}x{}@{dpi},{ROTATION_90}",
            shown.0, shown.1
        )),
        other => Err(format!(
            "composer read {other:?}, want {}x{} rotation {ROTATION_90}",
            shown.0, shown.1
        )),
    }
}

/// Sub-gates of stage `landscape`: the launch turned the display before the bind (`rotation`), the activity is told the turned size in landscape (`config`), created once
/// (`instances`), laid out at the turned size (`window`), and the in-app compositor reads the turned display (`composer`). `display` is the gate's monitor; `declared` the declaration.
pub(super) fn landscape_checks(
    app: &str,
    services: &str,
    display: DisplayModel,
    declared: Option<Declaration>,
) -> Vec<Sub> {
    let Some(shown) = declared.and_then(|d| turned_expected(d, display.dpi)) else {
        let why = "stage landscape needs --declare-display WxH with width < height";
        return ["rotation", "config", "instances", "window", "composer"]
            .into_iter()
            .map(|name| (name, Err(why.to_owned())))
            .collect();
    };
    let natural = (shown.1, shown.0);
    vec![
        (
            "rotation",
            launch_rotation_check(app, services, shown, display.dpi, natural),
        ),
        ("config", launch_config_check(app, shown, display)),
        ("instances", one_create(app)),
        ("window", window_check(app, shown)),
        ("composer", composer_check(app, shown, display.dpi)),
    ]
}

/// Sub-gate `config` of stage `orientation` with a declared display: the activity was told the turned size as onConfigurationChanged, landscape, with the width in dp.
fn request_config_check(
    app: &str,
    shown: (i32, i32),
    display: DisplayModel,
) -> Result<String, String> {
    let want_dp = shown.0 * 160 / display.dpi.max(1);
    let told = app.lines().filter(|l| l.contains(CHANGED_LINE)).any(|l| {
        word(l, "orientation") == Some(&CONFIG_LANDSCAPE.to_string())
            && word(l, "screenWidthDp") == Some(&want_dp.to_string())
    });
    if told {
        Ok(format!(
            "onConfigurationChanged landscape screenWidthDp={want_dp}, 1 onCreate"
        ))
    } else {
        Err(format!(
            "no configuration change to landscape screenWidthDp={want_dp} (orientation {CONFIG_LANDSCAPE})"
        ))
    }
}

/// Sub-gates of stage `orientation` with `--declare-display` (fixture window-request-landscape): the activity's setRequestedOrientation(LANDSCAPE) turned the display (`rotation`:
/// the services' turn line, then the last display line `settled` shows the turned display), the activity is told the change without a relaunch (`config`, `instances`), and the window is
/// laid out at the turned size after the request (`window`), and the in-app compositor takes the turned display while the app runs (`composer`, [`composer_turn_check`]).
pub(super) fn orientation_checks(
    app: &str,
    services: &str,
    display: DisplayModel,
    declared: Option<Declaration>,
) -> Vec<Sub> {
    let Some(shown) = declared.and_then(|d| turned_expected(d, display.dpi)) else {
        let why = "stage orientation with --declare-display needs WxH with width < height";
        return ["rotation", "config", "instances", "window", "composer"]
            .into_iter()
            .map(|name| (name, Err(why.to_owned())))
            .collect();
    };
    let settled = app_displays(app)
        .into_iter()
        .rev()
        .find(|(state, _)| state == "settled")
        .and_then(|(_, info)| info);
    let rotation = match settled {
        Some(m)
            if turn_logged(services, "setRequestedOrientation")
                && (m.width, m.height, m.rotation) == (shown.0, shown.1, ROTATION_90) =>
        {
            Ok(format!(
                "setRequestedOrientation turn 0 -> {ROTATION_90}: settled info {}x{} rotation {ROTATION_90}",
                shown.0, shown.1
            ))
        }
        other => Err(format!(
            "services turn logged {}, settled info {:?}; want {}x{} rotation {ROTATION_90} after `{REQUESTED_LANDSCAPE}`",
            turn_logged(services, "setRequestedOrientation"),
            other.map(|m| (m.width, m.height, m.rotation)),
            shown.0,
            shown.1
        )),
    };
    vec![
        ("rotation", rotation),
        ("config", request_config_check(app, shown, display)),
        ("instances", one_create(app)),
        ("window", last_window_check(app, shown)),
        ("composer", composer_turn_check(app, shown)),
    ]
}
