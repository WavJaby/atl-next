// SPDX-License-Identifier: MIT
//! Generates with atl-bridgegen, from AOSP's `nativebridge/native_bridge.h` parsed in the host's (glibc) view -- the NB core lives in ART's process -- `OUT_DIR/native_bridge.rs`:
//! `NativeBridgeCallbacks` and the records it passes with clang's sizes and offsets asserted, and its entry shells (`Table`): one `unsafe extern "C"` function per field with the header's exact signature, panic-shielded, calling `crate::exports::<field>`, plus `NativeBridgeCallbacks::shells()` with `version` = `crate::exports::VERSION` (bridge controls 1 and 2, plan S11).
//! A field added by an AOSP upgrade becomes a shell calling a function `exports` lacks: a compile error, not a silent null entry.

use std::path::PathBuf;

use anyhow::{Context, Result, ensure};
use atl_bridgegen::buildscript::{self, Upstreams};
use atl_bridgegen::ir::Ty;
use atl_bridgegen::jni::{LaneMap, Route};
use atl_bridgegen::rust::{Table, api_module_with_table};
use atl_bridgegen::{Lang, Source, Target};

/// The interface header, relative to the AOSP tree.
const HEADER: &str = "art/libnativebridge/include/nativebridge/native_bridge.h";
/// Where its `#include "jni.h"` resolves (libnativebridge's `export_header_lib_headers: ["jni_headers"]`).
const INCLUDE_DIRS: &[&str] = &["libnativehelper/include_jni"];

/// What a panic in an entry answers: the failure value each entry documents (native_bridge.h) -- false, null, or for `unloadLibrary` non-zero.
fn on_panic(field: &str, ret: &Ty) -> Result<String> {
    Ok(match (field, ret.resolved()) {
        ("unloadLibrary", _) => "-1",
        // a fn-pointer return: no handler
        ("getSignalHandler", _) => "None",
        (_, Ty::Pointer { is_const: true, .. }) => "::core::ptr::null()",
        (_, Ty::Pointer { .. }) => "::core::ptr::null_mut()",
        (_, Ty::Bool) => "false",
        _ => anyhow::bail!("{field}: no panic answer for {ret:?}"),
    }
    .to_owned())
}

/// What a guest thread is made of, relative to the AOSP tree: the thread record (`pthread_internal_t`) and its per-thread libc state (`bionic_tls`) -- the engine builds one per guest thread, as the NBS linker's `__libc_init_main_thread_early` reuses `__native_bridge_get_host_pthread()`'s answer as both -- and the static-TLS description the linker hands the host (`__native_bridge_config_static_tls`).
const THREAD_HEADERS: &[&str] = &[
    "bionic/libc/bionic/pthread_internal.h",
    // pthread_attr_t: LP64 guests' intercepted pthread API hands it to glibc unconverted (engine/intercepts.rs size gate); the arm guest's is read by this view
    "bionic/libc/include/bits/pthread_types.h",
    // __pthread_cleanup_t: thread.rs `detach` walks the record's cleanup_stack (bionic pthread_exit's cleanup step)
    "bionic/libc/include/pthread.h",
    // struct sigaction / sigset_t: guest sigaction's host side converts them (engine/signal.rs layout gate)
    "bionic/libc/include/bits/signal_types.h",
    "bionic/libc/private/bionic_tls.h",
    // TLS_SLOT_NATIVE_BRIDGE_GUEST_STATE
    "bionic/libc/platform/bionic/tls_defines.h",
    // struct addrinfo / hostent and the AI_ / NI_ / EAI_ values: the resolver's host side converts or gates them (engine/netdb.rs)
    "bionic/libc/include/netdb.h",
    "frameworks/libs/native_bridge_support/android_api/linker/include/native_bridge_support/linker/static_tls_config.h",
];
/// Their include directories: `bionic/…`, `private/…`, `platform/bionic/…` (bionic/libc), `async_safe/log.h`, `native_bridge_support/linker/…`.
const THREAD_INCLUDE_DIRS: &[&str] = &[
    "bionic/libc",
    "bionic/libc/async_safe/include",
    "frameworks/libs/native_bridge_support/android_api/linker/include",
];

/// `layout_module` of `THREAD_HEADERS` in the guest's bionic view: `guest` is the host arch for the identity engine, arm64 for the QEMU guest.
/// C++: the bionic headers are C++-only (classes, `_Atomic` next to `<atomic>`), parsed with libc++ first as
/// bionic's own C++ sources are.
fn bionic_thread(
    up: &Upstreams,
    host: &buildscript::ArchRow,
    out: &std::path::Path,
) -> Result<String> {
    let mut target =
        buildscript::bionic_libc_target(up, host, Some(&buildscript::libcxx_include_dir()?));
    target.args.extend(
        THREAD_INCLUDE_DIRS
            .iter()
            .map(|d| format!("-I{}", up.aosp(d).display())),
    );
    let unit = out.join(format!("bionic_thread_{}.cc", host.arch));
    std::fs::write(
        &unit,
        "#include \"bionic/pthread_internal.h\"\n#include \"native_bridge_support/linker/static_tls_config.h\"\n#include <netdb.h>\n",
    )?;
    let also: Vec<PathBuf> = THREAD_HEADERS.iter().map(|h| up.aosp(h)).collect();
    let src = Source {
        path: &unit,
        lang: Lang::Cxx,
        also: &also,
        // the guest TCB slot that holds the engine's per-thread record (x86_64 crossing: guest->host and signal entries find it there); the slot range (the QEMU crossing's scratch TCB)
        macros: &[
            "TLS_SLOT_NATIVE_BRIDGE_GUEST_STATE",
            "MIN_TLS_SLOT",
            "MAX_TLS_SLOT",
            // engine/intercepts.rs: the arm guest's own bionic attr (NBS patch 0012), its detach bit
            "PTHREAD_ATTR_FLAG_DETACHED",
            // engine/netdb.rs: the flags bionic's getaddrinfo accepts (AI_MASK) and getnameinfo's, and every error code
            "AI_MASK",
            "AI_PASSIVE",
            "AI_CANONNAME",
            "AI_NUMERICHOST",
            "AI_NUMERICSERV",
            "AI_ADDRCONFIG",
            "NI_NOFQDN",
            "NI_NUMERICHOST",
            "NI_NAMEREQD",
            "NI_NUMERICSERV",
            "NI_DGRAM",
            "EAI_ADDRFAMILY",
            "EAI_AGAIN",
            "EAI_BADFLAGS",
            "EAI_FAIL",
            "EAI_FAMILY",
            "EAI_MEMORY",
            "EAI_NODATA",
            "EAI_NONAME",
            "EAI_SERVICE",
            "EAI_SOCKTYPE",
            "EAI_SYSTEM",
            "EAI_OVERFLOW",
        ],
    };
    let iface = atl_bridgegen::interface(
        "bionic_thread",
        "bionic/libc/{bionic/pthread_internal.h,private/bionic_tls.h} + native_bridge_support static_tls_config.h",
        &src,
        std::slice::from_ref(&target),
    )
    .context("bionic thread record")?;
    atl_bridgegen::rust::layout_module(&iface, &host.arch)
}

/// The guest libc stubs whose host is the process's bionic provider (`libatl_bionic`: the bionic-only API glibc lacks, implemented with bionic's signature from these same headers, atl-bionic build.rs `APIS`), by header relative to the AOSP tree.
/// The one list: the binding table binds these names to the provider's own handle (`Host::Provider`), except the [`SHIMS`] among them; `provider.rs` types them by the header's signatures.
const PROVIDER_APIS: &[(&str, &[&str])] = &[
    (
        "sys/system_properties.h",
        &[
            "__system_properties_init",
            "__system_property_area_serial",
            "__system_property_find",
            "__system_property_foreach",
            "__system_property_get",
            "__system_property_read",
            "__system_property_read_callback",
            "__system_property_serial",
            "__system_property_set",
            "__system_property_wait",
        ],
    ),
    (
        "android/set_abort_message.h",
        &["android_set_abort_message"],
    ),
    // a platform header: bionic/libc/platform
    ("bionic/malloc.h", &["android_mallopt"]),
];
const PROVIDER_FILES: &[&str] = &[
    "bionic/libc/include/sys/system_properties.h",
    "bionic/libc/include/android/set_abort_message.h",
    "bionic/libc/platform/bionic/malloc.h",
];

/// `provider.rs`: `NAMES` (every `PROVIDER_APIS` function) and per function its fn-pointer type from the guest's bionic view (`import_module`), parsed as bionic's C++ sources are.
fn provider(up: &Upstreams, host: &buildscript::ArchRow, out: &std::path::Path) -> Result<String> {
    // the platform level, as atl-bionic reads them: `__system_property_read_callback` sits behind an availability guard (26)
    let mut target = buildscript::bionic_platform_target(
        up,
        host,
        buildscript::future_level(up, host)?,
        Some(&buildscript::libcxx_include_dir()?),
    );
    target
        .args
        .push(format!("-I{}", up.aosp("bionic/libc/platform").display()));
    let unit = out.join("provider.cc");
    let includes: String = PROVIDER_APIS
        .iter()
        .map(|(h, _)| format!("#include <{h}>\n"))
        .collect();
    std::fs::write(&unit, includes)?;
    let also: Vec<PathBuf> = PROVIDER_FILES.iter().map(|h| up.aosp(h)).collect();
    let src = Source {
        path: &unit,
        lang: Lang::Cxx,
        also: &also,
        macros: &[],
    };
    let iface = atl_bridgegen::interface(
        "provider",
        "bionic sys/system_properties.h + android/set_abort_message.h + bionic/malloc.h",
        &src,
        std::slice::from_ref(&target),
    )
    .context("provider API")?;
    let functions: Vec<&str> = PROVIDER_APIS
        .iter()
        .flat_map(|(_, f)| f.iter().copied())
        .collect();
    atl_bridgegen::rust::import_module(&iface, &host.arch, &functions, &[])
}

/// Guest libc stubs with a hand-written host side (the guest hands host code a guest function, or the host side owns a table): resolved by `bindings::shim`'s one match, its test proving every name here has an arm.
const SHIMS: &[&str] = &[
    // intercepts.rs: start routine / key destructor run on the guest thread pointer
    "pthread_create",
    "pthread_key_create",
    "pthread_key_delete",
    // intercepts.rs: reports the guest stack of the thread, not glibc's view of a bionic record
    "pthread_getattr_np",
    // tr-first blocker 4, intercepts.rs: a guest pthread_t is the thread's bionic record (thread.rs `THREADS`), mapped to glibc's handle; join and detach also end the record's life, gettid_np reads it (glibc has none)
    "pthread_join",
    "pthread_detach",
    "pthread_gettid_np",
    // intercepts.rs: ends the start routine's guest call (`Engine::end_call`)
    "pthread_exit",
    // intercepts.rs: the guest destructor wrapped for glibc's thread-exit list, as NBS cxa_trampolines.cc wraps it for the host's
    "native_bridge___cxa_thread_atexit_impl",
    // intercepts.rs `BY_HANDLE`: the handle mapped, then glibc's function
    "pthread_getcpuclockid",
    "pthread_getname_np",
    "pthread_getschedparam",
    "pthread_kill",
    "pthread_setname_np",
    "pthread_setschedparam",
    "pthread_setschedprio",
    "pthread_sigqueue",
    // engine/netdb.rs: struct addrinfo and the AI_ / NI_ / EAI_ values differ between bionic and glibc, so these convert (layouts generated from bionic's netdb.h); both engines (identity's layout equals the QEMU guest's, netdb.rs asserts it)
    "getaddrinfo",
    "freeaddrinfo",
    "getnameinfo",
    // engine/netdb.rs: glibc's static hostent copied into the guest's view per thread (window memory: a 32-bit guest cannot hold glibc's pointers). h_errno stays glibc's slot: the `__get_h_errno` row
    "gethostbyname",
    "gethostbyaddr",
    // engine/netdb.rs: bionic's own text (build.rs `gai_errlist_rs`), no host call
    "gai_strerror",
    // provider.rs: the provider calls a host trampoline that crosses to the guest callback
    "__system_property_foreach",
    "__system_property_read_callback",
];
/// Shims of the x86_64 stub list only (`libc_stubs`): signal.rs registers the guest handler with sigchain behind ART's claims.
const SIGNAL_SHIMS: &[&str] = &["sigaction", "sigaction64"];
/// Q5-8: the QEMU table's libc rows that bind to counted shims under `crossing-count` (counters.rs `memcpy`, `memset`, `memcmp`), which record the size argument and then run glibc's.
const COUNTED_SHIMS: &[&str] = &["memcpy", "memset", "memcmp"];
/// Proxy-library functions whose host side is hand-written, each bound `Entry::Shim` in its library; each must be in one proxy library's list (`bindings_table`). `slCreateEngine` (engine/sles.rs: its out object is wrapped in a guest proxy); `eglGetProcAddress` (engine/proc.rs: its answer is a row's guest address, never a raw host pointer; the fn-return arm below traps every other one); `android_getaddrinfofornetwork` (engine/netdb.rs: the host libandroid answers glibc's `struct addrinfo`, whose ai_addr / ai_canonname order is bionic's swapped -- the identity classifier names it `record:addrinfo`). A shim converts its own row, so the identity check skips it; an ILP32 row still traps on its kinds first.
const PROXY_SHIMS: &[&str] = &[
    "slCreateEngine",
    "eglGetProcAddress",
    "android_getaddrinfofornetwork",
];
/// [`PROXY_SHIMS`] that convert an ILP32 guest's row themselves, bound with the guest view's lane classes: `android_getaddrinfofornetwork` (engine/netdb.rs builds the list in the guest's view).
const ILP32_PROXY_SHIMS: &[&str] = &["android_getaddrinfofornetwork"];
/// Callback typedefs whose guest function pointer a lane-mapped row hands the host as a down shell (`Entry::Mapped`, engine/ndk.rs `call_mapped`; GS2.2 callback-param, tr-first blocker 5). Reviewed: every argument the host passes the callback is a value, the guest's own user data, or a host buffer the guest only loads from or stores to (QEMU's page table holds no host heap, so a host buffer must never reach a guest syscall or be fetched as code). A function-pointer parameter of another type traps `pending` (`atl_bridgegen::jni::route`); a name here no row uses is stale.
/// Not here, with why: `AMediaDataSourceReadAt` (its buffer is host memory the guest writes: needs the G2 copy arena), `GLDEBUGPROC*` (its message is host memory: G2 staging).
const CALLBACKS: &[&str] = &[
    // looper.h `int (*)(int fd, int events, void* data)`
    "ALooper_callbackFunc",
    // choreographer.h `void (*)(long frameTimeNanos, void* data)`, `void (*)(int64_t frameTimeNanos, void* data)`, `void (*)(int64_t vsyncPeriodNanos, void* data)`
    "AChoreographer_frameCallback",
    "AChoreographer_frameCallback64",
    // choreographer.h `void (*)(const AChoreographerFrameCallbackData*, void* data)`: the data is opaque in the NDK (declared, never defined) -- the guest only hands it back to the AChoreographerFrameCallbackData_* rows, which are host rows, so the host pointer crosses unchanged (LP64: same width). Lifetime: a host stack object of libgui Choreographer::dispatchCallbacks, valid only during the callback; AOSP's accessors abort outside it (AChoreographer.cpp "Data is only valid in callback"), so no bridge-side lifetime is kept. ILP32 guests: the pointer needs a handle (P4b s4), the classifier traps those rows
    "AChoreographer_vsyncCallback",
    "AChoreographer_refreshRateCallback",
    // NdkMediaDataSource.h `ssize_t (*)(void* userdata)`, `void (*)(void* userdata)`
    "AMediaDataSourceGetSize",
    "AMediaDataSourceClose",
    // AAudio.h `aaudio_data_callback_result_t (*)(AAudioStream*, void* userData, void* audioData, int32_t numFrames)`: unlike ReadAt the guest only stores into `audioData` (the track's buffer) and never hands it to a syscall, and a QEMU guest's loads and stores go straight to host addresses (only syscall buffers and code fetch need QEMU's page table); `AAudioStream*` is opaque, passed back to rows unchanged
    "AAudioStream_dataCallback",
    // AAudio.h `void (*)(AAudioStream*, void* userData, aaudio_result_t error)`, `void (*)(AAudioStream*, void* userData)`
    "AAudioStream_errorCallback",
    "AAudioStream_presentationEndCallback",
];
/// NDK functions with JNI types in their prototype, bound lane-mapped (`Entry::Mapped`, engine/ndk.rs `call_mapped`: the guest's `JNIEnv*` mapped to the host's, the rest passed as int lanes; references are the host's already). A generic row would hand the host library the guest's env (tr-first blocker 3: `ANativeWindow_fromSurface` -> SEGV in `android_view_Surface_getSurface`), so a JNI-typed prototype missing here fails the build naming it (`atl_bridgegen::jni::route`), and a name here without one is stale.
const ENV_SHIMS: &[&str] = &[
    "AAssetManager_fromJava",
    "AHardwareBuffer_fromHardwareBuffer",
    "AHardwareBuffer_toHardwareBuffer",
    "AInputEvent_toJava",
    "AInputQueue_fromJava",
    "AInputTransferToken_fromJava",
    "AInputTransferToken_toJava",
    "AKeyEvent_fromJava",
    "AMotionEvent_fromJava",
    "ANativeWindow_fromSurface",
    "ANativeWindow_toSurface",
    "APerformanceHint_borrowSessionFromJava",
    "ASharedMemory_dupFromJava",
    "ASurfaceControl_fromJava",
    "ASurfaceTexture_fromSurfaceTexture",
    "ASurfaceTransaction_fromJava",
    "AndroidBitmap_getDataSpace",
    "AndroidBitmap_getHardwareBuffer",
    "AndroidBitmap_getInfo",
    "AndroidBitmap_lockPixels",
    "AndroidBitmap_unlockPixels",
];

