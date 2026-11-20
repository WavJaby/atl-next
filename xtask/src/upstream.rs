// SPDX-License-Identifier: MIT
//! Upstreams built by their own build system for `providers.toml` rows `upstream:<name>`: `xtask native` configures, builds and installs each into `<out>/upstream/<name>/prefix` before the converter runs, then hands the converter one `Provided` entry per module (artifact, include dirs, defines; bp2ninja `-provided`).
//! CMake only: the one upstream built this way is BoringSSL (CLAUDE.md). `cmake` is shared with the LLVM runtimes for the bionic targets (`runtimes`), CMake's other allowed use. A second build system becomes a `Recipe` field when its first upstream arrives.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, ensure};

use crate::providers::Provided;

/// How xtask builds one upstream; keyed by its `sources.toml` name.
pub struct Recipe {
    pub name: &'static str,
    /// Android.bp modules the build provides as C artifacts, each with its file relative to `<out>/upstream/<name>`: `prefix/...` from the install, `build/...` a build output no install rule copies (checked after the build).
    pub modules: &'static [(&'static str, &'static str)],
    /// Android.bp Rust modules whose crate source the upstream provides (bp2ninja `ProvidedCrate`): the tree module stays, its source becomes the upstream crate's. Non-empty -> configured with `RUST_BINDINGS`.
    pub crates: &'static [RecipeCrate],
    /// CMake cache entries beyond the toolchain ones every recipe gets.
    cache: &'static [(&'static str, &'static str)],
    /// Cache entries that are also preprocessor defines of the installed headers: dependents compile with `-D<k>=<v>`, so library and users cannot disagree.
    shared_defines: &'static [(&'static str, &'static str)],
}

/// One provided Rust crate of a recipe.
pub struct RecipeCrate {
    pub module: &'static str,
    /// Crate dir in the upstream tree: `src/lib.rs` is the root, `Cargo.toml` `package.edition` the edition.
    dir: &'static str,
    /// The crate reads CMake's bindgen output (`<build>/<dir>/wrapper_<rust triple>.rs`, BoringSSL rust/bssl-sys/CMakeLists.txt) through its static-build hook: `--cfg bindgen_rs_file` + env `BINDGEN_RS_FILE` (bssl-sys src/lib.rs); cargo's build.rs route is not taken.
    bindings: bool,
}

/// What the converter compiles in place of a tree Rust module's sources (bp2ninja provided.go `ProvidedCrate`).
#[derive(Debug, Clone, serde::Serialize)]
pub struct ProvidedCrate {
    pub root: PathBuf,
    pub edition: String,
    pub cfgs: Vec<String>,
    /// `NAME=VALUE` rustc environment.
    pub env: Vec<String>,
}

/// Symbol prefix of our BoringSSL: its C symbols become `ATL_<name>`, its C++ ones move to `bssl::ATL`. App processes share one flat ELF symbol namespace with desktop libraries that may load a system OpenSSL; unprefixed, the first loaded `SHA256_Init` & co. would win for both.
const BORINGSSL_SYMBOL_PREFIX: &str = "ATL";

pub const RECIPES: &[Recipe] = &[Recipe {
    name: "boringssl",
    // libcrypto_static: AOSP's non-FIPS static copy (libcore's JNI libraries, dex2oat: bignums, digests); here the one shared libcrypto, so a process holds one BoringSSL
    modules: &[
        ("libcrypto", "prefix/lib/libcrypto.so"),
        ("libcrypto_static", "prefix/lib/libcrypto.so"),
        ("libssl", "prefix/lib/libssl.so"),
        // bindgen --wrap-static-fns output for bssl-sys (AOSP: the same role, generated from its own headers)
        (
            "libbssl_sys_raw_staticfns",
            "build/rust/bssl-sys/librust_wrapper.a",
        ),
    ],
    // only bssl-sys: AOSP's bssl-crypto (ISC, external/boringssl/src/rust) builds as-is over this bssl-sys
    crates: &[RecipeCrate {
        module: "libbssl_sys",
        dir: "rust/bssl-sys",
        bindings: true,
    }],
    // shared: dependents link libcrypto.so / libssl.so as AOSP's device build does; tests are not ours to build
    cache: &[("BUILD_SHARED_LIBS", "ON"), ("BUILD_TESTING", "OFF")],
    shared_defines: &[("BORINGSSL_PREFIX", BORINGSSL_SYMBOL_PREFIX)],
}];

