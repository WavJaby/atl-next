// SPDX-License-Identifier: MIT
//! The C side: glibc's loader, the JNI invocation API, liblog and bionic's API, each function typed by the header that declares it (build.rs).
//! Each entry converts to safe values at once; the launcher's logic (`main`, `args`, `log`) stays safe.

use core::ffi::{CStr, c_char, c_int, c_void};
use std::borrow::Cow;
use std::ffi::CString;
use std::io::Write;

use anyhow::{Context, Result, bail, ensure};

use crate::generated::{
    dlfcn, jni, jni_invocation, libdl_android, log as liblog, system_properties, unistd,
};
use crate::log::{self, Level, Record};

unsafe extern "C" {
    fn dlopen(filename: *const c_char, flag: c_int) -> *mut c_void;
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
    fn dlerror() -> *mut c_char;
    fn _exit(status: c_int);
}
// the declarations above have the host headers' signatures
const _: dlfcn::dlopen = dlopen;
const _: dlfcn::dlsym = dlsym;
const _: dlfcn::dlerror = dlerror;
const _: unistd::_exit = _exit;

/// The runtime's loader library, by soname: dalvikvm links it (art/dalvikvm Android.bp), found on the library path as the converter's staged ART lays it out.
/// Every function the launcher calls is looked up in it and the libraries it links (dlsym on its handle searches them, in load order): libnativehelper's own, liblog's, and the libc provider's (atl-bionic: system properties, libdl_android).
const NATIVEHELPER: &CStr = c"libnativehelper.so";

/// The process's last `dlerror`, or a placeholder.
fn dl_error() -> String {
    // SAFETY: dlerror takes no arguments; its result is null or a NUL-terminated string valid until the next dl call on this thread, copied at once
    let e = unsafe { dlerror() };
    if e.is_null() {
        "(no dlerror)".to_owned()
    } else {
        // SAFETY: non-null: a NUL-terminated string (above)
        unsafe { CStr::from_ptr(e) }.to_string_lossy().into_owned()
    }
}

/// The ART-side functions, resolved once.
pub struct Runtime {
    jni_invocation_create: jni_invocation::JniInvocationCreate,
    jni_invocation_init: jni_invocation::JniInvocationInit,
    create_java_vm: jni::JNI_CreateJavaVM,
    set_logger: liblog::__android_log_set_logger,
    properties_init: system_properties::__system_properties_init,
    set_target_sdk: libdl_android::android_set_application_target_sdk_version,
}

/// `name` in `handle`'s library and its dependencies, as `F`.
///
/// # Safety
/// `F` is the C function-pointer type of `name`; `handle` a live `dlopen` handle.
unsafe fn sym<F: Copy>(handle: *mut c_void, name: &CStr) -> Result<F> {
    const {
        assert!(size_of::<F>() == size_of::<*mut c_void>());
    }
    // SAFETY: a live handle and a NUL-terminated name (caller)
    let p = unsafe { dlsym(handle, name.as_ptr()) };
    ensure!(!p.is_null(), "{}: {}", name.to_string_lossy(), dl_error());
    // SAFETY: a function's address, F its pointer type of the same size (caller, asserted above)
    Ok(unsafe { core::mem::transmute_copy::<*mut c_void, F>(&p) })
}

impl Runtime {
    /// Loads libnativehelper (`RTLD_GLOBAL`: the libraries ART loads later bind to the same liblog and libc provider) and resolves every function.
    pub fn load() -> Result<Self> {
        // SAFETY: a NUL-terminated soname and the header's flag values
        let h = unsafe { dlopen(NATIVEHELPER.as_ptr(), dlfcn::RTLD_NOW | dlfcn::RTLD_GLOBAL) };
        ensure!(!h.is_null(), "loading the runtime: {}", dl_error());
        // SAFETY: each F is the generated type of the function of that name (build.rs `IMPORTS`); h is live (never closed)
        unsafe {
            Ok(Runtime {
                jni_invocation_create: sym(h, c"JniInvocationCreate")?,
                jni_invocation_init: sym(h, c"JniInvocationInit")?,
                create_java_vm: sym(h, c"JNI_CreateJavaVM")?,
                set_logger: sym(h, c"__android_log_set_logger")?,
                properties_init: sym(h, c"__system_properties_init")?,
                set_target_sdk: sym(h, c"android_set_application_target_sdk_version")?,
            })
        }
    }

    /// bionic `__system_properties_init`: true once the process's property area is mapped.
    pub fn has_property_area(&self) -> bool {
        // SAFETY: no arguments; callable any time (bionic maps the area or reports failure)
        unsafe { (self.properties_init)() == 0 }
    }

