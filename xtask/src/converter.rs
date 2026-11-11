// SPDX-License-Identifier: MIT
//! The Blueprint -> ninja converter `tools/bp2ninja` as Go sees it: one go.work naming this checkout's converter and the pinned Go modules it compiles (`MODULES`; their paths differ per checkout and task copy), offline.
//! `xtask native` builds it from here; `xtask check` step `converter` gofmt-checks, builds, vets and tests it, so a converter that does not compile fails before any native run.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, ensure};
use atl_layout::Layout;

const DIR: &str = "tools/bp2ninja";

/// The pinned Go modules the converter compiles: blueprint (the Blueprint parser), Soong (its toolchain config packages, read as data) and the protobuf runtime Soong's `android` package imports.
const MODULES: [&str; 3] = [
    "aosp/build/blueprint",
    "aosp/build/soong",
    "aosp/external/golang-protobuf",
];

/// Writes `<dir>/go.work` for the checkout at `root` and returns its path: each of `MODULES` replaced by its checkout, and every other module their go.mod files require by an empty stub module under `<dir>/stub/`.
/// Those requirements (starlark-go, go-cmp, the v1 protobuf API, ...) are never imported by the converter's packages, but go loads their go.mod files to build the module graph; a stub has no packages, so a new import of one fails the build instead of reaching the network.
pub fn go_work(root: &Path, layout: &Layout, dir: &Path) -> Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let mut text = format!("go 1.24\n\nuse {}\n\n", root.join(DIR).display());
    let mut real = Vec::new();
    let mut required = Vec::new();
    for upstream in MODULES {
        let checkout = layout.locate(Path::new(upstream)).dir;
        let go_mod = std::fs::read_to_string(checkout.join("go.mod")).with_context(|| {
            format!(
                "{} is not fetched (cargo xtask fetch --execute)",
                checkout.display()
            )
        })?;
        let (module, requires) = parse_go_mod(&go_mod)
            .with_context(|| format!("{}/go.mod", checkout.display()))?;
        text.push_str(&format!("replace {module} => {}\n", checkout.display()));
        real.push(module);
        required.extend(requires);
    }
    required.sort();
    required.dedup();
    let stubs = dir.join("stub");
    let _ = std::fs::remove_dir_all(&stubs);
    for module in required.iter().filter(|m| !real.contains(m)) {
        let stub = stubs.join(module);
        std::fs::create_dir_all(&stub)?;
        std::fs::write(stub.join("go.mod"), format!("module {module}\n"))?;
        text.push_str(&format!("replace {module} => {}\n", stub.display()));
    }
    let file = dir.join("go.work");
    std::fs::write(&file, text)?;
    Ok(file)
}

/// A go.mod's module path and the module paths it requires (single-line and block `require`).
fn parse_go_mod(text: &str) -> Result<(String, Vec<String>)> {
    let mut module = None;
    let mut requires = Vec::new();
    let mut in_block = false;
    for line in text.lines() {
        let line = line.split("//").next().unwrap_or("").trim();
        if in_block {
            if line == ")" {
                in_block = false;
            } else if let Some(path) = line.split_whitespace().next() {
                requires.push(path.to_owned());
            }
        } else if let Some(rest) = line.strip_prefix("module ") {
            module = Some(rest.trim().to_owned());
        } else if line == "require (" {
            in_block = true;
        } else if let Some(rest) = line.strip_prefix("require ") {
            if let Some(path) = rest.split_whitespace().next() {
                requires.push(path.to_owned());
            }
        }
    }
    let module = module.context("no module line")?;
    Ok((module, requires))
}

/// `go <args>` in the converter's directory under `go_work`: every dependency is a pinned checkout or a stub, so no toolchain or module downloads.
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

/// Builds the converter to `dest` with go's own cache deciding what to compile; `dest` keeps its file (and mtime) when the build is byte-identical, since `go build -o` rewrites it every time (measured) and the binary is an input of build.ninja's regeneration edge (tools/bp2ninja regen.go).
pub fn build(root: &Path, go_work: &Path, dest: &Path) -> Result<()> {
    let mut fresh = dest.as_os_str().to_owned();
    fresh.push(".new");
    let fresh = PathBuf::from(fresh);
    go(
        root,
        go_work,
        &["build", "-o", &fresh.display().to_string(), "."],
    )?;
    if std::fs::read(dest).ok() == Some(std::fs::read(&fresh)?) {
        std::fs::remove_file(&fresh)?;
    } else {
        std::fs::rename(&fresh, dest)?;
    }
    Ok(())
}

/// Go files under `dir` that `gofmt` would rewrite (`gofmt -l`), relative to `dir`.
fn unformatted(dir: &Path) -> Result<Vec<String>> {
    let out = Command::new("gofmt")
        .args(["-l", "."])
        .current_dir(dir)
        .output()
        .context("running gofmt -l")?;
    ensure!(
        out.status.success(),
        "gofmt -l exited with {}: {}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    Ok(String::from_utf8(out.stdout)?
        .lines()
        .map(str::to_owned)
        .collect())
}

/// Step `converter` of `xtask check`: gofmt, build, vet and test under a go.work in the checkout's cargo target dir (per checkout, ignored).
pub fn check(root: &Path) -> Result<()> {
    let bad = unformatted(&root.join(DIR))?;
    ensure!(
        bad.is_empty(),
        "{DIR}: not gofmt-formatted (gofmt -w): {}",
        bad.join(", ")
    );
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

    /// gofmt -l names a misformatted file and nothing for a formatted one (the probe responds both ways).
    #[test]
    fn unformatted_names_misformatted_files() {
        let tmp = std::env::temp_dir().join(format!("xtask-gofmt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(tmp.join("ok.go"), "package x\n\nfunc F() {}\n").unwrap();
        let clean = unformatted(&tmp);
        std::fs::write(tmp.join("bad.go"), "package x\nfunc  G( ) {}\n").unwrap();
        let dirty = unformatted(&tmp);
        std::fs::remove_dir_all(&tmp).unwrap();
        assert!(clean.unwrap().is_empty());
        assert_eq!(dirty.unwrap(), ["bad.go"]);
    }

    /// Both require forms, comments and the module line (golang-protobuf's go.mod shape: block form; Soong's: block form with versions).
    #[test]
    fn go_mod_requires() {
        let text = "module google.golang.org/protobuf\n\ngo 1.11\n\nrequire github.com/x/single v1.0.0 // indirect\n\nrequire (\n\tgithub.com/golang/protobuf v1.5.0\n\t// a comment\n\tgithub.com/google/go-cmp v0.5.5\n)\n";
        let (module, requires) = parse_go_mod(text).unwrap();
        assert_eq!(module, "google.golang.org/protobuf");
        assert_eq!(
            requires,
            [
                "github.com/x/single",
                "github.com/golang/protobuf",
                "github.com/google/go-cmp"
            ]
        );
        assert!(parse_go_mod("go 1.11\n").is_err());
    }
}
