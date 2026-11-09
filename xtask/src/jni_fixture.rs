// SPDX-License-Identifier: MIT
//! `xtask test jni-fixture`: the bridge-1 JNI fixture (xtask/testdata/jni-test), a stand-in for third-party app native code, built with the NDK's clang against bionic + libc++_shared exactly as an app's CMake/ndk-build would.
//! Two variants, because the TLS model depends on minSdk: [`EMULATED_TLS_API`] compiles `__thread` to emulated TLS (`__emutls_get_address` import, no PT_TLS), [`ELF_TLS_API`] (the first level with ELF TLS in bionic) to native TLS (`__tls_get_addr`, PT_TLS).
//! The M2 test APK (`xtask test app --fixture-api`) packages either variant into `lib/<abi>/`, its API level as the APK's minSdk.
//!
//! Gate, per variant:
//! - ELF: DT_NEEDED has `libc.so` (bionic's soname; glibc's is `libc.so.6`) and `libc++_shared.so`, the GNU verneed asks `libc.so` for version `LIBC`, the TLS shape is the variant's;
//! - glibc probe: host ld.so loading the fixture (LD_PRELOAD into a trivial host program; `dlopen(3)` would need unsafe, which xtask forbids, and preload maps through the same `_dl_map_object` path) must fail on the bionic ABI: `version `LIBC' not found`.
//!   The probe's library path maps the bionic sonames (`libc.so`, `libm.so`, `libdl.so`) to glibc's, so the load gets past name resolution (unmapped, glibc picks up `libm.so`, the GNU ld script, and fails for an unrelated reason) and reaches version binding, the failure bridge 1 removes.
//! - controls, so a pass means something: the probe loads a glibc library cleanly (it responds), and each variant fails the other variant's TLS expectation (the TLS check discriminates).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use anyhow::{Context, Result, bail, ensure};
use atl_layout::Layout;
use object::read::elf::{ElfFile64, FileHeader, ProgramHeader};
use object::{Endianness, Object, ObjectSymbol, elf};

use crate::atld_run::NativeLibs;
use crate::gl_scene::Scene;
use crate::{arches, gl_probe, host_art, provided_gate};

#[derive(clap::Args)]
pub struct Args {
    /// The fixture's arch (an arches.toml row with `[guest]`); its first `android_abis` entry names the APK lib dir.
    #[arg(long)]
    arch: String,
    /// NDK root (default: <workspace>/tools/[`NDK_DIR`]).
    #[arg(long)]
    ndk: Option<PathBuf>,
    /// Output directory (default: <workspace>/out/jni-fixture).
    #[arg(long)]
    out: Option<PathBuf>,
}

/// The NDK release the fixture is built with (owner decision D1, bridge-1 plan); `--ndk` overrides.
const NDK_DIR: &str = "android-ndk-r30";
/// The fixture's sources.
const SOURCES: &str = "xtask/testdata/jni-test";
/// The emulated-TLS variant: bionic had no ELF TLS before 29, so clang emits emulated TLS below it.
pub const EMULATED_TLS_API: u32 = 21;
/// First API level whose bionic supports ELF TLS (Android 10); clang's default TLS model switches to native at this target.
pub const ELF_TLS_API: u32 = 29;
/// The library `System.loadLibrary("atl-jni-test")` maps to, and the sibling it dlopens by name.
const LIB: &str = "libatl-jni-test.so";
const SIBLING: &str = "libatl-jni-sibling.so";
/// libc++ the fixture links (`-stdlib=libc++`, NDK clang++'s shared default); packaged next to the fixture as an app packages it.
const CXX_SHARED: &str = "libc++_shared.so";
/// The C12 legacy fixture (atl_jni_legacy.cpp): C and C++ calls old NDK binaries make, linked to the guest's libstdc++.so and libz.so instead of libc++.
/// Its libc is an empty unversioned stub, as old NDK platforms shipped: no GNU verneed, imports left undefined for the loader (Happy Jump's libndksetup.so has this shape).
const LEGACY: &str = "libatl-jni-legacy.so";
/// DT_NEEDED the legacy fixture names: the guest libraries that answer its calls (atld_run.rs NB_LIBS rows).
const LEGACY_NEEDED: [&str; 2] = ["libstdc++.so", "libz.so"];
/// Imports the legacy fixture carries, each one a call its source makes at run time: a folded call drops its import, and the check fails.
const LEGACY_IMPORTS: [&str; 9] = [
    "__sF",
    "_ctype_",
    "__strlen_chk",
    "_Znwm",
    "_ZdlPv",
    "__cxa_guard_acquire",
    "crc32",
    "compress",
    "uncompress",
];

/// The breaks selected by `ATL_JNI_BREAK` on the run that builds the fixture.
/// `seek` moves the asset probe's seek one byte off (atl_jni_test.c `ASSET_SEEK_TO`): the tail's hash and remaining length go red.
/// `es1` drops the ES1 scene's breakable steps, glPopMatrix and the COLOR_ARRAY enable (es1-scene.toml `break`): the rows go red.
const BREAK_ENV: &str = "ATL_JNI_BREAK";
const BREAKS: [&str; 2] = ["seek", "es1"];
/// The break `ATL_JNI_BREAK` selects, if any; an unknown name fails, naming the known ones.
fn selected_break() -> Result<Option<&'static str>> {
    match std::env::var(BREAK_ENV) {
        Err(_) => Ok(None),
        Ok(v) => match BREAKS.iter().copied().find(|&b| b == v) {
            Some(b) => Ok(Some(b)),
            None => bail!(
                "{BREAK_ENV}={v}: no such break (known: {})",
                BREAKS.join(", ")
            ),
        },
    }
}

