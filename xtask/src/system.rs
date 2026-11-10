// SPDX-License-Identifier: MIT
//! System libraries for `providers.toml` rows `system:<deps.toml row>`: `xtask native` resolves the row's pkg-config module into the `Provided` entry crate and upstream rows produce (artifact, include dirs, defines; bp2ninja `-provided`), before the converter runs.
//! Nothing is built: the library is linked as installed, from the build machine or, on a cross host, from the target sysroot (deps.toml names the package).

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail, ensure};

use crate::providers::Provided;

/// A cross host's target: its sysroot (`root` of deps.toml row `name`, a `sysroot_debs` row) and GNU triple, whose Debian multiarch dir `usr/lib/<triple>` holds the target's .pc files.
#[derive(Clone, Copy)]
pub struct Sysroot<'a> {
    pub name: &'a str,
    pub root: &'a Path,
    pub triple: &'a str,
}

/// Resolves pkg-config module `pkg` (deps.toml row `row`): its one `-l` library, found in its `-L` dirs then its `libdir`, plus its `-I` dirs and `-D` defines.
/// With `target`, pkg-config reads the target's .pc files only (PKG_CONFIG_LIBDIR; the build machine's PKG_CONFIG_PATH is cleared) and every dir it answers is kept under the sysroot (`in_target`).
pub fn resolve(row: &str, pkg: &str, target: Option<Sysroot>) -> Result<Provided> {
    let query = |flag: &str| -> Result<String> {
        let mut cmd = Command::new("pkg-config");
        if let Some(t) = target {
            cmd.env("PKG_CONFIG_SYSROOT_DIR", t.root)
                .env(
                    "PKG_CONFIG_LIBDIR",
                    format!(
                        "{0}/usr/lib/{1}/pkgconfig:{0}/usr/share/pkgconfig",
                        t.root.display(),
                        t.triple
                    ),
                )
                .env_remove("PKG_CONFIG_PATH");
        }
        let out = cmd.args([flag, pkg]).output().with_context(|| {
            format!("running pkg-config (deps.toml row `pkgconf`) for row `{row}`")
        })?;
        let hint = match target {
            Some(t) => format!(
                "is its package in sysroot {0}? `cargo xtask deps --fetch {0} --execute`",
                t.name
            ),
            None => "is its package installed? `cargo xtask deps --check`".to_owned(),
        };
        ensure!(
            out.status.success(),
            "pkg-config {flag} {pkg} failed (deps.toml row `{row}`: {hint}): {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
        Ok(String::from_utf8(out.stdout)?)
    };
    let flags = Flags {
        libs_l: &query("--libs-only-l")?,
        libs_dirs: &query("--libs-only-L")?,
        libdir: &query("--variable=libdir")?,
        cflags_i: &query("--cflags-only-I")?,
        cflags_other: &query("--cflags-only-other")?,
        target,
    };
    provided(&flags, Path::is_file)
        .with_context(|| format!("pkg-config module {pkg} (deps.toml row `{row}`)"))
}

/// pkg-config's answers for one module, as printed, and the cross target they are under, if any.
struct Flags<'a> {
    libs_l: &'a str,
    libs_dirs: &'a str,
    libdir: &'a str,
    cflags_i: &'a str,
    cflags_other: &'a str,
    target: Option<Sysroot<'a>>,
}

/// `dir` on a cross host, as a path under the sysroot: pkg-config already prefixes its answers, any other (a build-machine dir in a .pc file) is joined under it, so a host dir names only a path that does not exist in the target.
fn in_target(target: Option<Sysroot>, dir: &str) -> PathBuf {
    match target {
        Some(t) if !Path::new(dir).starts_with(t.root) => t.root.join(dir.trim_start_matches('/')),
        _ => PathBuf::from(dir),
    }
}

