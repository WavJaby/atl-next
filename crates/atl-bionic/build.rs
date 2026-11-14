// SPDX-License-Identifier: MIT
//! Generates, with atl-bridgegen from bionic's pinned sources, for the target cargo builds for:
//! - `OUT_DIR/capi/include/<header>`: C declarations of the bionic-only API this crate implements, for
//!   AOSP code built against glibc (cargo-c installs them; the converter puts them on device compiles).
//! - `OUT_DIR/<api>.rs`: that API's constants and opaque types, and a check that every export of
//!   `crate::ffi` has the header's signature (bridge control 1).
//! - `OUT_DIR/prop_area.rs`: bionic's property-area format -- record layouts, magic, version, flags --
//!   from `prop_area.cpp` / `prop_info.cpp` and their C++ headers (bridge control 2: the reader and the
//!   image builder take every offset from here, so a format change moves them or fails the build).
//! - `OUT_DIR/timespec.rs`: `struct timespec`'s offsets and field sizes, checked equal in bionic's and
//!   the host libc's view (the `__system_property_wait` timeout).
//!
//! - `OUT_DIR/capi/include/<header>`, also: include wrappers giving the host libc's headers bionic's
//!   header shape (`atl_bridgegen::shape`), installed with the declarations, so they reach exactly the
//!   units that see bionic's headers, substitutes for the bionic-only headers in `SUBSTITUTES`, and
//!   forwards to bionic's kernel uapi headers the host lacks and bionic's own arch view includes alone (`shape::Shape::uapi`). Each must add no
//!   error to its host header (a substitute or forward: compile alone), as C and C++.
//!   `OUT_DIR/shape_gaps.txt`: what they leave different (names bionic's private files bring in; uapi headers not forwarded because bionic's own arch view cannot include them).
//!   `OUT_DIR/capi/include/atl-bionic/uapi/<header>`: kernel uapi records code reads by layout
//!   (`UAPI_RECORDS`), alone, layout asserted; a shape wrapper includes each.
//!
//! Generates twice and fails unless byte-identical.
//!
//! Every header is parsed as bionic sees it (bionic's triple, its libc headers, no host headers): the
//! declarations are bionic's, the printed C is libc-neutral. The prop-area sources are C++ and include
//! `<atomic>` / `<new>`; bionic ships no C++ library, so the host toolchain's libc++ headers come first
//! (found by clang's own search rules). Layouts there use only `uint32_t`, `char` and 32-bit atomics.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use atl_bridgegen::buildscript::{self, Api, ArchRow, DECLARATIONS, Upstreams};
use atl_bridgegen::c::Kept;
use atl_bridgegen::ir::{ArchView, Interface, Ty};
use atl_bridgegen::{Lang, Refusals, Source, Target, front, shape};

