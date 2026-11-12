// SPDX-License-Identifier: MIT
//! `xtask test installer`, slice M3: the installer's full parse (ParsingPackageUtils + PackageInfoCommonUtils.generate) against oracles read here, independently of it.
//! - `aapt2 dump badging`: package, versionCode, versionName, minSdkVersion, targetSdkVersion, launchable-activity (the first enabled `<activity>`; badging ignores aliases, so an alias launcher is checked by name only), `uses-permission` / `uses-permission-sdk-23` (with AOSP's request filter: a permission whose
//!   `maxSdkVersion` is below the device's resource SDK, or the sdk-23 form below 23, is dropped, ParsingPackageUtils.parseUsesPermission:1459-1462).
//! - `aapt2 dump xmltree` of the manifest, scanned here: the class names of `<application>`, `<activity>` + `<activity-alias>` (AOSP lists aliases among activities), `<receiver>`, `<service>`, `<provider>`, and `<permission>` names.
//! - `manifest.rs` (the aapt2 reader the services use today): every field it shares with the installer. A difference there is a finding, printed, not a failure: AOSP is the reference unless shown otherwise.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result};
use atl_services_types::packages::Manifest;

/// ParsingPackageUtils: uses-permission-sdk-23's minimum SDK (parseUsesPermission, `minSdkVersion = Build.VERSION_CODES.M` for that tag).
const SDK_23: i32 = 23;
const ACTION_MAIN: &str = "android.intent.action.MAIN";
const CATEGORY_LAUNCHER: &str = "android.intent.category.LAUNCHER";
const ANDROID_NAME: &str = ":name(0x01010003)=";

/// One component line of the installer: kind, name, and its `key=value` fields.
#[derive(Debug, Default, Clone)]
pub struct Component {
    pub kind: String,
    pub name: String,
    pub fields: BTreeMap<String, String>,
}

/// One `filter` line: the component it belongs to and its lists.
#[derive(Debug, Default, Clone)]
pub struct Filter {
    pub kind: String,
    pub component: String,
    pub actions: Vec<String>,
    pub categories: Vec<String>,
}

/// The installer's full-parse lines (atl-app-launcher installer.rs documents them).
#[derive(Debug, Default)]
pub struct Full {
    pub values: BTreeMap<String, String>,
    pub uses_permissions: Vec<String>,
    /// (name, protection level).
    pub permissions: Vec<(String, u32)>,
    pub components: Vec<Component>,
    pub filters: Vec<Filter>,
}

fn kv(rest: &str) -> BTreeMap<String, String> {
    rest.split(' ')
        .filter_map(|f| f.split_once('='))
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect()
}

fn list(v: Option<&String>) -> Vec<String> {
    v.map_or_else(Vec::new, |v| {
        v.split(',')
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect()
    })
}

fn hex(v: &str) -> Result<u32> {
    u32::from_str_radix(v.trim_start_matches("0x"), 16).with_context(|| format!("hex {v:?}"))
}

impl Full {
    pub fn read(stdout: &str) -> Result<Self> {
        let mut f = Full::default();
        for line in stdout.lines() {
            let (key, rest) = line.split_once(' ').unwrap_or((line, ""));
            match key {
                "uses-permission" => f.uses_permissions.push(rest.to_owned()),
                "permission" => {
                    let (name, fields) = rest.split_once(' ').unwrap_or((rest, ""));
                    let level = kv(fields)
                        .get("protection-level")
                        .map(|v| hex(v))
                        .transpose()?
                        .context("permission without protection-level")?;
                    f.permissions.push((name.to_owned(), level));
                }
                "activity" | "receiver" | "service" | "provider" => {
                    let (name, fields) = rest.split_once(' ').unwrap_or((rest, ""));
                    f.components.push(Component {
                        kind: key.to_owned(),
                        name: name.to_owned(),
                        fields: kv(fields),
                    });
                }
                "filter" => {
                    let mut parts = rest.splitn(3, ' ');
                    let kind = parts.next().unwrap_or_default().to_owned();
                    let component = parts.next().unwrap_or_default().to_owned();
                    let fields = kv(parts.next().unwrap_or_default());
                    f.filters.push(Filter {
                        kind,
                        component,
                        actions: list(fields.get("actions")),
                        categories: list(fields.get("categories")),
                    });
                }
                _ => {
                    f.values.insert(key.to_owned(), rest.to_owned());
                }
            }
        }
        Ok(f)
    }

