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
//!   forwards to bionic's kernel uapi headers the host lacks (`shape::Shape::uapi`). Each must add no
//!   error to its host header (a substitute or forward: compile alone), as C and C++.
//!   `OUT_DIR/shape_gaps.txt`: what they leave different (names bionic's private files bring in).
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
use atl_bridgegen::buildscript::{self, Api, DECLARATIONS, HostArch, Upstreams};
use atl_bridgegen::c::Kept;
use atl_bridgegen::ir::{ArchView, Interface, Ty};
use atl_bridgegen::{Lang, Source, Target, front, shape};

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
    Api {
        name: "api_level",
        source: "bionic/libc/include/android/api-level.h",
        lang: Lang::C,
        header: Some("android/api-level.h"),
        functions: &[
            "android_get_device_api_level",
            "android_get_application_target_sdk_version",
        ],
        macros: &["__ANDROID_API_*__"],
        include_dirs: &[],
        kept: DECLARATIONS,
    },
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
];

/// bionic API declared in a header the host also has, as (API row, host header): the row's header is
/// `atl-bionic/<header>`, included by the shape wrapper of `<header>` (`shape::Inputs::extra`). A row is
/// generated, and implemented (`cfg(atl_bionic_api = "<row>")`), only where the host's header with
/// `_GNU_SOURCE` declares none of its functions; where it declares all, the host's serve, as any libc
/// function; some but not all fails. `pthread_gettid_np`: glibc >= 2.42. `setprogname` / `getprogname`: BSD, no glibc has them.
const IN_HOST_HEADERS: &[(&str, &str)] = &[("pthread", "pthread.h"), ("progname", "stdlib.h")];

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
const EXTRA_INCLUDES: &[(&str, &str)] = &[
    ("unistd.h", "asm/unistd.h"),
    ("sys/socket.h", "fcntl.h"),
    ("sys/vfs.h", "linux/magic.h"),
];

/// Records of a kernel uapi header, declared alone (bridgegen's C printer, layout asserted against
/// bionic's view) in a generated header the shape wrapper of a host header includes (`shape::Inputs::extra`).
struct UapiRecords {
    /// Include name; the file is the host arch's own, as bionic's search order finds it (`asm-<kernel_arch>`).
    uapi: &'static str,
    records: &'static [&'static str],
    host_header: &'static str,
    /// Generated, under `capi/include`.
    header: &'static str,
}

