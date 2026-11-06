// SPDX-License-Identifier: MIT
//! What the runtime gates that start atld share (`xtask test app`, `xtask test instrument`): the converter's ART staged as an install lays it out (host_art.rs), servicemanager, atl-system-services, atl-binderd and atld,
//! the Java pipeline's boot jars, aapt2, d8 and the SDK's android.jar, the test fonts, an APK built the way an app is built, and atld's command line for it.
//! The APK is built against the SDK's android.jar of the platform's API level (prebuilts/sdk/<ro.build.version.sdk>/public): aapt2 link (manifest only), javac + d8 (the Java pipeline's), classes.dex added uncompressed.
//! Needs the converter's ART, servicemanager, atl-services and the boot jars' JNI libraries in --out (`xtask native ... servicemanager libart libnativehelper libjavacore libopenjdk libicu_jni libandroid_runtime libandroid libsigchain atl-aidl-stubs-rust platform-bootclasspath-jni`).

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use anyhow::{Context, Result, ensure};
use atl_layout::Layout;
use atl_services::display_model::DisplayModel;
use atl_services::native_libs::SupportedAbi;
use atl_services::packages::{Library, Manifest};
use serde::{Deserialize, Serialize};

use crate::host_art::{self, HostArt};
use crate::{farm, manifest, packages, slot};

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
    pub apk: ApkTools,
    servicemanager: PathBuf,
    services: PathBuf,
    binderd: PathBuf,
    atld: PathBuf,
    framework_res: PathBuf,
    font_xml: PathBuf,
    font_file: PathBuf,
    /// AOSP's keyboards dir: its [`farm::KEYBOARD_FILES`] are linked into /system (the input service loads the maps, the app's input thread the layout).
    keychars: PathBuf,
    /// The graphics allocator HAL and the directory of `mapper.atl.so`, when [`Runtime::with_gralloc`] asked for them.
    gralloc: Option<(PathBuf, PathBuf)>,
    /// The directory of the NativeBridge core (atl-native-bridge `LIBRARY`), which ART of every app process opens by name (`ro.dalvik.vm.native.bridge`).
    native_bridge: PathBuf,
    /// The device's ABIs, preferred first (`ro.product.cpu.abilist`'s role), for the install's native-library extraction: the host row's `android_abis` (arches.toml); translated ABIs join when the bridge runs guest code.
    abis: Vec<SupportedAbi>,
    /// The headless pointer script (atld `--headless-pointer`), when [`Runtime::with_headless_pointer`] set it.
    headless_pointer: Option<String>,
    /// The display the run answers with (atld `--display`, `WxH@hz,dpi`).
    display: Option<String>,
    /// The app processes' UI mode (atld `--ui`), when [`Runtime::with_ui`] set it.
    ui: Option<String>,
    /// A composer rule a gate breaks on purpose (`ATL_UI_BREAK`, atl-composer start.rs), when [`Runtime::with_ui_break`] set it.
    ui_break: Option<&'static str>,
    /// The Wayland socket atld's children connect to (the window gate's proxy); the environment's when unset.
    wayland: Option<PathBuf>,
    /// Apps run under atld's app seccomp filter (atl-broker seccomp.rs) unless this is cleared (`--no-app-seccomp`, the deliberate-failure check).
    app_seccomp: bool,
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
    abis: &'a [SupportedAbi],
}

impl Run {
    pub fn atld_log(&self) -> PathBuf {
        self.scratch.join("atld.log")
    }

    pub fn log(&self, name: &str) -> PathBuf {
        self.root.join("log").join(format!("{name}.log"))
    }
}

