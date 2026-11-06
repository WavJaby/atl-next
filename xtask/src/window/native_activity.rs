// SPDX-License-Identifier: MIT
//! Stage `native-activity` of `xtask test window` (NA1): a pure native app (xtask/testdata/native-activity, `hasCode="false"`) whose only activity is the framework's `android.app.NativeActivity`,
//! its library named by the `android.app.lib_name` meta-data and built with NDK clang (as jni_fixture.rs builds its fixture). Headless, the pointer script injected (atld `--headless-pointer`).
//! Sub-gates ([`checks`]):
//! - lib: `ANativeActivity_onCreate` ran (its line), so libnativeloader found the library through the meta-data name (no `libmain.so` exists, NativeActivity.java's default).
//! - lifecycle: the callbacks in AOSP's order ([`LIFECYCLE`]), each once, none of pause / stop / destroy inside the window; focus gained after the input queue exists.
//! - picture: the first frame of the window's layer is the quadrants the fixture drew with `ANativeWindow_lock` (window.rs picture_check: digest computed here, not by the code under test).
//! - touch: the pointer script's touches arrive through `AInputQueue`, in order (window.rs input_check).
//! - controls: each of those checks rejects a broken copy of the real log ([`control_failures`]), so a pass means something.

use std::path::{Path, PathBuf};

use anyhow::Result;
use atl_services::display_model::DisplayModel;

use super::{POINTER_SCRIPT, Sub, input_check, picture_check};
use crate::host_art;
use crate::jni_fixture::{self, CFLAGS, NdkTarget};

/// The fixture's dir and its library source.
pub const FIXTURE: &str = "xtask/testdata/native-activity";
const SOURCE: &str = "jni/atl_native_activity.c";
/// `lib<android.app.lib_name>.so` (AndroidManifest.xml meta-data `atl-native-activity`).
const LIB: &str = "libatl-native-activity.so";
/// The manifest's minSdkVersion; the library is built for it.
const MIN_SDK: u32 = 21;
/// The library's arch: the host's own (NA1 is host-native; guest arches need the bridge's callback wrapping, NA3).
pub const ARCH: &str = "x86_64";
/// The marker: NativeActivity.onResume -> onResumeNative -> `callbacks.onResume` (android_app_NativeActivity.cpp).
pub const MARKER: &str = "AtlWindow: native onResume";
/// The layer name of the window: the activity's label (Activity.attach -> Window.setTitle; ViewRootImpl names its SurfaceControl after the title).
pub const TITLE: &str = "AtlNativeActivity";
/// The fixture's line prefix for a callback.
const CALLBACK: &str = "AtlWindow: native ";

/// The callbacks of a cold start, in AOSP's order, each from the code that calls it:
/// onCreate (NativeActivity.onCreate -> loadNativeCode -> ANativeActivity_onCreate), onStart, onResume (ActivityThread performStart / performResumeActivity),
/// onInputQueueCreated (handleResumeActivity -> addView -> ViewRootImpl.setView:1472, after onResume), onNativeWindowCreated (performTraversals:4073 surfaceCreated),
/// onContentRectChanged (performTraversals:4284 dispatchOnGlobalLayout -> NativeActivity.onGlobalLayout), onNativeWindowRedrawNeeded (performDraw, after layout).
pub const LIFECYCLE: [&str; 7] = [
    "onCreate",
    "onStart",
    "onResume",
    "onInputQueueCreated",
    "onNativeWindowCreated",
    "onContentRectChanged",
    "onNativeWindowRedrawNeeded",
];
/// Callbacks that end the activity's foreground: none may come inside the gate's window.
const ENDING: [&str; 3] = ["onPause", "onStop", "onDestroy"];
/// Allowed at most once, after onNativeWindowCreated: onSurfaceChanged_native compares the size with `lastWindowWidth`, which onSurfaceCreated_native never sets
/// (android_app_NativeActivity.cpp: assigned only on a window change inside onSurfaceChanged_native), so the first surfaceChanged after a create reports a resize unless the uninitialised field happens to match.
const FIRST_RESIZE: &str = "onNativeWindowResized";
/// The focus callback's line once focus is gained (InputChannel focus event -> ViewRootImpl.windowFocusChanged -> NativeActivity.onWindowFocusChanged).
const FOCUSED: &str = "onWindowFocusChanged focused=1";

