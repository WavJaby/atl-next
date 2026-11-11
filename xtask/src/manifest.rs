// SPDX-License-Identifier: MIT
//! A package's manifest as the services need it, read from the APK with AOSP's own `aapt2 dump xmltree --file AndroidManifest.xml` (owner D3, 2026-11-03: aapt2 is a build and test time tool; real installs
//! at run time read the manifest with libandroidfw's ResXMLTree through a boundary crate, and produce the same `Manifest`). No second binary-XML decoder is written here: this reads aapt2's text.
//! aapt2's format (tools/aapt2/Debug.cpp `Debug::DumpXml`): `N:` namespace lines, `E: <element> (line=n)`, `A: [<namespace uri>:]<name>[(0x<resource id>)]=<value>`; an element's attributes are indented two
//! columns deeper than it, its child elements four. A value is `"string" (Raw: "string")`, a decimal or `0x` integer, `true` / `false`, or `@0x<id>` / `(type 0x..)0x..` for the rest.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};
use atl_services_types::packages::{
    ActivityDecl, InstrumentationDecl, IntentFilterDecl, Manifest, MetaValue, PermissionDecl,
    ProviderDecl, ReceiverDecl, SCREEN_ORIENTATION_UNSPECIFIED, UsesLibrary,
};

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

/// `android:screenOrientation` as ActivityInfo.screenOrientation. aapt2 prints the enum in decimal, `-1` for unspecified (aapt2 2.19 on a probe manifest). Unwritten = SCREEN_ORIENTATION_UNSPECIFIED (PackageParser.java:4457-4459).
fn screen_orientation(element: &Element) -> Result<i32> {
    let Some(value) = element.attr("screenOrientation") else {
        return Ok(SCREEN_ORIENTATION_UNSPECIFIED);
    };
    value
        .parse::<i32>()
        .with_context(|| format!("screenOrientation {value:?} is not an enum value"))
}

/// `android:resizeableActivity`: the activity's, else `<application>`'s, else true. PackageParser.setActivityResizeMode takes the activity's explicit value, then the application's explicit default, then the targetSdk default (PackageParser.java:4677-4715). That default is resizable in both branches (ActivityInfo.isResizeableMode, ActivityInfo.java:2073-2080), so targetSdk does not enter.
fn resizeable(activity: &Element, application: Option<&Element>) -> bool {
    let application = application.is_none_or(|a| a.bool_attr("resizeableActivity", true));
    activity.bool_attr("resizeableActivity", application)
}

