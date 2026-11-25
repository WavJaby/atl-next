// SPDX-License-Identifier: MIT
//! Putting a package's staged code in place: one syscall, so no state lies between the old code and the new (P4b D4).

use anyhow::{Context, Result, anyhow};
use rustix::fs::{CWD, RenameFlags, renameat_with};
use rustix::io::Errno;
use std::path::Path;

/// Puts the staged code dir `staged` at `code`. `code` exists: an atomic RENAME_EXCHANGE, and true = the old code is now at `staged`. Else `staged` is renamed to `code`, false.
/// A filesystem without RENAME_EXCHANGE refuses the update, naming it; no second path: two renames would leave a state between the old code and the new.
pub fn replace(staged: &Path, code: &Path) -> Result<bool> {
    if !code.exists() {
        let parent = code.parent().context("package dir has no parent")?;
        std::fs::create_dir_all(parent)?;
        std::fs::rename(staged, code)
            .with_context(|| format!("moving {} to {}", staged.display(), code.display()))?;
        return Ok(false);
    }
    renameat_with(CWD, staged, CWD, code, RenameFlags::EXCHANGE).map_err(|e| match e {
        Errno::INVAL | Errno::NOSYS => anyhow!(
            "{}: its filesystem refuses RENAME_EXCHANGE ({e}); a package update needs it and no other path is taken",
            code.display()
        ),
        e => anyhow!("exchanging {} with {}: {e}", staged.display(), code.display()),
    })?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::replace;
    use std::path::PathBuf;

    /// A fresh dir for `test` under the temp dir; the test removes it.
    fn dir(test: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("atld-code-swap-{test}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("the test dir");
        dir
    }

    #[test]
    fn replace_swaps_whole_dirs() {
        let root = dir("swap");
        let (staged, code) = (root.join("vmdl1.tmp"), root.join("apps/p/apk"));
        std::fs::create_dir_all(staged.join("lib")).expect("the staged code");
        std::fs::write(staged.join("new.apk"), "new").expect("the new code");
        std::fs::create_dir_all(code.join("lib")).expect("the installed code");
        std::fs::write(code.join("old.apk"), "old").expect("the old code");
        assert!(
            replace(&staged, &code).expect("the swap"),
            "the old code is at the staging dir after the swap"
        );
        assert!(
            code.join("new.apk").is_file() && !code.join("old.apk").exists(),
            "the new code is in place, whole"
        );
        assert!(
            staged.join("old.apk").is_file() && !staged.join("new.apk").exists(),
            "the old code is at the staging dir, whole"
        );
        std::fs::remove_dir_all(&root).expect("cleanup");
    }

    #[test]
    fn replace_first_install_renames() {
        let root = dir("first");
        let (staged, code) = (root.join("vmdl2.tmp"), root.join("apps/q/apk"));
        std::fs::create_dir_all(&staged).expect("the staged code");
        std::fs::write(staged.join("new.apk"), "new").expect("the new code");
        assert!(
            !replace(&staged, &code).expect("the rename"),
            "no old code: nothing is left at the staging dir"
        );
        assert!(code.join("new.apk").is_file(), "the new code is installed");
        assert!(!staged.exists(), "the staging dir moved");
        std::fs::remove_dir_all(&root).expect("cleanup");
    }
}
