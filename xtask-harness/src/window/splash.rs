// SPDX-License-Identifier: MIT
//! Stage `splash` (splash plan slice 1a): the app's window opens before ART boots and shows the starting window, `ro.atl.splash`'s background, until the app's first present replaces it.
//! The gate sets the property itself ([`Runtime::with_vendor_property`](crate::atld_run::Runtime::with_vendor_property): test-environment setup until the installer writes it, slice 1b). Sub-gates:
//! `order` -- the launcher logs `splash: shown after=N` (its first paint, ms from the launcher's start) and later `splash: replaced after=M` (the app's first present), N < M, both printed: time-to-splash and time-to-first-app-frame;
//! `picture` -- the nested compositor's output, taken once `splash: shown` is logged and before `splash: replaced`, holds the window's content area in [`BACKGROUND`].
//! Break `--break-splash`: the property unset; the launcher must log `splash: none (no spec)` and show none, so `order` fails.

use std::path::Path;

use anyhow::Result;
use atl_services_types::compositor_registry::{SPLASH_PROPERTY, Splash};

use super::{ACK_WAIT, DRAW_LINE, Liveness, Sub, wait_for_line};
use crate::atld_run::Runtime;
use crate::nested_wayland::NestedCompositor;
use crate::screencopy::{self, Image};
use crate::wayland_proxy::Proxy;

/// The splash colour (ARGB): opaque, no channel at 0 or 255 and all three different, so neither the fixture's quadrants nor GTK's grey chrome nor an empty (black or transparent) window can match it.
pub const BACKGROUND: u32 = 0xFF20_6090;
const SHOWN: &str = "splash: shown after=";
const REPLACED: &str = "splash: replaced after=";
/// The launcher's line when the property is unset (atl-composer ui_window `open`).
const NONE: &str = "splash: none (no spec)";
/// The share of the window's content area the splash colour must cover in the picture: the whole area, less a margin for the corner rounding of the toplevel's frame.
const COVER: f64 = 0.95;

/// The run's splash property, unless the break leaves it unset.
pub fn setup(rt: Runtime, break_splash: bool) -> Result<Runtime> {
    if break_splash {
        return Ok(rt);
    }
    let arg = Splash {
        background: BACKGROUND,
    }
    .to_arg();
    rt.with_vendor_property(SPLASH_PROPERTY, &arg)
}

/// The probe: keeps the proxy until the fixture drew, so the window lives through the picture.
pub fn drive(_proxy: Proxy, app_log: &Path) -> Result<Liveness, String> {
    wait_for_line(app_log, DRAW_LINE, ACK_WAIT)
        .then_some(Liveness::default())
        .ok_or_else(|| "the fixture never drew".to_owned())
}

/// The picture taken while the splash shows: the output, and whether the app's first present was already logged once it was read (then it is not the splash's).
pub struct Shot {
    image: Image,
    late: bool,
}

/// Takes the picture once `splash: shown` is in `app` and nothing has been taken yet; called from the gate's poll loop. Saved to `out` for a look.
pub fn take(
    shot: &mut Option<Result<Shot, String>>,
    app: &str,
    app_log: &Path,
    nested: Option<&NestedCompositor>,
    out: &Path,
) {
    if shot.is_some() || !app.contains(SHOWN) {
        return;
    }
    *shot = Some(match nested {
        None => Err("no nested compositor".to_owned()),
        Some(n) => screencopy::capture(n)
            .map_err(|e| format!("{e:#}"))
            .map(|image| {
                let _ = image.write_png(out);
                let late = std::fs::read_to_string(app_log).is_ok_and(|a| a.contains(REPLACED));
                Shot { image, late }
            }),
    });
}

/// `after=<ms>` of the first line holding `prefix`.
fn after(app: &str, prefix: &str) -> Option<u64> {
    app.lines().find_map(|l| {
        let rest = &l[l.find(prefix)? + prefix.len()..];
        rest.split_whitespace().next()?.parse().ok()
    })
}

