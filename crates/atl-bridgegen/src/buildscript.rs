// SPDX-License-Identifier: MIT
//! What a consuming crate's `build.rs` needs around the generator: the checkout it builds in, the upstream sources as that checkout resolves them, and the arches.toml row of the target cargo is building for. Generation always targets cargo's `TARGET`; one checked target per `cargo check --target` run.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use atl_layout::Layout;

use crate::c::Kept;
use crate::{Lang, Source, Target};

/// One C API a crate implements, declared by an upstream file: what its build script generates for it.
pub struct Api {
    /// Output stem: `OUT_DIR/<name>.rs`.
    pub name: &'static str,
    /// Declaring file, relative to the AOSP tree.
    pub source: &'static str,
    pub lang: Lang,
    /// Where consumers include it (`None`: no public header declares it, nothing to print).
    pub header: Option<&'static str>,
    /// Exported by the implementation; only these are declared, so declared == implemented == checked.
    pub functions: &'static [&'static str],
    /// Macros (or globs) the C header defines and Rust code uses.
    pub macros: &'static [&'static str],
    /// Include directories the source needs besides the target's, relative to the AOSP tree.
    pub include_dirs: &'static [&'static str],
    /// What the C header keeps of the source for its callers.
    pub kept: Kept<'static>,
}

/// Nothing kept: declarations only.
pub const DECLARATIONS: Kept<'static> = Kept {
    includes: &[],
    macros: &[],
    types: &[],
    records: &[],
};

/// bionic's `android/api-level.h` as an API row: the level names and the two functions, printed include-free. One row for its two consumers: atl-bionic implements and installs it (the converter forces it into every host compile of AOSP code), and atl-native-bridge's host prototype view includes it, so NDK headers parse as the host libraries were compiled.
pub const API_LEVEL: Api = Api {
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
};

/// Generated files of `api` parsed for `target` (plus the API's include dirs), paths relative to
/// OUT_DIR: `<name>.rs` (its constants, opaque types and records, and the check that each function of
/// `implementation`, a Rust path, has the header's signature), `<name>.ir.json`, and with a header
/// `capi/include/<header>` (exactly `functions`, the constants, what `kept` keeps).
pub fn api_files(
    up: &Upstreams,
    api: &Api,
    target: Target,
    implementation: &str,
) -> Result<Vec<(PathBuf, String)>> {
    files_of(up, api, target, implementation, true)
}

/// `api_files` of an API declared in a header the host also has (bionic's `<pthread.h>`): the host's
/// header defines the constants, so the printed header declares the functions only -- a second
/// definition would turn the host's enumerators into macros.
pub fn api_files_beside_host(
    up: &Upstreams,
    api: &Api,
    target: Target,
    implementation: &str,
) -> Result<Vec<(PathBuf, String)>> {
    files_of(up, api, target, implementation, false)
}

fn files_of(
    up: &Upstreams,
    api: &Api,
    mut target: Target,
    implementation: &str,
    with_constants: bool,
) -> Result<Vec<(PathBuf, String)>> {
    target.args.extend(
        api.include_dirs
            .iter()
            .map(|d| format!("-I{}", up.aosp(d).display())),
    );
    let arch = target.arch.clone();
    let path = up.aosp(api.source);
    let src = Source {
        path: &path,
        lang: api.lang,
        also: &[],
        macros: api.macros,
    };
    let iface = crate::interface(api.name, api.source, &src, &[target]).context(api.source)?;
    let mut files = vec![
        (
            format!("{}.rs", api.name).into(),
            crate::rust::api_module(&iface, &arch, api.functions, api.kept.types, implementation)?,
        ),
        (
            format!("{}.ir.json", api.name).into(),
            crate::to_json(&iface)?,
        ),
    ];
    if let Some(header) = api.header {
        let view = &iface.arches[&arch];
        let constants: Vec<&str> = view
            .constants
            .keys()
            .chain(view.strings.keys())
            .map(String::as_str)
            .filter(|_| with_constants)
            .collect();
        files.push((
            Path::new("capi/include").join(header),
            crate::c::header(&iface, &arch, api.functions, &constants, &api.kept)?,
        ));
    }
    Ok(files)
}

