// SPDX-License-Identifier: MIT
//! `arches.toml`: the only list of architectures. Column meanings live in that file's header.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;

use crate::deps;

pub const FILE: &str = "arches.toml";

/// Row key = ART instruction-set string.
pub type Arches = BTreeMap<String, Arch>;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Arch {
    pub pointer_bits: u32,
    pub soong_arch_variant: Option<String>,
    pub kernel_arch: String,
    /// Records of `asm-<kernel_arch>/asm/ptrace.h` the `<sys/ptrace.h>` shape wrapper declares (atl-bionic build.rs).
    pub uapi_records: Vec<String>,
    pub host: Option<Host>,
    pub guest: Option<Guest>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
#[expect(dead_code, reason = "read by xtask deps/fetch/build, not written yet")]
pub struct Host {
    pub rust: String,
    pub meson_cpu_family: String,
    pub debian: String,
    pub archlinux: Option<String>,
    pub soong_prebuilt_os: Option<String>,
    pub isa: Option<String>,
    pub clang: Option<String>,
    pub sysroot: Option<String>,
    /// Newest GLIBC_x.y an output may need (dotted, `2.39`): the glibc of the oldest machine the outputs run on. Required with isa.
    pub glibc_floor: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Guest {
    pub android_abis: Vec<String>,
    pub clang: String,
    pub qemu_target: String,
}

pub fn load(root: &Path) -> Result<Arches> {
    let text =
        std::fs::read_to_string(root.join(FILE)).with_context(|| format!("reading {FILE}"))?;
    parse(&text).with_context(|| format!("in {FILE}"))
}

/// The row of the machine xtask runs on: the `[host]` row whose `rust` triple is rustc's host (`rustc -vV` in `root`, so rust-toolchain.toml's rustc).
pub fn build_machine<'a>(root: &Path, arches: &'a Arches) -> Result<(&'a str, &'a Arch)> {
    let out = std::process::Command::new("rustc")
        .arg("-vV")
        .current_dir(root)
        .output()
        .context("running rustc -vV")?;
    ensure!(out.status.success(), "rustc -vV failed");
    let text = String::from_utf8(out.stdout)?;
    let triple = text
        .lines()
        .find_map(|l| l.strip_prefix("host: "))
        .with_context(|| format!("rustc -vV printed no host line: {text:?}"))?;
    host_row(arches, triple)
}

/// The `[host]` row whose `rust` triple is `triple`.
fn host_row<'a>(arches: &'a Arches, triple: &str) -> Result<(&'a str, &'a Arch)> {
    arches
        .iter()
        .find(|(_, a)| a.host.as_ref().is_some_and(|h| h.rust == triple))
        .map(|(name, a)| (name.as_str(), a))
        .with_context(|| format!("no {FILE} [host] row has rust = {triple:?}"))
}

/// What a host of another arch than the build machine is compiled and linked for: its `[host]` row's clang, sysroot and isa.
#[derive(Debug, PartialEq)]
pub struct CrossTarget {
    pub clang: String,
    pub sysroot: String,
    pub isa: String,
}

/// The cross target of host `name` when the build machine is `build` (build_machine's name): None for the build machine itself; another arch needs all three columns of its row.
pub fn cross_target(name: &str, host: &Host, build: &str) -> Result<Option<CrossTarget>> {
    if name == build {
        return Ok(None);
    }
    match (&host.clang, &host.sysroot, &host.isa) {
        (Some(clang), Some(sysroot), Some(isa)) => Ok(Some(CrossTarget {
            clang: clang.clone(),
            sysroot: sysroot.clone(),
            isa: isa.clone(),
        })),
        _ => bail!(
            "{FILE}: {name} is not the build machine ({build}): a cross host needs clang, sysroot and isa in its [host] row"
        ),
    }
}

fn parse(text: &str) -> Result<Arches> {
    let arches: Arches = toml::from_str(text)?;
    validate(&arches)?;
    Ok(arches)
}

