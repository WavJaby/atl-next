// SPDX-License-Identifier: MIT
//! `launcher-closure.toml`: what of the host the app launcher's process may reach. Column meanings live in that file's header.
//! Two judges read it: `xtask check` step `launcher-closure` ([`check_own`]: the launcher's DT_NEEDED closure as glibc's own loader resolves it over the runtime library path) and `xtask test window --stage hwui` ([`judge_bindings`]: one UI-mode run's `LD_DEBUG=bindings` records).
//! Membership in "ours / AOSP" is derived, never listed: an object is host when it resolves outside the runtime library path ([`crate::host_art::runtime_libs`]) in the closure, or under a system lib dir ([`is_host`]) in a run's records.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;

pub const FILE: &str = "launcher-closure.toml";
/// The launcher's package and executable (packages.rs: built into the slot's converter target dir).
const LAUNCHER: &str = "atl-app-launcher";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Table {
    #[serde(default)]
    pub closure: Vec<ClosureRow>,
    #[serde(default)]
    pub binding: Vec<BindingRow>,
    #[serde(default)]
    pub reverse: Vec<ReverseRow>,
    #[serde(default)]
    pub forbidden_reverse: Vec<ForbiddenRow>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Class {
    Glibc,
    CxxRuntime,
    /// a providers.toml `system:` library: needs `provider`, and its bindings need `binding` rows
    System,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClosureRow {
    pub soname: String,
    pub class: Class,
    pub via: String,
    #[serde(default)]
    pub provider: Option<String>,
    pub reason: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BindingRow {
    pub from: String,
    pub to: String,
    pub symbols: Vec<String>,
    pub provider: String,
    pub reason: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReverseRow {
    pub to: String,
    /// ceiling on unique (host object, symbol) bindings into it per run: a growth fails
    pub max_bindings: usize,
    pub reason: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ForbiddenRow {
    pub to: String,
    pub reason: String,
}

/// Parses `launcher-closure.toml` under `root` and checks what needs no build: keys unique, reasons given, each binding's `provider` a `system:` row of providers.toml, no forbidden soname also allowed.
pub fn load(root: &Path) -> Result<Table> {
    let text =
        std::fs::read_to_string(root.join(FILE)).with_context(|| format!("reading {FILE}"))?;
    let providers: toml::Table =
        toml::from_str(&std::fs::read_to_string(root.join(crate::providers::FILE))?)?;
    parse(&text, &providers)
}

fn parse(text: &str, providers: &toml::Table) -> Result<Table> {
    let t: Table = toml::from_str(text).with_context(|| format!("parsing {FILE}"))?;
    let system_row = |what: &str, key: &str| -> Result<()> {
        let provider = providers
            .get(key)
            .and_then(|row| row.get("provider"))
            .and_then(|p| p.as_str());
        ensure!(
            provider.is_some_and(|p| p.starts_with("system:")),
            "{FILE}: {what}: provider {key} is not a `system:` row of {} (got {provider:?})",
            crate::providers::FILE
        );
        Ok(())
    };
    let mut seen = BTreeSet::new();
    for r in &t.closure {
        ensure!(
            seen.insert(("closure", r.soname.clone())),
            "{FILE}: closure row {} twice",
            r.soname
        );
        ensure!(
            !r.reason.trim().is_empty(),
            "{FILE}: closure row {} has no reason",
            r.soname
        );
        match (r.class, &r.provider) {
            (Class::System, Some(p)) => system_row(&format!("closure row {}", r.soname), p)?,
            (Class::System, None) => bail!(
                "{FILE}: closure row {}: class system needs `provider`",
                r.soname
            ),
            (_, Some(_)) => bail!(
                "{FILE}: closure row {}: only class system has a `provider`",
                r.soname
            ),
            (_, None) => {}
        }
    }
    for r in &t.binding {
        ensure!(
            seen.insert(("binding", format!("{} -> {}", r.from, r.to))),
            "{FILE}: binding {} -> {} twice",
            r.from,
            r.to
        );
        ensure!(
            !r.reason.trim().is_empty() && !r.symbols.is_empty(),
            "{FILE}: binding {} -> {} needs symbols and a reason",
            r.from,
            r.to
        );
        system_row(&format!("binding {} -> {}", r.from, r.to), &r.provider)?;
    }
    for r in &t.reverse {
        ensure!(
            seen.insert(("reverse", r.to.clone())),
            "{FILE}: reverse row {} twice",
            r.to
        );
        ensure!(
            !r.reason.trim().is_empty(),
            "{FILE}: reverse row {} has no reason",
            r.to
        );
    }
    for r in &t.forbidden_reverse {
        ensure!(
            !r.reason.trim().is_empty(),
            "{FILE}: forbidden_reverse {} has no reason",
            r.to
        );
        ensure!(
            !seen.contains(&("reverse", r.to.clone())),
            "{FILE}: {} is both a reverse row and forbidden",
            r.to
        );
    }
    Ok(t)
}

/// One resolved object of the closure: what DT_NEEDED names, and the file the loader picked (None: not found).
#[derive(Debug, PartialEq, Eq)]
struct Resolved {
    soname: String,
    path: Option<PathBuf>,
}

/// glibc's `ld.so --list` lines as resolved objects: `\tname => path (0x..)`, `\tname => not found`, `\t/path/ld.so (0x..)` (the interpreter, keyed by its file name). The vdso (no file) is not an object here.
fn parse_list(text: &str) -> Vec<Resolved> {
    let mut found = Vec::new();
    for line in text.lines().map(str::trim) {
        if let Some((name, rest)) = line.split_once(" => ") {
            let path = rest.split(" (").next().unwrap_or(rest).trim();
            found.push(Resolved {
                soname: name.to_owned(),
                path: (path != "not found" && !path.is_empty()).then(|| PathBuf::from(path)),
            });
        } else if line.starts_with('/') {
            let path = PathBuf::from(line.split(" (").next().unwrap_or(line));
            let soname = path
                .file_name()
                .map(|f| f.to_string_lossy().into_owned())
                .unwrap_or_default();
            found.push(Resolved {
                soname,
                path: Some(path),
            });
        }
    }
    found
}

/// The ELF interpreter (PT_INTERP) of `exe`: the loader that will resolve its closure at run time.
fn interpreter(exe: &Path) -> Result<PathBuf> {
    use object::Endianness;
    use object::read::elf::{ElfFile64, FileHeader, ProgramHeader};
    let data = std::fs::read(exe).with_context(|| format!("reading {}", exe.display()))?;
    let elf = ElfFile64::<Endianness>::parse(&*data)
        .with_context(|| format!("{}: not a 64-bit ELF", exe.display()))?;
    let e = elf.endian();
    for ph in elf.elf_header().program_headers(e, &*data)? {
        if let Some(interp) = ph.interpreter(e, &*data)? {
            return Ok(PathBuf::from(String::from_utf8_lossy(interp).into_owned()));
        }
    }
    bail!("{}: no PT_INTERP", exe.display())
}

/// Step `launcher-closure` on this checkout's slot; `Some(why)` (a skip) without a slot or a built launcher. The table's build-free rules are checked first, slot or not.
pub fn check_own(root: &Path) -> Result<Option<String>> {
    let table = load(root)?;
    let Some(n) = crate::slot::record_slot(root)? else {
        return Ok(Some("this checkout holds no slot: run `cargo xtask slot assign <n> --execute`, then `cargo xtask native`".into()));
    };
    let out = crate::slot::slot_dir(atl_layout::Layout::of(root)?.workspace(), n);
    let launcher = crate::packages::target_dir(&out)
        .join("release")
        .join(LAUNCHER);
    if !launcher.is_file() {
        return Ok(Some(format!(
            "no {}: run `cargo xtask native` first",
            launcher.display()
        )));
    }
    // reads built binaries: a slot from another source state would judge a stale launcher
    crate::build_state::ensure_current(root, &out)?;
    let lines = check(&table, &launcher, &crate::host_art::runtime_libs(&out)?)?;
    eprintln!("launcher-closure: {lines}");
    Ok(None)
}

/// The launcher's closure against `table`'s closure rows: every host object (resolved outside `runtime_libs`' dirs) has a row, every row is in the closure, every row's `via` names it in DT_NEEDED. Returns the ok summary.
fn check(table: &Table, launcher: &Path, runtime_libs: &[PathBuf]) -> Result<String> {
    let dirs: BTreeSet<&Path> = runtime_libs.iter().filter_map(|l| l.parent()).collect();
    let mut lib_path = std::ffi::OsString::new();
    for (i, d) in dirs.iter().enumerate() {
        if i > 0 {
            lib_path.push(":");
        }
        lib_path.push(d);
    }
    let ld = interpreter(launcher)?;
    let run = Command::new(&ld)
        .arg("--list")
        .arg("--library-path")
        .arg(&lib_path)
        .arg(launcher)
        .env_remove("LD_LIBRARY_PATH")
        .env_remove("LD_PRELOAD")
        .output()
        .with_context(|| format!("spawning {} --list", ld.display()))?;
    let text = String::from_utf8_lossy(&run.stdout);
    ensure!(
        run.status.success(),
        "{} --list {} failed: {}{}",
        ld.display(),
        launcher.display(),
        text,
        String::from_utf8_lossy(&run.stderr)
    );
    let resolved = parse_list(&text);
    let failures = judge_closure(
        table,
        launcher,
        &resolved,
        &|p| dirs.iter().any(|d| p.parent() == Some(*d)),
        &|p| crate::provided_gate::needed(p),
    )?;
    if !failures.is_empty() {
        bail!(
            "{} closure problems ({}):\n  {}",
            failures.len(),
            launcher.display(),
            failures.join("\n  ")
        );
    }
    let host = table.closure.len();
    Ok(format!(
        "{FILE} ok ({} objects, {host} host rows, {} ours/AOSP)",
        resolved.len(),
        resolved.len() - host
    ))
}

/// The problems of `resolved` (the launcher's closure) against the table's closure rows. `inside`: the path is on the runtime library path; `needed`: an object's DT_NEEDED.
fn judge_closure(
    table: &Table,
    launcher: &Path,
    resolved: &[Resolved],
    inside: &dyn Fn(&Path) -> bool,
    needed: &dyn Fn(&Path) -> Result<Vec<String>>,
) -> Result<Vec<String>> {
    let mut failures = Vec::new();
    let mut host = BTreeMap::new();
    for r in resolved {
        match &r.path {
            None => failures.push(format!(
                "{}: not found on the runtime library path or the system's",
                r.soname
            )),
            Some(p) if !inside(p) => {
                host.insert(r.soname.as_str(), p.as_path());
            }
            Some(_) => {}
        }
    }
    let rows: BTreeMap<&str, &ClosureRow> = table
        .closure
        .iter()
        .map(|r| (r.soname.as_str(), r))
        .collect();
    for (soname, path) in &host {
        if !rows.contains_key(soname) {
            failures.push(format!(
                "host library {soname} ({}) is not a row of {FILE}",
                path.display()
            ));
        }
    }
    for row in rows.values() {
        if !host.contains_key(row.soname.as_str()) {
            failures.push(format!(
                "row {} is no longer a host library of the closure: delete it",
                row.soname
            ));
            continue;
        }
        let via = if row.via == LAUNCHER {
            Some(launcher)
        } else {
            resolved
                .iter()
                .find(|r| r.soname == row.via)
                .and_then(|r| r.path.as_deref())
        };
        match via {
            None => failures.push(format!(
                "row {}: via {} is not in the closure",
                row.soname, row.via
            )),
            Some(v) if !needed(v)?.contains(&row.soname) => failures.push(format!(
                "row {}: via {} has no DT_NEEDED {}",
                row.soname, row.via, row.soname
            )),
            Some(_) => {}
        }
    }
    Ok(failures)
}

/// What a run's bindings count as host: a path under a system library dir, or a main program glibc names without a path (`bwrap`, the sandbox wrapper exec'd with the app's environment) other than the launcher.
pub fn is_host(path: &str) -> bool {
    if !path.contains('/') {
        return path != LAUNCHER;
    }
    ["/usr/", "/lib/", "/lib64/"]
        .iter()
        .any(|d| path.starts_with(d))
}

/// One `LD_DEBUG=bindings` record: `binding file <from> [n] to <to> [n]: normal symbol `<sym>'`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Binding {
    pub from: String,
    pub to: String,
    pub symbol: String,
}

/// The binding records in `text` (glibc's lines, other lines ignored).
pub fn parse_bindings(text: &str) -> Vec<Binding> {
    text.lines()
        .filter_map(|line| {
            let (_, rest) = line.split_once("binding file ")?;
            let (from, rest) = rest.split_once(" to ")?;
            let (to, rest) = rest.split_once(": ")?;
            let symbol = rest.split_once('`')?.1.split('\'').next()?;
            let object = |s: &str| s.split(" [").next().unwrap_or(s).to_owned();
            Some(Binding {
                from: object(from),
                to: object(to),
                symbol: symbol.to_owned(),
            })
        })
        .collect()
}

fn base(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// One run's bindings judged against the table.
#[derive(Debug, Default)]
pub struct Judged {
    /// failures, each naming its object and soname
    pub failures: Vec<String>,
    /// app / AOSP / ours -> host bindings matched to a `binding` row (unique records)
    pub declared: usize,
    /// O4: host -> ours/AOSP records, raw and unique (from, to, symbol), per target soname (unique)
    pub reverse_records: usize,
    pub reverse_unique: usize,
    pub reverse_by_to: BTreeMap<String, usize>,
}

/// `records` of one UI-mode run against `table`. `module`: the platform module's file name (its own bindings to its host closure are what it is for).
/// (a) a non-host object other than `module` binds host only to closure rows (glibc, C++ runtime) or a `binding` row's (from, to, symbol); every binding row symbol occurs (a row whose binding is gone is deleted, e.g. V0's libvulkan).
/// (b) host -> non-host: each target soname is a `reverse` row, none is a `forbidden_reverse` one, every reverse row occurs.
pub fn judge_bindings(table: &Table, records: &[Binding], module: &str) -> Judged {
    let mut j = Judged::default();
    // glibc and the C++ runtime serve every object; a system library's bindings are declared one by one
    let allowed: BTreeSet<&str> = table
        .closure
        .iter()
        .filter(|r| r.class != Class::System)
        .map(|r| r.soname.as_str())
        .collect();
    let mut seen_declared = BTreeSet::new();
    let mut undeclared = BTreeMap::<(String, String), BTreeSet<String>>::new();
    let mut reverse = BTreeMap::<String, BTreeSet<(String, String)>>::new();
    for b in records {
        let (from_host, to_host) = (is_host(&b.from), is_host(&b.to));
        if !from_host && to_host && base(&b.from) != module {
            if allowed.contains(base(&b.to)) {
                continue;
            }
            match table.binding.iter().find(|r| {
                r.from == base(&b.from) && r.to == base(&b.to) && r.symbols.contains(&b.symbol)
            }) {
                Some(r) => {
                    seen_declared.insert((r.from.as_str(), r.to.as_str(), b.symbol.clone()));
                }
                None => {
                    undeclared
                        .entry((base(&b.from).to_owned(), base(&b.to).to_owned()))
                        .or_default()
                        .insert(b.symbol.clone());
                }
            }
        } else if from_host && !to_host {
            j.reverse_records += 1;
            reverse
                .entry(base(&b.to).to_owned())
                .or_default()
                .insert((b.from.clone(), b.symbol.clone()));
        }
    }
    for ((from, to), syms) in &undeclared {
        let first: Vec<&str> = syms.iter().take(3).map(String::as_str).collect();
        j.failures.push(format!(
            "undeclared binding {from} -> {to}: {} symbols ({})",
            syms.len(),
            first.join(", ")
        ));
    }
    for r in &table.binding {
        for s in &r.symbols {
            if !seen_declared.contains(&(r.from.as_str(), r.to.as_str(), s.clone())) {
                j.failures.push(format!(
                    "binding row {} -> {} `{s}` did not occur: delete it from {FILE}",
                    r.from, r.to
                ));
            }
        }
    }
    j.declared = seen_declared.len();
    let rows: BTreeMap<&str, &ReverseRow> =
        table.reverse.iter().map(|r| (r.to.as_str(), r)).collect();
    for (to, pairs) in &reverse {
        if table.forbidden_reverse.iter().any(|f| f.to == *to) {
            let from: BTreeSet<&str> = pairs.iter().map(|(f, _)| base(f)).collect();
            j.failures
                .push(format!("forbidden reverse binding: host {from:?} -> {to}"));
        } else if let Some(row) = rows.get(to.as_str()) {
            if pairs.len() > row.max_bindings {
                j.failures.push(format!(
                    "reverse {to}: {} host bindings > max_bindings {}",
                    pairs.len(),
                    row.max_bindings
                ));
            }
        } else {
            let (f, s) = pairs.iter().next().expect("a target has a binding");
            j.failures.push(format!(
                "new reverse soname {to}: {} host bindings (first {} `{s}`)",
                pairs.len(),
                base(f)
            ));
        }
        j.reverse_unique += pairs.len();
        j.reverse_by_to.insert(to.clone(), pairs.len());
    }
    for r in &table.reverse {
        if !reverse.contains_key(&r.to) {
            j.failures.push(format!(
                "reverse row {} did not occur: delete it from {FILE}",
                r.to
            ));
        }
    }
    j
}

#[cfg(test)]
mod tests {
    use super::*;

    fn providers() -> toml::Table {
        toml::from_str(
            "[\"libvulkan\"]\nprovider = \"system:vulkan\"\n[\"libz\"]\nprovider = \"none\"\n",
        )
        .unwrap()
    }

    const TABLE: &str = r#"
[[closure]]
soname = "libc.so.6"
class = "glibc"
via = "atl-app-launcher"
reason = "C library"

[[binding]]
from = "libhwui.so"
to = "libvulkan.so.1"
symbols = ["vkGetInstanceProcAddr"]
provider = "libvulkan"
reason = "system:vulkan"

[[reverse]]
to = "libz.so"
max_bindings = 1
reason = "host png binds AOSP zlib in global scope"

[[forbidden_reverse]]
to = "libcrypto.so"
reason = "BoringSSL vs host OpenSSL"
"#;

    fn rec(from: &str, to: &str, symbol: &str) -> Binding {
        Binding {
            from: from.into(),
            to: to.into(),
            symbol: symbol.into(),
        }
    }

    /// A glibc record line parses to its objects and symbol.
    #[test]
    fn parses_ld_debug_line() {
        let line = "    123:\tbinding file /a/libhwui.so [0] to /usr/lib/x86_64-linux-gnu/libvulkan.so.1 [0]: normal symbol `vkGetInstanceProcAddr' [VER]";
        assert_eq!(
            parse_bindings(line),
            vec![rec(
                "/a/libhwui.so",
                "/usr/lib/x86_64-linux-gnu/libvulkan.so.1",
                "vkGetInstanceProcAddr"
            )]
        );
    }

    /// The table's build-free rules: a binding's provider must be a system: row.
    #[test]
    fn provider_must_be_system_row() {
        assert!(parse(TABLE, &providers()).is_ok());
        let bad = TABLE.replace("provider = \"libvulkan\"", "provider = \"libz\"");
        assert!(
            parse(&bad, &providers())
                .unwrap_err()
                .to_string()
                .contains("not a `system:` row")
        );
    }

    /// The declared run is clean; each break is named: an undeclared host binding, a new reverse soname, a forbidden one, rows that did not occur.
    #[test]
    fn judges_bindings() {
        let t = parse(TABLE, &providers()).unwrap();
        let clean = [
            rec(
                "/a/libhwui.so",
                "/usr/lib/libvulkan.so.1",
                "vkGetInstanceProcAddr",
            ),
            rec("/a/libhwui.so", "/lib/x86_64-linux-gnu/libc.so.6", "malloc"),
            rec(
                "/m/libatl_platform_ui_linux.so",
                "/usr/lib/libgtk-4.so.1",
                "gtk_init",
            ),
            rec("/usr/lib/libpng16.so.16", "/a/libz.so", "inflate"),
        ];
        let j = judge_bindings(&t, &clean, "libatl_platform_ui_linux.so");
        assert!(j.failures.is_empty(), "{:?}", j.failures);
        assert_eq!((j.declared, j.reverse_unique), (1, 1));
        let broken = [
            rec(
                "/a/libhwui.so",
                "/usr/lib/libharfbuzz.so.0",
                "hb_font_create",
            ),
            rec("/usr/lib/libssl.so.3", "/a/libcrypto.so", "CRYPTO_free"),
            rec("/usr/lib/libxml2.so.2", "/a/libexpat.so", "XML_Parse"),
        ];
        let f = judge_bindings(&t, &broken, "libatl_platform_ui_linux.so")
            .failures
            .join("\n");
        for want in [
            "undeclared binding libhwui.so -> libharfbuzz.so.0",
            "forbidden reverse binding",
            "new reverse soname libexpat.so",
            "binding row libhwui.so -> libvulkan.so.1 `vkGetInstanceProcAddr` did not occur",
            "reverse row libz.so did not occur",
        ] {
            assert!(f.contains(want), "{want} missing in:\n{f}");
        }
        // growth past the row's ceiling
        let grown = [
            rec("/usr/lib/libpng16.so.16", "/a/libz.so", "inflate"),
            rec("/usr/lib/libfreetype.so.6", "/a/libz.so", "inflate"),
        ];
        let f = judge_bindings(&t, &grown, "m.so").failures.join("\n");
        assert!(
            f.contains("reverse libz.so: 2 host bindings > max_bindings 1"),
            "{f}"
        );
    }

    /// `ld.so --list` lines: resolved, not found, the interpreter by file name, the vdso skipped.
    #[test]
    fn parses_ld_list() {
        let text = "\tlinux-vdso.so.1 (0x00007ffd)\n\tlibsigchain.so => /o/device/libsigchain/libsigchain.so (0x1)\n\tlibx.so => not found\n\t/lib64/ld-linux-x86-64.so.2 (0x2)\n";
        let r = parse_list(text);
        assert_eq!(r.len(), 3);
        assert_eq!(r[1].path, None);
        assert_eq!(r[2].soname, "ld-linux-x86-64.so.2");
    }

    /// A host library without a row, a row gone from the closure, a row whose via does not need it: each named.
    #[test]
    fn judges_closure() {
        let t = parse(TABLE, &providers()).unwrap();
        let resolved = vec![
            Resolved {
                soname: "libsigchain.so".into(),
                path: Some("/o/libsigchain.so".into()),
            },
            Resolved {
                soname: "libgtk-4.so.1".into(),
                path: Some("/usr/lib/libgtk-4.so.1".into()),
            },
        ];
        let f = judge_closure(
            &t,
            Path::new("/o/atl-app-launcher"),
            &resolved,
            &|p| p.starts_with("/o"),
            &|_| Ok(vec![]),
        )
        .unwrap()
        .join("\n");
        assert!(f.contains("host library libgtk-4.so.1"), "{f}");
        assert!(f.contains("row libc.so.6 is no longer"), "{f}");
        let ok = vec![Resolved {
            soname: "libc.so.6".into(),
            path: Some("/lib/libc.so.6".into()),
        }];
        let needs = |p: &Path| {
            Ok(if p.ends_with(LAUNCHER) {
                vec!["libc.so.6".to_owned()]
            } else {
                vec![]
            })
        };
        assert!(
            judge_closure(
                &t,
                Path::new("/o/atl-app-launcher"),
                &ok,
                &|p| p.starts_with("/o"),
                &needs
            )
            .unwrap()
            .is_empty()
        );
        let none = |_: &Path| Ok(vec![]);
        let f = judge_closure(
            &t,
            Path::new("/o/atl-app-launcher"),
            &ok,
            &|p| p.starts_with("/o"),
            &none,
        )
        .unwrap();
        assert!(f[0].contains("has no DT_NEEDED libc.so.6"), "{f:?}");
    }
}
