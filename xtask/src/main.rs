// SPDX-License-Identifier: MIT
//! `cargo xtask` -- the single build/check driver of this repo. Read-only or dry-run by default.
#![forbid(unsafe_code)]
// Terminal output is this tool's product, not logging.
#![allow(clippy::print_stdout, clippy::print_stderr)]
#![expect(
    clippy::disallowed_types,
    reason = "a build tool: it runs compilers, git and test drivers on the host; the runtime processes of its gates start through atl_broker"
)]

mod android_paths;
mod aosp;
mod apk_sig;
mod app;
mod arches;
mod atld_run;
mod audio_app;
mod audio_pcm;
mod binder;
mod broadcast;
mod build_pool;
mod build_state;
mod bwrap_flags;
mod cdylib;
mod check;
mod codec2_store;
mod composer;
mod compositor;
mod converter;
mod cts;
mod cts_report;
mod cts_table;
mod deps;
mod drive;
mod drive_gate;
mod fetch;
mod gate_goals;
mod gdk_dmabuf;
mod git;
mod git_dirs;
mod gl_dispatch;
mod gl_probe;
mod gl_proxies;
mod gl_scene;
mod gles2_fixture;
mod gralloc;
mod host_art;
mod host_disk;
mod instrument;
mod jni_fixture;
mod land;
mod libcxx_gate;
mod licence_gate;
mod licenses;
mod llvm_host;
mod manifest;
mod mesa;
mod modules;
mod native;
mod native_load;
mod ndk_surface;
mod nested_wayland;
mod none_gate;
mod notice;
mod packages;
mod par;
mod parcel;
mod patch;
mod platform_ui;
mod provided_gate;
mod provider;
mod providers;
mod prune;
mod remote;
mod run_apk;
mod runpath_gate;
mod runtimes;
mod sandbox;
mod screencopy;
mod sensors;
mod service_mirrors;
mod services;
mod slot;
mod soong;
mod sources;
mod spdx;
mod system;
mod test_apks;
mod test_fonts;
mod tree;
mod tree_gate;
mod triage;
mod upstream;
mod verdict;
mod wayland_proxy;
mod window;
mod workspace;

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::{CommandFactory, Parser, Subcommand};

