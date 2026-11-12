// SPDX-License-Identifier: MIT
//! The native goals each runtime gate reads, declared once here by gate name in [`GATES`] (the gates themselves live in xtask-harness; `native --for` must not link them).
//! `xtask native --for <gate>[,<gate>]` builds their union; a gate's [`crate::build_state::gate_out`] refuses a slot whose record lacks one (built-from state + goals, build_state.rs),
//! naming the missing goals and the `--for` command, so no gate discovers a missing input mid-run. The native-bridge guest is part of the same record (`nb:` goals), so stale `<out>/nb` is refused the same way.

use std::collections::BTreeSet;
use std::path::Path;

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

    /// As the converter's argument file names them (native.rs `CONVERTER_ARGS`): `<module>`, then `nb:<module>`.
    pub fn converter(&self) -> Vec<String> {
        self.host
            .iter()
            .cloned()
            .chain(
                self.nb
                    .iter()
                    .map(|m| format!("{}{m}", crate::native::NB_GOAL)),
            )
            .collect()
    }
}

/// Every gate's goals (`native --for all-gates`), as the converter's argument file names them: the set a slot's graph must cover for keepset to read it (keepset.rs).
pub fn all_converter(root: &Path) -> Result<BTreeSet<String>> {
    Ok(union(root, &[ALL.to_owned()])?
        .converter()
        .into_iter()
        .collect())
}

/// The record's name of nb goal `module` built with `--native-bridge <arch>`.
pub fn nb_key(arch: &str, module: &str) -> String {
    format!("nb:{arch}:{module}")
}

/// AOSP Loader.cpp `load_wrapper`'s libraries: the GL fronts the converter builds; what atld_run's `with_gl_driver` places and the `gl-proxies` check reads.
pub const GL_FRONTS: [&str; 4] = ["libEGL", "libGLESv1_CM", "libGLESv2", "libGLESv3"];
/// ANGLE's three libraries: AOSP's Loader dlopens them by name in the default namespace when an app's GL selection is ANGLE (Loader.cpp `load_angle`), so they are placed beside the fronts.
pub const ANGLE_FRONTS: [&str; 3] = ["libEGL_angle", "libGLESv1_CM_angle", "libGLESv2_angle"];
/// The extractor plugins atld_run's `platform_rows` places: every atld run reads them, so [`runtime`] builds them.
pub const EXTRACTOR_PLUGINS: [&str; 2] = ["liboggextractor", "libwavextractor"];
/// Framework JNI libraries an app loads by name (`System.loadLibrary`): libsoundpool (SoundPool.java:119) and libmedia_jni (MediaPlayer.java:623), built as AOSP's source as-is. Their closures build with them; stage_art links every built `device/<module>/*.so` into the ART lib dir, which atld's LD_LIBRARY_PATH names, so no placement row is needed.
pub const FRAMEWORK_JNI: [&str; 2] = ["libsoundpool", "libmedia_jni"];
/// The native-bridge guest's linker and main executable (NBS modules; `app_process64` = argv[1] of the guest linker, atl-native-bridge engine/mod.rs).
pub const NB_LINKER: &str = "native_bridge_guest_linker";
pub const NB_EXE: &str = "native_bridge_guest_app_process";
/// The guest libraries `app_process64`'s closure needs (its and their `DT_NEEDED`), by module: NBS's guest libc and linker helpers, AOSP's guest libcutils / libutils / liblog (NBS `NATIVE_BRIDGE_ORIG_GUEST_LIBS`), the GL / window proxies (`libnative_bridge_guest_*`, installed under their stems) that route GL calls to the host fronts (GS2), AOSP's guest zlib and libstdc++ (C12: DT_NEEDED of the legacy fixture, not of app_process), and the NDK proxies libandroid, libandroid_runtime and libjnigraphics (C3: DT_NEEDED of the fixtures' asset and bitmap calls), and the vdso. libc++.so comes from the runtimes build (runtimes.rs `shared_libcxx`). Installed file names: atld_run `nb_lib_file`.
pub const NB_GUEST_LIBS: [&str; 21] = [
    "libnative_bridge_guest_libc",
    "libm",
    "libdl",
    "ld-android",
    "liblog",
    "libbase",
    "libvndksupport",
    "libdl_android",
    "libcutils",
    "libutils",
    "libz",
    "libstdc++",
    "libnative_bridge_guest_libEGL",
    "libnative_bridge_guest_libGLESv1_CM",
    "libnative_bridge_guest_libGLESv2",
    "libnative_bridge_guest_libGLESv3",
    "libnative_bridge_guest_libnativewindow",
    "libnative_bridge_guest_libandroid",
    "libnative_bridge_guest_libandroid_runtime",
    "libnative_bridge_guest_libjnigraphics",
    "libnative_bridge_vdso",
];