pub fn recipe(name: &str) -> Option<&'static Recipe> {
    RECIPES.iter().find(|r| r.name == name)
}

/// What the converter derives for its host modules (`bp2ninja -upstream-toolchain`), not restated here: the compilers, the libc++ headers and, for a cross host, the target flags (`--target`, `--sysroot`, `-march`, cflags) from arches.toml.
#[derive(serde::Deserialize)]
pub struct Flags {
    pub cc: String,
    pub cxx: String,
    pub libcxx: PathBuf,
    pub compile: Vec<String>,
    pub link: Vec<String>,
}

/// The C/C++ toolchain of the converter's host modules; an upstream is compiled with the same one.
pub struct Toolchain<'a> {
    pub flags: &'a Flags,
    /// Cross host: its arch name, CMake's `CMAKE_SYSTEM_PROCESSOR` (with `CMAKE_SYSTEM_NAME` it puts CMake in cross mode: no try_run, no build-machine search paths).
    pub cross_processor: Option<&'a str>,
    /// rustc target of the machine's crates: `RUST_BINDINGS` (bindgen's `--target`, the wrapper file's name).
    pub rust_triple: &'a str,
    /// The `clang` row's libclang dir: bindgen's `LIBCLANG_PATH`, as the converter's rust_bindgen gets it.
    pub libclang: &'a Path,
}

/// The link flags of the default libc++ selection (bp2ninja stl.go `stlLinkFlags`).
const STL_LINK_FLAGS: &str = "-nostdlib++ -lc++";

