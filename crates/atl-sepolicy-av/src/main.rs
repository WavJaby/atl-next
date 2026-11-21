// SPDX-License-Identifier: MIT
//! Writes the service access table (crate atl-sepolicy) from a compiled binary policy: libsepol's `sepol_compute_av` for every source domain (atl-sepolicy `domains::SPAWNED` plus every
//! `domain=` of the seapp contexts) x every type the service contexts files name x each class of `atl_sepolicy::CLASSES`. Run at build time by the converter goal `atl-sepolicy` (tools/bp2ninja
//! sepolicy.go); the output is a deterministic function of its inputs (sorted rows, no paths or times in it). Fails on a context type or a class / permission the policy lacks, so a typo in
//! an ATL row stops the build rather than denying at run time.
//!
//! Default: mutating (writes --out)

use std::collections::BTreeSet;
use std::ffi::CString;
use std::path::PathBuf;

use anyhow::{Context, Result, bail, ensure};
use atl_sepolicy::{CLASSES, Table, context_type, domains, parse_service_contexts, seapp_domains};
use clap::Parser;

#[allow(non_camel_case_types, non_upper_case_globals, dead_code)]
mod sys {
    include!(concat!(env!("OUT_DIR"), "/sepol.rs"));
}

#[derive(Parser)]
#[command(about, long_about = None)]
struct Args {
    /// The binary policy (secilc output).
    #[arg(long)]
    policy: PathBuf,
    /// A service_contexts file (repeat): its types are the table's targets.
    #[arg(long = "service-contexts", required = true)]
    service_contexts: Vec<PathBuf>,
    /// The seapp_contexts file: its `domain=` values join the spawned domains as sources.
    #[arg(long = "seapp-contexts")]
    seapp_contexts: PathBuf,
    /// The table file to write.
    #[arg(long)]
    out: PathBuf,
}

/// The loaded policy: libsepol keeps one global policydb, so one value of this type per process.
struct Policy;

impl Policy {
    fn load(path: &std::path::Path) -> Result<Policy> {
        let c =
            CString::new(path.as_os_str().as_encoded_bytes()).context("policy path holds a NUL")?;
        // SAFETY: both arguments are NUL-terminated C strings that outlive the call.
        let fp = unsafe { libc::fopen(c.as_ptr(), c"r".as_ptr()) };
        ensure!(
            !fp.is_null(),
            "opening {}: {}",
            path.display(),
            std::io::Error::last_os_error()
        );
        // SAFETY: fp is an open FILE*; libsepol reads it and keeps the policydb in its own global state.
        let rc = unsafe { sys::sepol_set_policydb_from_file(fp) };
        // SAFETY: fp came from fopen above and is closed once.
        unsafe { libc::fclose(fp) };
        ensure!(rc == 0, "libsepol could not load {}", path.display());
        Ok(Policy)
    }

    fn sid(&self, context: &str) -> Result<u32> {
        let c = CString::new(context)?;
        let mut sid = 0;
        // SAFETY: c is a valid buffer of the given length; sid is a valid out pointer.
        let rc = unsafe { sys::sepol_context_to_sid(c.as_ptr(), c.as_bytes().len() + 1, &mut sid) };
        ensure!(rc == 0, "the policy has no context {context}");
        Ok(sid)
    }

    fn class(&self, name: &str) -> Result<u16> {
        let c = CString::new(name)?;
        let mut class = 0;
        // SAFETY: c is NUL-terminated; class is a valid out pointer.
        let rc = unsafe { sys::sepol_string_to_security_class(c.as_ptr(), &mut class) };
        ensure!(rc == 0, "the policy has no class {name}");
        Ok(class)
    }

    fn perm(&self, class: u16, name: &str) -> Result<u32> {
        let c = CString::new(name)?;
        let mut av = 0;
        // SAFETY: c is NUL-terminated; av is a valid out pointer.
        let rc = unsafe { sys::sepol_string_to_av_perm(class, c.as_ptr(), &mut av) };
        ensure!(rc == 0, "class {class} has no permission {name}");
        Ok(av)
    }

    fn allowed(&self, ssid: u32, tsid: u32, class: u16, requested: u32) -> Result<u32> {
        let mut avd = sys::sepol_av_decision {
            allowed: 0,
            decided: 0,
            auditallow: 0,
            auditdeny: 0,
            seqno: 0,
        };
        // SAFETY: the sids and class came from this policy; avd is a valid out pointer.
        let rc = unsafe { sys::sepol_compute_av(ssid, tsid, class, requested, &mut avd) };
        ensure!(rc == 0, "sepol_compute_av failed");
        Ok(avd.allowed & requested)
    }
}

fn main() -> Result<()> {
    let a = Args::parse();
    let policy = Policy::load(&a.policy)?;
    let mut sources: BTreeSet<String> = domains::SPAWNED
        .iter()
        .map(|c| {
            context_type(c)
                .expect("spawn contexts are contexts")
                .to_owned()
        })
        .collect();
    sources.extend(seapp_domains(
        &std::fs::read_to_string(&a.seapp_contexts)
            .with_context(|| format!("reading {}", a.seapp_contexts.display()))?,
    ));
    let mut targets = BTreeSet::new();
    for f in &a.service_contexts {
        let text =
            std::fs::read_to_string(f).with_context(|| format!("reading {}", f.display()))?;
        for row in parse_service_contexts(&text).with_context(|| f.display().to_string())? {
            targets.insert(
                context_type(&row.context)
                    .expect("parse checked it")
                    .to_owned(),
            );
        }
    }
    if targets.is_empty() {
        bail!("no service types in {:?}", a.service_contexts);
    }
    // servicemanager's own context is the target of `list` (ServiceManager.cpp canList: selinux_check_access(.., mThisProcessContext, "service_manager", "list"))
    targets.insert(
        context_type(domains::SERVICEMANAGER)
            .expect("a context")
            .to_owned(),
    );
    let mut table = Table::new(sources.clone(), targets.clone());
    let mut queries = 0u64;
    for (class_name, perms) in CLASSES {
        let class = policy.class(class_name)?;
        let bits: Vec<(&str, u32)> = perms
            .iter()
            .map(|p| Ok((*p, policy.perm(class, p)?)))
            .collect::<Result<_>>()?;
        let all = bits.iter().fold(0, |m, (_, b)| m | b);
        for s in &sources {
            let ssid = policy.sid(&format!("u:r:{s}:s0"))?;
            for t in &targets {
                let tsid = policy.sid(&format!("u:object_r:{t}:s0"))?;
                let allowed = policy.allowed(ssid, tsid, class, all)?;
                queries += 1;
                for (p, b) in &bits {
                    if allowed & b != 0 {
                        table.grant(s, t, class_name, p);
                    }
                }
            }
        }
    }
    let header = format!(
        "generated by atl-sepolicy-av from the compiled policy and its context files, do not edit\n{} domains x {} types, {queries} queries",
        sources.len(),
        targets.len()
    );
    std::fs::write(&a.out, table.to_text(&header))
        .with_context(|| format!("writing {}", a.out.display()))?;
    Ok(())
}
