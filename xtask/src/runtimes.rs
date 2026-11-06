// SPDX-License-Identifier: MIT
//! LLVM runtimes for a bionic guest arch, built from the pinned llvm-project (sources.toml: the tree of android-16's clang-r547379) by LLVM's own CMake runtimes build, run as-is (CLAUDE.md).
//! Each variant mirrors one runtime builder of AOSP's toolchain/llvm_android `src/llvm_android/builders.py` (at 9596955b, the revision clang-r547379's clang_source_info.md names): same runtimes, same cache entries, same output names, so the archives are the modules prebuilts/clang's Android.bp declares.
//! Differences from that build, each because we have no NDK-style sysroot yet: headers come straight from bionic's `libc_headers` export dirs as the converter evaluates them for the guest (`-nostdlibinc`), and try-compiles stop at a static library (no libc to link); no LTO / AutoFDO / debug info.
//! Output: `<out>/upstream/llvm-project/<arch>/<variant>/` (CMake build dir, `build.log`), then the licence gate over every object those builds compiled. Run by `xtask native --native-bridge <arch>`, whose converter run links the archives (-nb-provided).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use atl_layout::Layout;

use crate::{arches, licence_gate, licenses, sources, upstream};

/// Upstream name in sources.toml.
const UPSTREAM: &str = "llvm-project";

/// Android API level the platform is compiled for: `__ANDROID_API_FUTURE__` (bionic `api-level.h`), what Soong appends to a platform module's clang triple. Without it clang leaves `__ANDROID_API__` unset and bionic hides every `__INTRODUCED_IN` symbol (measured: libc++abi's `aligned_alloc` undeclared).
const PLATFORM_API: &str = "10000";

/// One CMake runtimes build and the Android.bp modules its archives are.
struct Variant {
    name: &'static str,
    runtimes: &'static str,
    cache: &'static [(&'static str, &'static str)],
    /// Extra C/C++ flags of the builder (`cflags` / `cxxflags` properties).
    cflags: &'static str,
    cxxflags: &'static str,
    /// (module, archive file name, found anywhere under the build dir).
    modules: &'static [(&'static str, &'static str)],
    /// (build-relative dir the build copies headers into, llvm-project dirs they come from): the licence gate judges each copy as its source (licence_gate `Staged`).
    staged: &'static [(&'static str, &'static [&'static str])],
}

/// libc++'s CMake copies its headers and libc++abi's public ones into the build's `include/c++/v1`; `__config_site` and `__assertion_handler` are `configure_file` outputs carrying their template's notice.
const LIBCXX_STAGED: &[(&str, &[&str])] =
    &[("include/c++/v1", &["libcxx/include", "libcxxabi/include"])];

/// Cache entries every runtimes variant needs and no builder sets differently: tests and benchmarks are not ours to build.
const NO_TESTS: &[(&str, &str)] = &[
    ("LIBCXX_INCLUDE_TESTS", "OFF"),
    ("LIBCXX_INCLUDE_BENCHMARKS", "OFF"),
    ("LIBCXXABI_INCLUDE_TESTS", "OFF"),
    ("LIBUNWIND_INCLUDE_TESTS", "OFF"),
    ("COMPILER_RT_INCLUDE_TESTS", "OFF"),
];

