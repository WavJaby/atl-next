// SPDX-License-Identifier: MIT
//! `xtask test gl`: GLES on the real driver in an app process -- AOSP's libEGL / libGLESv2 fronts (slow binding, patches 0009 + 0010) over Mesa, placed as for an app (`Runtime::with_gl_driver`).
//! The fixture (xtask/testdata/gl-probe) runs as a plain app process (`--run-class`): Probe.java makes a GLES3 pbuffer with the framework's EGL14 / GLES20, clears it, reads it back;
//! its JNI library (jni/, a cargo package of its own for `unsafe`, placed as [`LIB`] in /system's library dir) calls [`EXTENSION`] through eglGetProcAddress.
//! 0. elf: no Mesa object (mesa::OUTPUTS) has an undefined gl*/egl* dynamic symbol or a dynamic relocation naming one -- such a reference would bind to AOSP's fronts, which call back into Mesa (recursion).
//!    Break: the probe library built with feature `undefined-gl` (imports glClear) must fail it.
//! 1. candidates: Mesa's dispatch names (its generated glapi_mapi_tmp.h string pool) that Mesa's API XML enables for ES2+ (`es2="..."`), minus AOSP's entries.in: every name that reaches
//!    eglGetProcAddress's extension-slot path, where 0010 returns the driver's address instead of the asm forwarder. [`EXTENSION`] must be one; the count is printed.
//! 2. pixels: every pixel of the [`SIZE`] pbuffer equals [`CLEAR`]: GL's float -> unorm8 conversion (ES 3.2 §2.3.5.2, round(f x 255)) of c / 255 gives c back, so the bytes are the expectation.
//! 3. extension: [`EXTENSION`](GL_MAX_TEXTURE_SIZE) writes the value glGetIntegerv (an entries.in function) gives, and the address lies in a Mesa object, not libEGL.
//! 4. driver: a DRM node the probe holds after eglInitialize, each with a known kernel driver (`judge_driver`).
//! 5. renderer: `GL_RENDERER` starts with [`RENDERER`]. Break: [`SOFTPIPE_RENDERER`] must not match it.
//! 6. scene: the last frame of a timed scene ([`TIMED_SIZE`] pixels; [`FRAMES`] frames after one untimed warm-up, each clear + draw + glFinish) reads back [`TEXEL`] on every pixel.
//! 7. frames: one wall-clock time per timed frame. The gate prints their median and p95 (nearest rank) with the 1/5/15-minute load average before and after (`uptime`'s figures).
//!    A baseline only while the 1-minute load before the run is below [`QUIET_LOAD`]; the gate never fails on it.
//!
//! Then a deliberate break, every run: the probe skips glClear and the timed draws, and calls a no-op in place of the extension; the pixel, scene and extension checks must fail.
//! Mode es1 (es1-scene.toml, gl_scene.rs; Happy Jump's GLES1 path, S1 of gs22-plan): the scene through Java GL10 and through libGLESv1_CM.so, every pixel judged against the scene's reference model.
//! 1. GL_VERSION `OpenGL ES-CM 1.`, renderer llvmpipe; 2. /proc/self/maps maps libGLESv1_CM_mesa.so; 3. no unimplemented-stub line; 4. Java pixels; 5. native pixels; 6. the 20 names resolve into libGLESv1_CM.so.
//!
//! Break: the steps marked `break` are dropped (skip glPopMatrix and the GL_COLOR_ARRAY enable); checks 4 and 5 must then mismatch, with a readback present.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, ensure};
use atl_layout::Layout;

use crate::atld_run::{Run, Runtime, Shell};
use crate::gl_scene::{self, Judged};
use crate::{manifest, mesa, remote, tree};

#[derive(clap::Args)]
pub struct Args {
    /// The host arch (an arches.toml row with `[host]`): the default --out.
    #[arg(long)]
    host: String,
    /// Native build directory holding ART, the services, the fronts and Mesa (default: this checkout's slot, see `xtask slot`; else <workspace>/out/native/<host>).
    #[arg(long)]
    out: Option<PathBuf>,
    /// Java pipeline build directory: boot jars, aapt2, d8 (default: this checkout's slot; else <workspace>/out/java/build).
    #[arg(long)]
    java_out: Option<PathBuf>,
    /// Timed frames of the scene, after one untimed warm-up; the median and p95 are over these. At least 20: below that the p95 is the slowest frame.
    #[arg(long, default_value_t = FRAMES)]
    frames: u32,
    /// Print each probe log.
    #[arg(long, short)]
    verbose: bool,
}

