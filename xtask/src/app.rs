// SPDX-License-Identifier: MIT
//! `xtask test app`: the M2 gate. The test APK (xtask/testdata/app: a manifest and one Activity whose onCreate logs [`MARKER`]) is started the Android way by atld: atl-binderd,
//! AOSP servicemanager, the M2 services (atl-system-services), then the services start the app through atld's process control (the broker's sandbox); ActivityThread attaches, is bound, launches the activity.
//! The APK is built here as an app is built (atld_run.rs); the package record atld installs is read back from it (`aapt2 dump xmltree`, manifest.rs), not restated.
//! Every run has the graphics allocator HAL (it must register with servicemanager, the VINTF-declared way) and the GL driver (`Runtime::with_gl_driver`).
//! Fonts sub-gate ([`FONTS`]): the app's text advances equal the pinned test fonts' hmtx sums, i.e. atld's font list (atl-fonts via fontconfig) reached Typeface; `--font-break` must fail it.
//! Pass: [`MARKER`] in the app's log within [`LIMIT`]. Always printed: the services' counters (unimplemented calls and refused parcelables, atld_run.rs `counter_lines`); on failure also the atld, app and services log tails.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use atl_layout::Layout;

use crate::atld_run::{Run, Runtime, children_outside_broker, counter_lines, counters};
use crate::gl_scene::{Judged, Scene};
use crate::test_fonts;
use crate::window::{self, WINDOW_MS};
use crate::{gl_probe, jni_fixture, manifest};

#[derive(clap::Args)]
pub struct Args {
    /// The host arch (an arches.toml row with `[host]`): the default --out.
    #[arg(long)]
    host: String,
    /// Native build directory holding ART, servicemanager and the services (default: this checkout's slot, see `xtask slot`; else <workspace>/out/native/<host>).
    #[arg(long)]
    out: Option<PathBuf>,
    /// Java pipeline build directory: boot jars, d8, aapt2 (default: this checkout's slot, see `xtask slot`; else <workspace>/out/java/build).
    #[arg(long)]
    java_out: Option<PathBuf>,
    /// liblog tag filter of the app (ANDROID_LOG_TAGS).
    #[arg(long, default_value = "*:i")]
    log_tags: String,
    /// Print the whole app log.
    #[arg(long, short)]
    verbose: bool,
    /// The test fonts' conf prefers the mono font for sans-serif, so the fonts sub-gate must fail (the gate's deliberate break).
    #[arg(long)]
    font_break: bool,
    /// The bridge-1 JNI fixture variant packaged, by API level = the APK's minSdk: 21 = emulated TLS, 29 = ELF TLS (jni_fixture.rs).
    #[arg(long, default_value_t = jni_fixture::EMULATED_TLS_API)]
    fixture_api: u32,
    /// The C12 break: leaves this native-bridge guest library (an NB_LIBS file, e.g. libstdc++.so or libz.so) out of the guest, so the legacy fixture's load fails naming it; the gate must then go red.
    #[arg(long)]
    drop_nb_lib: Option<String>,
    /// The SoundPool break: unlinks libsoundpool.so from the ART lib dir, so `new SoundPool` must fail naming it and the soundpool line must go red.
    #[arg(long)]
    soundpool_break: bool,
    /// The routing break: the legacy fixture is linked without DT_NEEDED `libc.so`, so the NativeBridge core refuses it, glibc's dlerror surfaces and the legacy rows must go red.
    #[arg(long)]
    legacy_no_libc: bool,
}

