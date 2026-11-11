// SPDX-License-Identifier: MIT
//! `xtask test native-load`: the converter-built libandroid_runtime on the converter's ART (`stage_art`), started as an app process starts: atl-app-launcher over the default property area atl-property-service writes (the broker's place), in ten stages.
//!
//! 1. load: `System.load` with `LD_BIND_NOW=1`, so every undefined symbol of every object it pulls in must resolve at load time (lazy binding would fail only at the first call).
//! 2. register: `AndroidRuntime::startReg` must return 0, then gRegJNI walked entry by entry (startReg's own loop, register_jni_procs) must register every entry.
//!    framework.jar (the converter's framework-minus-apex) is on the boot classpath. "Every" = the `REG_JNI(` entries of the pinned AndroidRuntime.cpp, which must also equal the slots read from the library's relocations; a mismatch fails. Then register_android_os_SystemClock / _util_Log / _util_EventLog alone: each must return >= 0.
//! 3. call: natives checked against Android semantics -- uptimeMillis is CLOCK_MONOTONIC in ms (android_os_SystemClock.cpp; bracketed by System.nanoTime, also CLOCK_MONOTONIC in ART),
//!    isLoggable defaults to INFO (Log.isLoggable docs; liblog __android_log_is_loggable), println_native returns liblog __android_log_buf_write's 1 for INFO and -EPERM (-1) for VERBOSE, which liblog drops before any logger under the minimum priority INFO every run sets (`ANDROID_LOG_TAGS=*:i`; without it buf_write's default is VERBOSE and Log.v is written, as on Android);
//!    the INFO message reaches stderr as the launcher's logger writes it (`<L> liblog/<tag>: <text>`), the VERBOSE one nowhere: the logger receives only what liblog let through.
//! 4. copies: each library of [`SINGLE_COPY`] mapped once after the load (/proc/self/maps). A warning by default; `--strict-copies` makes it a failure.
//! 5. chain: a library loaded after ART calls `sigaction(SIGSEGV)` itself, as game engines and crash reporters do; the launcher links libsigchain first, so that call binds to libsigchain and is chained behind ART's fault handler.
//!    Pass: null derefs in JIT-compiled code still become NPEs, and a native fault ART declines reaches the library's handler. Bound to libc instead, the handler replaces ART's and the first implicit null check ends the process in it.
//! 6. crash: native code raises SIGABRT (where a registration's LOG_ALWAYS_FATAL ends); the process must die of SIGABRT, as debuggerd's handler resends it on Android.
//!    ART's unexpected-signal handler (device variant, `ANDROID_ROOT != /system`) calls the previous action at its end: the launcher's crash handler. Without one that is a null pointer: the jump to 0 faults, and the process ends by SIGSEGV or ART's `_exit(1)` on the reentry (measured 2026-11-02: registration aborts SIGSEGV, this stage exit 1).
//!    `raise`, not `abort`: glibc's abort re-raises with SIG_DFL itself once a handler returns, so it would pass a handler that never resends.
//! 7. overflow: unbounded Java recursion on the main thread, then on a thread ART created; each must end in a caught StackOverflowError and the process exit 0.
//!    JIT-compiled code checks the stack implicitly: a probe `ART_STACK_OVERFLOW_GAP` (8 KiB, art.go) below sp hits the guard page, a SIGSEGV ART's fault handler turns into the error.
//!    Only the main thread has an alternate signal stack (the launcher's); on Android bionic gives every thread one, and ART's device variant (thread_android.cc) sets none up. Threads ART creates on glibc have none, so the kernel's signal frame and ART's handler run in the gap above the probe, which suffices (measured 2026-11-02, x86_64, gdb: SS_DISABLE on the new thread, fault at sp - 0x2000, all handled).
//!    The stage guards that: a larger signal frame or handler would kill the process here first.
//! 8. media: NDK `AMediaCodec_createDecoderByType` for [`MEDIA_MIMES`], the run an app-uid binder process of its own binderd + AOSP servicemanager. A warning, not a failure, until the in-app Codec2 store exists (audio slice M1);
//!    the detail is the first fatal line, else the last Codec2 line. Measured 2026-11-06: without `media.c2.hal.selection=aidl` (atl-property-service CONSTANTS) Codec2Client::CacheServiceNames takes the HIDL path and aborts
//!    (`Check failed: serviceManager Hardware service manager is not running.`); with it the process exits 0, both codecs null (`No Codec2 services declared in the manifest.`), after MediaCodecList's 5 s wait for `media.player`.
//! 9. decode: the clips of `audio.rs` (Ogg Vorbis, WAV PCM, from the game APKs) decoded one by one through the NDK extractor and codec, as stage 8 runs them. Frames, channels, rate and PCM bytes equal each container's; raw output equals the WAV data chunk; the process's Codec2 store is in-process (`store=local`, declared, instance `default`).
//! 10. player: after startReg, as stage 8 runs: MediaPlayer's class init must succeed (loads libmedia_jni, whose JNI_OnLoad registers every media class, then native_init). Then Happy Jump's sequence (GameShared.loadMusic): `new MediaPlayer`, `setLooping`, `setVolume`, `release`, no exception and no `media.player` lookup; `start()` in Idle throws IllegalStateException, as AOSP's MediaPlayer.cpp answers it.
//!     The sequence is a warning here: construction resolves an AttributionSource, which needs ActivityThread or the package service (see `player_result`).
//!
//! Fixture (xtask-harness/testdata/native-load): NativeLoad.java (driver), compiled against the boot class jars (framework's included) with the Java pipeline's JDK and d8; never shipped.
//! shim/, a JNI cdylib (a cargo package of its own, for the `libc` crate's per-arch `struct sigaction`), calls the C++ entry points (hidden register_* by .symtab vaddr) and installs stage 5's handler; built into <out>/cargo.
//! The ART runs, the launcher and the property area: host_art.rs.
//! `--self-test` feeds the guards broken inputs and fails unless each fires: the load guard (a non-ELF file, a library with an undefined symbol), the launcher's (no property area); the result checks are unit tests.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use anyhow::{Context, Result, bail, ensure};
use atl_layout::Layout;
use object::{
    Object, ObjectSection, ObjectSymbol, ObjectSymbolTable, RelocationTarget, SectionKind,
};

use crate::host_art::{ArtRun, HostArt, JavaFixture, exec};
use crate::tree;
use crate::verdict::{Failure, Verdict};

mod audio;

#[derive(clap::Args)]
pub struct Args {
    /// The host arch (an arches.toml row with `[host]`): dalvikvm<pointer_bits>, the JDK's prebuilt OS directory, the default --out.
    #[arg(long)]
    host: String,
    /// ART root: bin/dalvikvm<bits>, com.android.art, com.android.i18n, com.android.tzdata, e.g. a host ART built outside this repo, in its install layout.
    /// Default: the converter's ART in --out (`xtask native` goals dalvikvm, libart, ...), staged into <out>/native-load-art as an install would lay it out (see `stage_art`).
    #[arg(long)]
    art: Option<PathBuf>,
    /// Java pipeline build directory: the ART host boot jars, framework-minus-apex and its d8 (default: this checkout's slot, see `xtask slot`; else <workspace>/out/java/build).
    #[arg(long)]
    java_out: Option<PathBuf>,
    /// Native build directory holding libandroid_runtime (default: this checkout's slot, see `xtask slot`; else <workspace>/out/native/<host>); the fixture is built into its native-load/.
    #[arg(long)]
    out: Option<PathBuf>,
    /// Stage 4 fails, instead of warning, when a library is mapped more than once (for an ART built by the converter).
    #[arg(long)]
    strict_copies: bool,
    /// Instead of the stages: check the load guard refuses broken inputs.
    #[arg(long)]
    self_test: bool,
    /// Stage 9 decodes cheer.ogg truncated to half its bytes against the full file's expectation: the stage must fail.
    #[arg(long)]
    break_media: bool,
    /// The property area names the DMABUF-heap tier whatever the probe found (atl-property-service `--linear-tier dmabuf-heap`): on a host without /dev/dma_heap/system stage 9 must fail (NO_MEMORY), which shows the probe's gralloc-BLOB tier is what keeps it working.
    #[arg(long)]
    break_linear_tier: bool,
    /// The Codec2 store break: leaves this store library (a codec2_store.rs name, e.g. libcodec2_soft_amrnbdec) off the library path of stages 8 + 9, so the store's constructor fails on its dlopen naming it, and stage 9 must go red.
    #[arg(long)]
    break_codec: Option<String>,
    /// Leaves libmedia_jni.so off the library path: MediaPlayer's static init throws UnsatisfiedLinkError, and stage 10 must go red.
    #[arg(long)]
    break_media_jni: bool,
    /// Stage 9's game APKs (assets/audio/cheer.ogg, assets/sound/game_music.ogg, assets/sounds/tap.wav). Default: <workspace>/apk.
    #[arg(long)]
    apks: Option<PathBuf>,
    /// Print every ART run's command, stdout and stderr.
    #[arg(long, short)]
    verbose: bool,
}