/// What every atld run (atld_run `Runtime`) reads: ART and its boot jars' JNI libraries, servicemanager, framework-res, the converter crates the service packages link (packages.rs), and the Codec2 store's libraries (codec2_store.rs: its constructor dlopens each one in every app process that decodes).
pub fn runtime() -> Goals {
    Goals::host(&[
        "servicemanager",
        "framework-res",
        "platform-bootclasspath",
        // the APEX jars' JNI libraries (libframework-connectivity-tiramisu-jni: TrafficStats' static init aborts ZygoteInit without it)
        "platform-bootclasspath-jni",
        "libart",
        "libnativehelper",
        "libjavacore",
        "libopenjdk",
        "libicu_jni",
        "libandroid_runtime",
        "libandroid",
        "libjnigraphics",
        "libsigchain",
        "libbinder",
        "libutils",
        "libinput",
        "libbinder_rs",
        "libatl_parcelables",
        "atl-aidl-stubs-rust",
        "libgui_aidl_types_rs",
        "librustutils",
    ])
    .with(Goals::host(&EXTRACTOR_PLUGINS))
    .with(Goals::host(&FRAMEWORK_JNI))
    .with(Goals::codec_store())
}

/// atld_run `with_gralloc` + `with_gl_driver`: the allocator HAL's AIDL crate and the GL fronts.
pub fn gralloc() -> Goals {
    Goals::host(&GL_FRONTS)
        .with(Goals::host(&ANGLE_FRONTS))
        .with(Goals::host(&[
            "android.hardware.graphics.allocator-V2-rust",
        ]))
}

/// atld_run `with_native_bridge_guest`: the guest linker, app_process and [`NB_GUEST_LIBS`], plus the host side that loads them.
pub fn nb_guest() -> Goals {
    let mut nb = vec![NB_LINKER, NB_EXE];
    nb.extend(NB_GUEST_LIBS);
    Goals::nb(&nb).with(Goals::host(&["libnativeloader", "libnativebridge"]))
}

/// `xtask test app` (and run-apk, audio-app, drive): atl-audio-bridge (the server's mixer, game audio A2) links AudioMixerBase's library and the utilities its gain conversion uses; the launcher builds it in every app run.
fn app() -> Goals {
    runtime()
        .with(gralloc())
        .with(nb_guest())
        .with(Goals::host(&["libaudioprocessing_base", "libaudioutils"]))
}

fn runtime_gralloc() -> Goals {
    runtime().with(gralloc())
}

fn window() -> Goals {
    runtime_gralloc().with(nb_guest())
}

fn binder() -> Goals {
    Goals::host(&[
        "servicemanager",
        "service",
        "atl-aidl-stubs-rust",
        "libbinder_rs",
        "libatl_parcelables",
    ])
}

/// Host ART with the boot classpath and its JNI libraries: what nb-isa runs, and native-load's base.
const HOST_ART: [&str; 8] = [
    "platform-bootclasspath",
    "libart",
    "libnativehelper",
    "libjavacore",
    "libopenjdk",
    "libicu_jni",
    "libandroid_runtime",
    "libsigchain",
];

fn native_load() -> Goals {
    Goals::host(&HOST_ART)
        .with(Goals::host(&[
            "servicemanager",
            "service",
            "libcodec2_aidl_noisurface",
            "libresourcemanagerservice",
            "liboggextractor",
            "libwavextractor",
            "libmedia_jni",
        ]))
        .with(Goals::codec_store())
}

fn instrument() -> Goals {
    runtime().with(Goals::host(&[
        "am",
        "android.test.runner.impl",
        "android.test.base.impl",
        "android.test.mock.impl",
    ]))
}

/// The NDK libraries with guest proxies (C3): the NativeBridge core loads their host side from SYSTEM_LIB (bindings.rs `Host::System`), atld_run places it there, and the `gl-proxies` step checks them beside the GL fronts.
pub const NDK_PROXIED: [&str; 2] = ["libandroid", "libjnigraphics"];

/// What the `gl-proxies` step checks: GL fronts then [`NDK_PROXIED`], each a host library at atld_run `gl_front`.
pub fn gl_proxied() -> Vec<&'static str> {
    GL_FRONTS.iter().copied().chain(NDK_PROXIED).collect()
}

/// The checked libraries and their guest proxies (`libnative_bridge_guest_<lib>`).
fn gl_proxies() -> Goals {
    let libs = gl_proxied();
    let proxies: Vec<String> = libs
        .iter()
        .map(|f| format!("libnative_bridge_guest_{f}"))
        .collect();
    let proxies: Vec<&str> = proxies.iter().map(String::as_str).collect();
    Goals::host(&libs).with(Goals::nb(&proxies))
}

/// A gate's goals.
type GoalsOf = fn() -> Goals;