    pub fn value(&self, key: &str) -> &str {
        self.values.get(key).map_or("", String::as_str)
    }

    fn names(&self, kind: &str) -> BTreeSet<String> {
        self.components
            .iter()
            .filter(|c| c.kind == kind)
            .map(|c| c.name.clone())
            .collect()
    }

    /// Enabled activities with a MAIN + LAUNCHER filter, in activity order: what a launcher's query resolves (a disabled component matches no intent query).
    pub fn launchers(&self) -> Vec<String> {
        self.components
            .iter()
            .filter(|c| {
                c.kind == "activity" && c.fields.get("enabled").is_some_and(|e| e == "true")
            })
            .filter(|c| {
                self.filters.iter().any(|f| {
                    f.kind == "activity"
                        && f.component == c.name
                        && f.actions.iter().any(|a| a == ACTION_MAIN)
                        && f.categories.iter().any(|a| a == CATEGORY_LAUNCHER)
                })
            })
            .map(|c| c.name.clone())
            .collect()
    }
}

/// What badging and the xmltree scan say the parse must give.
#[derive(Debug, Default)]
pub struct Oracle {
    pub package: String,
    pub version_code: String,
    pub version_name: String,
    pub min_sdk: String,
    pub target_sdk: String,
    pub launcher: Option<String>,
    pub uses_permissions: BTreeSet<String>,
    pub app_class: Option<String>,
    /// Component class names by installer kind (activity-alias counted as activity).
    pub components: BTreeMap<String, BTreeSet<String>>,
    /// The `<activity-alias>` names: badging's launchable-activity considers `<activity>` elements only.
    pub aliases: BTreeSet<String>,
    pub permissions: BTreeSet<String>,
}

/// `key='value'` of a badging line.
fn quoted(line: &str, key: &str) -> Option<String> {
    let start = line.find(&format!("{key}='"))? + key.len() + 2;
    let len = line[start..].find('\'')?;
    Some(line[start..start + len].to_owned())
}

/// AOSP's class-name rule (ParsingUtils.buildClassName): `.X` and `X` without a dot are relative to the package.
fn class_name(package: &str, name: &str) -> String {
    if name.starts_with('.') {
        format!("{package}{name}")
    } else if !name.contains('.') {
        format!("{package}.{name}")
    } else {
        name.to_owned()
    }
}

/// The xmltree's `android:name` of each element as (tag, parent tag, raw name), in order: an element's attributes are the `A:` lines indented one step below its `E:` line.
fn xml_names(tree: &str) -> Vec<(String, String, String)> {
    let mut stack: Vec<(usize, String)> = Vec::new();
    let mut out = Vec::new();
    for line in tree.lines() {
        let indent = line.len() - line.trim_start().len();
        let t = line.trim_start();
        if let Some(rest) = t.strip_prefix("E: ") {
            while stack.last().is_some_and(|(i, _)| *i >= indent) {
                stack.pop();
            }
            stack.push((
                indent,
                rest.split(' ').next().unwrap_or_default().to_owned(),
            ));
        } else if let Some(rest) = t.strip_prefix("A: ") {
            let Some(at) = rest.find(ANDROID_NAME) else {
                continue;
            };
            let Some((elem_indent, tag)) = stack.last() else {
                continue;
            };
            if indent <= *elem_indent {
                continue;
            }
            let value = &rest[at + ANDROID_NAME.len()..];
            let value = value
                .split(" (Raw")
                .next()
                .unwrap_or(value)
                .trim()
                .trim_matches('"');
            let parent = stack
                .len()
                .checked_sub(2)
                .map_or(String::new(), |i| stack[i].1.clone());
            out.push((tag.clone(), parent, value.to_owned()));
        }
    }
    out
}