const FIXTURE: &str = "xtask-harness/testdata/native-load";
const RUNTIME: &str = "device/libandroid_runtime/libandroid_runtime.so";
/// `android::AndroidRuntime::startReg(JNIEnv*)` and `android::gRegJNI` (internal linkage), Itanium-mangled.
const START_REG: &str = "_ZN7android14AndroidRuntime8startRegEP7_JNIEnv";
const REG_JNI: &str = "_ZN7androidL7gRegJNIE";
/// gRegJNI's definition, in the pinned AOSP tree: its `REG_JNI(` lines are the entry count stage 2 requires.
const ANDROID_RUNTIME_CPP: &str = "frameworks/base/core/jni/AndroidRuntime.cpp";
/// Registered alone in stage 2 (the classes stage 3 calls into, plus EventLog).
const ALONE: [&str; 3] = [
    "register_android_os_SystemClock",
    "register_android_util_Log",
    "register_android_util_EventLog",
];
/// Stage 4: libraries that must be mapped once. Key = file name up to `.so`, without `-host` (Soong's host ICU is libicuuc-host.so).
const SINGLE_COPY: [&str; 7] = [
    "libbase",
    "liblog",
    "libnativehelper",
    "libicuuc",
    "libicui18n",
    "libicu",
    "libc++",
];

/// Stage 5's VM options: JIT on, every method compiled at its first call (optimize threshold 0), each one logged (`-verbose:jit`, `JIT added` lines).
/// Compiled code checks null implicitly (a SIGSEGV ART's fault handler turns into an NPE); the interpreter checks explicitly and never faults, so interpreted the stage would pass on a libc-bound launcher too (measured 2026-11-02).
const CHAIN_VM: [&str; 3] = ["-Xusejit:true", "-Xjitthreshold:0", "-verbose:jit"];
/// The status the shim's handler ends the process with: none of the launcher's own (0, 1 for its errors) and no `128 + signal`.
const CHAIN_EXIT: i32 = 77;
/// Null derefs in compiled code, each a SIGSEGV sigchain must hand to ART's handler: more than one, so a handler that works only once also fails.
const CHAIN_ROUNDS: u32 = 100;
/// The driver methods the faulting null check may be compiled into (`read`, or `chain` inlining it), as ART's PrettyMethod prints them in the `JIT added` line (art jit_code_cache.cc).
const CHAIN_JIT: [&str; 2] = [
    "int NativeLoad.read(NativeLoad$Box)",
    "void NativeLoad.chain(int, int)",
];
/// Stack overflows per thread in stage 7: the main thread's rounds also warm the JIT (CHAIN_VM), so the new thread runs `recurse` compiled; more than one, so a handler that works only once also fails.
const OVERFLOW_ROUNDS: u32 = 3;
/// The recursing method, as ART's PrettyMethod prints it in the `JIT added` line: compiled, its stack check faults (interpreted, ART may check explicitly and never fault).
const OVERFLOW_JIT: &str = "void NativeLoad.recurse()";

pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    let env = Env::new(root, args)?;
    // stage_art restaged the ART lib dir (every device/<m>/<m>.so as a link) for this run only, so the unlink lasts one run
    if let Some(module) = &args.break_codec {
        let libs = crate::codec2_store::libraries(root)?;
        ensure!(
            libs.contains(module),
            "--break-codec {module}: not a Codec2 store library (codec2_store.rs)"
        );
        let at = env.host.lib_path.join(format!("{module}.so"));
        std::fs::remove_file(&at)
            .with_context(|| format!("--break-codec: unlinking {}", at.display()))?;
    }
    if args.break_media_jni {
        let at = env.host.lib_path.join("libmedia_jni.so");
        std::fs::remove_file(&at)
            .with_context(|| format!("--break-media-jni: unlinking {}", at.display()))?;
    }
    let fixture = env.build_fixture(root)?;
    if args.self_test {
        return self_test(root, &env, &fixture);
    }
    let mut report = Report::default();

    // 1 + 4: one run
    let load = env.art(&fixture, "load", &[env.runtime.display().to_string()])?;
    let maps = match load_result(&load) {
        Ok(maps) => {
            report.pass(
                "load",
                "loaded, every symbol bound (LD_BIND_NOW=1)".to_owned(),
            );
            maps
        }
        Err(e) => {
            report.fail("load", e);
            return Ok(report.finish());
        }
    };

    // 2
    let elf = RuntimeElf::read(&env.runtime)?;
    let base = [
        env.runtime.display().to_string(),
        fixture.shim.display().to_string(),
    ];
    let start = env.art(&fixture, "startreg", &base)?;
    let walk_args: Vec<String> = base
        .iter()
        .cloned()
        .chain(std::iter::once(format!("{:x}", elf.start_reg)))
        .chain(elf.reg_jni.iter().map(|(v, _)| format!("{v:x}")))
        .collect();
    let walk = env.art(&fixture, "walk", &walk_args)?;
    let names: Vec<&str> = elf.reg_jni.iter().map(|(_, n)| n.as_str()).collect();
    match start_reg_result(&start) {
        Ok(s) => report.pass("startReg", s),
        Err(e) => report.fail("startReg", e),
    }
    match walk_result(&walk, &names, env.reg_jni_entries) {
        Ok(s) => report.pass("gRegJNI", s),
        Err(e) => report.fail("gRegJNI", e),
    }

    let mut call_args: Vec<String> = base.to_vec();
    call_args.push(format!("{:x}", elf.start_reg));
    for name in ALONE {
        call_args.push(format!("{:x}", elf.symbol(&register_symbol(name))?));
    }
    let call = env.art(&fixture, "call", &call_args)?;
    match alone_result(&call, &ALONE) {
        Ok(s) => report.pass("register", s),
        Err(e) => report.fail("register", e),
    }

    // 3
    let checks = call_checks(&call);
    let failed: Vec<&String> = checks.iter().filter_map(|c| c.as_ref().err()).collect();
    if failed.is_empty() {
        let ok: Vec<&str> = checks
            .iter()
            .filter_map(|c| c.as_ref().ok().map(String::as_str))
            .collect();
        report.pass("call", ok.join("; "));
    } else {
        report.fail(
            "call",
            failed
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join("; "),
        );
    }

    // 4
    let dups = duplicates(&maps);
    if dups.is_empty() {
        report.pass(
            "copies",
            format!("{} libraries mapped once", SINGLE_COPY.len()),
        );
    } else {
        let detail = dups
            .iter()
            .map(|(lib, paths)| format!("{lib} x{}: {}", paths.len(), paths.join(", ")))
            .collect::<Vec<_>>()
            .join("; ");
        if args.strict_copies {
            report.fail("copies", detail);
        } else {
            report.warn("copies", detail);
        }
    }

    // 5
    let chain_args = [
        fixture.shim.display().to_string(),
        CHAIN_EXIT.to_string(),
        CHAIN_ROUNDS.to_string(),
    ];
    let chain = env.launch(
        &fixture,
        &fixture.java.area.1,
        &CHAIN_VM,
        "chain",
        &chain_args,
    )?;
    match chain_result(&chain) {
        Ok(s) => report.pass("chain", s),
        Err(e) => report.fail("chain", e),
    }

    // 6
    let crash = env.art(&fixture, "crash", &[fixture.shim.display().to_string()])?;
    match crash_result(&crash) {
        Ok(s) => report.pass("crash", s),
        Err(e) => report.fail("crash", e),
    }

    // 7
    let overflow = env.launch(
        &fixture,
        &fixture.java.area.1,
        &CHAIN_VM,
        "overflow",
        &[OVERFLOW_ROUNDS.to_string()],
    )?;
    match overflow_result(&overflow) {
        Ok(s) => report.pass("overflow", s),
        Err(e) => report.fail("overflow", e),
    }

    // 8
    let media_args: Vec<String> = base
        .iter()
        .cloned()
        .chain(MEDIA_MIMES.iter().map(|m| (*m).to_owned()))
        .collect();
    let media = env.binder_art(&fixture, "media", &media_args)?;
    match media_result(&media) {
        Ok(s) => report.pass("media", s),
        Err(e) => report.warn("media", e),
    }

    // 9
    let apks = match &args.apks {
        Some(dir) => dir.clone(),
        None => Layout::of(root)?.workspace().join("apk"),
    };
    match audio::run(
        &env,
        &fixture,
        &apks,
        &env.work.join(CLIP_DIR),
        args.break_media,
    ) {
        Ok(s) => report.pass("decode", s),
        Err(e) => report.fail("decode", e),
    }

    // 10
    let player = env.binder_art(&fixture, "player", &base)?;
    match player_result(&player) {
        Ok(s) => report.pass("player", s),
        Err(PlayerErr::Init(e)) => report.fail("player", e),
        Err(PlayerErr::Sequence(e)) => report.warn("player", e),
    }
    Ok(report.finish())
}

