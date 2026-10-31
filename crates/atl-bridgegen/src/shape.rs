// SPDX-License-Identifier: MIT
//! Backend: include wrappers giving the host libc's headers bionic's header shape -- what `#include <h>`
//! brings in besides `h`'s own declarations. AOSP code built against glibc relies on bionic's includes
//! (`<stdint.h>` -> `size_t`, `<unistd.h>` -> `ioctl`); a wrapper `h` on the include path does
//! `#include_next <h>`, then includes what bionic's `h` reaches and the host's does not.
//!
//! Derived from two sets of `front::probe_includes` probes, never listed by hand:
//! - bionic's: each of its public headers, parsed as bionic sees it; every file it reaches.
//! - the host's: the same names plus every name bionic's own files include, as the host sees them.
//!
//! What bionic's header includes is read from bionic's own files only: the header and the private files
//! (`bits/`, bionic-only headers) it reaches, stopping at includable names (angled, outside `bits/` and
//! the kernel uapi, which the host has). Direct includes suffice: an includable header's own wrapper
//! gives its shape, and glibc's internal `#include <x>` reaches the wrappers too. Includable names the
//! host's version already reaches are dropped.
//!
//! A private file's names the wrapped header still lacks but some host header declares are a gap. A
//! home (`homes`: host header for a private file, checked needed and sufficient) closes it by include;
//! the rest are reported, since a home brings a whole host header for a few names. A name no host header
//! declares is missing API, not shape.
//!
//! Macros: bionic macros the host lacks (`__unused`, `IS_ELF`) are defined, as bionic writes them, in the
//! wrapper of every public header that brings them in -- defining them itself or through its private
//! files (`<elf.h>` -> `bits/elf_common.h`); host headers that use such a name as an identifier are
//! wrapped to hide it (bionic renames those kernel fields).
//!
//! Known excess: a glibc header including `x` where bionic's does not (`sys/types.h` -> `sys/select.h`)
//! now also gets the wrapper's additions to `x`.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};

use crate::front::{Probe, Reached};

/// `bits/` holds both libcs' private headers ("never include directly"): never included by name.
const PRIVATE_DIR: &str = "bits/";

/// A libc header no code includes by name (`bits/…`).
pub fn is_private(name: &str) -> bool {
    name.starts_with(PRIVATE_DIR)
}

/// What the wrappers are derived from.
pub struct Inputs<'a> {
    /// bionic's public headers, as bionic sees them.
    pub bionic: &'a BTreeMap<String, Probe>,
    /// The host's view of every name in `host_names`.
    pub host: &'a BTreeMap<String, Probe>,
    /// bionic's public include directory: files under it are bionic's own.
    pub bionic_include: &'a Path,
    /// bionic's kernel uapi directories: files under them are kernel headers, not libc. bionic spells
    /// libc definitions with them, glibc writes its own; the distro's next to glibc's redefine them
    /// (linux/time.h `struct itimerspec`), so they are neither included nor compared.
    pub uapi: &'a [PathBuf],
    /// Private bionic file (include name) -> host header declaring what it brings in. Every row must
    /// be needed and must declare every needed name.
    pub homes: &'a [(&'a str, &'a str)],
    /// bionic macros the host lacks, defined in the wrapper of each public bionic header bringing them in.
    pub macros: &'a [&'a str],
    /// Host headers outside the probes the clash scan reads (the host's kernel uapi), by include name.
    pub scan: &'a BTreeMap<String, PathBuf>,
}

/// One generated header.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Wrapper {
    /// Macros hidden around the host header (it uses them as identifiers).
    pub hidden: BTreeSet<String>,
    /// Macro definitions as bionic writes them (`NAME body`).
    pub defines: Vec<String>,
    /// Headers bionic's version reaches and the host's does not.
    pub includes: Vec<String>,
}