/// Single build/check driver of the ATL-next repo. Read-only or dry-run by default.
#[derive(Parser)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Definition of done: every step must pass before a change is handed back or merged.
    ///
    /// Runs every step in order and keeps going after a failure, so one run reports every problem.
    /// Read-only: nothing is reformatted or fixed (`cargo fmt` itself does that).
    Check(check::Args),
    /// Host dependencies from deps.toml.
    ///
    /// Distro rows: package installed (dpkg-query / pacman), `min` met (pkg-config for libraries,
    /// package version otherwise). Cargo rows: exact version in `cargo install --list`. Read-only.
    Deps(deps::Args),
    /// Upstream checkouts under <workspace>/src at the pins in sources.toml. `--test-apks` fetches the test APKs of test-apks.toml instead.
    ///
    /// Dry run by default (offline): shows what each checkout needs. The AOSP manifest (pinned in
    /// sources.toml) is fetched first: it maps tree paths to repositories and holds the tree's link/copy
    /// files; the other checkouts then run in parallel. Checkouts are shallow (the pinned tag only);
    /// `git fetch --unshallow` inside one before bisecting.
    /// An upstream with patches/<upstream>/ ends on branch atl-patches = pin + those patches; commits on atl-patches that patches/ lacks are never dropped (fetch refuses and names the command to run), nor is a checkout with uncommitted changes moved.
    /// src/ holds origin/main's pins and patches: a shared checkout is changed only where this repo checkout's pin and patches/<upstream>/ equal origin/main's; elsewhere fetch refuses, and a task's own go to its task copy (`xtask patch --edit`), which fetch settles instead.
    Fetch(fetch::Args),
    /// Which AOSP project defines a module, from an index of every manifest project's `Android.bp`.
    ///
    /// `--index` fetches only the `Android.bp` files of every project in the pinned manifest (shallow,
    /// blobless, sparse; under <workspace>/cache/aosp-bp/<tag>, kept between runs) and rebuilds the
    /// index; `--which` reads it. Names come from each module's literal `name:` only: a name built
    /// from variables is not indexed.
    Aosp(aosp::Args),
    /// AOSP native modules built for this Linux host by our Blueprint converter (tools/bp2ninja).
    ///
    /// Runtime libraries come from each module's android source set compiled for glibc, build tools
    /// (aidl) from their host source set; bionic itself is replaced by the system C library. Output in
    /// this checkout's build-output slot (`xtask slot`), else <workspace>/out/native/<host>; failures are summarised per module (`ninja -k 0`) against the goals: which failed modules each goal reaches, those with no failed dependency first (most modules waiting on them first), each with its cause (compiler `error:` lines, a link's undefined symbols).
    /// Gates over what was built, each failing the run: the provided gate (provided libraries win symbol lookup) and the licence gate (every source and header linked into a device artifact has a licence licenses.toml allows in the runtime).
    /// Preflight: every AOSP checkout and every upstream built for providers.toml must be its pin + patches/ (the drift test), since the build compiles checkouts as they are and never writes patches/.
    /// Task copies (`xtask patch --edit`) replace their src/ checkout. With AOSP ones, `native` and `check` re-execute inside a mount namespace (`bwrap --dev-bind / /`) where each is bind-mounted over src/aosp/<project>, so the tree path is always src/aosp; the slot's `built-from` record names each bound copy and its state.
    /// Risk: `ninja` run by hand outside the namespace reads the unpatched src/aosp; `native --shell <goal> --host <h>` opens $SHELL inside it.
    Native(native::Args),
    /// Mesa `platforms=android` build inputs from a converter out dir: pkg-config files for the AOSP libraries Mesa looks up, and a meson native file with the G1 options.
    ///
    /// Writes only under --dest; the out dir is read. Mesa's meson is then run as-is with `--native-file <dest>/native.ini`.
    MesaConfig(mesa::Args),
    /// Build-output slots: the out dir each checkout builds into by default (`show`, `list`, `assign`).
    ///
    /// The pool is `slot::SLOTS` dirs <workspace>/out/slot-<n>; a checkout holds at most one, recorded in its git dir. `native`, `check` and `test` build into it without `--out` / `--java-out`.
    Slot {
        #[command(subcommand)]
        slot: slot::Slot,
    },
    /// Our commits on upstream checkouts: patch branch -> patches/<upstream>/*.patch.
    ///
    /// Without flags: per upstream, whether checkout, branch and patches/ are in step (read-only; `xtask check` step `drift` and `xtask native` run the same test). Untracked files count, except those xtask puts there (manifest link/copy files, prune markers).
    /// `xtask fetch --execute` applies patches/. Develop on a task branch: `--edit <upstream>` makes the task copy src-wt/<task>/<upstream> on atl-patches-<task>; commit there, every commit with `Upstream-Status:` and `Patch-Reason:` trailers, then `--export`; `--done` removes the copy (`--task <branch>` from main).
    /// After a pin bump `xtask fetch --execute` re-applies patches/ on the new pin; where they no longer apply: `--rebase`, resolve, `--export`.
    Patch(patch::Args),
    /// Lands a task branch on origin/main, after the branch's own patches pass the full check.
    ///
    /// Dry run by default: prints the steps after a read-only preflight (the branch is a fast-forward of origin/main; the main checkout is on origin/main, clean and in step; the branch's worktree is clean).
    /// `--execute` runs them: task copies for the upstreams whose patches/ or pin differ from origin/main's, `cargo xtask check` in the branch's worktree, push, `fetch --execute` in the main checkout, `cargo xtask check` there, then removal of the copies, worktree and branch.
    /// Nothing is pushed unless the branch's check passes. `--execute` fetches origin first; a dry run judges origin/main as last fetched.
    Land(land::Args),
    /// Licence of every source file in a fetched upstream checkout (read-only).
    ///
    /// Per file: a `paths` entry of the upstream in licenses.toml (longest prefix), else its SPDX tag,
    /// else its notice read by fixed phrase rules, else the upstream's `unlabelled` licence when the
    /// file has no notice at all. An unrecognised notice is reported, never guessed. The licence
    /// gate applies the same resolution to what a build links.
    Licence(notice::Args),
    /// Churn of the AOSP code the Rust system services mirror (services.toml) between two AOSP revisions: what a tag bump must be reviewed against.
    ///
    /// Per service, most churn first; per entry lines added + deleted, of the whole file or of the named declaration's body (overloads together), GONE when it vanished. Read-only.
    /// The src/aosp checkouts are shallow (pinned tag only): fetch the other tag there blobless, or pass `--repo <project>=<git dir>` holding both; a missing revision fails with the command it needs.
    UpgradeDiff(service_mirrors::Args),
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
}

