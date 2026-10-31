// SPDX-License-Identifier: MIT
//! What a consuming crate's `build.rs` needs around the generator: the checkout and shared workspace it
//! builds in, and the arches.toml row of the target cargo is building for. Generation always targets
//! cargo's `TARGET`; one checked target per `cargo check --target` run.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};

use crate::Target;

/// The arches.toml row of a host target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostArch {
    /// arches.toml key.
    pub arch: String,
    /// Linux kernel arch (`asm-<kernel_arch>` uapi directory).
    pub kernel_arch: String,
}

/// The checkout holding `manifest_dir`: the nearest ancestor with arches.toml.
pub fn checkout(manifest_dir: &Path) -> Result<PathBuf> {
    manifest_dir
        .ancestors()
        .find(|d| d.join("arches.toml").is_file())
        .map(Path::to_path_buf)
        .with_context(|| format!("no arches.toml above {}", manifest_dir.display()))
}

/// The shared workspace (`~/atl-next`): parent of the main checkout, found through the git common dir
/// so every worktree resolves the same `src/`. Same rule as xtask's `workspace::root_dir`.
pub fn workspace(checkout: &Path) -> Result<PathBuf> {
    let out = std::process::Command::new("git")
        .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
        .current_dir(checkout)
        .output()
        .context("running git rev-parse")?;
    ensure!(
        out.status.success(),
        "git rev-parse: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let common = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
    // <workspace>/repo/.git -> <workspace>
    common
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .with_context(|| format!("{} has no workspace parent", common.display()))
}

/// A fetched upstream file, failing with the fetch command when it is absent.
pub fn upstream_file(workspace: &Path, rel: &str) -> Result<PathBuf> {
    let path = workspace.join("src").join(rel);
    ensure!(
        path.is_file(),
        "{} missing: run `cargo xtask fetch --execute`",
        path.display()
    );
    Ok(path)
}

/// The row whose `[<arch>.host] rust` is `rust_target`, read from arches.toml's text.
pub fn host_arch(arches_toml: &str, rust_target: &str) -> Result<HostArch> {
    let table: toml::Table = toml::from_str(arches_toml).context("parsing arches.toml")?;
    let (arch, row) = table
        .iter()
        .find(|(_, row)| {
            row.get("host")
                .and_then(|h| h.get("rust"))
                .and_then(|r| r.as_str())
                == Some(rust_target)
        })
        .with_context(|| format!("arches.toml: no host row with rust = {rust_target}"))?;
    let kernel_arch = row
        .get("kernel_arch")
        .and_then(|k| k.as_str())
        .with_context(|| format!("arches.toml [{arch}]: no kernel_arch"))?;
    Ok(HostArch {
        arch: arch.clone(),
        kernel_arch: kernel_arch.to_owned(),
    })
}

/// clang's triple for the target cargo builds for, from cargo's `CARGO_CFG_TARGET_*` (cargo's own
/// triple is not always clang's: `riscv64gc-unknown-linux-gnu` is `riscv64-unknown-linux-gnu`).
pub fn clang_triple_from_cargo() -> Result<String> {
    let cfg = |k: &str| {
        std::env::var(format!("CARGO_CFG_TARGET_{k}")).with_context(|| format!("no TARGET_{k}"))
    };
    Ok(format!(
        "{}-{}-{}-{}",
        cfg("ARCH")?,
        cfg("VENDOR")?,
        cfg("OS")?,
        cfg("ENV")?
    ))
}

/// bionic's kernel uapi include directories for `host`, as Soong's `libc_uapi_headers` exports them:
/// the generic uapi, Android's additions (`linux/compiler_types.h`), the arch's `asm-<kernel_arch>`.
pub fn bionic_uapi_dirs(aosp: &Path, host: &HostArch) -> Vec<PathBuf> {
    let kernel = aosp.join("bionic/libc/kernel");
    vec![
        kernel.join("uapi"),
        kernel.join("android/uapi"),
        kernel
            .join("uapi")
            .join(format!("asm-{}", host.kernel_arch)),
    ]
}

/// The target to parse a bionic kernel-uapi header for `host`: no system headers, only
/// `bionic_uapi_dirs`.
pub fn bionic_uapi_target(aosp: &Path, host: &HostArch, clang_triple: &str) -> Target {
    let mut args = vec!["-nostdlibinc".to_owned()];
    args.extend(
        bionic_uapi_dirs(aosp, host)
            .iter()
            .map(|d| format!("-isystem{}", d.display())),
    );
    Target {
        arch: host.arch.clone(),
        triple: clang_triple.to_owned(),
        args,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_row_by_rust_triple() {
        let text = "[a]\nkernel_arch = \"ka\"\n[a.host]\nrust = \"a-unknown-linux-gnu\"\n[b]\nkernel_arch = \"kb\"\n[b.guest]\nclang = \"b-linux-android\"\n";
        assert_eq!(
            host_arch(text, "a-unknown-linux-gnu").unwrap(),
            HostArch {
                arch: "a".into(),
                kernel_arch: "ka".into()
            }
        );
        // a guest-only row is never a host
        assert!(host_arch(text, "b-linux-android").is_err());
    }
}