/// Stage 10's steps and what each must print (the app gate checks the same table, app.rs player_failures): Happy Jump's sequence, `start()` in Idle as AOSP answers it (MediaPlayer.cpp returns INVALID_OPERATION; _start's process_media_player_call throws nothing for it: exception NULL reports MEDIA_ERROR to the listener, android_media_MediaPlayer.cpp:193-199, 425-434).
pub(crate) const PLAYER_STEPS: [(&str, &str); 5] = [
    ("new", "ok"),
    ("setLooping", "ok"),
    ("setVolume", "ok"),
    ("start", "ok"),
    ("release", "ok"),
];

/// Stage 10's outcome short of a pass: class init failed (libmedia_jni, JNI_OnLoad, native_init: a failure), or the sequence (a warning).
enum PlayerErr {
    Init(String),
    Sequence(String),
}

/// Stage 10: class init must succeed; then every step as expected, the process exits 0, and nothing asked servicemanager for `media.player` (construction needs no service; a lookup there is the 5 s wait).
/// The sequence is a warning: `new MediaPlayer()` resolves an AttributionSource (AttributionSource.myAttributionSource: ActivityThread's, else PackageManager.getPackagesForUid), which this bare-ART harness has neither of; an app process has both.
fn player_result(run: &ArtRun) -> Result<String, PlayerErr> {
    let lines: Vec<&str> = run
        .stdout
        .lines()
        .filter_map(|l| l.strip_prefix("nlt player "))
        .collect();
    if !lines.contains(&"init ok") {
        return Err(PlayerErr::Init(format!(
            "MediaPlayer class init: [{}], {}",
            lines.join("; "),
            run.status
        )));
    }
    player_sequence(run, &lines).map_err(PlayerErr::Sequence)
}

fn player_sequence(run: &ArtRun, lines: &[&str]) -> Result<String, String> {
    let bad: Vec<String> = PLAYER_STEPS
        .iter()
        .filter(|(step, want)| {
            !lines.iter().any(|l| {
                l.strip_prefix(step)
                    .and_then(|r| r.strip_prefix(' '))
                    .is_some_and(|r| r.starts_with(want))
            })
        })
        .map(|(step, want)| format!("{step}: want {want}"))
        .collect();
    let lookup = run.stderr.contains("media.player");
    if run.ok && bad.is_empty() && !lookup {
        return Ok(lines.join("; "));
    }
    let why = run
        .stdout
        .lines()
        .find(|l| l.starts_with("nlt load-error"))
        .or_else(|| {
            run.stderr
                .lines()
                .find(|l| l.contains("UnsatisfiedLinkError") || l.contains(" F "))
        })
        .unwrap_or("");
    Err(format!(
        "[{}] {}{}, {}: {}",
        lines.join("; "),
        bad.join("; "),
        if lookup {
            "; media.player looked up"
        } else {
            ""
        },
        run.status,
        why.trim()
    ))
}

/// Stage 8's binder uid: AID_APP_START (libcutils android_filesystem_config.h), the first app uid, as an app process has.
const AID_APP_START: u32 = 10_000;

/// Stage 8's run limit: ART start (~1 s here) plus codec lookups; a hang (a wait for an absent service) ends as SIGTERM in the detail instead of a stuck gate.
const MEDIA_LIMIT: std::time::Duration = std::time::Duration::from_secs(60);

/// Stage 9's clip dir below the work dir: the clips extracted from the APKs and the decoder's .pcm output beside each.
const CLIP_DIR: &str = "media";

/// Stage 8's mimes: what games decode through SoundPool/MediaPlayer (Ogg Vorbis, WAV PCM).
const MEDIA_MIMES: [&str; 2] = ["audio/vorbis", "audio/raw"];

/// Stage 8: every mime's decoder created. A warning until the in-app Codec2 store exists (audio M1); the detail carries how it failed: the codec lines, the exit, the Codec2 log line.
fn media_result(run: &ArtRun) -> Result<String, String> {
    let codecs: Vec<&str> = run
        .stdout
        .lines()
        .filter_map(|l| l.strip_prefix("nlt codec "))
        .collect();
    if run.ok && codecs.len() == MEDIA_MIMES.len() && codecs.iter().all(|c| c.ends_with(" 1")) {
        return Ok(codecs.join("; "));
    }
    // the first fatal line is the cause; ART's thread dump after it repeats the stack
    let lines = || run.stderr.lines();
    let why = lines()
        .find(|l| l.contains("Check failed") || l.contains(" F liblog/Codec2"))
        .or_else(|| lines().rev().find(|l| l.contains("Codec2")))
        .unwrap_or("no Codec2 log line");
    let why = why.split_once(" F ").map_or(why, |(_, rest)| rest);
    Err(format!(
        "codecs [{}], {}: {}",
        codecs.join("; "),
        run.status,
        why.trim()
    ))
}

// ---- inputs --------------------------------------------------------------------------------------

struct Env {
    host: HostArt,
    runtime: PathBuf,
    /// gRegJNI's entry count in the pinned source ([`reg_jni_entries`]).
    reg_jni_entries: usize,
    /// The workspace and the native build directory: binderd's build (stage 8).
    root: PathBuf,
    out: PathBuf,
    work: PathBuf,
}

struct Fixture {
    java: JavaFixture,
    shim: PathBuf,
}

/// The launcher's refusal without a property area (atl-app-launcher lib.rs).
const NO_AREA: &str = "no system property area";

impl Env {
    fn new(root: &Path, args: &Args) -> Result<Self> {
        let layout = Layout::of(root)?;
        let out =
            crate::build_state::gate_out(root, args.out.as_deref(), &args.host, "native-load")?;
        let mut host = HostArt::new(
            root,
            &args.host,
            args.java_out.as_deref(),
            &out,
            "native-load-art",
            args.verbose,
        )?;
        if args.break_linear_tier {
            host.linear_tier = Some("dmabuf-heap");
        }
        let runtime = out.join(RUNTIME);
        ensure!(runtime.exists(), "missing input {}", runtime.display());
        let cpp = tree::shared_tree(&layout).join(ANDROID_RUNTIME_CPP);
        let src =
            std::fs::read_to_string(&cpp).with_context(|| format!("reading {}", cpp.display()))?;
        Ok(Env {
            host,
            runtime,
            reg_jni_entries: reg_jni_entries(&src).with_context(|| cpp.display().to_string())?,
            work: out.join("native-load"),
            root: root.to_owned(),
            out,
        })
    }

