// SPDX-License-Identifier: MIT
//! Which licence a source file carries: its SPDX tag, else its notice read by fixed phrase rules.
//! No similarity matching: GPL-2 and GPL-3 notices differ by one digit, and a wrong answer is worse
//! than none. Anything the rules do not recognise is reported, never guessed.

use std::collections::BTreeMap;

use anyhow::{Result, bail};

use crate::licenses::{PathLicence, Upstream};
use crate::spdx;

/// Lines searched for the leading comment block that holds the notice.
const NOTICE_LINES: usize = 80;

#[derive(Debug, PartialEq)]
pub enum Identified {
    Tagged(String),
    Notice(String),
    /// No licence wording in the header: the upstream's `unlabelled` licence applies.
    NoNotice,
    /// Licence wording no rule recognises, an unknown exception, or more than one licence family.
    Unrecognised,
}

pub fn identify(text: &str) -> Identified {
    if let Some(tag) = spdx::tag_value(text, spdx::UPSTREAM_TAG_LINES) {
        return Identified::Tagged(tag.to_owned());
    }
    let n = normalize(&header(text));
    let mentions = [
        "licen",
        "public domain",
        "redistribut",
        "permission is hereby granted",
        "permission to use",
        "gpl",
    ];
    if !mentions.iter().any(|m| n.contains(m)) {
        return Identified::NoNotice;
    }
    // an id the SPDX list does not have (e.g. a notice saying "GPL version 2.1") is not a reading
    classify(&n)
        .filter(|id| ::spdx::Expression::parse(id).is_ok())
        .map_or(Identified::Unrecognised, Identified::Notice)
}

#[derive(clap::Args)]
pub struct Args {
    /// Upstream checkout to scan, as named under src/ (e.g. qemu, aosp/art).
    upstream: String,
    /// Only files under this directory of the checkout.
    #[arg(long)]
    under: Option<String>,
    /// List every file that cannot be resolved (default: the first 10).
    #[arg(long)]
    list: bool,
}

/// Source files the scan reads; build files and docs carry no shipped code.
const SOURCE_EXTENSIONS: &[&str] = &[
    "c", "h", "cc", "cpp", "cxx", "hh", "hpp", "inc", "S", "s", "java", "kt", "rs", "go", "m", "mm",
];

pub fn run(root: &std::path::Path, args: &Args) -> Result<std::process::ExitCode> {
    use anyhow::Context;
    let sources = crate::sources::load(root)?;
    let licenses = crate::licenses::load(root, &sources.upstream_names())?;
    let all = sources.checkouts(None);
    let c = all
        .iter()
        .find(|c| c.dir.display().to_string() == args.upstream)
        .with_context(|| format!("no upstream {}", args.upstream))?;
    let checkout = atl_layout::Layout::of(root)?.locate(&c.dir).dir;
    let base = args
        .under
        .as_deref()
        .map_or(checkout.clone(), |u| checkout.join(u));
    anyhow::ensure!(
        base.is_dir(),
        "{} is not fetched (xtask fetch --execute)",
        base.display()
    );
    let upstream = licenses.upstream.get(&c.licence_key);
    let mut files = Vec::new();
    walk(&base, &mut files)?;
    let (mut counts, mut failed) = (BTreeMap::<String, usize>::new(), Vec::new());
    for f in &files {
        let rel = f.strip_prefix(&checkout).unwrap_or(f).display().to_string();
        let text = String::from_utf8_lossy(&std::fs::read(f)?).into_owned();
        match resolve(&rel, &text, upstream) {
            Ok(id) => *counts.entry(id).or_default() += 1,
            Err(e) => failed.push(format!("{e:#}")),
        }
    }
    for (id, n) in &counts {
        println!("licence: {n:6}  {id}");
    }
    let shown = if args.list {
        failed.len()
    } else {
        failed.len().min(10)
    };
    for f in &failed[..shown] {
        println!("licence: unresolved  {f}");
    }
    println!(
        "licence: {} files under {}, {} resolved, {} unresolved",
        files.len(),
        base.display(),
        files.len() - failed.len(),
        failed.len()
    );
    Ok(if failed.is_empty() {
        std::process::ExitCode::SUCCESS
    } else {
        std::process::ExitCode::FAILURE
    })
}

fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        // a symlink's target is scanned where it lives (or is outside the tree)
        if entry.file_type()?.is_symlink() {
            continue;
        }
        let path = entry.path();
        if path.is_dir() {
            if path.file_name().is_some_and(|n| n != ".git") {
                walk(&path, out)?;
            }
        } else if path
            .extension()
            .is_some_and(|e| SOURCE_EXTENSIONS.iter().any(|x| e == *x))
        {
            out.push(path);
        }
    }
    Ok(())
}

/// Licence of `path` in an upstream: per-path entry (longest prefix) > SPDX tag > notice >
/// `unlabelled` (files with no notice only) > error.
pub fn resolve(path: &str, text: &str, upstream: Option<&Upstream>) -> Result<String> {
    if let Some(entry) = upstream.and_then(|u| longest_prefix(&u.paths, path)) {
        return Ok(entry.licence.clone());
    }
    match identify(text) {
        Identified::Tagged(tag) => normalize_tag(&tag).map_err(|e| {
            anyhow::anyhow!(
                "{path}: SPDX tag `{tag}` is not a valid expression ({e}); add a `paths` entry"
            )
        }),
        Identified::Notice(e) => Ok(e),
        Identified::NoNotice => match upstream.and_then(|u| u.unlabelled.clone()) {
            Some(l) => Ok(l),
            None => bail!("{path}: no licence notice and no `unlabelled` licence for its upstream"),
        },
        Identified::Unrecognised => {
            bail!(
                "{path}: licence notice not recognised; add a `paths` entry for it in licenses.toml"
            )
        }
    }
}

/// An upstream's tag as a strict SPDX expression: deprecated GNU ids spelled out (`GPL-2.0` ->
/// `GPL-2.0-only`, `GPL-2.0+` -> `GPL-2.0-or-later`, as the SPDX list defines them), operators in
/// upper case. Anything else that does not parse strictly is rejected.
fn normalize_tag(tag: &str) -> Result<String> {
    let spaced = tag.replace('(', " ( ").replace(')', " ) ");
    let words: Vec<String> = spaced
        .split_whitespace()
        .map(|w| match w.to_ascii_lowercase().as_str() {
            "or" => "OR".to_owned(),
            "and" => "AND".to_owned(),
            "with" => "WITH".to_owned(),
            _ => deprecated_gnu(w).unwrap_or_else(|| w.to_owned()),
        })
        .collect();
    let expr = words.join(" ").replace("( ", "(").replace(" )", ")");
    ::spdx::Expression::parse(&expr)?;
    Ok(expr)
}

fn deprecated_gnu(id: &str) -> Option<String> {
    let (core, plus) = id.strip_suffix('+').map_or((id, false), |c| (c, true));
    let family = ["AGPL-", "LGPL-", "GPL-"]
        .into_iter()
        .find(|f| core.starts_with(f))?;
    matches!(&core[family.len()..], "1.0" | "2.0" | "2.1" | "3.0")
        .then(|| format!("{core}-{}", if plus { "or-later" } else { "only" }))
}

fn longest_prefix<'a>(
    paths: &'a BTreeMap<String, PathLicence>,
    path: &str,
) -> Option<&'a PathLicence> {
    paths
        .iter()
        .filter(|(p, _)| path.starts_with(p.as_str()))
        .max_by_key(|(p, _)| p.len())
        .map(|(_, e)| e)
}

/// The leading comment block: comment and blank lines until the first code line.
fn header(text: &str) -> String {
    let mut out = Vec::new();
    for line in text.lines().take(NOTICE_LINES) {
        let t = line.trim_start();
        // `#include` / `#define` are code; `# ` and `#!` are comments
        let hash_comment = t == "#" || t.starts_with("# ") || t.starts_with("#!");
        let comment = t.is_empty()
            || t.starts_with("/*")
            || t.starts_with('*')
            || t.starts_with("//")
            || hash_comment;
        if !comment {
            break;
        }
        out.push(t);
    }
    out.join("\n")
}

