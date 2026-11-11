// SPDX-License-Identifier: MIT
//! `xtask test gl`: GLES on the real driver in an app process -- AOSP's libEGL / libGLESv2 fronts (slow binding, patches 0009 + 0010) over `libGLES_atl.so` (the host's system EGL), placed as for an app (`Runtime::with_gl_driver`).
//! The fixture (xtask-harness/testdata/gl-probe) runs as a plain app process (`--run-class`): Probe.java makes a GLES3 pbuffer with the framework's EGL14 / GLES20, clears it, reads it back;
//! its JNI library (jni/, a cargo package of its own for `unsafe`, placed as [`LIB`] in /system's library dir) calls [`EXTENSION`] through eglGetProcAddress.
//! 0. elf: the driver has no undefined gl*/egl* dynamic symbol or a dynamic relocation naming one -- such a reference would bind to AOSP's fronts, which call back into the driver (recursion).
//!    Break: the probe library built with feature `undefined-gl` (imports glClear) must fail it.
//! 1. candidates: [`EXTENSION`] is not in AOSP's entries.in, so it reaches eglGetProcAddress's extension-slot path, where 0010 returns the driver's address (the host's, passed through) instead of the asm forwarder.
//! 2. pixels: every pixel of the [`SIZE`] pbuffer equals [`CLEAR`]: GL's float -> unorm8 conversion (ES 3.2 §2.3.5.2, round(f x 255)) of c / 255 gives c back, so the bytes are the expectation.
//! 3. extension: [`EXTENSION`](GL_MAX_TEXTURE_SIZE) writes the value glGetIntegerv (an entries.in function) gives, and the address lies in the host's GL, not in /system or /vendor (fronts, driver).
//! 4. device: the DRM nodes the probe holds after eglInitialize (its `drm` line) name the host EGL device it renders on ([`held_nodes`]). None held is the software device; a render node (`render` line) with none held fails as a silent fallback.
//! 5. renderer: `GL_RENDERER` is non-empty and equals the host EGL's for the same device (the probe's `host` line, [`judge_renderer`]). The host side is the probe's own query of the host's libEGL.so.1 (atl-egl-host), not AOSP's fronts or the driver: a bundled or stubbed GL answers a string the host does not.
//! 6. scene: the last frame of a timed scene ([`TIMED_SIZE`] pixels; [`FRAMES`] frames after one untimed warm-up, each clear + draw + glFinish) reads back [`TEXEL`] on every pixel.
//! 7. frames: one wall-clock time per timed frame. The gate prints their median and p95 (nearest rank) with the 1/5/15-minute load average before and after (`uptime`'s figures).
//!    A baseline only while the 1-minute load before the run is below [`QUIET_LOAD`]; the gate never fails on it.
//!
//! Then a deliberate break, every run: the probe skips glClear and the timed draws, calls a no-op in place of the extension, and mangles the host renderer; the pixel, scene, extension and renderer checks must fail.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, ensure};
use atl_layout::Layout;

use crate::atld_run::{Run, Runtime, Shell};
use crate::gl_scene;
use crate::{manifest, remote, tree};

