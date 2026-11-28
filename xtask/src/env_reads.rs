// SPDX-License-Identifier: MIT
//! `check` step `env-reads`: product code reads no environment-variable switch (AGENTS Design: no env switches, no test-only product paths).
//! A read = `env::var(` / `env::var_os(` / `env::vars(` / `env::vars_os(` / `env!(` / `option_env!(` in a crate's `src/` or `build.rs`, before its `#[cfg(test)]`; its name is the literal, or the `const` it names resolved (same file first, else the one workspace const of that name), else the argument text.
//! Cargo's own build/test variables (`CARGO_*`, `OUT_DIR`, `TARGET`, ...) pass unlisted. Every other read is listed in [`ALLOWED`] with its class and reason; an entry no read matches fails, so the list cannot rot.
//! Deliberate breaks and forced tiers are cargo features `break-<name>` / `tier-<name>` instead, enabled only by the harness on the command line: no Cargo.toml may enable one from another feature or a dependency spec.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::spdx;

/// What an allowed read is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Class {
    /// a Linux desktop convention a native program reads (XDG, HOME, Wayland, PipeWire, fontconfig, locale, Khronos loader), inside a platform or host-side crate
    Platform,
    /// a value one of our processes hands the next one it starts (an inherited fd number, a mapped file), set by the parent that owns the decision: configuration, not a switch
    Handoff,
    /// a build input path xtask passes a build script (a prebuilt tree to link, a generated object), read at build time only
    BuildInput,
    /// a deliberate-break selector still read from the environment (class c): to become a cargo feature; delete the row in that change
    Pending,
}