const VARIANTS: &[Variant] = &[
    // DeviceLibcxxBuilder, platform=True noexcept=True: the dynamic linker's libc++ ("the linker doesn't implement ELF TLS for itself, so it needs to avoid using the per-thread EH globals pointer", prebuilts/clang Android.bp)
    Variant {
        name: "libcxx-noexcept",
        runtimes: "libcxx;libcxxabi",
        cache: &[
            ("LIBCXXABI_ENABLE_SHARED", "OFF"),
            ("LIBCXXABI_USE_LLVM_UNWINDER", "OFF"),
            ("LIBCXXABI_NON_DEMANGLING_TERMINATE", "ON"),
            // patches/llvm-project: llvm_android's move-cxa-demangle-into-libcxxdemangle
            ("LIBCXXABI_STATIC_DEMANGLE_LIBRARY", "ON"),
            ("LIBCXX_ENABLE_SHARED", "OFF"),
            ("LIBCXXABI_ENABLE_EXCEPTIONS", "OFF"),
            ("LIBCXX_ENABLE_EXCEPTIONS", "OFF"),
            ("LIBCXX_STATIC_OUTPUT_NAME", "c++_static_noexcept"),
            ("LIBCXXABI_STATIC_OUTPUT_NAME", "c++abi_noexcept"),
            (
                "LIBCXXABI_DEMANGLE_STATIC_OUTPUT_NAME",
                "c++demangle_noexcept",
            ),
            ("LIBCXX_ENABLE_ABI_LINKER_SCRIPT", "OFF"),
            ("LIBCXX_ENABLE_STATIC_ABI_LIBRARY", "ON"),
            ("LIBCXX_STATICALLY_LINK_ABI_IN_SHARED_LIBRARY", "ON"),
            ("LIBCXX_STATICALLY_LINK_ABI_IN_STATIC_LIBRARY", "ON"),
            // feature detection under STATIC_LIBRARY try-compiles (builders.py sets the same for its fuzzer libc++)
            ("LIBCXX_HAS_PTHREAD_LIB", "OFF"),
            ("LIBCXX_HAS_RT_LIB", "OFF"),
            ("LIBCXXABI_HAS_PTHREAD_LIB", "OFF"),
        ],
        cflags: "",
        // "Required to prevent dlclose from causing crashes on thread exit" (builders.py, android/ndk#1200)
        cxxflags: "-DHAS_THREAD_LOCAL",
        modules: &[
            ("libc++_static_noexcept", "libc++_static_noexcept.a"),
            ("libc++demangle_noexcept", "libc++demangle_noexcept.a"),
        ],
        staged: LIBCXX_STAGED,
    },
    // DeviceLibcxxBuilder, platform=True noexcept=False (builders.py:1045, cache entries 1158-1188, platform libs 1236): the libc++ every other platform module's static STL links (cc/stl.go
    // libc++_static + libc++demangle). One difference: LIBCXX_ENABLE_SHARED stays OFF (builders.py:1176 sets ON): libc++.so links against bionic's libc.so, which this pre-converter build has not got.
    Variant {
        name: "libcxx",
        runtimes: "libcxx;libcxxabi",
        cache: &[
            ("LIBCXXABI_ENABLE_SHARED", "OFF"),
            ("LIBCXXABI_USE_LLVM_UNWINDER", "OFF"),
            ("LIBCXXABI_NON_DEMANGLING_TERMINATE", "ON"),
            ("LIBCXXABI_STATIC_DEMANGLE_LIBRARY", "ON"),
            ("LIBCXX_ENABLE_SHARED", "OFF"),
            ("LIBCXX_STATIC_OUTPUT_NAME", "c++_static"),
            ("LIBCXX_ENABLE_ABI_LINKER_SCRIPT", "OFF"),
            ("LIBCXX_ENABLE_STATIC_ABI_LIBRARY", "ON"),
            ("LIBCXX_STATICALLY_LINK_ABI_IN_SHARED_LIBRARY", "ON"),
            ("LIBCXX_STATICALLY_LINK_ABI_IN_STATIC_LIBRARY", "ON"),
            ("LIBCXX_HAS_PTHREAD_LIB", "OFF"),
            ("LIBCXX_HAS_RT_LIB", "OFF"),
            ("LIBCXXABI_HAS_PTHREAD_LIB", "OFF"),
        ],
        cflags: "",
        cxxflags: "-DHAS_THREAD_LOCAL",
        modules: &[
            ("libc++_static", "libc++_static.a"),
            ("libc++demangle", "libc++demangle.a"),
        ],
        staged: LIBCXX_STAGED,
    },
    // LibUnwindBuilder, is_exported=False: the hermetic copy every other module links
    Variant {
        name: "libunwind",
        runtimes: "libunwind",
        // the HAS_*_LIB checks pass under STATIC_LIBRARY try-compiles and would make the archive name pthread / dl as dependent libraries (lld: "unable to find library from dependent library specifier"); bionic folds both into libc / libdl, where Android's own check finds no such library
        cache: &[
            ("LIBUNWIND_HIDE_SYMBOLS", "TRUE"),
            ("LIBUNWIND_ENABLE_SHARED", "FALSE"),
            ("LIBUNWIND_ENABLE_ASSERTIONS", "FALSE"),
            ("LIBUNWIND_USE_FRAME_HEADER_CACHE", "FALSE"),
            ("LIBUNWIND_HAS_PTHREAD_LIB", "OFF"),
            ("LIBUNWIND_HAS_DL_LIB", "OFF"),
        ],
        cflags: "-D_LIBUNWIND_USE_DLADDR=0",
        cxxflags: "-D_LIBUNWIND_USE_DLADDR=0",
        modules: &[("libunwind", "libunwind.a")],
        staged: &[],
    },
    // LibUnwindBuilder, is_exported=True: libc.so exports the unwinder; the frame header cache needs dlpi_adds/dlpi_subs, which bionic has
    Variant {
        name: "libunwind-exported",
        runtimes: "libunwind",
        cache: &[
            ("LIBUNWIND_HIDE_SYMBOLS", "FALSE"),
            ("LIBUNWIND_ENABLE_SHARED", "FALSE"),
            ("LIBUNWIND_ENABLE_ASSERTIONS", "FALSE"),
            ("LIBUNWIND_USE_FRAME_HEADER_CACHE", "TRUE"),
            // as the libunwind variant: bionic has no libpthread / libdl archive to name
            ("LIBUNWIND_HAS_PTHREAD_LIB", "OFF"),
            ("LIBUNWIND_HAS_DL_LIB", "OFF"),
        ],
        cflags: "-D_LIBUNWIND_USE_DLADDR=0",
        cxxflags: "-D_LIBUNWIND_USE_DLADDR=0",
        modules: &[("libunwind-exported", "libunwind.a")],
        staged: &[],
    },
    // BuiltinsBuilder, is_exported=False (the -exported copy exists for arm and x86 only, prebuilts/clang Android.bp)
    Variant {
        name: "builtins",
        runtimes: "compiler-rt",
        cache: &[
            ("COMPILER_RT_BUILD_BUILTINS", "ON"),
            ("COMPILER_RT_BUILD_CRT", "OFF"),
            ("COMPILER_RT_BUILD_SANITIZERS", "OFF"),
            ("COMPILER_RT_BUILD_XRAY", "OFF"),
            ("COMPILER_RT_BUILD_LIBFUZZER", "OFF"),
            ("COMPILER_RT_BUILD_PROFILE", "OFF"),
            ("COMPILER_RT_BUILD_MEMPROF", "OFF"),
            ("COMPILER_RT_BUILD_ORC", "OFF"),
            ("COMPILER_RT_BUILD_CTX_PROFILE", "OFF"),
            ("COMPILER_RT_DEFAULT_TARGET_ONLY", "ON"),
            ("COMPILER_RT_BUILTINS_HIDE_SYMBOLS", "TRUE"),
        ],
        cflags: "",
        cxxflags: "",
        modules: &[("libclang_rt.builtins", "libclang_rt.builtins-")],
        staged: &[],
    },
];

