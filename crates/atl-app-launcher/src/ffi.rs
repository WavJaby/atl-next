// SPDX-License-Identifier: MIT
//! The C side: glibc's loader and signal API, the JNI invocation API, liblog and bionic's API, each function typed by the header that declares it (build.rs).
//! Each entry converts to safe values at once; the launcher's logic (`main`, `args`, `log`) stays safe.

use core::ffi::{CStr, c_char, c_int, c_long, c_void};
use std::borrow::Cow;
use std::ffi::CString;
use std::io::Write;
use std::os::fd::RawFd;
use std::os::unix::ffi::OsStringExt as _;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use anyhow::{Context, Result, anyhow, bail, ensure};

use crate::generated::{
    binder_process, dlfcn, invoked_vm, jni, jni_invocation, lib_names, libdl_android,
    log as liblog, looper, signal, syscall as sys, system_properties, unistd,
};
use crate::log::{self, Level, Record};

unsafe extern "C" {
    fn dlopen(filename: *const c_char, flag: c_int) -> *mut c_void;
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
    fn dlerror() -> *mut c_char;
    fn syscall(number: c_long, ...) -> c_long;
    fn getpid() -> c_int;
    fn sigfillset(set: *mut signal::__sigset_t) -> c_int;
    fn sigaltstack(stack: *const signal::stack_t, old: *mut signal::stack_t) -> c_int;
}
// the declarations above have the host headers' signatures
const _: dlfcn::dlopen = dlopen;
const _: dlfcn::dlsym = dlsym;
const _: dlfcn::dlerror = dlerror;
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
    property_get: system_properties::__system_property_get,
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
    /// Loads libnativehelper (`RTLD_LOCAL`: libraries ART loads later still bind the same liblog and libc provider, glibc deduplicating by soname; global scope stays glibc + libsigchain, main.rs) and resolves every function.
    pub fn load() -> Result<Self> {
        // SAFETY: a NUL-terminated soname and the header's flag values
        let h = unsafe { dlopen(NATIVEHELPER.as_ptr(), dlfcn::RTLD_NOW | dlfcn::RTLD_LOCAL) };
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
                property_get: sym(h, c"__system_property_get")?,
            })
        }
    }

    /// bionic `__system_properties_init`: true once the process's property area is mapped.
    pub fn has_property_area(&self) -> bool {
        // SAFETY: no arguments; callable any time (bionic maps the area or reports failure)
        unsafe { (self.properties_init)() == 0 }
    }

    /// bionic `__system_property_get`: the value of `name`, None when unset or empty.
    pub fn property(&self, name: &CStr) -> Option<String> {
        let mut value = [0 as c_char; system_properties::PROP_VALUE_MAX as usize];
        // SAFETY: a NUL-terminated name and a buffer of PROP_VALUE_MAX bytes, bionic's contract for `__system_property_get`
        let len = unsafe { (self.property_get)(name.as_ptr(), value.as_mut_ptr()) };
        if len <= 0 {
            return None;
        }
        // SAFETY: bionic wrote a NUL-terminated value into the buffer
        let text = unsafe { CStr::from_ptr(value.as_ptr()) }.to_string_lossy();
        (!text.is_empty()).then(|| text.into_owned())
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
    /// Loads libandroid_runtime (`RTLD_LOCAL`: libraries loaded later reach the same libbinder by soname, as `Runtime::load`) and resolves both functions.
    pub fn load() -> Result<Self> {
        // SAFETY: a NUL-terminated soname and the header's flag values
        let h = unsafe {
            dlopen(
                ANDROID_RUNTIME.as_ptr(),
                dlfcn::RTLD_NOW | dlfcn::RTLD_LOCAL,
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

/// libandroid's Looper API (NDK looper.h), loaded only in UI mode. libandroid_runtime does not link libandroid, so its handle cannot resolve these.
pub struct LooperApi {
    prepare: looper::ALooper_prepare,
    add_fd: looper::ALooper_addFd,
    remove_fd: looper::ALooper_removeFd,
}

/// libandroid by soname: found on the library path as libandroid_runtime is.
const ANDROID: &CStr = c"libandroid.so";

impl LooperApi {
    /// Loads libandroid (`RTLD_NOW | RTLD_LOCAL`, as `Runtime::load`) and resolves the three functions from its handle; the handle is never closed.
    pub fn load() -> Result<Self> {
        // SAFETY: a NUL-terminated soname and the header's flag values
        let h = unsafe { dlopen(ANDROID.as_ptr(), dlfcn::RTLD_NOW | dlfcn::RTLD_LOCAL) };
        ensure!(!h.is_null(), "loading libandroid: {}", dl_error());
        // SAFETY: each F is the generated type of the function of that name (build.rs `IMPORTS`); h is live
        unsafe {
            Ok(LooperApi {
                prepare: sym(h, c"ALooper_prepare")?,
                add_fd: sym(h, c"ALooper_addFd")?,
                remove_fd: sym(h, c"ALooper_removeFd")?,
            })
        }
    }

    /// The calling thread's Looper (`ALooper_prepare`, non-callback fds allowed). Called before Java's MessageQueue exists, so NativeMessageQueue adopts it (`Looper::getForThread`).
    pub fn prepare_looper(&self) -> Result<Looper> {
        let flags = looper::ALOOPER_PREPARE_ALLOW_NON_CALLBACKS as c_int;
        // SAFETY: the header's flag value; the returned Looper belongs to this thread (libutils keeps it for the thread's life)
        let ptr = unsafe { (self.prepare)(flags) };
        ensure!(!ptr.is_null(), "ALooper_prepare returned no Looper");
        Ok(Looper {
            ptr: ptr.cast(),
            add_fd: self.add_fd,
            remove_fd: self.remove_fd,
        })
    }
}

/// The callback `ALooper_addFd` takes: the signature of the generated `ALooper_addFd` (looper.h's `ALooper_callbackFunc` typedef is not generated, and this is the same type).
pub type Callback = Option<unsafe extern "C" fn(c_int, c_int, *mut c_void) -> c_int>;

/// A thread's Looper with the registration calls the GLib mirror makes. Thread-bound: only the thread that prepared it may register on it from its own callbacks.
#[derive(Clone, Copy)]
pub struct Looper {
    ptr: *mut c_void,
    add_fd: looper::ALooper_addFd,
    remove_fd: looper::ALooper_removeFd,
}

impl Looper {
    /// `ALooper_addFd` with a callback. `callback(fd, events, data)` runs on this thread inside the Looper's poll and returns 1 to stay registered; `data` stays valid until `remove_fd`.
    pub fn add_fd(
        &self,
        fd: RawFd,
        events: c_int,
        callback: Callback,
        data: *mut c_void,
    ) -> Result<()> {
        // SAFETY: this thread's live Looper; the callback is a function of this crate and `data` is the mirror, both valid until remove_fd
        let rc = unsafe {
            (self.add_fd)(
                self.ptr.cast(),
                fd,
                looper::ALOOPER_POLL_CALLBACK as c_int,
                events,
                callback,
                data,
            )
        };
        ensure!(rc == 1, "ALooper_addFd({fd}) returned {rc}");
        Ok(())
    }

    /// `ALooper_removeFd`: 1 when the fd was registered, 0 when it was not (both fine here).
    pub fn remove_fd(&self, fd: RawFd) -> Result<()> {
        // SAFETY: this thread's live Looper; removing an unregistered fd is a no-op
        let rc = unsafe { (self.remove_fd)(self.ptr.cast(), fd) };
        ensure!(rc == 0 || rc == 1, "ALooper_removeFd({fd}) returned {rc}");
        Ok(())
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
/// jni.h's reference and method-ID types as the generated table spells them (references are `void *` there).
type JClass = *mut c_void;
type JString = *mut c_void;
type JMethod = *mut jni::_jmethodID;
type JField = *mut jni::_jfieldID;
type JObject = *mut c_void;

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

/// The created VM's `JavaVM*`, which any thread may use (JNI invocation API: a JavaVM is shared between threads, a JNIEnv is not). Only the compositor's threads (feature `converter`) use it.
#[cfg(feature = "converter")]
#[derive(Clone, Copy)]
pub struct VmHandle(*mut *const jni::JNIInvokeInterface);

// SAFETY: the JNI specification makes the JavaVM pointer valid and usable on every thread for the VM's life, which is the process's
#[cfg(feature = "converter")]
unsafe impl Send for VmHandle {}
// SAFETY: as above: the invocation interface is thread-safe
#[cfg(feature = "converter")]
unsafe impl Sync for VmHandle {}

#[cfg(feature = "converter")]
impl VmHandle {
    /// The pointer; a method so a closure captures the whole (Send) handle, not its raw field.
    fn raw(self) -> *mut *const jni::JNIInvokeInterface {
        self.0
    }

    /// Runs `f` on a new thread called `name` that is attached to the VM (`AttachCurrentThread`, as libandroid_runtime's javaCreateThreadEtc makes a binder thread), detached when `f` returns.
    /// Native code that calls back into Java (libgui's SurfaceControl listeners) finds the JNIEnv of the thread it runs on.
    pub fn spawn_attached(self, name: &str, f: Box<dyn FnOnce() + Send>) -> std::io::Result<()> {
        std::thread::Builder::new()
            .name(name.to_owned())
            .spawn(move || {
                let vm = self.raw();
                let mut env: Env = core::ptr::null_mut();
                // SAFETY: the VM's own invoke table, valid for the process's life
                let (attach, detach) =
                    unsafe { ((**vm).AttachCurrentThread, (**vm).DetachCurrentThread) };
                let (Some(attach), Some(detach)) = (attach, detach) else {
                    let _ = writeln!(
                        std::io::stderr(),
                        "atl-app-launcher: the VM has no AttachCurrentThread"
                    );
                    return;
                };
                // SAFETY: a live VM, a writable out pointer, no thread arguments (a null name and group)
                if unsafe { attach(vm, &mut env, core::ptr::null_mut()) } != jni::JNI_OK {
                    let _ = writeln!(
                        std::io::stderr(),
                        "atl-app-launcher: AttachCurrentThread failed for {:?}",
                        std::thread::current().name()
                    );
                    return;
                }
                f();
                // SAFETY: attached above, from this thread
                unsafe { detach(vm) };
            })
            .map(|_| ())
    }
}

const EXIT_FAILURE: i32 = 1;

/// A package dir as AOSP's PackageLite describes it (`Vm::parse_cluster`): the values the installer prints, and the PackageLite object `install_natives` reads.
/// `lite` is a local reference of the main thread: valid until the VM is shut down, so the caller drops this first.
pub struct Cluster<'vm> {
    vm: &'vm Vm,
    lite: JObject,
    /// PackageLite.getPackageName.
    pub package: String,
    /// `getLongVersionCode`: versionCodeMajor << 32 | versionCode.
    pub version_code: i64,
    /// The split names in PackageLite's order (sorted by ApkLiteParseUtils).
    pub splits: Vec<String>,
}

/// AOSP's install-path decision for a package's native code (`Cluster::install_natives`).
pub struct Natives {
    /// `primaryCpuAbi`; None without native code.
    pub primary_abi: Option<String>,
    /// The dir the primary ABI's libraries were copied to; None when nothing was copied: no native code, or `extractNativeLibs` false (the libraries load from the APK).
    pub lib_dir: Option<PathBuf>,
}

/// PackageManager install codes of the native path (core/java/android/content/pm/PackageManager.java: 2122, 2258, 2485, 2518, 2528).
const INSTALL_FAILED_INVALID_APK: i32 = -2;
const INSTALL_FAILED_CPU_ABI_INCOMPATIBLE: i32 = -16;
const INSTALL_FAILED_INTERNAL_ERROR: i32 = -110;
const INSTALL_FAILED_NO_MATCHING_ABIS: i32 = -113;
const NO_NATIVE_LIBRARIES: i32 = -114;

/// The installer's error for an install code of the native path.
fn native_error(code: i32) -> anyhow::Error {
    let name = match code {
        INSTALL_FAILED_INVALID_APK => "INSTALL_FAILED_INVALID_APK",
        INSTALL_FAILED_CPU_ABI_INCOMPATIBLE => "INSTALL_FAILED_CPU_ABI_INCOMPATIBLE",
        INSTALL_FAILED_INTERNAL_ERROR => "INSTALL_FAILED_INTERNAL_ERROR",
        INSTALL_FAILED_NO_MATCHING_ABIS => "INSTALL_FAILED_NO_MATCHING_ABIS",
        _ => "unnamed",
    };
    anyhow!("native error {code}: {name}")
}

impl Cluster<'_> {
    /// Installer slice M2: the native code of this package dir on AOSP's install path, `PackageAbiHelperImpl.derivePackageAbi` (called at InstallPackageHelper.java:1952) for a non-multi-arch cluster install.
    /// `Handle.create(lite)`; the device's `Build.SUPPORTED_ABIS` (RenderScript bitcode on a 64-bit device: the 32-bit list, derivePackageAbi:481-494); with `extractNativeLibs` `copyNativeBinariesForSupportedAbi` into `lib_root/<isa>` (it finds the ABI and returns its index), else `findSupportedAbi` alone.
    /// `lib_root` stands for AOSP's cluster `<codePath>/lib` (deriveNativeLibraryPaths:209); its parent must exist (createNativeLibrarySubdir makes one level). Multi-arch packages are refused: their 32/64-bit branch depends on the aconfig flag forceMultiArchNativeLibsMatch.
    /// A no-match is `native error -113: INSTALL_FAILED_NO_MATCHING_ABIS`; a Java exception is described on stderr.
    pub fn install_natives(&self, lib_root: &Path) -> Result<Natives> {
        let vm = self.vm;
        let lite = vm.find_class(c"android/content/pm/parsing/PackageLite")?;
        let is_multi_arch = vm.method(lite, c"isMultiArch", c"()Z", false)?;
        ensure!(
            !vm.call_bool(self.lite, is_multi_arch, "PackageLite.isMultiArch")?,
            "multi-arch package: not in installer slice M2 (derivePackageAbi:387)"
        );
        let is_extract = vm.method(lite, c"isExtractNativeLibs", c"()Z", false)?;
        let extract = vm.call_bool(self.lite, is_extract, "PackageLite.isExtractNativeLibs")?;

        let handle_class =
            vm.find_class(c"com/android/internal/content/NativeLibraryHelper$Handle")?;
        let create = vm.method(
            handle_class,
            c"create",
            c"(Landroid/content/pm/parsing/PackageLite;)Lcom/android/internal/content/NativeLibraryHelper$Handle;",
            true,
        )?;
        let close = vm.method(handle_class, c"close", c"()V", false)?;
        let handle = NativeHandle {
            vm,
            obj: vm.call_static_object(
                handle_class,
                create,
                &[jni::jvalue { l: self.lite }],
                "NativeLibraryHelper.Handle.create",
            )?,
            close,
        };

        let helper = vm.find_class(c"com/android/internal/content/NativeLibraryHelper")?;
        let build = vm.find_class(c"android/os/Build")?;
        let string_array = c"[Ljava/lang/String;";
        let abis = vm.static_object_field(build, c"SUPPORTED_ABIS", string_array)?;
        let abis32 = vm.static_object_field(build, c"SUPPORTED_32_BIT_ABIS", string_array)?;
        let abis64 = vm.static_object_field(build, c"SUPPORTED_64_BIT_ABIS", string_array)?;
        let has_rs = vm.method(
            helper,
            c"hasRenderscriptBitcode",
            c"(Lcom/android/internal/content/NativeLibraryHelper$Handle;)Z",
            true,
        )?;
        // derivePackageAbi:481-494: RenderScript bitcode on a 64-bit device runs as 32-bit
        let (list, rs_override) = if vm.array_len(abis64)? > 0
            && vm.call_static_bool(
                helper,
                has_rs,
                &[jni::jvalue { l: handle.obj }],
                "hasRenderscriptBitcode",
            )? {
            if vm.array_len(abis32)? == 0 {
                return Err(native_error(INSTALL_FAILED_CPU_ABI_INCOMPATIBLE));
            }
            (abis32, true)
        } else {
            (abis, false)
        };

        let copy = if extract {
            let copy = vm.method(
                helper,
                c"copyNativeBinariesForSupportedAbi",
                c"(Lcom/android/internal/content/NativeLibraryHelper$Handle;Ljava/io/File;[Ljava/lang/String;ZZ)I",
                true,
            )?;
            let root = vm.file(lib_root)?;
            vm.call_static_int(
                helper,
                copy,
                &[
                    jni::jvalue { l: handle.obj },
                    jni::jvalue { l: root },
                    jni::jvalue { l: list },
                    jni::jvalue { z: 1 },
                    jni::jvalue { z: 0 },
                ],
                "copyNativeBinariesForSupportedAbi",
            )?
        } else {
            let find = vm.method(
                helper,
                c"findSupportedAbi",
                c"(Lcom/android/internal/content/NativeLibraryHelper$Handle;[Ljava/lang/String;)I",
                true,
            )?;
            vm.call_static_int(
                helper,
                find,
                &[jni::jvalue { l: handle.obj }, jni::jvalue { l: list }],
                "findSupportedAbi",
            )?
        };
        if copy < 0 && copy != NO_NATIVE_LIBRARIES {
            return Err(native_error(copy));
        }
        if copy >= 0 {
            let abi = vm.array_get(list, copy)?;
            let primary_abi = vm.string(abi)?.context("null ABI name")?;
            let lib_dir = if extract {
                let runtime = vm.find_class(c"dalvik/system/VMRuntime")?;
                let isa_of = vm.method(
                    runtime,
                    c"getInstructionSet",
                    c"(Ljava/lang/String;)Ljava/lang/String;",
                    true,
                )?;
                let isa = vm.call_static_object(
                    runtime,
                    isa_of,
                    &[jni::jvalue { l: abi }],
                    "VMRuntime.getInstructionSet",
                )?;
                Some(lib_root.join(vm.string(isa)?.context("null instruction set")?))
            } else {
                None
            };
            return Ok(Natives {
                primary_abi: Some(primary_abi),
                lib_dir,
            });
        }
        if rs_override {
            // no native code, RenderScript: abiList[0] (derivePackageAbi:522-523)
            let first = vm.array_get(list, 0)?;
            return Ok(Natives {
                primary_abi: Some(vm.string(first)?.context("null ABI name")?),
                lib_dir: None,
            });
        }
        Ok(Natives {
            primary_abi: None,
            lib_dir: None,
        })
    }
}

/// A `NativeLibraryHelper.Handle`, closed when it goes out of scope (AOSP's callers close it with `IoUtils.closeQuietly`).
struct NativeHandle<'vm> {
    vm: &'vm Vm,
    obj: JObject,
    close: JMethod,
}

impl Drop for NativeHandle<'_> {
    fn drop(&mut self) {
        // a close that fails leaves an exception `check` clears; a destructor has nowhere to report it
        let _ = self.vm.call_void(self.obj, self.close, "Handle.close");
    }
}

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

    /// A pending Java exception, described on stderr (`ExceptionDescribe`) and cleared, as an error naming `what`.
    fn check(&self, what: &str) -> Result<()> {
        // SAFETY: the main thread's env; checking, describing and clearing are allowed with an exception pending
        unsafe {
            if jni_call!(self.env, ExceptionCheck) != 0 {
                jni_call!(self.env, ExceptionDescribe);
                jni_call!(self.env, ExceptionClear);
                bail!("{what}: Java exception (described on stderr)");
            }
        }
        Ok(())
    }

    fn find_class(&self, name: &CStr) -> Result<JClass> {
        // SAFETY: the main thread's env, a NUL-terminated class name
        let c = unsafe { jni_call!(self.env, FindClass, name.as_ptr()) };
        self.check(&format!("FindClass {}", name.to_string_lossy()))?;
        ensure!(!c.is_null(), "no class {}", name.to_string_lossy());
        Ok(c)
    }

    /// The method `name` + `sig` of `class`: static when `is_static`.
    fn method(&self, class: JClass, name: &CStr, sig: &CStr, is_static: bool) -> Result<JMethod> {
        // SAFETY: the main thread's env, a live class reference, NUL-terminated name and signature
        let m = unsafe {
            if is_static {
                jni_call!(
                    self.env,
                    GetStaticMethodID,
                    class,
                    name.as_ptr(),
                    sig.as_ptr()
                )
            } else {
                jni_call!(self.env, GetMethodID, class, name.as_ptr(), sig.as_ptr())
            }
        };
        self.check(&format!(
            "method {}{}",
            name.to_string_lossy(),
            sig.to_string_lossy()
        ))?;
        ensure!(!m.is_null(), "no method {}", name.to_string_lossy());
        Ok(m)
    }

    /// A Java `String` as UTF-8; null -> None.
    fn string(&self, s: JString) -> Result<Option<String>> {
        if s.is_null() {
            return Ok(None);
        }
        // SAFETY: a live String reference; the chars are released below, after the copy
        unsafe {
            let chars = jni_call!(self.env, GetStringUTFChars, s, core::ptr::null_mut());
            ensure!(!chars.is_null(), "GetStringUTFChars failed");
            let text = CStr::from_ptr(chars).to_string_lossy().into_owned();
            jni_call!(self.env, ReleaseStringUTFChars, s, chars);
            Ok(Some(text))
        }
    }

    /// The static object field `name` (type `sig`) of `class`.
    fn static_object_field(&self, class: JClass, name: &CStr, sig: &CStr) -> Result<JObject> {
        let what = format!("field {}", name.to_string_lossy());
        // SAFETY: the main thread's env, a live class reference, NUL-terminated name and signature
        let field: JField = unsafe {
            jni_call!(
                self.env,
                GetStaticFieldID,
                class,
                name.as_ptr(),
                sig.as_ptr()
            )
        };
        self.check(&what)?;
        ensure!(!field.is_null(), "no field {}", name.to_string_lossy());
        // SAFETY: `field` is the static field of `class` just resolved, read on the same env
        let value = unsafe { jni_call!(self.env, GetStaticObjectField, class, field) };
        self.check(&what)?;
        Ok(value)
    }

    /// `class.method(args)`: a static method returning an int.
    fn call_static_int(
        &self,
        class: JClass,
        method: JMethod,
        args: &[jni::jvalue],
        what: &str,
    ) -> Result<i32> {
        // SAFETY: the main thread's env; `method` is a static method of `class` whose signature `args` match
        let value =
            unsafe { jni_call!(self.env, CallStaticIntMethodA, class, method, args.as_ptr()) };
        self.check(what)?;
        Ok(value)
    }

    /// As `call_static_int`, returning a boolean.
    fn call_static_bool(
        &self,
        class: JClass,
        method: JMethod,
        args: &[jni::jvalue],
        what: &str,
    ) -> Result<bool> {
        // SAFETY: as `call_static_int`
        let value = unsafe {
            jni_call!(
                self.env,
                CallStaticBooleanMethodA,
                class,
                method,
                args.as_ptr()
            )
        };
        self.check(what)?;
        Ok(value != 0)
    }

    /// As `call_static_int`, returning an object.
    fn call_static_object(
        &self,
        class: JClass,
        method: JMethod,
        args: &[jni::jvalue],
        what: &str,
    ) -> Result<JObject> {
        // SAFETY: as `call_static_int`
        let value = unsafe {
            jni_call!(
                self.env,
                CallStaticObjectMethodA,
                class,
                method,
                args.as_ptr()
            )
        };
        self.check(what)?;
        Ok(value)
    }

    /// `obj.method()`: an instance method with no arguments, returning a boolean.
    fn call_bool(&self, obj: JObject, method: JMethod, what: &str) -> Result<bool> {
        let none = core::ptr::null::<jni::jvalue>();
        // SAFETY: the main thread's env, a live object of the method's class, no arguments
        let value = unsafe { jni_call!(self.env, CallBooleanMethodA, obj, method, none) };
        self.check(what)?;
        Ok(value != 0)
    }

    /// `obj.method()`: an instance method with no arguments, returning void.
    fn call_void(&self, obj: JObject, method: JMethod, what: &str) -> Result<()> {
        let none = core::ptr::null::<jni::jvalue>();
        // SAFETY: as `call_bool`
        unsafe { jni_call!(self.env, CallVoidMethodA, obj, method, none) };
        self.check(what)
    }

    /// The length of a Java array.
    fn array_len(&self, array: JObject) -> Result<i32> {
        // SAFETY: the main thread's env and a live array reference
        Ok(unsafe { jni_call!(self.env, GetArrayLength, array) })
    }

    /// Element `index` of an object array.
    fn array_get(&self, array: JObject, index: i32) -> Result<JObject> {
        ensure!(
            (0..self.array_len(array)?).contains(&index),
            "array index {index} out of range"
        );
        // SAFETY: the main thread's env, a live object array, an index below its length
        let element = unsafe { jni_call!(self.env, GetObjectArrayElement, array, index) };
        self.check("array element")?;
        Ok(element)
    }

    /// `new File(path)`.
    fn file(&self, path: &Path) -> Result<JObject> {
        let text = CString::new(path.as_os_str().as_encoded_bytes()).context("path with a NUL")?;
        let class = self.find_class(c"java/io/File")?;
        let init = self.method(class, c"<init>", c"(Ljava/lang/String;)V", false)?;
        // SAFETY: the main thread's env, a NUL-terminated path, File's String constructor
        unsafe {
            let jpath = jni_call!(self.env, NewStringUTF, text.as_ptr());
            self.check("NewStringUTF")?;
            let file = jni_call!(
                self.env,
                NewObjectA,
                class,
                init,
                [jni::jvalue { l: jpath }].as_ptr()
            );
            self.check("new File")?;
            Ok(file)
        }
    }

    /// Installer slice M1: `ApkLiteParseUtils.parseClusterPackageLite(ParseTypeImpl.forParsingWithoutPlatformCompat().reset(), new File(dir), 0)`, which reads every APK in `dir` through ApkAssets and refuses a split whose package or versionCode differs.
    /// `forParsingWithoutPlatformCompat`: no IPlatformCompat service in this process; a deferred error's change counts as enabled by targetSdk alone (ParseTypeImpl). Flags 0: no certificates collected.
    /// A ParseResult error -> `parse error <code>: <message>` (PackageManager INSTALL_PARSE_FAILED_*).
    pub fn parse_cluster(&self, dir: &Path) -> Result<Cluster<'_>> {
        let file = self.file(dir)?;
        let parse_type = self.find_class(c"android/content/pm/parsing/result/ParseTypeImpl")?;
        let for_parsing = self.method(
            parse_type,
            c"forParsingWithoutPlatformCompat",
            c"()Landroid/content/pm/parsing/result/ParseTypeImpl;",
            true,
        )?;
        let reset = self.method(
            parse_type,
            c"reset",
            c"()Landroid/content/pm/parsing/result/ParseInput;",
            false,
        )?;
        let utils = self.find_class(c"android/content/pm/parsing/ApkLiteParseUtils")?;
        let parse = self.method(
            utils,
            c"parseClusterPackageLite",
            c"(Landroid/content/pm/parsing/result/ParseInput;Ljava/io/File;I)Landroid/content/pm/parsing/result/ParseResult;",
            true,
        )?;
        let result_class = self.find_class(c"android/content/pm/parsing/result/ParseResult")?;
        let is_error = self.method(result_class, c"isError", c"()Z", false)?;
        let error_code = self.method(result_class, c"getErrorCode", c"()I", false)?;
        let error_message = self.method(
            result_class,
            c"getErrorMessage",
            c"()Ljava/lang/String;",
            false,
        )?;
        let get_result = self.method(result_class, c"getResult", c"()Ljava/lang/Object;", false)?;
        let lite_class = self.find_class(c"android/content/pm/parsing/PackageLite")?;
        let package_name = self.method(
            lite_class,
            c"getPackageName",
            c"()Ljava/lang/String;",
            false,
        )?;
        let version_code = self.method(lite_class, c"getLongVersionCode", c"()J", false)?;
        let split_names = self.method(
            lite_class,
            c"getSplitNames",
            c"()[Ljava/lang/String;",
            false,
        )?;
        let none = core::ptr::null::<jni::jvalue>();
        // SAFETY: JNI calls on the main thread's env with the classes and method IDs resolved above, each with the argument list its signature names; every result is checked (`check`, null) before use
        unsafe {
            let input = jni_call!(
                self.env,
                CallStaticObjectMethodA,
                parse_type,
                for_parsing,
                none
            );
            self.check("ParseTypeImpl.forParsingWithoutPlatformCompat")?;
            let input = jni_call!(self.env, CallObjectMethodA, input, reset, none);
            self.check("ParseTypeImpl.reset")?;
            let args = [
                jni::jvalue { l: input },
                jni::jvalue { l: file },
                jni::jvalue { i: 0 },
            ];
            let result = jni_call!(
                self.env,
                CallStaticObjectMethodA,
                utils,
                parse,
                args.as_ptr()
            );
            self.check("ApkLiteParseUtils.parseClusterPackageLite")?;
            ensure!(!result.is_null(), "parseClusterPackageLite returned null");
            if jni_call!(self.env, CallBooleanMethodA, result, is_error, none) != 0 {
                let code = jni_call!(self.env, CallIntMethodA, result, error_code, none);
                let message = jni_call!(self.env, CallObjectMethodA, result, error_message, none);
                self.check("ParseResult error")?;
                bail!(
                    "parse error {code}: {}",
                    self.string(message)?.unwrap_or_default()
                );
            }
            let lite = jni_call!(self.env, CallObjectMethodA, result, get_result, none);
            self.check("ParseResult.getResult")?;
            ensure!(!lite.is_null(), "ParseResult holds no PackageLite");
            let name = jni_call!(self.env, CallObjectMethodA, lite, package_name, none);
            self.check("PackageLite.getPackageName")?;
            let package = self
                .string(name)?
                .context("PackageLite has no package name")?;
            let version_code = jni_call!(self.env, CallLongMethodA, lite, version_code, none);
            self.check("PackageLite.getLongVersionCode")?;
            let names = jni_call!(self.env, CallObjectMethodA, lite, split_names, none);
            self.check("PackageLite.getSplitNames")?;
            let mut splits = Vec::new();
            if !names.is_null() {
                let len = jni_call!(self.env, GetArrayLength, names);
                for i in 0..len {
                    let s = jni_call!(self.env, GetObjectArrayElement, names, i);
                    self.check("splitNames element")?;
                    splits.push(self.string(s)?.context("null split name")?);
                    jni_call!(self.env, DeleteLocalRef, s);
                }
            }
            Ok(Cluster {
                vm: self,
                lite,
                package,
                version_code,
                splits,
            })
        }
    }

    /// The VM, for threads other than this one.
    #[cfg(feature = "converter")]
    pub fn handle(&self) -> VmHandle {
        VmHandle(self.vm)
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

/// The runtime's one C entry, what the loader executable calls (main.rs): the process's argv -> its exit status, which the loader ends the process with (`_exit`, ART's FastExit).
/// This library's std buffers its own stdout: flushed here, the loader's std cannot.
///
/// # Safety
/// `argv` holds `argc` NUL-terminated strings.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn atl_app_main(argc: c_int, argv: *const *const c_char) -> c_int {
    // SAFETY: the caller's contract, passed on
    let status = crate::main(unsafe { c_args(argc, argv) });
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
    status
}

/// The installer's C entry (bin/atl-installer.rs loads it as atl-app-launcher loads `atl_app_main`), with the same contract.
///
/// # Safety
/// `argv` holds `argc` NUL-terminated strings.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn atl_installer_main(argc: c_int, argv: *const *const c_char) -> c_int {
    // SAFETY: the caller's contract, passed on
    let status = crate::installer::main(unsafe { c_args(argc, argv) });
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
    status
}

/// A C entry's argv as `OsString`s.
///
/// # Safety
/// `argv` holds `argc` NUL-terminated strings.
unsafe fn c_args(argc: c_int, argv: *const *const c_char) -> Vec<std::ffi::OsString> {
    (0..usize::try_from(argc).unwrap_or(0))
        .map(|i| {
            // SAFETY: the caller's contract: argc strings, each NUL-terminated
            let a = unsafe { CStr::from_ptr(*argv.add(i)) };
            std::ffi::OsString::from_vec(a.to_bytes().to_vec())
        })
        .collect()
}
