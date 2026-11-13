// SPDX-License-Identifier: MIT
//! `xtask check` step gl-proxies: the guest proxies (NBS `libnative_bridge_guest_*`, built by `xtask native --native-bridge`: the GL fronts and the NDK libraries of C3) against what an NDK app may call and what the host libraries serve. Static tier, on the slot's built files:
//! 1. every NDK name of an AOSP `<lib>.map.txt` (any `introduced` level; `# llndk` names are vendor-only, not NDK) is a defined dynamic symbol of the guest proxy, or an [`ABSENT`] entry;
//! 2. every stub of the binding table (atl-native-bridge engine `bindings::rows`) of a checked library is a defined dynamic symbol of its host library, unless the table traps it as unbound (no prototype in the EGL/GLES headers, or C++ internals);
//! 3. every [`ABSENT`] entry still holds: its map line names it, and neither the front nor the guest proxy exports it -- else the entry is stale (red).
//!
//! Skipped, naming why, when the slot has no guest proxies or fronts (`xtask native` without the nb guest set).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use atl_layout::Layout;
use atl_native_bridge::engine::bindings::{self, Binding};

/// The AOSP directory of the GL fronts' NDK symbol maps.
const MAPS: &str = "aosp/frameworks/native/opengl/libs";

/// An NDK map name that AOSP's own platform library does not define either, so neither front nor proxy exports it; [`check`] verifies each field.
struct Absent {
    /// the GL front (module = native goal)
    lib: &'static str,
    name: &'static str,
    /// the `<lib>.map.txt` line (1-based) that declares it: verified to name it
    map_line: usize,
    /// why AOSP's platform library lacks it, with the source evidence
    reason: &'static str,
}

const ABSENT: &[Absent] = &[Absent {
    lib: "libEGL",
    name: "eglCreateNativeClientBufferANDROID",
    map_line: 12,
    reason: "declared by the NDK map (introduced=24) but defined nowhere in frameworks/native/opengl/libs: only EGL/egl_entries.in:99 names it, the driver-dispatch table, not an export; the platform libEGL (our front, built from that source) lacks it",
}];

/// The defined dynamic symbols of `path`.
fn exports(path: &Path) -> Result<BTreeSet<String>> {
    use object::{Object, ObjectSymbol};
    let data = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let file =
        object::File::parse(&*data).with_context(|| format!("parsing {}", path.display()))?;
    Ok(file
        .dynamic_symbols()
        .filter(|s| !s.is_undefined())
        .filter_map(|s| s.name().ok().map(str::to_owned))
        .collect())
}

/// The NDK names of a `map.txt`: the `global:` entries, minus `# llndk` (vendor-only) and `# systemapi` (system-only) ones, which AOSP's stub generator omits from the NDK surface (build/soong/cc/symbolfile `_should_omit_tags`). `# var` marks a data symbol (libmediandk's `AMEDIA*_KEY_*`): still NDK surface, the stub generator emits it as a variable. A tag besides `introduced*` / `llndk` / `systemapi` / `var` fails: a new annotation must be classified, not guessed.
fn ndk_names(map: &str) -> Result<Vec<(usize, String)>> {
    let mut out = Vec::new();
    for (i, line) in map.lines().enumerate() {
        let line = line.trim();
        let (entry, tags) = line.split_once('#').unwrap_or((line, ""));
        let Some(name) = entry.trim().strip_suffix(';') else {
            continue;
        };
        // `global:` / `local:` / `*;` / `};` are structure, not names
        if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            continue;
        }
        let tags: Vec<&str> = tags.split_whitespace().collect();
        if let Some(t) = tags
            .iter()
            .find(|t| !t.starts_with("introduced") && !matches!(**t, "llndk" | "systemapi" | "var"))
        {
            bail!("map line {}: unclassified tag {t} on {name}", i + 1);
        }
        if !tags.iter().any(|t| matches!(*t, "llndk" | "systemapi")) {
            out.push((i + 1, name.to_owned()));
        }
    }
    Ok(out)
}

/// The guest proxy of GL front `lib`, as `xtask native --native-bridge <arch>` builds it.
fn guest_proxy(out: &Path, arch: &str, lib: &str) -> PathBuf {
    crate::native::nb_module_dir(out, arch, &format!("libnative_bridge_guest_{lib}"))
        .join(format!("{lib}.so"))
}

/// The NDK libraries whose guest proxies this step checks beside the GL fronts (C3): their NDK maps are AOSP's `frameworks/base/native`.
use crate::gate_goals::NDK_PROXIED as NDK_LIBS;

/// The libraries checked (gate_goals `gl_proxied`): each has a guest proxy and a host library at `gl_front` (the GL fronts, then the NDK libraries).
fn checked_libs() -> Vec<&'static str> {
    crate::gate_goals::gl_proxied()
}

