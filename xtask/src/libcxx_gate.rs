// SPDX-License-Identifier: MIT
//! Step `libcxx` of `xtask check`: device compiles define `__ANDROID__` (bp2ninja `androidDefine`) against the distro's libc++ headers, while its shared library was built without it.
//! That is sound only while libc++'s `__ANDROID__` sites change no ABI: libc++ 19 keys ABI-shaping config (ctype masks, regex word class) on `__BIONIC__` / `__GLIBC__`, and `__ANDROID__` only selects `locale_base_api/android.h`, which needs the `xlocale.h` shim (bp2ninja `libcxxShim`).
//! A libc++ bump could make `__ANDROID__` ABI-relevant, so this compiles one probe unit twice -- with and without `-D__ANDROID__` -- and fails when the `-dM` macro sets differ beyond `ALLOWED` or the objects differ in any byte.
//! The command line is the converter's own (`bp2ninja -device-cxx-flags`, its `probeCxxFlags`): what every device C++ compile in `PROBE_DIR` gets -- Soong's global and toolchain flags evaluated from the pinned tree, the default -std, the device defines and forced headers, the compat dirs it writes with the xlocale.h shim -- so the gate measures the configuration `xtask native` builds, for the build machine's arches.toml row.
//! The libc provider's headers (atl-bionic's in a build, generated from bionic's) are bionic's `libc/include` here: the probe needs only `android/api-level.h`, whose level (bionic's future one) keeps libc++ off its pre-21 `__posix_l_fallback.h`.

use std::collections::BTreeSet;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail, ensure};
use atl_layout::Layout;

use crate::{arches, converter, tree};

/// The tree dir whose device C++ command line the probe takes: libart's, the device build's main libc++ user (a platform dir: no ExternalCflags).
const PROBE_DIR: &str = "art/runtime";

/// The libc provider's include dir stand-in, in the tree.
const BIONIC_INCLUDE: &str = "bionic/libc/include";

/// What `-D__ANDROID__` may add to the macro set: itself, and the include guard of the header it selects (libc++ 19 `__locale_dir/locale_base_api.h:16`). The shim is `#pragma once`, no macro.
const ALLOWED: &[&str] = &["__ANDROID__", "_LIBCPP___LOCALE_LOCALE_BASE_API_ANDROID_H"];

/// Every header the measurement of 2026-11-01 covered, each used with external linkage so -O2 keeps code that depends on its layout.
const PROBE: &str = r#"#include <condition_variable>
#include <fstream>
#include <iomanip>
#include <iostream>
#include <locale>
#include <regex>
#include <sstream>
#include <string>

std::string probe_format(double v, const std::string& s) {
  std::ostringstream o;
  o.imbue(std::locale::classic());
  o << std::setw(12) << std::setprecision(5) << std::fixed << v << ' ' << std::quoted(s);
  return o.str();
}
long probe_parse(const std::string& s) {
  std::istringstream i(s);
  long v = 0;
  i >> std::hex >> v;
  return v;
}
bool probe_regex(const std::string& s) { return std::regex_search(s, std::regex("[[:alpha:]]+\\w*[[:digit:]]")); }
bool probe_ctype(char c) {
  return std::use_facet<std::ctype<char>>(std::locale()).is(std::ctype_base::alpha | std::ctype_base::space, c);
}
bool probe_file(const char* path) {
  std::ofstream f(path);
  f << "x" << std::endl;
  return f.good();
}
bool probe_wait(std::mutex& m, std::condition_variable& cv, bool& ready) {
  std::unique_lock<std::mutex> l(m);
  return cv.wait_for(l, std::chrono::milliseconds(1), [&] { return ready; });
}
void probe_print(const std::wstring& s) { std::wcout << s << std::endl; }
"#;

/// One comparison's inputs.
struct Inputs<'a> {
    cxx: &'a str,
    /// The compile's flags, `android` among them once.
    argv: &'a [String],
    /// The define the runs differ by: present in one, removed in the other.
    android: &'a str,
    probe: &'a str,
}

/// What `-D__ANDROID__` changed.
#[derive(Debug, PartialEq, Eq)]
struct Diff {
    /// Macro names whose definition differs between the runs, sorted.
    macros: Vec<String>,
    /// Object size, None when the objects are identical.
    object: Option<(usize, usize)>,
    /// Bytes of the identical objects.
    object_len: usize,
}

