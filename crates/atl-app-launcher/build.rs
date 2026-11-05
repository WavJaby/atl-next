// SPDX-License-Identifier: MIT
//! Generates with atl-bridgegen, for the target cargo builds for, each twice and failing unless byte-identical, `OUT_DIR/<name>.rs` per entry of `IMPORTS`:
//! the records and constants the launcher uses with clang's layout asserted, and per function it calls the header's fn-pointer type (`import_module`), so a header change fails the build, not a run.
//! The functions are looked up at run time (`crate::ffi`): the ART libraries are the converter's build output, not inputs of a cargo build.
//! Host headers (glibc's dlfcn.h, the JNI and liblog headers the converter compiles against) are parsed for the host triple; bionic's API in bionic's view, as atl-bionic's build.rs parses it.
//! With feature `converter` (the executable; `xtask native` builds it so) it also links the converter's libsigchain (Cargo.toml `[package.metadata.atl] converter`, atl-bridgegen converter.rs): `-lsigchain` + RUNPATH, as the converter links dalvikvm's device variant (art/dalvikvm Android.bp `target.android.shared_libs`).
//! `rustc-link-lib` of this package goes before std's `-lc` on the link line, so libsigchain precedes libc in DT_NEEDED and wins the global lookup of `sigaction` / `signal`; a `link-arg` would land after `-lc` and lose it.

use std::path::PathBuf;

use anyhow::{Context, Result, ensure};
use atl_bridgegen::buildscript::{self, Upstreams};
use atl_bridgegen::{Lang, Source, Target};

/// Where a header is parsed: host (glibc, cargo's triple) or bionic (bionic's triple and libc headers).
#[derive(PartialEq)]
enum View {
    Host,
    Bionic,
}

struct Import {
    name: &'static str,
    /// AOSP-tree path; `None` with `host_header`.
    source: Option<&'static str>,
    /// A host system header, found as clang finds it.
    host_header: Option<&'static str>,
    /// Host headers the header includes whose records belong to the interface too (else they are opaque), found the same way.
    also: &'static [&'static str],
    lang: Lang,
    view: View,
    functions: &'static [&'static str],
    /// Typedefs whose records the launcher uses besides the functions'.
    types: &'static [&'static str],
    macros: &'static [&'static str],
    include_dirs: &'static [&'static str],
}