    /// The fixture into <out>/native-load, rebuilt every run (a few seconds; no staleness logic to get wrong).
    fn build_fixture(&self, root: &Path) -> Result<Fixture> {
        let java = self
            .host
            .compile(&root.join(FIXTURE).join("java"), &self.work)?;
        Ok(Fixture {
            java,
            shim: build_shim(root, &self.host.cargo)?,
        })
    }

    /// NativeLoad.java `mode args`, started by the launcher over the fixture's property area.
    fn art(&self, fixture: &Fixture, mode: &str, args: &[String]) -> Result<ArtRun> {
        self.launch(fixture, &fixture.java.area.1, &[], mode, args)
    }

    /// `art`, run as an app-uid binder process of a binderd and AOSP servicemanager started for it (binder.rs harness), as an app process is: media code opens the driver and asks servicemanager before any codec lookup (without one, ProcessState waits for the context object forever, measured 2026-11-06).
    /// Killed after [`MEDIA_LIMIT`].
    fn binder_art(&self, fixture: &Fixture, mode: &str, args: &[String]) -> Result<ArtRun> {
        let device = self.out.join("device");
        let servicemanager = device.join("servicemanager/servicemanager");
        // both in this gate's goals (gate_goals.rs), which gate_out checked built
        let service = device.join("service/service");
        let allocator = crate::gralloc::allocator(&self.root, &self.out)?;
        let mapper_dir = crate::gralloc::stage_mapper(&self.root, &self.out)?;
        let test_root = self.work.join("binder");
        // the graphics HALs declared as atld declares them: the codec list asks GraphicBufferMapper which pixel formats it supports (Codec2 buildMediaCodecList -> isHalPixelFormatSupported), fatal without a mapper
        crate::binder::fresh_layout_with(&test_root, &atl_gralloc::vintf::device_hals())?;
        let aosp = Layout::of(&self.root)?.src().join("aosp");
        crate::binder::link_rows(
            &test_root,
            &crate::atld_run::platform_rows(&self.out, &aosp)?,
        )?;
        let mut lib_env = crate::stage_env::lib_env(&self.out)?;
        crate::gralloc::append_lib_path(&mut lib_env, &mapper_dir);
        let (broker, mut binderd) =
            crate::binder::start_binderd(&self.root, &self.out, &test_root.join("binderd.log"))?;
        let (mut sm, found) = crate::binder::start_servicemanager(
            &broker,
            &test_root,
            &servicemanager,
            &service,
            &lib_env,
        )?;
        ensure!(found.0, "servicemanager did not answer: {}", found.1);
        let mut alloc = crate::gralloc::start_allocator(
            &broker,
            &allocator,
            &lib_env,
            &test_root.join("allocator.log"),
            None,
        )?;
        let mut cmd = self.host.command(
            &fixture.java,
            &fixture.java.area.1,
            &[],
            "NativeLoad",
            &std::iter::once(mode.to_owned())
                .chain(args.iter().cloned())
                .collect::<Vec<_>>(),
        );
        // libui dlopens the mapper by name (android_load_sphal_library)
        let lib_path = format!("{}:{}", self.host.lib_path.display(), mapper_dir.display());
        cmd.env("LD_LIBRARY_PATH", lib_path);
        let what = format!("{cmd:?}");
        // the sandbox shows only what is mounted: the slot read-only at its own path (boot jars, fixture dex + shim, property area, extracted clips, launcher), the staged ART root with what its links name (ICU / tzdata files link out of the slot), ANDROID_DATA writable inside the slot
        let mut mounts = crate::binder::layout_mounts(&test_root)?;
        mounts.push(atl_broker::Mount {
            host: self.out.clone(),
            at: self.out.clone(),
            writable: false,
        });
        mounts.extend(atl_broker::tree_mounts(&self.host.art)?);
        // writable: ANDROID_DATA, and the decode stage's clip dir (its .pcm output)
        for dir in [fixture.java.work.join("data"), self.work.join(CLIP_DIR)] {
            std::fs::create_dir_all(&dir)?;
            mounts.push(atl_broker::Mount {
                host: dir.clone(),
                at: dir,
                writable: true,
            });
        }
        let run =
            crate::binder::spawn_piped_with(&broker, &cmd, AID_APP_START, mounts).and_then(|p| {
                // Kills the sandbox (kill_sandbox, as atld stops one), not bwrap: a hung run ends at MEDIA_LIMIT with SIGKILL, and a finished one cancels the timer.
                let (done, ended) = std::sync::mpsc::channel::<()>();
                let host_pid = p.host_pid;
                std::thread::spawn(move || {
                    if let Err(std::sync::mpsc::RecvTimeoutError::Timeout) =
                        ended.recv_timeout(MEDIA_LIMIT)
                    {
                        let _ = atl_broker::kill_sandbox(host_pid);
                    }
                });
                let run = self.host.finish(&what, p.child);
                drop(done);
                run
            });
        let _ = alloc.kill();
        alloc.wait()?;
        let _ = sm.child.kill();
        sm.child.wait()?;
        drop(broker);
        binderd.child.wait()?;
        run
    }

    /// `art` with the property-area variable set to `area` and the extra VM options `vm`.
    fn launch(
        &self,
        fixture: &Fixture,
        area: &str,
        vm: &[&str],
        mode: &str,
        args: &[String],
    ) -> Result<ArtRun> {
        let args: Vec<String> = std::iter::once(mode.to_owned())
            .chain(args.iter().cloned())
            .collect();
        self.host
            .launch(&fixture.java, area, vm, "NativeLoad", &args)
    }
}

/// The fixture's JNI shim (its own cargo package, `<FIXTURE>/shim`), release, in the cargo target dir `target`; its path.
fn build_shim(root: &Path, target: &Path) -> Result<PathBuf> {
    crate::unit_use::exec(
        crate::cargo_command()
            .current_dir(root)
            .args(["build", "--release", "--locked", "--manifest-path"])
            .arg(root.join(FIXTURE).join("shim/Cargo.toml"))
            .arg("--target-dir")
            .arg(target),
        "cargo build (shim)",
    )?;
    let lib = target.join("release/libnlt_shim.so");
    ensure!(lib.is_file(), "{} missing after cargo build", lib.display());
    Ok(lib)
}

/// A host cdylib from one Rust file, with the repo's pinned toolchain (rustup reads rust-toolchain.toml in `root`).
fn rustc(root: &Path, src: &Path, out: &Path, extra: &[&str]) -> Result<()> {
    exec(
        Command::new("rustc")
            .current_dir(root)
            .args(["--edition", "2024", "--crate-type", "cdylib", "-o"])
            .arg(out)
            .args(extra)
            .arg(src),
        "rustc",
    )
}

// ---- the library ---------------------------------------------------------------------------------

struct RuntimeElf {
    symbols: BTreeMap<String, u64>,
    start_reg: u64,
    /// gRegJNI's mProc slots in order: (slot vaddr, function name).
    reg_jni: Vec<(u64, String)>,
}

impl RuntimeElf {
    fn read(path: &Path) -> Result<Self> {
        let data = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        let file =
            object::File::parse(&*data).with_context(|| format!("parsing {}", path.display()))?;
        let mut symbols = BTreeMap::new();
        let mut functions = BTreeMap::new();
        for s in file.symbols() {
            if let Ok(name) = s.name() {
                symbols.insert(name.to_owned(), s.address());
                if s.kind() == object::SymbolKind::Text && s.address() != 0 {
                    functions.insert(s.address(), name.to_owned());
                }
            }
        }
        ensure!(
            !symbols.is_empty(),
            "{}: no .symtab (stripped): register_* and gRegJNI are found there",
            path.display()
        );
        let start_reg = *symbols
            .get(START_REG)
            .with_context(|| format!("no {START_REG}"))?;
        let table = file
            .symbols()
            .find(|s| s.name() == Ok(REG_JNI))
            .with_context(|| format!("no {REG_JNI} in .symtab"))?;
        let reg_jni = reg_jni(&file, &functions, table.address(), table.size())?;
        Ok(RuntimeElf {
            symbols,
            start_reg,
            reg_jni,
        })
    }

