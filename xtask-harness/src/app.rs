// SPDX-License-Identifier: MIT
//! `xtask test app`: the M2 gate. The test APK (xtask-harness/testdata/app: a manifest and one Activity whose onCreate logs [`MARKER`]) is started the Android way by atld: atl-binderd,
//! AOSP servicemanager, the M2 services (atl-system-services), then the services start the app through atld's process control (the broker's sandbox); ActivityThread attaches, is bound, launches the activity.
//! The APK is built here as an app is built (atld_run.rs); the package record atld installs is read back from it (`aapt2 dump xmltree`, manifest.rs), not restated.
//! Every run has the graphics allocator HAL (it must register with servicemanager, the VINTF-declared way) and the GL driver (`Runtime::with_gl_driver`).
//! Fonts sub-gate ([`FONTS`]): the app's text advances equal the pinned test fonts' hmtx sums, i.e. atld's font list (atl-fonts via fontconfig) reached Typeface; `--font-break` must fail it.
//! Config row ([`FIRST_CONFIG`]): the first config eglChooseConfig lists for RGB 8 minimum on pbuffer surfaces reports a FIXED Android pixel format (no float config for a request that omits the component type). The GL driver's break `--egl-break no_config_filter` must fail it.
//! Pass: [`MARKER`] in the app's log within [`LIMIT`], then the startActivityForResult round trip after it ([`result_failures`]; `--result-break` must fail it). Always printed: the services' counters (unimplemented calls and refused parcelables, atld_run.rs `counter_lines`); on failure also the atld, app and services log tails.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
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
    /// Native build directory holding ART, servicemanager and the services (default: this checkout's build dir, see `cargo xtask out show`; else <workspace>/out/native/<host>).
    #[arg(long)]
    out: Option<PathBuf>,
    /// Java pipeline build directory: boot jars, d8, aapt2 (default: this checkout's build dir, see `cargo xtask out show`; else <workspace>/out/java/build).
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
    /// The result break: the services owe no start its result (atld `--no-activity-result`), so Main's startActivityForResult answers never arrive and the result lines must go red.
    #[arg(long)]
    result_break: bool,
    /// The bound-service break: every bindService answers 0 (atld `--no-bind-service`), so no ServiceConnection connects and no job service is bound: the bound-service and jobrun lines must go red.
    #[arg(long)]
    bind_break: bool,
    /// The job-runner break: the jobscheduler stores jobs and never runs them (atld `--no-job-runner`), so AtlJob.onStartJob never runs and the jobrun line must go red.
    #[arg(long)]
    job_runner_break: bool,
    /// The routing break: the legacy fixture is linked without DT_NEEDED `libc.so`, so the NativeBridge core refuses it, glibc's dlerror surfaces and the legacy rows must go red.
    #[arg(long)]
    legacy_no_libc: bool,
    /// The thread-priority break: apps start without the app seccomp filter (atld `--no-app-seccomp`), so restoring a thread's priority after BACKGROUND meets the kernel's EACCES on a host that grants
    /// RLIMIT_NICE below 40, Process.setThreadPriority throws SecurityException and the priority line must go red.
    #[arg(long)]
    priority_break: bool,
    /// The cpuinfo break (with `--fixture-arch` foreign): atld gets no `--cpuinfo-probe`, so the app's Java reads the host's /proc/cpuinfo while its translated library reads the engine's, and the cpuinfoView line must go red.
    #[arg(long)]
    cpuinfo_break: bool,
    /// The system-features break: the run places none of AOSP's static permission files (atld_run.rs FEATURE_FILES), so the always-present feature reads false and the features line must go red.
    #[arg(long)]
    features_break: bool,
    /// The WebView provider break: the run does not place the provider package (`<out>/partitions/product/app/webview/webview.apk`), so webviewupdate has no current package and the
    /// webview-package line must go red (null).
    #[arg(long)]
    webview_provider_break: bool,
    /// The settings provider break: the run does not place AOSP SettingsProvider, so no package declares the `settings` authority, getContentProvider answers null and the settings line must go red.
    #[arg(long)]
    settings_provider_break: bool,
    /// The WebView engine break: the run's library dir lacks the engine module (libatl_webview_linux.so) = a host without WebKit, so atld's probe fails and declares no FEATURE_WEBVIEW:
    /// getProvider throws AOSP's no-WebView UnsupportedOperationException (:345) and the webview-ua, webview-package (null) and webview-page lines must go red.
    #[arg(long)]
    webview_engine_break: bool,
    /// The WebView sandbox break: atld runs without the apps' web-process channel (no `--sandbox-join` / `--flatpak-spawn`), so WebKit tries its own bubblewrap inside the app sandbox, which
    /// Landlock forbids, and aborts the app at the first load (`Connection: failed to receive credentials`): the webview-page line must go red.
    #[arg(long)]
    webview_sandbox_break: bool,
    /// A GL driver break (repeat; atl-egl-driver build.rs `BREAKS`, e.g. `no_config_filter`, which must fail the config row): the run places a driver built with feature `break-<name>`.
    #[arg(long = "egl-break", value_name = "NAME")]
    egl_break: Vec<String>,
    /// Set by `remote test app` ([`remote`]): the run goes to this target, headless.
    #[arg(skip)]
    remote: Option<RemoteTest>,
}

/// `remote test app`'s target: ssh alias (or `local:<dir>`), the bundle break, and whether it runs (`--apply`) or only prints the plan.
pub(crate) struct RemoteTest {
    host: String,
    brk: Option<crate::remote::Break>,
    apply: bool,
}

/// atld's limit on the target: the marker's [`LIMIT`], then the watch and one [`RESULT_STALL`] after it. atld runs to it (nothing stops it earlier there), and its logs come back then; the target is
/// not watched, so this is one fixed budget for the whole chain, not the local watch's progress bound.
const REMOTE_LIMIT: Duration =
    Duration::from_secs(LIMIT.as_secs() + RESULT_STALL.as_secs() + WINDOW_MS / 1000 + 1);

/// The local gate's checks the headless subset does not claim, each with why (printed as `app: SKIP`).
const REMOTE_SKIPPED: [(&str, &str); 4] = [
    (
        "children_outside_broker",
        "reads atld's process tree through this host's /proc; atld runs on the target",
    ),
    (
        "exit totals",
        "atld ends at the target's limit (timeout), not at the gate's stop, so the exit rows have nothing to compare",
    ),
    (
        "webview-ua, webview-settings, webview-cookie-set, webview-cookie-web, webview-download, webview-scale, webview-override, webview-scheme, webview-http, webview-threads, webview-reenter, webview-slow-callback, webview-looper-thread, webview-classloader",
        "WebKitSettings needs a GDK display: no compositor on a headless run",
    ),
    (
        "fonts, cacerts, soundpool, mediaplayer, media.player, services, async and result rows, signatures, pendingintent, processes, settings, webview-package, features, priority, counters, prop rows",
        "outside the headless subset (start, onCreate/onResume, no fault, guest JNI rows); the local gate judges them",
    ),
];

