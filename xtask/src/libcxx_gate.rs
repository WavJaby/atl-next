// SPDX-License-Identifier: MIT
//! Step `libcxx` of `xtask check`: device compiles define `__ANDROID__` (bp2ninja `androidDefine`) against the distro's libc++ headers, while its shared library was built without it.
//! That is sound only while libc++'s `__ANDROID__` sites change no ABI: libc++ 19 keys ABI-shaping config (ctype masks, regex word class) on `__BIONIC__` / `__GLIBC__`, and `__ANDROID__` only selects `locale_base_api/android.h`, which needs the `xlocale.h` shim (bp2ninja `libcxxShim`).
//! A libc++ bump could make `__ANDROID__` ABI-relevant, so this compiles one probe unit twice -- the device build's libc++-visible inputs, with and without `-D__ANDROID__` -- and fails when the `-dM` macro sets differ beyond `ALLOWED` or the `-O2` objects differ in any byte.
//! Both runs force-include bionic's `android/versioning.h` + `android/api-level.h` from the pinned tree, as every device compile does (api-level.h also sets `__ANDROID_API__` to bionic's future level, which keeps libc++ off its pre-21 `__posix_l_fallback.h`), and search the shim dir last.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail, ensure};
use atl_layout::Layout;

/// The C++ compiler device compiles use (`xtask native --cxx` default).
pub const CXX: &str = "clang++-19";

/// The shim bp2ninja embeds, relative to the checkout.
const SHIM: &str = "tools/bp2ninja/compat/xlocale.h";

/// Forced into every device compile (bp2ninja `deviceForced`), from the tree: bionic's own, of which atl-bionic's api-level.h is generated.
const FORCED: &[&str] = &["android/versioning.h", "android/api-level.h"];

/// Where `FORCED` sits in the tree.
const BIONIC_INCLUDE: &str = "aosp/bionic/libc/include";

/// What `-D__ANDROID__` may add to the macro set: itself, and the include guard of the header it selects (libc++ 19 `__locale_dir/locale_base_api.h:16`). The shim is `#pragma once`, no macro.
const ALLOWED: &[&str] = &["__ANDROID__", "_LIBCPP___LOCALE_LOCALE_BASE_API_ANDROID_H"];

/// The -std device C++ compiles get (Soong's CppStdVersion at the pin, which bp2ninja reads from the tree); the probe only needs libc++'s C++20 paths compiled.
const STD: &str = "-std=gnu++20";

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
    /// libc++'s header dir; None = the compiler's own (`-stdlib=libc++`), as device compiles.
    libcxx: Option<&'a Path>,
    /// Include name -> file, in order.
    forced: &'a [(&'a str, PathBuf)],
    /// Searched last (`-idirafter`), as bp2ninja's compat/libcxx.
    shim_dir: &'a Path,
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
    let mut base: Vec<String> = vec![
        "-x".into(),
        "c++".into(),
        STD.into(),
        "-O2".into(),
        "-fPIC".into(),
    ];
    match i.libcxx {
        Some(dir) => base.extend([
            "-nostdinc++".into(),
            "-isystem".into(),
            dir.display().to_string(),
        ]),
        None => base.push("-stdlib=libc++".into()),
    }
    // as bp2ninja's compat/device: on the include path by name (libc++'s android.h includes <android/api-level.h>), and forced
    let device = work.join("device");
    for (name, file) in i.forced {
        let to = device.join(name);
        std::fs::create_dir_all(to.parent().context("forced header without a dir")?)?;
        std::fs::copy(file, &to).with_context(|| format!("copying {}", file.display()))?;
    }
    base.extend(["-I".into(), device.display().to_string()]);
    for (name, _) in i.forced {
        base.extend(["-include".into(), (*name).to_owned()]);
    }
    base.extend(["-idirafter".into(), i.shim_dir.display().to_string()]);
    let run = |android: bool, out: &[&str]| -> Result<Vec<u8>> {
        let mut cmd = Command::new(i.cxx);
        cmd.args(&base);
        if android {
            cmd.arg("-D__ANDROID__");
        }
        cmd.args(out).arg(&src);
        let o = cmd.output().with_context(|| format!("running {}", i.cxx))?;
        if !o.status.success() {
            bail!(
                "{} {}{} failed: {}",
                i.cxx,
                if android { "-D__ANDROID__ " } else { "" },
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

/// The step: the distro libc++ as device compiles see it.
pub fn check(root: &Path) -> Result<()> {
    let layout = Layout::of(root)?;
    let forced: Vec<(&str, PathBuf)> = FORCED
        .iter()
        .map(|f| {
            let path = layout.locate(&Path::new(BIONIC_INCLUDE).join(f)).dir;
            ensure!(
                path.is_file(),
                "{} is not fetched (cargo xtask fetch --execute)",
                path.display()
            );
            Ok((*f, path))
        })
        .collect::<Result<_>>()?;
    let work = root.join("target").join("libcxx-gate");
    let shim_dir = work.join("shim");
    std::fs::create_dir_all(&shim_dir)?;
    std::fs::copy(root.join(SHIM), shim_dir.join("xlocale.h"))
        .with_context(|| format!("copying {SHIM}"))?;
    let d = compare(
        &Inputs {
            cxx: CXX,
            libcxx: None,
            forced: &forced,
            shim_dir: &shim_dir,
            probe: PROBE,
        },
        &work,
    )?;
    let seen = judge(&d)?;
    eprintln!(
        "libcxx: {CXX} -stdlib=libc++ with -D__ANDROID__: macros differ only by {seen:?}, {}-byte probe objects identical",
        d.object_len
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fake libc++: one header `<fake>` the probe includes, shaped by `body`.
    fn fake(name: &str, body: &str) -> (PathBuf, Diff) {
        let work = std::env::temp_dir().join(format!("xtask-libcxx-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&work);
        let inc = work.join("v1");
        std::fs::create_dir_all(&inc).unwrap();
        std::fs::write(inc.join("fake"), body).unwrap();
        let shim = work.join("shim");
        std::fs::create_dir_all(&shim).unwrap();
        let probe = "#include <fake>\nint probe_size() { return probe_layout(); }\n";
        let d = compare(
            &Inputs {
                cxx: CXX,
                libcxx: Some(&inc),
                forced: &[],
                shim_dir: &shim,
                probe,
            },
            &work.join("out"),
        )
        .unwrap();
        (work, d)
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