/// The oracle of one base APK; `sdk` = the device's resource SDK (Build.VERSION.RESOURCES_SDK_INT).
pub fn oracle(aapt2: &Path, apk: &Path, sdk: i32) -> Result<Oracle> {
    let badging = crate::host_art::output(
        Command::new(aapt2).arg("dump").arg("badging").arg(apk),
        "aapt2 dump badging",
    )?;
    let mut o = Oracle::default();
    for line in badging.lines() {
        if line.starts_with("package:") {
            o.package = quoted(line, " name").unwrap_or_default();
            o.version_code = quoted(line, "versionCode").unwrap_or_default();
            o.version_name = quoted(line, "versionName").unwrap_or_else(|| "none".to_owned());
        } else if let Some(v) = line.strip_prefix("minSdkVersion:") {
            o.min_sdk = v.trim_matches('\'').to_owned();
        } else if let Some(v) = line.strip_prefix("targetSdkVersion:") {
            o.target_sdk = v.trim_matches('\'').to_owned();
        } else if line.starts_with("launchable-activity:") && o.launcher.is_none() {
            o.launcher = quoted(line, " name");
        } else if line.starts_with("uses-permission:")
            || line.starts_with("uses-permission-sdk-23:")
        {
            let name = quoted(line, "name").context("uses-permission without a name")?;
            let max = quoted(line, "maxSdkVersion")
                .map(|m| m.parse::<i32>())
                .transpose()?;
            let min = if line.starts_with("uses-permission-sdk-23:") {
                SDK_23
            } else {
                0
            };
            if sdk >= min && max.is_none_or(|m| sdk <= m) {
                o.uses_permissions.insert(name);
            }
        }
    }
    if o.target_sdk.is_empty() {
        // aapt2 prints no targetSdkVersion line when the manifest has none; AOSP's default is the minSdkVersion (ParsingPackageUtils.parseUsesSdk)
        o.target_sdk = o.min_sdk.clone();
    }
    let tree = crate::host_art::output(
        Command::new(aapt2)
            .arg("dump")
            .arg("xmltree")
            .arg("--file")
            .arg("AndroidManifest.xml")
            .arg(apk),
        "aapt2 dump xmltree",
    )?;
    for (tag, parent, name) in xml_names(&tree) {
        let class = class_name(&o.package, &name);
        match (tag.as_str(), parent.as_str()) {
            ("application", "manifest") => o.app_class = Some(class),
            ("activity", "application") => {
                o.components
                    .entry("activity".into())
                    .or_default()
                    .insert(class);
            }
            ("activity-alias", "application") => {
                o.aliases.insert(class.clone());
                o.components
                    .entry("activity".into())
                    .or_default()
                    .insert(class);
            }
            (k @ ("receiver" | "service" | "provider"), "application") => {
                o.components.entry(k.into()).or_default().insert(class);
            }
            ("permission", "manifest") => {
                o.permissions.insert(name);
            }
            _ => {}
        }
    }
    Ok(o)
}

/// Every difference between the installer's parse and the oracle (empty: equal).
pub fn compare(full: &Full, o: &Oracle) -> Vec<String> {
    let mut bad = Vec::new();
    for (key, want) in [
        ("package", o.package.as_str()),
        ("version-code", o.version_code.as_str()),
        ("version-name", o.version_name.as_str()),
        ("min-sdk", o.min_sdk.as_str()),
        ("target-sdk", o.target_sdk.as_str()),
    ] {
        if full.value(key) != want {
            bad.push(format!("{key} {} != aapt2 {want}", full.value(key)));
        }
    }
    let app_class = o.app_class.as_deref().unwrap_or("none");
    if full.value("app-class") != app_class {
        bad.push(format!(
            "app-class {} != xmltree {app_class}",
            full.value("app-class")
        ));
    }
    let launchers = full.launchers();
    let activities: Vec<&String> = launchers
        .iter()
        .filter(|l| !o.aliases.contains(*l))
        .collect();
    match &o.launcher {
        Some(l) if activities.first() != Some(&l) => {
            bad.push(format!(
                "launcher activities {launchers:?}: first <activity> != badging {l}"
            ));
        }
        None if !activities.is_empty() => {
            bad.push(format!(
                "launcher activities {activities:?} != badging none"
            ));
        }
        _ => {}
    }
    let uses: BTreeSet<String> = full.uses_permissions.iter().cloned().collect();
    if uses != o.uses_permissions {
        bad.push(format!(
            "uses-permission: installer only {:?}, badging only {:?}",
            uses.difference(&o.uses_permissions).collect::<Vec<_>>(),
            o.uses_permissions.difference(&uses).collect::<Vec<_>>()
        ));
    }
    for kind in ["activity", "receiver", "service", "provider"] {
        let got = full.names(kind);
        let want = o.components.get(kind).cloned().unwrap_or_default();
        if got != want {
            bad.push(format!(
                "{kind}: installer only {:?}, xmltree only {:?}",
                got.difference(&want).collect::<Vec<_>>(),
                want.difference(&got).collect::<Vec<_>>()
            ));
        }
    }
    let perms: BTreeSet<String> = full.permissions.iter().map(|(n, _)| n.clone()).collect();
    if perms != o.permissions {
        bad.push(format!(
            "permission {perms:?} != xmltree {:?}",
            o.permissions
        ));
    }
    bad
}