/// (file, variable name or the unresolved argument text, class, why).
const ALLOWED: &[(&str, &str, Class, &str)] = &[
    (
        "crates/atl-platform-host-linux/src/timezone.rs",
        "TZ",
        Class::Platform,
        "POSIX TZ: the host's zone as glibc reads it",
    ),
    (
        "crates/atl-platform-host-linux/src/app_access.rs",
        "WAYLAND_DISPLAY",
        Class::Platform,
        "the compositor socket an app process gets (libwayland's rule)",
    ),
    (
        "crates/atl-platform-host-linux/src/app_access.rs",
        "XDG_RUNTIME_DIR",
        Class::Platform,
        "base dir of the Wayland and PipeWire sockets (XDG base dirs)",
    ),
    (
        "crates/atl-platform-host-linux/src/app_access.rs",
        "PIPEWIRE_RUNTIME_DIR",
        Class::Platform,
        "libpipewire's socket dir override",
    ),
    (
        "crates/atl-platform-host-linux/src/app_access.rs",
        "PIPEWIRE_REMOTE",
        Class::Platform,
        "libpipewire's remote name",
    ),
    (
        "crates/atl-platform-host-linux/src/app_access.rs",
        "FONTCONFIG_FILE",
        Class::Platform,
        "fontconfig's config file override",
    ),
    (
        "crates/atl-platform-host-linux/src/app_access.rs",
        "PULSE_SERVER",
        Class::Platform,
        "libpulse's server, the web process's share-sound socket (libpulse's rule)",
    ),
    (
        "crates/atl-platform-host-linux/src/data_dir.rs",
        "XDG_DATA_HOME",
        Class::Platform,
        "where per-user app data lives (XDG base dirs)",
    ),
    (
        "crates/atl-platform-host-linux/src/data_dir.rs",
        "HOME",
        Class::Platform,
        "XDG_DATA_HOME's default base",
    ),
    (
        "crates/atl-platform-host-linux/src/locale.rs",
        "name",
        Class::Platform,
        "POSIX locale lookup LC_ALL, LC_MESSAGES, LANG (atl-platform-host-api ui_locale_from)",
    ),
    (
        "crates/atl-vulkan-driver/src/icd.rs",
        "name",
        Class::Platform,
        "the Khronos loader's Linux manifest search: HOME, XDG_CONFIG_*, XDG_DATA_*",
    ),
    (
        "crates/atl-broker/src/lib.rs",
        "PATH",
        Class::Platform,
        "finding host helpers (bwrap) as exec does",
    ),
    (
        "crates/atl-broker/src/lib.rs",
        "k",
        Class::Platform,
        "the fixed host variables an app process inherits (app_env.rs APP_ENV)",
    ),
    (
        "crates/atl-binder-driver/src/exports.rs",
        "ATL_BINDER_FD",
        Class::Handoff,
        "the binder control fd the broker opened for this process (as systemd's LISTEN_FDS)",
    ),
    (
        "crates/atl-flatpak-spawn/src/main.rs",
        "ATL_WEBSPAWN_FD",
        Class::Handoff,
        "the web-process channel fd atld hands the spawner",
    ),
    (
        "crates/atl-bionic/src/store.rs",
        "ATL_PROP_AREA",
        Class::Handoff,
        "the property area file atld maps for the process until the sandbox mounts it",
    ),
    (
        "crates/atl-sandbox-probe/src/main.rs",
        "",
        Class::Platform,
        "test-environment probe: lists the names it inherited to prove the sandbox's scrub; no product binary",
    ),
    (
        "crates/atl-app-launcher/build.rs",
        "ATL_NATIVE_OUT",
        Class::BuildInput,
        "the native out dir whose converter libraries the launcher links (xtask packages.rs)",
    ),
    (
        "crates/atl-bridgegen/src/converter.rs",
        "ATL_NATIVE_OUT",
        Class::BuildInput,
        "the native out dir whose converter libraries a package links (xtask packages.rs)",
    ),
    (
        "crates/atl-qemu-engine/build.rs",
        "ATL_QEMU_BUILD",
        Class::BuildInput,
        "the QEMU build tree whose layouts the engine reads",
    ),
    (
        "crates/atl-qemu-engine/build.rs",
        "ATL_QEMU_SIGCHAIN_OBJ",
        Class::BuildInput,
        "AOSP libsigchain's object, linked into the spike test only (link-arg-tests)",
    ),
    (
        "crates/atl-native-bridge/build.rs",
        "ATL_NB_GL_GUEST_INCLUDE",
        Class::Pending,
        "identity gate's force-included guest header",
    ),
    (
        "crates/atl-qemu-engine/build.rs",
        "ATL_QEMU_ENGINE_BREAK",
        Class::Pending,
        "qemu-spike gate breaks",
    ),
    (
        "crates/atl-bionic/build.rs",
        "ATL_BIONIC_BREAK",
        Class::Pending,
        "bionic generation's overrides break",
    ),
];

/// Variables cargo itself sets for build scripts and tests (The Cargo Book, "Environment variables Cargo sets for build scripts"); `CARGO_*` by prefix.
const CARGO_VARS: [&str; 8] = [
    "OUT_DIR",
    "TARGET",
    "HOST",
    "PROFILE",
    "OPT_LEVEL",
    "DEBUG",
    "NUM_JOBS",
    "RUSTC",
];

/// The read calls, matched by their text.
const CALLS: [&str; 6] = [
    "env::var(",
    "env::var_os(",
    "env::vars(",
    "env::vars_os(",
    "option_env!(",
    "env!(",
];

