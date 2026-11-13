// SPDX-License-Identifier: MIT
//! Package-side plain types shared by atld, the services and xtask: the platform's permission definitions (framework-res, a JSON file the host forwards), SystemConfig's shared libraries
//! (`--shared-library`), and the ActivityInfo.screenOrientation values the services name. The installed packages themselves are the services' (atl-services `packages`: the installer's PackageInfo).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// ActivityInfo.SCREEN_ORIENTATION_* (ActivityInfo.java:824-909) that the services and the manifest reader name. `-1` is what an unwritten `screenOrientation` means (PackageParser.java:4457-4459).
pub const SCREEN_ORIENTATION_UNSPECIFIED: i32 = -1;
pub const SCREEN_ORIENTATION_LANDSCAPE: i32 = 0;
pub const SCREEN_ORIENTATION_PORTRAIT: i32 = 1;
pub const SCREEN_ORIENTATION_SENSOR_LANDSCAPE: i32 = 6;
pub const SCREEN_ORIENTATION_SENSOR_PORTRAIT: i32 = 7;
pub const SCREEN_ORIENTATION_REVERSE_LANDSCAPE: i32 = 8;
pub const SCREEN_ORIENTATION_REVERSE_PORTRAIT: i32 = 9;
pub const SCREEN_ORIENTATION_USER_LANDSCAPE: i32 = 11;
pub const SCREEN_ORIENTATION_USER_PORTRAIT: i32 = 12;

/// `<permission android:name android:protectionLevel>` (ParsedPermissionUtils): the level as written, base type in the low 4 bits (PermissionInfo.PROTECTION_MASK_BASE). framework-res's list is the
/// install-time grant rule's platform half (permission::install_grants).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionDecl {
    pub name: String,
    pub protection_level: u32,
}

/// A shared library as SystemConfig reads it from `<library name file dependency>` (frameworks/base/data/etc/platform.xml: `android.test.runner` depends on `android.test.base:android.test.mock`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Library {
    /// its jar as the app processes see it
    pub file: String,
    /// the libraries it needs, by name: a package that uses it gets them loaded for it too
    pub dependencies: Vec<String>,
}

impl Library {
    /// The command-line form `<name>=<file>[,<dependency>...]` (atld's `--shared-library` and the services'): the name, the file, and the names of the libraries it depends on.
    pub fn parse_arg(s: &str) -> Result<(String, Library), String> {
        let (name, rest) = s
            .split_once('=')
            .ok_or_else(|| format!("{s}: not <name>=<file>[,<dependency>...]"))?;
        let mut parts = rest.split(',');
        let file = parts.next().unwrap_or_default();
        if name.is_empty() || file.is_empty() {
            return Err(format!("{s}: not <name>=<file>[,<dependency>...]"));
        }
        Ok((
            name.to_owned(),
            Library {
                file: file.to_owned(),
                dependencies: parts.map(str::to_owned).collect(),
            },
        ))
    }

    /// [`Library::parse_arg`]'s form of this library, with `file` in place of its own (atld installs the jar elsewhere).
    pub fn to_arg(&self, name: &str, file: &str) -> String {
        std::iter::once(format!("{name}={file}"))
            .chain(self.dependencies.iter().cloned())
            .collect::<Vec<_>>()
            .join(",")
    }
}

/// A library a package uses and the ones it depends on ([`used_libraries`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibraryNode {
    pub name: String,
    pub file: String,
    pub dependencies: Vec<LibraryNode>,
}

/// The libraries named by `uses` (a package's `<uses-library>`s in the parse's order) that are installed, each with the libraries it depends on below it: the shape of ApplicationInfo.sharedLibraryInfos
/// (ComputerEngine / SharedLibraryUtils.findSharedLibraries), which LoadedApk turns into one class loader per library, dependencies as its delegates. A name with no installed library adds nothing; a
/// dependency cycle is cut where it closes.
pub fn used_libraries<'a>(
    libraries: &BTreeMap<String, Library>,
    uses: impl IntoIterator<Item = &'a str>,
) -> Vec<LibraryNode> {
    let names: Vec<&str> = uses.into_iter().collect();
    nodes(libraries, &names, &mut Vec::new())
}

fn nodes<'a>(
    libraries: &'a BTreeMap<String, Library>,
    names: &[&'a str],
    path: &mut Vec<&'a str>,
) -> Vec<LibraryNode> {
    let mut out = Vec::new();
    for &name in names {
        let Some((key, lib)) = libraries.get_key_value(name) else {
            continue;
        };
        if path.contains(&key.as_str()) {
            continue;
        }
        path.push(key);
        let deps: Vec<&str> = lib.dependencies.iter().map(String::as_str).collect();
        out.push(LibraryNode {
            name: name.to_owned(),
            file: lib.file.clone(),
            dependencies: nodes(libraries, &deps, path),
        });
        path.pop();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn used_libraries_follow_the_list_and_carry_their_dependencies() {
        let lib = |file: &str, deps: &[&str]| Library {
            file: file.into(),
            dependencies: deps.iter().map(|d| (*d).to_owned()).collect(),
        };
        let libraries = BTreeMap::from([
            ("runner".to_owned(), lib("/f/runner.jar", &["base", "mock"])),
            ("base".to_owned(), lib("/f/base.jar", &[])),
            ("mock".to_owned(), lib("/f/mock.jar", &[])),
            // a cycle is cut where it closes
            ("a".to_owned(), lib("/f/a.jar", &["b"])),
            ("b".to_owned(), lib("/f/b.jar", &["a"])),
        ]);
        let node = |name: &str, file: &str, dependencies: Vec<LibraryNode>| LibraryNode {
            name: name.into(),
            file: file.into(),
            dependencies,
        };
        // list order; "absent" names no installed library and adds nothing
        assert_eq!(
            used_libraries(&libraries, ["runner", "absent", "base", "a"]),
            vec![
                node(
                    "runner",
                    "/f/runner.jar",
                    vec![
                        node("base", "/f/base.jar", vec![]),
                        node("mock", "/f/mock.jar", vec![])
                    ]
                ),
                node("base", "/f/base.jar", vec![]),
                node("a", "/f/a.jar", vec![node("b", "/f/b.jar", vec![])]),
            ]
        );
    }

    #[test]
    fn library_arguments() {
        let (name, lib) =
            Library::parse_arg("android.test.runner=/f/r.jar,android.test.base,android.test.mock")
                .expect("parse");
        assert_eq!(name, "android.test.runner");
        assert_eq!(lib.file, "/f/r.jar");
        assert_eq!(lib.dependencies, ["android.test.base", "android.test.mock"]);
        assert_eq!(
            lib.to_arg(
                &name,
                &format!("{}/r.jar", atl_android_paths::SYSTEM_FRAMEWORK)
            ),
            "android.test.runner=/system/framework/r.jar,android.test.base,android.test.mock"
        );
        assert_eq!(
            Library::parse_arg("x=/y.jar")
                .expect("parse")
                .1
                .dependencies,
            Vec::<String>::new()
        );
        for bad in ["x", "=y", "x=", "x=,dep"] {
            assert!(Library::parse_arg(bad).is_err(), "{bad}");
        }
    }
}