/// A built module: its archive, and the headers dir dependents compile against (libc++'s staged `include/c++/v1`).
pub struct Built {
    pub module: &'static str,
    pub archive: PathBuf,
    pub include: Option<PathBuf>,
}

/// compiler-rt's public headers in the pinned llvm-project (sanitizer/, xray/): what prebuilts/clang's resource dir carries beside clang's own; bionic compiles include them (bp2ninja -clang-rt-include).
pub fn rt_include(root: &Path) -> Result<PathBuf> {
    let sources = sources::load(root)?;
    let c = sources
        .checkouts(None)
        .into_iter()
        .find(|c| matches!(&c.pin, sources::Pin::Git(n) if n == UPSTREAM))
        .with_context(|| format!("no sources.toml row {UPSTREAM}"))?;
    let dir = Layout::of(root)?
        .locate(&c.dir)
        .dir
        .join("compiler-rt/include");
    ensure!(dir.is_dir(), "{}: missing", dir.display());
    Ok(dir)
}

/// The runtimes of guest `arch` built under `<out>/upstream/llvm-project/<arch>/` with compilers `cc` / `cxx`, against `bionic`: the include flags the converter's `-nb-exports libc_headers` prints. Fails unless the licence gate over what they compiled passes.
pub fn build_guest(
    root: &Path,
    arch: &str,
    cc: &str,
    cxx: &str,
    out: &Path,
    bionic: &[String],
) -> Result<Vec<Built>> {
    let arches = arches::load(root)?;
    let guest = arches
        .get(arch)
        .and_then(|a| a.guest.as_ref())
        .with_context(|| format!("{arch}: no [guest] row in {}", arches::FILE))?;
    let layout = Layout::of(root)?;
    let sources = sources::load(root)?;
    let checkouts = sources.checkouts(None);
    let c = checkouts
        .iter()
        .find(|c| matches!(&c.pin, sources::Pin::Git(n) if n == UPSTREAM))
        .with_context(|| format!("no sources.toml row {UPSTREAM}"))?;
    let src = layout.locate(&c.dir).dir;
    let base = out.join("upstream").join(UPSTREAM).join(arch);
    let triple = format!("{}{PLATFORM_API}", guest.clang);
    ensure!(
        bionic.iter().any(|f| f == "-isystem"),
        "libc_headers exports no include dir: {bionic:?}"
    );
    let includes = std::iter::once("-nostdlibinc")
        .chain(bionic.iter().map(String::as_str))
        .collect::<Vec<_>>()
        .join(" ");
    let mut built = Vec::new();
    for v in VARIANTS {
        let dir = base.join(v.name);
        println!("runtimes: {arch} {} -> {}", v.name, dir.display());
        built.extend(build(
            &src.join("runtimes"),
            &dir,
            v,
            (cc, cxx),
            &triple,
            &includes,
        )?);
    }
    for b in &built {
        println!("runtimes: {:<26} {}", b.module, b.archive.display());
    }
    // licence gate: every source and header the runtimes builds compiled
    let licenses = licenses::load(root, &sources.upstream_names())?;
    let roots = licence_gate::roots(&layout, &checkouts);
    let mut ok = true;
    for v in VARIANTS {
        let build = std::fs::canonicalize(base.join(v.name).join("build"))?;
        let canon_src = std::fs::canonicalize(&src)?;
        let staged: Vec<licence_gate::Staged> = v
            .staged
            .iter()
            .map(|(dir, from)| licence_gate::Staged {
                dir: build.join(dir),
                from: from.iter().map(|f| canon_src.join(f)).collect(),
                key: c.licence_key.clone(),
            })
            .collect();
        let r = licence_gate::check_build(&build, &staged, &roots, &licenses)?;
        println!(
            "runtimes: licence {:<20} {} objects, {} files checked, {} generated, {} system",
            v.name, r.artifacts, r.checked, r.generated, r.system
        );
        for (expr, files) in &r.denied {
            println!(
                "runtimes: licence DENIED {expr}: {} files, e.g. {}",
                files.len(),
                files.keys().next().map_or("", |k| k)
            );
        }
        for u in r.unresolved.iter().take(5) {
            println!("runtimes: licence UNRESOLVED {u}");
        }
        ok &= r.ok();
    }
    println!("runtimes: verdict {}", if ok { "PASS" } else { "FAIL" });
    ensure!(ok, "runtimes {arch}: licence gate failed");
    Ok(built)
}