pub fn check(root: &Path) -> Result<()> {
    let files: Vec<String> = spdx::candidate_files(root)?
        .into_iter()
        .filter(|f| {
            f.starts_with("crates/")
                && f.ends_with(".rs")
                && (f.contains("/src/") || f.ends_with("/build.rs"))
        })
        .collect();
    let mut texts = BTreeMap::new();
    for rel in &files {
        let text =
            std::fs::read_to_string(root.join(rel)).with_context(|| format!("reading {rel}"))?;
        texts.insert(rel.clone(), text);
    }
    let consts = workspace_consts(&texts);
    let mut found = Vec::new();
    for (rel, text) in &texts {
        for (line, arg) in reads(text) {
            found.push((rel.clone(), line, name_of(&arg, text, &consts)));
        }
    }
    let mut bad = judge(&found, ALLOWED);
    for rel in spdx::candidate_files(root)?
        .into_iter()
        .filter(|f| f.ends_with("Cargo.toml"))
    {
        let text =
            std::fs::read_to_string(root.join(&rel)).with_context(|| format!("reading {rel}"))?;
        bad.extend(
            feature_enables(&text)
                .into_iter()
                .map(|(n, l)| format!("{rel}:{n}: enables a break/tier feature: {l}")),
        );
    }
    if bad.is_empty() {
        let pending = ALLOWED
            .iter()
            .filter(|(_, _, c, _)| *c == Class::Pending)
            .count();
        eprintln!(
            "env-reads: ok ({} files, {} reads, {} listed, {pending} pending break selectors)",
            files.len(),
            found.len(),
            ALLOWED.len()
        );
        return Ok(());
    }
    for b in &bad {
        eprintln!("env-reads: {b}");
    }
    bail!(
        "{} unlisted environment reads, stale entries or enabled break features: a test switch becomes a cargo feature `break-<name>` the harness enables; a platform convention or handoff is listed in env_reads.rs ALLOWED with its reason",
        bad.len()
    );
}

/// Each read not listed and not cargo's (`rel:line: NAME`), and each listed entry no read matches (stale).
fn judge(found: &[(String, usize, String)], listed: &[(&str, &str, Class, &str)]) -> Vec<String> {
    let covers = |(f, n, _, _): &(&str, &str, Class, &str),
                  (rel, _, name): &(String, usize, String)| {
        rel == f && name == n
    };
    let mut bad: Vec<String> = found
        .iter()
        .filter(|r| !is_cargo(&r.2) && !listed.iter().any(|l| covers(l, r)))
        .map(|(rel, n, name)| format!("{rel}:{n}: reads {name}"))
        .collect();
    bad.extend(
        listed
            .iter()
            .filter(|l| !found.iter().any(|r| covers(l, r)))
            .map(|(f, n, _, _)| format!("stale ALLOWED entry {f} {n}: no read matches")),
    );
    bad
}

/// Whether `name` is a variable cargo sets (a literal `CARGO_` prefix also inside a `format!`).
fn is_cargo(name: &str) -> bool {
    name.starts_with("CARGO_") || name.contains("\"CARGO_") || CARGO_VARS.contains(&name)
}

/// (line, argument text) of every read before the file's `#[cfg(test)]`, comment lines skipped.
fn reads(text: &str) -> Vec<(usize, String)> {
    let end = text.find("#[cfg(test)]").unwrap_or(text.len());
    let src = &text[..end];
    let mut out = Vec::new();
    for call in CALLS {
        for (i, _) in src.match_indices(call) {
            let before = src[..i].chars().next_back();
            // `env!(` inside `option_env!(`, or `env::var(` inside `my_env::var(`
            if before.is_some_and(|c| c.is_alphanumeric() || c == '_') {
                continue;
            }
            let start = src[..i].rfind('\n').map_or(0, |p| p + 1);
            if src[start..i].contains("//") {
                continue;
            }
            let line = src[..i].matches('\n').count() + 1;
            out.push((line, argument(&src[i + call.len()..])));
        }
    }
    out.sort();
    out
}

/// The text up to the `)` that closes a call whose `(` precedes `rest`, trimmed.
fn argument(rest: &str) -> String {
    let mut depth = 1usize;
    for (i, c) in rest.char_indices() {
        match c {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => {
                depth -= 1;
                if depth == 0 {
                    return rest[..i].trim().to_owned();
                }
            }
            _ => {}
        }
    }
    rest.trim().to_owned()
}

/// The variable an argument names: a plain literal's value, a const path's value (this file's const, else the one workspace const), else the text itself.
fn name_of(arg: &str, text: &str, consts: &BTreeMap<String, Vec<String>>) -> String {
    if let Some(lit) = arg
        .strip_prefix('"')
        .and_then(|a| a.strip_suffix('"'))
        .filter(|lit| !lit.contains('"'))
    {
        return lit.to_owned();
    }
    let ident = arg.rsplit("::").next().unwrap_or(arg);
    if ident
        .chars()
        .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
        && !ident.is_empty()
    {
        if let Some(v) = const_in(text, ident) {
            return v;
        }
        if let Some([v]) = consts.get(ident).map(Vec::as_slice) {
            return v.clone();
        }
    }
    arg.to_owned()
}