/// The bionic APIs this crate implements, in `crate::ffi`.
const APIS: &[Api] = &[
    Api {
        name: "system_properties",
        source: "bionic/libc/include/sys/system_properties.h",
        lang: Lang::C,
        header: Some("sys/system_properties.h"),
        functions: &[
            // libselinux android_device.c selinux_android_setcon re-inits after setcon
            "__system_properties_init",
            "__system_property_set",
            "__system_property_find",
            "__system_property_read_callback",
            "__system_property_foreach",
            "__system_property_read",
            "__system_property_get",
            "__system_property_area_serial",
            "__system_property_serial",
            "__system_property_wait",
        ],
        macros: &["PROP_VALUE_MAX", "PROP_NAME_MAX", "PROP_DIRNAME"],
        include_dirs: &[],
        kept: DECLARATIONS,
    },
    // bionic's sys/cdefs.h brings it into every unit; ours has no include, so it can be forced
    buildscript::API_LEVEL,
    // libdl's platform-only API, declared by its callers: the target SDK setter (ART; the launcher calls
    // it, owner Q5), the exported-namespace lookup (libvulkan, libgraphicsenv, libstagefright,
    // libvndksupport), namespace creation / linking (libgraphicsenv), the linker warnings (libandroid_runtime
    // android_app_Activity.cpp) and the default search paths (libopenjdk System.c, java.library.path)
    Api {
        name: "libdl_android",
        source: "bionic/libdl/libdl_android.cpp",
        lang: Lang::Cxx,
        header: None,
        functions: &[
            "android_set_application_target_sdk_version",
            "android_get_exported_namespace",
            "android_create_namespace",
            "android_link_namespaces",
            "android_dlwarning",
            "android_get_LD_LIBRARY_PATH",
        ],
        macros: &[],
        include_dirs: &[],
        kept: DECLARATIONS,
    },
    // libasync_safe (providers.toml row): what the native closure calls (2026-10-31: libmemunreachable,
    // libc_malloc_debug_backtrace, libunwindstack LogAndroid.cpp, perfetto logging.h, libandroid_runtime
    // Zygote). Its callers pass `ANDROID_LOG_*` from the `<android/log.h>` it includes and use the
    // `async_safe_fatal` macro.
    Api {
        name: "async_safe",
        source: "bionic/libc/async_safe/include/async_safe/log.h",
        lang: Lang::C,
        header: Some("async_safe/log.h"),
        functions: &[
            "async_safe_fatal_no_abort",
            "async_safe_format_buffer",
            "async_safe_format_log",
            "async_safe_format_log_va_list",
            "async_safe_write_log",
        ],
        macros: &[],
        include_dirs: &["system/logging/liblog/include"],
        // all of its includes, as its callers saw them before the list was explicit
        kept: Kept {
            includes: &[
                "<sys/cdefs.h>",
                "<stdarg.h>",
                "<stddef.h>",
                "<stdint.h>",
                "<stdlib.h>",
                "<android/log.h>",
            ],
            macros: &["async_safe_fatal"],
            types: &[],
            records: &[],
        },
    },
    // what the native closure calls of fdsan (2026-10-31: libsfplugin_ccodec C2NodeImpl.cpp; 2026-11-01:
    // libbase unique_fd.h DefaultCloser, libcutils native_handle.cpp, both under __ANDROID__)
    Api {
        name: "fdsan",
        source: "bionic/libc/include/android/fdsan.h",
        lang: Lang::C,
        header: Some("android/fdsan.h"),
        functions: &[
            "android_fdsan_set_error_level",
            "android_fdsan_create_owner_tag",
            "android_fdsan_exchange_owner_tag",
            "android_fdsan_close_with_tag",
            "android_fdsan_get_owner_tag",
        ],
        macros: &[],
        include_dirs: &[],
        kept: DECLARATIONS,
    },
    // libdl's extended dlopen (2026-10-31: libEGL, libgraphicsenv, libstagefright, libvulkan,
    // libvndksupport); callers build android_dlextinfo and pass ANDROID_DLEXT_USE_NAMESPACE
    Api {
        name: "dlext",
        source: "bionic/libc/include/android/dlext.h",
        lang: Lang::C,
        header: Some("android/dlext.h"),
        functions: &["android_dlopen_ext"],
        macros: &[],
        include_dirs: &[],
        kept: DECLARATIONS,
    },
    // a platform header (bionic_libc_platform_headers), C++: what the closure uses (2026-10-31:
    // libmediautils LimitProcessMemory.cpp M_SET_ALLOCATION_LIMIT_BYTES, MemoryLeakTrackUtil.cpp the leak
    // info struct). The GWP-ASan options struct (default member initialisers, a nested enum) is not printed.
    Api {
        name: "malloc",
        source: "bionic/libc/platform/bionic/malloc.h",
        lang: Lang::Cxx,
        header: Some("bionic/malloc.h"),
        functions: &["android_mallopt"],
        macros: &[],
        include_dirs: &[],
        kept: Kept {
            includes: &[],
            macros: &[],
            types: &["android_mallopt_leak_info_t"],
            records: &[],
        },
    },
    // bionic's malloc introspection, declared by its one caller (2026-11-01: libmemunreachable bionic.h, C++ so `()` is `(void)`).
    // No header printed: libmemunreachable includes its own declaration.
    Api {
        name: "malloc_introspection",
        source: "system/memory/libmemunreachable/bionic.h",
        lang: Lang::Cxx,
        header: None,
        functions: &[
            "malloc_disable",
            "malloc_enable",
            "malloc_iterate",
            "malloc_backtrace",
        ],
        macros: &[],
        include_dirs: &[],
        kept: DECLARATIONS,
    },
    // the per-network resolver entry netd and libandroid_net call (libc.map.txt `# apex`; 2026-11-01: libandroid_net net.c
    // android_getaddrinfofornetwork). No header printed: net.c includes bionic's own `resolv_netid.h` (its include_dirs name
    // `bionic/libc/dns/include`), so a printed one would be a second copy on the include path. Its other `*fornet` /
    // `*fornetcontext` functions have no caller in the closure; net.c's `android_res_*` call libnetd_client, not bionic.
    Api {
        name: "resolv_netid",
        source: "bionic/libc/dns/include/resolv_netid.h",
        lang: Lang::C,
        header: None,
        functions: &["android_getaddrinfofornet"],
        macros: &[],
        include_dirs: &[],
        kept: DECLARATIONS,
    },
    // the first abort message, kept for a crash report (2026-11-01: liblog, libbase, perfetto aborters
    // under __ANDROID__)
    Api {
        name: "set_abort_message",
        source: "bionic/libc/include/android/set_abort_message.h",
        lang: Lang::C,
        header: Some("android/set_abort_message.h"),
        functions: &["android_set_abort_message"],
        macros: &[],
        include_dirs: &[],
        kept: DECLARATIONS,
    },
    // bionic's <pthread.h> (IN_HOST_HEADERS): libutils Threads.cpp Thread::getTid
    Api {
        name: "pthread",
        source: "bionic/libc/include/pthread.h",
        lang: Lang::C,
        header: Some("atl-bionic/pthread.h"),
        functions: &["pthread_gettid_np"],
        macros: &[],
        include_dirs: &[],
        kept: DECLARATIONS,
    },
    // bionic's <stdlib.h> (IN_HOST_HEADERS): libandroid_runtime AndroidRuntime.cpp setArgv0 after rewriting argv[0]; ART oat_file_manager.cc (app-image opt-out by process name, __ANDROID__) reads it
    Api {
        name: "progname",
        source: "bionic/libc/include/stdlib.h",
        lang: Lang::C,
        header: Some("atl-bionic/stdlib.h"),
        functions: &["getprogname", "setprogname"],
        macros: &[],
        include_dirs: &[],
        kept: DECLARATIONS,
    },
    // bionic's <assert.h> (RENAMED_IN_HOST): deqp deDefs.c deAssertFail calls `__assert(file, line, msg)` under DE_OS_ANDROID
    Api {
        name: "assert",
        source: "bionic/libc/include/assert.h",
        lang: Lang::C,
        header: Some("atl-bionic/renamed/assert.h"),
        functions: &["__assert"],
        macros: &[],
        include_dirs: &[],
        kept: DECLARATIONS,
    },
    // a platform header, constants only (2026-11-01: libmediautils TimeCheck.cpp through
    // debuggerd/handler.h, DEBUGGER_SIGNAL): the signal numbers bionic reserves, `__SIGRTMIN + n`,
    // evaluated. Its inline signal-mask filter is not printed. 35 (the debugger's) is glibc's
    // SIGRTMIN + 1, an application signal: only sending is in the closure, no handler is installed.
    Api {
        name: "reserved_signals",
        source: "bionic/libc/platform/bionic/reserved_signals.h",
        // its macros.h is C++
        lang: Lang::Cxx,
        header: Some("bionic/reserved_signals.h"),
        functions: &[],
        macros: &["BIONIC_SIGNAL_*", "BIONIC_ENABLE_MTE"],
        include_dirs: &[],
        kept: DECLARATIONS,
    },
    // bionic's thread-pointer slot numbers, per arch; `bionic/tls.h` (TLS_HEADER) includes it. The GL
    // fronts (frameworks/native opengl hooks.h) keep the per-thread dispatch table in TLS_SLOT_OPENGL_API.
    Api {
        name: "tls_defines",
        source: "bionic/libc/platform/bionic/tls_defines.h",
        lang: Lang::C,
        header: Some("bionic/tls_defines.h"),
        functions: &[],
        macros: &["MIN_TLS_SLOT", "MAX_TLS_SLOT", "TLS_SLOT_*"],
        include_dirs: &[],
        kept: DECLARATIONS,
    },
];

/// bionic API declared in a header the host also has, as (API row, host header): the row's header is
/// `atl-bionic/<header>`, included by the shape wrapper of `<header>` (`shape::Inputs::extra`). A row is
/// generated, and implemented (`cfg(atl_bionic_api = "<row>")`), only where the host's header with
/// `_GNU_SOURCE` declares none of its functions; where it declares all, the host's serve, as any libc
/// function; some but not all fails. `pthread_gettid_np`: glibc >= 2.42. `setprogname` / `getprogname`: BSD, no glibc has them.
const IN_HOST_HEADERS: &[(&str, &str)] = &[("pthread", "pthread.h"), ("progname", "stdlib.h")];

/// bionic API whose names the host's libc exports with another ABI, as (API row, host header): glibc's
/// `__assert(assertion, file, line)` vs bionic's `__assert(file, line, msg)` -- binding the name to the
/// host's symbol would pass the arguments in the wrong slots. The row's printed declarations
/// (`atl-bionic/renamed/<header>`) are included by `atl-bionic/<header>` after `#define <f>
/// atl_bionic_<f>` for each function, so callers reach atl-bionic's export of that name; the shape
/// wrapper of `<header>` includes it after the host's. Always generated: the host declaring the name
/// is the point. Limit: a unit that includes `<header>` with `NDEBUG` and again without sees glibc's
/// `__assert` declared under the macro (conflicting types, a compile error, never a silent misbind).
/// Third field: `atl-bionic/<header>`, spelled out for a `'static` include name (checked).
const RENAMED_IN_HOST: &[(&str, &str, &str)] = &[("assert", "assert.h", "atl-bionic/assert.h")];

/// The link name of a `RENAMED_IN_HOST` function.
fn renamed(function: &str) -> String {
    format!("atl_bionic_{function}")
}