    /// dalvikvm's start: libnativehelper picks and loads the runtime library (`JniInvocationInit`, null = its default), then `JNI_CreateJavaVM`; the calling thread becomes the main thread.
    pub fn create_vm(&self, options: &[CString]) -> Result<Vm> {
        // SAFETY: no arguments; the instance lives for the process (dalvikvm keeps its JniInvocation until exit)
        let invocation = unsafe { (self.jni_invocation_create)() };
        ensure!(!invocation.is_null(), "JniInvocationCreate failed");
        // SAFETY: a live instance; null library = libnativehelper's default choice
        let ok = unsafe { (self.jni_invocation_init)(invocation, core::ptr::null()) };
        ensure!(ok, "failed to initialize the JNI invocation API");
        let mut opts: Vec<jni::JavaVMOption> = options
            .iter()
            .map(|o| jni::JavaVMOption {
                optionString: o.as_ptr(),
                extraInfo: core::ptr::null_mut(),
            })
            .collect();
        let mut init = jni::JavaVMInitArgs {
            version: jni::JNI_VERSION_1_6,
            nOptions: i32::try_from(opts.len()).context("too many VM options")?,
            options: opts.as_mut_ptr(),
            ignoreUnrecognized: jni::JNI_FALSE as u8,
        };
        let mut vm = core::ptr::null_mut();
        let mut env = core::ptr::null_mut();
        // SAFETY: out-pointers to locals; init and the option strings it points to outlive the call
        let rc =
            unsafe { (self.create_java_vm)(&mut vm, &mut env, (&raw mut init).cast::<c_void>()) };
        ensure!(
            rc == jni::JNI_OK && !vm.is_null() && !env.is_null(),
            "failed to initialize runtime (check log for details): JNI_CreateJavaVM returned {rc}"
        );
        Ok(Vm { vm, env })
    }

    /// Makes `logger` liblog's logger, replacing the one ART's InitLogging installed.
    pub fn install_logger(&self) {
        // SAFETY: a logger of the header's type, valid for the process
        unsafe { (self.set_logger)(Some(logger)) }
    }

    pub fn set_target_sdk(&self, target: i32) {
        // SAFETY: an int; bionic stores it
        unsafe { (self.set_target_sdk)(target) }
    }
}

/// liblog's logger: the message as a C6 record, written whole. liblog filtered it already (`log` module).
extern "C" fn logger(message: *const liblog::__android_log_message) {
    // SAFETY: liblog passes its message, valid for this call, or null
    let Some(m) = (unsafe { message.as_ref() }) else {
        return;
    };
    let text = |p: *const c_char| -> Option<Cow<'_, str>> {
        // SAFETY: liblog's strings are NUL-terminated and live for this call
        (!p.is_null()).then(|| unsafe { CStr::from_ptr(p) }.to_string_lossy())
    };
    let tag = text(m.tag);
    let file = text(m.file);
    let body = text(m.message);
    let monotonic = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
    log::write(&Record {
        monotonic: core::time::Duration::new(
            u64::try_from(monotonic.tv_sec).unwrap_or(0),
            u32::try_from(monotonic.tv_nsec).unwrap_or(0),
        ),
        realtime: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default(),
        pid: std::process::id(),
        tid: rustix::thread::gettid().as_raw_nonzero().get(),
        source: log::LIBLOG,
        tag: tag.as_deref().unwrap_or(""),
        level: Level::from_priority(m.priority),
        location: file.as_deref().map(|f| (f, m.line)),
        message: body.as_deref().unwrap_or(""),
    });
}

type Env = *mut *const jni::JNINativeInterface;

/// Calls the JNIEnv function `$f` of `$env` with the rest as arguments; inside an unsafe block, under that function's JNI contract.
macro_rules! jni_call {
    ($env:expr, $f:ident $(, $arg:expr)* $(,)?) => {{
        let env: Env = $env;
        let f = (**env).$f.expect(concat!("JNIEnv has no ", stringify!($f)));
        f(env $(, $arg)*)
    }};
}

/// A created VM and its main thread's JNIEnv.
pub struct Vm {
    vm: *mut *const jni::JNIInvokeInterface,
    env: Env,
}

const EXIT_FAILURE: i32 = 1;