/// The gates' display: the model's default size and density (fixed, so pictures and sizes are the same on every host, and small enough that WSLg keeps the display rate) at the monitor's refresh rate
/// (GDK, atl-platform-host), which the cadence gates judge against. No desktop or no rate: the default rate, and a line says so.
pub fn run_display() -> DisplayModel {
    let hz = match atl_platform_host::first_monitor() {
        Ok(m) => {
            println!("display: monitor {m:?}");
            m.and_then(|m| m.refresh_hz)
        }
        Err(e) => {
            println!("display: {e}, the default refresh rate");
            None
        }
    };
    let default = DisplayModel::default();
    DisplayModel {
        refresh_hz: hz.unwrap_or(default.refresh_hz),
        ..default
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

/// What building an APK needs, and nothing of the runtime: aapt2 and d8 of the Java pipeline, the prebuilt JDK, the SDK's android.jar of the platform's API level. The static fixture gates use it without a staged ART.
pub struct ApkTools {
    pub aapt2: PathBuf,
    android_jar: PathBuf,
    pub jdk: PathBuf,
    d8: PathBuf,
}

impl ApkTools {
    /// `out` = the native build dir (its build.prop names the API level), `java_out` = the Java pipeline build dir, `host` = the arches.toml host row (its prebuilt JDK); every input checked to exist.
    pub fn new(root: &Path, host: &str, out: &Path, java_out: &Path) -> Result<ApkTools> {
        let layout = Layout::of(root)?;
        let (d8, jdk) = host_art::java_tools(root, host, java_out)?;
        let t = ApkTools {
            aapt2: java_out.join("host/aapt2/aapt2"),
            android_jar: farm::shared_tree(&layout)
                .join("prebuilts/sdk")
                .join(sdk_level(&out.join(host_art::BUILD_PROP))?)
                .join("public/android.jar"),
            jdk,
            d8,
        };
        for p in [&t.aapt2, &t.android_jar, &t.d8] {
            ensure!(p.is_file(), "missing input {}", p.display());
        }
        ensure!(t.jdk.is_dir(), "missing input {}", t.jdk.display());
        Ok(t)
    }
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
        let out = crate::build_state::gate_out(root, out, host)?;
        let java_out = slot::out_dir(root, java_out, "--java-out", |workspace| {
            workspace.join("out/java/build")
        })?;
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
        let apk = ApkTools::new(root, host, &out, &java_out)?;
        let framework_res = java_out.join(FRAMEWORK_RES);
        ensure!(
            framework_res.is_file(),
            "missing input {}",
            framework_res.display()
        );
        let fonts = out.join("fonts");
        std::fs::create_dir_all(&fonts)?;
        let font_xml = fonts.join("font_fallback.xml");
        std::fs::write(&font_xml, FONT_FALLBACK_XML)?;
        let font_file = farm::shared_tree(&layout).join(FONT_DIR).join(FONT_FILE);
        ensure!(font_file.is_file(), "missing input {}", font_file.display());
        let keychars = farm::keyboards(&layout);
        for (file, _) in farm::KEYBOARD_FILES {
            let path = keychars.join(file);
            ensure!(path.is_file(), "missing input {}", path.display());
        }
        let native_bridge = crate::cdylib::stage(
            root,
            &out,
            "atl-native-bridge",
            "native-bridge/lib",
            atl_native_bridge::LIBRARY,
        )?;
        let arches = crate::arches::load(root)?;
        let guest = arches
            .get(host)
            .and_then(|a| a.guest.as_ref())
            .with_context(|| format!("{host}: no [guest] row in arches.toml"))?;
        let abis = guest
            .android_abis
            .iter()
            .map(|abi| SupportedAbi {
                abi: abi.clone(),
                isa: host.to_owned(),
            })
            .collect();
        Ok(Runtime {
            native_bridge,
            abis,
            art,
            out,
            java_out,
            apk,
            servicemanager,
            services,
            binderd,
            atld,
            framework_res,
            font_xml,
            font_file,
            keychars,
            gralloc: None,
            headless_pointer: None,
            display: None,
            ui: None,
            ui_break: None,
            wayland: None,
            app_seccomp: true,
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

    /// Feeds atld a headless pointer script (format: atl-composer `input::parse_script`); the app injects it after its first frame. Only the input gate sets it.
    pub fn with_headless_pointer(mut self, script: &str) -> Self {
        self.headless_pointer = Some(script.to_owned());
        self
    }

    /// The display the run answers with (atld `--display`, `WxH@hz,dpi`).
    pub fn with_display(mut self, display: String) -> Self {
        self.display = Some(display);
        self
    }

    /// The app processes' UI mode (atld `--ui`, value `gtk`): every app process presents into a GTK window. Only the `gtk` window stage sets it.
    pub fn with_ui(mut self, mode: &str) -> Self {
        self.ui = Some(mode.to_owned());
        self
    }

    /// The app processes' composer breaks `rule` (a deliberate-failure run): `ATL_UI_BREAK` in their environment.
    pub fn with_ui_break(mut self, rule: &'static str) -> Self {
        self.ui_break = Some(rule);
        self
    }

    /// Atld's WAYLAND_DISPLAY, an absolute socket path, which the app processes inherit: the window gate's proxy.
    pub fn with_wayland(mut self, socket: &Path) -> Self {
        self.wayland = Some(socket.to_owned());
        self
    }

    /// Apps start without the app seccomp filter (atld `--no-app-seccomp`): the deliberate-failure check of the thread-priority rule (seccomp.rs).
    pub fn without_app_seccomp(mut self) -> Self {
        self.app_seccomp = false;
        self
    }
}

impl ApkTools {
    /// Builds the APK of `fixture` (AndroidManifest.xml and java/**.java) into `work`; `class_jars` are on javac's and d8's class path besides android.jar.
    /// `native` = (ABI, shared libraries): stored under `lib/<abi>/`, as an app's packager lays them out.
    pub fn build(
        &self,
        fixture: &Path,
        work: &Path,
        class_jars: &[PathBuf],
        native: Option<&(String, Vec<PathBuf>)>,
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
        let gen_dir = work.join("gen");
        let mut link = Command::new(&self.aapt2);
        link.arg("link")
            .arg("--manifest")
            .arg(fixture.join("AndroidManifest.xml"))
            .arg("-I")
            .arg(&self.android_jar)
            .arg("-o")
            .arg(&apk);
        // real resources (res/): compiled, linked, and R.java generated for javac
        if fixture.join("res").is_dir() {
            let compiled = work.join("res.zip");
            host_art::exec(
                Command::new(&self.aapt2)
                    .arg("compile")
                    .arg("--dir")
                    .arg(fixture.join("res"))
                    .arg("-o")
                    .arg(&compiled),
                "aapt2 compile",
            )?;
            std::fs::create_dir_all(&gen_dir)?;
            // positional: `-R` would make it an overlay
            link.arg(&compiled).arg("--java").arg(&gen_dir);
        }
        host_art::exec(&mut link, "aapt2 link")?;
        let mut sources = Vec::new();
        if fixture.join("java").is_dir() {
            host_art::collect(&fixture.join("java"), "java", &mut sources)?;
        }
        if gen_dir.is_dir() {
            host_art::collect(&gen_dir, "java", &mut sources)?;
        }
        // no Java at all (a `hasCode="false"` app, e.g. a pure NativeActivity one): no classes.dex, as aapt2 + the NDK's packager leave it
        let manifest = std::fs::read_to_string(fixture.join("AndroidManifest.xml"))?;
        check_code_policy(&manifest, !sources.is_empty())?;
        if !sources.is_empty() {
            self.dex_into(&apk, work, &classes, &dex, class_jars, &sources)?;
        }
        let jdk_bin = self.jdk.join("bin");
        if let Some((abi, libs)) = native {
            let staged = work.join("native");
            let dir = staged.join("lib").join(abi);
            std::fs::create_dir_all(&dir)?;
            for l in libs {
                let name = l.file_name().context("native library name")?;
                std::fs::copy(l, dir.join(name))
                    .with_context(|| format!("copying {}", l.display()))?;
            }
            // stored, so the APK also loads with extractNativeLibs=false (page-aligned mapping is bridge-1 step 7)
            host_art::exec(
                Command::new(jdk_bin.join("jar"))
                    .args(["--update", "--no-manifest", "--no-compress", "--file"])
                    .arg(&apk)
                    .arg("-C")
                    .arg(&staged)
                    .arg("lib"),
                "jar --update lib",
            )?;
        }
        Ok(apk)
    }

    /// Compiles `sources` (javac -> jar -> d8) and stores the dex in `apk` as `classes.dex`.
    fn dex_into(
        &self,
        apk: &Path,
        work: &Path,
        classes: &Path,
        dex: &Path,
        class_jars: &[PathBuf],
        sources: &[PathBuf],
    ) -> Result<()> {
        let classpath = std::env::join_paths(std::iter::once(&self.android_jar).chain(class_jars))?;
        let jdk_bin = self.jdk.join("bin");
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
                .arg(classes)
                .args(sources),
            "javac",
        )?;
        let jar = work.join("classes.jar");
        host_art::exec(
            Command::new(jdk_bin.join("jar"))
                .arg("cf")
                .arg(&jar)
                .arg("-C")
                .arg(classes)
                .arg("."),
            "jar",
        )?;
        let path = std::env::join_paths(std::iter::once(jdk_bin.clone()).chain(
            std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()),
        ))?;
        let mut d8 = Command::new(&self.d8);
        d8.env("PATH", path).arg("--lib").arg(&self.android_jar);
        for lib in class_jars {
            d8.arg("--lib").arg(lib);
        }
        host_art::exec(d8.arg("--output").arg(dex).arg(&jar), "d8")?;
        // the dex stored, as the platform's packagers store it (ART maps an uncompressed dex in place)
        host_art::exec(
            Command::new(jdk_bin.join("jar"))
                .args(["--update", "--no-manifest", "--no-compress", "--file"])
                .arg(apk)
                .arg("-C")
                .arg(dex)
                .arg("classes.dex"),
            "jar --update",
        )?;
        Ok(())
    }
}

impl Runtime {
    /// Starts atld for `run`, its stdout and stderr in `run.atld_log()`. The caller waits for it and ends it.
    pub fn start_atld(&self, run: &Run) -> Result<Child> {
        std::fs::create_dir_all(&run.scratch)?;
        // atld makes the root again itself, but only once it runs: a poll must not see the previous run's logs
        if run.root.exists() {
            std::fs::remove_dir_all(&run.root)
                .with_context(|| format!("removing {}", run.root.display()))?;
        }
        // the staged ART's library dir, the NativeBridge core's, then mapper.atl.so's (a PathBuf would take the absolute second as a replacement)
        let mut lib_path = std::ffi::OsString::from(self.art.lib_path.as_os_str());
        lib_path.push(":");
        lib_path.push(&self.native_bridge);
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
            .args(farm::KEYBOARD_FILES.iter().flat_map(|(file, dir)| {
                [
                    "--system-file".to_owned(),
                    format!("{}={dir}/{file}", self.keychars.join(file).display()),
                ]
            }))
            .args(["--first-uid", &run.first_uid.to_string()])
            .args(["--log-tags", &run.log_tags]);
        if let Some(script) = &self.headless_pointer {
            cmd.args(["--headless-pointer", script]);
        }
        if let Some(display) = &self.display {
            cmd.args(["--display", display]);
        }
        if let Some(mode) = &self.ui {
            // the compositor logs each frame's `present` line at debug (atl-composer ui_window.rs): the UI runs that read them enable it
            cmd.args(["--ui", mode])
                .env("ATL_UI_CAPTURE", "1")
                .env("RUST_LOG", "info,atl_composer=debug");
        }
        if let Some(rule) = self.ui_break {
            cmd.env("ATL_UI_BREAK", rule);
        }
        if let Some(socket) = &self.wayland {
            cmd.env("WAYLAND_DISPLAY", socket);
        }
        if !self.app_seccomp {
            cmd.arg("--no-app-seccomp");
        }
        for jar in &self.art.boot {
            cmd.arg("--boot-jar").arg(jar);
        }
        for (i, (apk, manifest)) in run.packages.iter().enumerate() {
            let file = run.scratch.join(format!("package-{i}.json"));
            std::fs::write(
                &file,
                serde_json::to_vec_pretty(&PackageSource {
                    apk,
                    manifest,
                    abis: &self.abis,
                })?,
            )?;
            cmd.arg("--package").arg(file);
        }
        // the platform's permission definitions, read from framework-res's manifest as an app's are (aapt2): atld's install grants from them
        let platform = run.scratch.join("platform-permissions.json");
        std::fs::write(
            &platform,
            serde_json::to_vec_pretty(&manifest::read_permissions(
                &self.apk.aapt2,
                &self.framework_res,
            )?)?,
        )?;
        cmd.arg("--platform-permissions").arg(platform);
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

/// Every counter line of the services' log, in log order: `services: unimplemented|refused|sent <interface>.<method> <n> at=<stamp>`
/// (atl_services::counters), and `parcelables: refused <what> <n> at=<stamp>` (atl_parcelables `refuse`). A line without a parsable stamp is not one.
pub fn events(services_log: &str) -> Vec<Event> {
    const KINDS: [(&str, &str); 4] = [
        ("services: unimplemented ", "unimplemented"),
        ("services: refused ", "refused"),
        ("services: sent ", "sent"),
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

/// The line activity_task.rs writes when an app reports its resume (`activityResumed`, sent after onResume returns): `services: activity resumed <class> at=<stamp>`.
const RESUMED_PREFIX: &str = "services: activity resumed";
/// The lines activity_task.rs writes on each change of the settled state (activity_stack::Machine::settled): nothing pending, or a start or lifecycle step still unacknowledged.
const SETTLED_PREFIX: &str = "services: activity settled at=";
const UNSETTLED_PREFIX: &str = "services: activity unsettled at=";

/// The activity an app reported resumed last: its class name and onResume's time.
pub struct Resumed {
    pub class: String,
    pub ms: u64,
}

/// The last resume line of the services' log, i.e. the activity the app is on now. None: no resume line, or its stamp does not parse.
pub fn last_resumed(services_log: &str) -> Option<Resumed> {
    let line = services_log
        .lines()
        .rev()
        .find(|l| l.starts_with(RESUMED_PREFIX))?;
    let (class, stamp) = line[RESUMED_PREFIX.len()..].rsplit_once(" at=")?;
    Some(Resumed {
        class: class.trim().to_owned(),
        ms: parse_stamp(stamp)?,
    })
}

/// Whether the services are settled now: the last settled-state line says so. No such line (no launch yet) is not settled.
pub fn settled(services_log: &str) -> bool {
    services_log
        .lines()
        .rev()
        .find(|l| l.starts_with(SETTLED_PREFIX) || l.starts_with(UNSETTLED_PREFIX))
        .is_some_and(|l| l.starts_with(SETTLED_PREFIX))
}

/// AOSP's install-time code policy (ScanPackageUtils.assertCodePolicy): `<application android:hasCode>` (default true) needs a dex in the APK, else INSTALL_FAILED_INVALID_APK "code is missing". A fixture without Java must say hasCode="false", so a packaged fixture is one a real device installs.
fn check_code_policy(manifest_xml: &str, has_dex: bool) -> Result<()> {
    const ANDROID: &str = "http://schemas.android.com/apk/res/android";
    let doc = roxmltree::Document::parse(manifest_xml)?;
    let declared = doc
        .descendants()
        .find(|n| n.has_tag_name("application"))
        .and_then(|a| a.attribute((ANDROID, "hasCode")))
        .is_none_or(|v| v != "false");
    anyhow::ensure!(
        has_dex || !declared,
        "the fixture has no Java but declares hasCode (default true): AOSP refuses the APK (ScanPackageUtils.assertCodePolicy: code is missing)"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// hasCode="false" packages without a dex; the default (absent) and "true" need one; with a dex every declaration passes (AOSP checks one direction only).
    #[test]
    fn code_policy_follows_has_code() {
        let m = |attr: &str| {
            format!(
                "<manifest xmlns:android=\"http://schemas.android.com/apk/res/android\" package=\"p\"><application {attr}/></manifest>"
            )
        };
        assert!(check_code_policy(&m("android:hasCode=\"false\""), false).is_ok());
        assert!(check_code_policy(&m(""), false).is_err());
        assert!(check_code_policy(&m("android:hasCode=\"true\""), false).is_err());
        for attr in ["", "android:hasCode=\"false\"", "android:hasCode=\"true\""] {
            assert!(check_code_policy(&m(attr), true).is_ok());
        }
    }

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
                   services: sent IInputMethodManager.IInputMethod.bindInput 1 at=100.005\n\
                   services: unimplemented IPackageManager.getMoveStatus 9\n";
        assert_eq!(
            counter_lines(log),
            [
                "refused android.content.Intent.extras x2",
                "refused android.os.Bundle (value type) x1",
                "sent IInputMethodManager.IInputMethod.bindInput x1",
                "unimplemented IActivityManager.getProcessLimit x2",
            ]
        );
        assert!(counter_lines("nothing\n").is_empty());
    }

    /// The last resume line gives the class and its stamp; a last line without a parsable stamp, a line of another kind, and no line at all give None.
    #[test]
    fn last_resumed_reads_the_last_resume_line() {
        let log = "services: ready\nservices: activity resumed org.x.Splash at=100.250\nservices: activity unsettled at=100.300\nservices: activity resumed org.x.Main at=101.000\nservices: activity settled at=101.001\n";
        let last = last_resumed(log).expect("two resume lines");
        assert_eq!((last.class.as_str(), last.ms), ("org.x.Main", 101_000));
        assert!(last_resumed("services: activity resumed org.x.Main\n").is_none());
        assert!(last_resumed("services: ready at=100.250\n").is_none());
        assert!(last_resumed("nothing\n").is_none());
    }

    /// The last settle line decides; a resume line alone is not settled.
    #[test]
    fn settled_is_the_last_settle_line() {
        assert!(settled(
            "services: activity unsettled at=1.0\nservices: activity settled at=2.0\n"
        ));
        assert!(!settled(
            "services: activity settled at=1.0\nservices: activity unsettled at=2.0\n"
        ));
        assert!(!settled("services: activity resumed org.x.Main at=1.0\n"));
    }
}
