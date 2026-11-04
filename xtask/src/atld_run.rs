// SPDX-License-Identifier: MIT
//! What the runtime gates that start atld share (`xtask test app`, `xtask test instrument`): the converter's ART staged as an install lays it out (host_art.rs), servicemanager, atl-system-services, atl-binderd and atld,
//! the Java pipeline's boot jars, aapt2, d8 and the SDK's android.jar, the test fonts, an APK built the way an app is built, and atld's command line for it.
//! The APK is built against the SDK's android.jar of the platform's API level (prebuilts/sdk/<ro.build.version.sdk>/public): aapt2 link (manifest only), javac + d8 (the Java pipeline's), classes.dex added uncompressed.
//! Needs the converter's ART, servicemanager, atl-services and the boot jars' JNI libraries in --out (`xtask native ... servicemanager libart libnativehelper libjavacore libopenjdk libicu_jni libandroid_runtime libandroid libsigchain atl-aidl-stubs-rust platform-bootclasspath-jni`).

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use anyhow::{Context, Result, ensure};
use atl_layout::Layout;
use atl_services::packages::{Library, Manifest};
use serde::{Deserialize, Serialize};

use crate::host_art::{self, HostArt};
use crate::{farm, packages};

/// framework-res.apk in the Java pipeline's out: built as Soong builds it (manifest_fixer, link, merge, align; bp2ninja app.go), unsigned.
const FRAMEWORK_RES: &str = "device/framework-res/framework-res.apk";
/// The system fonts of the test layout (stand-in until the desktop's fonts come through the platform layer): ActivityThread.handleBindApplication loads `/system/etc/font_fallback.xml`
/// (SystemFonts.FONTS_XML) and every file it names under `/system/fonts/` (SystemFonts.DEVICE_FONT_DIR), and Typeface needs a `sans-serif` family (Typeface.DEFAULT_FAMILY) to start.
/// The one font in this tree that AOSP ships and licenses as Apache-2.0 with a Latin range, AOSP's own monospace font, serves as that family; the XML is FontListParser's format (fonts.xml's `<family name><font weight style>`).
const FONT_FILE: &str = "DroidSansMono.ttf";
const FONT_DIR: &str = "frameworks/base/data/fonts";
const FONT_FALLBACK_XML: &str = "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<familyset version=\"23\">\n    <family name=\"sans-serif\">\n        <font weight=\"400\" style=\"normal\">DroidSansMono.ttf</font>\n    </family>\n</familyset>\n";

/// The staged runtime and the tools around it.
pub struct Runtime {
    pub art: HostArt,
    /// The native build directory: the gates keep their work below it.
    pub out: PathBuf,
    pub java_out: PathBuf,
    pub aapt2: PathBuf,
    android_jar: PathBuf,
    servicemanager: PathBuf,
    services: PathBuf,
    binderd: PathBuf,
    atld: PathBuf,
    framework_res: PathBuf,
    font_xml: PathBuf,
    font_file: PathBuf,
    /// The graphics allocator HAL and the directory of `mapper.atl.so`, when [`Runtime::with_gralloc`] asked for them.
    gralloc: Option<(PathBuf, PathBuf)>,
}

/// A command the shell runs once the system is up (`am instrument ...`).
pub struct Shell {
    pub class: String,
    pub uid: u32,
    /// its log, `log/<name>.log`
    pub name: String,
    pub args: Vec<String>,
    /// the dex jars holding `class`
    pub boot_jars: Vec<PathBuf>,
}

/// One atld run.
pub struct Run {
    /// atld's runtime root (atld makes it again); its logs are `<root>/log/<process>.log`.
    pub root: PathBuf,
    /// Where the package files and atld's own log go (outside the root, which atld removes).
    pub scratch: PathBuf,
    /// Each package's APK and manifest, in uid order.
    pub packages: Vec<(PathBuf, Manifest)>,
    pub first_uid: u32,
    pub launch: Option<String>,
    /// Shared libraries by name; `file` is the host jar.
    pub libraries: Vec<(String, Library)>,
    pub shell: Option<Shell>,
    pub log_tags: String,
}

/// atld's `--package` file.
#[derive(Serialize)]
struct PackageSource<'a> {
    apk: &'a Path,
    manifest: &'a Manifest,
}

impl Run {
    pub fn atld_log(&self) -> PathBuf {
        self.scratch.join("atld.log")
    }

    pub fn log(&self, name: &str) -> PathBuf {
        self.root.join("log").join(format!("{name}.log"))
    }
}