/// What the shape wrapper of a host header includes beyond bionic's shape (`shape::Inputs::extra`),
/// besides `IN_HOST_HEADERS`. `asm/unistd.h`: under `__ANDROID__` without `__BIONIC__`, liblog's
/// logger_write.cpp and libbase's threads.cpp call `syscall(__NR_gettid)` / `__NR_tgkill` after
/// `<unistd.h>` alone (their `<syscall.h>` include is `!__ANDROID__`); bionic never builds that path
/// (it calls `gettid()`). The kernel's `__NR_*` names only, not glibc's `SYS_*` aliases.
/// `fcntl.h`: bionic's `<sys/socket.h>` includes kernel uapi `<asm/fcntl.h>` (the shape walk derives no
/// uapi include), so it brings `O_CLOEXEC` and the other `O_*` flags; libmemunreachable LeakPipe.h ->
/// ScopedPipe.h calls `pipe2(fd, O_CLOEXEC)` after `<sys/socket.h>` alone. glibc's home for those flags is
/// `<fcntl.h>`; uapi `<asm/fcntl.h>` itself would redefine glibc's `struct flock`.
/// `linux/magic.h`: bionic's `<sys/vfs.h>` (and `<sys/statfs.h>` through it) includes it, a kernel uapi include the shape walk does not derive; libandroid_runtime NativeLibraryHelper.cpp compares `statfs.f_type` with `EXT4_SUPER_MAGIC` after `<sys/statfs.h>` alone. Macros only, so glibc's header followed by it compiles.
/// `linux/xattr.h`: bionic's `<sys/xattr.h>` includes it, glibc's does not; adbd file_sync_service.cpp reads `XATTR_NAME_CAPS` after `<sys/xattr.h>` alone. uapi `libc-compat.h` sees glibc's `_SYS_XATTR_H` and drops its own `XATTR_CREATE` / `XATTR_REPLACE` enum, so the pair compiles.
/// glibc feature macros that `_GNU_SOURCE` turns on and bionic has no counterpart of, turned off after
/// the host's `<features.h>` by a generated front of it, so device compiles see bionic's view.
/// `__USE_DYNAMIC_STACK_SIZE` (glibc >= 2.34): `SIGSTKSZ` / `PTHREAD_STACK_MIN` become `sysconf` calls and
/// `<signal.h>` includes `<unistd.h>` (bits/sigstksz.h); bionic's are uapi / limits constants (x86_64
/// SIGSTKSZ 8192 in both libcs) and its `<signal.h>` declares nothing of `<unistd.h>`. Leak hit:
/// libvpx vpx_thread.c / libaom aom_thread.c define a file-local `static int sync(...)`, a conflict with
/// `<unistd.h>`'s `sync()` they never include on bionic.
const HOST_FEATURES_OFF: &[&str] = &["__USE_DYNAMIC_STACK_SIZE"];

/// The generated `<features.h>` front: the host's, then `HOST_FEATURES_OFF` undefined.
fn features_front() -> String {
    let mut text = String::from(
        "/* Generated by atl-bionic build.rs (HOST_FEATURES_OFF). */\n#include_next <features.h>\n",
    );
    for m in HOST_FEATURES_OFF {
        text.push_str(&format!("#undef {m}\n"));
    }
    text
}

const EXTRA_INCLUDES: &[(&str, &str)] = &[
    ("unistd.h", "asm/unistd.h"),
    ("sys/socket.h", "fcntl.h"),
    ("sys/vfs.h", "linux/magic.h"),
    ("sys/xattr.h", "linux/xattr.h"),
];

/// bionic's BSD signal tables (`<signal.h>` `sys_siglist` / `sys_signame`, strsignal.cpp), exported as data: mksh
/// builds with `HAVE_SYS_SIGNAME` / `HAVE_SYS_SIGLIST` (its Android.bp) and indexes both. glibc has no
/// `sys_signame` and, since 2.32, `sys_siglist` only as a non-default compat symbol, declared nowhere, so
/// links bind ours. Entries from bionic's own X-macro list; numbers and `_NSIG` from bionic's `<signal.h>`,
/// checked equal to the host's (the declaration callers see sizes the array with the host's `_NSIG`).
const SIGDEFS: &str = "bionic/libc/private/bionic_sigdefs.h";
/// Generated, under `capi/include`; the shape wrapper of `<signal.h>` includes it.
const SIGNAL_NAMES_HEADER: &str = "atl-bionic/signal_names.h";

/// Records of a kernel uapi header, declared alone (bridgegen's C printer, layout asserted against
/// bionic's view) in a generated header the shape wrapper of a host header includes (`shape::Inputs::extra`).
struct UapiRecords {
    /// Include name; the file is the host arch's own, as bionic's search order finds it (`asm-<kernel_arch>`).
    uapi: &'static str,
    host_header: &'static str,
    /// Generated, under `capi/include`.
    header: &'static str,
}

/// The register-set records of the host arch's uapi `asm/ptrace.h`, named by its arches.toml `uapi_records`:
/// libmemunreachable ThreadCapture.cpp reads the stack pointer's index in the `PTRACE_GETREGSET` buffer from
/// them after `<sys/ptrace.h>`, which reaches them on bionic through `<linux/ptrace.h>`. Records only: uapi's
/// `PTRACE_*` macros would turn glibc's C++ `enum __ptrace_request` enumerators into numbers.
const UAPI_RECORDS: &[UapiRecords] = &[UapiRecords {
    uapi: "asm/ptrace.h",
    host_header: "sys/ptrace.h",
    header: "atl-bionic/uapi/asm/ptrace.h",
}];

/// bionic's property-area implementation and the headers holding its layouts.
const PROP_AREA_SOURCES: &[&str] = &[
    "bionic/libc/system_properties/prop_area.cpp",
    "bionic/libc/system_properties/prop_info.cpp",
];
const PROP_AREA_HEADERS: &[&str] = &[
    "bionic/libc/system_properties/include/system_properties/prop_area.h",
    "bionic/libc/system_properties/include/system_properties/prop_info.h",
];
/// Include directories of those sources: `platform/bionic/macros.h`, `system_properties/…`,
/// `async_safe/log.h`, and the `android/log.h` it includes.
const PROP_AREA_INCLUDES: &[&str] = &[
    "bionic/libc",
    "bionic/libc/system_properties/include",
    "bionic/libc/async_safe/include",
    "system/logging/liblog/include",
];
/// What the generation reads besides the sources above, for cargo's rerun check.
const INPUT_DIRS: &[&str] = &[
    "bionic/libc/include",
    "bionic/libc/kernel",
    "bionic/libc/platform",
    "bionic/libc/system_properties",
    "bionic/libc/async_safe/include",
    "bionic/libdl",
    "bionic/libc/dns/include",
    "system/logging/liblog/include",
    "system/memory/libmemunreachable",
];

/// bionic's public headers, relative to the AOSP tree.
const BIONIC_INCLUDE: &str = "bionic/libc/include";

