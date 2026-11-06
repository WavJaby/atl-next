// SPDX-License-Identifier: MIT
//! Upstreams built by their own build system for `providers.toml` rows `upstream:<name>`: `xtask native` configures, builds and installs each into `<out>/upstream/<name>/prefix` before the converter runs, then hands the converter one `Provided` entry per module (artifact, include dirs, defines; bp2ninja `-provided`).
//! CMake only: the one upstream built this way is BoringSSL (CLAUDE.md). `cmake` is shared with the LLVM runtimes for the bionic targets (`runtimes`), CMake's other allowed use. A second build system becomes a `Recipe` field when its first upstream arrives.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, ensure};

use crate::providers::Provided;

/// How xtask builds one upstream; keyed by its `sources.toml` name.
pub struct Recipe {
    pub name: &'static str,
    /// Android.bp modules the install provides, each with its library: module `(m, lib)` is `<prefix>/lib/<lib>.so` (checked after install).
    pub modules: &'static [(&'static str, &'static str)],
    /// CMake cache entries beyond the toolchain ones every recipe gets.
    cache: &'static [(&'static str, &'static str)],
    /// Cache entries that are also preprocessor defines of the installed headers: dependents compile with `-D<k>=<v>`, so library and users cannot disagree.
    shared_defines: &'static [(&'static str, &'static str)],
}

/// Symbol prefix of our BoringSSL: its C symbols become `ATL_<name>`, its C++ ones move to `bssl::ATL`. App processes share one flat ELF symbol namespace with desktop libraries that may load a system OpenSSL; unprefixed, the first loaded `SHA256_Init` & co. would win for both.
const BORINGSSL_SYMBOL_PREFIX: &str = "ATL";

pub const RECIPES: &[Recipe] = &[Recipe {
    name: "boringssl",
    // libcrypto_static: AOSP's non-FIPS static copy (libcore's JNI libraries, dex2oat: bignums, digests); here the one shared libcrypto, so a process holds one BoringSSL
    modules: &[
        ("libcrypto", "libcrypto"),
        ("libcrypto_static", "libcrypto"),
        ("libssl", "libssl"),
    ],
    // shared: dependents link libcrypto.so / libssl.so as AOSP's device build does; tests are not ours to build
    cache: &[("BUILD_SHARED_LIBS", "ON"), ("BUILD_TESTING", "OFF")],
    shared_defines: &[("BORINGSSL_PREFIX", BORINGSSL_SYMBOL_PREFIX)],
}];

pub fn recipe(name: &str) -> Option<&'static Recipe> {
    RECIPES.iter().find(|r| r.name == name)
}

/// The C/C++ toolchain of the converter's build; an upstream is compiled with the same one.
pub struct Toolchain<'a> {
    pub cc: &'a str,
    pub cxx: &'a str,
    /// The libc++ headers `cxx` is installed with, as the converter derives them (`bp2ninja -libcxx-headers`), not restated here.
    pub libcxx: &'a Path,
}

/// The link flags of the default libc++ selection (bp2ninja stl.go `stlLinkFlags`).
const STL_LINK_FLAGS: &str = "-nostdlib++ -lc++";

/// Configures, builds and installs one upstream (incremental: cmake + ninja reuse `<out>/upstream/<name>/build`; install leaves unchanged files' mtimes alone, so dependents do not relink), then its provided modules.
pub fn build(
    src: &Path,
    out: &Path,
    recipe: &Recipe,
    tc: &Toolchain,
) -> Result<BTreeMap<String, Provided>> {
    ensure!(
        src.join("CMakeLists.txt").is_file(),
        "{}: no CMakeLists.txt (xtask fetch --execute)",
        src.display()
    );
    let dir = out.join("upstream").join(recipe.name);
    let (build, prefix) = (dir.join("build"), dir.join("prefix"));
    std::fs::create_dir_all(&build)?;
    let log = dir.join("build.log");
    let cache = [
        ("CMAKE_BUILD_TYPE", "Release".to_owned()),
        ("CMAKE_C_COMPILER", tc.cc.to_owned()),
        ("CMAKE_CXX_COMPILER", tc.cxx.to_owned()),
        // one C++ runtime per process, selected as the converter selects it for a module with the default stl (bp2ninja stl.go, after Soong's cc/stl.go non-Bionic branch)
        (
            "CMAKE_CXX_FLAGS",
            format!("-nostdinc++ -isystem {}", tc.libcxx.display()),
        ),
        ("CMAKE_SHARED_LINKER_FLAGS", STL_LINK_FLAGS.to_owned()),
        ("CMAKE_EXE_LINKER_FLAGS", STL_LINK_FLAGS.to_owned()),
        ("CMAKE_INSTALL_PREFIX", prefix.display().to_string()),
        // GNUInstallDirs may pick lib/<multiarch>; the artifact path below assumes lib
        ("CMAKE_INSTALL_LIBDIR", "lib".to_owned()),
    ];
    let cache: Vec<(&str, &str)> = cache
        .iter()
        .map(|(k, v)| (*k, v.as_str()))
        .chain(recipe.cache.iter().chain(recipe.shared_defines).copied())
        .collect();
    cmake(src, &build, &cache, &log, true).with_context(|| format!("upstream {}", recipe.name))?;
    let defines: Vec<String> = recipe
        .shared_defines
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect();
    let mut provided = BTreeMap::new();
    for (module, stem) in recipe.modules {
        let lib = prefix.join("lib").join(format!("{stem}.so"));
        ensure!(
            lib.is_file(),
            "upstream {}: install has no {} for module {module}",
            recipe.name,
            lib.display()
        );
        provided.insert(
            (*module).to_owned(),
            Provided {
                lib,
                include: vec![prefix.join("include")],
                defines: defines.clone(),
            },
        );
    }
    Ok(provided)
}

/// Configures (Ninja generator, `cache` as `-D` entries), builds and, with `install`, installs one CMake project; all output to `log`. Incremental: a rerun reuses `build`.
pub fn cmake(
    src: &Path,
    build: &Path,
    cache: &[(&str, &str)],
    log: &Path,
    install: bool,
) -> Result<()> {
    std::fs::create_dir_all(build)?;
    let mut configure = Command::new("cmake");
    configure
        .args(["-G", "Ninja", "-S"])
        .arg(src)
        .arg("-B")
        .arg(build);
    for (k, v) in cache {
        configure.arg(format!("-D{k}={v}"));
    }
    let mut steps = vec![configure, Command::new("cmake")];
    steps[1].arg("--build").arg(build);
    if install {
        let mut i = Command::new("cmake");
        i.arg("--install").arg(build);
        steps.push(i);
    }
    let file = std::fs::File::create(log)?;
    for step in &mut steps {
        let status = step
            .stdout(file.try_clone()?)
            .stderr(file.try_clone()?)
            .status()
            .context("running cmake (deps.toml row `cmake`)")?;
        ensure!(status.success(), "cmake failed, log {}", log.display());
    }
    Ok(())
}