/// The C3 probes' libraries (atl_jni_test.c `assetProbe` / `bitmapProbe`): DT_NEEDED names the NDK libraries, and each import is one the probes call.
const NDK_NEEDED: [&str; 2] = ["libandroid.so", "libjnigraphics.so"];
const NDK_IMPORTS: [&str; 11] = [
    "AAssetManager_fromJava",
    "AAssetManager_open",
    "AAsset_read",
    "AAsset_seek",
    "AAsset_getRemainingLength",
    "AAsset_getBuffer",
    "AAsset_getLength",
    "AAsset_close",
    "AndroidBitmap_getInfo",
    "AndroidBitmap_lockPixels",
    "AndroidBitmap_unlockPixels",
];

/// The GLES1 side (atl_jni_es1.c): its own translation unit, its GLES1 headers do not share one with the GLES2 ones.
const ES1_SOURCE: &str = "atl_jni_es1.c";
/// The ES1 scene's generated C (`es1_c`), in the fixture's out dir: ES1_SOURCE includes it.
const ES1_HEADER: &str = "es1_scene.inc";
/// The GLES1 proxy the scene calls (`-lGLESv1_CM`): DT_NEEDED of the fixture, checked by `es1_failures`.
const ES1_LIB: &str = "libGLESv1_CM.so";

/// One built variant: its API level and the libraries an APK carries for it.
pub struct Built {
    pub api: u32,
    pub dir: PathBuf,
    pub libs: Vec<PathBuf>,
}

/// The NDK's llvm prebuilt dir for this build machine.
pub fn toolchain(ndk: &Path) -> PathBuf {
    ndk.join("toolchains/llvm/prebuilt/linux-x86_64")
}

/// The default NDK root.
pub fn default_ndk(root: &Path) -> Result<PathBuf> {
    Ok(Layout::of(root)?.workspace().join("tools").join(NDK_DIR))
}

/// NDK clang for one guest arch at one API level, as an app's CMake/ndk-build drives it: `--target=<arches.toml clang triple><api>`.
pub struct NdkTarget {
    bin: PathBuf,
    target: String,
    /// `<triple><api>-clang` in the NDK's bin: the same compiler as `cmd("clang")`, as a wrapper that names its target.
    wrapper: PathBuf,
    /// The arch's `lib/<abi>/` name in an APK: its first arches.toml `android_abis` entry.
    pub abi: String,
    /// The NDK sysroot's per-triple lib dir (libc++_shared.so).
    sysroot_lib: PathBuf,
}

/// Compiler flags of every fixture object: PIC, hidden by default (exports are JNIEXPORT), warnings fatal.
pub const CFLAGS: &[&str] = &[
    "-fPIC",
    "-O2",
    "-fvisibility=hidden",
    "-Wall",
    "-Wextra",
    "-Werror",
    "-Wno-unused-parameter",
];

impl NdkTarget {
    pub fn new(root: &Path, ndk: &Path, arch: &str, api: u32) -> Result<NdkTarget> {
        let arches = arches::load(root)?;
        let guest = arches
            .get(arch)
            .and_then(|a| a.guest.as_ref())
            .with_context(|| format!("arches.toml has no [{arch}.guest] row"))?;
        let tc = toolchain(ndk);
        Ok(NdkTarget {
            bin: tc.join("bin"),
            target: format!("--target={}{api}", guest.clang),
            wrapper: tc.join("bin").join(format!("{}{api}-clang", guest.clang)),
            abi: guest
                .android_abis
                .first()
                .cloned()
                .with_context(|| format!("arches.toml [{arch}.guest] has no android_abis"))?,
            sysroot_lib: tc.join("sysroot/usr/lib").join(&guest.clang),
        })
    }

    /// `tool` (`clang` / `clang++`) with the target set.
    pub fn cmd(&self, tool: &str) -> Command {
        let mut c = Command::new(self.bin.join(tool));
        c.arg(&self.target);
        c
    }

    /// The per-API clang wrapper (`<triple><api>-clang`): a compiler that needs no `--target` argument, for tools that take a bare compiler path.
    pub fn wrapper(&self) -> &Path {
        &self.wrapper
    }
}

