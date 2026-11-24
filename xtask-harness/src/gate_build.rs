// SPDX-License-Identifier: MIT
//! The cargo products the runtime gates run, built and staged by one function ([`ensure`]) that every gate and `check`'s gates step call; nothing else in the harness builds them.
//! Each group is one fixed cargo invocation: the same package set every time, so feature unification gives one artifact per product and a second call is a no-op that neither recompiles nor
//! re-uplifts (cargo hard-links `release/<bin>` anew only when another package set built the unit last). Per-row package sets did exactly that: the stub flipped between two artifacts 8 times per
//! gates step (and a gate-built egl probe beside per-row egl driver builds 20 times), and a sandbox bound from the unlinked file broke the next bind from it. `check` calls [`ensure`] before its lanes, then [`Snapshot`] proves no product moved while they ran.

use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure as check};

use crate::{cargo_command, cdylib, unit_use};

/// The release binaries (`<out>/cargo/release/<name>`, package = bin name): the broker's helpers and atld (atld_run), binderd and the stub (binder.rs), the gates' probes.
const BINS: [&str; 9] = [
    "atl-binderd",
    "atl-sandbox-stub",
    "atl-sandbox-join",
    "atl-flatpak-spawn",
    "atld",
    "atl-sandbox-probe",
    "atl-egl-probe",
    "atl-engine-cpuinfo",
    // the labwc setup a remote test run starts on its target beside its labwc (remote.rs `labwc_setup_script`)
    "atl-labwc-setup",
];

/// atld runs: the native bridge's dir on every app's library path (atld_run).
pub const NATIVE_BRIDGE_DIR: &str = "native-bridge/lib";
/// nb-isa's copy: not below `<out>/nb-isa`, which its `compile` recreates.
pub const NB_ISA_DIR: &str = "nb-isa-lib";

/// The cdylibs a runtime process dlopens by file name, as `(package, staging dir under <out>, installed name)`; one built file may be staged in several dirs.
fn staged() -> [(&'static str, &'static str, String); 7] {
    [
        // the GL driver every atld run with `with_gl_driver` places (egl_driver.rs `placement`)
        (
            crate::egl_driver::DRIVER_PACKAGE,
            crate::egl_driver::DIR,
            crate::egl_driver::atl_egl_driver_name().to_owned(),
        ),
        (
            "atl-native-bridge",
            NATIVE_BRIDGE_DIR,
            atl_native_bridge::LIBRARY.to_owned(),
        ),
        (
            "atl-native-bridge",
            NB_ISA_DIR,
            atl_native_bridge::LIBRARY.to_owned(),
        ),
        (
            crate::ui_module::MODULE_PACKAGE,
            crate::ui_module::DIR,
            crate::ui_module::MODULE_FILE.to_owned(),
        ),
        (
            crate::ui_module::WEBVIEW_MODULE_PACKAGE,
            crate::ui_module::DIR,
            crate::ui_module::WEBVIEW_MODULE_FILE.to_owned(),
        ),
        (
            crate::ui_module::WEBVIEW_JNI_PACKAGE,
            crate::ui_module::DIR,
            crate::ui_module::WEBVIEW_JNI_FILE.to_owned(),
        ),
        // libui's `android_load_sphal_library` dlopens it by this name (gralloc.rs)
        (
            "atl-gralloc-mapper",
            crate::gralloc::MAPPER_DIR,
            atl_gralloc::vintf::mapper_library_name(),
        ),
    ]
}

/// One cargo invocation of [`ensure`]; groups use separate target roots or manifests, so building one never changes another's artifacts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Group {
    /// [`BINS`] + the staged cdylibs, release, `<out>/cargo` (a cross host's out dir: that host's).
    Release,
    /// The gl-dispatch probe: its own workspace (`gl_dispatch::PROBE`), release, `<out>/cargo`; build machine only.
    GlDispatch,
    /// atl-native-bridge's `sles_recorder` example, debug, cargo's default target dir (`CARGO_TARGET_DIR`, else `<root>/target`), `ATL_NB_BREAK` unset; build machine only.
    Recorder,
}

impl Group {
    pub const ALL: [Group; 3] = [Group::Release, Group::GlDispatch, Group::Recorder];
}

/// Where [`ensure`] put the products of the groups it was asked for.
pub struct Products {
    release: PathBuf,
    out: PathBuf,
    gl_dispatch: Option<PathBuf>,
    recorder: Option<PathBuf>,
}

impl Products {
    /// Release binary `name` ([`BINS`]).
    pub fn bin(&self, name: &str) -> PathBuf {
        debug_assert!(BINS.contains(&name), "{name} is not a gate binary");
        self.release.join(name)
    }

    /// The staging dir `dir` of the staged cdylibs (for a library path).
    pub fn staged_dir(&self, dir: &str) -> PathBuf {
        self.out.join(dir)
    }

    pub fn gl_dispatch(&self) -> Result<&Path> {
        self.gl_dispatch
            .as_deref()
            .context("gl-dispatch probe not built: ensure(Group::GlDispatch)")
    }

