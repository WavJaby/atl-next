// SPDX-License-Identifier: MIT
//! LLVM runtimes for a bionic guest arch, built from the pinned llvm-project (sources.toml: the tree of android-16's clang-r547379) by LLVM's own CMake runtimes build, run as-is (CLAUDE.md).
//! Each variant mirrors one runtime builder of AOSP's toolchain/llvm_android `src/llvm_android/builders.py` (at 9596955b, the revision clang-r547379's clang_source_info.md names): same runtimes, same cache entries, same output names, so the archives are the modules prebuilts/clang's Android.bp declares.
//! Differences from that build, each because we have no NDK-style sysroot yet: headers come straight from bionic's `libc_headers` export dirs (`-nostdlibinc`), and try-compiles stop at a static library (no libc to link); no LTO / AutoFDO / debug info.
//! Output: `<out>/upstream/llvm-project/<arch>/<variant>/` (CMake build dir, `build.log`), then the licence gate over every object those builds compiled.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use atl_layout::Layout;

use crate::{arches, licence_gate, licenses, slot, sources, upstream};

/// Upstream name in sources.toml.
const UPSTREAM: &str = "llvm-project";

/// Android API level the platform is compiled for: `__ANDROID_API_FUTURE__` (bionic `api-level.h`), what Soong appends to a platform module's clang triple. Without it clang leaves `__ANDROID_API__` unset and bionic hides every `__INTRODUCED_IN` symbol (measured: libc++abi's `aligned_alloc` undeclared).
const PLATFORM_API: &str = "10000";

/// bionic `libc_headers` + `libc_uapi_headers` export_system_include_dirs (bionic/libc/Android.bp), relative to bionic/libc; the arch block adds `kernel/uapi/asm-<kernel_arch>`.
const BIONIC_INCLUDES: &[&str] = &[
    "include",
    "kernel/uapi",
    "kernel/android/scsi",
    "kernel/android/uapi",
];

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
}

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
    },
    // LibUnwindBuilder, is_exported=False: the hermetic copy every other module links
    Variant {
        name: "libunwind",
        runtimes: "libunwind",
        cache: &[
            ("LIBUNWIND_HIDE_SYMBOLS", "TRUE"),
            ("LIBUNWIND_ENABLE_SHARED", "FALSE"),
            ("LIBUNWIND_ENABLE_ASSERTIONS", "FALSE"),
            ("LIBUNWIND_USE_FRAME_HEADER_CACHE", "FALSE"),
        ],
        cflags: "-D_LIBUNWIND_USE_DLADDR=0",
        cxxflags: "-D_LIBUNWIND_USE_DLADDR=0",
        modules: &[("libunwind", "libunwind.a")],
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
        ],
        cflags: "-D_LIBUNWIND_USE_DLADDR=0",
        cxxflags: "-D_LIBUNWIND_USE_DLADDR=0",
        modules: &[("libunwind-exported", "libunwind.a")],
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
    },
];

#[derive(clap::Args)]
pub struct Args {
    /// Guest arch (an arches.toml row with `[guest]`).
    #[arg(long, default_value = "x86_64")]
    arch: String,
    /// C compiler (deps.toml row `clang`).
    #[arg(long, default_value = "clang-19")]
    cc: String,
    /// C++ compiler (deps.toml row `clang`).
    #[arg(long, default_value = crate::libcxx_gate::CXX)]
    cxx: String,
    /// Build directory root (default: this checkout's slot, as `xtask native`).
    #[arg(long)]
    out: Option<PathBuf>,
}

/// A built module: its archive.
pub struct Built {
    pub module: &'static str,
    pub archive: PathBuf,
}

pub fn run(root: &Path, args: &Args) -> Result<std::process::ExitCode> {
    let arches = arches::load(root)?;
    let arch = arches
        .get(&args.arch)
        .with_context(|| format!("{}: no row in {}", args.arch, arches::FILE))?;
    let guest = arch
        .guest
        .as_ref()
        .with_context(|| format!("{}: no [guest] in {}", args.arch, arches::FILE))?;
    let layout = Layout::of(root)?;
    let sources = sources::load(root)?;
    let checkouts = sources.checkouts(None);
    let c = checkouts
        .iter()
        .find(|c| matches!(&c.pin, sources::Pin::Git(n) if n == UPSTREAM))
        .with_context(|| format!("no sources.toml row {UPSTREAM}"))?;
    let src = layout.locate(&c.dir).dir;
    let libc = layout.locate(Path::new("aosp/bionic/libc")).dir;
    let (host, _) = arches::build_machine(root, &arches)?;
    let out = slot::out_dir(root, args.out.as_deref(), "--out", |w| {
        w.join("out").join("native").join(host)
    })?;
    let base = out.join("upstream").join(UPSTREAM).join(&args.arch);
    let triple = format!("{}{PLATFORM_API}", guest.clang);
    let mut includes = String::from("-nostdlibinc");
    for d in BIONIC_INCLUDES
        .iter()
        .map(|d| libc.join(d))
        .chain([libc.join(format!("kernel/uapi/asm-{}", arch.kernel_arch))])
    {
        ensure!(
            d.is_dir(),
            "{}: missing (xtask fetch --execute)",
            d.display()
        );
        includes.push_str(&format!(" -isystem {}", d.display()));
    }
    let mut built = Vec::new();
    for v in VARIANTS {
        let dir = base.join(v.name);
        println!("runtimes: {} {} -> {}", args.arch, v.name, dir.display());
        built.extend(build(
            &src.join("runtimes"),
            &dir,
            v,
            args,
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
        let r = licence_gate::check_build(&build, &roots, &licenses)?;
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
    Ok(if ok {
        std::process::ExitCode::SUCCESS
    } else {
        std::process::ExitCode::FAILURE
    })
}

fn build(
    src: &Path,
    dir: &Path,
    v: &Variant,
    args: &Args,
    triple: &str,
    includes: &str,
) -> Result<Vec<Built>> {
    let build = dir.join("build");
    let cflags = format!("{includes} {}", v.cflags);
    let cxxflags = format!("{includes} {}", v.cxxflags);
    let mut cache: Vec<(&str, &str)> = vec![
        ("CMAKE_BUILD_TYPE", "Release"),
        ("CMAKE_C_COMPILER", &args.cc),
        ("CMAKE_CXX_COMPILER", &args.cxx),
        ("CMAKE_ASM_COMPILER", &args.cc),
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
            Ok(Built { module, archive })
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