const FIXTURE: &str = "xtask/testdata/gl-probe";
const PROBE_CLASS: &str = "com.example.atlgl.Probe";
const PROBE_NAME: &str = "gl-probe";
/// An app uid (AID_APP_START + 124): the probe runs as an app.
const APP_UID: u32 = 10_124;
/// The JNI library; the sandbox sees it in atl_android_paths::SYSTEM_LIB (atld `--file`).
const LIB: &str = "libatl_gl_probe.so";
/// The pbuffer's size and clear colour (bytes): odd sizes so no row or tile alignment hides a stray pixel; each channel a k / 255 value away from 0 and 255.
const SIZE: (u32, u32) = (67, 33);
const CLEAR: [u8; 4] = [51, 102, 204, 255];
/// The probe's first shell argument after the library: the clear test or the ES1 scene (Probe.java dispatches on it).
const CLEAR_MODE: &str = "clear";
const ES1_MODE: &str = "es1";
/// The ES1 scene (mode es1), read by gl_scene.rs; its program is the probe's arguments.
const SCENE: &str = "es1-scene.toml";
/// The message AOSP's libEGL logs when a hook falls to the unimplemented stub (egl.cpp:177): check 3 fails on it.
const UNIMPLEMENTED: &str = "called unimplemented OpenGL ES API";
/// The timed scene's surface. A multiple of the grid size (Probe.java `GRID`), so the cell edges fall on pixel boundaries and no pixel centre sits on one.
const TIMED_SIZE: (u32, u32) = (256, 256);
/// The scene's texel: every scene pixel must read it back. Its fragment loop sums 64 terms of texel / 64, exact up to float rounding, which the unorm8 conversion absorbs.
const TEXEL: [u8; 4] = [204, 153, 51, 255];
/// Timed frames after one untimed warm-up. Each frame's time is one sample; a longer run only lengthens LIMIT's share.
const FRAMES: u32 = 60;
/// Nearest-rank p95 of n samples is the maximum while n < 20 (rank ceil(0.95 n) = n); from 20 on it is not.
const P95_MIN_FRAMES: u32 = 20;
/// The renderer name the gate requires: Mesa's llvmpipe on the host LLVM (`llvmpipe (LLVM <version>, <bits> bits)`, lp_screen.c). The prefix's version is checked against llvm_host::VERSION (unit test).
const RENDERER: &str = "llvmpipe (LLVM 20.";
/// The software tier llvmpipe replaced (softpipe, sp_screen.c). A deliberate break every run: on the renderer above it must not match.
const SOFTPIPE_RENDERER: &str = "softpipe";
/// Median and p95 are a baseline only while the 1-minute load is below this: an unconfirmed first guess (a quarter of this host's 16 threads). Reported as `quiet`, never a failure.
const QUIET_LOAD: f64 = 4.0;
const NS_PER_MS: f64 = 1_000_000.0;
/// In Mesa's ES dispatch, not in entries.in; observable through a query whose answer an entries.in call also gives.
const EXTENSION: &str = "glGetInteger64vEXT";
/// Mesa's generated dispatch table (build-relative) and its API XML (tree-relative); AOSP's entries.in.
const MAPI_TMP: &str = "build/src/mapi/shared-glapi/glapi_mapi_tmp.h";
const MESA_XML: &str = "src/mapi/glapi/gen";
const ENTRIES_IN: &str = "frameworks/native/opengl/libs/entries.in";
/// Each probe run's time limit: the child is killed past it. A remote run gets the same limit as `timeout` on the target (remote.rs).
pub const LIMIT: Duration = Duration::from_secs(120);
const POLL: Duration = Duration::from_millis(50);

/// The ES1 scene, read by the host probe, the guest fixture's generator (jni_fixture.rs) and the app gate (app.rs): one description.
pub fn load_scene(root: &Path) -> Result<gl_scene::Scene> {
    gl_scene::Scene::load(&root.join(FIXTURE).join(SCENE))
}

/// Native goals (gate_goals.rs): the runtime with gralloc and the GL driver.
pub fn goals() -> crate::gate_goals::Goals {
    crate::atld_run::goals().with(crate::atld_run::gralloc_goals())
}

/// `test gl`: the gate on this host.
pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    judge(root, args, remote::Site::Host)
}

/// `remote test gl` (remote.rs): the same gate, its atld runs on `site`. `host` is the arches.toml `[host]` row, `out` the slot the bundle was pushed from.
pub fn run_on(root: &Path, host: &str, out: &Path, site: remote::Site) -> Result<ExitCode> {
    let args = Args {
        host: host.to_owned(),
        out: Some(out.to_path_buf()),
        java_out: None,
        frames: FRAMES,
        verbose: false,
    };
    judge(root, &args, site)
}

