// SPDX-License-Identifier: MIT
//! `xtask-harness`: the runtime gates (`test`, `run-apk`, `remote`) and the check steps that need product logic. `cargo xtask test|run-apk|remote ...` runs this binary (xtask harness.rs); run it through that alias, not directly.
//! Links the bootstrap library (xtask) for everything that is not a runtime gate: layout, slots, build records, native goals, verdict lines. Its modules reach that code as `crate::<module>` through the imports below.
#![forbid(unsafe_code)]
// Terminal output is this tool's product, not logging.
#![allow(clippy::print_stdout, clippy::print_stderr)]
#![expect(
    clippy::disallowed_types,
    reason = "a test harness: it runs compilers and test drivers on the host; the runtime processes of its gates start through atl_broker"
)]

mod app;
mod app_imports;
mod atld_run;
mod audio_app;
mod audio_pcm;
mod binder;
mod broadcast;
mod composer;
mod compositor;
mod crossing;
mod cts;
mod cts_report;
mod drive;
mod drive_gate;
mod egl_bridge;
mod egl_driver;
mod gdk_dmabuf;
mod gl_dispatch;
mod gl_probe;
mod gl_proxies;
mod gl_scene;
mod gles2_fixture;
mod gralloc;
mod installer;
mod installer_parse;
mod instrument;
mod jni_fixture;
mod manifest;
mod native_load;
mod nb_identity;
mod nb_isa;
mod nested_wayland;
mod observe;
mod parcel;
mod platform_ui;
mod provider;
mod qemu_abi;
mod qemu_gate;
mod qemu_gl;
mod qemu_jni;
mod qemu_linker;
mod qemu_spike;
mod qemu_thread;
mod remote;
mod run_apk;
mod sandbox;
mod screencopy;
mod sensors;
mod services;
mod signing;
mod sles_bridge;
mod system_scan;
mod test_fonts;
mod tree_gate;
mod ui_module;
mod vk_driver;
mod wayland_proxy;
mod window;

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::Result;
use clap::{Parser, Subcommand};

use xtask::{
    arches, build_state, cargo_command, cdylib, checkout_root, codec2_store, cts_table, deps,
    gate_goals, git, host_art, keys, licenses, native, ndk, ndk_surface, packages, provided_gate,
    qemu, runtimes, slot, sources, stage_env, test_apks, tree, triage, unit_use, verdict,
    ws_config,
};

/// Runtime gates of the ATL-next repo, run as `cargo xtask test|run-apk|remote ...`.
#[derive(Parser)]
#[command(name = "xtask", bin_name = "xtask")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Runs a test APK row (test-apks.toml), or an APK file given by path, under atld the way an app is: its launcher activity in a GTK window (UI mode), no probe, no injected input.
    ///
    /// A row's file comes from `<workspace>/cache/test-apks`, downloaded and verified on first use; the package file is the APK's own manifest, which must match the row. A path runs unpinned: no row checks its bytes, signer or identity. The APK is copied to `<out>/run-apk`. Pass: the app reports its resume (onResume), no fault in its log for the watch after it, and a presented frame.
    /// Without `--hold` the window maps in the gate's headless compositor (nested_wayland.rs); `--hold SECS` maps it on the desktop and keeps the app up that long after its resume, the window open for drags. Staging is `xtask test window`'s. Hardware acceleration is the APK's own setting: the pinned official builds run with it on.
    RunApk(run_apk::Args),
    /// Runs WSL-built outputs on a Linux desktop over ssh (the owner's laptop, plan L1): `push` bundles a gate's record and `lib/`, `test` runs the prerequisite check on the target.
    ///
    /// Read-only or dry-run by default; `--apply` is the only mode that copies or runs anything remotely. The host is an ssh alias from WSL `~/.ssh/config` or `local:<dir>`.
    Remote {
        #[command(subcommand)]
        remote: remote::Remote,
    },
    /// Runtime tests over build outputs (not `cargo test`: they need built AOSP artifacts and ART).
    Test {
        #[command(subcommand)]
        test: Test,
    },
    /// A `cargo xtask check` step that needs product logic, run by check as a process: prints `skip: <why>` when the slot lacks its inputs.
    #[command(hide = true)]
    CheckStep {
        step: CheckStep,
        #[arg(long)]
        out: PathBuf,
    },
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum CheckStep {
    GlProxies,
}

