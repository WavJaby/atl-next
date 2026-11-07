// SPDX-License-Identifier: MIT
//! `xtask test gles2-fixture`: the GLES2 test APK (xtask/testdata/gles2), shaped like Cut the Rope's draw path: an Activity with a Java `GLSurfaceView` (EGL14 from the framework), its renderer calling an NDK-built `.so` that draws with GLES2 only and reads back pixels.
//! The library stands in for a game's native renderer (libctro imports ~39 `gl*` from libGLESv2, no `egl*`); built with NDK clang as jni_fixture.rs builds its fixture, arch from arches.toml.
//! Static gate, then the runtime use (`xtask test window --stage gles2`, GS2.5: [`apk_libs`] builds the library it puts in the APK):
//! - `.so`: DT_NEEDED is exactly [`NEEDED`]; the `gl*` imports are exactly [`GL_IMPORTS`] (the calls atl_gles2.c makes); no `egl*` import.
//! - APK: built with the runtime gates' builder; manifest `<uses-feature glEsVersion>` = [`GL_ES_2`], `<application hardwareAccelerated="true">`, minSdk = [`MIN_SDK`]; the library stored in `lib/<abi>/`.
//! - Main.java's PROBES rows equal [`PROBES`], the window stage's expectation (two copies, one checked against the other).
//! - controls, so a pass means something: each check rejects a broken copy of the real input (an extra `libEGL.so`, a missing or extra GL call, an `egl*` import, glEsVersion 3.0, hardwareAccelerated false).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use anyhow::Result;
use atl_layout::Layout;

use crate::atld_run::{ApkTools, NativeLibs};
use crate::jni_fixture::{self, CFLAGS, NdkTarget};
use crate::{host_art, manifest, slot};

#[derive(clap::Args)]
pub struct Args {
    /// The library's arch (an arches.toml row with `[guest]`); its first `android_abis` entry names the APK lib dir.
    #[arg(long)]
    arch: String,
    /// The host arch (an arches.toml row with `[host]`): its prebuilt JDK builds the APK.
    #[arg(long)]
    host: String,
    /// NDK root (default: <workspace>/tools/android-ndk-r30, as `xtask test jni-fixture`).
    #[arg(long)]
    ndk: Option<PathBuf>,
    /// Native build directory: only its build.prop is read, for the SDK level of android.jar (default: this checkout's slot, see `xtask slot`; else <workspace>/out/native/<host>).
    #[arg(long)]
    out: Option<PathBuf>,
    /// Java pipeline build directory: aapt2, d8 (default: this checkout's slot; else <workspace>/out/java/build).
    #[arg(long)]
    java_out: Option<PathBuf>,
    /// Where the library and APK go (default: <workspace>/out/gles2-fixture).
    #[arg(long)]
    work: Option<PathBuf>,
}

pub(crate) const FIXTURE: &str = "xtask/testdata/gles2";
const SOURCE: &str = "jni/atl_gles2.c";
/// `System.loadLibrary("atl-gles2")` in Main.java.
const LIB: &str = "libatl-gles2.so";
/// The manifest's minSdkVersion; the library is built for it, as an app's build targets its minSdk.
const MIN_SDK: u32 = 21;
/// GLES 2.0 as ConfigurationInfo.reqGlEsVersion encodes it (major << 16 | minor).
const GL_ES_2: i64 = 0x2_0000;
/// DT_NEEDED: what the link line asks for (`-lGLESv2 -llog`) plus what NDK clang's Android driver always adds (libc, libdl; measured with r30); libctro's set minus what the fixture does not call (libandroid, libjnigraphics, libz, libm).
const NEEDED: &[&str] = &["libGLESv2.so", "liblog.so", "libdl.so", "libc.so"];
/// Every GL entry point atl_gles2.c calls, read off the source; a change to the scene changes this list in the same commit.
const GL_IMPORTS: &[&str] = &[
    "glAttachShader",
    "glBindAttribLocation",
    "glBindTexture",
    "glClear",
    "glClearColor",
    "glCompileShader",
    "glCreateProgram",
    "glCreateShader",
    "glDisable",
    "glDrawArrays",
    "glEnableVertexAttribArray",
    "glGenTextures",
    "glGetError",
    "glGetProgramiv",
    "glGetShaderInfoLog",
    "glGetShaderiv",
    "glGetUniformLocation",
    "glLinkProgram",
    "glReadPixels",
    "glShaderSource",
    "glTexImage2D",
    "glTexParameteri",
    "glUniform1i",
    "glUniform4f",
    "glUseProgram",
    "glVertexAttribPointer",
    "glViewport",
];