fn judge(root: &Path, args: &Args, site: remote::Site) -> Result<ExitCode> {
    ensure!(
        args.frames >= P95_MIN_FRAMES,
        "--frames {} is below {P95_MIN_FRAMES}: the p95 would be the slowest frame",
        args.frames
    );
    let layout = Layout::of(root)?;
    let rt = Runtime::new(
        root,
        "gl",
        &args.host,
        args.out.as_deref(),
        args.java_out.as_deref(),
        "gl-art",
        args.verbose,
    )?
    .with_gralloc(root)?
    .with_gl_driver()?
    .with_site(site);
    let tree = tree::shared_tree(&layout);
    let cands = candidates(
        &std::fs::read_to_string(rt.out.join(mesa::OUT_SUBDIR).join(MAPI_TMP))?,
        &es_names(&tree.join(mesa::MESA_TREE).join(MESA_XML))?,
        &std::fs::read_to_string(tree.join(ENTRIES_IN))?,
    );
    let mut ok = cands.iter().any(|c| c == EXTENSION);
    println!(
        "gl: {} candidates: {} ES names of Mesa's dispatch need eglGetProcAddress's extension path (not in entries.in); {EXTENSION} {}",
        verdict(ok),
        cands.len(),
        if ok { "is one" } else { "is NOT one" }
    );
    let work = rt.out.join("gl");
    let build = rt.out.join(mesa::OUT_SUBDIR).join("build");
    let mut found = Vec::new();
    for o in mesa::OUTPUTS {
        found.extend(gl_imports(&build.join(o))?);
    }
    ok &= found.is_empty();
    println!(
        "gl: {} elf: {} Mesa objects: {}",
        verdict(found.is_empty()),
        mesa::OUTPUTS.len(),
        if found.is_empty() {
            "no undefined gl*/egl* symbol, no dynamic relocation naming one (none binds back to AOSP's fronts)".to_owned()
        } else {
            found.join("; ")
        }
    );
    let links = mesa::driver_links(&build.join(mesa::DRIVER))?;
    ok &= links.failures.is_empty();
    println!(
        "gl: {} driver links: NEEDED {}{}",
        verdict(links.failures.is_empty()),
        links.needed.join(" "),
        links
            .failures
            .iter()
            .map(|f| format!("; {f}"))
            .collect::<String>()
    );
    let lib = build_lib(root, &work.join("cargo"), &[])?;
    // deliberate break: the probe library built with an undefined glClear
    let broken = build_lib(root, &work.join("cargo-undefined-gl"), &[UNDEFINED_GL])?;
    let hits = gl_imports(&broken)?;
    let fired = !hits.is_empty();
    ok &= fired;
    println!(
        "gl: {} break elf: {}",
        verdict(fired),
        if fired {
            format!("fails as it must: {}", hits.join("; "))
        } else {
            "PASSED on a library importing glClear".to_owned()
        }
    );
    let lib_at = format!("{}/{LIB}", atl_android_paths::SYSTEM_LIB);
    let rt = rt.with_fixture_file(lib, &lib_at);
    let apk = rt
        .apk
        .build(&root.join(FIXTURE), &work.join("apk"), &[], None, None)?;
    let scene = load_scene(root)?;
    // the normal run's frame times, its renderer, and the load average around it
    let mut timed = None;
    for broken in [false, true] {
        let tag = if broken { "break" } else { "run" };
        let mut shell_args: Vec<String> = vec![
            lib_at.clone(),
            CLEAR_MODE.to_owned(),
            SIZE.0.to_string(),
            SIZE.1.to_string(),
        ];
        shell_args.extend(CLEAR.iter().map(u8::to_string));
        shell_args.extend(
            [TIMED_SIZE.0, TIMED_SIZE.1, args.frames]
                .iter()
                .map(u32::to_string),
        );
        shell_args.extend(TEXEL.iter().map(u8::to_string));
        if broken {
            shell_args.push("break".to_owned());
        }
        let load_before = loadavg()?;
        let log = run_probe(&rt, &apk, &work.join(tag), shell_args, args.verbose, tag)?;
        let load_after = loadavg()?;
        let lines = probe_lines(&log);
        let pixels = judge_pixels(&lines, "pixels", CLEAR, SIZE.0 * SIZE.1);
        let ext = judge_extension(&lines);
        let scene_pixels = judge_scene(&lines);
        if broken {
            // the break passes when every check fires
            for (what, r) in [
                ("pixels", &pixels),
                ("extension", &ext),
                ("scene", &scene_pixels),
            ] {
                let fired = r.is_err();
                ok &= fired;
                println!(
                    "gl: {} break {what}: {}",
                    verdict(fired),
                    r.as_ref().map_or_else(
                        |e| format!("fails as it must: {e}"),
                        |d| format!("PASSED on a broken run: {d}")
                    )
                );
            }
            continue;
        }
        let driver = judge_driver(&lines);
        let renderer = judge_renderer(&lines, RENDERER);
        let timing = judge_frames(&lines, args.frames);
        let timing_check = timing
            .as_ref()
            .map(|ns| format!("{} frame times, all positive", ns.len()))
            .map_err(String::clone);
        for (what, r) in [
            ("pixels", &pixels),
            ("extension", &ext),
            ("driver", &driver),
            ("renderer", &renderer),
            ("scene", &scene_pixels),
            ("frames", &timing_check),
        ] {
            ok &= r.is_ok();
            println!(
                "gl: {} {what}: {}",
                verdict(r.is_ok()),
                r.as_ref().unwrap_or_else(|e| e)
            );
        }
        // deliberate break: the software tier's name must not match this renderer
        let wrong = judge_renderer(&lines, SOFTPIPE_RENDERER);
        let fired = wrong.is_err();
        ok &= fired;
        println!(
            "gl: {} break renderer: {}",
            verdict(fired),
            wrong.as_ref().map_or_else(
                |e| format!("fails as it must: {e}"),
                |d| format!("PASSED on {d}")
            )
        );
        let name = renderer.as_deref().unwrap_or("unknown").to_owned();
        timed = timing.ok().map(|ns| (name, ns, load_before, load_after));
    }
    if let Some((name, ns, before, after)) = timed {
        let (median, p95) = perf_ms(&ns);
        println!(
            "gl: perf {name}, {} frames: median {median:.3} ms, p95 {p95:.3} ms (nearest rank); load avg 1/5/15 min before {}, after {}; quiet: {}",
            ns.len(),
            fmt_load(before),
            fmt_load(after),
            if before[0] < QUIET_LOAD { "yes" } else { "no" }
        );
    }
    ok &= es1(&rt, &apk, &work, &lib_at, &scene, args.verbose)?;
    println!(
        "{}",
        crate::verdict::gate(
            "gl",
            &if ok {
                vec![]
            } else {
                vec!["see the FAIL lines".to_owned()]
            }
        )
    );
    Ok(if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

/// One probe process: the probe class with `args` as its shell arguments, its files under `dir`; returns the log.
fn run_probe(
    rt: &Runtime,
    apk: &Path,
    dir: &Path,
    args: Vec<String>,
    verbose: bool,
    tag: &str,
) -> Result<String> {
    let run = Run {
        root: dir.join("root"),
        scratch: dir.join("scratch"),
        launch: None,
        packages: vec![(apk.to_path_buf(), manifest::read(&rt.apk.aapt2, apk)?)],
        first_uid: APP_UID,
        libraries: vec![],
        shell: Some(Shell {
            class: PROBE_CLASS.to_owned(),
            uid: APP_UID,
            name: PROBE_NAME.to_owned(),
            args,
            boot_jars: vec![apk.to_path_buf()],
        }),
        log_tags: "*:i".to_owned(),
    };
    let mut child = rt.start_atld(&run)?;
    let started = Instant::now();
    while child.try_wait()?.is_none() {
        if started.elapsed() > LIMIT {
            let _ = child.kill();
            break;
        }
        std::thread::sleep(POLL);
    }
    let end = child.wait()?;
    rt.check_end(end, &run)?;
    let log = std::fs::read_to_string(run.log(PROBE_NAME)).unwrap_or_default();
    if verbose {
        println!("---- {tag} log\n{log}");
    }
    Ok(log)
}

/// Mode es1: the scene through Java GL10 and through libGLESv1_CM.so, read back and judged (checks 1-6). Then the deliberate break: checks 4 and 5 must fail.
fn es1(
    rt: &Runtime,
    apk: &Path,
    work: &Path,
    lib_at: &str,
    scene: &gl_scene::Scene,
    verbose: bool,
) -> Result<bool> {
    let mut ok = true;
    for broken in [false, true] {
        let tag = if broken { "es1-break" } else { "es1-run" };
        let (w, h) = scene.size();
        let mut args = vec![
            lib_at.to_owned(),
            ES1_MODE.to_owned(),
            w.to_string(),
            h.to_string(),
        ];
        args.extend(scene.program(broken));
        let log = run_probe(rt, apk, &work.join(tag), args, verbose, tag)?;
        let lines = probe_lines(&log);
        let java = scene.judge(&rows(&lines, "java row"))?;
        let native = scene.judge(&rows(&lines, "native row"))?;
        for line in lines.iter().filter(|l| l.contains(" error ")) {
            ok = false;
            println!("gl: FAIL es1 probe said: {line}");
        }
        if broken {
            for (who, j) in [("java", &java), ("native", &native)] {
                let fired = matches!(j, Judged::Mismatch(_));
                ok &= fired;
                match j {
                    Judged::Mismatch(m) => {
                        println!("gl: pass es1 break {who} pixels: fails as it must: {m}")
                    }
                    other => println!("gl: FAIL es1 break {who} pixels: {}", describe(other)),
                }
            }
            continue;
        }
        let version = first_line(&lines, "version ").unwrap_or("none");
        let renderer = first_line(&lines, "renderer ").unwrap_or("none");
        let maps = first_line(&lines, "maps ").unwrap_or("none");
        let unimplemented = log.contains(UNIMPLEMENTED);
        let dl = dladdr(&lines);
        let dl_ok = scene.names().iter().all(|n| {
            dl.iter()
                .any(|(name, file)| name == n && file.ends_with("libGLESv1_CM.so"))
        });
        let checks: [(&str, bool, String); 6] = [
            (
                "1 version",
                version.starts_with("OpenGL ES-CM 1."),
                version.to_owned(),
            ),
            (
                "1 renderer",
                renderer.starts_with(RENDERER),
                renderer.to_owned(),
            ),
            (
                "2 maps",
                maps.contains("libGLESv1_CM_mesa.so"),
                maps.to_owned(),
            ),
            (
                "3 unimplemented",
                !unimplemented,
                if unimplemented {
                    UNIMPLEMENTED.to_owned()
                } else {
                    "none".to_owned()
                },
            ),
            (
                "4 java pixels",
                matches!(java, Judged::Match(_)),
                describe(&java),
            ),
            (
                "5 native pixels",
                matches!(native, Judged::Match(_)),
                describe(&native),
            ),
        ];
        for (what, pass, detail) in checks {
            ok &= pass;
            println!("gl: {} es1 {what}: {detail}", verdict(pass));
        }
        ok &= dl_ok;
        println!(
            "gl: {} es1 6 dladdr: {} of {} names resolve into libGLESv1_CM.so",
            verdict(dl_ok),
            dl.iter()
                .filter(|(_, f)| f.ends_with("libGLESv1_CM.so"))
                .count(),
            scene.names().len()
        );
    }
    Ok(ok)
}

/// The `<prefix> <y> <hex>` lines of a probe, as row y -> hex.
fn rows(lines: &[String], prefix: &str) -> BTreeMap<usize, String> {
    lines
        .iter()
        .filter_map(|l| {
            let (y, hex) = l.strip_prefix(prefix)?.strip_prefix(' ')?.split_once(' ')?;
            Some((y.parse().ok()?, hex.to_owned()))
        })
        .collect()
}

/// The `native dladdr <name> <file>` lines: name and the object its address lies in.
fn dladdr(lines: &[String]) -> Vec<(String, String)> {
    lines
        .iter()
        .filter_map(|l| {
            let (name, file) = l.strip_prefix("native dladdr ")?.split_once(' ')?;
            Some((name.to_owned(), file.to_owned()))
        })
        .collect()
}

fn first_line<'a>(lines: &'a [String], prefix: &str) -> Option<&'a str> {
    lines.iter().find_map(|l| l.strip_prefix(prefix))
}