/// `xtask remote test app` (remote.rs): the headless subset of the gate on a target. The run is built for `--arch` and recorded here, pushed and checked on the target (remote.rs `Target::deliver`), then atld runs there headless
/// (no compositor) through the same `RemoteRun` as `gl`'s; the logs come back at its limit. Judged: the process start, Main's onCreate and resume lines, no fault, the guest JNI (bridge1) rows. Every other check is named as skipped.
pub fn remote(root: &Path, a: &crate::remote::TestArgs) -> Result<ExitCode> {
    let args = Args {
        host: a.target_arch.resolve(root)?,
        out: a.out.clone(),
        java_out: None,
        log_tags: "*:i".to_owned(),
        verbose: false,
        font_break: false,
        fixture_api: jni_fixture::EMULATED_TLS_API,
        fixture_arch: a.fixture_arch.clone(),
        drop_nb_lib: None,
        soundpool_break: false,
        mediaplayer_break: false,
        omit_service: Vec::new(),
        media_player_break: false,
        result_break: false,
        bind_break: false,
        job_runner_break: false,
        legacy_no_libc: false,
        priority_break: false,
        cpuinfo_break: false,
        features_break: false,
        webview_provider_break: false,
        settings_provider_break: false,
        webview_engine_break: false,
        webview_sandbox_break: false,
        egl_break: Vec::new(),
        remote: Some(RemoteTest {
            host: a.host.clone(),
            brk: a.brk,
            apply: a.apply,
        }),
    };
    run(root, &args)
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
/// Main.onCreate's lines for services an app reaches through a manager that is null without them: (servicemanager name, line prefix, expected value; None = derived from the host, [`service_want`]). A missing service logs `absent`.
/// connectivity: [`connectivity_want`].
/// permissionmgr: PackageManager.checkPermission for the fixture itself: ACCESS_NETWORK_STATE is requested + NORMAL -> granted (0, permission.rs rule A); CAMERA is not requested -> denied (-1).
/// notification: Main.channels creates channel `atl.test` at IMPORTANCE_LOW (2) and reads it back: PreferencesHelper.createNotificationChannel gives a new app channel the package's visibility
/// (VISIBILITY_NO_OVERRIDE, -1000); the channel list holds it alone; an unknown id reads null; re-creating it at IMPORTANCE_HIGH keeps 2 (an app may only lower an existing channel's importance).
/// power: Main.power: a desktop session is interactive with battery saver off; PARTIAL is a supported level; a PARTIAL wake lock is held after acquire and not after release
/// (WAKE_LOCK is requested + NORMAL -> granted; a refused acquire or release throws SecurityException and the line reads `threw ...`).
/// uimode: Main.uiMode: UI_MODE_TYPE_NORMAL (1), MODE_NIGHT_NO (1), and the Configuration's uiMode TYPE_NORMAL | NIGHT_NO = 0x11 = 17 (UiModeManagerService's default for a normal device).
/// locale: Main.locales: no per-app locale is stored, so the fixture's own are LocaleManagerService.getApplicationLocales' empty list (:450-460, "[]"); getSystemLocales (:534-554) is the global
/// configuration's list, the same the app's Configuration holds (`config`); `android` is not the caller's package and the fixture lacks READ_APP_SPECIFIC_LOCALES -> SecurityException; an unknown package
/// fails isPackageOwnedByCaller (:363) -> IllegalArgumentException.
/// shortcut: Main.shortcuts against ShortcutService (svc/pm/ShortcutService.java): max 15 per activity (DEFAULT_MAX_SHORTCUTS_PER_ACTIVITY :182), icon 96 dp (:188), not rate-limited = calls left (the fixture is in front: getApiCallCount resets for a foreground uid, ShortcutPackage :695), no launcher to pin -> pinSupported false; set [a, b] + add [c] answer true; ids come out in ArrayMap order
/// (String.hashCode "a" 97 < "b" 98 < "c" 99) with ranks 0..2 by adjustRanks (set's implicit ranks, then the added one last); nothing pinned; removing b re-ranks c to 1; 16 at once exceed 15 for the
/// one activity (enforceMaxActivityShortcuts :1805) -> IllegalArgumentException; package `android` is not the caller's (verifyCaller :1740) -> SecurityException; requestPinShortcut false; removeAll -> 0.
/// trust: Main.keyguard: KeyguardManager's constructor needs `trust` (getServiceOrThrow) and the window service's keyguard calls; with no lock-screen credential the manager is non-null and isDeviceLocked, isKeyguardLocked, isDeviceSecure, isKeyguardSecure are all false.
/// jobscheduler: Main.jobs: each enqueue answers RESULT_SUCCESS (1); the second, of an equal JobInfo, joins the first job (JobSchedulerService.scheduleAsPackage), so one job is pending; cancel removes it;
/// a job whose service is an activity fails enforceValidJobRequest ("non-existent component") -> IllegalArgumentException.
/// appops: Main.appOps (AppOpsManager.MODE_ALLOWED 0, MODE_IGNORED 1, MODE_ERRORED 2): POST_NOTIFICATION's permission is requested and denied -> PermissionPolicyService's MODE_IGNORED; COARSE_LOCATION's is not
/// requested -> the op's default MODE_ALLOWED, noted the same; noted for `android` (not the fixture's uid) -> MODE_ERRORED (noteOperationUnchecked); checkPackage passes for the fixture, throws for `android`.
/// queryservices: Main.queryServices: PackageManager.queryIntentServices of action atl.test.BIND scoped to the fixture finds its one `<service>` with that filter (AtlBound; ComputerEngine
/// queryIntentServicesInternalBody), and of Play billing's Intent (action ...InAppBillingService.BIND, package com.android.vending, not installed) an empty list. A service without the method
/// answers null, which ApplicationPackageManager turns into an empty list: `own=0`.
/// user: Main.userProfile: the process user is the full system user, not a profile (UserManagerService.getProfileType:2590 answers ""), so isProfile and isManagedProfile are false.
/// zen: Main.zen: Do Not Disturb off (ZenModeHelper.getZenMode:396 ZEN_MODE_OFF) -> NotificationManager.zenModeToInterruptionFilter -> INTERRUPTION_FILTER_ALL (1).
/// receiver: Main.receiverInfo: ComputerEngine.getReceiverInfo:3810 answers the declared `<receiver>` (exported="false" in the manifest) and null for an undeclared class, which
/// ApplicationPackageManager.getReceiverInfo turns into NameNotFoundException. A service without the method answers null for both: `own=threw NameNotFoundException`.
/// installer: Main.installerSessions: IPackageManagerBase.getPackageInstaller:599 answers the installer service; with no session PackageInstallerService.getMySessions:1374 and getAllSessions:1354
/// are empty and getSessionInfo:1324 null. A service without getPackageInstaller answers null, and PackageInstaller's first call throws NullPointerException.
/// audiodevices: Main.audioDevices: the app's AudioPolicyService (atl-audioflinger policy.rs) lists one device port, the speaker (AudioPolicyManager::listAudioPorts:5275; AudioDeviceInfo.TYPE_BUILTIN_SPEAKER 2,
/// a sink), no input; A2DP is not connected (getDeviceConnectionState:523 UNAVAILABLE). Without listAudioPorts, AudioManager.updateAudioPortCache fails and getDevices answers no device: `outputs=[]`.
/// telephony.registry: Main.telephony with no phone process (no `phone` / `isub` service) on a device whose vendor API level is unset (< 2024Q2), so TelephonyManager.hasCapability reads framework-res's
/// legacy config_voice/sms/mobile_data_capable (all true: one modem, getActiveModemCount): getSimState SIM_STATE_UNKNOWN (0; no isub -> slot -1, slot 0 asks a null ITelephony,
/// getSimStateForSlotIndex:19217); getPhoneType PHONE_TYPE_NONE (0; no ITelephony and no ro.telephony.default_network, getPhoneTypeFromNetworkType:2841); getNetworkOperator "" (operator_numeric
/// unset); getDataState DATA_DISCONNECTED (0; null ITelephony). registerTelephonyCallback reaches the registry, which delivers nothing (validatePhoneId: the service sees no modem) -> calls=0.
/// Without the registry the register NPEs. A Wi-Fi tablet's SIM_STATE_ABSENT needs the device declared modem-less (zero modems) first: not modelled.
/// batterystats: [`battery_want`] (the sticky ACTION_BATTERY_CHANGED read with a null receiver, isCharging, getIntProperty(CAPACITY)).
/// netstats: Main.netstats: TrafficStats for the app's uid and the device total read null stats (NetworkStatsService's native-null answer, netstats.rs) -> UNSUPPORTED -1 (TrafficStats.java:1343-1345).
/// account: Main.accounts on a device with no accounts: getAccountsByType / getAccounts -> AccountManagerService.getAccountsAsUser (:4584) empty; getAuthenticatorTypes (:1685) empty (no
/// authenticator installed); the fixture declares no authenticator service, so addAccountExplicitly fails isAccountManagedByCaller -> SecurityException (:504-508).
/// deviceidle: Main.deviceIdle: isIgnoringBatteryOptimizations (PowerManager.java:2701) -> isPowerSaveWhitelistApp (DeviceIdleController.java:2253-2259): no sysconfig allow-in-power-save entry and
/// no user allowlist -> false.
/// thermalservice: Main.thermal on a device with no thermal HAL (ThermalManagerService.onActivityManagerReady leaves mHalReady false, mStatus THROTTLING_NONE): status 0; the status listener is
/// called once with 0 on register (postStatusListenerLocked); getThermalHeadroom NaN (HAL not ready); getThermalHeadroomThresholds IllegalStateException; registerThermalHeadroomListener false ->
/// PowerManager's RuntimeException; the status listener's remove finds it. Without the methods the status-listener add throws RuntimeException (WeChat's death).
const SERVICE_LINES: [(&str, &str, Option<&str>); 22] = [
    (
        "account",
        "AtlTest: account: ",
        Some("byType=0 all=0 types=0 add=SecurityException"),
    ),
    (
        "deviceidle",
        "AtlTest: deviceidle: ",
        Some("ignoring=false"),
    ),
    (
        "thermalservice",
        "AtlTest: thermal: ",
        Some(
            "status=0 callback=0 headroom=NaN thresholds=IllegalStateException headroomListener=RuntimeException removed=ok",
        ),
    ),
    ("batterystats", "AtlTest: battery: ", None),
    ("netstats", "AtlTest: netstats: ", Some("uid=-1 total=-1")),
    (
        "audio",
        "AtlTest: audiodevices: ",
        Some("outputs=[2] inputs=0 a2dp=false"),
    ),
    (
        "package",
        "AtlTest: installer: ",
        Some("my=0 all=0 info=null"),
    ),
    (
        "package",
        "AtlTest: receiver: ",
        Some("own=com.example.atltest.AtlReceiver exported=false missing=NameNotFoundException"),
    ),
    (
        "user",
        "AtlTest: user: ",
        Some("profile=false managed=false"),
    ),
    ("notification", "AtlTest: zen: ", Some("filter=1")),
    (
        "package",
        "AtlTest: queryservices: ",
        Some("own=1 com.example.atltest.AtlBound billing=0"),
    ),
    (
        "jobscheduler",
        "AtlTest: jobscheduler: ",
        Some(
            "enqueue=1 again=1 pending=true count=1 cancelled=true rejected=IllegalArgumentException",
        ),
    ),
    ("connectivity", "AtlTest: connectivity: ", None),
    (
        "permissionmgr",
        "AtlTest: permission: ",
        Some("network_state=0 camera=-1"),
    ),
    (
        "notification",
        "AtlTest: notification: ",
        Some("importance=2 name=ATL test visibility=-1000 count=1 absent=null raised=2 notify=ok"),
    ),
    (
        "power",
        "AtlTest: power: ",
        Some("interactive=true powersave=false partial=true held=true released=true"),
    ),
    (
        "uimode",
        "AtlTest: uimode: ",
        Some("type=1 night=1 config=17"),
    ),
    (
        "shortcut",
        "AtlTest: shortcut: ",
        Some(
            "max=15 icon=dp96 limited=false pinSupported=false set=true add=true dynamic=a:0,b:1,c:2 pinned=0 removed=a:0,c:1 over=IllegalArgumentException other=SecurityException pin=false removeAll=0",
        ),
    ),
    (
        "locale",
        "AtlTest: locale: ",
        Some("app=[] system=config other=SecurityException unknown=IllegalArgumentException"),
    ),
    (
        "trust",
        "AtlTest: keyguard: ",
        Some("manager=yes locked=false keyguardLocked=false secure=false keyguardSecure=false"),
    ),
    (
        "telephony.registry",
        "AtlTest: telephony: ",
        Some("sim=0 phone=0 operator=[] data=0 callback=registered calls=0"),
    ),
    (
        "appops",
        "AtlTest: appops: ",
        Some("post=1 coarse=0 noted=0 noted-foreign=2 own=ok foreign=SecurityException"),
    ),
];
/// The fixture's package and its desktop id (atl-launch `app_id`: `atl.<package>`): the freedesktop `Notify` arguments name both.
const FIXTURE_PACKAGE: &str = "com.example.atltest";
const FIXTURE_APP_ID: &str = "atl.com.example.atltest";
/// Main.channels' notifications as the fake desktop service must receive them (notifications plan S1 gate), local runs only (a remote run has no private bus):
/// id 8 posted ("ATL test" / "posted") -> `Notify` with replaces_id 0, answered id 1 (the fake's first); re-posted ("updated") -> `Notify` replacing 1; cancelled -> `CloseNotification(1)`, after which the
/// fake leaves the bus, so id 9's post sends nothing. Hints per N4's baseline column for channel `atl.test` at IMPORTANCE_LOW (2, kept when raised: SERVICE_LINES): urgency 0, suppress-sound;
/// `desktop-entry` the app id; not ongoing, so no `resident` and expire_timeout -1 (the server's default). app_name: the package (no label reaches atld yet).
fn notify_calls() -> Vec<crate::session_bus::Call> {
    use crate::session_bus::Call;
    let hints: std::collections::BTreeMap<String, String> = [
        ("desktop-entry", FIXTURE_APP_ID),
        ("suppress-sound", "true"),
        ("urgency", "0"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v.to_owned()))
    .collect();
    let notify = |replaces_id, body: &str| Call::Notify {
        app_name: FIXTURE_PACKAGE.to_owned(),
        replaces_id,
        summary: "ATL test".to_owned(),
        body: body.to_owned(),
        hints: hints.clone(),
        expire_timeout: -1,
        id: 1,
    };
    vec![notify(0, "posted"), notify(1, "updated"), Call::Close(1)]
}
/// The services' counts of those posts (local runs): shown, updated in place, withdrawn by the app's cancel, and id 9 found no service (the app saw success).
const NOTIFY_COUNTS: [&str; 4] = [
    "INotificationManager.notification.shown",
    "INotificationManager.notification.updated",
    "INotificationManager.notification.withdrawn reason=app",
    "INotificationManager.notification.not_shown reason=no-service",
];
/// atld's tier lines of a local run: the fake service at start, none once it left; and the one log line for the package's unshown post.
const NOTIFY_ATLD_LINES: [&str; 3] = [
    "notification: tier fdo",
    "notification: tier none",
    "notification: not shown: no notification service (com.example.atltest)",
];
/// Calls a [`SERVICE_LINES`] row makes that the app cannot see refused (a method a service lacks reads back as no error and zeros), so the services' own `answered` count must show each:
/// (service, counter key). notification: Main.channels' NotificationManager.cancel of a never-posted id, then notify of id 8 (recorded) and its cancel, which finds the record.
/// appops: Main.appOps' first noted COARSE_LOCATION (runtime permission -> notes collected) reports its stack trace, AppOpsManager's sConfig starting expired.
/// user: Main.userProfile's isProfile; a missing method reads back as null = "not a profile" too, so only this count tells them apart (and UserManager then caches the answer instead of asking again).
/// notification (zen): a missing getZenMode reads back as 0 = ZEN_MODE_OFF too; only the count tells them apart.
/// locale: Main.locales' own getApplicationLocales: the empty list is also what a reply without a LocaleList would make of an app that sets none, so the count shows the service answered.
/// netstats: a missing getUidStats / getTotalStats reads back as null stats = UNSUPPORTED -1 too; only the count tells them apart.
/// account / deviceidle: a missing getAccountsAsUser / isPowerSaveWhitelistApp reads back as an empty array / false too; only the count tells them apart.
const SERVICE_CALLS: [(&str, &str); 17] = [
    ("account", "IAccountManager.getAccountsAsUser (no accounts)"),
    (
        "deviceidle",
        "IDeviceIdleController.isPowerSaveWhitelistApp (empty allowlist)",
    ),
    (
        "netstats",
        "INetworkStatsService.getUidStats (null: no per-app accounting)",
    ),
    (
        "netstats",
        "INetworkStatsService.getTotalStats (null: host-wide counters withheld)",
    ),
    ("user", "IUserManager.getProfileType"),
    (
        "locale",
        "ILocaleManager.getApplicationLocales (no override: empty)",
    ),
    ("notification", "INotificationManager.getZenMode"),
    (
        "telephony.registry",
        "ITelephonyRegistry.listenWithEventList (no modem: never notified)",
    ),
    (
        "telephony.registry",
        "ITelephonyRegistry.listenWithEventList (unregistered)",
    ),
    (
        "notification",
        "INotificationManager.cancelNotificationWithTag (nothing posted)",
    ),
    ("notification", "INotificationManager.notification.posted"),
    (
        "notification",
        "INotificationManager.cancelNotificationWithTag (cancelled)",
    ),
    (
        "appops",
        "IAppOpsService.reportRuntimeAppOpAccessMessageAndGetConfig (not sampled)",
    ),
    (
        "trust",
        "ITrustManager.isDeviceLocked (no credential: false)",
    ),
    (
        "trust",
        "ITrustManager.isDeviceSecure (no credential: false)",
    ),
    (
        "window",
        "IWindowManager.isKeyguardLocked (no keyguard: false)",
    ),
    (
        "window",
        "IWindowManager.isKeyguardSecure (no credential: false)",
    ),
];
/// Main.pendingIntent's line: PendingIntent's creator getters for this package's own immutable broadcast, then isImmutable of a FLAG_MUTABLE one (ActivityManagerService.getInfoForIntentSender reports the record's
/// key package, its creator uid, FLAG_IMMUTABLE and INTENT_SENDER_BROADCAST). A service without the method gives the app a null PendingIntentInfo, and the line reads `threw ...NullPointerException...`.
const PENDING_INTENT: (&str, &str) = (
    "AtlTest: pendingintent: ",
    "package=com.example.atltest self=true immutable=true broadcast=true mutable=true",
);
/// Main.features' line (platform-fidelity row 4): `landscape=<b> touchscreen=<b> aep=<b> gl-aep=<b> listed=<b> gles=<hex> config=<hex>` ([`features_why`]).
const FEATURES: &str = "AtlTest: features: ";
/// Main.priority's line: `start=<n> background=<r>/<nice> restore=<r>/<nice> foreground=<r>/<nice>`, r = `ok` or the throwable's simple name ([`priority_why`]).
const PRIORITY: &str = "AtlTest: priority: ";
/// Main.signatures' line: the fixture's signer as GET_SIGNATURES and GET_SIGNING_CERTIFICATES return it and checkSignatures against itself ([`signatures_want`]).
const SIGNATURES: &str = "AtlTest: signatures: ";
/// Main.processes' line: getRunningAppProcesses shows the fixture its one process (AOSP: a caller sees its own uid's processes only), with the fixture's Process.myPid / myUid, its processName and
/// package, and IMPORTANCE_FOREGROUND (100: ATL answers every app process as PROCESS_STATE_TOP). `null` = the list AOSP gives a caller with no attached process.
const PROCESSES: &str = "AtlTest: processes: ";
const PROCESSES_WANT: &str = "count=1 own=1 uid=true name=true pkg=true importance=100";
/// Main.sysAnswers' line: small system-service answers, one field each.
/// - `checksums=0`: requestChecksums reaches the listener with an empty list, ApkChecksums' answer when no APK file can be read (getAvailableApkChecksums / calculateChecksumIfRequested skip on
///   IOException). A device would list the required SHA256; the services cannot read app files yet (counted divergence). Before: the List argument was refused -> IllegalArgumentException.
const SYSANSWERS: &str = "AtlTest: sysanswers: ";
/// - `volume=15 max=15 ringer=2 mode=0`: STREAM_MUSIC of an app whose audio output registered as an absolute-volume device (registerDeviceVolumeDispatcherForAbsoluteVolume, done at the app's
///   AudioFlinger serve, before any app code): AudioService sets the group to the device's index (AS:8240-8250), which starts at the host's new-stream default, WirePlumber
///   `node.stream.default-playback-volume` 1.0 -> round(max * cbrt(1.0)) = max = AudioService.MAX_STREAM_VOLUME[3] = 15 (the output opens lazily: no restored value is seen yet).
///   AOSP's DEFAULT_STREAM_VOLUME[3] = 5 is what a uid that never registers reads. RINGER_MODE_NORMAL (2), MODE_NORMAL (0). Before: all four 0 (UNKNOWN_TRANSACTION's empty reply).
/// - `installer=null source=null/null`: an install with no installer of record (the `adb install` case, ComputerEngine.getInstallerPackageName / getInstallSourceInfo): no installer, and an
///   InstallSourceInfo whose installing and initiating packages are null. Before: the same null installer, but a null InstallSourceInfo (`nosource`), which AOSP gives only for an unknown package.
/// - `errors=null`: getProcessesInErrorState with no crashing or ANR process (AMS returns its lazily created list, still null). The unanswered call read the same; the run's diag counters tell them apart.
/// - `memthreshold=226492416 memsized=true`: ProcessList.getMemoryInfo on any host of >= 700 MB (updateOomLevels scale 1): threshold = getMemLevel(HOME_APP_ADJ 600) = the 64-bit-raised
///   mOomMinFreeHigh[4], 147456 * 3 / 2 = 221184 KB = 226492416 bytes; total > 0, avail <= total, advertised (roundStorageSize) >= total. Before: the out param untouched, threshold 0, sized false.
/// - `insets=0,0,0,0`: the desktop display has no system bars or cutout. The unanswered call left an empty InsetsState, the same zeros; the diag counters tell them apart.
const SYSANSWERS_WANT: &str = "checksums=0 volume=15 max=15 ringer=2 mode=0 installer=null source=null/null errors=null memthreshold=226492416 memsized=true insets=0,0,0,0";
/// Main.settings' line, answered by AOSP SettingsProvider in its own process (`id=<s> again=<b> screen_off=<s> auto_time=<s> rotation=<s>`, [`settings_why`]).
const SETTINGS: &str = "AtlTest: settings: ";
/// The line after `id=`: the defaults DatabaseHelper loads from SettingsProvider's res/values/defaults.xml -- def_screen_off_timeout 60000, def_auto_time true (loadBooleanSetting writes "1"),
/// def_accelerometer_rotation false ("0") -- and a second ANDROID_ID read equal to the first.
const SETTINGS_WANT_REST: &str = "again=true screen_off=60000 auto_time=1 rotation=0";

/// Why the settings line fails: ANDROID_ID must be the SSAID's form, 16 lowercase hex digits (SettingsProvider.generateSsaidLocked: HMAC-SHA256 of the signer, first 8 bytes as hex), and the rest
/// must equal [`SETTINGS_WANT_REST`]. Without the provider (`--settings-provider-break`) every read is null.
fn settings_why(line: Option<&str>) -> Option<String> {
    let ok = line.and_then(|l| l.strip_prefix("id=")).is_some_and(|r| {
        r.split_once(' ').is_some_and(|(id, rest)| {
            id.len() == 16
                && id
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                && rest == SETTINGS_WANT_REST
        })
    });
    (!ok).then(|| format!("settings: want \"id=<16 hex> {SETTINGS_WANT_REST}\", got {line:?}"))
}
/// Main.webView's line: WebSettings.getDefaultUserAgent with FEATURE_WEBVIEW declared (atld, when the web engine module loads).
/// Without the provider placed, AOSP: `webviewupdate`'s waitForAndGetProvider answers LIBLOAD_FAILED_LISTING_WEBVIEW_PACKAGES -> WebViewFactory.getWebViewContextAndSetProvider throws
/// MissingWebViewPackageException, getProviderClass wraps it in AndroidRuntimeException, getProvider's catch (Exception) wraps that once more. Without the feature: UnsupportedOperationException
/// alone (getProvider :345); without the service: asInterface(null)'s NullPointerException, wrapped once by getProvider (measured).
/// With the provider placed and shown to apps (system + forceQueryable, Packages::visible): getProvider loads the provider APK's class loader, WebViewChromiumFactoryProviderForT.create, then
/// getStatics -> getDefaultUserAgent in libatl_webview.so (webview plan W1). Pass ([`webview_ua_why`]): `cts=true` (CTS's pattern, run in the app) and `ua=` equal to [`webview_ua_want`].
const WEBVIEW_UA: &str = "AtlTest: webview-ua: ";
/// libatl_webview.so's record of the engine's own default UA (atl-webview `default_user_agent`), what [`webview_ua_want`] takes the engine tokens from.
const WEBVIEW_ENGINE_UA: &str = "webview: engine user agent: ";

/// The expected UA, assembled here from webview plan S5 (CDD 3.4.1 format), independently of atl-webview `ua`: the device fields the app logged (its android.os.Build), the engine's
/// `AppleWebKit/` and `Safari/` versions cut to `<major>.<minor>`, `Version/4.0`, the engine's Chrome quirk token `Chrome/300.0.0.0` (WebKit UserAgentQuirks.cpp), no ` Mobile` (desktop).
fn webview_ua_want(engine: &str, release: &str, model: &str, id: &str) -> Option<String> {
    let token = |name: &str| -> Option<String> {
        let v = engine.split(' ').find_map(|t| t.strip_prefix(name))?;
        let mut parts = v.splitn(3, '.');
        Some(format!("{name}{}.{}", parts.next()?, parts.next()?))
    };
    Some(format!(
        "Mozilla/5.0 (Linux; Android {release}; {model} Build/{id}; wv) {} (KHTML, like Gecko) Version/4.0 Chrome/300.0.0.0 {}",
        token("AppleWebKit/")?,
        token("Safari/")?
    ))
}

/// Why the webview-ua line fails, None when it passes: `line` = the line after [`WEBVIEW_UA`], `engine` = the engine record's UA.
fn webview_ua_why(line: Option<&str>, engine: Option<&str>) -> Option<String> {
    let Some(line) = line else {
        return Some("no webview-ua line".into());
    };
    let parsed = (|| {
        let rest = line.strip_prefix("cts=")?;
        let (cts, rest) = rest.split_once(" release=")?;
        let (fields, ua) = rest.split_once(" ua=")?;
        let mut f = fields.split('|');
        let release = f.next()?;
        let model = f.next()?.strip_prefix("model=")?;
        let id = f.next()?.strip_prefix("id=")?;
        Some((cts, release, model, id, ua))
    })();
    let Some((cts, release, model, id, ua)) = parsed else {
        return Some(format!("webview-ua: {line:?}"));
    };
    if cts != "true" {
        return Some(format!("webview-ua: CTS pattern does not match {ua:?}"));
    }
    let Some(engine) = engine else {
        return Some("webview-ua: no engine user agent record".into());
    };
    match webview_ua_want(engine, release, model, id) {
        Some(want) if want == ua => None,
        want => Some(format!("webview-ua: want {want:?}, got {ua:?}")),
    }
}
/// Main.webViewPackage's line: WebView.getCurrentWebViewPackage (webviewupdate's getCurrentWebViewPackage, WebView.java:3128-3140) with the provider placed: the scanned
/// `com.android.webview` with FLAG_SYSTEM (WebViewUpdateServiceImpl2.providerHasValidSignature accepts a system app). `null` = no valid provider.
const WEBVIEW_PACKAGE: Line = Line {
    row: "webview-package",
    prefix: "AtlTest: webview-package: ",
    want: Want::Exact("name=com.android.webview system=true"),
    timing: Timing::Create,
};
/// Main.webViewPage's line (webview plan W2 gate): an unattached WebView loads Main.WEBVIEW_PAGE through loadData, then runs Main.WEBVIEW_SCRIPT at the first onPageFinished.
/// `events`: the WebViewClient order -- onPageStarted, then onPageFinished once. `titles`: onReceivedTitle once with the page's `<title>`, in no fixed order with onPageFinished (AOSP fixes none:
/// CTS WebChromeClientTest.testOnReceivedTitle polls for the title after the load completes; WebKit notifies it after LOAD_FINISHED for this data: page, W2 run w2-app5x). `js`: evaluateJavascript's JSON of the script's array (JSON.stringify form):
/// `typeof atl.add` "function" (annotated, exposed), `atl.add(2, 3)` 5 and `atl.echo('hi')` "hi!" (the bridge round trip: Bridge.add / Bridge.echo), `typeof atl.hidden` "undefined"
/// (no @JavascriptInterface: not callable, WebView.addJavascriptInterface doc for API >= 17), `document.title` "atl" (the page's title).
const WEBVIEW_PAGE: Line = Line {
    row: "webview-page",
    prefix: "AtlTest: webview-page: ",
    want: Want::Exact(
        r#"events=started,finished, titles=atl, js=["function",5,"hi!","undefined","atl"]"#,
    ),
    timing: Timing::Async,
};
/// Main.webViewSettings's line (webview plan S4.1, W4a): a WebView reads back what it sets, a flag and an int and a text the engine applies (font-size 20, serif "atl-serif", text-zoom 150) and
/// the stored ones; the rest are AOSP's defaults (WebSettings.java:333, :934, :902, :640 MEDIUM, :797 NARROW_COLUMNS), the table in atl-webview settings.rs.
const WEBVIEW_SETTINGS: Line = Line {
    row: "webview-settings",
    prefix: "AtlTest: webview-settings: ",
    want: Want::Exact(
        "js=true font-size=20 serif=atl-serif text-zoom=150 min-font=8 zoom=MEDIUM layout=NARROW_COLUMNS support-zoom=true",
    ),
    timing: Timing::Create,
};
/// Main.webViewCookies's line (webview plan D5, W4b): CookieManager.setCookie from a thread without a Looper is readable at once on that thread (the copy), `a=1` being the cookie it set and
/// hasCookies true; the ValueCallback reports success on the main Looper (CookieManager.java javadoc: the caller's Looper, the main one for a caller without).
const WEBVIEW_COOKIE_SET: Line = Line {
    row: "webview-cookie-set",
    prefix: "AtlTest: webview-cookie-set: ",
    want: Want::Exact("get=a=1 has=true cb=true main=true"),
    timing: Timing::Async,
};
/// Main.webViewCookieWeb's line: a page's document.cookie set reaches CookieManager.getCookie (`visible=true`) within the poll's 5 s; `window-us=` is the measured D5 window, printed, not judged.
const WEBVIEW_COOKIE_WEB: Line = Line {
    row: "webview-cookie-web",
    prefix: "AtlTest: webview-cookie-web: ",
    want: Want::Prefix("visible=true "),
    timing: Timing::Async,
};
/// Main.webViewDownload's line: a response of a type the engine does not render goes to DownloadListener (its MIME type) with the view's user agent.
const WEBVIEW_DOWNLOAD: Line = Line {
    row: "webview-download",
    prefix: "AtlTest: webview-download: ",
    want: Want::Exact("mime=application/x-atl-test ua=true"),
    timing: Timing::Async,
};
/// Main.webViewNavigation's lines (WebViewClient.java shouldOverrideUrlLoading javadoc, :33-92): a page's script (no gesture, after the load) navigates; the app's own loadDataWithBaseURL is not asked
/// ("not called for navigations which the app initiated with loadUrl()"), so `calls=1`; the request is the main frame's GET, no redirect, no gesture. Each row then reads the page and destroys the view.
/// override: the client returns true -> the load is aborted: no onPageStarted for it (`started=https,` = the first page only), no error, the page is still the first one (`<row> https:`).
const WEBVIEW_OVERRIDE: Line = Line {
    row: "webview-override",
    prefix: "AtlTest: webview-override: ",
    want: Want::Exact(
        r#"calls=1 request=atl-test://done?x=1 main=true redirect=false gesture=false method=GET started=https, error=none page="webview-override https:" destroyed=true"#,
    ),
    timing: Timing::Async,
};
/// scheme: the client returns false for a scheme no loader takes -> onReceivedError(ERROR_UNSUPPORTED_SCHEME -10, WebViewClient.java:278) for the main-frame request; nothing loads (no onPageStarted:
/// the engine never starts it), the first page stays (not blank).
const WEBVIEW_SCHEME: Line = Line {
    row: "webview-scheme",
    prefix: "AtlTest: webview-scheme: ",
    want: Want::Exact(
        r#"calls=1 request=atl-test://fail main=true redirect=false gesture=false method=GET started=https, error=-10 atl-test://fail main=true page="webview-scheme https:" destroyed=true"#,
    ),
    timing: Timing::Async,
};
/// http: the client returns false for an http URL -> it loads as usual (onPageStarted for it). What follows the refused connection is printed, not judged: the engine's error page (WebKit loads one
/// for the failing URL: a second `http` start), the error code and the page. The app reads at the load's onPageFinished, not after a fixed delay: the start of this cross-site load waits for a new
/// web process (0.3 s to over 1 s on a loaded host), and a read before it judged a load that had not begun. A dropped start still fails (`started=https,`), a load that never ends leaves no line.
const WEBVIEW_HTTP: Line = Line {
    row: "webview-http",
    prefix: "AtlTest: webview-http: ",
    want: Want::Prefix(
        "calls=1 request=http://127.0.0.1:9/next main=true redirect=false gesture=false method=GET started=https,http,",
    ),
    timing: Timing::Async,
};
/// Main.webViewThreads' line (display-thread S2, WTC S5 row 1): every client callback of a main-Looper WebView runs on the WebView's Looper (`wv`: WebView.checkThread's thread; CTS
/// WebViewSyncLoader.pumpMessages waits for load callbacks by looping that Looper; evaluateJavascript's callback "on the UI thread", WebView.java:878); the @JavascriptInterface method runs on a
/// background thread (WebView.java:1915), named `JavaBridge` as in Chromium's provider (the name: Chromium, not AOSP; the AOSP part is "not the WebView's Looper", no `:wv` suffix).
const WEBVIEW_THREADS: Line = Line {
    row: "webview-threads",
    prefix: "AtlTest: webview-threads: ",
    want: Want::Exact(
        "override=wv started=wv finished=wv title=wv progress=wv eval=wv download=wv bridge=JavaBridge",
    ),
    timing: Timing::Async,
};
/// Main.webViewReenter's line (display-thread S2, WTC R3 / S5 row 2): getters inside client callbacks agree with the events delivered so far, and loadUrl from onPageFinished loads (Main.webViewReenter's
/// doc derives each value from AOSP's contract).
const WEBVIEW_REENTER: Line = Line {
    row: "webview-reenter",
    prefix: "AtlTest: webview-reenter: ",
    want: Want::Exact(
        "p1-url=true after-load=true p2-url=true back=true forward=false override-url=true title-match=true",
    ),
    timing: Timing::Async,
};
/// Main.webViewSlowCallback's line (WTC S5 row 5): a shouldOverrideUrlLoading that sleeps 2 s and answers "load" -- the load starts after the answer (`waited=true`), the http navigation runs
/// (`started=https,http,`). The rest (the engine's error page) is printed, not judged, as webview-http.
const WEBVIEW_SLOW_CALLBACK: Line = Line {
    row: "webview-slow-callback",
    prefix: "AtlTest: webview-slow-callback: ",
    want: Want::Prefix("calls=1 waited=true started=https,http,"),
    timing: Timing::Async,
};
/// Main.webViewLooperThread's line (display-thread S2, WTC S5 row 6): a WebView made on a HandlerThread has that thread's Looper as getWebViewLooper() (`looper=ht`, WebView.java mWebViewThread) and
/// every client callback runs on it (`wv`), as webview-threads' main-Looper view.
const WEBVIEW_LOOPER_THREAD: Line = Line {
    row: "webview-looper-thread",
    prefix: "AtlTest: webview-looper-thread: ",
    want: Want::Exact("looper=ht override=wv started=wv finished=wv title=wv progress=wv eval=wv"),
    timing: Timing::Async,
};
/// Main.webViewScale's line: setInitialScale(200) -> 200 % = 2 device pixels per CSS pixel whatever the screen density (WebView.java:1184-1188), the page's devicePixelRatio.
const WEBVIEW_SCALE: Line = Line {
    row: "webview-scale",
    prefix: "AtlTest: webview-scale: ",
    want: Want::Exact("dpr=2"),
    timing: Timing::Async,
};
/// Main.webViewClassLoader's line: WebView.getWebViewClassLoader is the loader of the provider's factory class (WebView.java:2428, WebViewFactoryProvider.java:262); it has no Chromium
/// support-library glue, so androidx.webkit reports its extra features unsupported.
const WEBVIEW_CLASSLOADER: Line = Line {
    row: "webview-classloader",
    prefix: "AtlTest: webview-classloader: ",
    want: Want::Exact("provider=true glue=false"),
    timing: Timing::Create,
};
/// Main.noSourceStarts' rows (after the permission callback): starts with no source activity, the call shape of Temple Run's rewarded ad. AOSP: a null resultTo, or one naming no record, leaves
/// sourceRecord and resultRecord null (ActivityStarter.executeRequest, ActivityStarter.java:1104-1120; no error), NEW_TASK is forced (computeLaunchingTaskFlags :2885-2891), findTask finds the
/// fixture's Task (the package affinity) and the standard NoSourceChild is added on top (complyActivityFlags :2595-2602, recycleTask :2398-2402: START_SUCCESS). So NoSourceChild is created,
/// and with no result record getCallingActivity is null even for the raw call's request code 5.
const START_APP_CONTEXT: Line = Line {
    row: "start-app-context",
    prefix: "AtlTest: start-app-context: ",
    want: Want::Exact("started calling=null"),
    timing: Timing::Async,
};
const START_STALE_RESULT_TO: Line = Line {
    row: "start-stale-result-to",
    prefix: "AtlTest: start-stale-result-to: ",
    want: Want::Exact("started calling=null"),
    timing: Timing::Async,
};
/// Flags.java's launch-flag scenarios (after the no-source starts): Main starts A1; each instance's events in its own order (c create, r onResume, n onNewIntent, s<req>:<code>:<from> result,
/// x the start threw, d onDestroy). The last start begins the unwind, so the instance that ends on top resumes once more and finishes, and so on down to Main. Expected values from AOSP:
/// clear-top: [A1 B1] + CLEAR_TOP A -> B1 finishes (Task.finishActivityAbove), the standard A1 finishes too without SINGLE_TOP (Task.clearTopActivities :1731-1736) and A2 is new
/// (complyActivityFlags :2533-2537); A1 stopped under B1 never resumes again. clear-top-single-top: A1 is kept and gets onNewIntent before its resume (deliverNewIntent :2532; onResume follows
/// onNewIntent, Activity.onNewIntent docs). single-top: the top A1 gets the intent (deliverToCurrentTopIfNeeded :2441-2483). no-history: B1 (NO_HISTORY) is finished where it would stop
/// (ActivityRecord.stopIfPossible :6324), which runs only after C1's idle (ActivityTaskSupervisor.activityIdleInternal -> processStoppingAndFinishingActivities), so Flags.java
/// holds C1's unwind until B1 is destroyed; C1's finish then resumes A1. reorder-to-front: A1 moves above B1 with onNewIntent (complyActivityFlags :2553-2570, Task.moveActivityToFront), B1 resumes
/// after A1 finishes. forward-result: B1's FORWARD_RESULT start moves A1's request 3 to C1 (executeRequest :1123-1136), so getCallingActivity of C1 is A and A1 gets C1's RESULT_OK (-1), not B1's; B1 itself was started for A1's result, so its getCallingActivity is A too.
/// refused-new-document (control, unchanged by the flags work): NEW_DOCUMENT needs a new Task, which ATL does not model: the start throws (START_CANCELED -96).
const FLAG_LINES: [Line; 7] = [
    flag_line(
        "flags-clear-top",
        "AtlTest: flags-clear-top: ",
        "A1[c,r,d] B1[c,r,d] A2[c,r,d]",
    ),
    flag_line(
        "flags-clear-top-single-top",
        "AtlTest: flags-clear-top-single-top: ",
        "A1[c,r,n,r,d] B1[c,r,d]",
    ),
    flag_line(
        "flags-single-top",
        "AtlTest: flags-single-top: ",
        "A1[c,r,n,r,d]",
    ),
    flag_line(
        "flags-no-history",
        "AtlTest: flags-no-history: ",
        "A1[c,r,r,d] B1[c,r,d] C1[c,r,d]",
    ),
    flag_line(
        "flags-reorder-to-front",
        "AtlTest: flags-reorder-to-front: ",
        "A1[c,r,n,r,d] B1[c,r,r,d]",
    ),
    flag_line(
        "flags-forward-result",
        "AtlTest: flags-forward-result: ",
        "A1[c,r,s3:-1:C1,r,d] B1[c<A,r,d] C1[c<A,r,d]",
    ),
    flag_line(
        "flags-refused-new-document",
        "AtlTest: flags-refused-new-document: ",
        "A1[c,r,x,d]",
    ),
];

const fn flag_line(row: &'static str, prefix: &'static str, want: &'static str) -> Line {
    Line {
        row,
        prefix,
        want: Want::Exact(want),
        timing: Timing::Async,
    }
}

/// Every judged prefix line: LOG_LINES, then FLAG_LINES.
fn judged_lines() -> impl Iterator<Item = &'static Line> {
    LOG_LINES.iter().chain(FLAG_LINES.iter())
}

