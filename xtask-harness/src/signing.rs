// SPDX-License-Identifier: MIT
//! `xtask test signing`: the converter signs each product APK with the key set role its `certificate` names (bp2ninja app.go appCertificate, signApk) and installs it below `<out>/partitions` (installDir).
//! - framework-res (`certificate: "platform"`) at `system/framework/framework-res.apk` carries the platform role's certificate;
//! - the repo bind's fixture (`java/signing-fixture`, seen at `src/aosp/vendor/atl`, no certificate = the default role) at `system/app/AtlSigningFixture/AtlSigningFixture.apk` carries testkey's.
//!
//! Both sides come from tools that are not ours: the APK's signer digest from AOSP's apksigner (`verify --print-certs`), each role certificate's from `openssl x509 -outform DER | sha256`.
//! Control every run: the two roles' digests differ, so a role swap is observable. `--break swap-role` expects each APK to carry the other role and must turn the gate red.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};

use anyhow::{Context, Result, ensure};

use crate::{build_state, host_art, keys};

const CMD: &str = "signing";
/// gate_goals.rs APK_SIGNER as the converter installs its jar (atld_run.rs Signer).
const APK_SIGNER_JAR: &str = "host/apksigner/withres/apksigner.jar";
const APK_SIGNER_MAIN: &str = "com.android.apksigner.ApkSignerTool";
const BREAK_SWAP: &str = "swap-role";

#[derive(clap::Args)]
pub struct Args {
    /// The host arch (an arches.toml row with `[host]`).
    #[arg(long)]
    host: String,
    /// Native build directory (default: this checkout's slot; else <workspace>/out/native/<host>).
    #[arg(long)]
    out: Option<PathBuf>,
    /// Key set the build was signed with (default: the workspace's dev set, as `native` defaults).
    #[arg(long)]
    keys: Option<PathBuf>,
    /// `swap-role`: expect each APK to carry the other role's certificate; the gate must fail.
    #[arg(long, value_parser = [BREAK_SWAP])]
    r#break: Option<String>,
}

pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    let out = build_state::gate_out(root, args.out.as_deref(), &args.host, CMD)?;
    let dir = match &args.keys {
        Some(d) => d.clone(),
        None => keys::dev_dir(&atl_layout::Layout::of(root)?),
    };
    keys::check_set(&dir)?;
    let (_, jdk) = host_art::java_tools(root, &args.host, &out)?;
    let java = jdk.join("bin/java");
    let jar = out.join(APK_SIGNER_JAR);
    ensure!(jar.is_file(), "missing input {}", jar.display());
    let platform = role_digest(&dir, "platform")?;
    let testkey = role_digest(&dir, "testkey")?;
    ensure!(
        platform != testkey,
        "control: the platform and testkey roles share a certificate ({platform}): a role swap would not show"
    );
    println!("signing: PASS control: platform and testkey certificates differ");
    let swap = args.r#break.is_some();
    let cases = [
        (
            "framework-res",
            "system/framework/framework-res.apk",
            "platform",
            &platform,
            &testkey,
        ),
        (
            "AtlSigningFixture",
            "system/app/AtlSigningFixture/AtlSigningFixture.apk",
            "testkey",
            &testkey,
            &platform,
        ),
    ];
    for (module, path, role, own, other) in cases {
        let apk = out.join("partitions").join(path);
        ensure!(apk.is_file(), "missing input {}", apk.display());
        let got = apk_digest(&java, &jar, &apk)?;
        let want = if swap { other } else { own };
        ensure!(
            got == *want,
            "{module}: signer SHA-256 {got}, want the {}role certificate {want}",
            if swap { "swapped " } else { "" }
        );
        println!("signing: PASS {module}: /{path} signed with the {role} role ({got})");
    }
    Ok(ExitCode::SUCCESS)
}

/// `apksigner verify --print-certs`' `Signer #1 certificate SHA-256 digest`.
fn apk_digest(java: &Path, jar: &Path, apk: &Path) -> Result<String> {
    let text = host_art::output(
        Command::new(java)
            .arg("-cp")
            .arg(jar)
            .arg(APK_SIGNER_MAIN)
            .args(["verify", "--print-certs"])
            .arg(apk),
        "apksigner verify",
    )?;
    text.lines()
        .find_map(|l| l.strip_prefix("Signer #1 certificate SHA-256 digest: "))
        .map(|h| h.trim().to_owned())
        .context("apksigner verify printed no Signer #1 SHA-256 digest")
}

/// SHA-256 (lower-case hex, as apksigner prints it) of the role certificate's DER: `openssl x509 -outform DER`, then `openssl dgst -sha256 -r`.
fn role_digest(dir: &Path, role: &str) -> Result<String> {
    let (pem, _) = keys::role_files(dir, role);
    let der = Command::new("openssl")
        .args(["x509", "-outform", "DER", "-in"])
        .arg(&pem)
        .output()
        .context("running openssl x509")?;
    ensure!(
        der.status.success(),
        "openssl x509 {} failed",
        pem.display()
    );
    let mut child = Command::new("openssl")
        .args(["dgst", "-sha256", "-r"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .context("running openssl dgst")?;
    child
        .stdin
        .take()
        .context("openssl dgst stdin")?
        .write_all(&der.stdout)?;
    let out = child.wait_with_output()?;
    ensure!(out.status.success(), "openssl dgst failed");
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .next()
        .map(str::to_owned)
        .context("openssl dgst printed nothing")
}