/// Guest libc stubs glibc's `libc.so.6` serves with the same ABI, by (stub name, glibc symbol). NBS names the allocator entries `native_bridge_<fn>` (one heap: the guest libc's malloc calls them).
/// The pthread rows rest on intercepts.rs's size gates (`pthread_key_t` int, `pthread_attr_t` only ever filled and read by glibc since every `pthread_attr_*` is here); none takes a `pthread_t`, which is the guest's own handle ([`SHIMS`]); the rest are the C contract (memory, allocation, time), identical in bionic and glibc on LP64.
const GLIBC: &[(&str, &str)] = &[
    ("memcpy", "memcpy"),
    ("memset", "memset"),
    ("memcmp", "memcmp"),
    ("native_bridge_malloc", "malloc"),
    ("native_bridge_free", "free"),
    ("native_bridge_calloc", "calloc"),
    ("native_bridge_realloc", "realloc"),
    ("native_bridge_memalign", "memalign"),
    ("native_bridge_posix_memalign", "posix_memalign"),
    ("native_bridge_aligned_alloc", "aligned_alloc"),
    ("native_bridge_malloc_usable_size", "malloc_usable_size"),
    // NBS exit.c: the guest's `exit` runs its `__cxa_finalize`, then this; the host `exit` (upstream's trampoline target) runs the process's own handlers
    ("native_bridge_exit", "exit"),
    ("clock_gettime", "clock_gettime"),
    ("clock_getres", "clock_getres"),
    ("gettimeofday", "gettimeofday"),
    ("time", "time"),
    ("pthread_attr_destroy", "pthread_attr_destroy"),
    ("pthread_attr_getdetachstate", "pthread_attr_getdetachstate"),
    ("pthread_attr_getguardsize", "pthread_attr_getguardsize"),
    (
        "pthread_attr_getinheritsched",
        "pthread_attr_getinheritsched",
    ),
    ("pthread_attr_getschedparam", "pthread_attr_getschedparam"),
    ("pthread_attr_getschedpolicy", "pthread_attr_getschedpolicy"),
    ("pthread_attr_getscope", "pthread_attr_getscope"),
    ("pthread_attr_getstack", "pthread_attr_getstack"),
    ("pthread_attr_getstacksize", "pthread_attr_getstacksize"),
    ("pthread_attr_init", "pthread_attr_init"),
    ("pthread_attr_setdetachstate", "pthread_attr_setdetachstate"),
    ("pthread_attr_setguardsize", "pthread_attr_setguardsize"),
    (
        "pthread_attr_setinheritsched",
        "pthread_attr_setinheritsched",
    ),
    ("pthread_attr_setschedparam", "pthread_attr_setschedparam"),
    ("pthread_attr_setschedpolicy", "pthread_attr_setschedpolicy"),
    ("pthread_attr_setscope", "pthread_attr_setscope"),
    ("pthread_attr_setstack", "pthread_attr_setstack"),
    ("pthread_attr_setstacksize", "pthread_attr_setstacksize"),
    ("pthread_getspecific", "pthread_getspecific"),
    ("pthread_setspecific", "pthread_setspecific"),
    // NBS patch 0009 stubs it on both lists. bionic's h_errno is *__get_h_errno(), the guest's own resolver slot, which the gethostby* shims never set; glibc's location is the one slot both sides use (the guest dereferences it, as it does the netdb lists)
    ("__get_h_errno", "__h_errno_location"),
];

/// The provider's soname, as cargo-c names a cdylib (`lib<crate>.so.<v>`): `<v>` is the version's semver-compatible prefix -- the major from 1.0, `0.<minor>` below it, the whole `0.0.<patch>` below that (measured: `libatl_bionic.so.0.0.0`, readelf SONAME). Read from atl-bionic's own manifest so a version bump moves it.
fn provider_soname(manifest_dir: &std::path::Path) -> Result<String> {
    const CRATE: &str = "atl_bionic";
    let path = manifest_dir.join("../atl-bionic/Cargo.toml");
    buildscript::rerun_if_changed(&path);
    let text = std::fs::read_to_string(&path).with_context(|| path.display().to_string())?;
    let version = text
        .lines()
        .find_map(|l| l.strip_prefix("version = \"")?.strip_suffix('"'))
        .with_context(|| format!("{}: no version line", path.display()))?;
    let parts: Vec<&str> = version.split('.').collect();
    ensure!(
        parts.len() == 3,
        "{}: version {version} is not x.y.z",
        path.display()
    );
    let v = match (parts[0], parts[1]) {
        ("0", "0") => version.to_owned(),
        ("0", minor) => format!("0.{minor}"),
        (major, _) => major.to_owned(),
    };
    Ok(format!("lib{CRATE}.so.{v}"))
}

/// bionic's `ai_errlist` entries: EAI_* 0 (success) through EAI_MAX (dns/net/getaddrinfo.c). One text per value, EAI_MAX's "Unknown error" last.
const GAI_ERRLIST_LEN: usize = 16;

/// `gai_errlist.rs`: `GAI_ERRLIST`, bionic's `ai_errlist` text, read from the pinned source so it is not a copy. One C string per array entry, in order; the count must be [`GAI_ERRLIST_LEN`], since a missing entry would shift every later text. netdb.rs checks the index of each code it uses against the header value.
fn gai_errlist_rs(up: &Upstreams) -> Result<String> {
    const REL: &str = "bionic/libc/dns/net/getaddrinfo.c";
    let path = up.aosp(REL);
    buildscript::rerun_if_changed(&path);
    let text = std::fs::read_to_string(&path).with_context(|| path.display().to_string())?;
    let head = "static const char * const ai_errlist[] = {";
    let body = text
        .find(head)
        .with_context(|| format!("{REL}: no ai_errlist"))?
        + head.len();
    let end = body
        + text[body..]
            .find("};")
            .with_context(|| format!("{REL}: ai_errlist not closed"))?;
    let mut texts = Vec::new();
    for line in text[body..end].lines() {
        let Some(open) = line.find('"') else {
            continue;
        };
        let rest = &line[open + 1..];
        let close = rest
            .find('"')
            .with_context(|| format!("{REL}: unterminated text: {line}"))?;
        let t = &rest[..close];
        ensure!(!t.contains('\\'), "{REL}: escape in ai_errlist text: {t}");
        texts.push(t);
    }
    ensure!(
        texts.len() == GAI_ERRLIST_LEN,
        "{REL}: ai_errlist has {} texts, bionic's EAI_* 0..=EAI_MAX is {GAI_ERRLIST_LEN}",
        texts.len()
    );
    let mut out = format!(
        "// generated by build.rs from {REL} (ai_errlist), do not edit\npub(super) const GAI_ERRLIST: [&std::ffi::CStr; {GAI_ERRLIST_LEN}] = [\n"
    );
    for t in &texts {
        out.push_str(&format!("    c{t:?},\n"));
    }
    out.push_str("];\n");
    Ok(out)
}

/// The JNI function tables, relative to the AOSP tree: `JNINativeInterface` (behind `JNIEnv`) and `JNIInvokeInterface` (behind `JavaVM`).
const JNI_HEADER: &str = "libnativehelper/include_jni/jni.h";
/// Their records, with the Rust module each one's slots go to.
const JNI_TABLES: &[(&str, &str)] = &[("JNINativeInterface", "env"), ("JNIInvokeInterface", "vm")];

/// The OpenSL ES headers (opensl-es plan 2.2 item 1), relative to the AOSP tree: every `SL<X>Itf_` record of them is a proxy object table (`atl_bridgegen::sles`). `OpenSLES_Android.h` includes the rest by `"..."` and `<jni.h>`.
const SLES_DIR: &str = "frameworks/wilhelm/include/SLES";
const SLES_HEADER: &str = "OpenSLES_Android.h";
const SLES_ALSO: &[&str] = &["OpenSLES.h", "OpenSLES_AndroidConfiguration.h"];

/// Every proxy object table (opensl-es plan 2.2 item 1), in registry order: the JNI tables, then the SL interfaces sorted by record. Each is (record, module, the parsed header's host view it comes from).
fn object_tables(
    up: &Upstreams,
    target: &Target,
) -> Result<Vec<(String, String, atl_bridgegen::ir::ArchView)>> {
    let jni = jni_interface(up, target)?;
    let jni_view = jni
        .arches
        .get(&target.arch)
        .context("jni.h: no view of the host arch")?;
    let mut out: Vec<_> = JNI_TABLES
        .iter()
        .map(|(record, module)| ((*record).to_owned(), (*module).to_owned(), jni_view.clone()))
        .collect();
    let sles = sles_interface(up, target)?;
    let view = sles
        .arches
        .get(&target.arch)
        .context("OpenSLES.h: no view of the host arch")?;
    for record in atl_bridgegen::sles::tables(view)? {
        let module = atl_bridgegen::sles::module(&record)
            .with_context(|| format!("{record}: no module name"))?;
        out.push((record, module, view.clone()));
    }
    Ok(out)
}

/// The parsed OpenSL ES headers in the host view, with wilhelm's include dirs and jni.h's.
fn sles_interface(up: &Upstreams, target: &Target) -> Result<atl_bridgegen::ir::Interface> {
    let dir = up.aosp(SLES_DIR);
    let path = dir.join(SLES_HEADER);
    let also: Vec<PathBuf> = SLES_ALSO.iter().map(|h| dir.join(h)).collect();
    let mut args = target.args.clone();
    args.push(format!("-I{}", dir.display()));
    args.push(format!(
        "-I{}",
        up.aosp("libnativehelper/include_jni").display()
    ));
    let t = Target {
        arch: target.arch.clone(),
        triple: target.triple.clone(),
        args,
    };
    let src = Source {
        path: &path,
        lang: Lang::C,
        also: &also,
        macros: &[],
    };
    atl_bridgegen::interface("sles", SLES_HEADER, &src, std::slice::from_ref(&t))
        .context(SLES_HEADER)
}

/// `object_tables.rs`: per table, `SLOTS` (field name by slot, in header order) and one `usize` constant per field = its slot; every field a pointer-sized slot at slot * pointer size, else the build fails (the engine's tables and its stub array index by slot). Then `TABLES` (name and slots of each, registry order) and `SL_IIDS` (an SL table's interface-ID symbol and its `TABLES` index).
fn object_tables_rs(tables: &[(String, String, atl_bridgegen::ir::ArchView)]) -> Result<String> {
    let mut out = format!(
        "// generated by build.rs from {JNI_HEADER} and {SLES_DIR}/{SLES_HEADER} (host view), do not edit\n"
    );
    for (index, (record, module, view)) in tables.iter().enumerate() {
        let rec = view
            .records
            .get(record.as_str())
            .with_context(|| format!("no record {record}"))?;
        let word = rec.align;
        ensure!(
            rec.size == word * rec.fields.len() as u64,
            "{record}: size {} is not {} slots of {word} bytes",
            rec.size,
            rec.fields.len()
        );
        out.push_str(&format!(
            "/// `{record}` slots; `TABLE` = its registry index.\n#[allow(non_upper_case_globals, dead_code)]\npub mod {module} {{\n    pub const TABLE: usize = {index};\n    pub const SLOTS: [&str; {}] = [\n",
            rec.fields.len()
        ));
        for f in &rec.fields {
            out.push_str(&format!("        {:?},\n", f.name));
        }
        out.push_str("    ];\n");
        for (slot, f) in rec.fields.iter().enumerate() {
            ensure!(
                f.offset == slot as u64 * word
                    && matches!(f.ty.resolved(), Ty::Pointer { bits, .. } if u64::from(*bits) == word * 8),
                "{record}.{}: not a pointer at slot {slot} (offset {}, {:?})",
                f.name,
                f.offset,
                f.ty
            );
            out.push_str(&format!("    pub const {}: usize = {slot};\n", f.name));
        }
        out.push_str("}\n");
    }
    out.push_str(&format!("/// Every table: name and slots, in registry order (`TABLES[i]` is table `i`).\npub const TABLES: [(&str, &[&str]); {}] = [\n", tables.len()));
    for (record, module, _) in tables {
        out.push_str(&format!("    ({record:?}, &{module}::SLOTS),\n"));
    }
    out.push_str("];\n");
    let iids: Vec<(String, usize)> = tables
        .iter()
        .enumerate()
        .filter_map(|(i, (record, _, _))| Some((atl_bridgegen::sles::iid(record)?, i)))
        .collect();
    out.push_str(&format!("/// SL interface IDs: the `SL_IID_*` symbol of each SL table and its `TABLES` index.\npub const SL_IIDS: [(&str, usize); {}] = [\n", iids.len()));
    for (iid, i) in iids {
        out.push_str(&format!("    ({iid:?}, {i}),\n"));
    }
    out.push_str("];\n");
    Ok(out)
}

/// The parsed jni.h in the host view: the one description of the JNI rows of `object_tables`.
fn jni_interface(up: &Upstreams, target: &Target) -> Result<atl_bridgegen::ir::Interface> {
    let path = up.file(&format!("aosp/{JNI_HEADER}"))?;
    let src = Source {
        path: &path,
        lang: Lang::C,
        also: &[],
        macros: &[],
    };
    atl_bridgegen::interface("jni", JNI_HEADER, &src, std::slice::from_ref(target))
        .context(JNI_HEADER)
}

