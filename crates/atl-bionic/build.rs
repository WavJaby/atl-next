// SPDX-License-Identifier: MIT
//! Generates, with atl-bridgegen from bionic's pinned sources, for the target cargo builds for:
//! - `OUT_DIR/capi/include/<header>`: C declarations of the bionic-only API this crate implements, for
//!   AOSP code built against glibc (cargo-c installs them; the converter puts them on device compiles).
//! - `OUT_DIR/<api>.rs`: that API's constants and opaque types, and a check that every export of
//!   `crate::ffi` has the header's signature (bridge control 1).
//! - `OUT_DIR/prop_area.rs`: bionic's property-area format -- record layouts, magic, version, flags --
//!   from `prop_area.cpp` / `prop_info.cpp` and their C++ headers (bridge control 2: the reader and the
//!   image builder take every offset from here, so a format change moves them or fails the build).
//!
//! - `OUT_DIR/capi/include/<header>`, also: include wrappers giving the host libc's headers bionic's
//!   header shape (`atl_bridgegen::shape`), installed with the declarations, so they reach exactly the
//!   units that see bionic's headers, and substitutes for the bionic-only headers in `SUBSTITUTES`. Each
//!   must add no error to its host header (a substitute: compile alone), as C and C++.
//!   `OUT_DIR/shape_gaps.txt`: what they leave different (names bionic's private files bring in).
//!
//! Generates twice and fails unless byte-identical.
//!
//! Every header is parsed as bionic sees it (bionic's triple, its libc headers, no host headers): the
//! declarations are bionic's, the printed C is libc-neutral. The prop-area sources are C++ and include
//! `<atomic>` / `<new>`; bionic ships no C++ library, so the host toolchain's libc++ headers come first
//! (found by clang's own search rules). Layouts there use only `uint32_t`, `char` and 32-bit atomics.

use std::collections::BTreeSet;
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
            "__system_property_set",
            "__system_property_find",
            "__system_property_read_callback",
            "__system_property_foreach",
            "__system_property_read",
            "__system_property_get",
            "__system_property_area_serial",
            "__system_property_serial",
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
    // it, owner Q5) and the exported-namespace lookup (libvulkan, libgraphicsenv, libstagefright,
    // libvndksupport)
    Api {
        name: "libdl_android",
        source: "bionic/libdl/libdl_android.cpp",
        lang: Lang::Cxx,
        header: None,
        functions: &[
            "android_set_application_target_sdk_version",
            "android_get_exported_namespace",
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
        },
    },
    // what the native closure calls of fdsan (2026-10-31: libsfplugin_ccodec C2NodeImpl.cpp)
    Api {
        name: "fdsan",
        source: "bionic/libc/include/android/fdsan.h",
        lang: Lang::C,
        header: Some("android/fdsan.h"),
        functions: &["android_fdsan_set_error_level"],
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
        },
    },
];

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
    "system/logging/liblog/include",
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
/// hidden visibility; bionic's `resolv_netid.h` as libandroid_net includes it).
const FORWARDED_MACROS: &[&str] = &["__unused", "IS_ELF", "GID_MAX", "__LIBC_HIDDEN__"];

/// bionic-only public headers AOSP code includes, generated whole (`shape::Inputs::substitutes`); each
/// row reviewed to mean the same on glibc. `sys/limits.h` is `<limits.h>` (libandroid_net net.c);
/// `sys/endian.h` is byte-order macros over compiler builtins, its `htonl` family the host's
/// (libmediametrics ItemSerialize-*.cpp).
const SUBSTITUTES: &[&str] = &["sys/endian.h", "sys/limits.h"];

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
    // the triple the wrappers are checked for, so tests compile against them as this does
    println!(
        "cargo::rustc-env=ATL_BIONIC_CLANG_TRIPLE={}",
        buildscript::clang_triple_from_cargo()?
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

fn generate(up: &Upstreams, host: &HostArch, libcxx: &Path, out: &Path) -> Result<Generated> {
    let mut files = Outputs::new();
    let level = future_level(up, host)?;
    for api in APIS {
        let target = bionic_target(up, host, level, (api.lang == Lang::Cxx).then_some(libcxx));
        files.extend(buildscript::api_files(up, api, target, "crate::ffi")?);
    }
    let iface = prop_area(up, bionic_target(up, host, level, Some(libcxx)), out)?;
    check_prop_area_shape(&iface.arches[&host.arch])?;
    files.push((
        "prop_area.rs".into(),
        atl_bridgegen::rust::layout_module(&iface, &host.arch)?,
    ));
    files.push(("prop_area.ir.json".into(), atl_bridgegen::to_json(&iface)?));
    let shape = shape(up, host, level)?;
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
    })
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

fn shape(up: &Upstreams, host: &HostArch, level: u64) -> Result<ShapeOut> {
    let include = up.aosp(BIONIC_INCLUDE);
    let public: Vec<String> = headers_under(&include)?
        .into_iter()
        .filter(|n| !shape::is_private(n))
        .collect();
    let bionic = front::probe_includes(
        Lang::C,
        &bionic_target(up, host, level, None),
        &strs(&public),
    )
    .context("bionic's public headers")?;
    let uapi: Vec<PathBuf> = buildscript::bionic_uapi_dirs(host)
        .iter()
        .map(|d| up.path(d))
        .collect();
    let host_target = Target {
        arch: host.arch.clone(),
        triple: buildscript::clang_triple_from_cargo()?,
        args: Vec::new(),
    };
    let names: Vec<String> = shape::host_names(&bionic, &include, &uapi, HOMES)
        .into_iter()
        .collect();
    let host_probes = front::probe_includes(Lang::C, &host_target, &strs(&names))
        .context("the host's headers")?;
    let mut kernel = BTreeSet::new();
    for dir in &uapi {
        kernel.extend(headers_under(dir)?);
    }
    let scan = front::resolve_includes(
        Lang::C,
        &host_target,
        &strs(&kernel.into_iter().collect::<Vec<_>>()),
    )?;
    let derived = shape::derive(&shape::Inputs {
        bionic: &bionic,
        host: &host_probes,
        bionic_include: &include,
        uapi: &uapi,
        homes: HOMES,
        macros: FORWARDED_MACROS,
        substitutes: SUBSTITUTES,
        scan: &scan,
    })?;
    shape::check_guards(derived.wrappers.keys())?;
    let wrappers = derived
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
    let host_inputs = host_probes
        .values()
        .flat_map(|p| p.files.keys().cloned())
        .chain(scan.into_values())
        .collect();
    Ok(ShapeOut {
        wrappers,
        gaps: shape::print_gaps(&derived.gaps),
        host_inputs,
    })
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