/// Every include name the host is probed for: bionic's public headers, the names bionic's own files
/// include that could be includable, the homes.
pub fn host_names(
    bionic: &BTreeMap<String, Probe>,
    bionic_include: &Path,
    uapi: &[PathBuf],
    homes: &[(&str, &str)],
) -> BTreeSet<String> {
    let mut out: BTreeSet<String> = bionic.keys().cloned().collect();
    out.extend(homes.iter().map(|(_, h)| (*h).to_owned()));
    for probe in bionic.values() {
        for (file, reached) in &probe.files {
            if !file.starts_with(bionic_include) {
                continue;
            }
            for inc in &reached.includes {
                if inc.angled && !is_private(&inc.name) && !under(&inc.file, uapi) {
                    out.insert(inc.name.clone());
                }
            }
        }
    }
    out
}

fn under(path: &Path, dirs: &[PathBuf]) -> bool {
    dirs.iter().any(|d| path.starts_with(d))
}

/// Names a probe makes visible.
fn visible(probe: &Probe) -> impl Iterator<Item = &String> {
    probe.files.values().flat_map(|r| r.names.iter())
}

/// Angled names a probe reaches, its own included.
fn reached_names<'p>(name: &'p str, probe: &'p Probe) -> BTreeSet<&'p str> {
    probe
        .files
        .values()
        .flat_map(|r| r.includes.iter())
        .filter(|i| i.angled)
        .map(|i| i.name.as_str())
        .chain(std::iter::once(name))
        .collect()
}

/// The derived shape.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Shape {
    /// By include name.
    pub wrappers: BTreeMap<String, Wrapper>,
    /// Private-file names no home covers: still different from bionic, reported, not failed on --
    /// a home includes a whole host header, so one is added where code needs it (`homes`).
    pub gaps: Vec<Gap>,
}

/// Names bionic's `header` brings in through private `file` that the wrapped host header lacks.
#[derive(Debug, PartialEq, Eq)]
pub struct Gap {
    pub header: String,
    pub file: String,
    pub names: Vec<String>,
    /// Host headers declaring all of them, fewest files reached first: home candidates.
    pub homes: Vec<String>,
}

/// What a bionic header includes itself, directly or through its private files.
#[derive(Default)]
struct Walk {
    includable: BTreeSet<String>,
    /// By the name they are included as.
    private: Vec<(String, PathBuf)>,
}

/// From `probe`'s header through bionic's own private files (`bits/`, headers the host lacks): the
/// includable names they include, and those private files. Not entered: includable headers -- their
/// own wrappers give their shape, and glibc's includes of them reach the wrappers too -- the kernel
/// uapi, the compiler's headers.
fn walk(inputs: &Inputs, probe: &Probe, host_has: &dyn Fn(&str) -> bool) -> Result<Walk> {
    let root = probe.file.as_ref().context("walk of an unusable probe")?;
    let mut todo = vec![root];
    let mut visited = BTreeSet::from([root]);
    let mut out = Walk::default();
    while let Some(f) = todo.pop() {
        let reached = probe
            .files
            .get(f)
            .with_context(|| format!("{} not in its probe", f.display()))?;
        for inc in &reached.includes {
            if inc.angled
                && !is_private(&inc.name)
                && !under(&inc.file, inputs.uapi)
                && host_has(&inc.name)
            {
                out.includable.insert(inc.name.clone());
            } else if inc.file.starts_with(inputs.bionic_include) && visited.insert(&inc.file) {
                out.private.push((inc.name.clone(), inc.file.clone()));
                todo.push(&inc.file);
            }
        }
    }
    Ok(out)
}