/// When the fixture logs a judged line. `Async`: from a binder or WebView callback after onCreate; the watch waits for it while awaited lines keep arriving ([`watch_on`]).
/// `Create`: in onCreate, or on a thread onCreate starts before any callback, so WINDOW_MS covers it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Timing {
    Create,
    Async,
}

/// What a judged line's value must be: `Exact` the whole value; `Prefix` its start (the rest carries measurements, printed not judged); `Host` the run host's zone ([`HostFacts::zone_want`]).
#[derive(Clone, Copy)]
enum Want {
    Exact(&'static str),
    Prefix(&'static str),
    Host,
}

/// A fixture log line the gate judges: `row` names it in verdicts, `prefix` finds it in the app log, `want` is its value, `timing` says when the fixture logs it.
#[derive(Clone, Copy)]
struct Line {
    row: &'static str,
    prefix: &'static str,
    want: Want,
    timing: Timing,
}

/// Every prefix-judged line of the fixture's log: the webview rows above and the binder callbacks below, one list read twice. The watch waits for each `Async` row
/// (`waiting` in `judge`), and the gate judges each row. A new async row is declared with `timing: Timing::Async` in this list: it is awaited and judged, and no second list names it.
/// bindservice: Main.bound's bindService(AtlBound, BIND_AUTO_CREATE) answers true (bindServiceLocked found the service: code 1).
/// bindimplicit: Main.bindImplicit's bindService(Intent atl.test.BIND, setPackage) answers true and connects to AtlBound: the implicit Intent resolves as retrieveServiceLocked resolves it (queryIntentServices, first match).
/// bound: its onServiceConnected names AtlBound and the binder it got is AtlBound's (FIRST_CALL_TRANSACTION answers AtlBound.ANSWER, 42).
/// unbind: unbindService from onServiceConnected reaches AtlBound.onUnbind with the bound Intent (action atl.test.BIND): the last connection left (removeConnectionLocked -> scheduleUnbindService).
/// jobrun: Main.jobRun's job 9 runs (onStartJob) and drains its one work item: action atl.test.WORK, delivery count 1 (JobStatus.dequeueWorkLocked bumps it from 0 on its first delivery),
/// completeWork accepted (JobParameters.completeWork throws when the item is not executing), then dequeueWork answers null (the last work dequeued finishes the job).
/// bindmissing: Main.bindMissing binds to the absent com.android.vending (implicit with package, then explicit): bindServiceLocked answers 0 for both (no ServiceRecord, no callback),
/// and unbindService right after finds the dispatcher ContextImpl.bindServiceCommon registered before the AM call (LoadedApk.forgetServiceDispatcher does not throw).
/// sdkext: Main.onCreate's SdkExtensions.getExtensionVersion for R and AD_SERVICES (not a binder callback; logged in onCreate). The values are the build.prop rows
/// sdkext.go derives as derive_sdk.cpp SetSdkLevels computes them: r stops at 8, the last db version that does not require EXT_SERVICES (not installed in ATL; version 9 is the first that does).
/// ad_services is AD_SERVICES' module version, 17.
/// timezone: Main.onCreate's TimeZone.getDefault(), from persist.sys.timezone (RuntimeInit.java:252 reads it; atld sets it to the host zone). None = the run host's zone ([`HostFacts::zone_want`]: the target's on a remote run, never the build host's).
/// display: Main.onCreate's Display.getAppVsyncOffsetNanos and getPresentationDeadlineNanos less the period of getRefreshRate: SurfaceFlinger.cpp getDynamicDisplayInfoInternal's appVsyncOffset
/// and `period - sfVsyncOffset + 1 ms`, with both offsets 0 (the compositor sends app vsync and latches at the tick itself), so 0 and 1 000 000 at any refresh rate.
/// alarm: Main.alarms: both AlarmManager delivery kinds fire once: the RTC_WAKEUP PendingIntent broadcast reaches the registered receiver, and the ELAPSED_REALTIME OnAlarmListener's onAlarm runs (alarm.rs, pending_intent.rs).
/// startservice, stopservice, stopself: Main.started's chain of AtlStarted (active_services.rs, service_records.rs: the start, stopService, stopSelf). START's onStartCommand (startId 1);
/// stopService's onDestroy; STOPSELF's stopSelf(1) then onDestroy. The STICKY restart and the NOT_STICKY process are judged by [`StickyCase`], from their own processes' logs.
/// fgsnotype: Main.foregroundStarts' AtlFgsNoType (targetSdk 36, no foregroundServiceType, main process): startForeground without a type throws `Starting FGS without a type` (AS :2883-2886); the line carries the message's start.
/// The `fgs` and `fgstimeout` rows are [`ForegroundCase`]'s, from their own processes' logs.
const LOG_LINES: [Line; 33] = [
    START_APP_CONTEXT,
    START_STALE_RESULT_TO,
    Line {
        row: "display",
        prefix: "AtlTest: display: ",
        want: Want::Exact("offset=0 deadline_less_period=1000000"),
        timing: Timing::Create,
    },
    Line {
        row: "bindservice",
        prefix: "AtlTest: bindservice: ",
        want: Want::Exact("bindService=true"),
        timing: Timing::Async,
    },
    Line {
        row: "bindimplicit",
        prefix: "AtlTest: bindimplicit: ",
        want: Want::Exact("bindService=true connected=com.example.atltest.AtlBound"),
        timing: Timing::Async,
    },
    Line {
        row: "bindmissing",
        prefix: "AtlTest: bindmissing: ",
        want: Want::Exact("bindService=false explicit=false unbind=ok called=false"),
        timing: Timing::Async,
    },
    Line {
        row: "bound",
        prefix: "AtlTest: bound: ",
        want: Want::Exact("connected=com.example.atltest.AtlBound answer=42"),
        timing: Timing::Async,
    },
    Line {
        row: "unbind",
        prefix: "AtlTest: unbind: ",
        want: Want::Exact("onUnbind action=atl.test.BIND"),
        timing: Timing::Async,
    },
    Line {
        row: "jobrun",
        prefix: "AtlTest: jobrun: ",
        want: Want::Exact("started=9 work=atl.test.WORK delivery=1 completed=true next=null"),
        timing: Timing::Async,
    },
    Line {
        row: "alarm",
        prefix: "AtlTest: alarm: ",
        want: Want::Exact("broadcast=1 listener=1"),
        timing: Timing::Async,
    },
    Line {
        row: "startservice",
        prefix: "AtlTest: startservice: ",
        want: Want::Exact("onStartCommand startId=1 action=atl.test.START"),
        timing: Timing::Async,
    },
    Line {
        row: "stopservice",
        prefix: "AtlTest: stopservice: ",
        want: Want::Exact("onDestroy"),
        timing: Timing::Async,
    },
    Line {
        row: "stopself",
        prefix: "AtlTest: stopself: ",
        want: Want::Exact("onDestroy after stopSelf startId=1"),
        timing: Timing::Async,
    },
    Line {
        row: "fgsnotype",
        prefix: "AtlTest: fgsnotype: ",
        want: Want::Prefix("Starting FGS without a type"),
        timing: Timing::Async,
    },
    Line {
        row: "sdkext",
        prefix: "AtlTest: sdkext: ",
        want: Want::Exact("r=8 ad_services=17"),
        timing: Timing::Create,
    },
    Line {
        row: "timezone",
        prefix: "AtlTest: timezone: ",
        want: Want::Host,
        timing: Timing::Create,
    },
    WEBVIEW_PACKAGE,
    WEBVIEW_PAGE,
    WEBVIEW_SETTINGS,
    WEBVIEW_COOKIE_SET,
    WEBVIEW_COOKIE_WEB,
    WEBVIEW_DOWNLOAD,
    WEBVIEW_SCALE,
    WEBVIEW_OVERRIDE,
    WEBVIEW_SCHEME,
    WEBVIEW_HTTP,
    WEBVIEW_THREADS,
    WEBVIEW_REENTER,
    WEBVIEW_SLOW_CALLBACK,
    WEBVIEW_LOOPER_THREAD,
    WEBVIEW_CLASSLOADER,
    COMPONENT_STATE,
    COMPONENT_STATE_RECEIVE,
];

/// A [`SERVICE_LINES`] expectation: the fixed value, or this host's (None): [`connectivity_want`], [`battery_want`].
fn service_want(service: &str, fixed: Option<&str>) -> String {
    match (fixed, service) {
        (Some(f), _) => f.to_owned(),
        (None, "batterystats") => battery_want(),
        (None, "connectivity") => connectivity_want(),
        (None, other) => format!("<no host-derived want for {other}>"),
    }
}

/// The battery line (Main.battery) for this host, from its power-supply sysfs read here, not through the HAL: each `/sys/class/power_supply/<name>`'s `type`; a `Battery` gives present (`present`, else 1),
/// level and CAPACITY (`capacity`), status (`status`: BatteryMonitor's names, Unknown 1 / Charging 2 / Discharging 3 / Not charging 4 / Full 5); plugged = the first online of Mains (AC 1), USB (2),
/// Wireless (4) (BatteryService.plugType's order); charging = BatteryStatsImpl's state at a fresh start: false only on battery (unplugged with a known status).
/// No battery (the WSL dev host): BatteryMonitor's initHealthInfo answer -- present false, level 0, status UNKNOWN 1 -- and CAPACITY unsupported: Health::getCapacity EX_UNSUPPORTED_OPERATION -> getProperty -1 ->
/// BatteryManager.queryProperty Long.MIN_VALUE -> getIntProperty Integer.MIN_VALUE (targetSdk >= P). Scale BatteryService.BATTERY_SCALE 100.
/// The EXTRA_LEVEL of the system's sticky ACTION_BATTERY_CHANGED on this host: [`battery_want`]'s level (BatteryService.sendBatteryChangedIntentLocked puts batteryLevel).
pub(crate) fn battery_level() -> String {
    battery_want()
        .split(' ')
        .find_map(|kv| kv.strip_prefix("level="))
        .unwrap_or_default()
        .to_owned()
}

fn battery_want() -> String {
    let read = |p: &Path| std::fs::read_to_string(p).map(|s| s.trim().to_owned()).ok();
    let (mut battery, mut plugged) = (None, 0);
    let mut online = Vec::new();
    for e in std::fs::read_dir("/sys/class/power_supply")
        .into_iter()
        .flatten()
        .flatten()
    {
        let d = e.path();
        let is_online = read(&d.join("online")).as_deref() == Some("1");
        match read(&d.join("type")).as_deref() {
            Some("Battery") => {
                battery = Some((
                    read(&d.join("present")).is_none_or(|p| p == "1"),
                    read(&d.join("capacity"))
                        .and_then(|c| c.parse::<i32>().ok())
                        .unwrap_or(0),
                    match read(&d.join("status")).as_deref() {
                        Some("Charging") => 2,
                        Some("Discharging") => 3,
                        Some("Not charging") => 4,
                        Some("Full") => 5,
                        _ => 1,
                    },
                ))
            }
            Some("Mains") if is_online => online.push(1),
            Some(t) if t.starts_with("USB") && is_online => online.push(2),
            Some("Wireless") if is_online => online.push(4),
            _ => {}
        }
    }
    for p in [1, 2, 4] {
        if online.contains(&p) {
            plugged = p;
            break;
        }
    }
    let (present, level, status, capacity) = match battery {
        Some((present, level, status)) => (present, level, status, i64::from(level)),
        None => (false, 0, 1, i64::from(i32::MIN)),
    };
    let charging = !(plugged == 0 && status != 1);
    format!(
        "present={present} level={level} status={status} plugged={plugged} charging={charging} capacity={capacity} scale=100"
    )
}

/// Main.componentState (PackageManager component state, values from AOSP): AtlStateReceiver starts DEFAULT 0 (PackageSetting.getCurrentEnabledStateLPr with no override); DISABLED 2 reads back 2 and
/// getReceiverInfo no longer finds it (PackageUserStateUtils.isEnabled: an explicit DISABLED); DEFAULT 0 reads back 0 and it resolves again (restoreComponentLPw, the manifest's enabled=true);
/// the manifest-disabled AtlOffReceiver does not resolve, ENABLED 1 makes it resolve, DEFAULT hides it again; an undeclared class of the fixture (targetSdk 36 >= JELLY_BEAN) is
/// IllegalArgumentException and another package's component SecurityException (PackageManagerService.setEnabledSettings :3913-4004: ownership first). Without the methods (UNKNOWN_TRANSACTION
/// reads back no exception and 0) the row is `get0=0 disable=0 resolve=1 default=0 resolve2=1 off=0 enable=0 offresolve=0 offdefault=0 unknown=none other=none`.
const COMPONENT_STATE: Line = Line {
    row: "componentstate",
    prefix: "AtlTest: componentstate: ",
    want: Want::Exact(
        "get0=0 disable=2 resolve=0 default=0 resolve2=1 off=0 enable=1 offresolve=1 offdefault=0 unknown=IllegalArgumentException other=SecurityException",
    ),
    timing: Timing::Create,
};
/// AtlStateReceiver's first onReceive: the explicit broadcast Main.componentState sent while it was DISABLED reaches nothing (BroadcastQueue resolves at send time; activity.rs
/// deliver_to_component_locked asks Packages::enabled), so the first to arrive is the one sent after DEFAULT, n=2.
const COMPONENT_STATE_RECEIVE: Line = Line {
    row: "componentstate-receive",
    prefix: "AtlTest: componentstate-receive: ",
    want: Want::Exact("n=2"),
    timing: Timing::Async,
};

/// The connectivity line (Main.connectivity) for this host, read with the host's own tools, not the platform layer's code: the default route's interface from iproute2 (`ip route show default`, IPv4 then IPv6, lowest metric);
/// its kind from sysfs (`wireless` dir -> Wi-Fi agent "WIFI", else Ethernet agent "Ethernet"); the Internet verdict and metered flag as GIO's network monitor takes them from NetworkManager (`nmcli`: connectivity none -> offline,
/// full / unknown -> VALIDATED, limited / portal -> not; device GENERAL.METERED yes) and, without NetworkManager, as its netlink base does (a default route = full, not metered).
/// Online: the active network's NetworkInfo is CONNECTED (NetworkAgent.markConnected), INTERNET is held, and the default-network callback delivers the same network with the same values.
/// Offline: ConnectivityService with no agent (getActiveNetworkInfo null, isActiveNetworkMetered true) and no callback.
fn connectivity_want() -> String {
    const OFFLINE: &str =
        "active=null metered=true internet=false validated=false iface=null callback=none";
    let run = |args: &[&str]| -> Option<String> {
        let out = std::process::Command::new(args[0])
            .args(&args[1..])
            .output()
            .ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
    };
    let best = |text: String| -> Option<String> {
        text.lines()
            .filter_map(|l| {
                let w: Vec<&str> = l.split_whitespace().collect();
                let field = |k: &str| w.iter().position(|x| *x == k).and_then(|i| w.get(i + 1));
                Some((
                    field("metric")
                        .and_then(|m| m.parse::<u32>().ok())
                        .unwrap_or(0),
                    field("dev")?.to_string(),
                ))
            })
            .min()
            .map(|(_, dev)| dev)
    };
    let Some(iface) = run(&["ip", "-4", "route", "show", "default"])
        .and_then(best)
        .or_else(|| run(&["ip", "-6", "route", "show", "default"]).and_then(best))
    else {
        return OFFLINE.to_owned();
    };
    let (validated, metered) = match run(&["nmcli", "-t", "networking", "connectivity"]) {
        None => (true, false),
        Some(c) => {
            let metered = run(&[
                "nmcli",
                "-t",
                "-g",
                "GENERAL.METERED",
                "device",
                "show",
                &iface,
            ])
            .is_some_and(|m| m.trim_start().starts_with("yes"));
            match c.trim() {
                "none" => return OFFLINE.to_owned(),
                "limited" | "portal" => (false, metered),
                _ => (true, metered),
            }
        }
    };
    let kind = if Path::new("/sys/class/net")
        .join(&iface)
        .join("wireless")
        .exists()
    {
        "WIFI"
    } else {
        "Ethernet"
    };
    format!(
        "active={kind}/CONNECTED metered={metered} internet=true validated={validated} iface={iface} callback=same validated={validated} iface={iface}"
    )
}

/// The signatures line for an APK whose one signer's certificate SHA-256 is `cert` (from `apksigner verify --print-certs`, not our code): one signature and one current signer with that digest,
/// no rotation history (apksigner signs with one key, no lineage), and checkSignatures of a package with itself SIGNATURE_MATCH (0, PackageManager.java:1480).
fn signatures_want(cert: &str) -> String {
    format!("signatures=1 sig0={cert} signers=1 signer0={cert} history=false check=0")
}
/// Main.priority's requests in order (android.os.Process THREAD_PRIORITY_BACKGROUND 10, the start value, THREAD_PRIORITY_FOREGROUND -2); None = the start value.
const PRIORITY_ASKS: [(&str, Option<i32>); 3] = [
    ("background", Some(10)),
    ("restore", None),
    ("foreground", Some(-2)),
];

/// Why the features line is wrong under `cap`, else None. This headless run has:
/// - landscape: an always-present feature (atld_run.rs FEATURE_FILES, AOSP's own file in /system/etc/permissions) -> true;
/// - touchscreen: none, as no window takes input (atld declares it only with `--ui` and mouse-touch) -> false;
/// - aep: atld's advertised feature, the host's: equal to `gl-aep` (the app's own ES 3 context; GL_EXTENSIONS has GL_ANDROID_extension_pack_es31a);
/// - listed: getSystemAvailableFeatures holds landscape; `gles` (its GL ES entry) = `config` (ActivityManager.getDeviceConfigurationInfo().reqGlEsVersion), both ro.opengles.version, non-zero (the run probes GL).
fn features_why(line: Option<&str>) -> Option<String> {
    let Some(line) = line else {
        return Some("no line".to_owned());
    };
    let fields: BTreeMap<&str, &str> = line
        .split_whitespace()
        .filter_map(|f| f.split_once('='))
        .collect();
    let get = |k: &str| fields.get(k).copied();
    for (key, want) in [
        ("landscape", "true"),
        ("touchscreen", "false"),
        ("listed", "true"),
    ] {
        if get(key) != Some(want) {
            return Some(format!("{key}: want {want}, got {:?}", get(key)));
        }
    }
    // the host's: equal to the app's GL list
    let aep_ok = match (get("aep"), get("gl-aep")) {
        (Some(a), Some(g)) => a == g && (a == "true" || a == "false"),
        _ => false,
    };
    if !aep_ok {
        return Some(format!(
            "aep {:?} vs the app's GL {:?}",
            get("aep"),
            get("gl-aep")
        ));
    }
    match (get("gles"), get("config")) {
        (Some(g), Some(c)) if g == c && g != "0x0" => None,
        (g, c) => Some(format!(
            "GL ES entry {g:?} != configuration {c:?}, or unset"
        )),
    }
}

/// Why the priority line is wrong for a host whose app tiers apply nice >= `floor` (atl-broker NiceTiers), else None.
/// Every request must succeed, as on Android; Process.getThreadPriority then reads the kernel's value: the request when it is at or above the floor, the previous nice when it was answered without effect.
fn priority_why(line: Option<&str>, floor: i32) -> Option<String> {
    let Some(line) = line else {
        return Some("no priority line".into());
    };
    let fields: BTreeMap<&str, &str> = line
        .split_whitespace()
        .filter_map(|kv| kv.split_once('='))
        .collect();
    let Some(start) = fields.get("start").and_then(|s| s.parse::<i32>().ok()) else {
        return Some(format!("no start in {line:?}"));
    };
    let mut nice = start;
    for (name, ask) in PRIORITY_ASKS {
        let ask = ask.unwrap_or(start);
        if ask >= floor {
            nice = ask;
        }
        let want = format!("ok/{nice}");
        if fields.get(name) != Some(&want.as_str()) {
            return Some(format!(
                "{name}: want {want}, got {:?} in {line:?}",
                fields.get(name)
            ));
        }
    }
    None
}

/// The native bridge's report of a fault handed to a guest handler (engine/fault_report.rs): identity reports it in signal.rs guest_handler, a foreign guest's QEMU engine through `atl_host.fault_report` (qemu/mod.rs `host_fault_report`); bridge1's sigsegvRecover must produce one (SIGSEGV = 11).
fn guest_fault_report(foreign: bool) -> &'static str {
    if foreign {
        "atl-native-bridge: fatal-signal report: guest fault handed to QEMU: sig 11 "
    } else {
        "atl-native-bridge: fatal-signal report: fault handed to the guest's handler: sig 11 "
    }
}
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
/// ResultChild's getCallingActivity / getCallingPackage: the resultTo record, Main (ActivityClientController.getCallingRecord). ResultChild is started by its filter's implicit action, so the
/// line also shows the start resolved through the package manager (ActivityTaskSupervisor.resolveIntent).
const CALLING: &str = "AtlTest: calling ";
const CALLING_MAIN: &str = "activity=com.example.atltest.Main package=com.example.atltest";
/// Main.implicitNone: a market:// VIEW no installed activity filters -> START_INTENT_NOT_RESOLVED -> Instrumentation.checkStartActivityResult throws ActivityNotFoundException (AOSP without a store).
const IMPLICIT_NONE: &str = "AtlTest: implicit-none ";
const IMPLICIT_NONE_THROWS: &str = "threw ActivityNotFoundException";
const MAIN_RESUMED: &str = "AtlTest: main resumed";
const MAIN_NEW_INTENT: &str = "AtlTest: main new intent";
/// Main.onNewIntent's requestPermissions(POST_NOTIFICATIONS, 11) (Main.java onRequestPermissionsResult), the round trip's last line. With no permission controller installed (runtime-permissions P1) the
/// services answer it as GrantPermissionsActivity's dismissal: setResultIfNeeded returns the requested names and checkPermission of each for a targetSdk >= M app (36 here); the never-granted
/// runtime permission is PERMISSION_DENIED (-1), and checkSelfPermission stays -1 (a dismissal grants nothing). Activity.dispatchRequestPermissionsResult hands both arrays to the callback.
const PERMISSION_REQUEST: &str = "AtlTest: permission-request ";
const PERMISSION_DISMISSED: &str =
    "request=11 names=[android.permission.POST_NOTIFICATIONS] results=[-1] self=-1";
/// How long the watch after the marker waits for the next awaited line ([`awaited_logged`]) once WINDOW_MS has passed: each awaited line that arrives restarts it, so a chain of rows logged seconds apart
/// (Flags.java: 5-10 s per scenario under load) is bounded by its progress, not by one budget for the whole chain; a stalled chain, broken round trip or missing callback line fails this long after
/// the last line that came. 30 s: the old whole-chain budget, which every single step fits many times over.
const RESULT_STALL: Duration = Duration::from_secs(30);

/// The awaited lines the log holds, of how many: the round trip's last line ([`PERMISSION_REQUEST`]) and every Async row of [`judged_lines`].
fn awaited_logged(log: &str) -> (usize, usize) {
    std::iter::once(PERMISSION_REQUEST)
        .chain(
            judged_lines()
                .filter(|l| l.timing == Timing::Async)
                .map(|l| l.prefix),
        )
        .fold((0, 0), |(got, all), p| {
            (got + usize::from(log.contains(p)), all + 1)
        })
}

/// Whether the watch after the marker goes on: always within WINDOW_MS (a crash shows there), then while an awaited line is missing and the last progress (an awaited line arriving, or the
/// watch's start) is less than [`RESULT_STALL`] ago. `timed`: a [`ForegroundCase`] timer runs (the 30 s start timeout, then the ANR's delay): no step comes in between, so the watch waits it out.
fn watch_on(since_watch: Duration, since_progress: Duration, missing: bool, timed: bool) -> bool {
    since_watch < Duration::from_millis(WINDOW_MS)
        || timed
        || (missing && since_progress < RESULT_STALL)
}
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
    // atl_jni_test.c keyDtorDetach: destructor ran once; DetachCurrentThread and AttachCurrentThread both JNI_OK (0); return and pthread_exit endings
    ("keyDtorDetachReturn", "ran 1 rc 0 attach 0"),
    ("keyDtorDetachExit", "ran 1 rc 0 attach 0"),
    // mode 2: the destructor's own AttachCurrentThread JNI_OK (0, already attached) and GetVersion = JNI_VERSION_1_6 (0x10006 = 65542, ART's answer)
    (
        "keyDtorAttach",
        "ran 1 rc 0 attach 0 dtor-attach 0 version 65542",
    ),
    ("errnoRoundTrip", "2"),
    ("cppThrowCatch", "1"),
    ("dlopenSibling", "42"),
    ("greet", "hello, bridge1"),
    ("callJavaAdd", "5"),
    ("registeredAnswer", "7"),
    // P5 JNI objects rows (atl_jni_test.c, AtlJniTest wrappers), values from the JNI specification and ART (src/aosp/art/runtime/jni/jni_internal.cc):
    // GetStringUTFChars always copies (isCopy 1); "é中" is 2 + 3 = 5 modified-UTF-8 bytes (c3a9 e4b8ad); GetStringChars of a movable string copies (1); GetStringRegion(1, 3) of "bridge1" = "rid"; GetStringCritical copies a compressed (ASCII, kUseStringCompression) string (1) and hands out an uncompressed one's own chars (0); NewString {'H','i'}
    (
        "stringChars",
        "utf bridge1 7 copy 1 wide 5 chars e9 4e2d copy 1 region rid c3a9e4b8ad critical b 1 wide e9 0 new Hi",
    ),
    // a movable int[] is copied (1); JNI_COMMIT copies back 11 and keeps the copy, JNI_ABORT then drops the 22; double {0.5} * 4 released with mode 0 = 2.0; bytes {0,0,0,1} with region {7,9} set sum to 17 through GetPrimitiveArrayCritical (ART: the array's own data, isCopy 0), whose write of 5 at [3] Java sees
    (
        "arrayElements",
        "int copy 1 region 11 2 3 double copy 1 critical copy 0 sum 17 len 4 java 11 2 3 2.0 [7, 9, 0, 5]",
    ),
    // NewDirectByteBuffer over the guest's 8 bytes {1..8}: same address back, capacity 8, Java reads [7] = 8; ByteBuffer.allocateDirect(16): capacity 16, the native write of 42 at [0] seen by Java
    ("directBuffer", "same 1 cap 8 16 java 1 get 8 42"),
    // (1, 0x100000002, 3, 2.5, -5, 1.25f) back as text; 0x100000002 * 3 + 1 = 12884901895; 2.5 * 2 + 1.25 = 6.25; mixLong(0x100000000, 2.5, 7) = 4294967296 + 25 + 7 through `...`, V and A
    (
        "wideArgs",
        "args 1 4294967298 3 2.500 -5 1.25 long 12884901895 double 6.25 var 4294967328 V 4294967328 A 4294967328",
    ),
    // ExceptionCheck 1 after ThrowNew, 0 after ExceptionClear; ExceptionOccurred an IllegalStateException, then null; the NewObject'ed exception thrown and pending across the native return
    ("exceptions", "caught check 1 0 occurred 1 null 1"),
    // MonitorEnter twice JNI_OK (0, reentrant), Thread.holdsLock true (1); MonitorExit twice 0, then false
    ("monitors", "enter 0 0 held 1 exit 0 0 held 0"),
    // GetObjectRefType: JNILocalRefType 1, JNIGlobalRefType 2, JNIWeakGlobalRefType 3 (jni.h); IsSameObject 1 1; PushLocalFrame 0, PopLocalFrame's result the same object (1); join("ab", 0x100000003, "obj") by CallStaticObjectMethodA; StringBuilder("x"/"y"/"z") by NewObject, NewObjectV, NewObjectA
    (
        "jniRefs",
        "types 1 2 3 same 1 1 frame 0 1 join ab4294967299obj new x y z",
    ),
    // a guest pthread: AttachCurrentThread JNI_OK (0), javaAdd(20, 22) = 42, DetachCurrentThread 0
    ("guestThreadAttach", "attach 0 add 42 detach 0"),
    ("sigsegvRecover", "1"),
    ("nullFieldRead", "caught"),
    ("jitUpCall", "8000000"),
    ("guestLog", "1"),
    // 10 (read_callback's guest callback ran once, on the caller's guest thread) + 1 (foreach's, every call): atl_jni_test.c propCallbacks
    ("propCallbacks", "11"),
    // GLES2 through the GL proxies: 64x64 pixels that equal the scene's geometry (jni-test atl_jni_test.c glesPbuffer)
    ("glesPbuffer", "4096"),
    // after glesPbuffer, the process maps libEGL_atl.so (a /proc/self/maps line naming it): the proxies reached the driver, not a stand-in
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
    // aaudio-guest-route (atl_jni_test.c aaudioProbe): the guest's dlopen finds the libaaudio proxy; open, start, stop, close answer AAUDIO_OK (0, AAudio.h); the data callback ran AAUDIO_PROBE_CALLS times through its down shell, never on the caller's thread and with its own stream and user data (bad 0); getFramesWritten covers what the callbacks produced (frames 1). Red before the proxy: "aaudio dlopen null"
    (
        "aaudioProbe",
        "aaudio open 0 start 0 calls 1 frames 1 bad 0 stop 0 close 0",
    ),
    // the same in Angry Birds 2's FMOD shape (aaudioFloatProbe): AAUDIO_FORMAT_PCM_FLOAT stereo at 24000 Hz, a float track the flinger serves (flinger.rs mixer_format). Red with float refused: "aaudio open <negative AAudio error>"
    (
        "aaudioFloatProbe",
        "aaudio open 0 start 0 calls 1 frames 1 bad 0 stop 0 close 0",
    ),
    // net-bp (engine/netdb.rs; atl_jni_test.c resolverProbe): getaddrinfo / freeaddrinfo / getnameinfo / gai_strerror through the bridge on both engines; the failures carry bionic's EAI_* codes (service 9, numerichost 8), the loop is 1000 lookups with every node freed; herr 1 = a failed gethostbyname left HOST_NOT_FOUND or TRY_AGAIN in the guest's h_errno
    (
        "resolverProbe",
        "numeric 1 passive 1 service 9 numerichost 8 text 1 loop 1000 list ok herr 1",
    ),
    // tr-first blocker 5 (atl_jni_test.c looperNow / looperArm / looperLater; engine/ndk.rs `call_mapped` hands the host a down shell for the guest callback): (a) one call, on the calling thread, pollOnce = ALOOPER_POLL_CALLBACK (-2, looper.h), no wrong user data; (b) the main Looper's later call of the callback a fresh thread registered: one call, on the main thread, which is not the arming one, no wrong user data
    ("looperNow", "now 1 1 -2 0"),
    ("looperArm", "armed"),
    ("looperLater", "later 1 1 1 0"),
    // candy-crush-next-blockers (atl_jni_test.c vsyncFrames; build.rs CALLBACKS `AChoreographer_vsyncCallback`): VSYNC_FRAMES (8) guest vsync callbacks on the main Looper, each re-posted from the previous one; frame times and preferred vsync ids strictly increasing, every timeline's deadline <= its presentation > frame time, presentations increasing (bad 0); median frame-time step within 25% of the display period Java reports (cadence 1). Red without the binding: the stub aborts the process
    ("vsyncFrames", "vsync 8 bad 0 cadence 1"),
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
/// The [`BRIDGE1_CALLS`] labels the QEMU engine does not carry yet: C++ exceptions through the guest unwinder (cppThrowCatch), the null-field fault (nullFieldRead), the JIT up-call (jitUpCall), guest liblog / property callbacks (guestLog, propCallbacks), the EGL config list (FIRST_CONFIG), and the C12 legacy library (legacyStdio, legacyNew, zRoundTrip).
/// Each is skipped by Main and must log `skipped`.
const QEMU_DEFERRED: &[&str] = &[
    "cppThrowCatch",
    "nullFieldRead",
    "jitUpCall",
    "guestLog",
    "propCallbacks",
    FIRST_CONFIG,
    "legacyStdio",
    "legacyNew",
    "zRoundTrip",
];
/// Main.logIdentity's line: `key=value` fields, space-separated.
const IDENTITY: &str = "AtlTest: identity ";

/// What the app process reports of its own width and ABIs (Main.logIdentity), against AOSP's rules for an app of fixture ISA `isa` (arches.toml `pointer_bits`, first `android_abis` entry `primary_abi`):
/// `Process.is64Bit()` = the ISA is 64-bit (a 32-bit app runs in a 32-bit zygote); `Build.CPU_ABI` / `CPU_ABI2` = entries 0 / 1 ("" when none) of `SUPPORTED_64_BIT_ABIS` or `SUPPORTED_32_BIT_ABIS` by that answer (Build.java:320-326);
/// `SUPPORTED_ABIS` (`ro.product.cpu.abilist`) holds the 64-bit and 32-bit lists, whatever the process width (board_config.mk: abilist = 64-bit + 32-bit ABIs); `primaryCpuAbi` the APK's ABI and `nativeLibraryDir` `.../lib/<isa>` (PackageManager's NativeLibraryHelper choice, VMRuntime.getInstructionSet).
/// `os.arch` is logged, not judged: AOSP takes it from `uname().machine` (libcore System.java) or the bridge's `getAppEnv` (libnativebridge `SetupEnvironment`), and neither the kernel's 32-bit answer nor a `getAppEnv` before Build's natives is settled here.
fn identity_failures(app: &str, isa: &str, pointer_bits: u32, primary_abi: &str) -> Vec<String> {
    let Some(line) = app
        .lines()
        .find_map(|l| l.split_once(IDENTITY).map(|(_, r)| r.trim_end()))
    else {
        return vec![format!("identity: no {IDENTITY:?} line")];
    };
    let field = |k: &str| {
        line.split(' ')
            .find_map(|f| f.strip_prefix(k)?.strip_prefix('='))
            .unwrap_or("<missing>")
    };
    let list = |k: &str| -> Vec<&str> { field(k).split(',').filter(|s| !s.is_empty()).collect() };
    let is64 = pointer_bits == 64;
    let width = if is64 { list("abis64") } else { list("abis32") };
    let want = [
        ("is64", is64.to_string()),
        ("cpu_abi", width.first().copied().unwrap_or("").to_owned()),
        ("cpu_abi2", width.get(1).copied().unwrap_or("").to_owned()),
        ("primary", primary_abi.to_owned()),
    ];
    let mut whys: Vec<String> = want
        .iter()
        .filter(|(k, v)| field(k) != v)
        .map(|(k, v)| format!("identity {k}: want {v}, got {} ({line})", field(k)))
        .collect();
    let mut all = list("abis");
    let mut both = [list("abis64"), list("abis32")].concat();
    all.sort_unstable();
    both.sort_unstable();
    if all != both || all.is_empty() {
        whys.push(format!("identity abis: want abis64 + abis32, got {line}"));
    }
    if !field("libdir").ends_with(&format!("/lib/{isa}")) {
        whys.push(format!("identity libdir: want .../lib/{isa}, got {line}"));
    }
    whys
}

#[cfg(test)]
mod identity_tests {
    use super::*;