#[derive(Subcommand)]
enum Test {
    /// The host LLVM the in-process JIT links (llvm_host.rs, `native --for llvm`): llvm-config reports LLVM 20.0.0 with the host target, no libstdc++ NEEDED, and `lli -jit-kind=mcjit` on `ret i32 42` exits 42. `--break` returns 43 and must fail.
    Llvm(llvm_host::Args),
    /// The converter-built libandroid_runtime on host ART: load (every symbol bound), JNI registration (startReg + gRegJNI count), native calls checked against Android semantics, one copy per library.
    ///
    /// Stages print pass / FAIL / measured / WARN with counts; the exit code fails on any FAIL.
    /// The gRegJNI count is measured, not judged, until framework.jar exists: the walk aborts at the first class the test fixture (xtask/testdata/native-load) does not declare.
    /// Duplicate libraries (libbase, liblog, libnativehelper, ICU, libc++) warn; --strict-copies fails on them, for an ART built by the converter.
    /// The fixture (Java declarations + JNI shim in Rust) is built into <out>/native-load on every run with the Java pipeline's JDK and d8 and the repo's rustc.
    NativeLoad(native_load::Args),
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
    /// The M2 gate: atld starts binderd, servicemanager, the M2 services and the test APK (xtask/testdata/app) through the broker; pass = its Activity.onCreate logs.
    ///
    /// Failure prints the app log's last lines; the services' unimplemented-call counters are printed either way.
    /// NativeBridge breaks: `ATL_NB_BREAK=<name>[,<name>]` in this run's environment; the run stages the core with it (names: atl-native-bridge build.rs `BREAKS`).
    App(app::Args),
    /// The A1a audio gate (xtask/testdata/audio): AudioSystem's session ids, AudioTrack's minimum buffer and a track's player id, served by atl-audioflinger in the app process (audio_app.rs).
    AudioApp(audio_app::Args),
    /// Broadcast delivery P1: two fixture apps (xtask/testdata/broadcast) under atld; a runtime receiver, a sticky returned and replayed, an explicit manifest receiver cold-started headless, a denied sticky send, an unexported receiver skipped.
    ///
    /// Each check prints pass / FAIL; the services' counters are printed either way, the logs' tails on failure.
    Broadcast(broadcast::Args),
    /// An app's ContentProviders (xtask/testdata/provider) under atld: initOrder before Application.onCreate, disabled and other-process ones skipped, PackageManager lookups, a call to its own provider.
    Provider(provider::Args),
    /// The CTS instrumentation path: AOSP's `am instrument` runs a JUnit3 test APK (xtask/testdata/instrument) through the services; its INSTRUMENTATION_* text is parsed and checked.
    ///
    /// Run 1: a passing, a failing and an erroring test: status codes 1/0, 1/-2, 1/-1, the final code -1, and the instrumented process killed once the run finished.
    /// Run 2: a test that kills its own process: the run is reported cancelled (code 0, shortMsg "Process crashed.").
    Instrument(instrument::Args),
    /// The CTS progress metric: every cts.toml row's test APK is built, installed and run with `am instrument`; the number of passing tests per module is the result.
    ///
    /// Per module: state (RAN / INCOMPLETE / CRASH / START_FAIL / BUILD_FAIL) and pass / fail / error / ignored / total, then the unimplemented and refused counters summed over the run, most hit first; answered and sent counts as totals. `<out>/cts/report.json` holds every test's outcome.
    /// Exit status is nonzero only for harness errors and, with `--baseline <report.json>`, for modules or tests that got worse than that saved report; failing tests are the metric, not errors.
    Cts(cts::Args),
    /// The in-app SurfaceFlinger against AOSP's libgui in a real app process (M3 slice 4): atld starts the services and a client class (xtask/testdata/composer) as an app process; its compositor is registered through the system process and found by libgui by name.
    ///
    /// Pass = layers created, 30 transactions applied, the transaction-committed callback delivered, vsync at the display's refresh interval on Choreographer, every compositor counter as the checks say (`--self-test` lists the defects they catch). Buffers come with gralloc.
    Composer(composer::Args),
    /// The window path, one stage at a time. `--stage resume` (M3 plan step 1, slice 1): a software window fixture (xtask/testdata/window) is started through atld, as `xtask test app` starts its app; pass = its activity's onResume is logged.
    ///
    /// The app then calls into WindowManager, which no service provides yet: its log after the marker and the services' counters that rose after it are printed.
    Window(window::Args),
    /// `run-apk --drive` on fixtures, judged from its PNGs: a shot's pixels are the known quadrants (also after `resize`), and a tap visibly changes the frame. `--break-blank` and `--break-colour` must turn it red.
    Drive(drive_gate::Args),
    /// The sensor service: a fixture (xtask/testdata/sensors) uses SensorManager under atld with a fake `/sys/bus` (private mount namespace). `--stage empty`: no sensor, no stall; `--stage iio`: a fake IIO accelerometer's events with hand-derived values. `--break` must turn it red.
    Sensors(sensors::Args),
    /// The GTK platform layer (M3 slice 6) on a real Wayland compositor: a window maps, frame-clock ticks present scenes, GSK readback equals the pattern, a blocked UI thread answers no `xdg_wm_base` ping.
    ///
    /// Runs crates/atl-platform-ui/examples/gate.rs in the gate's headless weston (`--hold-blocked`: on $WAYLAND_DISPLAY) through a Wayland proxy that injects the pings; the module doc of xtask/src/platform_ui.rs lists the gates and the evidence tier of each.
    /// The compositor's own "not responding" reaction is not observable on WSLg: `--hold-blocked <s>` keeps the UI thread blocked for the Arch-laptop run to watch.
    PlatformUi(platform_ui::Args),
    /// The PCM output of atl-platform-ui-linux (plan A0) on PipeWire: a 1 kHz tone played to a null sink the run makes the default, recorded from its monitor, checked for frequency, level and duration. `--break zeros` and `--break no-target` must turn it red.
    ///
    /// The client is crates/atl-platform-ui/examples/audio_pcm.rs; the module doc of xtask/src/audio_pcm.rs lists the gates and the environment it changes (restored on every exit). Needs pipewire, pw-cli, pw-metadata and pw-record on PATH.
    AudioPcm(audio_pcm::Args),
    /// The bridge-1 JNI fixture (xtask/testdata/jni-test), NDK-built for minSdk 21 (emulated TLS) and 29 (ELF TLS): DT_NEEDED libc.so + libc++_shared.so, verneed libc.so/LIBC, the variant's TLS shape, and host glibc's loader refusing it on `LIBC`.
    ///
    /// Controls run every time: the probe loads a glibc library cleanly, and each variant fails the other's TLS expectation. Needs the NDK only, no native build.
    JniFixture(jni_fixture::Args),
    /// The GLES2 test APK (xtask/testdata/gles2), shaped like Cut the Rope's path: GLSurfaceView + an NDK `.so` drawing with GLES2 only and reading pixels back. Static: `.so` DT_NEEDED and `gl*` imports equal the listed sets, manifest glEsVersion 2.0 + hardwareAccelerated, the library in `lib/<abi>/`.
    ///
    /// Controls run every time: each check rejects a broken copy of the real input. Needs the NDK, the Java pipeline's aapt2/d8 and a build.prop; no runtime.
    Gles2Fixture(gles2_fixture::Args),
    /// The task-copy namespace (task copies bound over src/aosp): javac accepts a bind-mounted `--patch-module=java.base` dir, the overlay check refuses a wrong copy or no namespace; with `--plain`/`--task`, two native out dirs' build.ninja differ only on bound projects' lines.
    ///
    /// Controls run every time: the symlinked (link-farm) shape must make javac fail. Needs the prebuilt JDK and bwrap; no native build unless comparing.
    TaskBind(tree_gate::Args),
    /// AOSP's GL fronts (USE_SLOW_BINDING) dispatch each thread's GL calls through its own hooks table, libEGL -> libGLESv2, via atl-bionic's `__get_tls()`; prints the per-call cost.
    ///
    /// Control every run: a libGLESv2 with its own atl-bionic copy (dlmopen) must fail. Needs libEGL + libGLESv2 in --out; no driver.
    GlDispatch(gl_dispatch::Args),
    /// GLES on the real driver in an app process: a pbuffer cleared and read back, and an extension outside entries.in through eglGetProcAddress (patch 0010); then a deliberate break that both checks must catch.
    ///
    /// Needs the fronts, Mesa, gralloc and the services in --out (`xtask native`).
    Gl(gl_probe::Args),
}