/// `object_lanes.rs`: per table, each slot's lane (classes of its fixed parameters and result, variadic flag) from `atl_bridgegen::jni`, in header field order, then `LANES` by registry index. A parameter with no lane class fails the build.
fn object_lanes_rs(tables: &[(String, String, atl_bridgegen::ir::ArchView)]) -> Result<String> {
    let mut lanes = Vec::new();
    for (record, module, view) in tables {
        let l =
            atl_bridgegen::jni::lanes(view, record).with_context(|| format!("{record}: lanes"))?;
        lanes.push((module.as_str(), l));
    }
    let mut out = format!(
        "// generated by build.rs from {JNI_HEADER} and {SLES_DIR}/{SLES_HEADER} (the QEMU guests' bionic views), do not edit\n{}",
        atl_bridgegen::jni::rust(&lanes)
    );
    out.push_str(&format!("/// Every table's lanes, by registry index (object_tables.rs `TABLES`).\npub const LANES: [&[Option<Lane>]; {}] = [\n", tables.len()));
    for (_, module, _) in tables {
        out.push_str(&format!("    &{module}::LANES,\n"));
    }
    out.push_str("];\n");
    Ok(out)
}

/// The guest proxy libraries (NBS `libnative_bridge_guest_*`): soname, and the NBS directory whose `stubs_arm64.cc` (arch-neutral, LP64) is the library's symbol set.
/// The NDK libraries (libnativewindow, libandroid, libandroid_runtime, libjnigraphics: C3; libmediandk: G6b): a stub gets a binding only when its prototype is in [`PROTOTYPE_HEADERS`]; the rest trap (`missing_prototype`).
const PROXY_LIBS: &[(&str, &str)] = &[
    ("libEGL.so", "libEGL"),
    ("libGLESv1_CM.so", "libGLESv1_CM"),
    ("libGLESv2.so", "libGLESv2"),
    ("libGLESv3.so", "libGLESv3"),
    ("libnativewindow.so", "libnativewindow"),
    ("libandroid.so", "libandroid"),
    ("libandroid_runtime.so", "libandroid_runtime"),
    ("libjnigraphics.so", "libjnigraphics"),
    // opensl-es O1: three functions (slCreateEngine a shim) and the SL_IID_* variables (`Entry::Variable`)
    ("libOpenSLES.so", "libOpenSLES"),
    // G6b: every Unity libunity.so NEEDs it; the host side is AOSP's libmediandk (gate_goals `NDK_PROXIED`)
    ("libmediandk.so", "libmediandk"),
    // aaudio-guest-route: Unity FMOD's AAudio output; host side AOSP libaaudio (gate_goals `NDK_FRONTS`, `NDK_PROXIED`)
    ("libaaudio.so", "libaaudio"),
];
/// The guest libc's stub list (NBS) of guest ISA `isa`: x86_64 for the identity guest (patch 0003 adds sigaction / sigaction64); the QEMU guests' have no sigaction stub (their guest libc's own syscall reaches QEMU's signal emulation) and, with patch 0006, no setjmp-family stubs (bionic's setjmp.S saves and restores guest registers in the guest). arm's also lacks `clock_*` / `gettimeofday` / `time` / `__get_h_errno`.
fn libc_stubs(isa: &str) -> String {
    format!("frameworks/libs/native_bridge_support/android_api/libc/stubs_{isa}.cpp")
}

/// The proxy libraries' stub list of a guest: NBS ships one per guest ISA it runs (`stubs_<isa>.cc`); the identity guest has none of its own and takes the LP64 arm64 list (arch-neutral at LP64).
fn proxy_stubs(dir: &str, isa: &str) -> String {
    format!("frameworks/libs/native_bridge_support/android_api/{dir}/stubs_{isa}.cc")
}

/// The QEMU guests a table is generated for, by arches.toml row, and the table's module (engine/bindings.rs, engine/qemu/arch.rs `Arch::table`): one per `Arch` of the QEMU engine.
const QEMU_GUESTS: &[(&str, &str)] = &[("arm64", "qemu_arm64"), ("arm", "qemu_arm")];

/// Handle typedefs (ILP32 classifier, atl_bridgegen::ilp32 `Views::handles`): typedef names that spell an opaque host handle as `void*`, so the two views cannot tell a handle from a buffer. One row per name, with why it is a handle; each must be declared by the guest view and used by some row of the arm table (build fails otherwise).
const HANDLE_TYPEDEFS: &[(&str, &str)] = &[
    (
        "EGLDisplay",
        "egl.h: a display connection the driver hands out",
    ),
    ("EGLConfig", "egl.h: a driver config handle"),
    ("EGLContext", "egl.h: a driver context handle"),
    ("EGLSurface", "egl.h: a driver surface handle"),
    (
        "EGLClientBuffer",
        "egl.h: a buffer handle (AHardwareBuffer*, a GL name cast) the driver interprets",
    ),
    ("EGLImage", "egl.h: a driver image handle"),
    ("EGLImageKHR", "eglext.h: a driver image handle"),
    ("EGLSync", "egl.h: a driver sync handle"),
    ("EGLSyncKHR", "eglext.h: a driver sync handle"),
    ("EGLSyncNV", "eglext.h: a driver fence-sync handle"),
    ("EGLStreamKHR", "eglext.h: a driver stream handle"),
    ("EGLDeviceEXT", "eglext.h: a driver device handle"),
    (
        "EGLOutputLayerEXT",
        "eglext.h: a driver output-layer handle",
    ),
    ("EGLOutputPortEXT", "eglext.h: a driver output-port handle"),
    (
        "EGLLabelKHR",
        "eglext.h: an app label the driver hands back unchanged",
    ),
    ("EGLObjectKHR", "eglext.h: any EGL object handle"),
    (
        "EGLNativeDisplayType",
        "eglplatform.h (android): a native display, EGL_DEFAULT_DISPLAY = 0",
    ),
    ("GLeglImageOES", "gl2ext.h: an EGLImage handle passed to GL"),
    (
        "GLeglClientBufferEXT",
        "gl2ext.h: an EGLClientBuffer handle passed to GL",
    ),
];
/// The prototypes of the proxy libraries: EGL and the GLES 1.x / 2.x / 3.x headers with their extensions (AOSP's Khronos headers), parsed once per view.
const PROTOTYPE_HEADERS: &[&str] = &[
    "EGL/egl.h",
    "EGL/eglext.h",
    "GLES/gl.h",
    "GLES/glext.h",
    "GLES2/gl2.h",
    "GLES2/gl2ext.h",
    "GLES3/gl3.h",
    "GLES3/gl31.h",
    "GLES3/gl32.h",
    // opensl-es O1: the libOpenSLES functions and the layouts sles.rs reads by pointer ([`SL_RECORDS`])
    "SLES/OpenSLES.h",
    "SLES/OpenSLES_Android.h",
];
/// The NDK headers of the NDK proxies (libandroid, libnativewindow, libjnigraphics: C3; libmediandk: G6b), by include directory relative to the AOSP tree: each directory is an include path of both views, each header is included by the prototype unit and its declarations count (bindings_table `also`).
/// Every public header of those libraries' NDK surface, so a stub gets a row whenever its prototype exists, not only the ones an app was seen importing (tr-first blocker 2). Left out: `hardware_buffer_aidl.h` and `native_window_aidl.h` (C++ inline helpers over libbinder's Parcel, no function of the proxies' stub lists).
const NDK_HEADERS: &[(&str, &[&str])] = &[
    (
        "frameworks/native/include",
        &[
            "android/asset_manager.h",
            "android/asset_manager_jni.h",
            "android/bitmap.h",
            "android/choreographer.h",
            "android/configuration.h",
            "android/display_luts.h",
            "android/font.h",
            "android/font_matcher.h",
            "android/hardware_buffer_jni.h",
            "android/imagedecoder.h",
            "android/input.h",
            "android/input_transfer_token_jni.h",
            "android/keycodes.h",
            "android/looper.h",
            "android/multinetwork.h",
            "android/native_activity.h",
            "android/native_window_jni.h",
            "android/obb.h",
            "android/performance_hint.h",
            "android/permission_manager.h",
            "android/sensor.h",
            "android/sharedmem.h",
            "android/sharedmem_jni.h",
            "android/storage_manager.h",
            "android/surface_control.h",
            "android/surface_control_input_receiver.h",
            "android/surface_control_jni.h",
            "android/surface_texture.h",
            "android/surface_texture_jni.h",
            "android/system_fonts.h",
            "android/system_health.h",
            "android/thermal.h",
            "android/trace.h",
            "android/window.h",
        ],
    ),
    (
        "frameworks/native/libs/nativewindow/include",
        &[
            "android/data_space.h",
            "android/hardware_buffer.h",
            "android/hdr_metadata.h",
            "android/native_window.h",
        ],
    ),
    ("frameworks/native/libs/arect/include", &["android/rect.h"]),
    (
        "frameworks/av/media/ndk/include",
        &[
            "media/NdkImage.h",
            "media/NdkImageReader.h",
            "media/NdkMediaCodec.h",
            "media/NdkMediaCodecInfo.h",
            "media/NdkMediaCodecStore.h",
            "media/NdkMediaCrypto.h",
            "media/NdkMediaDataSource.h",
            "media/NdkMediaDrm.h",
            "media/NdkMediaError.h",
            "media/NdkMediaExtractor.h",
            "media/NdkMediaFormat.h",
            "media/NdkMediaMuxer.h",
        ],
    ),
    // AAudio: Unity FMOD's first output (dlopen libaaudio.so); host side AOSP libaaudio (gate_goals `NDK_FRONTS`)
    (
        "frameworks/av/media/libaaudio/include",
        &["aaudio/AAudio.h"],
    ),
];
/// bionic's availability header, copied into the host view's compat dir ([`host_compat`]).
const VERSIONING_H: &str = "bionic/libc/include/android/versioning.h";

/// The host view's compat dir, as the converter gives every host compile of AOSP code (bp2ninja cc.go `deviceCompat`, `libcCompat`, `deviceForced`): bionic's `android/versioning.h` and atl-bionic's include-free `android/api-level.h` (`buildscript::API_LEVEL`, generated in bionic's view). Returns the include args: the dir, both forced in the converter's order, and Soong's weak-availability define (every declaration visible, `__INTRODUCED_IN` an availability attribute clang ignores on linux-gnu). NDK media headers include `<android/api-level.h>` and use `__INTRODUCED_IN` without a fallback.
fn host_compat(
    up: &Upstreams,
    host: &buildscript::ArchRow,
    out: &std::path::Path,
) -> Result<Vec<String>> {
    let dir = out.join("host-compat");
    let android = dir.join("android");
    std::fs::create_dir_all(&android)?;
    let target =
        buildscript::bionic_platform_target(up, host, buildscript::future_level(up, host)?, None);
    let header = std::path::Path::new("capi/include/android/api-level.h");
    let (_, text) = buildscript::api_files(up, &buildscript::API_LEVEL, target, "crate")?
        .into_iter()
        .find(|(p, _)| p == header)
        .context("API_LEVEL printed no android/api-level.h")?;
    std::fs::write(android.join("api-level.h"), text)?;
    std::fs::copy(up.aosp(VERSIONING_H), android.join("versioning.h"))?;
    for p in up.rerun_paths(&format!("aosp/{VERSIONING_H}")) {
        buildscript::rerun_if_changed(&p);
    }
    Ok(vec![
        format!("-I{}", dir.display()),
        "-D__ANDROID_UNAVAILABLE_SYMBOLS_ARE_WEAK__".to_owned(),
        "-include".to_owned(),
        "android/versioning.h".to_owned(),
        "-include".to_owned(),
        "android/api-level.h".to_owned(),
    ])
}
/// The SL headers' declarations count from [`SLES_DIR`] (bindings_table `also`).
const SL_HEADERS: &[&str] = &[
    "OpenSLES.h",
    "OpenSLES_Android.h",
    "OpenSLES_AndroidConfiguration.h",
    "OpenSLES_Platform.h",
    "OpenSLES_AndroidMetadata.h",
];
/// The SL structs the host reads in guest memory (opensl-es plan 2.2 item 7, AGENTS Bridges (2)): each must be identical in the guest and host views.
const SL_RECORDS: &[&str] = &[
    "SLInterfaceID_",
    "SLDataSource_",
    "SLDataSink_",
    "SLDataLocator_OutputMix",
    "SLDataLocator_BufferQueue",
    "SLDataLocator_AndroidSimpleBufferQueue",
    "SLDataFormat_PCM_",
    "SLAndroidDataFormat_PCM_EX_",
    "SLEngineOption_",
];
/// Include paths of both views besides the [`NDK_HEADERS`] directories.
const PROTOTYPE_INCLUDES: &[&str] = &[
    "frameworks/native/opengl/include",
    "libnativehelper/include_jni",
    "frameworks/wilhelm/include",
];
/// Env var naming a header force-included into the guest prototype view only: the identity gate's deliberate break.
const GUEST_INCLUDE_BREAK: &str = "ATL_NB_GL_GUEST_INCLUDE";
/// Functions whose prototype alone does not say what the binding does: `glGetPointerv` answers a wrapped GL_DEBUG_CALLBACK_FUNCTION, `glGetVkProcAddrNV` is refused (GS2.2).
const ANNOTATED: &[&str] = &["glGetPointerv", "glGetPointervKHR", "glGetVkProcAddrNV"];

/// Every `.h` under `dir`, recursively, in sorted order.
fn headers_under(dir: &std::path::Path, out: &mut Vec<PathBuf>) -> Result<()> {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .with_context(|| dir.display().to_string())?
        .map(|e| e.map(|e| e.path()))
        .collect::<std::io::Result<_>>()?;
    entries.sort();
    for p in entries {
        if p.is_dir() {
            headers_under(&p, out)?;
        } else if p.extension().is_some_and(|x| x == "h") {
            out.push(p);
        }
    }
    Ok(())
}

/// A fn-pointer type: a pointer to a function.
fn is_fn_ptr(t: &Ty) -> bool {
    matches!(t.resolved(), Ty::Pointer { pointee, .. } if matches!(pointee.resolved(), Ty::Function(_)))
}

/// The stub names of one NBS stub list (`DEFINE_INTERCEPTABLE_STUB_FUNCTION(name);`), in list order.
fn stub_names(up: &Upstreams, rel: &str) -> Result<Vec<String>> {
    let path = up.aosp(rel);
    let text = std::fs::read_to_string(&path).with_context(|| path.display().to_string())?;
    let names: Vec<String> = text
        .lines()
        .filter_map(|l| {
            l.strip_prefix("DEFINE_INTERCEPTABLE_STUB_FUNCTION(")?
                .strip_suffix(");")
        })
        .map(str::to_owned)
        .collect();
    ensure!(!names.is_empty(), "no stubs in {}", path.display());
    for p in up.rerun_paths(&format!("aosp/{rel}")) {
        buildscript::rerun_if_changed(&p);
    }
    Ok(names)
}

