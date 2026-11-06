// SPDX-License-Identifier: MIT
//! LLVM runtimes for a bionic guest arch, built from the pinned llvm-project (sources.toml: the tree of android-16's clang-r547379) by LLVM's own CMake runtimes build, run as-is (CLAUDE.md).
//! Each variant mirrors one runtime builder of AOSP's toolchain/llvm_android `src/llvm_android/builders.py` (at 9596955b, the revision clang-r547379's clang_source_info.md names): same runtimes, same cache entries, same output names, so the archives are the modules prebuilts/clang's Android.bp declares.
//! Differences from that build, each because we have no NDK-style sysroot yet: headers come straight from bionic's `libc_headers` export dirs as the converter evaluates them for the guest (`-nostdlibinc`), and try-compiles stop at a static library (no libc to link); no LTO / AutoFDO / debug info.
//! Output: `<out>/upstream/llvm-project/<arch>/<variant>/` (CMake build dir, `build.log`), then the licence gate over every object those builds compiled. Run by `xtask native --native-bridge <arch>` in two passes: the static runtimes before the converter, whose run links them (-nb-provided);
//! `libc++.so` (SHARED) after the converter's ninja built the guest libc it links against, before the ninja run that links its dependents.

use std::collections::{BTreeMap, BTreeSet};
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
    // libc++_static + libc++demangle). Its libc++.so comes from SHARED (pass 2): it links against bionic's libc.so, which this pre-converter build has not got.
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
            (STATIC_LIBCXX, "libc++_static.a"),
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
            // the file name Soong gives the prebuilt (its module name): every link passes -Wl,--exclude-libs,libunwind.a (cc/config/global.go), which would make libc.so's unwinder exports local (measured: libc++.so's _Unwind_* undefined)
            ("LIBUNWIND_STATIC_OUTPUT_NAME", "unwind-exported"),
            // as the libunwind variant: bionic has no libpthread / libdl archive to name
            ("LIBUNWIND_HAS_PTHREAD_LIB", "OFF"),
            ("LIBUNWIND_HAS_DL_LIB", "OFF"),
        ],
        cflags: "-D_LIBUNWIND_USE_DLADDR=0",
        cxxflags: "-D_LIBUNWIND_USE_DLADDR=0",
        modules: &[("libunwind-exported", "libunwind-exported.a")],
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
        modules: &[(BUILTINS, "libclang_rt.builtins-")],
        staged: &[],
    },
];

/// Pass 2, after the converter's ninja built the guest libc: the shared half of DeviceLibcxxBuilder platform=True noexcept=False (builders.py:1176 `LIBCXX_ENABLE_SHARED` ON), the `libc++` module dynamic-STL platform modules link (cc/stl.go).
/// A build dir of its own, so pass 1's libc++_static.a, which the converter's links already took, is never rebuilt. builders.py links it against its sysroot (`CMAKE_SYSROOT`, 1156): the NDK's libc/libm/libdl stubs + crt objects; ours holds the converter-built guest ones (`sysroot`).
const SHARED: Variant = Variant {
    name: "libcxx-shared",
    runtimes: "libcxx;libcxxabi",
    cache: &[
        ("LIBCXXABI_ENABLE_SHARED", "OFF"),
        ("LIBCXXABI_USE_LLVM_UNWINDER", "OFF"),
        ("LIBCXXABI_NON_DEMANGLING_TERMINATE", "ON"),
        ("LIBCXXABI_STATIC_DEMANGLE_LIBRARY", "ON"),
        ("LIBCXX_ENABLE_SHARED", "ON"),
        // pass 1 built the archive
        ("LIBCXX_ENABLE_STATIC", "OFF"),
        ("LIBCXX_ENABLE_ABI_LINKER_SCRIPT", "OFF"),
        ("LIBCXX_ENABLE_STATIC_ABI_LIBRARY", "ON"),
        ("LIBCXX_STATICALLY_LINK_ABI_IN_SHARED_LIBRARY", "ON"),
        ("LIBCXX_HAS_PTHREAD_LIB", "OFF"),
        ("LIBCXX_HAS_RT_LIB", "OFF"),
        ("LIBCXXABI_HAS_PTHREAD_LIB", "OFF"),
        // the STATIC_LIBRARY try-compiles pass these checks too, and the link would name the libraries (measured: -latomic -lgcc -lgcc_s unresolved); bionic has none, compiler-rt's builtins (linked) carry the atomics and personality helpers
        ("LIBCXX_HAS_ATOMIC_LIB", "OFF"),
        ("LIBCXX_HAS_GCC_LIB", "OFF"),
        ("LIBCXX_HAS_GCC_S_LIB", "OFF"),
        ("LIBCXXABI_HAS_GCC_LIB", "OFF"),
        ("LIBCXXABI_HAS_GCC_S_LIB", "OFF"),
        // soname `libc++.so`, not `libc++.so.1`: what CMake's Android platform module sets (Modules/Platform/Android.cmake), which CMAKE_SYSTEM_NAME Linux does not load
        ("CMAKE_PLATFORM_NO_VERSIONED_SONAME", "ON"),
    ],
    cflags: "",
    cxxflags: "-DHAS_THREAD_LOCAL",
    modules: &[(SHARED_LIBCXX, "libc++.so")],
    staged: LIBCXX_STAGED,
};