/// `android:minAspectRatio` (or `maxAspectRatio`): the activity's, else `<application>`'s (PackageParser.setMinAspectRatio / setMaxAspectRatio, PackageParser.java:4758-4768, :4721-4752). None = not declared.
/// aapt2 prints a float in its shortest form (`1.86`, and `2` for 2.0). A value that does not parse is an error; AOSP ignores a non-float here (PackageParser.java:4472-4481).
fn aspect_ratio(
    element: &Element,
    application: Option<&Element>,
    name: &str,
) -> Result<Option<f32>> {
    let Some(value) = element
        .attr(name)
        .or_else(|| application.and_then(|a| a.attr(name)))
    else {
        return Ok(None);
    };
    value
        .parse::<f32>()
        .map(Some)
        .with_context(|| format!("{name}={value:?} is not a float"))
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

/// The `<meta-data>` children of `element` (ParsingPackageUtils.parseMetaData): `android:resource` -> its id; else `android:value` typed by how aapt2 printed it (ResourceValues.cpp BinaryPrimitive::PrettyPrint):
/// quoted = string, `true`/`false` = boolean, a decimal / `0x%08x` / `#%08x` (color) = int, another number = float. A reference or other type AOSP drops (Slog.w, no Property); so does this.
/// Known loss (measured, aapt2 of slot 3): a float with no fraction prints as an int (`2.0` -> `2`), so it reads as Int here where AOSP puts a Float; the installer process (ParsingPackageUtils on the binary manifest) has the type.
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

/// An `<intent-filter>` (ParsedIntentInfoUtils.parseIntentInfo, reduced): `<action>`, `<category>`, `<data android:scheme / android:mimeType>`; a kind the filter does not write stays None (IntentFilter's null list).
fn intent_filter(f: &Element) -> IntentFilterDecl {
    let names = |kind: &str, attr: &str| -> Option<Vec<String>> {
        let v: Vec<String> = f
            .children(kind)
            .filter_map(|e| e.attr(attr).map(str::to_owned))
            .collect();
        (!v.is_empty()).then_some(v)
    };
    IntentFilterDecl {
        actions: names("action", "name").unwrap_or_default(),
        categories: names("category", "name"),
        schemes: names("data", "scheme"),
        types: names("data", "mimeType"),
    }
}

/// Every `<receiver>` of `application`, in manifest order (ParsedActivityUtils.parseReceiver): `exported` unwritten = it has an intent filter.
fn receivers(application: Option<&Element>, package: &str) -> Vec<ReceiverDecl> {
    application
        .into_iter()
        .flat_map(|a| a.children("receiver"))
        .filter_map(|r| {
            let filters: Vec<IntentFilterDecl> =
                r.children("intent-filter").map(intent_filter).collect();
            Some(ReceiverDecl {
                name: resolve_class(package, r.attr("name")?),
                exported: r.bool_attr("exported", !filters.is_empty()),
                enabled: r.bool_attr("enabled", true),
                permission: r.attr("permission").map(str::to_owned),
                process: process_attr(r, package),
                filters,
            })
        })
        .collect()
}

/// Every `<provider>` of `application`, in manifest order (ParsedProviderUtils.parseProvider): `exported` unwritten = targetSdk < JELLY_BEAN_MR1; a read or write permission falls back to `android:permission`, then the application's.
fn providers(
    application: Option<&Element>,
    package: &str,
    target_sdk: i32,
) -> Result<Vec<ProviderDecl>> {
    /// Build.VERSION_CODES.JELLY_BEAN_MR1.
    const JELLY_BEAN_MR1: i32 = 17;
    let Some(application) = application else {
        return Ok(vec![]);
    };
    application
        .children("provider")
        .filter(|p| p.attr("name").is_some())
        .map(|p| {
            let permission = |own: &str| {
                p.attr(own)
                    .or_else(|| p.attr("permission"))
                    .or_else(|| application.attr("permission"))
                    .map(str::to_owned)
            };
            let init_order = p.attr("initOrder").unwrap_or("0");
            Ok(ProviderDecl {
                name: resolve_class(package, p.attr("name").unwrap_or_default()),
                authority: p.attr("authorities").map(str::to_owned),
                exported: p.bool_attr("exported", target_sdk < JELLY_BEAN_MR1),
                enabled: p.bool_attr("enabled", true),
                process: process_attr(p, package),
                read_permission: permission("readPermission"),
                write_permission: permission("writePermission"),
                grant_uri_permissions: p.bool_attr("grantUriPermissions", false),
                force_uri_permissions: p.bool_attr("forceUriPermissions", false),
                multiprocess: p.bool_attr("multiprocess", false),
                init_order: init_order
                    .parse()
                    .with_context(|| format!("initOrder {init_order:?} is not an integer"))?,
                syncable: p.bool_attr("syncable", false),
                direct_boot_aware: p.bool_attr("directBootAware", false),
                meta_data: meta_data(p)?,
            })
        })
        .collect()
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
                meta_data: meta_data(activity)?,
                screen_orientation: screen_orientation(activity)?,
                resizeable: resizeable(activity, application),
                min_aspect_ratio: aspect_ratio(activity, application, "minAspectRatio")?,
                max_aspect_ratio: aspect_ratio(activity, application, "maxAspectRatio")?,
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
                meta_data: meta_data(alias)?,
                screen_orientation: screen_orientation(alias)?,
                resizeable: resizeable(alias, application),
                min_aspect_ratio: aspect_ratio(alias, application, "minAspectRatio")?,
                max_aspect_ratio: aspect_ratio(alias, application, "maxAspectRatio")?,
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
        application_class: application
            .and_then(|a| a.attr("name"))
            .map(|n| resolve_class(package, n)),
        app_component_factory: application
            .and_then(|a| a.attr("appComponentFactory"))
            .map(|n| resolve_class(package, n)),
        application_meta_data: application.map_or(Ok(vec![]), meta_data)?,
        application_booleans: application.map(booleans).unwrap_or_default(),
        activities,
        permissions: permissions(&root)?,
        receivers: receivers(application, package),
        providers: providers(application, package, target_sdk)?,
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

/// The permissions `apk` defines (framework-res: the platform's), through `aapt2`; only `<permission>` is read, so a manifest without the app fields (framework-res) parses.
pub fn read_permissions(aapt2: &Path, apk: &Path) -> Result<Vec<PermissionDecl>> {
    permissions(&parse_tree(&dump(aapt2, apk)?)?)
        .with_context(|| format!("the permissions of {}", apk.display()))
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

    /// `<application>`'s meta-data as aapt2 (slot 3) printed a manifest declaring value="12", "0x1F", "true", "1.5", "#ff112233", "text" and resource="@android:style/Theme" (2026-11-06).
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

    /// `screenOrientation`, `resizeableActivity` and the aspect limits: the activity's own value, else the application's (PackageParser.setActivityResizeMode, setMinAspectRatio, setMaxAspectRatio).
    #[test]
    fn orientation_resizeable_and_aspect_follow_the_manifest() {
        // targetSdk, the `<application>` attribute lines, the activity's attribute lines. aapt2 prints floats shortest (`1.86`, `2` for 2.0) and the enum in decimal.
        let xml = |target: i32, application: &str, activity: &str| {
            format!(
                r#"  E: manifest (line=3)
    A: package="com.example.atltest" (Raw: "com.example.atltest")
      E: uses-sdk (line=4)
        A: http://schemas.android.com/apk/res/android:targetSdkVersion(0x01010270)={target}
      E: application (line=7)
{application}
          E: activity (line=8)
            A: http://schemas.android.com/apk/res/android:name(0x01010003)=".A" (Raw: ".A")
{activity}
"#
            )
        };
        let decl = |target: i32, application: &str, activity: &str| {
            let a = parse(&xml(target, application, activity))
                .expect("parse")
                .activities
                .remove(0);
            (
                a.screen_orientation,
                a.resizeable,
                a.min_aspect_ratio,
                a.max_aspect_ratio,
            )
        };
        // The activity's own values.
        let own = r#"            A: http://schemas.android.com/apk/res/android:screenOrientation(0x0101001e)=1
            A: http://schemas.android.com/apk/res/android:resizeableActivity(0x010104f6)=false
            A: http://schemas.android.com/apk/res/android:maxAspectRatio(0x01010560)=1.86
            A: http://schemas.android.com/apk/res/android:minAspectRatio(0x0101059b)=0.5"#;
        assert_eq!(decl(33, "", own), (1, false, Some(0.5), Some(1.86)));
        // Unwritten on the activity: the application's values.
        let app = r#"            A: http://schemas.android.com/apk/res/android:resizeableActivity(0x010104f6)=false
            A: http://schemas.android.com/apk/res/android:minAspectRatio(0x0101059b)=1.5
            A: http://schemas.android.com/apk/res/android:maxAspectRatio(0x01010560)=2"#;
        assert_eq!(decl(33, app, ""), (-1, false, Some(1.5), Some(2.0)));
        // The activity's own value wins over the application's.
        let wins = r#"            A: http://schemas.android.com/apk/res/android:resizeableActivity(0x010104f6)=true
            A: http://schemas.android.com/apk/res/android:maxAspectRatio(0x01010560)=1.5"#;
        assert_eq!(decl(33, app, wins), (-1, true, Some(1.5), Some(1.5)));
        // Unwritten at both levels: resizable at either targetSdk (AOSP's SDK-24 default and its pre-N FORCE_RESIZABLE are both resizable).
        assert_eq!(decl(23, "", ""), (-1, true, None, None));
        assert_eq!(decl(33, "", ""), (-1, true, None, None));
        // A landscape-family value (sensorLandscape = 6).
        let sensor = r#"            A: http://schemas.android.com/apk/res/android:screenOrientation(0x0101001e)=6"#;
        assert_eq!(decl(33, "", sensor).0, 6);
        // A value that is not a number of its kind is an error, not a silent default.
        let bad = |activity: &str| parse(&xml(33, "", activity)).is_err();
        assert!(bad(
            r#"            A: http://schemas.android.com/apk/res/android:maxAspectRatio(0x01010560)=abc"#
        ));
        assert!(bad(
            r#"            A: http://schemas.android.com/apk/res/android:screenOrientation(0x0101001e)=portrait"#
        ));
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

    /// `<provider>`: no targetSdk written = 0 here, so `exported` unwritten is true (< 17, parseProvider); permission falls back own -> `android:permission` -> application; initOrder as `aapt2` prints it (decimal, CTR's manifest).
    #[test]
    fn providers_follow_parse_provider() {
        let text = r#"N: android=http://schemas.android.com/apk/res/android (line=3)
  E: manifest (line=3)
    A: package="com.example.p" (Raw: "com.example.p")
      E: application (line=7)
        A: http://schemas.android.com/apk/res/android:permission(0x01010006)="p.App" (Raw: "p.App")
          E: provider (line=8)
            A: http://schemas.android.com/apk/res/android:name(0x01010003)=".A" (Raw: ".A")
            A: http://schemas.android.com/apk/res/android:authorities(0x01010018)="a;b" (Raw: "a;b")
            A: http://schemas.android.com/apk/res/android:initOrder(0x0101001a)=100
            A: http://schemas.android.com/apk/res/android:readPermission(0x01010007)="p.R" (Raw: "p.R")
              E: meta-data (line=9)
                A: http://schemas.android.com/apk/res/android:name(0x01010003)="k" (Raw: "k")
                A: http://schemas.android.com/apk/res/android:value(0x01010024)="v" (Raw: "v")
"#;
        let m = parse(text).expect("parse");
        let p = &m.providers[0];
        assert_eq!(p.name, "com.example.p.A");
        assert_eq!(p.authority.as_deref(), Some("a;b"));
        assert_eq!(p.init_order, 100);
        assert!(p.exported && p.enabled);
        assert_eq!(p.read_permission.as_deref(), Some("p.R"));
        assert_eq!(p.write_permission.as_deref(), Some("p.App"));
        assert_eq!(
            p.meta_data,
            vec![("k".to_owned(), MetaValue::String("v".to_owned()))]
        );
    }

    /// `<receiver>`: exported unwritten = has a filter (ParsedActivityUtils:495-507), written wins; permission, process, filter parts as written, unwritten kinds None.
    #[test]
    fn receivers_follow_parse_receiver() {
        let text = r#"N: android=http://schemas.android.com/apk/res/android (line=3)
  E: manifest (line=3)
    A: package="com.example.r" (Raw: "com.example.r")
      E: application (line=7)
          E: receiver (line=8)
            A: http://schemas.android.com/apk/res/android:name(0x01010003)=".Plain" (Raw: ".Plain")
          E: receiver (line=9)
            A: http://schemas.android.com/apk/res/android:name(0x01010003)=".Filtered" (Raw: ".Filtered")
            A: http://schemas.android.com/apk/res/android:permission(0x01010006)="p.X" (Raw: "p.X")
            A: http://schemas.android.com/apk/res/android:process(0x01010011)=":bg" (Raw: ":bg")
              E: intent-filter (line=10)
                  E: action (line=11)
                    A: http://schemas.android.com/apk/res/android:name(0x01010003)="a.B" (Raw: "a.B")
                  E: data (line=12)
                    A: http://schemas.android.com/apk/res/android:scheme(0x01010027)="package" (Raw: "package")
          E: receiver (line=13)
            A: http://schemas.android.com/apk/res/android:name(0x01010003)=".Closed" (Raw: ".Closed")
            A: http://schemas.android.com/apk/res/android:exported(0x01010010)=false
              E: intent-filter (line=14)
                  E: action (line=15)
                    A: http://schemas.android.com/apk/res/android:name(0x01010003)="a.C" (Raw: "a.C")
"#;
        let m = parse(text).expect("parse");
        let shape: Vec<(&str, bool)> = m
            .receivers
            .iter()
            .map(|r| (r.name.as_str(), r.exported))
            .collect();
        assert_eq!(
            shape,
            [
                ("com.example.r.Plain", false),
                ("com.example.r.Filtered", true),
                ("com.example.r.Closed", false)
            ]
        );
        let f = &m.receivers[1];
        assert_eq!(f.permission.as_deref(), Some("p.X"));
        assert_eq!(f.process.as_deref(), Some("com.example.r:bg"));
        assert_eq!(
            f.filters,
            [IntentFilterDecl {
                actions: vec!["a.B".into()],
                categories: None,
                schemes: Some(vec!["package".into()]),
                types: None,
            }]
        );
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
        use atl_services_types::manifest_flags::{activity_flags, application_flags};
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