/// The sub-gates; `content`: the window's content size at opening (physical px).
pub fn checks(app: &str, shot: Option<Result<Shot, String>>, content: (u32, u32)) -> Vec<Sub> {
    let order = match (after(app, SHOWN), after(app, REPLACED)) {
        (Some(shown), Some(replaced)) if shown < replaced => Ok(format!(
            "time-to-splash {shown} ms, time-to-first-app-frame {replaced} ms (from the launcher's start)"
        )),
        (Some(shown), Some(replaced)) => Err(format!(
            "shown after={shown} is not before replaced after={replaced}"
        )),
        (None, _) if app.contains(NONE) => Err(format!("no splash: `{NONE}` logged")),
        (None, _) => Err(format!("no `{SHOWN}` line")),
        (Some(_), None) => Err(format!("no `{REPLACED}` line: the app never presented")),
    };
    let picture = match shot {
        None => Err("no picture: the splash was never shown".to_owned()),
        Some(Err(e)) => Err(format!("screencopy: {e}")),
        Some(Ok(Shot { late: true, .. })) => {
            Err("the app's first present came before the picture was read".to_owned())
        }
        Some(Ok(Shot { image, .. })) => {
            let [_, r, g, b] = BACKGROUND.to_be_bytes();
            let (pixels, _) = image.rgba.as_chunks::<4>();
            let hits = pixels
                .iter()
                .filter(|p| p[0] == r && p[1] == g && p[2] == b)
                .count();
            let area = f64::from(content.0) * f64::from(content.1);
            let share = hits as f64 / area;
            if share >= COVER {
                Ok(format!(
                    "{hits} px of #{:06X} = {:.3} of the {}x{} content",
                    BACKGROUND & 0xFF_FFFF,
                    share,
                    content.0,
                    content.1
                ))
            } else {
                Err(format!(
                    "{hits} px of #{:06X} = {share:.3} of the {}x{} content, under {COVER}",
                    BACKGROUND & 0xFF_FFFF,
                    content.0,
                    content.1
                ))
            }
        }
    };
    vec![("order", order), ("picture", picture)]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Both numbers are read from the launcher's lines; the break's line fails `order` naming it.
    #[test]
    fn order_reads_the_launchers_lines() {
        let app = "x splash: shown after=120 background=#FF206090\ny splash: replaced after=2400\n";
        let sub = checks(app, None, (1, 1));
        assert!(
            sub[0]
                .1
                .as_ref()
                .is_ok_and(|s| s.contains("120") && s.contains("2400"))
        );
        assert!(sub[1].1.is_err());
        let broken = checks("x splash: none (no spec)\n", None, (1, 1));
        assert!(broken[0].1.as_ref().is_err_and(|e| e.contains(NONE)));
        let reversed = "splash: shown after=9\nsplash: replaced after=3\n";
        assert!(checks(reversed, None, (1, 1))[0].1.is_err());
    }

    /// A picture whose whole content is the colour passes; a black one fails.
    #[test]
    fn picture_counts_the_colour() {
        let [_, r, g, b] = BACKGROUND.to_be_bytes();
        let image = |px: [u8; 4]| Image {
            width: 4,
            height: 4,
            rgba: px.repeat(16),
        };
        let lines = "splash: shown after=1\nsplash: replaced after=2\n";
        let ok = checks(
            lines,
            Some(Ok(Shot {
                image: image([r, g, b, 255]),
                late: false,
            })),
            (4, 4),
        );
        assert!(ok[1].1.is_ok());
        let black = checks(
            lines,
            Some(Ok(Shot {
                image: image([0, 0, 0, 255]),
                late: false,
            })),
            (4, 4),
        );
        assert!(black[1].1.is_err());
        let late = checks(
            lines,
            Some(Ok(Shot {
                image: image([r, g, b, 255]),
                late: true,
            })),
            (4, 4),
        );
        assert!(late[1].1.is_err());
    }
}