#[derive(Subcommand)]
enum Test {
    /// The converter-built libandroid_runtime on host ART: load (every symbol bound), JNI registration (startReg + gRegJNI count), native calls checked against Android semantics, one copy per library.
    ///
    /// Stages print pass / FAIL / measured / WARN with counts; the exit code fails on any FAIL.
    /// The gRegJNI count is measured, not judged, until framework.jar exists: the walk aborts at the first class the test fixture (xtask-harness/testdata/native-load) does not declare.
    /// Duplicate libraries (libbase, liblog, libnativehelper, ICU, libc++) warn; --strict-copies fails on them, for an ART built by the converter.
    /// The fixture (Java declarations + JNI shim in Rust) is built into <out>/native-load on every run with the Java pipeline's JDK and d8 and the repo's rustc.
    NativeLoad(native_load::Args),
    /// ART's `-Xnative-bridge-isa:<isa>` (patches/aosp/art 0004) under atl-app-launcher with the bridge loaded: `arm64` with no arm64 engine staged reaches the bridge's `initialize`, which refuses naming it; an unknown ISA is refused at parse. `--break delivery|parse` must each turn its check red. Logs: stdout.
    NbIsa(nb_isa::Args),
    /// The installer process (atl-installer, plan item 4b M1-M5): AOSP's ApkLiteParseUtils, ParsingPackageUtils and NativeLibraryHelper over a package dir on host ART, every run through the broker's installer profile (its own uid, read-only runtime and package, one output dir, no network, the measured seccomp filter); package, versionCode, splits, components and natives equal aapt2 and the zip. Prints the VM start, parse and natives times and the seccomp denials of the run. `--break` names each turn the gate red with the refusal named (installer.rs).
    Installer(installer::Args),
    /// AOSP servicemanager + `service list` / `check` through atl-binderd on libbinder's driver seam; binderd's counters must show the expected driver paths firing.
    Binder(binder::Args),
    /// The app sandbox (plan Q5): a probe as an app process (uid 10123, the broker's App profile) reaches neither the real $HOME nor anything outside its data dir, and writes its own. Its deliberate break is by hand (sandbox.rs).
    Sandbox(sandbox::Args),
    /// Parcel encodings vs ground truth: atl-aidl-probe through AOSP's C++ Parcel (its size check fires on a wrong size), then atl-parcelables' codecs vs Java's Parcelables on host ART, byte-equal both ways.
    Parcel(parcel::Args),
    /// atl-compositor's libgui wire codecs vs libgui's own C++ on host ART: Java-built Transactions marshalled by libgui decode and re-encode byte-equal in Rust, Rust's fully populated transaction survives libgui's reader and writer, Rust's callbacks run libgui's listener.
    Compositor(compositor::Args),
    /// AOSP libui's graphics buffers on our allocator service and mapper.atl.so: two processes allocate, lock, share and read a 64x64 RGBA_8888 buffer, the allocator on this host's DRM device (WSL: vgem).
    ///
    /// Needs a DRM device that allocates linear buffers (the allocator names what is missing); the probe's per-case lines are printed.
    Gralloc(gralloc::Args),
    /// The M2 gate: atld starts binderd, servicemanager, the M2 services and the test APK (xtask-harness/testdata/app) through the broker; pass = its Activity.onCreate logs.
    ///
    /// Failure prints the app log's last lines; the services' unimplemented-call counters are printed either way.
    /// NativeBridge breaks: `ATL_NB_BREAK=<name>[,<name>]` in this run's environment; the run stages the core with it (names: atl-native-bridge build.rs `BREAKS`).
    /// GL driver breaks: `ATL_EGL_BREAK=<name>[,<name>]` in this run's environment; the run stages libGLES_atl.so with it (names: atl-egl-driver build.rs `BREAKS`). `ATL_EGL_TIER=blit` the same way stages the baseline orientation tier (build.rs `TIERS`).
    App(app::Args),
    /// The A1a audio gate (xtask-harness/testdata/audio): AudioSystem's session ids, AudioTrack's minimum buffer and a track's player id, served by atl-audioflinger in the app process (audio_app.rs).
    AudioApp(audio_app::Args),
    /// Broadcast delivery P1: two fixture apps (xtask-harness/testdata/broadcast) under atld; a runtime receiver, a sticky returned and replayed, an explicit manifest receiver cold-started headless, a denied sticky send, an unexported receiver skipped.
    ///
    /// Each check prints pass / FAIL; the services' counters are printed either way, the logs' tails on failure.
    Broadcast(broadcast::Args),
    /// An app's ContentProviders (xtask-harness/testdata/provider) under atld: initOrder before Application.onCreate, disabled and other-process ones skipped, PackageManager lookups, a call to its own provider.
    Provider(provider::Args),
    /// A preinstalled package (xtask-harness/testdata/system-scan) placed in /product under atld: scanned at start, FLAG_SYSTEM and its /product sourceDir via PackageManager, readable from its sandbox, a write there EROFS.
    SystemScan(system_scan::Args),
    /// The CTS instrumentation path: AOSP's `am instrument` runs a JUnit3 test APK (xtask-harness/testdata/instrument) through the services; its INSTRUMENTATION_* text is parsed and checked.
    ///
    /// Run 1: a passing, a failing and an erroring test: status codes 1/0, 1/-2, 1/-1, the final code -1, and the instrumented process killed once the run finished.
    /// Run 2: a test that kills its own process: the run is reported cancelled (code 0, shortMsg "Process crashed.").
    Instrument(instrument::Args),
    /// The CTS progress metric: every cts.toml row's test APK is built, installed and run with `am instrument`; the number of passing tests per module is the result.
    ///
    /// Per module: state (RAN / INCOMPLETE / CRASH / START_FAIL / BUILD_FAIL) and pass / fail / error / ignored / total, then the unimplemented and refused counters summed over the run, most hit first; answered and sent counts as totals. `<out>/cts/report.json` holds every test's outcome.
    /// Exit status is nonzero only for harness errors and, with `--baseline <report.json>`, for modules or tests that got worse than that saved report; failing tests are the metric, not errors.
    Cts(cts::Args),
    /// The in-app SurfaceFlinger against AOSP's libgui in a real app process (M3 slice 4): atld starts the services and a client class (xtask-harness/testdata/composer) as an app process; its compositor is registered through the system process and found by libgui by name.
    ///
    /// Pass = layers created, 30 transactions applied, the transaction-committed callback delivered, vsync at the display's refresh interval on Choreographer, every compositor counter as the checks say (`--self-test` lists the defects they catch). Buffers come with gralloc.
    Composer(composer::Args),
    /// The window path, one stage at a time. `--stage resume` (M3 plan step 1, slice 1): a software window fixture (xtask-harness/testdata/window) is started through atld, as `xtask test app` starts its app; pass = its activity's onResume is logged.
    ///
    /// The app then calls into WindowManager, which no service provides yet: its log after the marker and the services' counters that rose after it are printed.
    Window(window::Args),
    /// `run-apk --drive` on fixtures, judged from its PNGs: a shot's pixels are the known quadrants (also after `resize`), and a tap visibly changes the frame. `--break-blank` and `--break-colour` must turn it red.
    Drive(drive_gate::Args),
    /// The sensor service: a fixture (xtask-harness/testdata/sensors) uses SensorManager under atld with a fake `/sys/bus` (private mount namespace). `--stage empty`: no sensor, no stall; `--stage iio`: a fake IIO accelerometer's events with hand-derived values; `--stage buffer`: the same on its kernel buffer (a FIFO at `/dev/iio:device0`). `--break` must turn it red.
    Sensors(sensors::Args),
    /// The GTK platform layer (M3 slice 6) on a real Wayland compositor: a window maps, frame-clock ticks present scenes, GSK readback equals the pattern, a blocked UI thread answers no `xdg_wm_base` ping.
    ///
    /// Runs crates/atl-platform-ui/examples/gate.rs in the gate's headless weston (`--hold-blocked`: on $WAYLAND_DISPLAY) through a Wayland proxy that injects the pings; the module doc of xtask-harness/src/platform_ui.rs lists the gates and the evidence tier of each.
    /// The compositor's own "not responding" reaction is not observable on WSLg: `--hold-blocked <s>` keeps the UI thread blocked for the Arch-laptop run to watch.
    PlatformUi(platform_ui::Args),
    /// The platform module loaded RTLD_LOCAL (launcher-ui-module plan M1): atl-platform-ui-module-fixture (the launcher's link shape) paints a window through it, then an app library's `libvulkan.so` import must bind the AOSP stand-in, with no host symbol in global scope; the module's log reaches the process subscriber.
    ///
    /// Controls run every time: the module loaded RTLD_GLOBAL must leak, no log sink must drop the records. Needs `xtask native`, labwc, pipewire and the NDK.
    UiModule(ui_module::Args),
    /// The PCM output of atl-platform-ui-linux (plan A0) on PipeWire: a 1 kHz tone played to a null sink the run makes the default, recorded from its monitor, checked for frequency, level and duration. `--break zeros` and `--break no-target` must turn it red.
    ///
    /// The client is crates/atl-platform-ui/examples/audio_pcm.rs; the module doc of xtask-harness/src/audio_pcm.rs lists the gates and the environment it changes (restored on every exit). Needs pipewire, pw-cli, pw-metadata and pw-record on PATH.
    AudioPcm(audio_pcm::Args),
    /// The bridge-1 JNI fixture (xtask-harness/testdata/jni-test), NDK-built for minSdk 21 (emulated TLS) and 29 (ELF TLS): DT_NEEDED libc.so + libc++_shared.so, verneed libc.so/LIBC, the variant's TLS shape, and host glibc's loader refusing it on `LIBC`.
    ///
    /// Controls run every time: the probe loads a glibc library cleanly, and each variant fails the other's TLS expectation. Needs the NDK only, no native build.
    JniFixture(jni_fixture::Args),
    /// The GLES2 test APK (xtask-harness/testdata/gles2), shaped like Cut the Rope's path: GLSurfaceView + an NDK `.so` drawing with GLES2 only and reading pixels back. Static: `.so` DT_NEEDED and `gl*` imports equal the listed sets, manifest glEsVersion 2.0 + hardwareAccelerated, the library in `lib/<abi>/`.
    ///
    /// Controls run every time: each check rejects a broken copy of the real input. Needs the NDK, the Java pipeline's aapt2/d8 and a build.prop; no runtime.
    Gles2Fixture(gles2_fixture::Args),
    /// The task-copy namespace (task copies bound over src/aosp): javac accepts a bind-mounted `--patch-module=java.base` dir, the overlay check refuses a wrong copy or no namespace; with `--plain`/`--task`, two native out dirs' build.ninja differ only on bound projects' lines.
    ///
    /// Controls run every time: the symlinked (link-farm) shape must make javac fail. Needs the prebuilt JDK and bwrap; no native build unless comparing.
    TaskBind(tree_gate::Args),
    /// Product APK signing (signing.rs): framework-res installed at system/framework carries the key set's platform certificate, the repo bind's fixture (src/aosp/vendor/atl) at system/app its testkey (default role), per AOSP's apksigner against openssl's digest of each role certificate.
    ///
    /// Control every run: the two roles' certificates differ. `--break swap-role` expects the swapped roles and must fail. Needs `native --for signing`.
    Signing(signing::Args),
    /// AOSP's GL fronts (USE_SLOW_BINDING) dispatch each thread's GL calls through its own hooks table, libEGL -> libGLESv2, via atl-bionic's `__get_tls()`; prints the per-call cost.
    ///
    /// Control every run: a libGLESv2 with its own atl-bionic copy (dlmopen) must fail. Needs libEGL + libGLESv2 in --out; no driver.
    GlDispatch(gl_dispatch::Args),
    /// The GLES driver over the host's system EGL (libGLES_atl.so) rendering into an ANativeWindow of gralloc buffers, loaded as AOSP's loader loads it; every pixel of every queued buffer checked with its transform applied.
    ///
    /// Controls every run: a window dropping the buffer transform and framebuffer entries taken from the host must fail. Needs no native build.
    EglDriver(egl_driver::Args),
    /// The Vulkan HAL module (vulkan.atl.so) loaded as AOSP's libvulkan loads it, driving this host's ICD: instance, physical devices, device, queue; every dispatch slot judged as libvulkan judges it.
    ///
    /// Controls every run: a judge expecting another slot value, and an environment with no ICD manifest, must fail. Needs no native build.
    VkDriver(vk_driver::Args),
    /// GLES on the real driver in an app process: a pbuffer cleared and read back, and an extension outside entries.in through eglGetProcAddress (patch 0010); then a deliberate break that both checks must catch.
    ///
    /// Needs the fronts, gralloc and the services in --out (`xtask native`).
    Gl(gl_probe::Args),
    /// QEMU's `qemu-<target>` against ground truth: a bionic hello built by the NDK clang, and QEMU's own tests/tcg smoke programs.
    ///
    /// Builds first (as `xtask qemu --execute`). Pass = every program's output and exit status as the checks say; the logs are in `<out>/qemu/<target>-linux-user/gate/`.
    Qemu(qemu_gate::Args),
    /// The QEMU engine spike (atl-qemu-engine, QEMU bridge plan Q2): QEMU's PIC objects linked `-shared` + P1, driven from a host harness; every gate in `qemu_spike::GATES`, then each break must turn its gate red.
    ///
    /// Builds first (as `xtask qemu --pic --execute`). Logs in `<out>/qemu/<target>-linux-user-pic/spike/`.
    QemuSpike(qemu_spike::Args),
    /// The QEMU crossing on the real arm64 NB images (QEMU bridge plan Q4-3..Q4-5): the six entries placed in place, the guest linker's init, a guest dlopen with DT_NEEDED; then each break must turn its run red.
    ///
    /// Builds first (as `xtask qemu --pic --execute`); needs the arm64 NB build (`xtask native --host x86_64 --native-bridge arm64 nb:libc nb:native_bridge_guest_linker nb:libnative_bridge_vdso nb:libnative_bridge_guest_libc nb:libdl nb:libm nb:ld-android`) and `--nb` pointing at its `<out>/nb-graph-arm64/nb/arm64`. Logs in `<out>/qemu/<target>-linux-user-pic/qemu-linker/`.
    QemuLinker(qemu_linker::Args),
    /// The QEMU crossing's guest threads on the real arm64 NB images (QEMU bridge plan Q5-6): a guest `pthread_create` attaches its own engine CPU, the provider's foreach reaches a guest callback, a key destructor runs at thread exit; then each break must turn its run red.
    ///
    /// Builds first (as `xtask qemu --pic --execute`); needs the arm64 NB build (as `test qemu-linker`) and `--nb` pointing at its `<out>/nb-graph-arm64/nb/arm64`. Logs in `<out>/qemu/<target>-linux-user-pic/qemu-thread/`.
    QemuThread(qemu_thread::Args),
    /// Guest JNI calls under the QEMU crossing, integer lanes (QEMU bridge plan Q6-2): a fixture's JNI calls reach a fake host JNIEnv, GetJavaVM routes to its own thunk; the `jni_guest_env` and `jni_swap` breaks must turn it red. Builds first (as `xtask qemu --pic --execute`); needs the arm64 NB build (as `test qemu-linker`) and `--nb` pointing at its `<out>/nb-graph-arm64/nb/arm64`. Logs in `<out>/qemu/<target>-linux-user-pic/qemu-jni/`.
    QemuJni(qemu_jni::Args),
    /// The engine ABI (QEMU bridge plan Q6-1), both directions: a guest host call's integer lanes, FP lanes and stack words reach the host, and the host's FP answer returns in the guest's v0; `call_ex` puts the host's lanes in the guest's registers and stack, and returns the callee's FP result. The `qa_stack_off`, `qa_v_lane`, `call-stack` and `call-v` breaks must each turn it red. Builds first (as `xtask qemu --pic --execute`); needs no NB images. Logs in `<out>/qemu/<target>-linux-user-pic/qemu-abi/`.
    QemuAbi(qemu_abi::Args),
    /// GLES2 proxy calls under QEMU (QEMU bridge plan Q7-4): fp arguments, 9-argument calls with the ninth on the guest stack and the fp result reach a host recorder bit for bit, each bound by its QEMU binding-table row. eglGetProcAddress (GS2.2 G1) answers a row's guest address for its names and NULL for an unknown one. The `v_drop`, `drop_stack`, `ret_int` and `egl_raw` breaks must each turn it red. Builds first (as `xtask qemu --pic --execute`); needs no NB images. Logs in `<out>/qemu/<target>-linux-user-pic/qemu-gl/`.
    QemuGl(qemu_gl::Args),
    /// Static audit of an APK's arm64 imports against the QEMU binding table (QEMU Q7 plan Q7-5): every dynamic undefined symbol of each `lib/arm64-v8a/*.so` is bound (same APK, a Generic/Shim row, guest bionic), trapped (with its reason) or unknown; red when a gated APK (default Cut the Rope 3.63.0) has a trapped or strong unknown import, each printed with its owning slice. `--info` APKs (default the Battle Cats TW arm64 split) are reported only. Executes nothing.
    AppImports(app_imports::Args),
    /// The crossing counter of both engines (QEMU bridge plan Q5-8, feature `crossing-count`): identity host-thunk and JNI shells, then QEMU; the fixture's N calls per binding count N, sizes fall in their literal bins, 2^16 crossings dump once; the `id_count` and `qe_id` breaks must turn it red; the library without the feature has no counter symbol. Builds first (as `xtask qemu --pic --execute`); needs the arm64 NB build (as `test qemu-thread`) and `--nb` pointing at its `<out>/nb-graph-arm64/nb/arm64`. Logs in `<out>/qemu/<target>-linux-user-pic/crossing/`.
    Crossing(crossing::Args),
    /// The identity native-bridge gates g1-g5 (x86_64 engine, no QEMU) with their `ATL_NB*` env taken from this checkout's out dir (`nb/x86_64`, the staged `app/`), not set by hand.
    ///
    /// Needs the x86_64 NB build and `xtask test app` (it stages `app/root` and `app/jni-fixture`). Logs in `<out>/nb-identity/`.
    NbIdentity(nb_identity::Args),
    /// OpenSL ES through the identity engine (opensl-es plan O1): an NDK fixture against a host recorder `libOpenSLES.so` staged at a bwrap `/system/lib64`; checks a-g, then each break must turn its named check red.
    ///
    /// Needs what `nb-identity` needs (the x86_64 NB build incl. the guest `libOpenSLES` proxy, `xtask test app`). Logs in `<out>/sles-bridge/`.
    SlesBridge(sles_bridge::Args),
    /// eglGetProcAddress through the identity engine (GS2.2 G1): an imported GL name equals the answer and the answer's call reaches a host recorder; a table-only name is callable; an unknown name is NULL; two answers for a described name are one address; a name no prototype describes traps by name (its own run). Needs what `nb-identity` needs (the x86_64 NB build incl. the guest `libEGL` and `libGLESv2` proxies, `xtask test app`). Logs in `<out>/egl-bridge/`.
    EglBridge(egl_bridge::Args),
}