/// The AOSP directory of `lib`'s NDK symbol map: the GL fronts' (`MAPS`) or the NDK libraries' own.
fn map_dir(lib: &str) -> &'static str {
    match lib {
        "libandroid" => "aosp/frameworks/base/native/android",
        "libjnigraphics" => "aosp/frameworks/base/native/graphics/jni",
        "libmediandk" => "aosp/frameworks/av/media/ndk",
        _ => MAPS,
    }
}

/// Runs the step on slot `out` for guest arch `arch` (the identity engine: the host's); `Some(why)` when the slot lacks the inputs.
pub fn check(root: &Path, out: &Path, arch: &str) -> Result<Option<String>> {
    let libs = checked_libs();
    let missing: Vec<String> = libs
        .iter()
        .flat_map(|l| [guest_proxy(out, arch, l), crate::atld_run::gl_front(out, l)])
        .filter(|p| !p.is_file())
        .map(|p| p.display().to_string())
        .collect();
    if !missing.is_empty() {
        return Ok(Some(format!(
            "no {}: build the checked libraries {libs:?} and nb:libnative_bridge_guest_<lib> first",
            missing.join(", ")
        )));
    }
    let layout = Layout::of(root)?;
    let mut problems = Vec::new();
    let mut counted = 0usize;
    for &lib in &libs {
        let guest = exports(&guest_proxy(out, arch, lib))?;
        let front = exports(&crate::atld_run::gl_front(out, lib))?;
        let maps = layout.locate(Path::new(map_dir(lib))).dir;
        let map_path = maps.join(format!("{lib}.map.txt"));
        let map =
            std::fs::read_to_string(&map_path).with_context(|| map_path.display().to_string())?;
        let names = ndk_names(&map).with_context(|| map_path.display().to_string())?;
        let absent: Vec<&Absent> = ABSENT.iter().filter(|a| a.lib == lib).collect();
        // 1. NDK map ⊆ guest exports ∪ ABSENT
        for (_, name) in &names {
            if !guest.contains(name) && !absent.iter().any(|a| a.name == name) {
                problems.push(format!(
                    "{lib}: NDK name {name} ({}) is not exported by the guest proxy",
                    map_path.display()
                ));
            }
        }
        // 2. table stubs ⊆ front exports ∪ unbound traps
        let soname = format!("{lib}.so");
        for (_, name, b) in bindings::rows().filter(|(l, ..)| *l == soname) {
            counted += 1;
            let unbound = matches!(b, Binding::Trap(why) if why.starts_with("unbound"));
            if !unbound && !front.contains(name) {
                problems.push(format!(
                    "{lib}: stub {name} ({b:?}) is not exported by the host front"
                ));
            }
        }
        // 3. ABSENT entries still hold
        for a in absent {
            if !names
                .iter()
                .any(|(line, n)| *line == a.map_line && n == a.name)
            {
                problems.push(format!(
                    "{lib}: ABSENT {}: {}:{} does not declare it",
                    a.name,
                    map_path.display(),
                    a.map_line
                ));
            }
            for (side, set) in [("host front", &front), ("guest proxy", &guest)] {
                if set.contains(a.name) {
                    problems.push(format!(
                        "{lib}: ABSENT {} is stale: the {side} exports it ({})",
                        a.name, a.reason
                    ));
                }
            }
        }
    }
    if !problems.is_empty() {
        bail!("{} problem(s): {}", problems.len(), problems.join(" | "));
    }
    // the NDK proxies' binding classes: a pending stub has no prototype in the set yet (build.rs `missing_prototype`)
    let split: Vec<String> = NDK_LIBS
        .iter()
        .map(|lib| {
            let soname = format!("{lib}.so");
            let (mut bound, mut pending, mut unbound) = (0usize, 0usize, 0usize);
            for (_, _, b) in bindings::rows().filter(|(l, ..)| *l == soname) {
                match b {
                    Binding::Generic { .. }
                    | Binding::Shim
                    | Binding::Env { .. }
                    | Binding::Variable { .. } => bound += 1,
                    Binding::Trap(why) if why.starts_with("pending") => pending += 1,
                    Binding::Trap(_) => unbound += 1,
                }
            }
            format!("{lib} {bound} bound, {pending} pending, {unbound} unbound")
        })
        .collect();
    eprintln!(
        "gl-proxies: ok ({} libraries: NDK maps in the guest proxies, {counted} table stubs in the fronts, {} ABSENT entries hold; NDK stubs: {})",
        libs.len(),
        ABSENT.len(),
        split.join(", ")
    );
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn map_names_skip_llndk_and_systemapi_and_refuse_unknown_tags() {
        let map = "LIBEGL {\n  global:\n    eglA;\n    eglB; # introduced=29\n    eglC; # llndk\n    eglD; # systemapi introduced=31\n    eglV; # var introduced=31\n  local:\n    *;\n};\n";
        let names: Vec<String> = ndk_names(map)
            .unwrap()
            .into_iter()
            .map(|(_, n)| n)
            .collect();
        assert_eq!(names, ["eglA", "eglB", "eglV"]);
        assert!(ndk_names("    eglE; # bogus\n").is_err());
    }
}
