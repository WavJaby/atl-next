// SPDX-License-Identifier: MIT
//! The default user agent (webview plan S5): the CDD 3.4.1 format CTS checks (WebSettingsTest.checkUserAgentStringHelper), every token from a real source.
//! `Mozilla/5.0 (Linux; Android <release>; <model> Build/<id>; wv) AppleWebKit/<M.m> (KHTML, like Gecko) Version/4.0 Chrome/<a.b.c.d>[ Mobile] Safari/<M.m>`
//! - release, model, id: `android.os.Build` (VERSION.RELEASE, MODEL, ID), the app's own view of the device.
//! - AppleWebKit, Safari: the engine's default UA tokens, cut to `<major>.<minor>`: CDD's form, and CTS requires `AppleWebKit/\d+\.\d+ ` (a space right after the minor), which WebKit's `605.1.15` fails.
//! - `Version/4.0`: CDD's literal (the CTS pattern quotes it).
//! - Chrome: [`CHROME_TOKEN`].
//! - ` Mobile`: omitted, the desktop form factor (CDD: phones add it, tablets do not).

/// What the engine's own UA is checked against.
#[derive(Debug, thiserror::Error, PartialEq)]
pub enum UaError {
    /// The engine's UA has no `<name>/<major>.<minor>` token.
    #[error("engine user agent {ua:?} has no {name}/<major>.<minor> token")]
    NoToken {
        /// `AppleWebKit` or `Safari`.
        name: &'static str,
        /// The engine's string.
        ua: String,
    },
}

/// The Chrome token (maintainer decision D4 open; plan S9 recommends including one): CDD/CTS require `Chrome/<a>.<b>.<c>.<d>` and no Chromium runs here, so no version is true.
/// 300.0.0.0 is the engine's own claim: WebKitGTK sends exactly this token to sites that need a Chrome UA (WebKit 2.54 Source/WebCore/platform/glib/UserAgentQuirks.cpp, `stringForQuirk(NeedsChromeBrowser)`); same engine, same claim.
pub const CHROME_TOKEN: &str = "Chrome/300.0.0.0";

/// The device fields the UA names (android.os.Build).
pub struct Device<'a> {
    /// `Build.VERSION.RELEASE`.
    pub release: &'a str,
    /// `Build.MODEL`.
    pub model: &'a str,
    /// `Build.ID`.
    pub id: &'a str,
}

/// `<name>/<major>.<minor>` from the first `<name>/<version>` token of `ua`.
fn engine_token(ua: &str, name: &'static str) -> Result<String, UaError> {
    let missing = || UaError::NoToken {
        name,
        ua: ua.to_owned(),
    };
    let version = ua
        .split(' ')
        .find_map(|t| t.strip_prefix(name)?.strip_prefix('/'))
        .ok_or_else(missing)?;
    let mut parts = version.split('.');
    let (major, minor) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    if !digits(major) || !digits(minor) {
        return Err(missing());
    }
    Ok(format!("{name}/{major}.{minor}"))
}

/// The provider's default user agent from the engine's own default UA and the device fields.
pub fn default_user_agent(engine: &str, d: &Device) -> Result<String, UaError> {
    let webkit = engine_token(engine, "AppleWebKit")?;
    let safari = engine_token(engine, "Safari")?;
    Ok(format!(
        "Mozilla/5.0 (Linux; Android {}; {} Build/{}; wv) {webkit} (KHTML, like Gecko) Version/4.0 {CHROME_TOKEN} {safari}",
        d.release, d.model, d.id
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// WebKitGTK 2.54's default, as `webkit_settings_get_user_agent` returned it (W0 and the W1c probe, measured).
    const WEBKIT_2_54: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/60.5 Safari/605.1.15";

    #[test]
    fn cdd_string_from_engine_and_device() {
        let d = Device {
            release: "16",
            model: "ATL",
            id: "BP2A.1",
        };
        // hand-assembled from the CDD format in the module doc
        assert_eq!(
            default_user_agent(WEBKIT_2_54, &d).unwrap(),
            "Mozilla/5.0 (Linux; Android 16; ATL Build/BP2A.1; wv) AppleWebKit/605.1 (KHTML, like Gecko) Version/4.0 Chrome/300.0.0.0 Safari/605.1"
        );
    }

    #[test]
    fn engine_without_tokens_is_refused() {
        let d = Device {
            release: "16",
            model: "ATL",
            id: "X",
        };
        assert_eq!(
            default_user_agent("Mozilla/5.0 Safari/605.1.15", &d),
            Err(UaError::NoToken {
                name: "AppleWebKit",
                ua: "Mozilla/5.0 Safari/605.1.15".into()
            })
        );
        assert!(default_user_agent("AppleWebKit/605 Safari/1.2", &d).is_err());
    }
}