/// The `Provided` entry `flags` describe; `is_file` decides which search dir holds the library.
fn provided(flags: &Flags, is_file: impl Fn(&Path) -> bool) -> Result<Provided> {
    // one Provided = one artifact: a module whose .pc links several libraries would need a provider per library
    let libs: Vec<&str> = flags.libs_l.split_whitespace().collect();
    let [lib] = libs[..] else {
        bail!(
            "--libs-only-l gives {} libraries ({}), not one",
            libs.len(),
            flags.libs_l.trim()
        )
    };
    let name = lib
        .strip_prefix("-l")
        .with_context(|| format!("`{lib}` is not -l<name>"))?;
    // -L dirs first, then libdir: the order the linker searches them in
    let mut dirs = flags
        .libs_dirs
        .split_whitespace()
        .map(|d| {
            d.strip_prefix("-L")
                .map(|d| in_target(flags.target, d))
                .with_context(|| format!("`{d}` is not -L<dir>"))
        })
        .collect::<Result<Vec<_>>>()?;
    let libdir = flags.libdir.trim();
    if !libdir.is_empty() {
        dirs.push(in_target(flags.target, libdir));
    }
    // the shared object dependents link, as `-l<name>` would find it (the linker prefers .so over .a)
    let file = format!("lib{name}.so");
    let lib = dirs
        .iter()
        .map(|d| d.join(&file))
        .find(|p| is_file(p))
        .with_context(|| {
            let shown: Vec<_> = dirs.iter().map(|d| d.display().to_string()).collect();
            format!("no {file} in {}", shown.join(", "))
        })?;
    let include = flags
        .cflags_i
        .split_whitespace()
        .map(|i| {
            i.strip_prefix("-I")
                .map(|d| in_target(flags.target, d))
                .with_context(|| format!("`{i}` is not -I<dir>"))
        })
        .collect::<Result<_>>()?;
    // Provided carries only defines: any other compile flag (-pthread, -f…) would have nowhere to go
    let defines = flags
        .cflags_other
        .split_whitespace()
        .map(|f| {
            f.strip_prefix("-D")
                .map(str::to_owned)
                .with_context(|| format!("cflag `{f}` is not -D<define>"))
        })
        .collect::<Result<_>>()?;
    Ok(Provided {
        lib,
        include,
        defines,
        ..Provided::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flags<'a>(
        libs_l: &'a str,
        libs_dirs: &'a str,
        cflags_i: &'a str,
        cflags_other: &'a str,
    ) -> Flags<'a> {
        Flags {
            libs_l,
            libs_dirs,
            libdir: "/usr/lib/x86_64-linux-gnu\n",
            cflags_i,
            cflags_other,
            target: None,
        }
    }

    /// Only these files exist.
    fn files(present: &'static [&'static str]) -> impl Fn(&Path) -> bool {
        move |p| present.iter().any(|f| Path::new(f) == p)
    }

    #[test]
    fn resolves_lib_headers_defines() {
        // pkg-config omits system -L/-I dirs: the library is found through libdir
        let p = provided(
            &flags("-lEGL \n", "\n", "\n", "\n"),
            files(&["/usr/lib/x86_64-linux-gnu/libEGL.so"]),
        )
        .unwrap();
        assert_eq!(p.lib, Path::new("/usr/lib/x86_64-linux-gnu/libEGL.so"));
        assert!(p.include.is_empty() && p.defines.is_empty());
        // -L dirs are searched before libdir, as the linker searches them
        let p = provided(
            &flags(
                "-lfoo",
                "-L/opt/foo/lib",
                "-I/opt/foo/include -I/opt/foo/include/x",
                "-DFOO=1 -DBAR",
            ),
            files(&[
                "/opt/foo/lib/libfoo.so",
                "/usr/lib/x86_64-linux-gnu/libfoo.so",
            ]),
        )
        .unwrap();
        assert_eq!(p.lib, Path::new("/opt/foo/lib/libfoo.so"));
        assert_eq!(
            p.include,
            [
                PathBuf::from("/opt/foo/include"),
                PathBuf::from("/opt/foo/include/x")
            ]
        );
        assert_eq!(p.defines, ["FOO=1", "BAR"]);
    }

    #[test]
    fn rejects_what_one_provided_entry_cannot_hold() {
        let err = |f: Flags| {
            format!(
                "{:#}",
                provided(&f, files(&["/usr/lib/x86_64-linux-gnu/libEGL.so"])).unwrap_err()
            )
        };
        assert!(err(flags("-lEGL -lGLdispatch", "", "", "")).contains("2 libraries"));
        assert!(err(flags("", "", "", "")).contains("0 libraries"));
        assert!(
            err(flags("-lnope", "", "", "")).contains("no libnope.so in /usr/lib/x86_64-linux-gnu")
        );
        assert!(err(flags("-lEGL", "", "", "-pthread")).contains("cflag `-pthread` is not -D"));
    }

    #[test]
    fn cross_answers_stay_under_the_sysroot() {
        const ROOT: &str = "/cache/sysroot/riscv64-sysroot/root";
        let target = Sysroot {
            name: "riscv64-sysroot",
            root: Path::new(ROOT),
            triple: "riscv64-linux-gnu",
        };
        // pkg-config's prefixed answers pass unchanged; an unprefixed one is joined under the root
        let prefixed = format!("-L{ROOT}/usr/lib/riscv64-linux-gnu");
        let mut f = flags("-lelf", &prefixed, "-I/usr/include", "");
        f.target = Some(target);
        let p = provided(
            &f,
            files(&["/cache/sysroot/riscv64-sysroot/root/usr/lib/riscv64-linux-gnu/libelf.so"]),
        )
        .unwrap();
        assert_eq!(
            p.lib,
            Path::new("/cache/sysroot/riscv64-sysroot/root/usr/lib/riscv64-linux-gnu/libelf.so")
        );
        assert_eq!(p.include, [PathBuf::from(format!("{ROOT}/usr/include"))]);
        // the default libdir is the build machine's: under the root it names a dir that holds nothing
        let mut h = flags("-lelf", "", "", "");
        h.target = Some(target);
        let err = format!(
            "{:#}",
            provided(&h, files(&["/usr/lib/x86_64-linux-gnu/libelf.so"])).unwrap_err()
        );
        assert!(
            err.contains(&format!("no libelf.so in {ROOT}/usr/lib/x86_64-linux-gnu")),
            "{err}"
        );
    }

    #[test]
    fn cross_failure_names_the_sysroot() {
        let target = Sysroot {
            name: "riscv64-sysroot",
            root: Path::new("/atl-no-such-sysroot/root"),
            triple: "riscv64-linux-gnu",
        };
        let err = format!(
            "{:#}",
            resolve("ghost", "atl-no-such-pkgconfig-module", Some(target)).unwrap_err()
        );
        assert!(
            err.contains("deps.toml row `ghost`") && err.contains("sysroot riscv64-sysroot"),
            "{err}"
        );
    }

    #[test]
    fn pkg_config_failure_names_the_row() {
        let err = format!(
            "{:#}",
            resolve("ghost", "atl-no-such-pkgconfig-module", None).unwrap_err()
        );
        assert!(err.contains("deps.toml row `ghost`"), "{err}");
    }
}
