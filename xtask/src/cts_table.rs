// SPDX-License-Identifier: MIT
//! `cts.toml`: the test modules `xtask test cts` runs, one row per `android_test` module. Column meanings live in that file's header.
//! `load` parses and checks what the file alone decides; `check` (step `tables` of `xtask check`) checks the rows against their real source: the module index of the pinned AOSP tree (`xtask aosp --index`) and the module's own sources.
//! The instrumentation (runner) is not stored: it is the one `<instrumentation>` of the built APK's manifest that targets the APK itself (cts.rs), the part of the row `xtask test cts` verifies at run time.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;

use crate::aosp::Index;

pub const FILE: &str = "cts.toml";

/// Row key = the module's `name` in its Android.bp.
pub type Table = BTreeMap<String, Row>;

#[derive(Debug, Default, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Row {
    /// Modules installed beside the test APK (the target app of an `<instrumentation android:targetPackage>` that is not the test package, helper apps the tests start).
    #[serde(default)]
    pub apps: Vec<String>,
    /// `am instrument -e class`: `<package>.<Class>` or `<package>.<Class>#<method>`.
    #[serde(default)]
    pub classes: Vec<String>,
    /// `am instrument -e package`: the tests of these packages.
    #[serde(default)]
    pub packages: Vec<String>,
    /// Why the run is filtered; required exactly when `classes` or `packages` is set.
    #[serde(default)]
    pub reason: Option<String>,
}

impl Row {
    /// The `am instrument` extras the filters make: `-e class a,b`, `-e package p,q`.
    pub fn filter_args(&self) -> Vec<String> {
        let mut args = Vec::new();
        for (key, values) in [("class", &self.classes), ("package", &self.packages)] {
            if !values.is_empty() {
                args.extend(["-e".to_owned(), key.to_owned(), values.join(",")]);
            }
        }
        args
    }
}

pub fn load(root: &Path) -> Result<Table> {
    let text =
        std::fs::read_to_string(root.join(FILE)).with_context(|| format!("reading {FILE}"))?;
    parse(&text).with_context(|| format!("in {FILE}"))
}

/// Module names are Soong's: ASCII letters, digits, `_`, `-`, `.`.
fn is_module_name(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
}

fn is_ident(s: &str) -> bool {
    let mut chars = s.chars();
    chars
        .next()
        .is_some_and(|c| c.is_alphabetic() || c == '_' || c == '$')
        && chars.all(|c| c.is_alphanumeric() || c == '_' || c == '$')
}

fn is_package(s: &str) -> bool {
    s.split('.').all(is_ident)
}

/// A class filter split into (package, outermost class, method): `a.b.C`, `a.b.C$D` (nested: its file is C's) or either with `#m`.
fn split_class(spec: &str) -> Result<(&str, &str, Option<&str>), String> {
    let (class, method) = match spec.split_once('#') {
        Some((c, m)) if is_ident(m) => (c, Some(m)),
        Some(_) => return Err(format!("{spec}: the method after `#` is not an identifier")),
        None => (spec, None),
    };
    let (package, simple) = class
        .rsplit_once('.')
        .ok_or_else(|| format!("{spec}: not <package>.<Class>[#<method>]"))?;
    if !is_package(package) || !is_ident(simple) {
        return Err(format!("{spec}: not <package>.<Class>[#<method>]"));
    }
    Ok((package, simple.split('$').next().unwrap_or(simple), method))
}

fn parse(text: &str) -> Result<Table> {
    let table: Table = toml::from_str(text)?;
    for (module, row) in &table {
        ensure!(is_module_name(module), "[{module}]: not a module name");
        let mut seen = BTreeSet::new();
        for app in &row.apps {
            ensure!(
                is_module_name(app),
                "[{module}] apps: {app}: not a module name"
            );
            ensure!(app != module, "[{module}] apps: names the module itself");
            ensure!(seen.insert(app), "[{module}] apps: {app} twice");
        }
        let mut seen = BTreeSet::new();
        for class in &row.classes {
            split_class(class).map_err(|e| anyhow::anyhow!("[{module}] classes: {e}"))?;
            ensure!(seen.insert(class), "[{module}] classes: {class} twice");
        }
        let mut seen = BTreeSet::new();
        for package in &row.packages {
            ensure!(
                is_package(package),
                "[{module}] packages: {package}: not a package name"
            );
            ensure!(seen.insert(package), "[{module}] packages: {package} twice");
        }
        let filtered = !row.classes.is_empty() || !row.packages.is_empty();
        match (&row.reason, filtered) {
            (Some(r), true) => ensure!(!r.trim().is_empty(), "[{module}]: empty reason"),
            (None, true) => bail!("[{module}]: a filtered row needs a `reason`"),
            (Some(_), false) => bail!("[{module}]: `reason` without a filter"),
            (None, false) => {}
        }
    }
    Ok(table)
}