/// Main.java's table, read by the static gate; the window stage (window/gles2.rs) reads the composed picture at these probes.
const MAIN_JAVA: &str = "java/com/example/atlgles2/Main.java";
/// The expected pixels (x, y in GL window coordinates, origin bottom-left; 0xRRGGBBAA) worked out in Main.java's PROBES comment, not by running the scene. Mirrored in Main.java; [`probe_failures`] checks the mirror.
pub(crate) const PROBES: [(u32, u32, u32); 6] = [
    (8, 8, 0xff0000ff),
    (24, 56, 0x4080c0ff),
    (40, 40, 0x00ff00ff),
    (56, 40, 0x0000ffff),
    (40, 56, 0xffffffff),
    (56, 56, 0xffff00ff),
];

/// Builds the library into `dir`; its path.
fn build_lib(root: &Path, t: &NdkTarget, dir: &Path) -> Result<PathBuf> {
    if dir.exists() {
        std::fs::remove_dir_all(dir)?;
    }
    std::fs::create_dir_all(dir)?;
    let lib = dir.join(LIB);
    host_art::exec(
        t.cmd("clang")
            .arg("-shared")
            .args(CFLAGS)
            .arg("-std=c11")
            .arg(root.join(FIXTURE).join(SOURCE))
            .args(["-lGLESv2", "-llog"])
            .arg(format!("-Wl,-soname,{LIB}"))
            .arg("-o")
            .arg(&lib),
        "NDK clang atl_gles2.c",
    )?;
    Ok(lib)
}

/// The library for `arch` (an arches.toml row with `[guest]`), built with NDK `ndk` into `out`: the `lib/<abi>/` entry of the APK.
pub(crate) fn apk_libs(root: &Path, ndk: &Path, arch: &str, out: &Path) -> Result<NativeLibs> {
    let t = NdkTarget::new(root, ndk, arch, MIN_SDK)?;
    let lib = build_lib(root, &t, out)?;
    Ok(NativeLibs {
        abi: t.abi.clone(),
        libs: vec![lib],
        min_sdk: MIN_SDK,
    })
}

/// Main.java's PROBES rows, in order (`{x, y, 0xRRGGBBAA},`); a row that does not parse is dropped and then named by [`probe_failures`].
fn java_probes(java: &str) -> Vec<(u32, u32, u32)> {
    java.lines()
        .filter_map(|line| {
            let row = line.trim().strip_prefix('{')?.strip_suffix("},")?;
            let mut f = row.split(',').map(str::trim);
            let x = f.next()?.parse().ok()?;
            let y = f.next()?.parse().ok()?;
            let rgba = u32::from_str_radix(f.next()?.strip_prefix("0x")?, 16).ok()?;
            f.next().is_none().then_some((x, y, rgba))
        })
        .collect()
}

/// Main.java's table must equal [`PROBES`] row for row: the window stage judges the composed picture against [`PROBES`], the app against Main.java.
fn probe_failures(java: &str) -> Vec<String> {
    let got = java_probes(java);
    if got == PROBES {
        vec![]
    } else {
        vec![format!(
            "Main.java PROBES {got:x?}, gles2_fixture::PROBES {PROBES:x?}"
        )]
    }
}

fn needed_failures(needed: &[String]) -> Vec<String> {
    let got: BTreeSet<&str> = needed.iter().map(String::as_str).collect();
    let want: BTreeSet<&str> = NEEDED.iter().copied().collect();
    if got == want {
        vec![]
    } else {
        vec![format!("DT_NEEDED {got:?}, want {want:?}")]
    }
}

