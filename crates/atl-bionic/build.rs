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
//! Generates twice and fails unless byte-identical.
//!
//! Every header is parsed as bionic sees it (bionic's triple, its libc headers, no host headers): the
//! declarations are bionic's, the printed C is libc-neutral. The prop-area sources are C++ and include
//! `<atomic>` / `<new>`; bionic ships no C++ library, so the host toolchain's libc++ headers come first
//! (found by clang's own search rules). Layouts there use only `uint32_t`, `char` and 32-bit atomics.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use atl_bridgegen::buildscript::{self, HostArch};
use atl_bridgegen::ir::{ArchView, Interface, Ty};
use atl_bridgegen::{Lang, Source, Target};

/// One bionic API this crate implements.
struct Api {
    /// Output stem: `OUT_DIR/<name>.rs`.
    name: &'static str,
    /// Declaring file, relative to the AOSP tree.
    source: &'static str,
    lang: Lang,
    /// Where consumers include it (`None`: no public header declares it, nothing to print).
    header: Option<&'static str>,
    /// Exported by `crate::ffi`; only these are declared, so declared == implemented == checked.
    functions: &'static [&'static str],
    /// Macros (or globs) the C header defines and Rust code uses.
    macros: &'static [&'static str],
}

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
    },
    // libdl's platform-only setter; ART declares it itself, the launcher calls it (owner Q5)
    Api {
        name: "libdl_android",
        source: "bionic/libdl/libdl_android.cpp",
        lang: Lang::Cxx,
        header: None,
        functions: &["android_set_application_target_sdk_version"],
        macros: &[],
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

/// Generated files: path relative to OUT_DIR -> text.
type Outputs = Vec<(PathBuf, String)>;

fn main() -> Result<()> {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR")?);
    let checkout = buildscript::checkout(&manifest)?;
    let workspace = buildscript::workspace(&checkout)?;
    let arches = checkout.join("arches.toml");
    let host = buildscript::host_arch(
        &std::fs::read_to_string(&arches)?,
        &std::env::var("TARGET")?,
    )?;
    let aosp = workspace.join("src/aosp");
    for rel in APIS
        .iter()
        .map(|a| a.source)
        .chain(PROP_AREA_SOURCES.iter().copied())
        .chain(PROP_AREA_HEADERS.iter().copied())
    {
        buildscript::upstream_file(&workspace, &format!("aosp/{rel}"))?;
    }
    let out = PathBuf::from(std::env::var("OUT_DIR")?);
    let libcxx = buildscript::libcxx_include_dir()?;

    let first = generate(&aosp, &host, &libcxx, &out)?;
    ensure!(
        first == generate(&aosp, &host, &libcxx, &out)?,
        "atl-bridgegen output differs between two runs on the same input"
    );
    for (rel, text) in &first {
        let path = out.join(rel);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&path, text).with_context(|| format!("writing {}", path.display()))?;
    }

    println!("cargo::rerun-if-changed={}", arches.display());
    for dir in INPUT_DIRS {
        println!("cargo::rerun-if-changed={}", aosp.join(dir).display());
    }
    Ok(())
}

/// The level platform code is built for: bionic's `__ANDROID_API_FUTURE__` (Soong's FutureApiLevelInt,
/// which the converter checks equal). Headers are read at it, so no declaration is hidden behind an
/// availability guard.
fn future_level(aosp: &Path, host: &HostArch) -> Result<u64> {
    let path = aosp.join("bionic/libc/include/android/api-level.h");
    let src = Source::c(&path, &["__ANDROID_API_FUTURE__"]);
    let target = buildscript::bionic_libc_target(aosp, host, None);
    let iface = atl_bridgegen::interface("api_level", "api-level.h", &src, &[target])?;
    Ok(iface.arches[&host.arch].constants["__ANDROID_API_FUTURE__"].bits)
}

/// bionic's view at the platform level, with a C++ library first for C++ sources.
fn bionic_target(aosp: &Path, host: &HostArch, level: u64, cxx: Option<&Path>) -> Target {
    let mut target = buildscript::bionic_libc_target(aosp, host, cxx);
    target.triple = format!("{}{level}", target.triple);
    target
}

fn generate(aosp: &Path, host: &HostArch, libcxx: &Path, out: &Path) -> Result<Outputs> {
    let mut files = Outputs::new();
    let level = future_level(aosp, host)?;
    for api in APIS {
        let target = bionic_target(aosp, host, level, (api.lang == Lang::Cxx).then_some(libcxx));
        let path = aosp.join(api.source);
        let src = Source {
            path: &path,
            lang: api.lang,
            also: &[],
            macros: api.macros,
        };
        let iface = atl_bridgegen::interface(api.name, api.source, &src, &[target])
            .with_context(|| api.source)?;
        files.push((
            format!("{}.rs", api.name).into(),
            atl_bridgegen::rust::api_module(&iface, &host.arch, api.functions, "crate::ffi")?,
        ));
        files.push((
            format!("{}.ir.json", api.name).into(),
            atl_bridgegen::to_json(&iface)?,
        ));
        if let Some(header) = api.header {
            let view = &iface.arches[&host.arch];
            let constants: Vec<&str> = view
                .constants
                .keys()
                .chain(view.strings.keys())
                .map(String::as_str)
                .collect();
            files.push((
                Path::new("capi/include").join(header),
                atl_bridgegen::c::header(&iface, &host.arch, api.functions, &constants)?,
            ));
        }
    }
    let iface = prop_area(aosp, bionic_target(aosp, host, level, Some(libcxx)), out)?;
    check_prop_area_shape(&iface.arches[&host.arch])?;
    files.push((
        "prop_area.rs".into(),
        atl_bridgegen::rust::layout_module(&iface, &host.arch)?,
    ));
    files.push(("prop_area.ir.json".into(), atl_bridgegen::to_json(&iface)?));
    Ok(files)
}

/// bionic's property-area format: both implementation files in one unit, layouts from their headers.
fn prop_area(aosp: &Path, mut target: Target, out: &Path) -> Result<Interface> {
    let unit = out.join("prop_area_format.cc");
    let text: String = PROP_AREA_SOURCES
        .iter()
        .map(|s| format!("#include \"{}\"\n", aosp.join(s).display()))
        .collect();
    std::fs::write(&unit, text)?;
    let also: Vec<PathBuf> = PROP_AREA_SOURCES
        .iter()
        .chain(PROP_AREA_HEADERS)
        .map(|s| aosp.join(s))
        .collect();
    target.args.extend(
        PROP_AREA_INCLUDES
            .iter()
            .map(|d| format!("-I{}", aosp.join(d).display())),
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
