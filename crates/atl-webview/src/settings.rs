// SPDX-License-Identifier: MIT
//! The WebSettings values of one WebView (webview plan S4.1: the core owns the defaults). One table row per property: AOSP's default, the WebSettings.java line it comes from, and the engine setting it maps to. A mapped setting is pushed to the view when it is set and when the view is made; the rest are stored and returned, and their engine effect is listed in docs/plan/atl-next/state/webview-w4a.md.

use atl_platform_ui::webview::{SettingValue, WebSetting};

/// What the app and the device give the defaults (WebSettings.java:426, :1022, :1061, :1500; webview plan S5).
pub struct AppDefaults {
    /// `ApplicationInfo.targetSdkVersion` of the app that made the WebView; `i32::MAX` when unknown (the newest rules).
    pub target_sdk: i32,
    /// The app holds android.permission.INTERNET.
    pub internet: bool,
    /// The device's default user agent (the CDD string of webview plan S5); empty when unknown, which is the engine's own.
    pub user_agent: String,
}

/// Declares [`Setting`]: `Name = default [=> WebSetting];` per property. The default is an expression over the app (`$app`); a row cites the WebSettings.java line it takes its default from, and `[infer]` marks a default that javadoc does not state.
macro_rules! settings_table {
    ($app:ident; $($name:ident = $default:expr $(=> $engine:ident)? ;)*) => {
        /// One WebSettings property of a WebView.
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub enum Setting {
            $($name,)*
        }

        impl Setting {
            /// Every setting in table order; `setting as usize` indexes [`Settings`].
            pub const ALL: &'static [Setting] = &[$(Setting::$name),*];

            /// AOSP's default for this setting in the app `$app`.
            pub fn default_for(self, $app: &AppDefaults) -> SettingValue {
                match self {
                    $(Setting::$name => SettingValue::from($default),)*
                }
            }

            /// The engine setting this one maps to; None when the engine has no equivalent (stored only).
            pub fn engine(self) -> Option<WebSetting> {
                match self {
                    $(Setting::$name => None $(.or(Some(WebSetting::$engine)))?,)*
                }
            }
        }
    };
}