fn compare(i: &Inputs, work: &Path) -> Result<Diff> {
    std::fs::create_dir_all(work)?;
    let src = work.join("probe.cpp");
    std::fs::write(&src, i.probe)?;
    let at: Vec<usize> = (0..i.argv.len())
        .filter(|&k| i.argv[k] == i.android)
        .collect();
    ensure!(
        at.len() == 1,
        "{} appears {} times in the probe's command line, not once: {:?}",
        i.android,
        at.len(),
        i.argv
    );
    let run = |android: bool, out: &[&str]| -> Result<Vec<u8>> {
        let mut cmd = Command::new(i.cxx);
        cmd.args(["-x", "c++"]);
        cmd.args(
            i.argv
                .iter()
                .enumerate()
                .filter(|&(k, _)| android || k != at[0])
                .map(|(_, a)| a),
        );
        cmd.args(out).arg(&src);
        let o = cmd.output().with_context(|| format!("running {}", i.cxx))?;
        if !o.status.success() {
            bail!(
                "{} ({} {}) {} failed: {}",
                i.cxx,
                if android { "with" } else { "without" },
                i.android,
                out.join(" "),
                String::from_utf8_lossy(&o.stderr)
            );
        }
        Ok(o.stdout)
    };
    let macros = |android: bool| -> Result<BTreeSet<String>> {
        Ok(String::from_utf8(run(android, &["-E", "-dM"])?)?
            .lines()
            .map(str::to_owned)
            .collect())
    };
    let (off, on) = (macros(false)?, macros(true)?);
    let names: BTreeSet<String> = off
        .symmetric_difference(&on)
        .filter_map(|l| l.strip_prefix("#define "))
        .map(|d| {
            d.split(|c: char| c == '(' || c.is_whitespace())
                .next()
                .unwrap_or(d)
                .to_owned()
        })
        .collect();
    let object = |android: bool| -> Result<Vec<u8>> {
        let obj = work.join(if android { "android.o" } else { "plain.o" });
        run(android, &["-c", "-o", &obj.display().to_string()])?;
        Ok(std::fs::read(&obj)?)
    };
    let (a, b) = (object(false)?, object(true)?);
    Ok(Diff {
        macros: names.into_iter().collect(),
        object: (a != b).then_some((a.len(), b.len())),
        object_len: a.len(),
    })
}

/// The verdict on a `Diff`: Ok with the allowed names seen, or what fails.
fn judge(d: &Diff) -> Result<Vec<&str>> {
    let extra: Vec<&String> = d
        .macros
        .iter()
        .filter(|m| !ALLOWED.contains(&m.as_str()))
        .collect();
    // the probe responds: a run where the define changed nothing measured nothing
    ensure!(
        d.macros.iter().any(|m| m == "__ANDROID__"),
        "-D__ANDROID__ did not reach the macro set: the probe measures nothing"
    );
    let mut problems = Vec::new();
    if !extra.is_empty() {
        problems.push(format!(
            "-D__ANDROID__ changes libc++ macros beyond {ALLOWED:?}: {extra:?}"
        ));
    }
    if let Some((a, b)) = d.object {
        problems.push(format!(
            "the probe's object differs with -D__ANDROID__ ({a} vs {b} bytes)"
        ));
    }
    ensure!(
        problems.is_empty(),
        "{}: libc++'s __ANDROID__ paths are no longer ABI-neutral on glibc -- review them before building device code with it",
        problems.join("; ")
    );
    Ok(ALLOWED
        .iter()
        .copied()
        .filter(|a| d.macros.iter().any(|m| m == a))
        .collect())
}

/// What `bp2ninja -device-cxx-flags` prints.
#[derive(serde::Deserialize)]
struct ProbeFlags {
    argv: Vec<String>,
    android: String,
}