fn validate(arches: &Arches) -> Result<()> {
    ensure!(!arches.is_empty(), "no arches");
    // an APK ABI must resolve to exactly one arch for the launcher's ABI selection
    let mut abi_owner: BTreeMap<&str, &str> = BTreeMap::new();
    for (name, arch) in arches {
        ensure!(
            matches!(arch.pointer_bits, 32 | 64),
            "{name}: pointer_bits {} is not 32 or 64",
            arch.pointer_bits
        );
        ensure!(
            arch.host.is_some() || arch.guest.is_some(),
            "{name}: neither [{name}.host] nor [{name}.guest]"
        );
        if let Some(host) = &arch.host {
            ensure!(
                host.isa.is_none() || host.glibc_floor.is_some(),
                "{name}: isa without glibc_floor (the outputs' glibc floor)"
            );
            if let Some(floor) = &host.glibc_floor {
                dotted_version(floor).with_context(|| format!("{name}: glibc_floor"))?;
            }
        }
        if let Some(guest) = &arch.guest {
            ensure!(!guest.android_abis.is_empty(), "{name}: empty android_abis");
            for abi in &guest.android_abis {
                if let Some(other) = abi_owner.insert(abi, name) {
                    bail!("android ABI {abi} listed by both {other} and {name}");
                }
            }
        }
    }
    Ok(())
}

/// bionic's build file, an upstream path; it owns the uapi arch directories.
const BIONIC_LIBC_BP: &str = "aosp/bionic/libc/Android.bp";

/// Every row's `kernel_arch` is the `asm-` directory bionic's `libc_uapi_headers` exports for that
/// (Soong) arch, in the bionic checkout `layout` resolves. Needs the fetched tree: build scripts parse that uapi, so nothing builds without it.
pub fn check_kernel_arch(layout: &atl_layout::Layout, arches: &Arches) -> Result<()> {
    let path = layout.locate(Path::new(BIONIC_LIBC_BP)).dir;
    let text = std::fs::read_to_string(&path).with_context(|| {
        format!(
            "reading {}: run `cargo xtask fetch --execute`",
            path.display()
        )
    })?;
    kernel_arch_matches(&text, arches).with_context(|| format!("{FILE} vs {}", path.display()))
}

/// Every row's `uapi_records` are declared by its `asm-<kernel_arch>/asm/ptrace.h` in the bionic checkout `layout` resolves (`struct <name> {`).
/// Needs the fetched tree, as check_kernel_arch does.
pub fn check_uapi_records(layout: &atl_layout::Layout, arches: &Arches) -> Result<()> {
    for (name, arch) in arches {
        let rel = format!(
            "aosp/bionic/libc/kernel/uapi/asm-{}/asm/ptrace.h",
            arch.kernel_arch
        );
        let path = layout.locate(Path::new(&rel)).dir;
        let text = std::fs::read_to_string(&path).with_context(|| {
            format!(
                "reading {}: run `cargo xtask fetch --execute`",
                path.display()
            )
        })?;
        for record in &arch.uapi_records {
            ensure!(
                text.contains(&format!("struct {record} {{")),
                "{name}: uapi_records names {record}, which {} does not declare",
                path.display()
            );
        }
    }
    Ok(())
}

fn kernel_arch_matches(android_bp: &str, arches: &Arches) -> Result<()> {
    let start = android_bp
        .find("name: \"libc_uapi_headers\"")
        .context("no libc_uapi_headers module")?;
    // a module ends at the first closing brace in column 0
    let block = &android_bp[start..];
    let block = &block[..block.find("\n}").unwrap_or(block.len())];
    let flat: String = block.chars().filter(|c| !c.is_whitespace()).collect();
    for (name, arch) in arches {
        let entry = format!(
            "{name}:{{export_system_include_dirs:[\"kernel/uapi/asm-{}\"],}}",
            arch.kernel_arch
        );
        ensure!(
            flat.contains(&entry),
            "{name}: kernel_arch {} is not what libc_uapi_headers exports for {name}",
            arch.kernel_arch
        );
    }
    Ok(())
}

