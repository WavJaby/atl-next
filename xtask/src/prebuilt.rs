// SPDX-License-Identifier: MIT
//! The record of check's prebuild: every product a gate row runs, keyed (`converter/<package>/<file>`, `release/dir`, `gl-dispatch`, `test/<package>/<test>[@<break>]`, ...).
//! check writes it once per gates step; a row started with `test --prebuilt <file>` installs it and builds nothing: a key it lacks is an error, not a build.
//! Without `--prebuilt` (a by-hand run) nothing is installed and every builder builds as before.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use anyhow::{Context, Result, anyhow};

/// Product key -> path.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Record(BTreeMap<String, PathBuf>);

/// The record of this process, set once by `test --prebuilt` (xtask-harness main.rs) before any lookup.
static INSTALLED: OnceLock<Record> = OnceLock::new();

impl Record {
    pub fn insert(&mut self, key: impl Into<String>, path: impl Into<PathBuf>) {
        self.0.insert(key.into(), path.into());
    }

    pub fn get(&self, key: &str) -> Option<&Path> {
        self.0.get(key).map(PathBuf::as_path)
    }

    pub fn entries(&self) -> usize {
        self.0.len()
    }

    /// Every product that is a file (directories such as `release/dir` left out).
    pub fn files(&self) -> Vec<PathBuf> {
        self.0.values().filter(|p| p.is_file()).cloned().collect()
    }

    /// One `key<TAB>path` line per product, sorted by key.
    pub fn write(&self, file: &Path) -> Result<()> {
        let text: String = self
            .0
            .iter()
            .map(|(k, p)| format!("{k}\t{}\n", p.display()))
            .collect();
        std::fs::write(file, text)
            .with_context(|| format!("writing the prebuilt record {}", file.display()))
    }

    fn read(file: &Path) -> Result<Record> {
        let text = std::fs::read_to_string(file)
            .with_context(|| format!("reading the prebuilt record {}", file.display()))?;
        let mut rec = Record::default();
        for line in text.lines().filter(|l| !l.is_empty()) {
            let (key, path) = line
                .split_once('\t')
                .with_context(|| format!("{}: no tab in `{line}`", file.display()))?;
            rec.insert(key, path);
        }
        Ok(rec)
    }
}

/// Makes `file` this process's record: every lookup reads it from now on. Once per process.
pub fn install(file: &Path) -> Result<()> {
    let rec = Record::read(file)?;
    INSTALLED
        .set(rec)
        .map_err(|_| anyhow!("a prebuilt record is installed already"))
}

/// The installed record, when this process was given one.
pub fn installed() -> Option<&'static Record> {
    INSTALLED.get()
}

/// Product `key` of the installed record: None when no record is installed (the caller builds it); an error when the record lacks it.
pub fn lookup(key: &str) -> Result<Option<PathBuf>> {
    let Some(rec) = installed() else {
        return Ok(None);
    };
    let path = rec.get(key).with_context(|| missing(key))?;
    Ok(Some(path.to_owned()))
}

/// The error for a key a row's record lacks: check's prebuild must build it, a row builds nothing.
pub fn missing(key: &str) -> String {
    format!(
        "prebuilt record lacks `{key}`: check's prebuild must build it (gate_build.rs `Group`); a gate row runs no cargo"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A record written is read back equal; directories are not files.
    #[test]
    fn record_round_trips_and_lists_files() {
        let d = std::env::temp_dir().join(format!("xtask-build-record-{}", std::process::id()));
        std::fs::create_dir_all(&d).expect("temp dir");
        let bin = d.join("atld");
        std::fs::write(&bin, b"x").expect("bin");
        let mut rec = Record::default();
        rec.insert("converter/atl-services/atl-am", &bin);
        rec.insert("release/dir", &d);
        let file = d.join("record.tsv");
        rec.write(&file).expect("write");
        let back = Record::read(&file).expect("read");
        assert_eq!(back, rec);
        assert_eq!(back.files(), vec![bin]);
        assert_eq!(back.get("release/dir"), Some(d.as_path()));
        std::fs::remove_dir_all(&d).expect("cleanup");
    }
}
