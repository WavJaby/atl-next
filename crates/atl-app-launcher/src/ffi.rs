// SPDX-License-Identifier: MIT
//! The C side: glibc's loader and signal API, the JNI invocation API, liblog and bionic's API, each function typed by the header that declares it (build.rs).
//! Each entry converts to safe values at once; the launcher's logic (`main`, `args`, `log`) stays safe.

use core::ffi::{CStr, c_char, c_int, c_long, c_void};
use std::borrow::Cow;
use std::ffi::CString;
use std::io::Write;
use std::sync::OnceLock;

use anyhow::{Context, Result, bail, ensure};

use crate::generated::{
    binder_process, dlfcn, invoked_vm, jni, jni_invocation, lib_names, libdl_android,
    log as liblog, signal, syscall as sys, system_properties, unistd,
};
use crate::log::{self, Level, Record};

unsafe extern "C" {
    fn dlopen(filename: *const c_char, flag: c_int) -> *mut c_void;
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
    fn dlerror() -> *mut c_char;
    fn _exit(status: c_int);
    fn syscall(number: c_long, ...) -> c_long;
    fn getpid() -> c_int;
    fn sigfillset(set: *mut signal::__sigset_t) -> c_int;
    fn sigaltstack(stack: *const signal::stack_t, old: *mut signal::stack_t) -> c_int;
}
// the declarations above have the host headers' signatures
const _: dlfcn::dlopen = dlopen;
const _: dlfcn::dlsym = dlsym;
const _: dlfcn::dlerror = dlerror;
const _: unistd::_exit = _exit;
const _: unistd::syscall = syscall;
const _: unistd::getpid = getpid;
const _: signal::sigfillset = sigfillset;
const _: signal::sigaltstack = sigaltstack;

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

/// libandroid_runtime by soname, found on the library path as libnativehelper is (app_process links it; the launcher loads it in app mode only). libbinder_ndk is one of its
/// dependencies, so dlsym on its handle finds the binder process API too.
const ANDROID_RUNTIME: &CStr = c"libandroid_runtime.so";

/// App mode's native side (lib.rs 5a, 5b), resolved once.
pub struct AndroidRuntime {
    attach: invoked_vm::AndroidRuntimeAttachInvokedVm,
    start_thread_pool: binder_process::ABinderProcess_startThreadPool,
}

impl AndroidRuntime {
    /// Loads libandroid_runtime (`RTLD_GLOBAL`, as linked into app_process: libraries loaded later bind to the same libbinder) and resolves both functions.
    pub fn load() -> Result<Self> {
        // SAFETY: a NUL-terminated soname and the header's flag values
        let h = unsafe {
            dlopen(
                ANDROID_RUNTIME.as_ptr(),
                dlfcn::RTLD_NOW | dlfcn::RTLD_GLOBAL,
            )
        };
        ensure!(!h.is_null(), "loading libandroid_runtime: {}", dl_error());
        // SAFETY: each F is the generated type of the function of that name (build.rs `IMPORTS`); h is live (never closed)
        unsafe {
            Ok(AndroidRuntime {
                attach: sym(h, c"AndroidRuntimeAttachInvokedVm")?,
                start_thread_pool: sym(h, c"ABinderProcess_startThreadPool")?,
            })
        }
    }

    /// `AndroidRuntimeAttachInvokedVm` for `vm`, with argv's memory as app_main computes it (computeArgBlockSize: argv[0] to the end of the last argument), which setArgv0 rewrites
    /// to the process name. argv[0] is glibc's `program_invocation_name`; the arguments are contiguous there, each NUL-terminated.
    pub fn attach(&self, vm: &Vm) -> Result<()> {
        let soname = CString::new(lib_names::LIBC_SO).context("libc soname")?;
        // SAFETY: a NUL-terminated soname and the header's flag values; RTLD_NOLOAD only finds the libc already mapped
        let libc = unsafe { dlopen(soname.as_ptr(), dlfcn::RTLD_LAZY | dlfcn::RTLD_NOLOAD) };
        ensure!(!libc.is_null(), "finding libc: {}", dl_error());
        // SAFETY: a live handle and a NUL-terminated name
        let name = unsafe { dlsym(libc, c"program_invocation_name".as_ptr()) };
        ensure!(!name.is_null(), "program_invocation_name: {}", dl_error());
        // SAFETY: glibc's `char *program_invocation_name`, set to argv[0] before main
        let start = unsafe { *name.cast::<*mut c_char>() };
        ensure!(!start.is_null(), "no argv[0]");
        let len: usize = std::env::args_os().map(|a| a.len() + 1).sum();
        // SAFETY: the main thread's JNIEnv, as the patch's declaration takes it (its own JNIEnv type, the same jni.h); the argument block is argv's, writable for the process's life
        let rc = unsafe { (self.attach)(vm.env.cast(), start, len) };
        ensure!(
            rc == 0,
            "AndroidRuntimeAttachInvokedVm returned {rc} (startReg failed?)"
        );
        Ok(())
    }