    /// An arm app on a 64-bit device with abilist64 "x86_64,arm64-v8a" and abilist32 "armeabi-v7a,armeabi" (hand-written, as board_config.mk builds them): a 32-bit process takes CPU_ABI / CPU_ABI2 from the 32-bit list (Build.java:320-326). The 64-bit answers -- what ART's own `is64Bit` gives without the launcher -- fail on is64, cpu_abi and cpu_abi2.
    #[test]
    fn a_32_bit_process_reports_the_32_bit_list() {
        let lists = "abis=x86_64,arm64-v8a,armeabi-v7a,armeabi abis32=armeabi-v7a,armeabi abis64=x86_64,arm64-v8a libdir=/data/app/p/lib/arm primary=armeabi-v7a os.arch=x86_64";
        let good =
            format!("I AtlTest: identity is64=false cpu_abi=armeabi-v7a cpu_abi2=armeabi {lists}");
        assert!(identity_failures(&good, "arm", 32, "armeabi-v7a").is_empty());
        let host =
            format!("I AtlTest: identity is64=true cpu_abi=x86_64 cpu_abi2=arm64-v8a {lists}");
        assert_eq!(identity_failures(&host, "arm", 32, "armeabi-v7a").len(), 3);
        assert_eq!(identity_failures("", "arm", 32, "armeabi-v7a").len(), 1);
    }
}

/// The diag key prefix of the bridge's refusals of a host pointer to a 32-bit guest (atl-native-bridge engine dispatch.rs `REFUSED_*`).
const ILP32_REFUSAL: &str = "native-bridge.ilp32-";
/// The bridge-1 calls a 32-bit guest carries (32-bit JNI objects: slice 1, the guest's JavaVM / JNIEnv in `JNI_OnLoad` with GetEnv, FindClass and RegisterNatives, and native methods answering an int; slice 2, variadic Call*Method: `callJavaAdd`, `jitUpCall`, then strings and arrays as window copies, direct buffers, jlong/jdouble, exceptions, monitors, references, NewObject*). Every other call of a 32-bit fixture is deferred until the slice that carries it (guest threads, the NDK rows).
const ILP32_CARRIED: &[&str] = &[
    "tlsCounter#1",
    "tlsCounter#2",
    "tlsCounter#3",
    "errnoRoundTrip",
    "dlopenSibling",
    "registeredAnswer",
    "callJavaAdd",
    "jitUpCall",
    "greet",
    "stringChars",
    "arrayElements",
    "directBuffer",
    "wideArgs",
    "exceptions",
    "monitors",
    "jniRefs",
    "guestThreadAttach",
];
/// The calls a run of fixture ISA `isa` (guest pointer width `pointer_bits`) on host `host` skips: none for the host's (identity), [`QEMU_DEFERRED`] for a foreign 64-bit one, all but [`ILP32_CARRIED`] for a 32-bit one.
fn deferred(isa: &str, host: &str, pointer_bits: u32) -> Vec<&'static str> {
    if isa == host {
        Vec::new()
    } else if pointer_bits == 32 {
        BRIDGE1_CALLS
            .iter()
            .map(|&(name, _)| name)
            .chain([FIRST_CONFIG, CPU_PROBE, CPUINFO_VIEW])
            .filter(|name| !ILP32_CARRIED.contains(name))
            .collect()
    } else {
        QEMU_DEFERRED.to_vec()
    }
}

/// Every bridge-1 call whose line is missing or wrong (a `deferred` label's line must be `skipped`), then the thread balance.
fn bridge1_call_failures(
    app: &str,
    deferred: &[&str],
    foreign: bool,
    cpu: &Result<HostFacts, String>,
) -> Vec<String> {
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
    // every carried call runs without the bridge refusing a host pointer to a 32-bit guest (engine dispatch.rs `report_refused`, keys `native-bridge.ilp32-*`): a JNI method's class or reference argument, a reference ART returned, the guest's env or vm
    if let Some(l) = app.lines().find(|l| l.contains(ILP32_REFUSAL)) {
        whys.push(format!("bridge1: a host pointer refused to the guest: {l}"));
    }
    // a deferred cpu row must log `skipped`, as every deferred row
    for name in [CPU_PROBE, CPUINFO_VIEW] {
        if deferred.contains(&name) && got(name).as_deref() != Some(SKIPPED) {
            whys.push(format!(
                "bridge1 call {name}: want {SKIPPED}, got {:?}",
                got(name)
            ));
        }
    }
    match cpu {
        _ if deferred.contains(&CPU_PROBE) => {}
        Ok(cpu) => {
            if let Some(why) = cpu_probe_why(got(CPU_PROBE).as_deref(), foreign, cpu) {
                whys.push(format!("bridge1 call {CPU_PROBE}: {why}"));
            }
        }
        Err(e) => whys.push(format!(
            "bridge1 call {CPU_PROBE}: the host's CPU facts: {e}"
        )),
    }
    let host_word = cpuinfo_features(cpu.as_ref().map_or("", |c| c.cpuinfo.as_str()));
    if !deferred.contains(&CPUINFO_VIEW)
        && let Some(why) = cpuinfo_view_why(got(CPUINFO_VIEW).as_deref(), foreign, host_word)
    {
        whys.push(format!("bridge1 call {CPUINFO_VIEW}: {why}"));
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
        // no row that starts a thread ran (a 32-bit fixture defers them), so no thread detached
        None if deferred.contains(&"tlsCounterFreshThread") && deferred.contains(&"threadSum") => {}
        other => whys.push(format!(
            "bridge1 threads: want attached = detached + 1 (the main thread), got {other:?}"
        )),
    }
    whys
}
/// Main.bridge1Calls' cpuProbe row (atl_jni_test.c cpuProbe): `conf C onln O affinity A java J cpuinfo P features F`, judged by [`cpu_probe_why`] against this host's own answers.
const CPU_PROBE: &str = "cpuProbe";
/// The host CPU lists an app's counts must equal: Android lets every domain read the whole dir (sepolicy domain.te `r_dir_file(domain, sysfs_devices_system_cpu)`).
const CPU_POSSIBLE: &str = "/sys/devices/system/cpu/possible";
const CPU_ONLINE: &str = "/sys/devices/system/cpu/online";

/// The run host's answers the cpuProbe / cpuinfoView / timezone rows are judged against: CPU counts, /proc/cpuinfo and time zone of the machine atld ran on.
struct HostFacts {
    /// [`CPU_POSSIBLE`]'s count
    possible: usize,
    /// [`CPU_ONLINE`]'s count
    online: usize,
    /// CPUs in the affinity mask atld inherits
    affinity: usize,
    /// /proc/cpuinfo text
    cpuinfo: String,
    /// [`ZONE_SH`]'s output: zone id, then its TZif footer
    zone: String,
}

use crate::remote::{CPU_INFO, HOST_FACTS, ZONE_SH};

