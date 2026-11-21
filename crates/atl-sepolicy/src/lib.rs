// SPDX-License-Identifier: MIT
//! The SELinux decisions ATL makes in the kernel's place (service-access plan D1-D3): the service access table the build derives from the compiled pinned policy (converter goal `atl-sepolicy`,
//! tool `atl-sepolicy-av`), read here; the context files' format; the AOSP domains ATL's spawns run in.
//! A decision is on the type fields of the two contexts only: AOSP's MLS policy (`private/mls`) constrains none of the classes the table holds, so the level never changes an answer.
//! The table holds the domains the build queried; an unknown source domain or target type is a denial the caller counts.
#![forbid(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet};

/// The classes and permissions the table holds, as the policy names them (`private/access_vectors` `class service_manager { add find list }`).
pub const CLASSES: &[(&str, &[&str])] = &[("service_manager", &["add", "find", "list"])];

/// AOSP domains of the processes atld starts, each as the process's context: init's `seclabel` / the domain its `.rc` executable transitions to on a device.
pub mod domains {
    /// `servicemanager.rc` (`private/servicemanager.te` init_daemon_domain).
    pub const SERVICEMANAGER: &str = "u:r:servicemanager:s0";
    /// atl-system-services in system_server's role.
    pub const SYSTEM_SERVER: &str = "u:r:system_server:s0";
    /// The graphics allocator HAL (`hal_graphics_allocator_default.te`).
    pub const GRAPHICS_ALLOCATOR: &str = "u:r:hal_graphics_allocator_default:s0";
    /// AOSP's software KeyMint HAL (`vendor/hal_keymint_default.te`).
    pub const KEYMINT: &str = "u:r:hal_keymint_default:s0";
    /// keystore2 (`private/keystore.te`).
    pub const KEYSTORE: &str = "u:r:keystore:s0";
    /// The default health HAL (`vendor/hal_health_default.te`).
    pub const HEALTH: &str = "u:r:hal_health_default:s0";
    /// atl-am and the shell's commands (`private/shell.te`).
    pub const SHELL: &str = "u:r:shell:s0";
    /// Every spawn domain above: the table tool queries each besides the seapp domains.
    pub const SPAWNED: &[&str] = &[
        SERVICEMANAGER,
        SYSTEM_SERVER,
        GRAPHICS_ALLOCATOR,
        KEYMINT,
        KEYSTORE,
        HEALTH,
        SHELL,
    ];
}

/// The type field of a context `user:role:type:level` (the level may hold more colons: `s0:c512,c768`).
pub fn context_type(context: &str) -> Option<&str> {
    let mut f = context.split(':');
    let (_, _, t) = (f.next()?, f.next()?, f.next()?);
    f.next()?;
    (!t.is_empty()).then_some(t)
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("service access table line {line}: {why}")]
    Table { line: usize, why: String },
    #[error("context file line {line}: {why}")]
    Contexts { line: usize, why: String },
}

/// What [`Table::check`] answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Deny,
    /// The source context names no domain the build queried, or is malformed: denied.
    UnknownSource,
    /// The target context names no type the build queried, or is malformed: denied.
    UnknownTarget,
}

impl Decision {
    pub fn allowed(self) -> bool {
        self == Decision::Allow
    }
}

/// The service access table: per source domain and target type, the allowed permissions of each class in [`CLASSES`].
/// Text form, sorted, so a pin bump diffs as rows: `# ...` comments, `domain <type>` per queried source, `target <type>` per queried target, `allow <src> <tgt> <class> <perm>...`.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Table {
    domains: BTreeSet<String>,
    targets: BTreeSet<String>,
    allow: BTreeMap<(String, String, String), BTreeSet<String>>,
}

impl Table {
    pub fn new(domains: BTreeSet<String>, targets: BTreeSet<String>) -> Table {
        Table {
            domains,
            targets,
            allow: BTreeMap::new(),
        }
    }

