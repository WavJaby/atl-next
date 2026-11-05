// SPDX-License-Identifier: MIT
//! A package's manifest as the services need it, read from the APK with AOSP's own `aapt2 dump xmltree --file AndroidManifest.xml` (owner D3, 2026-11-03: aapt2 is a build and test time tool; real installs
//! at run time read the manifest with libandroidfw's ResXMLTree through a boundary crate, and produce the same `Manifest`). No second binary-XML decoder is written here: this reads aapt2's text.
//! aapt2's format (tools/aapt2/Debug.cpp `Debug::DumpXml`): `N:` namespace lines, `E: <element> (line=n)`, `A: [<namespace uri>:]<name>[(0x<resource id>)]=<value>`; an element's attributes are indented two
//! columns deeper than it, its child elements four. A value is `"string" (Raw: "string")`, a decimal or `0x` integer, `true` / `false`, or `@0x<id>` / `(type 0x..)0x..` for the rest.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};
use atl_services::packages::{ActivityDecl, InstrumentationDecl, Manifest, UsesLibrary};

use crate::host_art;

/// ACTION_MAIN / CATEGORY_LAUNCHER (Intent.java): the pair that makes an activity the package's launcher activity.
const ACTION_MAIN: &str = "android.intent.action.MAIN";
const CATEGORY_LAUNCHER: &str = "android.intent.category.LAUNCHER";
/// What the manifest means when `<uses-sdk>` or its attributes are absent (ParsingPackageUtils: minSdkVersion 1, targetSdkVersion = min).
const DEFAULT_MIN_SDK: i32 = 1;

/// One parsed element: its name, attributes (namespace and resource id dropped), children.
#[derive(Debug, Default)]
struct Element {
    name: String,
    attrs: BTreeMap<String, String>,
    children: Vec<Element>,
}

impl Element {
    fn children<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Element> {
        self.children.iter().filter(move |c| c.name == name)
    }
    fn attr(&self, name: &str) -> Option<&str> {
        self.attrs.get(name).map(String::as_str)
    }
    fn bool_attr(&self, name: &str, default: bool) -> bool {
        match self.attr(name) {
            Some("true") => true,
            Some("false") => false,
            _ => default,
        }
    }
}

/// The attribute line's key and value: `A: <ns>:name(0x..)=<value>` -> (`name`, `<value>` unquoted when it is a string).
fn attribute(line: &str) -> Result<(String, String)> {
    let rest = line
        .trim_start()
        .strip_prefix("A: ")
        .context("not an A: line")?;
    let (key, value) = rest.split_once('=').context("attribute without `=`")?;
    // `http://schemas.android.com/apk/res/android:name(0x01010003)` -> `name`; `package` stays
    let key = key.split('(').next().unwrap_or(key);
    let key = key.rsplit(':').next().unwrap_or(key);
    let value = match value.strip_prefix('"') {
        // `"text" (Raw: "text")`: the quoted text up to the closing quote that precedes ` (Raw:`, else the last quote
        Some(quoted) => match quoted.find("\" (Raw: ") {
            Some(end) => &quoted[..end],
            None => quoted.strip_suffix('"').unwrap_or(quoted),
        },
        None => value.split(" (Raw: ").next().unwrap_or(value),
    };
    Ok((key.to_owned(), value.to_owned()))
}

