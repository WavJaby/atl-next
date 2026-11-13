// SPDX-License-Identifier: MIT
//! `xtask test app`: the M2 gate. The test APK (xtask-harness/testdata/app: a manifest and one Activity whose onCreate logs [`MARKER`]) is started the Android way by atld: atl-binderd,
//! AOSP servicemanager, the M2 services (atl-system-services), then the services start the app through atld's process control (the broker's sandbox); ActivityThread attaches, is bound, launches the activity.
//! The APK is built here as an app is built (atld_run.rs); the package record atld installs is read back from it (`aapt2 dump xmltree`, manifest.rs), not restated.
//! Every run has the graphics allocator HAL (it must register with servicemanager, the VINTF-declared way) and the GL driver (`Runtime::with_gl_driver`).
//! Fonts sub-gate ([`FONTS`]): the app's text advances equal the pinned test fonts' hmtx sums, i.e. atld's font list (atl-fonts via fontconfig) reached Typeface; `--font-break` must fail it.
//! Config row ([`FIRST_CONFIG`]): the first config eglChooseConfig lists for RGB 8 minimum on pbuffer surfaces reports a FIXED Android pixel format (no float config for a request that omits the component type). The GL driver's break `ATL_EGL_BREAK=no_config_filter` (set in this run's environment, staged with the driver) must fail it.
//! Pass: [`MARKER`] in the app's log within [`LIMIT`], then the startActivityForResult round trip after it ([`result_failures`]; `--result-break` must fail it). Always printed: the services' counters (unimplemented calls and refused parcelables, atld_run.rs `counter_lines`); on failure also the atld, app and services log tails.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use atl_layout::Layout;

use crate::atld_run::{Package, Run, Runtime, children_outside_broker, counter_lines, counters};
use crate::gl_scene::{Judged, Scene};
use crate::native_load::PLAYER_STEPS;
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
    /// The fixture's ISA (an arches.toml row with `[guest]`), default the host's (identity). A foreign one ships only that ABI's libraries, so the app runs under that ISA's QEMU engine (staged with its guest), and the calls the engine does not carry yet are skipped and listed (`QEMU_DEFERRED`).
    /// Needs the ISA's NB guest built (`xtask native --native-bridge <isa>`).
    #[arg(long)]
    fixture_arch: Option<String>,
    /// The C12 break: leaves this native-bridge guest library (an NB_LIBS file, e.g. libstdc++.so or libz.so) out of the guest, so the legacy fixture's load fails naming it; the gate must then go red.
    #[arg(long)]
    drop_nb_lib: Option<String>,
    /// The SoundPool break: unlinks libsoundpool.so from the ART lib dir, so `new SoundPool` must fail naming it and the soundpool line must go red.
    #[arg(long)]
    soundpool_break: bool,
    /// The MediaPlayer break: unlinks libmedia_jni.so from the ART lib dir, so `new MediaPlayer` must fail and the mediaplayer lines must go red.
    #[arg(long)]
    mediaplayer_break: bool,
    /// The per-service break: the services do not register this [`SERVICE_LINES`] service (repeat), so its manager is null and its line must go red.
    #[arg(long)]
    omit_service: Vec<String>,
    /// The media player break: the app starts with `--no-media-player-service` (atld, then the launcher), so the view answers no media.player; the probe's lookup is never answered and its line reads "no answer", so the gate must go red.
    #[arg(long)]
    media_player_break: bool,
    /// The GL selection break: packages install with the native GL driver (atld `--no-gl-selection`), so the ES1 scene's EGL10 context cannot be made and the es1 lines must go red.
    #[arg(long)]
    gl_selection_break: bool,
    /// The result break: the services owe no start its result (atld `--no-activity-result`), so Main's startActivityForResult answers never arrive and the result lines must go red.
    #[arg(long)]
    result_break: bool,
    /// The routing break: the legacy fixture is linked without DT_NEEDED `libc.so`, so the NativeBridge core refuses it, glibc's dlerror surfaces and the legacy rows must go red.
    #[arg(long)]
    legacy_no_libc: bool,
}