/// The shape. Fails on an unneeded or insufficient home, or a forwarded macro the host already has, no
/// public bionic header the host also has brings in, or bionic defines two ways.
pub fn derive(inputs: &Inputs) -> Result<Shape> {
    let host_has = |n: &str| inputs.host.get(n).is_some_and(Probe::usable);
    let host_all: BTreeSet<&String> = inputs.host.values().flat_map(visible).collect();
    let mut out: BTreeMap<String, Wrapper> = BTreeMap::new();
    let mut homes_used = BTreeSet::new();
    let mut gaps = Vec::new();

    for (name, bionic) in inputs.bionic {
        if !bionic.usable() || !host_has(name) {
            continue;
        }
        let host = &inputs.host[name];
        let host_reached = reached_names(name, host);
        let walk = walk(inputs, bionic, &host_has)?;
        let mut extras: BTreeSet<String> = walk
            .includable
            .into_iter()
            .filter(|n| n != name && !host_reached.contains(n.as_str()))
            .collect();
        let mut seen: BTreeSet<&String> = visible(host)
            .chain(extras.iter().flat_map(|e| visible(&inputs.host[e])))
            .collect();
        for (label, file) in walk.private {
            let needed: BTreeSet<&String> = bionic.files[&file]
                .names
                .iter()
                .filter(|n| !seen.contains(n) && host_all.contains(n))
                .collect();
            if needed.is_empty() {
                continue;
            }
            let Some(&(_, home)) = inputs.homes.iter().find(|(f, _)| *f == label) else {
                gaps.push(Gap {
                    header: name.clone(),
                    homes: home_candidates(inputs.host, &needed),
                    file: label,
                    names: needed.iter().map(|n| (*n).clone()).collect(),
                });
                continue;
            };
            homes_used.insert(label.clone());
            let home_probe = inputs
                .host
                .get(home)
                .filter(|p| p.usable())
                .with_context(|| format!("home of {label}: the host has no <{home}>"))?;
            let declared: BTreeSet<&String> = visible(home_probe).collect();
            let lacking: Vec<&&String> = needed.iter().filter(|n| !declared.contains(*n)).collect();
            ensure!(
                lacking.is_empty(),
                "home of {label}: <{home}> does not declare {lacking:?}, which bionic's <{name}> brings in"
            );
            seen.extend(declared);
            extras.insert(home.to_owned());
        }
        let includes = reduce(&extras, inputs.host);
        if !includes.is_empty() {
            out.entry(name.clone()).or_default().includes = includes;
        }
    }
    let unused: Vec<&str> = inputs
        .homes
        .iter()
        .map(|(f, _)| *f)
        .filter(|f| !homes_used.contains(*f))
        .collect();
    ensure!(
        unused.is_empty(),
        "homes no header needs any more (delete them): {unused:?}"
    );

    for m in inputs.macros {
        ensure!(
            !host_all.contains(&(*m).to_owned()),
            "the host's headers define {m}: drop it from the forwarded macros"
        );
        for (header, def) in macro_homes(inputs, m, &host_has)? {
            out.entry(header).or_default().defines.push(def);
        }
        for (name, path) in scanned(inputs) {
            let text = std::fs::read_to_string(&path)
                .with_context(|| format!("reading {}", path.display()))?;
            if uses_identifier(&text, m) {
                out.entry(name).or_default().hidden.insert((*m).to_owned());
            }
        }
    }
    Ok(Shape {
        wrappers: out,
        gaps,
    })
}

/// Host headers declaring every one of `needed`, fewest files reached first.
fn home_candidates(host: &BTreeMap<String, Probe>, needed: &BTreeSet<&String>) -> Vec<String> {
    let mut fits: Vec<(usize, &String)> = host
        .iter()
        .filter(|(_, p)| {
            let names: BTreeSet<&String> = visible(p).collect();
            needed.iter().all(|n| names.contains(n))
        })
        .map(|(n, p)| (p.files.len(), n))
        .collect();
    fits.sort();
    fits.into_iter().map(|(_, n)| n.clone()).collect()
}

/// The gaps as text, one per line: `<header> via <file>: names (homes: candidates)`.
pub fn print_gaps(gaps: &[Gap]) -> String {
    gaps.iter()
        .map(|g| {
            format!(
                "<{}> via {}: {} (homes: {})\n",
                g.header,
                g.file,
                g.names.join(" "),
                g.homes.join(" ")
            )
        })
        .collect()
}

/// `extras` without those another of them already reaches on the host (two reaching each other stay).
fn reduce(extras: &BTreeSet<String>, host: &BTreeMap<String, Probe>) -> Vec<String> {
    let reach: BTreeMap<&str, BTreeSet<&str>> = extras
        .iter()
        .map(|e| (e.as_str(), reached_names(e, &host[e])))
        .collect();
    extras
        .iter()
        .filter(|n| {
            !reach.iter().any(|(m, r)| {
                *m != n.as_str() && r.contains(n.as_str()) && !reach[n.as_str()].contains(m)
            })
        })
        .cloned()
        .collect()
}