/// The build script of a crate implementing (or importing) only `apis`, each checked against
/// `implementation`: generates `api_files` for cargo's target in bionic's view (a C++ source with the
/// host's libc++ first) twice and fails unless byte-identical, replaces `OUT_DIR/capi` (a header the
/// generation stopped writing must not stay on the include path), writes the files. Returns the paths
/// cargo reruns the script on, for it to print (only a build script prints).
pub fn api_crate(apis: &[Api], implementation: &str) -> Result<Vec<PathBuf>> {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR")?);
    let checkout = checkout(&manifest)?;
    let up = Upstreams::of(&checkout)?;
    let arches = checkout.join("arches.toml");
    let host = host_arch(
        &std::fs::read_to_string(&arches)?,
        &std::env::var("TARGET")?,
    )?;
    for api in apis {
        up.file(&format!("aosp/{}", api.source))?;
    }
    let libcxx = if apis.iter().any(|a| a.lang == Lang::Cxx) {
        Some(libcxx_include_dir()?)
    } else {
        None
    };
    let generate = || -> Result<Vec<(PathBuf, String)>> {
        let mut files = Vec::new();
        for api in apis {
            let cxx = libcxx.as_deref().filter(|_| api.lang == Lang::Cxx);
            let target = bionic_libc_target(&up, &host, cxx);
            files.extend(api_files(&up, api, target, implementation)?);
        }
        Ok(files)
    };
    let files = generate()?;
    ensure!(
        files == generate()?,
        "atl-bridgegen output differs between two runs on the same input"
    );
    let out = PathBuf::from(std::env::var("OUT_DIR")?);
    let capi = out.join("capi");
    if capi.exists() {
        std::fs::remove_dir_all(&capi)?;
    }
    for (rel, text) in &files {
        let path = out.join(rel);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&path, text).with_context(|| format!("writing {}", path.display()))?;
    }
    // a source's includes are read too: its directory and the API's include directories
    let mut inputs: Vec<String> = Vec::new();
    for api in apis {
        let dir = Path::new(api.source)
            .parent()
            .map(|d| d.to_string_lossy().into_owned())
            .unwrap_or_default();
        inputs.push(dir);
        inputs.extend(api.include_dirs.iter().map(|d| (*d).to_owned()));
    }
    inputs.sort();
    inputs.dedup();
    let mut rerun = vec![arches];
    for rel in inputs {
        rerun.extend(up.rerun_paths(&format!("aosp/{rel}")));
    }
    Ok(rerun)
}

/// The arches.toml row of a target: the host's or a guest's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchRow {
    /// arches.toml key.
    pub arch: String,
    /// Linux kernel arch (`asm-<kernel_arch>` uapi directory).
    pub kernel_arch: String,
    /// clang triple of bionic on this arch (`[<arch>.guest] clang`): the view bionic's own headers are
    /// parsed in, whatever libc the host build uses.
    pub bionic_clang: String,
    /// Records of `asm-<kernel_arch>/asm/ptrace.h` that bionic-view consumers read by layout (arches.toml `uapi_records`).
    pub uapi_records: Vec<String>,
}

/// The checkout holding `manifest_dir`: the nearest ancestor with arches.toml.
pub fn checkout(manifest_dir: &Path) -> Result<PathBuf> {
    manifest_dir
        .ancestors()
        .find(|d| d.join("arches.toml").is_file())
        .map(Path::to_path_buf)
        .with_context(|| format!("no arches.toml above {}", manifest_dir.display()))
}

/// The clang args that make a probe read host `rust_target`'s pinned sysroot (arches.toml `[<arch>.host] sysroot`, a deps.toml `sysroot_debs` row) and its libc++: `--sysroot`, `-nostdinc++ -isystem <c++/v1>`, as device compiles use libc++.
/// One environment for every host, build machine or cross: generation never reads the build machine's /usr/include, its optional -dev packages or libstdc++ (each once changed a verdict). None: `rust_target` is no arches.toml host (a guest target: its views carry their own `-nostdlibinc` headers).
/// Also the sysroot's `stamp.txt`, for the build script to rerun on a refetch.
pub fn host_sysroot_args(
    checkout: &Path,
    rust_target: &str,
) -> Result<Option<(Vec<String>, PathBuf)>> {
    let table: toml::Table =
        toml::from_str(&std::fs::read_to_string(checkout.join("arches.toml"))?)
            .context("parsing arches.toml")?;
    let Some((arch, host)) = table.iter().find_map(|(arch, row)| {
        let host = row.get("host")?;
        (host.get("rust")?.as_str()? == rust_target).then_some((arch, host))
    }) else {
        return Ok(None);
    };
    let name = host
        .get("sysroot")
        .and_then(|s| s.as_str())
        .with_context(|| {
            format!("arches.toml [{arch}.host]: no sysroot (every host's probes read a pinned one)")
        })?;
    let dir = Layout::of(checkout)?.sysroot(name);
    let root = dir.join("root");
    ensure!(
        root.is_dir(),
        "sysroot {name} of host {arch} is not fetched: cargo xtask deps --fetch {name} --execute"
    );
    let libcxx = atl_layout::libcxx_dir(&root)?;
    let args = vec![
        format!("--sysroot={}", root.display()),
        "-nostdinc++".to_owned(),
        "-isystem".to_owned(),
        libcxx.display().to_string(),
    ];
    Ok(Some((args, dir.join("stamp.txt"))))
}