/// The fixture's dir.
const FIXTURE: &str = "xtask-harness/testdata/app";
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
/// Main.onCreate's MediaPlayer lines: `AtlTest: mediaplayer: <step> ok`, or `<step> <throwable>`, one per step of native_load.rs PLAYER_STEPS (Happy Jump's GameShared.loadMusic sequence).
const MEDIAPLAYER: &str = "AtlTest: mediaplayer: ";
/// AOSP's ALOGE in MediaPlayer::start for a player in Idle (mediaplayer.cpp:364, tag MediaPlayerNative, state 1 = MEDIA_PLAYER_IDLE): start() reached the native player and was refused there. Java's _start throws nothing for that (android_media_MediaPlayer.cpp), so this line is the only evidence the native path ran.
const MEDIAPLAYER_START: &str = "MediaPlayerNative: start called in state 1";
/// Main.onCreate's lines for services an app reaches through a manager that is null without them: (servicemanager name, line prefix, expected value). A missing service logs `absent`.
/// connectivity: a device with no network agent (ConnectivityService.getActiveNetworkInfo -> null, isActiveNetworkMetered -> true).
/// permissionmgr: PackageManager.checkPermission for the fixture itself: ACCESS_NETWORK_STATE is requested + NORMAL -> granted (0, permission.rs rule A); CAMERA is not requested -> denied (-1).
const SERVICE_LINES: [(&str, &str, &str); 2] = [
    (
        "connectivity",
        "AtlTest: connectivity: ",
        "active=null metered=true",
    ),
    (
        "permissionmgr",
        "AtlTest: permission: ",
        "network_state=0 camera=-1",
    ),
];
/// The native bridge's report of a fault handed to a guest handler (identity/mod.rs fault_report via signal.rs guest_handler); bridge1's sigsegvRecover must produce one (SIGSEGV = 11).
const GUEST_FAULT_REPORT: &str =
    "atl-native-bridge: fatal-signal report: fault handed to the guest's handler: sig 11 ";
