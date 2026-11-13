// SPDX-License-Identifier: MIT
//! The services' install (owner option I, 2026-11-13): PackageManagerService's half. The host does the file work (atld: staging, the installer run, the rename, the data dirs; installd's role) over the
//! control socket; here the installer's frames are decoded ([`crate::installed`]), the package gets its uid, code and data paths as PMS sets them, its instruction set from `primaryCpuAbi`, the GL driver
//! from the owner table and the host's ES1 fact, and the install-time grants over every definition (PermissionManagerService).

use std::collections::BTreeSet;

use anyhow::{Context, Result, ensure};
use atl_android_paths::{DATA_APP, DATA_USER, DATA_USER_DE};
use atl_services_types::gl_driver;
use atl_services_types::installer_output::Kind;
use atl_services_types::native_libs::{LIB_DIR_NAME, SupportedAbi, instruction_set};
use atl_services_types::packages::{Manifest, PackageRecord, PermissionDecl};

use crate::activity::log;
use crate::installed::{ComponentIntents, Installed};
use crate::process_control::ControlSocket;

/// One install session the host holds files for, and (until the services answer from PackageInfo) the manifest xtask read from its base APK.
pub struct Session {
    pub id: u32,
    pub manifest: Manifest,
}

/// What the install takes besides the sessions.
pub struct Config {
    /// The first package's uid; the i-th session gets `first_uid + i`. The host refuses any at or below the installer's (atl_broker::INSTALLER_UID).
    pub first_uid: u32,
    /// The device's ABIs, preferred first: primaryCpuAbi -> instruction set.
    pub abis: Vec<SupportedAbi>,
    /// framework-res's `<permission>` definitions: the grant rule's platform half.
    pub platform_permissions: Vec<PermissionDecl>,
    /// Test only: every package gets the native GL driver (the GL selection gate's break).
    pub no_gl_selection: bool,
}

/// Installs every session in order (PMS's boot scan of installed packages): the records the services answer from.
pub fn install(
    control: &ControlSocket,
    sessions: Vec<Session>,
    c: &Config,
) -> Result<Vec<PackageRecord>> {
    ensure!(!c.abis.is_empty(), "no device ABI");
    let owner = gl_driver::owner_table(gl_driver::OWNER_TABLE).map_err(anyhow::Error::msg)?;
    let mut records: Vec<PackageRecord> = Vec::new();
    for (i, s) in sessions.into_iter().enumerate() {
        let uid = c.first_uid + u32::try_from(i)?;
        let frames = control
            .install(s.id, uid)
            .with_context(|| format!("installing session {}", s.id))?;
        let bytes = std::fs::read(&frames).with_context(|| format!("reading {frames}"))?;
        let installed = crate::installed::decode(&bytes)
            .map_err(|e| anyhow::anyhow!("session {}: {frames}: {e:?}", s.id))?;
        let name = installed
            .package
            .package_name
            .clone()
            .with_context(|| format!("session {}: no package name", s.id))?;
        ensure!(
            name == s.manifest.name,
            "session {}: the installer parsed {name}, the manifest names {}",
            s.id,
            s.manifest.name
        );
        ensure!(
            !records.iter().any(|r| r.name == name),
            "two packages named {name}"
        );
        let primary = installed
            .package
            .application_info
            .as_ref()
            .and_then(|a| a.primary_cpu_abi.clone());
        for d in route_differences(&s.manifest, &installed) {
            log(format_args!("install {name}: route difference: {d}"));
        }
        let needs_es1 = control
            .commit(s.id, &name, uid)
            .with_context(|| format!("committing {name}"))?;
        let gl = if c.no_gl_selection {
            gl_driver::GlDriver::Native
        } else {
            gl_driver::select(&owner, &name, needs_es1)
        };
        let isa = instruction_set(primary.as_deref(), &c.abis).to_owned();
        let code = format!("{DATA_APP}/{name}");
        log(format_args!(
            "install {name}: uid {uid}, primary-abi {}, isa {isa}",
            primary.as_deref().unwrap_or("none")
        ));
        records.push(PackageRecord {
            gl_driver: gl,
            uid,
            source_dir: format!("{code}/base.apk"),
            primary_cpu_abi: primary,
            // PMS: `<codePath>/lib/<isa>` whether or not anything was copied (PackageImpl / ScanPackageUtils derive it from the code path)
            native_library_dir: format!("{code}/{LIB_DIR_NAME}/{isa}"),
            instruction_set: isa,
            data_dir: format!("{DATA_USER}/0/{name}"),
            device_protected_data_dir: format!("{DATA_USER_DE}/0/{name}"),
            manifest: s.manifest,
            granted_permissions: vec![],
        });
    }
    // install-time grants over every definition: the platform's first, then the packages' (permission::install_grants)
    let grants: Vec<Vec<String>> = records
        .iter()
        .map(|p| {
            let defs = c.platform_permissions.iter().map(|d| ("android", d)).chain(
                records
                    .iter()
                    .flat_map(|o| o.permissions.iter().map(move |d| (o.name.as_str(), d))),
            );
            atl_services_types::permission::install_grants(&p.name, &p.requested_permissions, defs)
        })
        .collect();
    for (record, granted) in records.iter_mut().zip(grants) {
        record.granted_permissions = granted;
    }
    Ok(records)
}