/// Module names other code looks up in what the runtimes build: compiler-rt's builtins (pass 2 links them), the static libc++ (whose headers the shared one exports too), the shared libc++.
pub const BUILTINS: &str = "libclang_rt.builtins";
pub const STATIC_LIBCXX: &str = "libc++_static";
pub const SHARED_LIBCXX: &str = "libc++";

/// The guest libraries and objects pass 2 links against (`sysroot`): converter modules of the native bridge side, which the converter's ninja builds before pass 2 (native.rs).
pub const SYSROOT_MODULES: &[&str] = &["libc", "libm", "libdl", "crtbegin_so", "crtend_so"];

/// Where pass 2 puts `libc++.so`, known before it runs: the converter's graph names it as an input of the modules that link `libc++`. `build_shared` fails unless the build put it there.
pub fn shared_libcxx(out: &Path, arch: &str) -> PathBuf {
    out.join("upstream")
        .join(UPSTREAM)
        .join(arch)
        .join(SHARED.name)
        .join("build/lib/libc++.so")
}

/// Pass 2: `libc++.so` for guest `arch`, linked against `libs` (the SYSROOT_MODULES outputs) and pass 1's compiler-rt `builtins` archive; licence gate over what it compiled.
pub fn build_shared(
    root: &Path,
    arch: &str,
    (cc, cxx): (&str, &str),
    out: &Path,
    bionic: &[String],
    libs: &[PathBuf],
    builtins: &Path,
) -> Result<Built> {
    let arches = arches::load(root)?;
    let guest = arches
        .get(arch)
        .and_then(|a| a.guest.as_ref())
        .with_context(|| format!("{arch}: no [guest] row in {}", arches::FILE))?;
    let src = llvm_src(root)?;
    let base = out.join("upstream").join(UPSTREAM).join(arch);
    // the sysroot layout clang's Android toolchain searches for libraries and crt objects: <sysroot>/usr/lib/<triple>
    let sysroot = base.join("sysroot");
    let lib_dir = sysroot.join("usr/lib").join(&guest.clang);
    if sysroot.exists() {
        std::fs::remove_dir_all(&sysroot)?;
    }
    std::fs::create_dir_all(&lib_dir)?;
    for l in libs {
        let name = l.file_name().context("sysroot input has no file name")?;
        std::os::unix::fs::symlink(l, lib_dir.join(name))
            .with_context(|| format!("sysroot {}", l.display()))?;
    }
    let dir = base.join(SHARED.name);
    println!("runtimes: {arch} {} -> {}", SHARED.name, dir.display());
    let sysroot_s = sysroot.display().to_string();
    // builders.py's ldflags (1121-1133): -nostdlib++ -unwindlib=none (libc.so exports the unwinder), packed relocations. -nodefaultlibs + the explicit libs take the place of the first two and of clang's default rtlib: the distro clang defaults to libgcc for Android, prebuilts/clang to its own builtins
    let ldflags = "-nodefaultlibs -Wl,--pack-dyn-relocs=android+relr";
    let stdlibs = format!("-lc -lm -ldl {}", builtins.display());
    let link: &[(&str, &str)] = &[
        ("CMAKE_SYSROOT", &sysroot_s),
        ("CMAKE_SHARED_LINKER_FLAGS", ldflags),
        ("CMAKE_CXX_STANDARD_LIBRARIES", &stdlibs),
    ];
    let built = build(
        &src.join("runtimes"),
        &dir,
        &SHARED,
        (cc, cxx),
        &format!("{}{PLATFORM_API}", guest.clang),
        &includes(bionic)?,
        link,
    )?
    .pop()
    .context("libcxx-shared builds one module")?;
    let want = shared_libcxx(out, arch);
    ensure!(
        built.lib == want,
        "libc++.so at {}, the converter was told {}",
        built.lib.display(),
        want.display()
    );
    println!("runtimes: {:<26} {}", built.module, built.lib.display());
    ensure!(
        gate(root, &base, &[&SHARED])?,
        "runtimes {arch}: licence gate failed"
    );
    Ok(built)
}