/// The upstream sources a build script reads, where `atl_layout` places them for this repo checkout: a task copy (`xtask patch --edit`) replaces its `src/` checkout here exactly as in xtask.
pub struct Upstreams {
    layout: Layout,
}

impl Upstreams {
    /// The upstreams of the repo checkout `checkout`.
    pub fn of(checkout: &Path) -> Result<Self> {
        Ok(Upstreams {
            layout: Layout::of(checkout)?,
        })
    }

    /// `rel`, relative to `src/` (`aosp/bionic/libc/include`), where this checkout reads it.
    pub fn path(&self, rel: &str) -> PathBuf {
        self.layout.locate(Path::new(rel)).dir
    }

    /// An AOSP tree path (`bionic/libc/include`) where this checkout reads it.
    pub fn aosp(&self, tree_rel: &str) -> PathBuf {
        self.path(&format!("aosp/{tree_rel}"))
    }

    /// `path(rel)`, failing with the fetch command when it is not a file.
    pub fn file(&self, rel: &str) -> Result<PathBuf> {
        let path = self.path(rel);
        ensure!(
            path.is_file(),
            "{} missing: run `cargo xtask fetch --execute`",
            path.display()
        );
        Ok(path)
    }

    /// The `cargo::rerun-if-changed` paths for reading `rel`: where it is now, and what moves it (a branch switch, a task copy made or removed).
    pub fn rerun_paths(&self, rel: &str) -> Vec<PathBuf> {
        let mut out = vec![self.path(rel)];
        out.extend(self.layout.resolution_inputs(Path::new(rel)));
        out
    }
}

/// The row whose `[<arch>.host] rust` is `rust_target`, read from arches.toml's text.
pub fn host_arch(arches_toml: &str, rust_target: &str) -> Result<ArchRow> {
    let table: toml::Table = toml::from_str(arches_toml).context("parsing arches.toml")?;
    let (arch, row) = table
        .iter()
        .find(|(_, row)| {
            row.get("host")
                .and_then(|h| h.get("rust"))
                .and_then(|r| r.as_str())
                == Some(rust_target)
        })
        .with_context(|| format!("arches.toml: no host row with rust = {rust_target}"))?;
    arch_row(arch, row)
}

/// The row `arch` (an arches.toml key), read from arches.toml's text: the bionic view of a guest arch that is not the host's (the QEMU guest).
pub fn guest_arch(arches_toml: &str, arch: &str) -> Result<ArchRow> {
    let table: toml::Table = toml::from_str(arches_toml).context("parsing arches.toml")?;
    let row = table
        .get(arch)
        .with_context(|| format!("arches.toml: no row {arch}"))?;
    arch_row(arch, row)
}

fn arch_row(arch: &str, row: &toml::Value) -> Result<ArchRow> {
    let kernel_arch = row
        .get("kernel_arch")
        .and_then(|k| k.as_str())
        .with_context(|| format!("arches.toml [{arch}]: no kernel_arch"))?;
    let bionic_clang = row
        .get("guest")
        .and_then(|g| g.get("clang"))
        .and_then(|c| c.as_str())
        .with_context(|| format!("arches.toml [{arch}.guest]: no clang triple"))?;
    let uapi_records = row
        .get("uapi_records")
        .and_then(|r| r.as_array())
        .with_context(|| format!("arches.toml [{arch}]: no uapi_records"))?
        .iter()
        .map(|r| {
            r.as_str()
                .map(str::to_owned)
                .with_context(|| format!("arches.toml [{arch}]: uapi_records holds a non-string"))
        })
        .collect::<Result<Vec<_>>>()?;
    ensure!(
        !uapi_records.is_empty(),
        "arches.toml [{arch}]: uapi_records is empty"
    );
    Ok(ArchRow {
        arch: arch.to_owned(),
        kernel_arch: kernel_arch.to_owned(),
        bionic_clang: bionic_clang.to_owned(),
        uapi_records,
    })
}