/// The public bionic headers (the host has them too) bringing macro `m` in -- defined in the header itself
/// or in a private file its walk enters -- each with the definition, from the probes. An includable
/// header that defines it is not entered: its own wrapper gets the definition.
fn macro_homes(
    inputs: &Inputs,
    m: &str,
    host_has: &dyn Fn(&str) -> bool,
) -> Result<Vec<(String, String)>> {
    let mut found = Vec::new();
    for (name, probe) in inputs.bionic {
        if is_private(name) || !probe.usable() || !host_has(name) {
            continue;
        }
        let root = probe.file.as_ref().context("usable probe without a file")?;
        let walk = walk(inputs, probe, host_has)?;
        let def = std::iter::once(root)
            .chain(walk.private.iter().map(|(_, f)| f))
            .find_map(|f| probe.files.get(f).and_then(|r: &Reached| r.macros.get(m)));
        if let Some(def) = def {
            found.push((name.clone(), def.clone()));
        }
    }
    ensure!(
        !found.is_empty(),
        "no public bionic header the host also has defines {m} or brings it in"
    );
    ensure!(
        found.iter().all(|(_, d)| *d == found[0].1),
        "bionic defines {m} more than one way: {found:?}"
    );
    Ok(found)
}

/// Every host header the clash scan reads, by include name: what the host probes reach, and `scan`.
fn scanned(inputs: &Inputs) -> BTreeMap<String, PathBuf> {
    let mut out = inputs.scan.clone();
    for probe in inputs.host.values() {
        for inc in probe.files.values().flat_map(|r| r.includes.iter()) {
            if inc.angled {
                out.entry(inc.name.clone())
                    .or_insert_with(|| inc.file.clone());
            }
        }
    }
    for (name, probe) in inputs.host {
        if let Some(f) = &probe.file {
            out.entry(name.clone()).or_insert_with(|| f.clone());
        }
    }
    out
}

/// `ident` occurs in C source `text` as an identifier: outside comments and string / character
/// literals, not part of a longer identifier.
pub fn uses_identifier(text: &str, ident: &str) -> bool {
    let b = text.as_bytes();
    let word = |c: u8| c == b'_' || c.is_ascii_alphanumeric();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'/' if b.get(i + 1) == Some(&b'/') => {
                i = text[i..].find('\n').map_or(b.len(), |n| i + n);
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                i = text[i + 2..].find("*/").map_or(b.len(), |n| i + 2 + n + 2);
            }
            q @ (b'"' | b'\'') => {
                i += 1;
                while i < b.len() && b[i] != q && b[i] != b'\n' {
                    i += if b[i] == b'\\' { 2 } else { 1 };
                }
                i += 1;
            }
            c if word(c) => {
                let start = i;
                while i < b.len() && word(b[i]) {
                    i += 1;
                }
                if &text[start..i] == ident {
                    return true;
                }
            }
            _ => i += 1,
        }
    }
    false
}

/// The wrapper's text. `source`: what generated text names as its origin.
pub fn print(name: &str, w: &Wrapper, source: &str) -> Result<String> {
    let mut out = format!("// generated by atl-bridgegen from {source}, do not edit\n");
    for m in &w.hidden {
        writeln!(out, "#pragma push_macro(\"{m}\")\n#undef {m}")?;
    }
    writeln!(out, "#include_next <{name}>")?;
    for m in w.hidden.iter().rev() {
        writeln!(out, "#pragma pop_macro(\"{m}\")")?;
    }
    for def in &w.defines {
        let m = def
            .split(|c: char| c == '(' || c.is_whitespace())
            .next()
            .context("empty macro definition")?;
        writeln!(out, "#ifndef {m}\n#define {def}\n#endif")?;
    }
    if !w.includes.is_empty() {
        // once: the host header itself may be included again (assert.h), its additions need not
        let guard = guard(name);
        writeln!(out, "#ifndef {guard}\n#define {guard}")?;
        for i in &w.includes {
            writeln!(out, "#include <{i}>")?;
        }
        writeln!(out, "#endif")?;
    }
    Ok(out)
}