fn describe(j: &Judged) -> String {
    match j {
        Judged::Match(n) => format!("all {n} non-edge pixels match"),
        Judged::Mismatch(m) => m.clone(),
        Judged::Missing(m) => format!("no readback: {m}"),
    }
}

fn verdict(ok: bool) -> &'static str {
    if ok { "pass" } else { "FAIL" }
}

/// Builds the JNI library into `target`; returns the .so.
fn build_lib(root: &Path, target: &Path, features: &[&str]) -> Result<PathBuf> {
    let status = Command::new(env!("CARGO"))
        .args(["build", "--release", "--quiet", "--manifest-path"])
        .arg(root.join(FIXTURE).join("jni/Cargo.toml"))
        .arg(format!("--features={}", features.join(",")))
        .arg("--target-dir")
        .arg(target)
        .status()
        .context("running cargo for the gl-probe library")?;
    ensure!(status.success(), "building the gl-probe library failed");
    Ok(target.join("release").join(LIB))
}

/// The fixture feature that makes the library import glClear.
const UNDEFINED_GL: &str = "undefined-gl";

/// A GL/EGL entry-point name: what AOSP's fronts export, so a reference to one from inside the driver resolves to the fronts and recurses.
fn gl_name(name: &str) -> bool {
    name.starts_with("gl") || name.starts_with("egl")
}

