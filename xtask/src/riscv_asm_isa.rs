// SPDX-License-Identifier: MIT
//! Source rule `riscv-asm-isa` (check step source-scan, source_scan.rs): ART's hand-written riscv64 assembly uses a Zba/Zbb/Zbs instruction only inside `#if defined(__riscv_<ext>)`, so it assembles at plain rv64gc (patches/aosp/art 0007 macros in asm_support_riscv64.S; the second riscv64 host target is plain rv64gc). Text only; the build's -march is the run-time side.

use std::path::Path;

use anyhow::{Context, Result, ensure};
use atl_layout::Layout;

/// Directories of ART's riscv64 `.S` files (runtime entrypoints, nterp templates), relative to `src/`.
const ASM_DIRS: &[&str] = &[
    "aosp/art/runtime/arch/riscv64",
    "aosp/art/runtime/interpreter/mterp/riscv64",
];

/// Mnemonics beyond rv64gc, by the extension whose predefined macro (`__riscv_<ext>`) guards them (RISC-V bitmanip spec 1.0.0).
const EXT_MNEMONICS: &[(&str, &[&str])] = &[
    (
        "zba",
        &[
            "add.uw",
            "sh1add",
            "sh2add",
            "sh3add",
            "sh1add.uw",
            "sh2add.uw",
            "sh3add.uw",
            "slli.uw",
            "zext.w",
        ],
    ),
    (
        "zbb",
        &[
            "andn", "orn", "xnor", "clz", "clzw", "ctz", "ctzw", "cpop", "cpopw", "max", "maxu",
            "min", "minu", "sext.b", "sext.h", "zext.h", "rol", "rolw", "ror", "rori", "roriw",
            "rorw", "orc.b", "rev8",
        ],
    ),
    (
        "zbs",
        &[
            "bclr", "bclri", "bext", "bexti", "binv", "binvi", "bset", "bseti",
        ],
    ),
];

/// Extension a mnemonic needs, if it is beyond rv64gc.
fn ext_of(mnemonic: &str) -> Option<&'static str> {
    let m = mnemonic.to_ascii_lowercase();
    EXT_MNEMONICS
        .iter()
        .find(|(_, ms)| ms.contains(&m.as_str()))
        .map(|(e, _)| *e)
}

/// Extension an `#if` / `#ifdef` line tests positively (`defined(__riscv_zba)`, `#ifdef __riscv_zba`); None for any other condition.
fn guard_of(directive: &str) -> Option<&'static str> {
    let d: String = directive.split_whitespace().collect();
    EXT_MNEMONICS.iter().map(|(e, _)| *e).find(|e| {
        d == format!("#ifdef__riscv_{e}")
            || d == format!("#ifdefined(__riscv_{e})")
            || d == format!("#ifdefined__riscv_{e}")
    })
}

/// Lines of `text` with a Zba/Zbb/Zbs instruction outside the `#if` branch of its extension's guard: (line number, line).
/// The first token of a code line counts (after a `label:`); `//` and `/* */` comments, preprocessor lines and the nterp templates' `%` Python lines are not code.
fn violations(text: &str) -> Vec<(usize, String)> {
    // one entry per open #if: the extension its current branch guarantees, if any
    let mut stack: Vec<Option<&str>> = Vec::new();
    let mut in_block = false;
    let mut bad = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let t = line.trim_start();
        if let Some(d) = t.strip_prefix('#') {
            let word = d.split_whitespace().next().unwrap_or_default();
            match word {
                "if" | "ifdef" | "ifndef" => stack.push(guard_of(t)),
                "else" | "elif" => {
                    if let Some(top) = stack.last_mut() {
                        *top = None;
                    }
                }
                "endif" => {
                    stack.pop();
                }
                _ => {}
            }
            continue;
        }
        if t.starts_with('%') {
            continue;
        }
        // code only: drop /* */ (possibly spanning lines) and the // comment
        let mut code = String::new();
        let mut rest = line;
        loop {
            if in_block {
                match rest.find("*/") {
                    Some(e) => {
                        rest = &rest[e + 2..];
                        in_block = false;
                    }
                    None => break,
                }
            } else {
                match rest.find("/*") {
                    Some(s) => {
                        code.push_str(&rest[..s]);
                        rest = &rest[s + 2..];
                        in_block = true;
                    }
                    None => {
                        code.push_str(rest);
                        break;
                    }
                }
            }
        }
        let code = code.split("//").next().unwrap_or_default();
        let mut tokens = code.split_whitespace();
        let mut first = tokens.next().unwrap_or_default();
        if first.ends_with(':') {
            first = tokens.next().unwrap_or_default();
        }
        if let Some(ext) = ext_of(first)
            && !stack.contains(&Some(ext))
        {
            bad.push((i + 1, line.trim().to_owned()));
        }
    }
    bad
}

/// Rule `riscv-asm-isa`: no unguarded Zba/Zbb/Zbs instruction in ART's riscv64 `.S` files, in the AOSP tree this checkout builds (task copy or `src/`).
pub fn check(root: &Path, _tracked: &crate::spdx::Tree) -> Result<()> {
    let layout = Layout::of(root)?;
    let mut bad = Vec::new();
    let mut files = 0;
    for d in ASM_DIRS {
        let dir = layout.locate(Path::new(d)).dir;
        let mut paths: Vec<_> = std::fs::read_dir(&dir)
            .with_context(|| format!("{}: cargo xtask fetch --execute", dir.display()))?
            .map(|e| e.map(|e| e.path()))
            .collect::<Result<_, _>>()?;
        paths.retain(|p| p.extension().is_some_and(|x| x == "S"));
        paths.sort();
        for path in paths {
            let text = std::fs::read_to_string(&path)
                .with_context(|| format!("reading {}", path.display()))?;
            files += 1;
            for (n, l) in violations(&text) {
                bad.push(format!("{}:{n}: {l}", path.display()));
            }
        }
    }
    ensure!(
        bad.is_empty(),
        "Zba/Zbb/Zbs instruction outside its #if defined(__riscv_<ext>) (use the asm_support_riscv64.S macros):\n{}",
        bad.join("\n")
    );
    eprintln!("riscv-asm-isa: ok ({files} files, every Zba/Zbb/Zbs instruction guarded)");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::violations;

    fn lines(text: &str) -> Vec<usize> {
        violations(text).iter().map(|v| v.0).collect()
    }

    #[test]
    fn guarded_forms_pass() {
        let ok = "#if defined(__riscv_zba)\n    sh2add a0, a1, a2\n#else\n    slli a0, a1, 2\n#endif\n#ifdef __riscv_zbb\n  .if 1\n    andn a0, a0, a1\n  .endif\n#endif\n// sh2add in a comment\n    add a0, a0, a1 /* zext.w */\n%  sext_b = 1\n    SHADD 2, a0, a1, a2\n";
        assert!(violations(ok).is_empty(), "{:?}", violations(ok));
    }

    #[test]
    fn unguarded_and_wrong_branch_fire() {
        let bad = "    sh2add a0, a1, a2\n1:  zext.w a0, a0\n#if defined(__riscv_zba)\n    nop\n#else\n    add.uw a0, a1, a2\n#endif\n#if defined(__riscv_zba)\n    sext.b a0, a0\n#endif\n/*\n  sh1add in a block comment\n*/  bexti a0, a0, 3\n";
        assert_eq!(lines(bad), vec![1, 2, 6, 9, 13]);
    }
}