const IMPORTS: &[Import] = &[
    // glibc's loader API: how the launcher reaches the ART libraries
    Import {
        name: "dlfcn",
        source: None,
        host_header: Some("dlfcn.h"),
        also: &[],
        lang: Lang::C,
        view: View::Host,
        functions: &["dlopen", "dlsym", "dlerror"],
        types: &[],
        macros: &["RTLD_NOW", "RTLD_GLOBAL", "RTLD_LAZY", "RTLD_NOLOAD"],
        include_dirs: &[],
    },
    // the exit dalvikvm takes (ART's FastExit): no static destructors while runtime daemons may still run; the crash handler's resend (ffi `crash`)
    Import {
        name: "unistd",
        source: None,
        host_header: Some("unistd.h"),
        also: &[],
        lang: Lang::C,
        view: View::Host,
        functions: &["_exit", "syscall", "getpid"],
        types: &[],
        macros: &[],
        include_dirs: &[],
    },
    // debuggerd's crash-signal set and the calls of its handler's equivalent, bionic's main-thread signal stack (ffi `crash`)
    Import {
        name: "signal",
        source: None,
        host_header: Some("signal.h"),
        // glibc defines struct sigaction, sigset_t and stack_t in these
        also: &[
            "bits/sigaction.h",
            "bits/types/__sigset_t.h",
            "bits/types/stack_t.h",
        ],
        lang: Lang::C,
        view: View::Host,
        functions: &["sigaction", "sigfillset", "sigaltstack"],
        types: &[],
        macros: &[
            "SIGABRT",
            "SIGBUS",
            "SIGFPE",
            "SIGILL",
            "SIGSEGV",
            "SIGSTKFLT",
            "SIGSYS",
            "SIGTRAP",
            "SA_RESTART",
            "SA_SIGINFO",
            "SA_ONSTACK",
        ],
        include_dirs: &[],
    },
    Import {
        name: "syscall",
        source: None,
        host_header: Some("sys/syscall.h"),
        also: &[],
        lang: Lang::C,
        view: View::Host,
        functions: &[],
        types: &[],
        macros: &["SYS_rt_tgsigqueueinfo"],
        include_dirs: &[],
    },
    // libc's soname: the object libc's own sigaction is looked up in, as libsigchain looks up its linked_sigaction
    Import {
        name: "lib_names",
        source: None,
        host_header: Some("gnu/lib-names.h"),
        also: &[],
        lang: Lang::C,
        view: View::Host,
        functions: &[],
        types: &[],
        macros: &["LIBC_SO"],
        include_dirs: &[],
    },
    // the JNI invocation API and the JNIEnv / JavaVM function tables
    Import {
        name: "jni",
        source: Some("libnativehelper/include_jni/jni.h"),
        host_header: None,
        also: &[],
        lang: Lang::C,
        view: View::Host,
        functions: &["JNI_CreateJavaVM"],
        types: &["JavaVMInitArgs", "JavaVMOption"],
        macros: &["JNI_VERSION_1_6", "JNI_OK", "JNI_FALSE"],
        include_dirs: &[],
    },
    // libnativehelper's choice of the runtime library (dalvikvm's JniInvocation): JNI_CreateJavaVM forwards to what it loads
    Import {
        name: "jni_invocation",
        source: Some("libnativehelper/include_platform/nativehelper/JniInvocation.h"),
        host_header: None,
        also: &[],
        lang: Lang::C,
        view: View::Host,
        functions: &["JniInvocationCreate", "JniInvocationInit"],
        types: &[],
        macros: &[],
        include_dirs: &["libnativehelper/include_jni"],
    },
    Import {
        name: "log",
        source: Some("system/logging/liblog/include/android/log.h"),
        host_header: None,
        also: &[],
        lang: Lang::C,
        view: View::Host,
        functions: &["__android_log_set_logger"],
        types: &[],
        macros: &[],
        include_dirs: &[],
    },
    Import {
        name: "system_properties",
        source: Some("bionic/libc/include/sys/system_properties.h"),
        host_header: None,
        also: &[],
        lang: Lang::C,
        view: View::Bionic,
        functions: &["__system_properties_init", "__system_property_get"],
        types: &[],
        macros: &["PROP_VALUE_MAX"],
        include_dirs: &[],
    },
    // app mode: libandroid_runtime around the VM the launcher created (patches/aosp/frameworks/base: AndroidRuntime::attachInvokedVm's C ABI)
    Import {
        name: "invoked_vm",
        source: Some("frameworks/base/core/jni/include/android_runtime/invoked_vm.h"),
        host_header: None,
        also: &[],
        lang: Lang::C,
        view: View::Host,
        functions: &["AndroidRuntimeAttachInvokedVm"],
        types: &[],
        macros: &[],
        include_dirs: &["libnativehelper/include_jni"],
    },
    // app mode: libbinder's thread pool, as app_process starts it (AppRuntime::onZygoteInit / onStarted); bionic's view: the NDK headers' availability macros are bionic's sys/cdefs.h
    Import {
        name: "binder_process",
        source: Some("frameworks/native/libs/binder/ndk/include_platform/android/binder_process.h"),
        host_header: None,
        also: &[],
        lang: Lang::C,
        view: View::Bionic,
        functions: &["ABinderProcess_startThreadPool"],
        types: &[],
        macros: &[],
        include_dirs: &["frameworks/native/libs/binder/ndk/include_ndk"],
    },
    // app mode, UI mode: the UI thread's Looper (NDK looper.h, libandroid). The GLib main context's fds and timeout are mirrored into it (atl-platform-ui-linux mirror.rs, over looper.rs); Java's MessageQueue adopts it (NativeMessageQueue: Looper::getForThread)
    Import {
        name: "looper",
        source: Some("frameworks/native/include/android/looper.h"),
        host_header: None,
        also: &[],
        lang: Lang::C,
        view: View::Bionic,
        functions: &["ALooper_prepare", "ALooper_addFd", "ALooper_removeFd"],
        types: &[],
        macros: &[],
        include_dirs: &[],
    },
    // no public header: libdl_android's own declaration (atl-bionic implements it, libdl_android row)
    Import {
        name: "libdl_android",
        source: Some("bionic/libdl/libdl_android.cpp"),
        host_header: None,
        also: &[],
        lang: Lang::Cxx,
        view: View::Bionic,
        functions: &["android_set_application_target_sdk_version"],
        types: &[],
        macros: &[],
        include_dirs: &[],
    },
];

