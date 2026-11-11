// SPDX-License-Identifier: MIT
//! The binary with a controlled PATH: missing switcherooctl -> named error, never a fallback; a stub switcherooctl receives the documented argv; `desktop` writes the key iff discrete.
#![expect(
    clippy::disallowed_types,
    reason = "the test runs the launcher binary and the validator as child processes"
)]
#![expect(
    clippy::unwrap_used,
    reason = "integration test: a failed setup step is the failure"
)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_atl-launch")
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("atl-launch-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn script(dir: &Path, name: &str, body: &str) {
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn discrete_without_switcherooctl_fails_naming_it() {
    let dir = scratch("noswitch");
    // a PATH holding only a `marker` command: had it fallen back to running the command directly, the marker file would appear
    script(&dir, "marker", &format!("touch {}/ran", dir.display()));
    let out = Command::new(bin())
        .env("PATH", &dir)
        .args(["run", "--gpu=discrete", "--", "marker"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "{stderr}");
    assert!(stderr.contains("switcherooctl"), "{stderr}");
    assert!(
        !dir.join("ran").exists(),
        "fell back to running the command"
    );
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn discrete_execs_switcherooctl_launch_with_the_command() {
    let dir = scratch("stub");
    script(&dir, "switcherooctl", "printf '%s\\n' \"$@\"");
    let out = Command::new(bin())
        .env("PATH", &dir)
        .args(["run", "--gpu=discrete", "--", "atld", "--root", "a b"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "launch\natld\n--root\na b\n"
    );
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn default_runs_the_command_directly() {
    let dir = scratch("default");
    script(&dir, "switcherooctl", "exit 99");
    let out = Command::new(bin())
        .env("PATH", &dir)
        .args(["run", "--", "/bin/echo", "--gpu=x"])
        .output()
        .unwrap();
    assert!(out.status.success());
    assert_eq!(String::from_utf8_lossy(&out.stdout), "--gpu=x\n");
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn desktop_entry_carries_the_key_iff_discrete() {
    let dir = scratch("desktop");
    let settings = dir.join("apps.toml");
    let run = |args: &[&str]| {
        let out = Command::new(bin()).args(args).output().unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    let s = settings.to_str().unwrap();
    let o = dir.to_str().unwrap();
    run(&["set-gpu", "--settings", s, "com.example.game", "discrete"]);
    run(&[
        "desktop",
        "--settings",
        s,
        "--out",
        o,
        "--name",
        "Game",
        "com.example.game",
        "--",
        "atld",
        "--package",
        "game",
    ]);
    run(&[
        "desktop",
        "--settings",
        s,
        "--out",
        o,
        "--name",
        "Other",
        "com.example.other",
        "--",
        "atld",
    ]);
    let game = std::fs::read_to_string(dir.join("atl.com.example.game.desktop")).unwrap();
    let other = std::fs::read_to_string(dir.join("atl.com.example.other.desktop")).unwrap();
    assert!(
        game.lines().any(|l| l == "PrefersNonDefaultGPU=true"),
        "{game}"
    );
    assert!(
        game.lines()
            .any(|l| l == "Exec=atl-launch run -- atld --package game"),
        "{game}"
    );
    assert!(!other.contains("PrefersNonDefaultGPU"), "{other}");
    // desktop-file-validate where installed (not in WSL Debian at the time of writing; nothing is installed for this test)
    let validator = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|d| d.join("desktop-file-validate"))
        .find(|p| p.exists());
    if let Some(v) = validator {
        for f in [
            "atl.com.example.game.desktop",
            "atl.com.example.other.desktop",
        ] {
            let out = Command::new(&v).arg(dir.join(f)).output().unwrap();
            assert!(
                out.status.success() && out.stdout.is_empty(),
                "{f}: {}",
                String::from_utf8_lossy(&out.stdout)
            );
        }
    }
    std::fs::remove_dir_all(dir).unwrap();
}