/// The fields both routes carry, as `key value` lines, where they differ: the manifest xtask read with aapt2, and the installer's PackageInfo. Process names spelled out on both sides; launcher = the
/// first activity with a MAIN + LAUNCHER filter. Known differences (M3 findings): resource-valued `enabled`, activity-alias launchers, `maxSdkVersion` permissions.
pub fn route_differences(m: &Manifest, i: &Installed) -> Vec<String> {
    let own = |p: &Option<String>| {
        p.clone()
            .or_else(|| m.process_name.clone())
            .unwrap_or_else(|| m.name.clone())
    };
    let none = |v: &Option<String>| v.clone().unwrap_or_else(|| "none".into());
    let mut left = BTreeSet::new();
    left.insert(format!("package {} {}", m.name, m.version_code));
    left.insert(format!("version-name {}", none(&m.version_name)));
    left.insert(format!("sdk {} {}", m.min_sdk, m.target_sdk));
    left.insert(format!("app-class {}", none(&m.application_class)));
    left.insert(format!("app-process {}", own(&None)));
    left.insert(format!("app-theme {:#x}", m.application_theme));
    left.extend(
        m.requested_permissions
            .iter()
            .map(|p| format!("uses-permission {p}")),
    );
    left.extend(
        m.permissions
            .iter()
            .map(|p| format!("permission {} {:#x}", p.name, p.protection_level)),
    );
    left.insert(format!(
        "launcher {}",
        m.launch_activity().map_or("none", |a| a.name.as_str())
    ));
    for a in &m.activities {
        left.insert(format!(
            "activity {} theme={:#x} launch-mode={} config-changes={:#x} orientation={} process={}",
            a.name,
            a.theme,
            a.launch_mode,
            a.config_changes,
            a.screen_orientation,
            own(&a.process)
        ));
    }
    for r in &m.receivers {
        left.insert(format!(
            "receiver {} exported={} enabled={} permission={} process={}",
            r.name,
            r.exported,
            r.enabled,
            none(&r.permission),
            own(&r.process)
        ));
    }
    for p in &m.providers {
        left.insert(format!(
            "provider {} authority={} exported={} enabled={} process={} read={} write={}",
            p.name,
            none(&p.authority),
            p.exported,
            p.enabled,
            own(&p.process),
            none(&p.read_permission),
            none(&p.write_permission)
        ));
    }
    let pkg = &i.package;
    let mut right = BTreeSet::new();
    right.insert(format!(
        "package {} {}",
        none(&pkg.package_name),
        (i64::from(pkg.version_code_major) << 32) | i64::from(pkg.version_code as u32)
    ));
    right.insert(format!("version-name {}", none(&pkg.version_name)));
    if let Some(app) = pkg.application_info.as_ref() {
        right.insert(format!(
            "sdk {} {}",
            app.min_sdk_version, app.target_sdk_version
        ));
        right.insert(format!("app-class {}", none(&app.class_name)));
        right.insert(format!("app-process {}", none(&app.process_name)));
        right.insert(format!("app-theme {:#x}", app.theme));
    }
    right.extend(
        pkg.requested_permissions
            .iter()
            .flatten()
            .flatten()
            .map(|p| format!("uses-permission {p}")),
    );
    right.extend(pkg.permissions.iter().flatten().flatten().map(|p| {
        format!(
            "permission {} {:#x}",
            none(&p.item.name),
            p.protection_level
        )
    }));
    let launches = |c: &ComponentIntents| {
        c.kind == Kind::ActivityIntents
            && c.intents
                .iter()
                .filter_map(|x| x.intent_filter.as_ref())
                .any(|f| {
                    f.actions
                        .iter()
                        .flatten()
                        .any(|a| a == "android.intent.action.MAIN")
                        && f.categories
                            .iter()
                            .flatten()
                            .flatten()
                            .any(|c| c == "android.intent.category.LAUNCHER")
                })
    };
    let launcher = pkg.activities.iter().flatten().flatten().find_map(|a| {
        let name = a.component.item.name.as_deref()?;
        i.intents
            .iter()
            .any(|c| c.name == name && launches(c))
            .then(|| name.to_owned())
    });
    right.insert(format!(
        "launcher {}",
        launcher.as_deref().unwrap_or("none")
    ));
    for a in pkg.activities.iter().flatten().flatten() {
        let c = &a.component;
        right.insert(format!(
            "activity {} theme={:#x} launch-mode={} config-changes={:#x} orientation={} process={}",
            none(&c.item.name),
            a.theme,
            a.launch_mode,
            a.config_changes,
            a.screen_orientation,
            none(&c.process_name)
        ));
    }
    for a in pkg.receivers.iter().flatten().flatten() {
        let c = &a.component;
        right.insert(format!(
            "receiver {} exported={} enabled={} permission={} process={}",
            none(&c.item.name),
            c.exported,
            c.enabled,
            none(&a.permission),
            none(&c.process_name)
        ));
    }
    for p in pkg.providers.iter().flatten().flatten() {
        let c = &p.component;
        right.insert(format!(
            "provider {} authority={} exported={} enabled={} process={} read={} write={}",
            none(&c.item.name),
            none(&p.authority),
            c.exported,
            c.enabled,
            none(&c.process_name),
            none(&p.read_permission),
            none(&p.write_permission)
        ));
    }
    left.difference(&right)
        .map(|l| format!("aapt2 only: {l}"))
        .chain(
            right
                .difference(&left)
                .map(|l| format!("installer only: {l}")),
        )
        .collect()
}