fn main() -> Result<()> {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR")?);
    let checkout = buildscript::checkout(&manifest)?;
    let up = Upstreams::of(&checkout)?;
    let arches = checkout.join("arches.toml");
    let host = buildscript::host_arch(
        &std::fs::read_to_string(&arches)?,
        &std::env::var("TARGET")?,
    )?;
    let triple = buildscript::clang_triple_from_cargo()?;
    let host_target = || Target {
        arch: host.arch.clone(),
        triple: triple.clone(),
        args: Vec::new(),
    };
    let libcxx = buildscript::libcxx_include_dir()?;
    let mut rerun = vec![arches];
    let generate = |import: &Import| -> Result<String> {
        let path = match (import.source, import.host_header) {
            (Some(src), None) => up.file(&format!("aosp/{src}"))?,
            (None, Some(h)) => {
                atl_bridgegen::front::resolve_include(import.lang, &host_target(), h)?
            }
            _ => anyhow::bail!("{}: one of source / host_header", import.name),
        };
        let mut target = match import.view {
            View::Host => host_target(),
            View::Bionic => buildscript::bionic_libc_target(
                &up,
                &host,
                (import.lang == Lang::Cxx).then_some(libcxx.as_path()),
            ),
        };
        target.args.extend(
            import
                .include_dirs
                .iter()
                .map(|d| format!("-I{}", up.aosp(d).display())),
        );
        // not `resolve_include`: an internal header refuses to be included alone; this probe only resolves
        let found =
            atl_bridgegen::front::resolve_includes(import.lang, &host_target(), import.also)?;
        let also = import
            .also
            .iter()
            .map(|h| {
                found
                    .get(*h)
                    .cloned()
                    .with_context(|| format!("<{h}> not found"))
            })
            .collect::<Result<Vec<_>>>()?;
        let src = Source {
            path: &path,
            lang: import.lang,
            also: &also,
            macros: import.macros,
        };
        let label = import.source.or(import.host_header).unwrap_or_default();
        let iface =
            atl_bridgegen::interface(import.name, label, &src, std::slice::from_ref(&target))
                .with_context(|| label.to_owned())?;
        atl_bridgegen::rust::import_module(&iface, &host.arch, import.functions, import.types)
    };
    let out = PathBuf::from(std::env::var("OUT_DIR")?);
    for import in IMPORTS {
        let code = generate(import)?;
        ensure!(
            code == generate(import)?,
            "atl-bridgegen output differs between two runs on the same input"
        );
        std::fs::write(out.join(format!("{}.rs", import.name)), code)?;
        if let Some(src) = import.source {
            let dir = std::path::Path::new(src)
                .parent()
                .context("a source in a directory")?;
            rerun.extend(up.rerun_paths(&format!("aosp/{}", dir.display())));
        }
        for d in import.include_dirs {
            rerun.extend(up.rerun_paths(&format!("aosp/{d}")));
        }
    }
    for path in rerun {
        println!("cargo::rerun-if-changed={}", path.display());
    }
    if std::env::var_os("CARGO_FEATURE_CONVERTER").is_some() {
        let lines = atl_bridgegen::converter::link_package(&manifest)?;
        for l in lines {
            println!("{l}");
        }
        // why: exports.map
        let map = manifest.join("exports.map");
        println!("cargo::rerun-if-changed={}", map.display());
        println!(
            "cargo::rustc-link-arg-bins=-Wl,--version-script={}",
            map.display()
        );
    }
    Ok(())
}