/// An ILP32 guest's libc stub that has a host side on LP64 but no prototype in [`LIBC_HEADERS`] to classify: trapped, no lanes to place it by.
const ILP32_LIBC: &str = "pending: ilp32 libc, no prototype";
/// An ILP32 guest's libc stub whose guest and host bionic prototypes differ in a way the classifier has no kind for (`ilp32_rows.txt` names it).
const ILP32_LIBC_UNCLASSIFIED: &str = "pending: ilp32 libc, unclassified difference";
/// An ILP32 guest's libc shim whose host side reads or writes guest memory by the LP64 layout (a `pthread_t*`, a guest attr, an `addrinfo`): trapped until it converts by the guest's layout and is listed in [`ILP32_SHIMS`].
const ILP32_SHIM: &str = "pending: ilp32 shim, LP64 guest layout";
/// Shims (of [`SHIMS`]) whose host side handles a 32-bit guest's layout itself (reviewed per name), so an ILP32 table binds them whatever their kinds: each reads the guest's words by the guest's width.
const ILP32_SHIMS: &[&str] = &[
    // counters.rs: size recorded, then glibc's on the same arguments (a size_t widened by its lanes)
    "memcpy",
    "memset",
    "memcmp",
    // intercepts.rs: the arm guest's bionic attr read by its generated view, `*thread` / `*value` written as one guest word, the start routine a lane value; getattr_np writes the arm attr's six words
    "pthread_create",
    "pthread_join",
    "pthread_getattr_np",
    // intercepts.rs: an int key and a guest function, both lane values
    "pthread_key_create",
    "pthread_key_delete",
    // intercepts.rs: a guest pthread_t is its bionic record's address, a window address below 4 GiB, zero-extended by its lane
    "pthread_detach",
    "pthread_gettid_np",
    "pthread_exit",
    // intercepts.rs: a guest function and its argument, both lane values, kept as words; the dso handle unused
    "native_bridge___cxa_thread_atexit_impl",
    // intercepts.rs `BY_HANDLE`: the handle mapped, the rest lane values or pointers to ints, chars and `sched_param` (one int on both)
    "pthread_getcpuclockid",
    "pthread_getname_np",
    "pthread_getschedparam",
    "pthread_kill",
    "pthread_setname_np",
    "pthread_setschedparam",
    "pthread_setschedprio",
    "pthread_sigqueue",
    // engine/netdb.rs: every structure the guest receives is built in its view (thread.rs `layout::qemu_arm`) in window memory, and the guest's hints read in it; the `socklen_t` (int32 vs uint32: classifier refuses) and `size_t` lanes are read as 32-bit values
    "getaddrinfo",
    "freeaddrinfo",
    "getnameinfo",
    "gai_strerror",
    "gethostbyname",
    "gethostbyaddr",
];
/// Headers declaring the libc stubs' prototypes, parsed for the guest's bionic view and the host arch's bionic view (the LP64 contract every host side -- glibc row, provider, shim -- implements); `bionic/libc/platform` for `bionic/malloc.h`.
const LIBC_HEADERS: &[&str] = &[
    "string.h",
    "pthread.h",
    "netdb.h",
    "sys/system_properties.h",
    "android/set_abort_message.h",
    "bionic/malloc.h",
    // `exit` (NBS `native_bridge_exit`, upstream json header "stdlib.h"), `pthread_kill` / `pthread_sigqueue` (`sigval`)
    "stdlib.h",
    "signal.h",
];
/// The prototype name of libc stub `name`: NBS names a stub whose guest-libc wrapper keeps the public name `native_bridge_<public>` (exit.c `exit` -> `native_bridge_exit`; the trampoline json's `custom_thunk` for the malloc family), so its prototype is the public function's.
fn libc_prototype_name(name: &str) -> &str {
    name.strip_prefix("native_bridge_").unwrap_or(name)
}
/// NBS's own declaration of `__cxa_thread_atexit_impl` (no bionic header declares it; NBS `proxy/cxa_trampolines.cc` does, for its custom trampoline), taken from that file: `extern "C" int __cxa_thread_atexit_impl(...);`.
fn cxa_thread_atexit_decl(up: &Upstreams) -> Result<String> {
    const REL: &str =
        "frameworks/libs/native_bridge_support/android_api/libc/proxy/cxa_trampolines.cc";
    let path = up.aosp(REL);
    buildscript::rerun_if_changed(&path);
    let text = std::fs::read_to_string(&path).with_context(|| path.display().to_string())?;
    let start = text
        .find("extern \"C\" int __cxa_thread_atexit_impl(")
        .with_context(|| format!("{REL}: no __cxa_thread_atexit_impl declaration"))?;
    let end = start
        + text[start..]
            .find(';')
            .context("unterminated __cxa_thread_atexit_impl declaration")?;
    Ok(format!("{}\n", &text[start..=end]))
}

/// The libc stubs' prototypes in two bionic views, `guest` (the guest's row) and `host` (the host arch's row): an ILP32 table classifies each bound libc row by them ([`libc_entries`]).
fn libc_prototypes(
    up: &Upstreams,
    guest: &buildscript::ArchRow,
    host: &buildscript::ArchRow,
    out: &std::path::Path,
) -> Result<atl_bridgegen::ir::Interface> {
    let view = |row: &buildscript::ArchRow, name: &str| -> Result<Target> {
        let mut t = buildscript::bionic_platform_target(
            up,
            row,
            buildscript::future_level(up, row)?,
            Some(&buildscript::libcxx_include_dir()?),
        );
        t.args
            .push(format!("-I{}", up.aosp("bionic/libc/platform").display()));
        t.arch = name.to_owned();
        Ok(t)
    };
    let unit = out.join(format!("libc_prototypes_{}.cc", guest.arch));
    let mut text: String = LIBC_HEADERS
        .iter()
        .map(|h| format!("#include <{h}>\n"))
        .collect();
    text.push_str(&cxa_thread_atexit_decl(up)?);
    std::fs::write(&unit, text)?;
    // declarations count only from the headers themselves (`Source::also`), each in bionic's public or platform include dir
    let mut also = Vec::new();
    for h in LIBC_HEADERS {
        let path = ["bionic/libc/include", "bionic/libc/platform"]
            .iter()
            .map(|d| up.aosp(&format!("{d}/{h}")))
            .find(|p| p.is_file())
            .with_context(|| format!("{h}: not in bionic's include or platform dir"))?;
        buildscript::rerun_if_changed(&path);
        also.push(path);
    }
    let src = Source {
        path: &unit,
        lang: Lang::Cxx,
        also: &also,
        macros: DATA_MODEL,
    };
    atl_bridgegen::interface(
        "libc_prototypes",
        "bionic libc headers, guest and host views",
        &src,
        &[view(guest, "guest")?, view(host, "host")?],
    )
    .context("libc prototypes")
}

/// An ILP32 libc row's entry for a stub `name` whose LP64 entry is `lp64` (a `Generic` or `Shim` literal without lanes, `{sig}` where they go): bound with the guest view's lanes when every position is carried by its lane class (a pointer result included: dispatch.rs refuses one at or above 4 GiB, counted) or the shim converts itself ([`ILP32_SHIMS`]); else trapped with its heaviest kind's reason, or [`ILP32_LIBC`] without a prototype. Also its `ilp32_rows.txt` row.
fn ilp32_libc_entry(
    name: &str,
    lp64: std::result::Result<String, &'static str>,
    protos: &atl_bridgegen::ir::Interface,
    rows: &mut Ilp32Rows,
    trap: &mut dyn FnMut(&str, &'static str) -> String,
) -> Result<String> {
    let guest = protos.arches.get("guest").context("no guest view")?;
    let host = protos.arches.get("host").context("no host view")?;
    // deliberate break (`ATL_NB_BREAK=main_exit_unbound`): native_bridge_exit keeps its stub name, which no header declares
    let proto = if name == "native_bridge_exit"
        && selected_breaks()?.contains(&"atl_nb_main_exit_unbound")
    {
        name
    } else {
        libc_prototype_name(name)
    };
    let (Some(g), Some(h)) = (guest.functions.get(proto), host.functions.get(proto)) else {
        rows.push((
            "libc.so".to_owned(),
            name.to_owned(),
            "no prototype".to_owned(),
            "libc-unprototyped",
        ));
        return Ok(match lp64 {
            Ok(_) => trap(name, ILP32_LIBC),
            Err(why) => trap(name, why),
        });
    };
    let views = atl_bridgegen::ilp32::Views {
        guest,
        host,
        handles: &[],
    };
    // a difference the classifier has no kind for (bionic arm `socklen_t` is int32_t, x86_64's uint32_t: gethostbyaddr), or a by-value aggregate with no lane class (pthread_sigqueue's `union sigval`), traps by name, the row recorded
    let classified = atl_bridgegen::ilp32::classify(name, g, h, &views, &mut Default::default())
        .and_then(|row| {
            let ret = atl_bridgegen::jni::result_class(&g.ret)?;
            Ok((row, ret, atl_bridgegen::jni::param_classes(g)?))
        });
    let own_shim =
        lp64.as_deref().is_ok_and(|e| e.starts_with("Entry::Shim")) && ILP32_SHIMS.contains(&name);
    let (row, ret, params) = match classified {
        Ok(c) => c,
        // a shim converts its own row from the guest view: bound with the guest view's lane classes when every position has one (a by-value union has none: trapped below)
        Err(e)
            if own_shim
                && atl_bridgegen::jni::result_class(&g.ret).is_ok()
                && atl_bridgegen::jni::param_classes(g).is_ok() =>
        {
            rows.push((
                "libc.so".to_owned(),
                name.to_owned(),
                format!("{e:#}"),
                "shim",
            ));
            let ret = atl_bridgegen::jni::result_class(&g.ret)?;
            let params = atl_bridgegen::jni::param_classes(g)?;
            let lp64 = lp64.map_err(|why| anyhow::anyhow!("{name}: {why}"))?;
            return Ok(lp64.replace("{sig}", &sig_literal(ret, &params)));
        }
        Err(e) => {
            rows.push((
                "libc.so".to_owned(),
                name.to_owned(),
                format!("{e:#}"),
                "libc-unclassified",
            ));
            return Ok(trap(name, ILP32_LIBC_UNCLASSIFIED));
        }
    };
    let shim = lp64.as_deref().is_ok_and(|e| e.starts_with("Entry::Shim"));
    let carried = row
        .kinds
        .iter()
        .all(|k| k.carried() || matches!(k, atl_bridgegen::ilp32::Kind::PtrResult));
    if !row.same() {
        let waits = if carried {
            "carried"
        } else {
            row.heaviest().label()
        };
        rows.push(("libc.so".to_owned(), name.to_owned(), row.describe(), waits));
    }
    let lp64 = match lp64 {
        Ok(e) => e,
        Err(why) => return Ok(trap(name, why)),
    };
    if shim && !ILP32_SHIMS.contains(&name) {
        return Ok(trap(name, ILP32_SHIM));
    }
    if !carried && !shim {
        return Ok(trap(name, row.heaviest().trap_reason()));
    }
    Ok(lp64.replace("{sig}", &sig_literal(ret, &params)))
}
/// An ILP32 guest's proxy variable rows: a host variable's value is a host pointer (`SL_IID_*`, `AMEDIAFORMAT_KEY_*` strings), which the guest's `uintptr_t` cannot hold, so the row traps (no value written) until a window copy of what it points to exists.
const ILP32_VARIABLE: &str = "pending: ilp32 variable, a host pointer value";
/// Rows of an ILP32 table whose kinds the headers fix (function, position: 0 = result, the kind's `describe`): a classifier change that moves one fails the build.
const ILP32_EXPECTED: &[(&str, usize, &str)] = &[
    // asset_manager.h `off_t AAsset_getLength(AAsset*)`: bionic arm off_t = long
    ("AAsset_getLength", 0, "widen:s"),
    ("AAsset_getLength", 1, "opaque"),
    // looper.h `int ALooper_pollOnce(int, int* outFd, int* outEvents, void** outData)`
    ("ALooper_pollOnce", 4, "out-word:ptr"),
    ("ALooper_pollOnce", 2, "ptr"),
    // egl.h `EGLImage eglCreateImage(EGLDisplay, EGLContext, EGLenum, EGLClientBuffer, const EGLAttrib*)`: an EGL_NONE-terminated list
    ("eglCreateImage", 1, "opaque"),
    ("eglCreateImage", 5, "array"),
    // jni.h `JNIEnv*` (AAssetManager_fromJava): P5's proxy object, not a record copy
    ("AAssetManager_fromJava", 1, "jni"),
    // native_window.h `int32_t ANativeWindow_lock(ANativeWindow*, ANativeWindow_Buffer*, ARect*)`: ARect 16 bytes on both
    ("ANativeWindow_lock", 2, "record:ANativeWindow_Buffer"),
    ("ANativeWindow_lock", 3, "ptr"),
];
/// Macros read from both prototype views: equal values = an equal data model, where the identity rule holds; else the ILP32 classifier decides every row.
const DATA_MODEL: &[&str] = &["__SIZEOF_POINTER__", "__SIZEOF_LONG__"];

/// The libc rows: per NBS libc stub, its entry -- [`SHIMS`] (+ [`SIGNAL_SHIMS`] for the x86_64 list), [`GLIBC`], the provider's [`PROVIDER_APIS`], else a named trap. A name in those lists that the stub list lacks fails the build (a stale row), on the LP64 lists (an ILP32 list lacks the allocator and `pthread_attr_*` rows: NBS patch 0012 leaves them to the guest's bionic).
/// LP64 rows cross with `Sig::INT` (no prototype; LP64 bionic and every host side agree). `ilp32`: the guest's and the host's bionic prototypes ([`libc_prototypes`]) classify each row ([`ilp32_libc_entry`]), and a bound row carries the guest view's lanes.
fn libc_entries(
    names: &[String],
    qemu: bool,
    ilp32: Option<(&atl_bridgegen::ir::Interface, &mut Ilp32Rows)>,
    trap: &mut dyn FnMut(&str, &'static str) -> String,
) -> Result<Vec<String>> {
    let lp64 = libc_lp64_entries(names, qemu, ilp32.is_none())?;
    match ilp32 {
        None => Ok(names
            .iter()
            .zip(lp64)
            .map(|(name, e)| match e {
                Ok(e) => e.replace("{sig}", "Sig::INT"),
                Err(why) => trap(name, why),
            })
            .collect()),
        Some((protos, rows)) => names
            .iter()
            .zip(lp64)
            .map(|(name, e)| ilp32_libc_entry(name, e, protos, rows, trap))
            .collect(),
    }
}

/// Per libc stub its LP64 entry literal, `{sig}` where the lanes go, or the reason it traps. `stale`: fail on a binding-list name the stub list lacks.
fn libc_lp64_entries(
    names: &[String],
    qemu: bool,
    stale: bool,
) -> Result<Vec<std::result::Result<String, &'static str>>> {
    let provider: Vec<&str> = PROVIDER_APIS
        .iter()
        .flat_map(|(_, f)| f.iter().copied())
        .collect();
    let per_isa: &[&str] = if qemu { &[] } else { SIGNAL_SHIMS };
    // cargo sets CARGO_FEATURE_<NAME> for build scripts; identity and shipped builds keep the glibc rows
    let counted: &[&str] = if qemu && std::env::var_os("CARGO_FEATURE_CROSSING_COUNT").is_some() {
        COUNTED_SHIMS
    } else {
        &[]
    };
    let shims: Vec<&str> = SHIMS
        .iter()
        .chain(per_isa)
        .chain(counted)
        .copied()
        .collect();
    for n in shims
        .iter()
        .chain(GLIBC.iter().map(|(n, _)| n))
        .chain(&provider)
    {
        ensure!(
            !stale || names.iter().any(|s| s == n),
            "libc.so: {n} is in a binding list but not an NBS libc stub"
        );
    }
    let getattr_glibc = selected_breaks()?.contains(&"atl_nb_qt_getattr_glibc");
    let dns_unbound = selected_breaks()?.contains(&"atl_nb_dns_unbound");
    let herr_trap = selected_breaks()?.contains(&"atl_nb_dns_herr_trap");
    let exit_unbound = selected_breaks()?.contains(&"atl_nb_main_exit_unbound");
    let tls_dtor_unbound = selected_breaks()?.contains(&"atl_nb_main_tls_dtor_unbound");
    Ok(names
        .iter()
        .map(|name| {
            if tls_dtor_unbound && name == "native_bridge___cxa_thread_atexit_impl" {
                Err("unbound: no host side (break main_tls_dtor_unbound)")
            } else if dns_unbound && name == "getaddrinfo" {
                Err("unbound: no host side (break dns_unbound)")
            } else if herr_trap && name == "__get_h_errno" {
                Err("unbound: no host side (break dns_herr_trap)")
            } else if exit_unbound && name == "native_bridge_exit" && stale {
                Err("unbound: no host side (break main_exit_unbound)")
            } else if getattr_glibc && name == "pthread_getattr_np" {
                Ok(format!("Entry::Generic(Host::Glibc, {name:?}, {{sig}})"))
            } else if shims.contains(&name.as_str()) {
                Ok("Entry::Shim({sig})".to_owned())
            } else if let Some((_, sym)) = GLIBC.iter().find(|(n, _)| n == name) {
                Ok(format!("Entry::Generic(Host::Glibc, {sym:?}, {{sig}})"))
            } else if provider.contains(&name.as_str()) {
                Ok(format!("Entry::Generic(Host::Provider, {name:?}, {{sig}})"))
            } else {
                Err("unbound: no host side")
            }
        })
        .collect())
}

/// Why a stub with no prototype in the set traps. The NDK proxies: every public NDK header of theirs is in [`NDK_HEADERS`], so a stub without a prototype is platform API no NDK header declares (measured 2026-11-13: `*ForTesting`, `*ForClient`, the LL-NDK `ANativeWindow_dequeueBuffer` family, `AActivityManager_*`); libandroid_runtime is C++ internals no app calls.
fn missing_prototype(soname: &str) -> &'static str {
    match soname {
        "libnativewindow.so" | "libandroid.so" | "libjnigraphics.so" | "libmediandk.so" => {
            "unbound: platform API, no NDK header declares it"
        }
        "libandroid_runtime.so" => "unbound: C++ internals",
        _ => "unbound: no prototype in the EGL/GLES headers",
    }
}