/// `struct pt_regs`: libmemunreachable ThreadCapture.cpp takes the stack pointer's index in the
/// `PTRACE_GETREGSET` buffer from it after `<sys/ptrace.h>`, which reaches it on bionic through
/// `<linux/ptrace.h>`. The record only: uapi's `PTRACE_*` macros would turn glibc's C++ `enum
/// __ptrace_request` enumerators into numbers. An arch whose uapi names it otherwise (arm64
/// `user_pt_regs`) fails the generation until its record is listed.
const UAPI_RECORDS: &[UapiRecords] = &[UapiRecords {
    uapi: "asm/ptrace.h",
    records: &["pt_regs"],
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
const FORWARDED_MACROS: &[&str] = &[
    "__unused",
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
    {
        up.file(&format!("aosp/{rel}"))?;
    }
    let out = PathBuf::from(std::env::var("OUT_DIR")?);
    let libcxx = buildscript::libcxx_include_dir()?;

    let first = generate(&up, &host, &libcxx, &out)?;
    ensure!(
        first == generate(&up, &host, &libcxx, &out)?,
        "atl-bridgegen output differs between two runs on the same input"
    );
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
    check_wrappers(&first.wrappers, &capi.join("include"))?;
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

/// The level platform code is built for: bionic's `__ANDROID_API_FUTURE__` (Soong's FutureApiLevelInt,
/// which the converter checks equal). Headers are read at it, so no declaration is hidden behind an
/// availability guard.
fn future_level(up: &Upstreams, host: &HostArch) -> Result<u64> {
    let path = up.aosp("bionic/libc/include/android/api-level.h");
    let src = Source::c(&path, &["__ANDROID_API_FUTURE__"]);
    let target = buildscript::bionic_libc_target(up, host, None);
    let iface = atl_bridgegen::interface("api_level", "api-level.h", &src, &[target])?;
    Ok(iface.arches[&host.arch].constants["__ANDROID_API_FUTURE__"].bits)
}

/// bionic's view at the platform level, with a C++ library first for C++ sources.
fn bionic_target(up: &Upstreams, host: &HostArch, level: u64, cxx: Option<&Path>) -> Target {
    let mut target = buildscript::bionic_libc_target(up, host, cxx);
    target.triple = format!("{}{level}", target.triple);
    target
}

/// What selects the second view headers are read in (`shape::Inputs::gnu`): clang++ predefines it, so
/// every device C++ unit sees both libcs' `__USE_GNU` sections.
const GNU_VIEW: &str = "-D_GNU_SOURCE";

/// The host libc's view, as device compiles see it.
fn host_target(host: &HostArch) -> Result<Target> {
    Ok(Target {
        arch: host.arch.clone(),
        triple: buildscript::clang_triple_from_cargo()?,
        args: Vec::new(),
    })
}

/// Whether the host's `header`, with `_GNU_SOURCE`, declares `functions`: all (true), none (false); some fails.
fn host_declares(host: &HostArch, header: &str, functions: &[&str]) -> Result<bool> {
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

fn generate(up: &Upstreams, host: &HostArch, libcxx: &Path, out: &Path) -> Result<Generated> {
    let mut files = Outputs::new();
    let level = future_level(up, host)?;
    let mut extra: Vec<(&str, &str)> = EXTRA_INCLUDES.to_vec();
    let mut host_rows = Vec::new();
    for api in APIS {
        let target = bionic_target(up, host, level, (api.lang == Lang::Cxx).then_some(libcxx));
        let Some(&(_, header)) = IN_HOST_HEADERS.iter().find(|(n, _)| *n == api.name) else {
            files.extend(buildscript::api_files(up, api, target, "crate::ffi")?);
            continue;
        };
        if host_declares(host, header, api.functions)? {
            continue;
        }
        extra.push((
            header,
            api.header
                .context("an IN_HOST_HEADERS row without a header")?,
        ));
        host_rows.push(api.name);
        files.extend(buildscript::api_files_beside_host(
            up,
            api,
            target,
            "crate::ffi",
        )?);
    }
    for row in UAPI_RECORDS {
        files.push(uapi_records(up, host, level, row)?);
        extra.push((row.host_header, row.header));
    }
    let iface = prop_area(up, bionic_target(up, host, level, Some(libcxx)), out)?;
    check_prop_area_shape(&iface.arches[&host.arch])?;
    files.push((
        "prop_area.rs".into(),
        atl_bridgegen::rust::layout_module(&iface, &host.arch)?,
    ));
    files.push(("prop_area.ir.json".into(), atl_bridgegen::to_json(&iface)?));
    let timespec = timespec(up, host, level, out)?;
    files.push((
        "timespec.rs".into(),
        atl_bridgegen::rust::layout_module(&timespec, &host.arch)?,
    ));
    let shape = shape(up, host, level, &extra)?;
    for (name, text) in &shape.wrappers {
        let path = Path::new("capi/include").join(name);
        ensure!(
            !files.iter().any(|(p, _)| *p == path),
            "{name}: both a generated declaration header and a shape wrapper"
        );
        files.push((path, text.clone()));
    }
    files.push(("shape_gaps.txt".into(), shape.gaps));
    Ok(Generated {
        files,
        wrappers: shape.wrappers.into_iter().map(|(n, _)| n).collect(),
        host_inputs: shape.host_inputs,
        host_rows,
    })
}

/// The header declaring `row`'s records as bionic's uapi does, with their layout asserted.
fn uapi_records(
    up: &Upstreams,
    host: &HostArch,
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
    let kept = Kept {
        records: row.records,
        ..Kept::default()
    };
    let text = atl_bridgegen::c::header(&iface, &host.arch, &[], &[], &kept)
        .with_context(|| format!("{label}: {:?}", row.records))?;
    Ok((Path::new("capi/include").join(row.header), text))
}

/// The shape wrappers and what they leave different from bionic.
struct ShapeOut {
    /// Include name -> text.
    wrappers: Vec<(String, String)>,
    /// `shape::print_gaps`.
    gaps: String,
    /// The host headers they were derived from.
    host_inputs: BTreeSet<PathBuf>,
}

fn shape(up: &Upstreams, host: &HostArch, level: u64, extra: &[(&str, &str)]) -> Result<ShapeOut> {
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
    let kernel = kernel_headers(&uapi)?;
    let scan = front::resolve_includes(
        Lang::C,
        &host_target,
        &strs(&kernel.keys().cloned().collect::<Vec<_>>()),
    )?;
    let declared: Vec<&str> = APIS.iter().filter_map(|a| a.header).collect();
    let derived = shape::derive(&shape::Inputs {
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
        declared: &declared,
        scan: &scan,
        kernel: &kernel,
        extra,
        cxx: &host_cxx,
    })?;
    shape::check_guards(derived.wrappers.keys())?;
    let mut wrappers: Vec<(String, String)> = derived
        .wrappers
        .iter()
        .map(|(name, w)| {
            let source = if w.substitute {
                format!("{BIONIC_INCLUDE}/{name}")
            } else if bionic.contains_key(name) {
                format!("{BIONIC_INCLUDE}/{name} and the host's <{name}>")
            } else {
                format!("the host's <{name}>")
            };
            Ok((name.clone(), shape::print(name, w, &source)?))
        })
        .collect::<Result<_>>()?;
    let aosp = up.aosp("");
    for (name, file) in &derived.uapi {
        ensure!(
            !derived.wrappers.contains_key(name),
            "<{name}>: both a shape wrapper and a bionic-only uapi header"
        );
        let source = file
            .strip_prefix(&aosp)
            .unwrap_or(file)
            .display()
            .to_string();
        wrappers.push((name.clone(), shape::print_forward(file, &source)?));
    }
    let host_inputs = host_probes
        .values()
        .chain(host_gnu.values())
        .chain(host_cxx.values())
        .flat_map(|p| p.files.keys().cloned())
        .chain(scan.into_values())
        .collect();
    Ok(ShapeOut {
        wrappers,
        gaps: shape::print_gaps(&derived.gaps),
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
/// alone): an include cycle or a definition clash fails the build, not the converter.
fn check_wrappers(wrappers: &[String], include: &Path) -> Result<()> {
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
            ensure!(
                added.is_empty(),
                "the generated <{name}> adds errors as {lang:?}:\n  {}",
                added
                    .iter()
                    .map(|e| e.as_str())
                    .collect::<Vec<_>>()
                    .join("\n  ")
            );
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

/// `struct timespec`'s layout, the field `__system_property_wait`'s timeout is read through: evaluated in
/// bionic's view and in the host libc's, which must agree -- bionic's header declares the parameter,
/// glibc-built callers pass glibc's struct (Rust callers built for Android pass the `libc` crate's
/// bionic one). The result is bionic's view, its constants named after `TIMESPEC_PROBE`'s macros.
fn timespec(up: &Upstreams, host: &HostArch, level: u64, out: &Path) -> Result<Interface> {
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