/// The fixture's dir.
const FIXTURE: &str = "xtask/testdata/app";
/// What Main.onCreate logs (tag `AtlTest`), as the launcher's logger writes it (`<tag>: <message>`).
const MARKER: &str = "AtlTest: onCreate reached";
/// Main.onCreate's CA line before [`MARKER`]: the default TrustManagerFactory trusts a distro root (atld's /system CA store, cacerts.rs); `cacerts: trusted ...` passes, `cacerts: FAIL ...` says why not.
const CACERTS: &str = "AtlTest: cacerts: ";
/// Main.onCreate's fonts line: `default=<adv> monospace=<adv>`, Typeface.DEFAULT's and MONOSPACE's advance of [`FONT_TEXT`] at text size 2048 (Main.FONT_SIZE), kerning/ligatures off.
/// Pass: each equals the pinned font's hmtx sum (test_fonts.rs `advance`, read-fonts): Roboto for the default (atld's fonts.toml sans-serif row through fontconfig), Droid Sans Mono for monospace.
const FONTS: &str = "AtlTest: fonts: ";
/// Main.onCreate's SoundPool line: `soundpool: constructed` when `new SoundPool` loaded libsoundpool.so and returned (the load gate, plan A-early), after its release; otherwise what it threw.
const SOUNDPOOL: &str = "AtlTest: soundpool: ";
/// The audio service's calls the SoundPool row makes (IAudioService, services.toml `audio`): the track at construction, the release. Each must be answered in the services' log.
const SOUNDPOOL_CALLS: [&str; 2] = ["trackPlayer", "releasePlayer"];
const FONT_TEXT: &str = "Handgloves";
/// Main.FONT_SIZE: at unitsPerEm every advance is whole pixels, so the comparison is exact; a font with another unitsPerEm fails the gate instead of being compared with rounding.
const FONT_SIZE: u16 = 2048;
/// What Main.onCreate logs before MARKER once the bridge-1 fixture (`libatl-jni-test.so`, an x86_64 bionic library) loaded: libnativeloader routed it to the NativeBridge core, whose guest linker loaded it.
const BRIDGE1: &str = "AtlTest: bridge1 loaded";
/// Main.bridge1Calls' lines: `AtlTest: bridge1 call <name> = <value>`.
const BRIDGE1_CALL: &str = "AtlTest: bridge1 call ";
/// (call, expected value), from the fixture's sources (xtask/testdata/jni-test, AtlJniTest.java): `tlsCounter` is a `__thread` counter per thread (1, 2, untouched by threadSum's worker: 3, a fresh Java thread's own: 1); threadSum = 1 + .. + 100; errno ENOENT = 2; atl_jni_sibling.c's answer 42; RegisterNatives' `registeredAnswer` 7; `sigsegvRecover` 1 (guest handler + siglongjmp ran); `nullFieldRead` "caught" (ART's null check still first, guest handler installed); `jitUpCall` 8000000 (AtlJniTest.UP_CALLS up-calls into JIT-compiled jitWork; derivation there).
const BRIDGE1_CALLS: &[(&str, &str)] = &[
    ("tlsCounter#1", "1"),
    ("tlsCounter#2", "2"),
    ("threadSum", "5050"),
    ("tlsCounter#3", "3"),
    ("tlsCounterFreshThread", "1"),
    ("errnoRoundTrip", "2"),
    ("cppThrowCatch", "1"),
    ("dlopenSibling", "42"),
    ("greet", "hello, bridge1"),
    ("callJavaAdd", "5"),
    ("registeredAnswer", "7"),
    ("sigsegvRecover", "1"),
    ("nullFieldRead", "caught"),
    ("jitUpCall", "8000000"),
    ("guestLog", "1"),
    // 10 (read_callback's guest callback ran once, on the caller's guest thread) + 1 (foreach's, every call): atl_jni_test.c propCallbacks
    ("propCallbacks", "11"),
    // GLES2 through the GL proxies: 64x64 pixels that equal the scene's geometry (jni-test atl_jni_test.c glesPbuffer)
    ("glesPbuffer", "4096"),
    // after glesPbuffer, the process maps the host driver (a /proc/self/maps line naming "mesa"): the proxies reached Mesa, not a stand-in
    ("mesaMapped", "1"),
    // S3 guest ES1 (plan gs3 S3): es1Begin makes an EGL10 pbuffer and ES1 context on this thread, as Happy Jump's GLSurfaceView does; es1Pixels is 1 when the scene ran with no GL error, and its rows are judged by es1_pixels
    ("es1Begin", "true"),
    ("es1Version", "1"),
    ("es1Driver", "1"),
    ("es1Pixels", "1"),
    ("es1End", "true"),
    // C12 legacy fixture (AtlLegacyTest.java, atl_jni_legacy.cpp): seven values in order: 17 (fprintf "legacy-stdout 42\n"), 1 (_CTYPE_U of 'A'), 6 (strlen "abcdef"), 3 (strchr 'd' offset), 2 (strlen after strncpy "xy"), 2 (fpclassify(NAN) = FP_NAN 0x02), 2 (errno ENOENT after a failing open)
    ("legacyStdio", "17 1 6 3 2 2 2"),
    // new int(40) through operator new/delete, then the guarded static g_seed + 2
    ("legacyNew", "40 42"),
    // compress + uncompress of "123456789", then crc32 of the result: the CRC-32 check value 0xCBF43926 (3421780262)
    ("zRoundTrip", "3421780262"),
    // C3 asset probe (atl_jni_test.c assetProbe): the 70000-byte assets, stored and deflated, streamed to the same SHA-256 F; the tail after seek 1000: remaining 69000 and SHA-256 T; AAsset_getBuffer of the stored one: F; a missing name: NULL. F and T: sha256sum over the bytes of an awk generator (`asset_row_matches_the_generated_bytes` derives them again from write_assets' bytes)
    (
        "assetProbe",
        "stream b56c58471e5e93f1329a5878b818e0cd23ab1a59925a7dc2b1576063a9109955 deflated b56c58471e5e93f1329a5878b818e0cd23ab1a59925a7dc2b1576063a9109955 seek 69000 df8a2c0f36b885db9597fe5aa4d4776d7068707f53d44b45efb587c405b15f86 buffer b56c58471e5e93f1329a5878b818e0cd23ab1a59925a7dc2b1576063a9109955 missing null",
    ),
    // C3 bitmap probe (AtlJniTest.bitmapProbe, 3x2 ARGB_8888: RGBA bytes, stride 12, format 1, flags 0 = ANDROID_BITMAP_FLAGS_ALPHA_PREMUL in bitmap.h): red, green, blue / white, black, gray 0x80
    (
        "bitmapArgb8888",
        "info 3 2 12 1 0 bytes ff0000ff00ff00ff0000ffffffffffff000000ff808080ff",
    ),
    // C3 bitmap probe, 3x2 RGB_565 (stride 6, format 4, flags 1 = ANDROID_BITMAP_FLAGS_ALPHA_OPAQUE in bitmap.h: no alpha): red, green, blue / white, black, yellow, as little-endian 565 words
    (
        "bitmapRgb565",
        "info 3 2 6 4 1 bytes 00f8e0071f00ffff0000e0ff",
    ),
];
/// The line atl_jni_test.c `guestLog` writes through the guest's liblog, as the launcher's logdw receiver records it (`<W> liblog/<tag>: <text>`, log.rs `format`): priority, tag and text all checked.
const GUEST_LOG: &str = " W liblog/AtlJniGuest: guest liblog 42";
/// The NativeBridge core's line at each thread's detach (atl-native-bridge engine/thread.rs `detach`): `attached A, detached D`. By the run's end every thread that ran guest code has exited except the main thread: A = D + 1.
const BRIDGE1_DETACH: &str = "atl-native-bridge: thread detached (attached ";
/// The log prefix of atl_jni_es1.c's lines, `row <y> <hex>` (RGBA bytes, row 0 first) and `version ...`: liblog writes ` W liblog/<tag>: <text>`, the tag `AtlEs1`.
const ES1_LOG: &str = "liblog/AtlEs1: ";
/// The C3 assets (game-support §2): `stored.bin` (aapt2 stores `.bin`: `-0 bin`) and `deflated.txt`, `ASSET_LEN` bytes each, byte i = (i*31+7) mod 251. The probe seeks to 1000 (atl_jni_test.c `ASSET_SEEK`).
const ASSET_LEN: usize = 70_000;
/// The bytes both assets hold.
fn asset_bytes() -> Vec<u8> {
    (0..ASSET_LEN).map(|i| ((i * 31 + 7) % 251) as u8).collect()
}
/// Writes the two assets into `dir`, which `Apk::build` packs as the APK's assets.
fn write_assets(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    let bytes = asset_bytes();
    std::fs::write(dir.join("stored.bin"), &bytes)?;
    std::fs::write(dir.join("deflated.txt"), &bytes)?;
    Ok(())
}

