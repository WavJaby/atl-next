// SPDX-License-Identifier: MIT
//! The JNI exports: `JNI_OnLoad` and one `Java_*` per native method of the forwarders, each converting at once to safe values and calling the crate root.
//! Symbol names are JNI's short form (class + method; no overloads in these classes). Natives run on whatever thread the app called from.

use std::ffi::{CStr, CString, c_void};
use std::sync::OnceLock;

mod generated {
    #![allow(non_camel_case_types, non_snake_case, dead_code, clippy::all)]
    include!(concat!(env!("OUT_DIR"), "/jni.rs"));
}
use generated::JNINativeInterface;

type Env = *mut *const JNINativeInterface;
type Obj = *mut c_void;

macro_rules! jni_call {
    ($env:expr, $f:ident $(, $arg:expr)* $(,)?) => {{
        let env: Env = $env;
        // SAFETY: a JNIEnv ART passed to this native call; its table is ART's for the thread's life.
        let f = unsafe { (**env).$f }.expect(concat!("JNIEnv has no ", stringify!($f)));
        // SAFETY: the function's own contract, met by each caller (live local refs, NUL-terminated names).
        unsafe { f(env $(, $arg)*) }
    }};
}

/// `JNI_VERSION_1_6` (jni.h).
const JNI_VERSION_1_6: i32 = 0x0001_0006;

/// Installs this library's tracing subscriber (its own copy of `tracing`: the launcher's is not reachable): stderr, which atld points at the process log, as the launcher's trace.rs does.
#[unsafe(no_mangle)]
pub extern "C" fn JNI_OnLoad(_vm: *mut c_void, _reserved: *mut c_void) -> i32 {
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .with_target(false)
        .try_init();
    JNI_VERSION_1_6
}

fn pending(env: Env) -> bool {
    jni_call!(env, ExceptionCheck) != 0
}

/// Throws `class(message)`; the native then returns its default and ART raises it.
fn throw(env: Env, class: &CStr, message: &str) {
    let c = jni_call!(env, FindClass, class.as_ptr());
    if c.is_null() {
        return; // FindClass left its NoClassDefFoundError pending
    }
    let m = CString::new(message.replace('\0', " ")).unwrap_or_default();
    jni_call!(env, ThrowNew, c, m.as_ptr());
}

fn throw_uoe(env: Env, api: &str) {
    throw(
        env,
        c"java/lang/UnsupportedOperationException",
        crate::refuse(api),
    );
}

fn new_string(env: Env, s: &str) -> Obj {
    let c = CString::new(s.replace('\0', " ")).unwrap_or_default();
    jni_call!(env, NewStringUTF, c.as_ptr())
}

/// A static String field of `class`, or None (with an exception pending).
fn static_string(env: Env, class: &CStr, field: &CStr) -> Option<String> {
    let c = jni_call!(env, FindClass, class.as_ptr());
    if c.is_null() {
        return None;
    }
    let id = jni_call!(
        env,
        GetStaticFieldID,
        c,
        field.as_ptr(),
        c"Ljava/lang/String;".as_ptr()
    );
    if id.is_null() {
        return None;
    }
    let s = jni_call!(env, GetStaticObjectField, c, id);
    if s.is_null() || pending(env) {
        return None;
    }
    let chars = jni_call!(env, GetStringUTFChars, s, std::ptr::null_mut());
    if chars.is_null() {
        return None;
    }
    // SAFETY: GetStringUTFChars returned a NUL-terminated modified-UTF-8 buffer, valid until released below.
    let out = unsafe { CStr::from_ptr(chars) }
        .to_string_lossy()
        .into_owned();
    jni_call!(env, ReleaseStringUTFChars, s, chars);
    Some(out)
}

/// The one Statics instance (a global ref, kept for the process).
static STATICS: OnceLock<usize> = OnceLock::new();

#[unsafe(no_mangle)]
pub extern "C" fn Java_com_android_webview_chromium_WebViewChromiumFactoryProviderForT_getStatics(
    env: Env,
    _this: Obj,
) -> Obj {
    if let Some(&g) = STATICS.get() {
        return g as Obj;
    }
    // FindClass in a native resolves through the declaring class's loader: the provider APK's
    let c = jni_call!(
        env,
        FindClass,
        c"com/android/webview/chromium/WebViewChromiumStatics".as_ptr()
    );
    if c.is_null() {
        return std::ptr::null_mut();
    }
    let init = jni_call!(env, GetMethodID, c, c"<init>".as_ptr(), c"()V".as_ptr());
    if init.is_null() {
        return std::ptr::null_mut();
    }
    let obj = jni_call!(env, NewObjectA, c, init, std::ptr::null());
    if obj.is_null() {
        return std::ptr::null_mut();
    }
    let global = jni_call!(env, NewGlobalRef, obj) as usize;
    // a racing thread's instance wins; this one's global ref stays (one object, never freed: the process keeps its Statics)
    *STATICS.get_or_init(|| global) as Obj
}