/// The element tree of `aapt2 dump xmltree` text; the root is the manifest element.
fn parse_tree(text: &str) -> Result<Element> {
    // the open elements, with their indentation; a line closes every element at its indentation or deeper
    let mut stack: Vec<(usize, Element)> = Vec::new();
    let mut root = None;
    let pop_to = |stack: &mut Vec<(usize, Element)>, root: &mut Option<Element>, indent: usize| {
        while stack.last().is_some_and(|(i, _)| *i >= indent) {
            let (_, done) = stack.pop().expect("checked non-empty");
            match stack.last_mut() {
                Some((_, parent)) => parent.children.push(done),
                None => *root = Some(done),
            }
        }
    };
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let indent = line.len() - line.trim_start().len();
        let body = line.trim_start();
        if body.starts_with("N: ") {
            continue;
        }
        if let Some(rest) = body.strip_prefix("E: ") {
            pop_to(&mut stack, &mut root, indent);
            let name = rest.split(" (line=").next().unwrap_or(rest).to_owned();
            stack.push((
                indent,
                Element {
                    name,
                    ..Element::default()
                },
            ));
        } else if body.starts_with("A: ") {
            let (key, value) =
                attribute(line).with_context(|| format!("attribute line {line:?}"))?;
            let Some((_, element)) = stack.last_mut() else {
                bail!("attribute outside an element: {line:?}");
            };
            element.attrs.insert(key, value);
        } else {
            bail!("unrecognised line in aapt2's xmltree: {line:?}");
        }
    }
    pop_to(&mut stack, &mut root, 0);
    root.context("no element in aapt2's xmltree")
}

/// The boolean attributes of `element` by name: those whose text is `true` or `false` (aapt2 prints a boolean resource that way).
fn booleans(element: &Element) -> BTreeMap<String, bool> {
    element
        .attrs
        .iter()
        .filter_map(|(name, value)| match value.as_str() {
            "true" => Some((name.clone(), true)),
            "false" => Some((name.clone(), false)),
            _ => None,
        })
        .collect()
}

/// `android:theme` of `element` as a resource id: aapt2 prints a reference `@0x01030128`. None written = 0 (ParsedActivityUtils: `getResourceId(theme, 0)`); a reference to the app's own or the framework's style reads alike.
fn theme(element: &Element) -> Result<i32> {
    let Some(value) = element.attr("theme") else {
        return Ok(0);
    };
    let hex = value
        .strip_prefix("@0x")
        .with_context(|| format!("theme {value:?} is not a resource reference"))?;
    let id = u32::from_str_radix(hex, 16).with_context(|| format!("theme {value:?}"))?;
    Ok(id as i32)
}

/// ParsedActivityUtils.RECREATE_ON_CONFIG_CHANGES_MASK: ActivityInfo.CONFIG_MCC | CONFIG_MNC.
const RECREATE_ON_CONFIG_CHANGES_MASK: u32 = 0x1 | 0x2;

/// A flags attribute as aapt2 prints it (`0x00000d80`); unwritten = 0 (`sa.getInt(.., 0)`).
fn flags_attr(element: &Element, name: &str) -> Result<u32> {
    let Some(value) = element.attr(name) else {
        return Ok(0);
    };
    let hex = value
        .strip_prefix("0x")
        .with_context(|| format!("{name} {value:?} is not a flags value"))?;
    u32::from_str_radix(hex, 16).with_context(|| format!("{name} {value:?}"))
}

/// ParsedActivityUtils.getActivityConfigChanges: `configChanges`, plus MCC and MNC unless `recreateOnConfigChanges` names them (an activity handles those two by default).
fn config_changes(activity: &Element) -> Result<i32> {
    let written = flags_attr(activity, "configChanges")?;
    let recreate = flags_attr(activity, "recreateOnConfigChanges")?;
    Ok((written | (!recreate & RECREATE_ON_CONFIG_CHANGES_MASK)) as i32)
}

/// `android:launchMode` as ActivityInfo.launchMode. aapt2's xmltree prints the enum's value as a decimal (`launchMode(0x0101001d)=2` for singleTask; checked on prebuilts/sdk/tools aapt2 with a
/// manifest declaring singleTask / singleTop / none); `(type 0x10)0x<n>` is not what this aapt2 prints. Unwritten = 0 (LAUNCH_MULTIPLE, `standard`).
fn launch_mode(element: &Element) -> Result<i32> {
    let Some(value) = element.attr("launchMode") else {
        return Ok(0);
    };
    value
        .parse::<i32>()
        .with_context(|| format!("launchMode {value:?} is not an enum value"))
}

