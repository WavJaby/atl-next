// SPDX-License-Identifier: MIT
//! atl-installer-dump: the installer's output (frames of upstream parcels on stdin) decoded by `atl_services::installed::decode`, printed as `key value` lines for `xtask test installer`, which compares them
//! with its oracles. Test-only. A decode error goes to stderr with its name, exit 1, and nothing on stdout.
//! Lines: `package`, `version-code` (getLongVersionCode), `version-name`, `min-sdk`, `target-sdk`, `app-class`, `app-flags`, `extract-native-libs`, `has-code`, `app-process`, `app-theme`, `uses-permission`*,
//! `permission <name> protection-level=`*, `activity`/`receiver`/`service`/`provider <name> key=value...`*, `filter <kind> <component> actions= categories= schemes= types=`*, `uses-library <name> required=`*, `instrumentation <name> target=`*, `split`*, `primary-abi`, `lib-dir`.

extern crate atl_parcelables;
extern crate binder;

use std::io::{Read, Write};
use std::process::ExitCode;

use anyhow::{Context, Result};
use atl_parcelables::android::content::pm::PackageInfo;
use atl_parcelables::parts::ComponentInfo;
use atl_services::installed::{Installed, decode};
use atl_services_types::installer_output::Kind;

/// ApplicationInfo.FLAG_EXTRACT_NATIVE_LIBS (ApplicationInfo.java).
const FLAG_EXTRACT_NATIVE_LIBS: i32 = 1 << 28;
/// ApplicationInfo.FLAG_HAS_CODE (ApplicationInfo.java).
const FLAG_HAS_CODE: i32 = 1 << 2;

fn or_none(v: &Option<String>) -> &str {
    v.as_deref().unwrap_or("none")
}

fn component(kind: &str, c: &ComponentInfo) -> String {
    format!(
        "{kind} {} exported={} enabled={} process={}",
        or_none(&c.item.name),
        c.exported,
        c.enabled,
        or_none(&c.process_name)
    )
}

fn lines(installed: &Installed) -> Result<Vec<String>> {
    let pkg: &PackageInfo = &installed.package;
    let app = pkg
        .application_info
        .as_ref()
        .context("PackageInfo without applicationInfo")?;
    let mut out = vec![
        format!(
            "package {}",
            pkg.package_name.as_deref().context("no packageName")?
        ),
        // PackageInfo.getLongVersionCode: versionCodeMajor in the high 32 bits, versionCode in the low
        format!(
            "version-code {}",
            (i64::from(pkg.version_code_major) << 32) | i64::from(pkg.version_code as u32)
        ),
        format!("version-name {}", or_none(&pkg.version_name)),
        format!("min-sdk {}", app.min_sdk_version),
        format!("target-sdk {}", app.target_sdk_version),
        format!("app-class {}", or_none(&app.class_name)),
        format!("app-flags {:#x}", app.flags),
        format!(
            "extract-native-libs {}",
            app.flags & FLAG_EXTRACT_NATIVE_LIBS != 0
        ),
        format!("has-code {}", app.flags & FLAG_HAS_CODE != 0),
        format!("app-process {}", or_none(&app.process_name)),
        format!("app-theme {:#x}", app.theme),
    ];
    for p in pkg.requested_permissions.iter().flatten().flatten() {
        out.push(format!("uses-permission {p}"));
    }
    for p in pkg.permissions.iter().flatten().flatten() {
        out.push(format!(
            "permission {} protection-level={:#x}",
            or_none(&p.item.name),
            p.protection_level
        ));
    }
    for (kind, list) in [("activity", &pkg.activities), ("receiver", &pkg.receivers)] {
        for a in list.iter().flatten().flatten() {
            out.push(format!(
                "{} launch-mode={} theme={:#x} config-changes={:#x} orientation={} flags={:#x} permission={}",
                component(kind, &a.component),
                a.launch_mode,
                a.theme,
                a.config_changes,
                a.screen_orientation,
                a.flags,
                or_none(&a.permission)
            ));
        }
    }
    for s in pkg.services.iter().flatten().flatten() {
        out.push(format!(
            "{} permission={}",
            component("service", &s.component),
            or_none(&s.permission)
        ));
    }
    for p in pkg.providers.iter().flatten().flatten() {
        out.push(format!(
            "{} authority={} read-permission={} write-permission={}",
            component("provider", &p.component),
            or_none(&p.authority),
            or_none(&p.read_permission),
            or_none(&p.write_permission)
        ));
    }
    let join = |v: Option<&Vec<Option<String>>>| {
        v.into_iter()
            .flatten()
            .map(|s| or_none(s).to_owned())
            .collect::<Vec<_>>()
            .join(",")
    };
    for c in &installed.intents {
        let kind = match c.kind {
            Kind::ActivityIntents => "activity",
            Kind::ReceiverIntents => "receiver",
            Kind::ServiceIntents => "service",
            Kind::ProviderIntents => "provider",
            Kind::Package | Kind::UsesLibraries => continue,
        };
        for i in &c.intents {
            let f = i
                .intent_filter
                .as_ref()
                .context("intent without a filter")?;
            out.push(format!(
                "filter {kind} {} actions={} categories={} schemes={} types={}",
                c.name,
                join(Some(&f.actions)),
                join(f.categories.as_ref()),
                join(f.data_schemes.as_ref()),
                join(f.data_types.as_ref())
            ));
        }
    }
    for l in &installed.libraries {
        out.push(format!("uses-library {} required={}", l.name, l.required));
    }
    for i in pkg.instrumentation.iter().flatten().flatten() {
        out.push(format!(
            "instrumentation {} target={}",
            or_none(&i.item.name),
            or_none(&i.target_package)
        ));
    }
    for s in pkg.split_names.iter().flatten().flatten() {
        out.push(format!("split {s}"));
    }
    out.push(format!("primary-abi {}", or_none(&app.primary_cpu_abi)));
    out.push(format!("lib-dir {}", or_none(&app.native_library_dir)));
    Ok(out)
}

fn run() -> Result<()> {
    let mut data = Vec::new();
    std::io::stdin()
        .read_to_end(&mut data)
        .context("reading stdin")?;
    let installed = decode(&data)?;
    let lines = lines(&installed)?;
    let mut out = std::io::stdout().lock();
    for l in lines {
        writeln!(out, "{l}")?;
    }
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            let _ = writeln!(std::io::stderr(), "atl-installer-dump: {e:#}");
            ExitCode::FAILURE
        }
    }
}