/// Main.mediaPlayerService's line: "returned" when the view answered the lookup and setDataSource(fd) returned, "no answer" when nothing did. A return means an answer (getMediaPlayerService waits for media.player, IMediaDeathNotifier.cpp:41); a native setDataSource error is logged and dropped by MediaPlayer.java:1305, so a return does not prove setDataSource ran: MEDIA_WAV is that check.
const MEDIA_PLAYER_SERVICE: &str = "AtlTest: media.player: ";
/// The probe's line when its lookup was answered and setDataSource(fd) returned (MEDIA_PLAYER_SERVICE).
const MEDIA_PLAYER_ANSWERED: &str = "returned";
/// Main.mediaPlayerWav's lines: `round <i> ok <ms> ms` per round, `round <i> <step> <throwable>` when a step threw, `no answer within <ms> ms at <step>` when a round never
/// returned. Rounds alternate fd and path (Main.WAV_ROUNDS). A pass means the served service sniffed and extracted a valid WAV (prepare throws without an extractor).
/// This replaces the failure-text marker "initFromDataSource, cannot create extractor!": GenericSource prints it on every stored.bin probe, pass or fail (GenericSource.cpp:181).
const MEDIA_WAV: &str = "AtlTest: mediaplayer-wav: ";
const MEDIA_WAV_ROUNDS: usize = 4;
/// Main.mediaPlayerReset's lines: `round <i> ok <ms> ms` per reset, `round <i> <step> <throwable>`, or `no answer within <ms> ms at <step>` when a round never returned.
const MEDIA_RESET: &str = "AtlTest: mediaplayer-reset: ";
const MEDIA_RESET_ROUNDS: usize = 4;
/// Main.ROUND_BOUND_MS: the most one round of the WAV and reset probes may take; a probe whose worker outlives rounds x this is named by the probe.
const MEDIA_ROUND_BOUND_MS: u64 = 10_000;
/// The startActivityForResult round trip (Main.java, ResultChild.java): after the marker Main starts ResultChild for request 7; its RESULT_OK (-1) and extra come back in
/// onActivityResult, then Main's singleTop self-start cancels request 9 (RESULT_CANCELED, 0) before its new intent. Lines are `AtlTest: result request=<r> code=<c> extra=<e>`.
const RESULT: &str = "AtlTest: result ";
const RESULT_CHILD: &str = "request=7 code=-1 extra=atl-result-7";
const RESULT_CANCELED: &str = "request=9 code=0 extra=none";
const CHILD_FINISHING: &str = "AtlTest: result child finishing";
const MAIN_RESUMED: &str = "AtlTest: main resumed";
const MAIN_NEW_INTENT: &str = "AtlTest: main new intent";
/// How long the watch after the marker waits for the round trip's last line ([`MAIN_NEW_INTENT`]) once WINDOW_MS has passed. The gate reads the log without it after this, so a slow host
/// gets this long and a broken round trip fails after it.
const RESULT_WAIT: Duration = Duration::from_secs(30);
const FONT_TEXT: &str = "Handgloves";
/// Main.FONT_SIZE: at unitsPerEm every advance is whole pixels, so the comparison is exact; a font with another unitsPerEm fails the gate instead of being compared with rounding.
const FONT_SIZE: u16 = 2048;
/// What Main.onCreate logs before MARKER once the bridge-1 fixture (`libatl-jni-test.so`, an x86_64 bionic library) loaded: libnativeloader routed it to the NativeBridge core, whose guest linker loaded it.
const BRIDGE1: &str = "AtlTest: bridge1 loaded";
/// Main.bridge1Calls' lines: `AtlTest: bridge1 call <name> = <value>`.
const BRIDGE1_CALL: &str = "AtlTest: bridge1 call ";
/// (call, expected value), from the fixture's sources (xtask-harness/testdata/jni-test, AtlJniTest.java): `tlsCounter` is a `__thread` counter per thread (1, 2, untouched by threadSum's worker: 3, a fresh Java thread's own: 1); threadSum = 1 + .. + 100; errno ENOENT = 2; atl_jni_sibling.c's answer 42; RegisterNatives' `registeredAnswer` 7; `sigsegvRecover` 1 (guest handler + siglongjmp ran); `nullFieldRead` "caught" (ART's null check still first, guest handler installed); `jitUpCall` 8000000 (AtlJniTest.UP_CALLS up-calls into JIT-compiled jitWork; derivation there).
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
    // after glesPbuffer, the process maps libGLES_atl.so (a /proc/self/maps line naming it): the proxies reached the driver, not a stand-in
    ("driverMapped", "1"),
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
    // tr-first blockers 2-3 (atl_jni_test.c windowProbe, Main.probeSurface): a SurfaceTexture producer with setDefaultBufferSize(48, 32); width and height are BufferQueueProducer::query's mDefaultWidth/Height (Surface::query forwards NATIVE_WINDOW_WIDTH/HEIGHT), format 1 = BufferQueueCore's default PIXEL_FORMAT_RGBA_8888 (Surface.cpp: no mReqFormat yet)
    ("windowProbe", "window 48 32 1"),
    // G6b: the fixture library (DT_NEEDED libmediandk.so) loaded, so the guest found the libmediandk proxy; 1 = C's `f != NULL` for AMediaFormat_new (atl_jni_test.c mediandkLinked)
    ("mediandkLinked", "1"),
    // net-bp (engine/netdb.rs; atl_jni_test.c resolverProbe): getaddrinfo / freeaddrinfo / getnameinfo / gai_strerror through the bridge on both engines; the failures carry bionic's EAI_* codes (service 9, numerichost 8), the loop is 1000 lookups with every node freed; herr 1 = a failed gethostbyname left HOST_NOT_FOUND or TRY_AGAIN in the guest's h_errno
    (
        "resolverProbe",
        "numeric 1 passive 1 service 9 numerichost 8 text 1 loop 1000 list ok herr 1",
    ),
    // tr-first blocker 5 (atl_jni_test.c looperNow / looperArm / looperLater; engine/ndk.rs `call_mapped` hands the host a down shell for the guest callback): (a) one call, on the calling thread, pollOnce = ALOOPER_POLL_CALLBACK (-2, looper.h), no wrong user data; (b) the main Looper's later call of the callback a fresh thread registered: one call, on the main thread, which is not the arming one, no wrong user data
    ("looperNow", "now 1 1 -2 0"),
    ("looperArm", "armed"),
    ("looperLater", "later 1 1 1 0"),
];
/// Main.bridge1Calls' firstConfig row: `r=<r> g=<g> b=<b> a=<a> visual=<v>`, the first config eglChooseConfig lists for RGB 8 minimum on pbuffer surfaces (atl_jni_test.c firstConfig).
/// The driver orders the list as the host does, so the row is judged ([`first_config_why`]), not compared with a fixed line.
const FIRST_CONFIG: &str = "firstConfig";
/// The Android pixel formats (hardware/interfaces/graphics/common/aidl PixelFormat.aidl) the row's visual may be, all FIXED: RGBA_8888 0x1, RGBX_8888 0x2, RGB_565 0x4, RGBA_1010102 0x2B.
/// A driver lists only configs that map to an Android format (real Android drivers do the same). RGBA_FP16 (0x16) is out: the row's request omits EGL_COLOR_COMPONENT_TYPE_EXT, whose default is FIXED, so its first config is never a float one.
const FIXED_FORMATS: [i32; 4] = [0x1, 0x2, 0x4, 0x2B];
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
/// Writes the two assets into `dir`, which `Apk::build` packs as the APK's assets, and [`SKIP_ASSET`] naming the `deferred` calls when there are any (removed otherwise: `dir` outlives runs).
fn write_assets(dir: &Path, deferred: &[&str]) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    let bytes = asset_bytes();
    std::fs::write(dir.join("stored.bin"), &bytes)?;
    std::fs::write(dir.join("deflated.txt"), &bytes)?;
    let skip = dir.join(SKIP_ASSET);
    if deferred.is_empty() {
        if skip.exists() {
            std::fs::remove_file(&skip)?;
        }
    } else {
        std::fs::write(&skip, deferred.join("\n") + "\n")?;
    }
    Ok(())
}