/// Private bionic header -> the host header declaring what it brings in that the including header's
/// host version lacks (`shape::Inputs::homes`; each row checked needed and sufficient). `bits/ioctl.h`:
/// bionic's `<unistd.h>` declares `ioctl` (libnetdutils, libdmabufheap), glibc only `<sys/ioctl.h>`.
const HOMES: &[(&str, &str)] = &[("bits/ioctl.h", "sys/ioctl.h")];

/// bionic macros the host lacks and AOSP code uses without defining them, defined where bionic's headers
/// bring them in: `__unused` (`sys/cdefs.h`; bpf's KernelUtils.h, libtimeinstate), `IS_ELF` (`elf.h`
/// via `bits/elf_common.h`; libc_malloc_debug_backtrace MapData.cpp), `GID_MAX` (`limits.h`, `UINT_MAX`:
/// glibc's `gid_t` is the same `unsigned int`; libpackagelistparser), `__LIBC_HIDDEN__` (`sys/cdefs.h`,
/// hidden visibility; bionic's `resolv_netid.h` as libandroid_net includes it), `__RENAME_IF_FILE_OFFSET64`
/// (`sys/cdefs.h`; androidfw under `__ANDROID__`): empty on LP64, as read here, where off_t is 64-bit
/// on both libcs. `__packed` (`sys/cdefs.h`; libmedia mediascanner.h `struct MediaAlbumArt {…} __packed;`):
/// undefined, it declares a variable of the struct, which calls its private constructor. `M_PURGE`
/// (`malloc.h`, -101; hwui Bitmap.cpp `mallopt(M_PURGE, 0)` under `__ANDROID__`, result ignored): glibc
/// 2.41's `mallopt` ignores an unknown option and returns 1, after consolidating the main arena's fastbins.
/// `M_LOG_STATS` (`malloc.h`, -205; libandroid_runtime android_os_Debug.cpp `mallopt(M_LOG_STATS, 0) == 1`):
/// the same glibc path, so `Debug.logAllocatorStats()` reports true and nothing is logged.
/// `__CPU_BITTYPE` (`sched.h`, `__USE_GNU` only: forwarded under it; libandroid_runtime
/// android_util_Process.cpp divides glibc's `CPU_ALLOC_SIZE` by it): `unsigned long int`, glibc's
/// `__cpu_mask`, the element of both libcs' `cpu_set_t.__bits`.
/// `TEMP_FAILURE_RETRY` (`unistd.h`; glibc defines it under `__USE_GNU` only, so the C view lacks it; libcore
/// ojluni FileDescriptor_md.c): the same retry-on-EINTR statement expression in both libcs.
/// `__printflike` (`sys/cdefs.h`, printf format attribute; libdebuggerd_client debuggerd_client.cpp `log_error`).
/// `IN_LOOPBACK` (`netinet/in.h` via uapi `linux/in.h`; libstagefright_rtsp MyHandler.h): glibc has only `IN_LOOPBACKNET`; a 127/8 test on a host-order address, the same on any libc.
const FORWARDED_MACROS: &[&str] = &[
    "IN_LOOPBACK",
    "__unused",
    "__printflike",
    "TEMP_FAILURE_RETRY",
    "M_PURGE",
    "M_LOG_STATS",
    "__CPU_BITTYPE",
    "__packed",
    "IS_ELF",
    "GID_MAX",
    "__LIBC_HIDDEN__",
    "__RENAME_IF_FILE_OFFSET64",
];

/// bionic-only public headers AOSP code includes, generated whole (`shape::Inputs::substitutes`); each
/// row reviewed to mean the same on glibc. `sys/limits.h` is `<limits.h>` (libandroid_net net.c);
/// `sys/endian.h` is byte-order macros over compiler builtins, its `htonl` family the host's
/// (libmediametrics ItemSerialize-*.cpp). `sys/_system_properties.h`, bionic's private property header,
/// is now only `#include <sys/system_properties.h>`, the generated declarations (frameworks/native
/// tracing_perfetto_internal.cpp: `__system_property_find` / `__system_property_serial`).
const SUBSTITUTES: &[&str] = &["sys/_system_properties.h", "sys/endian.h", "sys/limits.h"];

/// Host macros a substitute redefines as bionic writes them (`shape::Override`). `sys/endian.h`'s byte-order
/// family: glibc's are calls of `static __inline` functions (`__uint32_identity`, `__bswap_32`), bionic's
/// constant expressions (identity, `__builtin_bswap*`); libmtp MtpDescriptors.cpp brace-initialises
/// `__le32` fields with `htole32(1)`, a narrowing error unless constant. Same values on both libcs.
const OVERRIDES: &[shape::Override] = &[shape::Override {
    substitute: "sys/endian.h",
    host: "endian.h",
    macros: &[
        "htobe16", "htobe32", "htobe64", "htole16", "htole32", "htole64", "be16toh", "be32toh",
        "be64toh", "le16toh", "le32toh", "le64toh",
    ],
}];

/// bionic's private `bionic/tls.h`, replaced: bionic's reads the thread pointer by inline asm, which on glibc is glibc's TCB. Ours declares `__get_tls()` out of line -> `crate::tls`, one per-thread slot array in this library, so every DSO sees the same one (a header-inline `thread_local` would give each DSO its own).
/// `__set_tls` is not declared: only bionic's own startup sets the thread pointer.
/// Only reader so far: frameworks/native opengl hooks.h (get_tls_hooks), built with `USE_SLOW_BINDING` (patches/) so no GL trampoline reads the thread pointer directly.
const TLS_HEADER: (&str, &str) = (
    "bionic/tls.h",
    "#pragma once\n#include <sys/cdefs.h>\n__BEGIN_DECLS\nvoid** __get_tls(void);\n__END_DECLS\n#include <bionic/tls_defines.h>\n",
);

/// Generated files: path relative to OUT_DIR -> text.
type Outputs = Vec<(PathBuf, String)>;

/// One generation's result.
#[derive(PartialEq)]
struct Generated {
    files: Outputs,
    /// Include names of the shape wrappers among `files`.
    wrappers: Vec<String>,
    /// Host headers the wrappers are derived from, for cargo's rerun check.
    host_inputs: BTreeSet<PathBuf>,
    /// The `IN_HOST_HEADERS` rows generated (the host lacks their functions).
    host_rows: Vec<&'static str>,
}

