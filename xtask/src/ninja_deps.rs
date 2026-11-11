// SPDX-License-Identifier: MIT
//! A ninja build's deps log (`<dir>/.ninja_deps`, `deps = gcc` records), read-only: each output's dependencies as its last record names them, what `ninja -t deps` prints.
//! Read from the file, never through `ninja -t deps`: that loads the log for writing, which recompacts it when ninja finds it worth it (`DepsLog::OpenForWrite`), and truncates a torn last record (`DepsLog::Load`) --
//! inside a running build (the licence gate is one of its edges) or beside one (keepset over a slot mid-build) either rewrites the log under the build that appends to it.
//! Here a torn last record (a write in progress) ends the read; every record before it is complete, ninja appending each with one flush.
//!
//! Format, ninja 1.10+ (deps_log.cc, version 4, native byte order): `# ninjadeps\n`, u32 version; then records, each a u32 size word.
//! Size with the high bit set: a deps record of size bytes = i32 output id, u32 mtime low, u32 mtime high, i32 dependency ids. Otherwise a path record: the path, NUL-padded to 4 bytes, then u32 checksum = !id, ids counting path records from 0.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};

const FILE: &str = ".ninja_deps";
const SIGNATURE: &[u8] = b"# ninjadeps\n";
const VERSION: u32 = 4;
const DEPS_FLAG: u32 = 0x8000_0000;

/// Output -> its dependencies (an object's source first, then its headers), paths as the build names them.
pub type Deps = BTreeMap<PathBuf, Vec<PathBuf>>;

/// A read log: paths by id, and each output's dependency ids of its last record.
pub struct Log {
    paths: Vec<PathBuf>,
    records: BTreeMap<u32, Vec<u32>>,
}

impl Log {
    /// Every output's dependencies.
    pub fn deps(&self) -> Deps {
        self.records
            .iter()
            .map(|(&out, deps)| {
                let name = |i: u32| self.paths[i as usize].clone();
                (name(out), deps.iter().map(|&i| name(i)).collect())
            })
            .collect()
    }

    /// Each path a step depends on, once, except the steps whose edge has an output `skip` names: a graph's reads without a map of millions of entries (keepset).
    /// A skipped edge's every output is skipped: ninja records an edge's dependencies under each of its outputs (build.cc, `RecordDeps` per output), so its records are those holding the named output's list.
    pub fn dependencies(&self, skip: impl Fn(&Path) -> bool) -> impl Iterator<Item = &Path> {
        let skipped: Vec<&Vec<u32>> = self
            .records
            .iter()
            .filter(|(out, _)| skip(&self.paths[**out as usize]))
            .map(|(_, deps)| deps)
            .collect();
        let ids: std::collections::BTreeSet<u32> = self
            .records
            .values()
            .filter(|deps| !skipped.contains(deps))
            .flat_map(|deps| deps.iter().copied())
            .collect();
        ids.into_iter().map(|i| self.paths[i as usize].as_path())
    }
}

/// The deps log of the build in `dir`.
pub fn read(dir: &Path) -> Result<Log> {
    let file = dir.join(FILE);
    let bytes = std::fs::read(&file).with_context(|| format!("reading {}", file.display()))?;
    parse(&bytes).with_context(|| file.display().to_string())
}