/// A field both carry: (field, installer value, manifest.rs value).
type Finding = (String, String, String);

fn opt(v: Option<&str>) -> String {
    v.unwrap_or("none").to_owned()
}

/// The installer's parse against manifest.rs over the fields both carry; each difference is a finding.
pub fn manifest_findings(full: &Full, m: &Manifest) -> Vec<Finding> {
    let mut f: Vec<Finding> = Vec::new();
    let mut eq = |field: String, ours: String, theirs: String| {
        if ours != theirs {
            f.push((field, ours, theirs));
        }
    };
    let app_process = m.process_name.clone().unwrap_or_else(|| m.name.clone());
    eq(
        "version-name".into(),
        full.value("version-name").into(),
        opt(m.version_name.as_deref()),
    );
    eq(
        "min-sdk".into(),
        full.value("min-sdk").into(),
        m.min_sdk.to_string(),
    );
    eq(
        "target-sdk".into(),
        full.value("target-sdk").into(),
        m.target_sdk.to_string(),
    );
    eq(
        "app-class".into(),
        full.value("app-class").into(),
        opt(m.application_class.as_deref()),
    );
    eq(
        "app-process".into(),
        full.value("app-process").into(),
        app_process.clone(),
    );
    eq(
        "app-theme".into(),
        full.value("app-theme").into(),
        format!("{:#x}", m.application_theme),
    );
    let comp = |kind: &str, name: &str| {
        full.components
            .iter()
            .find(|c| c.kind == kind && c.name == name)
            .cloned()
    };
    let field = |c: &Option<Component>, k: &str| {
        c.as_ref()
            .and_then(|c| c.fields.get(k).cloned())
            .unwrap_or_else(|| "absent".to_owned())
    };
    let process = |p: &Option<String>| p.clone().unwrap_or_else(|| app_process.clone());
    for a in &m.activities {
        let c = comp("activity", &a.name);
        let n = &a.name;
        eq(
            format!("{n} launch-mode"),
            field(&c, "launch-mode"),
            a.launch_mode.to_string(),
        );
        eq(
            format!("{n} theme"),
            field(&c, "theme"),
            format!("{:#x}", a.theme),
        );
        eq(
            format!("{n} config-changes"),
            field(&c, "config-changes"),
            format!("{:#x}", a.config_changes),
        );
        eq(
            format!("{n} orientation"),
            field(&c, "orientation"),
            a.screen_orientation.to_string(),
        );
        eq(
            format!("{n} process"),
            field(&c, "process"),
            process(&a.process),
        );
    }
    let launchers: Vec<String> = m
        .activities
        .iter()
        .filter(|a| a.launcher)
        .map(|a| a.name.clone())
        .collect();
    eq(
        "launchers".into(),
        full.launchers().join(","),
        launchers.join(","),
    );
    for r in &m.receivers {
        let c = comp("receiver", &r.name);
        let n = &r.name;
        eq(
            format!("{n} exported"),
            field(&c, "exported"),
            r.exported.to_string(),
        );
        eq(
            format!("{n} enabled"),
            field(&c, "enabled"),
            r.enabled.to_string(),
        );
        eq(
            format!("{n} permission"),
            field(&c, "permission"),
            opt(r.permission.as_deref()),
        );
        eq(
            format!("{n} process"),
            field(&c, "process"),
            process(&r.process),
        );
    }
    for p in &m.providers {
        let c = comp("provider", &p.name);
        let n = &p.name;
        eq(
            format!("{n} authority"),
            field(&c, "authority"),
            opt(p.authority.as_deref()),
        );
        eq(
            format!("{n} exported"),
            field(&c, "exported"),
            p.exported.to_string(),
        );
        eq(
            format!("{n} enabled"),
            field(&c, "enabled"),
            p.enabled.to_string(),
        );
        eq(
            format!("{n} process"),
            field(&c, "process"),
            process(&p.process),
        );
        eq(
            format!("{n} read-permission"),
            field(&c, "read-permission"),
            opt(p.read_permission.as_deref()),
        );
        eq(
            format!("{n} write-permission"),
            field(&c, "write-permission"),
            opt(p.write_permission.as_deref()),
        );
    }
    let mut ours = full.uses_permissions.clone();
    ours.sort();
    let mut theirs = m.requested_permissions.clone();
    theirs.sort();
    theirs.dedup();
    eq("uses-permission".into(), ours.join(","), theirs.join(","));
    let mut ours: Vec<String> = full
        .permissions
        .iter()
        .map(|(n, l)| format!("{n}:{l:#x}"))
        .collect();
    ours.sort();
    let mut theirs: Vec<String> = m
        .permissions
        .iter()
        .map(|p| format!("{}:{:#x}", p.name, p.protection_level))
        .collect();
    theirs.sort();
    eq("permission".into(), ours.join(","), theirs.join(","));
    f
}