fn main() -> Result<()> {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR")?);
    let checkout = buildscript::checkout(&manifest)?;
    let up = Upstreams::of(&checkout)?;
    let arches = checkout.join("arches.toml");
    let host = buildscript::host_arch(
        &std::fs::read_to_string(&arches)?,
        &std::env::var("TARGET")?,
    )?;
    for rel in APIS
        .iter()
        .map(|a| a.source)
        .chain(PROP_AREA_SOURCES.iter().copied())
        .chain(PROP_AREA_HEADERS.iter().copied())
        .chain([SIGDEFS])
    {
        up.file(&format!("aosp/{rel}"))?;
    }
    let out = PathBuf::from(std::env::var("OUT_DIR")?);
    let libcxx = buildscript::libcxx_include_dir()?;

    // every refusal of the run, reported together at the end; the guards are as strict as a first-error stop
    let mut refusals = Refusals::default();
    let first = generate(&up, &host, &libcxx, &out, &mut refusals)?;
    if refusals.is_empty() {
        let mut again = Refusals::default();
        ensure!(
            first == generate(&up, &host, &libcxx, &out, &mut again)?,
            "atl-bridgegen output differs between two runs on the same input"
        );
        again.finish()?;
    }
    // a header the generation stopped writing must not stay on the include path
    let capi = out.join("capi");
    if capi.exists() {
        std::fs::remove_dir_all(&capi)?;
    }
    for (rel, text) in &first.files {
        let path = out.join(rel);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&path, text).with_context(|| format!("writing {}", path.display()))?;
    }
    check_wrappers(&first.wrappers, &capi.join("include"), &mut refusals)?;
    refusals.finish()?;
    let rows: Vec<String> = IN_HOST_HEADERS
        .iter()
        .map(|(n, _)| format!("\"{n}\""))
        .collect();
    println!(
        "cargo::rustc-check-cfg=cfg(atl_bionic_api, values({}))",
        rows.join(", ")
    );
    for row in &first.host_rows {
        println!("cargo::rustc-cfg=atl_bionic_api=\"{row}\"");
    }
    // the triple the wrappers are checked for, so tests compile against them as this does; the
    // target, so tests read bionic's headers for its arch
    println!(
        "cargo::rustc-env=ATL_BIONIC_CLANG_TRIPLE={}",
        buildscript::clang_triple_from_cargo()?
    );
    println!(
        "cargo::rustc-env=ATL_BIONIC_TARGET={}",
        std::env::var("TARGET")?
    );

    println!("cargo::rerun-if-changed={}", arches.display());
    println!("cargo::rerun-if-env-changed=ATL_BIONIC_BREAK");
    for path in &first.host_inputs {
        println!("cargo::rerun-if-changed={}", path.display());
    }
    for dir in INPUT_DIRS {
        for path in up.rerun_paths(&format!("aosp/{dir}")) {
            println!("cargo::rerun-if-changed={}", path.display());
        }
    }
    Ok(())
}

use buildscript::{bionic_platform_target as bionic_target, future_level};

/// What selects the second view headers are read in (`shape::Inputs::gnu`): clang++ predefines it, so
/// every device C++ unit sees both libcs' `__USE_GNU` sections.
const GNU_VIEW: &str = "-D_GNU_SOURCE";

/// The host libc's view, as device compiles see it.
fn host_target(host: &ArchRow) -> Result<Target> {
    Ok(Target {
        arch: host.arch.clone(),
        triple: buildscript::clang_triple_from_cargo()?,
        args: Vec::new(),
    })
}

/// Whether the host's `header`, with `_GNU_SOURCE`, declares `functions`: all (true), none (false); some fails.
fn host_declares(host: &ArchRow, header: &str, functions: &[&str]) -> Result<bool> {
    let mut target = host_target(host)?;
    target.args.push(GNU_VIEW.to_owned());
    let probes = front::probe_includes(Lang::C, &target, &[header])?;
    let probe = probes
        .get(header)
        .filter(|p| p.usable())
        .with_context(|| format!("the host has no usable <{header}>"))?;
    let names: BTreeSet<&String> = probe.files.values().flat_map(|r| r.names.iter()).collect();
    let has: Vec<&str> = functions
        .iter()
        .copied()
        .filter(|f| names.contains(&(*f).to_owned()))
        .collect();
    ensure!(
        has.is_empty() || has.len() == functions.len(),
        "the host's <{header}> declares {has:?} of {functions:?}: split the row"
    );
    Ok(!has.is_empty())
}

/// One generation; each refused item is recorded in `refusals` and left out. An error is a failure to run at all.
fn generate(
    up: &Upstreams,
    host: &ArchRow,
    libcxx: &Path,
    out: &Path,
    refusals: &mut Refusals,
) -> Result<Generated> {
    let mut files = Outputs::new();
    let level = future_level(up, host)?;
    let mut extra: Vec<(&str, &str)> = EXTRA_INCLUDES.to_vec();
    let mut host_rows = Vec::new();
    for api in APIS {
        let target = bionic_target(up, host, level, (api.lang == Lang::Cxx).then_some(libcxx));
        let row = || -> Result<()> {
            if let Some(&(_, header, front)) =
                RENAMED_IN_HOST.iter().find(|(n, _, _)| *n == api.name)
            {
                let printed = api
                    .header
                    .context("a RENAMED_IN_HOST row without a header")?;
                ensure!(
                    front == format!("atl-bionic/{header}")
                        && printed == format!("atl-bionic/renamed/{header}"),
                    "RENAMED_IN_HOST {}: headers must be atl-bionic/{header} and atl-bionic/renamed/{header}",
                    api.name
                );
                let api_files = buildscript::api_files_beside_host(up, api, target, "crate::ffi")?;
                extra.push((header, front));
                let mut text =
                    String::from("/* Generated by atl-bionic build.rs (RENAMED_IN_HOST). */\n");
                for f in api.functions {
                    text.push_str(&format!("#undef {f}\n#define {f} {}\n", renamed(f)));
                }
                text.push_str(&format!("#include <{printed}>\n"));
                files.push((Path::new("capi/include").join(front), text));
                files.extend(api_files);
                return Ok(());
            }
            let Some(&(_, header)) = IN_HOST_HEADERS.iter().find(|(n, _)| *n == api.name) else {
                files.extend(buildscript::api_files(up, api, target, "crate::ffi")?);
                return Ok(());
            };
            if host_declares(host, header, api.functions)? {
                return Ok(());
            }
            let printed = api
                .header
                .context("an IN_HOST_HEADERS row without a header")?;
            let api_files = buildscript::api_files_beside_host(up, api, target, "crate::ffi")?;
            extra.push((header, printed));
            host_rows.push(api.name);
            files.extend(api_files);
            Ok(())
        };
        refusals.take(row().with_context(|| format!("API row {}", api.name)));
    }
    for row in UAPI_RECORDS {
        if let Some(file) = refusals.take(uapi_records(up, host, level, row)) {
            files.push(file);
            extra.push((row.host_header, row.header));
        }
    }
    let prop = prop_area(up, bionic_target(up, host, level, Some(libcxx)), out).and_then(|iface| {
        check_prop_area_shape(&iface.arches[&host.arch])?;
        Ok([
            (
                PathBuf::from("prop_area.rs"),
                atl_bridgegen::rust::layout_module(&iface, &host.arch)?,
            ),
            ("prop_area.ir.json".into(), atl_bridgegen::to_json(&iface)?),
        ])
    });
    files.extend(refusals.take(prop).into_iter().flatten());
    let timespec = timespec(up, host, level, out).and_then(|t| {
        Ok((
            PathBuf::from("timespec.rs"),
            atl_bridgegen::rust::layout_module(&t, &host.arch)?,
        ))
    });
    files.extend(refusals.take(timespec));
    if let Some(names) = refusals.take(signal_names(up, host, level, out)) {
        files.extend(names);
        extra.push(("signal.h", SIGNAL_NAMES_HEADER));
    }
    let shape = shape(up, host, level, &extra, refusals)?;
    for (name, text) in &shape.wrappers {
        let path = Path::new("capi/include").join(name);
        if files.iter().any(|(p, _)| *p == path) {
            refusals.push(anyhow::anyhow!(
                "{name}: both a generated declaration header and a shape wrapper"
            ));
            continue;
        }
        files.push((path, text.clone()));
    }
    files.push(("shape_gaps.txt".into(), shape.gaps));
    let features = Path::new("capi/include").join("features.h");
    ensure!(
        !files.iter().any(|(p, _)| *p == features),
        "features.h: both a shape wrapper and the HOST_FEATURES_OFF front"
    );
    files.push((features, features_front()));
    let mut wrappers: Vec<String> = shape.wrappers.into_iter().map(|(n, _)| n).collect();
    wrappers.push("features.h".to_owned());
    Ok(Generated {
        files,
        wrappers,
        host_inputs: shape.host_inputs,
        host_rows,
    })
}