/// Main's line value for a skipped call (Main.java `bridge1Call`).
const SKIPPED: &str = "skipped";
/// The asset naming the calls Main skips, one label a line (Main.java SKIP_ASSET); written only for a run with [`deferred`] calls.
const SKIP_ASSET: &str = "bridge1-skip.txt";
/// The [`BRIDGE1_CALLS`] labels (and [`FIRST_CONFIG`], `guestLog`) the QEMU engine does not carry yet (QEMU Q6 plan Q6-6 subset; the rest is Q7): C++ exceptions through the guest unwinder, guest signals,
/// the JIT up-call and guest liblog / property callbacks (not in the subset), GL through the proxies, and the C12 legacy fixture. Each is skipped by Main and must log `skipped`.
const QEMU_DEFERRED: &[&str] = &[
    "cppThrowCatch",
    "sigsegvRecover",
    "nullFieldRead",
    "jitUpCall",
    "guestLog",
    "propCallbacks",
    FIRST_CONFIG,
    "glesPbuffer",
    "driverMapped",
    "es1Begin",
    "es1Version",
    "es1Driver",
    "es1Pixels",
    "es1End",
    "legacyStdio",
    "legacyNew",
    "zRoundTrip",
];
/// The calls a run of fixture ISA `isa` on host `host` skips: none for the host's (identity), [`QEMU_DEFERRED`] for a foreign one.
fn deferred(isa: &str, host: &str) -> &'static [&'static str] {
    if isa == host { &[] } else { QEMU_DEFERRED }
}

