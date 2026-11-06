// SPDX-License-Identifier: MIT
//! `xtask test gl-dispatch`: AOSP's GL fronts built with `USE_SLOW_BINDING` (providers.toml `defines`) dispatch each thread's GL calls through the hooks table that thread stored, across the libEGL -> libGLESv2 boundary.
//! The slot is atl-bionic's `__get_tls()` array (one per thread, one storage for every DSO); the probe (xtask/testdata/gl-dispatch, its own cargo package for `unsafe`) stores two fake tables on two threads with libEGL's `setGlThreadSpecific` and calls libGLESv2's `glGetError` on each.
//! Control, every run: libGLESv2 loaded by `dlmopen(LM_ID_NEWLM)` gets its own atl-bionic, i.e. its own slot array -- the per-DSO-copy failure; the probe must fail there.
//! Prints the per-call cost of the slow binding (glGetError through the table vs the table's function called directly). No driver needed: the tables are fakes.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use anyhow::{Context, Result, bail, ensure};
use atl_layout::Layout;

use crate::host_art::exec;
use crate::{slot, tree};

const PROBE: &str = "xtask/testdata/gl-dispatch";
/// The fronts' sources, tree-relative: gl_hooks_t is `GL_ENTRY` x entries.in + `MAX_NUMBER_OF_GL_EXTENSIONS` pointers (hooks.h).
const ENTRIES_IN: &str = "frameworks/native/opengl/libs/entries.in";
const HOOKS_H: &str = "frameworks/native/opengl/libs/hooks.h";

#[derive(clap::Args)]
pub struct Args {
    /// The native build dir (default: this checkout's slot).
    #[arg(long)]
    out: Option<PathBuf>,
}

pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    let out = slot::out_dir(root, args.out.as_deref(), "--out", |w| {
        w.join("out/native/x86_64")
    })?;
    let tree = tree::shared_tree(&Layout::of(root)?);
    let size = table_pointers(
        &std::fs::read_to_string(tree.join(ENTRIES_IN))?,
        &std::fs::read_to_string(tree.join(HOOKS_H))?,
    )?;
    let lib = |m: &str| out.join("device").join(m).join(format!("{m}.so"));
    let (egl, gles) = (lib("libEGL"), lib("libGLESv2"));
    for l in [&egl, &gles] {
        ensure!(l.is_file(), "missing input {}", l.display());
    }
    let probe = build_probe(root, &out.join("cargo"))?;
    let go = |mode: &str| -> Result<(bool, String)> {
        let o = Command::new(&probe)
            .args([egl.as_os_str(), gles.as_os_str()])
            .arg(size.to_string())
            .arg(mode)
            .output()
            .context("running the probe")?;
        let text =
            String::from_utf8_lossy(&o.stdout).into_owned() + &String::from_utf8_lossy(&o.stderr);
        Ok((o.status.success(), text))
    };
    let (ok, same) = go("same")?;
    print!("{same}");
    let (broken_ok, split) = go("split")?;
    print!("{split}");
    if !ok {
        bail!("gl-dispatch: FAILED: threads do not dispatch through their own tables");
    }
    if broken_ok {
        bail!("gl-dispatch: FAILED control: a libGLESv2 with its own slot array passed");
    }
    println!("gl-dispatch: ok (control: a per-DSO slot array fails)");
    Ok(ExitCode::SUCCESS)
}

/// gl_hooks_t's size in pointers: one per `GL_ENTRY(` line of entries.in, plus hooks.h's extension array.
fn table_pointers(entries: &str, hooks: &str) -> Result<usize> {
    let gl = entries
        .lines()
        .filter(|l| l.starts_with("GL_ENTRY("))
        .count();
    let ext = hooks
        .lines()
        .find_map(|l| l.strip_prefix("#define MAX_NUMBER_OF_GL_EXTENSIONS"))
        .context("hooks.h: no MAX_NUMBER_OF_GL_EXTENSIONS")?
        .trim()
        .parse::<usize>()?;
    ensure!(gl > 0, "entries.in: no GL_ENTRY");
    Ok(gl + ext)
}

fn build_probe(root: &Path, target: &Path) -> Result<PathBuf> {
    exec(
        crate::cargo_command()
            .current_dir(root)
            .args(["build", "--release", "--locked", "--manifest-path"])
            .arg(root.join(PROBE).join("Cargo.toml"))
            .arg("--target-dir")
            .arg(target),
        "cargo build (gl-dispatch probe)",
    )?;
    let bin = target.join("release/gl-dispatch-probe");
    ensure!(bin.is_file(), "{} missing after cargo build", bin.display());
    Ok(bin)
}

#[cfg(test)]
mod tests {
    #[test]
    fn table_pointers() {
        let entries = "// c\nGL_ENTRY(void, glA, int)\nGL_ENTRY(GLenum, glGetError)\n";
        let hooks = "#define MAX_NUMBER_OF_GL_EXTENSIONS 256\n";
        // 2 entries + 256 extension slots
        assert_eq!(super::table_pointers(entries, hooks).unwrap(), 258);
        assert!(super::table_pointers(entries, "").is_err());
    }
}