/// clang's triple for the target cargo builds for, from cargo's `CARGO_CFG_TARGET_*` (cargo's own
/// triple is not always clang's: `riscv64gc-unknown-linux-gnu` is `riscv64-unknown-linux-gnu`).
pub fn clang_triple_from_cargo() -> Result<String> {
    let cfg = |k: &str| {
        std::env::var(format!("CARGO_CFG_TARGET_{k}")).with_context(|| format!("no TARGET_{k}"))
    };
    Ok(format!(
        "{}-{}-{}-{}",
        cfg("ARCH")?,
        cfg("VENDOR")?,
        cfg("OS")?,
        cfg("ENV")?
    ))
}

/// bionic's kernel uapi include directories for `row`, as Soong's `libc_uapi_headers` exports them:
/// the generic uapi, Android's additions (`linux/compiler_types.h`), the arch's `asm-<kernel_arch>`.
/// Paths relative to `src/`, for `Upstreams::path` / `rerun_paths`.
pub fn bionic_uapi_dirs(row: &ArchRow) -> Vec<String> {
    let kernel = "aosp/bionic/libc/kernel";
    vec![
        format!("{kernel}/uapi"),
        format!("{kernel}/android/uapi"),
        format!("{kernel}/uapi/asm-{}", row.kernel_arch),
    ]
}

/// The target to parse a bionic kernel-uapi header for `row`: no system headers, only
/// `bionic_uapi_dirs`.
pub fn bionic_uapi_target(up: &Upstreams, row: &ArchRow, clang_triple: &str) -> Target {
    let mut args = vec!["-nostdlibinc".to_owned()];
    args.extend(
        bionic_uapi_dirs(row)
            .iter()
            .map(|d| format!("-isystem{}", up.path(d).display())),
    );
    Target {
        arch: row.arch.clone(),
        triple: clang_triple.to_owned(),
        args,
    }
}

/// The target to parse bionic's own libc headers in: bionic's triple for `row`'s arch, no system
/// headers, bionic's `libc/include` then its kernel uapi. `cxx_include`: a C++ standard library to put
/// first (C++ sources); bionic ships none.
pub fn bionic_libc_target(up: &Upstreams, row: &ArchRow, cxx_include: Option<&Path>) -> Target {
    let mut args = vec!["-nostdlibinc".to_owned()];
    let dirs = cxx_include
        .map(Path::to_path_buf)
        .into_iter()
        .chain(std::iter::once(up.aosp("bionic/libc/include")))
        .chain(bionic_uapi_dirs(row).into_iter().map(|d| up.path(&d)));
    args.extend(dirs.map(|d| format!("-isystem{}", d.display())));
    Target {
        arch: row.arch.clone(),
        triple: row.bionic_clang.clone(),
        args,
    }
}

/// The level platform code is built for: bionic's `__ANDROID_API_FUTURE__` (Soong's FutureApiLevelInt, which the converter checks equal). Headers are read at it, so no declaration is hidden behind an availability guard.
pub fn future_level(up: &Upstreams, row: &ArchRow) -> Result<u64> {
    let path = up.aosp("bionic/libc/include/android/api-level.h");
    let src = Source::c(&path, &["__ANDROID_API_FUTURE__"]);
    let target = bionic_libc_target(up, row, None);
    let iface = crate::interface("api_level", "api-level.h", &src, &[target])?;
    Ok(iface.arches[&row.arch].constants["__ANDROID_API_FUTURE__"].bits)
}

/// [`bionic_libc_target`] at API `level` (the triple's suffix): platform code's view at [`future_level`].
pub fn bionic_platform_target(
    up: &Upstreams,
    row: &ArchRow,
    level: u64,
    cxx: Option<&Path>,
) -> Target {
    let mut target = bionic_libc_target(up, row, cxx);
    target.triple = format!("{}{level}", target.triple);
    target
}

/// The host toolchain's libc++ include directory (where `<atomic>` lives), as clang itself finds it
/// for the target cargo builds for.
pub fn libcxx_include_dir() -> Result<PathBuf> {
    let host = Target {
        arch: String::new(),
        triple: clang_triple_from_cargo()?,
        args: vec!["-stdlib=libc++".to_owned()],
    };
    let atomic = crate::front::resolve_include(crate::Lang::Cxx, &host, "atomic")?;
    atomic
        .parent()
        .map(Path::to_path_buf)
        .with_context(|| format!("{} has no directory", atomic.display()))
}