/// The `Sig` literal of a prototype's lanes (engine/objects.rs `Sig`): its result class and parameter classes, which the QEMU crossing places by (dispatch.rs `call_host`).
fn sig_literal(ret: atl_bridgegen::jni::Class, params: &[atl_bridgegen::jni::Class]) -> String {
    let params: Vec<String> = params.iter().map(|c| format!("Class::{c:?}")).collect();
    format!(
        "Sig {{ ret: Class::{ret:?}, params: Some(&[{}]) }}",
        params.join(", ")
    )
}

/// Fixed bindings besides the stub rows: the NBS vdso's four entries and the linker's two hooks (engine/vdso.rs `bind`, `bind_linker`).
const FIXED_BINDINGS: usize = 6;
/// JNI stubs: one per slot of the largest JNI table (`JNINativeInterface`: 233 in jni.h today, `JNIInvokeInterface` fewer; engine/jni.rs asserts the fit), rounded up to 256. The x86 engine's stub array has this length; the QEMU engine reserves as many pool slots.
const OBJECT_STUBS: usize = 256;
/// An `Entry::Mapped` row as generated: library, name, the (parameter, map) pairs.
type MappedRow = (String, String, Vec<(usize, LaneMap)>);
/// `bindings_table.rs` (module `bindings`): every guest library whose stubs the engine binds -- the GL / window / NDK proxies and libc -- with its NBS stub names in list order, each with the entry it binds. The one table `native_bridge_intercept_symbol` resolves (library, name) in.
/// `Entry::Generic(host, symbol, sig)` binds `symbol` looked up on that host library's own handle (a GL front, the bionic provider, glibc's libc.so.6) through the generic crossing, `sig` its lanes ([`sig_literal`]: `atl_bridgegen::jni::result_class` + `param_classes`; LP64 libc rows an integer result, no prototype; ILP32 libc rows the guest view's, [`libc_entries`]); `Entry::Shim(sig)` a hand-written host side of those lanes; `Entry::Trap(i)` `TRAPS[i]`, a named trap (`TRAP_NAMES[i]`).
/// The proxy prototypes come from one unit parsed for two views: `guest` (bionic, as NDK apps compile) and `host` (glibc). A stub whose prototype differs between the views fails the build, naming it.
/// The guest a binding table is generated for: its bionic view's arches.toml row, the ISA of its NBS stub lists, whether the QEMU engine crosses it (else identity), and its thunk pool size (0 until [`thunks_of`] sets it: QEMU tables share the largest).
struct Guest<'a> {
    row: &'a buildscript::ArchRow,
    proxy_isa: &'a str,
    qemu: bool,
    thunks: usize,
}

/// The stub rows of a guest's table: every proxy function and variable, every libc stub ([`bindings_table`]'s `total`).
fn stub_rows(up: &Upstreams, guest: &Guest) -> Result<usize> {
    let mut n = stub_names(up, &libc_stubs(&guest.row.arch))?.len();
    for &(soname, dir) in PROXY_LIBS {
        n += stub_names(up, &proxy_stubs(dir, guest.proxy_isa))?.len()
            + var_names(up, soname, guest.proxy_isa)?.len();
    }
    Ok(n)
}

/// ILP32 record converters by record name ([`ilp32_maps`]): the converters, nested ones first, or why none (atl_bridgegen `ilp32::converters`).
type Convs = std::collections::BTreeMap<String, Result<Vec<atl_bridgegen::ilp32::Conv>, String>>;

/// The break cfg that swaps two guest offsets of the first generated converter (atl_bridgegen `ilp32::converters_rs`): its const assert fails the build.
const CONV_SWAP_CFG: &str = "atl_nb_conv_swap";

/// Non-const pointer-to-pointer parameters that are input arrays, not one out word (function, parameter index, the header's words): the views type them as an out word (`Kind::OutWord`), and only the header's prose gives the length. Reviewed over every arm row that would otherwise convert (`ilp32_rows.txt` `converted`); each must name an out-word position of some arm row (build fails otherwise).
const ILP32_ARRAY_PARAMS: &[(&str, usize, &str)] = &[
    // gl2ext.h EXT_separate_shader_objects: `GLuint glCreateShaderProgramvEXT(GLenum type, GLsizei count, const GLchar **strings)` -- `count` strings (GLES3's core glCreateShaderProgramv spells it `const GLchar *const*`, an array by its constness)
    ("glCreateShaderProgramvEXT", 2, "count strings"),
];

/// An ILP32 converted row's lane maps and its lanes literal ([`ilp32_maps`]).
type Ilp32Maps = (Vec<(usize, LaneMap)>, String);

/// The lane maps of an ILP32 proxy row `row` (guest prototype `sig`) whose non-carried positions all convert: a `Record` pointer parameter with a converter (in; back out when the pointee is not const), an `OutWord` parameter; with its lanes literal (the guest view's classes). Records named by any position get their converters tried first (`convs`), whatever else blocks the row. Err: the trap reason -- the heaviest kind no converter takes, or a lane rule the six-int-lane forwarder (`jni::MAPPED_LANES`) cannot keep.
fn ilp32_maps(
    name: &str,
    sig: &atl_bridgegen::ir::Signature,
    row: &atl_bridgegen::ilp32::Row,
    route: &Route,
    qemu: bool,
    views: &atl_bridgegen::ilp32::Views,
    convs: &mut Convs,
) -> std::result::Result<Ilp32Maps, &'static str> {
    use atl_bridgegen::ilp32::Kind;
    for k in &row.kinds {
        if let Kind::Record(name) = k {
            convs.entry(name.clone()).or_insert_with(|| {
                atl_bridgegen::ilp32::converters(name, views).map_err(|e| format!("{e:#}"))
            });
        }
    }
    let mut maps = Vec::new();
    let array = Kind::Array;
    let mut blocking: Option<&Kind> = None;
    // a record with a deep pointer field (a list) converts only in a hand shim
    let shallow = |name: &str| matches!(convs.get(name), Some(Ok(list)) if list.last().is_some_and(|c| !c.deep(list)));
    for (at, k) in row.kinds.iter().enumerate() {
        let param = at.checked_sub(1);
        let listed_array = ILP32_ARRAY_PARAMS
            .iter()
            .any(|(f, p, _)| *f == name && Some(*p) == param);
        let k = if listed_array && matches!(k, Kind::OutWord(_)) {
            &array
        } else {
            k
        };
        let map = match (k, param) {
            _ if k.carried() => continue,
            (Kind::OutWord(w), Some(p)) => Some((p, LaneMap::OutWord(*w))),
            (Kind::Record(name), Some(p)) if shallow(name) => match sig.params[p].resolved() {
                atl_bridgegen::ir::Ty::Pointer { is_const, .. } => Some((
                    p,
                    LaneMap::Record {
                        name: name.clone(),
                        out: !is_const,
                    },
                )),
                _ => None,
            },
            _ => None,
        };
        match map {
            Some(m) => maps.push(m),
            None => {
                if blocking.is_none_or(|b| k.weight() > b.weight()) {
                    blocking = Some(k);
                }
            }
        }
    }
    if let Some(k) = blocking {
        return Err(k.trap_reason());
    }
    if !matches!(route, Route::Plain) {
        return Err("pending: ilp32 converted row with a JNI or callback lane");
    }
    let (Ok(ret), Ok(params)) = (
        atl_bridgegen::jni::result_class(&sig.ret),
        atl_bridgegen::jni::param_classes(sig),
    ) else {
        return Err("pending: a record by value has no lane class");
    };
    if ret.is_fp()
        || params.len() > atl_bridgegen::jni::MAPPED_LANES
        || params.iter().any(|c| c.is_fp())
    {
        return Err("pending: ilp32 converted row past six int lanes");
    }
    if let Some(why) = atl_bridgegen::jni::lane_refusal(&params).filter(|_| qemu) {
        return Err(why);
    }
    Ok((maps, sig_literal(ret, &params)))
}