/// `path`'s undefined gl*/egl* dynamic symbols and dynamic relocations naming a gl*/egl* symbol, as `file: kind name` lines.
fn gl_imports(path: &Path) -> Result<Vec<String>> {
    use object::{Object, ObjectSymbol, ObjectSymbolTable, RelocationTarget};
    let data = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let file =
        object::File::parse(&*data).with_context(|| format!("parsing {}", path.display()))?;
    let short = path
        .file_name()
        .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
    let mut out = Vec::new();
    for s in file.dynamic_symbols() {
        if let Ok(n) = s.name()
            && s.is_undefined()
            && gl_name(n)
        {
            out.push(format!("{short}: undefined {n}"));
        }
    }
    let table = file.dynamic_symbol_table();
    for (_, r) in file.dynamic_relocations().into_iter().flatten() {
        if let (RelocationTarget::Symbol(i), Some(t)) = (r.target(), &table)
            && let Ok(n) = t
                .symbol_by_index(i)
                .and_then(|s| s.name().map(str::to_owned))
            && gl_name(&n)
        {
            out.push(format!("{short}: relocation {n}"));
        }
    }
    Ok(out)
}

/// The texts of the probe's `gl-probe: ` log lines.
fn probe_lines(log: &str) -> Vec<String> {
    let marker = format!("{PROBE_NAME}: ");
    log.lines()
        .filter_map(|l| l.split_once(&marker).map(|(_, t)| t.to_owned()))
        .collect()
}

