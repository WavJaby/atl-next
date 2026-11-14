// SPDX-License-Identifier: MIT
//! The key set product APKs are signed with: Soong's DefaultAppCertificateDir layout, `<role>.x509.pem` + `<role>.pk8` (PKCS#8 DER, unencrypted) per `certificate` role, the converter's `-keys` (bp2ninja app.go appCertificate).
//! Dev builds: one set per workspace, `<workspace>/keys`, shared by every slot so a slot change never changes a signer; made once by `xtask keys --execute` as AOSP's development/tools/make_key makes a key (openssl genrsa -f4 2048, req -x509 -sha256 -days 10000, pkcs8 -topk8 -nocrypt).
//! Never AOSP's published keys (build/make/target/product/security): our PackageManager grants `android`'s signature permissions to every APK signed like `android`, so a published platform key would hand them to any downloaded APK signed with it.
//! Never the slot's `test-keys/debug.keystore` either: that is the fixture apps' signer; a fixture signed like the platform would void every signature-permission gate.
//! Release sets live wherever their holder keeps them (owner decision open): `native --keys <dir>` takes any set of this layout; nothing here stores or names one.

use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use anyhow::{Context, Result, bail, ensure};
use atl_layout::Layout;

use crate::host_art;

/// The Soong `certificate` roles product modules use: `platform` (framework-res), `testkey` (the default role, DefaultAppCertificate). A row is added when a module first needs one (`shared`, `media`, ...).
pub const ROLES: [&str; 2] = ["platform", "testkey"];

/// gen_build_prop.py TEST_KEY_DIR: AOSP's published keys, refused as a key set.
const AOSP_KEY_DIR: &str = "build/make/target/product/security";

/// make_key's key: RSA, public exponent F4 (65537), 2048 bits.
const RSA_BITS: &str = "2048";
/// make_key's certificate validity in days.
const VALIDITY_DAYS: &str = "10000";

#[derive(clap::Args)]
pub struct Args {
    /// Key set dir (default: the workspace's dev set, <workspace>/keys).
    #[arg(long)]
    dir: Option<PathBuf>,
    /// Make the roles the set lacks (openssl). Without it: prints which roles exist and which would be made.
    #[arg(long)]
    execute: bool,
}

/// The workspace's dev key set dir.
pub fn dev_dir(layout: &Layout) -> PathBuf {
    layout.workspace().join("keys")
}

/// A role's certificate and key in `dir`.
pub fn role_files(dir: &Path, role: &str) -> (PathBuf, PathBuf) {
    (
        dir.join(format!("{role}.x509.pem")),
        dir.join(format!("{role}.pk8")),
    )
}

/// `dir` is a usable key set: not AOSP's published one, and every role of [`ROLES`] has both files.
pub fn check_set(dir: &Path) -> Result<()> {
    ensure!(
        !dir.ends_with(AOSP_KEY_DIR),
        "{} holds AOSP's published keys: never sign product APKs with them (any APK signed with the published platform key would be signed like the platform)",
        dir.display()
    );
    let missing: Vec<&str> = ROLES
        .iter()
        .copied()
        .filter(|r| {
            let (pem, key) = role_files(dir, r);
            !(pem.is_file() && key.is_file())
        })
        .collect();
    ensure!(
        missing.is_empty(),
        "key set {} lacks role(s) {}: cargo xtask keys --execute{}",
        dir.display(),
        missing.join(", "),
        if missing.len() == ROLES.len() {
            " makes the workspace's dev set"
        } else {
            ""
        }
    );
    Ok(())
}

pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    let dir = match &args.dir {
        Some(d) => d.clone(),
        None => dev_dir(&Layout::of(root)?),
    };
    ensure!(
        !dir.ends_with(AOSP_KEY_DIR),
        "{} holds AOSP's published keys",
        dir.display()
    );
    let mut made = 0;
    for role in ROLES {
        let (pem, key) = role_files(&dir, role);
        match (pem.is_file(), key.is_file()) {
            (true, true) => println!("keys: {role}: present"),
            (false, false) if args.execute => {
                make_role(&dir, role)?;
                made += 1;
                println!("keys: {role}: made");
            }
            (false, false) => println!("keys: {role}: missing (--execute makes it)"),
            _ => bail!(
                "keys: {role}: only one of {} and {} exists: a half-made role is never completed (remove it, then --execute)",
                pem.display(),
                key.display()
            ),
        }
    }
    if args.execute {
        check_set(&dir)?;
        println!("keys: ok ({made} made) {}", dir.display());
    }
    Ok(ExitCode::SUCCESS)
}

/// One role as make_key makes it; the dir is created 0700 and the key file left 0600. The private key is written to temporary names and renamed last, certificate first, so a reader never sees a role with a key but no certificate.
fn make_role(dir: &Path, role: &str) -> Result<()> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .with_context(|| format!("creating {}", dir.display()))?;
    let (pem, key) = role_files(dir, role);
    let tmp = |p: &Path| p.with_extension(format!("tmp{}", std::process::id()));
    let rsa = dir.join(format!("{role}.rsa.tmp{}", std::process::id()));
    let result = (|| -> Result<()> {
        host_art::exec(
            Command::new("openssl")
                .args(["genrsa", "-f4", "-out"])
                .arg(&rsa)
                .arg(RSA_BITS),
            "openssl genrsa",
        )?;
        std::fs::set_permissions(&rsa, std::fs::Permissions::from_mode(0o600))?;
        let subject = format!("/O=ATL/OU=workspace dev key set/CN={role}");
        host_art::exec(
            Command::new("openssl")
                .args(["req", "-new", "-x509", "-sha256", "-key"])
                .arg(&rsa)
                .arg("-out")
                .arg(tmp(&pem))
                .args(["-days", VALIDITY_DAYS, "-subj", &subject]),
            "openssl req -x509",
        )?;
        host_art::exec(
            Command::new("openssl")
                .args(["pkcs8", "-topk8", "-outform", "DER", "-nocrypt", "-in"])
                .arg(&rsa)
                .arg("-out")
                .arg(tmp(&key)),
            "openssl pkcs8",
        )?;
        std::fs::set_permissions(tmp(&key), std::fs::Permissions::from_mode(0o600))?;
        std::fs::rename(tmp(&pem), &pem)?;
        std::fs::rename(tmp(&key), &key)?;
        Ok(())
    })();
    for p in [rsa, tmp(&pem), tmp(&key)] {
        let _ = std::fs::remove_file(p);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    /// AOSP's own dir is refused by its tree path, whatever the prefix; a dir lacking a role names the command that makes it.
    #[test]
    fn check_set_refuses_aosp_and_incomplete_sets() {
        let aosp = Path::new("/ws/src/aosp").join(AOSP_KEY_DIR);
        let e = format!("{:#}", check_set(&aosp).unwrap_err());
        assert!(e.contains("AOSP's published keys"), "{e}");
        let dir = std::env::temp_dir().join(format!("xtask-keys-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let e = format!("{:#}", check_set(&dir).unwrap_err());
        assert!(e.contains("lacks role(s) platform, testkey"), "{e}");
        for role in ROLES {
            let (pem, key) = role_files(&dir, role);
            std::fs::write(pem, "").unwrap();
            std::fs::write(key, "").unwrap();
        }
        check_set(&dir).unwrap();
        std::fs::remove_file(role_files(&dir, "platform").1).unwrap();
        let e = format!("{:#}", check_set(&dir).unwrap_err());
        assert!(e.contains("lacks role(s) platform:"), "{e}");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