    fn symbol(&self, name: &str) -> Result<u64> {
        self.symbols
            .get(name)
            .copied()
            .with_context(|| format!("no {name} in .symtab"))
    }
}

/// gRegJNI's mProc slots in order: (slot vaddr, function name). The walk calls through the slot as relocated at run time: 6 of 172 point into other libraries (libhwui's register_android_graphics_*), which no vaddr of this object reaches.
/// Read from the dynamic relocations over the table: RegJNIRec is {mProc} with NDEBUG (Soong's default cflags), {mProc, mName} without, so a slot is an mProc unless its value lands outside executable code (mName, a string).
/// Relocation types are not decoded: absolute target -> addend (RELATIVE); symbol -> that symbol (defined: its value + addend; undefined: another library's function); REL (implicit addend) reads the slot.
fn reg_jni(
    file: &object::File,
    functions: &BTreeMap<u64, String>,
    addr: u64,
    size: u64,
) -> Result<Vec<(u64, String)>> {
    let word = if file.is_64() { 8 } else { 4 };
    let little = file.is_little_endian();
    let bytes = file
        .sections()
        .find_map(|s| s.data_range(addr, size).ok().flatten())
        .context("gRegJNI's bytes are in no section")?;
    let dynsym = file.dynamic_symbol_table();
    let text = |v: u64| {
        file.sections().any(|s| {
            s.kind() == SectionKind::Text && (s.address()..s.address() + s.size()).contains(&v)
        })
    };
    let mut slots = BTreeMap::new();
    for (offset, r) in file.dynamic_relocations().into_iter().flatten() {
        if !(addr..addr + size).contains(&offset) {
            continue;
        }
        let at = usize::try_from(offset - addr)?;
        let implicit = if r.has_implicit_addend() {
            let b = bytes.get(at..at + word).context("slot past gRegJNI")?;
            let mut buf = [0u8; 8];
            if little {
                buf[..word].copy_from_slice(b);
                u64::from_le_bytes(buf)
            } else {
                buf[8 - word..].copy_from_slice(b);
                u64::from_be_bytes(buf)
            }
        } else {
            0
        };
        let value = |base: u64| base.wrapping_add_signed(r.addend()).wrapping_add(implicit);
        let local = |v: u64| {
            text(v).then(|| {
                functions
                    .get(&v)
                    .map_or_else(|| format!("<{v:#x}>"), |n| demangle_register(n))
            })
        };
        let slot = match r.target() {
            RelocationTarget::Absolute => local(value(0)),
            RelocationTarget::Symbol(i) => {
                let sym = dynsym
                    .as_ref()
                    .context("symbol relocation without .dynsym")?
                    .symbol_by_index(i)?;
                if sym.is_undefined() {
                    Some(demangle_register(sym.name()?))
                } else {
                    local(value(sym.address()))
                }
            }
            _ => bail!("gRegJNI slot {at}: unexpected relocation target"),
        };
        if let Some(name) = slot {
            slots.insert(offset, name);
        }
    }
    // every record relocated: a slot the walk would read as null otherwise
    let n = u64::try_from(slots.len())?;
    ensure!(
        n > 0 && (n * word as u64 == size || 2 * n * word as u64 == size),
        "gRegJNI: {n} relocated function slots in {size} bytes, neither {{mProc}} nor {{mProc, mName}} records"
    );
    Ok(slots.into_iter().collect())
}

/// `_ZN7android<n>register_xEP7_JNIEnv` / `_Z<n>register_xP7_JNIEnv` -> `register_x`; other names as they are.
fn demangle_register(name: &str) -> String {
    name.find("register_")
        .and_then(|i| {
            let s = &name[i..];
            s.strip_suffix("EP7_JNIEnv")
                .or_else(|| s.strip_suffix("P7_JNIEnv"))
        })
        .map_or_else(|| name.to_owned(), str::to_owned)
}

/// `android::<name>(JNIEnv*)`, Itanium-mangled.
fn register_symbol(name: &str) -> String {
    format!("_ZN7android{}{name}EP7_JNIEnv", name.len())
}

// ---- results -------------------------------------------------------------------------------------

/// The probe's `nlt <key> <value>` result lines in an [`ArtRun`]'s stdout.
trait NltLines {
    /// Values of the `nlt <key> <value>` lines with this key.
    fn values(&self, key: &str) -> Vec<&str>;
    fn value(&self, key: &str) -> Option<&str> {
        self.values(key).into_iter().next()
    }
}

impl NltLines for ArtRun {
    fn values(&self, key: &str) -> Vec<&str> {
        self.stdout
            .lines()
            .filter_map(|l| l.strip_prefix("nlt "))
            .filter_map(|l| {
                let (k, v) = l.split_once(' ').unwrap_or((l, ""));
                (k == key).then_some(v)
            })
            .collect()
    }
}

/// Stage 1: the mapped files, or the loader's message.
fn load_result(run: &ArtRun) -> Result<Vec<String>, String> {
    if let Some(e) = run.value("load-error") {
        return Err(format!("UnsatisfiedLinkError: {}", e.replace('\t', ": ")));
    }
    if run.value("loaded").is_none() || !run.ok {
        return Err(format!("no load: {}", run.failure()));
    }
    Ok(run.values("map").into_iter().map(str::to_owned).collect())
}

/// gRegJNI's entries in AndroidRuntime.cpp: the `REG_JNI(` lines between `gRegJNI[] = {` and its closing `};` (the two `#define REG_JNI` lines precede it).
fn reg_jni_entries(src: &str) -> Result<usize> {
    let (_, body) = src
        .split_once("gRegJNI[] = {")
        .context("no `gRegJNI[] = {`")?;
    let (body, _) = body
        .split_once("\n};")
        .context("gRegJNI: no closing `};`")?;
    let n = body
        .lines()
        .filter(|l| l.trim_start().starts_with("REG_JNI("))
        .count();
    ensure!(n > 0, "gRegJNI: no REG_JNI( entries");
    Ok(n)
}

/// Stage 2: startReg returns 0 (AndroidRuntime.cpp: `return 0` once register_jni_procs succeeds, -1 otherwise).
fn start_reg_result(run: &ArtRun) -> Result<String, String> {
    match run.value("startreg") {
        Some("0") => Ok("returned 0".to_owned()),
        Some(rc) => Err(format!("returned {rc}, expected 0")),
        None => Err(format!("aborted: {}", run.failure())),
    }
}

/// Stage 2 walk: every entry registers, and the slots read from the relocations (`names`) number the source's `expected` entries.
fn walk_result(run: &ArtRun, names: &[&str], expected: usize) -> Result<String, String> {
    if names.len() != expected {
        return Err(format!(
            "{} relocated slots, but {ANDROID_RUNTIME_CPP} has {expected} REG_JNI entries",
            names.len()
        ));
    }
    let ok = run
        .values("ret")
        .iter()
        .filter(|v| {
            v.split(' ')
                .nth(1)
                .and_then(|rc| rc.parse::<i64>().ok())
                .is_some_and(|rc| rc >= 0)
        })
        .count();
    let mut s = format!("{ok}/{} registered", names.len());
    if ok < names.len() {
        let name = names.get(ok).copied().unwrap_or("?");
        let why = match run.values("ret").get(ok) {
            Some(v) => format!("returned {}", v.split(' ').nth(1).unwrap_or("?")),
            None => format!("aborted: {}", run.failure()),
        };
        s.push_str(&format!("; entry {} {name} {why}", ok + 1));
        return Err(s);
    }
    Ok(format!("{s} (= {ANDROID_RUNTIME_CPP}'s REG_JNI entries)"))
}

/// Stage 2 alone: every listed registration returned >= 0.
fn alone_result(run: &ArtRun, names: &[&str]) -> Result<String, String> {
    let rets = run.values("ret");
    for (i, name) in names.iter().enumerate() {
        match rets.get(i).and_then(|v| v.split(' ').nth(1)) {
            Some(rc) if rc.parse::<i64>().is_ok_and(|rc| rc >= 0) => {}
            Some(rc) => return Err(format!("{name} returned {rc}")),
            None => return Err(format!("{name} aborted: {}", run.failure())),
        }
    }
    Ok(format!("{} registered alone", names.join(", ")))
}