/// Every bridge-1 call whose line is missing or wrong, then the thread balance.
fn bridge1_call_failures(app: &str) -> Vec<String> {
    let got = |name: &str| {
        app.lines().find_map(|l| {
            let (_, r) = l.split_once(BRIDGE1_CALL)?;
            let (n, v) = r.split_once(" = ")?;
            (n == name).then(|| v.trim_end().to_owned())
        })
    };
    let mut whys: Vec<String> = BRIDGE1_CALLS
        .iter()
        .filter_map(|&(name, want)| match got(name) {
            Some(v) if v == want => None,
            other => Some(format!("bridge1 call {name}: want {want}, got {other:?}")),
        })
        .collect();
    if app.contains("called unimplemented OpenGL ES API") {
        whys.push("GL: the front reports an unimplemented OpenGL ES API call".to_owned());
    }
    if !app.lines().any(|l| l.ends_with(GUEST_LOG)) {
        whys.push(format!("guest liblog: no line ending {GUEST_LOG:?}"));
    }
    let last = app.lines().rev().find_map(|l| l.split_once(BRIDGE1_DETACH));
    let balance = last.and_then(|(_, r)| {
        let (a, d) = r
            .trim_end()
            .trim_end_matches(')')
            .split_once(", detached ")?;
        Some((a.parse::<u32>().ok()?, d.parse::<u32>().ok()?))
    });
    match balance {
        Some((a, d)) if a == d + 1 => {}
        other => whys.push(format!(
            "bridge1 threads: want attached = detached + 1 (the main thread), got {other:?}"
        )),
    }
    whys
}
/// The ES1 scene's pixels: the `row <y> <hex>` lines of the app log, judged by the scene's model (gl_scene.rs `judge`). Ok: the pixels compared.
fn es1_pixels(app: &str, scene: &Scene) -> Result<usize, String> {
    let mut rows = BTreeMap::new();
    for text in app
        .lines()
        .filter_map(|l| l.split_once(ES1_LOG).map(|(_, t)| t))
    {
        let Some(row) = text.strip_prefix("row ") else {
            continue;
        };
        let (y, hex) = row
            .split_once(' ')
            .ok_or_else(|| format!("row line without pixels: {text}"))?;
        let y: usize = y.parse().map_err(|e| format!("row number {y:?}: {e}"))?;
        if rows.insert(y, hex.trim_end().to_owned()).is_some() {
            return Err(format!("row {y} logged twice"));
        }
    }
    match scene.judge(&rows) {
        Ok(Judged::Match(n)) => Ok(n),
        Ok(Judged::Mismatch(why)) => Err(why),
        Ok(Judged::Missing(why)) => Err(format!("no readback: {why}")),
        Err(e) => Err(format!("{e:#}")),
    }
}