/// Builds the fixture for `arch` at `api` into `out/android-<api>`.
/// `legacy_no_libc`: the legacy library loses DT_NEEDED `libc.so`, the marker the NativeBridge core routes on (`xtask test app --legacy-no-libc`).
pub fn build(
    root: &Path,
    ndk: &Path,
    arch: &str,
    api: u32,
    out: &Path,
    legacy_no_libc: bool,
) -> Result<Built> {
    let t = NdkTarget::new(root, ndk, arch, api)?;
    let dir = out.join(format!("android-{api}"));
    if dir.exists() {
        std::fs::remove_dir_all(&dir)?;
    }
    std::fs::create_dir_all(&dir)?;
    let src = root.join(SOURCES);
    let compile = |cc: &str, out_obj: &Path, file: &str, extra: &[&str]| {
        host_art::exec(
            t.cmd(cc)
                .arg("-c")
                .args(CFLAGS)
                .args(extra)
                .arg(src.join(file))
                .arg("-o")
                .arg(out_obj),
            &format!("NDK {cc} {file}"),
        )
    };
    let sibling = dir.join(SIBLING);
    host_art::exec(
        t.cmd("clang")
            .arg("-shared")
            .args(CFLAGS)
            .arg(src.join("atl_jni_sibling.c"))
            .arg(format!("-Wl,-soname,{SIBLING}"))
            .arg("-o")
            .arg(&sibling),
        "NDK clang sibling",
    )?;
    let c_obj = dir.join("atl_jni_test.o");
    let cxx_obj = dir.join("atl_jni_cxx.o");
    let es1_obj = dir.join("atl_jni_es1.o");
    let brk = selected_break()?;
    let mut c_flags = vec!["-std=c11"];
    if brk == Some("seek") {
        c_flags.push("-DATL_JNI_SEEK_BREAK=1");
    }
    compile("clang", &c_obj, "atl_jni_test.c", &c_flags)?;
    compile("clang++", &cxx_obj, "atl_jni_cxx.cpp", &["-std=c++17"])?;
    std::fs::write(
        dir.join(ES1_HEADER),
        es1_c(&gl_probe::load_scene(root)?, brk == Some("es1"))?,
    )?;
    let include = format!("-I{}", dir.display());
    compile(
        "clang",
        &es1_obj,
        ES1_SOURCE,
        &["-std=c11", include.as_str()],
    )?;
    let lib = dir.join(LIB);
    host_art::exec(
        t.cmd("clang++")
            .args(["-shared", "-static-libgcc"])
            .arg(&c_obj)
            .arg(&cxx_obj)
            .arg(&es1_obj)
            .arg(format!("-Wl,-soname,{LIB}"))
            .arg("-llog")
            .arg("-lEGL")
            .arg("-lGLESv2")
            .arg("-lGLESv1_CM")
            .arg("-landroid")
            .arg("-ljnigraphics")
            .arg("-o")
            .arg(&lib),
        "NDK clang++ link",
    )?;
    std::fs::remove_file(&c_obj)?;
    std::fs::remove_file(&cxx_obj)?;
    std::fs::remove_file(&es1_obj)?;
    let legacy = dir.join(LEGACY);
    let legacy_obj = dir.join("atl_jni_legacy.o");
    // Old NDK default: C++11 without sized operator delete. The guest's libstdc++ has no `_ZdlPvm` (bionic/libstdc++/include/new: TODO), so clang's C++14 default would make a modern-ABI import this fixture must not carry.
    compile(
        "clang++",
        &legacy_obj,
        "atl_jni_legacy.cpp",
        &[
            "-std=c++11",
            "-fno-sized-deallocation",
            "-D_FORTIFY_SOURCE=2",
        ],
    )?;
    let stubs = dir.join("legacy-stubs");
    std::fs::create_dir_all(&stubs)?;
    host_art::exec(
        t.cmd("clang")
            .args(["-shared", "-nostdlib", "-Wl,-soname,libc.so"])
            .args(["-x", "c", "/dev/null", "-o"])
            .arg(stubs.join("libc.so")),
        "NDK clang legacy libc stub",
    )?;
    // the stub defines nothing, so --as-needed drops it from DT_NEEDED
    let libc_needed = if legacy_no_libc {
        "-Wl,--as-needed"
    } else {
        "-Wl,--no-as-needed"
    };
    host_art::exec(
        t.cmd("clang++")
            .args(["-shared", "-nostdlib++", libc_needed])
            .arg(format!("-L{}", stubs.display()))
            .arg(&legacy_obj)
            .arg(format!("-Wl,-soname,{LEGACY}"))
            .args(["-lstdc++", "-lz"])
            .arg("-o")
            .arg(&legacy),
        "NDK clang++ legacy link",
    )?;
    std::fs::remove_file(&legacy_obj)?;
    std::fs::remove_dir_all(&stubs)?;
    let cxx = t.sysroot_lib.join(CXX_SHARED);
    let cxx_copy = dir.join(CXX_SHARED);
    std::fs::copy(&cxx, &cxx_copy).with_context(|| format!("copying {}", cxx.display()))?;
    Ok(Built {
        api,
        dir,
        libs: vec![lib, sibling, cxx_copy, legacy],
    })
}

/// The ES1 scene as C (`ES1_HEADER`): `es1_c` of the scene, `broken` = the `es1` break.
fn es1_c(scene: &Scene, broken: bool) -> Result<String> {
    let body = c_program(scene.size(), &scene.program(broken))?;
    Ok(format!(
        "/* generated by xtask (jni_fixture.rs) from es1-scene.toml, break es1: {broken}, do not edit */\n{body}"
    ))
}

/// C for one scene program (`Scene::program`): `es1_program(px)` makes its calls in order, and `read` ends in glReadPixels into `px`.
/// `resolve` makes no call: the scene's own calls name the functions, and DT_NEEDED `libGLESv1_CM.so` supplies them (`es1_failures`).
fn c_program(size: (usize, usize), program: &[String]) -> Result<String> {
    let mut out = format!("#define ES1_W {}\n#define ES1_H {}\n", size.0, size.1);
    out.push_str("static void es1_program(GLubyte *px)\n{\n");
    let mut arrays = 0;
    for line in program {
        for stmt in c_statements(line, size, &mut arrays)? {
            out.push_str("    ");
            out.push_str(&stmt);
            out.push('\n');
        }
    }
    out.push_str("}\n");
    Ok(out)
}

