// SPDX-License-Identifier: MIT
//! Generates with atl-bridgegen, from AOSP's `nativebridge/native_bridge.h` parsed in the host's (glibc) view -- the NB core lives in ART's process -- `OUT_DIR/native_bridge.rs`:
//! `NativeBridgeCallbacks` and the records it passes with clang's sizes and offsets asserted, and its entry shells (`Table`): one `unsafe extern "C"` function per field with the header's exact signature, panic-shielded, calling `crate::exports::<field>`, plus `NativeBridgeCallbacks::shells()` with `version` = `crate::exports::VERSION` (bridge controls 1 and 2, plan S11).
//! A field added by an AOSP upgrade becomes a shell calling a function `exports` lacks: a compile error, not a silent null entry.

use std::path::PathBuf;

use anyhow::{Context, Result, ensure};
use atl_bridgegen::buildscript::{self, Upstreams};
use atl_bridgegen::ir::Ty;
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
    // pthread_attr_t: the intercepted pthread API hands it to glibc unconverted (engine/intercepts.rs size gate)
    "bionic/libc/include/bits/pthread_types.h",
    // struct sigaction / sigset_t: guest sigaction's host side converts them (engine/signal.rs layout gate)
    "bionic/libc/include/bits/signal_types.h",
    "bionic/libc/private/bionic_tls.h",
    // TLS_SLOT_NATIVE_BRIDGE_GUEST_STATE
    "bionic/libc/platform/bionic/tls_defines.h",
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
        "#include \"bionic/pthread_internal.h\"\n#include \"native_bridge_support/linker/static_tls_config.h\"\n",
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
    // provider.rs: the provider calls a host trampoline that crosses to the guest callback
    "__system_property_foreach",
    "__system_property_read_callback",
];
/// Shims of the x86_64 stub list only (`libc_stubs`): signal.rs registers the guest handler with sigchain behind ART's claims.
const SIGNAL_SHIMS: &[&str] = &["sigaction", "sigaction64"];
/// Q5-8: the QEMU table's libc rows that bind to counted shims under `crossing-count` (counters.rs `memcpy`, `memset`, `memcmp`), which record the size argument and then run glibc's.
const COUNTED_SHIMS: &[&str] = &["memcpy", "memset", "memcmp"];
/// NDK functions taking a `JNIEnv*` (C3, engine/ndk.rs): the generic crossing would hand the host library the guest's env, so each binds `Entry::Shim` in its library. Any NDK proxy library may list them; each must be in one (`bindings_table`).
const NDK_SHIMS: &[&str] = &[
    "AAssetManager_fromJava",
    "AndroidBitmap_getInfo",
    "AndroidBitmap_lockPixels",
    "AndroidBitmap_unlockPixels",
];

/// Guest libc stubs glibc's `libc.so.6` serves with the same ABI, by (stub name, glibc symbol). NBS names the allocator entries `native_bridge_<fn>` (one heap: the guest libc's malloc calls them).
/// The pthread rows rest on intercepts.rs's size gates (`pthread_t` long, `pthread_key_t` int, `pthread_attr_t` only ever filled and read by glibc since every `pthread_attr_*` is here); the rest are the C contract (memory, allocation, time), identical in bionic and glibc on LP64.
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
    ("pthread_detach", "pthread_detach"),
    ("pthread_getattr_np", "pthread_getattr_np"),
    ("pthread_getcpuclockid", "pthread_getcpuclockid"),
    ("pthread_getname_np", "pthread_getname_np"),
    ("pthread_getschedparam", "pthread_getschedparam"),
    ("pthread_join", "pthread_join"),
    ("pthread_kill", "pthread_kill"),
    ("pthread_setname_np", "pthread_setname_np"),
    ("pthread_setschedparam", "pthread_setschedparam"),
    ("pthread_setschedprio", "pthread_setschedprio"),
    ("pthread_sigqueue", "pthread_sigqueue"),
    ("pthread_getspecific", "pthread_getspecific"),
    ("pthread_setspecific", "pthread_setspecific"),
];

