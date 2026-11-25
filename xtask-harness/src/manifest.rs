// SPDX-License-Identifier: MIT
//! A package's manifest as the gates' oracle reads it, from the APK with AOSP's own `aapt2 dump xmltree --file AndroidManifest.xml` (maintainer D3, 2026-11-03: aapt2 is a build and test time tool). The
//! product never sees it: the services answer from the installer's parse (atl-services `packages`); gates use it for what to expect and whom to start. No second binary-XML decoder is written
//! here: this reads aapt2's text.
//! aapt2's format (tools/aapt2/Debug.cpp `Debug::DumpXml`): `N:` namespace lines, `E: <element> (line=n)`, `A: [<namespace uri>:]<name>[(0x<resource id>)]=<value>`; an element's attributes are indented two
//! columns deeper than it, its child elements four. A value is `"string" (Raw: "string")`, a decimal or `0x` integer, `true` / `false`, or `@0x<id>` / `(type 0x..)0x..` for the rest.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};
use atl_services_types::packages::{PermissionDecl, PlatformManifest};

use crate::host_art;

/// `<uses-library android:name android:required>` (`required` defaults to true).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsesLibrary {
    pub name: String,
    pub required: bool,
}

/// `<instrumentation>` (ParsedInstrumentation): `name` resolved against the package; `target_processes` None = the target's default process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstrumentationDecl {
    pub name: String,
    pub target_package: String,
    pub target_processes: Option<String>,
    pub handle_profiling: bool,
    pub functional_test: bool,
}

/// `<activity>` as aapt2 prints it, reduced to what the gates read: the class and whether it handles MAIN + LAUNCHER.
#[derive(Debug, Clone, PartialEq)]
pub struct ActivityDecl {
    pub name: String,
    pub launcher: bool,
}

/// What aapt2 shows of an APK's AndroidManifest.xml that the gates read.
#[derive(Debug, Clone, PartialEq)]
pub struct Manifest {
    pub name: String,
    pub version_code: i64,
    pub version_name: Option<String>,
    pub min_sdk: i32,
    pub target_sdk: i32,
    pub application_meta_data: Vec<(String, MetaValue)>,
    pub application_booleans: BTreeMap<String, bool>,
    pub activities: Vec<ActivityDecl>,
    /// `<receiver>` / `<provider>` children of `<application>` that carry an `android:name`
    pub receiver_count: usize,
    pub provider_count: usize,
    pub libraries: Vec<UsesLibrary>,
    pub instrumentation: Vec<InstrumentationDecl>,
}

impl Manifest {
    /// The first `<activity>` that handles MAIN + LAUNCHER; an enabled launcher alias counts as its target (`parse`).
    pub fn launch_activity(&self) -> Option<&ActivityDecl> {
        self.activities.iter().find(|a| a.launcher)
    }
}

/// A `<meta-data>` value as ParsingPackageUtils.parseMetaData types it.
#[derive(Debug, Clone, PartialEq)]
pub enum MetaValue {
    String(String),
    Int(i32),
    Bool(bool),
    Float(f32),
}

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
    /// The attributes aapt2 printed as a quoted string (TYPE_STRING): their text alone does not tell `"12"` from the int 12.
    strings: BTreeSet<String>,
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