/// The per-guest ILP32 rows of an unequal-data-model guest ([`bindings_table`]): (library, function, the row's kinds, what it waits for: `carried`, its heaviest kind's label, `variable` or `libc-unprototyped`) for every row not wholly `Same`.
type Ilp32Rows = Vec<(String, String, String, &'static str)>;

fn bindings_table(
    up: &Upstreams,
    host: &buildscript::ArchRow,
    guest: &Guest,
    out: &std::path::Path,
    provider_soname: &str,
    x86_disp_bias: usize,
    x86_counted: bool,
) -> Result<(String, Ilp32Rows)> {
    let qemu = guest.qemu;
    let thunks = guest.thunks;
    let mut lists: Vec<(&str, Vec<String>)> = Vec::new();
    for &(soname, dir) in PROXY_LIBS {
        let names = stub_names(up, &proxy_stubs(dir, guest.proxy_isa))
            .with_context(|| soname.to_owned())?;
        lists.push((soname, names));
    }
    let libc_names = stub_names(up, &libc_stubs(&guest.row.arch))?;
    for n in PROXY_SHIMS {
        ensure!(
            lists
                .iter()
                .any(|(_, names)| names.iter().any(|s| s.as_str() == *n)),
            "{n}: a proxy shim that no proxy library lists"
        );
    }

    let unit = out.join(format!("proxy_prototypes_{}.cc", guest.row.arch));
    let mut text = String::from("#define EGL_EGLEXT_PROTOTYPES\n#define GL_GLEXT_PROTOTYPES\n");
    let ndk_headers = NDK_HEADERS.iter().flat_map(|(_, hs)| hs.iter());
    for h in PROTOTYPE_HEADERS.iter().chain(ndk_headers) {
        text.push_str(&format!("#include <{h}>\n"));
    }
    std::fs::write(&unit, text)?;
    let includes: Vec<String> = PROTOTYPE_INCLUDES
        .iter()
        .chain(NDK_HEADERS.iter().map(|(d, _)| d))
        .map(|d| format!("-I{}", up.aosp(d).display()))
        .collect();
    let mut guest_target = buildscript::bionic_platform_target(
        up,
        guest.row,
        buildscript::future_level(up, guest.row)?,
        Some(&buildscript::libcxx_include_dir()?),
    );
    guest_target.arch = "guest".to_owned();
    guest_target.args.extend(includes.iter().cloned());
    // deliberate break of the identity gate: a header force-included into the guest view only (e.g. one widening a GL typedef) must fail the build naming the function
    println!("cargo::rerun-if-env-changed={GUEST_INCLUDE_BREAK}");
    if let Ok(h) = std::env::var(GUEST_INCLUDE_BREAK) {
        println!("cargo::rerun-if-changed={h}");
        guest_target.args.extend(["-include".to_owned(), h]);
    }
    let mut host_args = host_compat(up, host, out)?;
    host_args.extend(includes);
    let host_view = Target {
        arch: "host".to_owned(),
        triple: buildscript::clang_triple_from_cargo()?,
        args: host_args,
    };
    // declarations count only from the files named here (`Source::also`): every header of the tree
    let root = up.aosp("frameworks/native/opengl/include");
    let mut also = Vec::new();
    for dir in ["EGL", "GLES", "GLES2", "GLES3", "KHR"] {
        headers_under(&root.join(dir), &mut also)?;
    }
    buildscript::rerun_if_changed(&root);
    for (dir, headers) in NDK_HEADERS {
        let dir = up.aosp(dir);
        also.extend(headers.iter().map(|h| dir.join(h)));
        buildscript::rerun_if_changed(&dir);
    }
    let sles = up.aosp(SLES_DIR);
    also.extend(SL_HEADERS.iter().map(|h| sles.join(h)));
    buildscript::rerun_if_changed(&sles);
    let src = Source {
        path: &unit,
        lang: Lang::Cxx,
        also: &also,
        macros: DATA_MODEL,
    };
    let iface = atl_bridgegen::interface(
        "proxy_prototypes",
        "EGL and GLES headers, guest and host views",
        &src,
        &[guest_target, host_view],
    )
    .context("GL prototypes")?;
    let guest_view = iface.arches.get("guest").context("no guest view")?;
    let host_ir = iface.arches.get("host").context("no host view")?;
    // the data model, read from the two views: equal (identity, arm64 on an LP64 host) or not (arm's ILP32)
    let model = |v: &atl_bridgegen::ir::ArchView| -> Result<Vec<u64>> {
        DATA_MODEL
            .iter()
            .map(|m| {
                v.constants
                    .get(*m)
                    .map(|c| c.bits)
                    .with_context(|| format!("{m}: not in the view"))
            })
            .collect()
    };
    let equal_model = model(guest_view)? == model(host_ir)?;

    let handles: Vec<&str> = HANDLE_TYPEDEFS.iter().map(|(n, _)| *n).collect();
    let views = atl_bridgegen::ilp32::Views {
        guest: guest_view,
        host: host_ir,
        handles: &handles,
    };
    let mut used_handles = std::collections::BTreeSet::new();
    // per stub name both views declare: its ILP32 row; an unclassified difference fails the build naming the function
    let mut classes: std::collections::BTreeMap<&str, atl_bridgegen::ilp32::Row> =
        std::collections::BTreeMap::new();
    let mut unclassified = Vec::new();
    for (_, names) in &lists {
        for name in names {
            let (Some(g), Some(h)) = (guest_view.functions.get(name), host_ir.functions.get(name))
            else {
                continue;
            };
            match atl_bridgegen::ilp32::classify(name, g, h, &views, &mut used_handles) {
                Ok(row) => {
                    classes.insert(name, row);
                }
                Err(e) => unclassified.push(format!("{e:#}")),
            }
        }
    }
    ensure!(
        unclassified.is_empty(),
        "{} stub prototype(s) with an unclassified guest/host difference ({}): {}",
        unclassified.len(),
        guest.row.arch,
        unclassified.join(" | ")
    );
    if equal_model {
        // identity: every stub name both views declare is ABI-identical (pass-through only where proven): every position `Same` (a data pointer and an integer of its width agree; a pointee's signedness is no ABI fact, arm64's plain char being unsigned)
        let converted: Vec<String> = classes
            .iter()
            .filter(|(n, r)| !r.same() && !PROXY_SHIMS.contains(n))
            .map(|(n, r)| format!("{n}: {}", r.describe()))
            .collect();
        ensure!(
            converted.is_empty(),
            "{} stub prototype(s) differ between the guest ({}) and host views: {}",
            converted.len(),
            guest.row.arch,
            converted.join(" | ")
        );
        // layouts the host reads by pointer in guest memory: identical in both views, or the build fails naming the record
        let records = atl_bridgegen::report::compare(&iface, "guest", "host");
        for name in SL_RECORDS {
            let r = records
                .iter()
                .find(|r| r.record == *name)
                .with_context(|| format!("{name}: not in the prototype unit"))?;
            ensure!(
                r.identical,
                "{name}: guest and host layouts differ: {}",
                r.differences.join("; ")
            );
        }
    } else {
        // the real headers agree with the classifier's unit tests: expectations from bionic arm32 / the NDK headers (AAPCS32: off_t 4 bytes; native_window.h ANativeWindow_Buffer 44 vs 48 bytes; looper.h `void** outData`; egl.h `const EGLAttrib*`), position 0 = result
        for (name, at, want) in ILP32_EXPECTED {
            let got = classes
                .get(name)
                .and_then(|r| r.kinds.get(*at))
                .map(atl_bridgegen::ilp32::Kind::describe);
            ensure!(
                got.as_deref() == Some(*want),
                "{name} position {at}: ILP32 kind {got:?}, the header says {want}"
            );
        }
        let undeclared = views.undeclared_handles();
        ensure!(
            undeclared.is_empty(),
            "HANDLE_TYPEDEFS: not typedefs of the {} view: {undeclared:?}",
            guest.row.arch
        );
    }
    let sl_layout = sl_layout_rs(host_ir)?;
    let mut ilp32_rows: Ilp32Rows = Vec::new();

    let mut traps: Vec<(String, String, &'static str)> = Vec::new();
    // ILP32 record converters by record name: the converters (nested first) or why none
    let mut convs: Convs = std::collections::BTreeMap::new();
    let mut rows: Vec<(&str, Vec<(String, String)>)> = Vec::new();
    // `Entry::Mapped(i)` rows: library, name, the (parameter, map) pairs; and every prototype refused a row, named together in one build error
    let mut mapped_rows: Vec<MappedRow> = Vec::new();
    let mut refused: Vec<String> = Vec::new();
    for (soname, names) in &lists {
        let mut entries = Vec::new();
        for name in names {
            let route = match (guest_view.functions.get(name), host_ir.functions.get(name)) {
                (Some(sig), Some(_)) => atl_bridgegen::jni::route(
                    name,
                    sig,
                    ENV_SHIMS.contains(&name.as_str()),
                    CALLBACKS,
                )
                .unwrap_or_else(|e| {
                    refused.push(format!("{e:#}"));
                    Route::Plain
                }),
                _ => Route::Plain,
            };
            let mut trap = |why: &'static str| {
                traps.push(((*soname).to_owned(), name.clone(), why));
                format!("Entry::Trap({})", traps.len() - 1)
            };
            // a proxy shim's own layouts are LP64 bionic's: under an equal model it converts its row itself
            let ilp32 = classes
                .get(name.as_str())
                .filter(|r| !r.same())
                .filter(|_| !(equal_model && PROXY_SHIMS.contains(&name.as_str())));
            if let Some(row) = ilp32 {
                let waits = if row.carried() {
                    "carried"
                } else {
                    row.heaviest().label()
                };
                ilp32_rows.push(((*soname).to_owned(), name.clone(), row.describe(), waits));
            }
            // ILP32: a row with a position its lane class cannot convert binds lane-mapped when every such position has a generated converter ([`ilp32_maps`]), else traps naming the conversion it waits for (p4b s3-s4), before any route: no truncated value passes
            let entry = if ilp32.is_some() && ILP32_PROXY_SHIMS.contains(&name.as_str()) {
                if let Some(last) = ilp32_rows.last_mut() {
                    last.3 = "shim";
                }
                let sig = &guest_view.functions[name.as_str()];
                let ret = atl_bridgegen::jni::result_class(&sig.ret)?;
                let params = atl_bridgegen::jni::param_classes(sig)?;
                format!("Entry::Shim({})", sig_literal(ret, &params))
            } else if let Some(row) = ilp32.filter(|r| !r.carried()) {
                let sig = &guest_view.functions[name.as_str()];
                match ilp32_maps(name, sig, row, &route, qemu, &views, &mut convs) {
                    Ok((maps, lanes)) => {
                        if let Some(last) = ilp32_rows.last_mut() {
                            last.3 = "converted";
                        }
                        mapped_rows.push(((*soname).to_owned(), name.clone(), maps));
                        format!("Entry::Mapped({}, {lanes})", mapped_rows.len() - 1)
                    }
                    Err(why) => trap(why),
                }
            } else if let Route::Mapped(maps) = route {
                mapped_rows.push(((*soname).to_owned(), name.clone(), maps));
                format!("Entry::Mapped({}, Sig::INT)", mapped_rows.len() - 1)
            } else if let Route::Pending(why) = route {
                trap(why)
            } else {
                match (guest_view.functions.get(name), host_ir.functions.get(name)) {
                    (Some(_), Some(_)) if PROXY_SHIMS.contains(&name.as_str()) => {
                        "Entry::Shim(Sig::INT)".to_owned()
                    }
                    (Some(_), Some(_)) if ANNOTATED.contains(&name.as_str()) => {
                        trap("pending: annotated, GS2.2")
                    }
                    // fn-return rows that are not shims (PROXY_SHIMS, eglGetProcAddress): a host function pointer must never reach the guest as a result
                    (Some(sig), Some(_)) if is_fn_ptr(&sig.ret) => {
                        trap("pending: fn-return, GS2.2")
                    }
                    (Some(sig), Some(_)) => {
                        // a record by value (NDK imagedecoder.h `ARect AImageDecoderFrameInfo_getFrameRect`) has no lane class: trapped by name, both engines (the identity table's Generic rows carry a class too)
                        match (
                            atl_bridgegen::jni::result_class(&sig.ret),
                            atl_bridgegen::jni::param_classes(sig),
                        ) {
                            (Ok(ret), Ok(params)) => {
                                // QEMU crosses by the lane image (dispatch.rs `call_host`): a prototype it cannot carry traps by name here, at bind time (R-1, Q7-4); identity passes the call through natively
                                match atl_bridgegen::jni::lane_refusal(&params).filter(|_| qemu) {
                                    Some(why) => trap(why),
                                    None => format!(
                                        "Entry::Generic(Host::System, {name:?}, {})",
                                        sig_literal(ret, &params)
                                    ),
                                }
                            }
                            _ => trap("pending: a record by value has no lane class"),
                        }
                    }
                    _ => trap(missing_prototype(soname)),
                }
            };
            let prototyped =
                guest_view.functions.contains_key(name) && host_ir.functions.contains_key(name);
            ensure!(
                !PROXY_SHIMS.contains(&name.as_str()) || prototyped,
                "{name}: a proxy shim without a prototype in the set ({PROTOTYPE_HEADERS:?})"
            );
            entries.push((name.clone(), entry));
        }
        // NBS `DEFINE_INTERCEPTABLE_STUB_VARIABLE`: the guest variable takes the host variable's value (vdso.rs)
        for name in var_names(up, soname, guest.proxy_isa)? {
            let entry = if equal_model {
                format!("Entry::Variable(Host::System, {name:?})")
            } else {
                // a host variable's value is a host pointer (`SL_IID_*`): an ILP32 guest's `uintptr_t` cannot hold it, so the row stays unbound (no value written) until P6
                ilp32_rows.push((
                    (*soname).to_owned(),
                    name.clone(),
                    "variable".to_owned(),
                    "variable",
                ));
                traps.push(((*soname).to_owned(), name.clone(), ILP32_VARIABLE));
                format!("Entry::Trap({})", traps.len() - 1)
            };
            entries.push((name.clone(), entry));
        }
        rows.push((soname, entries));
    }
    ensure!(
        refused.is_empty(),
        "{} proxy prototype(s) refused a lane-mapped binding (build.rs ENV_SHIMS, CALLBACKS): {}",
        refused.len(),
        refused.join(" | ")
    );
    // stale-row checks of ENV_SHIMS / CALLBACKS: on the equal-model tables, where those rows bind (an ILP32 table traps them before any route)
    for n in ENV_SHIMS.iter().filter(|_| equal_model) {
        ensure!(
            mapped_rows
                .iter()
                .any(|(_, r, m)| r == n && m.iter().any(|(_, x)| *x == LaneMap::Env)),
            "{n}: in ENV_SHIMS but no proxy library lists it with a prototype in the set"
        );
    }
    for t in CALLBACKS.iter().filter(|_| equal_model) {
        ensure!(
            mapped_rows.iter().any(|(_, _, m)| m
                .iter()
                .any(|(_, x)| matches!(x, LaneMap::Down { ty, .. } if ty == t))),
            "{t}: in CALLBACKS but no proxy row with a prototype in the set takes it"
        );
    }
    // stale-row check of ILP32_ARRAY_PARAMS: on the ILP32 table, where they apply
    for (f, p, _) in ILP32_ARRAY_PARAMS.iter().filter(|_| !equal_model) {
        ensure!(
            classes
                .get(f)
                .and_then(|r| r.kinds.get(p + 1))
                .is_some_and(|k| matches!(k, atl_bridgegen::ilp32::Kind::OutWord(_))),
            "{f} parameter {p}: in ILP32_ARRAY_PARAMS but not an out-word position of the {} table",
            guest.row.arch
        );
    }
    let libc_protos = if equal_model {
        None
    } else {
        Some(libc_prototypes(up, guest.row, host, out)?)
    };
    let libc = libc_entries(
        &libc_names,
        qemu,
        libc_protos.as_ref().map(|p| (p, &mut ilp32_rows)),
        &mut |name, why| {
            traps.push(("libc.so".to_owned(), name.to_owned(), why));
            format!("Entry::Trap({})", traps.len() - 1)
        },
    )?;
    rows.push(("libc.so", libc_names.into_iter().zip(libc).collect()));

    let mut code = String::from(
        "// generated by build.rs (atl-native-bridge) from the NBS stub lists, the binding lists and the EGL/GLES prototypes of the guest and host views, do not edit\nuse super::{Class, Entry, Host, LaneMap, Lib, MappedRow, Sig};\n\npub const LIBS: &[Lib] = &[\n",
    );
    let mut total = 0usize;
    for (soname, entries) in &rows {
        code.push_str(&format!(
            "    Lib {{\n        soname: {soname:?},\n        stubs: &[\n"
        ));
        for (name, entry) in entries {
            total += 1;
            code.push_str(&format!("            ({name:?}, {entry}),\n"));
        }
        code.push_str("        ],\n    },\n");
    }
    code.push_str("];\n\n");
    ensure!(
        total == stub_rows(up, guest)?,
        "{}: {total} table rows, stub_rows counts {}",
        guest.row.arch,
        stub_rows(up, guest)?
    );
    // descriptors for a name with no row (engine/proc.rs): the lanes (the guest view's classes) and, for QEMU, the lane refusal of the parameters or, ILP32, the conversion the prototype waits for; a prototype whose classes do not resolve has none, so such a name traps by name
    code.push_str("/// Prototype descriptors by name: lanes, and the QEMU lane refusal of the parameters (None: carried). Engine/proc.rs binds a host answer for a name with no row by its descriptor.\npub const PROTOTYPES: &[(&str, Sig, Option<&str>)] = &[\n");
    let mut unclassified = Vec::new();
    for (name, sig) in &guest_view.functions {
        let Some(host_sig) = host_ir.functions.get(name) else {
            continue;
        };
        let (Ok(ret), Ok(params)) = (
            atl_bridgegen::jni::result_class(&sig.ret),
            atl_bridgegen::jni::param_classes(sig),
        ) else {
            continue;
        };
        let pending = if equal_model {
            None
        } else {
            match atl_bridgegen::ilp32::classify(name, sig, host_sig, &views, &mut used_handles) {
                Ok(row) => (!row.carried()).then(|| row.heaviest().trap_reason()),
                Err(e) => {
                    unclassified.push(format!("{e:#}"));
                    None
                }
            }
        };
        let why = pending.or_else(|| atl_bridgegen::jni::lane_refusal(&params).filter(|_| qemu));
        code.push_str(&format!(
            "    ({name:?}, {}, {why:?}),\n",
            sig_literal(ret, &params)
        ));
    }
    ensure!(
        unclassified.is_empty(),
        "{} prototype descriptor(s) with an unclassified guest/host difference ({}): {}",
        unclassified.len(),
        guest.row.arch,
        unclassified.join(" | ")
    );
    if !equal_model {
        let unused: Vec<&str> = handles
            .iter()
            .copied()
            .filter(|h| !used_handles.contains(*h))
            .collect();
        ensure!(
            unused.is_empty(),
            "HANDLE_TYPEDEFS: no {} stub row or prototype descriptor uses {unused:?} (stale rows)",
            guest.row.arch
        );
    }
    code.push_str("];\n\n");
    // one pool slot per binding plus the JNI slots (QEMU's JNI stubs are binding ids too), rounded to a power of two: `thunks`, the largest of the QEMU guests' (main), so one pool size serves every QEMU table (engine/qemu/place.rs)
    ensure!(
        thunks >= total + FIXED_BINDINGS + OBJECT_STUBS,
        "{}: {thunks} thunks, the table needs {}",
        guest.row.arch,
        total + FIXED_BINDINGS + OBJECT_STUBS
    );
    if !qemu {
        std::fs::write(
            out.join("x86_entries.s"),
            atl_bridgegen::x86::entries_asm(thunks, OBJECT_STUBS, x86_disp_bias, x86_counted),
        )?;
    }
    code.push_str(&format!(
        "/// Thunk pool slots: the stub rows, the fixed bindings and the JNI slots, rounded to a power of two.\npub const THUNKS: usize = {thunks};\n\n/// JNI stubs: one per slot of the largest JNI table (build.rs `OBJECT_STUBS`).\npub const OBJECT_STUBS: usize = {OBJECT_STUBS};\n\n/// The bionic provider's soname (cargo-c's, build.rs `provider_soname`): `Host::Provider` resolves on its handle.\npub const PROVIDER_SONAME: &str = {provider_soname:?};\n\n"
    ));
    code.push_str("/// Per `Entry::Trap(i)`: library, name, why trapped.\npub const TRAP_NAMES: &[(&str, &str, &str)] = &[\n");
    for (lib, name, why) in &traps {
        code.push_str(&format!("    ({lib:?}, {name:?}, {why:?}),\n"));
    }
    code.push_str(
        "];\n\n/// A trapped stub of this table: aborts, naming `TRAP_NAMES[I]`.\nextern \"C\" fn trap<const I: usize>() {\n    super::trapped(TRAP_NAMES[I])\n}\n\n/// `trap::<i>` per `Entry::Trap(i)`.\npub const TRAPS: &[extern \"C\" fn()] = &[\n",
    );
    for i in 0..traps.len() {
        code.push_str(&format!("    trap::<{i}>,\n"));
    }
    code.push_str("];\n\n");
    // the lane-mapped class's host sides: one six-lane forwarder per row (atl_bridgegen::jni::MAPPED_LANES)
    ensure!(
        atl_bridgegen::jni::MAPPED_LANES == 6,
        "the mapped forwarder below takes 6 lanes, atl_bridgegen::jni::MAPPED_LANES is {}",
        atl_bridgegen::jni::MAPPED_LANES
    );
    code.push_str("/// Per `Entry::Mapped(i)`: library, symbol, the (parameter, map) pairs.\npub const MAPPED_ROWS: &[MappedRow] = &[\n");
    for (lib, name, maps) in &mapped_rows {
        let maps: Vec<String> = maps
            .iter()
            .map(|(i, m)| match m {
                LaneMap::Env => format!("({i}, LaneMap::Env)"),
                LaneMap::Down { shorty, .. } => format!("({i}, LaneMap::Down(b{shorty:?}))"),
                LaneMap::Record { name, out } => {
                    format!("({i}, LaneMap::Record(&C_{name}, {out}))")
                }
                LaneMap::OutWord(atl_bridgegen::ilp32::Word::Ptr) => {
                    format!("({i}, LaneMap::OutWord(Word::Ptr))")
                }
                LaneMap::OutWord(atl_bridgegen::ilp32::Word::Int {
                    guest,
                    host,
                    signed,
                }) => format!(
                    "({i}, LaneMap::OutWord(Word::Int {{ guest: {guest}, host: {host}, signed: {signed} }}))"
                ),
            })
            .collect();
        code.push_str(&format!(
            "    ({lib:?}, {name:?}, &[{}]),\n",
            maps.join(", ")
        ));
    }
    code.push_str(
        "];\n\n/// The host side of `Entry::Mapped(I)`: its six int lanes to ndk.rs `call_mapped`.\nextern \"C\" fn mapped<const I: usize>(a0: usize, a1: usize, a2: usize, a3: usize, a4: usize, a5: usize) -> usize {\n    super::super::ndk::call_mapped(MAPPED_ROWS[I], [a0, a1, a2, a3, a4, a5])\n}\n\n/// `mapped::<i>` per `Entry::Mapped(i)`.\npub const MAPPED_FNS: &[extern \"C\" fn(usize, usize, usize, usize, usize, usize) -> usize] = &[\n",
    );
    for i in 0..mapped_rows.len() {
        code.push_str(&format!("    mapped::<{i}>,\n"));
    }
    code.push_str("];\n");
    // ILP32 record converters (atl_bridgegen ilp32 `converters`), each const-asserted; and why a record has none, in ilp32_rows.txt
    let mut emitted = Vec::new();
    for (record, c) in &convs {
        match c {
            Ok(list) => {
                for c in list {
                    if !emitted
                        .iter()
                        .any(|e: &atl_bridgegen::ilp32::Conv| e.name == c.name)
                    {
                        emitted.push(c.clone());
                    }
                }
            }
            Err(why) => ilp32_rows.push((
                "-".to_owned(),
                record.clone(),
                format!("no converter: {why}"),
                "record-unconverted",
            )),
        }
    }
    if !emitted.is_empty() {
        code.push_str("\nuse super::{Conv, Field, Op, Word};\n\n");
        code.push_str(&atl_bridgegen::ilp32::converters_rs(
            &emitted,
            CONV_SWAP_CFG,
        ));
    }
    // one copy: sles.rs asserts against the arm64 QEMU table's, the one every core has (the layout is the host view's, the same for every table); an ILP32 guest's SL rows trap until P6
    if qemu && equal_model {
        code.push_str(&sl_layout);
    }
    Ok((code, ilp32_rows))
}

/// The thunk pool size of a guest's table: its stub rows, the fixed bindings and the JNI slots, rounded to a power of two.
fn thunks_of(up: &Upstreams, guest: &Guest) -> Result<usize> {
    Ok((stub_rows(up, guest)? + FIXED_BINDINGS + OBJECT_STUBS).next_power_of_two())
}

/// `ilp32_rows.txt`: per ILP32 guest, every row not wholly `Same` (guest, library, function, what it waits for, kinds per position), tab-separated, sorted; and per "waits for" its count, also printed as one `cargo::warning` each. The list p4b s2-s4 are planned from.
fn ilp32_rows_txt(per_guest: &[(&str, Ilp32Rows)]) -> String {
    let mut out = String::from(
        "# generated by atl-native-bridge build.rs: ILP32 binding rows (guest, library, function, kinds), do not edit\n",
    );
    for (arch, rows) in per_guest {
        let mut counts: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
        let mut sorted = rows.clone();
        sorted.sort();
        for (lib, name, kinds, waits) in &sorted {
            out.push_str(&format!("{arch}\t{lib}\t{name}\t{waits}\t{kinds}\n"));
            *counts.entry(waits).or_default() += 1;
        }
        for (kind, n) in &counts {
            out.push_str(&format!("# {arch} count {kind} {n}\n"));
            println!("cargo::warning=ilp32 {arch}: {n} rows {kind}");
        }
    }
    out
}

/// The variable names of proxy library `soname`'s NBS stub list (`DEFINE_INTERCEPTABLE_STUB_VARIABLE(name);`), in list order; none for a library with no variables.
fn var_names(up: &Upstreams, soname: &str, isa: &str) -> Result<Vec<String>> {
    let Some(&(_, dir)) = PROXY_LIBS.iter().find(|(s, _)| *s == soname) else {
        return Ok(Vec::new());
    };
    let path = up.aosp(&proxy_stubs(dir, isa));
    let text = std::fs::read_to_string(&path).with_context(|| path.display().to_string())?;
    Ok(text
        .lines()
        .filter_map(|l| {
            l.strip_prefix("DEFINE_INTERCEPTABLE_STUB_VARIABLE(")?
                .strip_suffix(");")
        })
        .map(str::to_owned)
        .collect())
}

/// The host view's layout of the two SL structs sles.rs mirrors in Rust (`DataEnds`, `OutputMixLocator`), for its const asserts: size, then each field's byte offset in header order.
fn sl_layout_rs(host: &atl_bridgegen::ir::ArchView) -> Result<String> {
    let mut out = String::new();
    for (record, konst) in [
        ("SLDataSink_", "SL_DATA_SINK"),
        ("SLDataLocator_OutputMix", "SL_OUTPUT_MIX"),
    ] {
        let r = host
            .records
            .get(record)
            .with_context(|| format!("no record {record}"))?;
        ensure!(
            r.fields.len() == 2,
            "{record}: {} fields, sles.rs mirrors 2",
            r.fields.len()
        );
        out.push_str(&format!(
            "/// `{record}` in the host view: size, then the byte offsets of its two fields.\npub const {konst}: [u64; 3] = [{}, {}, {}];\n",
            r.size, r.fields[0].offset, r.fields[1].offset
        ));
    }
    Ok(out)
}

/// `ATL_NB_BREAK=<name>[,<name>]` selects [`BREAKS`]. Read here, so the cfg reaches this package only; a global RUSTFLAGS cfg reached every crate and failed unrelated builds.
/// Set it on the run that stages the core (`xtask test app`, atld_run.rs `cdylib::stage`, a plain `cargo build`): cargo rebuilds the core when the value changes, so that run's environment decides the artifact.
const BREAK_ENV: &str = "ATL_NB_BREAK";
/// The deliberate breaks: name -> the cfg it turns on. A break is a gate failing on purpose: red with it, green without.
const BREAKS: &[(&str, &str)] = &[
    // the app gate's `glesPbuffer` (engine/vdso.rs): glClear's stub bound to the front's raw address, no crossing
    ("gl_direct", "atl_nb_gl_direct"),
    // the app gate's `jitUpCall` (engine::identity): GS = the record, not ART's Thread*, while an up call's host target runs
    ("foreign_gs", "atl_nb_foreign_gs"),
    // gate 4.4 (engine/jni.rs): a guest env slot = the host function, no crossing
    ("jni_direct", "atl_nb_jni_direct"),
    // C3 (engine/ndk.rs, jni.rs `host_env`): the NDK shims hand the host library the guest env unmapped
    ("ndk_env", "atl_nb_ndk_env"),
    // tr-first blocker 5 (engine/ndk.rs `call_mapped`): a guest callback reaches the host unmapped, no down shell (app gate, QEMU arm64: rows looperNow, looperLater)
    ("cb_down_skip", "atl_nb_cb_down_skip"),
    // gate 4.4 (exports.rs): isNativeBridgeFunctionPointer answers false
    ("nbfp_false", "atl_nb_nbfp_false"),
    // spike gates g1/g2 (engine::identity): the host->guest crossing without its FS restore
    ("no_fs_restore", "atl_nb_no_fs_restore"),
    // Q5-4 (engine/identity/mod.rs `Identity::call`): the guest enters on the host thread pointer, so g1 stage 0's guest FS check fails
    ("swap_tp", "atl_nb_swap_tp"),
    // g4 (tests/g4.rs): a syscall in the host target, must leave the band
    ("g4_break", "atl_g4_break"),
    // Q4-2 (engine/qemu/place.rs `is_stub`): the placeholder check decides on its first two bytes only (unit test stub_shape_is_the_whole_placeholder)
    ("qe_prefix", "atl_nb_qe_prefix"),
    // Q4-3 (engine/qemu/place.rs `ENTRY_BODY_BYTES`): a vdso entry's placeholder is searched at the symbol only, so native_bridge_call_guest (placeholder at +8) fails closed
    ("qe_site", "atl_nb_qe_site"),
    // Q4-3 (engine/qemu/place.rs `register`): no page is registered with QEMU, so a guest fetch from a placed site faults
    ("qe_nomap", "atl_nb_qe_nomap"),
    // Q4-3 (engine/dispatch.rs `call_binding`, `ID_SKEW`): a binding trap calls its neighbour's host function, so each placed site answers the wrong sentinel
    ("qe_id", "atl_nb_qe_id"),
    // L3 JNI count (engine/dispatch.rs `call_object`): QEMU's JNI route skips the shared counter entry, so only identity counts JNI (crossing gate, QEMU check j)
    ("qe_jni_count", "atl_nb_qe_jni_count"),
    // Q4-4 (engine/vdso.rs `bind_linker`): the static-TLS hook keeps its placeholder, so the linker's `bl` branches to 0 and the start gate goes red
    ("qe_no_tls", "atl_nb_qe_no_tls"),
    // Q5-5 (engine/mod.rs `decide`): the host ISA routed to the QEMU engine (unit test `engine::tests::selection`)
    ("host_qemu", "atl_nb_host_qemu"),
    // Q5-5 (engine/mod.rs `decide`): a second ISA needing another engine accepted in one process (unit test `engine::tests::selection`)
    ("second_isa", "atl_nb_second_isa"),
    // Q5-6 (engine/thread.rs `detach`): a guest key destructor never runs at thread exit (qemu-thread gate, `qt_last_value`)
    ("qt_skip_dtor", "atl_nb_qt_skip_dtor"),
    // P4a (engine/mod.rs `Active::release`): QEMU windows are never given back, so thread churn keeps every exited thread's record and TLS (qemu-thread gate, `c`)
    ("qt_keep_windows", "atl_nb_qt_keep_windows"),
    // Q5-6 (engine/intercepts.rs `run`): a guest thread's routine gets the wrong argument (qemu-thread gate, `qt_join`)
    ("qt_routine_arg", "atl_nb_qt_routine_arg"),
    // R10 S3 (engine/intercepts.rs `pthread_create`): the guest attr's stack size is not passed to attach, every guest stack is the default (qemu-thread gate, `g`)
    ("qt_attr_size", "atl_nb_qt_attr_size"),
    // tr-first blocker 4 (engine/intercepts.rs `pthread_create`): the guest gets glibc's handle, not its thread's record, so the record-keyed rows miss it (qemu-thread gate, `q`)
    ("qt_handle_glibc", "atl_nb_qt_handle_glibc"),
    // R10 S3 (build.rs `libc_entries`): pthread_getattr_np bound to glibc again, which reads the bionic record as a glibc thread (qemu-thread gate, `g`)
    ("qt_getattr_glibc", "atl_nb_qt_getattr_glibc"),
    // Q7-7 (engine/intercepts.rs `pthread_exit`): the routine's guest call is not ended, so the guest resumes after its noreturn pthread_exit (qemu-thread gate, `x`)
    ("qt_exit_resume", "atl_nb_qt_exit_resume"),
    // Q6-2 (engine/dispatch.rs `call_object`): the host JNI function gets the guest's env instead of the host object (qemu-jni gate, `e`)
    ("jni_guest_env", "atl_nb_jni_guest_env"),
    // Q7-7 (engine/netdb.rs `view`): the guest addrinfo copy keeps glibc's field order (ai_addr before ai_canonname), so the guest reads its address from the name (qemu-thread gate, `r`)
    ("dns_glibc_order", "atl_nb_dns_glibc_order"),
    // P4b s3d (engine/netdb.rs `freeaddrinfo`): the list's window block is not given back, so the window count grows across a resolver round (qemu-main gate, `n`)
    ("dns_keep_list", "atl_nb_dns_keep_list"),
    // net-bp (build.rs `libc_entries`): getaddrinfo trapped, its row removed: the app gate's resolver probe aborts at the first call
    ("dns_unbound", "atl_nb_dns_unbound"),
    // net-bp (engine/netdb.rs `guest_error`): host EAI_* codes reach the guest unmapped (resolver probe: service code)
    ("dns_eai_raw", "atl_nb_dns_eai_raw"),
    // net-bp (build.rs `libc_entries`): `__get_h_errno` trapped: the resolver probe's h_errno read aborts the app
    ("dns_herr_trap", "atl_nb_dns_herr_trap"),
    // P4b (build.rs `libc_prototype_name`, `libc_lp64_entries`): `native_bridge_exit` has no prototype on an ILP32 table and no host side on an LP64 one, so a guest main's exit traps naming it (qemu-linker gate, qemu_main `e`)
    ("main_exit_unbound", "atl_nb_main_exit_unbound"),
    // build.rs `libc_lp64_entries`: `native_bridge___cxa_thread_atexit_impl` unbound, so a guest thread_local destructor's registration traps naming it (qemu-linker gate, qemu_main `e`/`d`)
    ("main_tls_dtor_unbound", "atl_nb_main_tls_dtor_unbound"),
    // P4b s3e (engine/thread.rs `run_cleanup`): the guest record's cleanup handlers never run at thread exit (qemu-linker gate, qemu_main status 8)
    ("exit_skip_cleanup", "atl_nb_exit_skip_cleanup"),
    // Q6-2 (engine/dispatch.rs `call_object`): x3 and x4 exchanged, so CallStaticIntMethod(2, 3) reaches the host as (3, 2) (qemu-jni gate, `c`)
    ("jni_swap", "atl_nb_jni_swap"),
    // Q6-7 (engine/va_list.rs `fp`): a guest va_list's fp arguments are read from its integer save area (qemu-jni gate, `V`)
    ("va_fp_gp", "atl_nb_va_fp_gp"),
    // Q6-3 (engine/dispatch.rs `call_host`): the fp lanes reach the host as zeros (qemu-jni gate, `d` and `f`)
    ("v_drop", "atl_nb_v_drop"),
    // Q6-3 (engine/dispatch.rs `call_host`): the stack words reach the host one word early (qemu-jni gate, `d`)
    ("drop_stack", "atl_nb_drop_stack"),
    // Q6-3 (engine/dispatch.rs `fp_result`): an fp result is read from x0 (qemu-jni gate, `d` and `f`)
    ("ret_int", "atl_nb_ret_int"),
    // C4 (engine/dispatch/riscv64.rs `UP`): a float placed into an LP64D fp register is not NaN-boxed, so the host reads canonical NaN (sifive app gate: glesPbuffer 1024, the clear colours lost)
    ("up_no_nanbox", "atl_nb_up_no_nanbox"),
    // Q6-3 part 2b (engine/dispatch/variadic.rs `call_al0`): a host call runs with %al = 0, so glibc's snprintf skips saving the xmm arguments of its double (qemu-jni gate, `p`)
    ("al_zero", "atl_nb_al_zero"),
    // Q6-1 (engine/qemu/mod.rs `host_trap_ex`): the stack words a trap delivers arrive one word late (qemu-abi gate, `k`)
    ("qa_stack_off", "atl_qa_stack_off"),
    // Q6-1 (engine/qemu/mod.rs `host_trap_ex`): the FP lanes a trap delivers arrive one register late (qemu-abi gate, `v`)
    ("qa_v_lane", "atl_qa_v_lane"),
    // P3 (engine/qemu/mod.rs `host_trap_ex`): integer lanes 0 and 1 arrive swapped (qemu-abi gate, `x`)
    ("qa_x_lane", "atl_qa_x_lane"),
    // Q5-7 (bridgegen x86.rs `entries_asm`, disp_bias): every identity pool call's displacement one byte short, so each lands one byte past its shell (identity gates g1-g5 and unit test `thunk_slots_call_their_shell`)
    ("x86_disp", X86_DISP_CFG),
    // Q5-8b (bridgegen x86.rs `entries_asm`, `counted`): the identity shells call no counter, so every identity crossing counts 0 (identity gate `identity_crossing`, check t)
    ("id_count", X86_COUNT_CFG),
    // Q6-4 (bridgegen down.rs `entries_asm`, `drop_stack`): a down call's stack words start one word late (qemu-jni gate, check m)
    ("down_drop_stack", DOWN_DROP_CFG),
    // Q6-4 (down.rs `entries_asm`, `swap_v`): a down call's fp argument registers 0 and 1 are exchanged on entry (qemu-jni gate, checks m and x: the one fp argument moves to register 1)
    ("down_swap_v", DOWN_SWAP_CFG),
    // Q6-4 (engine/dispatch.rs `down_params`): the first shorty parameter's class is flipped (qemu-jni gate, checks m, x and R: the 23-integer R-1 call then fits the shell's sixteen stack words)
    ("shorty_swap", "atl_nb_shorty_swap"),
    // Q6-4 (engine/dispatch.rs `down_body`): a down call's first argument stays the host's, not the guest env or vm (qemu-jni gate, checks o, m and x)
    ("down_noenv", "atl_nb_down_noenv"),
    // p4b s3 (engine/dispatch.rs `HostRules::int_arg`): argument k is register k on every guest, AAPCS32's stack words ignored (unit test int_arg_follows_the_guest_rules)
    ("regs_k_raw", "atl_nb_regs_k_raw"),
    // p4b s3c (generated `C_<record>` converters, [`CONV_SWAP_CFG`]): the first two guest offsets of the first converter swapped -> its const assert `Conv::ordered` fails the build
    ("conv_swap", CONV_SWAP_CFG),
    // O1 (engine/sles.rs `create_audio_player`): the sink's OutputMix locator keeps the guest proxy, so the host gets a guest object (sles-bridge gate, check c)
    ("sles_raw_sink", "atl_nb_sles_raw_sink"),
    // O1 (engine/mod.rs `Active::down`): every down request takes a new shell (sles-bridge gate, check e)
    ("sles_no_dedup", "atl_nb_sles_no_dedup"),
    // O1 (engine/vdso.rs variable rows): a guest variable binding is not written, so SL_IID_* stay 0 (sles-bridge gate, check f)
    ("sles_var_skip", "atl_nb_sles_var_skip"),
    // GS2.2 G1 (engine/proc.rs `row_address`): eglGetProcAddress answers a row's host address, not its exported entry (qemu-gl gate, checks e and f)
    ("egl_raw", "atl_nb_egl_raw"),
    // GS2.2 G1 (engine/proc.rs `bind_host`): every host answer takes a new binding, so one name answers two addresses (unit test `fallback_dedup`)
    ("egl_nodedup", "atl_nb_egl_nodedup"),
];
/// The cfg of the `x86_disp` break, read here: it selects the generator's displacement bias, no crate code.
const X86_DISP_CFG: &str = "atl_nb_x86_disp";
/// The cfg of the `id_count` break: with it the identity shells are generated plain, feature or not.
const X86_COUNT_CFG: &str = "atl_nb_id_count";
/// The cfgs of the `down_drop_stack` and `down_swap_v` breaks, read here: they select the down shells' generator flags (bridgegen down.rs `entries_asm`), no crate code.
const DOWN_DROP_CFG: &str = "atl_nb_down_drop_stack";
const DOWN_SWAP_CFG: &str = "atl_nb_down_swap_v";
/// The cfg naming a host down-shell backend whose text is AT&T syntax (bridgegen down.rs `att_syntax`).
const DOWN_ATT_CFG: &str = "atl_nb_down_att";
/// Down shells of the QEMU engine (Q6-4, bridgegen down.rs): one per host->guest trampoline a process binds (each native method, JNI_OnLoad, JNI_OnUnload). Fixed; a binding past it fails with an error (place.rs `down`).
const DOWN_SLOTS: usize = 1024;

/// The cfg naming a core built with the identity engine (arches.toml `[<arch>.host] identity`); crate code gates on it, never on an arch name.
const IDENTITY_CFG: &str = "atl_nb_identity";

/// The cfgs `ATL_NB_BREAK` names, in order; an unknown name fails the build, naming it and the known ones.
fn selected_breaks() -> Result<Vec<&'static str>> {
    let Ok(value) = std::env::var(BREAK_ENV) else {
        return Ok(Vec::new());
    };
    value
        .split(',')
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(|name| {
            BREAKS
                .iter()
                .find(|(n, _)| *n == name)
                .map(|&(_, cfg)| cfg)
                .with_context(|| {
                    let known: Vec<&str> = BREAKS.iter().map(|&(n, _)| n).collect();
                    format!(
                        "{BREAK_ENV}={name}: no such break (known: {})",
                        known.join(", ")
                    )
                })
        })
        .collect()
}