/// The provider's soname, as cargo-c names a cdylib (`lib<crate>.so.<v>`): `<v>` is the version's semver-compatible prefix -- the major from 1.0, `0.<minor>` below it, the whole `0.0.<patch>` below that (measured: `libatl_bionic.so.0.0.0`, readelf SONAME). Read from atl-bionic's own manifest so a version bump moves it.
fn provider_soname(manifest_dir: &std::path::Path) -> Result<String> {
    const CRATE: &str = "atl_bionic";
    let path = manifest_dir.join("../atl-bionic/Cargo.toml");
    println!("cargo::rerun-if-changed={}", path.display());
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

/// The JNI function tables, relative to the AOSP tree: `JNINativeInterface` (behind `JNIEnv`) and `JNIInvokeInterface` (behind `JavaVM`).
const JNI_HEADER: &str = "libnativehelper/include_jni/jni.h";
/// Their records, with the Rust module each one's slots go to.
const JNI_TABLES: &[(&str, &str)] = &[("JNINativeInterface", "env"), ("JNIInvokeInterface", "vm")];

/// `jni_tables.rs`: per table, `SLOTS` (field name by slot, in jni.h's order) and one `usize` constant per field = its slot; every field a pointer-sized slot at slot * pointer size, else the build fails (the engine's tables and its stub array index by slot).
fn jni_tables(up: &Upstreams, target: &Target) -> Result<String> {
    let iface = jni_interface(up, target)?;
    let view = iface
        .arches
        .get(&target.arch)
        .context("jni.h: no view of the host arch")?;
    let mut out = format!("// generated by build.rs from {JNI_HEADER} (host view), do not edit\n");
    for (record, module) in JNI_TABLES {
        let rec = view
            .records
            .get(*record)
            .with_context(|| format!("jni.h lacks {record}"))?;
        let word = rec.align;
        ensure!(
            rec.size == word * rec.fields.len() as u64,
            "{record}: size {} is not {} slots of {word} bytes",
            rec.size,
            rec.fields.len()
        );
        out.push_str(&format!(
            "/// `{record}` slots.\n#[allow(non_upper_case_globals, dead_code)]\npub mod {module} {{\n    pub const SLOTS: [&str; {}] = [\n",
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
    Ok(out)
}

/// The parsed jni.h in the host view: the one description `jni_tables` and `jni_lanes` both read.
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

/// `jni_lanes.rs`: per table, each slot's lane (classes of its fixed parameters and result, variadic flag) from `atl_bridgegen::jni`, in jni.h's field order. A parameter with no lane class fails the build.
fn jni_lanes(up: &Upstreams, target: &Target) -> Result<String> {
    let iface = jni_interface(up, target)?;
    let view = iface
        .arches
        .get(&target.arch)
        .context("jni.h: no view of the host arch")?;
    let mut tables = Vec::new();
    for (record, module) in JNI_TABLES {
        let lanes = atl_bridgegen::jni::lanes(view, record)
            .with_context(|| format!("jni.h {record}: lanes"))?;
        tables.push((*module, lanes));
    }
    Ok(format!(
        "// generated by build.rs from {JNI_HEADER} (host view), do not edit\n{}",
        atl_bridgegen::jni::rust(&tables)
    ))
}

/// The guest proxy libraries (NBS `libnative_bridge_guest_*`): soname, and the NBS directory whose `stubs_arm64.cc` (arch-neutral, LP64) is the library's symbol set.
/// The last three are the NDK libraries of C3: a stub gets a binding only when its prototype is in [`PROTOTYPE_HEADERS`]; the rest trap (`missing_prototype`).
const PROXY_LIBS: &[(&str, &str)] = &[
    ("libEGL.so", "libEGL"),
    ("libGLESv1_CM.so", "libGLESv1_CM"),
    ("libGLESv2.so", "libGLESv2"),
    ("libGLESv3.so", "libGLESv3"),
    ("libnativewindow.so", "libnativewindow"),
    ("libandroid.so", "libandroid"),
    ("libandroid_runtime.so", "libandroid_runtime"),
    ("libjnigraphics.so", "libjnigraphics"),
];
/// The guest libc's stub list (NBS), per guest ISA: x86_64 for the identity guest (patch 0003 adds sigaction / sigaction64); arm64 for the QEMU guest, which has no sigaction stub (its guest libc's own syscall reaches QEMU's signal emulation) but setjmp / longjmp ones.
fn libc_stubs(qemu: bool) -> &'static str {
    if qemu {
        "frameworks/libs/native_bridge_support/android_api/libc/stubs_arm64.cpp"
    } else {
        "frameworks/libs/native_bridge_support/android_api/libc/stubs_x86_64.cpp"
    }
}

/// Why the NDK shims (`JNIEnv*` takers) trap in the QEMU guest: their env is the QEMU JNI table, which lands with the JNI slice (plan Q6, D-C).
const QEMU_NDK_PENDING: &str = "pending: NDK shim under QEMU, Q6";
/// The QEMU guest's arches.toml row (engine/qemu/mod.rs `GUEST_ISA`).
const QEMU_GUEST_ARCH: &str = "arm64";
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
    // C3: the NDK headers of libandroid / libjnigraphics; their declarations count through `also` (bindings_table)
    "android/asset_manager.h",
    "android/asset_manager_jni.h",
    "android/bitmap.h",
];
/// The NDK headers' declarations count from `frameworks/native/include/android` (bindings_table `also`).
const NDK_HEADERS: &[&str] = &["asset_manager.h", "asset_manager_jni.h", "bitmap.h"];
const PROTOTYPE_INCLUDES: &[&str] = &[
    "frameworks/native/opengl/include",
    "frameworks/native/include",
    "libnativehelper/include_jni",
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
        println!("cargo::rerun-if-changed={}", p.display());
    }
    Ok(names)
}

/// The libc rows: per NBS libc stub, its entry -- [`SHIMS`] (+ [`SIGNAL_SHIMS`] for the x86_64 list), [`GLIBC`], the provider's [`PROVIDER_APIS`], else a named trap. A name in those lists that the stub list lacks fails the build (a stale row).
fn libc_entries(
    names: &[String],
    qemu: bool,
    trap: &mut dyn FnMut(&str, &'static str) -> String,
) -> Result<Vec<String>> {
    let provider: Vec<&str> = PROVIDER_APIS
        .iter()
        .flat_map(|(_, f)| f.iter().copied())
        .collect();
    let signal: &[&str] = if qemu { &[] } else { SIGNAL_SHIMS };
    // cargo sets CARGO_FEATURE_<NAME> for build scripts; identity and shipped builds keep the glibc rows
    let counted: &[&str] = if qemu && std::env::var_os("CARGO_FEATURE_CROSSING_COUNT").is_some() {
        COUNTED_SHIMS
    } else {
        &[]
    };
    let shims: Vec<&str> = SHIMS.iter().chain(signal).chain(counted).copied().collect();
    for n in shims
        .iter()
        .chain(GLIBC.iter().map(|(n, _)| n))
        .chain(&provider)
    {
        ensure!(
            names.iter().any(|s| s == n),
            "libc.so: {n} is in a binding list but not an NBS libc stub"
        );
    }
    Ok(names
        .iter()
        .map(|name| {
            if shims.contains(&name.as_str()) {
                "Entry::Shim".to_owned()
            } else if let Some((_, sym)) = GLIBC.iter().find(|(n, _)| n == name) {
                format!("Entry::Generic(Host::Glibc, {sym:?}, Class::Int)")
            } else if provider.contains(&name.as_str()) {
                format!("Entry::Generic(Host::Provider, {name:?}, Class::Int)")
            } else {
                trap(name, "unbound: no host side")
            }
        })
        .collect())
}

/// Why a stub with no prototype in the set traps. libandroid and libjnigraphics: the NDK prototypes outside [`PROTOTYPE_HEADERS`] are pending; libandroid_runtime is C++ internals no app calls.
fn missing_prototype(soname: &str) -> &'static str {
    match soname {
        "libnativewindow.so" => "pending: nativewindow prototypes not in the EGL/GLES set",
        "libandroid.so" | "libjnigraphics.so" => "pending: NDK prototypes not in the set",
        "libandroid_runtime.so" => "unbound: C++ internals",
        _ => "unbound: no prototype in the EGL/GLES headers",
    }
}

/// Fixed bindings besides the stub rows: the NBS vdso's four entries and the linker's two hooks (engine/vdso.rs `bind`, `bind_linker`).
const FIXED_BINDINGS: usize = 6;
/// JNI stubs: one per slot of the largest JNI table (`JNINativeInterface`: 233 in jni.h today, `JNIInvokeInterface` fewer; engine/jni.rs asserts the fit), rounded up to 256. The x86 engine's stub array has this length; the QEMU engine reserves as many pool slots.
const JNI_STUBS: usize = 256;
/// `bindings_table.rs` (module `bindings`): every guest library whose stubs the engine binds -- the GL / window / NDK proxies and libc -- with its NBS stub names in list order, each with the entry it binds. The one table `native_bridge_intercept_symbol` resolves (library, name) in.
/// `Entry::Generic(host, symbol, class)` binds `symbol` looked up on that host library's own handle (a GL front, the bionic provider, glibc's libc.so.6) through the generic crossing, `class` the lane class of its result (`atl_bridgegen::jni::result_class`); `Entry::Shim` a hand-written host side; `Entry::Trap(i)` `TRAPS[i]`, a named trap (`TRAP_NAMES[i]`).
/// The proxy prototypes come from one unit parsed for two views: `guest` (bionic, as NDK apps compile) and `host` (glibc). A stub whose prototype differs between the views fails the build, naming it.
fn bindings_table(
    up: &Upstreams,
    host: &buildscript::ArchRow,
    out: &std::path::Path,
    provider_soname: &str,
    qemu: bool,
    x86_disp_bias: usize,
    x86_counted: bool,
) -> Result<String> {
    let mut lists: Vec<(&str, Vec<String>)> = Vec::new();
    for &(soname, dir) in PROXY_LIBS {
        let names = stub_names(
            up,
            &format!("frameworks/libs/native_bridge_support/android_api/{dir}/stubs_arm64.cc"),
        )
        .with_context(|| soname.to_owned())?;
        lists.push((soname, names));
    }
    let libc_names = stub_names(up, libc_stubs(qemu))?;
    for n in NDK_SHIMS {
        ensure!(
            lists
                .iter()
                .any(|(_, names)| names.iter().any(|s| s.as_str() == *n)),
            "{n}: an NDK shim that no NDK proxy library lists"
        );
    }

    let unit = out.join("proxy_prototypes.cc");
    let mut text = String::from("#define EGL_EGLEXT_PROTOTYPES\n#define GL_GLEXT_PROTOTYPES\n");
    for h in PROTOTYPE_HEADERS {
        text.push_str(&format!("#include <{h}>\n"));
    }
    std::fs::write(&unit, text)?;
    let includes: Vec<String> = PROTOTYPE_INCLUDES
        .iter()
        .map(|d| format!("-I{}", up.aosp(d).display()))
        .collect();
    let mut guest = buildscript::bionic_platform_target(
        up,
        host,
        buildscript::future_level(up, host)?,
        Some(&buildscript::libcxx_include_dir()?),
    );
    guest.arch = "guest".to_owned();
    guest.args.extend(includes.iter().cloned());
    // deliberate break of the identity gate: a header force-included into the guest view only (e.g. one widening a GL typedef) must fail the build naming the function
    println!("cargo::rerun-if-env-changed={GUEST_INCLUDE_BREAK}");
    if let Ok(h) = std::env::var(GUEST_INCLUDE_BREAK) {
        println!("cargo::rerun-if-changed={h}");
        guest.args.extend(["-include".to_owned(), h]);
    }
    let host_view = Target {
        arch: "host".to_owned(),
        triple: buildscript::clang_triple_from_cargo()?,
        args: includes,
    };
    // declarations count only from the files named here (`Source::also`): every header of the tree
    let root = up.aosp("frameworks/native/opengl/include");
    let mut also = Vec::new();
    for dir in ["EGL", "GLES", "GLES2", "GLES3", "KHR"] {
        headers_under(&root.join(dir), &mut also)?;
    }
    println!("cargo::rerun-if-changed={}", root.display());
    let ndk = up.aosp("frameworks/native/include/android");
    also.extend(NDK_HEADERS.iter().map(|h| ndk.join(h)));
    println!("cargo::rerun-if-changed={}", ndk.display());
    let src = Source {
        path: &unit,
        lang: Lang::Cxx,
        also: &also,
        macros: &[],
    };
    let iface = atl_bridgegen::interface(
        "proxy_prototypes",
        "EGL and GLES headers, guest and host views",
        &src,
        &[guest, host_view],
    )
    .context("GL prototypes")?;
    let guest_view = iface.arches.get("guest").context("no guest view")?;
    let host_ir = iface.arches.get("host").context("no host view")?;

    // identity: every stub name both views declare must agree (pass-through only where proven)
    let diffs = atl_bridgegen::report::compare_functions(&iface, "guest", "host");
    let mut broken = Vec::new();
    for (_, names) in &lists {
        for name in names {
            if !guest_view.functions.contains_key(name) || !host_ir.functions.contains_key(name) {
                continue;
            }
            // only an equal-width TyDiff::PointerVsInteger passes (abi_identical); every other kind fails
            if let Some(d) = diffs
                .iter()
                .find(|d| &d.name == name)
                .filter(|d| !d.abi_identical())
            {
                let why: Vec<String> = d.differences.iter().map(ToString::to_string).collect();
                broken.push(format!("{name}: {}", why.join("; ")));
            }
        }
    }
    ensure!(
        broken.is_empty(),
        "{} stub prototype(s) differ between the guest and host views: {}",
        broken.len(),
        broken.join(" | ")
    );

    let mut traps: Vec<(String, String, &'static str)> = Vec::new();
    let mut rows: Vec<(&str, Vec<(String, String)>)> = Vec::new();
    for (soname, names) in &lists {
        let mut entries = Vec::new();
        for name in names {
            let mut trap = |why: &'static str| {
                traps.push(((*soname).to_owned(), name.clone(), why));
                format!("Entry::Trap({})", traps.len() - 1)
            };
            let entry = match (guest_view.functions.get(name), host_ir.functions.get(name)) {
                (Some(_), Some(_)) if NDK_SHIMS.contains(&name.as_str()) => {
                    if qemu {
                        trap(QEMU_NDK_PENDING)
                    } else {
                        "Entry::Shim".to_owned()
                    }
                }
                (Some(_), Some(_)) if ANNOTATED.contains(&name.as_str()) => {
                    trap("pending: annotated, GS2.2")
                }
                (Some(sig), Some(_)) if is_fn_ptr(&sig.ret) => trap("pending: fn-return, GS2.2"),
                (Some(sig), Some(_)) if sig.params.iter().any(is_fn_ptr) => {
                    trap("pending: callback-param, GS2.2")
                }
                (Some(sig), Some(_)) => {
                    let ret = atl_bridgegen::jni::result_class(&sig.ret)
                        .with_context(|| format!("{name}: result class"))?;
                    format!("Entry::Generic(Host::System, {name:?}, Class::{ret:?})")
                }
                _ => trap(missing_prototype(soname)),
            };
            let prototyped =
                guest_view.functions.contains_key(name) && host_ir.functions.contains_key(name);
            ensure!(
                !NDK_SHIMS.contains(&name.as_str()) || prototyped,
                "{name}: an NDK shim without a prototype in the set ({PROTOTYPE_HEADERS:?})"
            );
            entries.push((name.clone(), entry));
        }
        rows.push((soname, entries));
    }
    let libc = libc_entries(&libc_names, qemu, &mut |name, why| {
        traps.push(("libc.so".to_owned(), name.to_owned(), why));
        format!("Entry::Trap({})", traps.len() - 1)
    })?;
    rows.push(("libc.so", libc_names.into_iter().zip(libc).collect()));

    let mut code = String::from(
        "// generated by build.rs (atl-native-bridge) from the NBS stub lists, the binding lists and the EGL/GLES prototypes of the guest and host views, do not edit\nuse super::{Class, Entry, Host, Lib};\n\npub const LIBS: &[Lib] = &[\n",
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
    // one pool slot per binding plus the JNI slots (QEMU's JNI stubs are binding ids too), rounded to a power of two; the QEMU pool (engine/qemu/place.rs) is generated at the QEMU table's size (aarch64_pool.rs)
    let thunks = (total + FIXED_BINDINGS + JNI_STUBS).next_power_of_two();
    if qemu {
        std::fs::write(
            out.join("aarch64_pool.rs"),
            format!(
                "{}{}",
                atl_bridgegen::aarch64::constants_rust(),
                atl_bridgegen::aarch64::pool_rust(u32::try_from(thunks)?)
            ),
        )?;
    }
    if !qemu {
        std::fs::write(
            out.join("x86_entries.s"),
            atl_bridgegen::x86::entries_asm(thunks, JNI_STUBS, x86_disp_bias, x86_counted),
        )?;
    }
    code.push_str(&format!(
        "/// Thunk pool slots: the stub rows, the fixed bindings and the JNI slots, rounded to a power of two.\npub const THUNKS: usize = {thunks};\n\n/// JNI stubs: one per slot of the largest JNI table (build.rs `JNI_STUBS`).\npub const JNI_STUBS: usize = {JNI_STUBS};\n\n/// The bionic provider's soname (cargo-c's, build.rs `provider_soname`): `Host::Provider` resolves on its handle.\npub const PROVIDER_SONAME: &str = {provider_soname:?};\n\n"
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
    code.push_str("];\n");
    Ok(code)
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
    // Q4-4 (engine/vdso.rs `bind_linker`): the static-TLS hook keeps its placeholder, so the linker's `bl` branches to 0 and the start gate goes red
    ("qe_no_tls", "atl_nb_qe_no_tls"),
    // Q5-5 (engine/mod.rs `decide`): the host ISA routed to the QEMU engine (unit test `engine::tests::selection`)
    ("host_qemu", "atl_nb_host_qemu"),
    // Q5-5 (engine/mod.rs `decide`): a second ISA needing another engine accepted in one process (unit test `engine::tests::selection`)
    ("second_isa", "atl_nb_second_isa"),
    // Q5-6 (engine/thread.rs `detach`): a guest key destructor never runs at thread exit (qemu-thread gate, `qt_last_value`)
    ("qt_skip_dtor", "atl_nb_qt_skip_dtor"),
    // Q5-6 (engine/intercepts.rs `run`): a guest thread's routine gets the wrong argument (qemu-thread gate, `qt_join`)
    ("qt_routine_arg", "atl_nb_qt_routine_arg"),
    // Q6-2 (engine/dispatch.rs `call_jni`): the host JNI function gets the guest's env instead of the host object (qemu-jni gate, `e`)
    ("jni_guest_env", "atl_nb_jni_guest_env"),
    // Q6-2 (engine/dispatch.rs `call_jni`): x3 and x4 exchanged, so CallStaticIntMethod(2, 3) reaches the host as (3, 2) (qemu-jni gate, `c`)
    ("jni_swap", "atl_nb_jni_swap"),
    // Q6-3 (engine/dispatch.rs `call_host`): the fp lanes reach the host as zeros (qemu-jni gate, `d` and `f`)
    ("v_drop", "atl_nb_v_drop"),
    // Q6-3 (engine/dispatch.rs `call_host`): the stack words reach the host one word early (qemu-jni gate, `d`)
    ("drop_stack", "atl_nb_drop_stack"),
    // Q6-3 (engine/dispatch.rs `fp_result`): an fp result is read from x0 (qemu-jni gate, `d` and `f`)
    ("ret_int", "atl_nb_ret_int"),
    // Q6-3 part 2b (engine/dispatch.rs `call_al0`): a host call runs with %al = 0, so glibc's snprintf skips saving the xmm arguments of its double (qemu-jni gate, `p`)
    ("al_zero", "atl_nb_al_zero"),
    // Q6-1 (engine/qemu/mod.rs `host_trap_ex`): the stack words a trap delivers arrive one word late (qemu-abi gate, `k`)
    ("qa_stack_off", "atl_qa_stack_off"),
    // Q6-1 (engine/qemu/mod.rs `host_trap_ex`): the FP lanes a trap delivers arrive one register late (qemu-abi gate, `v`)
    ("qa_v_lane", "atl_qa_v_lane"),
    // Q5-7 (bridgegen x86.rs `entries_asm`, disp_bias): every identity pool call's displacement one byte short, so each lands one byte past its shell (identity gates g1-g5 and unit test `thunk_slots_call_their_shell`)
    ("x86_disp", X86_DISP_CFG),
    // Q5-8b (bridgegen x86.rs `entries_asm`, `counted`): the identity shells call no counter, so every identity crossing counts 0 (identity gate `identity_crossing`, check t)
    ("id_count", X86_COUNT_CFG),
    // Q6-4 (bridgegen x86.rs `down_entries_asm`, `drop_stack`): a down call's stack words start one word late (qemu-jni gate, check m)
    ("down_drop_stack", X86_DOWN_DROP_CFG),
    // Q6-4 (x86.rs `down_entries_asm`, `swap_v`): a down call's %xmm0 and %xmm1 are exchanged on entry (qemu-jni gate, checks m and x: the one fp argument moves to %xmm1)
    ("down_swap_v", X86_DOWN_SWAP_CFG),
    // Q6-4 (engine/dispatch.rs `down_params`): the first shorty parameter's class is flipped (qemu-jni gate, checks m, x and R: the 15-integer R-1 call then fits in eight stack words)
    ("shorty_swap", "atl_nb_shorty_swap"),
    // Q6-4 (engine/dispatch.rs `down_body`): a down call's first argument stays the host's, not the guest env or vm (qemu-jni gate, checks o, m and x)
    ("down_noenv", "atl_nb_down_noenv"),
];
/// The cfg of the `x86_disp` break, read here: it selects the generator's displacement bias, no crate code.
const X86_DISP_CFG: &str = "atl_nb_x86_disp";
/// The cfg of the `id_count` break: with it the identity shells are generated plain, feature or not.
const X86_COUNT_CFG: &str = "atl_nb_id_count";
/// The cfgs of the `down_drop_stack` and `down_swap_v` breaks, read here: they select the down shells' generator flags (x86.rs `down_entries_asm`), no crate code.
const X86_DOWN_DROP_CFG: &str = "atl_nb_down_drop_stack";
const X86_DOWN_SWAP_CFG: &str = "atl_nb_down_swap_v";
/// Down shells of the QEMU engine (Q6-4, x86.rs `down_entries_asm`): one per host->guest trampoline a process binds (each native method, JNI_OnLoad, JNI_OnUnload). Fixed; a binding past it fails with an error (place.rs `down`).
const DOWN_SLOTS: usize = 1024;

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
    // one guest bionic view per engine, both compiled into one core (runtime selection, engine/mod.rs `Active`): identity = the host arch, QEMU = arm64 (engine/qemu/mod.rs GUEST_ISA)
    let qemu_guest = buildscript::guest_arch(&arches_text, QEMU_GUEST_ARCH)?;
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
        bionic_thread(&up, &qemu_guest, &out)?,
    )?;
    std::fs::write(out.join("jni_tables.rs"), jni_tables(&up, &target)?)?;
    std::fs::write(out.join("jni_lanes.rs"), jni_lanes(&up, &target)?)?;
    std::fs::write(out.join("provider.rs"), provider(&up, &host, &out)?)?;
    let provider_soname = provider_soname(&manifest)?;
    // Q5-7: the identity engine's byte templates (patch encodings), and its shells and pools (`bindings_table`)
    std::fs::write(
        out.join("x86_encodings.rs"),
        atl_bridgegen::x86::constants_rust(),
    )?;
    let x86_disp_bias = usize::from(selected_breaks()?.contains(&X86_DISP_CFG));
    // the counted shells: feature `crossing-count` (cargo's env for build scripts), unless the `id_count` break is selected
    let x86_counted = std::env::var_os("CARGO_FEATURE_CROSSING_COUNT").is_some()
        && !selected_breaks()?.contains(&X86_COUNT_CFG);
    // Q6-4: the QEMU engine's down shells (x86.rs `down_entries_asm`) and their sizes for place.rs
    let breaks = selected_breaks()?;
    std::fs::write(
        out.join("x86_down.s"),
        atl_bridgegen::x86::down_entries_asm(
            DOWN_SLOTS,
            breaks.contains(&X86_DOWN_DROP_CFG),
            breaks.contains(&X86_DOWN_SWAP_CFG),
        ),
    )?;
    std::fs::write(
        out.join("down_pool.rs"),
        format!(
            "pub const DOWN_SLOTS: usize = {DOWN_SLOTS};\npub const DOWN_SHELL: usize = {};\n",
            atl_bridgegen::x86::DOWN_SHELL
        ),
    )?;
    for (name, qemu) in [("identity", false), ("qemu", true)] {
        std::fs::write(
            out.join(format!("bindings_table_{name}.rs")),
            bindings_table(
                &up,
                &host,
                &out,
                &provider_soname,
                qemu,
                x86_disp_bias,
                x86_counted,
            )?,
        )?;
    }
    println!("cargo::rerun-if-changed={}", arches.display());
    for rel in std::iter::once(&HEADER)
        .chain(std::iter::once(&JNI_HEADER))
        .chain(INCLUDE_DIRS)
        .chain(THREAD_HEADERS)
        .chain(THREAD_INCLUDE_DIRS)
        .chain(PROVIDER_FILES)
    {
        for p in up.rerun_paths(&format!("aosp/{rel}")) {
            println!("cargo::rerun-if-changed={}", p.display());
        }
    }
    Ok(())
}
