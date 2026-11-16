// SPDX-License-Identifier: MIT
//! The JNI exports: `JNI_OnLoad` and one `Java_*` per native method of the forwarders, each converting at once to safe values and calling safe code.
//! Symbol names are JNI's short form (class + method; no native overloads in these classes). Provider natives run on the UI thread (WebView's own thread check), which is the GTK main thread
//! where the engine's events arrive too; `Statics.getDefaultUserAgent` runs on whatever thread called; bridge calls run on the "JavaBridge" thread ([`bridge_thread`]).

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::ffi::{CStr, CString, c_void};
use std::rc::Rc;
use std::sync::OnceLock;
use std::sync::mpsc;

use atl_platform_ui::webview::{
    InputClaim, LoadError, PromptReply, SettingValue, WebEngine as _, WebEvent, WebView as _,
};
use atl_platform_ui::{PlatformWebEngine, PlatformWebView};

use crate::bridge;
use crate::settings::{AppDefaults, Setting, Settings};

mod generated {
    #![allow(non_camel_case_types, non_snake_case, dead_code, clippy::all)]
    include!(concat!(env!("OUT_DIR"), "/jni.rs"));
}
mod window {
    #![allow(non_camel_case_types, non_snake_case, dead_code, clippy::all)]
    include!(concat!(env!("OUT_DIR"), "/window.rs"));
}
use generated::{_jfieldID, _jmethodID, JNIInvokeInterface, JNINativeInterface, jvalue};

type Env = *mut *const JNINativeInterface;
type Vm = *mut *const JNIInvokeInterface;
type Obj = *mut c_void;
type Mid = *mut _jmethodID;

macro_rules! jni_call {
    ($env:expr, $f:ident $(, $arg:expr)* $(,)?) => {{
        let env: Env = $env;
        // SAFETY: a JNIEnv ART gave this thread (native call or attach); its table is ART's for the thread's life.
        let f = unsafe { (**env).$f }.expect(concat!("JNIEnv has no ", stringify!($f)));
        // SAFETY: the function's own contract, met by each caller (live refs, NUL-terminated names, signatures matching the arguments).
        unsafe { f(env $(, $arg)*) }
    }};
}

/// `JNI_VERSION_1_6` (jni.h).
const JNI_VERSION_1_6: i32 = 0x0001_0006;
const NULL: Obj = std::ptr::null_mut();

/// The process's JavaVM (JNI_OnLoad), for threads and callbacks with no JNIEnv in hand.
static VM: OnceLock<usize> = OnceLock::new();

/// Installs this library's tracing subscriber (its own copy of `tracing`: the launcher's is not reachable): stderr, which atld points at the process log, as the launcher's trace.rs does.
#[unsafe(no_mangle)]
pub extern "C" fn JNI_OnLoad(vm: *mut c_void, _reserved: *mut c_void) -> i32 {
    let _ = VM.set(vm as usize);
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .with_target(false)
        .try_init();
    JNI_VERSION_1_6
}

fn vm() -> Vm {
    *VM.get().expect("JNI_OnLoad ran before any native") as Vm
}

/// The calling thread's JNIEnv; the thread is attached (the UI thread is ART's main thread).
fn env_now() -> Option<Env> {
    let vm = vm();
    let mut env: *mut c_void = NULL;
    // SAFETY: ART's JavaVM, valid for the process; GetEnv writes the env or fails.
    let get = unsafe { (**vm).GetEnv }?;
    // SAFETY: as above.
    (unsafe { get(vm, &mut env, JNI_VERSION_1_6) } == 0).then_some(env.cast())
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
    let m = cstring(message);
    jni_call!(env, ThrowNew, c, m.as_ptr());
}

fn throw_uoe(env: Env, api: &str) {
    throw(
        env,
        c"java/lang/UnsupportedOperationException",
        crate::refuse(api),
    );
}

fn cstring(s: &str) -> CString {
    CString::new(s.replace('\0', " ")).unwrap_or_default()
}

fn new_string(env: Env, s: &str) -> Obj {
    let c = cstring(s);
    jni_call!(env, NewStringUTF, c.as_ptr())
}

/// A Java String's contents, None for null.
fn string(env: Env, s: Obj) -> Option<String> {
    if s.is_null() {
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

fn delete_local(env: Env, o: Obj) {
    if !o.is_null() {
        jni_call!(env, DeleteLocalRef, o);
    }
}

/// `obj`'s method `name` `sig`, or null with NoSuchMethodError pending.
fn method(env: Env, obj: Obj, name: &CStr, sig: &CStr) -> Mid {
    let c = jni_call!(env, GetObjectClass, obj);
    let m = jni_call!(env, GetMethodID, c, name.as_ptr(), sig.as_ptr());
    delete_local(env, c);
    m
}

fn call_void(env: Env, obj: Obj, name: &CStr, sig: &CStr, args: &[jvalue]) {
    let m = method(env, obj, name, sig);
    if !m.is_null() {
        jni_call!(env, CallVoidMethodA, obj, m, args.as_ptr());
    }
}

fn call_obj(env: Env, obj: Obj, name: &CStr, sig: &CStr, args: &[jvalue]) -> Obj {
    let m = method(env, obj, name, sig);
    if m.is_null() {
        return NULL;
    }
    jni_call!(env, CallObjectMethodA, obj, m, args.as_ptr())
}

fn call_bool(env: Env, obj: Obj, name: &CStr, sig: &CStr, args: &[jvalue]) -> bool {
    let m = method(env, obj, name, sig);
    !m.is_null() && jni_call!(env, CallBooleanMethodA, obj, m, args.as_ptr()) != 0
}

fn call_int(env: Env, obj: Obj, name: &CStr, sig: &CStr, args: &[jvalue]) -> i32 {
    let m = method(env, obj, name, sig);
    if m.is_null() {
        return 0;
    }
    jni_call!(env, CallIntMethodA, obj, m, args.as_ptr())
}

fn call_long(env: Env, obj: Obj, name: &CStr, sig: &CStr) -> i64 {
    let m = method(env, obj, name, sig);
    if m.is_null() {
        return 0;
    }
    jni_call!(env, CallLongMethodA, obj, m, std::ptr::null())
}

fn field(env: Env, obj: Obj, name: &CStr, sig: &CStr) -> *mut _jfieldID {
    let c = jni_call!(env, GetObjectClass, obj);
    let f = jni_call!(env, GetFieldID, c, name.as_ptr(), sig.as_ptr());
    delete_local(env, c);
    f
}

fn object_field(env: Env, obj: Obj, name: &CStr, sig: &CStr) -> Obj {
    let f = field(env, obj, name, sig);
    if f.is_null() {
        NULL
    } else {
        jni_call!(env, GetObjectField, obj, f)
    }
}

fn l(o: Obj) -> jvalue {
    jvalue { l: o }
}

/// A static String field of `class`, or None (with an exception pending).
fn static_string(env: Env, class: &CStr, name: &CStr) -> Option<String> {
    let c = jni_call!(env, FindClass, class.as_ptr());
    if c.is_null() {
        return None;
    }
    let id = jni_call!(
        env,
        GetStaticFieldID,
        c,
        name.as_ptr(),
        c"Ljava/lang/String;".as_ptr()
    );
    if id.is_null() {
        return None;
    }
    let s = jni_call!(env, GetStaticObjectField, c, id);
    if pending(env) {
        return None;
    }
    string(env, s)
}

/// An exception a callback left pending where no Java frame catches it (an engine event on the GLib loop): handed to the thread's uncaught-exception handler, as ART does for one escaping a Looper callback.
fn raise_uncaught(env: Env) {
    let t = jni_call!(env, ExceptionOccurred);
    if t.is_null() {
        return;
    }
    jni_call!(env, ExceptionClear);
    let thread_class = jni_call!(env, FindClass, c"java/lang/Thread".as_ptr());
    let current = jni_call!(
        env,
        GetStaticMethodID,
        thread_class,
        c"currentThread".as_ptr(),
        c"()Ljava/lang/Thread;".as_ptr()
    );
    let thread = jni_call!(
        env,
        CallStaticObjectMethodA,
        thread_class,
        current,
        std::ptr::null()
    );
    let handler = call_obj(
        env,
        thread,
        c"getUncaughtExceptionHandler",
        c"()Ljava/lang/Thread$UncaughtExceptionHandler;",
        &[],
    );
    if !handler.is_null() {
        call_void(
            env,
            handler,
            c"uncaughtException",
            c"(Ljava/lang/Thread;Ljava/lang/Throwable;)V",
            &[l(thread), l(t)],
        );
    }
}

// ---- Statics, refusals ----

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
        return NULL;
    }
    let init = jni_call!(env, GetMethodID, c, c"<init>".as_ptr(), c"()V".as_ptr());
    if init.is_null() {
        return NULL;
    }
    let obj = jni_call!(env, NewObjectA, c, init, std::ptr::null());
    if obj.is_null() {
        return NULL;
    }
    let global = jni_call!(env, NewGlobalRef, obj) as usize;
    // a racing thread's instance wins; this one's global ref stays (one object, never freed: the process keeps its Statics)
    *STATICS.get_or_init(|| global) as Obj
}