fn parse(bytes: &[u8]) -> Result<Log> {
    ensure!(bytes.starts_with(SIGNATURE), "not a ninja deps log");
    let mut at = SIGNATURE.len();
    let word = |at: usize| -> Option<u32> {
        Some(u32::from_ne_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
    };
    let version = word(at).context("no version")?;
    if version != VERSION {
        bail!("deps log version {version}, this reader knows {VERSION} (ninja 1.10+)");
    }
    at += 4;
    let mut paths: Vec<PathBuf> = Vec::new();
    // output id -> dependency ids of its last complete record
    let mut records: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
    while let Some(size_word) = word(at) {
        let size = (size_word & !DEPS_FLAG) as usize;
        let Some(body) = bytes.get(at + 4..at + 4 + size) else {
            break; // torn last record
        };
        at += 4 + size;
        let (words, _) = body.as_chunks::<4>();
        let ids: Vec<u32> = words.iter().map(|c| u32::from_ne_bytes(*c)).collect();
        if size_word & DEPS_FLAG != 0 {
            let [out, _mtime_lo, _mtime_hi, deps @ ..] = &ids[..] else {
                bail!("deps record of {size} bytes");
            };
            ensure!(
                deps.iter()
                    .chain([out])
                    .all(|&i| (i as usize) < paths.len()),
                "deps record naming an unknown path id"
            );
            records.insert(*out, deps.to_vec());
        } else {
            ensure!(size >= 4, "path record of {size} bytes");
            let checksum = ids.last().copied().unwrap_or_default();
            ensure!(
                !checksum as usize == paths.len(),
                "path record {} has checksum {checksum:#x}",
                paths.len()
            );
            let text = &body[..size - 4];
            let end = text.iter().rposition(|&b| b != 0).map_or(0, |i| i + 1);
            paths.push(PathBuf::from(
                String::from_utf8_lossy(&text[..end]).into_owned(),
            ));
        }
    }
    Ok(Log { paths, records })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real ninja build's log equals what `ninja -t deps` prints for it (the oracle: ninja's own reader), after a rebuild added a second record for one output.
    /// A torn last record (a build still appending) is dropped, the rest kept.
    #[test]
    fn reads_what_ninja_prints() {
        let tmp = std::env::temp_dir().join(format!("xtask-ninja-deps-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let write = |n: &str, s: &str| std::fs::write(tmp.join(n), s).unwrap();
        write("a.h", "");
        write("b.h", "");
        write("a.c", "#include \"a.h\"\n");
        write("b.c", "#include \"b.h\"\n");
        write("c.h", "");
        write("c.c", "#include \"c.h\"\n");
        // c's edge has two outputs: ninja files its record under each
        write(
            "build.ninja",
            "rule cc\n  command = clang-19 -MD -MF $out.d -c $in -o $out\n  depfile = $out.d\n  deps = gcc\nbuild a.o: cc a.c\nbuild b.o: cc b.c\n\
             rule cc2\n  command = clang-19 -MD -MF c.d -c $in -o c.o && touch c.x\n  depfile = c.d\n  deps = gcc\nbuild c.o c.x: cc2 c.c\n",
        );
        let ninja = |args: &[&str]| {
            let o = std::process::Command::new("ninja")
                .arg("-C")
                .arg(&tmp)
                .args(args)
                .output()
                .unwrap();
            assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
            String::from_utf8(o.stdout).unwrap()
        };
        ninja(&[]);
        // a second record for a.o: the last one counts
        std::thread::sleep(std::time::Duration::from_millis(50));
        write("a.c", "#include \"a.h\"\n#include \"b.h\"\n");
        ninja(&[]);
        let printed = ninja(&["-t", "deps"]);
        let mut oracle = Deps::new();
        let mut target = None;
        for line in printed.lines() {
            if let Some(dep) = line.strip_prefix("    ") {
                oracle
                    .entry(target.clone().unwrap())
                    .or_default()
                    .push(PathBuf::from(dep));
            } else if let Some((t, _)) = line.split_once(": #deps") {
                target = Some(PathBuf::from(t));
            }
        }
        let bytes = std::fs::read(tmp.join(FILE)).unwrap();
        let _ = std::fs::remove_dir_all(&tmp);
        assert_eq!(oracle.len(), 4, "a.o b.o c.o c.x: {printed}");
        let log = parse(&bytes).unwrap();
        assert_eq!(log.deps(), oracle);
        let all: Vec<&Path> = log.dependencies(|_| false).collect();
        assert_eq!(all.len(), 6, "a.c a.h b.c b.h c.c c.h, each once: {all:?}");
        let mut but_a: Vec<&Path> = log.dependencies(|o| o == Path::new("a.o")).collect();
        but_a.sort();
        assert_eq!(but_a, ["b.c", "b.h", "c.c", "c.h"].map(Path::new));
        // naming one output of c's edge skips the other's record too
        let mut but_c: Vec<&Path> = log.dependencies(|o| o == Path::new("c.x")).collect();
        but_c.sort();
        assert_eq!(but_c, ["a.c", "a.h", "b.c", "b.h"].map(Path::new));
        // torn: the last record (a.o's rebuild) cut short
        let torn = parse(&bytes[..bytes.len() - 3]).unwrap().deps();
        assert_eq!(torn.len(), 4);
        assert_eq!(
            torn[Path::new("a.o")],
            [PathBuf::from("a.c"), PathBuf::from("a.h")]
        );
    }

    #[test]
    fn refuses_another_format() {
        assert!(parse(b"# ninjalog v5\n").is_err());
        let mut v3 = SIGNATURE.to_vec();
        v3.extend(3u32.to_ne_bytes());
        assert!(parse(&v3).is_err());
    }
}