/// The CMake cache entries of `tc`: compilers, target flags on every language (asm sources too) and on links, the libc++ selection.
fn toolchain_cache(tc: &Toolchain) -> Vec<(&'static str, String)> {
    let target = tc.flags.compile.join(" ");
    let link = format!("{} {STL_LINK_FLAGS}", tc.flags.link.join(" "));
    let mut cache = vec![
        ("CMAKE_C_COMPILER", tc.flags.cc.clone()),
        ("CMAKE_CXX_COMPILER", tc.flags.cxx.clone()),
        ("CMAKE_C_FLAGS", target.clone()),
        ("CMAKE_ASM_FLAGS", target.clone()),
        // one C++ runtime per process, selected as the converter selects it for a module with the default stl (bp2ninja stl.go, after Soong's cc/stl.go non-Bionic branch)
        (
            "CMAKE_CXX_FLAGS",
            format!(
                "{target} -nostdinc++ -isystem {}",
                tc.flags.libcxx.display()
            )
            .trim_start()
            .to_owned(),
        ),
        ("CMAKE_SHARED_LINKER_FLAGS", link.clone()),
        ("CMAKE_EXE_LINKER_FLAGS", link),
    ];
    if let Some(processor) = tc.cross_processor {
        cache.push(("CMAKE_SYSTEM_NAME", "Linux".to_owned()));
        cache.push(("CMAKE_SYSTEM_PROCESSOR", processor.to_owned()));
    }
    cache
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
    let mut cache = vec![("CMAKE_BUILD_TYPE", "Release".to_owned())];
    cache.extend(toolchain_cache(tc));
    cache.extend([
        ("CMAKE_INSTALL_PREFIX", prefix.display().to_string()),
        // GNUInstallDirs may pick lib/<multiarch>; the artifact path below assumes lib
        ("CMAKE_INSTALL_LIBDIR", "lib".to_owned()),
    ]);
    let mut env = Vec::new();
    // bindgen parses the headers as the C compiler compiles them: upstream's CMake gives it only -I and --target, so a cross host's
    // --sysroot comes through bindgen's own BINDGEN_EXTRA_CLANG_ARGS (else it reads the build machine's glibc headers: other layouts)
    let bindgen_args = tc.flags.compile.join(" ");
    if !recipe.crates.is_empty() {
        cache.push(("RUST_BINDINGS", tc.rust_triple.to_owned()));
        env.push(("LIBCLANG_PATH", tc.libclang.as_os_str()));
        env.push((
            "BINDGEN_EXTRA_CLANG_ARGS",
            std::ffi::OsStr::new(&bindgen_args),
        ));
    }
    let cache: Vec<(&str, &str)> = cache
        .iter()
        .map(|(k, v)| (*k, v.as_str()))
        .chain(recipe.cache.iter().chain(recipe.shared_defines).copied())
        .collect();
    cmake(src, &build, &cache, &log, &[], true, &env)
        .with_context(|| format!("upstream {}", recipe.name))?;
    let defines: Vec<String> = recipe
        .shared_defines
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect();
    let mut provided = BTreeMap::new();
    for c in recipe.crates {
        let edition = crate_edition(&src.join(c.dir).join("Cargo.toml"))?;
        let (mut cfgs, mut env) = (Vec::new(), Vec::new());
        if c.bindings {
            let rs = build
                .join(c.dir)
                .join(format!("wrapper_{}.rs", tc.rust_triple));
            ensure!(
                rs.is_file(),
                "upstream {}: build has no {} for module {}",
                recipe.name,
                rs.display(),
                c.module
            );
            cfgs.push("bindgen_rs_file".to_owned());
            env.push(format!("BINDGEN_RS_FILE={}", rs.display()));
        }
        provided.insert(
            c.module.to_owned(),
            Provided {
                krate: Some(ProvidedCrate {
                    root: src.join(c.dir).join("src/lib.rs"),
                    edition,
                    cfgs,
                    env,
                }),
                ..Provided::default()
            },
        );
    }
    for (module, file) in recipe.modules {
        let lib = dir.join(file);
        ensure!(
            lib.is_file(),
            "upstream {}: build has no {} for module {module}",
            recipe.name,
            lib.display()
        );
        provided.insert(
            (*module).to_owned(),
            Provided {
                lib,
                include: vec![prefix.join("include")],
                defines: defines.clone(),
                ..Provided::default()
            },
        );
    }
    Ok(provided)
}

/// `package.edition` of a crate's Cargo.toml (cargo's default 2015 when absent).
fn crate_edition(manifest: &Path) -> Result<String> {
    let text = std::fs::read_to_string(manifest)
        .with_context(|| format!("reading {}", manifest.display()))?;
    let doc: toml::Table =
        toml::from_str(&text).with_context(|| format!("parsing {}", manifest.display()))?;
    Ok(doc
        .get("package")
        .and_then(|p| p.get("edition"))
        .and_then(toml::Value::as_str)
        .unwrap_or("2015")
        .to_owned())
}

/// Compiler entries whose change makes cmake delete its cache and re-run configure without the other `-D`s: CMAKE_INSTALL_PREFIX then falls back to the project's default
/// (BoringSSL's `${CMAKE_SOURCE_DIR}/install`, its CMakeLists.txt), and install writes into the source tree.
const COMPILER_ENTRIES: [&str; 2] = ["CMAKE_C_COMPILER", "CMAKE_CXX_COMPILER"];

