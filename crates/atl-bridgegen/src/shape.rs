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
//! the kernel uapi, which the host has or a substitute generates). Direct includes suffice: an includable header's own wrapper
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
//! Substitutes (`substitutes`): bionic-only public headers (the host has none) whose content means the
//! same on the host, generated whole -- no `#include_next`; the same includes, homes and gaps with the
//! header itself walked as its first private file; every macro it and its private files define that the
//! host lacks, as bionic writes it, with the bionic-only macros their bodies name (`__BIONIC_CAST`).
//! Listed, not derived: whether a bionic macro means the same on glibc is not decidable from the headers
//! (`sys/sysconf.h`'s `_SC_NSIG 0x9e` is a bionic `sysconf` argument). Refused when the header declares
//! anything else no host header does (bionic-only API: `buildscript::Api` implements it) or a host
//! header uses one of its macros as an identifier without defining it the same way. A substitute may
//! include a bionic header whose declarations are generated (`declared`, an `Api` row's header) as it
//! includes a host header (bionic's `sys/_system_properties.h` is `#include <sys/system_properties.h>`);
//! only substitutes do: every other wrapper keeps bionic's shape of the host's headers.
//!
//! Kernel uapi headers only bionic carries (`kernel`; Android kernel features a desktop distro's
//! headers lack: `linux/incrementalfs.h`, `linux/ashmem.h`): every one the host does not resolve is
//! forwarded -- a generated header including bionic's file by absolute path, so depfiles (and the licence
//! gate) name bionic's file, not a copy. Derived, never listed; a name the host resolves is never
//! shadowed. Refused when a host header uses one of its macros as an identifier without defining it the
//! same way, as for substitutes.
//!
//! Views: everything above is derived twice, from probes without feature macros and with `_GNU_SOURCE`
//! (`Inputs::gnu`). clang++ predefines `_GNU_SOURCE`, so device C++ sees bionic's `__USE_GNU` sections
//! (`sched.h` `__CPU_BITTYPE`) and C does not. An include or macro derived in one view only keeps bionic's
//! own guard in the wrapper (`Guard`: `#if defined(__USE_GNU)`, or its negation), which both libcs
//! define from `_GNU_SOURCE`. A home, extra include or forwarded macro must be needed in some view.
//!
//! Linkage: bionic declares every libc function with C linkage (`__BEGIN_DECLS`) for C++ callers. A host
//! header whose own file declares functions with C++ linkage when read as C++ (`Inputs::cxx`; glibc
//! 2.41's `sys/pidfd.h` has no `__BEGIN_DECLS`) is included inside `extern "C"` in C++, so a C++ caller
//! references the C symbol the host library defines. A private file doing so fails: no wrapper reaches it.
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

/// One view's probes: both libcs' headers read with the same feature macros.
pub struct Probes<'a> {
    /// bionic's public headers, as bionic sees them.
    pub bionic: &'a BTreeMap<String, Probe>,
    /// The host's view of every name in `host_names`.
    pub host: &'a BTreeMap<String, Probe>,
}

/// The macro both libcs define from `_GNU_SOURCE`, and bionic's headers test for their GNU sections.
const GNU_MACRO: &str = "__USE_GNU";

/// The views a wrapper item is derived in, printed as the condition around it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Guard {
    /// Both views: unconditional.
    Always,
    /// The `_GNU_SOURCE` view only: `#if defined(__USE_GNU)`.
    Gnu,
    /// The default view only: `#if !defined(__USE_GNU)`.
    NotGnu,
}

impl Guard {
    /// Printing order of the groups.
    const ALL: [Guard; 3] = [Guard::Always, Guard::Gnu, Guard::NotGnu];

    fn condition(self) -> Option<String> {
        match self {
            Guard::Always => None,
            Guard::Gnu => Some(format!("defined({GNU_MACRO})")),
            Guard::NotGnu => Some(format!("!defined({GNU_MACRO})")),
        }
    }
}

/// What the wrappers are derived from.
pub struct Inputs<'a> {
    /// Read without feature macros: C as AOSP compiles it.
    pub default: Probes<'a>,
    /// Read with `_GNU_SOURCE`: every C++ unit (clang++ predefines it).
    pub gnu: Probes<'a>,
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
    /// bionic-only public headers generated whole. Each must be a usable bionic header the host lacks.
    pub substitutes: &'a [&'a str],
    /// bionic public headers whose declarations atl-bionic generates (`buildscript::Api::header`), by
    /// include name: what a substitute may include besides host headers and substitutes.
    pub declared: &'a [&'a str],
    /// Host header -> what its wrapper includes beyond bionic's shape: names AOSP code takes from the
    /// header in a configuration bionic never builds (`__ANDROID__` without `__BIONIC__`), or bionic
    /// API declared in a header the host also has (its generated declarations). Each must be needed:
    /// the host has the header and its version does not already reach the include in some view (in
    /// the other, the include is guarded off).
    pub extra: &'a [(&'a str, &'a str)],
    /// Host headers outside the probes the clash scan reads (the host's kernel uapi), by include name.
    pub scan: &'a BTreeMap<String, PathBuf>,
    /// bionic's kernel uapi headers by include name, each the file bionic's search order finds; `scan`
    /// is the host's resolution of the same names.
    pub kernel: &'a BTreeMap<String, PathBuf>,
    /// The host's view of every name in `host_names` read as C++ with `_GNU_SOURCE`, as device C++
    /// includes them (`Reached::cxx_linkage`).
    pub cxx: &'a BTreeMap<String, Probe>,
}

/// One generated header.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Wrapper {
    /// The host has no such header: a substitute, nothing to `#include_next`.
    pub substitute: bool,
    /// Macros hidden around the host header (it uses them as identifiers).
    pub hidden: BTreeSet<String>,
    /// Macro definitions as bionic writes them (`NAME body`), each with the views it is derived in.
    pub defines: Vec<(Guard, String)>,
    /// Headers bionic's version reaches and the host's does not, each with the views.
    pub includes: Vec<(Guard, String)>,
    /// The host header declares functions with C++ linkage, which bionic's gives C linkage: included
    /// inside `extern "C"` in C++.
    pub c_linkage: bool,
}

/// One view's wrapper.
#[derive(Default)]
struct Plain {
    substitute: bool,
    defines: Vec<String>,
    includes: Vec<String>,
}