fn build(
    src: &Path,
    dir: &Path,
    v: &Variant,
    (cc, cxx): (&str, &str),
    triple: &str,
    includes: &str,
) -> Result<Vec<Built>> {
    let build = dir.join("build");
    let cflags = format!("{includes} {}", v.cflags);
    let cxxflags = format!("{includes} {}", v.cxxflags);
    let mut cache: Vec<(&str, &str)> = vec![
        ("CMAKE_BUILD_TYPE", "Release"),
        ("CMAKE_C_COMPILER", cc),
        ("CMAKE_CXX_COMPILER", cxx),
        ("CMAKE_ASM_COMPILER", cc),
        ("CMAKE_C_COMPILER_TARGET", triple),
        ("CMAKE_CXX_COMPILER_TARGET", triple),
        ("CMAKE_ASM_COMPILER_TARGET", triple),
        // cross build for a Linux kernel; `Android` would make CMake look for an NDK
        ("CMAKE_SYSTEM_NAME", "Linux"),
        // no bionic libc to link try-compiles against yet
        ("CMAKE_TRY_COMPILE_TARGET_TYPE", "STATIC_LIBRARY"),
        ("CMAKE_C_FLAGS", &cflags),
        ("CMAKE_CXX_FLAGS", &cxxflags),
        ("CMAKE_ASM_FLAGS", includes),
        // HandleLLVMOptions.cmake reads it (builders.py sets it for every Android runtime)
        ("ANDROID_NATIVE_API_LEVEL", PLATFORM_API),
        ("LLVM_ENABLE_RUNTIMES", v.runtimes),
    ];
    cache.extend(NO_TESTS);
    cache.extend(v.cache);
    upstream::cmake(src, &build, &cache, &dir.join("build.log"), false)
        .with_context(|| format!("runtimes {}", v.name))?;
    v.modules
        .iter()
        .map(|(module, file)| {
            let archive = find(&build, file)?.with_context(|| {
                format!("runtimes {}: no {file}* under {}", v.name, build.display())
            })?;
            let include = v
                .staged
                .first()
                .map(|(d, _)| build.join(d))
                .filter(|_| module.starts_with("libc++"));
            Ok(Built {
                module,
                archive,
                include,
            })
        })
        .collect()
}

/// The one `*.a` under `dir` whose name is `prefix` (or starts with it, for a prefix ending in `-`).
fn find(dir: &Path, prefix: &str) -> Result<Option<PathBuf>> {
    let mut hits = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d)? {
            let e = e?;
            let p = e.path();
            if e.file_type()?.is_dir() {
                stack.push(p);
            } else if let Some(n) = p.file_name().and_then(|n| n.to_str())
                && n.ends_with(".a")
                && (n == prefix || (prefix.ends_with('-') && n.starts_with(prefix)))
            {
                hits.push(p);
            }
        }
    }
    ensure!(hits.len() <= 1, "several {prefix}: {hits:?}");
    Ok(hits.pop())
}