/// `package`'s `bin`, built plainly (release) into `out`'s cargo target dir; its path.
fn cargo_bin(root: &Path, out: &Path, package: &str) -> Result<PathBuf> {
    host_art::exec(
        crate::cargo_command()
            .current_dir(root)
            .args(["build", "--release", "--locked", "-p", package])
            .arg("--target-dir")
            .arg(out.join("cargo")),
        &format!("cargo build -p {package}"),
    )?;
    Ok(out.join("cargo/release").join(package))
}

/// The platform's API level: `ro.build.version.sdk` of the converter's build.prop.
fn sdk_level(build_prop: &Path) -> Result<String> {
    let text = std::fs::read_to_string(build_prop)?;
    text.lines()
        .find_map(|l| l.strip_prefix("ro.build.version.sdk="))
        .map(|v| v.trim().to_owned())
        .with_context(|| format!("no ro.build.version.sdk in {}", build_prop.display()))
}

impl Runtime {
    /// Stages ART into `<out>/<stage>`, builds the cargo binaries, checks every input exists; the test fonts are written to `<out>/fonts`.
    pub fn new(
        root: &Path,
        host: &str,
        out: Option<&Path>,
        java_out: Option<&Path>,
        stage: &str,
        verbose: bool,
    ) -> Result<Runtime> {
        let layout = Layout::of(root)?;
        let out = match out {
            Some(dir) => std::path::absolute(dir)?,
            None => layout.workspace().join("out/native").join(host),
        };
        let java_out = match java_out {
            Some(dir) => std::path::absolute(dir)?,
            None => layout.workspace().join("out/java/build"),
        };
        let art = HostArt::new(root, host, Some(&java_out), &out, stage, verbose)?;
        let servicemanager = out.join("device/servicemanager/servicemanager");
        ensure!(
            servicemanager.is_file(),
            "{} missing: build servicemanager",
            servicemanager.display()
        );
        let services = packages::bin(root, &out, "atl-services", "atl-system-services")?;
        let binderd = cargo_bin(root, &out, "atl-binderd")?;
        let atld = cargo_bin(root, &out, "atld")?;
        let aapt2 = java_out.join("host/aapt2/aapt2");
        let android_jar = farm::shared_tree(&layout)
            .join("prebuilts/sdk")
            .join(sdk_level(&art.build_prop)?)
            .join("public/android.jar");
        let framework_res = java_out.join(FRAMEWORK_RES);
        for p in [&aapt2, &android_jar, &framework_res] {
            ensure!(p.is_file(), "missing input {}", p.display());
        }
        let fonts = out.join("fonts");
        std::fs::create_dir_all(&fonts)?;
        let font_xml = fonts.join("font_fallback.xml");
        std::fs::write(&font_xml, FONT_FALLBACK_XML)?;
        let font_file = farm::shared_tree(&layout).join(FONT_DIR).join(FONT_FILE);
        ensure!(font_file.is_file(), "missing input {}", font_file.display());
        Ok(Runtime {
            art,
            out,
            java_out,
            aapt2,
            android_jar,
            servicemanager,
            services,
            binderd,
            atld,
            framework_res,
            font_xml,
            font_file,
            gralloc: None,
        })
    }

    /// Also starts the graphics allocator HAL under atld (`--gralloc`): the allocator `xtask native` built, and `mapper.atl.so` installed in `<out>/gralloc/lib`, which joins every process's library path.
    /// Needs a DRM device that allocates (atld names what is missing when it does not start).
    pub fn with_gralloc(mut self, root: &Path) -> Result<Self> {
        let allocator = packages::bin(
            root,
            &self.out,
            "atl-gralloc-service",
            "atl-gralloc-allocator",
        )?;
        let lib = crate::gralloc::stage_mapper(root, &self.out)?;
        self.gralloc = Some((allocator, lib));
        Ok(self)
    }

