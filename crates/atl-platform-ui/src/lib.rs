// SPDX-License-Identifier: MIT
//! The platform layer's app-process facade: the `atl-platform-ui-api` traits and types, plus the implementation for the target OS as [`PlatformUi`], [`PlatformWindow`], [`PlatformTextInput`], [`PlatformAudioHost`] and [`PlatformPcmOutput`].
//! Core crates depend on this crate only, so the choice of OS is made here and nowhere else (plan S1); `deny.toml` keeps GTK out of every other crate.
//! On Linux the implementation is the platform module `libatl_platform_ui_linux.so` (launcher-ui-module plan S2/S3): `module` loads it `RTLD_NOW | RTLD_LOCAL` by file name from the runtime library path on first use, and the `Platform*` types are proxies over its `abi` vtable, so GTK, GLib and PipeWire never join the process's global symbol scope. A missing module or one of another build is `UiError::Unavailable` naming the file: the process runs without UI, as with no display.
//! The web engine is a second module, `libatl_webview_linux.so` ([`PlatformWebEngine`], webview plan S2.3), loaded the same way on the first WebView call only.
#![deny(unsafe_code)]

pub use atl_platform_ui_api::*;

#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
mod module;
#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
mod proxy;

#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
mod web_engine;

#[cfg(target_os = "linux")]
pub use web_engine::{PlatformWebEngine, PlatformWebView, probe_web_engine};

#[cfg(target_os = "linux")]
pub use proxy::{
    PlatformAudioHost, PlatformFrameRequester, PlatformInvoker, PlatformPcmOutput,
    PlatformTextInput, PlatformUi, PlatformWindow,
};

#[cfg(not(target_os = "linux"))]
compile_error!("atl-platform-ui has no implementation for this target OS");

/// Every implementation item satisfies its api trait, checked at compile time.
#[cfg(target_os = "linux")]
#[allow(dead_code)]
fn implements_the_api() {
    fn ui<T: Ui<Invoker = PlatformInvoker>>() {}
    fn invoker<T: DisplayInvoker>() {}
    fn window<
        T: Window<Ui = PlatformUi, TextInput = PlatformTextInput> + FrameClock + InputSource,
    >() {
    }
    fn text_input<T: TextInput>() {}
    fn audio_host<T: audio::AudioHost<Output = PlatformPcmOutput>>() {}
    fn pcm_output<T: audio::PcmOutput>() {}
    ui::<PlatformUi>();
    invoker::<PlatformInvoker>();
    window::<PlatformWindow>();
    text_input::<PlatformTextInput>();
    audio_host::<PlatformAudioHost>();
    pcm_output::<PlatformPcmOutput>();
    fn web_engine<T: webview::WebEngine>() {}
    web_engine::<PlatformWebEngine>();
}