fn main() -> Result<ExitCode> {
    let cli = Cli::parse();
    let root = checkout_root()?;
    let name = verdict_name(&cli.command);
    let result = match cli.command {
        Command::RunApk(args) => run_apk::run(&root, &args),
        Command::Remote { remote } => remote::run(&root, &remote),
        Command::Test {
            test: Test::NativeLoad(args),
        } => native_load::run(&root, &args),
        Command::Test {
            test: Test::NbIsa(args),
        } => nb_isa::run(&root, &args),
        Command::Test {
            test: Test::Installer(args),
        } => installer::run(&root, &args),
        Command::Test {
            test: Test::Signing(args),
        } => signing::run(&root, &args),
        Command::Test {
            test: Test::Parcel(args),
        } => parcel::run(&root, &args),
        Command::Test {
            test: Test::Compositor(args),
        } => compositor::run(&root, &args),
        Command::Test {
            test: Test::Gralloc(args),
        } => gralloc::run(&root, &args),
        Command::Test {
            test: Test::Binder(args),
        } => binder::run(&root, &args),
        Command::Test {
            test: Test::Sandbox(args),
        } => sandbox::run(&root, &args),
        Command::Test {
            test: Test::App(args),
        } => app::run(&root, &args),
        Command::Test {
            test: Test::AudioApp(args),
        } => audio_app::run(&root, &args),
        Command::Test {
            test: Test::Broadcast(args),
        } => broadcast::run(&root, &args),
        Command::Test {
            test: Test::Provider(args),
        } => provider::run(&root, &args),
        Command::Test {
            test: Test::SystemScan(args),
        } => system_scan::run(&root, &args),
        Command::Test {
            test: Test::Instrument(args),
        } => instrument::run(&root, &args),
        Command::Test {
            test: Test::Cts(args),
        } => cts::run(&root, &args),
        Command::Test {
            test: Test::Composer(args),
        } => composer::run(&root, &args),
        Command::Test {
            test: Test::Window(args),
        } => window::run(&root, &args),
        Command::Test {
            test: Test::Drive(args),
        } => drive_gate::run(&root, &args),
        Command::Test {
            test: Test::Sensors(args),
        } => sensors::run(&root, &args),
        Command::Test {
            test: Test::PlatformUi(args),
        } => platform_ui::run(&root, &args),
        Command::Test {
            test: Test::UiModule(args),
        } => ui_module::run(&root, &args),
        Command::Test {
            test: Test::AudioPcm(args),
        } => audio_pcm::run(&root, &args),
        Command::Test {
            test: Test::JniFixture(args),
        } => jni_fixture::run(&root, &args),
        Command::Test {
            test: Test::Gles2Fixture(args),
        } => gles2_fixture::run(&root, &args),
        Command::Test {
            test: Test::TaskBind(args),
        } => tree_gate::run(&root, &args),
        Command::Test {
            test: Test::GlDispatch(args),
        } => gl_dispatch::run(&root, &args),
        Command::Test {
            test: Test::EglDriver(args),
        } => egl_driver::run(&root, &args),
        Command::Test {
            test: Test::VkDriver(args),
        } => vk_driver::run(&root, &args),
        Command::Test {
            test: Test::Gl(args),
        } => gl_probe::run(&root, &args),
        Command::Test {
            test: Test::Qemu(args),
        } => qemu_gate::run(&root, &args),
        Command::Test {
            test: Test::QemuSpike(args),
        } => qemu_spike::run(&root, &args),
        Command::Test {
            test: Test::QemuLinker(args),
        } => qemu_linker::run(&root, &args),
        Command::Test {
            test: Test::QemuThread(args),
        } => qemu_thread::run(&root, &args),
        Command::Test {
            test: Test::QemuJni(args),
        } => qemu_jni::run(&root, &args),
        Command::Test {
            test: Test::QemuAbi(args),
        } => qemu_abi::run(&root, &args),
        Command::Test {
            test: Test::QemuGl(args),
        } => qemu_gl::run(&root, &args),
        Command::Test {
            test: Test::AppImports(args),
        } => app_imports::run(&root, &args),
        Command::Test {
            test: Test::Crossing(args),
        } => crossing::run(&root, &args),
        Command::Test {
            test: Test::NbIdentity(args),
        } => nb_identity::run(&root, &args),
        Command::Test {
            test: Test::SlesBridge(args),
        } => sles_bridge::run(&root, &args),
        Command::Test {
            test: Test::EglBridge(args),
        } => egl_bridge::run(&root, &args),
        Command::CheckStep { step, out } => check_step(&root, step, &out),
    };
    // a run that ends in an error (`?`, bail!) still ends with its verdict line; main prints the error after it
    if let (Err(e), Some(name)) = (&result, name) {
        println!("{}", verdict::error(name, e));
    }
    result
}