#[derive(clap::Args)]
pub struct Args {
    /// The host arch (an arches.toml row with `[host]`): the default --out.
    #[arg(long)]
    host: String,
    /// Native build directory holding ART, the services and the fronts (default: this checkout's slot, see `xtask slot`; else <workspace>/out/native/<host>).
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

const FIXTURE: &str = "xtask-harness/testdata/gl-probe";
const PROBE_CLASS: &str = "com.example.atlgl.Probe";
const PROBE_NAME: &str = "gl-probe";
/// An app uid (AID_APP_START + 124): the probe runs as an app.
const APP_UID: u32 = 10_124;
/// The JNI library; the sandbox sees it in atl_android_paths::SYSTEM_LIB (atld `--file`).
const LIB: &str = "libatl_gl_probe.so";
/// The pbuffer's size and clear colour (bytes): odd sizes so no row or tile alignment hides a stray pixel; each channel a k / 255 value away from 0 and 255.
const SIZE: (u32, u32) = (67, 33);
const CLEAR: [u8; 4] = [51, 102, 204, 255];
/// The ES1 scene, read by [`load_scene`].
const SCENE: &str = "es1-scene.toml";
/// The timed scene's surface. A multiple of the grid size (Probe.java `GRID`), so the cell edges fall on pixel boundaries and no pixel centre sits on one.
const TIMED_SIZE: (u32, u32) = (256, 256);
/// The scene's texel: every scene pixel must read it back. Its fragment loop sums 64 terms of texel / 64, exact up to float rounding, which the unorm8 conversion absorbs.
const TEXEL: [u8; 4] = [204, 153, 51, 255];
/// Timed frames after one untimed warm-up. Each frame's time is one sample; a longer run only lengthens LIMIT's share.
const FRAMES: u32 = 60;
/// Nearest-rank p95 of n samples is the maximum while n < 20 (rank ceil(0.95 n) = n); from 20 on it is not.
const P95_MIN_FRAMES: u32 = 20;
/// Median and p95 are a baseline only while the 1-minute load is below this: an unconfirmed first guess (a quarter of this host's 16 threads). Reported as `quiet`, never a failure.
const QUIET_LOAD: f64 = 4.0;
const NS_PER_MS: f64 = 1_000_000.0;
/// In Mesa's ES dispatch, not in entries.in; observable through a query whose answer an entries.in call also gives.
const EXTENSION: &str = "glGetInteger64vEXT";
/// AOSP's entries.in.
const ENTRIES_IN: &str = "frameworks/native/opengl/libs/entries.in";
/// Each probe run's time limit: the child is killed past it. A remote run gets the same limit as `timeout` on the target (remote.rs).
pub const LIMIT: Duration = Duration::from_secs(120);
const POLL: Duration = Duration::from_millis(50);

/// The ES1 scene, read by the guest fixture's generator (jni_fixture.rs) and the app gate (app.rs): one description.
pub fn load_scene(root: &Path) -> Result<gl_scene::Scene> {
    gl_scene::Scene::load(&root.join(FIXTURE).join(SCENE))
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
    .with_gl_driver(root)?
    .with_site(site);
    let tree = tree::shared_tree(&layout);
    let mut ok = !entry_names(&std::fs::read_to_string(tree.join(ENTRIES_IN))?)
        .iter()
        .any(|n| n == EXTENSION);
    println!(
        "gl: {} candidates: {EXTENSION} {} in entries.in, so it reaches eglGetProcAddress's extension path",
        verdict(ok),
        if ok { "is not" } else { "IS" }
    );
    let work = rt.out.join("gl");
    let driver = rt
        .out
        .join("egl")
        .join(crate::egl_driver::atl_egl_driver_name());
    let found = gl_imports(&driver)?;
    ok &= found.is_empty();
    println!(
        "gl: {} elf: {}: {}",
        verdict(found.is_empty()),
        driver.display(),
        if found.is_empty() {
            "no undefined gl*/egl* symbol, no dynamic relocation naming one (none binds back to AOSP's fronts)".to_owned()
        } else {
            found.join("; ")
        }
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
    // the normal run's frame times, its renderer, and the load average around it
    let mut timed = None;
    for broken in [false, true] {
        let tag = if broken { "break" } else { "run" };
        let mut shell_args: Vec<String> =
            vec![lib_at.clone(), SIZE.0.to_string(), SIZE.1.to_string()];
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
        let renderer = judge_renderer(&lines);
        if broken {
            // the break passes when every check fires
            for (what, r) in [
                ("pixels", &pixels),
                ("extension", &ext),
                ("scene", &scene_pixels),
                ("renderer", &renderer),
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
        let device = held_nodes(&lines).map(|held| {
            if held.is_empty() {
                "software device (none held)".to_owned()
            } else {
                format!("held {}", held.join(" "))
            }
        });
        let timing = judge_frames(&lines, args.frames);
        let timing_check = timing
            .as_ref()
            .map(|ns| format!("{} frame times, all positive", ns.len()))
            .map_err(String::clone);
        for (what, r) in [
            ("pixels", &pixels),
            ("extension", &ext),
            ("device", &device),
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
        let name = renderer_name(&lines)
            .ok()
            .filter(|n| !n.is_empty())
            .unwrap_or("unknown")
            .to_owned();
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

fn first_line<'a>(lines: &'a [String], prefix: &str) -> Option<&'a str> {
    lines.iter().find_map(|l| l.strip_prefix(prefix))
}

fn verdict(ok: bool) -> &'static str {
    if ok { "pass" } else { "FAIL" }
}

/// Builds the JNI library into `target`; returns the .so.
fn build_lib(root: &Path, target: &Path, features: &[&str]) -> Result<PathBuf> {
    let status = crate::unit_use::status(
        Command::new(env!("CARGO"))
            .args(["build", "--release", "--quiet", "--manifest-path"])
            .arg(root.join(FIXTURE).join("jni/Cargo.toml"))
            .arg(format!("--features={}", features.join(",")))
            .arg("--target-dir")
            .arg(target),
    )
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

/// The text of the probe's `renderer <GL_RENDERER>` line; empty when GL gave none.
fn renderer_name(lines: &[String]) -> Result<&str, String> {
    lines
        .iter()
        .find_map(|l| match l.as_str() {
            "renderer" => Some(""),
            text => text.strip_prefix("renderer "),
        })
        .ok_or_else(|| no_line("renderer", lines))
}

/// The app's GL_RENDERER is non-empty and equals the host EGL's renderer for the same device: the probe's `host` line, `ok <device> <renderer>` or `err <message>`.
fn judge_renderer(lines: &[String]) -> Result<String, String> {
    let app = renderer_name(lines)?;
    if app.is_empty() {
        return Err("GL_RENDERER is empty".to_owned());
    }
    let host = first_line(lines, "host ").ok_or_else(|| no_line("host", lines))?;
    if let Some(err) = host.strip_prefix("err ") {
        return Err(format!("host EGL query failed: {err}"));
    }
    let (device, host_name) = host
        .strip_prefix("ok ")
        .and_then(|rest| rest.split_once(' '))
        .ok_or_else(|| format!("unreadable host line `{host}`"))?;
    if host_name.is_empty() {
        return Err(format!("host EGL renderer on {device} is empty"));
    }
    if app == host_name {
        Ok(format!("`{app}` = host EGL on {device}"))
    } else {
        Err(format!(
            "app renderer `{app}` != host EGL renderer `{host_name}` on {device}"
        ))
    }
}

/// The DRM nodes the probe holds after eglInitialize (`drm` line): the device the host EGL renders on; none held is the software device.
/// A render node (`render` line) with none held is a silent fallback to software, so it fails.
fn held_nodes(lines: &[String]) -> Result<Vec<&str>, String> {
    let drm = first_line(lines, "drm ").ok_or_else(|| no_line("drm", lines))?;
    let render = first_line(lines, "render ").ok_or_else(|| no_line("render", lines))?;
    let mut held: Vec<&str> = drm.split_whitespace().filter(|e| *e != "none").collect();
    let present: Vec<&str> = render.split_whitespace().filter(|e| *e != "none").collect();
    if held.is_empty() && !present.is_empty() {
        return Err(format!(
            "render node {} present but no DRM node held after eglInitialize: silent fallback to the software device",
            present.join(" ")
        ));
    }
    // two fds on one node (the `drm` line lists each fd) are one node
    held.sort_unstable();
    held.dedup();
    Ok(held)
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

/// `ext <EXTENSION> <v> glGetIntegerv <w> from <file>`: v == w > 0, and the address in the host's GL.
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
    // libGLES_atl passes non-own names to the host's eglGetProcAddress: the address is the host GL's, never an AOSP front's or the driver's own
    let in_android = |root: &str| Path::new(file).starts_with(root);
    if in_android(atl_android_paths::SYSTEM) || in_android(atl_android_paths::VENDOR) {
        return Err(format!(
            "{name}'s address lies in {file}, not the host's GL"
        ));
    }
    Ok(format!(
        "{name}(GL_MAX_TEXTURE_SIZE) = {v} = glGetIntegerv's, address in {file}"
    ))
}

fn no_line(what: &str, lines: &[String]) -> String {
    format!("no `{what}` line (probe said: {})", lines.join(" | "))
}

/// Every GL_ENTRY name of entries.in (`GL_ENTRY(ret, name, args)`): the names AOSP's fronts dispatch themselves; any other reaches eglGetProcAddress's extension path.
fn entry_names(entries_in: &str) -> Vec<String> {
    entries_in
        .split("GL_ENTRY(")
        .skip(1)
        .filter_map(|e| e.split(',').nth(1).map(|n| n.trim().to_owned()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(text: &[&str]) -> Vec<String> {
        text.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn entry_names_read_the_second_field() {
        let entries =
            "GL_ENTRY(GLenum, glGetError, void)\nGL_ENTRY(void, glClear, GLbitfield mask)\n";
        assert_eq!(entry_names(entries), vec!["glGetError", "glClear"]);
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

    /// `held_nodes` on one `drm` line and one `render` line: the held nodes, or the error.
    fn held(drm: &str, render: &str) -> Result<Vec<String>, String> {
        let drm = format!("drm {drm}");
        let render = format!("render {render}");
        held_nodes(&lines(&[drm.as_str(), render.as_str()]))
            .map(|h| h.into_iter().map(str::to_owned).collect())
    }

    #[test]
    fn held_node_names_the_device() {
        let node = "/dev/dri/renderD128";
        assert_eq!(held(node, node), Ok(vec![node.to_owned()]));
        assert_eq!(
            held("/dev/dri/card0 /dev/dri/card0", "none"),
            Ok(vec!["/dev/dri/card0".to_owned()])
        );
        // gralloc's card node is held as well: the probe reports every held node, the host EGL picks the device
        assert_eq!(
            held("/dev/dri/card0 /dev/dri/renderD128", node).map(|h| h.len()),
            Ok(2)
        );
    }

    #[test]
    fn none_held_and_no_render_node_is_the_software_device() {
        assert_eq!(held("none", "none"), Ok(Vec::new()));
    }

    #[test]
    fn render_node_with_none_held_fails_as_a_silent_fallback() {
        let err = held("none", "/dev/dri/renderD128")
            .err()
            .unwrap_or_default();
        assert!(
            err.contains("renderD128") && err.contains("silent fallback"),
            "{err}"
        );
    }

    #[test]
    fn missing_probe_line_fails() {
        assert!(held_nodes(&lines(&["render none"])).is_err());
        assert!(held_nodes(&lines(&["drm none"])).is_err());
    }

    #[test]
    fn renderer_must_be_non_empty() {
        let host = "host ok software llvmpipe (LLVM 20.1.0, 256 bits)";
        assert!(judge_renderer(&lines(&["renderer ", host])).is_err());
        assert!(judge_renderer(&lines(&["renderer", host])).is_err());
        assert!(judge_renderer(&lines(&[host])).is_err());
    }

    #[test]
    fn renderer_must_equal_the_host_renderer_naming_both() {
        let app = "Mesa Intel(R) Graphics (LNL)";
        let host = format!("host ok /dev/dri/renderD128 {app}");
        assert!(judge_renderer(&lines(&[&format!("renderer {app}"), &host])).is_ok());
        // the every-run break's shape: the host renderer mangled, the app's untouched
        let mangled = format!("{host} (mangled host)");
        let err = judge_renderer(&lines(&[&format!("renderer {app}"), &mangled]))
            .err()
            .unwrap_or_default();
        assert!(
            err.contains(&format!("`{app}`")) && err.contains(&format!("`{app} (mangled host)`")),
            "{err}"
        );
    }

    #[test]
    fn failed_or_empty_host_query_fails() {
        let renderer = "renderer llvmpipe (LLVM 20.1.0, 256 bits)";
        let err = judge_renderer(&lines(&[
            renderer,
            "host err no EGL device for `/dev/dri/renderD128`",
        ]))
        .err()
        .unwrap_or_default();
        assert!(err.contains("no EGL device"), "{err}");
        assert!(judge_renderer(&lines(&[renderer, "host ok software "])).is_err());
        assert!(judge_renderer(&lines(&[renderer, "host ok software"])).is_err());
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
    fn extension_needs_the_same_value_from_the_host() {
        let ok = "ext glGetInteger64vEXT 16384 glGetIntegerv 16384 from /usr/lib/x86_64-linux-gnu/libGLdispatch.so.0";
        assert!(judge_extension(&lines(&[ok])).is_ok());
        assert!(judge_extension(&lines(&["ext glGetInteger64vEXT -1 glGetIntegerv 16384 from /usr/lib/x86_64-linux-gnu/libGLdispatch.so.0"])).is_err());
        assert!(judge_extension(&lines(&["ext glGetInteger64vEXT 16384 glGetIntegerv 16384 from /vendor/lib64/egl/libGLES_atl.so"])).is_err());
        assert!(
            judge_extension(&lines(&[
                "ext glGetInteger64vEXT 16384 glGetIntegerv 16384 from /system/lib64/libEGL.so"
            ]))
            .is_err()
        );
    }
}
