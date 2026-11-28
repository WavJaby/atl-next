// SPDX-License-Identifier: MIT
//! Check step `source-scan`: one walk of the tree the next commit can contain (spdx.rs `Tree`, each file read once), then every source rule over it ([`RULES`]).
//! Each rule keeps its own judgement and prints its own verdict line under its old step name (`spdx: ...`, `env-reads: FAILED: ...`); the step fails naming every failed rule, the first one leading.

use std::path::Path;

use anyhow::{Result, bail};

use crate::spdx::Tree;

/// A source rule: its verdict-line name (the step it was) and its check over the checkout and the tree.
type Rule = (&'static str, fn(&Path, &Tree) -> Result<()>);

/// Report order = the old step order.
pub const RULES: [Rule; 8] = [
    // SPDX header on every tracked or new, non-ignored file
    ("spdx", crate::spdx::check),
    // libbinder's driver I/O goes only through BinderDriver.h (patches/aosp/frameworks/native 0002), in the AOSP tree this checkout builds (not the tracked tree)
    ("seam", crate::seam_gate::check),
    // Android path literals (`/system`, `/data`, ...) only in crate atl-android-paths, others allowed by android-paths.toml with a reason
    ("android-paths", crate::android_paths::check),
    // bwrap flag literals only in atl-bwrap, besides the golden argv tests
    ("bwrap-flags", crate::bwrap_flags::check),
    // tracked code never invokes Soong (`soong_ui`, `lunch`; Soong retired a84d9b6): comments and Markdown pass
    ("soong", crate::soong::check),
    // no crate logs a free-text `refused` / `unimplemented` line: API outcomes go through atl-diag
    ("diag-shape", crate::diag_shape::check),
    // the WebView provider throws UnsupportedOperationException only for the class-C methods listed with a reason
    ("provider-refusals", crate::provider_refusals::check),
    // product code reads no environment-variable switch; no Cargo.toml enables a `break-*` / `tier-*` feature
    ("env-reads", crate::env_reads::check),
];

/// Runs every rule over one walk; fails naming each failed rule (`<rule>: <error>`, one per line, the first failed rule first).
pub fn check(root: &Path) -> Result<()> {
    let tree = Tree::load(root)?;
    let fails = run(root, &tree, &RULES);
    if fails.is_empty() {
        eprintln!(
            "source-scan: ok ({} rules over {} files)",
            RULES.len(),
            tree.len()
        );
        return Ok(());
    }
    bail!("{}", fails.join("\n"))
}

/// Every rule's outcome in order: a failed rule's `<rule>: <error>` line (also printed as its verdict line).
fn run(root: &Path, tree: &Tree, rules: &[Rule]) -> Vec<String> {
    let mut fails = Vec::new();
    for (name, rule) in rules {
        if let Err(e) = rule(root, tree) {
            eprintln!("{name}: FAILED: {e:#}");
            fails.push(format!("{name}: {e:#}"));
        }
    }
    fails
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pass(_: &Path, _: &Tree) -> Result<()> {
        Ok(())
    }

    fn fail(_: &Path, tree: &Tree) -> Result<()> {
        bail!("{} files seen", tree.len())
    }

    /// Every rule runs whatever an earlier one judged; the failures name their rule, in rule order.
    #[test]
    fn each_failed_rule_is_named_and_every_rule_runs() {
        let tree = Tree::of(&[("a.rs", b"x"), ("b.md", b"y")]);
        let rules: [Rule; 3] = [("one", fail), ("two", pass), ("three", fail)];
        assert_eq!(
            run(Path::new("/"), &tree, &rules),
            ["one: 2 files seen", "three: 2 files seen"]
        );
    }

    #[test]
    fn a_kept_file_that_is_not_utf8_is_named() {
        let tree = Tree::of(&[("bin.rs", &[0xff, 0xfe])]);
        let err = tree.texts(|_| true).expect_err("not UTF-8");
        assert!(format!("{err:#}").contains("bin.rs"), "{err:#}");
        assert!(tree.texts(|r| r.ends_with(".md")).unwrap().is_empty());
    }
}