/// The CDD default UA from the app's Build fields and the engine ([`crate::default_user_agent`]).
fn device_user_agent(env: Env) -> Result<String, String> {
    let f = |class: &CStr, name: &CStr| static_string(env, class, name);
    let (Some(release), Some(model), Some(id)) = (
        f(c"android/os/Build$VERSION", c"RELEASE"),
        f(c"android/os/Build", c"MODEL"),
        f(c"android/os/Build", c"ID"),
    ) else {
        return Err("android.os.Build field is null".into());
    };
    crate::default_user_agent(&crate::ua::Device {
        release: &release,
        model: &model,
        id: &id,
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn Java_com_android_webview_chromium_WebViewChromiumStatics_getDefaultUserAgent(
    env: Env,
    _this: Obj,
    _context: Obj,
) -> Obj {
    match device_user_agent(env) {
        Ok(ua) => new_string(env, &ua),
        Err(e) => {
            if !pending(env) {
                // the engine module is missing or broken: AndroidRuntimeException is what WebViewFactory raises for a provider that cannot load
                tracing::error!("webview: getDefaultUserAgent: {e}");
                throw(env, c"android/util/AndroidRuntimeException", &e);
            }
            NULL
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn Java_com_android_webview_chromium_Unimplemented_refuse(
    env: Env,
    _class: Obj,
    api: Obj,
) -> Obj {
    let api = string(env, api).unwrap_or_default();
    let c = jni_call!(
        env,
        FindClass,
        c"java/lang/UnsupportedOperationException".as_ptr()
    );
    if c.is_null() {
        return NULL;
    }
    let init = jni_call!(
        env,
        GetMethodID,
        c,
        c"<init>".as_ptr(),
        c"(Ljava/lang/String;)V".as_ptr()
    );
    if init.is_null() {
        return NULL;
    }
    let msg = new_string(env, crate::refuse(&api));
    jni_call!(env, NewObjectA, c, init, [l(msg)].as_ptr())
}

#[unsafe(no_mangle)]
pub extern "C" fn Java_com_android_webview_chromium_Unimplemented_accept(
    env: Env,
    _class: Obj,
    api: Obj,
) {
    let api = string(env, api).unwrap_or_default();
    crate::refuse(&api);
}

// ---- one WebView ----

/// The native side of one `WebViewProviderImpl` (its `mNative`).
struct Native {
    view: PlatformWebView,
    /// The view's sideband stream as a `native_handle_t` (no fds, the stream's ints): leaked, because the carrier's BufferQueue keeps a non-owning NativeHandle on it
    /// (Surface::dispatchSetSidebandStream, `NativeHandle::create(h, false)`) that may outlive this provider; 20 bytes per WebView.
    sideband: &'static [i32],
    shared: Rc<Shared>,
    /// `addJavascriptInterface` objects: name, global ref, the `@JavascriptInterface` method names.
    interfaces: RefCell<Vec<(String, usize, Vec<String>)>>,
    /// The WebSettings values (settings.rs table): stored here, the engine's own pushed on set.
    settings: RefCell<Settings>,
    /// CookieManager.setAcceptThirdPartyCookies(this view) (= WebSettings.setAcceptThirdPartyCookies); counted in [`crate::cookies::third_party`] while on.
    third_party: Cell<bool>,
}

impl Native {
    fn set_third_party(&self, on: bool) {
        if self.third_party.replace(on) != on {
            crate::cookies::third_party(on);
        }
    }
}

impl Native {
    /// Stores `v` for `s` and pushes it to the view when the engine has the setting.
    fn put(&self, s: Setting, v: SettingValue) {
        if let Some(e) = s.engine() {
            self.view.set_setting(e, &v);
        }
        self.settings.borrow_mut().set(s, v);
    }

    fn flag(&self, s: Setting) -> bool {
        self.settings.borrow().flag(s)
    }

    fn int(&self, s: Setting) -> i32 {
        self.settings.borrow().int(s)
    }

    fn text(&self, s: Setting) -> String {
        self.settings.borrow().text(s).to_owned()
    }
}

/// What the event sink needs: the provider (a weak global ref, so the app's WebView stays collectable).
struct Shared {
    provider: Cell<usize>,
    /// Copies of the keys the page was given (dispatchKeyEvent's claim), oldest first: (stamp ns, global ref to the KeyEvent copy, action, keycode). An unhandled report names one by its stamp.
    keys: RefCell<VecDeque<(i64, usize, i32, i32)>>,
    /// Keycodes whose press the page left unhandled while their release had not reached the view yet: that release is reported unhandled when it comes.
    unhandled_presses: RefCell<Vec<i32>>,
}

/// Key copies a view keeps at most ([`Shared::keys`]): a report comes one engine round trip after its key (WebKitGTK: after the release), so a few keys are in flight; 32 is a fast typist's second of presses and releases.
/// A handled key is never reported, so the bound is also what evicts those.
const KEPT_KEYS: usize = 32;

/// KeyEvent.ACTION_DOWN / ACTION_UP.
const KEY_ACTION_DOWN: i32 = 0;
const KEY_ACTION_UP: i32 = 1;

/// `provider.mNative` as the view, None after destroy.
fn native<'a>(env: Env, provider: Obj) -> Option<&'a Native> {
    let f = field(env, provider, c"mNative", c"J");
    if f.is_null() {
        return None;
    }
    let p = jni_call!(env, GetLongField, provider, f) as usize as *const Native;
    // SAFETY: mNative is 0 or the Box<Native> createWebView leaked; only destroy (same UI thread) frees it, clearing the field first.
    unsafe { p.as_ref() }
}

/// The settings' provider's view.
fn settings_native<'a>(env: Env, settings: Obj) -> Option<&'a Native> {
    let p = object_field(
        env,
        settings,
        c"mProvider",
        c"Lcom/android/webview/chromium/WebViewProviderImpl;",
    );
    if p.is_null() { None } else { native(env, p) }
}

/// What the settings defaults depend on, from the WebView's app: `targetSdkVersion`, the INTERNET permission (WebSettings.java:426, :1022, :1061, :1500), and the device user agent (webview plan S5; empty when unknown).
fn app_defaults(env: Env, web_view: Obj) -> AppDefaults {
    let user_agent = device_user_agent(env).unwrap_or_else(|e| {
        tracing::warn!("webview: default user agent not set: {e}");
        String::new()
    });
    let ctx = call_obj(
        env,
        web_view,
        c"getContext",
        c"()Landroid/content/Context;",
        &[],
    );
    if ctx.is_null() {
        return AppDefaults {
            target_sdk: i32::MAX,
            internet: false,
            user_agent,
        };
    }
    let info = call_obj(
        env,
        ctx,
        c"getApplicationInfo",
        c"()Landroid/content/pm/ApplicationInfo;",
        &[],
    );
    let target_sdk = if info.is_null() {
        i32::MAX
    } else {
        let f = field(env, info, c"targetSdkVersion", c"I");
        jni_call!(env, GetIntField, info, f)
    };
    let permission = new_string(env, "android.permission.INTERNET");
    // PackageManager.PERMISSION_GRANTED
    let internet = call_int(
        env,
        ctx,
        c"checkSelfPermission",
        c"(Ljava/lang/String;)I",
        &[l(permission)],
    ) == 0;
    delete_local(env, permission);
    if !info.is_null() {
        delete_local(env, info);
    }
    delete_local(env, ctx);
    AppDefaults {
        target_sdk,
        internet,
        user_agent,
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn Java_com_android_webview_chromium_WebViewChromiumFactoryProviderForT_createWebView(
    env: Env,
    _this: Obj,
    web_view: Obj,
    access: Obj,
) -> Obj {
    let app = app_defaults(env, web_view);
    if pending(env) {
        return NULL;
    }
    let shared = Rc::new(Shared {
        provider: Cell::new(0),
        keys: RefCell::default(),
        unhandled_presses: RefCell::default(),
    });
    let sink_shared = shared.clone();
    let view = match PlatformWebEngine::create_view(Box::new(move |ev| on_event(&sink_shared, ev)))
    {
        Ok(v) => v,
        Err(e) => {
            tracing::error!("webview: createWebView: {e}");
            throw_uoe(env, "WebViewFactoryProvider.createWebView");
            return NULL;
        }
    };
    // CookieManager.setAcceptThirdPartyCookies javadoc: apps targeting KITKAT (19) or below allow them by default, LOLLIPOP (21) and later do not
    let third_party = app.target_sdk < 21;
    crate::cookies::start();
    // the defaults reach the engine before the page can load; the user agent is the device's string (S5)
    let settings = Settings::new(app);
    for (which, value) in settings.engine_values() {
        view.set_setting(which, value);
    }
    if pending(env) {
        return NULL;
    }
    let stream = view.sideband_stream();
    let handle: Vec<i32> = [
        std::mem::size_of::<window::native_handle>() as i32, // version: sizeof(native_handle_t), as native_handle_create sets it
        0,                                                   // numFds
        stream.len() as i32,                                 // numInts
    ]
    .into_iter()
    .chain(stream)
    .collect();
    let native = Box::into_raw(Box::new(Native {
        view,
        sideband: Box::leak(handle.into_boxed_slice()),
        shared: shared.clone(),
        interfaces: RefCell::default(),
        settings: RefCell::new(settings),
        third_party: Cell::new(false),
    }));
    // SAFETY: just made; the only reference.
    unsafe { &*native }.set_third_party(third_party);
    let c = jni_call!(
        env,
        FindClass,
        c"com/android/webview/chromium/WebViewProviderImpl".as_ptr()
    );
    let init = if c.is_null() {
        std::ptr::null_mut()
    } else {
        jni_call!(
            env,
            GetMethodID,
            c,
            c"<init>".as_ptr(),
            c"(JLandroid/webkit/WebView;Landroid/webkit/WebView$PrivateAccess;)V".as_ptr()
        )
    };
    let obj = if init.is_null() {
        NULL
    } else {
        jni_call!(
            env,
            NewObjectA,
            c,
            init,
            [jvalue { j: native as i64 }, l(web_view), l(access)].as_ptr()
        )
    };
    if obj.is_null() {
        // SAFETY: never handed to Java: this is its only owner
        let n = unsafe { Box::from_raw(native) };
        n.set_third_party(false);
        drop(n);
        return NULL;
    }
    shared
        .provider
        .set(jni_call!(env, NewWeakGlobalRef, obj) as usize);
    obj
}

/// One engine event on the UI thread, to the app's clients in the engine's order (onPageStarted first; title/progress as they come).
fn on_event(shared: &Shared, ev: WebEvent) {
    let Some(env) = env_now() else { return };
    if let WebEvent::Prompt { message, reply, .. } = ev {
        return on_prompt(env, shared, &message, reply);
    }
    if let WebEvent::UnhandledKey(t) = ev {
        return on_unhandled_key(env, shared, t);
    }
    let provider = jni_call!(env, NewLocalRef, shared.provider.get() as Obj);
    if provider.is_null() {
        return; // the app's WebView was collected
    }
    let web_view = object_field(env, provider, c"mWebView", c"Landroid/webkit/WebView;");
    let client = object_field(
        env,
        provider,
        c"mWebViewClient",
        c"Landroid/webkit/WebViewClient;",
    );
    let chrome = object_field(
        env,
        provider,
        c"mWebChromeClient",
        c"Landroid/webkit/WebChromeClient;",
    );
    match ev {
        WebEvent::Download {
            url,
            content_disposition,
            mime_type,
            content_length,
        } => on_download(
            env,
            provider,
            &[&url, &content_disposition, &mime_type],
            content_length,
        ),
        WebEvent::LoadStarted(url) if !client.is_null() => {
            let u = new_string(env, &url);
            call_void(
                env,
                client,
                c"onPageStarted",
                c"(Landroid/webkit/WebView;Ljava/lang/String;Landroid/graphics/Bitmap;)V",
                &[l(web_view), l(u), l(NULL)],
            );
        }
        WebEvent::LoadFinished(url) if !client.is_null() => {
            let u = new_string(env, &url);
            call_void(
                env,
                client,
                c"onPageFinished",
                c"(Landroid/webkit/WebView;Ljava/lang/String;)V",
                &[l(web_view), l(u)],
            );
        }
        // Chromium reports no error for an aborted load (a newer navigation replaced it)
        WebEvent::LoadFailed {
            kind: LoadError::Cancelled,
            ..
        } => {}
        WebEvent::LoadFailed { uri, message, kind } if !client.is_null() => {
            // WebViewClient.ERROR_* (WebViewClient.java)
            let code = match kind {
                LoadError::NotFound => -14, // ERROR_FILE_NOT_FOUND
                LoadError::Network => -6,   // ERROR_CONNECT
                _ => -1,                    // ERROR_UNKNOWN
            };
            let (d, u) = (new_string(env, &message), new_string(env, &uri));
            call_void(
                env,
                client,
                c"onReceivedError",
                c"(Landroid/webkit/WebView;ILjava/lang/String;Ljava/lang/String;)V",
                &[l(web_view), jvalue { i: code }, l(d), l(u)],
            );
        }
        WebEvent::Title(t) if !chrome.is_null() => {
            let s = new_string(env, &t);
            call_void(
                env,
                chrome,
                c"onReceivedTitle",
                c"(Landroid/webkit/WebView;Ljava/lang/String;)V",
                &[l(web_view), l(s)],
            );
        }
        WebEvent::Progress(p) if !chrome.is_null() => {
            call_void(
                env,
                chrome,
                c"onProgressChanged",
                c"(Landroid/webkit/WebView;I)V",
                &[l(web_view), jvalue { i: p }],
            );
        }
        _ => {}
    }
    raise_uncaught(env);
    for o in [provider, web_view, client, chrome] {
        delete_local(env, o);
    }
}

/// A navigation the engine handed over as a download: DownloadListener.onDownloadStart(url, userAgent, contentDisposition, mimetype, contentLength), the user agent being the view's
/// (WebSettings.getUserAgentString); without a listener nothing happens (the engine already dropped the navigation, the page stays).
fn on_download(env: Env, provider: Obj, [url, disposition, mime]: &[&str; 3], length: i64) {
    let listener = object_field(
        env,
        provider,
        c"mDownloadListener",
        c"Landroid/webkit/DownloadListener;",
    );
    if listener.is_null() {
        tracing::info!("webview: download {url} ({mime}) with no DownloadListener: dropped");
        return;
    }
    let ua = native(env, provider)
        .map(|n| n.text(Setting::UserAgentString))
        .unwrap_or_default();
    let args: Vec<Obj> = [*url, &ua, *disposition, *mime]
        .iter()
        .map(|s| new_string(env, s))
        .collect();
    call_void(
        env,
        listener,
        c"onDownloadStart",
        c"(Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;J)V",
        &[
            l(args[0]),
            l(args[1]),
            l(args[2]),
            l(args[3]),
            jvalue { j: length },
        ],
    );
    for o in args.into_iter().chain([listener]) {
        delete_local(env, o);
    }
}

/// A page's `prompt`: a bridge call goes to the JavaBridge thread with its object; any other prompt answers null (WebChromeClient.onJsPrompt dispatch is a later slice, counted).
fn on_prompt(env: Env, shared: &Shared, message: &str, reply: Box<dyn PromptReply>) {
    let Some(call) = bridge::parse(message) else {
        crate::refuse("WebChromeClient.onJsPrompt");
        return reply.finish(None);
    };
    let provider = jni_call!(env, NewLocalRef, shared.provider.get() as Obj);
    let object = (!provider.is_null())
        .then(|| native(env, provider))
        .flatten()
        .and_then(|n| {
            n.interfaces
                .borrow()
                .iter()
                .find(|(name, ..)| *name == call.object)
                .map(|(_, g, _)| *g as Obj)
        })
        .map(|g| jni_call!(env, NewGlobalRef, g) as usize);
    delete_local(env, provider);
    let Some(object) = object else {
        // removed since the page was loaded: Chromium answers as for a missing method
        return reply.finish(Some(bridge::reply_error(bridge::JAVA_EXCEPTION)));
    };
    let job = Job {
        object,
        call,
        reply,
    };
    if let Err(mpsc::SendError(job)) = bridge_thread().send(job) {
        job.reply
            .finish(Some(bridge::reply_error(bridge::JAVA_EXCEPTION)));
    }
}

/// One bridge call for the JavaBridge thread; `object` is a global ref it deletes.
struct Job {
    object: usize,
    call: bridge::Call,
    reply: Box<dyn PromptReply>,
}

/// The JavaBridge thread (Chromium runs injected-object methods on such a background thread, not the UI thread), started on first use.
fn bridge_thread() -> &'static mpsc::Sender<Job> {
    static TX: OnceLock<mpsc::Sender<Job>> = OnceLock::new();
    TX.get_or_init(|| {
        let (tx, rx) = mpsc::channel::<Job>();
        let spawned = std::thread::Builder::new()
            .name("JavaBridge".into())
            .spawn(move || {
                let vm = vm();
                let mut env: *mut *const JNINativeInterface = std::ptr::null_mut();
                // SAFETY: ART's JavaVM; a daemon attach so the bridge never holds the process open.
                let attached = unsafe { (**vm).AttachCurrentThreadAsDaemon }.is_some_and(
                    |attach| unsafe { attach(vm, &mut env, std::ptr::null_mut()) } == 0,
                );
                if !attached {
                    tracing::error!("webview: JavaBridge thread cannot attach to the VM");
                    return;
                }
                for job in rx {
                    let answer = invoke(env, job.object as Obj, &job.call);
                    jni_call!(env, DeleteGlobalRef, job.object as Obj);
                    job.reply.finish(Some(answer));
                }
            });
        if let Err(e) = spawned {
            tracing::error!("webview: JavaBridge thread: {e}");
        }
        tx
    })
}

/// Runs `call` on `object` (reflection: Chromium's rules, [`bridge`]) and returns the prompt answer.
fn invoke(env: Env, object: Obj, call: &bridge::Call) -> String {
    jni_call!(env, PushLocalFrame, 64);
    let answer = invoke_in_frame(env, object, call).unwrap_or_else(|| {
        jni_call!(env, ExceptionClear);
        bridge::reply_error(bridge::JAVA_EXCEPTION)
    });
    jni_call!(env, PopLocalFrame, NULL);
    answer
}

fn invoke_in_frame(env: Env, object: Obj, call: &bridge::Call) -> Option<String> {
    let m = find_method(env, object, &call.method, Some(call.args.len()))?;
    let params = call_obj(env, m, c"getParameterTypes", c"()[Ljava/lang/Class;", &[]);
    let object_class = jni_call!(env, FindClass, c"java/lang/Object".as_ptr());
    let args = jni_call!(
        env,
        NewObjectArray,
        call.args.len() as i32,
        object_class,
        NULL
    );
    for (i, v) in call.args.iter().enumerate() {
        let ty = jni_call!(env, GetObjectArrayElement, params, i as i32);
        let name = string(
            env,
            call_obj(env, ty, c"getName", c"()Ljava/lang/String;", &[]),
        )?;
        let boxed = box_arg(env, bridge::convert(&name, v));
        jni_call!(env, SetObjectArrayElement, args, i as i32, boxed);
    }
    let ret = call_obj(
        env,
        m,
        c"invoke",
        c"(Ljava/lang/Object;[Ljava/lang/Object;)Ljava/lang/Object;",
        &[l(object), l(args)],
    );
    if pending(env) {
        return None;
    }
    let ret_type = call_obj(env, m, c"getReturnType", c"()Ljava/lang/Class;", &[]);
    let ret_name = string(
        env,
        call_obj(env, ret_type, c"getName", c"()Ljava/lang/String;", &[]),
    )?;
    if ret_name == "void" {
        return Some(bridge::reply_value(None));
    }
    if ret.is_null() {
        return Some(bridge::reply_value(Some("null".into())));
    }
    let class = call_obj(env, ret, c"getClass", c"()Ljava/lang/Class;", &[]);
    let class = string(
        env,
        call_obj(env, class, c"getName", c"()Ljava/lang/String;", &[]),
    )?;
    let text = string(
        env,
        call_obj(env, ret, c"toString", c"()Ljava/lang/String;", &[]),
    )?;
    Some(bridge::reply_value(bridge::result_json(&class, &text)))
}

/// The first public `@JavascriptInterface` method of `object` named `name` (with `arity` parameters when given), as a java.lang.reflect.Method local ref.
fn find_method(env: Env, object: Obj, name: &str, arity: Option<usize>) -> Option<Obj> {
    let annotation = jni_call!(
        env,
        FindClass,
        c"android/webkit/JavascriptInterface".as_ptr()
    );
    let class = call_obj(env, object, c"getClass", c"()Ljava/lang/Class;", &[]);
    let methods = call_obj(
        env,
        class,
        c"getMethods",
        c"()[Ljava/lang/reflect/Method;",
        &[],
    );
    if annotation.is_null() || methods.is_null() {
        return None;
    }
    let n = jni_call!(env, GetArrayLength, methods);
    (0..n).find_map(|i| {
        let m = jni_call!(env, GetObjectArrayElement, methods, i);
        let ok = string(
            env,
            call_obj(env, m, c"getName", c"()Ljava/lang/String;", &[]),
        )
        .as_deref()
            == Some(name)
            && call_bool(
                env,
                m,
                c"isAnnotationPresent",
                c"(Ljava/lang/Class;)Z",
                &[l(annotation)],
            )
            && arity.is_none_or(|a| {
                let p = call_obj(env, m, c"getParameterTypes", c"()[Ljava/lang/Class;", &[]);
                jni_call!(env, GetArrayLength, p) as usize == a
            });
        if ok {
            Some(m)
        } else {
            delete_local(env, m);
            None
        }
    })
}

/// The names of `object`'s public `@JavascriptInterface` methods (the only ones a page may call, since API 17).
fn interface_methods(env: Env, object: Obj) -> Vec<String> {
    jni_call!(env, PushLocalFrame, 64);
    let annotation = jni_call!(
        env,
        FindClass,
        c"android/webkit/JavascriptInterface".as_ptr()
    );
    let class = call_obj(env, object, c"getClass", c"()Ljava/lang/Class;", &[]);
    let methods = call_obj(
        env,
        class,
        c"getMethods",
        c"()[Ljava/lang/reflect/Method;",
        &[],
    );
    let mut names = Vec::new();
    if !annotation.is_null() && !methods.is_null() {
        for i in 0..jni_call!(env, GetArrayLength, methods) {
            let m = jni_call!(env, GetObjectArrayElement, methods, i);
            if call_bool(
                env,
                m,
                c"isAnnotationPresent",
                c"(Ljava/lang/Class;)Z",
                &[l(annotation)],
            ) && let Some(n) = string(
                env,
                call_obj(env, m, c"getName", c"()Ljava/lang/String;", &[]),
            ) && !names.contains(&n)
            {
                names.push(n);
            }
            delete_local(env, m);
        }
    }
    jni_call!(env, PopLocalFrame, NULL);
    names
}

/// A converted argument boxed for Method.invoke.
fn box_arg(env: Env, a: bridge::Arg) -> Obj {
    use bridge::Arg;
    let boxed = |class: &CStr, sig: &CStr, v: jvalue| {
        let c = jni_call!(env, FindClass, class.as_ptr());
        let m = jni_call!(env, GetStaticMethodID, c, c"valueOf".as_ptr(), sig.as_ptr());
        jni_call!(env, CallStaticObjectMethodA, c, m, [v].as_ptr())
    };
    match a {
        Arg::Int(i) => boxed(
            c"java/lang/Integer",
            c"(I)Ljava/lang/Integer;",
            jvalue { i },
        ),
        Arg::Long(j) => boxed(c"java/lang/Long", c"(J)Ljava/lang/Long;", jvalue { j }),
        Arg::Short(s) => boxed(c"java/lang/Short", c"(S)Ljava/lang/Short;", jvalue { s }),
        Arg::Byte(b) => boxed(c"java/lang/Byte", c"(B)Ljava/lang/Byte;", jvalue { b }),
        Arg::Char(c) => boxed(
            c"java/lang/Character",
            c"(C)Ljava/lang/Character;",
            jvalue { c },
        ),
        Arg::Float(f) => boxed(c"java/lang/Float", c"(F)Ljava/lang/Float;", jvalue { f }),
        Arg::Double(d) => boxed(c"java/lang/Double", c"(D)Ljava/lang/Double;", jvalue { d }),
        Arg::Bool(z) => boxed(
            c"java/lang/Boolean",
            c"(Z)Ljava/lang/Boolean;",
            jvalue { z: z as u8 },
        ),
        Arg::Str(Some(s)) => new_string(env, &s),
        Arg::Str(None) | Arg::Null => NULL,
    }
}

/// Rebuilds the document-start scripts from the interfaces (takes effect on the next load, as Android documents for addJavascriptInterface).
fn refresh_scripts(n: &Native) {
    let scripts: Vec<String> = n
        .interfaces
        .borrow()
        .iter()
        .map(|(name, _, ms)| bridge::script(name, ms))
        .collect();
    n.view.set_user_scripts(&scripts);
}

fn add_interface(env: Env, n: &Native, object: Obj, name: &str) {
    let methods = interface_methods(env, object);
    let global = jni_call!(env, NewGlobalRef, object) as usize;
    let mut list = n.interfaces.borrow_mut();
    if let Some(i) = list.iter().position(|(nm, ..)| nm == name) {
        let (_, old, _) = list.remove(i);
        jni_call!(env, DeleteGlobalRef, old as Obj);
    }
    list.push((name.to_owned(), global, methods));
    drop(list);
    refresh_scripts(n);
}

// ---- the carrier layer (webview plan S2.2 amended) ----

/// A new object of `class` through constructor `sig` (private ones too), or null with an exception pending.
fn new_object(env: Env, class: &CStr, sig: &CStr, args: &[jvalue]) -> Obj {
    let c = jni_call!(env, FindClass, class.as_ptr());
    if c.is_null() {
        return NULL;
    }
    let init = jni_call!(env, GetMethodID, c, c"<init>".as_ptr(), sig.as_ptr());
    let o = if init.is_null() {
        NULL
    } else {
        jni_call!(env, NewObjectA, c, init, args.as_ptr())
    };
    delete_local(env, c);
    o
}

/// Adds the carrier of `provider`'s view: a SurfaceView filling the WebView (WebView is an AbsoluteLayout), whose holder's LayerCallback sets the view's sideband stream on each new Surface.
/// SurfaceView brings the rest: its layer below the window, the hole hwui leaves there, the transparent region, the layer following the view's position.
fn add_carrier(env: Env, provider: Obj) {
    jni_call!(env, PushLocalFrame, 16);
    let web_view = object_field(env, provider, c"mWebView", c"Landroid/webkit/WebView;");
    let context = call_obj(
        env,
        web_view,
        c"getContext",
        c"()Landroid/content/Context;",
        &[],
    );
    let carrier = new_object(
        env,
        c"android/view/SurfaceView",
        c"(Landroid/content/Context;)V",
        &[l(context)],
    );
    let callback = new_object(
        env,
        c"com/android/webview/chromium/LayerCallback",
        c"(Lcom/android/webview/chromium/WebViewProviderImpl;)V",
        &[l(provider)],
    );
    // ViewGroup.LayoutParams.MATCH_PARENT
    const MATCH_PARENT: i32 = -1;
    let params = new_object(
        env,
        c"android/view/ViewGroup$LayoutParams",
        c"(II)V",
        &[jvalue { i: MATCH_PARENT }, jvalue { i: MATCH_PARENT }],
    );
    if !pending(env) && !carrier.is_null() && !callback.is_null() && !params.is_null() {
        let holder = call_obj(
            env,
            carrier,
            c"getHolder",
            c"()Landroid/view/SurfaceHolder;",
            &[],
        );
        call_void(
            env,
            holder,
            c"addCallback",
            c"(Landroid/view/SurfaceHolder$Callback;)V",
            &[l(callback)],
        );
        call_void(
            env,
            web_view,
            c"addView",
            c"(Landroid/view/View;Landroid/view/ViewGroup$LayoutParams;)V",
            &[l(carrier), l(params)],
        );
    }
    jni_call!(env, PopLocalFrame, NULL);
}

/// libandroid's `ANativeWindow_fromSurface` (NDK: the Surface's window, one reference taken), looked up once.
fn window_from_surface() -> Result<WindowFromSurface, String> {
    static F: OnceLock<Result<usize, String>> = OnceLock::new();
    let f = F
        .get_or_init(|| {
            // SAFETY: dlopen/dlsym with NUL-terminated names; libandroid.so is on every app's library path (the runtime's LD_LIBRARY_PATH) and stays loaded (no dlclose).
            unsafe {
                let lib = libc::dlopen(c"libandroid.so".as_ptr(), libc::RTLD_NOW);
                if lib.is_null() {
                    return Err("dlopen libandroid.so failed".into());
                }
                let sym = libc::dlsym(lib, c"ANativeWindow_fromSurface".as_ptr());
                if sym.is_null() {
                    return Err("libandroid.so has no ANativeWindow_fromSurface".into());
                }
                Ok(sym as usize)
            }
        })
        .clone()?;
    // SAFETY: the NDK's prototype `ANativeWindow* ANativeWindow_fromSurface(JNIEnv*, jobject)` (android/native_window_jni.h).
    Ok(unsafe { std::mem::transmute::<usize, WindowFromSurface>(f) })
}

type WindowFromSurface = unsafe extern "C" fn(Env, Obj) -> *mut window::ANativeWindow;

/// `native_window_set_sideband_stream(window(surface), handle)` (system/window.h): libgui's Surface hands it to BLASTBufferQueue, which sets it on the carrier's layer (eSidebandStreamChanged).
fn set_sideband(env: Env, surface: Obj, handle: &'static [i32]) -> Result<(), String> {
    if surface.is_null() {
        return Err("no Surface".into());
    }
    let from_surface = window_from_surface()?;
    // SAFETY: a live Surface local ref on a thread with this env.
    let w = unsafe { from_surface(env, surface) };
    if w.is_null() {
        return Err("ANativeWindow_fromSurface returned null".into());
    }
    // SAFETY: `w` is a live window holding the reference fromSurface took; `perform` is variadic and this op's one argument is a `native_handle_t*` (window.h), whose memory is 'static.
    // decRef is ANativeWindow_release's call (the reference fromSurface took).
    let r = unsafe {
        let r = match (*w).perform {
            Some(perform) => perform(
                w,
                window::NATIVE_WINDOW_SET_SIDEBAND_STREAM as i32,
                handle.as_ptr(),
            ),
            None => i32::MIN,
        };
        if let Some(dec) = (*w).common.decRef {
            dec(&raw mut (*w).common);
        }
        r
    };
    match r {
        0 => Ok(()),
        i32::MIN => Err("the window has no perform hook".into()),
        e => Err(format!("NATIVE_WINDOW_SET_SIDEBAND_STREAM: {e}")),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn Java_com_android_webview_chromium_LayerCallback_surfaceCreated(
    env: Env,
    this: Obj,
    holder: Obj,
) {
    let provider = object_field(
        env,
        this,
        c"mProvider",
        c"Lcom/android/webview/chromium/WebViewProviderImpl;",
    );
    let Some(n) = (!provider.is_null())
        .then(|| native(env, provider))
        .flatten()
    else {
        return; // destroyed: the carrier shows nothing
    };
    let surface = call_obj(env, holder, c"getSurface", c"()Landroid/view/Surface;", &[]);
    match set_sideband(env, surface, n.sideband) {
        Ok(()) => tracing::info!("webview: carrier stream set {:?}", &n.sideband[3..]),
        Err(e) => tracing::error!("webview: carrier stream not set: {e}"),
    }
    delete_local(env, surface);
    delete_local(env, provider);
}

/// Declares one provider native whose body gets `(env, &Native, args...)`; after destroy it returns `$default` (Chromium logs and ignores calls on a destroyed WebView).
macro_rules! provider_native {
    ($sym:ident, ($env:ident, $n:ident $(, $p:ident: $t:ty)*) -> $ret:ty = $default:expr, $body:block) => {
        #[allow(clippy::unused_unit)] // `-> ()` keeps one macro shape for void and valued natives
        #[unsafe(no_mangle)]
        pub extern "C" fn $sym($env: Env, this: Obj $(, $p: $t)*) -> $ret {
            let Some($n) = native($env, this) else { return $default };
            $body
        }
    };
}

provider_native!(Java_com_android_webview_chromium_WebViewProviderImpl_init, (env, n, interfaces: Obj, private_browsing: u8) -> () = (), {
    if private_browsing != 0 {
        // Chromium's WebViewChromium.init for apps targeting KitKat and later
        return throw(env, c"java/lang/IllegalArgumentException", "Private browsing is not supported in WebView.");
    }
    let provider = jni_call!(env, NewLocalRef, n.shared.provider.get() as Obj);
    add_carrier(env, provider);
    delete_local(env, provider);
    if pending(env) {
        return;
    }
    if interfaces.is_null() {
        return;
    }
    let entries = call_obj(env, interfaces, c"entrySet", c"()Ljava/util/Set;", &[]);
    let it = call_obj(env, entries, c"iterator", c"()Ljava/util/Iterator;", &[]);
    while call_bool(env, it, c"hasNext", c"()Z", &[]) {
        let e = call_obj(env, it, c"next", c"()Ljava/lang/Object;", &[]);
        let k = call_obj(env, e, c"getKey", c"()Ljava/lang/Object;", &[]);
        let v = call_obj(env, e, c"getValue", c"()Ljava/lang/Object;", &[]);
        if let Some(name) = string(env, k) && !v.is_null() {
            add_interface(env, n, v, &name);
        }
        for o in [e, k, v] {
            delete_local(env, o);
        }
    }
});

#[unsafe(no_mangle)]
pub extern "C" fn Java_com_android_webview_chromium_WebViewProviderImpl_destroy(
    env: Env,
    this: Obj,
) {
    let f = field(env, this, c"mNative", c"J");
    if f.is_null() {
        return;
    }
    let p = jni_call!(env, GetLongField, this, f) as usize as *mut Native;
    if p.is_null() {
        return;
    }
    jni_call!(env, SetLongField, this, f, 0);
    // SAFETY: the Box createWebView leaked; the field is cleared first, so no native reaches it again.
    let n = unsafe { Box::from_raw(p) };
    n.set_third_party(false);
    for (_, g, _) in n.interfaces.borrow_mut().drain(..) {
        jni_call!(env, DeleteGlobalRef, g as Obj);
    }
    jni_call!(
        env,
        DeleteWeakGlobalRef,
        n.shared.provider.replace(0) as Obj
    );
    drop(n);
}

provider_native!(Java_com_android_webview_chromium_WebViewProviderImpl_loadUrl, (env, n, url: Obj, headers: Obj) -> () = (), {
    if !headers.is_null() && !call_bool(env, headers, c"isEmpty", c"()Z", &[]) {
        return throw_uoe(env, "WebViewProvider.loadUrl(additionalHttpHeaders)");
    }
    match string(env, url) {
        Some(u) => n.view.load_uri(&u),
        None => tracing::warn!("webview: loadUrl(null) ignored"),
    }
});

provider_native!(Java_com_android_webview_chromium_WebViewProviderImpl_loadData, (env, n, data: Obj, mime: Obj, encoding: Obj) -> () = (), {
    let data = string(env, data).unwrap_or_default();
    n.view.load_uri(&bridge::data_url(&data, string(env, mime).as_deref(), string(env, encoding).as_deref()));
});

provider_native!(Java_com_android_webview_chromium_WebViewProviderImpl_loadDataWithBaseURL, (env, n, base: Obj, data: Obj, mime: Obj, encoding: Obj, history: Obj) -> () = (), {
    let data = string(env, data).unwrap_or_default();
    let mime = string(env, mime);
    let encoding = string(env, encoding);
    let base = string(env, base).filter(|b| !b.is_empty());
    if !history.is_null() {
        crate::refuse("WebViewProvider.loadDataWithBaseURL(historyUrl)");
    }
    match base {
        // a data: base makes it loadData (Chromium's AwContents.loadDataWithBaseURL)
        Some(b) if b.starts_with("data:") => n.view.load_uri(&bridge::data_url(&data, mime.as_deref(), encoding.as_deref())),
        b => n.view.load_bytes(
            data.as_bytes(),
            mime.as_deref().filter(|m| !m.is_empty()).unwrap_or("text/html"),
            // the Java String is UTF-16; it crosses as UTF-8 whatever charset the caller named
            "UTF-8",
            b.as_deref().unwrap_or("about:blank"),
        ),
    }
});

provider_native!(Java_com_android_webview_chromium_WebViewProviderImpl_evaluateJavaScript, (env, n, script: Obj, callback: Obj) -> () = (), {
    let script = string(env, script).unwrap_or_default();
    let cb = (!callback.is_null()).then(|| jni_call!(env, NewGlobalRef, callback) as usize);
    n.view.evaluate_javascript(&script, Box::new(move |r| {
        let Some(cb) = cb else { return };
        let Some(env) = env_now() else { return };
        // a script that throws reports "null", as Chromium's evaluateJavascript does
        let s = new_string(env, r.as_deref().unwrap_or("null"));
        call_void(env, cb as Obj, c"onReceiveValue", c"(Ljava/lang/Object;)V", &[l(s)]);
        raise_uncaught(env);
        delete_local(env, s);
        jni_call!(env, DeleteGlobalRef, cb as Obj);
    }));
});

provider_native!(Java_com_android_webview_chromium_WebViewProviderImpl_stopLoading, (env, n) -> () = (), {
    let _ = env;
    n.view.stop_loading();
});

provider_native!(Java_com_android_webview_chromium_WebViewProviderImpl_reload, (env, n) -> () = (), {
    let _ = env;
    n.view.reload();
});

provider_native!(Java_com_android_webview_chromium_WebViewProviderImpl_getUrl, (env, n) -> Obj = NULL, {
    n.view.uri().map_or(NULL, |u| new_string(env, &u))
});

provider_native!(Java_com_android_webview_chromium_WebViewProviderImpl_getTitle, (env, n) -> Obj = NULL, {
    n.view.title().map_or(NULL, |t| new_string(env, &t))
});

provider_native!(Java_com_android_webview_chromium_WebViewProviderImpl_getProgress, (env, n) -> i32 = 0, {
    let _ = env;
    n.view.progress()
});

// WebView.setBackgroundColor hands the colour to the provider only (WebView.java:3072): the page's background is the one there is
provider_native!(Java_com_android_webview_chromium_WebViewProviderImpl_setBackgroundColor, (env, n, color: i32) -> () = (), {
    let _ = env;
    n.view.set_background_color(color as u32);
});

provider_native!(Java_com_android_webview_chromium_WebViewProviderImpl_setInitialScale, (env, n, percent: i32) -> () = (), {
    let _ = env;
    n.view.set_initial_scale(percent);
});

// ---- CookieManager (webview plan D5): the copy in crate::cookies; one CookieManagerImpl per process ----

static COOKIE_MANAGER: OnceLock<usize> = OnceLock::new();

#[unsafe(no_mangle)]
pub extern "C" fn Java_com_android_webview_chromium_WebViewChromiumFactoryProviderForT_getCookieManager(
    env: Env,
    _this: Obj,
) -> Obj {
    if let Some(&g) = COOKIE_MANAGER.get() {
        return g as Obj;
    }
    crate::cookies::start();
    let obj = new_object(
        env,
        c"com/android/webview/chromium/CookieManagerImpl",
        c"()V",
        &[],
    );
    if obj.is_null() {
        return NULL;
    }
    let global = jni_call!(env, NewGlobalRef, obj) as usize;
    delete_local(env, obj);
    // a racing thread's instance wins, as getStatics
    *COOKIE_MANAGER.get_or_init(|| global) as Obj
}

/// A CookieReply (global ref) as a done callback: `post(value)` on the thread the engine answers on (the UI thread), which the reply hands to the caller's Looper.
fn cookie_reply(env: Env, reply: Obj) -> Box<dyn FnOnce(bool) + Send> {
    let g = (!reply.is_null()).then(|| jni_call!(env, NewGlobalRef, reply) as usize);
    Box::new(move |ok| {
        let Some(g) = g else { return };
        let Some(env) = env_now() else { return };
        call_void(env, g as Obj, c"post", c"(Z)V", &[jvalue { z: ok as u8 }]);
        raise_uncaught(env);
        jni_call!(env, DeleteGlobalRef, g as Obj);
    })
}

/// The provider of `web_view` (WebView.getWebViewProvider), as its native view.
fn web_view_native<'a>(env: Env, web_view: Obj) -> Option<&'a Native> {
    if web_view.is_null() {
        return None;
    }
    let p = call_obj(
        env,
        web_view,
        c"getWebViewProvider",
        c"()Landroid/webkit/WebViewProvider;",
        &[],
    );
    let n = (!p.is_null()).then(|| native(env, p)).flatten();
    delete_local(env, p);
    n
}

#[unsafe(no_mangle)]
pub extern "C" fn Java_com_android_webview_chromium_CookieManagerImpl_setAcceptCookie(
    _env: Env,
    _this: Obj,
    accept: u8,
) {
    crate::cookies::set_accept(accept != 0);
}

#[unsafe(no_mangle)]
pub extern "C" fn Java_com_android_webview_chromium_CookieManagerImpl_acceptCookie(
    _env: Env,
    _this: Obj,
) -> u8 {
    crate::cookies::accept() as u8
}

#[unsafe(no_mangle)]
pub extern "C" fn Java_com_android_webview_chromium_CookieManagerImpl_setAcceptThirdPartyCookies(
    env: Env,
    _this: Obj,
    web_view: Obj,
    accept: u8,
) {
    if let Some(n) = web_view_native(env, web_view) {
        n.set_third_party(accept != 0);
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn Java_com_android_webview_chromium_CookieManagerImpl_acceptThirdPartyCookies(
    env: Env,
    _this: Obj,
    web_view: Obj,
) -> u8 {
    web_view_native(env, web_view).is_some_and(|n| n.third_party.get()) as u8
}

#[unsafe(no_mangle)]
pub extern "C" fn Java_com_android_webview_chromium_CookieManagerImpl_setCookieNative(
    env: Env,
    _class: Obj,
    url: Obj,
    value: Obj,
    reply: Obj,
) {
    let done = cookie_reply(env, reply);
    match (string(env, url), string(env, value)) {
        (Some(u), Some(v)) => crate::cookies::set(&u, &v, done),
        _ => done(false),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn Java_com_android_webview_chromium_CookieManagerImpl_getCookie(
    env: Env,
    _this: Obj,
    url: Obj,
) -> Obj {
    string(env, url)
        .and_then(|u| crate::cookies::get(&u))
        .map_or(NULL, |c| new_string(env, &c))
}

#[unsafe(no_mangle)]
pub extern "C" fn Java_com_android_webview_chromium_CookieManagerImpl_removeNative(
    env: Env,
    _class: Obj,
    all: u8,
    reply: Obj,
) {
    crate::cookies::remove(all != 0, cookie_reply(env, reply));
}

#[unsafe(no_mangle)]
pub extern "C" fn Java_com_android_webview_chromium_CookieManagerImpl_hasCookies(
    _env: Env,
    _this: Obj,
) -> u8 {
    crate::cookies::any() as u8
}

#[unsafe(no_mangle)]
pub extern "C" fn Java_com_android_webview_chromium_CookieManagerImpl_removeExpiredCookie(
    _env: Env,
    _this: Obj,
) {
    crate::cookies::remove_expired();
}

#[unsafe(no_mangle)]
pub extern "C" fn Java_com_android_webview_chromium_CookieManagerImpl_allowFileSchemeCookiesImpl(
    _env: Env,
    _this: Obj,
) -> u8 {
    crate::cookies::file_scheme() as u8
}

#[unsafe(no_mangle)]
pub extern "C" fn Java_com_android_webview_chromium_CookieManagerImpl_setAcceptFileSchemeCookiesImpl(
    _env: Env,
    _this: Obj,
    accept: u8,
) {
    crate::cookies::set_file_scheme(accept != 0);
}

// ---- input (webview plan S2.2 amended, hold-and-replay) ----

/// MotionEvent.ACTION_* (MotionEvent.java).
mod action {
    pub const DOWN: i32 = 0;
    pub const UP: i32 = 1;
    pub const MOVE: i32 = 2;
    pub const CANCEL: i32 = 3;
    pub const POINTER_DOWN: i32 = 5;
    pub const POINTER_UP: i32 = 6;
    pub const SCROLL: i32 = 8;
}

/// The divergence a Cancel can leave: the page got the press, Android cancelled the gesture (a parent intercepted), and the page is told of no cancel (webview plan S2.2: no toolkit API sends one).
const INTERCEPT_NO_CANCEL: &str = "webview.input.intercept-no-cancel";

/// Claims `claim` at `ev`'s own time (`getEventTimeNanos`: the CLOCK_MONOTONIC stamp the toolkit event was published with).
fn claim(env: Env, n: &Native, claim: InputClaim, ev: Obj) {
    let t = call_long(env, ev, c"getEventTimeNanos", c"()J");
    if n.view.claim_input(claim, t) {
        atl_diag::count(atl_diag::DIVERGENCE, INTERCEPT_NO_CANCEL, None);
    }
}

// Chromium consumes every touch it is given (AwContents.onTouchEvent)
provider_native!(Java_com_android_webview_chromium_WebViewProviderImpl_onTouchEvent, (env, n, ev: Obj) -> u8 = 0, {
    let c = match call_int(env, ev, c"getActionMasked", c"()I", &[]) {
        action::DOWN => InputClaim::Down,
        action::UP => InputClaim::Up,
        action::CANCEL => InputClaim::Cancel,
        action::MOVE | action::POINTER_DOWN | action::POINTER_UP => InputClaim::Move,
        _ => InputClaim::Move,
    };
    claim(env, n, c, ev);
    1
});

provider_native!(Java_com_android_webview_chromium_WebViewProviderImpl_onHoverEvent, (env, n, ev: Obj) -> u8 = 0, {
    claim(env, n, InputClaim::Hover, ev);
    1
});

// a scroll is the page's; any other generic motion (a joystick) goes on as View's
provider_native!(Java_com_android_webview_chromium_WebViewProviderImpl_onGenericMotionEvent, (env, n, ev: Obj) -> u8 = 0, {
    if call_int(env, ev, c"getActionMasked", c"()I", &[]) == action::SCROLL {
        claim(env, n, InputClaim::Move, ev);
        return 1;
    }
    super_bool(env, n, c"super_onGenericMotionEvent", c"(Landroid/view/MotionEvent;)Z", ev)
});

// a system key (BACK, HOME, volume: KeyEvent.isSystem), or one WebViewClient.shouldOverrideKeyEvent takes, goes on to the framework as View's, as Chromium leaves it; any other is the page's, claimed
// (WebViewClient.java: "Except system keys, WebView always consumes the keys in the normal flow"); one the page leaves unhandled comes back later as WebEvent::UnhandledKey
provider_native!(Java_com_android_webview_chromium_WebViewProviderImpl_dispatchKeyEvent, (env, n, ev: Obj) -> u8 = 0, {
    if call_bool(env, ev, c"isSystem", c"()Z", &[]) || should_override_key(env, &n.shared, ev) {
        return super_bool(env, n, c"super_dispatchKeyEvent", c"(Landroid/view/KeyEvent;)Z", ev);
    }
    claim(env, n, InputClaim::Key, ev);
    keep_key(env, &n.shared, ev);
    1
});

/// The provider's WebView and its WebViewClient (a default `WebViewClient` when the app set none: Chromium's adapter uses a null-object client the same way), as local refs; None once the WebView was collected.
fn view_and_client(env: Env, shared: &Shared) -> Option<(Obj, Obj)> {
    let provider = jni_call!(env, NewLocalRef, shared.provider.get() as Obj);
    if provider.is_null() {
        return None;
    }
    let web_view = object_field(env, provider, c"mWebView", c"Landroid/webkit/WebView;");
    let mut client = object_field(
        env,
        provider,
        c"mWebViewClient",
        c"Landroid/webkit/WebViewClient;",
    );
    delete_local(env, provider);
    if client.is_null() {
        client = new_object(env, c"android/webkit/WebViewClient", c"()V", &[]);
    }
    if client.is_null() || web_view.is_null() {
        for o in [web_view, client] {
            delete_local(env, o);
        }
        return None;
    }
    Some((web_view, client))
}

/// WebViewClient.shouldOverrideKeyEvent(view, ev): the app takes the key from the page.
fn should_override_key(env: Env, shared: &Shared, ev: Obj) -> bool {
    let Some((web_view, client)) = view_and_client(env, shared) else {
        return false;
    };
    let r = call_bool(
        env,
        client,
        c"shouldOverrideKeyEvent",
        c"(Landroid/webkit/WebView;Landroid/view/KeyEvent;)Z",
        &[l(web_view), l(ev)],
    );
    raise_uncaught(env);
    for o in [web_view, client] {
        delete_local(env, o);
    }
    r
}

/// Keeps a copy of claimed key `ev` (the framework recycles the original after dispatch) for its unhandled report; a release whose press was already reported unhandled is reported now.
fn keep_key(env: Env, shared: &Shared, ev: Obj) {
    let t = call_long(env, ev, c"getEventTimeNanos", c"()J");
    let action = call_int(env, ev, c"getAction", c"()I", &[]);
    let code = call_int(env, ev, c"getKeyCode", c"()I", &[]);
    let copy = new_object(
        env,
        c"android/view/KeyEvent",
        c"(Landroid/view/KeyEvent;)V",
        &[l(ev)],
    );
    if copy.is_null() {
        raise_uncaught(env);
        return;
    }
    let global = jni_call!(env, NewGlobalRef, copy) as usize;
    delete_local(env, copy);
    let pressed_unhandled = action == KEY_ACTION_UP && {
        let mut p = shared.unhandled_presses.borrow_mut();
        let found = p.iter().position(|c| *c == code).map(|i| p.remove(i));
        found.is_some()
    };
    if pressed_unhandled {
        report_unhandled(env, shared, global);
        return;
    }
    let evicted = {
        let mut keys = shared.keys.borrow_mut();
        let evicted = (keys.len() == KEPT_KEYS)
            .then(|| keys.pop_front())
            .flatten();
        keys.push_back((t, global, action, code));
        evicted
    };
    if let Some((_, g, ..)) = evicted {
        jni_call!(env, DeleteGlobalRef, g as Obj);
    }
}

/// The page left the press stamped `t` unhandled (WebEvent::UnhandledKey): it goes to the app, then its release (claimed already, or when it comes). WebKitGTK reports presses only; the release follows
/// its press's verdict, so SyntheticKeyboardHandler gets the pair a fallback needs (a back press acts on the up). A page that handled the press (an Esc handler) has neither reported.
fn on_unhandled_key(env: Env, shared: &Shared, t: i64) {
    let press = {
        let mut keys = shared.keys.borrow_mut();
        keys.iter()
            .position(|(s, _, a, _)| *s == t && *a == KEY_ACTION_DOWN)
            .and_then(|i| keys.remove(i))
    };
    let Some((_, global, _, code)) = press else {
        tracing::warn!("webview: unhandled key {t}: no kept press with that stamp");
        return;
    };
    report_unhandled(env, shared, global);
    let release = {
        let mut keys = shared.keys.borrow_mut();
        keys.iter()
            .position(|(s, _, a, c)| *s > t && *a == KEY_ACTION_UP && *c == code)
            .and_then(|i| keys.remove(i))
    };
    match release {
        Some((_, g, ..)) => report_unhandled(env, shared, g),
        None => {
            let mut p = shared.unhandled_presses.borrow_mut();
            if !p.contains(&code) {
                p.push(code);
            }
        }
    }
}

/// WebViewClient.onUnhandledKeyEvent(view, key) with the kept copy `global` (released after: the framework queue holds its own reference). Its default re-dispatches through
/// ViewRootImpl.dispatchUnhandledInputEvent -> SyntheticKeyboardHandler, which sends the key map's fallback (Esc -> BACK with the desktop's esc-back map).
fn report_unhandled(env: Env, shared: &Shared, global: usize) {
    let key = global as Obj;
    if let Some((web_view, client)) = view_and_client(env, shared) {
        let code = call_int(env, key, c"getKeyCode", c"()I", &[]);
        let action = call_int(env, key, c"getAction", c"()I", &[]);
        tracing::info!("webview: unhandled key code={code} action={action} -> onUnhandledKeyEvent");
        call_void(
            env,
            client,
            c"onUnhandledKeyEvent",
            c"(Landroid/webkit/WebView;Landroid/view/KeyEvent;)V",
            &[l(web_view), l(key)],
        );
        raise_uncaught(env);
        for o in [web_view, client] {
            delete_local(env, o);
        }
    }
    jni_call!(env, DeleteGlobalRef, key);
}

provider_native!(Java_com_android_webview_chromium_WebViewProviderImpl_onFocusChanged, (env, n, focused: u8, _direction: i32, _previous: Obj) -> () = (), {
    let _ = env;
    n.view.set_focused(focused != 0);
});

/// `WebView.PrivateAccess.<name>(ev)` of the provider's WebView (View's own answer).
fn super_bool(env: Env, n: &Native, name: &CStr, sig: &CStr, ev: Obj) -> u8 {
    let provider = jni_call!(env, NewLocalRef, n.shared.provider.get() as Obj);
    let access = object_field(
        env,
        provider,
        c"mWebViewPrivate",
        c"Landroid/webkit/WebView$PrivateAccess;",
    );
    let r = !access.is_null() && call_bool(env, access, name, sig, &[l(ev)]);
    delete_local(env, access);
    delete_local(env, provider);
    r as u8
}

provider_native!(Java_com_android_webview_chromium_WebViewProviderImpl_addJavascriptInterface, (env, n, object: Obj, name: Obj) -> () = (), {
    // WebView.addJavascriptInterface's own null checks are the caller's; a null name or object adds nothing
    if let Some(name) = string(env, name) && !object.is_null() {
        add_interface(env, n, object, &name);
    }
});

provider_native!(Java_com_android_webview_chromium_WebViewProviderImpl_removeJavascriptInterface, (env, n, name: Obj) -> () = (), {
    let Some(name) = string(env, name) else { return };
    let removed = {
        let mut list = n.interfaces.borrow_mut();
        list.iter().position(|(nm, ..)| *nm == name).map(|i| list.remove(i))
    };
    if let Some((_, g, _)) = removed {
        jni_call!(env, DeleteGlobalRef, g as Obj);
        refresh_scripts(n);
    }
});

// ---- WebSettingsImpl ----

macro_rules! settings_native {
    ($sym:ident, ($env:ident, $n:ident $(, $p:ident: $t:ty)*) -> $ret:ty = $default:expr, $body:block) => {
        #[allow(clippy::unused_unit)] // as provider_native
        #[unsafe(no_mangle)]
        pub extern "C" fn $sym($env: Env, this: Obj $(, $p: $t)*) -> $ret {
            let Some($n) = settings_native($env, this) else { return $default };
            $body
        }
    };
}

// ---- WebSettingsImpl: each property is one row of the table in settings.rs. A setter stores the value and pushes the engine's; a getter returns the stored value. ----

/// A flag property's setter and getter.
macro_rules! flag_setting {
    ($set:ident, $get:ident, $which:ident) => {
        flag_set!($set, $which);
        settings_native!($get, (env, n) -> u8 = 0, {
            let _ = env;
            n.flag(Setting::$which) as u8
        });
    };
}

/// A flag property's setter alone.
macro_rules! flag_set {
    ($set:ident, $which:ident) => {
        settings_native!($set, (env, n, on: u8) -> () = (), {
            let _ = env;
            n.put(Setting::$which, SettingValue::Bool(on != 0));
        });
    };
}

/// An int property's setter and getter.
macro_rules! int_setting {
    ($set:ident, $get:ident, $which:ident) => {
        int_set!($set, $which);
        settings_native!($get, (env, n) -> i32 = 0, {
            let _ = env;
            n.int(Setting::$which)
        });
    };
}

/// An int property's setter alone.
macro_rules! int_set {
    ($set:ident, $which:ident) => {
        settings_native!($set, (env, n, v: i32) -> () = (), {
            let _ = env;
            n.put(Setting::$which, SettingValue::Int(v));
        });
    };
}

/// A text property's setter and getter.
macro_rules! text_setting {
    ($set:ident, $get:ident, $which:ident) => {
        settings_native!($set, (env, n, v: Obj) -> () = (), {
            n.put(Setting::$which, SettingValue::Str(string(env, v).unwrap_or_default()));
        });
        text_get!($get, $which);
    };
}

/// A text property's getter.
macro_rules! text_get {
    ($get:ident, $which:ident) => {
        settings_native!($get, (env, n) -> Obj = NULL, {
            new_string(env, &n.text(Setting::$which))
        });
    };
}

flag_setting!(
    Java_com_android_webview_chromium_WebSettingsImpl_setJavaScriptEnabled,
    Java_com_android_webview_chromium_WebSettingsImpl_getJavaScriptEnabled,
    JavaScriptEnabled
);
flag_setting!(
    Java_com_android_webview_chromium_WebSettingsImpl_setDomStorageEnabled,
    Java_com_android_webview_chromium_WebSettingsImpl_getDomStorageEnabled,
    DomStorageEnabled
);
flag_setting!(
    Java_com_android_webview_chromium_WebSettingsImpl_setNavDump,
    Java_com_android_webview_chromium_WebSettingsImpl_getNavDump,
    NavDump
);
flag_setting!(
    Java_com_android_webview_chromium_WebSettingsImpl_setSupportZoom,
    Java_com_android_webview_chromium_WebSettingsImpl_supportZoom,
    SupportZoom
);
flag_setting!(
    Java_com_android_webview_chromium_WebSettingsImpl_setMediaPlaybackRequiresUserGesture,
    Java_com_android_webview_chromium_WebSettingsImpl_getMediaPlaybackRequiresUserGesture,
    MediaPlaybackRequiresUserGesture
);
flag_setting!(
    Java_com_android_webview_chromium_WebSettingsImpl_setBuiltInZoomControls,
    Java_com_android_webview_chromium_WebSettingsImpl_getBuiltInZoomControls,
    BuiltInZoomControls
);
flag_setting!(
    Java_com_android_webview_chromium_WebSettingsImpl_setDisplayZoomControls,
    Java_com_android_webview_chromium_WebSettingsImpl_getDisplayZoomControls,
    DisplayZoomControls
);
flag_setting!(
    Java_com_android_webview_chromium_WebSettingsImpl_setAllowFileAccess,
    Java_com_android_webview_chromium_WebSettingsImpl_getAllowFileAccess,
    AllowFileAccess
);
flag_setting!(
    Java_com_android_webview_chromium_WebSettingsImpl_setAllowContentAccess,
    Java_com_android_webview_chromium_WebSettingsImpl_getAllowContentAccess,
    AllowContentAccess
);
flag_setting!(
    Java_com_android_webview_chromium_WebSettingsImpl_setLoadWithOverviewMode,
    Java_com_android_webview_chromium_WebSettingsImpl_getLoadWithOverviewMode,
    LoadWithOverviewMode
);
flag_setting!(
    Java_com_android_webview_chromium_WebSettingsImpl_setEnableSmoothTransition,
    Java_com_android_webview_chromium_WebSettingsImpl_enableSmoothTransition,
    EnableSmoothTransition
);
flag_setting!(Java_com_android_webview_chromium_WebSettingsImpl_setUseWebViewBackgroundForOverscrollBackground, Java_com_android_webview_chromium_WebSettingsImpl_getUseWebViewBackgroundForOverscrollBackground, UseWebViewBackgroundForOverscrollBackground);
flag_setting!(
    Java_com_android_webview_chromium_WebSettingsImpl_setSaveFormData,
    Java_com_android_webview_chromium_WebSettingsImpl_getSaveFormData,
    SaveFormData
);
flag_setting!(
    Java_com_android_webview_chromium_WebSettingsImpl_setSavePassword,
    Java_com_android_webview_chromium_WebSettingsImpl_getSavePassword,
    SavePassword
);
flag_setting!(
    Java_com_android_webview_chromium_WebSettingsImpl_setLightTouchEnabled,
    Java_com_android_webview_chromium_WebSettingsImpl_getLightTouchEnabled,
    LightTouchEnabled
);
flag_setting!(
    Java_com_android_webview_chromium_WebSettingsImpl_setUseWideViewPort,
    Java_com_android_webview_chromium_WebSettingsImpl_getUseWideViewPort,
    UseWideViewPort
);
flag_setting!(
    Java_com_android_webview_chromium_WebSettingsImpl_setSupportMultipleWindows,
    Java_com_android_webview_chromium_WebSettingsImpl_supportMultipleWindows,
    SupportMultipleWindows
);
flag_setting!(
    Java_com_android_webview_chromium_WebSettingsImpl_setBlockNetworkImage,
    Java_com_android_webview_chromium_WebSettingsImpl_getBlockNetworkImage,
    BlockNetworkImage
);
settings_native!(Java_com_android_webview_chromium_WebSettingsImpl_setAcceptThirdPartyCookies, (env, n, accept: u8) -> () = (), {
    let _ = env;
    n.set_third_party(accept != 0);
});
settings_native!(Java_com_android_webview_chromium_WebSettingsImpl_getAcceptThirdPartyCookies, (env, n) -> u8 = 0, {
    let _ = env;
    n.third_party.get() as u8
});
settings_native!(Java_com_android_webview_chromium_WebSettingsImpl_setBlockNetworkLoads, (env, n, block: u8) -> () = (), {
    // WebSettings.java:1019-1021: false needs the INTERNET permission
    if block == 0 && !n.settings.borrow().internet() {
        return throw(env, c"java/lang/SecurityException", "setBlockNetworkLoads(false) needs android.permission.INTERNET");
    }
    n.put(Setting::BlockNetworkLoads, SettingValue::Bool(block != 0));
});
settings_native!(Java_com_android_webview_chromium_WebSettingsImpl_getBlockNetworkLoads, (env, n) -> u8 = 0, {
    let _ = env;
    n.flag(Setting::BlockNetworkLoads) as u8
});
flag_setting!(
    Java_com_android_webview_chromium_WebSettingsImpl_setAllowUniversalAccessFromFileURLs,
    Java_com_android_webview_chromium_WebSettingsImpl_getAllowUniversalAccessFromFileURLs,
    AllowUniversalAccessFromFileUrls
);
flag_setting!(
    Java_com_android_webview_chromium_WebSettingsImpl_setAllowFileAccessFromFileURLs,
    Java_com_android_webview_chromium_WebSettingsImpl_getAllowFileAccessFromFileURLs,
    AllowFileAccessFromFileUrls
);
flag_setting!(
    Java_com_android_webview_chromium_WebSettingsImpl_setPluginsEnabled,
    Java_com_android_webview_chromium_WebSettingsImpl_getPluginsEnabled,
    PluginsEnabled
);
flag_setting!(
    Java_com_android_webview_chromium_WebSettingsImpl_setDatabaseEnabled,
    Java_com_android_webview_chromium_WebSettingsImpl_getDatabaseEnabled,
    DatabaseEnabled
);
flag_set!(
    Java_com_android_webview_chromium_WebSettingsImpl_setGeolocationEnabled,
    GeolocationEnabled
);
flag_setting!(
    Java_com_android_webview_chromium_WebSettingsImpl_setJavaScriptCanOpenWindowsAutomatically,
    Java_com_android_webview_chromium_WebSettingsImpl_getJavaScriptCanOpenWindowsAutomatically,
    JavaScriptCanOpenWindowsAutomatically
);
flag_set!(
    Java_com_android_webview_chromium_WebSettingsImpl_setNeedInitialFocus,
    NeedInitialFocus
);
flag_setting!(
    Java_com_android_webview_chromium_WebSettingsImpl_setOffscreenPreRaster,
    Java_com_android_webview_chromium_WebSettingsImpl_getOffscreenPreRaster,
    OffscreenPreRaster
);
flag_setting!(
    Java_com_android_webview_chromium_WebSettingsImpl_setSafeBrowsingEnabled,
    Java_com_android_webview_chromium_WebSettingsImpl_getSafeBrowsingEnabled,
    SafeBrowsingEnabled
);
flag_setting!(
    Java_com_android_webview_chromium_WebSettingsImpl_setLoadsImagesAutomatically,
    Java_com_android_webview_chromium_WebSettingsImpl_getLoadsImagesAutomatically,
    LoadsImagesAutomatically
);
flag_setting!(Java_com_android_webview_chromium_WebSettingsImpl_setVideoOverlayForEmbeddedEncryptedVideoEnabled, Java_com_android_webview_chromium_WebSettingsImpl_getVideoOverlayForEmbeddedEncryptedVideoEnabled, VideoOverlayForEmbeddedEncryptedVideoEnabled);
int_setting!(
    Java_com_android_webview_chromium_WebSettingsImpl_setTextZoom,
    Java_com_android_webview_chromium_WebSettingsImpl_getTextZoom,
    TextZoom
);
int_setting!(
    Java_com_android_webview_chromium_WebSettingsImpl_setUserAgent,
    Java_com_android_webview_chromium_WebSettingsImpl_getUserAgent,
    UserAgentCode
);
int_setting!(
    Java_com_android_webview_chromium_WebSettingsImpl_setMinimumFontSize,
    Java_com_android_webview_chromium_WebSettingsImpl_getMinimumFontSize,
    MinimumFontSize
);
int_setting!(
    Java_com_android_webview_chromium_WebSettingsImpl_setMinimumLogicalFontSize,
    Java_com_android_webview_chromium_WebSettingsImpl_getMinimumLogicalFontSize,
    MinimumLogicalFontSize
);
int_setting!(
    Java_com_android_webview_chromium_WebSettingsImpl_setDefaultFontSize,
    Java_com_android_webview_chromium_WebSettingsImpl_getDefaultFontSize,
    DefaultFontSize
);
int_setting!(
    Java_com_android_webview_chromium_WebSettingsImpl_setDefaultFixedFontSize,
    Java_com_android_webview_chromium_WebSettingsImpl_getDefaultFixedFontSize,
    DefaultFixedFontSize
);
int_setting!(
    Java_com_android_webview_chromium_WebSettingsImpl_setCacheMode,
    Java_com_android_webview_chromium_WebSettingsImpl_getCacheMode,
    CacheMode
);
int_setting!(
    Java_com_android_webview_chromium_WebSettingsImpl_setMixedContentMode,
    Java_com_android_webview_chromium_WebSettingsImpl_getMixedContentMode,
    MixedContentMode
);
int_setting!(
    Java_com_android_webview_chromium_WebSettingsImpl_setDisabledActionModeMenuItems,
    Java_com_android_webview_chromium_WebSettingsImpl_getDisabledActionModeMenuItems,
    DisabledActionModeMenuItems
);
int_setting!(
    Java_com_android_webview_chromium_WebSettingsImpl_setDefaultZoomValue,
    Java_com_android_webview_chromium_WebSettingsImpl_getDefaultZoomValue,
    DefaultZoom
);
int_setting!(
    Java_com_android_webview_chromium_WebSettingsImpl_setLayoutAlgorithmValue,
    Java_com_android_webview_chromium_WebSettingsImpl_getLayoutAlgorithmValue,
    LayoutAlgorithm
);
int_setting!(
    Java_com_android_webview_chromium_WebSettingsImpl_setPluginStateValue,
    Java_com_android_webview_chromium_WebSettingsImpl_getPluginStateValue,
    PluginState
);
int_set!(
    Java_com_android_webview_chromium_WebSettingsImpl_setRenderPriorityValue,
    RenderPriority
);
text_setting!(
    Java_com_android_webview_chromium_WebSettingsImpl_setStandardFontFamily,
    Java_com_android_webview_chromium_WebSettingsImpl_getStandardFontFamily,
    StandardFontFamily
);
text_setting!(
    Java_com_android_webview_chromium_WebSettingsImpl_setFixedFontFamily,
    Java_com_android_webview_chromium_WebSettingsImpl_getFixedFontFamily,
    FixedFontFamily
);
text_setting!(
    Java_com_android_webview_chromium_WebSettingsImpl_setSansSerifFontFamily,
    Java_com_android_webview_chromium_WebSettingsImpl_getSansSerifFontFamily,
    SansSerifFontFamily
);
text_setting!(
    Java_com_android_webview_chromium_WebSettingsImpl_setSerifFontFamily,
    Java_com_android_webview_chromium_WebSettingsImpl_getSerifFontFamily,
    SerifFontFamily
);
text_setting!(
    Java_com_android_webview_chromium_WebSettingsImpl_setCursiveFontFamily,
    Java_com_android_webview_chromium_WebSettingsImpl_getCursiveFontFamily,
    CursiveFontFamily
);
text_setting!(
    Java_com_android_webview_chromium_WebSettingsImpl_setFantasyFontFamily,
    Java_com_android_webview_chromium_WebSettingsImpl_getFantasyFontFamily,
    FantasyFontFamily
);
text_get!(
    Java_com_android_webview_chromium_WebSettingsImpl_getDatabasePath,
    DatabasePath
);
text_setting!(
    Java_com_android_webview_chromium_WebSettingsImpl_setDefaultTextEncodingName,
    Java_com_android_webview_chromium_WebSettingsImpl_getDefaultTextEncodingName,
    DefaultTextEncodingName
);

settings_native!(Java_com_android_webview_chromium_WebSettingsImpl_setUserAgentString, (env, n, ua: Obj) -> () = (), {
    // null or empty restores the default: the device's user agent (WebSettings.java:1416-1419)
    let text = string(env, ua)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| n.settings.borrow().default_text(Setting::UserAgentString));
    n.put(Setting::UserAgentString, SettingValue::Str(text));
});

text_get!(
    Java_com_android_webview_chromium_WebSettingsImpl_getUserAgentString,
    UserAgentString
);