/// Deletes `build`'s CMakeCache.txt + CMakeFiles when cmake cannot reuse it with `-D` entries `wanted`: the cache names another source dir (`CMAKE_HOME_DIRECTORY`, e.g. a removed task copy: cmake refuses it),
/// or another compiler (COMPILER_ENTRIES: cmake's own reset would drop the other entries). Returns why when it deleted.
fn drop_stale_cache(src: &Path, build: &Path, wanted: &[(&str, &str)]) -> Result<Option<String>> {
    let cache = build.join("CMakeCache.txt");
    let Ok(text) = std::fs::read_to_string(&cache) else {
        return Ok(None);
    };
    let entry = |key: &str| {
        text.lines().find_map(|l| {
            let (k, v) = l.split_once('=')?;
            (k.split(':').next() == Some(key)).then_some(v)
        })
    };
    let same_file = |a: &Path, b: &Path| match (std::fs::canonicalize(a), std::fs::canonicalize(b))
    {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    };
    let mut why = None;
    if let Some(old) = entry("CMAKE_HOME_DIRECTORY")
        && !same_file(Path::new(old), src)
    {
        why = Some(format!("generated from {old}, not {}", src.display()));
    }
    // configured before the canonical build path (out::CANONICAL), e.g. in a base copied from such a build: cmake accepts the bound dir as the same build dir (CMAKE_CACHEFILE_DIR is rewritten),
    // but keeps derived entries naming that build dir's real path (compiler-rt's COMPILER_RT_OUTPUT_DIR, LLVM_THINLTO_CACHE_PATH), so the copy would write into the original
    if why.is_none()
        && let Some(other) = crate::out::foreign_build_dir(&text)
    {
        why = Some(format!("entries name {other}, not {}", build.display()));
    }
    for (key, value) in wanted.iter().filter(|(k, _)| COMPILER_ENTRIES.contains(k)) {
        // both sides through the same PATH lookup: a fresh configure records `:STRING=/usr/bin/clang-19`, a later configure given the same -D rewrites it `:UNINITIALIZED=clang-19`;
        // comparing the bare name as a path dropped the cache every second run -> full reconfigure + rebuild + relink of every dependant (1,301 ninja edges). Do not compare unresolved values.
        let resolve = |v: &str| crate::arches::on_path(v).unwrap_or_else(|| PathBuf::from(v));
        if let Some(old) = entry(key)
            && why.is_none()
            && !same_file(&resolve(old), &resolve(value))
        {
            why = Some(format!("{key} was {old}, now {value}"));
        }
    }
    let Some(why) = why else {
        return Ok(None);
    };
    std::fs::remove_file(&cache).context("removing a stale CMakeCache.txt")?;
    match std::fs::remove_dir_all(build.join("CMakeFiles")) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            return Err(e).context("removing a foreign CMakeFiles");
        }
        _ => {}
    }
    Ok(Some(why))
}