    /// Builds the APK of `fixture` (AndroidManifest.xml and java/**.java) into `work`; `class_jars` are on javac's and d8's class path besides android.jar.
    pub fn build_apk(
        &self,
        fixture: &Path,
        work: &Path,
        class_jars: &[PathBuf],
    ) -> Result<PathBuf> {
        if work.exists() {
            std::fs::remove_dir_all(work)?;
        }
        let classes = work.join("classes");
        let dex = work.join("dex");
        for d in [&classes, &dex] {
            std::fs::create_dir_all(d)?;
        }
        let apk = work.join("base.apk");
        host_art::exec(
            Command::new(&self.aapt2)
                .arg("link")
                .arg("--manifest")
                .arg(fixture.join("AndroidManifest.xml"))
                .arg("-I")
                .arg(&self.android_jar)
                .arg("-o")
                .arg(&apk),
            "aapt2 link",
        )?;
        let mut sources = Vec::new();
        host_art::collect(&fixture.join("java"), "java", &mut sources)?;
        let classpath = std::env::join_paths(std::iter::once(&self.android_jar).chain(class_jars))?;
        let jdk_bin = self.art.jdk.join("bin");
        host_art::exec(
            Command::new(jdk_bin.join("javac"))
                .args([
                    "-source",
                    "8",
                    "-target",
                    "8",
                    "-Xlint:-options",
                    "-bootclasspath",
                ])
                .arg(&classpath)
                .arg("-d")
                .arg(&classes)
                .args(&sources),
            "javac",
        )?;
        let jar = work.join("classes.jar");
        host_art::exec(
            Command::new(jdk_bin.join("jar"))
                .arg("cf")
                .arg(&jar)
                .arg("-C")
                .arg(&classes)
                .arg("."),
            "jar",
        )?;
        let path = std::env::join_paths(std::iter::once(jdk_bin.clone()).chain(
            std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()),
        ))?;
        let mut d8 = Command::new(&self.art.d8);
        d8.env("PATH", path).arg("--lib").arg(&self.android_jar);
        for lib in class_jars {
            d8.arg("--lib").arg(lib);
        }
        host_art::exec(d8.arg("--output").arg(&dex).arg(&jar), "d8")?;
        // the dex stored, as the platform's packagers store it (ART maps an uncompressed dex in place)
        host_art::exec(
            Command::new(jdk_bin.join("jar"))
                .args(["--update", "--no-manifest", "--no-compress", "--file"])
                .arg(&apk)
                .arg("-C")
                .arg(&dex)
                .arg("classes.dex"),
            "jar --update",
        )?;
        Ok(apk)
    }

    /// Starts atld for `run`, its stdout and stderr in `run.atld_log()`. The caller waits for it and ends it.
    pub fn start_atld(&self, run: &Run) -> Result<Child> {
        std::fs::create_dir_all(&run.scratch)?;
        // atld makes the root again itself, but only once it runs: a poll must not see the previous run's logs
        if run.root.exists() {
            std::fs::remove_dir_all(&run.root)
                .with_context(|| format!("removing {}", run.root.display()))?;
        }
        // the staged ART's library dir, then mapper.atl.so's (a PathBuf would take the absolute second as a replacement)
        let mut lib_path = std::ffi::OsString::from(self.art.lib_path.as_os_str());
        if let Some((_, dir)) = &self.gralloc {
            lib_path.push(":");
            lib_path.push(dir);
        }
        let mut cmd = Command::new(&self.atld);
        if let Some((allocator, _)) = &self.gralloc {
            cmd.arg("--gralloc").arg(allocator);
        }
        cmd.arg("--root")
            .arg(&run.root)
            .arg("--binderd")
            .arg(&self.binderd)
            .arg("--servicemanager")
            .arg(&self.servicemanager)
            .arg("--services")
            .arg(&self.services)
            .arg("--launcher")
            .arg(&self.art.launcher)
            .arg("--lib-path")
            .arg(&lib_path)
            .arg("--art-root")
            .arg(&self.art.art)
            .arg("--build-prop")
            .arg(&self.art.build_prop)
            .arg("--framework-res")
            .arg(&self.framework_res)
            .arg("--system-file")
            .arg(format!("{}=etc/font_fallback.xml", self.font_xml.display()))
            .arg("--system-file")
            .arg(format!("{}=fonts/{FONT_FILE}", self.font_file.display()))
            .args(["--first-uid", &run.first_uid.to_string()])
            .args(["--log-tags", &run.log_tags]);
        for jar in &self.art.boot {
            cmd.arg("--boot-jar").arg(jar);
        }
        for (i, (apk, manifest)) in run.packages.iter().enumerate() {
            let file = run.scratch.join(format!("package-{i}.json"));
            std::fs::write(
                &file,
                serde_json::to_vec_pretty(&PackageSource { apk, manifest })?,
            )?;
            cmd.arg("--package").arg(file);
        }
        for (name, lib) in &run.libraries {
            cmd.arg("--shared-library").arg(lib.to_arg(name, &lib.file));
        }
        if let Some(p) = &run.launch {
            cmd.args(["--launch", p]);
        }
        if let Some(s) = &run.shell {
            cmd.args(["--run-class", &s.class])
                .args(["--run-uid", &s.uid.to_string()])
                .args(["--run-name", &s.name]);
            for a in &s.args {
                cmd.arg(format!("--run-arg={a}"));
            }
            for j in &s.boot_jars {
                cmd.arg("--run-boot-jar").arg(j);
            }
        }
        let log_file = std::fs::File::create(run.atld_log())?;
        cmd.stdin(Stdio::null())
            .stdout(log_file.try_clone()?)
            .stderr(log_file);
        cmd.spawn().context("starting atld")
    }
}