impl HostFacts {
    /// This host's own answers (a local run: the app inherits atld's affinity and environment, which inherit the gate's).
    fn here() -> Result<Self, String> {
        let read = |p: &str| std::fs::read_to_string(p).map_err(|e| format!("{p}: {e}"));
        let count =
            |p: &str| cpu_list_count(&read(p)?).ok_or_else(|| format!("{p} does not parse"));
        let set = rustix::thread::sched_getaffinity(None)
            .map_err(|e| format!("the gate's own sched_getaffinity: {e}"))?;
        Ok(Self {
            possible: count(CPU_POSSIBLE)?,
            online: count(CPU_ONLINE)?,
            affinity: (0..rustix::thread::CpuSet::MAX_CPU)
                .filter(|&i| set.is_set(i))
                .count(),
            cpuinfo: read("/proc/cpuinfo")?,
            zone: std::process::Command::new("sh")
                .args(["-c", ZONE_SH])
                .output()
                .map_err(|e| format!("sh -c ZONE_SH: {e}"))
                .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())?,
        })
    }

    /// The target's answers a remote run brought back in `scratch`.
    fn of_remote(scratch: &Path) -> Result<Self, String> {
        let read = |n: &str| {
            let p = scratch.join(n);
            std::fs::read_to_string(&p).map_err(|e| format!("{}: {e}", p.display()))
        };
        let facts = read(HOST_FACTS)?;
        let mut lines = facts.lines();
        let mut next = |what: &str, parse: fn(&str) -> Option<usize>| {
            lines
                .next()
                .and_then(parse)
                .ok_or_else(|| format!("{HOST_FACTS}: no {what} line"))
        };
        Ok(Self {
            possible: next("possible", cpu_list_count)?,
            online: next("online", cpu_list_count)?,
            affinity: next("nproc", |l| l.trim().parse().ok())?,
            cpuinfo: read(CPU_INFO)?,
            zone: lines.collect::<Vec<_>>().join("\n"),
        })
    }

    /// The timezone row this host must show: `id=<zone> raw=<ms>`, raw = the footer's standard offset (POSIX TZ: `std offset ...`, offset `[+-]hh[:mm[:ss]]` west of Greenwich, so negated), as libcore's getRawOffset.
    fn zone_want(&self) -> Result<String, String> {
        let mut lines = self.zone.lines();
        let (Some(id), Some(footer)) = (lines.next(), lines.next()) else {
            return Err(format!(
                "zone facts: want id and footer lines, got {:?}",
                self.zone
            ));
        };
        let raw = posix_std_offset_s(footer)
            .ok_or_else(|| format!("zone facts: footer {footer:?} does not parse"))?;
        Ok(format!("id={id} raw={}", -raw * 1000))
    }
}

/// The std offset (seconds, positive west) of a POSIX TZ string (XBD 8.3): std a `<...>` quoted name or 3+ letters, then `[+-]hh[:mm[:ss]]`. None when it does not parse.
fn posix_std_offset_s(tz: &str) -> Option<i64> {
    let rest = match tz.strip_prefix('<') {
        Some(q) => q.split_once('>')?.1,
        None => tz.trim_start_matches(|c: char| c.is_ascii_alphabetic()),
    };
    let (sign, rest) = match rest.as_bytes().first()? {
        b'-' => (-1, &rest[1..]),
        b'+' => (1, &rest[1..]),
        _ => (1, rest),
    };
    let end = rest
        .find(|c: char| !c.is_ascii_digit() && c != ':')
        .unwrap_or(rest.len());
    let mut secs = 0;
    let mut scale = 3600;
    for part in rest[..end].split(':') {
        if part.is_empty() || scale == 0 {
            return None;
        }
        secs += part.parse::<i64>().ok()? * scale;
        scale /= 60;
    }
    Some(sign * secs)
}

/// The number of CPUs a kernel cpu list names (Documentation/admin-guide/cputopology.rst: `0-3,8,10-11`); None when it does not parse.
fn cpu_list_count(list: &str) -> Option<usize> {
    list.trim().split(',').try_fold(0, |n, part| {
        let (a, b) = part.split_once('-').unwrap_or((part, part));
        let (a, b): (usize, usize) = (a.parse().ok()?, b.parse().ok()?);
        (b >= a).then(|| n + b - a + 1)
    })
}

/// The features word of a /proc/cpuinfo text, by the fixture's rule: "asimd" from a `Features` line, "sse2" from a `flags` line, else "none".
fn cpuinfo_features(text: &str) -> &'static str {
    let mut word = "none";
    for line in text.lines() {
        let words = || {
            line.split_once(':')
                .map(|(_, w)| w.split_whitespace())
                .into_iter()
                .flatten()
        };
        if line.starts_with("Features") && words().any(|w| w == "asimd") {
            word = "asimd";
        } else if line.starts_with("flags") && words().any(|w| w == "sse2") {
            word = "sse2";
        }
    }
    word
}

/// The cpuProbe row's judge. Each count must equal the host's answer to the same question: conf the possible list, onln, java and the cpuinfo `processor` lines the online list, affinity this process's affinity mask (the app inherits atld's, which inherits the gate's).
/// features: an identity guest sees the host's own cpuinfo (the host's word); a foreign guest (`foreign`, QEMU) must see its own ISA's, so a word that is neither the host's nor "none". `None`: the row passes.
fn cpu_probe_why(value: Option<&str>, foreign: bool, cpu: &HostFacts) -> Option<String> {
    let Some(value) = value else {
        return Some("no line".to_owned());
    };
    let HostFacts {
        possible,
        online,
        affinity,
        ref cpuinfo,
        ..
    } = *cpu;
    let host_word = cpuinfo_features(cpuinfo);
    let fields: BTreeMap<&str, &str> = value
        .split_whitespace()
        .collect::<Vec<_>>()
        .chunks(2)
        .filter_map(|c| Some((*c.first()?, *c.get(1)?)))
        .collect();
    let want = [
        ("conf", possible),
        ("onln", online),
        ("java", online),
        ("cpuinfo", online),
        ("affinity", affinity),
    ];
    let mut wrong: Vec<String> = want
        .iter()
        .filter(|&&(k, n)| fields.get(k).and_then(|v| v.parse::<usize>().ok()) != Some(n))
        .map(|&(k, n)| format!("{k} want {n}"))
        .collect();
    let features = fields.get("features").copied().unwrap_or("missing");
    let features_ok = if foreign {
        features != host_word && features != "none" && features != "missing"
    } else {
        features == host_word
    };
    if !features_ok {
        wrong.push(format!(
            "features {features:?}: want {} (host's is {host_word:?})",
            if foreign {
                "the guest ISA's own word"
            } else {
                "the host's"
            }
        ));
    }
    (!wrong.is_empty()).then(|| format!("{value:?}: {}", wrong.join(", ")))
}

/// Main.bridge1Calls' cpuinfoView row (atl_jni_test.c cpuinfoView): `same features W` or `differs java J guest G bytes A/B`.
const CPUINFO_VIEW: &str = "cpuinfoView";

/// The cpuinfoView row's judge: Java and the app's native code must read one /proc/cpuinfo (on a device the bridge's cpuinfo.<isa>.txt is bound over it), the native code's own ISA's: a foreign guest's word is neither
/// `host_word` nor "none" (atld's guest cpuinfo bound for Java), an identity guest's is the host's. `None`: the row passes.
fn cpuinfo_view_why(value: Option<&str>, foreign: bool, host_word: &str) -> Option<String> {
    let Some(value) = value else {
        return Some("no line".to_owned());
    };
    let ok = match value.strip_prefix("same features ") {
        Some(w) if foreign => w != host_word && w != "none",
        Some(w) => w == host_word,
        None => false,
    };
    (!ok).then(|| {
        format!(
            "{value:?}: want `same features <{}>` (host's word {host_word:?})",
            if foreign {
                "the guest ISA's word"
            } else {
                "the host's word"
            }
        )
    })
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

/// A name the fixture looks up that nobody serves: Main.permission()'s PackageManager.checkPermission fetches PermissionManager, whose fetcher asks `getServiceOrThrow("legacy_permission")`
/// (SystemServiceRegistry). The day the services serve it, this row goes red by design (the plan's break): move it to another unserved name.
const MISSING_JAVA: &str = "legacy_permission";

/// The missing-service row (plan diagnostics-f4 D5): the app log counts `missing-service legacy_permission` (SystemServiceRegistry's "No service published for:" line, atl-diag
/// MISSING_SERVICE_LINES). At least once: a fetcher that found nothing may ask again. A native miss (libbinder's 5 s give-up) has no bounded trigger an app can make by name (libbinder's
/// getService is not NDK API; Java's waitForService never returns for an unserved name, window.rs UNSERVED_WAIT): atl-diag's unit test and the pin test cover its line.
fn missing_service_failures(app: &str) -> Vec<String> {
    let count = counters(app)
        .iter()
        .find(|c| c.outcome == atl_diag::MISSING_SERVICE && c.api == MISSING_JAVA)
        .map_or(0, |c| c.count);
    if count == 0 {
        vec![format!(
            "missing-services: the app log holds no `missing-service {MISSING_JAVA}`"
        )]
    } else {
        vec![]
    }
}

/// The web-process row: no line of the shim's failure (atl_broker::webspawn::SHIM_FAILED, written to the app's stderr = the app log) -> None; else how many and the first, which carries the broker's reason.
fn web_spawn_failure(app: &str) -> Option<String> {
    let mut failed = app.lines().filter_map(|l| {
        l.find(atl_broker::webspawn::SHIM_FAILED)
            .map(|i| l[i..].to_owned())
    });
    let first = failed.next()?;
    Some(format!(
        "web-process starts: {} failed, first {first:?}",
        1 + failed.count()
    ))
}

/// Each line atl-diag reads a missing service from is in the pinned source that writes it, in MISSING_SERVICE_LINES order, as (file, literal with the name's place).
#[cfg(test)]
const MISSING_SERVICE_SOURCES: [(&str, &str); 3] = [
    (
        "aosp/frameworks/base/core/java/android/os/ServiceManager.java",
        "\"No service published for: \" + name",
    ),
    (
        "aosp/frameworks/native/libs/binder/IServiceManager.cpp",
        "\"Service %s didn't start. Returning NULL\"",
    ),
    (
        "aosp/frameworks/native/cmds/servicemanager/ServiceManager.cpp",
        // C string concatenation splits the format at the source line break: checked up to it
        "Since '%s' could not be found trying to start it as a lazy AIDL service. (if it's ",
    ),
];

/// Needs the fetched tree; without it the run cannot build either.
#[cfg(test)]
fn missing_service_lines_exist_at_the_pin(layout: &Layout) -> Result<()> {
    for ((before, after), (file, literal)) in atl_diag::summary::MISSING_SERVICE_LINES
        .iter()
        .zip(MISSING_SERVICE_SOURCES)
    {
        let path = layout.locate(Path::new(file)).dir;
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        anyhow::ensure!(
            text.contains(literal),
            "{} lacks {literal:?}",
            path.display()
        );
        // the parser's halves are the literal's text around the name
        anyhow::ensure!(
            literal.contains(before.trim_start()) && literal.contains(after.trim_end()),
            "atl-diag MISSING_SERVICE_LINES ({before:?}, {after:?}) is not {literal:?}"
        );
    }
    Ok(())
}

/// Main.java resultToSecurity's line, and what it must read: the services' SecurityException reached the app.
const RESULT_TO_SECURITY: &str = "AtlTest: result-to-security ";
const RESULT_TO_SECURITY_WANT: &str = "threw SecurityException";
/// The key the reply hook counts that refusal under (atl-diag `binder_reply`: outcome `exception:SECURITY`, api `<Interface>.<method>`).
const SECURITY_OUTCOME: &str = "exception:SECURITY";
const START_ACTIVITY_API: &str = "IActivityTaskManager.startActivity";

/// The resultTo refusal row (plan diagnostics-f4 D3): the app's line says it threw SecurityException and the services counted `exception:SECURITY` for startActivity at least once
/// (a lower bound until the services exit cleanly: the fixture makes one such call, and no other call of the run is refused that way).
fn result_to_security_failures(app: &str, services: &str) -> Vec<String> {
    let mut whys = Vec::new();
    let got = app
        .lines()
        .find_map(|l| l.split_once(RESULT_TO_SECURITY).map(|(_, r)| r.trim_end()));
    if got != Some(RESULT_TO_SECURITY_WANT) {
        whys.push(format!(
            "result-to-security: want {RESULT_TO_SECURITY_WANT:?}, got {got:?}"
        ));
    }
    let counted = counters(services)
        .iter()
        .any(|c| c.outcome == SECURITY_OUTCOME && c.api == START_ACTIVITY_API && c.count > 0);
    if !counted {
        whys.push(format!("result-to-security: no `{SECURITY_OUTCOME} {START_ACTIVITY_API}` count in the services log"));
    }
    whys
}

const DEX_LOAD_API: &str = "IPackageManager.notifyDexLoad";

/// Every app's startup dex-load report (DexLoadReporter, a non-empty classLoaderContextMap through the generated Proxy's typed Map) reached PackageService's handler: it counts
/// `answered IPackageManager.notifyDexLoad (not recorded)`. A map the codec cannot decode never reaches the handler (refused-parcel + status:BAD_VALUE instead).
fn dex_load_failures(services: &str) -> Vec<String> {
    let counted = counters(services)
        .iter()
        .any(|c| c.outcome == atl_diag::ANSWERED && c.api.starts_with(DEX_LOAD_API) && c.count > 0);
    if counted {
        Vec::new()
    } else {
        vec![format!(
            "dex-load: no `{} {DEX_LOAD_API}` count in the services log",
            atl_diag::ANSWERED
        )]
    }
}

/// Main.drawingCache: `drawing-cache app=<bytes> real=<w>x<h>`. The application context's resources carry the global configuration, whose maxBounds is the display
/// (DisplayContent.computeScreenConfiguration:2486-2488), and ViewConfiguration's limit is 4 x maxBounds area (ViewConfiguration.java:485, ARGB_8888 bytes): so 4 x w x h of the real size.
const DRAWING_CACHE: &str = "AtlTest: drawing-cache ";

fn drawing_cache_failures(app: &str) -> Vec<String> {
    let Some(rest) = app
        .lines()
        .find_map(|l| l.split_once(DRAWING_CACHE).map(|(_, r)| r.trim_end()))
    else {
        return vec![format!("drawing-cache: no {DRAWING_CACHE:?} line")];
    };
    let parsed = rest.split_once(" real=").and_then(|(app, real)| {
        let bytes: i64 = app.strip_prefix("app=")?.parse().ok()?;
        let (w, h) = real.split_once('x')?;
        Some((bytes, w.parse::<i64>().ok()?, h.parse::<i64>().ok()?))
    });
    match parsed {
        Some((bytes, w, h)) if w > 0 && h > 0 && bytes == 4 * w * h => Vec::new(),
        Some((bytes, w, h)) => vec![format!(
            "drawing-cache: application context limit {bytes}, want 4 x {w} x {h} (maxBounds = the display)"
        )],
        None => vec![format!("drawing-cache: unreadable {rest:?}")],
    }
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
    let permission = app
        .lines()
        .find_map(|l| l.split_once(PERMISSION_REQUEST).map(|(_, r)| r.trim_end()));
    if permission != Some(PERMISSION_DISMISSED) {
        whys.push(format!(
            "permission request: want {PERMISSION_DISMISSED:?}, got {permission:?}"
        ));
    }
    for (prefix, want) in [
        (CALLING, CALLING_MAIN),
        (IMPLICIT_NONE, IMPLICIT_NONE_THROWS),
    ] {
        let got = app
            .lines()
            .find_map(|l| l.split_once(prefix).map(|(_, r)| r.trim_end()));
        if got != Some(want) {
            whys.push(format!("{}: want {want:?}, got {got:?}", prefix.trim_end()));
        }
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
        "app",
        args.verbose,
    )?
    // the activity's window renders through hwui, which needs EGL: the allocator and the GL driver are part of every run (needs a DRM device that allocates, WSL: vgem)
    .with_gralloc(root)?
    .with_gl_driver_breaks(root, &args.egl_break)?;
    // the host ISA's guest exists only where its identity engine does (arches.toml `identity`); elsewhere every fixture is foreign
    let identity = crate::arches::identity(root, &args.host)?;
    let rt = if identity {
        rt.with_native_bridge_guest(&args.host)?
    } else {
        rt
    };
    let isa = args.fixture_arch.as_deref().unwrap_or(&args.host);
    ensure!(
        identity || isa != args.host,
        "--host {}: no identity engine (arches.toml), so the fixture needs a foreign --fixture-arch",
        args.host
    );
    let (pointer_bits, primary_abi) = crate::arches::load(root)?
        .get(isa)
        .map(|a| {
            let abi = a.guest.as_ref().and_then(|g| g.android_abis.first());
            (a.pointer_bits, abi.cloned().unwrap_or_default())
        })
        .with_context(|| format!("--fixture-arch {isa}: no arches.toml row"))?;
    let deferred = &deferred(isa, &args.host, pointer_bits)[..];
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
    let rt = if args.bind_break {
        rt.without_bind_service()
    } else {
        rt
    };
    let rt = if args.job_runner_break {
        rt.without_job_runner()
    } else {
        rt
    };
    let rt = if args.result_break {
        rt.without_activity_result()
    } else {
        rt
    };
    let rt = if args.cpuinfo_break {
        rt.without_cpuinfo()
    } else {
        rt
    };
    let rt = if args.priority_break {
        rt.without_app_seccomp()
    } else {
        rt
    };
    let rt = if args.font_break {
        rt.with_sans_serif(&layout, test_fonts::MONO.2)?
    } else {
        rt
    };
    let rt = if args.features_break {
        rt.without_feature_files()
    } else {
        rt
    };
    let rt = if args.webview_provider_break {
        rt.without_webview_provider()
    } else {
        rt
    };
    let rt = if args.settings_provider_break {
        rt.without_settings_provider()
    } else {
        rt
    };
    let rt = if args.webview_engine_break {
        rt.without_webview_engine()?
    } else {
        rt
    };
    let rt = if args.webview_sandbox_break {
        rt.without_webspawn()
    } else {
        rt
    };
    let work = rt.out.join("app");
    // UI mode on a nested compositor: an app on a desktop has a display, and WebKit cannot make its settings (the default UA's source) without one -- WebKitSettings' constructor
    // dereferences a missing GDK display (atl-webview-linux `default_user_agent`); its AppleWebKit token is a literal in WebKit's UserAgentGLib.cpp, not derivable from webkit_get_*_version
    // a remote run takes the target's display (remote.rs Display: its session's socket, else labwc nested in its X11 session) and the full judge; with none it falls back to the headless subset ([`REMOTE_SKIPPED`])
    let display = match &args.remote {
        Some(r) => {
            let (display, why) = crate::remote::probe_display(&r.host)?;
            if display == crate::remote::Display::Headless {
                println!("app: remote display: none ({why}): falling back to the headless subset");
            } else {
                println!("app: remote display: {why}: the full gate (UI mode)");
            }
            display
        }
        None => crate::remote::Display::Headless,
    };
    let (_nested, rt) = if args.remote.is_some() {
        if display == crate::remote::Display::Headless {
            (None, rt)
        } else {
            (None, rt.with_ui("gtk"))
        }
    } else {
        let nested = crate::nested_wayland::NestedCompositor::start(&work.join("nested"))?;
        let rt = rt.with_ui("gtk").with_wayland(&nested.socket);
        (Some(nested), rt)
    };
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
    let cert = rt.apk.signer.cert_sha256(&apk)?;
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
        libraries: vec![],
        shell: None,
        log_tags: args.log_tags.clone(),
    };
    if let Some(r) = &args.remote {
        rt.record_inputs(&run)?;
        let target = crate::remote::Target::of(root, &rt.out, "app", &r.host, r.brk)?;
        if !r.apply {
            println!("{}", target.summary());
            println!(
                "dry run: pushes the bundle to {}, checks {} ELF files there, then runs {} {} with atld's limit at {} s; --apply runs it",
                target.target(),
                target.elf_count(),
                m.name,
                match display {
                    crate::remote::Display::Headless => "headless (the headless subset)",
                    crate::remote::Display::Session => "on the session's display (the full gate)",
                    crate::remote::Display::NestedX11 =>
                        "on labwc nested in the X11 session (the full gate)",
                },
                REMOTE_LIMIT.as_secs()
            );
            return Ok(ExitCode::SUCCESS);
        }
        let (pushed, problems) = target.deliver("app")?;
        if !problems.is_empty() {
            println!("{}", crate::verdict::gate("remote app", &problems));
            return Ok(ExitCode::FAILURE);
        }
        let dir = rt
            .out
            .join(crate::remote::REMOTE_DIR)
            .join(target.log_dir());
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let log = dir.join("app.log");
        std::fs::write(&log, "").with_context(|| format!("writing {}", log.display()))?;
        let rt = rt.with_site(target.run_site(&pushed, REMOTE_LIMIT, log.clone(), display)?);
        let child = rt.start_atld(&run)?;
        if display != crate::remote::Display::Headless {
            let (rc, atld_exited, elapsed) = remote_end(&rt, &run, child)?;
            let code = judge(&AppJudge {
                run: &run,
                app_name: &m.name,
                verbose: args.verbose,
                libs: &native.libs,
                layout: &layout,
                cert: &cert,
                deferred,
                isa,
                pointer_bits,
                primary_abi: &primary_abi,
                foreign: isa != args.host,
                scene: &scene,
                activity: &activity,
                elapsed,
                passed: atl_diag::logfiles::read(&run.log(&m.name))
                    .lines()
                    .any(|l| l.contains(MARKER)),
                atld_exited,
                no_marker: format!("no {MARKER:?} line (atld rc {rc})"),
                local: None,
                sticky: &StickyCase::default(),
                foreground: &ForegroundCase::default(),
                desktop: None,
            })?;
            println!("remote test app: transcript {}", log.display());
            return Ok(code);
        }
        let code = remote_verdict(
            &rt,
            &run,
            child,
            &m.name,
            &native.libs,
            deferred,
            isa != args.host,
            &scene,
        )?;
        println!("remote test app: transcript {}", log.display());
        return Ok(code);
    }
    // the notification rows' desktop: a private bus with the fake freedesktop service (session_bus.rs); atld finds it through its environment only
    let bus = crate::session_bus::PrivateBus::start(&work.join("bus"))?;
    let fake = crate::session_bus::FakeNotifications::serve(&bus)?;
    let rt = rt.with_desktop_env(&[("DBUS_SESSION_BUS_ADDRESS".to_owned(), bus.address.clone())]);
    let mut atld_child = rt.start_atld(&run)?;
    let app_log = run.log(&m.name);
    let start = Instant::now();
    let read = atl_diag::logfiles::read;
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
    // the watch after the marker: a crash shows within it (window.rs WINDOW_MS); the round trip and every Async row run in it too, until they are all logged
    // or RESULT_STALL passes with no new one ([`watch_on`]); the app's death ends the watch, atld outlives it
    let mut ended = ended;
    let mut sticky = StickyCase::default();
    let mut foreground = ForegroundCase::default();
    if passed && ended.is_none() {
        let watch = Instant::now();
        let (mut seen, mut progress) = (0, watch);
        while ended.is_none() {
            let now = sticky_wall_ms();
            sticky.poll(&run, &m.name);
            foreground.poll(&run, &m.name, now);
            let steps = sticky.steps() + foreground.steps();
            let (got, all) = awaited_logged(&read(&app_log));
            if got + steps > seen {
                (seen, progress) = (got + steps, Instant::now());
            }
            if !watch_on(
                watch.elapsed(),
                progress.elapsed(),
                got < all || !sticky.finished(now) || !foreground.finished(now),
                foreground.timer_running(now),
            ) {
                break;
            }
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
    let no_marker = match ended {
        Some(s) => format!("atld exited {s} before the marker"),
        None if elapsed > LIMIT => format!("no marker after {LIMIT:?}"),
        None => "no marker".to_owned(),
    };
    judge(&AppJudge {
        run: &run,
        app_name: &m.name,
        verbose: args.verbose,
        libs: &native.libs,
        layout: &layout,
        cert: &cert,
        deferred,
        isa,
        pointer_bits,
        primary_abi: &primary_abi,
        foreign: isa != args.host,
        scene: &scene,
        activity: &activity,
        elapsed,
        passed,
        atld_exited: ended.is_some(),
        no_marker,
        local: Some(outside),
        sticky: &sticky,
        foreground: &foreground,
        desktop: Some(fake.calls()),
    })
}

/// The STICKY case's processes (S6c): AtlSticky (START_STICKY) in `:sticky`, AtlNotSticky (START_NOT_STICKY) in `:notsticky`; each in its own process, so its log is
/// `log/<pkg>:<process>.log`. Once both logged their first start, the gate ends both on purpose (SIGKILL to the host pid atld logged as `started <process>`).
/// The exemption is these two ends and no other: a killed process is expected, not a fault, and not the run's end (atld's run end follows the first app process, the main one).
const STICKY_PROC: &str = ":sticky";
const NOT_STICKY_PROC: &str = ":notsticky";
/// AtlSticky's first start (startId 1) and its sticky restart (startId 2, null Intent); AtlNotSticky's first start.
const STICKY_FIRST: &str = "AtlTest: sticky: onStartCommand startId=1 action=atl.test.STICKY";
const STICKY_RESTART: &str = "AtlTest: sticky: onStartCommand startId=2 action=null";
const NOT_STICKY_FIRST: &str =
    "AtlTest: notsticky: onStartCommand startId=1 action=atl.test.NOTSTICKY";
/// ActivityManagerConstants.java:223 DEFAULT_SERVICE_RESTART_DURATION: a killed service's restart comes this long after the kill (ActiveServices.scheduleServiceRestartLocked).
const SERVICE_RESTART_DURATION_MS: u64 = 1_000;
/// ActivityManagerConstants.java:226 DEFAULT_SERVICE_MIN_RESTART_TIME_BETWEEN: a restart of a service that died with another one waits this long after it.
const SERVICE_MIN_RESTART_TIME_BETWEEN_MS: u64 = 10_000;
/// ActivityManagerService.java:568 PROC_START_TIMEOUT: the restarted process must attach within it, else it failed to attach (activity.rs PROC_START_TIMEOUT_MS).
const PROC_START_TIMEOUT_MS: u64 = 10_000;
/// Above the upper bound: the death's binder notice and the spawn's own latency, which the rule's numbers do not count.
const STICKY_SLACK_MS: u64 = 1_000;
/// After the kill, the not-sticky process must stay down this long: the rule's restart of a second service that died with the first one, with its own delay.
const NOT_STICKY_WINDOW_MS: u64 = SERVICE_RESTART_DURATION_MS + SERVICE_MIN_RESTART_TIME_BETWEEN_MS;

/// The STICKY case's state: [`StickyCase::poll`] in the watch after the marker, [`StickyCase::verdict`] in the judge.
#[derive(Default)]
struct StickyCase {
    /// the kill: wall ms when it was sent, and atld's not-sticky starts logged before it
    killed: Option<(u64, usize)>,
    /// the first kill that failed (no pid, or the signal), as the reason
    kill_failed: Option<String>,
    /// the sticky restart line's stamp, wall ms
    restart_ms: Option<u64>,
}

impl StickyCase {
    /// Progress steps for the watch: the kill and the restart line.
    fn steps(&self) -> usize {
        usize::from(self.killed.is_some()) + usize::from(self.restart_ms.is_some())
    }

    /// Whether the case has run its course at `now` (wall ms): the restart is logged and the not-sticky window has passed.
    fn finished(&self, now: u64) -> bool {
        match (self.killed, self.restart_ms) {
            (Some((at, _)), Some(_)) => now >= at + NOT_STICKY_WINDOW_MS,
            _ => false,
        }
    }

    /// One watch step: once both first starts are logged, kills both processes; then looks for the sticky restart line.
    fn poll(&mut self, run: &Run, package: &str) {
        let log = |proc: &str| atl_diag::logfiles::read(&run.log(&format!("{package}{proc}")));
        if self.killed.is_none() {
            if !(log(STICKY_PROC).contains(STICKY_FIRST)
                && log(NOT_STICKY_PROC).contains(NOT_STICKY_FIRST))
            {
                return;
            }
            let atld = atl_diag::logfiles::read(&run.atld_log());
            self.killed = Some((
                sticky_wall_ms(),
                started_count(&atld, package, NOT_STICKY_PROC),
            ));
            for proc in [STICKY_PROC, NOT_STICKY_PROC] {
                let end = last_started_pid(&atld, package, proc)
                    .ok_or_else(|| format!("no `started` line for {package}{proc} in atld's log"))
                    .and_then(|pid| {
                        kill_pid(pid).map_err(|e| format!("killing {package}{proc} pid {pid}: {e}"))
                    });
                if let Err(e) = end {
                    self.kill_failed.get_or_insert(e);
                }
            }
            return;
        }
        if self.restart_ms.is_none() {
            self.restart_ms = log(STICKY_PROC)
                .lines()
                .find(|l| l.contains(STICKY_RESTART))
                .and_then(stamp_ms);
        }
    }

    /// The pass line, or the reason the case failed. `atld` is atld's log at judge time: a not-sticky start after the kill is the restart it must not have.
    fn verdict(&self, atld: &str, package: &str) -> Result<String, String> {
        let Some((at, starts_before)) = self.killed else {
            return Err(
                "sticky: the first starts were never logged (AtlSticky, AtlNotSticky)".to_owned(),
            );
        };
        if let Some(e) = &self.kill_failed {
            return Err(format!("sticky: {e}"));
        }
        let Some(restart) = self.restart_ms else {
            return Err(format!("sticky: no {STICKY_RESTART:?} line after the kill"));
        };
        let gap = restart.saturating_sub(at);
        let (lo, hi) = (
            SERVICE_RESTART_DURATION_MS,
            SERVICE_RESTART_DURATION_MS + PROC_START_TIMEOUT_MS + STICKY_SLACK_MS,
        );
        if !(lo..=hi).contains(&gap) {
            return Err(format!(
                "sticky: restart {gap} ms after the kill, want {lo}..={hi} ms (the restart timer, then an attach within PROC_START_TIMEOUT)"
            ));
        }
        let restarted = started_count(atld, package, NOT_STICKY_PROC).saturating_sub(starts_before);
        if restarted > 0 {
            return Err(format!(
                "notsticky: {restarted} start(s) after its process was ended: a NOT_STICKY service is not restarted (killServicesLocked, canStopIfKilled)"
            ));
        }
        Ok(format!(
            "sticky: restart {gap} ms after the kill (want {lo}..={hi}), notsticky: not restarted in {NOT_STICKY_WINDOW_MS} ms"
        ))
    }
}

/// The wall clock in ms, the clock of the app's logcat stamps ([`stamp_ms`]).
fn sticky_wall_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|d| u64::try_from(d.as_millis()).ok())
        .unwrap_or(0)
}