/// Soong's config, an upstream path: PrebuiltOS names the prebuilts directories.
const SOONG_CONFIG_GO: &str = "aosp/build/soong/android/config.go";
/// Tree-relative directory of the prebuilt build tools, one subdirectory per PrebuiltOS.
const BUILD_TOOLS: &str = "prebuilts/build-tools";
/// The converter's Java tools in AOSP tree `tree` for PrebuiltOS `os`: the JDK home and the prebuilt build tools' bin (soong_zip, ...), as Soong's ANDROID_JAVA_HOME and prebuilt Go tools. `jdk` is deps.toml row `jdk`'s AOSP path (Soong's prebuilts/jdk/jdk21, one subdirectory per PrebuiltOS).
pub fn java_tools(tree: &Path, os: &str, jdk: &str) -> (PathBuf, PathBuf) {
    (
        tree.join(jdk).join(os),
        tree.join(BUILD_TOOLS).join(os).join("bin"),
    )
}

/// `rv64gc_zba_zbb` -> `["zba", "zbb"]`: the extensions past the base, which the Rust target fixes (`rv64gc`).
pub fn isa_extensions(isa: &str) -> Result<Vec<String>> {
    let mut parts = isa.split('_');
    ensure!(
        parts.next() == Some("rv64gc"),
        "isa `{isa}`: the base must be rv64gc, the riscv64gc Rust target's base"
    );
    let extensions: Vec<String> = parts.map(str::to_owned).collect();
    for e in &extensions {
        ensure!(
            e.starts_with('z')
                && e.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit()),
            "isa `{isa}`: `{e}` is not a Z extension"
        );
    }
    Ok(extensions)
}

/// The extensions an `isa` row allows, implied ones included: `rv64gc_zba_zbb` -> a c d f i m zaamo zalrsc zba zbb zca zcd zicsr zifencei zmmul.
/// The base is the riscv64gc Rust target's (G = IMAFD_Zicsr_Zifencei, plus C); the implications are the RISC-V unprivileged ISA's. rv64 only: zcf (C with F) is an rv32 rule.
pub fn isa_closure(isa: &str) -> Result<BTreeSet<String>> {
    const BASE: [&str; 8] = ["i", "m", "a", "f", "d", "c", "zicsr", "zifencei"];
    const IMPLIED: [(&[&str], &[&str]); 6] = [
        (&["m"], &["zmmul"]),
        (&["a"], &["zaamo", "zalrsc"]),
        (&["f"], &["zicsr"]),
        (&["d"], &["f"]),
        (&["c"], &["zca"]),
        (&["c", "d"], &["zcd"]),
    ];
    let mut set: BTreeSet<String> = BASE.iter().map(|e| e.to_string()).collect();
    set.extend(isa_extensions(isa)?);
    loop {
        let before = set.len();
        for (premises, implied) in IMPLIED {
            if premises.iter().all(|p| set.contains(*p)) {
                set.extend(implied.iter().map(|e| e.to_string()));
            }
        }
        if set.len() == before {
            return Ok(set);
        }
    }
}

/// `2.39` -> `[2, 39]`: a dotted numeric version (glibc_floor, and the GLIBC_ names), compared as a vector.
pub fn dotted_version(text: &str) -> Result<Vec<u32>> {
    text.split('.')
        .map(|part| part.parse::<u32>())
        .collect::<std::result::Result<Vec<u32>, _>>()
        .with_context(|| format!("`{text}` is not a dotted version"))
}

/// The cross columns of every host row that has them: `isa` extensions are rustc target features of its Rust
/// target, `clang` is a triple clang accepts with the arch name as its arch part, and `sysroot` names a `sysroot_debs` row.
pub fn check_cross(root: &Path, arches: &Arches) -> Result<()> {
    for (name, host) in arches
        .iter()
        .filter_map(|(n, a)| Some((n, a.host.as_ref()?)))
    {
        if let Some(isa) = &host.isa {
            let features = rustc_features(root, &host.rust)?;
            for e in isa_extensions(isa)? {
                ensure!(
                    features.contains(&e),
                    "{FILE}: {name}: isa extension `{e}` is not a rustc target feature of {}",
                    host.rust
                );
            }
        }
        if let Some(triple) = &host.clang {
            let out = Command::new(deps::binary(root, "clang")?)
                .arg(format!("--target={triple}"))
                .arg("-print-target-triple")
                .output()
                .context("running clang -print-target-triple")?;
            ensure!(
                out.status.success() && triple.split('-').next() == Some(name.as_str()),
                "{FILE}: {name}: clang does not accept {triple} as a target with arch {name}"
            );
        }
        if let Some(sysroot) = &host.sysroot {
            let row =
                deps::row(root, sysroot).with_context(|| format!("{FILE}: {name}: sysroot"))?;
            ensure!(
                row.sysroot_debs.is_some(),
                "{FILE}: {name}: sysroot {sysroot} is not a sysroot_debs row in deps.toml"
            );
        }
    }
    Ok(())
}