/// Lower case; letters, digits, `.` and `+` kept, everything else a word break; trailing dots dropped.
fn normalize(s: &str) -> String {
    let mapped: String = s
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '+' {
                c.to_ascii_lowercase()
            } else {
                ' '
            }
        })
        .collect();
    mapped
        .split_whitespace()
        .map(|w| w.trim_end_matches('.'))
        .filter(|w| !w.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

fn classify(n: &str) -> Option<String> {
    let has = |p: &str| n.contains(p);
    let mut found = Vec::new();
    if has("apache license version 2.0") || has("apache license 2.0") {
        found.push("Apache-2.0".to_owned());
    }
    if let Some(gnu) = gnu_family(n) {
        found.push(gnu?);
    }
    if has("permission is hereby granted free of charge to any person obtaining a copy") {
        found.push("MIT".to_owned());
    }
    if has(
        "permission to use copy modify and or distribute this software for any purpose with or without fee",
    ) || has(
        "permission to use copy modify and distribute this software for any purpose with or without fee",
    ) {
        found.push("ISC".to_owned());
    }
    if has("redistribution and use in source and binary forms") {
        found.push(
            if has("all advertising materials mentioning features") {
                if has("university of california") {
                    "BSD-4-Clause-UC"
                } else {
                    "BSD-4-Clause"
                }
            } else if has("neither the name")
                || has("may be used to endorse or promote products derived")
            {
                "BSD-3-Clause"
            } else {
                "BSD-2-Clause"
            }
            .to_owned(),
        );
    }
    if has(
        "permission is granted to anyone to use this software for any purpose including commercial applications",
    ) {
        found.push("Zlib".to_owned());
    }
    if has("public domain") {
        found.push("LicenseRef-Public-Domain".to_owned());
    }
    // one family, or nothing: dual or mixed notices go to a per-path entry
    match found.as_slice() {
        [one] => Some(one.clone()),
        _ => None,
    }
}

/// GNU family: None = not a GNU notice; Some(None) = a GNU notice the rules cannot pin down.
fn gnu_family(n: &str) -> Option<Option<String>> {
    const FAMILIES: [(&str, &str); 10] = [
        ("affero general public license", "AGPL"),
        ("lesser general public license", "LGPL"),
        ("library general public license", "LGPL"),
        ("gnu lgpl", "LGPL"),
        ("lgplv", "LGPL"),
        ("general public license", "GPL"),
        ("gnu gpl", "GPL"),
        ("gplv", "GPL"),
        (" gpl ", "GPL"),
        (" gpl", "GPL"),
    ];
    let padded = format!(" {n} ");
    let (pat, kind) = FAMILIES.iter().find(|(p, _)| padded.contains(p))?;
    let at = padded.find(pat).unwrap_or(0);
    Some(gnu_id(&padded, at, kind))
}

fn gnu_id(n: &str, at: usize, kind: &str) -> Option<String> {
    let (major, minor, plus) = version_after(&n[at..])?;
    let later = plus || n.contains("any later version") || n.contains("or later");
    // "version 2 … or version 3" is a choice of two, not "or later"
    if !later && n.contains("or version 3") {
        return None;
    }
    let ver = match (kind, major, minor) {
        ("LGPL", 2, None) => "2.0".to_owned(),
        (_, m, Some(mi)) => format!("{m}.{mi}"),
        (_, m, None) => format!("{m}.0"),
    };
    let base = format!("{kind}-{ver}-{}", if later { "or-later" } else { "only" });
    let exception = if n.contains("classpath exception") {
        Some("Classpath-exception-2.0")
    } else if n.contains("bison") && n.contains("special exception") {
        Some("Bison-exception-2.2")
    } else if n.contains("autoconf") && n.contains("exception") {
        Some(if major == 3 {
            "Autoconf-exception-3.0"
        } else {
            "Autoconf-exception-2.0"
        })
    } else if n.contains("gcc runtime library exception") {
        Some("GCC-exception-3.1")
    } else if n.contains("linux syscall note") || n.contains("normal system calls") {
        Some("Linux-syscall-note")
    } else if n.contains("exception") {
        return None; // an exception we cannot name
    } else {
        None
    };
    Some(match exception {
        Some(e) => format!("{base} WITH {e}"),
        None => base,
    })
}

/// First version after the family name: "version 2", "version 2.1", "v2", "gplv2+", "2+".
fn version_after(s: &str) -> Option<(u32, Option<u32>, bool)> {
    let words: Vec<&str> = s.split(' ').filter(|w| !w.is_empty()).take(40).collect();
    for (i, w) in words.iter().enumerate() {
        let candidate = if *w == "version" {
            words.get(i + 1).copied()
        } else {
            let bare = w
                .trim_start_matches("lgpl")
                .trim_start_matches("agpl")
                .trim_start_matches("gpl");
            bare.strip_prefix('v').or((i <= 3).then_some(bare))
        };
        if let Some(v) = candidate.and_then(parse_version) {
            return Some(v);
        }
    }
    None
}

fn parse_version(s: &str) -> Option<(u32, Option<u32>, bool)> {
    let (s, plus) = s.strip_suffix('+').map_or((s, false), |s| (s, true));
    let (major, minor) = match s.split_once('.') {
        Some((a, b)) => (a, Some(b)),
        None => (s, None),
    };
    let major: u32 = major.parse().ok().filter(|m| (1..=3).contains(m))?;
    let minor = match minor {
        Some(m) => Some(m.parse().ok().filter(|m: &u32| *m <= 1)?),
        None => None,
    };
    Some((major, minor, plus))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(text: &str) -> Identified {
        identify(text)
    }

    fn notice(id: &str) -> Identified {
        Identified::Notice(id.to_owned())
    }

    // Notice texts below are the licences' own recommended notices (FSF "How to apply", Apache
    // appendix, OSI templates) or verbatim upstream variants; each expected id is what that text states.

    #[test]
    fn fsf_gpl_notices_by_version() {
        let fsf = |v: u32| {
            format!(
                "/*\n * This program is free software; you can redistribute it and/or modify\n * it under the terms of the GNU General Public License as published by\n * the Free Software Foundation; either version {v} of the License, or\n * (at your option) any later version.\n */\n#include <x.h>\n"
            )
        };
        // the case similarity matching gets wrong: v2-or-later read as GPL-3.0+
        assert_eq!(id(&fsf(2)), notice("GPL-2.0-or-later"));
        assert_eq!(id(&fsf(3)), notice("GPL-3.0-or-later"));
    }

    #[test]
    fn qemu_short_notices() {
        let only = "/*\n * This work is licensed under the terms of the GNU GPL, version 2.  See\n * the COPYING file in the top-level directory.\n */\n";
        assert_eq!(id(only), notice("GPL-2.0-only"));
        let later = "/*\n * This work is licensed under the terms of the GNU GPL, version 2 or later.\n */\n";
        assert_eq!(id(later), notice("GPL-2.0-or-later"));
        assert_eq!(
            id("/* This code is licensed under the GNU GPL v2. */\n"),
            notice("GPL-2.0-only")
        );
        assert_eq!(
            id("// Licensed under GPLv2+\nint x;\n"),
            notice("GPL-2.0-or-later")
        );
    }

    #[test]
    fn lgpl_notices() {
        let lgpl21 = "/*\n * This library is free software; you can redistribute it and/or\n * modify it under the terms of the GNU Lesser General Public\n * License as published by the Free Software Foundation; either\n * version 2.1 of the License, or (at your option) any later version.\n */\n";
        assert_eq!(id(lgpl21), notice("LGPL-2.1-or-later"));
        let lgpl2 = "/* GNU Library General Public License as published by the Free Software Foundation; version 2. */\n";
        assert_eq!(id(lgpl2), notice("LGPL-2.0-only"));
    }

    #[test]
    fn classpath_and_unknown_exceptions() {
        let oracle = "/*\n * This code is free software; you can redistribute it and/or modify it\n * under the terms of the GNU General Public License version 2 only, as\n * published by the Free Software Foundation.  Oracle designates this\n * particular file as subject to the \"Classpath\" exception as provided\n * by Oracle in the LICENSE file that accompanied this code.\n */\n";
        assert_eq!(
            id(oracle),
            notice("GPL-2.0-only WITH Classpath-exception-2.0")
        );
        let odd = "/* GNU General Public License version 2. As a special exception, you may link with Foo. */\n";
        assert_eq!(id(odd), Identified::Unrecognised);
    }

    #[test]
    fn permissive_notices() {
        let apache = "/*\n * Licensed under the Apache License, Version 2.0 (the \"License\");\n * you may not use this file except in compliance with the License.\n */\n";
        assert_eq!(id(apache), notice("Apache-2.0"));
        let mit = "/* Permission is hereby granted, free of charge, to any person obtaining a copy of this software */\n";
        assert_eq!(id(mit), notice("MIT"));
        let isc = "/* Permission to use, copy, modify, and/or distribute this software for any purpose with or without fee is hereby granted */\n";
        assert_eq!(id(isc), notice("ISC"));
        let bsd = |extra: &str| {
            format!(
                "/* Redistribution and use in source and binary forms, with or without modification, are permitted provided that {extra} */\n"
            )
        };
        assert_eq!(id(&bsd("conditions are met")), notice("BSD-2-Clause"));
        assert_eq!(
            id(&bsd("Neither the name of the project")),
            notice("BSD-3-Clause")
        );
        assert_eq!(
            id(&bsd(
                "All advertising materials mentioning features ... University of California"
            )),
            notice("BSD-4-Clause-UC")
        );
    }

    #[test]
    fn tag_first_none_and_ambiguous() {
        assert_eq!(
            id("// SPDX-License-Identifier: MIT\n/* GNU GPL v2 */\n"),
            Identified::Tagged("MIT".into())
        );
        assert_eq!(
            id("/*\n * Copyright (c) 2010 Someone\n */\n#include <x.h>\n"),
            Identified::NoNotice
        );
        let dual = "/* Licensed under the Apache License, Version 2.0, or the GNU GPL v2 */\n";
        assert_eq!(id(dual), Identified::Unrecognised);
        assert_eq!(
            id("/* Licensed under the Foo Public License */\n"),
            Identified::Unrecognised
        );
        // there is no GPL 2.1 (only LGPL 2.1): a mistaken notice is not turned into an id
        let no_such = "/* GNU General Public License version 2.1 or any later version */\n";
        assert_eq!(id(no_such), Identified::Unrecognised);
    }

    #[test]
    fn include_lines_end_the_header() {
        // the notice after code is not the file's header notice
        assert_eq!(
            id("#include <a.h>\n/* GNU GPL v2 */\n"),
            Identified::NoNotice
        );
    }

    #[test]
    fn upstream_tags_normalized() {
        // deprecated ids as the SPDX list defines them; operators are case-insensitive in the spec
        let n = |t: &str| normalize_tag(t).unwrap();
        assert_eq!(
            n("GPL-2.0 WITH Linux-syscall-note"),
            "GPL-2.0-only WITH Linux-syscall-note"
        );
        assert_eq!(n("GPL-2.0+"), "GPL-2.0-or-later");
        assert_eq!(n("(LGPL-2.1 or MIT)"), "(LGPL-2.1-only OR MIT)");
        assert_eq!(n("GPL-2.0-or-later"), "GPL-2.0-or-later");
        assert!(normalize_tag("MIT CC0-1.0").is_err());
    }

    #[test]
    fn identified_ids_are_valid_spdx() {
        for e in [
            "GPL-2.0-only WITH Classpath-exception-2.0",
            "LGPL-2.1-or-later",
            "BSD-4-Clause-UC",
            "GPL-3.0-or-later WITH Bison-exception-2.2",
            "LicenseRef-Public-Domain",
        ] {
            assert!(::spdx::Expression::parse(e).is_ok(), "{e}");
        }
    }
}