/// The header declaring the host's `uapi_records` as bionic's uapi does, with their layout asserted.
fn uapi_records(
    up: &Upstreams,
    host: &ArchRow,
    level: u64,
    row: &UapiRecords,
) -> Result<(PathBuf, String)> {
    let target = bionic_target(up, host, level, None);
    let file = front::resolve_include(Lang::C, &target, row.uapi)?;
    let aosp = up.aosp("");
    let label = file
        .strip_prefix(&aosp)
        .unwrap_or(&file)
        .display()
        .to_string();
    let iface = atl_bridgegen::interface(row.uapi, &label, &Source::c(&file, &[]), &[target])?;
    let records: Vec<&str> = host.uapi_records.iter().map(String::as_str).collect();
    let kept = Kept {
        records: &records,
        ..Kept::default()
    };
    let text = atl_bridgegen::c::header(&iface, &host.arch, &[], &[], &kept)
        .with_context(|| format!("{label}: {records:?}"))?;
    Ok((Path::new("capi/include").join(row.header), text))
}

/// The shape wrappers and what they leave different from bionic.
struct ShapeOut {
    /// Include name -> text.
    wrappers: Vec<(String, String)>,
    /// `shape::print_gaps`, then `shape::print_uapi_gaps`.
    gaps: String,
    /// The host headers they were derived from.
    host_inputs: BTreeSet<PathBuf>,
}

fn shape(
    up: &Upstreams,
    host: &ArchRow,
    level: u64,
    extra: &[(&str, &str)],
    refusals: &mut Refusals,
) -> Result<ShapeOut> {
    let include = up.aosp(BIONIC_INCLUDE);
    let public: Vec<String> = headers_under(&include)?
        .into_iter()
        .filter(|n| !shape::is_private(n))
        .collect();
    let gnu = |mut t: Target| {
        t.args.push(GNU_VIEW.to_owned());
        t
    };
    let bionic_t = bionic_target(up, host, level, None);
    let bionic = front::probe_includes(Lang::C, &bionic_t, &strs(&public))
        .context("bionic's public headers")?;
    let bionic_gnu = front::probe_includes(Lang::C, &gnu(bionic_t), &strs(&public))
        .context("bionic's public headers with _GNU_SOURCE")?;
    let uapi: Vec<PathBuf> = buildscript::bionic_uapi_dirs(host)
        .iter()
        .map(|d| up.path(d))
        .collect();
    let host_target = host_target(host)?;
    let mut names: BTreeSet<String> =
        shape::host_names(&[&bionic, &bionic_gnu], &include, &uapi, HOMES);
    names.extend(extra.iter().map(|(h, _)| (*h).to_owned()));
    let names: Vec<String> = names.into_iter().collect();
    let host_probes = front::probe_includes(Lang::C, &host_target, &strs(&names))
        .context("the host's headers")?;
    let host_gnu = front::probe_includes(Lang::C, &gnu(self::host_target(host)?), &strs(&names))
        .context("the host's headers with _GNU_SOURCE")?;
    // as device C++ includes them: the linkage they give their functions
    let host_cxx = front::probe_includes(Lang::Cxx, &gnu(self::host_target(host)?), &strs(&names))
        .context("the host's headers as C++")?;
    let cxx_std = front::resolve_include(Lang::Cxx, &gnu(self::host_target(host)?), "new")
        .context("the host's C++ standard library")?;
    let cxx_std = cxx_std.parent().context("<new> has no directory")?;
    let kernel = kernel_headers(&uapi)?;
    let scan = front::resolve_includes(
        Lang::C,
        &host_target,
        &strs(&kernel.keys().cloned().collect::<Vec<_>>()),
    )?;
    let declared: Vec<&str> = APIS.iter().filter_map(|a| a.header).collect();
    let no_errors = BTreeMap::new();
    let mut inputs = shape::Inputs {
        default: shape::Probes {
            bionic: &bionic,
            host: &host_probes,
        },
        gnu: shape::Probes {
            bionic: &bionic_gnu,
            host: &host_gnu,
        },
        bionic_include: &include,
        uapi: &uapi,
        homes: HOMES,
        macros: FORWARDED_MACROS,
        substitutes: SUBSTITUTES,
        // deliberate break for the gate: ATL_BIONIC_BREAK=overrides drops them -> libmtp fails to compile
        overrides: if std::env::var("ATL_BIONIC_BREAK").as_deref() == Ok("overrides") {
            &[]
        } else {
            OVERRIDES
        },
        declared: &declared,
        scan: &scan,
        kernel: &kernel,
        extra,
        cxx: &host_cxx,
        cxx_std,
        bionic_uapi_errors: &no_errors,
    };
    // C only: a missing file fails both languages; check_wrappers checks C and C++ of what is forwarded
    let candidates: Vec<String> = shape::uapi_candidates(&inputs).into_keys().collect();
    let uapi_errors = front::include_errors(
        Lang::C,
        &bionic_target(up, host, level, None),
        &strs(&candidates),
    )
    .context("bionic's uapi forward candidates in bionic's own view")?;
    inputs.bionic_uapi_errors = &uapi_errors;
    let derived = shape::derive_all(&inputs, refusals)?;
    refusals.take(shape::check_guards(derived.wrappers.keys()));
    let mut wrappers: Vec<(String, String)> = Vec::new();
    for (name, w) in &derived.wrappers {
        let source = if w.substitute {
            format!("{BIONIC_INCLUDE}/{name}")
        } else if bionic.contains_key(name) {
            format!("{BIONIC_INCLUDE}/{name} and the host's <{name}>")
        } else {
            format!("the host's <{name}>")
        };
        if let Some(text) = refusals.take(shape::print(name, w, &source)) {
            wrappers.push((name.clone(), text));
        }
    }
    let aosp = up.aosp("");
    for (name, file) in &derived.uapi {
        if derived.wrappers.contains_key(name) {
            refusals.push(anyhow::anyhow!(
                "<{name}>: both a shape wrapper and a bionic-only uapi header"
            ));
            continue;
        }
        let source = file
            .strip_prefix(&aosp)
            .unwrap_or(file)
            .display()
            .to_string();
        if let Some(text) = refusals.take(shape::print_forward(file, &source)) {
            wrappers.push((name.clone(), text));
        }
    }
    wrappers.push((TLS_HEADER.0.to_owned(), TLS_HEADER.1.to_owned()));
    let host_inputs = host_probes
        .values()
        .chain(host_gnu.values())
        .chain(host_cxx.values())
        .flat_map(|p| p.files.keys().cloned())
        .chain(scan.into_values())
        .collect();
    Ok(ShapeOut {
        wrappers,
        gaps: shape::print_gaps(&derived.gaps)
            + &shape::print_uapi_gaps(&host.arch, &derived.uapi_gaps),
        host_inputs,
    })
}

