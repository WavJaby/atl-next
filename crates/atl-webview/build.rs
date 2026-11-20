// SPDX-License-Identifier: MIT
//! Generates `OUT_DIR/jni.rs` with atl-bridgegen from libnativehelper's `jni.h` (the header ART's natives compile against), host view, twice and failing unless byte-identical: the JNIEnv function table with clang's layout asserted, so a header change fails the build, not a run.

use std::path::PathBuf;

use anyhow::{Context, Result, ensure};
use atl_bridgegen::buildscript::{self, HeaderImport, Upstreams, import_headers};
use atl_bridgegen::{Lang, Source, Target};

const JNI_H: &str = "libnativehelper/include_jni/jni.h";

fn main() -> Result<()> {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR")?);
    let checkout = buildscript::checkout(&manifest)?;
    let up = Upstreams::of(&checkout)?;
    let arches = checkout.join("arches.toml");
    let host = buildscript::host_arch(
        &std::fs::read_to_string(&arches)?,
        &std::env::var("TARGET")?,
    )?;
    let target = Target {
        arch: host.arch.clone(),
        triple: buildscript::clang_triple_from_cargo()?,
        args: Vec::new(),
    };
    let path = up.file(&format!("aosp/{JNI_H}"))?;
    let generate = || -> Result<String> {
        let src = Source {
            path: &path,
            lang: Lang::C,
            also: &[],
            macros: &["JNI_VERSION_1_6"],
        };
        let iface = atl_bridgegen::interface("jni", JNI_H, &src, std::slice::from_ref(&target))
            .context(JNI_H)?;
        // JavaVMAttachArgs: the JavaBridge thread attaches under its own name (jni `bridge_thread`)
        atl_bridgegen::rust::import_module(
            &iface,
            &host.arch,
            &[],
            &["JNINativeInterface", "JavaVMAttachArgs"],
        )
    };
    let code = generate()?;
    ensure!(
        code == generate()?,
        "atl-bridgegen output differs between two runs on the same input"
    );
    std::fs::write(
        PathBuf::from(std::env::var("OUT_DIR")?).join("jni.rs"),
        code,
    )?;
    let mut rerun = vec![arches];
    rerun.extend(up.rerun_paths("aosp/libnativehelper/include_jni"));
    for p in rerun {
        buildscript::rerun_if_changed(&p);
    }
    // the library's ABI is its `Java_*` and `JNI_OnLoad` exports; every archive symbol stays local, as the launcher runtime's
    println!("cargo::rustc-link-arg-cdylib=-Wl,--exclude-libs,ALL");
    // the carrier layer's sideband stream (jni `set_sideband`): ANativeWindow's perform hook and native_handle_t, bionic view, layout asserted (atl-egl-driver's import)
    import_headers(&[HeaderImport {
        name: "window",
        source: "frameworks/native/libs/nativewindow/include/system/window.h",
        also: &[
            "frameworks/native/libs/nativebase/include/nativebase/nativebase.h",
            "system/core/libcutils/include/cutils/native_handle.h",
        ],
        include_dirs: &[
            "frameworks/native/libs/nativewindow/include",
            "frameworks/native/libs/nativebase/include",
            "frameworks/native/libs/arect/include",
            "system/core/libcutils/include",
            "system/core/libsystem/include",
        ],
        defines: &[],
        force_include: None,
        functions: &[],
        types: &["ANativeWindow", "native_handle_t"],
        macros: &[],
    }])
}