    pub fn recorder(&self) -> Result<&Path> {
        self.recorder
            .as_deref()
            .context("sles recorder not built: ensure(Group::Recorder)")
    }

    /// Every file a group built or staged, cargo's outputs and the staged copies.
    pub fn files(&self) -> Vec<PathBuf> {
        let mut f: Vec<PathBuf> = BINS.iter().map(|b| self.release.join(b)).collect();
        for (package, dir, name) in staged() {
            f.push(self.release.join(lib_file(package)));
            f.push(self.out.join(dir).join(name));
        }
        f.extend(self.gl_dispatch.clone());
        f.extend(self.recorder.clone());
        f.sort();
        f.dedup();
        f
    }
}

fn lib_file(package: &str) -> String {
    format!("lib{}.so", package.replace('-', "_"))
}

/// The package set of [`Group::Release`], the same for every host.
fn packages(staged: &[(&'static str, &'static str, String)]) -> Vec<&'static str> {
    let mut packages = BINS.to_vec();
    for (p, _, _) in staged {
        if !packages.contains(p) {
            packages.push(p);
        }
    }
    packages
}

/// Builds `groups` on build dir `out` and stages the cdylibs; a group already current costs one no-op cargo run and no copy.
pub fn ensure(root: &Path, out: &Path, groups: &[Group]) -> Result<Products> {
    let staged = staged();
    let packages = packages(&staged);
    let (mut cmd, release) = cdylib::release_build(root, out, &packages)?;
    let mut products = Products {
        release,
        out: out.to_owned(),
        gl_dispatch: None,
        recorder: None,
    };
    if groups.contains(&Group::Release) {
        xtask::timing::phase("gate build: release", || {
            unit_use::exec(cmd.arg("--locked"), "cargo build (gate products)")
        })?;
        for (package, dir, name) in &staged {
            install(
                &products.release.join(lib_file(package)),
                &out.join(dir),
                name,
            )?;
        }
    }
    if groups.contains(&Group::GlDispatch) {
        let target = out.join("cargo");
        unit_use::exec(
            cargo_command()
                .current_dir(root)
                .args(["build", "--release", "--locked", "--manifest-path"])
                .arg(root.join(crate::gl_dispatch::PROBE).join("Cargo.toml"))
                .arg("--target-dir")
                .arg(&target),
            "cargo build (gl-dispatch probe)",
        )?;
        products.gl_dispatch = Some(built(target.join("release/gl-dispatch-probe"))?);
    }
    if groups.contains(&Group::Recorder) {
        let example = crate::sles_bridge::RECORDER;
        unit_use::exec(
            cargo_command()
                .args([
                    "build",
                    "--locked",
                    "-p",
                    "atl-native-bridge",
                    "--example",
                    example,
                ])
                .env_remove("ATL_NB_BREAK")
                .current_dir(root),
            "cargo build (sles recorder)",
        )?;
        let target =
            std::env::var_os("CARGO_TARGET_DIR").map_or_else(|| root.join("target"), PathBuf::from);
        products.recorder = Some(built(
            target.join(format!("debug/examples/lib{example}.so")),
        )?);
    }
    Ok(products)
}

fn built(path: PathBuf) -> Result<PathBuf> {
    check!(
        path.is_file(),
        "{} missing after cargo build",
        path.display()
    );
    Ok(path)
}

/// Installs `built` as `<dir>/<name>` unless it is there already (same size and mtime: the copy keeps the build's mtime). A process may map the installed file: replaced by rename, never rewritten
/// in place; the tmp name is per process, since gates run alone may stage at once.
fn install(built: &Path, dir: &Path, name: &str) -> Result<()> {
    let src = std::fs::metadata(built)
        .with_context(|| format!("{} missing after cargo build", built.display()))?;
    let dst = dir.join(name);
    if std::fs::metadata(&dst)
        .is_ok_and(|d| d.len() == src.len() && d.modified().ok() == src.modified().ok())
    {
        return Ok(());
    }
    std::fs::create_dir_all(dir).with_context(|| dir.display().to_string())?;
    let tmp = dir.join(format!(".{name}.{}.tmp", std::process::id()));
    std::fs::copy(built, &tmp).with_context(|| format!("installing {}", built.display()))?;
    std::fs::File::options()
        .write(true)
        .open(&tmp)
        .and_then(|f| f.set_modified(src.modified()?))
        .with_context(|| format!("setting the mtime of {}", tmp.display()))?;
    std::fs::rename(&tmp, &dst).with_context(|| format!("installing {}", dst.display()))?;
    Ok(())
}

/// A file's identity: a re-link or a re-stage gives a new inode, a rewrite in place a new mtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Id {
    dev: u64,
    ino: u64,
    mtime_ns: i128,
}

fn id_of(path: &Path) -> Option<Id> {
    let m = std::fs::metadata(path).ok()?;
    Some(Id {
        dev: m.dev(),
        ino: m.ino(),
        mtime_ns: i128::from(m.mtime()) * 1_000_000_000 + i128::from(m.mtime_nsec()),
    })
}