/// The C statements of one program line. An array step declares `static const <type> a<n>[]` and points GL at it; `arrays` counts them.
fn c_statements(line: &str, size: (usize, usize), arrays: &mut usize) -> Result<Vec<String>> {
    let t: Vec<&str> = line.split_whitespace().collect();
    let Some((op, a)) = t.split_first() else {
        bail!("empty program line");
    };
    let stmts = match *op {
        "clear" => {
            let c = channels(op, a, 4)?;
            vec![
                format!(
                    "glClearColor({} / 255.0f, {} / 255.0f, {} / 255.0f, {} / 255.0f);",
                    c[0], c[1], c[2], c[3]
                ),
                "glClear(GL_COLOR_BUFFER_BIT);".to_owned(),
            ]
        }
        "color" => {
            let c = channels(op, a, 4)?;
            vec![format!(
                "glColor4ub({}, {}, {}, {});",
                c[0], c[1], c[2], c[3]
            )]
        }
        "enable" | "disable" | "client_enable" | "client_disable" => {
            let n = gl_names(op, a, 1)?;
            let f = match *op {
                "enable" => "glEnable",
                "disable" => "glDisable",
                "client_enable" => "glEnableClientState",
                _ => "glDisableClientState",
            };
            vec![format!("{f}(GL_{});", n[0])]
        }
        "scissor" => {
            let v = arity(op, a, 4)?;
            vec![format!(
                "glScissor({}, {}, {}, {});",
                v[0], v[1], v[2], v[3]
            )]
        }
        "matrix" => {
            let n = gl_names(op, a, 1)?;
            ensure!(
                matches!(n[0], "MODELVIEW" | "PROJECTION"),
                "matrix mode {}",
                n[0]
            );
            vec![format!("glMatrixMode(GL_{});", n[0])]
        }
        "load_identity" | "push" | "pop" => {
            ensure!(a.is_empty(), "{op} takes no numbers");
            vec![
                match *op {
                    "load_identity" => "glLoadIdentity();",
                    "push" => "glPushMatrix();",
                    _ => "glPopMatrix();",
                }
                .to_owned(),
            ]
        }
        "ortho" => {
            let v = arity(op, a, 6)?;
            vec![format!(
                "glOrthof({}, {}, {}, {}, {}, {});",
                v[0], v[1], v[2], v[3], v[4], v[5]
            )]
        }
        "translate" => {
            let v = arity(op, a, 3)?;
            vec![format!("glTranslatef({}, {}, {});", v[0], v[1], v[2])]
        }
        "scale" => {
            let v = arity(op, a, 3)?;
            vec![format!("glScalef({}, {}, {});", v[0], v[1], v[2])]
        }
        "rotate" => {
            let v = arity(op, a, 4)?;
            vec![format!(
                "glRotatef({}, {}, {}, {});",
                v[0], v[1], v[2], v[3]
            )]
        }
        "vertices" => {
            let v = nums(a)?;
            ensure!(v.len().is_multiple_of(2), "vertices come in pairs");
            let (decl, name) = static_array(arrays, "GLfloat", &v);
            vec![decl, format!("glVertexPointer(2, GL_FLOAT, 0, {name});")]
        }
        "colors" => {
            let v = whole(a, 255.0)?;
            ensure!(v.len().is_multiple_of(4), "colors come in fours");
            let (decl, name) = static_array(arrays, "GLubyte", &v);
            vec![
                decl,
                format!("glColorPointer(4, GL_UNSIGNED_BYTE, 0, {name});"),
            ]
        }
        "texcoords" => {
            let v = nums(a)?;
            ensure!(v.len().is_multiple_of(2), "texcoords come in pairs");
            let (decl, name) = static_array(arrays, "GLfloat", &v);
            vec![decl, format!("glTexCoordPointer(2, GL_FLOAT, 0, {name});")]
        }
        "point_sizes" => {
            let v = nums(a)?;
            ensure!(!v.is_empty(), "point_sizes takes sizes");
            let (decl, name) = static_array(arrays, "GLfloat", &v);
            vec![decl, format!("glPointSizePointerOES(GL_FLOAT, 0, {name});")]
        }
        "elements" => {
            let v = whole(a, f64::from(u16::MAX))?;
            ensure!(
                !v.is_empty() && v.len().is_multiple_of(3),
                "{} indices are not triangles",
                v.len()
            );
            let (decl, name) = static_array(arrays, "GLushort", &v);
            vec![
                decl,
                format!(
                    "glDrawElements(GL_TRIANGLES, {}, GL_UNSIGNED_SHORT, {name});",
                    v.len()
                ),
            ]
        }
        "points" => {
            let v = whole(a, f64::from(u16::MAX))?;
            ensure!(v.len() == 2, "points takes FIRST COUNT");
            vec![format!("glDrawArrays(GL_POINTS, {}, {});", v[0], v[1])]
        }
        "blend_func" => {
            let n = gl_names(op, a, 2)?;
            vec![format!("glBlendFunc(GL_{}, GL_{});", n[0], n[1])]
        }
        "bind_texture" => {
            let v = whole(a, f64::from(u16::MAX))?;
            ensure!(v.len() == 1, "bind_texture takes one name");
            vec![format!("glBindTexture(GL_TEXTURE_2D, {});", v[0])]
        }
        "texture" => {
            ensure!(a.len() >= 2, "texture takes W H and RGBA bytes");
            let wh = whole(&a[..2], f64::from(u16::MAX))?;
            let (w, h) = (wh[0], wh[1]);
            let data = whole(&a[2..], 255.0)?;
            ensure!(
                data.len() as f64 == 4.0 * w * h,
                "texture {w}x{h} takes {} bytes, got {}",
                4.0 * w * h,
                data.len()
            );
            let (decl, name) = static_array(arrays, "GLubyte", &data);
            vec![
                decl,
                format!(
                    "glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA, {w}, {h}, 0, GL_RGBA, GL_UNSIGNED_BYTE, {name});"
                ),
            ]
        }
        "texture_filter" => {
            let n = gl_names(op, a, 1)?;
            ensure!(n[0] == "NEAREST", "texture_filter {}: NEAREST only", n[0]);
            vec![
                "glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);".to_owned(),
                "glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);".to_owned(),
            ]
        }
        "read" => {
            let v = whole(a, f64::from(u16::MAX))?;
            ensure!(
                v == [size.0 as f64, size.1 as f64],
                "read takes the surface, {}x{}",
                size.0,
                size.1
            );
            vec![format!(
                "glReadPixels(0, 0, {}, {}, GL_RGBA, GL_UNSIGNED_BYTE, px);",
                size.0, size.1
            )]
        }
        "resolve" => Vec::new(),
        _ => bail!("unknown scene step `{op}`"),
    };
    Ok(stmts)
}

