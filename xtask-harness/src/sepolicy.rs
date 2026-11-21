// SPDX-License-Identifier: MIT
//! `xtask test sepolicy`: the service access table the converter goal `atl-sepolicy` derived from the compiled pinned policy (tools/bp2ninja sepolicy.go, crate atl-sepolicy-av) answers as the policy's sources say.
//! - cited rows: each expected value is read from the pinned system/sepolicy source named beside it, never computed by the table code;
//! - registrations: every name atl-system-services registers (services.toml rows) is add-allowed for system_server, and every name a HAL / daemon atld starts registers is add-allowed for that daemon's
//!   domain (atl_sepolicy::domains), each name labelled through the built service_contexts files as libselinux does (exact name, else `*`);
//! - control, every run: the same checks over a copy of the table that grants untrusted_app find on hal_keymint_service must fail, so a check that never fires cannot pass.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use atl_sepolicy::{ServiceContext, Table, domains, parse_service_contexts};

/// The goal's outputs below the build dir (tools/bp2ninja sepolicy.go sepolicyOuts).
const GEN: &str = "gen/sepolicy";
const TABLE: &str = "service_access";
const CONTEXTS: [&str; 2] = ["plat_service_contexts", "system_ext_service_contexts"];
const APP: &str = "u:r:untrusted_app:s0:c512,c768";
const SM: &str = "service_manager";

/// (scontext, service name or context, permission, allowed, source of the expectation in system/sepolicy at the pin).
const CITED: &[(&str, &str, &str, bool, &str)] = &[
    (
        APP,
        "android.hardware.security.keymint.IKeyMintDevice/default",
        "find",
        false,
        "private/app_neverallows.te:254 (protected_service), public/service.te:336",
    ),
    (
        domains::KEYSTORE,
        "android.hardware.security.keymint.IKeyMintDevice/default",
        "find",
        true,
        "private/keystore.te:12 hal_client_domain(keystore, hal_keymint), te_macros:771",
    ),
    (
        APP,
        "activity",
        "find",
        true,
        "private/untrusted_app_all.te:99 app_api_service",
    ),
    (
        APP,
        "android.hardware.graphics.allocator.IAllocator/default",
        "find",
        true,
        "private/technical_debt.cil hal_graphics_allocator_client",
    ),
    (
        APP,
        "u:r:servicemanager:s0",
        "list",
        true,
        "private/untrusted_app_all.te:88",
    ),
    (
        domains::SYSTEM_SERVER,
        "activity",
        "add",
        true,
        "private/system_server.te:1004 add_service(system_server, system_server_service)",
    ),
    (
        domains::KEYMINT,
        "android.hardware.security.keymint.IKeyMintDevice/default",
        "add",
        true,
        "vendor/hal_keymint_default.te:2 hal_server_domain(hal_keymint_default, hal_keymint), public/hal_keymint.te hal_attribute_service",
    ),
    (
        APP,
        "atl.compositor_registry",
        "find",
        true,
        "repo java/sepolicy/atl.te app_api_service",
    ),
];

/// Names each non-services daemon atld starts registers, with its domain: (domain, names, where the daemon's source adds them).
const DAEMON_NAMES: &[(&str, &[&str], &str)] = &[
    (
        domains::GRAPHICS_ALLOCATOR,
        &["android.hardware.graphics.allocator.IAllocator/default"],
        "crates/atl-gralloc-service allocator.rs add_service",
    ),
    (
        domains::HEALTH,
        &["android.hardware.health.IHealth/default"],
        "hardware/interfaces/health/aidl/default main.cpp",
    ),
    (
        domains::KEYMINT,
        &[
            "android.hardware.security.sharedsecret.ISharedSecret/default",
            "android.hardware.security.keymint.IKeyMintDevice/default",
            "android.hardware.security.keymint.IRemotelyProvisionedComponent/default",
            "android.hardware.security.secureclock.ISecureClock/default",
        ],
        "hardware/interfaces/security/keymint/aidl/default/main.rs:99-126",
    ),
    (
        domains::KEYSTORE,
        &[
            "android.system.keystore2.IKeystoreService/default",
            "android.security.apc",
            "android.security.authorization",
            "android.security.maintenance",
            "android.security.metrics",
            "android.security.legacykeystore",
        ],
        "system/security/keystore2/src/keystore2_main.rs:111-151",
    ),
];

#[derive(clap::Args)]
pub struct Args {
    /// The native build dir (default: this checkout's build dir).
    #[arg(long)]
    out: Option<PathBuf>,
    #[arg(long, default_value = "x86_64")]
    host: String,
}

pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    let out = crate::build_state::gate_out(root, args.out.as_deref(), &args.host, "sepolicy")?;
    let dir = out.join(GEN);
    let read = |f: &str| {
        std::fs::read_to_string(dir.join(f))
            .with_context(|| format!("reading {}", dir.join(f).display()))
    };
    let table = Table::parse(&read(TABLE)?)?;
    let mut contexts = Vec::new();
    for f in CONTEXTS {
        contexts.extend(parse_service_contexts(&read(f)?)?);
    }
    let services = services_names(root)?;
    let problems = checks(&table, &contexts, &services);
    for p in &problems {
        println!("sepolicy: {p}");
    }
    let mut broken = table.clone();
    broken.grant("untrusted_app", "hal_keymint_service", SM, "find");
    let control = checks(&broken, &contexts, &services);
    println!(
        "sepolicy: {} domains, {} types, {} cited rows, {} registered names checked",
        table.domains().len(),
        table.targets().len(),
        CITED.len(),
        services.len() + DAEMON_NAMES.iter().map(|d| d.1.len()).sum::<usize>()
    );
    if !problems.is_empty() {
        bail!("sepolicy: FAILED: {} problem(s)", problems.len());
    }
    if control.is_empty() {
        bail!(
            "sepolicy: FAILED control: a table granting untrusted_app find on hal_keymint_service passed"
        );
    }
    println!("sepolicy: ok (control: an app-findable KeyMint fails)");
    Ok(ExitCode::SUCCESS)
}

/// services.toml's row keys: the names atl-system-services registers (step `tables` checks them against its add_service calls).
fn services_names(root: &Path) -> Result<Vec<String>> {
    let text =
        std::fs::read_to_string(root.join("services.toml")).context("reading services.toml")?;
    let t: toml::Table = toml::from_str(&text).context("parsing services.toml")?;
    Ok(t.keys().cloned().collect())
}

/// The context of a service name: the exact row, else the `*` row (private/service_contexts ends with `*`).
fn label<'a>(contexts: &'a [ServiceContext], name: &str) -> Option<&'a str> {
    let exact = contexts.iter().find(|c| c.name == name);
    exact
        .or_else(|| contexts.iter().find(|c| c.name == "*"))
        .map(|c| c.context.as_str())
}

fn checks(table: &Table, contexts: &[ServiceContext], services: &[String]) -> Vec<String> {
    let mut p = Vec::new();
    let target = |name: &str| -> Option<String> {
        if name.starts_with("u:") {
            Some(name.to_owned())
        } else {
            label(contexts, name).map(str::to_owned)
        }
    };
    for (s, name, perm, want, why) in CITED {
        let Some(t) = target(name) else {
            p.push(format!("{name}: no context"));
            continue;
        };
        let got = table.check(s, &t, SM, perm);
        if got.allowed() != *want {
            p.push(format!(
                "{s} {perm} {name} ({t}): {got:?}, expected allowed={want} ({why})"
            ));
        }
    }
    // private/app_neverallows.te:36: no app domain adds any service
    let added: BTreeSet<&String> = table
        .targets()
        .iter()
        .filter(|t| {
            table
                .check(APP, &format!("u:object_r:{t}:s0"), SM, "add")
                .allowed()
        })
        .collect();
    if !added.is_empty() {
        p.push(format!(
            "untrusted_app may add {added:?} (private/app_neverallows.te:36)"
        ));
    }
    let mut registrations: Vec<(&str, &str, &str)> = services
        .iter()
        .map(|n| (domains::SYSTEM_SERVER, n.as_str(), "services.toml"))
        .collect();
    for (d, names, why) in DAEMON_NAMES {
        registrations.extend(names.iter().map(|n| (*d, *n, *why)));
    }
    for (d, name, why) in registrations {
        match label(contexts, name) {
            None => p.push(format!("{name}: no context ({why})")),
            Some(t) if !table.check(d, t, SM, "add").allowed() => p.push(format!("{d} may not add {name} ({t}; {why}): needs a row in java/sepolicy or a different spawn domain")),
            Some(_) => {}
        }
    }
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn label_takes_the_exact_row_then_star() {
        let c = parse_service_contexts(
            "activity u:object_r:activity_service:s0\n* u:object_r:default_android_service:s0\n",
        )
        .expect("parses");
        assert_eq!(
            label(&c, "activity"),
            Some("u:object_r:activity_service:s0")
        );
        assert_eq!(
            label(&c, "nope"),
            Some("u:object_r:default_android_service:s0")
        );
        assert_eq!(label(&c[..1], "nope"), None);
    }
}