/// Stage 3: one entry per native call, Ok(summary) or Err(why).
fn call_checks(run: &ArtRun) -> Vec<Result<String, String>> {
    let missing = || format!("no result: {}", run.failure());
    let expect = |key: &str, want: &str, what: &str| match run.value(key) {
        Some(v) if v == want => Ok(format!("{what} = {v}")),
        Some(v) => Err(format!("{what} = {v}, expected {want}")),
        None => Err(format!("{what}: {}", missing())),
    };
    let uptime = match run.value("uptime").map(|v| {
        v.split(' ')
            .map(str::parse::<i64>)
            .collect::<Result<Vec<_>, _>>()
    }) {
        Some(Ok(v)) if v.len() == 3 => {
            const NS_PER_MS: i64 = 1_000_000;
            let (lo, up, hi) = (v[0] / NS_PER_MS, v[1], v[2] / NS_PER_MS);
            if (lo..=hi).contains(&up) {
                Ok(format!(
                    "uptimeMillis = {up} (CLOCK_MONOTONIC {lo}..={hi} ms)"
                ))
            } else {
                Err(format!(
                    "uptimeMillis = {up}, outside CLOCK_MONOTONIC {lo}..={hi} ms"
                ))
            }
        }
        Some(_) => Err("uptimeMillis: malformed result".to_owned()),
        None => Err(format!("uptimeMillis: {}", missing())),
    };
    vec![
        uptime,
        // Log.isLoggable: "The default level of any tag is set to INFO"
        expect("loggable-verbose", "false", "isLoggable(VERBOSE)"),
        expect("loggable-info", "true", "isLoggable(INFO)"),
        // liblog logger_write.cpp __android_log_buf_write: `return 1` once the message is written
        expect("println", "1", "println_native"),
        // ... and `return -EPERM` (-1) when __android_log_is_loggable refuses, before any logger
        expect("println-verbose", "-1", "println_native(VERBOSE)"),
        logger_result(run),
    ]
}

/// NativeLoad.java's two println_native messages: INFO and VERBOSE.
const LOGGED: &str = "native-load println_native";
const FILTERED: &str = "native-load verbose";

/// The launcher's logger got the INFO message (its line: `I liblog/<tag>: <text>`, atl-app-launcher log.rs) and not the VERBOSE one liblog dropped.
fn logger_result(run: &ArtRun) -> Result<String, String> {
    let ours = format!(" I liblog/nlt: {LOGGED}");
    if !run.stderr.lines().any(|l| l.ends_with(&ours)) {
        return Err(format!("logger: no `{ours}` line on stderr"));
    }
    if run.stderr.contains(FILTERED) {
        return Err(format!("logger: wrote `{FILTERED}`, which liblog refuses"));
    }
    Ok("logger: INFO written, VERBOSE never reached it".to_owned())
}

/// Stage 4: libraries of [`SINGLE_COPY`] mapped from more than one path.
fn duplicates(maps: &[String]) -> BTreeMap<&'static str, Vec<String>> {
    let mut by_lib: BTreeMap<&'static str, Vec<String>> = BTreeMap::new();
    for path in maps {
        let file = path.rsplit('/').next().unwrap_or(path);
        let Some(stem) = file.split_once(".so").map(|(s, _)| s) else {
            continue;
        };
        let stem = stem.strip_suffix("-host").unwrap_or(stem);
        if let Some(lib) = SINGLE_COPY.iter().find(|l| **l == stem) {
            by_lib.entry(lib).or_default().push(path.clone());
        }
    }
    by_lib.retain(|_, paths| paths.len() > 1);
    by_lib
}

/// The stage lines; the first failing stage is the one the verdict names and quotes.
#[derive(Default)]
struct Report {
    first: Option<(String, String)>,
}

impl Report {
    fn line(&self, stage: &str, verdict: &str, detail: &str) {
        println!("native-load: {stage:<9} {verdict:<8} {detail}");
    }
    fn pass(&mut self, stage: &str, detail: String) {
        self.line(stage, "pass", &detail);
    }
    fn warn(&mut self, stage: &str, detail: String) {
        self.line(stage, "WARN", &detail);
    }
    fn fail(&mut self, stage: &str, detail: String) {
        self.line(stage, "FAIL", &detail);
        self.first.get_or_insert_with(|| (stage.to_owned(), detail));
    }
    fn finish(self) -> ExitCode {
        let Some((stage, detail)) = self.first else {
            return ExitCode::SUCCESS;
        };
        let verdict = Verdict {
            cmd: "native-load",
            total: None,
            skipped: Vec::new(),
            failures: vec![Failure::step(&stage, detail)],
        };
        println!("{}", verdict.line());
        ExitCode::FAILURE
    }
}

// ---- self test -----------------------------------------------------------------------------------

/// The load guard on real ART: each broken input must be refused with the loader's reason.
fn self_test(root: &Path, env: &Env, fixture: &Fixture) -> Result<ExitCode> {
    let broken = env.work.join("broken");
    std::fs::create_dir_all(&broken)?;
    let not_elf = broken.join("libnot_elf.so");
    // longer than an ELF header: a shorter file is refused as "file too short" before the magic is read
    std::fs::write(&not_elf, "not an ELF object\n".repeat(16))?;
    let unresolved = broken.join("libnlt_unresolved.so");
    // the linker must not refuse the undefined symbol itself: the loader is what is tested
    rustc(
        root,
        &root.join(FIXTURE).join("unresolved.rs"),
        &unresolved,
        &["-C", "link-arg=-Wl,-z,undefs"],
    )?;
    let cases = [
        ("non-ELF file", not_elf, "invalid ELF header"),
        (
            "undefined symbol",
            unresolved,
            "undefined symbol: nlt_missing_symbol",
        ),
    ];
    let mut report = Report::default();
    for (what, lib, reason) in cases {
        let run = env.art(fixture, "load", &[lib.display().to_string()])?;
        match load_result(&run) {
            Err(e) if e.contains(reason) => {
                report.pass("self-test", format!("{what} refused: {e}"))
            }
            Err(e) => report.fail(
                "self-test",
                format!("{what} refused without `{reason}`: {e}"),
            ),
            Ok(_) => report.fail(
                "self-test",
                format!("{what} loaded: the load guard did not fire"),
            ),
        }
    }
    // the launcher's guard: an area path with no file behind it (and none at /dev/__properties__ on this host) must stop the start
    let missing = broken.join("no-property-area");
    let run = env.launch(
        fixture,
        &missing.display().to_string(),
        &[],
        "load",
        &[env.runtime.display().to_string()],
    )?;
    match area_refused(&run) {
        Ok(s) => report.pass("self-test", s),
        Err(e) => report.fail("self-test", e),
    }
    Ok(report.finish())
}

/// Stage 5: every null deref an NPE with the shim's handler installed, the faulting code JIT-compiled (else nothing faulted and the stage proves nothing), then the native fault ended the process in the shim's handler.
fn chain_result(run: &ArtRun) -> Result<String, String> {
    if run.value("sigaction") != Some("0") {
        return Err(format!(
            "the shim's sigaction(SIGSEGV) did not return 0: {}",
            run.failure()
        ));
    }
    let in_handler = run.code == Some(CHAIN_EXIT);
    let want = CHAIN_ROUNDS.to_string();
    match run.value("npe") {
        Some(n) if n == want => {}
        Some(n) => return Err(format!("{n} of {CHAIN_ROUNDS} null derefs became NPEs")),
        None if in_handler => {
            return Err(format!(
                "the shim's SIGSEGV handler replaced ART's fault handler: the first implicit null check ended the process in it (exit {CHAIN_EXIT}), no NPE; sigaction bound past libsigchain"
            ));
        }
        None => return Err(format!("no NPE count: {}", run.failure())),
    }
    let jit = run
        .stderr
        .lines()
        .filter(|l| l.contains("JIT added"))
        .find_map(|l| CHAIN_JIT.iter().find(|m| l.contains(*m)));
    let Some(method) = jit else {
        return Err(format!(
            "none of {CHAIN_JIT:?} JIT-compiled: the null checks ran interpreted and never faulted, so chaining was not exercised"
        ));
    };
    if run.value("fault").is_some() || !in_handler {
        return Err(format!(
            "the native fault did not reach the shim's handler (exit {CHAIN_EXIT} expected): {}",
            run.status
        ));
    }
    Ok(format!(
        "{CHAIN_ROUNDS} null derefs in JIT code ({method}) -> NPEs under the shim's own sigaction(SIGSEGV); the native fault reached its handler (exit {CHAIN_EXIT})"
    ))
}