/// `android:taskAffinity` as written; None = unwritten, the package name is the affinity (ActivityRecord's default).
fn task_affinity(element: &Element) -> Option<String> {
    element.attr("taskAffinity").map(str::to_owned)
}

/// `android:process` of an `<application>` or `<activity>` (ParsingUtils.buildProcessName: `:name` is private to the package); None = unwritten, the package's process.
fn process_attr(element: &Element, package: &str) -> Option<String> {
    element.attr("process").map(|p| match p.strip_prefix(':') {
        Some(suffix) => format!("{package}:{suffix}"),
        None => p.to_owned(),
    })
}

/// A class name as ParsedComponent resolves it against the package: `.Foo` -> `<package>.Foo`, a bare `Foo` likewise, a dotted name as it is.
fn resolve_class(package: &str, name: &str) -> String {
    if name.starts_with('.') {
        format!("{package}{name}")
    } else if !name.contains('.') {
        format!("{package}.{name}")
    } else {
        name.to_owned()
    }
}

/// The manifest of the text `aapt2 dump xmltree --file AndroidManifest.xml` printed.
/// Whether an `<activity>` or `<activity-alias>` has an intent filter with ACTION_MAIN and CATEGORY_LAUNCHER.
fn is_launcher(element: &Element) -> bool {
    element.children("intent-filter").any(|f| {
        let has =
            |kind: &str, wanted: &str| f.children(kind).any(|e| e.attr("name") == Some(wanted));
        has("action", ACTION_MAIN) && has("category", CATEGORY_LAUNCHER)
    })
}

pub fn parse(text: &str) -> Result<Manifest> {
    let root = parse_tree(text)?;
    if root.name != "manifest" {
        bail!("the root element is `{}`, not `manifest`", root.name);
    }
    let package = root.attr("package").context("manifest without a package")?;
    let int = |element: &Element, name: &str| -> Result<Option<i64>> {
        element
            .attr(name)
            .map(|v| {
                v.strip_prefix("0x")
                    .map_or_else(|| v.parse(), |h| i64::from_str_radix(h, 16))
                    .with_context(|| format!("{name}={v}: not an integer"))
            })
            .transpose()
    };
    let sdk = root.children("uses-sdk").next();
    let min_sdk = match sdk {
        Some(s) => int(s, "minSdkVersion")?.map_or(DEFAULT_MIN_SDK, |v| v as i32),
        None => DEFAULT_MIN_SDK,
    };
    let target_sdk = match sdk {
        Some(s) => int(s, "targetSdkVersion")?.map_or(min_sdk, |v| v as i32),
        None => min_sdk,
    };
    let application = root.children("application").next();
    let process_name = application.and_then(|a| process_attr(a, package));
    // every `<activity>` in manifest order: its class, whether it handles MAIN + LAUNCHER, its boolean attributes (ParsedActivity's inputs)
    let mut activities: Vec<ActivityDecl> = application
        .into_iter()
        .flat_map(|a| a.children("activity"))
        .map(|activity| {
            let Some(name) = activity.attr("name") else {
                return Ok(None);
            };
            Ok(Some(ActivityDecl {
                name: resolve_class(package, name),
                launcher: is_launcher(activity),
                theme: theme(activity)?,
                config_changes: config_changes(activity)?,
                booleans: booleans(activity),
                launch_mode: launch_mode(activity)?,
                task_affinity: task_affinity(activity),
                process: process_attr(activity, package),
            }))
        })
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .flatten()
        .collect();
    // an enabled `<activity-alias>` that is MAIN + LAUNCHER launches its target: PackageManager resolves the alias, and the record's real activity is the target (ActivityRecord.realActivity)
    for alias in application
        .into_iter()
        .flat_map(|a| a.children("activity-alias"))
    {
        let target = alias
            .attr("targetActivity")
            .context("<activity-alias> without android:targetActivity")?;
        if !alias.bool_attr("enabled", true) || !is_launcher(alias) {
            continue;
        }
        let target = resolve_class(package, target);
        match activities.iter_mut().find(|a| a.name == target) {
            Some(decl) => decl.launcher = true,
            None => activities.push(ActivityDecl {
                name: target,
                launcher: true,
                theme: theme(alias)?,
                config_changes: config_changes(alias)?,
                booleans: booleans(alias),
                launch_mode: launch_mode(alias)?,
                task_affinity: task_affinity(alias),
                process: process_attr(alias, package),
            }),
        }
    }
    Ok(Manifest {
        name: package.to_owned(),
        version_code: int(&root, "versionCode")?.unwrap_or(0),
        version_name: root.attr("versionName").map(str::to_owned),
        min_sdk,
        target_sdk,
        process_name,
        application_theme: application.map_or(Ok(0), theme)?,
        application_booleans: application.map(booleans).unwrap_or_default(),
        activities,
        requested_permissions: root
            .children("uses-permission")
            .filter_map(|e| e.attr("name").map(str::to_owned))
            .collect(),
        libraries: application
            .into_iter()
            .flat_map(|a| a.children("uses-library"))
            .filter_map(|e| {
                Some(UsesLibrary {
                    name: e.attr("name")?.to_owned(),
                    required: e.bool_attr("required", true),
                })
            })
            .collect(),
        instrumentation: root
            .children("instrumentation")
            .filter_map(|e| {
                Some(InstrumentationDecl {
                    name: resolve_class(package, e.attr("name")?),
                    target_package: e.attr("targetPackage")?.to_owned(),
                    target_processes: e.attr("targetProcesses").map(str::to_owned),
                    handle_profiling: e.bool_attr("handleProfiling", false),
                    functional_test: e.bool_attr("functionalTest", false),
                })
            })
            .collect(),
    })
}