/// One AOSP C header a crate calls or implements by its declared types: [`import_headers`] writes `OUT_DIR/<name>.rs` (`rust::import_module`: per function its fn-pointer type, the records with clang's layout asserted, the macros).
/// Parsed in bionic's view, as every AOSP C header here is; a host-libc crate may use it where the C types have the same sizes on every arches.toml host.
pub struct HeaderImport {
    pub name: &'static str,
    /// The translation unit, relative to the AOSP tree.
    pub source: &'static str,
    /// Headers it includes whose declarations belong to the interface.
    pub also: &'static [&'static str],
    /// Include dirs, relative to the AOSP tree.
    pub include_dirs: &'static [&'static str],
    /// `-D` defines (the Khronos headers declare extension prototypes only under `*_EXT_PROTOTYPES`).
    pub defines: &'static [&'static str],
    /// A header the source needs first and does not include itself (Khronos `eglext.h` / `gl2ext.h` use the core header's types; the registry's convention is that the caller includes both).
    pub force_include: Option<&'static str>,
    pub functions: &'static [&'static str],
    pub types: &'static [&'static str],
    pub macros: &'static [&'static str],
}

/// A build script's whole job for [`HeaderImport`]s: each generated twice, failing unless byte-identical (bridge control 1), written to `OUT_DIR`, with its rerun lines.
pub fn import_headers(imports: &[HeaderImport]) -> Result<()> {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR")?);
    let checkout = checkout(&manifest)?;
    let up = Upstreams::of(&checkout)?;
    let arches = checkout.join("arches.toml");
    let host = host_arch(
        &std::fs::read_to_string(&arches)?,
        &std::env::var("TARGET")?,
    )?;
    let mut rerun = vec![arches];
    let out = PathBuf::from(std::env::var("OUT_DIR")?);
    for import in imports {
        let path = up.file(&format!("aosp/{}", import.source))?;
        let also = import
            .also
            .iter()
            .map(|f| up.file(&format!("aosp/{f}")))
            .collect::<Result<Vec<_>>>()?;
        let mut target = bionic_libc_target(&up, &host, None);
        target.args.extend(
            import
                .include_dirs
                .iter()
                .map(|d| format!("-I{}", up.aosp(d).display())),
        );
        target
            .args
            .extend(import.defines.iter().map(|d| format!("-D{d}")));
        if let Some(h) = import.force_include {
            target.args.extend(["-include".to_owned(), h.to_owned()]);
        }
        let generate = || -> Result<String> {
            let src = Source {
                path: &path,
                lang: Lang::C,
                also: &also,
                macros: import.macros,
            };
            let iface = crate::interface(
                import.name,
                import.source,
                &src,
                std::slice::from_ref(&target),
            )
            .context(import.source)?;
            crate::rust::import_module(&iface, &host.arch, import.functions, import.types)
        };
        let code = generate()?;
        ensure!(
            code == generate()?,
            "atl-bridgegen output differs between two runs on the same input"
        );
        std::fs::write(out.join(format!("{}.rs", import.name)), code)?;
        for d in import.include_dirs {
            rerun.extend(up.rerun_paths(&format!("aosp/{d}")));
        }
        rerun.extend(up.rerun_paths(&format!("aosp/{}", import.source)));
    }
    #[allow(
        clippy::print_stdout,
        reason = "a build script's stdout is cargo's instruction channel"
    )]
    for path in rerun {
        println!("cargo::rerun-if-changed={}", path.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_row_by_rust_triple() {
        let text = "[a]\nkernel_arch = \"ka\"\nuapi_records = [\"r\"]\n[a.host]\nrust = \"a-unknown-linux-gnu\"\n[a.guest]\nclang = \"a-linux-android\"\n[b]\nkernel_arch = \"kb\"\n[b.guest]\nclang = \"b-linux-android\"\n";
        assert_eq!(
            host_arch(text, "a-unknown-linux-gnu").unwrap(),
            ArchRow {
                arch: "a".into(),
                kernel_arch: "ka".into(),
                bionic_clang: "a-linux-android".into(),
                uapi_records: vec!["r".into()],
            }
        );
        // a guest-only row is never a host
        assert!(host_arch(text, "b-linux-android").is_err());
        // a host must name its bionic triple
        let no_guest = "[c]\nkernel_arch = \"kc\"\n[c.host]\nrust = \"c-unknown-linux-gnu\"\n";
        assert!(host_arch(no_guest, "c-unknown-linux-gnu").is_err());
    }
}
