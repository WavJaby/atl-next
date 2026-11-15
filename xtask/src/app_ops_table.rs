// SPDX-License-Identifier: MIT
//! Step `tables` of `xtask check`: `atl_services_types::app_ops::OPS` and `NUM_OP` equal AppOpsManager.sAppOpInfos at the pin, row by row (default mode, switch op, permission) and in length (`_NUM_OP`).
//! The pinned Java is read with `git show` at the commit `sources.toml` pins, never the checkout's working tree. Parsing is textual, like `service_mirrors`: comments go first, and each `new AppOpInfo.Builder(...)` chain is read as a sequence of calls, so a call split over lines and two calls on one line parse alike.
//! A flag-gated permission `Flags.f() ? A : B` takes B: the flag is assumed off, because aconfig values are read only by the converter (atl-parcelables/src/aconfig.rs). The flag names come back so the step prints them.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use anyhow::{Context, Result, bail, ensure};
use atl_services_types::app_ops::{NUM_OP, OPS};

use crate::git::git_raw;
use crate::sources::Aosp;

/// AOSP project holding `sAppOpInfos`; its pin is the `[aosp.projects]` row of `sources.toml`.
const PROJECT: &str = "frameworks/base";
const FILE: &str = "core/java/android/app/AppOpsManager.java";
/// The array head, after comments are stripped.
const ARRAY_HEAD: &str = "sAppOpInfos = new AppOpInfo[]{";
/// Opens one chain per op, in op order: `new AppOpInfo.Builder(OP_X, OPSTR_X, "NAME")`, calls, `.build()`.
const BUILDER: &str = "new AppOpInfo.Builder";
/// AppOpInfo.Builder's `mDefaultMode` when `setDefaultMode` is absent (AppOpInfo.java:136 at the pin).
const ABSENT_DEFAULT_MODE: &str = "MODE_DEFAULT";
/// Non-literal default modes, resolved by name: (expression, MODE_ constant it answers on this desktop).
/// `getSystemAlertWindowDefault()` (AppOpsManager.java:11663-11681 at the pin) answers MODE_IGNORED only on a low-RAM device; ATL's desktop is not low-RAM, so MODE_DEFAULT.
const NON_LITERAL_DEFAULTS: [(&str, &str); 1] = [("getSystemAlertWindowDefault()", "MODE_DEFAULT")];
/// The value of a `Manifest.permission.X` constant is this prefix plus X.
const PERMISSION_PREFIX: &str = "android.permission.";

/// One AppOpInfo as the pin states it.
#[derive(Debug, Clone, PartialEq)]
pub struct PinOp {
    /// The first Builder argument, `OP_*`.
    pub name: String,
    pub default_mode: i32,
    /// The switch op's index; the op's own index when `setSwitchCode` is absent.
    pub switch: usize,
    pub permission: Option<String>,
}

/// AppOpsManager's op table as the pin states it.
#[derive(Debug, PartialEq)]
pub struct Table {
    /// In op order: the index is the op code.
    pub ops: Vec<PinOp>,
    /// The `_NUM_OP` literal.
    pub num_op: usize,
    /// Flag names whose permission took the flag-off branch.
    pub flags: BTreeSet<String>,
}

/// What `check` verified: the op count and the flags assumed off.
#[derive(Debug)]
pub struct Summary {
    pub ops: usize,
    pub flags: BTreeSet<String>,
}