/// One service counter: what it counts (`unimplemented` call or `refused` parcelable), its name and its total.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Counter {
    pub kind: String,
    pub name: String,
    pub count: u64,
}

/// One counter line of the services' log: the counter's running count after that call, and the wall-clock time (ms since the epoch) the call was counted at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    pub at_ms: u64,
    pub counter: Counter,
}

/// Milliseconds per second: the unit of a stamp's `secs.mmm`, logcat's and the services' wall clock.
pub const MS_PER_S: u64 = 1_000;
/// Digits after the point of a stamp: milliseconds, as logcat prints them.
const STAMP_DIGITS: usize = 3;

/// A wall-clock stamp `secs.mmm` (logcat's first field, a counter line's `at=`) as milliseconds since the epoch.
pub fn parse_stamp(text: &str) -> Option<u64> {
    let (secs, frac) = text.split_once('.')?;
    if frac.len() != STAMP_DIGITS || !frac.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(secs.parse::<u64>().ok()? * MS_PER_S + frac.parse::<u64>().ok()?)
}

/// A counter line without its ` at=<stamp>`: the text the exact-match checks compare.
pub fn without_stamp(line: &str) -> &str {
    line.rsplit_once(" at=").map_or(line, |(head, _)| head)
}

/// Every counter line of the services' log, in log order: `services: unimplemented <interface>.<method> <n> at=<stamp>` and `services: refused <interface>.<method> <n> at=<stamp>`
/// (atl_services::counters), and `parcelables: refused <what> <n> at=<stamp>` (atl_parcelables `refuse`). A line without a parsable stamp is not one.
pub fn events(services_log: &str) -> Vec<Event> {
    const KINDS: [(&str, &str); 3] = [
        ("services: unimplemented ", "unimplemented"),
        ("services: refused ", "refused"),
        ("parcelables: refused ", "refused"),
    ];
    let mut out = Vec::new();
    for line in services_log.lines() {
        for (prefix, kind) in KINDS {
            let Some((body, stamp)) = line
                .strip_prefix(prefix)
                .and_then(|rest| rest.rsplit_once(" at="))
            else {
                continue;
            };
            let Some(at_ms) = parse_stamp(stamp) else {
                continue;
            };
            let Some((name, n)) = body.rsplit_once(' ') else {
                continue;
            };
            let Ok(count) = n.parse::<u64>() else {
                continue;
            };
            out.push(Event {
                at_ms,
                counter: Counter {
                    kind: kind.to_owned(),
                    name: name.to_owned(),
                    count,
                },
            });
        }
    }
    out
}

/// The last count of each (kind, name) among `events`: the totals. The services are killed at the end of a run and print no summary, so the last line per name is the total.
pub fn totals<'a>(events: impl IntoIterator<Item = &'a Event>) -> Vec<Counter> {
    let mut last = std::collections::BTreeMap::new();
    for e in events {
        last.insert(
            (e.counter.kind.clone(), e.counter.name.clone()),
            e.counter.count,
        );
    }
    last.into_iter()
        .map(|((kind, name), count)| Counter { kind, name, count })
        .collect()
}

/// The services' counters, from their log: the [`totals`] of [`events`].
pub fn counters(services_log: &str) -> Vec<Counter> {
    totals(&events(services_log))
}

/// [`counters`] as `<kind> <name> x<total>` lines.
pub fn counter_lines(services_log: &str) -> Vec<String> {
    counters(services_log)
        .into_iter()
        .map(|c| format!("{} {} x{}", c.kind, c.name, c.count))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Both counter kinds, the last count per name wins, a name with spaces keeps them, other lines are ignored, and a counter line without a stamp is not one.
    #[test]
    fn counter_lines_take_the_last_count_of_each_kind() {
        let log = "services: ready\n\
                   services: unimplemented IActivityManager.getProcessLimit 1 at=100.000\n\
                   parcelables: refused android.content.Intent.extras 1 at=100.001\n\
                   services: unimplemented IActivityManager.getProcessLimit 2 at=100.002\n\
                   parcelables: refused android.os.Bundle (value type) 1 at=100.003\n\
                   parcelables: refused android.content.Intent.extras 2 at=100.004\n\
                   services: Failure reporting\n\
                   services: unimplemented IPackageManager.getMoveStatus 9\n";
        assert_eq!(
            counter_lines(log),
            [
                "refused android.content.Intent.extras x2",
                "refused android.os.Bundle (value type) x1",
                "unimplemented IActivityManager.getProcessLimit x2",
            ]
        );
        assert!(counter_lines("nothing\n").is_empty());
    }
}