/// Include guard of a wrapper's additions: the name, every other character `_`. Injective over the
/// names it is used on is checked by `check_guards`.
fn guard(name: &str) -> String {
    let mangled: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    format!("__ATL_BIONIC_SHAPE_{mangled}")
}

/// Fails when two wrappers would share an include guard.
pub fn check_guards<'a>(names: impl Iterator<Item = &'a String>) -> Result<()> {
    let mut seen: BTreeMap<String, &str> = BTreeMap::new();
    for n in names {
        if let Some(other) = seen.insert(guard(n), n) {
            bail!("wrappers {other} and {n} share an include guard");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifiers_outside_comments_and_literals() {
        assert!(uses_identifier("struct x { int __unused; };", "__unused"));
        assert!(uses_identifier("int a;\n__unused[2];", "__unused"));
        assert!(!uses_identifier(
            "int __unused1; int x__unused;",
            "__unused"
        ));
        assert!(!uses_identifier(
            "/* __unused */ // __unused\n\"__unused\" '_'",
            "__unused"
        ));
        assert!(!uses_identifier("char *s = \"a\\\"__unused\";", "__unused"));
    }

    #[test]
    fn printed_shape() {
        let w = Wrapper {
            hidden: ["__unused".to_owned()].into(),
            defines: vec!["__unused __attribute__((__unused__))".to_owned()],
            includes: vec!["stddef.h".to_owned()],
        };
        let text = print("sys/x.h", &w, "test").unwrap();
        // hidden around the host header only; additions once, after it
        assert_eq!(
            text,
            "// generated by atl-bridgegen from test, do not edit\n#pragma push_macro(\"__unused\")\n#undef __unused\n#include_next <sys/x.h>\n#pragma pop_macro(\"__unused\")\n#ifndef __unused\n#define __unused __attribute__((__unused__))\n#endif\n#ifndef __ATL_BIONIC_SHAPE_sys_x_h\n#define __ATL_BIONIC_SHAPE_sys_x_h\n#include <stddef.h>\n#endif\n"
        );
        assert!(check_guards(["a/b.h".to_owned(), "a_b.h".to_owned()].iter()).is_err());
    }

    use crate::Target;
    use crate::front::{self, Lang};

    struct Tmp(PathBuf);
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Two fake libcs. bionic's `w.h` includes `x.h` (the host's does not), private `bits/p.h` (which
    /// includes `y.h` and declares `q`, the host's in `q.h` and `z.h`), and a uapi header; its
    /// `sys/cdefs.h` defines `__mac`, which the host's `k/u.h` uses as a field name; `bits/p.h` defines
    /// `__pm(x)`. bionic's `q.h` and `z.h` exist so the host's are probed; bionic's `q.h` reaches `y.h`,
    /// which the host's lacks.
    fn libcs(tag: &str) -> Tmp {
        let root = std::env::temp_dir().join(format!("atl-shape-{tag}-{}", std::process::id()));
        let files: &[(&str, &str)] = &[
            (
                "bionic/w.h",
                "#include <sys/cdefs.h>\n#include <x.h>\n#include <bits/p.h>\n#include <k/u.h>\nint w;\n",
            ),
            (
                "bionic/sys/cdefs.h",
                "#define __mac __attribute__((unused))\n",
            ),
            ("bionic/x.h", "int x;\n"),
            ("bionic/y.h", "int y;\n"),
            (
                "bionic/bits/p.h",
                "#include <y.h>\nint p_only;\nint q;\n#define __pm(x) ((x) + \\\n  1)\n",
            ),
            ("bionic/q.h", "#include <bits/p.h>\n"),
            ("bionic/z.h", "int z;\n"),
            ("uapi/k/u.h", "int kern;\n"),
            ("host/w.h", "#include <sys/cdefs.h>\nint w;\n"),
            ("host/sys/cdefs.h", "int cdefs;\n"),
            ("host/x.h", "int x;\n"),
            ("host/y.h", "int y;\n"),
            ("host/q.h", "int q;\n"),
            ("host/z.h", "int q;\nint z;\n"),
            ("host/k/u.h", "struct s { int __mac; };\n"),
        ];
        for (rel, text) in files {
            let path = root.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        Tmp(root)
    }

    fn sysroot(dirs: &[PathBuf]) -> Target {
        let mut args = vec!["-nostdlibinc".to_owned()];
        args.extend(dirs.iter().map(|d| format!("-isystem{}", d.display())));
        Target {
            arch: "x86_64".into(),
            triple: "x86_64-unknown-linux-gnu".into(),
            args,
        }
    }

    fn derived(dir: &Tmp, homes: &[(&str, &str)]) -> Result<Shape> {
        derived_with(dir, homes, &["__mac", "__pm"])
    }

    fn derived_with(dir: &Tmp, homes: &[(&str, &str)], macros: &[&str]) -> Result<Shape> {
        let include = dir.0.join("bionic");
        let uapi = [dir.0.join("uapi")];
        let bionic_t = sysroot(&[include.clone(), uapi[0].clone()]);
        let host_t = sysroot(&[dir.0.join("host")]);
        let public = ["q.h", "sys/cdefs.h", "w.h", "x.h", "y.h", "z.h"];
        let bionic = front::probe_includes(Lang::C, &bionic_t, &public)?;
        let names: Vec<String> = host_names(&bionic, &include, &uapi, homes)
            .into_iter()
            .collect();
        let names: Vec<&str> = names.iter().map(String::as_str).collect();
        let host = front::probe_includes(Lang::C, &host_t, &names)?;
        let scan = front::resolve_includes(Lang::C, &host_t, &["k/u.h"])?;
        derive(&Inputs {
            bionic: &bionic,
            host: &host,
            bionic_include: &include,
            uapi: &uapi,
            homes,
            macros,
            scan: &scan,
        })
    }

    fn strings(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn shape_of_fake_libcs() {
        let dir = libcs("derive");
        let with_home = derived(&dir, &[("bits/p.h", "q.h")]).unwrap();
        // x.h directly, y.h through the private file, q.h as its home; sys/cdefs.h the host's has too;
        // the uapi header is not shape
        assert_eq!(
            with_home.wrappers["w.h"].includes,
            strings(&["q.h", "x.h", "y.h"])
        );
        assert_eq!(
            with_home.wrappers["sys/cdefs.h"].defines,
            strings(&["__mac __attribute__((unused))"])
        );
        assert_eq!(
            with_home.wrappers["k/u.h"].hidden,
            BTreeSet::from(["__mac".to_owned()])
        );
        // q.h needs no home: its host version declares q itself
        assert_eq!(with_home.wrappers["q.h"].includes, strings(&["y.h"]));
        // a private file's macro, as written, in every header whose walk enters it
        let pm = strings(&["__pm(x) ((x) + \\\n  1)"]);
        assert_eq!(with_home.wrappers["w.h"].defines, pm);
        assert_eq!(with_home.wrappers["q.h"].defines, pm);
        assert_eq!(with_home.wrappers.len(), 4);
        // a macro the host defines, or no bionic header brings in, is refused
        assert!(derived_with(&dir, &[("bits/p.h", "q.h")], &["q"]).is_err());
        assert!(derived_with(&dir, &[("bits/p.h", "q.h")], &["__nowhere"]).is_err());
        assert!(with_home.gaps.is_empty());

        // without the home: reported, with both host headers declaring q (one file each, by name);
        // p_only is declared by no host header, so it is not shape
        let without = derived(&dir, &[]).unwrap();
        assert_eq!(without.wrappers["w.h"].includes, strings(&["x.h", "y.h"]));
        assert_eq!(
            without.gaps,
            [Gap {
                header: "w.h".into(),
                file: "bits/p.h".into(),
                names: strings(&["q"]),
                homes: strings(&["q.h", "z.h"]),
            }]
        );

        // a home must declare the names and must be needed
        assert!(derived(&dir, &[("bits/p.h", "x.h")]).is_err());
        assert!(derived(&dir, &[("bits/p.h", "q.h"), ("bits/none.h", "q.h")]).is_err());
    }
}