/// Step `tables`: every row of `OPS` and `NUM_OP` against the pinned file. `repo_of` maps a project tree path to its git checkout.
pub fn check(aosp: &Aosp, repo_of: &dyn Fn(&str) -> PathBuf) -> Result<Summary> {
    let commit = aosp
        .projects
        .get(PROJECT)
        .with_context(|| format!("{PROJECT}: no [aosp.projects] row"))?;
    let at = format!("{PROJECT}@{commit}:{FILE}");
    let text = git_raw(&repo_of(PROJECT), &["show", &format!("{commit}:{FILE}")])
        .with_context(|| at.clone())?;
    let pin = parse(&text).with_context(|| at.clone())?;
    let mut errors = Vec::new();
    if NUM_OP != pin.num_op {
        errors.push(format!(
            "_NUM_OP: OPS has {NUM_OP} rows, the pin {}",
            pin.num_op
        ));
    }
    for (i, (ours, theirs)) in OPS.iter().zip(&pin.ops).enumerate() {
        let &(mode, switch, permission) = ours;
        let name = &theirs.name;
        if mode != theirs.default_mode {
            errors.push(format!(
                "op {i} {name}: default mode {mode} in OPS, {} at the pin",
                theirs.default_mode
            ));
        }
        if switch != theirs.switch {
            errors.push(format!(
                "op {i} {name}: switch {switch} in OPS, {} at the pin",
                theirs.switch
            ));
        }
        if permission != theirs.permission.as_deref() {
            errors.push(format!(
                "op {i} {name}: permission {permission:?} in OPS, {:?} at the pin",
                theirs.permission
            ));
        }
    }
    if !errors.is_empty() {
        bail!(
            "{} app_ops mismatches with {at}:\n  {}",
            errors.len(),
            errors.join("\n  ")
        );
    }
    Ok(Summary {
        ops: NUM_OP,
        flags: pin.flags,
    })
}

/// The op table of AppOpsManager.java text: the MODE_ constants, `_NUM_OP` and the sAppOpInfos array. Everything else in the file is ignored.
pub fn parse(java: &str) -> Result<Table> {
    let code = strip_comments(java);
    let num_op = usize::try_from(int_const(&code, "_NUM_OP")?).context("_NUM_OP is negative")?;
    let head = code
        .find(ARRAY_HEAD)
        .with_context(|| format!("no `{ARRAY_HEAD}`"))?;
    let open = head + ARRAY_HEAD.len() - 1;
    let body = &code[open + 1..close_of(&code, open)? - 1];
    let chains = parse_chains(body)?;
    ensure!(
        chains.len() == num_op,
        "sAppOpInfos has {} Builder chains, _NUM_OP is {num_op}",
        chains.len()
    );
    let index: BTreeMap<&str, usize> = chains
        .iter()
        .enumerate()
        .map(|(i, c)| (c.name.as_str(), i))
        .collect();
    ensure!(
        index.len() == chains.len(),
        "two Builder chains name the same op constant"
    );
    let mut flags = BTreeSet::new();
    let mut ops = Vec::with_capacity(chains.len());
    for (i, c) in chains.iter().enumerate() {
        let default_mode = match &c.default_mode {
            None => mode_value(&code, ABSENT_DEFAULT_MODE),
            Some(expr) => default_mode_of(&code, expr),
        }
        .with_context(|| format!("{}: setDefaultMode", c.name))?;
        let switch = match &c.switch {
            None => i,
            Some(expr) => *index
                .get(expr.as_str())
                .with_context(|| format!("{}: setSwitchCode({expr}) names no op", c.name))?,
        };
        let permission = match &c.permission {
            None => None,
            Some(expr) => permission_of(expr, &mut flags)
                .with_context(|| format!("{}: setPermission({expr})", c.name))?,
        };
        ops.push(PinOp {
            name: c.name.clone(),
            default_mode,
            switch,
            permission,
        });
    }
    Ok(Table { ops, num_op, flags })
}

/// One `new AppOpInfo.Builder(...)` chain: its op constant and the calls read here, expressions whitespace-normalized.
#[derive(Debug, Default)]
struct Chain {
    name: String,
    default_mode: Option<String>,
    switch: Option<String>,
    permission: Option<String>,
}