/// `<what> <r,g,b,a>x<count> ...`: exactly one value, `want`, on all `count` pixels.
fn judge_pixels(lines: &[String], what: &str, want: [u8; 4], count: u32) -> Result<String, String> {
    let line = lines
        .iter()
        .find_map(|l| l.strip_prefix(what).and_then(|r| r.strip_prefix(' ')))
        .ok_or_else(|| no_line(what, lines))?;
    let expect = format!("{},{},{},{}x{count}", want[0], want[1], want[2], want[3]);
    if line == expect {
        Ok(format!("all {count} pixels {want:?}"))
    } else {
        Err(format!("read back {line}, want {expect}"))
    }
}

/// The timed scene's last frame: every pixel [`TEXEL`], and no GL or shader error from the probe. A failed draw reads back the clear colour.
fn judge_scene(lines: &[String]) -> Result<String, String> {
    let errors: Vec<&str> = lines
        .iter()
        .filter(|l| l.starts_with("scene-error"))
        .map(String::as_str)
        .collect();
    if !errors.is_empty() {
        return Err(errors.join(" | "));
    }
    judge_pixels(lines, "scene", TEXEL, TIMED_SIZE.0 * TIMED_SIZE.1)
}

/// The text of the probe's `renderer <GL_RENDERER>` line.
fn renderer_name(lines: &[String]) -> Result<&str, String> {
    lines
        .iter()
        .find_map(|l| l.strip_prefix("renderer "))
        .ok_or_else(|| no_line("renderer", lines))
}

/// The renderer is the tier the gate wants when its name starts with `want`; Ok carries the name.
fn judge_renderer(lines: &[String], want: &str) -> Result<String, String> {
    let name = renderer_name(lines)?;
    if name.starts_with(want) {
        Ok(name.to_owned())
    } else {
        Err(format!("{name} does not start with `{want}`"))
    }
}

/// `frame_ns <n>,<n>,...`: exactly `frames` times, each positive (ns).
fn judge_frames(lines: &[String], frames: u32) -> Result<Vec<u64>, String> {
    let line = lines
        .iter()
        .find_map(|l| l.strip_prefix("frame_ns "))
        .ok_or_else(|| no_line("frame_ns", lines))?;
    let ns = line
        .split(',')
        .map(str::parse::<u64>)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("unreadable frame_ns `{line}`: {e}"))?;
    if ns.len() == frames as usize && ns.iter().all(|&n| n > 0) {
        Ok(ns)
    } else {
        Err(format!(
            "{} frame times, want {frames}, all positive",
            ns.len()
        ))
    }
}

/// Median and p95 of the frame times, ms, by nearest rank.
fn perf_ms(ns: &[u64]) -> (f64, f64) {
    let mut sorted = ns.to_vec();
    sorted.sort_unstable();
    (
        nearest_rank(&sorted, 50) as f64 / NS_PER_MS,
        nearest_rank(&sorted, 95) as f64 / NS_PER_MS,
    )
}

/// The nearest-rank `percent` value of a non-empty sorted slice: 1-based rank ceil(percent x n / 100), no interpolation.
fn nearest_rank(sorted: &[u64], percent: usize) -> u64 {
    let rank = (percent * sorted.len()).div_ceil(100);
    sorted[rank.clamp(1, sorted.len()) - 1]
}

/// The 1, 5 and 15 minute load averages: `/proc/loadavg`, the figures `uptime` prints.
fn loadavg() -> Result<[f64; 3]> {
    let text = std::fs::read_to_string("/proc/loadavg").context("reading /proc/loadavg")?;
    parse_loadavg(&text)
}

fn parse_loadavg(text: &str) -> Result<[f64; 3]> {
    let words = text
        .split_whitespace()
        .take(3)
        .map(str::parse::<f64>)
        .collect::<Result<Vec<_>, _>>()
        .with_context(|| format!("parsing loadavg `{text}`"))?;
    <[f64; 3]>::try_from(words).map_err(|w| anyhow!("loadavg has {} fields, want 3", w.len()))
}

fn fmt_load(load: [f64; 3]) -> String {
    format!("{:.2} {:.2} {:.2}", load[0], load[1], load[2])
}

