// SPDX-License-Identifier: MIT
//! The library environment of a process run from a converter build: what every runtime gate's process and the converter packages' test binaries get (packages.rs).

use std::path::Path;

use anyhow::{Context, Result};

/// The environment of every process run from a converter build in `out`: LD_LIBRARY_PATH = every library the converter linked and the provided crates' (libatl_binder_driver), as native-load stages them, then the provided shared libraries its modules need (`needed_dirs`).
pub fn lib_env(out: &Path) -> Result<Vec<(String, String)>> {
    let device = out.join("device");
    let mut lib_dirs: Vec<String> = Vec::new();
    for dir in [device.clone(), out.join("crates")] {
        for e in std::fs::read_dir(&dir)? {
            let p = e?.path();
            let p = if dir == device { p } else { p.join("lib") };
            if p.is_dir() {
                lib_dirs.push(p.display().to_string());
            }
        }
    }
    for d in needed_dirs(out)? {
        if !lib_dirs.contains(&d) {
            lib_dirs.push(d);
        }
    }
    Ok(vec![("LD_LIBRARY_PATH".to_owned(), lib_dirs.join(":"))])
}

/// The directories of the provided shared libraries graph.json's artifacts name in `needs` (bp2ninja graph.go artifact.Needs), each once, sorted.
/// They lie outside `out`, so the device dirs of `lib_env` do not hold them.
fn needed_dirs(out: &Path) -> Result<Vec<String>> {
    let file = out.join("graph.json");
    let text =
        std::fs::read_to_string(&file).with_context(|| format!("reading {}", file.display()))?;
    let graph: serde_json::Value =
        serde_json::from_str(&text).with_context(|| file.display().to_string())?;
    let artifacts = &graph["artifacts"];
    let mut dirs = std::collections::BTreeSet::new();
    for (module, a) in artifacts.as_object().into_iter().flatten() {
        for need in a["needs"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|n| n.as_str())
        {
            let path = artifacts[need]["out"].as_str().with_context(|| {
                format!(
                    "{module} needs {need}: no such artifact in {}",
                    file.display()
                )
            })?;
            let dir = Path::new(path)
                .parent()
                .with_context(|| format!("{need}: output without a directory"))?;
            dirs.insert(dir.display().to_string());
        }
    }
    Ok(dirs.into_iter().collect())
}

#[cfg(test)]
mod lib_env_tests {
    use super::*;

    /// LD_LIBRARY_PATH: the device dirs, then the provided dir a shared artifact needs, once; a provided library nothing needs stays out.
    #[test]
    fn needed_provided_dirs_only() {
        let out = std::env::temp_dir().join(format!("atl-lib-env-{}", std::process::id()));
        std::fs::create_dir_all(out.join("device/libmid")).unwrap();
        std::fs::create_dir_all(out.join("crates")).unwrap();
        let json = serde_json::json!({"artifacts": {
            "device/libmid": {"kind": "shared", "out": "/o/device/libmid/libmid.so", "needs": ["device/libcrypto"]},
            "device/libcrypto": {"kind": "shared", "out": "/upstream/boringssl/prefix/lib/libcrypto.so"},
            "device/libunused": {"kind": "shared", "out": "/usr/lib/unused/libunused.so"},
        }});
        std::fs::write(out.join("graph.json"), json.to_string()).unwrap();
        let env = lib_env(&out).unwrap();
        let dirs: Vec<&str> = env[0].1.split(':').collect();
        let crypto = "/upstream/boringssl/prefix/lib";
        assert_eq!(dirs.iter().filter(|d| **d == crypto).count(), 1, "{dirs:?}");
        assert!(!dirs.contains(&"/usr/lib/unused"), "{dirs:?}");
        assert!(
            dirs.contains(&out.join("device/libmid").to_str().unwrap()),
            "{dirs:?}"
        );
    }
}