/// Every bridge-1 call whose line is missing or wrong (a `deferred` label's line must be `skipped`), then the thread balance.
fn bridge1_call_failures(app: &str, deferred: &[&str]) -> Vec<String> {
    let got = |name: &str| {
        app.lines().find_map(|l| {
            let (_, r) = l.split_once(BRIDGE1_CALL)?;
            let (n, v) = r.split_once(" = ")?;
            (n == name).then(|| v.trim_end().to_owned())
        })
    };
    let mut whys: Vec<String> = BRIDGE1_CALLS
        .iter()
        .map(|&(name, want)| {
            (
                name,
                if deferred.contains(&name) {
                    SKIPPED
                } else {
                    want
                },
            )
        })
        .filter_map(|(name, want)| match got(name) {
            Some(v) if v == want => None,
            other => Some(format!("bridge1 call {name}: want {want}, got {other:?}")),
        })
        .collect();
    if deferred.contains(&FIRST_CONFIG) {
        if got(FIRST_CONFIG).as_deref() != Some(SKIPPED) {
            whys.push(format!(
                "bridge1 call {FIRST_CONFIG}: want {SKIPPED}, got {:?}",
                got(FIRST_CONFIG)
            ));
        }
    } else if let Some(why) = first_config_why(got(FIRST_CONFIG).as_deref()) {
        whys.push(format!("bridge1 call {FIRST_CONFIG}: {why}"));
    }
    if app.contains("called unimplemented OpenGL ES API") {
        whys.push("GL: the front reports an unimplemented OpenGL ES API call".to_owned());
    }
    if !deferred.contains(&"guestLog") && !app.lines().any(|l| l.ends_with(GUEST_LOG)) {
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
/// The firstConfig row's judge: its visual (EGL_NATIVE_VISUAL_ID) must be one of [`FIXED_FORMATS`]. The sizes sit in the reason, so a failure shows which config came first. `None`: the row passes.
fn first_config_why(value: Option<&str>) -> Option<String> {
    let Some(value) = value else {
        return Some("no line".to_owned());
    };
    let visual = value
        .split_whitespace()
        .find_map(|w| w.strip_prefix("visual="))
        .and_then(|v| v.parse::<i32>().ok());
    match visual {
        Some(v) if FIXED_FORMATS.contains(&v) => None,
        _ => Some(format!(
            "{value:?}: visual {visual:?}, want one of {FIXED_FORMATS:?}"
        )),
    }
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

/// Whether a media.player lookup was logged between Happy Jump's first and last MediaPlayer line: its steps run in that window, and the media.player probe starts after it.
fn mediaplayer_looked_up(app: &str) -> bool {
    let lines: Vec<&str> = app.lines().collect();
    let first = lines.iter().position(|l| l.contains(MEDIAPLAYER));
    let last = lines.iter().rposition(|l| l.contains(MEDIAPLAYER));
    match (first, last) {
        (Some(first), Some(last)) => lines[first..=last]
            .iter()
            .any(|l| l.contains("media.player")),
        _ => false,
    }
}

/// Main.onCreate's MediaPlayer lines in order, joined: `new ok; setLooping ok; ...`.
fn mediaplayer_steps(app: &str) -> String {
    app.lines()
        .filter_map(|l| l.split_once(MEDIAPLAYER).map(|(_, r)| r.trim_end()))
        .collect::<Vec<_>>()
        .join("; ")
}

/// The MediaPlayer lines against PLAYER_STEPS: one reason per step whose line is missing or does not start with what the sequence must print.
fn player_failures(app: &str) -> Vec<String> {
    PLAYER_STEPS
        .iter()
        .filter_map(|&(step, want)| {
            let got = app.lines().find_map(|l| {
                let (_, r) = l.split_once(MEDIAPLAYER)?;
                let r = r.strip_prefix(step)?.strip_prefix(' ')?;
                Some(r.trim_end().to_owned())
            });
            match got {
                Some(r) if r.starts_with(want) => None,
                other => Some(format!("mediaplayer {step}: want {want}, got {other:?}")),
            }
        })
        .collect()
}

/// The rounds of a Main probe (`prefix` = its line prefix): round i must log `round <i> ok <ms> ms` in order, within `bound_ms` when given. A round whose step threw names
/// that step's throwable; `no answer` names the round a reset never returned; a probe short of `rounds` names where it stopped.
fn round_failures(app: &str, prefix: &str, rounds: usize, bound_ms: Option<u64>) -> Vec<String> {
    let name = prefix.trim_end().trim_end_matches(':');
    let mut whys = Vec::new();
    let mut done = 0;
    for line in app
        .lines()
        .filter_map(|l| l.split_once(prefix).map(|(_, r)| r.trim_end()))
    {
        let ok = line
            .strip_prefix("round ")
            .and_then(|r| r.split_once(" ok "))
            .and_then(|(n, r)| {
                Some((
                    n.parse::<usize>().ok()?,
                    r.strip_suffix(" ms")?.parse::<u64>().ok()?,
                ))
            });
        let in_bound = match bound_ms {
            Some(b) => ok.is_some_and(|(_, ms)| ms <= b),
            None => true,
        };
        match ok {
            Some((n, _)) if n == done && in_bound => done += 1,
            _ if line.starts_with("no answer") => {
                whys.push(format!("{name}: {line}: round {done} never returned"));
            }
            _ => whys.push(format!("{name}: {line:?}: want round {done} ok")),
        }
    }
    if done != rounds {
        whys.push(format!("{name}: {done} of {rounds} rounds ok"));
    }
    whys
}

/// The round trip's lines against what Main.java and ResultChild.java log, one reason per missing or wrong line. The child's result carries its code and extra. Main's first
/// result or resume after the child finished is that result: a pending result goes out before the resume (ActivityResultItem, TaskFragment.java:1665-1693). The self-start's
/// cancel comes before its new intent (ActivityStarter.java:2469).
fn result_failures(app: &str) -> Vec<String> {
    let lines: Vec<&str> = app.lines().collect();
    let results: Vec<(usize, &str)> = lines
        .iter()
        .enumerate()
        .filter_map(|(i, l)| l.split_once(RESULT).map(|(_, r)| (i, r.trim_end())))
        .collect();
    let line_of = |prefix: &str| results.iter().copied().find(|(_, r)| r.starts_with(prefix));
    let mut whys = Vec::new();
    let child = line_of("request=7 ");
    if child.map(|(_, r)| r) != Some(RESULT_CHILD) {
        whys.push(format!(
            "result 7: want {RESULT_CHILD:?}, got {:?}",
            child.map(|(_, r)| r)
        ));
    }
    if let (Some((at, _)), Some(done)) = (
        child,
        lines.iter().position(|l| l.contains(CHILD_FINISHING)),
    ) {
        let first = lines
            .iter()
            .enumerate()
            .skip(done + 1)
            .find(|(_, l)| l.contains(RESULT) || l.contains(MAIN_RESUMED))
            .map(|(i, _)| i);
        if first != Some(at) {
            whys.push(format!(
                "result 7 (line {at}) is not Main's first result or resume after the child finished (line {first:?})"
            ));
        }
    }
    let cancel = line_of("request=9 ");
    if cancel.map(|(_, r)| r) != Some(RESULT_CANCELED) {
        whys.push(format!(
            "result 9: want {RESULT_CANCELED:?}, got {:?}",
            cancel.map(|(_, r)| r)
        ));
    }
    match (
        cancel,
        lines.iter().position(|l| l.contains(MAIN_NEW_INTENT)),
    ) {
        (Some((c, _)), Some(i)) if c > i => whys.push(format!(
            "result 9 (line {c}) comes after the new intent (line {i})"
        )),
        (_, None) => whys.push(format!("main new intent: no {MAIN_NEW_INTENT:?} line")),
        _ => {}
    }
    whys
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
    .with_gl_driver(root)?
    .with_native_bridge_guest(&args.host)?;
    let isa = args.fixture_arch.as_deref().unwrap_or(&args.host);
    let deferred = deferred(isa, &args.host);
    let rt = if isa == args.host {
        rt
    } else {
        rt.with_native_bridge_guest(isa)?
            .with_qemu_engine(root, isa)?
    };
    let rt = match &args.drop_nb_lib {
        Some(file) => rt.without_nb_lib(file)?,
        None => rt,
    };
    let rt = if args.soundpool_break {
        rt.without_art_lib("libsoundpool.so")?
    } else {
        rt
    };
    let rt = if args.mediaplayer_break {
        rt.without_art_lib("libmedia_jni.so")?
    } else {
        rt
    };
    let rt = args
        .omit_service
        .iter()
        .fold(rt, |rt, s| rt.without_service(s));
    let rt = if args.media_player_break {
        rt.without_media_player_service()
    } else {
        rt
    };
    let rt = if args.gl_selection_break {
        rt.without_gl_selection()
    } else {
        rt
    };
    let rt = if args.result_break {
        rt.without_activity_result()
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
        isa,
        args.fixture_api,
        &work.join("jni-fixture"),
        args.legacy_no_libc,
    )?;
    let assets = work.join("assets");
    write_assets(&assets, deferred)?;
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
        packages: vec![Package::base(apk)],
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
    // the watch after the marker: a crash shows within it (window.rs WINDOW_MS), and the round trip runs in it too, until its last line or RESULT_WAIT;
    // the app's death ends the watch, atld outlives it
    let mut ended = ended;
    if passed && ended.is_none() {
        let watch = Instant::now();
        while ended.is_none()
            && (watch.elapsed() < Duration::from_millis(WINDOW_MS)
                || (watch.elapsed() < RESULT_WAIT && !read(&app_log).contains(MAIN_NEW_INTENT)))
        {
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
    for (service, prefix, want) in SERVICE_LINES {
        let got = app
            .lines()
            .find_map(|l| l.split_once(prefix).map(|(_, r)| r.to_owned()));
        if passed && got.as_deref() != Some(want) {
            whys.push(format!("{service}: want {want:?}, got {got:?}"));
        }
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
    // Happy Jump's MediaPlayer sequence (native_load.rs PLAYER_STEPS): construction and these steps need no service, so a media.player lookup (the wait J1 forbids) fails the row
    if passed {
        whys.extend(player_failures(&app));
        // J1's steps run between its first and last line; the probe starts after them, so its own lookups fall outside
        if mediaplayer_looked_up(&app) {
            whys.push("mediaplayer: the app looked up media.player".to_owned());
        }
        if !app.contains(MEDIAPLAYER_START) {
            whys.push("mediaplayer: start() did not reach the native player".to_owned());
        }
    }
    // media.player through the view (audio P1a): the probe's setDataSource was answered by the served service and returned
    let probe = app.lines().find_map(|l| {
        l.split_once(MEDIA_PLAYER_SERVICE)
            .map(|(_, r)| r.trim_end().to_owned())
    });
    if passed && probe.as_deref() != Some(MEDIA_PLAYER_ANSWERED) {
        whys.push(format!(
            "media.player: want {MEDIA_PLAYER_ANSWERED:?}, got {probe:?}"
        ));
    }
    // media.extractor end to end (l1-sniff): a valid WAV is prepared, started, sought and reset through the served service, by fd and by path; the reset with a seek
    // pending returns within its bound (the in-process notify path, patch 0004)
    if passed {
        whys.extend(round_failures(
            &app,
            MEDIA_WAV,
            MEDIA_WAV_ROUNDS,
            Some(MEDIA_ROUND_BOUND_MS),
        ));
        whys.extend(round_failures(
            &app,
            MEDIA_RESET,
            MEDIA_RESET_ROUNDS,
            Some(MEDIA_ROUND_BOUND_MS),
        ));
    }
    // startActivityForResult round trip (Main.java, ResultChild.java): the result, its order against Main's resume, and the singleTop self-start's cancel
    if passed {
        whys.extend(result_failures(&app));
    }
    if passed && !bridge1_ok {
        whys.push(format!(
            "bridge1: want {BRIDGE1:?}, got {:?}",
            bridge1.or_else(|| app.lines().find(|l| l.contains("AtlTest: bridge1")))
        ));
    }
    let mut es1_compared = None;
    if passed && bridge1_ok {
        whys.extend(bridge1_call_failures(&app, deferred));
        // sigsegvRecover's fault reaches the fixture's own handler through the bridge (signal.rs guest_handler), which reports it first: the report's only gate in a real app process
        if !deferred.contains(&"sigsegvRecover")
            && !app.lines().any(|l| l.contains(GUEST_FAULT_REPORT))
        {
            whys.push(format!(
                "bridge1: sigsegvRecover left no {GUEST_FAULT_REPORT:?} line"
            ));
        }
        if !deferred.contains(&"es1Pixels") {
            match es1_pixels(&app, &scene) {
                Ok(n) => es1_compared = Some(n),
                Err(why) => whys.push(format!("es1 pixels: {why}")),
            }
        }
    }
    if !deferred.is_empty() {
        println!(
            "app: {isa} under QEMU: not claimed (Q7): {}",
            deferred.join(", ")
        );
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
        println!(
            "app: pass mediaplayer {} (no media.player lookup)",
            mediaplayer_steps(&app)
        );
        for (service, _, want) in SERVICE_LINES {
            println!("app: pass {service} {want}");
        }
        println!(
            "app: pass media.player {} (answered through the view; setDataSource returned)",
            probe.unwrap_or_default()
        );
        println!(
            "app: pass mediaplayer-wav {MEDIA_WAV_ROUNDS} rounds (prepare, start, seekTo, reset; fd and path), each within {MEDIA_ROUND_BOUND_MS} ms"
        );
        println!(
            "app: pass mediaplayer-reset {MEDIA_RESET_ROUNDS} resets with a seek pending, each within {MEDIA_ROUND_BOUND_MS} ms"
        );
        println!("app: pass result {RESULT_CHILD}; {RESULT_CANCELED} before main new intent");
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

    /// Each FIXED Android format passes. Fails: RGBA_FP16 (0x16: a float config as the default request's first, round 6's ANGLE result), the host's 16-bit FIXED layout (visual 0: no format, the driver does not hand it out), RGB_888 (PixelFormat.aidl 0x3, not in the list), a visual that is not a number, a placeholder line, and no line.
    #[test]
    fn first_config_visual_must_be_a_fixed_android_format() {
        for v in FIXED_FORMATS {
            assert_eq!(
                first_config_why(Some(&format!("r=8 g=8 b=8 a=8 visual={v}"))),
                None
            );
        }
        assert!(first_config_why(Some("r=16 g=16 b=16 a=16 visual=22")).is_some());
        assert!(first_config_why(Some("r=16 g=16 b=16 a=16 visual=0")).is_some());
        assert!(first_config_why(Some("r=8 g=8 b=8 a=0 visual=3")).is_some());
        assert!(first_config_why(Some("r=8 g=8 b=8 a=8 visual=x")).is_some());
        assert!(first_config_why(Some("no display")).is_some());
        assert!(first_config_why(None).is_some());
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

    /// Main.onCreate's MediaPlayer lines when every call behaves as AOSP's does: start() in Idle throws nothing (android_media_MediaPlayer.cpp _start).
    const SEQUENCE: &str = concat!(
        "AtlTest: mediaplayer: new ok\n",
        "AtlTest: mediaplayer: setLooping ok\n",
        "AtlTest: mediaplayer: setVolume ok\n",
        "AtlTest: mediaplayer: start ok\n",
        "AtlTest: mediaplayer: release ok\n",
    );

    #[test]
    fn mediaplayer_sequence_judged_against_the_steps() {
        assert_eq!(player_failures(SEQUENCE), Vec::<String>::new());
        // start() throwing in Idle is wrong: AOSP's _start throws nothing for INVALID_OPERATION
        let threw = SEQUENCE.replace("start ok", "start java.lang.IllegalStateException: null");
        let why = player_failures(&threw);
        assert!(why.iter().any(|w| w.starts_with("mediaplayer start:")));
        // the libmedia_jni break: `new` throws and no later step is logged, so every step fails
        let broken = "AtlTest: mediaplayer: new java.lang.UnsatisfiedLinkError: dlopen failed\n";
        let why = player_failures(broken);
        assert!(why.iter().any(|w| w.starts_with("mediaplayer new:")));
        assert_eq!(why.len(), PLAYER_STEPS.len());
    }

    #[test]
    fn media_rounds_judged_in_order_and_bound() {
        let ok: String = (0..MEDIA_RESET_ROUNDS)
            .map(|i| format!("AtlTest: mediaplayer-reset: round {i} ok 40 ms\n"))
            .collect();
        assert_eq!(
            round_failures(
                &ok,
                MEDIA_RESET,
                MEDIA_RESET_ROUNDS,
                Some(MEDIA_ROUND_BOUND_MS)
            ),
            Vec::<String>::new()
        );
        // a reset that never returns: its round is missing and the probe's own line names it
        let frozen = "AtlTest: mediaplayer-reset: round 0 ok 40 ms\nAtlTest: mediaplayer-reset: no answer within 40000 ms at reset\n";
        let why = round_failures(
            frozen,
            MEDIA_RESET,
            MEDIA_RESET_ROUNDS,
            Some(MEDIA_ROUND_BOUND_MS),
        );
        assert!(why.iter().any(|w| w.contains("never returned")));
        assert!(why.iter().any(|w| w.contains("1 of 4 rounds ok")));
        // a round over the bound, or one that threw, is named where it stands
        let slow = format!(
            "AtlTest: mediaplayer-reset: round 0 ok {} ms\n",
            MEDIA_ROUND_BOUND_MS + 1
        );
        assert!(!round_failures(&slow, MEDIA_RESET, 1, Some(MEDIA_ROUND_BOUND_MS)).is_empty());
        let threw =
            "AtlTest: mediaplayer-wav: round 0 prepare java.io.IOException: Prepare failed.\n";
        let why = round_failures(
            threw,
            MEDIA_WAV,
            MEDIA_WAV_ROUNDS,
            Some(MEDIA_ROUND_BOUND_MS),
        );
        assert!(
            why.iter()
                .any(|w| w.contains("prepare java.io.IOException"))
        );
        assert!(why.iter().any(|w| w.ends_with("0 of 4 rounds ok")));
        // the WAV probe's worker outlives its bound: the probe names the step the round was in
        let hung = "AtlTest: mediaplayer-wav: round 0 ok 1 ms\nAtlTest: mediaplayer-wav: no answer within 40000 ms at reset\n";
        let why = round_failures(
            hung,
            MEDIA_WAV,
            MEDIA_WAV_ROUNDS,
            Some(MEDIA_ROUND_BOUND_MS),
        );
        assert!(
            why.iter()
                .any(|w| w.contains("no answer within 40000 ms at reset: round 1 never returned"))
        );
    }

    #[test]
    fn mediaplayer_lookup_counts_only_inside_the_steps() {
        let steps = "AtlTest: mediaplayer: new ok\nAtlTest: mediaplayer: release ok\n";
        assert!(!mediaplayer_looked_up(steps));
        let inside = "AtlTest: mediaplayer: new ok\nW ServiceManagerCppClient: Waited one second for media.player\nAtlTest: mediaplayer: release ok\n";
        assert!(mediaplayer_looked_up(inside));
        // after the steps: the probe's own wait, not J1's lookup
        let after =
            format!("{steps}W ServiceManagerCppClient: Waited one second for media.player\n");
        assert!(!mediaplayer_looked_up(&after));
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
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("testdata/gl-probe/es1-scene.toml");
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

    /// The round trip as Main.java and ResultChild.java log it when AOSP's order holds: the child's result, Main's resume, the self-start's cancel, its new intent.
    const ROUND_TRIP: &str = concat!(
        "AtlTest: onCreate reached\n",
        "AtlTest: main resumed\n",
        "AtlTest: result child finishing\n",
        "AtlTest: result request=7 code=-1 extra=atl-result-7\n",
        "AtlTest: main resumed\n",
        "AtlTest: result request=9 code=0 extra=none\n",
        "AtlTest: main new intent\n",
    );

    #[test]
    fn round_trip_as_logged_passes() {
        assert_eq!(result_failures(ROUND_TRIP), Vec::<String>::new());
    }

    #[test]
    fn round_trip_judged_line_by_line() {
        // no child result (the break): the child's line and the cancel's line are both missing, and so is the new intent
        let broken =
            ROUND_TRIP.replace("AtlTest: result request=7 code=-1 extra=atl-result-7\n", "");
        let why = result_failures(&broken);
        assert!(
            why.iter().any(|w| w.starts_with("result 7: want")),
            "{why:?}"
        );
        // a wrong extra is a wrong value, not a missing line
        let wrong = ROUND_TRIP.replace("extra=atl-result-7", "extra=other");
        assert!(
            result_failures(&wrong)
                .iter()
                .any(|w| w.starts_with("result 7: want"))
        );
        // a cancel with a code other than RESULT_CANCELED is wrong too
        let code = ROUND_TRIP.replace("request=9 code=0", "request=9 code=-1");
        assert!(
            result_failures(&code)
                .iter()
                .any(|w| w.starts_with("result 9: want"))
        );
        // nothing logged after the marker: all three required lines are reported
        assert_eq!(result_failures("AtlTest: onCreate reached\n").len(), 3);
    }

    #[test]
    fn result_must_precede_main_resume() {
        // the child's result logged after Main resumed is the wrong order (AOSP delivers it before the resume)
        let late = concat!(
            "AtlTest: result child finishing\n",
            "AtlTest: main resumed\n",
            "AtlTest: result request=7 code=-1 extra=atl-result-7\n",
        );
        let why = result_failures(late);
        assert!(
            why.iter()
                .any(|w| w.contains("is not Main's first result or resume")),
            "{why:?}"
        );
    }

    #[test]
    fn cancel_must_precede_new_intent() {
        // the self-start's new intent logged before its cancel is the wrong order (ActivityStarter.java:2469)
        let swapped = ROUND_TRIP.replace(
            "AtlTest: result request=9 code=0 extra=none\nAtlTest: main new intent\n",
            "AtlTest: main new intent\nAtlTest: result request=9 code=0 extra=none\n",
        );
        assert!(
            result_failures(&swapped)
                .iter()
                .any(|w| w.contains("comes after the new intent"))
        );
    }
}