#[unsafe(no_mangle)]
pub extern "C" fn Java_com_android_webview_chromium_WebViewChromiumStatics_getDefaultUserAgent(
    env: Env,
    _this: Obj,
    _context: Obj,
) -> Obj {
    let field = |class: &CStr, name: &CStr| static_string(env, class, name);
    let (Some(release), Some(model), Some(id)) = (
        field(c"android/os/Build$VERSION", c"RELEASE"),
        field(c"android/os/Build", c"MODEL"),
        field(c"android/os/Build", c"ID"),
    ) else {
        if !pending(env) {
            throw(
                env,
                c"java/lang/IllegalStateException",
                "android.os.Build field is null",
            );
        }
        return std::ptr::null_mut();
    };
    let device = crate::ua::Device {
        release: &release,
        model: &model,
        id: &id,
    };
    match crate::default_user_agent(&device) {
        Ok(ua) => new_string(env, &ua),
        Err(e) => {
            // the engine module is missing or broken: AndroidRuntimeException is what WebViewFactory raises for a provider that cannot load
            tracing::error!("webview: getDefaultUserAgent: {e}");
            throw(env, c"android/util/AndroidRuntimeException", &e);
            std::ptr::null_mut()
        }
    }
}

/// One refusing native per unimplemented method: `(name, interface.method, extra parameter types, return type)`; returns the type's default after throwing.
macro_rules! refuse {
    ($sym:ident, $api:literal, ($($p:ident: $t:ty),*) -> $ret:ty = $default:expr) => {
        #[unsafe(no_mangle)]
        pub extern "C" fn $sym(env: Env, _this: Obj $(, $p: $t)*) -> $ret {
            $(let _ = $p;)*
            throw_uoe(env, $api);
            $default
        }
    };
}

const NULL: Obj = std::ptr::null_mut();

refuse!(Java_com_android_webview_chromium_WebViewChromiumFactoryProviderForT_createWebView, "WebViewFactoryProvider.createWebView", (web_view: Obj, access: Obj) -> Obj = NULL);
refuse!(Java_com_android_webview_chromium_WebViewChromiumFactoryProviderForT_getGeolocationPermissions, "WebViewFactoryProvider.getGeolocationPermissions", () -> Obj = NULL);
refuse!(Java_com_android_webview_chromium_WebViewChromiumFactoryProviderForT_getCookieManager, "WebViewFactoryProvider.getCookieManager", () -> Obj = NULL);
refuse!(Java_com_android_webview_chromium_WebViewChromiumFactoryProviderForT_getTokenBindingService, "WebViewFactoryProvider.getTokenBindingService", () -> Obj = NULL);
refuse!(Java_com_android_webview_chromium_WebViewChromiumFactoryProviderForT_getTracingController, "WebViewFactoryProvider.getTracingController", () -> Obj = NULL);
refuse!(Java_com_android_webview_chromium_WebViewChromiumFactoryProviderForT_getServiceWorkerController, "WebViewFactoryProvider.getServiceWorkerController", () -> Obj = NULL);
refuse!(Java_com_android_webview_chromium_WebViewChromiumFactoryProviderForT_getWebIconDatabase, "WebViewFactoryProvider.getWebIconDatabase", () -> Obj = NULL);
refuse!(Java_com_android_webview_chromium_WebViewChromiumFactoryProviderForT_getWebStorage, "WebViewFactoryProvider.getWebStorage", () -> Obj = NULL);
refuse!(Java_com_android_webview_chromium_WebViewChromiumFactoryProviderForT_getWebViewDatabase, "WebViewFactoryProvider.getWebViewDatabase", (context: Obj) -> Obj = NULL);
refuse!(Java_com_android_webview_chromium_WebViewChromiumFactoryProviderForT_getWebViewClassLoader, "WebViewFactoryProvider.getWebViewClassLoader", () -> Obj = NULL);
refuse!(Java_com_android_webview_chromium_WebViewChromiumStatics_findAddress, "WebViewFactoryProvider.Statics.findAddress", (addr: Obj) -> Obj = NULL);
refuse!(Java_com_android_webview_chromium_WebViewChromiumStatics_freeMemoryForTests, "WebViewFactoryProvider.Statics.freeMemoryForTests", () -> () = ());
refuse!(Java_com_android_webview_chromium_WebViewChromiumStatics_setWebContentsDebuggingEnabled, "WebViewFactoryProvider.Statics.setWebContentsDebuggingEnabled", (enable: u8) -> () = ());
refuse!(Java_com_android_webview_chromium_WebViewChromiumStatics_clearClientCertPreferences, "WebViewFactoryProvider.Statics.clearClientCertPreferences", (on_cleared: Obj) -> () = ());
refuse!(Java_com_android_webview_chromium_WebViewChromiumStatics_enableSlowWholeDocumentDraw, "WebViewFactoryProvider.Statics.enableSlowWholeDocumentDraw", () -> () = ());
refuse!(Java_com_android_webview_chromium_WebViewChromiumStatics_parseFileChooserResult, "WebViewFactoryProvider.Statics.parseFileChooserResult", (result_code: i32, intent: Obj) -> Obj = NULL);
refuse!(Java_com_android_webview_chromium_WebViewChromiumStatics_initSafeBrowsing, "WebViewFactoryProvider.Statics.initSafeBrowsing", (context: Obj, callback: Obj) -> () = ());
refuse!(Java_com_android_webview_chromium_WebViewChromiumStatics_setSafeBrowsingWhitelist, "WebViewFactoryProvider.Statics.setSafeBrowsingWhitelist", (hosts: Obj, callback: Obj) -> () = ());
refuse!(Java_com_android_webview_chromium_WebViewChromiumStatics_getSafeBrowsingPrivacyPolicyUrl, "WebViewFactoryProvider.Statics.getSafeBrowsingPrivacyPolicyUrl", () -> Obj = NULL);
