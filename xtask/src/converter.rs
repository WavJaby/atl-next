// SPDX-License-Identifier: MIT
//! The Blueprint -> ninja converter `tools/bp2ninja` as Go sees it: one go.work naming this checkout's converter and the pinned `build/blueprint` (that path differs per checkout and task copy), offline.
//! `xtask native` builds it from here; `xtask check` step `converter` builds, vets and tests it, so a converter that does not compile fails before any native run.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, ensure};
use atl_layout::Layout;

const DIR: &str = "tools/bp2ninja";

/// Writes `<dir>/go.work` for the checkout at `root` and returns its path.
pub fn go_work(root: &Path, layout: &Layout, dir: &Path) -> Result<PathBuf> {
    let blueprint = layout.locate(Path::new("aosp/build/blueprint")).dir;
    ensure!(
        blueprint.join("go.mod").is_file(),
        "{} is not fetched (cargo xtask fetch --execute)",
        blueprint.display()
    );
    std::fs::create_dir_all(dir)?;
    let file = dir.join("go.work");
    std::fs::write(
        &file,
        format!(
            "go 1.24\n\nuse {}\n\nreplace github.com/google/blueprint => {}\n",
            root.join(DIR).display(),
            blueprint.display()
        ),
    )?;
    Ok(file)
}

/// `go <args>` in the converter's directory under `go_work`: the pinned blueprint is the only dependency, so no toolchain or module downloads.
pub fn go(root: &Path, go_work: &Path, args: &[&str]) -> Result<()> {
    let status = Command::new("go")
        .args(args)
        .current_dir(root.join(DIR))
        .env("GOWORK", go_work)
        .env("GOTOOLCHAIN", "local")
        .env("GOPROXY", "off")
        .status()
        .with_context(|| format!("running go {}", args.join(" ")))?;
    ensure!(
        status.success(),
        "go {} exited with {status}",
        args.join(" ")
    );
    Ok(())
}

/// Step `converter` of `xtask check`: build, vet and test under a go.work in the checkout's cargo target dir (per checkout, ignored).
pub fn check(root: &Path) -> Result<()> {
    let dir = root.join("target").join("bp2ninja");
    let work = go_work(root, &Layout::of(root)?, &dir)?;
    let bin = dir.join("bp2ninja").display().to_string();
    go(root, &work, &["build", "-o", &bin, "."])?;
    go(root, &work, &["vet", "./..."])?;
    go(root, &work, &["test", "./..."])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The defect the step exists for: a merge leaving a struct with two fields of one name. go build must fail on it, and pass the same package without it (the probe responds both ways).
    #[test]
    fn duplicate_field_fails_the_build() {
        let tmp = std::env::temp_dir().join(format!("xtask-converter-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let pkg = tmp.join(DIR);
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::write(pkg.join("go.mod"), "module x\n\ngo 1.24\n").unwrap();
        let work = tmp.join("go.work");
        std::fs::write(&work, format!("go 1.24\n\nuse {}\n", pkg.display())).unwrap();
        let main = |fields: &str| {
            let src = format!(
                "package main\n\ntype Graph struct {{\n{fields}}}\n\nfunc main() {{ _ = Graph{{}} }}\n"
            );
            std::fs::write(pkg.join("main.go"), src).unwrap();
        };
        main("\tdropped []string\n");
        let ok = go(&tmp, &work, &["build", "-o", "/dev/null", "."]);
        main("\tdropped []string\n\tdropped []string\n");
        let broken = go(&tmp, &work, &["build", "-o", "/dev/null", "."]);
        std::fs::remove_dir_all(&tmp).unwrap();
        ok.unwrap();
        assert!(broken.is_err());
    }
}
