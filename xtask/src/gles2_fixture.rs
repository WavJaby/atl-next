// SPDX-License-Identifier: MIT
//! `xtask test gles2-fixture`: the GLES2 test APK (xtask/testdata/gles2), shaped like Cut the Rope's draw path: an Activity with a Java `GLSurfaceView` (EGL14 from the framework), its renderer calling an NDK-built `.so` that draws with GLES2 only and reads back pixels.
//! The library stands in for a game's native renderer (libctro imports ~39 `gl*` from libGLESv2, no `egl*`); built with NDK clang as jni_fixture.rs builds its fixture, arch from arches.toml.
//! Static gate (runtime use waits for bridge-1 N4 + GPU G3):
//! - `.so`: DT_NEEDED is exactly [`NEEDED`]; the `gl*` imports are exactly [`GL_IMPORTS`] (the calls atl_gles2.c makes); no `egl*` import.
//! - APK: built with the runtime gates' builder; manifest `<uses-feature glEsVersion>` = [`GL_ES_2`], `<application hardwareAccelerated="true">`, minSdk = [`MIN_SDK`]; the library stored in `lib/<abi>/`.
//! - controls, so a pass means something: each check rejects a broken copy of the real input (an extra `libEGL.so`, a missing or extra GL call, an `egl*` import, glEsVersion 3.0, hardwareAccelerated false).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use anyhow::Result;
use atl_layout::Layout;

use crate::atld_run::ApkTools;
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

const FIXTURE: &str = "xtask/testdata/gles2";
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

pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    let ndk = match &args.ndk {
        Some(n) => n.clone(),
        None => jni_fixture::default_ndk(root)?,
    };
    let work = match &args.work {
        Some(w) => w.clone(),
        None => Layout::of(root)?.workspace().join("out/gles2-fixture"),
    };
    let out = crate::build_state::gate_out(root, args.out.as_deref(), &args.host)?;
    let java_out = slot::out_dir(root, args.java_out.as_deref(), "--java-out", |w| {
        w.join("out/java/build")
    })?;
    let t = NdkTarget::new(root, &ndk, &args.arch, MIN_SDK)?;
    let lib = build_lib(root, &t, &work.join("lib"))?;
    let shape = jni_fixture::shape(&lib)?;
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

    let tools = ApkTools::new(root, &args.host, &out, &java_out)?;
    let native = (t.abi.clone(), vec![lib]);
    let apk = tools.build(&root.join(FIXTURE), &work.join("apk"), &[], Some(&native))?;
    let m = manifest::read(&tools.aapt2, &apk)?;
    let am = ApkManifest {
        gl_es: manifest::gl_es_version(&tools.aapt2, &apk)?,
        hardware_accelerated: m.application_booleans.get("hardwareAccelerated").copied(),
        min_sdk: m.min_sdk,
    };
    let mf = manifest_failures(&am);
    let entry = format!("lib/{}/{LIB}", t.abi);
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