/// Runs check step `step` on slot `out`; a skip is printed for check (xtask harness.rs `check_step`) and exits 0.
fn check_step(root: &std::path::Path, step: CheckStep, out: &std::path::Path) -> Result<ExitCode> {
    let skipped = match step {
        CheckStep::GlProxies => {
            let arches = arches::load(root)?;
            let (arch, _) = arches::build_machine(root, &arches)?;
            gl_proxies::check(root, out, arch)?
        }
    };
    if let Some(why) = skipped {
        println!("{}{why}", xtask::harness::SKIP_PREFIX);
    }
    Ok(ExitCode::SUCCESS)
}

/// The verdict-line name of a command whose failures end in a verdict line.
fn verdict_name(command: &Command) -> Option<&'static str> {
    Some(match command {
        Command::RunApk(_) => "run-apk",
        Command::Remote { .. } => "remote",
        Command::Test { test } => match test {
            Test::NativeLoad(_) => "native-load",
            Test::NbIsa(_) => "nb-isa",
            Test::Installer(_) => "installer",
            Test::Signing(_) => "signing",
            Test::Parcel(_) => "parcel",
            Test::Provider(_) => "provider",
            Test::SystemScan(_) => "system-scan",
            Test::Compositor(_) => "compositor",
            Test::Gralloc(_) => "gralloc",
            Test::Binder(_) => "binder",
            Test::Sandbox(_) => "sandbox",
            Test::App(_) => "app",
            Test::AudioApp(_) => "audio-app",
            Test::Broadcast(_) => "broadcast",
            Test::Instrument(_) => "instrument",
            Test::Cts(_) => "cts",
            Test::Composer(_) => "composer",
            Test::Window(_) => "window",
            Test::Drive(_) => "drive",
            Test::Sensors(_) => "sensors",
            Test::PlatformUi(_) => "platform-ui",
            Test::UiModule(_) => "ui-module",
            Test::AudioPcm(_) => "audio-pcm",
            Test::JniFixture(_) => "jni-fixture",
            Test::Gles2Fixture(_) => "gles2-fixture",
            Test::TaskBind(_) => "task-bind",
            Test::GlDispatch(_) => "gl-dispatch",
            Test::EglDriver(_) => "egl-driver",
            Test::VkDriver(_) => "vk-driver",
            Test::Gl(_) => "gl",
            Test::Qemu(_) => "qemu-gate",
            Test::QemuSpike(_) => "qemu-spike",
            Test::QemuLinker(_) => "qemu-linker",
            Test::QemuThread(_) => "qemu-thread",
            Test::QemuJni(_) => "qemu-jni",
            Test::QemuAbi(_) => "qemu-abi",
            Test::QemuGl(_) => "qemu-gl",
            Test::AppImports(_) => "app-imports",
            Test::Crossing(_) => "crossing",
            Test::NbIdentity(_) => "nb-identity",
            Test::SlesBridge(_) => "sles-bridge",
            Test::EglBridge(_) => "egl-bridge",
        },
        Command::CheckStep { .. } => return None,
    })
}