    /// Records `perm` of `class` as allowed from `src` to `tgt`.
    pub fn grant(&mut self, src: &str, tgt: &str, class: &str, perm: &str) {
        self.allow
            .entry((src.to_owned(), tgt.to_owned(), class.to_owned()))
            .or_default()
            .insert(perm.to_owned());
    }

    pub fn domains(&self) -> &BTreeSet<String> {
        &self.domains
    }

    pub fn targets(&self) -> &BTreeSet<String> {
        &self.targets
    }

    /// May `scontext` exercise `perm` of `class` on `tcontext`?
    pub fn check(&self, scontext: &str, tcontext: &str, class: &str, perm: &str) -> Decision {
        let Some(s) = context_type(scontext).filter(|s| self.domains.contains(*s)) else {
            return Decision::UnknownSource;
        };
        let Some(t) = context_type(tcontext).filter(|t| self.targets.contains(*t)) else {
            return Decision::UnknownTarget;
        };
        match self
            .allow
            .get(&(s.to_owned(), t.to_owned(), class.to_owned()))
        {
            Some(perms) if perms.contains(perm) => Decision::Allow,
            _ => Decision::Deny,
        }
    }

    pub fn to_text(&self, header: &str) -> String {
        let mut s = String::new();
        for l in header.lines() {
            s += &format!("# {l}\n");
        }
        for d in &self.domains {
            s += &format!("domain {d}\n");
        }
        for t in &self.targets {
            s += &format!("target {t}\n");
        }
        for ((src, tgt, class), perms) in &self.allow {
            let perms: Vec<&str> = perms.iter().map(String::as_str).collect();
            s += &format!("allow {src} {tgt} {class} {}\n", perms.join(" "));
        }
        s
    }

    pub fn parse(text: &str) -> Result<Table, Error> {
        let mut t = Table::default();
        for (i, l) in text.lines().enumerate() {
            let err = |why: &str| Error::Table {
                line: i + 1,
                why: why.to_owned(),
            };
            let w: Vec<&str> = l.split_whitespace().collect();
            match w.as_slice() {
                [] => {}
                [c, ..] if c.starts_with('#') => {}
                ["domain", d] => {
                    t.domains.insert((*d).to_owned());
                }
                ["target", d] => {
                    t.targets.insert((*d).to_owned());
                }
                ["allow", src, tgt, class, perms @ ..] if !perms.is_empty() => {
                    if !t.domains.contains(*src) || !t.targets.contains(*tgt) {
                        return Err(err("allow row before its domain / target row"));
                    }
                    for p in perms {
                        t.grant(src, tgt, class, p);
                    }
                }
                _ => return Err(err("not a table row")),
            }
        }
        Ok(t)
    }
}

/// One row of a `service_contexts` file (libselinux `label_backends_android.c`: `<name> <context>`, `#` comments).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceContext {
    pub name: String,
    pub context: String,
}

/// The rows of a `service_contexts` file, in file order.
pub fn parse_service_contexts(text: &str) -> Result<Vec<ServiceContext>, Error> {
    let mut out = Vec::new();
    for (i, l) in text.lines().enumerate() {
        let l = l.split('#').next().unwrap_or("").trim();
        if l.is_empty() {
            continue;
        }
        let w: Vec<&str> = l.split_whitespace().collect();
        let [name, context] = w.as_slice() else {
            return Err(Error::Contexts {
                line: i + 1,
                why: format!("not `<name> <context>`: {l}"),
            });
        };
        if context_type(context).is_none() {
            return Err(Error::Contexts {
                line: i + 1,
                why: format!("not a context: {context}"),
            });
        }
        out.push(ServiceContext {
            name: (*name).to_owned(),
            context: (*context).to_owned(),
        });
    }
    Ok(out)
}