/// Builds the library into `dir`; its path.
pub fn build_lib(root: &Path, ndk: &Path, dir: &Path) -> Result<(String, PathBuf)> {
    let t = NdkTarget::new(root, ndk, ARCH, MIN_SDK)?;
    if dir.exists() {
        std::fs::remove_dir_all(dir)?;
    }
    std::fs::create_dir_all(dir)?;
    let lib = dir.join(LIB);
    host_art::exec(
        t.cmd("clang")
            .arg("-shared")
            .args(CFLAGS)
            .arg("-std=c11")
            .arg(root.join(FIXTURE).join(SOURCE))
            .args(["-landroid", "-llog"])
            .arg(format!("-Wl,-soname,{LIB}"))
            .arg("-o")
            .arg(&lib),
        "NDK clang atl_native_activity.c",
    )?;
    Ok((t.abi.clone(), lib))
}

/// The default NDK (as `xtask test jni-fixture`).
pub fn default_ndk(root: &Path) -> Result<PathBuf> {
    jni_fixture::default_ndk(root)
}

/// The callback names the fixture logged, in order (the word after [`CALLBACK`]; `onWindowFocusChanged focused=N` keeps its argument).
fn callbacks(app: &str) -> Vec<&str> {
    app.lines()
        .filter_map(|l| l.split_once(CALLBACK).map(|(_, rest)| rest))
        .filter(|rest| rest.starts_with("on"))
        .map(|rest| {
            if rest.starts_with("onWindowFocusChanged") {
                rest.trim_end()
            } else {
                rest.split_whitespace().next().unwrap_or(rest)
            }
        })
        .collect()
}

fn lib_check(app: &str) -> Result<String, String> {
    let line = app
        .lines()
        .find(|l| l.contains("AtlWindow: native onCreate "))
        .ok_or("ANativeActivity_onCreate never ran: the library named by android.app.lib_name was not loaded")?;
    Ok(format!(
        "{LIB} loaded by its meta-data name, `{}`",
        line.split_once(CALLBACK).map_or(line, |(_, r)| r)
    ))
}

fn lifecycle_check(app: &str) -> Result<String, String> {
    let all = callbacks(app);
    let resizes: Vec<usize> = all
        .iter()
        .enumerate()
        .filter(|(_, c)| **c == FIRST_RESIZE)
        .map(|(i, _)| i)
        .collect();
    let created = all.iter().position(|c| *c == "onNativeWindowCreated");
    if resizes.len() > 1
        || resizes
            .first()
            .is_some_and(|r| created.is_none_or(|c| *r < c))
    {
        return Err(format!(
            "{FIRST_RESIZE} other than once after the create: {all:?}"
        ));
    }
    let got: Vec<&str> = all.into_iter().filter(|c| *c != FIRST_RESIZE).collect();
    let mut at = Vec::new();
    for name in LIFECYCLE {
        let n = got.iter().filter(|c| **c == name).count();
        if n != 1 {
            return Err(format!(
                "{name} called {n} times, once wanted; callbacks {got:?}"
            ));
        }
        at.push(got.iter().position(|c| *c == name).unwrap_or(usize::MAX));
    }
    if at.windows(2).any(|w| w[0] > w[1]) {
        return Err(format!("order {got:?}, AOSP's {LIFECYCLE:?}"));
    }
    if let Some(end) = got.iter().find(|c| ENDING.contains(c)) {
        return Err(format!("{end} inside the window: {got:?}"));
    }
    let queue = at[3];
    match got.iter().position(|c| *c == FOCUSED) {
        Some(f) if f > queue => {}
        Some(_) => return Err(format!("focus before the input queue: {got:?}")),
        None => return Err(format!("focus never gained: {got:?}")),
    }
    Ok(format!(
        "{} in AOSP's order, focus after the input queue",
        LIFECYCLE.join(" -> ")
    ))
}