fn main() -> Result<()> {
    // g3 stage D invokes the sigaction interposer: its setter must be found with dlsym(RTLD_DEFAULT); a test that defines none exports nothing
    println!(
        "cargo::rustc-link-arg-tests={}",
        atl_sigaction_hook::link_arg()
    );
    // every break's cfg is declared, so an unselected one is no unexpected-cfg warning
    for &(_, cfg) in BREAKS {
        println!("cargo::rustc-check-cfg=cfg({cfg})");
    }
    println!("cargo::rerun-if-env-changed={BREAK_ENV}");
    for cfg in selected_breaks()? {
        println!("cargo::rustc-cfg={cfg}");
    }
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR")?);
    let checkout = buildscript::checkout(&manifest)?;
    let up = Upstreams::of(&checkout)?;
    let arches = checkout.join("arches.toml");
    let arches_text = std::fs::read_to_string(&arches)?;
    let host = buildscript::host_arch(&arches_text, &std::env::var("TARGET")?)?;
    // the identity engine is a per-host-ISA capability (engine/mod.rs `Active`): cfg `IDENTITY_CFG` on a host ISA that has its crossing
    println!("cargo::rustc-check-cfg=cfg({IDENTITY_CFG})");
    let identity = host.identity;
    if identity {
        println!("cargo::rustc-cfg={IDENTITY_CFG}");
    }
    // one guest bionic view per engine guest compiled into the core (runtime selection, engine/mod.rs `Active`): identity = the host arch, QEMU = each of [`QEMU_GUESTS`]
    let qemu_rows: Vec<(buildscript::ArchRow, &str)> = QEMU_GUESTS
        .iter()
        .map(|&(arch, module)| Ok((buildscript::guest_arch(&arches_text, arch)?, module)))
        .collect::<Result<_>>()?;
    let qemu_guest = &qemu_rows
        .iter()
        .find(|(r, _)| r.arch == "arm64")
        .context("QEMU_GUESTS: no arm64 row")?
        .0;
    // the arm guest's thread view (engine/thread.rs `layout::qemu_arm`): its TCB, record and static-TLS config for the arm linker run (phase4-32bit P4a)
    let qemu_arm = &qemu_rows
        .iter()
        .find(|(r, _)| r.arch == "arm")
        .context("QEMU_GUESTS: no arm row")?
        .0;
    let path = up.file(&format!("aosp/{HEADER}"))?;
    let target = Target {
        arch: host.arch.clone(),
        triple: buildscript::clang_triple_from_cargo()?,
        args: INCLUDE_DIRS
            .iter()
            .map(|d| format!("-I{}", up.aosp(d).display()))
            .collect(),
    };
    let generate = || -> Result<String> {
        let src = Source {
            path: &path,
            // C: the header's `namespace android { extern "C" {` is `#ifdef __cplusplus`, so C sees the same records at top level with C linkage and the same layout
            lang: Lang::C,
            also: &[],
            macros: &[],
        };
        let iface =
            atl_bridgegen::interface("native_bridge", HEADER, &src, std::slice::from_ref(&target))
                .context(HEADER)?;
        api_module_with_table(
            &iface,
            &host.arch,
            &[],
            &["NativeBridgeCallbacks"],
            "crate::exports",
            &Table {
                record: "NativeBridgeCallbacks",
                implementation: "crate::exports",
                on_panic: &on_panic,
                values: &[("version", "crate::exports::VERSION")],
            },
        )
    };
    let code = generate()?;
    ensure!(
        code == generate()?,
        "atl-bridgegen output differs between two runs on the same input"
    );
    let out = PathBuf::from(std::env::var("OUT_DIR")?);
    std::fs::write(out.join("native_bridge.rs"), code)?;
    std::fs::write(
        out.join("bionic_thread_identity.rs"),
        bionic_thread(&up, &host, &out)?,
    )?;
    std::fs::write(
        out.join("bionic_thread_qemu.rs"),
        bionic_thread(&up, qemu_guest, &out)?,
    )?;
    std::fs::write(
        out.join("bionic_thread_qemu_arm.rs"),
        bionic_thread(&up, qemu_arm, &out)?,
    )?;
    let tables = object_tables(&up, &target)?;
    std::fs::write(out.join("object_tables.rs"), object_tables_rs(&tables)?)?;
    // the lanes are the guest's view of each slot (its classes place and convert a guest's arguments): from each QEMU guest's bionic view; one module serves every guest, so their lanes must be equal (a 32-bit JNI difference splits it, P5)
    let mut lanes: Option<(String, String)> = None;
    for (row, _) in &qemu_rows {
        let guest_tables = object_tables(&up, &buildscript::bionic_libc_target(&up, row, None))?;
        let rs = object_lanes_rs(&guest_tables)?;
        match &lanes {
            None => lanes = Some((row.arch.clone(), rs)),
            Some((first, text)) => ensure!(
                *text == rs,
                "object lanes differ between the {first} and {} views: object_lanes.rs needs one module per guest",
                row.arch
            ),
        }
    }
    let (_, lanes) = lanes.context("QEMU_GUESTS is empty")?;
    std::fs::write(out.join("object_lanes.rs"), lanes)?;
    std::fs::write(out.join("provider.rs"), provider(&up, &host, &out)?)?;
    std::fs::write(out.join("gai_errlist.rs"), gai_errlist_rs(&up)?)?;
    let provider_soname = provider_soname(&manifest)?;
    // Q5-7: the identity engine's byte templates (patch encodings), and its shells and pools (`bindings_table`)
    if identity {
        std::fs::write(
            out.join("x86_encodings.rs"),
            atl_bridgegen::x86::constants_rust(),
        )?;
    }
    let x86_disp_bias = usize::from(selected_breaks()?.contains(&X86_DISP_CFG));
    // the counted shells: feature `crossing-count` (cargo's env for build scripts), unless the `id_count` break is selected
    let x86_counted = std::env::var_os("CARGO_FEATURE_CROSSING_COUNT").is_some()
        && !selected_breaks()?.contains(&X86_COUNT_CFG);
    // Q6-4: the QEMU engine's down shells for the host ISA (atl-bridgegen down.rs `backend`), their sizes and the host ABI rules for place.rs / dispatch.rs
    let breaks = selected_breaks()?;
    let down = atl_bridgegen::down::backend(&host.arch)
        .with_context(|| format!("host {}: no down-shell backend in atl-bridgegen", host.arch))?;
    // the backend's asm syntax picks place.rs's `global_asm!` options
    println!("cargo::rustc-check-cfg=cfg({DOWN_ATT_CFG})");
    if down.att_syntax {
        println!("cargo::rustc-cfg={DOWN_ATT_CFG}");
    }
    ensure!(
        DOWN_SLOTS <= down.max_slots,
        "DOWN_SLOTS {DOWN_SLOTS}: host {} shells name at most {}",
        host.arch,
        down.max_slots
    );
    std::fs::write(
        out.join("down.s"),
        (down.entries_asm)(
            DOWN_SLOTS,
            breaks.contains(&DOWN_DROP_CFG),
            breaks.contains(&DOWN_SWAP_CFG),
        ),
    )?;
    std::fs::write(
        out.join("down_pool.rs"),
        atl_bridgegen::down::pool_rust(&down, DOWN_SLOTS),
    )?;
    // one table per engine guest: identity's (the host arch's own view and stub list, the LP64 proxy lists), one per QEMU guest
    let mut tables: Vec<(String, Guest)> = Vec::new();
    if identity {
        tables.push((
            "identity".to_owned(),
            Guest {
                row: &host,
                proxy_isa: "arm64",
                qemu: false,
                thunks: 0,
            },
        ));
    }
    for (row, module) in &qemu_rows {
        tables.push((
            (*module).to_owned(),
            Guest {
                row,
                proxy_isa: &row.arch,
                qemu: true,
                thunks: 0,
            },
        ));
    }
    // the QEMU pool (engine/qemu/place.rs) is generated once, at the largest QEMU table's size, so every QEMU table's ids fit it
    let mut qemu_thunks = 0;
    for (_, g) in tables.iter().filter(|(_, g)| g.qemu) {
        qemu_thunks = qemu_thunks.max(thunks_of(&up, g)?);
    }
    for (_, g) in &mut tables {
        g.thunks = if g.qemu {
            qemu_thunks
        } else {
            thunks_of(&up, g)?
        };
    }
    std::fs::write(
        out.join("qemu_pool.rs"),
        format!(
            "{}{}{}{}",
            atl_bridgegen::aarch64::constants_rust(),
            atl_bridgegen::aarch64::pool_rust(u32::try_from(qemu_thunks)?),
            atl_bridgegen::arm::constants_rust(),
            atl_bridgegen::arm::pool_rust(u32::try_from(qemu_thunks)?)
        ),
    )?;
    let mut ilp32: Vec<(&str, Ilp32Rows)> = Vec::new();
    for (module, g) in &tables {
        let (code, rows) = bindings_table(
            &up,
            &host,
            g,
            &out,
            &provider_soname,
            x86_disp_bias,
            x86_counted,
        )?;
        std::fs::write(out.join(format!("bindings_table_{module}.rs")), code)?;
        if !rows.is_empty() {
            ilp32.push((&g.row.arch, rows));
        }
    }
    std::fs::write(out.join("ilp32_rows.txt"), ilp32_rows_txt(&ilp32))?;
    buildscript::rerun_if_changed(&arches);
    for rel in std::iter::once(&HEADER)
        .chain(std::iter::once(&JNI_HEADER))
        .chain(INCLUDE_DIRS)
        .chain(THREAD_HEADERS)
        .chain(THREAD_INCLUDE_DIRS)
        .chain(PROVIDER_FILES)
    {
        for p in up.rerun_paths(&format!("aosp/{rel}")) {
            buildscript::rerun_if_changed(&p);
        }
    }
    Ok(())
}