fn main() -> Result<ExitCode> {
    let cli = Cli::parse();
    let root = repo_root()?;
    let name = verdict_name(&cli.command);
    // the commands that read src/aosp see the task copies through it (tree.rs)
    let shell = match &cli.command {
        Command::Native(a) => Some(a.shell),
        Command::Check(_) => Some(false),
        _ => None,
    };
    if let Some(shell) = shell {
        let overlays = tree::overlays_of(&root)?;
        // `native --shell`: the shell gets the compiler-cache variables of its own source view, as each ninja run does
        let env = match &cli.command {
            Command::Native(a) if a.shell => {
                native::shell_env(a, &root, &tree::describe(&overlays))?
            }
            _ => Vec::new(),
        };
        if let Some(code) = tree::enter(&overlays, shell, &env)? {
            return Ok(code);
        }
    }
    let result = match cli.command {
        Command::Check(args) => check::run(&root, &args),
        Command::Deps(args) => deps::run(&root, &args),
        Command::Fetch(args) => fetch::run(&root, &args),
        Command::Aosp(args) => aosp::run(&root, &args),
        Command::Native(args) => native::run(&root, &args),
        Command::MesaConfig(args) => mesa::run(&root, &args),
        Command::Slot { slot } => slot::run(&root, &slot),
        Command::Patch(args) => patch::run(&root, &args),
        Command::Land(args) => land::run(&root, &args),
        Command::Licence(args) => notice::run(&root, &args),
        Command::RunApk(args) => run_apk::run(&root, &args),
        Command::Remote { remote } => remote::run(&root, &remote),
        Command::UpgradeDiff(args) => {
            let layout = atl_layout::Layout::of(&root)?;
            service_mirrors::upgrade_diff(&root, &args, &|project| {
                layout
                    .locate(&std::path::Path::new("aosp").join(project))
                    .dir
            })?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Test {
            test: Test::Llvm(args),
        } => llvm_host::run(&root, &args),
        Command::Test {
            test: Test::NativeLoad(args),
        } => native_load::run(&root, &args),
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
            test: Test::Gl(args),
        } => gl_probe::run(&root, &args),
    };
    // a run that ends in an error (`?`, bail!) still ends with its verdict line; main prints the error after it
    if let (Err(e), Some(name)) = (&result, name) {
        println!("{}", verdict::error(name, e));
    }
    result
}