    /// libbinder's thread pool (AppRuntime::onZygoteInit); idempotent in libbinder.
    pub fn start_thread_pool(&self) {
        // SAFETY: no arguments; the process's binder state is libbinder's own
        unsafe { (self.start_thread_pool)() }
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
        let array = match self.string_array(args.iter().map(CString::as_c_str)) {
            Ok(a) => a,
            Err(e) => return fail(e),
        };
        // SAFETY: JNI calls on the main thread's env with NUL-terminated names and the references they return; each result is checked before use
        unsafe {
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

    /// A `String[]` of `items`; the message of the step that failed otherwise.
    fn string_array<'a>(
        &self,
        items: impl ExactSizeIterator<Item = &'a CStr>,
    ) -> Result<*mut c_void, String> {
        let len = i32::try_from(items.len()).map_err(|_| "too many arguments".to_owned())?;
        // SAFETY: JNI calls on the main thread's env with NUL-terminated strings and the references they return; each result is checked before use
        unsafe {
            let string = jni_call!(self.env, FindClass, c"java/lang/String".as_ptr());
            if string.is_null() {
                return Err("Failed to find class java.lang.String".to_owned());
            }
            let array = jni_call!(self.env, NewObjectArray, len, string, core::ptr::null_mut());
            if array.is_null() {
                return Err("Failed to create the argument array".to_owned());
            }
            for (i, a) in (0..len).zip(items) {
                let s = jni_call!(self.env, NewStringUTF, a.as_ptr());
                if s.is_null() {
                    return Err("Failed to create an argument string".to_owned());
                }
                jni_call!(self.env, SetObjectArrayElement, array, i, s);
                jni_call!(self.env, DeleteLocalRef, s);
            }
            Ok(array)
        }
    }

    /// App mode's Java start (lib.rs 5c), as zygote's child: `ZygoteInit.zygoteInit(targetSdk, new long[0], [class, args...], null).run()`. The exit status as `run_main`'s: an app's
    /// ActivityThread.main loops until the process ends, so a return is a failure (its "Main thread loop unexpectedly exited" exception, or an earlier one).
    pub fn run_zygote_init(&self, target_sdk: i32, class: &str, args: &[CString]) -> i32 {
        let fail = |what: String| {
            let _ = writeln!(std::io::stderr(), "{what}");
            // SAFETY: the main thread's env; describing a pending exception (or none) is always allowed
            unsafe { jni_call!(self.env, ExceptionDescribe) };
            EXIT_FAILURE
        };
        let Ok(class) = CString::new(class) else {
            return fail(format!("class name with a NUL byte: {class}"));
        };
        let argv = match self.string_array(
            core::iter::once(class.as_c_str())
                .chain(args.iter().map(CString::as_c_str))
                .collect::<Vec<_>>()
                .into_iter(),
        ) {
            Ok(a) => a,
            Err(e) => return fail(e),
        };
        // SAFETY: JNI calls on the main thread's env with NUL-terminated names and signatures, and the references they return; each result is checked before use
        unsafe {
            let zygote_init = jni_call!(
                self.env,
                FindClass,
                c"com/android/internal/os/ZygoteInit".as_ptr()
            );
            if zygote_init.is_null() {
                return fail("Unable to locate class 'com/android/internal/os/ZygoteInit'".into());
            }
            let init = jni_call!(
                self.env,
                GetStaticMethodID,
                zygote_init,
                c"zygoteInit".as_ptr(),
                c"(I[J[Ljava/lang/String;Ljava/lang/ClassLoader;)Ljava/lang/Runnable;".as_ptr()
            );
            if init.is_null() {
                return fail("Unable to find ZygoteInit.zygoteInit".into());
            }
            let disabled = jni_call!(self.env, NewLongArray, 0);
            if disabled.is_null() {
                return fail("Failed to create the disabled-changes array".into());
            }
            let init_args = [
                jni::jvalue { i: target_sdk },
                jni::jvalue { l: disabled },
                jni::jvalue { l: argv },
                jni::jvalue {
                    l: core::ptr::null_mut(),
                },
            ];
            let runnable = jni_call!(
                self.env,
                CallStaticObjectMethodA,
                zygote_init,
                init,
                init_args.as_ptr()
            );
            if jni_call!(self.env, ExceptionCheck) != 0 || runnable.is_null() {
                return fail("ZygoteInit.zygoteInit failed".into());
            }
            let runnable_class = jni_call!(self.env, FindClass, c"java/lang/Runnable".as_ptr());
            if runnable_class.is_null() {
                return fail("Unable to locate class 'java/lang/Runnable'".into());
            }
            let run = jni_call!(
                self.env,
                GetMethodID,
                runnable_class,
                c"run".as_ptr(),
                c"()V".as_ptr()
            );
            if run.is_null() {
                return fail("Unable to find Runnable.run".into());
            }
            jni_call!(self.env, CallVoidMethodA, runnable, run, core::ptr::null());
            if jni_call!(self.env, ExceptionCheck) != 0 {
                return fail(format!("{} ended", class.to_string_lossy()));
            }
        }
        0
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

// ---- crash signals: what debuggerd's handler does for the process's own outcome

/// bionic's alternate signal stack size above its guard page (libc/bionic/pthread_internal.h `SIGNAL_STACK_SIZE_WITHOUT_GUARD`): 32 KiB on LP64, 16 KiB otherwise.
const SIGNAL_STACK: usize = if usize::BITS == 64 {
    32 * 1024
} else {
    16 * 1024
};

/// The main thread's alternate signal stack as bionic sets it up for every thread (pthread_create.cpp `__init_alternate_signal_stack`): [`SIGNAL_STACK`] above a PROT_NONE guard page, mapped for the process's life.
/// Why: device-variant ART sets up no signal stack of its own (thread_linux.cc refuses under ART_TARGET_ANDROID, bionic has one), and its handlers run SA_ONSTACK. The one present is Rust std's (SIGSTKSZ, 8 KiB), which ART's unexpected-signal dump (libunwindstack) overflows: a SIGSEGV inside the dump, and ART `_exit(1)`s on the reentry (measured 2026-11-02).
/// Only this thread: glibc gives the threads ART starts none, so their handlers run on the thread's own stack.
pub fn install_signal_stack() -> Result<()> {
    let guard = rustix::param::page_size();
    // SAFETY: a fresh private anonymous mapping, no address hint; nothing else refers to it
    let base = unsafe {
        rustix::mm::mmap_anonymous(
            core::ptr::null_mut(),
            guard + SIGNAL_STACK,
            rustix::mm::ProtFlags::READ | rustix::mm::ProtFlags::WRITE,
            rustix::mm::MapFlags::PRIVATE,
        )
    }
    .context("mapping the signal stack")?;
    // SAFETY: the first page of the mapping just made, which nothing uses
    unsafe { rustix::mm::mprotect(base, guard, rustix::mm::MprotectFlags::empty()) }
        .context("guarding the signal stack")?;
    let stack = signal::stack_t {
        ss_sp: base.cast::<u8>().wrapping_add(guard).cast(),
        ss_flags: 0,
        ss_size: SIGNAL_STACK,
    };
    // SAFETY: the stack is mapped read-write for the process's life (never unmapped); the previous one is not asked for
    let rc = unsafe { sigaltstack(&stack, core::ptr::null_mut()) };
    ensure!(rc == 0, "sigaltstack failed");
    Ok(())
}

/// The signals AOSP's `debuggerd_init` registers its handler for on a process that has not disabled it (system/core/debuggerd/include/debuggerd/handler.h `debuggerd_register_handlers`), which bionic's linker runs before any app code.
/// Not `BIONIC_SIGNAL_DEBUGGER` (bionic `__SIGRTMIN + 3`): it asks for a dump, never ends the process, and is a real-time signal an app may use under glibc (libdebuggerd_client never sends it here: it fails at the tombstoned connect first).
const CRASH_SIGNALS: [c_int; 8] = [
    signal::SIGABRT,
    signal::SIGBUS,
    signal::SIGFPE,
    signal::SIGILL,
    signal::SIGSEGV,
    signal::SIGSTKFLT,
    signal::SIGSYS,
    signal::SIGTRAP,
];

/// libc's own `sigaction`, below libsigchain; set before the handler is installed, never changed.
static LIBC_SIGACTION: OnceLock<signal::sigaction_fn> = OnceLock::new();

/// Installs [`on_crash`] for [`CRASH_SIGNALS`] with debuggerd_init's flags and mask: the action ART finds as the old one.
/// Why: ART's device-variant runtime (art/runtime/runtime_android.cc, active when `ANDROID_ROOT != "/system"`) installs HandleUnexpectedSignalAndroid for these signals, which logs and then calls the previous action's `sa_sigaction` unconditionally.
/// On Android that is debuggerd's; with none, glibc's SIG_DFL is a null pointer, and an abort ends in a jump to address 0 -> SIGSEGV instead of SIGABRT.
/// Installed through libc's `sigaction`, not libsigchain's: on Android debuggerd lives in the linker, whose libc calls never pass through libsigchain; before the VM exists the two set the same kernel action anyway.
pub fn install_crash_handler() -> Result<()> {
    let soname = CString::new(lib_names::LIBC_SO).context("libc soname")?;
    // SAFETY: a NUL-terminated soname and the header's flag values; RTLD_NOLOAD only finds the libc already mapped
    let libc = unsafe { dlopen(soname.as_ptr(), dlfcn::RTLD_LAZY | dlfcn::RTLD_NOLOAD) };
    ensure!(!libc.is_null(), "finding libc: {}", dl_error());
    // SAFETY: sigaction_fn is the header's type of libc's sigaction; the handle is live (libc is never unloaded)
    let libc_sigaction: signal::sigaction_fn = unsafe { sym(libc, c"sigaction")? };
    let libc_sigaction = *LIBC_SIGACTION.get_or_init(|| libc_sigaction);
    let mut action = signal::sigaction {
        __sigaction_handler: signal::sigaction___sigaction_handler {
            sa_sigaction: Some(on_crash),
        },
        sa_mask: signal::__sigset_t { __val: [0; _] },
        // debuggerd_init: SA_RESTART | SA_SIGINFO | SA_ONSTACK (stack overflows reach it on the alternate stack); its SA_EXPOSE_TAGBITS only adds MTE tag bits to si_addr, which nothing here reads
        sa_flags: signal::SA_RESTART | signal::SA_SIGINFO | signal::SA_ONSTACK,
        sa_restorer: None,
    };
    // debuggerd_init blocks every signal while its handler runs
    // SAFETY: a valid set to fill
    let filled = unsafe { sigfillset(&mut action.sa_mask) };
    ensure!(filled == 0, "sigfillset failed");
    for sig in CRASH_SIGNALS {
        // SAFETY: a valid signal number and an initialised action whose handler has the SA_SIGINFO signature; the old action is not asked for
        let rc = unsafe { libc_sigaction(sig, &action, core::ptr::null_mut()) };
        ensure!(rc == 0, "sigaction({sig}) failed");
    }
    Ok(())
}

/// debuggerd's `resend_signal` for a crash: the signal's action back to SIG_DFL, then the same signal with the same siginfo queued to this thread (`rt_tgsigqueueinfo`), so the process dies of it and a parent's `waitpid` sees it.
/// The queued signal is blocked (the full mask) until the handler returns, and so is the faulting one when ART's handler chains here; on return it is delivered under SIG_DFL.
/// Not done: the dump. debuggerd forks crash_dump, which writes a tombstone through tombstoned; this host has neither, and ART's handler has already logged the signal, the fault address and its own backtrace.
/// Async-signal-safe only: libc's sigaction, getpid, gettid (a raw syscall) and syscall. errno is not restored as debuggerd does: no interrupted code runs after the resent signal is delivered.
extern "C" fn on_crash(sig: c_int, info: *mut signal::siginfo_t, _context: *mut c_void) {
    // set before any handler was installed: always present
    let Some(libc_sigaction) = LIBC_SIGACTION.get() else {
        return;
    };
    let default = signal::sigaction {
        // SIG_DFL is the null handler
        __sigaction_handler: signal::sigaction___sigaction_handler { sa_handler: None },
        sa_mask: signal::__sigset_t { __val: [0; _] },
        sa_flags: 0,
        sa_restorer: None,
    };
    let tid = rustix::thread::gettid().as_raw_nonzero().get();
    // SAFETY: a valid signal number and action. info is the kernel's (SA_SIGINFO; ART's handler passes on its own): rt_tgsigqueueinfo to the calling thread accepts it whatever its si_code
    unsafe {
        libc_sigaction(sig, &default, core::ptr::null_mut());
        syscall(
            c_long::from(sys::SYS_rt_tgsigqueueinfo),
            c_long::from(getpid()),
            c_long::from(tid),
            c_long::from(sig),
            info,
        );
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