/// The target features rustc lists for `triple`: the first word of each indented feature line of `rustc --print target-features`.
fn rustc_features(root: &Path, triple: &str) -> Result<BTreeSet<String>> {
    let out = Command::new("rustc")
        .current_dir(root)
        .args(["--print", "target-features", "--target", triple])
        .output()
        .context("running rustc --print target-features")?;
    ensure!(
        out.status.success(),
        "rustc --print target-features --target {triple}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| l.starts_with("    "))
        .filter_map(|l| l.split_whitespace().next())
        .map(str::to_owned)
        .collect())
}

/// The QEMU linux-user target of guest arch `name`: its `qemu_target` column (`aarch64` for `<target>-linux-user`).
pub fn qemu_target<'a>(arches: &'a Arches, name: &str) -> Result<&'a str> {
    let arch = arches
        .get(name)
        .with_context(|| format!("{FILE}: no arch {name}"))?;
    let guest = arch
        .guest
        .as_ref()
        .with_context(|| format!("{FILE}: {name} has no [{name}.guest] row"))?;
    Ok(&guest.qemu_target)
}

/// Every guest row's `qemu_target` is a linux-user target of the pinned QEMU: `configs/targets/<it>-linux-user.mak` in the checkout `layout` resolves.
pub fn check_qemu_target(layout: &atl_layout::Layout, arches: &Arches) -> Result<()> {
    let configs = layout
        .locate(Path::new(crate::qemu::UPSTREAM))
        .dir
        .join("configs/targets");
    for (name, arch) in arches {
        let Some(guest) = &arch.guest else {
            continue;
        };
        let mak = configs.join(format!("{}-linux-user.mak", guest.qemu_target));
        ensure!(
            mak.is_file(),
            "{FILE}: {name}: qemu_target {} has no {} (not a linux-user target of the pinned QEMU)",
            guest.qemu_target,
            mak.display()
        );
    }
    Ok(())
}

/// Every `soong_prebuilt_os` is a value Soong's PrebuiltOS returns on Linux, and `prebuilts/build-tools/<it>/bin` exists in the checkout `layout`
/// resolves. Which row gets which value (PrebuiltOS keys on Go's GOARCH, a name this table does not carry) is not checked.
pub fn check_prebuilt_os(layout: &atl_layout::Layout, arches: &Arches) -> Result<()> {
    let path = layout.locate(Path::new(SOONG_CONFIG_GO)).dir;
    let text = std::fs::read_to_string(&path).with_context(|| {
        format!(
            "reading {}: run `cargo xtask fetch --execute`",
            path.display()
        )
    })?;
    let known =
        linux_prebuilt_os(&text).with_context(|| format!("{FILE} vs {}", path.display()))?;
    let tools = layout.locate(&Path::new("aosp").join(BUILD_TOOLS)).dir;
    for (name, host) in arches
        .iter()
        .filter_map(|(n, a)| Some((n, a.host.as_ref()?)))
    {
        let Some(os) = &host.soong_prebuilt_os else {
            continue;
        };
        ensure!(
            known.contains(os),
            "{FILE}: {name}: soong_prebuilt_os {os} is not a Linux PrebuiltOS of {} ({known:?})",
            path.display()
        );
        let bin = tools.join(os).join("bin");
        ensure!(
            bin.is_dir(),
            "{FILE}: {name}: soong_prebuilt_os {os}: {} does not exist",
            bin.display()
        );
    }
    Ok(())
}

