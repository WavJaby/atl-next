// SPDX-License-Identifier: MIT
//! Test support: writes the executables a test then exec's.
//! Why: a fork of another test thread inherits a write fd until its own exec; an exec of the file in that window fails with ETXTBSY.
#![forbid(unsafe_code)]
#![expect(
    clippy::disallowed_types,
    reason = "test support: runs sh to write the file; runtime processes start through atl_broker, never from this crate"
)]

use std::path::Path;
use std::process::Command;

/// Writes `text` to `path` as an executable (mode 0755) with no write fd of this process on it: `sh` writes and exits before the exec.
/// Callers exec the file as usual, so the kernel's mode and shebang checks apply.
pub fn write_exe(path: &Path, text: &str) {
    use std::os::unix::fs::PermissionsExt;
    let out = Command::new("sh")
        .args(["-c", "printf '%s' \"$2\" > \"$1\"", "sh"])
        .arg(path)
        .arg(text)
        .output()
        .expect("running sh to write an executable");
    assert!(
        out.status.success(),
        "writing {}: {}",
        path.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
        .expect("chmod an executable");
}

#[cfg(test)]
mod tests {
    use super::write_exe;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::process::Command;

    fn scratch(name: &str) -> PathBuf {
        let d =
            std::env::temp_dir().join(format!("atl-exec-fixture-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).expect("scratch");
        d
    }

    /// The mode bits and the shebang are asserted apart from the exec; the exec then returns the script's own status.
    #[test]
    fn writes_an_executable_script() {
        let d = scratch("exec");
        let exe = d.join("tool");
        write_exe(&exe, "#!/bin/sh\nexit 3\n");
        let mode = std::fs::metadata(&exe).expect("stat").permissions().mode() & 0o777;
        assert_eq!(mode, 0o755);
        assert!(
            std::fs::read_to_string(&exe)
                .expect("read")
                .starts_with("#!/bin/sh\n")
        );
        assert_eq!(Command::new(&exe).status().expect("exec").code(), Some(3));
        std::fs::remove_dir_all(&d).expect("cleanup");
    }

    /// A second write replaces the first (the redirect truncates).
    #[test]
    fn rewrites_an_existing_file() {
        let d = scratch("rewrite");
        let exe = d.join("tool");
        write_exe(&exe, "#!/bin/sh\nexit 1\n");
        write_exe(&exe, "#!/bin/sh\nexit 0\n");
        assert_eq!(Command::new(&exe).status().expect("exec").code(), Some(0));
        std::fs::remove_dir_all(&d).expect("cleanup");
    }
}
