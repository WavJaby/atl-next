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
    includes: false,
    macros: &[],
};

/// Generated files of `api` parsed for `target` (plus the API's include dirs), paths relative to
/// OUT_DIR: `<name>.rs` (its constants and opaque types, and the check that each function of
/// `implementation`, a Rust path, has the header's signature), `<name>.ir.json`, and with a header
/// `capi/include/<header>` (exactly `functions`, the constants, what `kept` keeps).
pub fn api_files(
    up: &Upstreams,
    api: &Api,
    mut target: Target,
    implementation: &str,
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
            crate::rust::api_module(&iface, &arch, api.functions, implementation)?,
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
            .collect();
        files.push((
            Path::new("capi/include").join(header),
            crate::c::header(&iface, &arch, api.functions, &constants, &api.kept)?,
        ));
    }
    Ok(files)
}

/// The arches.toml row of a host target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostArch {
    /// arches.toml key.
    pub arch: String,
    /// Linux kernel arch (`asm-<kernel_arch>` uapi directory).
    pub kernel_arch: String,
    /// clang triple of bionic on this arch (`[<arch>.guest] clang`): the view bionic's own headers are
    /// parsed in, whatever libc the host build uses.
    pub bionic_clang: String,
}

/// The checkout holding `manifest_dir`: the nearest ancestor with arches.toml.
pub fn checkout(manifest_dir: &Path) -> Result<PathBuf> {
    manifest_dir
        .ancestors()
        .find(|d| d.join("arches.toml").is_file())
        .map(Path::to_path_buf)
        .with_context(|| format!("no arches.toml above {}", manifest_dir.display()))
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
pub fn host_arch(arches_toml: &str, rust_target: &str) -> Result<HostArch> {
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
    let kernel_arch = row
        .get("kernel_arch")
        .and_then(|k| k.as_str())
        .with_context(|| format!("arches.toml [{arch}]: no kernel_arch"))?;
    let bionic_clang = row
        .get("guest")
        .and_then(|g| g.get("clang"))
        .and_then(|c| c.as_str())
        .with_context(|| format!("arches.toml [{arch}.guest]: no clang triple"))?;
    Ok(HostArch {
        arch: arch.clone(),
        kernel_arch: kernel_arch.to_owned(),
        bionic_clang: bionic_clang.to_owned(),
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

/// bionic's kernel uapi include directories for `host`, as Soong's `libc_uapi_headers` exports them:
/// the generic uapi, Android's additions (`linux/compiler_types.h`), the arch's `asm-<kernel_arch>`.
/// Paths relative to `src/`, for `Upstreams::path` / `rerun_paths`.
pub fn bionic_uapi_dirs(host: &HostArch) -> Vec<String> {
    let kernel = "aosp/bionic/libc/kernel";
    vec![
        format!("{kernel}/uapi"),
        format!("{kernel}/android/uapi"),
        format!("{kernel}/uapi/asm-{}", host.kernel_arch),
    ]
}

/// The target to parse a bionic kernel-uapi header for `host`: no system headers, only
/// `bionic_uapi_dirs`.
pub fn bionic_uapi_target(up: &Upstreams, host: &HostArch, clang_triple: &str) -> Target {
    let mut args = vec!["-nostdlibinc".to_owned()];
    args.extend(
        bionic_uapi_dirs(host)
            .iter()
            .map(|d| format!("-isystem{}", up.path(d).display())),
    );
    Target {
        arch: host.arch.clone(),
        triple: clang_triple.to_owned(),
        args,
    }
}

/// The target to parse bionic's own libc headers in: bionic's triple for `host`'s arch, no system
/// headers, bionic's `libc/include` then its kernel uapi. `cxx_include`: a C++ standard library to put
/// first (C++ sources); bionic ships none.
pub fn bionic_libc_target(up: &Upstreams, host: &HostArch, cxx_include: Option<&Path>) -> Target {
    let mut args = vec!["-nostdlibinc".to_owned()];
    let dirs = cxx_include
        .map(Path::to_path_buf)
        .into_iter()
        .chain(std::iter::once(up.aosp("bionic/libc/include")))
        .chain(bionic_uapi_dirs(host).into_iter().map(|d| up.path(&d)));
    args.extend(dirs.map(|d| format!("-isystem{}", d.display())));
    Target {
        arch: host.arch.clone(),
        triple: host.bionic_clang.clone(),
        args,
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_row_by_rust_triple() {
        let text = "[a]\nkernel_arch = \"ka\"\n[a.host]\nrust = \"a-unknown-linux-gnu\"\n[a.guest]\nclang = \"a-linux-android\"\n[b]\nkernel_arch = \"kb\"\n[b.guest]\nclang = \"b-linux-android\"\n";
        assert_eq!(
            host_arch(text, "a-unknown-linux-gnu").unwrap(),
            HostArch {
                arch: "a".into(),
                kernel_arch: "ka".into(),
                bionic_clang: "a-linux-android".into(),
            }
        );
        // a guest-only row is never a host
        assert!(host_arch(text, "b-linux-android").is_err());
        // a host must name its bionic triple
        let no_guest = "[c]\nkernel_arch = \"kc\"\n[c.host]\nrust = \"c-unknown-linux-gnu\"\n";
        assert!(host_arch(no_guest, "c-unknown-linux-gnu").is_err());
    }
}