settings_table! { app;
    // :1041 "The default is false" (setJavaScriptEnabled :1045)
    JavaScriptEnabled = false => JavaScript;
    // :1249
    DomStorageEnabled = false => DomStorage;
    // :304 (deprecated, hidden)
    NavDump = false;
    // :333
    SupportZoom = true;
    // :349
    MediaPlaybackRequiresUserGesture = true => MediaPlaybackRequiresUserGesture;
    // :368
    BuiltInZoomControls = false;
    // :394
    DisplayZoomControls = true;
    // :426-428: true for targetSdk up to Q (29), false from R (30)
    AllowFileAccess = app.target_sdk <= 29;
    // :442 "enabled"
    AllowContentAccess = true;
    // :458
    LoadWithOverviewMode = false;
    // :475
    EnableSmoothTransition = false;
    // :496 "Default is true"
    UseWebViewBackgroundForOverscrollBackground = true;
    // [infer] :520-548: deprecated since O, no default stated; true = the pre-O behaviour the note at :524 keeps
    SaveFormData = true;
    // :551
    SavePassword = true;
    // [infer] :668-682: obsolete, no default stated; the boolean zero
    LightTouchEnabled = false;
    // [infer] :748-768: no default stated; the boolean zero
    UseWideViewPort = false;
    // :777-778
    SupportMultipleWindows = false => SupportMultipleWindows;
    // :996
    BlockNetworkImage = false;
    // :1022-1024: false when the app holds INTERNET, else true
    BlockNetworkLoads = !app.internet;
    // :1061-1064: true for targetSdk up to ICE_CREAM_SANDWICH_MR1 (15), false from JELLY_BEAN (16)
    AllowUniversalAccessFromFileUrls = app.target_sdk <= 15 => AllowUniversalAccessFromFileUrls;
    // :1090-1093: the same targetSdk rule
    AllowFileAccessFromFileUrls = app.target_sdk <= 15 => AllowFileAccessFromFileUrls;
    // :1108
    PluginsEnabled = false;
    // :1231 (WebSQL, deprecated)
    DatabaseEnabled = false;
    // :1286
    GeolocationEnabled = true;
    // :1377-1378
    JavaScriptCanOpenWindowsAutomatically = false => JavaScriptCanOpenWindowsAutomatically;
    // :1454
    NeedInitialFocus = true;
    // :1561-1562
    OffscreenPreRaster = false;
    // :1591 "enabled by default for devices which support it": true; no Safe Browsing service on ATL (state note)
    SafeBrowsingEnabled = true;
    // :972
    LoadsImagesAutomatically = true => AutoLoadImages;
    // [infer] :1527-1555: @hide, no default stated; the boolean zero
    VideoOverlayForEmbeddedEncryptedVideoEnabled = false;
    // :568
    TextZoom = 100;
    // :717-718: the deprecated int code, 0 = the Android user agent
    UserAgentCode = 0;
    // :902
    MinimumFontSize = 8 => MinimumFontSize;
    // :918
    MinimumLogicalFontSize = 8;
    // :934
    DefaultFontSize = 16 => DefaultFontSize;
    // :950
    DefaultFixedFontSize = 16 => DefaultMonospaceFontSize;
    // LOAD_DEFAULT = -1 (:179), default at :1481
    CacheMode = -1;
    // :1500-1502: MIXED_CONTENT_ALWAYS_ALLOW = 0 (:238) below LOLLIPOP (21), MIXED_CONTENT_NEVER_ALLOW = 1 (:247) from it
    MixedContentMode = if app.target_sdk < 21 { 0 } else { 1 };
    // :1755, MENU_ITEM_NONE = 0 (:1766)
    DisabledActionModeMenuItems = 0;
    // :640 MEDIUM; ZoomDensity.getValue() (:114) is the stored int, Java converts
    DefaultZoom = 100;
    // :797-798 NARROW_COLUMNS = ordinal 2 (:74); Java converts
    LayoutAlgorithm = 2;
    // :1124-1125 OFF = ordinal 2 (:228); Java converts
    PluginState = 2;
    // :1462-1463 NORMAL = ordinal 0 (:211); Java converts
    RenderPriority = 0;
    // :1416-1419 null or empty = the system default (the device user agent, webview plan S5)
    UserAgentString = app.user_agent.as_str() => UserAgent;
    // :812
    StandardFontFamily = "sans-serif" => DefaultFontFamily;
    // :827
    FixedFontFamily = "monospace" => MonospaceFontFamily;
    // :842
    SansSerifFontFamily = "sans-serif" => SansSerifFontFamily;
    // :864 (the setter's javadoc at :857 says "sans-serif", a copy error: the getter and the family name say "serif")
    SerifFontFamily = "serif" => SerifFontFamily;
    // :872
    CursiveFontFamily = "cursive" => CursiveFontFamily;
    // :887
    FantasyFontFamily = "fantasy" => FantasyFontFamily;
    // [infer] :1264-1272: managed by the implementation, no default stated; the empty path
    DatabasePath = "";
    // :1399-1400
    DefaultTextEncodingName = "UTF-8" => DefaultCharset;
}

/// One WebView's settings: a value per [`Setting`], starting at the defaults of its app.
pub struct Settings {
    app: AppDefaults,
    values: Vec<SettingValue>,
}

impl Settings {
    /// The defaults for `app`.
    pub fn new(app: AppDefaults) -> Self {
        let values = Setting::ALL.iter().map(|s| s.default_for(&app)).collect();
        Self { app, values }
    }

    /// The stored value of `s`.
    pub fn get(&self, s: Setting) -> &SettingValue {
        &self.values[s as usize]
    }

    /// Stores `v` for `s`.
    pub fn set(&mut self, s: Setting, v: SettingValue) {
        self.values[s as usize] = v;
    }

    /// The boolean of `s`; the table's kind for it, so another kind is a broken row.
    pub fn flag(&self, s: Setting) -> bool {
        self.get(s)
            .as_bool()
            .expect("settings table: a boolean setting")
    }