/// The verdict-line name of a command whose failures end in a verdict line; None for the others.
fn verdict_name(command: &Command) -> Option<&'static str> {
    Some(match command {
        Command::Native(_) => "native",
        Command::Check(_) => "check",
        Command::Land(_) => "land",
        Command::RunApk(_) => "run-apk",
        Command::Remote { .. } => "remote",
        Command::Test { test } => match test {
            Test::Llvm(_) => "llvm",
            Test::NativeLoad(_) => "native-load",
            Test::Parcel(_) => "parcel",
            Test::Provider(_) => "provider",
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
            Test::AudioPcm(_) => "audio-pcm",
            Test::JniFixture(_) => "jni-fixture",
            Test::Gles2Fixture(_) => "gles2-fixture",
            Test::TaskBind(_) => "task-bind",
            Test::GlDispatch(_) => "gl-dispatch",
            Test::Gl(_) => "gl",
        },
        _ => return None,
    })
}

/// Subcommand names, the targets of `xtask:<name>` references in deps.toml.
pub fn subcommands() -> BTreeSet<String> {
    Cli::command()
        .get_subcommands()
        .map(|c| c.get_name().to_owned())
        .collect()
}

/// `cargo` of the pinned toolchain: `CARGO` is set by `cargo run`.
pub fn cargo_command() -> std::process::Command {
    std::process::Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
}

/// The checkout being worked on (main repo or a worktree), i.e. the parent of `xtask/`.
fn repo_root() -> Result<PathBuf> {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest_dir
        .parent()
        .map(PathBuf::from)
        .context("xtask manifest dir has no parent")
}