/// A built module: its archive or shared object, and the headers dir dependents compile against (libc++'s staged `include/c++/v1`).
pub struct Built {
    pub module: &'static str,
    pub lib: PathBuf,
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
    let src = llvm_src(root)?;
    let base = out.join("upstream").join(UPSTREAM).join(arch);
    let triple = format!("{}{PLATFORM_API}", guest.clang);
    let includes = includes(bionic)?;
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
            &[],
        )?);
    }
    for b in &built {
        println!("runtimes: {:<26} {}", b.module, b.lib.display());
    }
    ensure!(
        gate(root, &base, &VARIANTS.iter().collect::<Vec<_>>())?,
        "runtimes {arch}: licence gate failed"
    );
    Ok(built)
}

/// The pinned llvm-project checkout.
fn llvm_src(root: &Path) -> Result<PathBuf> {
    let sources = sources::load(root)?;
    let c = sources
        .checkouts(None)
        .into_iter()
        .find(|c| matches!(&c.pin, sources::Pin::Git(n) if n == UPSTREAM))
        .with_context(|| format!("no sources.toml row {UPSTREAM}"))?;
    Ok(Layout::of(root)?.locate(&c.dir).dir)
}

/// The C/C++ include flags of every guest runtimes compile: bionic's (`bionic`, the converter's `libc_headers` exports) instead of the host's.
fn includes(bionic: &[String]) -> Result<String> {
    ensure!(
        bionic.iter().any(|f| f == "-isystem"),
        "libc_headers exports no include dir: {bionic:?}"
    );
    Ok(std::iter::once("-nostdlibinc")
        .chain(bionic.iter().map(String::as_str))
        .collect::<Vec<_>>()
        .join(" "))
}

/// Licence gate over every source and header the `variants` builds under `base` compiled; prints a line per variant and the verdict.
fn gate(root: &Path, base: &Path, variants: &[&Variant]) -> Result<bool> {
    let layout = Layout::of(root)?;
    let sources = sources::load(root)?;
    let checkouts = sources.checkouts(None);
    let c = checkouts
        .iter()
        .find(|c| matches!(&c.pin, sources::Pin::Git(n) if n == UPSTREAM))
        .with_context(|| format!("no sources.toml row {UPSTREAM}"))?;
    let src = layout.locate(&c.dir).dir;
    let licenses = licenses::load(root, &sources.upstream_names())?;
    let roots = licence_gate::roots(&layout, &checkouts);
    let mut ok = true;
    for v in variants {
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
    Ok(ok)
}

/// One variant's CMake build; `link` = cache entries of a build that links against the guest libc (pass 2), empty otherwise.
fn build(
    src: &Path,
    dir: &Path,
    v: &Variant,
    (cc, cxx): (&str, &str),
    triple: &str,
    includes: &str,
    link: &[(&str, &str)],
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
        // pass 1 has no bionic libc to link try-compiles against; pass 2 keeps the feature checks pass 1 got
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
    cache.extend(link);
    upstream::cmake(src, &build, &cache, &dir.join("build.log"), false)
        .with_context(|| format!("runtimes {}", v.name))?;
    v.modules
        .iter()
        .map(|(module, file)| {
            let lib = find(&build, file)?.with_context(|| {
                format!("runtimes {}: no {file}* under {}", v.name, build.display())
            })?;
            let include = v
                .staged
                .first()
                .map(|(d, _)| build.join(d))
                .filter(|_| module.starts_with("libc++"));
            Ok(Built {
                module,
                lib,
                include,
            })
        })
        .collect()
}

/// The one `*.a` / `*.so` under `dir` whose name is `prefix` (or starts with it, for a prefix ending in `-`).
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
                && (n.ends_with(".a") || n.ends_with(".so"))
                && (n == prefix || (prefix.ends_with('-') && n.starts_with(prefix)))
            {
                hits.push(p);
            }
        }
    }
    ensure!(hits.len() <= 1, "several {prefix}: {hits:?}");
    Ok(hits.pop())
}

/// Builder classes of AOSP's llvm_android `builders.py` whose literal defines the variants above mirror:
/// variant name -> class. `builtins` is not mapped: BuiltinsBuilder sets none of its defines as literals.
const MIRRORS: &[(&str, &str)] = &[
    ("libcxx-noexcept", "DeviceLibcxxBuilder"),
    ("libcxx", "DeviceLibcxxBuilder"),
    ("libcxx-shared", "DeviceLibcxxBuilder"),
    ("libunwind", "LibUnwindBuilder"),
    ("libunwind-exported", "LibUnwindBuilder"),
];

/// The pinned llvm_android checkout's builder script (sources.toml `llvm-android`), relative to `src/`.
const BUILDERS_PY: &str = "llvm-android/src/llvm_android/builders.py";