/// Every gate that reads a native out dir: `xtask test <name>`, `xtask run-apk`, and the `xtask check` steps that read built files. Declared here, not in the gates' own modules: `xtask native --for` (bootstrap) reads them, and the gates live in xtask-harness.
pub const GATES: &[(&str, GoalsOf)] = &[
    ("app", app),
    ("audio-app", app),
    ("binder", binder),
    ("broadcast", runtime),
    ("provider", runtime),
    ("composer", runtime_gralloc),
    ("compositor", || {
        Goals::host(&["servicemanager", "service", "libgui"]).with(native_load())
    }),
    ("drive", app),
    ("cts", instrument),
    ("gl", runtime_gralloc),
    ("gl-dispatch", || Goals::host(&["libEGL", "libGLESv2"])),
    ("gl-proxies", gl_proxies),
    ("gles2-fixture", Goals::default),
    ("gralloc", || {
        Goals::host(&[
            "servicemanager",
            "service",
            "libnativewindow",
            "libui",
            "android.hardware.graphics.allocator-V2-rust",
        ])
    }),
    ("instrument", instrument),
    ("native-load", native_load),
    ("nb-isa", || Goals::host(&HOST_ART)),
    ("parcel", || {
        native_load().with(Goals::host(&[
            "libbinder_rs",
            "libatl_parcelables",
            "atl-aidl-stubs-rust",
        ]))
    }),
    ("run-apk", app),
    ("sandbox", Goals::default),
    ("sensors", runtime_gralloc),
    // the fixture's libsigchain is a converter-package goal of every build (packages.rs)
    ("ui-module", Goals::default),
    ("window", window),
];

/// The goals of gate `name`, expanded from the checkout at `root` ([`resolve`]).
pub fn of(root: &Path, name: &str) -> Result<Goals> {
    resolve(root, declared(name)?)
}

/// The goals gate `name` declares, not yet expanded; an unknown name is an error (the name check that reads nothing from disk).
pub fn declared(name: &str) -> Result<Goals> {
    match GATES.iter().find(|(n, _)| *n == name) {
        Some((_, f)) => Ok(f()),
        None => bail!(
            "no gate `{name}` declares native goals (gate_goals.rs GATES: {}, or `{ALL}`)",
            names()
        ),
    }
}

/// The union of `names`' goals (`all-gates` = every gate).
pub fn union(root: &Path, names: &[String]) -> Result<Goals> {
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
    resolve(root, g)
}

/// `g` with its symbolic groups expanded: the store's libraries, from the store source of the checkout at `root` (tree::build_file).
fn resolve(root: &Path, mut g: Goals) -> Result<Goals> {
    if std::mem::take(&mut g.store) {
        g.host.extend(crate::codec2_store::libraries(root)?);
    }
    Ok(g)
}

/// Whether `name` is a `--for` value: a [`GATES`] name or [`ALL`].
pub fn is_for_value(name: &str) -> bool {
    name == ALL || GATES.iter().any(|(n, _)| *n == name)
}

fn names() -> String {
    GATES.iter().map(|(n, _)| *n).collect::<Vec<_>>().join(", ")
}

/// Refuses unless `built` (the record's goals) holds all of gate `name`'s, naming the missing ones and the command that builds them.
pub fn ensure_built(
    root: &Path,
    name: &str,
    host_arch: &str,
    built: &BTreeSet<String>,
) -> Result<()> {
    let missing: Vec<String> = of(root, name)?
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
        let root = crate::test_root();
        assert!(
            union(&root, &[ALL.to_owned()])
                .unwrap()
                .host
                .contains("servicemanager")
        );
        assert!(of(&root, "nope").is_err());
    }

    #[test]
    fn missing_goal_is_named() {
        let root = crate::test_root();
        let built: BTreeSet<String> = ["servicemanager".to_owned()].into();
        let e = ensure_built(&root, "binder", "x86_64", &built)
            .unwrap_err()
            .to_string();
        // binder needs `service` too (binder.rs goals)
        assert!(e.contains(" service") && e.contains("--for binder"), "{e}");
        let app = of(&root, "app").unwrap();
        let nb = app.nb.iter().next().expect("app needs the nb guest");
        let e = ensure_built(&root, "app", "x86_64", &app.host)
            .unwrap_err()
            .to_string();
        assert!(e.contains(&nb_key("x86_64", nb)), "{e}");
        assert!(ensure_built(&root, "app", "x86_64", &app.recorded("x86_64")).is_ok());
    }

    #[test]
    fn store_libraries_reach_every_runtime_gate() {
        // the store's own list (C2Store.cpp) is the expectation; no literal names
        let root = crate::test_root();
        let libs = crate::codec2_store::libraries(&root).expect("store list");
        assert!(!libs.is_empty());
        for gate in ["app", "native-load", "run-apk"] {
            let g = of(&root, gate).expect(gate);
            assert!(!g.store, "{gate}: store not expanded");
            assert!(libs.iter().all(|m| g.host.contains(m)), "{gate}");
        }
    }
}