/// The chains of the array body, in order. Only commas and chains may stand between them.
fn parse_chains(body: &str) -> Result<Vec<Chain>> {
    let mut out = Vec::new();
    let mut pos = 0;
    loop {
        pos = skip_ws(body, pos);
        if pos == body.len() {
            return Ok(out);
        }
        if body[pos..].starts_with(',') {
            pos += 1;
            continue;
        }
        ensure!(
            body[pos..].starts_with(BUILDER),
            "sAppOpInfos: `{}` is not a Builder chain",
            snippet(&body[pos..])
        );
        let open = pos + BUILDER.len();
        ensure!(
            body[open..].starts_with('('),
            "sAppOpInfos: no `(` after {BUILDER}"
        );
        let close = close_of(body, open)?;
        let args = split_args(&body[open + 1..close - 1]);
        let name = normalize(args.first().copied().unwrap_or_default());
        ensure!(
            name.starts_with("OP_") && is_ident(&name),
            "sAppOpInfos[{}]: first Builder argument `{name}` is not an OP_ constant",
            out.len()
        );
        let mut chain = Chain {
            name,
            ..Chain::default()
        };
        pos = close;
        loop {
            pos = skip_ws(body, pos);
            ensure!(
                body[pos..].starts_with('.'),
                "{}: Builder chain has no build()",
                chain.name
            );
            let method_at = skip_ws(body, pos + 1);
            let method = &body[method_at..method_at + ident_len(&body[method_at..])];
            ensure!(
                !method.is_empty(),
                "{}: `.` without a method name",
                chain.name
            );
            let paren = skip_ws(body, method_at + method.len());
            ensure!(
                body[paren..].starts_with('('),
                "{}: `.{method}` without arguments",
                chain.name
            );
            let end = close_of(body, paren)?;
            let arg = normalize(&body[paren + 1..end - 1]);
            pos = end;
            let field = match method {
                "build" => {
                    ensure!(arg.is_empty(), "{}: build() takes no arguments", chain.name);
                    break;
                }
                "setDefaultMode" => &mut chain.default_mode,
                "setSwitchCode" => &mut chain.switch,
                "setPermission" => &mut chain.permission,
                // setRestriction, setDisableReset, ...: not read here
                _ => continue,
            };
            ensure!(field.is_none(), "{}: `.{method}` called twice", chain.name);
            *field = Some(arg);
        }
        out.push(chain);
    }
}

/// The value of a `MODE_*` constant (AppOpsManager.java), by its full name.
fn mode_value(code: &str, name: &str) -> Result<i32> {
    i32::try_from(int_const(code, name)?).with_context(|| format!("{name} out of range"))
}

/// `setDefaultMode(expr)`: a MODE_ constant, qualified or not, or a NON_LITERAL_DEFAULTS expression.
fn default_mode_of(code: &str, expr: &str) -> Result<i32> {
    let expr = expr.strip_prefix("AppOpsManager.").unwrap_or(expr);
    if let Some(&(_, mode)) = NON_LITERAL_DEFAULTS.iter().find(|(call, _)| *call == expr) {
        return mode_value(code, mode);
    }
    ensure!(
        expr.starts_with("MODE_") && is_ident(expr),
        "`{expr}` is neither a MODE_ constant nor a known non-literal default"
    );
    mode_value(code, expr)
}

/// `setPermission(expr)`: `null`, `[android.]Manifest.permission.X`, or `Flags.f() ? A : B` taking B (flag off). A is never read.
fn permission_of(expr: &str, flags: &mut BTreeSet<String>) -> Result<Option<String>> {
    let Some((cond, branches)) = expr.split_once('?') else {
        return permission_name(expr);
    };
    let call = cond
        .trim()
        .strip_suffix("()")
        .context("flag condition is not `Flags.f()`")?;
    ensure!(
        call.split('.').all(is_ident),
        "flag condition `{call}` is not a dotted name"
    );
    let (_on, off) = branches
        .split_once(':')
        .context("flag-gated permission has no `:`")?;
    flags.insert(call.rsplit('.').next().unwrap_or(call).to_owned());
    permission_name(off.trim())
}

/// `null`, or `[android.]Manifest.permission.X` as `android.permission.X`.
fn permission_name(expr: &str) -> Result<Option<String>> {
    if expr == "null" {
        return Ok(None);
    }
    let konst = expr
        .strip_prefix("android.")
        .unwrap_or(expr)
        .strip_prefix("Manifest.permission.")
        .with_context(|| {
            format!("`{expr}` is neither `null` nor a Manifest.permission constant")
        })?;
    ensure!(is_ident(konst), "`{expr}`: `{konst}` is not an identifier");
    Ok(Some(format!("{PERMISSION_PREFIX}{konst}")))
}