fn describe(id: Option<Id>) -> String {
    id.map_or_else(
        || "missing".to_owned(),
        |i| format!("inode {} mtime {}ns", i.ino, i.mtime_ns),
    )
}

/// The products' identities when taken; [`Snapshot::changed`] names each that differs now.
pub struct Snapshot {
    files: Vec<(PathBuf, Option<Id>)>,
}

impl Snapshot {
    pub fn take(files: &[PathBuf]) -> Snapshot {
        Snapshot {
            files: files.iter().map(|f| (f.clone(), id_of(f))).collect(),
        }
    }

    /// `<path> (<before> -> <now>)` for each file whose identity changed since [`Snapshot::take`].
    pub fn changed(&self) -> Vec<(PathBuf, String)> {
        self.files
            .iter()
            .filter_map(|(f, before)| {
                let now = id_of(f);
                (now != *before).then(|| {
                    (
                        f.clone(),
                        format!("{} -> {}", describe(*before), describe(now)),
                    )
                })
            })
            .collect()
    }
}

/// What moved under the lanes: each changed product once, with the row whose end first saw it (`None`: seen only after the lanes).
#[derive(Default)]
pub struct Moved {
    seen: Vec<(PathBuf, String, Option<String>)>,
}

impl Moved {
    /// Records the products changed now that were not seen before, attributed to `row` (the row that just ended; parallel rows overlap, so the culprit ran at or before it).
    pub fn note(&mut self, snap: &Snapshot, row: Option<&str>) {
        for (path, how) in snap.changed() {
            if !self.seen.iter().any(|(p, _, _)| *p == path) {
                self.seen.push((path, how, row.map(str::to_owned)));
            }
        }
    }

    /// One line per moved product (`gates: product <path> changed during the lanes (<how>), first seen after row <row>`).
    pub fn lines(&self) -> Vec<String> {
        self.seen
            .iter()
            .map(|(p, how, row)| {
                let at = row.as_deref().map_or_else(
                    || "after the lanes".to_owned(),
                    |r| format!("first seen when row {r} ended"),
                );
                format!(
                    "gates: product {} changed during the lanes ({how}), {at}",
                    p.display()
                )
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("xtask-gate-build-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).expect("temp dir");
        d
    }

    /// The guard fires: a product replaced by rename (what cargo's re-uplift and a re-stage do) is named with the row that saw it; an untouched one is not; a second note does not repeat it.
    #[test]
    fn a_relinked_product_is_named_with_its_row() {
        let d = temp("guard");
        let (stub, kept) = (d.join("atl-sandbox-stub"), d.join("atld"));
        std::fs::write(&stub, b"variant a").expect("stub");
        std::fs::write(&kept, b"atld").expect("atld");
        let snap = Snapshot::take(&[stub.clone(), kept.clone()]);
        let mut moved = Moved::default();
        moved.note(&snap, Some("sandbox"));
        assert!(
            moved.lines().is_empty(),
            "nothing moved yet: {:?}",
            moved.lines()
        );
        let other = d.join("other");
        std::fs::write(&other, b"variant b").expect("other");
        std::fs::rename(&other, &stub).expect("re-link");
        moved.note(&snap, Some("provider"));
        moved.note(&snap, Some("app"));
        let lines = moved.lines();
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(
            lines[0].starts_with(&format!(
                "gates: product {} changed during the lanes (inode ",
                stub.display()
            )),
            "{}",
            lines[0]
        );
        assert!(
            lines[0].ends_with("first seen when row provider ended"),
            "{}",
            lines[0]
        );
        std::fs::remove_dir_all(&d).expect("cleanup");
    }

    /// The build [`ensure`] runs makes every program a remote run starts (remote.rs `RUN_PROGRAMS`), for any host: the package set does not vary. Break: a program dropped from [`BINS`].
    #[test]
    fn release_group_builds_every_run_program() {
        let packages = packages(&staged());
        for p in crate::remote::RUN_PROGRAMS {
            assert!(packages.contains(&p), "{p} not in {packages:?}");
        }
    }

    /// Staging twice keeps the installed file (same inode): a re-stage under a running process is what the guard would catch; a changed build is installed anew.
    #[test]
    fn install_is_idempotent_and_follows_a_new_build() {
        let d = temp("install");
        let built = d.join("libx.so");
        std::fs::write(&built, b"build 1").expect("built");
        install(&built, &d.join("lib"), "x.so").expect("install");
        let first = id_of(&d.join("lib/x.so")).expect("installed");
        install(&built, &d.join("lib"), "x.so").expect("install again");
        assert_eq!(id_of(&d.join("lib/x.so")), Some(first));
        std::fs::write(&built, b"build two").expect("rebuilt");
        install(&built, &d.join("lib"), "x.so").expect("install rebuilt");
        assert_eq!(
            std::fs::read(d.join("lib/x.so")).expect("read"),
            b"build two"
        );
        std::fs::remove_dir_all(&d).expect("cleanup");
    }
}