/// A logcat line's stamp (`<secs>.<ms>`, its first field) in wall ms.
fn stamp_ms(line: &str) -> Option<u64> {
    let (secs, frac) = line.split_whitespace().next()?.split_once('.')?;
    Some(secs.parse::<u64>().ok()? * 1_000 + frac.get(..3)?.parse::<u64>().ok()?)
}

/// The `started` lines atld logged for `proc` of `package`: one per start of that process.
fn started_lines<'a>(atld: &'a str, package: &str, proc: &str) -> impl Iterator<Item = &'a str> {
    let needle = format!("started {package}{proc} (package ");
    atld.lines().filter(move |l| l.contains(&needle))
}

fn started_count(atld: &str, package: &str, proc: &str) -> usize {
    started_lines(atld, package, proc).count()
}

/// The host pid of the newest start of `proc` (atld's `started ..., host pid <n>`).
fn last_started_pid(atld: &str, package: &str, proc: &str) -> Option<i32> {
    started_lines(atld, package, proc)
        .last()?
        .rsplit("host pid ")
        .next()?
        .trim()
        .parse()
        .ok()
}

/// SIGKILL to a process atld started, by its host pid: the STICKY case's own end of a process, not a product path.
fn kill_pid(pid: i32) -> Result<(), String> {
    let pid = rustix::process::Pid::from_raw(pid).ok_or_else(|| "pid 0".to_owned())?;
    rustix::process::kill_process(pid, rustix::process::Signal::KILL).map_err(|e| e.to_string())
}

#[cfg(test)]
mod sticky_case_tests {
    use super::*;

    const ATLD_START: &str = "atld: started com.example.atltest:notsticky (package com.example.atltest): uid 10001, host pid ";

    /// The case as killed at wall 10 000 with `restart` logged: the restart's gap from the kill is the verdict's number.
    fn case(restart: u64) -> StickyCase {
        StickyCase {
            killed: Some((10_000, 1)),
            kill_failed: None,
            restart_ms: Some(restart),
        }
    }

    #[test]
    fn stamp_is_the_logcat_first_field_in_wall_ms() {
        assert_eq!(
            stamp_ms("1793747156.733 1 2 I AtlTest: sticky: x"),
            Some(1_793_747_156_733)
        );
        assert_eq!(stamp_ms("no stamp here"), None);
    }

    /// Lower bound: SERVICE_RESTART_DURATION (1 000 ms) after the kill; upper: that plus PROC_START_TIMEOUT (10 000) plus 1 000 of slack = 12 000 ms.
    #[test]
    fn restart_gap_bounds_are_the_rule() {
        let pass = |restart| {
            case(restart)
                .verdict(ATLD_START, "com.example.atltest")
                .is_ok()
        };
        assert!(!pass(10_999), "999 ms: before the restart timer");
        assert!(pass(11_000), "exactly SERVICE_RESTART_DURATION");
        assert!(pass(22_000), "exactly 12 000 ms");
        assert!(!pass(22_001), "past the attach budget");
    }

    /// A not-sticky start logged after the kill (atld's `started` line) fails the verdict; the starts before the kill do not count.
    #[test]
    fn not_sticky_restart_fails_and_its_first_start_does_not_count() {
        let before = case(11_000);
        let atld = format!("{ATLD_START}7\n");
        assert!(before.verdict(&atld, "com.example.atltest").is_ok());
        let restarted = format!("{ATLD_START}7\n{ATLD_START}9\n");
        let why = StickyCase {
            killed: Some((10_000, 1)),
            ..case(11_000)
        }
        .verdict(&restarted, "com.example.atltest")
        .unwrap_err();
        assert!(why.starts_with("notsticky: 1 start(s)"), "{why}");
    }
}

/// The `:fgs` process of the `fgs` row (AtlFgs, dataSync) and the `:fgstimeout` process of the `fgstimeout` row (AtlFgsSilent, never startForeground): each its own process,
/// so its log is `log/<pkg><proc>.log`.
const FGS_PROC: &str = ":fgs";
const FGS_TIMEOUT_PROC: &str = ":fgstimeout";
/// AtlFgs's startForeground returned (logged after the call).
const FGS_STARTED: &str = "AtlTest: fgs: startForeground ok";
/// AtlFgsSilent's onStartCommand (the delivered start arms the timer, ActiveServices :6199-6204) and onDestroy (the timeout's stop, ActiveServices :7628).
const FGS_TIMEOUT_START: &str = "AtlTest: fgstimeout: onStartCommand";
const FGS_TIMEOUT_STOP: &str = "AtlTest: fgstimeout: onDestroy";
/// The services' counters the rows prove: the foreground state ends with its process (ActiveServices, uid FGS reason drops); the ANR ends a headless process (AS :7640).
const FGS_ENDED: &str = "IActivityManager.foreground service ended: uid FGS reason drops";
const FGS_ANR_KILLED: &str =
    "IActivityManager.fgs start timeout: headless process ended (silent bg ANR)";
/// ActivityManagerConstants.java:318 DEFAULT_SERVICE_START_FOREGROUND_TIMEOUT_MS: a started service must call startForeground within this long, else it is stopped (AS :7597).
const FGS_START_TIMEOUT_MS: u64 = 30_000;
/// ActivityManagerConstants.java:320 DEFAULT_SERVICE_START_FOREGROUND_ANR_DELAY_MS: the ANR follows the stop this long later (AS :7640), and a headless process ends then.
const FGS_ANR_DELAY_MS: u64 = 10_000;
/// The stamps' latency (dispatch, logcat), as STICKY_SLACK_MS.
const FGS_SLACK_MS: u64 = 1_000;
/// Sampled this long after the stop, well before the ANR: alive here shows the ANR, not something earlier, is what ends the process.
const FGS_ALIVE_AFTER_STOP_MS: u64 = 5_000;

/// The FGS rows' state: [`ForegroundCase::poll`] in the watch after the marker, [`ForegroundCase::verdicts`] in the judge. `fgs`: the gate kills `:fgs` once its startForeground is logged,
/// and the foreground state must end with the process. `fgstimeout`: AtlFgsSilent is left alone; it is stopped 30 s after its start and the headless process ends 10 s after that.
#[derive(Default)]
struct ForegroundCase {
    /// the `:fgs` kill: wall ms when it was sent
    fgs_killed: Option<u64>,
    /// the first kill that failed (no pid, or the signal), as the reason
    kill_failed: Option<String>,
    /// `:fgstimeout`: onStartCommand's stamp, its host pid (atld's newest `started` line), onDestroy's stamp (wall ms)
    timeout_start: Option<u64>,
    timeout_pid: Option<i32>,
    timeout_stop: Option<u64>,
    /// the process's state sampled at stop + FGS_ALIVE_AFTER_STOP_MS, and at the ANR's end (stop + FGS_ANR_DELAY_MS + FGS_SLACK_MS): true = gone
    alive_after_stop: Option<bool>,
    gone_after_anr: Option<bool>,
}

impl ForegroundCase {
    /// Progress steps for the watch: the kill, each stamp, each sample.
    fn steps(&self) -> usize {
        [
            self.fgs_killed.is_some(),
            self.timeout_start.is_some(),
            self.timeout_stop.is_some(),
            self.alive_after_stop.is_some(),
            self.gone_after_anr.is_some(),
        ]
        .into_iter()
        .filter(|&set| set)
        .count()
    }

    /// The latest wall ms the `fgstimeout` case can end at, from its start: the timeout's upper bound, the ANR delay, and the slacks of the stop and of the sample.
    fn timeout_window_end(start: u64) -> u64 {
        start + FGS_START_TIMEOUT_MS + FGS_SLACK_MS + FGS_ANR_DELAY_MS + FGS_SLACK_MS + FGS_SLACK_MS
    }

    /// Whether the `fgstimeout` case has run its course at `now` (wall ms): its ANR's end sampled, or its window run out (the verdict then names what is missing).
    fn timeout_done(&self, now: u64) -> bool {
        self.gone_after_anr.is_some()
            || self
                .timeout_start
                .is_some_and(|s| now >= Self::timeout_window_end(s))
    }

    /// Whether the `fgstimeout` timer runs at `now`: the watch waits it out ([`watch_on`] `timed`), since no progress step comes between the start and the stop.
    fn timer_running(&self, now: u64) -> bool {
        self.timeout_start.is_some() && !self.timeout_done(now)
    }

    /// Whether both rows have run their course at `now`: the `fgs` kill has settled (its counter is written at the death), and `fgstimeout` is done.
    fn finished(&self, now: u64) -> bool {
        self.fgs_killed.is_some_and(|at| now >= at + FGS_SLACK_MS) && self.timeout_done(now)
    }

    /// One watch step: kills `:fgs` once its startForeground is logged; records `:fgstimeout`'s stamps, then the process's state at the two marks (`now`: wall ms).
    fn poll(&mut self, run: &Run, package: &str, now: u64) {
        let log = |proc: &str| atl_diag::logfiles::read(&run.log(&format!("{package}{proc}")));
        let pid_of = |proc: &str| {
            last_started_pid(&atl_diag::logfiles::read(&run.atld_log()), package, proc)
        };
        if self.fgs_killed.is_none() && log(FGS_PROC).contains(FGS_STARTED) {
            self.fgs_killed = Some(now);
            let end = pid_of(FGS_PROC)
                .ok_or_else(|| format!("no `started` line for {package}{FGS_PROC} in atld's log"))
                .and_then(|pid| {
                    kill_pid(pid).map_err(|e| format!("killing {package}{FGS_PROC} pid {pid}: {e}"))
                });
            if let Err(e) = end {
                self.kill_failed.get_or_insert(e);
            }
        }
        let timeout = log(FGS_TIMEOUT_PROC);
        if self.timeout_start.is_none()
            && let Some(start) = timeout
                .lines()
                .find(|l| l.contains(FGS_TIMEOUT_START))
                .and_then(stamp_ms)
        {
            self.timeout_start = Some(start);
            self.timeout_pid = pid_of(FGS_TIMEOUT_PROC);
        }
        if self.timeout_stop.is_none() {
            self.timeout_stop = timeout
                .lines()
                .find(|l| l.contains(FGS_TIMEOUT_STOP))
                .and_then(stamp_ms);
        }
        let (Some(stop), Some(pid)) = (self.timeout_stop, self.timeout_pid) else {
            return;
        };
        if self.alive_after_stop.is_none() && now >= stop + FGS_ALIVE_AFTER_STOP_MS {
            self.alive_after_stop = Some(process_alive(pid));
        }
        if self.gone_after_anr.is_none() && now >= stop + FGS_ANR_DELAY_MS + FGS_SLACK_MS {
            self.gone_after_anr = Some(!process_alive(pid));
        }
    }

    /// One result per row: the pass line, or the reason it failed. `services` is atld's services log: the counters are written at its orderly exit, after the judged run.
    fn verdicts(&self, services: &str) -> [Result<String, String>; 2] {
        [self.fgs_verdict(services), self.timeout_verdict(services)]
    }

    fn fgs_verdict(&self, services: &str) -> Result<String, String> {
        if let Some(e) = &self.kill_failed {
            return Err(format!("fgs: {e}"));
        }
        if self.fgs_killed.is_none() {
            return Err(format!(
                "fgs: no {FGS_STARTED:?} line in log/<pkg>{FGS_PROC}.log (AtlFgs)"
            ));
        }
        let ended = answered_count(services, FGS_ENDED);
        if ended == 0 {
            return Err(format!(
                "fgs: the kill of {FGS_PROC} left no foreground state ended: no {FGS_ENDED:?} count"
            ));
        }
        Ok(format!(
            "fgs: startForeground ok; the kill ended the foreground state (x{ended})"
        ))
    }

    fn timeout_verdict(&self, services: &str) -> Result<String, String> {
        let (Some(start), Some(stop)) = (self.timeout_start, self.timeout_stop) else {
            return Err(format!(
                "fgstimeout: no {FGS_TIMEOUT_START:?} and {FGS_TIMEOUT_STOP:?} lines (AtlFgsSilent)"
            ));
        };
        // the timer is armed at the start's delivery, before onStartCommand logs its stamp (AS :6199-6204): the stop can fall a little before 30 s after the stamp
        let stopped_after = stop.saturating_sub(start);
        let (lo, hi) = (
            FGS_START_TIMEOUT_MS - FGS_SLACK_MS,
            FGS_START_TIMEOUT_MS + FGS_SLACK_MS,
        );
        if !(lo..=hi).contains(&stopped_after) {
            return Err(format!(
                "fgstimeout: stopped {stopped_after} ms after its start, want {lo}..={hi} ms (ActivityManagerConstants :318)"
            ));
        }
        if self.timeout_pid.is_none() {
            return Err(format!(
                "fgstimeout: no `started` line for {FGS_TIMEOUT_PROC} in atld's log"
            ));
        }
        if self.alive_after_stop != Some(true) {
            return Err(format!(
                "fgstimeout: alive {FGS_ALIVE_AFTER_STOP_MS} ms after its stop = {:?}, want Some(true): something ended it before the ANR",
                self.alive_after_stop
            ));
        }
        if self.gone_after_anr != Some(true) {
            return Err(format!(
                "fgstimeout: gone {} ms after its stop = {:?}, want Some(true): the silent bg ANR did not end it",
                FGS_ANR_DELAY_MS + FGS_SLACK_MS,
                self.gone_after_anr
            ));
        }
        let killed = answered_count(services, FGS_ANR_KILLED);
        if killed == 0 {
            return Err(format!("fgstimeout: no {FGS_ANR_KILLED:?} count"));
        }
        Ok(format!(
            "fgstimeout: stopped {stopped_after} ms after its start (want {lo}..={hi}); alive at +{FGS_ALIVE_AFTER_STOP_MS} ms; ended by the ANR (x{killed})"
        ))
    }
}

/// The total of an `answered` key in atld's services log (`diag: total`, written at its orderly exit); 0 when absent.
fn answered_count(services: &str, key: &str) -> u64 {
    counters(services)
        .iter()
        .find(|c| c.outcome == "answered" && c.api == key)
        .map_or(0, |c| c.count)
}

/// Whether host pid `pid` still runs: `/proc/<pid>/stat` is there and its state is not a zombie (Z) or dead (X), since the parent may not have reaped it yet.
fn process_alive(pid: i32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
        stat.rsplit(')')
            .next()
            .and_then(|rest| rest.trim_start().chars().next())
            .is_some_and(|state| !matches!(state, 'Z' | 'X'))
    })
}

#[cfg(test)]
mod foreground_case_tests {
    use super::*;

    /// atld's services log with the two counters' totals.
    fn services(fgs_ended: u64, anr_killed: u64) -> String {
        format!(
            "diag: total answered IActivityManager.foreground service ended: uid FGS reason drops {fgs_ended} first=1793747150.000\n\
             diag: total answered IActivityManager.fgs start timeout: headless process ended (silent bg ANR) {anr_killed} first=1793747180.000\n"
        )
    }

    /// The case as the rows log it: `:fgs` killed at 5 000 ms; `:fgstimeout` started at 10 000 ms, stopped 30 s later (AMC :318), alive 5 s after that and gone at the ANR's end.
    fn passing() -> ForegroundCase {
        ForegroundCase {
            fgs_killed: Some(5_000),
            kill_failed: None,
            timeout_start: Some(10_000),
            timeout_pid: Some(77),
            timeout_stop: Some(10_000 + 30_000),
            alive_after_stop: Some(true),
            gone_after_anr: Some(true),
        }
    }

    #[test]
    fn a_case_as_logged_passes_both_rows() {
        let verdicts = passing().verdicts(&services(1, 1));
        assert!(verdicts.iter().all(Result::is_ok), "{verdicts:?}");
    }