/// `static final int NAME = <literal>;` in `code`, the literal as an integer.
fn int_const(code: &str, name: &str) -> Result<i64> {
    let decl = format!("static final int {name} = ");
    let at = code
        .find(&decl)
        .with_context(|| format!("no `{decl}` at the pin"))?
        + decl.len();
    let end = code[at..]
        .find(';')
        .with_context(|| format!("{name}: no `;` after its value"))?;
    code[at..at + end]
        .trim()
        .parse()
        .with_context(|| format!("{name}: not an integer literal"))
}

/// Comments removed, each replaced by a space (a line comment keeps its newline). String and char literals are kept whole.
fn strip_comments(java: &str) -> String {
    let mut out = String::with_capacity(java.len());
    let mut chars = java.chars().peekable();
    let mut quote = None;
    while let Some(c) = chars.next() {
        if let Some(q) = quote {
            out.push(c);
            if c == '\\' {
                out.extend(chars.next());
            } else if c == q {
                quote = None;
            }
            continue;
        }
        match c {
            '"' | '\'' => {
                quote = Some(c);
                out.push(c);
            }
            '/' if chars.peek() == Some(&'/') => {
                for n in chars.by_ref() {
                    if n == '\n' {
                        out.push('\n');
                        break;
                    }
                }
            }
            '/' if chars.peek() == Some(&'*') => {
                chars.next();
                let mut prev = '\0';
                for n in chars.by_ref() {
                    if prev == '*' && n == '/' {
                        break;
                    }
                    prev = n;
                }
                out.push(' ');
            }
            _ => out.push(c),
        }
    }
    out
}

/// Index just past the bracket closing the `(` or `{` at byte `open`; quoted text is skipped.
fn close_of(s: &str, open: usize) -> Result<usize> {
    let b = s.as_bytes();
    ensure!(
        matches!(b.get(open).copied(), Some(b'(' | b'{')),
        "no bracket at byte {open}"
    );
    let (mut depth, mut quote, mut i) = (0i32, None::<u8>, open);
    while i < b.len() {
        match (quote, b[i]) {
            (Some(_), b'\\') => i += 1,
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, b'"' | b'\'') => quote = Some(b[i]),
            (None, b'(' | b'{') => depth += 1,
            (None, b')' | b'}') => {
                depth -= 1;
                if depth == 0 {
                    return Ok(i + 1);
                }
            }
            (None, _) => {}
        }
        i += 1;
    }
    bail!("unclosed bracket at byte {open}")
}

/// The top-level comma-separated parts of an argument list; quoted text and nested brackets stay whole.
fn split_args(s: &str) -> Vec<&str> {
    let b = s.as_bytes();
    let (mut depth, mut quote, mut start, mut out) = (0i32, None::<u8>, 0, Vec::new());
    let mut i = 0;
    while i < b.len() {
        match (quote, b[i]) {
            (Some(_), b'\\') => i += 1,
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, b'"' | b'\'') => quote = Some(b[i]),
            (None, b'(' | b'{' | b'[') => depth += 1,
            (None, b')' | b'}' | b']') => depth -= 1,
            (None, b',') if depth == 0 => {
                out.push(&s[start..i]);
                start = i + 1;
            }
            (None, _) => {}
        }
        i += 1;
    }
    out.push(&s[start..]);
    out
}

/// The first position at or after `pos` that is not whitespace.
fn skip_ws(s: &str, pos: usize) -> usize {
    pos + s[pos..].len() - s[pos..].trim_start().len()
}

/// Length of the identifier at the start of `s` (ASCII letters, digits, `_`, `$`).
fn ident_len(s: &str) -> usize {
    s.bytes()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == b'_' || *c == b'$')
        .count()
}

fn is_ident(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_')
}