/// `n` GL enum names of the scene (`SCISSOR_TEST`, `NEAREST`, ...): upper-case letters, digits and `_`; C spells each `GL_<name>`.
fn gl_names<'a>(step: &str, a: &[&'a str], n: usize) -> Result<Vec<&'a str>> {
    ensure!(a.len() == n, "{step} takes {n} name(s), got {a:?}");
    for s in a {
        ensure!(
            !s.is_empty()
                && s.bytes()
                    .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_'),
            "`{s}` is not a GL name"
        );
    }
    Ok(a.to_vec())
}

/// Every token as a finite number.
fn nums(a: &[&str]) -> Result<Vec<f64>> {
    a.iter()
        .map(|s| {
            let v: f64 = s.parse().with_context(|| format!("number `{s}`"))?;
            ensure!(v.is_finite(), "number `{s}` is not finite");
            Ok(v)
        })
        .collect()
}

/// Every token as a whole number in `0..=max` (a colour channel, an index, a count).
fn whole(a: &[&str], max: f64) -> Result<Vec<f64>> {
    let v = nums(a)?;
    for (s, x) in a.iter().zip(&v) {
        ensure!(
            x.fract() == 0.0 && (0.0..=max).contains(x),
            "`{s}` is not a whole number in 0..={max}"
        );
    }
    Ok(v)
}

/// Exactly `n` finite numbers.
fn arity(step: &str, a: &[&str], n: usize) -> Result<Vec<f64>> {
    ensure!(a.len() == n, "{step} takes {n} numbers, got {}", a.len());
    nums(a)
}

/// Exactly `n` colour channels, 0..=255 each.
fn channels(step: &str, a: &[&str], n: usize) -> Result<Vec<f64>> {
    ensure!(a.len() == n, "{step} takes {n} channels, got {}", a.len());
    whole(a, 255.0)
}

/// A static C array of `vals` as `ty`, named `a<n>` with `n` counted in `arrays`: its declaration and its name.
fn static_array(arrays: &mut usize, ty: &str, vals: &[f64]) -> (String, String) {
    let name = format!("a{arrays}");
    *arrays += 1;
    let list: Vec<String> = vals.iter().map(|v| v.to_string()).collect();
    (
        format!("static const {ty} {name}[] = {{ {} }};", list.join(", ")),
        name,
    )
}

/// What the gate reads from one ELF.
pub struct Shape {
    pub needed: Vec<String>,
    /// (file, version) pairs of the GNU verneed.
    verneed: BTreeSet<(String, String)>,
    pub imports: BTreeSet<String>,
    pt_tls: bool,
}

pub fn shape(path: &Path) -> Result<Shape> {
    let data = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let file = ElfFile64::<Endianness>::parse(&*data)
        .with_context(|| format!("{}: not a 64-bit ELF", path.display()))?;
    let endian = file.endian();
    let sections = file.elf_section_table();
    let mut verneed = BTreeSet::new();
    if let Some((mut it, link)) = sections.gnu_verneed(endian, &*data)? {
        let strings = sections.strings(endian, &*data, link)?;
        while let Some((need, mut aux)) = it.next()? {
            let f = String::from_utf8_lossy(
                strings
                    .get(need.vn_file.get(endian))
                    .map_err(|()| anyhow::anyhow!("bad vn_file"))?,
            )
            .into_owned();
            while let Some(a) = aux.next()? {
                let v = strings
                    .get(a.vna_name.get(endian))
                    .map_err(|()| anyhow::anyhow!("bad vna_name"))?;
                verneed.insert((f.clone(), String::from_utf8_lossy(v).into_owned()));
            }
        }
    }
    let pt_tls = file
        .elf_header()
        .program_headers(endian, &*data)?
        .iter()
        .any(|p| p.p_type(endian) == elf::PT_TLS);
    let imports = file
        .dynamic_symbols()
        .filter(|s| s.is_undefined())
        .filter_map(|s| s.name().ok().map(str::to_owned))
        .collect();
    Ok(Shape {
        needed: provided_gate::needed(path)?,
        verneed,
        imports,
        pt_tls,
    })
}