/// The attribute line's key, value and whether the value is a string: `A: <ns>:name(0x..)=<value>` -> (`name`, `<value>` unquoted when it is a string).
fn attribute(line: &str) -> Result<(String, String, bool)> {
    let rest = line
        .trim_start()
        .strip_prefix("A: ")
        .context("not an A: line")?;
    let (key, value) = rest.split_once('=').context("attribute without `=`")?;
    // `http://schemas.android.com/apk/res/android:name(0x01010003)` -> `name`; `package` stays
    let key = key.split('(').next().unwrap_or(key);
    let key = key.rsplit(':').next().unwrap_or(key);
    let quoted = value.starts_with('"');
    let value = match value.strip_prefix('"') {
        // `"text" (Raw: "text")`: the quoted text up to the closing quote that precedes ` (Raw:`, else the last quote
        Some(quoted) => match quoted.find("\" (Raw: ") {
            Some(end) => &quoted[..end],
            None => quoted.strip_suffix('"').unwrap_or(quoted),
        },
        None => value.split(" (Raw: ").next().unwrap_or(value),
    };
    Ok((key.to_owned(), value.to_owned(), quoted))
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
        // `T:` = a text node: stray text in a manifest; PackageParser skips it too
        if body.starts_with("N: ") || body.starts_with("T: ") {
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
            let (key, value, quoted) =
                attribute(line).with_context(|| format!("attribute line {line:?}"))?;
            let Some((_, element)) = stack.last_mut() else {
                bail!("attribute outside an element: {line:?}");
            };
            if quoted {
                element.strings.insert(key.clone());
            }
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

/// The `<meta-data>` children of `element` (ParsingPackageUtils.parseMetaData): `android:resource` -> its id; else `android:value` typed by how aapt2 printed it (ResourceValues.cpp BinaryPrimitive::PrettyPrint):
/// quoted = string, `true`/`false` = boolean, a decimal / `0x%08x` / `#%08x` (color) = int, another number = float. A reference or other type AOSP drops (Slog.w, no Property); so does this.
/// Known loss (measured, aapt2): a float with no fraction prints as an int (`2.0` -> `2`), so it reads as Int here where AOSP puts a Float; the installer process (ParsingPackageUtils on the binary manifest) has the type.
fn meta_data(element: &Element) -> Result<Vec<(String, MetaValue)>> {
    let mut out = Vec::new();
    for m in element.children("meta-data") {
        let name = m.attr("name").context("<meta-data> without android:name")?;
        let value = if let Some(r) = m.attr("resource") {
            let hex = r
                .strip_prefix("@0x")
                .with_context(|| format!("<meta-data {name}> resource {r:?} is not a reference"))?;
            Some(MetaValue::Int(u32::from_str_radix(hex, 16)? as i32))
        } else if let Some(v) = m.attr("value") {
            meta_value(v, m.strings.contains("value"))
        } else {
            bail!("<meta-data {name}> requires android:value or android:resource");
        };
        if let Some(value) = value {
            out.push((name.to_owned(), value));
        }
    }
    Ok(out)
}

fn meta_value(v: &str, quoted: bool) -> Option<MetaValue> {
    if quoted {
        return Some(MetaValue::String(v.to_owned()));
    }
    let hex = v.strip_prefix("0x").or_else(|| v.strip_prefix('#'));
    match v {
        "true" => Some(MetaValue::Bool(true)),
        "false" => Some(MetaValue::Bool(false)),
        _ => match hex {
            Some(h) => u32::from_str_radix(h, 16)
                .ok()
                .map(|n| MetaValue::Int(n as i32)),
            None => v
                .parse::<i32>()
                .map(MetaValue::Int)
                .or_else(|_| v.parse::<f32>().map(MetaValue::Float))
                .ok(),
        },
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

/// `<kind>` children of `application` that carry an `android:name`: the count the gates read (nameless ones are not counted).
fn named_components(application: Option<&Element>, kind: &str) -> usize {
    application.map_or(0, |a| {
        a.children(kind)
            .filter(|c| c.attr("name").is_some())
            .count()
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
    // every `<activity>` in manifest order: its class and whether it handles MAIN + LAUNCHER
    let mut activities: Vec<ActivityDecl> = application
        .into_iter()
        .flat_map(|a| a.children("activity"))
        .filter_map(|activity| {
            Some(ActivityDecl {
                name: resolve_class(package, activity.attr("name")?),
                launcher: is_launcher(activity),
            })
        })
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
            }),
        }
    }
    Ok(Manifest {
        name: package.to_owned(),
        version_code: int(&root, "versionCode")?.unwrap_or(0),
        version_name: root.attr("versionName").map(str::to_owned),
        min_sdk,
        target_sdk,
        application_meta_data: application.map_or(Ok(vec![]), meta_data)?,
        application_booleans: application.map(booleans).unwrap_or_default(),
        activities,
        receiver_count: named_components(application, "receiver"),
        provider_count: named_components(application, "provider"),
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

/// The `<permission>` elements under the manifest root: name and `protectionLevel` (aapt2 prints the flags as hex; absent = 0, normal, as ParsedPermissionUtils defaults it).
fn permissions(root: &Element) -> Result<Vec<PermissionDecl>> {
    root.children("permission")
        .filter_map(|e| Some((e, e.attr("name")?)))
        .map(|(e, name)| {
            Ok(PermissionDecl {
                name: name.to_owned(),
                protection_level: flags_attr(e, "protectionLevel")?,
            })
        })
        .collect()
}

/// `apk`'s manifest as aapt2 prints it.
fn dump(aapt2: &Path, apk: &Path) -> Result<String> {
    host_art::output(
        Command::new(aapt2)
            .args(["dump", "xmltree", "--file", "AndroidManifest.xml"])
            .arg(apk),
        "aapt2 dump xmltree",
    )
}

/// `apk`'s manifest, through `aapt2`.
pub fn read(aapt2: &Path, apk: &Path) -> Result<Manifest> {
    read_with(aapt2, apk, true)
}

/// [`read`] without resolving `android:value` references: `xtask test provider --break-meta-ref`, whose meta-data check must then fail.
pub fn read_unresolved(aapt2: &Path, apk: &Path) -> Result<Manifest> {
    read_with(aapt2, apk, false)
}

fn read_with(aapt2: &Path, apk: &Path, resolve: bool) -> Result<Manifest> {
    let mut text = dump(aapt2, apk)?;
    if resolve && text.lines().any(|l| value_reference(l).is_some()) {
        let resources = host_art::output(
            Command::new(aapt2).args(["dump", "resources"]).arg(apk),
            "aapt2 dump resources",
        )?;
        text = resolve_value_references(&text, &resources);
    }
    parse(&text).with_context(|| format!("the manifest of {}", apk.display()))
}

/// The `@0x...` of an `android:value` attribute line of the xmltree dump, with the attribute's text up to `=`.
fn value_reference(line: &str) -> Option<(&str, &str)> {
    let (attribute, id) = line.split_once("=@0x")?;
    attribute
        .contains(":value(")
        .then_some((attribute, id.trim()))
}

/// `<meta-data android:value="@string/x">`: obtainAttributes resolves a reference, so parseMetaData sees the value's own type (ParsingPackageUtils.parseMetaData). Rewrites each such line to the default-configuration value `aapt2 dump resources` prints
/// (`resource 0x7f110001 string/x`, then `() "text"`), as if written literally; a reference with no default value stays and meta_value drops it.
fn resolve_value_references(xmltree: &str, resources: &str) -> String {
    let mut values = BTreeMap::new();
    let mut current = None;
    for line in resources.lines().map(str::trim) {
        if let Some(rest) = line.strip_prefix("resource 0x") {
            current = rest.split_whitespace().next();
        } else if let (Some(id), Some(value)) = (current, line.strip_prefix("() ")) {
            values.insert(id, value);
        }
    }
    let resolved = xmltree.lines().map(|line| {
        match value_reference(line).and_then(|(a, id)| Some((a, *values.get(id)?))) {
            Some((attribute, value)) if value.starts_with('"') => {
                format!("{attribute}={value} (Raw: {value})")
            }
            Some((attribute, value)) => format!("{attribute}={value}"),
            None => line.to_owned(),
        }
    });
    resolved.collect::<Vec<_>>().join("\n") + "\n"
}

/// The highest `<uses-feature android:glEsVersion>` (ConfigurationInfo.reqGlEsVersion: major << 16 | minor); None when no feature names one.
/// Read by the static fixture gate only; the services do not consume it yet, so it is not a `Manifest` field.
pub fn gl_es_version(aapt2: &Path, apk: &Path) -> Result<Option<i64>> {
    gl_es_version_of(&dump(aapt2, apk)?)
}

fn gl_es_version_of(text: &str) -> Result<Option<i64>> {
    let root = parse_tree(text)?;
    let mut best = None;
    for f in root.children("uses-feature") {
        if let Some(v) = f.attr("glEsVersion") {
            let n = match v.strip_prefix("0x") {
                Some(h) => i64::from_str_radix(h, 16),
                None => v.parse(),
            }
            .with_context(|| format!("glEsVersion={v}: not an integer"))?;
            best = best.max(Some(n));
        }
    }
    Ok(best)
}

/// The platform manifest of framework-res `apk`, through `aapt2`: only `<permission>` and `<protected-broadcast>` are read, so a manifest without the app fields parses.
pub fn read_platform(aapt2: &Path, apk: &Path) -> Result<PlatformManifest> {
    platform(&parse_tree(&dump(aapt2, apk)?)?)
        .with_context(|| format!("the platform manifest of {}", apk.display()))
}

/// The `<permission>`s and the `<protected-broadcast android:name>`s (ParsingPackageUtils.parseProtectedBroadcast: a nameless one is skipped), in manifest order.
fn platform(root: &Element) -> Result<PlatformManifest> {
    Ok(PlatformManifest {
        permissions: permissions(root)?,
        protected_broadcasts: root
            .children("protected-broadcast")
            .filter_map(|e| e.attr("name"))
            .map(str::to_owned)
            .collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// aapt2 2.20's `dump xmltree` of xtask-harness/testdata/instrument/AndroidManifest.xml, as measured 2026-11-03.
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

    /// `<application>`'s meta-data as aapt2 printed a manifest declaring value="12", "0x1F", "true", "1.5", "#ff112233", "text" and resource="@android:style/Theme" (2026-11-06).
    const META: &str = r#"N: android=http://schemas.android.com/apk/res/android (line=1)
  E: manifest (line=1)
    A: package="com.example.meta" (Raw: "com.example.meta")
      E: application (line=2)
          E: meta-data (line=3)
            A: http://schemas.android.com/apk/res/android:name(0x01010003)="i" (Raw: "i")
            A: http://schemas.android.com/apk/res/android:value(0x01010024)=12
          E: meta-data (line=4)
            A: http://schemas.android.com/apk/res/android:name(0x01010003)="h" (Raw: "h")
            A: http://schemas.android.com/apk/res/android:value(0x01010024)=0x0000001f
          E: meta-data (line=5)
            A: http://schemas.android.com/apk/res/android:name(0x01010003)="b" (Raw: "b")
            A: http://schemas.android.com/apk/res/android:value(0x01010024)=true
          E: meta-data (line=6)
            A: http://schemas.android.com/apk/res/android:name(0x01010003)="f" (Raw: "f")
            A: http://schemas.android.com/apk/res/android:value(0x01010024)=1.5
          E: meta-data (line=7)
            A: http://schemas.android.com/apk/res/android:name(0x01010003)="c" (Raw: "c")
            A: http://schemas.android.com/apk/res/android:value(0x01010024)=#ff112233
          E: meta-data (line=8)
            A: http://schemas.android.com/apk/res/android:name(0x01010003)="r" (Raw: "r")
            A: http://schemas.android.com/apk/res/android:resource(0x01010025)=@0x01030005
          E: meta-data (line=9)
            A: http://schemas.android.com/apk/res/android:name(0x01010003)="t" (Raw: "t")
            A: http://schemas.android.com/apk/res/android:value(0x01010024)="text" (Raw: "text")
"#;

    #[test]
    fn text_nodes_are_skipped() {
        let tree = parse_tree("E: manifest (line=1)\n  T: ' > '\n  E: application (line=2)\n")
            .expect("parse");
        assert_eq!(tree.name, "manifest");
    }

    #[test]
    fn application_meta_data_types() {
        let m = parse(META).expect("parse");
        // expected: the values the manifest declared, typed as parseMetaData types them; 0x01030005 = android.R.style.Theme (public.xml)
        assert_eq!(
            m.application_meta_data,
            [
                ("i".to_owned(), MetaValue::Int(12)),
                ("h".to_owned(), MetaValue::Int(0x1f)),
                ("b".to_owned(), MetaValue::Bool(true)),
                ("f".to_owned(), MetaValue::Float(1.5)),
                ("c".to_owned(), MetaValue::Int(0xff11_2233_u32 as i32)),
                ("r".to_owned(), MetaValue::Int(0x0103_0005)),
                ("t".to_owned(), MetaValue::String("text".to_owned())),
            ]
        );
    }

    #[test]
    fn the_instrumentation_fixture() {
        // the values xtask-harness/testdata/instrument/AndroidManifest.xml declares
        let m = parse(INSTRUMENT).expect("parse");
        assert_eq!(m.name, "com.example.atlinstr");
        assert_eq!(
            (m.version_code, m.version_name.as_deref()),
            (3, Some("1.0"))
        );
        assert_eq!((m.min_sdk, m.target_sdk), (21, 36));
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
        assert!(m.activities.is_empty() && m.launch_activity().is_none());
        assert!(m.application_booleans.is_empty());
    }

    /// A string and a bool reference resolve to their default-configuration values and type; one with no default value stays a reference, which meta_value drops. Values as CTR's `aapt2 dump resources` prints them.
    #[test]
    fn value_references_resolve() {
        let value = "http://schemas.android.com/apk/res/android:value(0x01010024)";
        let text = format!(
            r#"N: android=http://schemas.android.com/apk/res/android (line=3)
  E: manifest (line=3)
    A: package="com.example.p" (Raw: "com.example.p")
      E: application (line=7)
          E: meta-data (line=8)
            A: http://schemas.android.com/apk/res/android:name(0x01010003)="s" (Raw: "s")
            A: {value}=@0x7f110001
          E: meta-data (line=9)
            A: http://schemas.android.com/apk/res/android:name(0x01010003)="b" (Raw: "b")
            A: {value}=@0x7f050000
          E: meta-data (line=10)
            A: http://schemas.android.com/apk/res/android:name(0x01010003)="none" (Raw: "none")
            A: {value}=@0x7f110002
"#
        );
        let resources = r#"    resource 0x7f050000 bool/flag
      () true
    resource 0x7f110001 string/id
      (fr) "autre"
      () "ca-app~1"
    resource 0x7f110002 string/only_fr
      (fr) "x"
"#;
        let m = parse(&resolve_value_references(&text, resources)).expect("parse");
        assert_eq!(
            m.application_meta_data,
            vec![
                ("s".to_owned(), MetaValue::String("ca-app~1".to_owned())),
                ("b".to_owned(), MetaValue::Bool(true)),
            ]
        );
    }

    /// `<provider>`: the count is of the named ones; a nameless provider is not counted.
    #[test]
    fn providers_counted_by_name() {
        let text = r#"N: android=http://schemas.android.com/apk/res/android (line=3)
  E: manifest (line=3)
    A: package="com.example.p" (Raw: "com.example.p")
      E: application (line=7)
          E: provider (line=8)
            A: http://schemas.android.com/apk/res/android:name(0x01010003)=".A" (Raw: ".A")
          E: provider (line=9)
            A: http://schemas.android.com/apk/res/android:authorities(0x01010018)="a;b" (Raw: "a;b")
"#;
        assert_eq!(parse(text).expect("parse").provider_count, 1);
    }

    /// `<receiver>`: the count is of the named ones; a nameless receiver is not counted.
    #[test]
    fn receivers_counted_by_name() {
        let text = r#"N: android=http://schemas.android.com/apk/res/android (line=3)
  E: manifest (line=3)
    A: package="com.example.r" (Raw: "com.example.r")
      E: application (line=7)
          E: receiver (line=8)
            A: http://schemas.android.com/apk/res/android:name(0x01010003)=".Plain" (Raw: ".Plain")
          E: receiver (line=9)
            A: http://schemas.android.com/apk/res/android:name(0x01010003)=".Closed" (Raw: ".Closed")
            A: http://schemas.android.com/apk/res/android:exported(0x01010010)=false
          E: receiver (line=10)
            A: http://schemas.android.com/apk/res/android:exported(0x01010010)=true
"#;
        assert_eq!(parse(text).expect("parse").receiver_count, 2);
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

    /// A launcher activity named relative to the package, a relative instrumentation name: the resolution rules ParsedComponent / ParsingUtils apply.
    #[test]
    fn launcher_activity_and_instrumentation_resolution() {
        let text = r#"N: android=http://schemas.android.com/apk/res/android (line=3)
  E: manifest (line=3)
    A: package="com.example.atltest" (Raw: "com.example.atltest")
      E: application (line=7)
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
        // both activities, in order; only Main handles MAIN + LAUNCHER
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

    /// aapt2 2.20's `dump xmltree` of xtask-harness/testdata/window/AndroidManifest.xml (built against android-36's android.jar), as measured 2026-11-04. The activity alone is not hardware
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

    /// The window fixture's manifest: its launcher activity, and no boolean on the application.
    #[test]
    fn the_window_fixture_record() {
        let m = parse(WINDOW).expect("parse");
        assert_eq!(m.name, "com.example.atlwindow");
        assert_eq!((m.min_sdk, m.target_sdk), (21, 36));
        assert!(m.application_booleans.is_empty());
        let launch = m.launch_activity().expect("a launcher activity");
        assert_eq!(launch.name, "com.example.atlwindow.Main");
    }

    /// aapt2's `dump xmltree` of framework-res.apk (base b75bb52a, 2026-11-21), cut to two `<protected-broadcast>`s and one `<permission>`; core/res/AndroidManifest.xml:29, :69, :840. The full dump has 682
    /// protected broadcasts, as many as the source has `<protected-broadcast` lines.
    const PLATFORM: &str = r#"N: android=http://schemas.android.com/apk/res/android (line=20)
  E: manifest (line=20)
    A: http://schemas.android.com/apk/res/android:sharedUserId(0x0101000b)="android.uid.system" (Raw: "android.uid.system")
    A: coreApp=true
    A: package="android" (Raw: "android")
      E: protected-broadcast (line=29)
        A: http://schemas.android.com/apk/res/android:name(0x01010003)="android.intent.action.SCREEN_OFF" (Raw: "android.intent.action.SCREEN_OFF")
      E: protected-broadcast (line=69)
        A: http://schemas.android.com/apk/res/android:name(0x01010003)="android.intent.action.BATTERY_CHANGED" (Raw: "android.intent.action.BATTERY_CHANGED")
      E: permission (line=840)
        A: http://schemas.android.com/apk/res/android:label(0x01010001)=@0x01040856
        A: http://schemas.android.com/apk/res/android:name(0x01010003)="android.permission.READ_CONTACTS" (Raw: "android.permission.READ_CONTACTS")
        A: http://schemas.android.com/apk/res/android:protectionLevel(0x01010009)=0x00000001
"#;

    #[test]
    fn the_platform_manifest_reads_protected_broadcasts_and_permissions() {
        let p = platform(&parse_tree(PLATFORM).expect("tree")).expect("platform");
        assert_eq!(
            p.protected_broadcasts,
            [
                "android.intent.action.SCREEN_OFF",
                "android.intent.action.BATTERY_CHANGED"
            ]
        );
        // READ_CONTACTS is dangerous (PermissionInfo.PROTECTION_DANGEROUS = 1)
        assert_eq!(
            p.permissions,
            [PermissionDecl {
                name: "android.permission.READ_CONTACTS".into(),
                protection_level: 1
            }]
        );
    }
}