/// Whitespace runs collapsed to one space, so a multi-line expression compares as one line.
fn normalize(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn snippet(s: &str) -> String {
    s.chars().take(60).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The MODE_ constants (AppOpsManager.java:428-467 at the pin) around one sAppOpInfos array of `chains`, declaring `num_op` ops.
    fn java(num_op: usize, chains: &str) -> String {
        let modes = "public static final int MODE_ALLOWED = 0;\npublic static final int MODE_IGNORED = 1;\npublic static final int MODE_ERRORED = 2;\npublic static final int MODE_DEFAULT = 3;\npublic static final int MODE_FOREGROUND = 4;\n";
        format!(
            "{modes}static final int _NUM_OP = {num_op};\nstatic final AppOpInfo[] sAppOpInfos = new AppOpInfo[]{{\n{chains}\n}};\n"
        )
    }

    /// (default mode, switch index, permission) per op.
    fn rows(t: &Table) -> Vec<(i32, usize, Option<&str>)> {
        t.ops
            .iter()
            .map(|op| (op.default_mode, op.switch, op.permission.as_deref()))
            .collect()
    }

    /// The pin's first four Builders (AppOpsManager.java:2803-2819), comments dropped.
    const FIRST_FOUR: &str = r#"
        new AppOpInfo.Builder(OP_COARSE_LOCATION, OPSTR_COARSE_LOCATION, "COARSE_LOCATION")
            .setPermission(android.Manifest.permission.ACCESS_COARSE_LOCATION)
            .setRestriction(UserManager.DISALLOW_SHARE_LOCATION)
            .setAllowSystemRestrictionBypass(new RestrictionBypass(true, false, false))
            .setDefaultMode(AppOpsManager.MODE_ALLOWED).build(),
        new AppOpInfo.Builder(OP_FINE_LOCATION, OPSTR_FINE_LOCATION, "FINE_LOCATION")
            .setPermission(android.Manifest.permission.ACCESS_FINE_LOCATION)
            .setRestriction(UserManager.DISALLOW_SHARE_LOCATION)
            .setAllowSystemRestrictionBypass(new RestrictionBypass(true, false, false))
            .setDefaultMode(AppOpsManager.MODE_ALLOWED).build(),
        new AppOpInfo.Builder(OP_GPS, OPSTR_GPS, "GPS")
            .setSwitchCode(OP_COARSE_LOCATION)
            .setRestriction(UserManager.DISALLOW_SHARE_LOCATION)
            .setDefaultMode(AppOpsManager.MODE_ALLOWED).build(),
        new AppOpInfo.Builder(OP_VIBRATE, OPSTR_VIBRATE, "VIBRATE")
            .setSwitchCode(OP_VIBRATE).setPermission(android.Manifest.permission.VIBRATE)
            .setDefaultMode(AppOpsManager.MODE_ALLOWED).build(),
"#;

    #[test]
    fn first_four_rows_match_the_pin() {
        let t = parse(&java(4, FIRST_FOUR)).expect("fixture parses");
        assert_eq!(t.num_op, 4);
        assert_eq!(
            rows(&t),
            vec![
                // MODE_ALLOWED = 0; an absent switch is the op itself; GPS switches to COARSE_LOCATION (0)
                (0, 0, Some("android.permission.ACCESS_COARSE_LOCATION")),
                (0, 1, Some("android.permission.ACCESS_FINE_LOCATION")),
                (0, 0, None),
                (0, 3, Some("android.permission.VIBRATE")),
            ]
        );
        assert!(t.flags.is_empty());
    }

    #[test]
    fn absent_default_mode_is_mode_default() {
        // AppOpInfo.java:136: mDefaultMode = MODE_DEFAULT = 3; WRITE_SETTINGS sets a permission only
        let chain = r#"
            new AppOpInfo.Builder(OP_WRITE_SETTINGS, OPSTR_WRITE_SETTINGS, "WRITE_SETTINGS")
                .setPermission(android.Manifest.permission.WRITE_SETTINGS).build(),"#;
        let t = parse(&java(1, chain)).expect("fixture parses");
        assert_eq!(
            rows(&t),
            vec![(3, 0, Some("android.permission.WRITE_SETTINGS"))]
        );
    }

    #[test]
    fn flag_gated_permissions_take_the_flag_off_branch_and_name_the_flag() {
        let chains = r#"
            new AppOpInfo.Builder(OP_READ_HEART_RATE, OPSTR_READ_HEART_RATE, "READ_HEART_RATE")
                .setPermission(Flags.replaceBodySensorPermissionEnabled() ?
                    HealthPermissions.READ_HEART_RATE : null)
                .setDefaultMode(AppOpsManager.MODE_ALLOWED).build(),
            new AppOpInfo.Builder(OP_WRITE_SYSTEM_PREFERENCES, OPSTR_WRITE_SYSTEM_PREFERENCES,
                "WRITE_SYSTEM_PREFERENCES").setPermission(
                    com.android.settingslib.flags.Flags.writeSystemPreferencePermissionEnabled()
                    ? Manifest.permission.WRITE_SYSTEM_PREFERENCES : null).build(),"#;
        let t = parse(&java(2, chains)).expect("fixture parses");
        assert_eq!(rows(&t), vec![(0, 0, None), (3, 1, None)]);
        assert_eq!(
            t.flags,
            BTreeSet::from([
                "replaceBodySensorPermissionEnabled".to_owned(),
                "writeSystemPreferencePermissionEnabled".to_owned(),
            ])
        );
    }

    #[test]
    fn commented_out_calls_do_not_count() {
        // READ_SYSTEM_GRAMMATICAL_GENDER: the pin comments its setPermission out, so it has none
        let chain = r#"
            new AppOpInfo.Builder(OP_READ_SYSTEM_GRAMMATICAL_GENDER, OPSTR_READ_SYSTEM_GRAMMATICAL_GENDER,
                    "READ_SYSTEM_GRAMMATICAL_GENDER")
                // will make it an app-op permission in the future.
                // .setPermission(Manifest.permission.READ_SYSTEM_GRAMMATICAL_GENDER)
                /* .setDefaultMode(AppOpsManager.MODE_ERRORED) */
                .build(),"#;
        let t = parse(&java(1, chain)).expect("fixture parses");
        assert_eq!(rows(&t), vec![(3, 0, None)]);
    }

    #[test]
    fn system_alert_window_default_is_named_and_other_calls_fail_naming_the_op() {
        let sawd = r#"
            new AppOpInfo.Builder(OP_SYSTEM_ALERT_WINDOW, OPSTR_SYSTEM_ALERT_WINDOW, "SYSTEM_ALERT_WINDOW")
                .setPermission(android.Manifest.permission.SYSTEM_ALERT_WINDOW)
                .setDefaultMode(getSystemAlertWindowDefault()).build(),"#;
        let t = parse(&java(1, sawd)).expect("fixture parses");
        assert_eq!(
            rows(&t),
            vec![(3, 0, Some("android.permission.SYSTEM_ALERT_WINDOW"))]
        );
        let unknown = r#"
            new AppOpInfo.Builder(OP_CAMERA, OPSTR_CAMERA, "CAMERA")
                .setDefaultMode(computeMode()).build(),"#;
        let err = parse(&java(1, unknown)).expect_err("unknown default-mode call");
        assert!(format!("{err:#}").contains("OP_CAMERA"), "{err:#}");
    }

    #[test]
    fn unflagged_permission_outside_manifest_fails_naming_the_op() {
        let chain = r#"
            new AppOpInfo.Builder(OP_READ_HEART_RATE, OPSTR_READ_HEART_RATE, "READ_HEART_RATE")
                .setPermission(HealthPermissions.READ_HEART_RATE).build(),"#;
        let err = parse(&java(1, chain)).expect_err("HealthPermissions without a flag");
        assert!(format!("{err:#}").contains("OP_READ_HEART_RATE"), "{err:#}");
    }

    #[test]
    fn num_op_must_equal_the_builder_count() {
        assert!(parse(&java(5, FIRST_FOUR)).is_err());
    }

    #[test]
    fn comments_go_and_literals_stay() {
        let stripped = strip_comments("a = \"//x\"; // c\nb /* d */ e = '/';");
        assert_eq!(
            stripped.split_whitespace().collect::<Vec<_>>(),
            ["a", "=", "\"//x\";", "b", "e", "=", "'/';"]
        );
    }
}
