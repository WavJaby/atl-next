// SPDX-License-Identifier: MIT
//! `xtask check` step `seam`: a static scan of libbinder's sources (patches/aosp/frameworks/native 0003) -- every driver call goes through `BinderDriver.h`. Text only; the runtime side is `xtask test binder` (binder.rs).

use std::path::Path;

use anyhow::{Context, Result, ensure};
use atl_layout::Layout;

/// libbinder's files holding the driver I/O (patches/aosp/frameworks/native 0003), relative to `src/`.
const SEAM_FILES: &[&str] = &[
    "aosp/frameworks/native/libs/binder/ProcessState.cpp",
    "aosp/frameworks/native/libs/binder/IPCThreadState.cpp",
];

/// Lines of `text` that do driver I/O past the seam: a raw `ioctl(` / `mmap(` / `munmap(` anywhere (every one in these files is on the driver), or a raw `open(` / `close(` on a line naming the driver (`mDriverFD`, `driver`). `binder_driver_*` calls and other identifiers ending in these names (`::close`, `fclose`) are not raw.
/// Limit: a driver fd copied into another variable first and closed on a later line is not seen; review catches it, the gate guards the patched one-line sites.
fn seam_violations(text: &str) -> Vec<(usize, String)> {
    let raw = |line: &str, call: &str| {
        line.match_indices(call).any(|(i, _)| {
            line[..i].chars().next_back().is_none_or(|c| {
                !(c.is_alphanumeric() || c == '_' || c == ':' || c == '.' || c == '>')
            })
        })
    };
    text.lines()
        .enumerate()
        .filter(|(_, l)| {
            // code only: without the line comment and the text of string literals (log messages name ioctl())
            let mut code = String::new();
            let mut quoted = false;
            let mut prev = ' ';
            for c in l.split("//").next().unwrap_or_default().chars() {
                if c == '"' && prev != '\\' {
                    quoted = !quoted;
                } else if !quoted {
                    code.push(c);
                }
                prev = c;
            }
            let code = code.as_str();
            ["ioctl(", "mmap(", "munmap("].iter().any(|c| raw(code, c))
                || ((code.contains("mDriverFD") || code.contains("driver"))
                    && ["open(", "close("].iter().any(|c| raw(code, c)))
        })
        .map(|(i, l)| (i + 1, l.trim().to_owned()))
        .collect()
}

/// `xtask check` step `seam`: no driver I/O in libbinder bypasses `BinderDriver.h`, in the tree this checkout builds (task copy or `src/`).
pub fn check(root: &Path) -> Result<()> {
    let layout = Layout::of(root)?;
    let mut bad = Vec::new();
    for f in SEAM_FILES {
        let path = layout.locate(Path::new(f)).dir;
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("{}: cargo xtask fetch --execute", path.display()))?;
        for (n, l) in seam_violations(&text) {
            bad.push(format!("{}:{n}: {l}", path.display()));
        }
    }
    ensure!(
        bad.is_empty(),
        "driver I/O past BinderDriver.h:\n{}",
        bad.join("\n")
    );
    eprintln!(
        "seam: ok ({} files, every driver call through binder_driver_*)",
        SEAM_FILES.len()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::seam_violations;

    #[test]
    fn raw_driver_calls_fire() {
        let ok = "x = binder_driver_ioctl(mDriverFD, BINDER_WRITE_READ, &bwr);\nbinder_driver_close(mDriverFD);\n    close(fd); // feature file\n";
        assert!(seam_violations(ok).is_empty());
        let bad = "if (ioctl(mDriverFD, BINDER_WRITE_READ, &bwr) >= 0)\n  close(gProcess->mDriverFD);\n p = mmap(nullptr, 1, 0, 0, fd, 0);\n";
        assert_eq!(
            seam_violations(bad).iter().map(|v| v.0).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
    }
}