/// Stage 6: the process died of the signal the shim raised (its number as the shim printed it, `nlt signal`).
fn crash_result(run: &ArtRun) -> Result<String, String> {
    let Some(want) = run.value("signal").and_then(|v| v.parse::<i32>().ok()) else {
        return Err(format!("no signal number printed: {}", run.failure()));
    };
    if run.value("crash").is_some() {
        return Err(format!(
            "the process survived its SIGABRT ({want}): the crash handler returned without resending it"
        ));
    }
    match run.signal {
        Some(s) if s == want => Ok(format!(
            "native SIGABRT -> ART's unexpected-signal handler -> the launcher's crash handler -> died of signal {s}"
        )),
        Some(s) => Err(format!(
            "died of signal {s}, not the raised SIGABRT ({want}): ART's unexpected-signal handler chained to no crash handler ({})",
            run.failure()
        )),
        // ART's `_exit(1)` when its handler is reentered: a fault inside the first one (null old action, or a signal stack too small for its dump)
        None => Err(format!(
            "exited instead of dying of SIGABRT ({want}): {}",
            run.failure()
        )),
    }
}

/// Stage 7: every overflow caught on both threads (depths as printed, `nlt soe-main` / `nlt soe-thread`), `recurse` JIT-compiled (else the stack check may never have faulted), then a normal exit.
fn overflow_result(run: &ArtRun) -> Result<String, String> {
    let rounds = OVERFLOW_ROUNDS as usize;
    let main = run.values("soe-main");
    if main.len() != rounds {
        return Err(format!(
            "main thread: {} of {OVERFLOW_ROUNDS} overflows caught: {}",
            main.len(),
            run.failure()
        ));
    }
    let thread = run.values("soe-thread");
    if thread.len() != rounds {
        return Err(format!(
            "new thread: {} of {OVERFLOW_ROUNDS} overflows caught, the main thread's all were: {} (a thread without an alternate signal stack cannot run ART's fault handler)",
            thread.len(),
            run.failure()
        ));
    }
    if !run
        .stderr
        .lines()
        .any(|l| l.contains("JIT added") && l.contains(OVERFLOW_JIT))
    {
        return Err(format!(
            "{OVERFLOW_JIT} not JIT-compiled: the stack checks may have been explicit, so the fault handler was not exercised"
        ));
    }
    if !run.ok || run.value("overflow") != Some("done") {
        return Err(format!(
            "every overflow caught, then no normal exit: {}",
            run.failure()
        ));
    }
    Ok(format!(
        "{OVERFLOW_ROUNDS} StackOverflowErrors caught in JIT code on the main thread (depth {}) and on a new thread (depth {}); exit 0",
        main.join("/"),
        thread.join("/")
    ))
}