/// The DRM driver in use: `drm <node>=<modalias> ...`, the /dev/dri nodes the probe holds after eglInitialize. Mesa's Android platform opens one for every driver it loads, and with software rendering
/// forced it takes only vgem / virtio_gpu and loads kms_swrast on it (`platform_android.c` droid_probe_device); a pure software rasteriser holds none. So: at least one node, each with a known kernel driver.
fn judge_driver(lines: &[String]) -> Result<String, String> {
    let line = lines
        .iter()
        .find_map(|l| l.strip_prefix("drm "))
        .ok_or_else(|| no_line("drm", lines))?;
    let nodes: Vec<&str> = line.split_whitespace().filter(|n| *n != "none").collect();
    let unknown = nodes
        .iter()
        .any(|n| n.ends_with("=unknown") || n.ends_with("=unreadable"));
    match (nodes.is_empty(), unknown) {
        (true, _) => Err("no DRM node open after eglInitialize: not a DRI driver".to_owned()),
        (false, true) => Err(format!("kernel driver unknown: {line}")),
        (false, false) => Ok(format!("DRI driver on {line}")),
    }
}

/// `ext <EXTENSION> <v> glGetIntegerv <w> from <file>`: v == w > 0, and the address in a Mesa object.
fn judge_extension(lines: &[String]) -> Result<String, String> {
    let line = lines
        .iter()
        .find_map(|l| l.strip_prefix("ext "))
        .ok_or_else(|| no_line("ext", lines))?;
    let w: Vec<&str> = line.split(' ').collect();
    let [name, v, "glGetIntegerv", g, "from", file] = w[..] else {
        return Err(format!("unreadable line: ext {line}"));
    };
    let (v, g): (i64, i64) = (
        v.parse().map_err(|_| format!("ext value {v}"))?,
        g.parse().map_err(|_| format!("glGetIntegerv value {g}"))?,
    );
    if name != EXTENSION || v != g || g <= 0 {
        return Err(format!("{name} wrote {v}, glGetIntegerv gives {g}"));
    }
    if !file.contains("_mesa") && !file.contains("gallium") && !file.contains("glapi") {
        return Err(format!(
            "{name}'s address lies in {file}, not a Mesa object"
        ));
    }
    Ok(format!(
        "{name}(GL_MAX_TEXTURE_SIZE) = {v} = glGetIntegerv's, address in {file}"
    ))
}

fn no_line(what: &str, lines: &[String]) -> String {
    format!("no `{what}` line (probe said: {})", lines.join(" | "))
}

/// Function names (with `gl`) that Mesa's API XML in `dir` enables for ES2+: a `<function name="X" ... es2="v">` element.
fn es_names(dir: &Path) -> Result<Vec<String>> {
    let mut names = Vec::new();
    for e in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let p = e?.path();
        if p.extension().is_some_and(|x| x == "xml") {
            names.extend(es_functions(&std::fs::read_to_string(&p)?));
        }
    }
    Ok(names)
}

fn es_functions(xml: &str) -> Vec<String> {
    xml.split("<function ")
        .skip(1)
        .filter_map(|el| {
            let head = &el[..el.find('>')?];
            let name = head.split_once("name=\"")?.1.split('"').next()?;
            head.contains(" es2=\"").then(|| format!("gl{name}"))
        })
        .collect()
}