/// Every `const NAME: &str = "value";` in the workspace sources, by name (a name defined twice with different values stays ambiguous).
fn workspace_consts(texts: &BTreeMap<String, String>) -> BTreeMap<String, Vec<String>> {
    let mut map: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for text in texts.values() {
        for (name, value) in str_consts(text) {
            let values = map.entry(name).or_default();
            if !values.contains(&value) {
                values.push(value);
            }
        }
    }
    map
}

/// The value of `const <name>: &str = "...";` in `text`.
fn const_in(text: &str, name: &str) -> Option<String> {
    str_consts(text)
        .into_iter()
        .find_map(|(n, v)| (n == name).then_some(v))
}

/// (name, value) of each one-line `const NAME: &str = "value";`.
fn str_consts(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|l| {
            let rest = l.trim_start();
            let rest = rest.split_once("const ")?.1;
            let (name, rest) = rest.split_once(": &str = \"")?;
            let (value, _) = rest.split_once("\";")?;
            Some((name.trim().to_owned(), value.to_owned()))
        })
        .collect()
}

/// (line, text) of each Cargo.toml line that enables a `break-*` / `tier-*` feature: a quoted name in another feature's list or a dependency's `features`. A feature's own definition (`break-x = []`) is not one.
fn feature_enables(text: &str) -> Vec<(usize, String)> {
    text.lines()
        .enumerate()
        .filter(|(_, l)| {
            let l = l.split('#').next().unwrap_or("");
            ["\"break-", "/break-", "\"tier-", "/tier-"]
                .iter()
                .any(|p| l.contains(p))
        })
        .map(|(i, l)| (i + 1, l.trim().to_owned()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A switch read through a const is named by the const's value, and an unlisted one is reported; cargo's own variable passes.
    #[test]
    fn unlisted_switch_is_reported_cargo_var_passes() {
        let text = "const BREAK_ENV: &str = \"ATL_X_BREAK\";\nfn f() { std::env::var(BREAK_ENV); std::env::var_os(\"OUT_DIR\"); }\n";
        let found: Vec<_> = reads(text)
            .into_iter()
            .map(|(n, a)| {
                (
                    "crates/x/src/lib.rs".to_owned(),
                    n,
                    name_of(&a, text, &BTreeMap::new()),
                )
            })
            .collect();
        assert_eq!(
            judge(&found, &[]),
            ["crates/x/src/lib.rs:2: reads ATL_X_BREAK"]
        );
    }

    /// Reads after `#[cfg(test)]`, in comments, or under another path (`my_env::var`) are not product reads.
    #[test]
    fn test_module_comment_and_foreign_path_are_skipped() {
        let text = "// std::env::var(\"A\")\nfn f() { my_env::var(\"B\"); }\n#[cfg(test)]\nmod tests { fn g() { std::env::var(\"C\"); } }\n";
        assert!(reads(text).is_empty());
    }

    /// An entry whose read is gone is stale.
    #[test]
    fn stale_entry_is_reported() {
        let listed = [("crates/x/src/lib.rs", "HOME", Class::Platform, "r")];
        assert_eq!(
            judge(&[], &listed),
            ["stale ALLOWED entry crates/x/src/lib.rs HOME: no read matches"]
        );
    }

    /// A feature list or dependency enabling a break is caught; the break's own definition is not.
    #[test]
    fn enabling_a_break_feature_is_caught() {
        let toml = "[features]\nbreak-z = []\ndefault = [\"break-z\"]\n[dependencies]\nfoo = { path = \"x\", features = [\"tier-blit\"] }\nbar = [\"atl-composer/break-q\"]\n";
        let lines: Vec<usize> = feature_enables(toml).into_iter().map(|(n, _)| n).collect();
        assert_eq!(lines, [3, 5, 6]);
    }
}