/// The sub-gates on the app log, the controls last.
pub fn checks(app: &str, after: &[&str], display: DisplayModel) -> Vec<Sub> {
    vec![
        ("lib", lib_check(app)),
        ("lifecycle", lifecycle_check(app)),
        ("picture", picture_check(app, display, TITLE)),
        ("touch", input_check(after, POINTER_SCRIPT)),
        ("controls", controls(app, after, display)),
    ]
}

/// Each check applied to a broken copy of the real log must fail.
fn controls(app: &str, after: &[&str], display: DisplayModel) -> Result<String, String> {
    let f = control_failures(app, after, display);
    if f.is_empty() {
        Ok("4 broken logs rejected (callbacks swapped, onCreate dropped, digest changed, a touch dropped)".to_owned())
    } else {
        Err(f.join("; "))
    }
}

fn control_failures(app: &str, after: &[&str], display: DisplayModel) -> Vec<String> {
    let mut f = Vec::new();
    let mut check = |name: &str, rejected: bool| {
        if !rejected {
            f.push(format!("control {name} passed the gate"));
        }
    };
    // the window created before the input queue
    let swapped = app
        .replace("native onInputQueueCreated", "native SWAP")
        .replace("native onNativeWindowCreated", "native onInputQueueCreated")
        .replace("native SWAP", "native onNativeWindowCreated");
    check("callbacks swapped", lifecycle_check(&swapped).is_err());
    let no_create: String = app
        .lines()
        .filter(|l| !l.contains("native onCreate "))
        .map(|l| format!("{l}\n"))
        .collect();
    check(
        "onCreate dropped",
        lib_check(&no_create).is_err() && lifecycle_check(&no_create).is_err(),
    );
    let redigested: String = app
        .lines()
        .map(|l| {
            if l.starts_with("composer: quad ") && l.contains(&format!("name={TITLE} ")) {
                format!(
                    "{}digest=0000000000000000\n",
                    l.split("digest=").next().unwrap_or(l)
                )
            } else {
                format!("{l}\n")
            }
        })
        .collect();
    check(
        "digest changed",
        picture_check(&redigested, display, TITLE).is_err(),
    );
    let last_touch = after.iter().rposition(|l| l.contains("AtlWindow: touch "));
    let dropped: Vec<&str> = after
        .iter()
        .enumerate()
        .filter(|(i, _)| Some(*i) != last_touch)
        .map(|(_, l)| *l)
        .collect();
    check(
        "a touch dropped",
        input_check(&dropped, POINTER_SCRIPT).is_err(),
    );
    f
}

#[cfg(test)]
mod tests {
    use super::*;

    fn log(names: &[&str]) -> String {
        names
            .iter()
            .map(|n| format!("1.0 I AtlWindow: native {n}\n"))
            .collect()
    }

    /// AOSP's order with focus after the queue passes; the window before the queue, a missing callback, a pause inside the window, and focus before the queue fail.
    #[test]
    fn lifecycle_order() {
        let mut ok: Vec<&str> = LIFECYCLE.to_vec();
        ok.push(FOCUSED);
        assert!(lifecycle_check(&log(&ok)).is_ok());
        let mut swapped = ok.clone();
        swapped.swap(3, 4);
        assert!(lifecycle_check(&log(&swapped)).is_err());
        assert!(lifecycle_check(&log(&ok[1..])).is_err());
        let mut paused = ok.clone();
        paused.push("onPause");
        assert!(lifecycle_check(&log(&paused)).is_err());
        let mut early: Vec<&str> = LIFECYCLE[..3].to_vec();
        early.push(FOCUSED);
        early.extend_from_slice(&LIFECYCLE[3..]);
        assert!(lifecycle_check(&log(&early)).is_err());
    }

    /// Callback lines keep only the name, the focus line its argument; other fixture lines are not callbacks.
    #[test]
    fn callback_names() {
        let app = "1 I AtlWindow: native onCreate sdk=36 saved=0\n1 I AtlWindow: native drew size=2x2\n1 I AtlWindow: native onWindowFocusChanged focused=1\n";
        assert_eq!(callbacks(app), ["onCreate", FOCUSED]);
    }
}