/// `apk`'s manifest, through `aapt2`.
pub fn read(aapt2: &Path, apk: &Path) -> Result<Manifest> {
    let text = host_art::output(
        Command::new(aapt2)
            .args(["dump", "xmltree", "--file", "AndroidManifest.xml"])
            .arg(apk),
        "aapt2 dump xmltree",
    )?;
    parse(&text).with_context(|| format!("the manifest of {}", apk.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// aapt2 2.20's `dump xmltree` of xtask/testdata/instrument/AndroidManifest.xml, as measured 2026-11-03.
    const INSTRUMENT: &str = r#"N: android=http://schemas.android.com/apk/res/android (line=4)
  E: manifest (line=4)
    A: http://schemas.android.com/apk/res/android:versionCode(0x0101021b)=3
    A: http://schemas.android.com/apk/res/android:versionName(0x0101021c)="1.0" (Raw: "1.0")
    A: http://schemas.android.com/apk/res/android:compileSdkVersion(0x01010572)=36
    A: http://schemas.android.com/apk/res/android:compileSdkVersionCodename(0x01010573)="16" (Raw: "16")
    A: package="com.example.atlinstr" (Raw: "com.example.atlinstr")
    A: platformBuildVersionCode=36
    A: platformBuildVersionName=16
      E: uses-sdk (line=9)
        A: http://schemas.android.com/apk/res/android:minSdkVersion(0x0101020c)=21
        A: http://schemas.android.com/apk/res/android:targetSdkVersion(0x01010270)=36
      E: uses-permission (line=10)
        A: http://schemas.android.com/apk/res/android:name(0x01010003)="android.permission.INTERNET" (Raw: "android.permission.INTERNET")
      E: application (line=12)
        A: http://schemas.android.com/apk/res/android:label(0x01010001)="ATL instrumentation fixture" (Raw: "ATL instrumentation fixture")
          E: uses-library (line=13)
            A: http://schemas.android.com/apk/res/android:name(0x01010003)="android.test.runner" (Raw: "android.test.runner")
          E: uses-library (line=14)
            A: http://schemas.android.com/apk/res/android:name(0x01010003)="android.test.base" (Raw: "android.test.base")
          E: uses-library (line=15)
            A: http://schemas.android.com/apk/res/android:name(0x01010003)="android.test.mock" (Raw: "android.test.mock")
            A: http://schemas.android.com/apk/res/android:required(0x0101028e)=false
      E: instrumentation (line=18)
        A: http://schemas.android.com/apk/res/android:label(0x01010001)="ATL instrumentation fixture" (Raw: "ATL instrumentation fixture")
        A: http://schemas.android.com/apk/res/android:name(0x01010003)="android.test.InstrumentationTestRunner" (Raw: "android.test.InstrumentationTestRunner")
        A: http://schemas.android.com/apk/res/android:targetPackage(0x01010021)="com.example.atlinstr" (Raw: "com.example.atlinstr")
        A: http://schemas.android.com/apk/res/android:functionalTest(0x01010023)=true
"#;

    #[test]
    fn the_instrumentation_fixture() {
        // the values xtask/testdata/instrument/AndroidManifest.xml declares
        let m = parse(INSTRUMENT).expect("parse");
        assert_eq!(m.name, "com.example.atlinstr");
        assert_eq!(
            (m.version_code, m.version_name.as_deref()),
            (3, Some("1.0"))
        );
        assert_eq!((m.min_sdk, m.target_sdk), (21, 36));
        assert_eq!(m.requested_permissions, ["android.permission.INTERNET"]);
        assert_eq!(
            m.libraries,
            [
                UsesLibrary {
                    name: "android.test.runner".into(),
                    required: true
                },
                UsesLibrary {
                    name: "android.test.base".into(),
                    required: true
                },
                UsesLibrary {
                    name: "android.test.mock".into(),
                    required: false
                },
            ]
        );
        assert_eq!(
            m.instrumentation,
            [InstrumentationDecl {
                name: "android.test.InstrumentationTestRunner".into(),
                target_package: "com.example.atlinstr".into(),
                target_processes: None,
                handle_profiling: false,
                functional_test: true,
            }]
        );
        assert_eq!(m.process_name, None);
        assert!(m.activities.is_empty() && m.launch_activity().is_none());
        assert!(m.application_booleans.is_empty());
    }

    /// `android:configChanges` as aapt2 prints it. getActivityConfigChanges: unwritten -> MCC | MNC (0x3, the default handled pair); 0xd80 (screenSize, smallestScreenSize, orientation, screenLayout... as written) keeps them; recreateOnConfigChanges=mcc (0x1) drops MCC.
    #[test]
    /// `launchMode`, `taskAffinity` and `process` of an `<activity>` as ActivityDecl holds them; unwritten = standard (0), no affinity written (the package name applies), no process written.
    fn launch_mode_affinity_and_process_follow_the_manifest() {
        let text = |attrs: &str| {
            format!(
                r#"  E: manifest (line=3)
    A: package="com.example.atltest" (Raw: "com.example.atltest")
      E: application (line=7)
          E: activity (line=8)
            A: http://schemas.android.com/apk/res/android:name(0x01010003)=".A" (Raw: ".A")
{attrs}
"#
            )
        };
        let written = text(
            r#"            A: http://schemas.android.com/apk/res/android:launchMode(0x0101001d)=1
            A: http://schemas.android.com/apk/res/android:taskAffinity(0x01010204)="com.example.tasks" (Raw: "com.example.tasks")
            A: http://schemas.android.com/apk/res/android:process(0x01010584)=":remote" (Raw: ":remote")"#,
        );
        let a = &parse(&written).expect("parse").activities[0];
        assert_eq!(
            (
                a.launch_mode,
                a.task_affinity.as_deref(),
                a.process.as_deref()
            ),
            (
                1,
                Some("com.example.tasks"),
                Some("com.example.atltest:remote")
            )
        );
        let plain = &parse(&text("")).expect("parse").activities[0];
        assert_eq!(
            (
                plain.launch_mode,
                plain.task_affinity.clone(),
                plain.process.clone()
            ),
            (0, None, None)
        );
    }

    #[test]
    fn config_changes_follow_get_activity_config_changes() {
        let text = |attrs: &str| {
            format!(
                r#"  E: manifest (line=3)
    A: package="com.example.atltest" (Raw: "com.example.atltest")
      E: application (line=7)
          E: activity (line=8)
            A: http://schemas.android.com/apk/res/android:name(0x01010003)=".A" (Raw: ".A")
{attrs}
"#
            )
        };
        let changes = "            A: http://schemas.android.com/apk/res/android:configChanges(0x0101001f)=0x00000d80";
        let recreate = "            A: http://schemas.android.com/apk/res/android:recreateOnConfigChanges(0x01010547)=0x00000001";
        let value = |attrs: &str| parse(&text(attrs)).expect("parse").activities[0].config_changes;
        assert_eq!(value(""), 0x3);
        assert_eq!(value(changes), 0xd83);
        assert_eq!(value(&format!("{changes}\n{recreate}")), 0xd82);
        assert!(parse(&text("            A: http://schemas.android.com/apk/res/android:configChanges(0x0101001f)=\"x\" (Raw: \"x\")")).is_err());
    }

    /// `android:theme` as aapt2 prints it, on the application and on one activity; unwritten = 0; a theme that is no reference is refused.
    #[test]
    fn themes_are_resource_ids() {
        let text = |value: &str| {
            format!(
                r#"  E: manifest (line=3)
    A: package="com.example.atltest" (Raw: "com.example.atltest")
      E: application (line=7)
        A: http://schemas.android.com/apk/res/android:theme(0x01010000)=@0x7f0a0001
          E: activity (line=8)
            A: http://schemas.android.com/apk/res/android:name(0x01010003)=".A" (Raw: ".A")
            A: http://schemas.android.com/apk/res/android:theme(0x01010000)={value}
          E: activity (line=9)
            A: http://schemas.android.com/apk/res/android:name(0x01010003)=".B" (Raw: ".B")
"#
            )
        };
        let m = parse(&text("@0x0103012a")).expect("parse");
        assert_eq!(m.application_theme, 0x7f0a_0001);
        assert_eq!(m.activities[0].theme, 0x0103_012a);
        assert_eq!(m.activities[1].theme, 0);
        assert!(parse(&text("\"x\" (Raw: \"x\")")).is_err());
    }

    /// An enabled MAIN + LAUNCHER `<activity-alias>` makes its target the launcher (the Fossify Calculator's launcher icons: 18 disabled aliases, one enabled, all to SplashActivity); a disabled alias counts for nothing.
    #[test]
    fn enabled_launcher_alias_names_its_target() {
        let text = r#"N: android=http://schemas.android.com/apk/res/android (line=3)
  E: manifest (line=3)
    A: package="com.example.atltest" (Raw: "com.example.atltest")
      E: application (line=7)
          E: activity (line=8)
            A: http://schemas.android.com/apk/res/android:name(0x01010003)=".Splash" (Raw: ".Splash")
          E: activity (line=9)
            A: http://schemas.android.com/apk/res/android:name(0x01010003)=".Other" (Raw: ".Other")
          E: activity-alias (line=10)
            A: http://schemas.android.com/apk/res/android:name(0x01010003)=".Off" (Raw: ".Off")
            A: http://schemas.android.com/apk/res/android:enabled(0x0101000e)=false
            A: http://schemas.android.com/apk/res/android:targetActivity(0x01010202)=".Other" (Raw: ".Other")
              E: intent-filter (line=11)
                  E: action (line=12)
                    A: http://schemas.android.com/apk/res/android:name(0x01010003)="android.intent.action.MAIN" (Raw: "android.intent.action.MAIN")
                  E: category (line=13)
                    A: http://schemas.android.com/apk/res/android:name(0x01010003)="android.intent.category.LAUNCHER" (Raw: "android.intent.category.LAUNCHER")
          E: activity-alias (line=14)
            A: http://schemas.android.com/apk/res/android:name(0x01010003)=".Green" (Raw: ".Green")
            A: http://schemas.android.com/apk/res/android:enabled(0x0101000e)=true
            A: http://schemas.android.com/apk/res/android:targetActivity(0x01010202)=".Splash" (Raw: ".Splash")
              E: intent-filter (line=15)
                  E: action (line=16)
                    A: http://schemas.android.com/apk/res/android:name(0x01010003)="android.intent.action.MAIN" (Raw: "android.intent.action.MAIN")
                  E: category (line=17)
                    A: http://schemas.android.com/apk/res/android:name(0x01010003)="android.intent.category.LAUNCHER" (Raw: "android.intent.category.LAUNCHER")
"#;
        let m = parse(text).expect("parse");
        assert_eq!(
            m.launch_activity().map(|a| a.name.as_str()),
            Some("com.example.atltest.Splash")
        );
        let shape: Vec<(&str, bool)> = m
            .activities
            .iter()
            .map(|a| (a.name.as_str(), a.launcher))
            .collect();
        assert_eq!(
            shape,
            [
                ("com.example.atltest.Splash", true),
                ("com.example.atltest.Other", false)
            ]
        );
    }

    /// A launcher activity named relative to the package, a private process, a relative instrumentation name: the resolution rules ParsedComponent / ParsingUtils apply.
    #[test]
    fn launcher_activity_and_process_resolution() {
        let text = r#"N: android=http://schemas.android.com/apk/res/android (line=3)
  E: manifest (line=3)
    A: package="com.example.atltest" (Raw: "com.example.atltest")
      E: application (line=7)
        A: http://schemas.android.com/apk/res/android:process(0x01010011)=":remote" (Raw: ":remote")
          E: activity (line=8)
            A: http://schemas.android.com/apk/res/android:name(0x01010003)=".Other" (Raw: ".Other")
          E: activity (line=9)
            A: http://schemas.android.com/apk/res/android:name(0x01010003)=".Main" (Raw: ".Main")
              E: intent-filter (line=10)
                  E: action (line=11)
                    A: http://schemas.android.com/apk/res/android:name(0x01010003)="android.intent.action.MAIN" (Raw: "android.intent.action.MAIN")
                  E: category (line=12)
                    A: http://schemas.android.com/apk/res/android:name(0x01010003)="android.intent.category.LAUNCHER" (Raw: "android.intent.category.LAUNCHER")
      E: instrumentation (line=14)
        A: http://schemas.android.com/apk/res/android:name(0x01010003)=".Runner" (Raw: ".Runner")
        A: http://schemas.android.com/apk/res/android:targetPackage(0x01010021)="com.example.other" (Raw: "com.example.other")
        A: http://schemas.android.com/apk/res/android:targetProcesses(0x01010a1b)="*" (Raw: "*")
"#;
        let m = parse(text).expect("parse");
        assert_eq!(
            m.launch_activity().map(|a| a.name.as_str()),
            Some("com.example.atltest.Main")
        );
        // both activities, in order; only Main handles MAIN + LAUNCHER and it wrote no boolean
        let shape: Vec<(&str, bool)> = m
            .activities
            .iter()
            .map(|a| (a.name.as_str(), a.launcher))
            .collect();
        assert_eq!(
            shape,
            [
                ("com.example.atltest.Other", false),
                ("com.example.atltest.Main", true)
            ]
        );
        assert!(m.activities.iter().all(|a| a.booleans.is_empty()));
        assert_eq!(
            m.process_name.as_deref(),
            Some("com.example.atltest:remote")
        );
        assert_eq!(m.instrumentation[0].name, "com.example.atltest.Runner");
        assert_eq!(m.instrumentation[0].target_processes.as_deref(), Some("*"));
        // no <uses-sdk>: ParsingPackageUtils' defaults
        assert_eq!((m.min_sdk, m.target_sdk, m.version_code), (1, 1, 0));
    }

    #[test]
    fn refuses_what_is_not_a_manifest() {
        assert!(parse("").is_err());
        assert!(parse("  E: application (line=1)\n").is_err());
        assert!(parse("garbage\n").is_err());
    }

    /// aapt2 2.20's `dump xmltree` of xtask/testdata/window/AndroidManifest.xml (built against android-36's android.jar), as measured 2026-11-04. The activity alone is not hardware
    /// accelerated; the application wrote no boolean, so its flags are the defaults (targetSdkVersion 36): hasCode, hardwareAccelerated, allowBackup, allowClearUserData, extractNativeLibs.
    const WINDOW: &str = r#"N: android=http://schemas.android.com/apk/res/android (line=4)
  E: manifest (line=4)
    A: http://schemas.android.com/apk/res/android:versionCode(0x0101021b)=1
    A: http://schemas.android.com/apk/res/android:versionName(0x0101021c)="1.0" (Raw: "1.0")
    A: http://schemas.android.com/apk/res/android:compileSdkVersion(0x01010572)=36
    A: http://schemas.android.com/apk/res/android:compileSdkVersionCodename(0x01010573)="16" (Raw: "16")
    A: package="com.example.atlwindow" (Raw: "com.example.atlwindow")
    A: platformBuildVersionCode=36
    A: platformBuildVersionName=16
      E: uses-sdk (line=9)
        A: http://schemas.android.com/apk/res/android:minSdkVersion(0x0101020c)=21
        A: http://schemas.android.com/apk/res/android:targetSdkVersion(0x01010270)=36
      E: application (line=11)
        A: http://schemas.android.com/apk/res/android:label(0x01010001)="ATL window test" (Raw: "ATL window test")
          E: activity (line=12)
            A: http://schemas.android.com/apk/res/android:name(0x01010003)=".Main" (Raw: ".Main")
            A: http://schemas.android.com/apk/res/android:exported(0x01010010)=true
            A: http://schemas.android.com/apk/res/android:hardwareAccelerated(0x010102d3)=false
              E: intent-filter (line=13)
                  E: action (line=14)
                    A: http://schemas.android.com/apk/res/android:name(0x01010003)="android.intent.action.MAIN" (Raw: "android.intent.action.MAIN")
                  E: category (line=15)
                    A: http://schemas.android.com/apk/res/android:name(0x01010003)="android.intent.category.LAUNCHER" (Raw: "android.intent.category.LAUNCHER")
"#;

    /// The window fixture's record: the launcher activity's `hardwareAccelerated=false` is its ActivityInfo bit clear (0), the application's defaults are ApplicationInfo.flags 0x3000_8044
    /// (manifest_flags' default test), and the activity's other boolean (`exported`) is kept as written but sets no flag.
    #[test]
    fn the_window_fixture_record() {
        use atl_services::manifest_flags::{activity_flags, application_flags};
        let m = parse(WINDOW).expect("parse");
        assert_eq!(m.name, "com.example.atlwindow");
        assert_eq!((m.min_sdk, m.target_sdk), (21, 36));
        assert!(m.application_booleans.is_empty());
        let launch = m.launch_activity().expect("a launcher activity");
        assert_eq!(launch.name, "com.example.atlwindow.Main");
        assert_eq!(
            launch.booleans,
            BTreeMap::from([
                ("exported".to_owned(), true),
                ("hardwareAccelerated".to_owned(), false)
            ])
        );
        assert_eq!(application_flags(&m), 0x3000_8044);
        assert_eq!(activity_flags(&m, launch), 0);
    }
}