/// The test app's uid: AID_APP_START + 123, any app id.
const APP_UID: u32 = 10_123;
/// How long a cold start may take: ART without a boot image interprets framework.jar's class initialisation (the parcel stage's runs take seconds); 180 s for a loaded machine.
const LIMIT: Duration = Duration::from_secs(180);
/// App log lines shown on failure.
const TAIL: usize = 40;
fn tail(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

/// The fonts line against the pinned fonts' tables: one reason per mismatch.
fn font_check(layout: &Layout, line: Option<&str>) -> Result<Vec<String>> {
    let Some(line) = line else {
        return Ok(vec!["no fonts line".into()]);
    };
    let mut whys = Vec::new();
    for (key, font) in [
        ("default", test_fonts::ROBOTO),
        ("monospace", test_fonts::MONO),
    ] {
        let (want, upem) = test_fonts::advance(&test_fonts::file(layout, font), FONT_TEXT)?;
        if upem != FONT_SIZE {
            whys.push(format!(
                "{}: unitsPerEm {upem}, the fixture measures at {FONT_SIZE}",
                font.1
            ));
            continue;
        }
        let got = line
            .split_whitespace()
            .find_map(|w| w.strip_prefix(&format!("{key}=")))
            .and_then(|v| v.parse::<f32>().ok());
        if got != Some(want as f32) {
            whys.push(format!(
                "{key}: advance {got:?}, want {want} ({} hmtx)",
                font.1
            ));
        }
    }
    Ok(whys)
}

/// Native goals (gate_goals.rs): the runtime with gralloc, the GL driver and the native-bridge guest, as [`run`] stages it.
pub fn goals() -> crate::gate_goals::Goals {
    use crate::atld_run;
    atld_run::goals()
        .with(atld_run::gralloc_goals())
        .with(atld_run::nb_guest_goals())
}

pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    let layout = Layout::of(root)?;
    let scene = gl_probe::load_scene(root)?;
    anyhow::ensure!(
        [jni_fixture::EMULATED_TLS_API, jni_fixture::ELF_TLS_API].contains(&args.fixture_api),
        "--fixture-api: {} or {}",
        jni_fixture::EMULATED_TLS_API,
        jni_fixture::ELF_TLS_API
    );
    let rt = Runtime::new(
        root,
        "app",
        &args.host,
        args.out.as_deref(),
        args.java_out.as_deref(),
        "app-art",
        args.verbose,
    )?
    // the activity's window renders through hwui, which needs EGL: the allocator and the GL driver are part of every run (needs a DRM device that allocates, WSL: vgem)
    .with_gralloc(root)?
    .with_gl_driver()?
    .with_native_bridge_guest(&args.host)?;
    let rt = match &args.drop_nb_lib {
        Some(file) => rt.without_nb_lib(file)?,
        None => rt,
    };
    let rt = if args.soundpool_break {
        rt.without_art_lib("libsoundpool.so")?
    } else {
        rt
    };
    let rt = if args.font_break {
        rt.with_sans_serif(&layout, test_fonts::MONO.2)?
    } else {
        rt
    };
    let work = rt.out.join("app");
    // the bridge-1 JNI fixture rides along in lib/<abi>/ (jni_fixture.rs); the NativeBridge core loads it into the guest
    let native = jni_fixture::apk_libs(
        root,
        &args.host,
        args.fixture_api,
        &work.join("jni-fixture"),
        args.legacy_no_libc,
    )?;
    let assets = work.join("assets");
    write_assets(&assets)?;
    let apk = rt.apk.build(
        &root.join(FIXTURE),
        &work.join("apk"),
        &[],
        Some(&native),
        Some(&assets),
    )?;
    let m = manifest::read(&rt.apk.aapt2, &apk)?;
    let activity = m
        .launch_activity()
        .map(|a| a.name.clone())
        .context("the fixture has no launcher activity")?;
    anyhow::ensure!(
        i64::from(m.min_sdk) == i64::from(args.fixture_api),
        "APK minSdk {} != --fixture-api {}",
        m.min_sdk,
        args.fixture_api
    );
    println!(
        "app: built {} ({} versionCode {}, sdk {}..{}, launcher activity {})",
        apk.display(),
        m.name,
        m.version_code,
        m.min_sdk,
        m.target_sdk,
        activity
    );
    let run = Run {
        root: work.join("root"),
        scratch: work.join("scratch"),
        launch: Some(m.name.clone()),
        packages: vec![(apk, m.clone())],
        first_uid: APP_UID,
        libraries: vec![],
        shell: None,
        log_tags: args.log_tags.clone(),
    };
    let mut atld_child = rt.start_atld(&run)?;
    let app_log = run.log(&m.name);
    let start = Instant::now();
    let read = |p: &Path| std::fs::read_to_string(p).unwrap_or_default();
    let (passed, ended) = loop {
        if read(&app_log).lines().any(|l| l.contains(MARKER)) {
            break (true, None);
        }
        if let Some(s) = atld_child.try_wait()? {
            break (read(&app_log).lines().any(|l| l.contains(MARKER)), Some(s));
        }
        // the app's end is the run's end here, as when atld ended with it
        if run.app_ended() {
            let s = run.stop(&mut atld_child)?;
            break (read(&app_log).lines().any(|l| l.contains(MARKER)), Some(s));
        }
        if start.elapsed() > LIMIT {
            break (false, None);
        }
        std::thread::sleep(Duration::from_millis(200));
    };
    let elapsed = start.elapsed();
    // while atld runs with everything started: each child of it must be a broker launch (plan F1.2 B.4, runtime side of the clippy gate)
    let outside = match ended {
        None => Some(children_outside_broker(atld_child.id())?),
        Some(_) => None,
    };
    // the watch after the marker: a crash shows within it (window.rs WINDOW_MS); the app's death ends the watch, atld outlives it
    let mut ended = ended;
    if passed && ended.is_none() {
        let watch = Instant::now();
        while ended.is_none() && watch.elapsed() < Duration::from_millis(WINDOW_MS) {
            ended = atld_child.try_wait()?;
            if ended.is_none() && run.app_ended() {
                ended = Some(run.stop(&mut atld_child)?);
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    if ended.is_none() {
        run.stop(&mut atld_child)?;
    }
    let app = read(&app_log);
    let fault = fault(&app, ended.is_some());
    let services_log = read(&run.log("services"));
    if args.verbose {
        println!("---- app log\n{app}");
    }
    for l in counter_lines(&services_log) {
        println!("app: counter {l}");
    }
    // atld waits for the allocator's ready line before it starts the app, so a started app means a registered allocator; the line is checked anyway, with the allocator's own log as the explanation
    let gralloc_log = read(&run.log("gralloc"));
    let gralloc_ok = gralloc_log.contains("gralloc: ready");
    let cacerts = app
        .lines()
        .find_map(|l| l.split_once(CACERTS).map(|(_, r)| r.to_owned()))
        .unwrap_or_else(|| "FAIL no cacerts line".to_owned());
    let bridge1 = app.lines().find(|l| l.contains(BRIDGE1));
    let bridge1_ok = bridge1.is_some();
    let mut whys = jni_fixture::legacy_failures(&native.libs)?;
    whys.extend(jni_fixture::ndk_failures(&native.libs)?);
    whys.extend(jni_fixture::es1_failures(&native.libs)?);
    if let Some(f) = &fault {
        whys.push(format!("the app faulted: {f}"));
        println!("---- app log (last {TAIL} lines)\n{}", tail(&app, TAIL));
    }
    if passed && !cacerts.starts_with("trusted ") {
        whys.push(format!("CA store: {cacerts}"));
    }
    match &outside {
        Some(o) => {
            println!("app: counter children_outside_broker {}", o.len());
            if !o.is_empty() {
                whys.push(format!("atld children outside the broker: {o:?}"));
            }
        }
        None if passed => whys.push(
            "children_outside_broker not measured: atld had ended before the snapshot".into(),
        ),
        None => {}
    }
    let fonts_line = app
        .lines()
        .find_map(|l| l.split_once(FONTS).map(|(_, r)| r.to_owned()));
    if passed {
        whys.extend(
            font_check(&layout, fonts_line.as_deref())?
                .into_iter()
                .map(|w| format!("fonts: {w}")),
        );
    }
    let soundpool = app
        .lines()
        .find_map(|l| l.split_once(SOUNDPOOL).map(|(_, r)| r.to_owned()));
    if passed && soundpool.as_deref() != Some("constructed") {
        whys.push(format!("soundpool: want constructed, got {soundpool:?}"));
    }
    // the row's release: releasePlayer is oneway, so the app sees no error; the audio service's own count shows that the track and the release reached it and were served
    if passed {
        let served = counters(&services_log);
        for method in SOUNDPOOL_CALLS {
            let want = format!("IAudioService.{method}");
            if !served
                .iter()
                .any(|c| c.kind == "answered" && c.name == want && c.count > 0)
            {
                whys.push(format!(
                    "soundpool: the audio service did not answer {method}"
                ));
            }
        }
    }
    if passed && !bridge1_ok {
        whys.push(format!(
            "bridge1: want {BRIDGE1:?}, got {:?}",
            bridge1.or_else(|| app.lines().find(|l| l.contains("AtlTest: bridge1")))
        ));
    }
    let mut es1_compared = None;
    if passed && bridge1_ok {
        whys.extend(bridge1_call_failures(&app));
        match es1_pixels(&app, &scene) {
            Ok(n) => es1_compared = Some(n),
            Err(why) => whys.push(format!("es1 pixels: {why}")),
        }
    }
    if !whys.is_empty() {
        for why in &whys {
            println!("app: FAIL {why}");
        }
        println!("{}", crate::verdict::gate("app", &whys));
        return Ok(ExitCode::FAILURE);
    }
    if passed {
        println!("app: pass {}", bridge1.unwrap_or_default().trim());
    }
    if passed && gralloc_ok {
        println!("app: cacerts {cacerts}");
        println!(
            "app: pass fonts {} = pinned hmtx",
            fonts_line.unwrap_or_default()
        );
        println!(
            "app: pass soundpool {} (the audio service answered {})",
            soundpool.unwrap_or_default(),
            SOUNDPOOL_CALLS.join(", ")
        );
        if let Some(n) = es1_compared {
            println!(
                "app: pass es1 {n} pixels match the scene model (libGLESv1_CM.so through the proxies)"
            );
        }
        println!("app: pass ({activity} Activity.onCreate logged after {elapsed:.1?})");
        println!("app: gralloc registered: {}", gralloc_log.trim());
        return Ok(ExitCode::SUCCESS);
    }
    if passed {
        println!(
            "app: FAIL the allocator did not register:\n{}",
            gralloc_log.trim()
        );
        let why = format!("the allocator did not register: {}", gralloc_log.trim());
        println!(
            "{}",
            crate::verdict::gate("app", std::slice::from_ref(&why))
        );
        return Ok(ExitCode::FAILURE);
    }
    let why = match ended {
        Some(s) => format!("atld exited {s} before the marker"),
        None if elapsed > LIMIT => format!("no marker after {LIMIT:?}"),
        None => "no marker".to_owned(),
    };
    println!("app: FAIL {why}");
    println!("---- atld log\n{}", tail(&read(&run.atld_log()), TAIL));
    println!("---- app log (last {TAIL} lines)\n{}", tail(&app, TAIL));
    println!(
        "---- services log (last {TAIL} lines)\n{}",
        tail(&services_log, TAIL)
    );
    println!(
        "{}",
        crate::verdict::gate("app", std::slice::from_ref(&why))
    );
    Ok(ExitCode::FAILURE)
}

/// The app's first fault up to the watch's end (marker + WINDOW_MS), from the log's start: the app's other threads run before onCreate (hwui's RenderThread made its EGL context 50 ms before the marker when it aborted, round-4 run). None without a marker.
fn fault(app_log: &str, atld_exited: bool) -> Option<String> {
    let marker_ms = window::verdict(app_log, MARKER, atld_exited, WINDOW_MS).marker_ms?;
    window::fault_after(app_log, 0, marker_ms + WINDOW_MS, marker_ms, atld_exited)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::window::stamp_text;

    const T: u64 = 1_000_000;

    fn line(at: u64, prio: &str, text: &str) -> String {
        format!(
            "{} 34636.4 2 14 {prio} liblog/Tag: {text}\n",
            stamp_text(at)
        )
    }

    #[test]
    fn clean_run_has_no_fault() {
        assert_eq!(fault(&line(T, "I", MARKER), false), None);
    }

    #[test]
    fn abort_before_the_marker_is_a_fault() {
        // the round-4 shape: RenderThread's F line, then the main thread's marker
        let log = line(
            T - 50,
            "F",
            "HWUI: Failed to create context, error = EGL_BAD_ATTRIBUTE",
        ) + &line(T, "I", MARKER);
        assert!(fault(&log, false).is_some_and(|f| f.contains("fatal log line")));
    }

    #[test]
    fn death_inside_the_watch_is_a_fault() {
        assert!(fault(&line(T, "I", MARKER), true).is_some());
        let crash = line(T, "I", MARKER) + &line(T + 100, "E", "FATAL EXCEPTION: main");
        assert!(fault(&crash, false).is_some());
    }

    #[test]
    fn asset_row_matches_the_generated_bytes() {
        // the row's digests, recomputed over the bytes write_assets writes (sha2, not the guest's C SHA-256); seek: atl_jni_test.c's ASSET_SEEK
        let seek = 1_000;
        let bytes = asset_bytes();
        let whole = crate::test_apks::sha256_hex(&bytes);
        let tail = crate::test_apks::sha256_hex(&bytes[seek..]);
        let want = format!(
            "stream {whole} deflated {whole} seek {} {tail} buffer {whole} missing null",
            ASSET_LEN - seek
        );
        let row = BRIDGE1_CALLS
            .iter()
            .find(|(n, _)| *n == "assetProbe")
            .map(|(_, v)| *v);
        assert_eq!(row, Some(want.as_str()));
    }

    /// The app-log lines atl_jni_es1.c writes for `px`: one `row` line each, in liblog's form.
    fn es1_log(px: &[crate::gl_scene::Pixel], w: usize, h: usize) -> String {
        (0..h)
            .map(|y| {
                let hex: String = (0..w)
                    .flat_map(|x| px[y * w + x].rgba)
                    .map(|b| format!("{b:02x}"))
                    .collect();
                format!(" W liblog/AtlEs1: row {y} {hex}\n")
            })
            .collect()
    }

    #[test]
    fn es1_rows_judge_against_the_scene_model() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/gl-probe/es1-scene.toml");
        let scene = Scene::load(&path).expect("the ES1 scene parses");
        let (w, h) = scene.size();
        let px = scene.expect().expect("the model runs");
        let compared = px.iter().filter(|p| !p.edge).count();
        let log = es1_log(&px, w, h);
        assert_eq!(es1_pixels(&log, &scene), Ok(compared));
        // one flipped pixel inside the flat quad [8,24)^2 is judged and named
        let mut bad = px.clone();
        bad[16 * w + 16].rgba[1] ^= 0x10;
        let why = es1_pixels(&es1_log(&bad, w, h), &scene).expect_err("a flipped pixel fails");
        assert!(why.contains("(16,16)"), "{why}");
        // a row missing from the log has no readback
        let missing: String = log
            .lines()
            .filter(|l| !l.contains("row 63 "))
            .map(|l| format!("{l}\n"))
            .collect();
        assert!(es1_pixels(&missing, &scene).is_err_and(|e| e.contains("no readback")));
        // a row logged twice is refused
        let first = log.lines().next().expect("a row").to_owned();
        assert_eq!(
            es1_pixels(&format!("{log}{first}\n"), &scene),
            Err("row 0 logged twice".to_owned())
        );
    }
}