/// Every `.java` / `.kt` file under `dir`, relative to it with `/`.
fn sources(dir: &Path) -> Result<Vec<String>> {
    fn walk(dir: &Path, rel: &str, out: &mut Vec<String>) -> Result<()> {
        for e in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
            let e = e?;
            let name = e.file_name().to_string_lossy().into_owned();
            let path = e.path();
            let rel = if rel.is_empty() {
                name.clone()
            } else {
                format!("{rel}/{name}")
            };
            if path.is_dir() {
                walk(&path, &rel, out)?;
            } else if name.ends_with(".java") || name.ends_with(".kt") {
                out.push(rel);
            }
        }
        Ok(())
    }
    let mut out = Vec::new();
    walk(dir, "", &mut out)?;
    Ok(out)
}

/// The module types an `apps` entry may be: something that packages an APK.
const APP_TYPES: [&str; 3] = ["android_app", "android_test", "android_test_helper_app"];

/// The row checks against the real source: the module is an `android_test` of the module index, every app an APK module, every class a source file of the module (a method named in it appears in that file), every package a source directory.
/// `locate` maps a tree path to the checkout's directory (`Layout::locate` of `aosp/<path>`).
pub fn check(table: &Table, index: &Index, locate: &dyn Fn(&str) -> PathBuf) -> Result<()> {
    let mut problems = Vec::new();
    for (module, row) in table {
        let defs = index.definitions_of(module);
        let tests: Vec<_> = defs.iter().filter(|(t, _)| *t == "android_test").collect();
        let [(_, file)] = tests.as_slice() else {
            problems.push(format!(
                "[{module}]: {} android_test definitions in the module index (found: {:?})",
                tests.len(),
                defs
            ));
            continue;
        };
        for app in &row.apps {
            let apps = index.definitions_of(app);
            if !apps.iter().any(|(t, _)| APP_TYPES.contains(t)) {
                problems.push(format!(
                    "[{module}] apps: {app} is no android_app / android_test / android_test_helper_app of the module index (found: {apps:?})"
                ));
            }
        }
        if row.classes.is_empty() && row.packages.is_empty() {
            continue;
        }
        let dir = locate(
            Path::new(file)
                .parent()
                .and_then(Path::to_str)
                .unwrap_or(""),
        );
        let files = match sources(&dir) {
            Ok(f) => f,
            Err(e) => {
                problems.push(format!("[{module}]: {e:#}"));
                continue;
            }
        };
        for class in &row.classes {
            let Ok((package, outer, method)) = split_class(class) else {
                continue;
            };
            let tail = |ext: &str| format!("{}/{outer}.{ext}", package.replace('.', "/"));
            let found = files.iter().find(|f| {
                [tail("java"), tail("kt")]
                    .iter()
                    .any(|t| f.ends_with(t.as_str()))
            });
            match (found, method) {
                (None, _) => problems.push(format!(
                    "[{module}] classes: {class}: no {} or {} in {}",
                    tail("java"),
                    tail("kt"),
                    dir.display()
                )),
                (Some(f), Some(m)) => {
                    let text = std::fs::read_to_string(dir.join(f)).unwrap_or_default();
                    if !text.contains(m) {
                        problems.push(format!("[{module}] classes: {class}: {f} never names {m}"));
                    }
                }
                (Some(_), None) => {}
            }
        }
        for package in &row.packages {
            let want = package.replace('.', "/");
            let has = files.iter().any(|f| {
                f.rsplit_once('/')
                    .is_some_and(|(d, _)| d == want || d.ends_with(&format!("/{want}")))
            });
            if !has {
                problems.push(format!(
                    "[{module}] packages: {package}: no source directory {want} in {}",
                    dir.display()
                ));
            }
        }
    }
    ensure!(problems.is_empty(), "{}", problems.join("\n"));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn err(text: &str) -> String {
        format!("{:#}", parse(text).expect_err("should be rejected"))
    }

    #[test]
    fn rows_parse_and_make_filter_args() {
        let t = parse(
            "[M]\n\n[N]\napps = [\"A\"]\nclasses = [\"a.b.C\", \"a.b.D#t\"]\npackages = [\"a.b\"]\nreason = \"r\"\n",
        )
        .unwrap();
        assert_eq!(t["M"], Row::default());
        assert!(t["M"].filter_args().is_empty());
        assert_eq!(
            t["N"].filter_args(),
            ["-e", "class", "a.b.C,a.b.D#t", "-e", "package", "a.b"]
        );
    }

    #[test]
    fn schema_violations() {
        assert!(err("[\"a b\"]\n").contains("not a module name"));
        assert!(err("[M]\napps = [\"M\"]\n").contains("names the module itself"));
        assert!(err("[M]\napps = [\"A\", \"A\"]\n").contains("A twice"));
        assert!(err("[M]\nclasses = [\"C\"]\nreason = \"r\"\n").contains("not <package>.<Class>"));
        assert!(err("[M]\nclasses = [\"a.C#1x\"]\nreason = \"r\"\n").contains("not an identifier"));
        assert!(err("[M]\nclasses = [\"a.C\"]\n").contains("needs a `reason`"));
        assert!(err("[M]\nreason = \"r\"\n").contains("`reason` without a filter"));
        assert!(err("[M]\npackages = [\"a.C\"]\nreason = \" \"\n").contains("empty reason"));
        assert!(err("[M]\nrunner = \"x\"\n").contains("unknown field"));
        // a nested class is its outermost class's file
        assert_eq!(split_class("a.b.C$D#m"), Ok(("a.b", "C", Some("m"))));
    }

    /// Module index and sources of a two-module tree: the check passes on matching rows and names each way a row can disagree with its source (the probe responds both ways).
    #[test]
    fn rows_are_checked_against_the_index_and_the_sources() {
        let tmp = std::env::temp_dir().join(format!("xtask-cts-table-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let dir = tmp.join("cts/tests/m");
        std::fs::create_dir_all(dir.join("src/a/b")).unwrap();
        std::fs::write(dir.join("src/a/b/C.java"), "class C { void testOne() {} }").unwrap();
        let index: Index = serde_json::from_str(
            r#"{"modules":{
                "M":[{"module_type":"android_test","project":"cts","file":"cts/tests/m/Android.bp"}],
                "H":[{"module_type":"android_test_helper_app","project":"cts","file":"cts/tests/h/Android.bp"}],
                "L":[{"module_type":"android_library","project":"cts","file":"cts/tests/l/Android.bp"}]},
               "module_types":{}}"#,
        )
        .unwrap();
        let root = tmp.clone();
        let locate = move |p: &str| root.join(p);
        let ok = parse(
            "[M]\napps = [\"H\"]\nclasses = [\"a.b.C#testOne\"]\npackages = [\"a.b\"]\nreason = \"r\"\n",
        )
        .unwrap();
        check(&ok, &index, &locate).unwrap();
        let bad = |row: &str| {
            let t = parse(&format!("[M]\n{row}")).unwrap();
            format!("{:#}", check(&t, &index, &locate).expect_err("must fail"))
        };
        assert!(bad("apps = [\"L\"]\n").contains("apps: L is no"));
        assert!(bad("apps = [\"Z\"]\n").contains("apps: Z is no"));
        assert!(bad("classes = [\"a.b.X\"]\nreason = \"r\"\n").contains("no a/b/X.java"));
        assert!(bad("classes = [\"a.b.C#nope\"]\nreason = \"r\"\n").contains("never names nope"));
        assert!(bad("packages = [\"a.c\"]\nreason = \"r\"\n").contains("no source directory a/c"));
        let missing = parse("[L]\n").unwrap();
        assert!(
            format!("{:#}", check(&missing, &index, &locate).unwrap_err())
                .contains("0 android_test definitions")
        );
        std::fs::remove_dir_all(&tmp).unwrap();
    }
}