#[cfg(test)]
mod tests {
    use super::{Full, class_name, xml_names};

    #[test]
    fn reads_full_lines_and_launchers() {
        let f = Full::read(
            "package a.b\nversion-code 3\nuses-permission p.Q\npermission a.b.P protection-level=0x2\nactivity a.b.M exported=true theme=0x0\nactivity a.b.N exported=false enabled=true\nactivity a.b.O enabled=false\nfilter activity a.b.O actions=android.intent.action.MAIN categories=android.intent.category.LAUNCHER schemes= types=\nfilter activity a.b.N actions=android.intent.action.MAIN categories=android.intent.category.LAUNCHER schemes= types=\n",
        )
        .unwrap();
        assert_eq!(f.value("package"), "a.b");
        assert_eq!(f.permissions, [("a.b.P".to_owned(), 2)]);
        assert_eq!(f.launchers(), ["a.b.N"]);
        assert_eq!(f.components[0].fields["exported"], "true");
    }

    #[test]
    fn class_names_follow_build_class_name() {
        assert_eq!(class_name("a.b", ".M"), "a.b.M");
        assert_eq!(class_name("a.b", "M"), "a.b.M");
        assert_eq!(class_name("a.b", "x.y.M"), "x.y.M");
    }

    #[test]
    fn xml_names_take_the_own_attribute_and_parent() {
        let tree = "N: android=http://schemas.android.com/apk/res/android\n  E: manifest (line=2)\n    A: http://schemas.android.com/apk/res/android:versionCode(0x0101021b)=1\n    E: permission (line=3)\n      A: http://schemas.android.com/apk/res/android:name(0x01010003)=\"a.P\" (Raw: \"a.P\")\n    E: application (line=4)\n      A: http://schemas.android.com/apk/res/android:name(0x01010003)=\"a.App\" (Raw: \"a.App\")\n      E: activity (line=5)\n        A: http://schemas.android.com/apk/res/android:name(0x01010003)=\".M\" (Raw: \".M\")\n        E: intent-filter (line=6)\n          E: action (line=7)\n            A: http://schemas.android.com/apk/res/android:name(0x01010003)=\"android.intent.action.MAIN\" (Raw: \"android.intent.action.MAIN\")\n";
        assert_eq!(
            xml_names(tree),
            [
                ("permission".into(), "manifest".into(), "a.P".into()),
                ("application".into(), "manifest".into(), "a.App".into()),
                ("activity".into(), "application".into(), ".M".into()),
                (
                    "action".into(),
                    "intent-filter".into(),
                    "android.intent.action.MAIN".into()
                ),
            ]
        );
    }
}