/// The TLS shape expected at `api`: emulated below [`ELF_TLS_API`], native from it.
fn tls_failures(s: &Shape, api: u32) -> Vec<String> {
    let mut f = Vec::new();
    let emulated = s.imports.contains("__emutls_get_address");
    let native = s.pt_tls && s.imports.contains("__tls_get_addr");
    if api < ELF_TLS_API {
        if !emulated || s.pt_tls {
            f.push(format!("android-{api}: expected emulated TLS (__emutls_get_address import, no PT_TLS), got emutls={emulated} PT_TLS={}", s.pt_tls));
        }
    } else if !native || emulated {
        f.push(format!("android-{api}: expected ELF TLS (PT_TLS + __tls_get_addr), got PT_TLS={} __tls_get_addr={} emutls={emulated}", s.pt_tls, s.imports.contains("__tls_get_addr")));
    }
    f
}

/// ELF expectations of the fixture library itself (the sibling and libc++ need libc.so too; the probe covers their load).
fn elf_failures(s: &Shape, api: u32) -> Vec<String> {
    let mut f = Vec::new();
    for n in ["libc.so", CXX_SHARED] {
        if !s.needed.iter().any(|x| x == n) {
            f.push(format!(
                "android-{api}: DT_NEEDED lacks {n} (has {:?})",
                s.needed
            ));
        }
    }
    if !s
        .verneed
        .contains(&("libc.so".to_owned(), "LIBC".to_owned()))
    {
        f.push(format!(
            "android-{api}: no verneed libc.so/LIBC (has {:?})",
            s.verneed
        ));
    }
    f.extend(tls_failures(s, api));
    f
}

/// Host ld.so loading `lib` with `libdir` first on its path; its stderr, and whether the program ran cleanly.
fn ld_load(lib: &Path, libdir: &Path) -> Result<(bool, String)> {
    let out = Command::new("/bin/true")
        .env("LD_PRELOAD", lib)
        .env("LD_LIBRARY_PATH", libdir)
        .output()
        .context("running /bin/true under LD_PRELOAD")?;
    let err = String::from_utf8_lossy(&out.stderr).trim().to_owned();
    Ok((out.status.success() && err.is_empty(), err))
}

/// The bionic sonames the probe maps to glibc's; the rest of DT_NEEDED is the fixture's own dir.
const SONAME_MAP: &[(&str, &str)] = &[
    ("libc.so", "/lib/x86_64-linux-gnu/libc.so.6"),
    ("libm.so", "/lib/x86_64-linux-gnu/libm.so.6"),
    ("libdl.so", "/lib/x86_64-linux-gnu/libdl.so.2"),
];

/// The probe's library dir: the soname map, then the fixture's own libraries.
fn probe_dir(built: &Built) -> Result<PathBuf> {
    let dir = built.dir.join("probe-libs");
    if dir.exists() {
        std::fs::remove_dir_all(&dir)?;
    }
    std::fs::create_dir_all(&dir)?;
    for (bionic, glibc) in SONAME_MAP {
        std::os::unix::fs::symlink(glibc, dir.join(bionic))?;
    }
    for l in &built.libs {
        std::os::unix::fs::symlink(l, dir.join(l.file_name().context("lib name")?))?;
    }
    Ok(dir)
}

/// The glibc probe of one variant; failure messages.
fn probe_failures(built: &Built) -> Result<Vec<String>> {
    let dir = probe_dir(built)?;
    let mut f = Vec::new();
    for lib in [LIB, SIBLING] {
        let (clean, err) = ld_load(&built.dir.join(lib), &dir)?;
        let line = err.lines().next().unwrap_or("");
        if clean {
            f.push(format!("android-{}: glibc loaded {lib} cleanly", built.api));
        } else if !err.contains("version `LIBC' not found") {
            f.push(format!(
                "android-{}: glibc rejected {lib} for another reason: {line}",
                built.api
            ));
        } else {
            println!("pass glibc probe android-{} {lib}: {line}", built.api);
        }
    }
    std::fs::remove_dir_all(&dir)?;
    Ok(f)
}

/// The NativeBridge core's `isSupported` verdict (atl-native-bridge `elf::classify`) on the fixture: ours; with glibc's libc as control: not ours.
fn nb_failures(lib: &Path, api: u32) -> Result<Vec<String>> {
    use atl_native_bridge::elf::{Verdict, classify, host_machine};
    let host = host_machine().map_err(anyhow::Error::msg)?;
    let mut fails = Vec::new();
    match classify(&std::fs::read(lib)?, host) {
        Verdict::Bridged => println!("pass nb android-{api}: isSupported takes it"),
        v => fails.push(format!("nb android-{api}: isSupported refuses it ({v:?})")),
    }
    let glibc = Path::new(SONAME_MAP[1].1);
    match classify(&std::fs::read(glibc)?, host) {
        Verdict::NotBionic => {}
        v => fails.push(format!(
            "nb control: {} judged {v:?}, want NotBionic",
            glibc.display()
        )),
    }
    Ok(fails)
}

pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    let ndk = match &args.ndk {
        Some(n) => n.clone(),
        None => default_ndk(root)?,
    };
    let out = match &args.out {
        Some(o) => o.clone(),
        None => Layout::of(root)?.workspace().join("out/jni-fixture"),
    };
    let mut fails = Vec::new();
    // control: the probe responds -- a glibc library loads without a word
    let (clean, err) = ld_load(Path::new(SONAME_MAP[1].1), Path::new("/nonexistent"))?;
    if clean {
        println!(
            "pass probe control: glibc loads {} cleanly",
            SONAME_MAP[1].1
        );
    } else {
        fails.push(format!(
            "probe control: glibc failed to load {}: {err}",
            SONAME_MAP[1].1
        ));
    }
    let mut shapes = Vec::new();
    for api in [EMULATED_TLS_API, ELF_TLS_API] {
        let built = build(root, &ndk, &args.arch, api, &out, false)?;
        let s = shape(&built.dir.join(LIB))?;
        let f = elf_failures(&s, api);
        if f.is_empty() {
            println!(
                "pass elf android-{api}: NEEDED {:?}, verneed libc.so/LIBC, TLS {}",
                s.needed,
                if api < ELF_TLS_API { "emulated" } else { "ELF" }
            );
        }
        fails.extend(f);
        fails.extend(probe_failures(&built)?);
        fails.extend(nb_failures(&built.dir.join(LIB), api)?);
        fails.extend(nb_failures(&built.dir.join(LEGACY), api)?);
        shapes.push((api, s));
    }
    // control: each variant fails the other's TLS expectation
    for ((api, s), (other, _)) in shapes.iter().zip(shapes.iter().rev()) {
        if tls_failures(s, *other).is_empty() {
            fails.push(format!(
                "TLS control: android-{api} also passes android-{other}'s TLS expectation"
            ));
        } else {
            println!("pass TLS control: android-{api} fails android-{other}'s TLS expectation");
        }
    }
    for f in &fails {
        println!("FAIL {f}");
    }
    println!("{}", crate::verdict::gate("jni-fixture", &fails));
    if fails.is_empty() {
        Ok(ExitCode::SUCCESS)
    } else {
        Ok(ExitCode::FAILURE)
    }
}

/// The libraries the M2 test APK carries in `lib/<abi>/`: the `api` variant, built into `out`; `api` is also the APK's minSdk.
pub fn apk_libs(
    root: &Path,
    arch: &str,
    api: u32,
    out: &Path,
    legacy_no_libc: bool,
) -> Result<NativeLibs> {
    let ndk = default_ndk(root)?;
    let abi = NdkTarget::new(root, &ndk, arch, api)?.abi;
    let built = build(root, &ndk, arch, api, out, legacy_no_libc)?;
    if built.libs.is_empty() {
        bail!("no fixture libraries built");
    }
    Ok(NativeLibs {
        abi,
        libs: built.libs,
        min_sdk: api,
    })
}

/// A fixture's ELF expectations: DT_NEEDED names the libraries that answer its calls, and each import is one its source calls at run time (a folded call drops its import).
fn calls_failures(file: &str, s: &Shape, needed: &[&str], imports: &[&str]) -> Vec<String> {
    let mut f = Vec::new();
    for &n in needed {
        if !s.needed.iter().any(|x| x == n) {
            f.push(format!("{file}: DT_NEEDED lacks {n} (has {:?})", s.needed));
        }
    }
    for &i in imports {
        if !s.imports.contains(i) {
            f.push(format!("{file}: no import {i}"));
        }
    }
    f
}

/// The C12 legacy fixture's ELF expectations (`calls_failures`), plus no GNU verneed on libc.so: the old-NDK stub is unversioned. Legacy-only; calls_failures stays generic.
fn legacy_shape_failures(s: &Shape) -> Vec<String> {
    let mut f = Vec::new();
    // libstdc++.so's own stub stays versioned: only libc's is the old-NDK shape
    if s.verneed.iter().any(|(file, _)| file == "libc.so") {
        f.push(format!(
            "{LEGACY}: want no verneed on libc.so, got {:?}",
            s.verneed
        ));
    }
    f.extend(calls_failures(LEGACY, s, &LEGACY_NEEDED, &LEGACY_IMPORTS));
    f
}

/// The fixture library `name` among the APK's `libs` (apk_libs).
fn lib_named<'a>(libs: &'a [PathBuf], name: &str) -> Result<&'a PathBuf> {
    libs.iter()
        .find(|l| l.file_name().is_some_and(|n| n == name))
        .with_context(|| format!("no {name} among the fixture libraries"))
}

/// The legacy fixture among the APK's `libs` (apk_libs), checked by `legacy_shape_failures`.
pub fn legacy_failures(libs: &[PathBuf]) -> Result<Vec<String>> {
    Ok(legacy_shape_failures(&shape(lib_named(libs, LEGACY)?)?))
}

/// The C3 probes' fixture among the APK's `libs`, checked by `calls_failures` against `NDK_NEEDED` / `NDK_IMPORTS`.
pub fn ndk_failures(libs: &[PathBuf]) -> Result<Vec<String>> {
    let s = shape(lib_named(libs, LIB)?)?;
    Ok(calls_failures(LIB, &s, &NDK_NEEDED, &NDK_IMPORTS))
}