    /// The int of `s`; the table's kind for it.
    pub fn int(&self, s: Setting) -> i32 {
        self.get(s)
            .as_int()
            .expect("settings table: an int setting")
    }

    /// The text of `s`; the table's kind for it.
    pub fn text(&self, s: Setting) -> &str {
        self.get(s)
            .as_str()
            .expect("settings table: a text setting")
    }

    /// `s`'s default for this app: what an empty text restores.
    pub fn default_text(&self, s: Setting) -> String {
        s.default_for(&self.app)
            .as_str()
            .unwrap_or_default()
            .to_owned()
    }

    /// Whether the app holds INTERNET (`setBlockNetworkLoads(false)` needs it, WebSettings.java:1019-1021).
    pub fn internet(&self) -> bool {
        self.app.internet
    }

    /// Every engine-backed setting with its value: what a new view takes.
    pub fn engine_values(&self) -> impl Iterator<Item = (WebSetting, &SettingValue)> + '_ {
        Setting::ALL
            .iter()
            .zip(&self.values)
            .filter_map(|(s, v)| Some((s.engine()?, v)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(target_sdk: i32, internet: bool) -> AppDefaults {
        AppDefaults {
            target_sdk,
            internet,
            user_agent: "device-ua".to_owned(),
        }
    }

    /// Expected values are the javadoc's literals (WebSettings.java lines in the table), not the code's output.
    #[test]
    fn defaults_are_aosp_javadoc() {
        let a = app(28, true);
        let d = |s: Setting| s.default_for(&a);
        assert_eq!(d(Setting::JavaScriptEnabled), SettingValue::Bool(false));
        assert_eq!(d(Setting::SupportZoom), SettingValue::Bool(true));
        assert_eq!(d(Setting::LoadWithOverviewMode), SettingValue::Bool(false));
        assert_eq!(d(Setting::AllowContentAccess), SettingValue::Bool(true));
        assert_eq!(
            d(Setting::LoadsImagesAutomatically),
            SettingValue::Bool(true)
        );
        assert_eq!(d(Setting::GeolocationEnabled), SettingValue::Bool(true));
        assert_eq!(
            d(Setting::JavaScriptCanOpenWindowsAutomatically),
            SettingValue::Bool(false)
        );
        assert_eq!(d(Setting::TextZoom), SettingValue::Int(100));
        assert_eq!(d(Setting::MinimumFontSize), SettingValue::Int(8));
        assert_eq!(d(Setting::DefaultFontSize), SettingValue::Int(16));
        assert_eq!(d(Setting::DefaultFixedFontSize), SettingValue::Int(16));
        assert_eq!(d(Setting::CacheMode), SettingValue::Int(-1));
        assert_eq!(
            d(Setting::DisabledActionModeMenuItems),
            SettingValue::Int(0)
        );
        assert_eq!(d(Setting::DefaultZoom), SettingValue::Int(100));
        assert_eq!(d(Setting::LayoutAlgorithm), SettingValue::Int(2));
        assert_eq!(d(Setting::PluginState), SettingValue::Int(2));
        assert_eq!(d(Setting::RenderPriority), SettingValue::Int(0));
        assert_eq!(
            d(Setting::StandardFontFamily),
            SettingValue::from("sans-serif")
        );
        assert_eq!(d(Setting::FixedFontFamily), SettingValue::from("monospace"));
        assert_eq!(d(Setting::SerifFontFamily), SettingValue::from("serif"));
        assert_eq!(d(Setting::CursiveFontFamily), SettingValue::from("cursive"));
        assert_eq!(d(Setting::FantasyFontFamily), SettingValue::from("fantasy"));
        assert_eq!(
            d(Setting::DefaultTextEncodingName),
            SettingValue::from("UTF-8")
        );
        assert_eq!(d(Setting::UserAgentString), SettingValue::from("device-ua"));
    }

    /// The rules that depend on the app (WebSettings.java:426, :1022, :1061, :1090, :1500): the boundary releases on each side.
    #[test]
    fn app_dependent_defaults_follow_target_sdk_and_permission() {
        let d = |s: Setting, a: &AppDefaults| s.default_for(a);
        // Q (29) and below: file access on; R (30): off
        assert_eq!(
            d(Setting::AllowFileAccess, &app(29, true)),
            SettingValue::Bool(true)
        );
        assert_eq!(
            d(Setting::AllowFileAccess, &app(30, true)),
            SettingValue::Bool(false)
        );
        // ICE_CREAM_SANDWICH_MR1 (15) and below: file URLs reach other origins; JELLY_BEAN (16): they do not
        assert_eq!(
            d(Setting::AllowUniversalAccessFromFileUrls, &app(15, true)),
            SettingValue::Bool(true)
        );
        assert_eq!(
            d(Setting::AllowUniversalAccessFromFileUrls, &app(16, true)),
            SettingValue::Bool(false)
        );
        assert_eq!(
            d(Setting::AllowFileAccessFromFileUrls, &app(15, true)),
            SettingValue::Bool(true)
        );
        assert_eq!(
            d(Setting::AllowFileAccessFromFileUrls, &app(16, true)),
            SettingValue::Bool(false)
        );
        // KITKAT (19) always allows mixed content; LOLLIPOP (21) never does
        assert_eq!(
            d(Setting::MixedContentMode, &app(19, true)),
            SettingValue::Int(0)
        );
        assert_eq!(
            d(Setting::MixedContentMode, &app(21, true)),
            SettingValue::Int(1)
        );
        // no INTERNET: network loads blocked by default
        assert_eq!(
            d(Setting::BlockNetworkLoads, &app(28, true)),
            SettingValue::Bool(false)
        );
        assert_eq!(
            d(Setting::BlockNetworkLoads, &app(28, false)),
            SettingValue::Bool(true)
        );
    }

    /// Every engine mapping has the kind of its setting's default, and no two settings share one WebKit setting; the list is the design's engine-backed set (18: 7 bool, 3 int, 8 text).
    #[test]
    fn engine_mapping_keeps_kinds_and_is_one_to_one() {
        let a = app(28, true);
        let mut seen = Vec::new();
        for s in Setting::ALL {
            if let Some(e) = s.engine() {
                assert_eq!(s.default_for(&a).kind(), e.kind(), "{s:?} -> {e:?}");
                assert!(!seen.contains(&e), "{e:?} mapped twice");
                seen.push(e);
            }
        }
        let engine = [
            WebSetting::JavaScript,
            WebSetting::DomStorage,
            WebSetting::MediaPlaybackRequiresUserGesture,
            WebSetting::AllowUniversalAccessFromFileUrls,
            WebSetting::AllowFileAccessFromFileUrls,
            WebSetting::JavaScriptCanOpenWindowsAutomatically,
            WebSetting::AutoLoadImages,
            WebSetting::MinimumFontSize,
            WebSetting::DefaultFontSize,
            WebSetting::DefaultMonospaceFontSize,
            WebSetting::UserAgent,
            WebSetting::DefaultFontFamily,
            WebSetting::MonospaceFontFamily,
            WebSetting::SansSerifFontFamily,
            WebSetting::SerifFontFamily,
            WebSetting::CursiveFontFamily,
            WebSetting::FantasyFontFamily,
            WebSetting::DefaultCharset,
            WebSetting::SupportMultipleWindows,
        ];
        assert_eq!(seen.len(), engine.len());
        for e in engine {
            assert!(seen.contains(&e), "{e:?} has no setting");
        }
    }

    /// The store starts at the defaults, keeps a set, and hands the engine its mapped settings only.
    #[test]
    fn store_starts_at_defaults_and_keeps_sets() {
        let mut s = Settings::new(app(28, true));
        assert_eq!(s.int(Setting::TextZoom), 100);
        assert_eq!(s.text(Setting::UserAgentString), "device-ua");
        s.set(Setting::TextZoom, SettingValue::Int(150));
        assert_eq!(s.int(Setting::TextZoom), 150);
        assert_eq!(s.default_text(Setting::UserAgentString), "device-ua");
        assert_eq!(s.engine_values().count(), 19);
        assert!(s.internet());
    }
}