/// Configures (Ninja generator, `cache` as `-D` entries), builds `all`, then `targets` (what `all` leaves out, e.g. tools under `LLVM_BUILD_TOOLS=OFF`) and, with `install`, installs one CMake project; `env` on every step; all output to `log`. Incremental: a rerun reuses `build`.
pub fn cmake(
    src: &Path,
    build: &Path,
    cache: &[(&str, &str)],
    log: &Path,
    targets: &[&str],
    install: bool,
    env: &[(&str, &std::ffi::OsStr)],
) -> Result<()> {
    std::fs::create_dir_all(build)?;
    if let Some(why) = drop_stale_cache(src, build, cache)? {
        println!(
            "cmake: {}: cache {why}: deleted, reconfiguring",
            build.display()
        );
    }
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
    if !targets.is_empty() {
        let mut t = Command::new("cmake");
        t.arg("--build").arg(build).arg("--target").args(targets);
        steps.push(t);
    }
    if install {
        let mut i = Command::new("cmake");
        i.arg("--install").arg(build);
        steps.push(i);
    }
    let file = std::fs::File::create(log)?;
    for step in &mut steps {
        let status = step
            .envs(env.iter().copied())
            .stdout(file.try_clone()?)
            .stderr(file.try_clone()?)
            .status()
            .context("running cmake (deps.toml row `cmake`)")?;
        ensure!(status.success(), "cmake failed, log {}", log.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cache_from_another_source_dir_is_dropped() {
        let dir = std::env::temp_dir().join(format!("xtask-cmake-cache-{}", std::process::id()));
        let (src, gone, build) = (
            dir.join("src"),
            dir.join("removed-task-copy"),
            dir.join("build"),
        );
        std::fs::create_dir_all(build.join("CMakeFiles")).expect("test dir");
        std::fs::create_dir_all(&src).expect("test dir");
        let cache = |home: &Path| {
            std::fs::write(
                build.join("CMakeCache.txt"),
                format!("CMAKE_HOME_DIRECTORY:INTERNAL={}\n", home.display()),
            )
            .expect("test cache")
        };
        cache(&src);
        assert_eq!(drop_stale_cache(&src, &build, &[]).expect("same"), None);
        assert!(build.join("CMakeCache.txt").is_file());
        cache(&gone);
        let why = drop_stale_cache(&src, &build, &[]).expect("foreign");
        assert!(
            why.expect("dropped")
                .contains(gone.to_str().expect("utf-8"))
        );
        assert!(!build.join("CMakeCache.txt").exists() && !build.join("CMakeFiles").exists());
        std::fs::remove_dir_all(&dir).expect("test dir cleanup");
    }

    /// A compiler change drops the cache before configure: cmake's own reset would re-run without CMAKE_INSTALL_PREFIX and install into the source tree.
    #[test]
    fn a_cache_from_another_compiler_is_dropped() {
        let dir = std::env::temp_dir().join(format!("xtask-cmake-cc-{}", std::process::id()));
        let (src, build) = (dir.join("src"), dir.join("build"));
        std::fs::create_dir_all(build.join("CMakeFiles")).expect("test dir");
        std::fs::create_dir_all(&src).expect("test dir");
        // two stand-in compilers by absolute path: what the cache records and what is given
        let (old_cc, new_cc) = (dir.join("cc-old"), dir.join("cc-new"));
        std::fs::write(&old_cc, "").expect("test file");
        std::fs::write(&new_cc, "").expect("test file");
        let write = || {
            std::fs::write(
                build.join("CMakeCache.txt"),
                format!(
                    "CMAKE_HOME_DIRECTORY:INTERNAL={}\nCMAKE_C_COMPILER:STRING={}\n",
                    src.display(),
                    old_cc.display()
                ),
            )
            .expect("test cache")
        };
        let (old_s, new_s) = (
            old_cc.to_str().expect("utf-8"),
            new_cc.to_str().expect("utf-8"),
        );
        write();
        assert_eq!(
            drop_stale_cache(&src, &build, &[("CMAKE_C_COMPILER", old_s)]).expect("same"),
            None
        );
        // a non-compiler entry never drops it: -D sets those on the next configure
        assert_eq!(
            drop_stale_cache(&src, &build, &[("CMAKE_INSTALL_PREFIX", new_s)]).expect("other"),
            None
        );
        // cmake's rewrite on a reconfigure: `:UNINITIALIZED=<bare name>`, the same compiler as its PATH lookup; `sh` stands in for a compiler name found on PATH
        let sh = crate::arches::on_path("sh").expect("sh on PATH");
        std::fs::write(
            build.join("CMakeCache.txt"),
            format!(
                "CMAKE_HOME_DIRECTORY:INTERNAL={}\nCMAKE_C_COMPILER:UNINITIALIZED=sh\n",
                src.display()
            ),
        )
        .expect("test cache");
        assert_eq!(
            drop_stale_cache(
                &src,
                &build,
                &[("CMAKE_C_COMPILER", sh.to_str().expect("utf-8"))]
            )
            .expect("bare name"),
            None
        );
        write();
        let why = drop_stale_cache(&src, &build, &[("CMAKE_C_COMPILER", new_s)]).expect("changed");
        assert!(why.expect("dropped").contains("CMAKE_C_COMPILER"));
        assert!(!build.join("CMakeCache.txt").exists() && !build.join("CMakeFiles").exists());
        std::fs::remove_dir_all(&dir).expect("test dir cleanup");
    }
}
