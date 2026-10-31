// SPDX-License-Identifier: MIT
//! Upstreams built by their own build system for `providers.toml` rows `upstream:<name>`: `xtask native` configures, builds and installs each into `<out>/upstream/<name>/prefix` before the converter runs, then hands the converter one artifact + compile flags per provided module (bp2ninja `-provided`).
//! CMake only: the one upstream built this way is BoringSSL, and CMake is allowed for it alone (CLAUDE.md). A second build system becomes a `Recipe` field when its first upstream arrives.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, ensure};
use serde::Serialize;

/// How xtask builds one upstream; keyed by its `sources.toml` name.
pub struct Recipe {
    pub name: &'static str,
    /// Android.bp modules the install provides; module `m` is `<prefix>/lib/<m>.so` (checked after install).
    pub modules: &'static [&'static str],
    /// CMake cache entries beyond the toolchain ones every recipe gets.
    cache: &'static [(&'static str, &'static str)],
    /// Cache entries that are also preprocessor defines of the installed headers: dependents compile with `-D<k>=<v>`, so library and users cannot disagree.
    shared_defines: &'static [(&'static str, &'static str)],
}

/// Symbol prefix of our BoringSSL: its C symbols become `ATL_<name>`, its C++ ones move to `bssl::ATL`. App processes share one flat ELF symbol namespace with desktop libraries that may load a system OpenSSL; unprefixed, the first loaded `SHA256_Init` & co. would win for both.
const BORINGSSL_SYMBOL_PREFIX: &str = "ATL";

pub const RECIPES: &[Recipe] = &[Recipe {
    name: "boringssl",
    modules: &["libcrypto", "libssl"],
    // shared: dependents link libcrypto.so / libssl.so as AOSP's device build does; tests are not ours to build
    cache: &[("BUILD_SHARED_LIBS", "ON"), ("BUILD_TESTING", "OFF")],
    shared_defines: &[("BORINGSSL_PREFIX", BORINGSSL_SYMBOL_PREFIX)],
}];

pub fn recipe(name: &str) -> Option<&'static Recipe> {
    RECIPES.iter().find(|r| r.name == name)
}

/// One provided module as the converter reads it (bp2ninja provided.go `Provided`).
#[derive(Serialize)]
pub struct Provided {
    pub lib: String,
    pub cflags: Vec<String>,
}

/// The C/C++ toolchain of the converter's build; an upstream is compiled with the same one.
pub struct Toolchain<'a> {
    pub cc: &'a str,
    pub cxx: &'a str,
}

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
        // one C++ runtime per process: the converter compiles and links all C++ against libc++
        ("CMAKE_CXX_FLAGS", "-stdlib=libc++".to_owned()),
        ("CMAKE_SHARED_LINKER_FLAGS", "-stdlib=libc++".to_owned()),
        ("CMAKE_EXE_LINKER_FLAGS", "-stdlib=libc++".to_owned()),
        ("CMAKE_INSTALL_PREFIX", prefix.display().to_string()),
        // GNUInstallDirs may pick lib/<multiarch>; the artifact path below assumes lib
        ("CMAKE_INSTALL_LIBDIR", "lib".to_owned()),
    ];
    let mut configure = Command::new("cmake");
    configure
        .args(["-G", "Ninja", "-S"])
        .arg(src)
        .arg("-B")
        .arg(&build);
    for (k, v) in cache.iter().map(|(k, v)| (*k, v.as_str())).chain(
        recipe
            .cache
            .iter()
            .chain(recipe.shared_defines)
            .map(|(k, v)| (*k, *v)),
    ) {
        configure.arg(format!("-D{k}={v}"));
    }
    let mut steps = [configure, Command::new("cmake"), Command::new("cmake")];
    steps[1].arg("--build").arg(&build);
    steps[2].arg("--install").arg(&build);
    let file = std::fs::File::create(&log)?;
    for step in &mut steps {
        let status = step
            .stdout(file.try_clone()?)
            .stderr(file.try_clone()?)
            .status()
            .context("running cmake (deps.toml row `cmake`)")?;
        ensure!(
            status.success(),
            "upstream {}: cmake failed, log {}",
            recipe.name,
            log.display()
        );
    }
    let mut cflags = vec![format!("-I{}", prefix.join("include").display())];
    cflags.extend(
        recipe
            .shared_defines
            .iter()
            .map(|(k, v)| format!("-D{k}={v}")),
    );
    let mut provided = BTreeMap::new();
    for module in recipe.modules {
        let lib = prefix.join("lib").join(format!("{module}.so"));
        ensure!(
            lib.is_file(),
            "upstream {}: install has no {} for module {module}",
            recipe.name,
            lib.display()
        );
        provided.insert(
            (*module).to_owned(),
            Provided {
                lib: lib.display().to_string(),
                cflags: cflags.clone(),
            },
        );
    }
    Ok(provided)
}