/// The `domain=` values of a `seapp_contexts` file's rules (`private/seapp_contexts`: `key=value` selectors and outputs per line), sorted; `neverallow` lines (regex selectors, read only by checkseapp) are no rules.
pub fn seapp_domains(text: &str) -> BTreeSet<String> {
    text.lines()
        .map(|l| l.split('#').next().unwrap_or(""))
        .filter(|l| l.split_whitespace().next() != Some("neverallow"))
        .flat_map(str::split_whitespace)
        .filter_map(|kv| kv.strip_prefix("domain="))
        .map(str::to_owned)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_type_takes_the_third_field() {
        assert_eq!(
            context_type("u:r:untrusted_app:s0:c512,c768"),
            Some("untrusted_app")
        );
        assert_eq!(
            context_type("u:object_r:activity_service:s0"),
            Some("activity_service")
        );
        assert_eq!(context_type("untrusted_app"), None);
        assert_eq!(context_type("u:r::s0"), None);
    }

    fn table() -> Table {
        let mut t = Table::new(
            ["a".into(), "b".into()].into(),
            ["x".into(), "y".into()].into(),
        );
        t.grant("a", "x", "service_manager", "find");
        t.grant("a", "x", "service_manager", "add");
        t
    }

    #[test]
    fn check_answers_from_rows_and_denies_unknowns() {
        let t = table();
        assert_eq!(
            t.check("u:r:a:s0", "u:object_r:x:s0", "service_manager", "find"),
            Decision::Allow
        );
        assert_eq!(
            t.check(
                "u:r:a:s0:c1,c2",
                "u:object_r:x:s0",
                "service_manager",
                "add"
            ),
            Decision::Allow
        );
        assert_eq!(
            t.check("u:r:a:s0", "u:object_r:x:s0", "service_manager", "list"),
            Decision::Deny
        );
        assert_eq!(
            t.check("u:r:b:s0", "u:object_r:x:s0", "service_manager", "find"),
            Decision::Deny
        );
        assert_eq!(
            t.check("u:r:a:s0", "u:object_r:y:s0", "service_manager", "find"),
            Decision::Deny
        );
        assert_eq!(
            t.check("u:r:c:s0", "u:object_r:x:s0", "service_manager", "find"),
            Decision::UnknownSource
        );
        assert_eq!(
            t.check("", "u:object_r:x:s0", "service_manager", "find"),
            Decision::UnknownSource
        );
        assert_eq!(
            t.check("u:r:a:s0", "u:object_r:z:s0", "service_manager", "find"),
            Decision::UnknownTarget
        );
    }

    #[test]
    fn text_round_trips_and_is_sorted() {
        let t = table();
        let text = t.to_text("made by a test");
        assert_eq!(
            text,
            "# made by a test\ndomain a\ndomain b\ntarget x\ntarget y\nallow a x service_manager add find\n"
        );
        assert_eq!(Table::parse(&text).expect("parses"), t);
        assert!(Table::parse("allow a x service_manager find\n").is_err());
        assert!(Table::parse("domain a\ntarget x\nallow a x service_manager\n").is_err());
    }

    #[test]
    fn service_contexts_rows() {
        let rows = parse_service_contexts("# c\nactivity  u:object_r:activity_service:s0\n\n*  u:object_r:default_android_service:s0 # last\n")
            .expect("parses");
        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows[1],
            ServiceContext {
                name: "*".into(),
                context: "u:object_r:default_android_service:s0".into()
            }
        );
        assert!(parse_service_contexts("activity\n").is_err());
    }

    #[test]
    fn seapp_domain_values() {
        let d = seapp_domains(
            "# x\nneverallow user=_app domain=((?!x).)*\nuser=_app domain=untrusted_app type=app_data_file levelFrom=all\nuser=system domain=system_app\nuser=_app seinfo=platform\n",
        );
        assert_eq!(
            d,
            ["system_app".to_owned(), "untrusted_app".to_owned()].into()
        );
    }
}
