// SPDX-License-Identifier: MIT
//! The JNI exports: `JNI_OnLoad` and one `Java_*` per native method of the forwarders, each converting at once to safe values and calling safe code.
//! Symbol names are JNI's short form (class + method; no native overloads in these classes). Provider natives run on the UI thread (WebView's own thread check), which is the GTK main thread
//! where the engine's events arrive too; `Statics.getDefaultUserAgent` runs on whatever thread called; bridge calls run on the "JavaBridge" thread ([`bridge_thread`]).

use std::cell::{Cell, RefCell};
use std::ffi::{CStr, CString, c_void};
use std::rc::Rc;
use std::sync::OnceLock;
use std::sync::mpsc;

use atl_platform_ui::webview::{
    InputClaim, LoadError, PromptReply, WebEngine as _, WebEvent, WebView as _,
};
use atl_platform_ui::{PlatformWebEngine, PlatformWebView};

use crate::bridge;

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

fn call_int(env: Env, obj: Obj, name: &CStr, sig: &CStr) -> i32 {
    let m = method(env, obj, name, sig);
    if m.is_null() {
        return 0;
    }
    jni_call!(env, CallIntMethodA, obj, m, std::ptr::null())
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
}

/// What the event sink needs: the provider (a weak global ref, so the app's WebView stays collectable).
struct Shared {
    provider: Cell<usize>,
}

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

#[unsafe(no_mangle)]
pub extern "C" fn Java_com_android_webview_chromium_WebViewChromiumFactoryProviderForT_createWebView(
    env: Env,
    _this: Obj,
    web_view: Obj,
    access: Obj,
) -> Obj {
    let shared = Rc::new(Shared {
        provider: Cell::new(0),
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
    // S5: the engine sends exactly the string getDefaultUserAgent reports
    match device_user_agent(env) {
        Ok(ua) => view.set_user_agent(&ua),
        Err(e) => tracing::warn!("webview: default user agent not set: {e}"),
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
    }));
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
        drop(unsafe { Box::from_raw(native) });
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
    let c = match call_int(env, ev, c"getActionMasked", c"()I") {
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
    if call_int(env, ev, c"getActionMasked", c"()I") == action::SCROLL {
        claim(env, n, InputClaim::Move, ev);
        return 1;
    }
    super_bool(env, n, c"super_onGenericMotionEvent", c"(Landroid/view/MotionEvent;)Z", ev)
});

// a system key (BACK, HOME, volume: KeyEvent.isSystem) goes on to the framework as View's, as Chromium leaves it; any other is the page's
provider_native!(Java_com_android_webview_chromium_WebViewProviderImpl_dispatchKeyEvent, (env, n, ev: Obj) -> u8 = 0, {
    if call_bool(env, ev, c"isSystem", c"()Z", &[]) {
        return super_bool(env, n, c"super_dispatchKeyEvent", c"(Landroid/view/KeyEvent;)Z", ev);
    }
    claim(env, n, InputClaim::Key, ev);
    1
});

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

settings_native!(Java_com_android_webview_chromium_WebSettingsImpl_setJavaScriptEnabled, (env, n, on: u8) -> () = (), {
    let _ = env;
    n.view.set_javascript_enabled(on != 0);
});

settings_native!(Java_com_android_webview_chromium_WebSettingsImpl_getJavaScriptEnabled, (env, n) -> u8 = 0, {
    let _ = env;
    n.view.javascript_enabled() as u8
});

settings_native!(Java_com_android_webview_chromium_WebSettingsImpl_setDomStorageEnabled, (env, n, on: u8) -> () = (), {
    let _ = env;
    n.view.set_dom_storage_enabled(on != 0);
});

settings_native!(Java_com_android_webview_chromium_WebSettingsImpl_getDomStorageEnabled, (env, n) -> u8 = 0, {
    let _ = env;
    n.view.dom_storage_enabled() as u8
});

settings_native!(Java_com_android_webview_chromium_WebSettingsImpl_setUserAgentString, (env, n, ua: Obj) -> () = (), {
    // null or empty restores the default (WebSettings.setUserAgentString doc)
    match string(env, ua).filter(|s| !s.is_empty()) {
        Some(s) => n.view.set_user_agent(&s),
        None => match device_user_agent(env) {
            Ok(d) => n.view.set_user_agent(&d),
            Err(e) => tracing::warn!("webview: default user agent not restored: {e}"),
        },
    }
});

settings_native!(Java_com_android_webview_chromium_WebSettingsImpl_getUserAgentString, (env, n) -> Obj = NULL, {
    new_string(env, &n.view.user_agent())
});