/// The launcher refused to start: failed, its refusal on stderr, the fixture never ran.
fn area_refused(run: &ArtRun) -> Result<String, String> {
    if run.ok || run.value("loaded").is_some() {
        return Err("no property area: the launcher started the VM anyway".to_owned());
    }
    match run.stderr.lines().find(|l| l.contains(NO_AREA)) {
        Some(l) => Ok(format!("no property area refused: {}", l.trim())),
        None => Err(format!(
            "no property area: failed without `{NO_AREA}`: {}",
            run.failure()
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(stdout: &str, ok: bool) -> ArtRun {
        ArtRun {
            ok,
            code: ok.then_some(0),
            signal: (!ok).then_some(SIGABRT),
            status: if ok {
                "exit status: 0"
            } else {
                "signal: 6 (SIGABRT)"
            }
            .to_owned(),
            stdout: stdout.to_owned(),
            stderr: "x F nlt: Unable to find class android/util/MemoryIntArray\n".to_owned(),
        }
    }

    /// A chain-mode run: stdout lines, the `JIT added` method if any, the exit status.
    fn chain_run(stdout: &str, jit: Option<&str>, code: i32) -> ArtRun {
        let stderr = jit.map_or(String::new(), |m| {
            format!("I atl-app-launcher: jit_code_cache.cc:836 JIT added (kind=Optimized) {m}@0x1 ccache_size=1KB\n")
        });
        ArtRun {
            ok: code == 0,
            code: Some(code),
            signal: None,
            status: format!("exit status: {code}"),
            stdout: stdout.to_owned(),
            stderr,
        }
    }

    /// Linux's numbers (asm-generic/signal.h, x86 alike); the check only needs them distinct, as the shim prints the raised one.
    const SIGABRT: i32 = 6;
    const SIGSEGV: i32 = 11;

    /// A crash-mode run: stdout lines, the signal that ended it (None: an exit with status 0).
    fn crash_run(stdout: &str, signal: Option<i32>) -> ArtRun {
        ArtRun {
            ok: signal.is_none(),
            code: signal.is_none().then_some(0),
            signal,
            status: signal.map_or("exit status: 0".to_owned(), |s| format!("signal: {s}")),
            stdout: stdout.to_owned(),
            stderr: String::new(),
        }
    }

    #[test]
    fn crash_check() {
        let printed = format!("nlt signal {SIGABRT}\n");
        assert!(crash_result(&crash_run(&printed, Some(SIGABRT))).is_ok());
        // no crash handler: ART's unexpected-signal handler jumps to the null old action
        let e = crash_result(&crash_run(&printed, Some(SIGSEGV))).unwrap_err();
        assert!(e.contains("died of signal 11"), "{e}");
        // a handler that returns without resending: raise returns, the driver goes on
        let survived = format!("{printed}nlt crash survived\n");
        let e = crash_result(&crash_run(&survived, None)).unwrap_err();
        assert!(e.contains("survived"), "{e}");
        assert!(crash_result(&crash_run("", Some(SIGABRT))).is_err());
    }

    #[test]
    fn overflow_check() {
        let lines = |key: &str, n: u32| -> String {
            (0..n)
                .map(|i| format!("nlt {key} {}\n", 1000 + i))
                .collect()
        };
        let main = lines("soe-main", OVERFLOW_ROUNDS);
        let both = format!("{main}{}", lines("soe-thread", OVERFLOW_ROUNDS));
        let done = format!("{both}nlt overflow done\n");
        let jit = Some(OVERFLOW_JIT);
        assert!(overflow_result(&chain_run(&done, jit, 0)).is_ok());
        // no alternate stack on the new thread: its first overflow kills the process
        let e = overflow_result(&crash_run(&main, Some(SIGSEGV))).unwrap_err();
        assert!(e.starts_with("new thread: 0 of"), "{e}");
        // one overflow handled, the next not
        let once = format!("{main}{}", lines("soe-thread", 1));
        assert!(overflow_result(&crash_run(&once, Some(SIGSEGV))).is_err());
        assert!(overflow_result(&crash_run("", Some(SIGSEGV))).is_err());
        // interpreted: maybe never faulted
        let e = overflow_result(&chain_run(&done, None, 0)).unwrap_err();
        assert!(e.contains("not JIT-compiled"), "{e}");
        // every overflow caught, then a bad exit
        assert!(overflow_result(&chain_run(&both, jit, 1)).is_err());
    }

    #[test]
    fn chain_check() {
        let read = Some(CHAIN_JIT[0]);
        let npe = format!("nlt sigaction 0\nnlt npe {CHAIN_ROUNDS}\n");
        // chained: every NPE, then the shim's handler ended the process
        assert!(chain_result(&chain_run(&npe, read, CHAIN_EXIT)).is_ok());
        assert!(chain_result(&chain_run(&npe, Some(CHAIN_JIT[1]), CHAIN_EXIT)).is_ok());
        // libc-bound: the handler took the first implicit null check
        let e = chain_result(&chain_run("nlt sigaction 0\n", read, CHAIN_EXIT)).unwrap_err();
        assert!(e.contains("replaced ART's fault handler"), "{e}");
        // interpreted: nothing faulted, so nothing was tested
        let e = chain_result(&chain_run(&npe, None, CHAIN_EXIT)).unwrap_err();
        assert!(e.contains("never faulted"), "{e}");
        // the native fault never reached the shim (sigchain dropped it, or ART's handler aborted)
        let e = chain_result(&chain_run(&npe, read, 134)).unwrap_err();
        assert!(e.contains("did not reach the shim's handler"), "{e}");
        let survived = format!("{npe}nlt fault survived\n");
        assert!(chain_result(&chain_run(&survived, read, 0)).is_err());
        // some NPEs lost
        assert!(
            chain_result(&chain_run("nlt sigaction 0\nnlt npe 1\n", read, CHAIN_EXIT)).is_err()
        );
        // sigaction itself failed
        assert!(chain_result(&chain_run("nlt sigaction -1\n", read, 1)).is_err());
    }

    #[test]
    fn load_guard_fires_on_link_error() {
        let r = run(
            "nlt load-error /l.so\tdlopen failed: undefined symbol: foo\n",
            false,
        );
        let e = load_result(&r).expect_err("guard must fire");
        assert!(e.contains("undefined symbol: foo"), "{e}");
        assert!(load_result(&run("nlt loaded /l.so\nnlt map /a\n", true)).is_ok());
        // a crash after a successful System.load is still a load failure
        assert!(load_result(&run("nlt loaded /l.so\n", false)).is_err());
    }

    #[test]
    fn walk_counts_until_abort() {
        let r = run(
            "nlt reg 0\nnlt ret 0 0\nnlt reg 1\nnlt ret 1 0\nnlt reg 2\n",
            false,
        );
        // short walk: aborted at entry 3
        let s = walk_result(&r, &["a", "b", "register_x", "d"], 4).unwrap_err();
        assert!(
            s.starts_with("2/4 registered; entry 3 register_x aborted"),
            "{s}"
        );
        assert!(s.contains("MemoryIntArray"), "{s}");
        let neg = run("nlt ret 0 0\nnlt ret 1 -1\n", true);
        let e = walk_result(&neg, &["a", "b"], 2).unwrap_err();
        assert!(e.contains("entry 2 b returned -1"), "{e}");
        let all = run("nlt ret 0 0\nnlt ret 1 0\n", true);
        assert!(walk_result(&all, &["a", "b"], 2).is_ok());
        // relocated slots (2) vs source entries (3): fails even with every slot registered
        let e = walk_result(&all, &["a", "b"], 3).unwrap_err();
        assert!(
            e.contains("2 relocated slots") && e.contains("has 3"),
            "{e}"
        );
    }

    #[test]
    fn start_reg_must_return_zero() {
        assert!(start_reg_result(&run("nlt startreg 0\n", true)).is_ok());
        let e = start_reg_result(&run("nlt startreg -1\n", true)).unwrap_err();
        assert!(e.contains("returned -1"), "{e}");
        assert!(start_reg_result(&run("", false)).is_err());
    }

    #[test]
    fn reg_jni_entries_counts_the_table_only() {
        // shape of AndroidRuntime.cpp: the two #define lines and a comment outside/inside the table are not entries
        let src = "#define REG_JNI(name) { name }\n#define REG_JNI(name) { name, #name }\nstatic const RegJNIRec gRegJNI[] = {\n        REG_JNI(a),\n        // c\n        REG_JNI(b),\n};\nREG_JNI(after)\n";
        assert_eq!(reg_jni_entries(src).unwrap(), 2);
        assert!(reg_jni_entries("gRegJNI[] = {\n};\n").is_err());
    }

    #[test]
    fn alone_fails_on_negative_or_abort() {
        let good = run("nlt ret 0 0\nnlt ret 1 0\n", true);
        assert!(alone_result(&good, &["a", "b"]).is_ok());
        assert!(alone_result(&run("nlt ret 0 0\nnlt ret 1 -1\n", true), &["a", "b"]).is_err());
        assert!(alone_result(&run("nlt ret 0 0\n", false), &["a", "b"]).is_err());
    }

    /// The launcher's logger line for the INFO message (atl-app-launcher log.rs format).
    const INFO_LINE: &str =
        "1762000000.007 5.250    42    43 I liblog/nlt: native-load println_native\n";

    #[test]
    fn call_checks_fire_on_wrong_results() {
        // 5 ms between the two nanoTime reads; uptime inside, at the edges, outside
        let base = "nlt loggable-verbose false\nnlt loggable-info true\nnlt println 1\nnlt println-verbose -1\n";
        let mut ok = run(
            &format!("nlt uptime 1000000000 1002 1005000000\n{base}"),
            true,
        );
        ok.stderr.push_str(INFO_LINE);
        assert!(
            call_checks(&ok).iter().all(Result::is_ok),
            "{:?}",
            call_checks(&ok)
        );
        let edge = run(
            &format!("nlt uptime 1000999999 1000 1005000000\n{base}"),
            true,
        );
        assert!(call_checks(&edge)[0].is_ok());
        let late = run(
            &format!("nlt uptime 1000000000 1006 1005000000\n{base}"),
            true,
        );
        assert!(call_checks(&late)[0].is_err());
        let wrong = run(
            "nlt uptime 1 0 1\nnlt loggable-verbose true\nnlt loggable-info false\nnlt println 0\nnlt println-verbose 1\n",
            true,
        );
        assert!(call_checks(&wrong).iter().skip(1).all(Result::is_err));
        assert!(call_checks(&run("", false)).iter().all(Result::is_err));
    }

    #[test]
    fn logger_check_fires() {
        let mut r = run("", true);
        // no line from our logger (e.g. liblog's own stderr logger still installed)
        assert!(logger_result(&r).is_err());
        r.stderr.push_str(INFO_LINE);
        assert!(logger_result(&r).is_ok());
        // a VERBOSE line written: something filtered nothing
        r.stderr
            .push_str("1 2 3 4 V liblog/nlt: native-load verbose\n");
        assert!(logger_result(&r).is_err());
    }

    #[test]
    fn area_guard_check() {
        let mut refused = run("", false);
        refused.stderr =
            "atl-app-launcher: no system property area: neither $ATL_PROP_AREA nor ...\n"
                .to_owned();
        assert!(area_refused(&refused).is_ok());
        // started anyway, or failed for another reason
        assert!(area_refused(&run("nlt loaded /l.so\n", true)).is_err());
        assert!(area_refused(&run("", false)).is_err());
    }

    #[test]
    fn copies_by_library() {
        let maps: Vec<String> = [
            "/art/lib64/libbase.so",
            "/out/libbase/libbase.so",
            "/art/lib64/libicuuc-host.so",
            "/usr/lib/libicuuc.so.76.1",
            "/usr/lib/libc++.so.1.0",
            "/art/lib64/libc++.so",
            "/art/lib64/liblog.so",
            "/art/lib64/libbase_extra.so",
        ]
        .map(str::to_owned)
        .to_vec();
        let d = duplicates(&maps);
        assert_eq!(
            d.keys().copied().collect::<Vec<_>>(),
            ["libbase", "libc++", "libicuuc"]
        );
        assert!(duplicates(&maps[6..]).is_empty());
    }

    #[test]
    fn symbol_names() {
        // Itanium: <length><identifier>; "register_android_os_SystemClock" is 31 bytes
        assert_eq!(
            register_symbol("register_android_os_SystemClock"),
            "_ZN7android31register_android_os_SystemClockEP7_JNIEnv"
        );
        assert_eq!(
            demangle_register("_ZN7android31register_android_os_SystemClockEP7_JNIEnv"),
            "register_android_os_SystemClock"
        );
    }
}