/// Per key, the values that `defines['K'] = 'V'` lines of class `class` set literally. A line whose value is a
/// condition (`'A' if c else 'B'`) is not literal and is skipped.
fn literal_defines(text: &str, class: &str) -> BTreeMap<String, BTreeSet<String>> {
    let header = format!("class {class}(");
    let mut out: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut inside = false;
    for line in text.lines() {
        // a column-0 line (class, def, import, comment) ends the previous class body
        if !line.is_empty() && !line.starts_with(char::is_whitespace) {
            inside = line.starts_with(&header);
            continue;
        }
        let Some((key, value)) = line
            .trim_start()
            .strip_prefix("defines['")
            .and_then(|rest| rest.split_once("'] = '"))
            .and_then(|(key, rest)| Some((key, rest.strip_suffix('\'')?)))
        else {
            continue;
        };
        if inside && !value.contains('\'') {
            out.entry(key.to_owned())
                .or_default()
                .insert(value.to_owned());
        }
    }
    out
}

/// The variant's defines against its class: a define the class sets literally must have that value among its
/// literal values. Returns the problems and the count of defines the class does not set literally (not judged).
fn mirror_problems(
    variant: &Variant,
    class: &str,
    upstream: &BTreeMap<String, BTreeSet<String>>,
) -> (Vec<String>, usize) {
    let mut problems = Vec::new();
    let mut unjudged = 0;
    for &(key, value) in variant.cache {
        match upstream.get(key) {
            None => unjudged += 1,
            Some(values) if values.contains(value) => {}
            Some(values) => problems.push(format!(
                "`{}`: {key}={value} here, but {class} sets {key} to {values:?}",
                variant.name
            )),
        }
    }
    (problems, unjudged)
}

/// `xtask check` step `runtimes`: each mirrored variant agrees with builders.py on every define that builders.py
/// sets as a literal. A changed upstream value, or a value that differs here, is named by its define.
pub fn check(root: &Path) -> Result<()> {
    let path = Layout::of(root)?.src().join(BUILDERS_PY);
    ensure!(
        path.is_file(),
        "{} is not fetched (cargo xtask fetch --execute)",
        path.display()
    );
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let mut problems = Vec::new();
    let mut unjudged = 0;
    for &(name, class) in MIRRORS {
        let variant = VARIANTS
            .iter()
            .chain(std::iter::once(&SHARED))
            .find(|v| v.name == name)
            .with_context(|| format!("MIRRORS names no variant `{name}`"))?;
        let (found, not_literal) = mirror_problems(variant, class, &literal_defines(&text, class));
        problems.extend(found);
        unjudged += not_literal;
    }
    ensure!(
        problems.is_empty(),
        "runtimes.rs differs from builders.py:\n  {}",
        problems.join("\n  ")
    );
    eprintln!(
        "runtimes: {} variants match builders.py ({unjudged} defines not set as literals upstream, not judged)",
        MIRRORS.len()
    );
    Ok(())
}

#[cfg(test)]
mod drift_tests {
    use super::*;

    // builders.py's shape: a class with literal and conditional defines, a second class, a column-0 def
    const SAMPLE: &str = "class Other(base):\n    defines['OTHER_ONLY'] = 'ON'\n\nclass DeviceLibcxxBuilder(base):\n    def f(self):\n        defines['LIBCXX_ENABLE_SHARED'] = 'ON'\n        defines['LIBCXXABI_ENABLE_SHARED'] = 'OFF'\n        if self.p:\n            defines['LIBCXX_ENABLE_SHARED'] = 'OFF'\n        defines['LIBUNWIND_HIDE_SYMBOLS'] = 'TRUE' if x else 'FALSE'\n\ndef top():\n    defines['TOP_ONLY'] = 'ON'\n";

    #[test]
    fn literal_defines_of_one_class() {
        let d = literal_defines(SAMPLE, "DeviceLibcxxBuilder");
        assert_eq!(
            d.keys().map(String::as_str).collect::<Vec<_>>(),
            ["LIBCXXABI_ENABLE_SHARED", "LIBCXX_ENABLE_SHARED"]
        );
        assert_eq!(
            d["LIBCXX_ENABLE_SHARED"],
            BTreeSet::from(["ON".to_owned(), "OFF".to_owned()])
        );
    }

    #[test]
    fn drift_names_the_define() {
        let upstream = literal_defines(SAMPLE, "DeviceLibcxxBuilder");
        let ours = Variant {
            name: "test",
            runtimes: "",
            cache: &[
                ("LIBCXX_ENABLE_SHARED", "ON"),
                ("LIBCXXABI_ENABLE_SHARED", "ON"),
                ("LIBCXX_NOT_IN_CLASS", "OFF"),
            ],
            cflags: "",
            cxxflags: "",
            modules: &[],
            staged: &[],
        };
        let (problems, not_literal) = mirror_problems(&ours, "DeviceLibcxxBuilder", &upstream);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(
            problems[0].contains("LIBCXXABI_ENABLE_SHARED=ON"),
            "{problems:?}"
        );
        assert_eq!(not_literal, 1);
    }
}