/// Every include name the host is probed for: bionic's public headers, the names bionic's own files
/// include that could be includable (in any of the views `bionic` holds), the homes.
pub fn host_names(
    bionic: &[&BTreeMap<String, Probe>],
    bionic_include: &Path,
    uapi: &[PathBuf],
    homes: &[(&str, &str)],
) -> BTreeSet<String> {
    let mut out: BTreeSet<String> = bionic.iter().flat_map(|b| b.keys().cloned()).collect();
    out.extend(homes.iter().map(|(_, h)| (*h).to_owned()));
    for probe in bionic.iter().flat_map(|b| b.values()) {
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
    /// bionic's kernel uapi headers the host lacks, by include name -> bionic's file (`print_forward`).
    pub uapi: BTreeMap<String, PathBuf>,
}

/// Names bionic's `header` brings in through private `file` that the wrapped host header lacks.
#[derive(Debug, PartialEq, Eq)]
pub struct Gap {
    pub header: String,
    pub file: String,
    pub names: Vec<String>,
    /// Host headers declaring all of them, fewest files reached first: home candidates.
    pub homes: Vec<String>,
    /// The views it is found in.
    pub guard: Guard,
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
/// uapi, the compiler's headers. `includable`: the names a wrapper may include (the host's; substitutes).
fn walk(inputs: &Inputs, probe: &Probe, includable: &dyn Fn(&str) -> bool) -> Result<Walk> {
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
                && includable(&inc.name)
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

/// The shape, both views merged. Fails on a home no view needs or one insufficient where used, an extra
/// include the host reaches in every view, a forwarded macro the host has in every view bringing it in,
/// no public bionic header the host also has brings in, or bionic defines two ways; on a substitute the
/// host has, bionic lacks or that does not substitute (`substitute_macros`).
pub fn derive(inputs: &Inputs) -> Result<Shape> {
    let views = [&inputs.default, &inputs.gnu];
    let host_has = |n: &str| {
        views
            .iter()
            .any(|v| v.host.get(n).is_some_and(Probe::usable))
    };
    let uapi: BTreeMap<String, PathBuf> = inputs
        .kernel
        .iter()
        .filter(|(n, _)| !inputs.scan.contains_key(*n) && !host_has(n))
        .map(|(n, f)| (n.clone(), f.clone()))
        .collect();
    let host_text = if inputs.substitutes.is_empty() && uapi.is_empty() {
        HostText::default()
    } else {
        HostText::read(inputs)?
    };
    for (name, file) in &uapi {
        let text =
            std::fs::read_to_string(file).with_context(|| format!("reading {}", file.display()))?;
        let clashes: Vec<String> = defines_in(&text)
            .into_iter()
            .filter(|d| host_text.clashes(d))
            .filter_map(|d| macro_name(&d).map(str::to_owned))
            .collect();
        ensure!(
            clashes.is_empty(),
            "bionic-only uapi header <{name}>: host headers use its macros {clashes:?} as identifiers"
        );
    }

    let d = derive_view(inputs, &inputs.default, &host_text)?;
    let g = derive_view(inputs, &inputs.gnu, &host_text)?;
    let unused: Vec<&str> = inputs
        .homes
        .iter()
        .map(|(f, _)| *f)
        .filter(|f| !d.homes_used.contains(*f) && !g.homes_used.contains(*f))
        .collect();
    ensure!(
        unused.is_empty(),
        "homes no header needs any more (delete them): {unused:?}"
    );
    for (header, inc) in inputs.extra {
        let row = ((*header).to_owned(), (*inc).to_owned());
        ensure!(
            !(d.reached.contains(&row) && g.reached.contains(&row)),
            "extra include of <{header}>: the host's already reaches <{inc}> (delete the row)"
        );
    }
    for &m in inputs.macros {
        if d.emitted.contains(m) || g.emitted.contains(m) {
            continue;
        }
        ensure!(
            !(d.host_macros.contains(m) && g.host_macros.contains(m)),
            "the host's headers define {m}: drop it from the forwarded macros"
        );
        bail!("no public bionic header the host also has defines {m} or brings it in");
    }

    let mut out: BTreeMap<String, Wrapper> = BTreeMap::new();
    let names: BTreeSet<&String> = d.wrappers.keys().chain(g.wrappers.keys()).collect();
    let none = Plain::default();
    for name in names {
        let (dw, gw) = (d.wrappers.get(name), g.wrappers.get(name));
        let substitute = dw.or(gw).is_some_and(|w| w.substitute);
        ensure!(
            dw.into_iter().chain(gw).all(|w| w.substitute == substitute),
            "<{name}>: a substitute in one view only"
        );
        let (dw, gw) = (dw.unwrap_or(&none), gw.unwrap_or(&none));
        out.insert(
            name.clone(),
            Wrapper {
                substitute,
                hidden: BTreeSet::new(),
                defines: merge(&dw.defines, &gw.defines),
                includes: merge(&dw.includes, &gw.includes),
                c_linkage: false,
            },
        );
    }
    for name in cxx_linkage(inputs.cxx)? {
        out.entry(name).or_default().c_linkage = true;
    }
    // the hidden names: host headers using an emitted macro as an identifier, whichever view reaches them;
    // not one the header defines itself (glibc's TEMP_FAILURE_RETRY under __USE_GNU): hiding it would pop its own definition away
    let emitted: Vec<&str> = inputs
        .macros
        .iter()
        .copied()
        .filter(|m| d.emitted.contains(*m) || g.emitted.contains(*m))
        .collect();
    if !emitted.is_empty() {
        for (name, path) in scanned(inputs) {
            let text = std::fs::read_to_string(&path)
                .with_context(|| format!("reading {}", path.display()))?;
            let idents: BTreeSet<&str> = identifiers(&text).collect();
            let defined: BTreeSet<String> = defines_in(&text)
                .iter()
                .filter_map(|def| macro_name(def).map(str::to_owned))
                .collect();
            for m in emitted
                .iter()
                .filter(|m| idents.contains(**m) && !defined.contains(**m))
            {
                out.entry(name.clone())
                    .or_default()
                    .hidden
                    .insert((*m).to_owned());
            }
        }
    }
    Ok(Shape {
        wrappers: out,
        gaps: merge_gaps(d.gaps, g.gaps),
        uapi,
    })
}

/// The host headers (include names) whose own file declares functions with C++ linkage in C++ `cxx`
/// probes. Fails on another file doing so: a private one, which no wrapper includes by its name.
fn cxx_linkage(cxx: &BTreeMap<String, Probe>) -> Result<BTreeSet<String>> {
    let own: BTreeMap<&PathBuf, &String> = cxx
        .iter()
        .filter(|(_, p)| p.usable())
        .filter_map(|(n, p)| p.file.as_ref().map(|f| (f, n)))
        .collect();
    let mut out = BTreeSet::new();
    for probe in cxx.values() {
        for (file, reached) in &probe.files {
            if reached.cxx_linkage.is_empty() {
                continue;
            }
            let name = own.get(file).with_context(|| {
                format!(
                    "{} declares {:?} with C++ linkage, and is no probed header's own file",
                    file.display(),
                    reached.cxx_linkage
                )
            })?;
            out.insert((*name).clone());
        }
    }
    Ok(out)
}

/// Items of the default view `d` and the `_GNU_SOURCE` view `g`, each with the views it is in: `d`'s
/// order, then `g`'s own.
fn merge(d: &[String], g: &[String]) -> Vec<(Guard, String)> {
    let mut out: Vec<(Guard, String)> = d
        .iter()
        .map(|x| {
            let guard = if g.contains(x) {
                Guard::Always
            } else {
                Guard::NotGnu
            };
            (guard, x.clone())
        })
        .collect();
    out.extend(
        g.iter()
            .filter(|x| !d.contains(x))
            .map(|x| (Guard::Gnu, x.clone())),
    );
    out
}

/// `merge` of the gaps (each view's are `Guard::Always`).
fn merge_gaps(d: Vec<Gap>, g: Vec<Gap>) -> Vec<Gap> {
    let mut out = Vec::new();
    for mut gap in d {
        if !g.contains(&gap) {
            gap.guard = Guard::NotGnu;
        }
        out.push(gap);
    }
    for mut gap in g {
        if !out.iter().any(|o| o.guard == Guard::Always && *o == gap) {
            gap.guard = Guard::Gnu;
            out.push(gap);
        }
    }
    out
}

/// One view's shape, before the checks across views (`derive`).
#[derive(Default)]
struct ViewShape {
    wrappers: BTreeMap<String, Plain>,
    gaps: Vec<Gap>,
    /// `Inputs::homes` rows used.
    homes_used: BTreeSet<String>,
    /// `Inputs::extra` rows whose include the host's header already reaches.
    reached: BTreeSet<(String, String)>,
    /// Forwarded macros the host's headers define.
    host_macros: BTreeSet<String>,
    /// Forwarded macros defined in some wrapper.
    emitted: BTreeSet<String>,
}

/// The shape of one view's probes `p` (`derive`).
fn derive_view(inputs: &Inputs, p: &Probes, host_text: &HostText) -> Result<ViewShape> {
    let host_has = |n: &str| p.host.get(n).is_some_and(Probe::usable);
    let host_all: BTreeSet<&String> = p.host.values().flat_map(visible).collect();
    let mut out = ViewShape::default();
    for s in inputs.substitutes {
        ensure!(
            !host_has(s),
            "the host has <{s}>: drop it from the substitutes"
        );
        ensure!(
            p.bionic.get(*s).is_some_and(Probe::usable),
            "substitute <{s}>: bionic has no usable such public header"
        );
    }
    // what an included name gives: the host's header, or for a substitute bionic's (generated whole), so
    // `<endian.h>` includes the `<sys/endian.h>` substitute (`betoh64`) instead of walking it as private
    let shaped: BTreeMap<&str, &Probe> = p
        .host
        .iter()
        .filter(|(_, p)| p.usable())
        .map(|(n, p)| (n.as_str(), p))
        .chain(
            inputs
                .substitutes
                .iter()
                .filter_map(|s| p.bionic.get(*s).map(|p| (*s, p))),
        )
        .collect();
    // a substitute also takes the generated declaration headers (`Inputs::declared`) as given
    let for_substitutes: BTreeMap<&str, &Probe> = shaped
        .iter()
        .map(|(n, p)| (*n, *p))
        .chain(
            inputs
                .declared
                .iter()
                .filter_map(|d| p.bionic.get(*d).map(|p| (*d, p))),
        )
        .collect();
    for (name, bionic) in p.bionic {
        if !bionic.usable() {
            continue;
        }
        let host = p.host.get(name).filter(|p| p.usable());
        if host.is_none() && !inputs.substitutes.contains(&name.as_str()) {
            continue;
        }
        let shaped = if host.is_none() {
            &for_substitutes
        } else {
            &shaped
        };
        let host_reached = host.map_or_else(
            || BTreeSet::from([name.as_str()]),
            |h| reached_names(name, h),
        );
        let walk = walk(inputs, bionic, &|n: &str| shaped.contains_key(n))?;
        let mut extras: BTreeSet<String> = walk
            .includable
            .into_iter()
            .filter(|n| n != name && !host_reached.contains(n.as_str()))
            .collect();
        let mut seen: BTreeSet<&String> = host
            .into_iter()
            .flat_map(visible)
            .chain(extras.iter().flat_map(|e| visible(shaped[e.as_str()])))
            .collect();
        let root = bionic
            .file
            .as_ref()
            .context("usable probe without a file")?;
        // a substitute's own names are walked as a private file's: the host declares them elsewhere
        let own = host.is_none().then(|| (name.clone(), root.clone()));
        let files: Vec<(String, PathBuf)> = own.into_iter().chain(walk.private).collect();
        let defines = if host.is_none() {
            substitute_macros(inputs, name, bionic, &files, &host_all, host_text)?
        } else {
            Vec::new()
        };
        for (label, file) in files {
            let needed: BTreeSet<&String> = bionic.files[&file]
                .names
                .iter()
                .filter(|n| !seen.contains(n) && host_all.contains(n))
                .collect();
            if needed.is_empty() {
                continue;
            }
            let Some(&(_, home)) = inputs.homes.iter().find(|(f, _)| *f == label) else {
                out.gaps.push(Gap {
                    header: name.clone(),
                    homes: home_candidates(p.host, &needed),
                    file: label,
                    names: needed.iter().map(|n| (*n).clone()).collect(),
                    guard: Guard::Always,
                });
                continue;
            };
            out.homes_used.insert(label.clone());
            let home_probe = p
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
        let includes = reduce(&extras, shaped);
        if host.is_none() {
            out.wrappers.insert(
                name.clone(),
                Plain {
                    substitute: true,
                    includes,
                    defines,
                },
            );
        } else if !includes.is_empty() {
            out.wrappers.entry(name.clone()).or_default().includes = includes;
        }
    }
    for (header, inc) in inputs.extra {
        let host = p
            .host
            .get(*header)
            .filter(|p| p.usable())
            .with_context(|| format!("extra include of <{header}>: the host has no such header"))?;
        if reached_names(header, host).contains(inc) {
            out.reached
                .insert(((*header).to_owned(), (*inc).to_owned()));
            continue;
        }
        let includes = &mut out
            .wrappers
            .entry((*header).to_owned())
            .or_default()
            .includes;
        includes.push((*inc).to_owned());
        includes.sort();
        includes.dedup();
    }

    for m in inputs.macros {
        if host_all.contains(&(*m).to_owned()) {
            out.host_macros.insert((*m).to_owned());
            continue;
        }
        for (header, def) in macro_homes(inputs, p, m, &host_has)? {
            out.emitted.insert((*m).to_owned());
            out.wrappers.entry(header).or_default().defines.push(def);
        }
    }
    Ok(out)
}

/// What the host headers' text says, for a substitute's macros: every identifier they use, and every
/// macro definition they write (`NAME body`, whitespace collapsed).
#[derive(Default)]
struct HostText {
    idents: BTreeSet<String>,
    defines: BTreeMap<String, BTreeSet<String>>,
}

impl HostText {
    /// Every header the clash scan reads (`scanned`).
    fn read(inputs: &Inputs) -> Result<Self> {
        let mut out = Self::default();
        for path in scanned(inputs).values() {
            let text = std::fs::read_to_string(path)
                .with_context(|| format!("reading {}", path.display()))?;
            out.idents.extend(identifiers(&text).map(str::to_owned));
            for def in defines_in(&text) {
                if let Some(name) = macro_name(&def) {
                    out.defines
                        .entry(name.to_owned())
                        .or_default()
                        .insert(def.clone());
                }
            }
        }
        Ok(out)
    }

    /// Defining `def` (`NAME body`) would change what a host header means: it uses the name and does
    /// not define it exactly so itself (the kernel's `__LITTLE_ENDIAN_BITFIELD` is bionic's too).
    fn clashes(&self, def: &str) -> bool {
        let Some(name) = macro_name(def) else {
            return false;
        };
        self.idents.contains(name)
            && self
                .defines
                .get(name)
                .is_none_or(|d| d.len() != 1 || !d.contains(&collapse(&def.replace("\\\n", " "))))
    }
}

/// Every macro definition C `text` writes (`NAME body`, continuations joined, whitespace collapsed).
fn defines_in(text: &str) -> Vec<String> {
    let joined = text.replace("\\\n", " ");
    joined
        .lines()
        .filter_map(|line| {
            let rest = line.trim_start().strip_prefix('#')?;
            let def = rest.trim_start().strip_prefix("define")?;
            def.starts_with(char::is_whitespace).then(|| collapse(def))
        })
        .collect()
}

/// Whitespace runs as one space, trimmed.
fn collapse(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The name of a definition `NAME body` / `NAME(args) body`.
fn macro_name(def: &str) -> Option<&str> {
    def.split(|c: char| c == '(' || c.is_whitespace())
        .next()
        .filter(|n| !n.is_empty())
}

/// A substitute's macros: those `files` (the header, its private files) define that no host header
/// does, plus the bionic-only macros their bodies name, defined in any of bionic's own files `probe`
/// reaches (not the compiler's `limits.h`, which bionic's redefines); by name. Fails when the header
/// provides more than macros the host lacks, or one of them clashes (`HostText::clashes`).
fn substitute_macros(
    inputs: &Inputs,
    name: &str,
    probe: &Probe,
    files: &[(String, PathBuf)],
    host_all: &BTreeSet<&String>,
    host: &HostText,
) -> Result<Vec<String>> {
    let mut defined: BTreeMap<&str, &str> = BTreeMap::new();
    let own = probe
        .files
        .iter()
        .filter(|(f, _)| f.starts_with(inputs.bionic_include));
    for (_, r) in own {
        for (m, def) in &r.macros {
            if let Some(other) = defined.insert(m, def) {
                ensure!(
                    other == def,
                    "<{name}>: bionic defines {m} two ways: {other:?}, {def:?}"
                );
            }
        }
    }
    let mut api = BTreeSet::new();
    let mut todo = Vec::new();
    for (_, file) in files {
        let reached = &probe.files[file];
        for n in reached.names.iter().filter(|n| !host_all.contains(n)) {
            if reached.macros.contains_key(n) {
                todo.push(n.as_str());
            } else {
                api.insert(n.clone());
            }
        }
    }
    let mut defs: BTreeMap<&str, &str> = BTreeMap::new();
    while let Some(m) = todo.pop() {
        if defs.contains_key(m) || host_all.contains(&m.to_owned()) {
            continue;
        }
        let def = defined[m];
        defs.insert(m, def);
        todo.extend(identifiers(def).filter(|i| defined.contains_key(i)));
    }
    ensure!(
        api.is_empty(),
        "substitute <{name}> declares bionic-only API the host lacks (atl-bionic implements it as an Api, or drop the substitute): {api:?}"
    );
    let clashes: Vec<&str> = defs
        .iter()
        .filter(|(_, d)| host.clashes(d))
        .map(|(m, _)| *m)
        .collect();
    ensure!(
        clashes.is_empty(),
        "substitute <{name}>: host headers use its macros {clashes:?} as identifiers"
    );
    Ok(defs.into_values().map(str::to_owned).collect())
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

/// The gaps as text, one per line: `<header> via <file>: names (homes: candidates)`, then ` [if <condition>]`
/// for a gap of one view only.
pub fn print_gaps(gaps: &[Gap]) -> String {
    gaps.iter()
        .map(|g| {
            let only = g
                .guard
                .condition()
                .map_or_else(String::new, |c| format!(" [if {c}]"));
            format!(
                "<{}> via {}: {} (homes: {}){only}\n",
                g.header,
                g.file,
                g.names.join(" "),
                g.homes.join(" ")
            )
        })
        .collect()
}

/// `extras` without those another of them already reaches on the host (two reaching each other stay).
fn reduce(extras: &BTreeSet<String>, shaped: &BTreeMap<&str, &Probe>) -> Vec<String> {
    let reach: BTreeMap<&str, BTreeSet<&str>> = extras
        .iter()
        .map(|e| (e.as_str(), reached_names(e, shaped[e.as_str()])))
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
/// or in a private file its walk enters -- each with the definition, from one view's probes `p`; none
/// when this view has none. An includable header that defines it is not entered: its own wrapper gets
/// the definition.
fn macro_homes(
    inputs: &Inputs,
    p: &Probes,
    m: &str,
    host_has: &dyn Fn(&str) -> bool,
) -> Result<Vec<(String, String)>> {
    let mut found = Vec::new();
    for (name, probe) in p.bionic {
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
        found.iter().all(|(_, d)| *d == found[0].1),
        "bionic defines {m} more than one way: {found:?}"
    );
    Ok(found)
}

/// Every host header the clash scan reads, by include name: what the host probes of either view reach,
/// and `scan`.
fn scanned(inputs: &Inputs) -> BTreeMap<String, PathBuf> {
    let mut out = inputs.scan.clone();
    let hosts = [inputs.default.host, inputs.gnu.host];
    for probe in hosts.iter().flat_map(|h| h.values()) {
        for inc in probe.files.values().flat_map(|r| r.includes.iter()) {
            if inc.angled {
                out.entry(inc.name.clone())
                    .or_insert_with(|| inc.file.clone());
            }
        }
    }
    for (name, probe) in hosts.iter().flat_map(|h| h.iter()) {
        if let Some(f) = &probe.file {
            out.entry(name.clone()).or_insert_with(|| f.clone());
        }
    }
    out
}

/// `ident` occurs in C source `text` as an identifier: outside comments and string / character
/// literals, not part of a longer identifier.
pub fn uses_identifier(text: &str, ident: &str) -> bool {
    identifiers(text).any(|i| i == ident)
}

/// The identifiers of C source `text`, in order: outside comments and string / character literals.
/// Number tokens come out too (`1234`, `1u`); they never equal an identifier.
pub fn identifiers(text: &str) -> impl Iterator<Item = &str> {
    let b = text.as_bytes();
    let word = |c: u8| c == b'_' || c.is_ascii_alphanumeric();
    let mut i = 0;
    std::iter::from_fn(move || {
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
                    return Some(&text[start..i]);
                }
                _ => i += 1,
            }
        }
        None
    })
}

/// The wrapper's text. `source`: what generated text names as its origin.
pub fn print(name: &str, w: &Wrapper, source: &str) -> Result<String> {
    let mut out = format!("// generated by atl-bridgegen from {source}, do not edit\n");
    if w.substitute {
        // bionic's header whole, once, from host headers
        let guard = guard(name);
        writeln!(out, "#ifndef {guard}\n#define {guard}")?;
        print_includes(&mut out, &w.includes)?;
        print_defines(&mut out, &w.defines)?;
        writeln!(out, "#endif")?;
        return Ok(out);
    }
    for m in &w.hidden {
        writeln!(out, "#pragma push_macro(\"{m}\")\n#undef {m}")?;
    }
    if w.c_linkage {
        writeln!(out, "#ifdef __cplusplus\nextern \"C\" {{\n#endif")?;
    }
    writeln!(out, "#include_next <{name}>")?;
    if w.c_linkage {
        writeln!(out, "#ifdef __cplusplus\n}}\n#endif")?;
    }
    for m in w.hidden.iter().rev() {
        writeln!(out, "#pragma pop_macro(\"{m}\")")?;
    }
    print_defines(&mut out, &w.defines)?;
    if !w.includes.is_empty() {
        // once: the host header itself may be included again (assert.h), its additions need not
        let guard = guard(name);
        writeln!(out, "#ifndef {guard}\n#define {guard}")?;
        print_includes(&mut out, &w.includes)?;
        writeln!(out, "#endif")?;
    }
    Ok(out)
}

/// The forwarding header of a bionic-only uapi header: bionic's `file` itself, by absolute path.
pub fn print_forward(file: &Path, source: &str) -> Result<String> {
    let path = file.to_str().context("non-UTF-8 uapi header path")?;
    ensure!(
        file.is_absolute() && !path.contains(['"', '\n']),
        "uapi header path {path:?} cannot be an include"
    );
    Ok(format!(
        "// generated by atl-bridgegen from {source}, do not edit\n#include \"{path}\"\n"
    ))
}

/// Each definition (`NAME body`) unless the name is defined already.
fn print_defines(out: &mut String, defines: &[(Guard, String)]) -> Result<()> {
    print_guarded(out, defines, |out, def| {
        let m = macro_name(def).context("empty macro definition")?;
        writeln!(out, "#ifndef {m}\n#define {def}\n#endif")?;
        Ok(())
    })
}

fn print_includes(out: &mut String, includes: &[(Guard, String)]) -> Result<()> {
    print_guarded(out, includes, |out, i| Ok(writeln!(out, "#include <{i}>")?))
}

/// `items` grouped by guard, unconditional ones first, each group inside its condition.
fn print_guarded(
    out: &mut String,
    items: &[(Guard, String)],
    each: impl Fn(&mut String, &str) -> Result<()>,
) -> Result<()> {
    for g in Guard::ALL {
        let group: Vec<&String> = items
            .iter()
            .filter(|(x, _)| *x == g)
            .map(|(_, s)| s)
            .collect();
        if group.is_empty() {
            continue;
        }
        let condition = g.condition();
        if let Some(c) = &condition {
            writeln!(out, "#if {c}")?;
        }
        for s in group {
            each(out, s)?;
        }
        if condition.is_some() {
            writeln!(out, "#endif")?;
        }
    }
    Ok(())
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
            defines: always(&["__unused __attribute__((__unused__))"]),
            includes: always(&["stddef.h"]),
            ..Wrapper::default()
        };
        let text = print("sys/x.h", &w, "test").unwrap();
        // hidden around the host header only; additions once, after it
        assert_eq!(
            text,
            "// generated by atl-bridgegen from test, do not edit\n#pragma push_macro(\"__unused\")\n#undef __unused\n#include_next <sys/x.h>\n#pragma pop_macro(\"__unused\")\n#ifndef __unused\n#define __unused __attribute__((__unused__))\n#endif\n#ifndef __ATL_BIONIC_SHAPE_sys_x_h\n#define __ATL_BIONIC_SHAPE_sys_x_h\n#include <stddef.h>\n#endif\n"
        );
        assert!(check_guards(["a/b.h".to_owned(), "a_b.h".to_owned()].iter()).is_err());

        // C linkage: the host header alone inside extern "C", in C++ only; the hidden macros around both
        let l = Wrapper {
            hidden: ["__unused".to_owned()].into(),
            c_linkage: true,
            ..Wrapper::default()
        };
        assert_eq!(
            print("sys/x.h", &l, "test").unwrap(),
            "// generated by atl-bridgegen from test, do not edit\n#pragma push_macro(\"__unused\")\n#undef __unused\n#ifdef __cplusplus\nextern \"C\" {\n#endif\n#include_next <sys/x.h>\n#ifdef __cplusplus\n}\n#endif\n#pragma pop_macro(\"__unused\")\n"
        );

        // a substitute: no host header to include, everything once
        let s = Wrapper {
            substitute: true,
            defines: always(&["_M 1"]),
            includes: always(&["limits.h"]),
            ..Wrapper::default()
        };
        assert_eq!(
            print("sys/x.h", &s, "test").unwrap(),
            "// generated by atl-bridgegen from test, do not edit\n#ifndef __ATL_BIONIC_SHAPE_sys_x_h\n#define __ATL_BIONIC_SHAPE_sys_x_h\n#include <limits.h>\n#ifndef _M\n#define _M 1\n#endif\n#endif\n"
        );

        // one view's items inside bionic's guard, unconditional ones first
        let v = Wrapper {
            defines: vec![(Guard::Gnu, "_G 1".to_owned())],
            includes: vec![
                (Guard::NotGnu, "d.h".to_owned()),
                (Guard::Always, "a.h".to_owned()),
                (Guard::Gnu, "g.h".to_owned()),
            ],
            ..Wrapper::default()
        };
        assert_eq!(
            print("x.h", &v, "test").unwrap(),
            "// generated by atl-bridgegen from test, do not edit\n#include_next <x.h>\n#if defined(__USE_GNU)\n#ifndef _G\n#define _G 1\n#endif\n#endif\n#ifndef __ATL_BIONIC_SHAPE_x_h\n#define __ATL_BIONIC_SHAPE_x_h\n#include <a.h>\n#if defined(__USE_GNU)\n#include <g.h>\n#endif\n#if !defined(__USE_GNU)\n#include <d.h>\n#endif\n#endif\n"
        );
    }

    /// Unguarded items.
    fn always(v: &[&str]) -> Vec<(Guard, String)> {
        v.iter().map(|s| (Guard::Always, (*s).to_owned())).collect()
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
    /// which the host's lacks. bionic-only headers: `o.h` includes `x.h` and defines `__om` in terms of
    /// `__oc` from its `sys/cdefs.h`; `f.h` declares a function, `s.h` only includes `f.h`; `c.h` defines a macro in terms of
    /// `__mac`; `e.h` defines `__kb` as `k/u.h` does, `d.h` defines `__kd` other than `k/u.h`, which
    /// uses both. Views: bionic's `g.h` includes `x.h` and defines `__gm` with `_GNU_SOURCE`, else
    /// includes `y.h`; the host's `z.h` includes `y.h` with `_GNU_SOURCE`. Linkage: the host's `l.h`
    /// declares a function with no linkage specification (and a static one), the host's `m.h` one in
    /// `extern "C"` and one in `extern "C++"`; bionic's both in C.
    fn libcs(tag: &str) -> Tmp {
        let root = std::env::temp_dir().join(format!("atl-shape-{tag}-{}", std::process::id()));
        let files: &[(&str, &str)] = &[
            ("bionic/l.h", "int l_fn(int);\n"),
            ("bionic/m.h", "int m_fn(void);\n"),
            (
                "host/l.h",
                "#include <m.h>\nint l_fn(int);\nstatic int l_static(void) { return 0; }\n",
            ),
            (
                "host/m.h",
                "#ifdef __cplusplus\nextern \"C\" {\n#endif\nint m_fn(void);\n#ifdef __cplusplus\n}\nextern \"C++\" int m_overload(int);\n#endif\n",
            ),
            (
                "bionic/w.h",
                "#include <sys/cdefs.h>\n#include <x.h>\n#include <bits/p.h>\n#include <k/u.h>\nint w;\n",
            ),
            (
                "bionic/sys/cdefs.h",
                "#define __mac __attribute__((unused))\n#define __oc(v) ((v) + 1)\n",
            ),
            (
                "bionic/o.h",
                "#include <sys/cdefs.h>\n#include <x.h>\nint x;\n#define __om(v) __oc(v)\n",
            ),
            ("bionic/n.h", "#include <o.h>\n"),
            ("bionic/f.h", "int f_only(void);\n"),
            ("bionic/s.h", "#include <f.h>\n"),
            ("bionic/c.h", "#include <sys/cdefs.h>\n#define __cm __mac\n"),
            ("bionic/e.h", "#define __kb\n"),
            ("bionic/d.h", "#define __kd 2\n"),
            ("bionic/x.h", "int x;\n"),
            ("bionic/y.h", "int y;\n"),
            (
                "bionic/bits/p.h",
                "#include <y.h>\nint p_only;\nint q;\n#define __pm(x) ((x) + \\\n  1)\n",
            ),
            ("bionic/q.h", "#include <bits/p.h>\n"),
            ("bionic/z.h", "int z;\n"),
            (
                "bionic/g.h",
                "#include <sys/cdefs.h>\n#ifdef _GNU_SOURCE\n#include <x.h>\n#define __gm 1\n#else\n#include <y.h>\n#endif\nint g;\n",
            ),
            ("host/g.h", "int g;\n"),
            ("uapi/k/u.h", "int kern;\n"),
            ("uapi/k/v.h", "#define __kv 1\n"),
            ("uapi/k/m.h", "#define __mac 1\n"),
            ("host/w.h", "#include <sys/cdefs.h>\nint w;\n"),
            ("host/n.h", "int n;\n"),
            ("host/sys/cdefs.h", "int cdefs;\n"),
            ("host/x.h", "int x;\n"),
            ("host/y.h", "int y;\n"),
            ("host/q.h", "int q;\n"),
            // the host defines a forwarded macro itself in the GNU view only (glibc's TEMP_FAILURE_RETRY under __USE_GNU)
            ("bionic/t.h", "int t;\n#define __tm(e) (e)\n"),
            (
                "host/t.h",
                "int t;\n#ifdef _GNU_SOURCE\n#define __tm(e) (e)\nstatic int t_v = __tm(1);\n#endif\n",
            ),
            (
                "host/z.h",
                "int q;\nint z;\n#ifdef _GNU_SOURCE\n#include <y.h>\n#endif\n",
            ),
            (
                "host/k/u.h",
                "struct s { int __mac; };\n#  define  __kb\n#define __kd 1\n#if defined(__kb) && __kd\n#endif\n",
            ),
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
        derived_with(
            dir,
            homes,
            &["__mac", "__pm", "__gm", "__tm"],
            &["e.h", "o.h"],
        )
    }

    fn derived_with(
        dir: &Tmp,
        homes: &[(&str, &str)],
        macros: &[&str],
        substitutes: &[&str],
    ) -> Result<Shape> {
        derived_extra(dir, homes, macros, substitutes, &[])
    }

    fn derived_extra(
        dir: &Tmp,
        homes: &[(&str, &str)],
        macros: &[&str],
        substitutes: &[&str],
        extra: &[(&str, &str)],
    ) -> Result<Shape> {
        derived_kernel(dir, homes, macros, substitutes, extra, &["k/u.h", "k/v.h"])
    }

    fn derived_kernel(
        dir: &Tmp,
        homes: &[(&str, &str)],
        macros: &[&str],
        substitutes: &[&str],
        extra: &[(&str, &str)],
        kernel: &[&str],
    ) -> Result<Shape> {
        derived_declared(dir, homes, macros, substitutes, extra, kernel, &[])
    }

    fn derived_declared(
        dir: &Tmp,
        homes: &[(&str, &str)],
        macros: &[&str],
        substitutes: &[&str],
        extra: &[(&str, &str)],
        kernel: &[&str],
        declared: &[&str],
    ) -> Result<Shape> {
        let include = dir.0.join("bionic");
        let uapi = [dir.0.join("uapi")];
        let bionic_t = sysroot(&[include.clone(), uapi[0].clone()]);
        let host_t = sysroot(&[dir.0.join("host")]);
        let gnu = |mut t: Target| {
            t.args.push("-D_GNU_SOURCE".to_owned());
            t
        };
        let public = [
            "c.h",
            "d.h",
            "e.h",
            "f.h",
            "g.h",
            "l.h",
            "m.h",
            "n.h",
            "o.h",
            "q.h",
            "s.h",
            "sys/cdefs.h",
            "t.h",
            "w.h",
            "x.h",
            "y.h",
            "z.h",
        ];
        let bionic = front::probe_includes(Lang::C, &bionic_t, &public)?;
        let bionic_gnu = front::probe_includes(Lang::C, &gnu(bionic_t), &public)?;
        let names: Vec<String> = host_names(&[&bionic, &bionic_gnu], &include, &uapi, homes)
            .into_iter()
            .collect();
        let names: Vec<&str> = names.iter().map(String::as_str).collect();
        let host = front::probe_includes(Lang::C, &host_t, &names)?;
        let host_gnu =
            front::probe_includes(Lang::C, &gnu(sysroot(&[dir.0.join("host")])), &names)?;
        let cxx = front::probe_includes(Lang::Cxx, &gnu(sysroot(&[dir.0.join("host")])), &names)?;
        let scan = front::resolve_includes(Lang::C, &host_t, kernel)?;
        let kernel = front::resolve_includes(Lang::C, &sysroot(&uapi), kernel)?;
        derive(&Inputs {
            default: Probes {
                bionic: &bionic,
                host: &host,
            },
            gnu: Probes {
                bionic: &bionic_gnu,
                host: &host_gnu,
            },
            bionic_include: &include,
            uapi: &uapi,
            homes,
            macros,
            substitutes,
            declared,
            scan: &scan,
            kernel: &kernel,
            extra,
            cxx: &cxx,
        })
    }

    /// A substitute including a generated declaration header (bionic sys/_system_properties.h ->
    /// sys/system_properties.h): refused as bionic-only API unless that header is `declared`, then
    /// included; no other wrapper includes a declared header.
    #[test]
    fn substitute_includes_declared() {
        let dir = libcs("declared");
        let home = &[("bits/p.h", "q.h")];
        let forwarded = &["__mac", "__pm"];
        let refused = derived_with(&dir, home, forwarded, &["s.h"]).unwrap_err();
        assert!(refused.to_string().contains("bionic-only API"), "{refused}");
        let kernel = &["k/u.h", "k/v.h"];
        let s = derived_declared(&dir, home, forwarded, &["s.h"], &[], kernel, &["f.h"]).unwrap();
        assert_eq!(
            s.wrappers["s.h"],
            Wrapper {
                substitute: true,
                includes: always(&["f.h"]),
                ..Wrapper::default()
            }
        );
        let without = derived_with(&dir, home, forwarded, &[]).unwrap();
        assert_eq!(
            s.wrappers.len(),
            without.wrappers.len() + 1,
            "only the substitute is added"
        );
        for (name, w) in &without.wrappers {
            assert_eq!(&s.wrappers[name], w, "{name}");
        }
    }

    fn strings(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn cxx_linkage_only_in_includable_files() {
        let root = std::env::temp_dir().join(format!("atl-shape-linkage-{}", std::process::id()));
        let dir = Tmp(root.clone());
        for (rel, text) in [
            ("a.h", "#include <bits/b.h>\n"),
            ("bits/b.h", "int b_fn(void);\n"),
            ("c.h", "int c_fn(void);\n"),
        ] {
            let path = root.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        let t = sysroot(std::slice::from_ref(&dir.0));
        let cxx = front::probe_includes(Lang::Cxx, &t, &["c.h"]).unwrap();
        assert_eq!(
            cxx_linkage(&cxx).unwrap(),
            BTreeSet::from(["c.h".to_owned()])
        );
        // read as C, nothing has C++ linkage
        let c = front::probe_includes(Lang::C, &t, &["c.h"]).unwrap();
        assert!(cxx_linkage(&c).unwrap().is_empty());
        // a private file has no wrapper to put it in extern "C"
        let cxx = front::probe_includes(Lang::Cxx, &t, &["a.h"]).unwrap();
        let err = cxx_linkage(&cxx).unwrap_err().to_string();
        assert!(err.contains("bits/b.h") && err.contains("b_fn"), "{err}");
    }

    #[test]
    fn shape_of_fake_libcs() {
        let dir = libcs("derive");
        let with_home = derived(&dir, &[("bits/p.h", "q.h")]).unwrap();
        // the host's l.h declares l_fn with C++ linkage, bionic's with C linkage; m.h's are in linkage specifications, l.h's static one has internal linkage
        assert_eq!(
            with_home.wrappers["l.h"],
            Wrapper {
                c_linkage: true,
                ..Wrapper::default()
            }
        );
        assert!(!with_home.wrappers.contains_key("m.h"));
        // x.h directly, y.h through the private file, q.h as its home; sys/cdefs.h the host's has too;
        // the uapi header is not shape
        assert_eq!(
            with_home.wrappers["w.h"].includes,
            always(&["q.h", "x.h", "y.h"])
        );
        assert_eq!(
            with_home.wrappers["sys/cdefs.h"].defines,
            always(&["__mac __attribute__((unused))"])
        );
        assert_eq!(
            with_home.wrappers["k/u.h"].hidden,
            BTreeSet::from(["__mac".to_owned()])
        );
        // the host's t.h defines __tm itself (GNU view): not hidden, or its definition would be popped away
        assert!(
            with_home
                .wrappers
                .get("t.h")
                .is_none_or(|w| w.hidden.is_empty()),
            "{:?}",
            with_home.wrappers.get("t.h")
        );
        // q.h needs no home: its host version declares q itself
        assert_eq!(with_home.wrappers["q.h"].includes, always(&["y.h"]));
        // a private file's macro, as written, in every header whose walk enters it
        let pm = always(&["__pm(x) ((x) + \\\n  1)"]);
        assert_eq!(with_home.wrappers["w.h"].defines, pm);
        assert_eq!(with_home.wrappers["q.h"].defines, pm);
        // substitute o.h: its includes and its macros with the bionic-only one they name; e.h's macro
        // the host's k/u.h uses but defines the same way
        assert_eq!(
            with_home.wrappers["o.h"],
            Wrapper {
                substitute: true,
                includes: always(&["sys/cdefs.h", "x.h"]),
                defines: always(&["__oc(v) ((v) + 1)", "__om(v) __oc(v)"]),
                ..Wrapper::default()
            }
        );
        assert_eq!(with_home.wrappers["e.h"].defines, always(&["__kb"]));
        // a header including a substitute includes it, not walks it as private (bionic endian.h ->
        // sys/endian.h); not substituted, o.h is walked and only its includes reach n.h
        assert_eq!(with_home.wrappers["n.h"].includes, always(&["o.h"]));
        let unsubstituted =
            derived_with(&dir, &[("bits/p.h", "q.h")], &["__mac", "__pm"], &["e.h"]).unwrap();
        assert_eq!(
            unsubstituted.wrappers["n.h"].includes,
            always(&["sys/cdefs.h", "x.h"])
        );
        // views: g.h's includes of one view only keep bionic's condition; __gm, which only the
        // _GNU_SOURCE view defines, is forwarded under it
        assert_eq!(
            with_home.wrappers["g.h"],
            Wrapper {
                includes: vec![
                    (Guard::Always, "sys/cdefs.h".to_owned()),
                    (Guard::NotGnu, "y.h".to_owned()),
                    (Guard::Gnu, "x.h".to_owned()),
                ],
                defines: vec![(Guard::Gnu, "__gm 1".to_owned())],
                ..Wrapper::default()
            }
        );
        // unlisted bionic-only headers get nothing: the 9 shape wrappers (t.h's: __tm outside the GNU view) + l.h's linkage one
        assert_eq!(with_home.wrappers.len(), 10);
        let home = &[("bits/p.h", "q.h")];
        let forwarded = &["__mac", "__pm"];
        // refused substitutes: bionic-only API (f.h); a macro needing __mac, a k/u.h field name (c.h);
        // a macro k/u.h defines otherwise (d.h); one the host has (x.h); one bionic lacks
        for (s, why) in [
            ("f.h", "bionic-only API the host lacks"),
            ("c.h", "[\"__mac\"] as identifiers"),
            ("d.h", "[\"__kd\"] as identifiers"),
            ("x.h", "the host has <x.h>"),
            ("none.h", "bionic has no usable"),
        ] {
            let err = derived_with(&dir, home, forwarded, &[s])
                .err()
                .map(|e| e.to_string());
            assert!(
                err.as_ref().is_some_and(|e| e.contains(why)),
                "{s}: {err:?}"
            );
        }
        // a macro the host defines, or no bionic header brings in, is refused
        assert!(derived_with(&dir, home, &["q"], &[]).is_err());
        assert!(derived_with(&dir, home, &["__nowhere"], &[]).is_err());
        assert!(with_home.gaps.is_empty());

        // without the home: reported, with both host headers declaring q (one file each, by name);
        // p_only is declared by no host header, so it is not shape
        let without = derived(&dir, &[]).unwrap();
        assert_eq!(without.wrappers["w.h"].includes, always(&["x.h", "y.h"]));
        assert_eq!(
            without.gaps,
            [Gap {
                header: "w.h".into(),
                file: "bits/p.h".into(),
                names: strings(&["q"]),
                homes: strings(&["q.h", "z.h"]),
                guard: Guard::Always,
            }]
        );

        // a home must declare the names and must be needed
        assert!(derived(&dir, &[("bits/p.h", "x.h")]).is_err());
        assert!(derived(&dir, &[("bits/p.h", "q.h"), ("bits/none.h", "q.h")]).is_err());

        // extra includes: added to a wrapper with shape (w.h) or making one (z.h), sorted, once; only
        // in the view whose host header does not reach it (host z.h includes y.h with _GNU_SOURCE)
        let extra = |rows: &[(&str, &str)]| {
            derived_extra(&dir, home, forwarded, &[], rows).map_err(|e| e.to_string())
        };
        let with_extra = extra(&[("w.h", "z.h"), ("w.h", "x.h"), ("z.h", "y.h")]).unwrap();
        assert_eq!(
            with_extra.wrappers["w.h"].includes,
            always(&["q.h", "x.h", "y.h", "z.h"])
        );
        assert_eq!(
            with_extra.wrappers["z.h"].includes,
            [(Guard::NotGnu, "y.h".to_owned())]
        );
        // refused: the host's header already reaches it (host w.h includes sys/cdefs.h); no host header
        let e = extra(&[("w.h", "sys/cdefs.h")]).unwrap_err();
        assert!(e.contains("already reaches <sys/cdefs.h>"), "{e}");
        let e = extra(&[("o.h", "x.h")]).unwrap_err();
        assert!(e.contains("the host has no such header"), "{e}");

        // uapi: k/v.h (host lacks) forwarded to bionic's file, k/u.h (host has) never shadowed;
        // k/m.h refused: it defines __mac, which the host's k/u.h uses as a field name
        assert_eq!(
            with_home.uapi,
            BTreeMap::from([("k/v.h".to_owned(), dir.0.join("uapi/k/v.h"))])
        );
        let e = derived_kernel(&dir, home, forwarded, &[], &[], &["k/u.h", "k/m.h"])
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("<k/m.h>: host headers use its macros [\"__mac\"]"),
            "{e}"
        );
        assert_eq!(
            print_forward(Path::new("/b/uapi/k/v.h"), "uapi/k/v.h").unwrap(),
            "// generated by atl-bridgegen from uapi/k/v.h, do not edit\n#include \"/b/uapi/k/v.h\"\n"
        );
        assert!(print_forward(Path::new("k/v.h"), "x").is_err());
    }
}
