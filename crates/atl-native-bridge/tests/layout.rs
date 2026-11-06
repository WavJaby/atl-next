// SPDX-License-Identifier: MIT
//! The generated layout asserts bite: the records part of build.rs's output compiles as generated, and fails to compile once one field is given a wrong type (bridge control 2, plan S11).
#![allow(
    clippy::unwrap_used,
    reason = "test helpers outside #[test] functions; a failure is the test failing"
)]
#![expect(clippy::disallowed_types, reason = "a test: it runs rustc on the host")]

use std::path::PathBuf;
use std::process::Command;

/// The records, constants and their layout asserts: the generated file up to the first entry shell (shells call `crate::exports`, which a standalone compile lacks).
fn records() -> String {
    let code = std::fs::read_to_string(concat!(env!("OUT_DIR"), "/native_bridge.rs")).unwrap();
    let shell = code
        .find("unsafe extern \"C\" fn shell_")
        .expect("generated shells");
    // back to the start of the shell's attribute line
    let end = code[..shell]
        .trim_end_matches('\n')
        .rfind("\n#[")
        .expect("shell attribute");
    code[..end].to_owned()
}

/// Whether rustc accepts `src` as a library.
fn compiles(name: &str, src: &str) -> bool {
    let dir = std::env::temp_dir().join(format!("atl-nb-layout-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file: PathBuf = dir.join(format!("{name}.rs"));
    std::fs::write(&file, src).unwrap();
    let ok = Command::new("rustc")
        .args([
            "--edition",
            "2024",
            "--crate-type",
            "lib",
            "-A",
            "warnings",
            "-o",
        ])
        .arg(dir.join(format!("lib{name}.rlib")))
        .arg(&file)
        .output()
        .unwrap()
        .status
        .success();
    let _ = std::fs::remove_dir_all(&dir);
    ok
}

#[test]
fn wrong_layout_fails_to_compile() {
    let good = records();
    assert!(
        good.contains("offset_of!(NativeBridgeCallbacks, loadLibrary)"),
        "no layout asserts in the generated records"
    );
    assert!(compiles("good", &good));
    // `version` widened to 16 bytes (8 would still fit the padding before `initialize` at 8 on LP64): every later field's offset moves past clang's
    let wrong = good.replacen("pub version: u32,", "pub version: [u64; 2],", 1);
    assert_ne!(wrong, good);
    assert!(!compiles("wrong", &wrong));
}