/// bionic's kernel uapi headers by include name -> file, each `dir` in bionic's search order (first wins).
/// A subdirectory holding `asm/` is an arch's tree (`uapi/asm-arm64`), an include root of its own that
/// only its arch puts on the path (the host's is one of `dirs`): its files are not names under `dir`.
fn kernel_headers(dirs: &[PathBuf]) -> Result<BTreeMap<String, PathBuf>> {
    let mut out = BTreeMap::new();
    for dir in dirs {
        for name in headers_under(dir)? {
            let arch_tree = name
                .split_once('/')
                .is_some_and(|(top, _)| dir.join(top).join("asm").is_dir());
            if !arch_tree {
                out.entry(name.clone()).or_insert_with(|| dir.join(&name));
            }
        }
    }
    Ok(out)
}

fn strs(v: &[String]) -> Vec<&str> {
    v.iter().map(String::as_str).collect()
}

/// Every `.h` file under `dir`, as a `/`-separated path relative to it (its include name), sorted.
fn headers_under(dir: &Path) -> Result<Vec<String>> {
    let mut out = Vec::new();
    let mut todo = vec![dir.to_path_buf()];
    while let Some(d) = todo.pop() {
        for entry in std::fs::read_dir(&d).with_context(|| format!("reading {}", d.display()))? {
            let path = entry?.path();
            if path.is_dir() {
                todo.push(path);
            } else if path.is_file() && path.extension().is_some_and(|e| e == "h") {
                let rel = path.strip_prefix(dir)?;
                let parts: Vec<&str> = rel
                    .iter()
                    .map(|c| c.to_str().context("non-UTF-8 header path"))
                    .collect::<Result<_>>()?;
                out.push(parts.join("/"));
            }
        }
    }
    out.sort();
    Ok(out)
}

/// Every wrapper, included alone as C and as C++ against libc++ with the generated headers where device
/// compiles have them, adds no error to its host header alone (some distro uapi headers do not compile
/// alone): an include cycle or a definition clash fails the build, not the converter. Each header and language that adds
/// errors is one refusal.
fn check_wrappers(wrappers: &[String], include: &Path, refusals: &mut Refusals) -> Result<()> {
    let triple = buildscript::clang_triple_from_cargo()?;
    let gen_dir = format!("-isystem{}", include.display());
    for (lang, base) in [
        (Lang::C, vec![]),
        (Lang::Cxx, vec!["-stdlib=libc++".to_owned()]),
    ] {
        let target = |args: Vec<String>| Target {
            arch: String::new(),
            triple: triple.clone(),
            args,
        };
        let bare = front::include_errors(lang, &target(base.clone()), &strs(wrappers))?;
        let wrapped = front::include_errors(
            lang,
            &target([vec![gen_dir.clone()], base].concat()),
            &strs(wrappers),
        )?;
        for (name, errs) in &wrapped {
            let added: Vec<&String> = errs.iter().filter(|e| !bare[name].contains(e)).collect();
            if !added.is_empty() {
                refusals.push(anyhow::anyhow!(
                    "the generated <{name}> adds errors as {lang:?}:\n  {}",
                    added
                        .iter()
                        .map(|e| e.as_str())
                        .collect::<Vec<_>>()
                        .join("\n  ")
                ));
            }
        }
    }
    Ok(())
}

/// bionic's property-area format: both implementation files in one unit, layouts from their headers.
fn prop_area(up: &Upstreams, mut target: Target, out: &Path) -> Result<Interface> {
    let unit = out.join("prop_area_format.cc");
    let text: String = PROP_AREA_SOURCES
        .iter()
        .map(|s| format!("#include \"{}\"\n", up.aosp(s).display()))
        .collect();
    std::fs::write(&unit, text)?;
    let also: Vec<PathBuf> = PROP_AREA_SOURCES
        .iter()
        .chain(PROP_AREA_HEADERS)
        .map(|s| up.aosp(s))
        .collect();
    target.args.extend(
        PROP_AREA_INCLUDES
            .iter()
            .map(|d| format!("-I{}", up.aosp(d).display())),
    );
    let src = Source {
        path: &unit,
        lang: Lang::Cxx,
        also: &also,
        macros: &["PROP_VALUE_MAX"],
    };
    atl_bridgegen::interface(
        "prop_area",
        "bionic/libc/system_properties/{prop_area,prop_info}.cpp",
        &src,
        &[target],
    )
}

/// `SIGDEFS`' entries as (macro name, description), in file order.
fn sigdefs(text: &str) -> Result<Vec<(&str, &str)>> {
    let mut defs = Vec::new();
    for line in text.lines() {
        let Some(args) = line.trim().strip_prefix("__BIONIC_SIGDEF(") else {
            continue;
        };
        let parsed = args.split_once(',').and_then(|(name, rest)| {
            let desc = rest.trim().strip_suffix(')')?.trim();
            let desc = desc.strip_prefix('"')?.strip_suffix('"')?;
            // printed into a Rust c"" literal as is
            (!desc.contains(['"', '\\'])).then_some((name.trim(), desc))
        });
        defs.push(parsed.with_context(|| format!("{SIGDEFS}: unparsed entry `{line}`"))?);
    }
    ensure!(!defs.is_empty(), "{SIGDEFS}: no __BIONIC_SIGDEF entries");
    Ok(defs)
}