/// The step: the distro libc++ as device compiles of the build machine's arch see it.
pub fn check(root: &Path) -> Result<()> {
    // the C++ driver: deps.toml row clang-cxx's binary, native's --cxx default
    let cxx = crate::deps::binary(root, "clang-cxx")?;
    let layout = Layout::of(root)?;
    let arches = arches::load(root)?;
    let (name, arch) = arches::build_machine(root, &arches)?;
    // the task copies are bound over it: main runs `check` inside the namespace (tree::enter)
    let tree = tree::shared_tree(&layout);
    tree::verify(&tree::overlays_of(root)?)?;
    let libc = tree.join(BIONIC_INCLUDE);
    ensure!(
        libc.join("android/api-level.h").is_file(),
        "{} is not fetched (cargo xtask fetch --execute)",
        libc.display()
    );
    // the converter as step converter builds it, under this checkout's target dir
    let tools = root.join("target").join("bp2ninja");
    let go_work = converter::go_work(root, &layout, &tools)?;
    let bin = tools.join("bp2ninja");
    converter::go(
        root,
        &go_work,
        &["build", "-o", &bin.display().to_string(), "."],
    )
    .context("building bp2ninja")?;
    let work = root.join("target").join("libcxx-gate");
    std::fs::create_dir_all(&work)?;
    let provided = work.join("provided.json");
    std::fs::write(
        &provided,
        serde_json::json!({"libc": {"libs": [], "include": [libc.display().to_string()], "defines": []}})
            .to_string(),
    )?;
    let o = Command::new(&bin)
        .arg("-tree")
        .arg(&tree)
        .arg("-out")
        .arg(&work)
        .args([
            "-arch",
            name,
            "-pointer-bits",
            &arch.pointer_bits.to_string(),
            "-arch-variant",
            arch.soong_arch_variant.as_deref().unwrap_or_default(),
            "-device-cxx-flags",
            PROBE_DIR,
            "-cxx",
            &cxx,
        ])
        .arg("-provided")
        .arg(&provided)
        .output()
        .context("running bp2ninja -device-cxx-flags")?;
    ensure!(
        o.status.success(),
        "bp2ninja -device-cxx-flags failed: {}",
        String::from_utf8_lossy(&o.stderr)
    );
    let flags: ProbeFlags =
        serde_json::from_slice(&o.stdout).context("parsing bp2ninja -device-cxx-flags output")?;
    let d = compare(
        &Inputs {
            cxx: &cxx,
            argv: &flags.argv,
            android: &flags.android,
            probe: PROBE,
        },
        &work,
    )?;
    let seen = judge(&d)?;
    eprintln!(
        "libcxx: {cxx} with {name}'s device C++ flags ({} args, {PROBE_DIR}) +/- {}: macros differ only by {seen:?}, {}-byte probe objects identical",
        flags.argv.len() - 1,
        flags.android,
        d.object_len
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// The driver native would compile with (deps.toml row clang-cxx).
    fn cxx_row() -> String {
        crate::deps::binary(&crate::checkout_root().unwrap(), "clang-cxx").unwrap()
    }

    /// A fake libc++: one header `<fake>` the probe includes, shaped by `body`.
    fn fake(name: &str, body: &str) -> (PathBuf, Diff) {
        let work = std::env::temp_dir().join(format!("xtask-libcxx-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&work);
        let inc = work.join("v1");
        std::fs::create_dir_all(&inc).unwrap();
        std::fs::write(inc.join("fake"), body).unwrap();
        let d = compare(
            &Inputs {
                cxx: &cxx_row(),
                argv: &fake_argv(&inc, &[ANDROID]),
                android: ANDROID,
                probe: FAKE_PROBE,
            },
            &work.join("out"),
        )
        .unwrap();
        (work, d)
    }

    const ANDROID: &str = "-D__ANDROID__";
    const FAKE_PROBE: &str = "#include <fake>\nint probe_size() { return probe_layout(); }\n";

    /// A command line on the fake libc++ in `inc`, with `extra` at its end.
    fn fake_argv(inc: &Path, extra: &[&str]) -> Vec<String> {
        let mut argv: Vec<String> = ["-nostdinc++", "-isystem"].map(str::to_owned).into();
        argv.push(inc.display().to_string());
        argv.extend(["-O2", "-fPIC"].map(str::to_owned));
        argv.extend(extra.iter().map(|s| (*s).to_owned()));
        argv
    }

    #[test]
    fn define_not_once_fails() {
        // the define the runs differ by must be in the command line exactly once: absent, both runs would be the same compile
        let work = std::env::temp_dir().join(format!("xtask-libcxx-once-{}", std::process::id()));
        let inc = work.join("v1");
        for extra in [&[][..], &[ANDROID, ANDROID][..]] {
            let e = compare(
                &Inputs {
                    cxx: &cxx_row(),
                    argv: &fake_argv(&inc, extra),
                    android: ANDROID,
                    probe: FAKE_PROBE,
                },
                &work,
            )
            .unwrap_err()
            .to_string();
            assert!(e.contains(&format!("appears {} times", extra.len())), "{e}");
        }
        std::fs::remove_dir_all(work).unwrap();
    }

    #[test]
    fn layout_macro_flip_fires() {
        // __ANDROID__ selects a wider struct: an ABI change the shared library would not share
        let (work, d) = fake(
            "layout",
            "#ifdef __ANDROID__\n#define _LIBCPP_FAKE_WIDE 1\nstruct S { int a; long b; };\n#else\nstruct S { int a; };\n#endif\ninline int probe_layout() { return sizeof(S); }\n",
        );
        assert_eq!(d.macros, ["_LIBCPP_FAKE_WIDE", "__ANDROID__"]);
        assert!(d.object.is_some());
        let e = judge(&d).unwrap_err().to_string();
        assert!(
            e.contains("_LIBCPP_FAKE_WIDE") && e.contains("object differs"),
            "{e}"
        );
        std::fs::remove_dir_all(work).unwrap();
    }

    #[test]
    fn macro_only_flip_fires() {
        // a config macro that changes no code yet is still refused: the next libc++ may read it
        let (work, d) = fake(
            "macro",
            "#ifdef __ANDROID__\n#define _LIBCPP_FAKE_CONFIG 1\n#endif\ninline int probe_layout() { return 4; }\n",
        );
        assert!(d.object.is_none());
        let e = judge(&d).unwrap_err().to_string();
        assert!(
            e.contains("_LIBCPP_FAKE_CONFIG") && !e.contains("object differs"),
            "{e}"
        );
        std::fs::remove_dir_all(work).unwrap();
    }

    #[test]
    fn allowed_guard_passes() {
        // libc++ 19's shape: __ANDROID__ only pulls a guarded header that changes nothing
        let (work, d) = fake(
            "guard",
            "#ifdef __ANDROID__\n#ifndef _LIBCPP___LOCALE_LOCALE_BASE_API_ANDROID_H\n#define _LIBCPP___LOCALE_LOCALE_BASE_API_ANDROID_H\n#endif\n#endif\ninline int probe_layout() { return 4; }\n",
        );
        assert_eq!(judge(&d).unwrap(), ALLOWED);
        std::fs::remove_dir_all(work).unwrap();
    }
}