impl Vm {
    /// dalvikvm `InvokeMain`: `class.main(args)` if public, static, `([Ljava/lang/String;)V`; the exit status (0, or 1 on any failure or an uncaught exception).
    /// Messages are dalvikvm's, on stderr, followed by the pending exception (`ExceptionDescribe`).
    pub fn run_main(&self, class: &str, args: &[CString]) -> i32 {
        let fail = |what: String| {
            let _ = writeln!(std::io::stderr(), "{what}");
            // SAFETY: the main thread's env; describing a pending exception (or none) is always allowed
            unsafe { jni_call!(self.env, ExceptionDescribe) };
            EXIT_FAILURE
        };
        let Ok(name) = CString::new(class.replace('.', "/")) else {
            return fail(format!("Unable to locate class '{class}'"));
        };
        // SAFETY: JNI calls on the main thread's env with NUL-terminated names and the references they return; each result is checked before use
        unsafe {
            let string = jni_call!(self.env, FindClass, c"java/lang/String".as_ptr());
            if string.is_null() {
                return fail("Failed to find class java.lang.String".to_owned());
            }
            let Ok(len) = i32::try_from(args.len()) else {
                return fail("too many arguments".to_owned());
            };
            let array = jni_call!(self.env, NewObjectArray, len, string, core::ptr::null_mut());
            if array.is_null() {
                return fail("Failed to create the argument array".to_owned());
            }
            for (i, a) in (0..len).zip(args) {
                let s = jni_call!(self.env, NewStringUTF, a.as_ptr());
                if s.is_null() {
                    return fail("Failed to create an argument string".to_owned());
                }
                jni_call!(self.env, SetObjectArrayElement, array, i, s);
                jni_call!(self.env, DeleteLocalRef, s);
            }
            let klass = jni_call!(self.env, FindClass, name.as_ptr());
            if klass.is_null() {
                return fail(format!(
                    "Unable to locate class '{}'",
                    name.to_string_lossy()
                ));
            }
            let main = jni_call!(
                self.env,
                GetStaticMethodID,
                klass,
                c"main".as_ptr(),
                c"([Ljava/lang/String;)V".as_ptr()
            );
            if main.is_null() {
                return fail(format!(
                    "Unable to find static main(String[]) in '{}'",
                    name.to_string_lossy()
                ));
            }
            // JNI does not refuse a private method: check public as dalvikvm does, through Method.getModifiers
            let reflected = jni_call!(
                self.env,
                ToReflectedMethod,
                klass,
                main,
                jni::JNI_FALSE as u8
            );
            let method = jni_call!(self.env, FindClass, c"java/lang/reflect/Method".as_ptr());
            if reflected.is_null() || method.is_null() {
                return fail("Failed to get reflected method".to_owned());
            }
            let get = jni_call!(
                self.env,
                GetMethodID,
                method,
                c"getModifiers".as_ptr(),
                c"()I".as_ptr()
            );
            if get.is_null() {
                return fail("Failed to find java.lang.reflect.Method.getModifiers".to_owned());
            }
            // java.lang.reflect.Modifier.PUBLIC
            const PUBLIC: i32 = 0x0001;
            if jni_call!(self.env, CallIntMethodA, reflected, get, core::ptr::null()) & PUBLIC == 0
            {
                return fail(format!(
                    "Sorry, main() is not public in '{}'",
                    name.to_string_lossy()
                ));
            }
            let argv = [jni::jvalue { l: array }];
            jni_call!(self.env, CallStaticVoidMethodA, klass, main, argv.as_ptr());
            // detaching logs an uncaught exception and clears it: check before
            if jni_call!(self.env, ExceptionCheck) != 0 {
                EXIT_FAILURE
            } else {
                0
            }
        }
    }

    /// dalvikvm's end (release builds): detach the main thread, destroy the VM; false if either fails.
    pub fn shut_down(self) -> Result<()> {
        // SAFETY: the VM's own invoke table, called from the thread that created it, once
        unsafe {
            let detach = (**self.vm)
                .DetachCurrentThread
                .context("no DetachCurrentThread")?;
            if detach(self.vm) != jni::JNI_OK {
                bail!("unable to detach main thread");
            }
            let destroy = (**self.vm).DestroyJavaVM.context("no DestroyJavaVM")?;
            if destroy(self.vm) != 0 {
                bail!("runtime did not shut down cleanly");
            }
        }
        Ok(())
    }
}

/// ART's FastExit: `_exit`, no static destructors while runtime daemons may still run.
pub fn fast_exit(status: i32) -> ! {
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
    // SAFETY: ends the process; nothing after it runs
    unsafe { _exit(status) };
    unreachable!("_exit returned")
}