/// The signal tables (`SIGDEFS`): `signal_names.rs` (`NSIG`, `ENTRIES`) and the declarations header.
fn signal_names(up: &Upstreams, host: &ArchRow, level: u64, out: &Path) -> Result<Outputs> {
    let path = up.aosp(SIGDEFS);
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let defs = sigdefs(&text)?;
    let mut probe = String::from("#include <signal.h>\n#define ATL_SIG__NSIG _NSIG\n");
    for (name, _) in &defs {
        probe.push_str(&format!("#define ATL_SIG_{name} {name}\n"));
    }
    let unit = out.join("signal_numbers.c");
    std::fs::write(&unit, probe)?;
    let src = Source::c(&unit, &["ATL_SIG_*"]);
    let mut views = Vec::new();
    for target in [bionic_target(up, host, level, None), host_target(host)?] {
        let iface =
            atl_bridgegen::interface("signal_names", "<signal.h> signal numbers", &src, &[target])?;
        let view: BTreeMap<String, u64> = iface.arches[&host.arch]
            .constants
            .iter()
            .map(|(n, c)| (n.clone(), c.bits))
            .collect();
        views.push(view);
    }
    ensure!(
        views[0] == views[1],
        "signal numbers differ between bionic and the host libc: bionic {:?}, host {:?}",
        views[0],
        views[1]
    );
    let number = |name: &str| {
        views[0]
            .get(&format!("ATL_SIG_{name}"))
            .copied()
            .with_context(|| format!("<signal.h> {name}: not evaluated"))
    };
    let nsig = number("_NSIG")?;
    let mut rs = format!(
        "// generated by atl-bionic build.rs from {SIGDEFS} and bionic's <signal.h>, do not edit\n\n/// bionic's `_NSIG`: both tables' length.\npub const NSIG: usize = {nsig};\n/// (signal number, `sys_signame` entry, `sys_siglist` entry); every other slot is null, as bionic's.\npub const ENTRIES: &[(usize, &core::ffi::CStr, &core::ffi::CStr)] = &[\n"
    );
    for (name, desc) in &defs {
        let n = number(name)?;
        ensure!(n < nsig, "{name} = {n}: outside _NSIG {nsig}");
        // strsignal.cpp: `&(#signal_number)[3]`
        let short = name
            .strip_prefix("SIG")
            .with_context(|| format!("{SIGDEFS}: {name} lacks the SIG prefix"))?;
        rs.push_str(&format!("    ({n}, c\"{short}\", c\"{desc}\"),\n"));
    }
    rs.push_str("];\n");
    // bionic signal.h:57-58 without the nullability qualifiers; `_NSIG` is the host's, checked equal above
    let header = "/* Generated by atl-bionic build.rs (SIGDEFS): bionic's <signal.h> tables. */\n#pragma once\n#include <sys/cdefs.h>\n__BEGIN_DECLS\nextern const char* const sys_siglist[_NSIG];\nextern const char* const sys_signame[_NSIG];\n__END_DECLS\n";
    Ok(vec![
        (PathBuf::from("signal_names.rs"), rs),
        (
            Path::new("capi/include").join(SIGNAL_NAMES_HEADER),
            header.to_owned(),
        ),
    ])
}

/// `struct timespec`'s layout, the field `__system_property_wait`'s timeout is read through: evaluated in
/// bionic's view and in the host libc's, which must agree -- bionic's header declares the parameter,
/// glibc-built callers pass glibc's struct (Rust callers built for Android pass the `libc` crate's
/// bionic one). The result is bionic's view, its constants named after `TIMESPEC_PROBE`'s macros.
fn timespec(up: &Upstreams, host: &ArchRow, level: u64, out: &Path) -> Result<Interface> {
    let unit = out.join("timespec_layout.c");
    std::fs::write(&unit, TIMESPEC_PROBE)?;
    let src = Source::c(&unit, &["TIMESPEC_*"]);
    let mut views = Vec::new();
    for target in [
        bionic_target(up, host, level, None),
        Target {
            arch: host.arch.clone(),
            triple: buildscript::clang_triple_from_cargo()?,
            args: Vec::new(),
        },
    ] {
        views.push(atl_bridgegen::interface(
            "timespec",
            "<time.h> struct timespec",
            &src,
            &[target],
        )?);
    }
    let bits = |i: &Interface| -> Vec<(String, u64)> {
        i.arches[&host.arch]
            .constants
            .iter()
            .map(|(n, c)| (n.clone(), c.bits))
            .collect()
    };
    ensure!(
        bits(&views[0]) == bits(&views[1]),
        "struct timespec differs between bionic and the host libc: bionic {:?}, host {:?}",
        bits(&views[0]),
        bits(&views[1])
    );
    Ok(views.swap_remove(0))
}

/// The layout facts of `struct timespec` the timeout reader uses, as clang computes them.
const TIMESPEC_PROBE: &str = "\
#include <stddef.h>
#include <time.h>
#define TIMESPEC_SIZE sizeof(struct timespec)
#define TIMESPEC_TV_SEC offsetof(struct timespec, tv_sec)
#define TIMESPEC_TV_SEC_SIZE sizeof(((struct timespec *)0)->tv_sec)
#define TIMESPEC_TV_NSEC offsetof(struct timespec, tv_nsec)
#define TIMESPEC_TV_NSEC_SIZE sizeof(((struct timespec *)0)->tv_nsec)
";

/// The C spelling of a field type, for the shape checks below.
fn spell(ty: &Ty) -> String {
    match ty {
        Ty::Typedef { name, .. } => name.clone(),
        Ty::Int { c, .. } => c.c_name().to_owned(),
        Ty::Array { elem, len } => format!("{}[{len}]", spell(elem)),
        Ty::Record { name } => name.clone(),
        other => format!("{other:?}"),
    }
}

/// What the reader and builder assume beyond offsets: each field they touch exists with this type.
/// `prop_info.anon0` is the value / long-property union.
fn check_prop_area_shape(view: &ArchView) -> Result<()> {
    let value_max = view
        .constants
        .get("PROP_VALUE_MAX")
        .context("PROP_VALUE_MAX")?
        .bits;
    let value = format!("char[{value_max}]");
    let error_len = view
        .constants
        .get("prop_info::kLongLegacyErrorBufferSize")
        .context("prop_info::kLongLegacyErrorBufferSize")?
        .bits;
    let error = format!("char[{error_len}]");
    let atomic = "atomic_uint_least32_t";
    let want: &[(&str, &str, &str)] = &[
        ("prop_area", "bytes_used_", "uint32_t"),
        ("prop_area", "serial_", atomic),
        ("prop_area", "magic_", "uint32_t"),
        ("prop_area", "version_", "uint32_t"),
        ("prop_area", "data_", "char[0]"),
        ("prop_trie_node", "namelen", "uint32_t"),
        ("prop_trie_node", "prop", atomic),
        ("prop_trie_node", "left", atomic),
        ("prop_trie_node", "right", atomic),
        ("prop_trie_node", "children", atomic),
        ("prop_trie_node", "name", "char[0]"),
        ("prop_info", "serial", atomic),
        ("prop_info", "anon0", "prop_info_anon0"),
        ("prop_info", "name", "char[0]"),
        ("prop_info_anon0", "value", &value),
        (
            "prop_info_anon0",
            "long_property",
            "prop_info_anon0_long_property",
        ),
        ("prop_info_anon0_long_property", "error_message", &error),
        ("prop_info_anon0_long_property", "offset", "uint32_t"),
    ];
    for (record, field, ty) in want {
        let r = view
            .records
            .get(*record)
            .with_context(|| format!("bionic's prop-area format has no {record}"))?;
        let Some(f) = r.fields.iter().find(|f| f.name == *field) else {
            bail!("{record} has no field {field}: the prop-area reader needs updating");
        };
        let got = spell(&f.ty);
        ensure!(
            got == *ty,
            "{record}.{field} is {got}, the prop-area reader assumes {ty}"
        );
    }
    for c in [
        "PROP_AREA_MAGIC",
        "PROP_AREA_VERSION",
        "PA_SIZE",
        "prop_info::kLongFlag",
    ] {
        ensure!(
            view.constants.contains_key(c),
            "bionic's prop-area format has no {c}"
        );
    }
    ensure!(
        view.strings.contains_key("kLongLegacyError"),
        "bionic's prop-area format has no kLongLegacyError"
    );
    Ok(())
}