/// Mesa's dispatch names (`public_string_pool` of glapi_mapi_tmp.h, `"Name\0"` lines, prefixed `gl`) in `es`, minus every GL_ENTRY name of entries.in; sorted.
fn candidates(mapi_tmp: &str, es: &[String], entries_in: &str) -> Vec<String> {
    let pool = mapi_tmp
        .split("public_string_pool[] =")
        .nth(1)
        .and_then(|s| s.split(';').next())
        .unwrap_or("");
    let entries: std::collections::BTreeSet<&str> = entries_in
        .split("GL_ENTRY(")
        .skip(1)
        .filter_map(|e| e.split(',').nth(1).map(str::trim))
        .collect();
    let es: std::collections::BTreeSet<&str> = es.iter().map(String::as_str).collect();
    let mut out: Vec<String> = pool
        .lines()
        .filter_map(|l| {
            l.trim()
                .strip_prefix('"')?
                .strip_suffix("\\0\"")
                .map(|n| format!("gl{n}"))
        })
        .filter(|n| es.contains(n.as_str()) && !entries.contains(n.as_str()))
        .collect();
    out.sort();
    out.dedup();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(text: &[&str]) -> Vec<String> {
        text.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn driver_needs_a_known_drm_node() {
        assert!(judge_driver(&lines(&["drm /dev/dri/card0=platform:vgem"])).is_ok());
        assert!(judge_driver(&lines(&["drm none"])).is_err());
        assert!(judge_driver(&lines(&["drm /dev/dri/card0=unreadable"])).is_err());
        assert!(judge_driver(&lines(&["renderer x"])).is_err());
    }

    #[test]
    fn candidates_are_es_names_outside_entries_in() {
        let tmp = "x\nstatic const char public_string_pool[] =\n   \"Accum\\0\"\n   \"GetError\\0\"\n   \"GetInteger64vEXT\\0\";\n";
        let es = lines(&["glGetError", "glGetInteger64vEXT"]);
        let entries = "GL_ENTRY(GLenum, glGetError, void)\n";
        assert_eq!(candidates(tmp, &es, entries), vec!["glGetInteger64vEXT"]);
    }

    #[test]
    fn es_attribute_selects() {
        let xml =
            r#"<function name="A" es2="3.0"><x/></function><function name="B"><x/></function>"#;
        assert_eq!(es_functions(xml), vec!["glA"]);
    }

    #[test]
    fn pixels_need_one_value_everywhere() {
        let n = SIZE.0 * SIZE.1;
        let check = |text: String| judge_pixels(&lines(&[text.as_str()]), "pixels", CLEAR, n);
        assert!(check(format!("pixels 51,102,204,255x{n}")).is_ok());
        assert!(check(format!("pixels 0,0,0,0x{n}")).is_err());
        assert!(check(format!("pixels 0,0,0,0x1 51,102,204,255x{}", n - 1)).is_err());
        assert!(judge_pixels(&lines(&["renderer x"]), "pixels", CLEAR, n).is_err());
    }

    #[test]
    fn scene_needs_texel_everywhere_and_no_error() {
        let n = TIMED_SIZE.0 * TIMED_SIZE.1;
        assert!(judge_scene(&lines(&[&format!("scene 204,153,51,255x{n}")])).is_ok());
        // a frame that never drew reads back the clear colour
        assert!(judge_scene(&lines(&[&format!("scene 51,102,204,255x{n}")])).is_err());
        let shader = judge_scene(&lines(&[
            &format!("scene 204,153,51,255x{n}"),
            "scene-error shader: 0:1: syntax error",
        ]));
        assert!(shader.is_err_and(|e| e.contains("syntax error")));
    }

    #[test]
    fn renderer_prefix_selects_the_tier() {
        let soft = lines(&["renderer softpipe", "version OpenGL ES 3.2 Mesa"]);
        let llvm = lines(&["renderer llvmpipe (LLVM 20.0.0git, 256 bits)"]);
        assert!(judge_renderer(&soft, SOFTPIPE_RENDERER).is_ok());
        assert!(judge_renderer(&soft, RENDERER).is_err());
        assert!(judge_renderer(&llvm, RENDERER).is_ok());
        assert!(judge_renderer(&llvm, SOFTPIPE_RENDERER).is_err());
        assert!(judge_renderer(&lines(&["drm none"]), RENDERER).is_err());
    }

    #[test]
    fn renderer_prefix_names_the_pinned_llvm() {
        let major_minor = RENDERER
            .strip_prefix("llvmpipe (LLVM ")
            .expect("llvmpipe name");
        assert!(crate::llvm_host::VERSION.starts_with(major_minor));
    }

    #[test]
    fn frames_must_all_be_timed() {
        assert_eq!(
            judge_frames(&lines(&["frame_ns 1,2,3"]), 3),
            Ok(vec![1, 2, 3])
        );
        assert!(judge_frames(&lines(&["frame_ns 1,2,3"]), 4).is_err());
        assert!(judge_frames(&lines(&["frame_ns 1,0,3"]), 3).is_err());
        assert!(judge_frames(&lines(&["frame_ns 1,x,3"]), 3).is_err());
    }

    #[test]
    fn nearest_rank_is_the_ceiling_rank() {
        let one_to = |n: u64| (1..=n).collect::<Vec<u64>>();
        assert_eq!(nearest_rank(&one_to(5), 50), 3);
        assert_eq!(nearest_rank(&one_to(20), 50), 10);
        assert_eq!(nearest_rank(&one_to(20), 95), 19);
        // below 20 samples the p95 is the maximum
        assert_eq!(nearest_rank(&one_to(19), 95), 19);
        assert_eq!(nearest_rank(&[7], 95), 7);
    }

    #[test]
    fn loadavg_takes_three_fields() {
        assert_eq!(
            parse_loadavg("0.64 1.70 0.99 2/256 11879\n").unwrap(),
            [0.64, 1.70, 0.99]
        );
        assert!(parse_loadavg("1.00 2.00").is_err());
    }

    #[test]
    fn extension_needs_the_same_value_from_mesa() {
        let ok = "ext glGetInteger64vEXT 16384 glGetIntegerv 16384 from /vendor/lib64/egl/libGLESv2_mesa.so";
        assert!(judge_extension(&lines(&[ok])).is_ok());
        assert!(judge_extension(&lines(&["ext glGetInteger64vEXT -1 glGetIntegerv 16384 from /vendor/lib64/egl/libGLESv2_mesa.so"])).is_err());
        assert!(
            judge_extension(&lines(&[
                "ext glGetInteger64vEXT 16384 glGetIntegerv 16384 from /system/lib64/libEGL.so"
            ]))
            .is_err()
        );
    }
}
