// SPDX-License-Identifier: MIT
//! The services' install (owner option I, 2026-11-13): PackageManagerService's half. The host does the file work (atld: staging, the installer run, the rename, the data dirs; installd's role) over the
//! control socket; here the installer's frames are decoded ([`crate::installed`]) into the package's record: PMS's per-install fields (uid, code and data paths) set on its PackageInfo, its instruction
//! set from `primaryCpuAbi`, the GL driver from the owner table and the host's ES1 fact, and the install-time grants over every definition (PermissionManagerService).
//! All or nothing: any session that fails (installer exit, decode, commit) fails the install, and no package is registered.

use anyhow::{Context, Result, ensure};
use atl_android_paths::{DATA_APP, DATA_USER, DATA_USER_DE};
use atl_services_types::gl_driver;
use atl_services_types::native_libs::{SupportedAbi, instruction_set};
use atl_services_types::packages::PermissionDecl;

use crate::activity::log;
use crate::packages::{InstallFields, PackageRecord};
use crate::process_control::ControlSocket;

/// What the install takes besides the session ids.
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

/// Installs every session the host holds files for, in order (PMS's boot scan of installed packages): the records the services answer from.
pub fn install(
    control: &ControlSocket,
    sessions: &[u32],
    c: &Config,
) -> Result<Vec<PackageRecord>> {
    ensure!(!c.abis.is_empty(), "no device ABI");
    let owner = gl_driver::owner_table(gl_driver::OWNER_TABLE).map_err(anyhow::Error::msg)?;
    let mut records: Vec<PackageRecord> = Vec::new();
    for (i, &session) in sessions.iter().enumerate() {
        let uid = c.first_uid + u32::try_from(i)?;
        let frames = control
            .install(session, uid)
            .with_context(|| format!("installing session {session}"))?;
        let bytes = std::fs::read(&frames).with_context(|| format!("reading {frames}"))?;
        let installed = crate::installed::decode(&bytes)
            .map_err(|e| anyhow::anyhow!("session {session}: {frames}: {e}"))?;
        let mut info = installed.package;
        let name = info
            .package_name
            .clone()
            .with_context(|| format!("session {session}: no package name"))?;
        ensure!(
            !records.iter().any(|r| r.name() == name),
            "two packages named {name}"
        );
        let primary = info
            .application_info
            .as_ref()
            .and_then(|a| a.primary_cpu_abi.clone());
        let needs_es1 = control
            .commit(session, &name, uid)
            .with_context(|| format!("committing {name}"))?;
        let gl_driver = if c.no_gl_selection {
            gl_driver::GlDriver::Native
        } else {
            gl_driver::select(&owner, &name, needs_es1)
        };
        let isa = instruction_set(primary.as_deref(), &c.abis).to_owned();
        InstallFields {
            uid,
            code_dir: &format!("{DATA_APP}/{name}"),
            isa: &isa,
            data_dir: &format!("{DATA_USER}/0/{name}"),
            device_protected_data_dir: &format!("{DATA_USER_DE}/0/{name}"),
        }
        .apply(&mut info);
        log(format_args!(
            "install {name}: uid {uid}, primary-abi {}, isa {isa}",
            primary.as_deref().unwrap_or("none")
        ));
        records.push(PackageRecord {
            info,
            intents: installed.intents,
            libraries: installed.libraries,
            uid,
            instruction_set: isa,
            granted_permissions: Vec::new(),
            gl_driver,
        });
    }
    // install-time grants over every definition: the platform's first, then the packages' (permission::install_grants)
    let grants: Vec<Vec<String>> = records
        .iter()
        .map(|p| {
            let platform = c
                .platform_permissions
                .iter()
                .map(|d| ("android", d.name.as_str(), d.protection_level));
            let packages = records.iter().flat_map(|o| {
                o.permission_decls().filter_map(move |d| {
                    Some((o.name(), d.item.name.as_deref()?, d.protection_level as u32))
                })
            });
            atl_services_types::permission::install_grants(
                p.name(),
                p.requested_permissions(),
                platform.chain(packages),
            )
        })
        .collect();
    for (record, granted) in records.iter_mut().zip(grants) {
        record.granted_permissions = granted;
    }
    Ok(records)
}
