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

/// `layout_module` of `THREAD_HEADERS` in the guest's bionic view (the identity engine: the host arch).
/// C++: the bionic headers are C++-only (classes, `_Atomic` next to `<atomic>`), parsed with libc++ first as
/// bionic's own C++ sources are.
fn bionic_thread(
    up: &Upstreams,
    host: &buildscript::HostArch,
    out: &std::path::Path,
) -> Result<String> {
    let mut target =
        buildscript::bionic_libc_target(up, host, Some(&buildscript::libcxx_include_dir()?));
    target.args.extend(
        THREAD_INCLUDE_DIRS
            .iter()
            .map(|d| format!("-I{}", up.aosp(d).display())),
    );
    let unit = out.join("bionic_thread.cc");
    std::fs::write(
        &unit,
        "#include \"bionic/pthread_internal.h\"\n#include \"native_bridge_support/linker/static_tls_config.h\"\n",
    )?;
    let also: Vec<PathBuf> = THREAD_HEADERS.iter().map(|h| up.aosp(h)).collect();
    let src = Source {
        path: &unit,
        lang: Lang::Cxx,
        also: &also,
        // the guest TCB slot that holds the engine's per-thread record (x86_64 crossing: guest->host and signal entries find it there)
        macros: &["TLS_SLOT_NATIVE_BRIDGE_GUEST_STATE"],
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
/// The one list: engine/vdso.rs binds exactly these names to the provider, typed by the header's signatures (`provider.rs`).
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
fn provider(up: &Upstreams, host: &buildscript::HostArch, out: &std::path::Path) -> Result<String> {
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
    let mut code = atl_bridgegen::rust::import_module(&iface, &host.arch, &functions, &[])?;
    code.push_str(&format!(
        "\n/// Every provider function, by C name.\npub const NAMES: &[&str] = &{functions:?};\n"
    ));
    Ok(code)
}

/// The JNI function tables, relative to the AOSP tree: `JNINativeInterface` (behind `JNIEnv`) and `JNIInvokeInterface` (behind `JavaVM`).
const JNI_HEADER: &str = "libnativehelper/include_jni/jni.h";
/// Their records, with the Rust module each one's slots go to.
const JNI_TABLES: &[(&str, &str)] = &[("JNINativeInterface", "env"), ("JNIInvokeInterface", "vm")];

/// `jni_tables.rs`: per table, `SLOTS` (field name by slot, in jni.h's order) and one `usize` constant per field = its slot; every field a pointer-sized slot at slot * pointer size, else the build fails (the engine's tables and its stub array index by slot).
fn jni_tables(up: &Upstreams, target: &Target) -> Result<String> {
    let path = up.file(&format!("aosp/{JNI_HEADER}"))?;
    let src = Source {
        path: &path,
        lang: Lang::C,
        also: &[],
        macros: &[],
    };
    let iface = atl_bridgegen::interface("jni", JNI_HEADER, &src, std::slice::from_ref(target))
        .context(JNI_HEADER)?;
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

/// The guest proxy libraries (NBS `libnative_bridge_guest_*`): soname, and the NBS directory whose `stubs_arm64.cc` (arch-neutral, LP64) is the library's symbol set.
const PROXY_LIBS: &[(&str, &str)] = &[
    ("libEGL.so", "libEGL"),
    ("libGLESv1_CM.so", "libGLESv1_CM"),
    ("libGLESv2.so", "libGLESv2"),
    ("libGLESv3.so", "libGLESv3"),
    ("libnativewindow.so", "libnativewindow"),
];
/// The libc stub list whose count sizes the thunk pool (NBS, x86_64 entries).
const LIBC_STUBS: &str = "frameworks/libs/native_bridge_support/android_api/libc/stubs_x86_64.cpp";
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
];
const PROTOTYPE_INCLUDES: &[&str] = &["frameworks/native/opengl/include"];
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

/// `proxy_table.rs` (module `proxy`): per guest proxy library, its NBS stub names in list order, each with the entry it binds.
/// `Entry::Generic` binds the host front's symbol of that name through the generic crossing; `Entry::Trap(i)` binds `TRAPS[i]`, a named trap (`TRAP_NAMES[i]`).
/// The prototypes come from one unit parsed for two views: `guest` (bionic, as NDK apps compile) and `host` (glibc). A stub whose prototype differs between the views fails the build, naming it.
fn proxy_table(up: &Upstreams, host: &buildscript::HostArch, out: &std::path::Path) -> Result<String> {
    let mut lists: Vec<(&str, Vec<String>)> = Vec::new();
    for &(soname, dir) in PROXY_LIBS {
        let rel = format!("frameworks/libs/native_bridge_support/android_api/{dir}/stubs_arm64.cc");
        let path = up.aosp(&rel);
        let text = std::fs::read_to_string(&path).with_context(|| path.display().to_string())?;
        let names: Vec<String> = text
            .lines()
            .filter_map(|l| l.strip_prefix("DEFINE_INTERCEPTABLE_STUB_FUNCTION(")?.strip_suffix(");"))
            .map(str::to_owned)
            .collect();
        ensure!(!names.is_empty(), "{soname}: no stubs in {}", path.display());
        for p in up.rerun_paths(&format!("aosp/{rel}")) {
            println!("cargo::rerun-if-changed={}", p.display());
        }
        lists.push((soname, names));
    }
    let libc_text = std::fs::read_to_string(up.aosp(LIBC_STUBS))?;
    let libc_stubs = libc_text
        .lines()
        .filter(|l| l.starts_with("DEFINE_INTERCEPTABLE_STUB_FUNCTION("))
        .count();
    for p in up.rerun_paths(&format!("aosp/{LIBC_STUBS}")) {
        println!("cargo::rerun-if-changed={}", p.display());
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
            if let Some(d) = diffs.iter().find(|d| &d.name == name).filter(|d| !d.identical) {
                broken.push(format!("{name}: {}", d.differences.join("; ")));
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
    let mut code = String::from(
        "// generated by build.rs (atl-native-bridge) from the NBS stub lists and the EGL/GLES prototypes of the guest and host views, do not edit\nuse super::{Entry, Lib};\n\npub const LIBS: &[Lib] = &[\n",
    );
    let mut total = 0usize;
    for (soname, names) in &lists {
        code.push_str(&format!(
            "    Lib {{\n        soname: {soname:?},\n        stubs: &[\n"
        ));
        for name in names {
            total += 1;
            let mut trap = |why: &'static str| {
                traps.push(((*soname).to_owned(), name.clone(), why));
                format!("Entry::Trap({})", traps.len() - 1)
            };
            let entry = match (guest_view.functions.get(name), host_ir.functions.get(name)) {
                (Some(_), Some(_)) if ANNOTATED.contains(&name.as_str()) => {
                    trap("pending: annotated, GS2.2")
                }
                (Some(sig), Some(_)) if is_fn_ptr(&sig.ret) => trap("pending: fn-return, GS2.2"),
                (Some(sig), Some(_)) if sig.params.iter().any(|p| is_fn_ptr(p)) => {
                    trap("pending: callback-param, GS2.2")
                }
                (Some(_), Some(_)) => "Entry::Generic".to_owned(),
                _ if *soname == "libnativewindow.so" => {
                    trap("pending: nativewindow prototypes not in the EGL/GLES set")
                }
                _ => trap("unbound: no prototype in the EGL/GLES headers"),
            };
            code.push_str(&format!("            ({name:?}, {entry}),\n"));
        }
        code.push_str("        ],\n    },\n");
    }
    code.push_str("];\n\n");
    code.push_str(&format!(
        "/// Stub names over all lists; the libc stubs (`LIBC_STUB_COUNT`) bind from the same pool.\npub const STUB_COUNT: usize = {total};\npub const LIBC_STUB_COUNT: usize = {libc_stubs};\n\n"
    ));
    code.push_str("/// Per `Entry::Trap(i)`: library, name, why trapped.\npub const TRAP_NAMES: &[(&str, &str, &str)] = &[\n");
    for (lib, name, why) in &traps {
        code.push_str(&format!("    ({lib:?}, {name:?}, {why:?}),\n"));
    }
    code.push_str("];\n\n/// `trap::<i>` per `Entry::Trap(i)`.\npub const TRAPS: &[extern \"C\" fn()] = &[\n");
    for i in 0..traps.len() {
        code.push_str(&format!("    super::trap::<{i}>,\n"));
    }
    code.push_str("];\n");
    Ok(code)
}

fn main() -> Result<()> {
    // `--cfg atl_nb_no_fs_restore`: deliberate break of spike gates g1/g2 (engine::x86_64: the host->guest crossing without its FS restore)
    println!("cargo::rustc-check-cfg=cfg(atl_nb_no_fs_restore)");
    // `--cfg atl_g4_break`: g4's deliberate break (tests/g4.rs: a syscall in the host target, must leave the band)
    println!("cargo::rustc-check-cfg=cfg(atl_g4_break)");
    // gate 4.4's deliberate breaks: `atl_nb_jni_direct` (engine/jni.rs: a guest env slot = the host function, no crossing), `atl_nb_nbfp_false` (exports.rs: isNativeBridgeFunctionPointer answers false)
    println!("cargo::rustc-check-cfg=cfg(atl_nb_jni_direct)");
    println!("cargo::rustc-check-cfg=cfg(atl_nb_nbfp_false)");
    // `atl_nb_foreign_gs`: deliberate break of the app gate's `jitUpCall` (engine::x86_64: GS = the record, not ART's Thread*, while an up call's host target runs)
    println!("cargo::rustc-check-cfg=cfg(atl_nb_foreign_gs)");
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR")?);
    let checkout = buildscript::checkout(&manifest)?;
    let up = Upstreams::of(&checkout)?;
    let arches = checkout.join("arches.toml");
    let host = buildscript::host_arch(
        &std::fs::read_to_string(&arches)?,
        &std::env::var("TARGET")?,
    )?;
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
        out.join("bionic_thread.rs"),
        bionic_thread(&up, &host, &out)?,
    )?;
    std::fs::write(out.join("jni_tables.rs"), jni_tables(&up, &target)?)?;
    std::fs::write(out.join("provider.rs"), provider(&up, &host, &out)?)?;
    std::fs::write(
        out.join("proxy_table.rs"),
        proxy_table(&up, &host, &out)?,
    )?;
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
