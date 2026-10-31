// SPDX-License-Identifier: MIT
//! System libraries for `providers.toml` rows `system:<deps.toml row>`: `xtask native` resolves the row's pkg-config module into the `Provided` entry crate and upstream rows produce (artifact, include dirs, defines; bp2ninja `-provided`), before the converter runs.
//! Nothing is built: the host's library is linked as it is installed (deps.toml names the package).

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail, ensure};

use crate::providers::Provided;

/// Resolves pkg-config module `pkg` (deps.toml row `row`): its one `-l` library, found in its `-L` dirs then its `libdir`, plus its `-I` dirs and `-D` defines.
pub fn resolve(row: &str, pkg: &str) -> Result<Provided> {
    let query = |flag: &str| -> Result<String> {
        let out = Command::new("pkg-config")
            .args([flag, pkg])
            .output()
            .with_context(|| {
                format!("running pkg-config (deps.toml row `pkgconf`) for row `{row}`")
            })?;
        ensure!(
            out.status.success(),
            "pkg-config {flag} {pkg} failed (deps.toml row `{row}`: is its package installed? `cargo xtask deps --check`): {}",
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
    };
    provided(&flags, Path::is_file)
        .with_context(|| format!("pkg-config module {pkg} (deps.toml row `{row}`)"))
}

/// pkg-config's answers for one module, as printed.
struct Flags<'a> {
    libs_l: &'a str,
    libs_dirs: &'a str,
    libdir: &'a str,
    cflags_i: &'a str,
    cflags_other: &'a str,
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
    let dirs = flags
        .libs_dirs
        .split_whitespace()
        .map(|d| {
            d.strip_prefix("-L")
                .with_context(|| format!("`{d}` is not -L<dir>"))
        })
        .chain(
            std::iter::once(Ok(flags.libdir.trim()))
                .filter(|d| d.as_ref().is_ok_and(|d| !d.is_empty())),
        )
        .collect::<Result<Vec<_>>>()?;
    // the shared object dependents link, as `-l<name>` would find it (the linker prefers .so over .a)
    let file = format!("lib{name}.so");
    let lib = dirs
        .iter()
        .map(|d| Path::new(d).join(&file))
        .find(|p| is_file(p))
        .with_context(|| format!("no {file} in {}", dirs.join(", ")))?;
    let include = flags
        .cflags_i
        .split_whitespace()
        .map(|i| {
            i.strip_prefix("-I")
                .map(PathBuf::from)
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
    fn pkg_config_failure_names_the_row() {
        let err = format!(
            "{:#}",
            resolve("ghost", "atl-no-such-pkgconfig-module").unwrap_err()
        );
        assert!(err.contains("deps.toml row `ghost`"), "{err}");
    }
}