/// The values `func (c *config) PrebuiltOS()` returns in its `case "linux":` arm.
fn linux_prebuilt_os(config_go: &str) -> Result<Vec<String>> {
    let start = config_go
        .find("func (c *config) PrebuiltOS() string {")
        .context("no PrebuiltOS")?;
    let body = &config_go[start..];
    let body = &body[..body.find("\n}").unwrap_or(body.len())];
    let linux = body
        .split_once("case \"linux\":")
        .context("PrebuiltOS has no linux case")?
        .1;
    // the linux arm ends at the next GOOS case, the first `case` indented one tab like it
    let linux = &linux[..linux.find("\n\tcase ").unwrap_or(linux.len())];
    let out: Vec<String> = linux
        .split("return \"")
        .skip(1)
        .filter_map(|s| s.split_once('"').map(|(v, _)| v.to_owned()))
        .collect();
    ensure!(!out.is_empty(), "PrebuiltOS returns nothing for linux");
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::{host_row, kernel_arch_matches, linux_prebuilt_os, parse, qemu_target};

    #[test]
    fn host_row_by_rust_triple() {
        let host = |t: &str| format!("rust = \"{t}\"\nmeson_cpu_family = \"m\"\ndebian = \"d\"\n");
        let arches = parse(&format!(
            "[a]\npointer_bits = 64\n{K}[a.host]\n{}\n[b]\npointer_bits = 64\n{K}[b.host]\n{}\n[c]\npointer_bits = 32\n{K}[c.guest]\n{GUEST}",
            host("ta"),
            host("tb")
        ))
        .unwrap();
        assert_eq!(host_row(&arches, "tb").unwrap().0, "b");
        // a guest-only row or an unknown triple: no row
        assert!(host_row(&arches, "tc").is_err());
    }

    #[test]
    fn prebuilt_os_from_soong() {
        // build/soong/android/config.go's shape: GOOS switch, linux arm switching on GOARCH
        let go = "func (c *config) PrebuiltOS() string {\n\tswitch runtime.GOOS {\n\tcase \"linux\":\n\t\tswitch runtime.GOARCH {\n\t\tcase \"amd64\":\n\t\t\treturn \"linux-x86\"\n\t\tcase \"arm64\":\n\t\t\treturn \"linux-arm64\"\n\t\tdefault:\n\t\t\tpanic(1)\n\t\t}\n\tcase \"darwin\":\n\t\treturn \"darwin-x86\"\n\t}\n}\n";
        assert_eq!(linux_prebuilt_os(go).unwrap(), ["linux-x86", "linux-arm64"]);
        assert!(linux_prebuilt_os("func other() {}").is_err());
    }

    #[test]
    fn isa_extensions_past_the_rv64gc_base() {
        assert_eq!(
            super::isa_extensions("rv64gc_zba_zbb").unwrap(),
            ["zba", "zbb"]
        );
        assert!(super::isa_extensions("rv64gcv_zba_zbb").is_err());
        assert!(super::isa_extensions("rv64gc_v").is_err());
        assert!(super::isa_extensions("rv64gc_Zba").is_err());
    }

    #[test]
    fn isa_closure_adds_the_implied_extensions() {
        // written from the ISA manual's rules (G, M, A, F, D, C), not computed by isa_closure
        let want = [
            "a", "c", "d", "f", "i", "m", "zaamo", "zalrsc", "zba", "zbb", "zca", "zcd", "zicsr",
            "zifencei", "zmmul",
        ];
        let got: Vec<String> = super::isa_closure("rv64gc_zba_zbb")
            .unwrap()
            .into_iter()
            .collect();
        assert_eq!(got, want);
    }

    #[test]
    fn dotted_versions_compare_as_vectors() {
        assert_eq!(super::dotted_version("2.39").unwrap(), [2, 39]);
        assert!(super::dotted_version("2.41").unwrap() > super::dotted_version("2.39").unwrap());
        assert!(super::dotted_version("2.").is_err());
    }

    #[test]
    fn cross_target_needs_all_three_columns_off_the_build_machine() {
        let row = |clang: Option<&str>, sysroot: Option<&str>, isa: Option<&str>| super::Host {
            rust: "t".into(),
            meson_cpu_family: "m".into(),
            debian: "d".into(),
            archlinux: None,
            soong_prebuilt_os: None,
            isa: isa.map(Into::into),
            clang: clang.map(Into::into),
            sysroot: sysroot.map(Into::into),
            glibc_floor: None,
        };
        // the build machine's own row is native, whatever it lists
        assert_eq!(
            super::cross_target("x86_64", &row(None, None, None), "x86_64").unwrap(),
            None
        );
        // another arch takes its row's clang, sysroot and isa
        let riscv = row(
            Some("riscv64-linux-gnu"),
            Some("riscv64-sysroot"),
            Some("rv64gc_zba_zbb"),
        );
        assert_eq!(
            super::cross_target("riscv64", &riscv, "x86_64").unwrap(),
            Some(super::CrossTarget {
                clang: "riscv64-linux-gnu".into(),
                sysroot: "riscv64-sysroot".into(),
                isa: "rv64gc_zba_zbb".into(),
            })
        );
        // another arch with no or a partial row: an error, never the build machine's flags
        assert!(super::cross_target("arm64", &row(None, None, None), "x86_64").is_err());
        assert!(
            super::cross_target("riscv64", &row(Some("c"), None, Some("rv64gc")), "x86_64")
                .is_err()
        );
    }

    const GUEST: &str = "android_abis = [\"x\"]\nclang = \"c\"\nqemu_target = \"q\"\n";
    const K: &str = "kernel_arch = \"k\"\nuapi_records = [\"r\"]\n";

    #[test]
    fn kernel_arch_against_bionic() {
        let bp = "cc_library_headers {\n    name: \"libc_uapi_headers\",\n    arch: {\n        a: {\n            export_system_include_dirs: [\"kernel/uapi/asm-ka\"],\n        },\n    },\n}\n\ncc_library_headers {\n    name: \"other\",\n    arch: {\n        a: {\n            export_system_include_dirs: [\"kernel/uapi/asm-wrong\"],\n        },\n    },\n}\n";
        let row = |k: &str| {
            parse(&format!(
                "[a]\npointer_bits = 32\nkernel_arch = \"{k}\"\nuapi_records = [\"r\"]\n[a.guest]\n{GUEST}"
            ))
            .unwrap()
        };
        kernel_arch_matches(bp, &row("ka")).unwrap();
        // another module's block does not count
        assert!(kernel_arch_matches(bp, &row("wrong")).is_err());
    }

    fn err(text: &str) -> String {
        format!("{:#}", parse(text).expect_err("should be rejected"))
    }

    #[test]
    fn minimal_guest_row() {
        let arches = parse(&format!("[a]\npointer_bits = 32\n{K}[a.guest]\n{GUEST}")).unwrap();
        assert_eq!(arches["a"].pointer_bits, 32);
        assert!(arches["a"].host.is_none());
    }

    #[test]
    fn qemu_target_is_the_guest_column() {
        let arches = parse(&format!(
            "[a]\npointer_bits = 64\n{K}[a.guest]\nandroid_abis = [\"x\"]\nclang = \"c\"\nqemu_target = \"aarch64\"\n[b]\npointer_bits = 64\n{K}[b.host]\nrust = \"t\"\nmeson_cpu_family = \"m\"\ndebian = \"d\"\n"
        ))
        .unwrap();
        assert_eq!(qemu_target(&arches, "a").unwrap(), "aarch64");
        // a host-only row has no QEMU target, an unknown name no row
        assert!(qemu_target(&arches, "b").is_err());
        assert!(qemu_target(&arches, "c").is_err());
    }

    #[test]
    fn row_without_role() {
        assert!(err(&format!("[a]\npointer_bits = 64\n{K}")).contains("neither"));
    }

    #[test]
    fn uapi_records_column_is_required() {
        let text = format!("[a]\npointer_bits = 32\nkernel_arch = \"k\"\n[a.guest]\n{GUEST}");
        assert!(err(&text).contains("uapi_records"));
    }

    #[test]
    fn odd_pointer_width() {
        let text = format!("[a]\npointer_bits = 48\n{K}[a.guest]\n{GUEST}");
        assert!(err(&text).contains("pointer_bits 48"));
    }

    #[test]
    fn unknown_column() {
        let text = format!("[a]\npointer_bits = 64\n{K}priority = 1\n[a.guest]\n{GUEST}");
        assert!(err(&text).contains("unknown field `priority`"));
    }

    #[test]
    fn abi_in_two_rows() {
        let text = format!(
            "[a]\npointer_bits = 64\n{K}[a.guest]\n{GUEST}[b]\npointer_bits = 64\n{K}[b.guest]\n{GUEST}"
        );
        assert!(err(&text).contains("listed by both a and b"));
    }
}
