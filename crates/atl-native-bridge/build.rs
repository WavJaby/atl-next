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
    println!("cargo::rerun-if-changed={}", arches.display());
    for rel in std::iter::once(&HEADER)
        .chain(std::iter::once(&JNI_HEADER))
        .chain(INCLUDE_DIRS)
        .chain(THREAD_HEADERS)
        .chain(THREAD_INCLUDE_DIRS)
    {
        for p in up.rerun_paths(&format!("aosp/{rel}")) {
            println!("cargo::rerun-if-changed={}", p.display());
        }
    }
    Ok(())
}