    /// The stop is judged within one slack either side of AOSP's 30 s (ActivityManagerConstants :318): both edges pass, a millisecond past either fails.
    #[test]
    fn the_stop_must_come_30_s_after_the_start() {
        let stopped = |after: u64| ForegroundCase {
            timeout_stop: Some(10_000 + after),
            ..passing()
        };
        assert!(stopped(30_000).timeout_verdict(&services(1, 1)).is_ok());
        assert!(stopped(29_000).timeout_verdict(&services(1, 1)).is_ok());
        assert!(stopped(31_000).timeout_verdict(&services(1, 1)).is_ok());
        assert!(stopped(31_001).timeout_verdict(&services(1, 1)).is_err());
        assert!(stopped(28_999).timeout_verdict(&services(1, 1)).is_err());
    }

    /// The process must run past the ANR's start and end when the ANR is what ends it: alive at +5 s, gone at +11 s (AMC :320 + one slack).
    #[test]
    fn the_process_is_alive_before_the_anr_and_gone_after_it() {
        let alive_at_5 = ForegroundCase {
            alive_after_stop: Some(false),
            ..passing()
        };
        assert!(
            alive_at_5
                .timeout_verdict(&services(1, 1))
                .unwrap_err()
                .contains("before the ANR")
        );
        let still_there = ForegroundCase {
            gone_after_anr: Some(false),
            ..passing()
        };
        assert!(
            still_there
                .timeout_verdict(&services(1, 1))
                .unwrap_err()
                .contains("silent bg ANR")
        );
    }

    #[test]
    fn the_anr_count_and_the_foreground_end_count_must_be_seen() {
        assert!(
            passing()
                .timeout_verdict(&services(1, 0))
                .unwrap_err()
                .contains("silent bg ANR")
        );
        assert!(
            passing()
                .fgs_verdict(&services(0, 1))
                .unwrap_err()
                .contains("foreground state ended")
        );
        assert!(passing().fgs_verdict(&services(1, 0)).is_ok());
    }

    /// The timer runs from the start to the window's end (30 s + 1 s, the ANR's 10 s, two 1 s slacks = 43 s), and the watch waits it out.
    #[test]
    fn the_timer_runs_until_its_window_ends() {
        let timed = ForegroundCase {
            timeout_start: Some(10_000),
            ..ForegroundCase::default()
        };
        assert!(timed.timer_running(10_000 + 42_999));
        assert!(!timed.timer_running(10_000 + 43_000));
        assert!(!ForegroundCase::default().timer_running(10_000));
    }
}

/// What [`judge`] reads besides the run's logs.
struct AppJudge<'a> {
    run: &'a Run,
    /// the STICKY case's state (polled by the watch; judged here)
    sticky: &'a StickyCase,
    /// the FGS rows' state (polled by the watch; judged here)
    foreground: &'a ForegroundCase,
    app_name: &'a str,
    verbose: bool,
    libs: &'a [PathBuf],
    layout: &'a Layout,
    cert: &'a str,
    deferred: &'a [&'a str],
    isa: &'a str,
    /// The fixture ISA's arches.toml `pointer_bits` and first `android_abis` entry ([`identity_failures`]).
    pointer_bits: u32,
    primary_abi: &'a str,
    /// The fixture's ISA is not the host's (a guest run through the bridge).
    foreign: bool,
    scene: &'a Scene,
    activity: &'a str,
    /// Start to marker (or to the give-up).
    elapsed: Duration,
    /// The marker was logged.
    passed: bool,
    atld_exited: bool,
    /// Why there is no marker, for the verdict.
    no_marker: String,
    /// A run on this host: the children-outside-broker snapshot (None = atld had ended first). None here = a remote UI run, whose checks that read this host
    /// (its /proc, its nice limit, its network) or need the gate's own stop (exit totals) are skipped and named ([`REMOTE_UI_SKIPPED`]).
    local: Option<Option<Vec<String>>>,
    /// What the fake notification service on the run's private bus received ([`NOTIFY_CALLS`]); None = a remote run (no private bus there: atld's tier is none).
    desktop: Option<Vec<crate::session_bus::Call>>,
}

/// The checks a remote UI run (app.rs `remote` with a target display) does not claim, each with why (printed as `app: SKIP`); every other row of the local gate is judged from the logs that came back.
const REMOTE_UI_SKIPPED: [(&str, &str); 4] = [
    REMOTE_SKIPPED[0],
    REMOTE_SKIPPED[1],
    (
        "priority",
        "its want derives from this host's RLIMIT_NICE (atl_broker::granted_nice), not the target's",
    ),
    (
        "connectivity",
        "its want derives from this host's default route and NetworkManager, not the target's",
    ),
];

