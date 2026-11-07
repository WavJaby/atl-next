// SPDX-License-Identifier: MIT
//! The native goals each runtime gate reads, declared once in the gate's own module (`pub fn goals() -> Goals`) and listed by name in [`GATES`].
//! `xtask native --for <gate>[,<gate>]` builds their union; a gate's [`crate::build_state::gate_out`] refuses a slot whose record lacks one (built-from state + goals, build_state.rs),
//! naming the missing goals and the `--for` command, so no gate discovers a missing input mid-run. The native-bridge guest is part of the same record (`nb:` goals), so stale `<out>/nb` is refused the same way.

use std::collections::BTreeSet;

use anyhow::{Result, bail};

/// `--for` value meaning every gate in [`GATES`].
pub const ALL: &str = "all-gates";

/// Native goals: host modules, native-bridge (`nb:`) modules built for the host's own arch (the identity engine's guest, atld_run.rs `with_native_bridge_guest`), and the Codec2 store's libraries (`store`, a symbolic group: [`of`] and [`union`] expand it to names from the store source, codec2_store.rs).
#[derive(Default, Debug, Clone, PartialEq)]
pub struct Goals {
    pub host: BTreeSet<String>,
    pub nb: BTreeSet<String>,
    /// Unexpanded until [`resolve`] adds [`crate::codec2_store::libraries`] to `host` and clears it.
    pub store: bool,
}

impl Goals {
    pub fn host(names: &[&str]) -> Goals {
        Goals {
            host: names.iter().map(|s| s.to_string()).collect(),
            ..Goals::default()
        }
    }

    pub fn nb(names: &[&str]) -> Goals {
        Goals {
            nb: names.iter().map(|s| s.to_string()).collect(),
            ..Goals::default()
        }
    }

    /// The Codec2 store's libraries: the list its constructor dlopens (codec2_store.rs), so every process that can decode builds them.
    pub fn codec_store() -> Goals {
        Goals {
            store: true,
            ..Goals::default()
        }
    }

    pub fn with(mut self, other: Goals) -> Goals {
        self.host.extend(other.host);
        self.nb.extend(other.nb);
        self.store |= other.store;
        self
    }

    /// As the build record names them: `<module>` and `nb:<arch>:<module>`.
    pub fn recorded(&self, host_arch: &str) -> BTreeSet<String> {
        let mut r = self.host.clone();
        r.extend(self.nb.iter().map(|m| nb_key(host_arch, m)));
        r
    }
}

/// The record's name of nb goal `module` built with `--native-bridge <arch>`.
pub fn nb_key(arch: &str, module: &str) -> String {
    format!("nb:{arch}:{module}")
}

/// A gate module's `goals`.
type GoalsOf = fn() -> Goals;

/// Every gate that reads a native out dir: `xtask test <name>`, `xtask run-apk`, and the `xtask check` steps that read built files.
pub const GATES: &[(&str, GoalsOf)] = &[
    ("app", crate::app::goals),
    ("audio-app", crate::audio_app::goals),
    ("binder", crate::binder::goals),
    ("broadcast", crate::broadcast::goals),
    ("provider", crate::provider::goals),
    ("composer", crate::composer::goals),
    ("compositor", crate::compositor::goals),
    ("drive", crate::drive_gate::goals),
    ("cts", crate::cts::goals),
    ("gl", crate::gl_probe::goals),
    ("gl-dispatch", crate::gl_dispatch::goals),
    ("gl-proxies", crate::gl_proxies::goals),
    ("gles2-fixture", crate::gles2_fixture::goals),
    ("gralloc", crate::gralloc::goals),
    ("instrument", crate::instrument::goals),
    ("llvm", crate::llvm_host::goals),
    ("native-load", crate::native_load::goals),
    ("parcel", crate::parcel::goals),
    ("run-apk", crate::run_apk::goals),
    ("sandbox", crate::sandbox::goals),
    ("sensors", crate::sensors::goals),
    ("window", crate::window::goals),
];

/// The goals of gate `name`, expanded ([`resolve`]).
pub fn of(name: &str) -> Result<Goals> {
    resolve(declared(name)?)
}

/// The goals gate `name` declares, not yet expanded.
fn declared(name: &str) -> Result<Goals> {
    match GATES.iter().find(|(n, _)| *n == name) {
        Some((_, f)) => Ok(f()),
        None => bail!(
            "no gate `{name}` declares native goals (gate_goals.rs GATES: {}, or `{ALL}`)",
            names()
        ),
    }
}

/// The union of `names`' goals (`all-gates` = every gate).
pub fn union(names: &[String]) -> Result<Goals> {
    let mut g = Goals::default();
    for name in names {
        if name == ALL {
            for (_, f) in GATES {
                g = g.with(f());
            }
        } else {
            g = g.with(declared(name)?);
        }
    }
    resolve(g)
}

/// `g` with its symbolic groups expanded: the store's libraries, from the store source of this checkout (tree::build_file).
fn resolve(mut g: Goals) -> Result<Goals> {
    if std::mem::take(&mut g.store) {
        g.host
            .extend(crate::codec2_store::libraries(&crate::repo_root()?)?);
    }
    Ok(g)
}

fn names() -> String {
    GATES.iter().map(|(n, _)| *n).collect::<Vec<_>>().join(", ")
}

/// Refuses unless `built` (the record's goals) holds all of gate `name`'s, naming the missing ones and the command that builds them.
pub fn ensure_built(name: &str, host_arch: &str, built: &BTreeSet<String>) -> Result<()> {
    let missing: Vec<String> = of(name)?
        .recorded(host_arch)
        .into_iter()
        .filter(|g| !built.contains(g))
        .collect();
    if !missing.is_empty() {
        bail!(
            "gate `{name}`: the slot was not built with native goal(s) {}: run `cargo xtask native --host {host_arch} --for {name}`",
            missing.join(" ")
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_gate_resolves_and_names_are_unique() {
        let mut seen = BTreeSet::new();
        for (n, f) in GATES {
            assert!(seen.insert(*n), "{n} twice");
            f();
        }
        assert!(
            union(&[ALL.to_owned()])
                .unwrap()
                .host
                .contains("servicemanager")
        );
        assert!(of("nope").is_err());
    }

    #[test]
    fn missing_goal_is_named() {
        let built: BTreeSet<String> = ["servicemanager".to_owned()].into();
        let e = ensure_built("binder", "x86_64", &built)
            .unwrap_err()
            .to_string();
        // binder needs `service` too (binder.rs goals)
        assert!(e.contains(" service") && e.contains("--for binder"), "{e}");
        let app = of("app").unwrap();
        let nb = app.nb.iter().next().expect("app needs the nb guest");
        let e = ensure_built("app", "x86_64", &app.host)
            .unwrap_err()
            .to_string();
        assert!(e.contains(&nb_key("x86_64", nb)), "{e}");
        assert!(ensure_built("app", "x86_64", &app.recorded("x86_64")).is_ok());
    }

    #[test]
    fn store_libraries_reach_every_runtime_gate() {
        // the store's own list (C2Store.cpp) is the expectation; no literal names
        let root = crate::repo_root().expect("repo root");
        let libs = crate::codec2_store::libraries(&root).expect("store list");
        assert!(!libs.is_empty());
        for gate in ["app", "native-load", "run-apk"] {
            let g = of(gate).expect(gate);
            assert!(!g.store, "{gate}: store not expanded");
            assert!(libs.iter().all(|m| g.host.contains(m)), "{gate}");
        }
    }
}