/// The ES1 scene's fixture among the APK's `libs`: DT_NEEDED names `libGLESv1_CM.so`, the proxy the scene's calls go to.
pub fn es1_failures(libs: &[PathBuf]) -> Result<Vec<String>> {
    let s = shape(lib_named(libs, LIB)?)?;
    Ok(calls_failures(LIB, &s, &[ES1_LIB], &[]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_shape_wants_its_guest_libs_and_calls() {
        let shape_of = |needed: &[&str], drop: Option<&str>| Shape {
            needed: needed.iter().map(|&n| n.to_owned()).collect(),
            verneed: BTreeSet::new(),
            imports: LEGACY_IMPORTS
                .iter()
                .filter(|&&i| Some(i) != drop)
                .map(|&i| i.to_owned())
                .collect(),
            pt_tls: false,
        };
        let all = ["libc.so", "libstdc++.so", "libz.so"];
        assert!(legacy_shape_failures(&shape_of(&all, None)).is_empty());
        let mut versioned = shape_of(&all, None);
        versioned.verneed.insert(("libc.so".into(), "LIBC".into()));
        assert_eq!(legacy_shape_failures(&versioned).len(), 1);
        assert_eq!(
            legacy_shape_failures(&shape_of(&["libc.so", "libstdc++.so"], None)).len(),
            1
        );
        assert_eq!(
            legacy_shape_failures(&shape_of(&all, Some("_Znwm"))).len(),
            1
        );
    }

    fn s(imports: &[&str], pt_tls: bool) -> Shape {
        Shape {
            needed: vec!["libc.so".into(), CXX_SHARED.into()],
            verneed: BTreeSet::from([("libc.so".into(), "LIBC".into())]),
            imports: imports.iter().map(|&i| i.to_owned()).collect(),
            pt_tls,
        }
    }

    #[test]
    fn tls_shape_follows_api() {
        let emu = s(&["__emutls_get_address"], false);
        let elf = s(&["__tls_get_addr"], true);
        assert!(elf_failures(&emu, EMULATED_TLS_API).is_empty());
        assert!(elf_failures(&elf, ELF_TLS_API).is_empty());
        assert!(!tls_failures(&emu, ELF_TLS_API).is_empty());
        assert!(!tls_failures(&elf, EMULATED_TLS_API).is_empty());
    }

    #[test]
    fn glibc_soname_is_not_bionic() {
        let mut x = s(&["__emutls_get_address"], false);
        x.needed = vec!["libc.so.6".into(), CXX_SHARED.into()];
        x.verneed = BTreeSet::from([("libc.so.6".into(), "GLIBC_2.2.5".into())]);
        assert_eq!(elf_failures(&x, EMULATED_TLS_API).len(), 2);
    }

    fn es1_scene() -> Scene {
        let path = crate::checkout_root()
            .expect("checkout root")
            .join("xtask/testdata/gl-probe/es1-scene.toml");
        Scene::load(&path).expect("the ES1 scene parses")
    }

    /// Expected counts are the toml's steps, counted by hand: five `elements` steps; one `pop` and one COLOR_ARRAY enable, both breakable, so none in the broken program.
    #[test]
    fn es1_c_makes_each_scene_call_as_the_toml_says() {
        let s = es1_scene();
        let full = es1_c(&s, false).expect("the scene generates");
        let broken = es1_c(&s, true).expect("the broken scene generates");
        let count = |c: &str, pat: &str| c.matches(pat).count();
        assert_eq!(count(&full, "glPopMatrix();"), 1);
        assert_eq!(count(&broken, "glPopMatrix();"), 0);
        assert_eq!(count(&full, "glEnableClientState(GL_COLOR_ARRAY);"), 1);
        assert_eq!(count(&broken, "glEnableClientState(GL_COLOR_ARRAY);"), 0);
        for c in [&full, &broken] {
            assert!(c.contains("#define ES1_W 64\n#define ES1_H 64\n"));
            assert_eq!(count(c, "glDrawElements("), 5);
            assert_eq!(count(c, "glPushMatrix();"), 1);
            assert_eq!(count(c, "glDrawArrays(GL_POINTS, 0, 1);"), 1);
            assert_eq!(
                count(
                    c,
                    "glReadPixels(0, 0, 64, 64, GL_RGBA, GL_UNSIGNED_BYTE, px);"
                ),
                1
            );
            assert_eq!(count(c, "resolve"), 0, "resolve makes no call");
        }
    }

    #[test]
    fn c_program_refuses_what_the_vocabulary_lacks() {
        let size = (64, 64);
        let one = |s: &str| c_program(size, &[s.to_owned()]);
        assert!(one("frob 1").is_err(), "unknown step");
        assert!(one("clear 1 2 3").is_err(), "clear takes RGBA");
        assert!(one("clear 1 2 3 256").is_err(), "a channel is 0..=255");
        assert!(one("read 32 32").is_err(), "read takes the surface");
        assert!(one("elements 0 1").is_err(), "indices are triangles");
        assert!(
            one("enable fog").is_err(),
            "a lower-case name is no GL name"
        );
    }

    #[test]
    fn es1_shape_wants_the_proxy() {
        let shape_of = |needed: &[&str]| Shape {
            needed: needed.iter().map(|&n| n.to_owned()).collect(),
            verneed: BTreeSet::new(),
            imports: BTreeSet::new(),
            pt_tls: false,
        };
        assert!(calls_failures(LIB, &shape_of(&[ES1_LIB]), &[ES1_LIB], &[]).is_empty());
        assert_eq!(
            calls_failures(LIB, &shape_of(&["libc.so"]), &[ES1_LIB], &[]).len(),
            1
        );
    }
}