/// The app gate's verdict from a finished run's logs: one judge for a run on this host and a remote UI run.
fn judge(j: &AppJudge) -> Result<ExitCode> {
    let AppJudge {
        run,
        sticky,
        foreground,
        app_name,
        verbose,
        libs,
        layout,
        cert,
        deferred,
        isa,
        pointer_bits,
        primary_abi,
        foreign,
        scene,
        activity,
        elapsed,
        passed,
        atld_exited,
        ref no_marker,
        ref local,
        ref desktop,
    } = *j;
    let read = atl_diag::logfiles::read;
    let app = read(&run.log(app_name));
    let fault = fault(&app, atld_exited);
    let services_log = read(&run.log("services"));
    if verbose {
        println!("---- app log\n{app}");
    }
    let remote = local.is_none();
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
    let mut whys = jni_fixture::shape_failures(libs)?;
    if let Some(f) = &fault {
        whys.push(format!("the app faulted: {f}"));
        println!("---- app log (last {TAIL} lines)\n{}", tail(&app, TAIL));
    }
    if passed && !cacerts.starts_with("trusted ") {
        whys.push(format!("CA store: {cacerts}"));
    }
    let atld_log = read(&run.atld_log());
    for l in summary_rows(&atld_log).iter().take(SUMMARY_SHOWN) {
        println!("app: summary {l}");
    }
    for l in block_rows(&atld_log, PROP_SUMMARY_PREFIX)
        .iter()
        .take(SUMMARY_SHOWN)
    {
        println!("app: prop-summary {l}");
    }
    // rows about what the app did need an app that got past onCreate's marker: without it they would fail first and hide the no-marker verdict and its log tails (an ART abort at start showed only as missing-services)
    if passed {
        whys.extend(prop_block_failures(&atld_log));
        whys.extend(device_set_failures(&app));
        whys.extend(missing_service_failures(&app));
    }
    // the STICKY case runs only in the local watch; a remote run has no watch and judges nothing here
    if passed && !remote {
        match sticky.verdict(&atld_log, app_name) {
            Ok(pass) => println!("app: pass {pass}"),
            Err(why) => whys.push(why),
        }
        // the FGS rows (ForegroundCase) run in the same watch, from their own processes' logs
        for verdict in foreground.verdicts(&services_log) {
            match verdict {
                Ok(pass) => println!("app: pass {pass}"),
                Err(why) => whys.push(why),
            }
        }
    }
    if !remote {
        whys.extend(exit_failures(
            passed,
            &services_log,
            &atld_log,
            &read(&run.log("binderd")),
        ));
    }
    match local {
        Some(Some(o)) => {
            println!("app: counter children_outside_broker {}", o.len());
            if !o.is_empty() {
                whys.push(format!("atld children outside the broker: {o:?}"));
            }
        }
        Some(None) if passed => whys.push(
            "children_outside_broker not measured: atld had ended before the snapshot".into(),
        ),
        _ => {}
    }
    let fonts_line = app
        .lines()
        .find_map(|l| l.split_once(FONTS).map(|(_, r)| r.to_owned()));
    if passed {
        whys.extend(
            font_check(layout, fonts_line.as_deref())?
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
    // every web-process start WebKit asks for must start: a failed one leaves its view's load waiting with no callback, and a webview row shows it only when WebKit has no other process to use
    if let Some(why) = web_spawn_failure(&app) {
        whys.push(why);
    }
    // a host-derived want (connectivity) is this host's: a remote run skips it (REMOTE_UI_SKIPPED)
    for (service, prefix, want) in SERVICE_LINES
        .iter()
        .filter(|(_, _, w)| !remote || w.is_some())
    {
        let want = service_want(service, *want);
        let got = app
            .lines()
            .find_map(|l| l.split_once(prefix).map(|(_, r)| r.to_owned()));
        if passed && got.as_deref() != Some(want.as_str()) {
            whys.push(format!("{service}: want {want:?}, got {got:?}"));
        }
    }
    let facts = if remote {
        HostFacts::of_remote(&run.scratch)
    } else {
        HostFacts::here()
    };
    if passed {
        let served = counters(&services_log);
        for (service, key) in SERVICE_CALLS {
            if !served
                .iter()
                .any(|c| c.outcome == "answered" && c.api == key && c.count > 0)
            {
                whys.push(format!("{service}: the services did not answer {key}"));
            }
        }
        if let Some(calls) = desktop {
            let want = notify_calls();
            if *calls != want {
                whys.push(format!(
                    "notification desktop: want {want:?}, got {calls:?}"
                ));
            }
            for key in NOTIFY_COUNTS {
                if !served
                    .iter()
                    .any(|c| c.outcome == "answered" && c.api == key && c.count > 0)
                {
                    whys.push(format!("notification: the services did not count {key}"));
                }
            }
            let atld_log = read(&run.atld_log());
            for line in NOTIFY_ATLD_LINES {
                if !atld_log.contains(line) {
                    whys.push(format!("notification: atld logged no {line:?}"));
                }
            }
        }
    }
    let signatures = app.lines().find_map(|l| {
        l.split_once(SIGNATURES)
            .map(|(_, r)| r.trim_end().to_owned())
    });
    let want = signatures_want(cert);
    if passed && signatures.as_deref() != Some(want.as_str()) {
        whys.push(format!("signatures: want {want:?}, got {signatures:?}"));
    }
    let (prefix, want) = PENDING_INTENT;
    let pending = app
        .lines()
        .find_map(|l| l.split_once(prefix).map(|(_, r)| r.trim_end().to_owned()));
    if passed && pending.as_deref() != Some(want) {
        whys.push(format!("pendingintent: want {want:?}, got {pending:?}"));
    }
    let processes = app.lines().find_map(|l| {
        l.split_once(PROCESSES)
            .map(|(_, r)| r.trim_end().to_owned())
    });
    if passed && processes.as_deref() != Some(PROCESSES_WANT) {
        whys.push(format!(
            "processes: want {PROCESSES_WANT:?}, got {processes:?}"
        ));
    }
    let sysanswers = app.lines().find_map(|l| {
        l.split_once(SYSANSWERS)
            .map(|(_, r)| r.trim_end().to_owned())
    });
    if passed && sysanswers.as_deref() != Some(SYSANSWERS_WANT) {
        whys.push(format!(
            "sysanswers: want {SYSANSWERS_WANT:?}, got {sysanswers:?}"
        ));
    }
    let after = |prefix: &str| {
        app.lines()
            .find_map(|l| l.split_once(prefix).map(|(_, r)| r.trim_end().to_owned()))
    };
    if passed && let Some(why) = settings_why(after(SETTINGS).as_deref()) {
        whys.push(why);
    }
    let (webview_ua, engine_ua) = (after(WEBVIEW_UA), after(WEBVIEW_ENGINE_UA));
    if passed && let Some(why) = webview_ua_why(webview_ua.as_deref(), engine_ua.as_deref()) {
        whys.push(why);
    }
    // every LOG_LINES and FLAG_LINES row, Async ones included: the watch above waited for them, so a missing line here is a real miss
    for line in judged_lines() {
        let got = after(line.prefix);
        let why = match line.want {
            Want::Exact(want) => {
                (got.as_deref() != Some(want)).then(|| format!("want {want:?}, got {got:?}"))
            }
            Want::Prefix(want) => (!got.as_deref().is_some_and(|g| g.starts_with(want)))
                .then(|| format!("want a value starting {want:?}, got {got:?}")),
            Want::Host => match facts
                .as_ref()
                .map_err(Clone::clone)
                .and_then(HostFacts::zone_want)
            {
                Err(e) => Some(format!("the run host's facts: {e}")),
                Ok(want) => (got.as_deref() != Some(want.as_str()))
                    .then(|| format!("want {want:?}, got {got:?}")),
            },
        };
        if let Some(why) = why.filter(|_| passed) {
            whys.push(format!("{}: {why}", line.row));
        }
    }
    let features = app
        .lines()
        .find_map(|l| l.split_once(FEATURES).map(|(_, r)| r.trim_end().to_owned()));
    if passed && let Some(why) = features_why(features.as_deref()) {
        whys.push(format!("features: {why} (line {features:?})"));
    }
    let priority = app
        .lines()
        .find_map(|l| l.split_once(PRIORITY).map(|(_, r)| r.trim_end().to_owned()));
    let tiers = atl_broker::NiceTiers::for_limit(atl_broker::granted_nice());
    if passed
        && !remote
        && let Some(why) = priority_why(priority.as_deref(), tiers.floor)
    {
        whys.push(format!("priority ({}): {why}", tiers.describe()));
    }
    // the row's release: releasePlayer is oneway, so the app sees no error; the audio service's own count shows that the track and the release reached it and were served
    if passed {
        let served = counters(&services_log);
        for method in SOUNDPOOL_CALLS {
            let want = format!("IAudioService.{method}");
            if !served
                .iter()
                .any(|c| c.outcome == "answered" && c.api == want && c.count > 0)
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
    // a startActivity whose resultTo is not the calling package's (Main.java resultToSecurity): the app sees SecurityException, the reply hook counts it
    if passed {
        whys.extend(result_to_security_failures(&app, &services_log));
    }
    if passed {
        whys.extend(dex_load_failures(&services_log));
    }
    // the application context's software-layer limit (Main.drawingCache): 0 hides every software-layer view
    if passed {
        whys.extend(drawing_cache_failures(&app));
    }
    // startActivityForResult round trip (Main.java, ResultChild.java): the result, its order against Main's resume, and the singleTop self-start's cancel
    if passed {
        whys.extend(result_failures(&app));
        whys.extend(identity_failures(&app, isa, pointer_bits, primary_abi));
    }
    if passed && !bridge1_ok {
        whys.push(format!(
            "bridge1: want {BRIDGE1:?}, got {:?}",
            bridge1.or_else(|| app.lines().find(|l| l.contains("AtlTest: bridge1")))
        ));
    }
    let mut es1_compared = None;
    if passed && bridge1_ok {
        whys.extend(bridge1_call_failures(&app, deferred, foreign, &facts));
        // sigsegvRecover's fault reaches the fixture's own handler through the bridge, which reports it first: the report's only gate in a real app process
        let report = guest_fault_report(foreign);
        if !deferred.contains(&"sigsegvRecover") && !app.lines().any(|l| l.contains(report)) {
            whys.push(format!("bridge1: sigsegvRecover left no {report:?} line"));
        }
        if !deferred.contains(&"es1Pixels") {
            match es1_pixels(&app, scene) {
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
    let gate = if remote { "remote app" } else { "app" };
    if remote {
        for (row, why) in REMOTE_UI_SKIPPED {
            println!("app: SKIP {row}: {why}");
        }
    }
    if !whys.is_empty() {
        for why in &whys {
            println!("app: FAIL {why}");
        }
        println!("{}", crate::verdict::gate(gate, &whys));
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
        for (service, _, want) in SERVICE_LINES
            .iter()
            .filter(|(_, _, w)| !remote || w.is_some())
        {
            println!("app: pass {service} {}", service_want(service, *want));
        }
        println!("app: pass processes {PROCESSES_WANT}");
        println!("app: pass sysanswers {SYSANSWERS_WANT}");
        println!(
            "app: pass webview-ua {}",
            webview_ua.as_deref().unwrap_or_default()
        );
        for line in judged_lines() {
            println!(
                "app: pass {} {}",
                line.row,
                after(line.prefix).unwrap_or_default()
            );
        }
        if !remote {
            println!(
                "app: pass priority {} ({})",
                priority.as_deref().unwrap_or_default(),
                tiers.describe()
            );
        }
        println!(
            "app: pass features {}",
            features.as_deref().unwrap_or_default()
        );
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
        println!(
            "app: pass result {RESULT_CHILD}; {RESULT_CANCELED} before main new intent; permission-request {PERMISSION_DISMISSED}"
        );
        if let Some(n) = es1_compared {
            println!(
                "app: pass es1 {n} pixels match the scene model (libGLESv1_CM.so through the proxies)"
            );
        }
        println!("app: pass ({activity} Activity.onCreate logged after {elapsed:.1?})");
        println!("app: gralloc registered: {}", gralloc_log.trim());
        // the local gate's ok line is the test runner's; a remote run's verdict line is its own, as the headless subset's
        if remote {
            println!("{}", crate::verdict::gate(gate, &[]));
        }
        return Ok(ExitCode::SUCCESS);
    }
    if passed {
        println!(
            "app: FAIL the allocator did not register:\n{}",
            gralloc_log.trim()
        );
        let why = format!("the allocator did not register: {}", gralloc_log.trim());
        println!("{}", crate::verdict::gate(gate, std::slice::from_ref(&why)));
        return Ok(ExitCode::FAILURE);
    }
    let why = no_marker.clone();
    println!("app: FAIL {why}");
    println!("---- atld log\n{}", tail(&read(&run.atld_log()), TAIL));
    println!("---- app log (last {TAIL} lines)\n{}", tail(&app, TAIL));
    println!(
        "---- services log (last {TAIL} lines)\n{}",
        tail(&services_log, TAIL)
    );
    println!("{}", crate::verdict::gate(gate, std::slice::from_ref(&why)));
    Ok(ExitCode::FAILURE)
}

/// A remote run's end: waits for the transfer `child` (atld's limit + the margin), checks it, reads the target's end record. (atld rc, atld ended by itself rather than at the limit, the wait's duration).
fn remote_end(
    rt: &Runtime,
    run: &Run,
    mut child: std::process::Child,
) -> Result<(i32, bool, Duration)> {
    let started = Instant::now();
    let bound = REMOTE_LIMIT + crate::run_apk::TRANSFER_MARGIN;
    let status = crate::run_apk::wait_transfer(&mut child, bound, || {})?.with_context(|| {
        format!(
            "the transfer did not end within {} s and was stopped",
            bound.as_secs()
        )
    })?;
    rt.check_end(status, run)?;
    let (rc, _) = crate::run_apk::end_record(&run.scratch).map_err(anyhow::Error::msg)?;
    let atld_exited = !crate::run_apk::stopped_at_limit(rc);
    println!(
        "app: remote run ended after {:.1?}: atld rc {rc} ({})",
        started.elapsed(),
        if atld_exited {
            "its own end"
        } else {
            "the target's limit"
        }
    );
    Ok((rc, atld_exited, started.elapsed()))
}

/// The headless subset's verdict ([`remote`]) once the transfer `child` ends: the target's end record, then the process start ([`MARKER`]), Main's resume ([`MAIN_RESUMED`]), no fault, the bridge1 rows as the local gate judges them, and the fixture's own ELF checks. The rest is printed as skipped.
#[allow(clippy::too_many_arguments)]
fn remote_verdict(
    rt: &Runtime,
    run: &Run,
    child: std::process::Child,
    app_name: &str,
    libs: &[PathBuf],
    deferred: &[&str],
    foreign: bool,
    scene: &Scene,
) -> Result<ExitCode> {
    let (rc, atld_exited, _) = remote_end(rt, run, child)?;
    let read = atl_diag::logfiles::read;
    let app = read(&run.log(app_name));
    let mut whys = jni_fixture::shape_failures(libs)?;
    let passed = app.lines().any(|l| l.contains(MARKER));
    if passed {
        if !app.contains(MAIN_RESUMED) {
            whys.push(format!("no {MAIN_RESUMED:?} line after onCreate"));
        }
        if let Some(f) = fault(&app, atld_exited) {
            whys.push(format!("the app faulted: {f}"));
        }
        match app.lines().find(|l| l.contains(BRIDGE1)) {
            None => whys.push(format!(
                "bridge1: want {BRIDGE1:?}, got {:?}",
                app.lines().find(|l| l.contains("AtlTest: bridge1"))
            )),
            Some(line) => {
                whys.extend(bridge1_call_failures(
                    &app,
                    deferred,
                    foreign,
                    &HostFacts::of_remote(&run.scratch),
                ));
                let report = guest_fault_report(foreign);
                if !deferred.contains(&"sigsegvRecover") && !app.lines().any(|l| l.contains(report))
                {
                    whys.push(format!("bridge1: sigsegvRecover left no {report:?} line"));
                }
                if !deferred.contains(&"es1Pixels")
                    && let Err(why) = es1_pixels(&app, scene)
                {
                    whys.push(format!("es1 pixels: {why}"));
                }
                println!("app: remote {}", line.trim());
            }
        }
    } else {
        whys.push(format!("no {MARKER:?} line (atld rc {rc})"));
        println!("---- atld log\n{}", tail(&read(&run.atld_log()), TAIL));
        println!(
            "---- services log (last {TAIL} lines)\n{}",
            tail(&read(&run.log("services")), TAIL)
        );
    }
    if !whys.is_empty() {
        println!("---- app log (last {TAIL} lines)\n{}", tail(&app, TAIL));
    }
    if !deferred.is_empty() {
        println!("app: under QEMU: not claimed (Q7): {}", deferred.join(", "));
    }
    for (row, why) in REMOTE_SKIPPED {
        println!("app: SKIP {row}: {why}");
    }
    for why in &whys {
        println!("app: FAIL {why}");
    }
    println!("{}", crate::verdict::gate("remote app", &whys));
    Ok(if whys.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

/// The app's first fault up to the watch's end (marker + WINDOW_MS), from the log's start: the app's other threads run before onCreate (hwui's RenderThread made its EGL context 50 ms before the marker when it aborted, round-4 run). None without a marker.
fn fault(app_log: &str, atld_exited: bool) -> Option<String> {
    let marker_ms = window::verdict(app_log, MARKER, atld_exited, WINDOW_MS).marker_ms?;
    window::fault_after(app_log, 0, marker_ms + WINDOW_MS, marker_ms, atld_exited)
}

/// The key whose exact exit total the fixture forces: `Main.channels()` (run once from onCreate) calls `getNotificationChannel` 3 times, each counted `answered` by the services (notification.rs).
/// 3 is not a power of two, so only the exit total reads 3: a SIGKILLed services process leaves its last power-of-two line, 2.
const FORCED_TOTAL: &str =
    "diag: total answered INotificationManager.getNotificationChannel 3 first=";
/// atld's summary lines (atld main.rs print_summary): `atld: summary <n> keys`, then one row per key.
const SUMMARY_PREFIX: &str = "atld: summary ";
/// atld's line for a system process that ended within its grace after SIGTERM (atld main.rs stop_system).
const SERVICES_STOPPED: &str = "atld: stopped services";
/// binderd's orderly-exit line (atl-binderd main.rs EXIT_LINE), written after its counters.
const BINDERD_EXIT: &str = "binderd: exit";
/// Summary rows the gate prints.
const SUMMARY_SHOWN: usize = 5;

/// atld's prop family block (atld main.rs print_summary, atl_diag::FAMILIES): `atld: prop-summary <n> keys`, then its rows.
const PROP_SUMMARY_PREFIX: &str = "atld: prop-summary ";
/// A property every app run reads unset: libEGL's layer loader reads it at EGL init (frameworks/native/opengl/libs/EGL/egl_layers.cpp `LayerLoader::InitLayers`: `GetProperty("debug.gles.layers", "")`),
/// and ATL's area does not set it (a debug knob, unset on devices too). Its count is a lower bound above the API keys' (x>=64 on the run that found the regression), the case the split exists for.
const PROP_FIXTURE: &str = "debug.gles.layers";

/// The rows of atld's summary block, without its `<n> keys` header.
fn summary_rows(atld_log: &str) -> Vec<&str> {
    block_rows(atld_log, SUMMARY_PREFIX)
}

/// The rows of the atld block whose lines start with `prefix`, without its `<n> keys` header.
fn block_rows<'a>(atld_log: &'a str, prefix: &str) -> Vec<&'a str> {
    atld_log
        .lines()
        .filter_map(|l| l.strip_prefix(prefix))
        .filter(|r| !r.ends_with(" keys"))
        .collect()
}

/// Plan diagnostics-misses S17: the prop block lists `prop:miss` [`PROP_FIXTURE`], and the API block holds no `prop:` row (a family's lower bounds never rank there).
fn prop_block_failures(atld: &str) -> Vec<String> {
    let mut whys = Vec::new();
    let want = format!("{} {PROP_FIXTURE} ", atl_diag::PROP_MISS);
    if !block_rows(atld, PROP_SUMMARY_PREFIX)
        .iter()
        .any(|r| r.starts_with(&want))
    {
        whys.push(format!(
            "`{PROP_SUMMARY_PREFIX}` block lists no `{want}x..` row"
        ));
    }
    let family = format!("{}:", atl_diag::PROP_FAMILY);
    if let Some(r) = summary_rows(atld).iter().find(|r| r.starts_with(&family)) {
        whys.push(format!("the API summary block holds a prop row: {r}"));
    }
    whys
}

/// Properties every device sets that every app process reads at start (plan diagnostics-misses S18, the A024 dump and an ATL Bad Piggies run): apexd, base_system.mk, ATMS retrieveSettings, the display density (atl-property-service).
const DEVICE_SET: [&str; 4] = [
    "apex.all.ready",
    "debug.atrace.tags.enableflags",
    "debug.force_rtl",
    "ro.sf.lcd_density",
];

/// S18: the app's own reads of [`DEVICE_SET`] found a value: its log (atl_diag `prop:miss` lines, raw stderr) names none of them.
fn device_set_failures(app_log: &str) -> Vec<String> {
    DEVICE_SET
        .iter()
        .filter(|name| {
            let key = format!("{} {name} ", atl_diag::PROP_MISS);
            app_log.lines().any(|l| l.contains(&key))
        })
        .map(|name| format!("the app read {name} unset (a device sets it)"))
        .collect()
}

/// A summary row's count (`... x<n> +...`) and whether it is exact (`x>=<n>`: a lower bound).
fn row_count(row: &str) -> Option<(u64, bool)> {
    let (_, rest) = row.rsplit_once(" x")?;
    let n = rest.split(' ').next()?;
    match n.strip_prefix(">=") {
        Some(lower) => Some((lower.parse().ok()?, false)),
        None => Some((n.parse().ok()?, true)),
    }
}

/// The end of the run (plan diagnostics-f4 D4): the services stopped by SIGTERM within their grace and wrote the fixture key's exact total (only checked once the app ran: `ran`); atld printed a
/// summary whose first row is exact and whose rows rank by count; binderd ended in order.
fn exit_failures(ran: bool, services: &str, atld: &str, binderd: &str) -> Vec<String> {
    let mut whys = Vec::new();
    if !atld.lines().any(|l| l == SERVICES_STOPPED) {
        whys.push(format!("no `{SERVICES_STOPPED}` line: the services did not end within their grace after SIGTERM"));
    }
    if ran && !services.lines().any(|l| l.starts_with(FORCED_TOTAL)) {
        whys.push(format!("no `{FORCED_TOTAL}...` line in the services log: no exact exit total for the fixture's key"));
    }
    let rows = summary_rows(atld);
    match rows.first() {
        None => whys.push("atld printed no summary row".to_owned()),
        Some(first) if !row_count(first).is_some_and(|(_, exact)| exact) => {
            whys.push(format!(
                "summary's first row is not an exact count: {first}"
            ));
        }
        Some(_) => {
            let counts: Vec<u64> = rows
                .iter()
                .filter_map(|r| row_count(r))
                .map(|c| c.0)
                .collect();
            if counts.windows(2).any(|w| w[0] < w[1]) {
                whys.push(format!("summary not ranked by count: {counts:?}"));
            }
        }
    }
    if !binderd.lines().any(|l| l.starts_with(BINDERD_EXIT)) {
        whys.push(format!("binderd.log holds no `{BINDERD_EXIT}` line"));
    }
    whys
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::window::stamp_text;

    /// XBD 8.3: the offset is added to local time to reach UTC, so east of Greenwich is negative; expected seconds = hh*3600 + mm*60 + ss with that sign.
    #[test]
    fn posix_tz_std_offset() {
        for (tz, want) in [
            ("CST-8", Some(-8 * 3600)),
            ("UTC0", Some(0)),
            ("EST5EDT,M3.2.0,M11.1.0", Some(5 * 3600)),
            ("<+0545>-5:45", Some(-(5 * 3600 + 45 * 60))),
            ("<-03>3", Some(3 * 3600)),
            ("XYZ+1:02:03", Some(3600 + 2 * 60 + 3)),
            ("UTC", None),
            ("", None),
            ("ABC1:2:3:4", None),
        ] {
            assert_eq!(posix_std_offset_s(tz), want, "{tz:?}");
        }
        let taipei = HostFacts {
            possible: 1,
            online: 1,
            affinity: 1,
            cpuinfo: String::new(),
            zone: "Asia/Taipei\nCST-8\n".to_owned(),
        };
        // UTC+8 = 8 * 3600 * 1000 ms, libcore's getRawOffset for Asia/Taipei
        assert_eq!(
            taipei.zone_want().as_deref(),
            Ok("id=Asia/Taipei raw=28800000")
        );
    }

    /// glibc reads an empty TZ as UTC without a zone file; the probe says so on any host.
    #[test]
    fn zone_probe_empty_tz_is_utc() {
        let out = std::process::Command::new("sh")
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("TZ", "")
            .args(["-c", ZONE_SH])
            .output()
            .expect("sh runs");
        assert_eq!(String::from_utf8_lossy(&out.stdout), "UTC\nUTC0\n");
    }

    /// The webview-ua judge: the CDD string from WebKit 2.54's measured default passes; a CTS mismatch, the engine's raw 3-part tokens, a missing engine record and a thrown line fail.
    #[test]
    fn webview_ua_judge() {
        let engine = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/60.5 Safari/605.1.15";
        let good = "Mozilla/5.0 (Linux; Android 16; ATL Build/BP2A.1; wv) AppleWebKit/605.1 (KHTML, like Gecko) Version/4.0 Chrome/300.0.0.0 Safari/605.1";
        let line =
            |cts: &str, ua: &str| format!("cts={cts} release=16|model=ATL|id=BP2A.1 ua={ua}");
        assert_eq!(
            webview_ua_why(Some(&line("true", good)), Some(engine)),
            None
        );
        assert!(webview_ua_why(Some(&line("false", good)), Some(engine)).is_some());
        let raw = good.replace("605.1 ", "605.1.15 ");
        assert!(webview_ua_why(Some(&line("true", &raw)), Some(engine)).is_some());
        assert!(webview_ua_why(Some(&line("true", good)), None).is_some());
        assert!(
            webview_ua_why(
                Some("threw=java.lang.UnsupportedOperationException at=x.y"),
                Some(engine)
            )
            .is_some()
        );
        assert!(webview_ua_why(None, Some(engine)).is_some());
    }

    /// The features line: AEP is the host's, so it must equal the app's own GL list (`gl-aep`). The break run (no static files), a GL ES mismatch and a probe that disagrees with the app's GL fail.
    #[test]
    fn features_line_judges_presence_probe_and_gles() {
        // the host GL lists AEP and the device advertises it
        let ok = "landscape=true touchscreen=false aep=true gl-aep=true listed=true gles=0x30002 config=0x30002";
        assert_eq!(features_why(Some(ok)), None);
        let no_aep = "landscape=true touchscreen=false aep=false gl-aep=false listed=true gles=0x30001 config=0x30001";
        assert_eq!(features_why(Some(no_aep)), None);
        for bad in [
            // --features-break: no static permission files
            "landscape=false touchscreen=false aep=false gl-aep=false listed=false gles=0x30002 config=0x30002",
            "landscape=true touchscreen=true aep=false gl-aep=false listed=true gles=0x30002 config=0x30002",
            "landscape=true touchscreen=false aep=false gl-aep=false listed=true gles=0x0 config=0x0",
            "landscape=true touchscreen=false aep=false gl-aep=false listed=true gles=0x30002 config=0x30001",
            // the host GL lists AEP, the device does not advertise it
            "landscape=true touchscreen=false aep=false gl-aep=true listed=true gles=0x30002 config=0x30002",
            // advertised, the app's GL does not list it
            "landscape=true touchscreen=false aep=true gl-aep=false listed=true gles=0x30002 config=0x30002",
        ] {
            assert!(features_why(Some(bad)).is_some(), "{bad}");
        }
        assert!(features_why(None).is_some());
    }

    /// A good end passes; the break (services SIGKILLed: no stop line, only the power-of-two line 2, lower bounds in the summary, no binderd exit line) fails on each count.
    #[test]
    fn exit_failures_need_totals_summary_and_binderd_exit() {
        let services = "diag: count answered INotificationManager.getNotificationChannel 2 at=1.000\n\
                        diag: total answered INotificationManager.getNotificationChannel 3 first=1.000\n";
        let atld = "atld: stopped services\n\
                    atld: summary 2 keys\n\
                    atld: summary unimplemented I.a x5 +0.100s\n\
                    atld: summary refused-parcel P x>=1 +0.400s\n";
        let binderd = "binderd: exit (0 nonzero counters)\n";
        assert!(exit_failures(true, services, atld, binderd).is_empty());
        let killed_services =
            "diag: count answered INotificationManager.getNotificationChannel 2 at=1.000\n";
        let killed_atld = "atld: services: no exit 2s after SIGTERM: killed\n\
                           atld: summary 1 keys\n\
                           atld: summary unimplemented I.a x>=4 +0.100s\n";
        assert_eq!(
            exit_failures(true, killed_services, killed_atld, "").len(),
            4
        );
        // not ranked
        let unranked =
            "atld: stopped services\natld: summary I.a x1 +0.1s\natld: summary I.b x2 +0.2s\n";
        assert_eq!(exit_failures(true, services, unranked, binderd).len(), 1);
    }

    /// S18: a miss line of a device-set name fails once; another name's miss does not.
    #[test]
    fn device_set_names_read_found() {
        let log = "diag: first prop:miss debug.gles.layers 1 at=1\n";
        assert!(device_set_failures(log).is_empty());
        let log = format!("{log}diag: first prop:miss apex.all.ready 1 at=2\n");
        assert_eq!(device_set_failures(&log).len(), 1);
    }

    /// S17: the fixture's row in the prop block passes; a missing prop block row and a `prop:` row in the API block (the regression's shape) each fail once.
    #[test]
    fn prop_block_holds_the_fixture_and_the_api_block_none() {
        let good = "atld: summary 1 keys\n\
                    atld: summary unimplemented I.a x5 +0.100s\n\
                    atld: prop-summary 1 keys\n\
                    atld: prop-summary prop:miss debug.gles.layers x>=64 +0.200s\n";
        assert!(prop_block_failures(good).is_empty());
        let mixed = "atld: summary 2 keys\n\
                     atld: summary prop:miss debug.gles.layers x>=64 +0.200s\n\
                     atld: summary unimplemented I.a x5 +0.100s\n";
        assert_eq!(prop_block_failures(mixed).len(), 2);
    }

    #[test]
    fn cpu_lists_count_as_the_kernel_documents() {
        // cputopology.rst's forms: one range, a single cpu, ranges and singles mixed (0-3 = 4, 8 = 1, 10-11 = 2)
        assert_eq!(cpu_list_count("0-15\n"), Some(16));
        assert_eq!(cpu_list_count("0"), Some(1));
        assert_eq!(cpu_list_count("0-3,8,10-11"), Some(7));
        assert_eq!(cpu_list_count("3-1"), None);
        assert_eq!(cpu_list_count(""), None);
    }

    #[test]
    fn cpuinfo_features_names_the_isa_line() {
        assert_eq!(
            cpuinfo_features("processor\t: 0\nFeatures\t: fp asimd evtstrm\n"),
            "asimd"
        );
        assert_eq!(
            cpuinfo_features("processor\t: 0\nflags\t\t: fpu sse sse2 avx\n"),
            "sse2"
        );
        assert_eq!(
            cpuinfo_features("processor\t: 0\nflags\t\t: fpu sse\n"),
            "none"
        );
    }

    /// cpuinfoView: one text for Java and native, of the native code's ISA; a difference (the bind missing) or the wrong ISA's word fails.
    #[test]
    fn cpuinfo_view_wants_one_text_of_the_guest_isa() {
        assert_eq!(
            cpuinfo_view_why(Some("same features asimd"), true, "sse2"),
            None
        );
        assert_eq!(
            cpuinfo_view_why(Some("same features sse2"), false, "sse2"),
            None
        );
        // the break: Java reads the host's file, the guest the engine's
        assert!(
            cpuinfo_view_why(
                Some("differs java sse2 guest asimd bytes 9000/800"),
                true,
                "sse2"
            )
            .is_some()
        );
        assert!(cpuinfo_view_why(Some("same features sse2"), true, "sse2").is_some());
        assert!(cpuinfo_view_why(Some("same features none"), true, "sse2").is_some());
        assert!(cpuinfo_view_why(None, true, "sse2").is_some());
    }

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

    /// Floors: 20 = WSL's limit 0 (every request ignored, nice stays 0), -10 = a GameMode host's 30 (background and restore applied, foreground -2 applied), and the break's SecurityException.
    #[test]
    fn priority_judged_against_the_floor() {
        let ignored = "start=0 background=ok/0 restore=ok/0 foreground=ok/0";
        assert_eq!(priority_why(Some(ignored), 20), None);
        let applied = "start=0 background=ok/10 restore=ok/0 foreground=ok/-2";
        assert_eq!(priority_why(Some(applied), -10), None);
        assert!(priority_why(Some(applied), 20).is_some());
        let broken =
            "start=0 background=ok/10 restore=SecurityException/10 foreground=SecurityException/10";
        assert!(priority_why(Some(broken), 20).is_some());
        assert!(priority_why(None, 20).is_some());
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
        "AtlTest: implicit-none threw ActivityNotFoundException\n",
        "AtlTest: main resumed\n",
        "AtlTest: calling activity=com.example.atltest.Main package=com.example.atltest\n",
        "AtlTest: result child finishing\n",
        "AtlTest: result request=7 code=-1 extra=atl-result-7\n",
        "AtlTest: main resumed\n",
        "AtlTest: result request=9 code=0 extra=none\n",
        "AtlTest: main new intent\n",
        "AtlTest: permission-request request=11 names=[android.permission.POST_NOTIFICATIONS] results=[-1] self=-1\n",
    );

    /// 1440 x 900 x 4 bytes = 5184000 (ARGB_8888); the pre-fix run logged `app=0` (empty maxBounds).
    #[test]
    fn drawing_cache_is_four_bytes_per_display_pixel() {
        assert!(
            drawing_cache_failures("AtlTest: drawing-cache app=5184000 real=1440x900\n").is_empty()
        );
        assert!(
            drawing_cache_failures("AtlTest: drawing-cache app=0 real=1440x900\n")[0]
                .contains("limit 0")
        );
        assert!(
            drawing_cache_failures("AtlTest: drawing-cache app=0 real=0x0\n")[0]
                .contains("limit 0")
        );
        assert!(drawing_cache_failures("AtlTest: onCreate reached\n")[0].contains("no "));
    }

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
        // a request answered as RESULT_CANCELED with no data (empty arrays) or granted is not AOSP's dismissal
        for got in [
            "names=[] results=[] self=-1",
            "names=[android.permission.POST_NOTIFICATIONS] results=[0] self=0",
        ] {
            let other = ROUND_TRIP.replace(
                "names=[android.permission.POST_NOTIFICATIONS] results=[-1] self=-1",
                got,
            );
            assert!(
                result_failures(&other)
                    .iter()
                    .any(|w| w.starts_with("permission request: want")),
                "{got}"
            );
        }
        // a null caller (getCallingActivity unanswered) and a start that did not throw ANFE are each named
        for (from, to, why) in [
            (
                "activity=com.example.atltest.Main package=com.example.atltest",
                "activity=null package=null",
                "AtlTest: calling: want",
            ),
            (
                "implicit-none threw ActivityNotFoundException",
                "implicit-none started",
                "AtlTest: implicit-none: want",
            ),
        ] {
            let other = ROUND_TRIP.replace(from, to);
            assert!(
                result_failures(&other).iter().any(|w| w.starts_with(why)),
                "{to}"
            );
        }
        // nothing logged after the marker: all six required lines are reported
        assert_eq!(result_failures("AtlTest: onCreate reached\n").len(), 6);
    }

    /// Lines in the shapes Main.java and atl-diag write; each half missing gives its own reason.
    #[test]
    fn result_to_security_needs_the_throw_and_the_count() {
        let app = "I AtlTest: result-to-security threw SecurityException\n";
        let services =
            "diag: first exception:SECURITY IActivityTaskManager.startActivity 1 at=1.000\n";
        assert!(result_to_security_failures(app, services).is_empty());
        assert_eq!(
            result_to_security_failures("I AtlTest: result-to-security returned 0\n", services)
                .len(),
            1
        );
        assert_eq!(
            result_to_security_failures(
                app,
                "diag: first exception:SECURITY IActivityTaskManager.finishActivity 1 at=1.000\n"
            )
            .len(),
            1
        );
    }

    /// The accepted report's count passes; the refusal the untagged-map bug produced, or no line, fails.
    #[test]
    fn dex_load_needs_the_answered_count() {
        let ok = "diag: first answered IPackageManager.notifyDexLoad (not recorded) 1 at=1.000\n";
        assert!(dex_load_failures(ok).is_empty());
        let refused = "diag: first refused-parcel java.util.Map value 1 at=1.000\n\
                       diag: first status:BAD_VALUE IPackageManager.notifyDexLoad 1 at=1.001\n";
        assert_eq!(dex_load_failures(refused).len(), 1);
        assert_eq!(dex_load_failures("").len(), 1);
    }

    /// SystemServiceRegistry's line in the app log's shape counts; another name's does not.
    #[test]
    fn missing_service_needs_the_key() {
        let line = |n: &str| {
            format!(
                "1700000000.100 5.000 2 2 W liblog/SystemServiceRegistry: No service published for: {n}\n"
            )
        };
        assert!(missing_service_failures(&line(MISSING_JAVA)).is_empty());
        assert_eq!(missing_service_failures(&line("netstats")).len(), 1);
        assert_eq!(missing_service_failures("").len(), 1);
    }

    /// The shim's failure line fails the row wherever stderr interleaving puts it on the line (seen mid-line in a run log); a log without one passes.
    #[test]
    fn web_spawn_failure_reads_the_shims_line() {
        let ok =
            "1700000000.100 5.000 2 2 I liblog/AtlTest: webview-page: events=started,finished,\n";
        assert_eq!(web_spawn_failure(ok), None);
        let bad = format!(
            "{ok}flatpak-spawn: failed: pidfd of sandbox 147: No such process (os error 3)\naudio-mix: requested=flatpak-spawn: failed: x\n"
        );
        assert_eq!(
            web_spawn_failure(&bad).as_deref(),
            Some(
                "web-process starts: 2 failed, first \"flatpak-spawn: failed: pidfd of sandbox 147: No such process (os error 3)\""
            )
        );
    }

    #[test]
    fn missing_service_lines_match_the_pinned_sources() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        missing_service_lines_exist_at_the_pin(&Layout::of(root).unwrap()).unwrap();
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

    /// The watch's stop rule ([`watch_on`]) from its definition: on within WINDOW_MS; after it, on only while a line is missing and the last progress is under RESULT_STALL.
    #[test]
    fn watch_is_bounded_by_progress() {
        let s = Duration::from_secs;
        let window = Duration::from_millis(WINDOW_MS);
        // a crash window is always watched, even with nothing missing
        assert!(watch_on(window / 2, window / 2, false, false));
        // after it: everything logged -> done
        assert!(!watch_on(window + s(1), s(0), false, false));
        // a missing line with recent progress -> on
        assert!(watch_on(window + s(1), RESULT_STALL / 2, true, false));
        // no progress for RESULT_STALL -> off
        assert!(!watch_on(window + s(1), RESULT_STALL, true, false));
        // the flags-* case: a chain past the old whole-chain budget (30 s since the watch) still advancing 8 s ago -> on
        assert!(watch_on(RESULT_STALL * 3, s(8), true, false));
        // a timer running (fgstimeout's 30 s start timeout) with no step for RESULT_STALL -> on; its end (not timed) -> off
        assert!(watch_on(RESULT_STALL * 2, RESULT_STALL, true, true));
        assert!(!watch_on(RESULT_STALL * 2, RESULT_STALL, true, false));
    }

    #[test]
    fn awaited_lines_are_the_round_trip_and_every_async_row() {
        let asyncs = judged_lines().filter(|l| l.timing == Timing::Async).count();
        // nothing logged -> none of the round trip's last line + the Async rows
        assert_eq!(awaited_logged(""), (0, 1 + asyncs));
        // a flags chain advancing: each logged row counts once
        let one = format!("{}x\n", FLAG_LINES[0].prefix);
        let two = format!("{one}{}y\n", FLAG_LINES[1].prefix);
        assert_eq!(awaited_logged(&one).0, 1);
        assert_eq!(awaited_logged(&two).0, 2);
        let all: String = std::iter::once(PERMISSION_REQUEST)
            .chain(judged_lines().map(|l| l.prefix))
            .map(|p| format!("{p}v\n"))
            .collect();
        assert_eq!(awaited_logged(&all), (1 + asyncs, 1 + asyncs));
    }
}