fn import_failures(imports: &BTreeSet<String>) -> Vec<String> {
    let mut f = Vec::new();
    let gl: BTreeSet<&str> = imports
        .iter()
        .map(String::as_str)
        .filter(|s| s.starts_with("gl"))
        .collect();
    let want: BTreeSet<&str> = GL_IMPORTS.iter().copied().collect();
    let missing: Vec<_> = want.difference(&gl).collect();
    let extra: Vec<_> = gl.difference(&want).collect();
    if !missing.is_empty() || !extra.is_empty() {
        f.push(format!("gl* imports: missing {missing:?}, extra {extra:?}"));
    }
    let egl: Vec<_> = imports.iter().filter(|s| s.starts_with("egl")).collect();
    if !egl.is_empty() {
        f.push(format!("egl* imports (the context is Java's): {egl:?}"));
    }
    f
}

/// What the gate reads from the built APK's manifest.
#[derive(Clone, Copy)]
struct ApkManifest {
    gl_es: Option<i64>,
    hardware_accelerated: Option<bool>,
    min_sdk: i32,
}

fn manifest_failures(m: &ApkManifest) -> Vec<String> {
    let mut f = Vec::new();
    if m.gl_es != Some(GL_ES_2) {
        f.push(format!(
            "uses-feature glEsVersion {:?}, want {GL_ES_2:#x}",
            m.gl_es.map(|v| format!("{v:#x}"))
        ));
    }
    if m.hardware_accelerated != Some(true) {
        f.push(format!(
            "<application hardwareAccelerated> {:?}, want written true",
            m.hardware_accelerated
        ));
    }
    if i64::from(m.min_sdk) != i64::from(MIN_SDK) {
        f.push(format!(
            "minSdk {}, the library is built for {MIN_SDK}",
            m.min_sdk
        ));
    }
    f
}

/// Entry names of `apk` (`jar tf`).
fn apk_entries(jdk: &Path, apk: &Path) -> Result<Vec<String>> {
    let text = host_art::output(
        Command::new(jdk.join("bin/jar")).arg("tf").arg(apk),
        "jar tf",
    )?;
    Ok(text.lines().map(str::to_owned).collect())
}

/// Each check rejects a broken copy of the real input; failure messages for the checks that did not.
fn control_failures(needed: &[String], imports: &BTreeSet<String>, m: &ApkManifest) -> Vec<String> {
    let mut f = Vec::new();
    let mut check = |name: &str, rejected: bool| {
        if rejected {
            println!("pass control: {name} rejected");
        } else {
            f.push(format!("control: {name} passed the gate"));
        }
    };
    let mut n = needed.to_vec();
    n.push("libEGL.so".into());
    check("DT_NEEDED + libEGL.so", !needed_failures(&n).is_empty());
    let mut i = imports.clone();
    i.remove(GL_IMPORTS[0]);
    check(
        &format!("imports - {}", GL_IMPORTS[0]),
        !import_failures(&i).is_empty(),
    );
    let mut i = imports.clone();
    i.insert("glBlendFunc".into());
    check("imports + glBlendFunc", !import_failures(&i).is_empty());
    let mut i = imports.clone();
    i.insert("eglGetCurrentContext".into());
    check(
        "imports + eglGetCurrentContext",
        !import_failures(&i).is_empty(),
    );
    let gl3 = ApkManifest {
        gl_es: Some(0x3_0000),
        ..*m
    };
    check("glEsVersion 0x30000", !manifest_failures(&gl3).is_empty());
    let sw = ApkManifest {
        hardware_accelerated: Some(false),
        ..*m
    };
    check(
        "hardwareAccelerated false",
        !manifest_failures(&sw).is_empty(),
    );
    f
}

/// Native goals (gate_goals.rs): none beyond a current slot (it reads the Java pipeline's tools and build.prop there).
pub fn goals() -> crate::gate_goals::Goals {
    crate::gate_goals::Goals::default()
}

pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    let ndk = match &args.ndk {
        Some(n) => n.clone(),
        None => jni_fixture::default_ndk(root)?,
    };
    let work = match &args.work {
        Some(w) => w.clone(),
        None => Layout::of(root)?.workspace().join("out/gles2-fixture"),
    };
    let out = crate::build_state::gate_out(root, args.out.as_deref(), &args.host, "gles2-fixture")?;
    let java_out = slot::out_dir(root, args.java_out.as_deref(), "--java-out", |w| {
        w.join("out/java/build")
    })?;
    let native = apk_libs(root, &ndk, &args.arch, &work.join("lib"))?;
    let shape = jni_fixture::shape(&work.join("lib").join(LIB))?;
    let mut fails = needed_failures(&shape.needed);
    let imports = import_failures(&shape.imports);
    if fails.is_empty() && imports.is_empty() {
        println!(
            "pass elf: NEEDED {:?}, {} gl* imports as listed, no egl*",
            shape.needed,
            GL_IMPORTS.len()
        );
    }
    fails.extend(imports);
    fails.extend(probe_failures(&std::fs::read_to_string(
        root.join(FIXTURE).join(MAIN_JAVA),
    )?));

    let tools = ApkTools::new(root, &args.host, &out, &java_out)?;
    let apk = tools.build(
        &root.join(FIXTURE),
        &work.join("apk"),
        &[],
        Some(&native),
        None,
    )?;
    let m = manifest::read(&tools.aapt2, &apk)?;
    let am = ApkManifest {
        gl_es: manifest::gl_es_version(&tools.aapt2, &apk)?,
        hardware_accelerated: m.application_booleans.get("hardwareAccelerated").copied(),
        min_sdk: m.min_sdk,
    };
    let mf = manifest_failures(&am);
    let entry = format!("lib/{}/{LIB}", native.abi);
    let has_lib = apk_entries(&tools.jdk, &apk)?.contains(&entry);
    if !has_lib {
        fails.push(format!("{} lacks {entry}", apk.display()));
    }
    if mf.is_empty() && has_lib {
        println!(
            "pass apk {}: glEsVersion {GL_ES_2:#x}, hardwareAccelerated, minSdk {MIN_SDK}, {entry}",
            apk.display()
        );
    }
    fails.extend(mf);
    fails.extend(control_failures(&shape.needed, &shape.imports, &am));
    for f in &fails {
        println!("FAIL {f}");
    }
    println!("{}", crate::verdict::gate("gles2-fixture", &fails));
    Ok(if fails.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn imports() -> BTreeSet<String> {
        GL_IMPORTS
            .iter()
            .map(|s| (*s).to_owned())
            .chain([
                "__android_log_print".to_owned(),
                "__cxa_finalize".to_owned(),
            ])
            .collect()
    }

    #[test]
    fn listed_set_passes_and_breaks_fail() {
        let needed: Vec<String> = NEEDED.iter().map(|s| (*s).to_owned()).collect();
        let m = ApkManifest {
            gl_es: Some(GL_ES_2),
            hardware_accelerated: Some(true),
            min_sdk: 21,
        };
        assert!(needed_failures(&needed).is_empty());
        assert!(import_failures(&imports()).is_empty());
        assert!(manifest_failures(&m).is_empty());
        assert!(control_failures(&needed, &imports(), &m).is_empty());
    }

    /// Main.java's table is the gate's table; a changed row is named.
    #[test]
    fn probe_table_is_the_java_one() {
        let java = include_str!("../testdata/gles2/java/com/example/atlgles2/Main.java");
        assert!(probe_failures(java).is_empty());
        let broken = java.replace("{24, 56, 0x4080c0ff}", "{24, 56, 0x4080c0fe}");
        assert_eq!(probe_failures(&broken).len(), 1);
    }

    #[test]
    fn unwritten_hardware_accelerated_fails() {
        let m = ApkManifest {
            gl_es: Some(GL_ES_2),
            hardware_accelerated: None,
            min_sdk: 21,
        };
        assert_eq!(manifest_failures(&m).len(), 1);
    }
}
