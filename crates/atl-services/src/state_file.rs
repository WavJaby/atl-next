// SPDX-License-Identifier: MIT
//! The one file envelope for state the services persist in their state dir (atld `--system-dir`): JSON `{"version": N, ...state}`, written temp + fsync + rename so a reader sees the old file or the new
//! one, never a torn one. A missing file is the default state; an unreadable one (I/O, JSON, another version) is the default state too, counted (`refused <diag> load: unreadable file, default state`)
//! and logged, never a start failure -- the next save replaces it. Nothing reads AOSP's XML forms of these files, so the formats are ours.

use std::path::Path;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
struct Versioned<T> {
    version: u32,
    #[serde(flatten)]
    state: T,
}

#[derive(Deserialize)]
struct VersionOnly {
    version: u32,
}

/// The state in `file` written by [`save`] with `version`; None when the file is missing or unreadable (the latter counted under `diag` and logged).
pub fn load<T: DeserializeOwned>(file: &Path, version: u32, diag: &str) -> Option<T> {
    let bytes = match std::fs::read(file) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => return unreadable(file, diag, &e.to_string()),
    };
    match serde_json::from_slice::<VersionOnly>(&bytes) {
        Ok(v) if v.version != version => {
            return unreadable(
                file,
                diag,
                &format!("version {} (this build reads {version})", v.version),
            );
        }
        Ok(_) => {}
        Err(e) => return unreadable(file, diag, &e.to_string()),
    }
    match serde_json::from_slice::<Versioned<T>>(&bytes) {
        Ok(v) => Some(v.state),
        Err(e) => unreadable(file, diag, &e.to_string()),
    }
}

/// Writes `state` with `version` over `file`: a temp file beside it, fsync, rename. The parent dir is made when missing.
pub fn save<T: Serialize>(file: &Path, version: u32, state: &T) -> std::io::Result<()> {
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let bytes =
        serde_json::to_vec_pretty(&Versioned { version, state }).map_err(std::io::Error::other)?;
    let mut temp = file.as_os_str().to_owned();
    temp.push(".tmp");
    {
        use std::io::Write;
        let mut f = std::fs::File::create(&temp)?;
        f.write_all(&bytes)?;
        f.sync_all()?;
    }
    std::fs::rename(&temp, file)
}

fn unreadable<T>(file: &Path, diag: &str, why: &str) -> Option<T> {
    use std::io::Write;
    atl_diag::refused(diag, "load: unreadable file, default state");
    let _ = writeln!(
        std::io::stderr(),
        "services: {diag}: {}: unreadable ({why}): default state",
        file.display()
    );
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct S {
        n: u32,
    }

    fn scratch(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("atl-state-file-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn round_trip_and_missing() {
        let f = scratch("rt").join("sub/s.json");
        assert_eq!(load::<S>(&f, 1, "t"), None);
        save(&f, 1, &S { n: 7 }).expect("save");
        assert_eq!(load::<S>(&f, 1, "t"), Some(S { n: 7 }));
        // the flattened form: version beside the state's own fields
        let text = std::fs::read_to_string(&f).expect("read");
        assert!(
            text.contains("\"version\": 1") && text.contains("\"n\": 7"),
            "{text}"
        );
    }

    #[test]
    fn corrupt_or_other_version_is_default() {
        let d = scratch("bad");
        std::fs::create_dir_all(&d).expect("dir");
        let f = d.join("s.json");
        std::fs::write(&f, b"{not json").expect("write");
        assert_eq!(load::<S>(&f, 1, "t"), None);
        std::fs::write(&f, br#"{"version": 2, "n": 1}"#).expect("write");
        assert_eq!(load::<S>(&f, 1, "t"), None);
    }
}
