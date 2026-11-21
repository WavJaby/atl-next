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
    /// SurfaceFlinger (`private/surfaceflinger.te`: the only domain that may add `surfaceflinger_service`). atld starts none (each app process runs its own compositor, registered with
    /// atl.compositor_registry); the compositor gate's driver registers the stand-in SurfaceFlinger names libgui waits for, so it runs here.
    pub const SURFACEFLINGER: &str = "u:r:surfaceflinger:s0";
    /// Every spawn domain above: the table tool queries each besides the seapp domains.
    pub const SPAWNED: &[&str] = &[
        SERVICEMANAGER,
        SYSTEM_SERVER,
        GRAPHICS_ALLOCATOR,
        KEYMINT,
        KEYSTORE,
        HEALTH,
        SHELL,
        SURFACEFLINGER,
    ];
    /// plat_seapp_contexts' domain of a system uid package process (`user=system seinfo=platform domain=system_app`); a seapp domain, so not in [`SPAWNED`].
    pub const SYSTEM_APP: &str = "u:r:system_app:s0";
    /// plat_seapp_contexts' domain of an app uid at the current target SDK (`user=_app ... domain=untrusted_app`), without the per-app categories (the service access table is type-level);
    /// a seapp domain, so not in [`SPAWNED`].
    pub const UNTRUSTED_APP: &str = "u:r:untrusted_app:s0";
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

/// libselinux android_seapp.c `enum levelFrom`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum LevelFrom {
    #[default]
    None,
    App,
    User,
    All,
}

/// A user= or name= selector: a trailing `*` makes it a prefix (android_seapp.c `prefix_str`), compared case-insensitively.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Selector {
    text: String,
    prefix: bool,
}

impl Selector {
    fn new(v: &str) -> Selector {
        match v.strip_suffix('*') {
            Some(p) => Selector {
                text: p.to_owned(),
                prefix: true,
            },
            None => Selector {
                text: v.to_owned(),
                prefix: false,
            },
        }
    }

    fn matches(&self, s: &str) -> bool {
        if self.prefix {
            s.len() >= self.text.len()
                && s.as_bytes()[..self.text.len()].eq_ignore_ascii_case(self.text.as_bytes())
        } else {
            s.eq_ignore_ascii_case(&self.text)
        }
    }
}

/// One rule of a seapp_contexts file (android_seapp.c `struct seapp_context`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct SeappRule {
    is_system_server: bool,
    is_ephemeral_app: Option<bool>,
    user: Option<Selector>,
    seinfo: Option<String>,
    name: Option<Selector>,
    domain: Option<String>,
    level_from: LevelFrom,
    level: Option<String>,
    is_priv_app: Option<bool>,
    min_target_sdk: i32,
    from_run_as: bool,
    is_isolated_compute_app: bool,
    is_sdk_sandbox_audit: bool,
    is_sdk_sandbox_next: bool,
}

/// The facts ATL's installer has about an app process: zygote's `selinux_android_setcontext(uid, isSystemServer=false, seinfo, pkgname)` inputs. seinfo is always `default` (ATL holds no platform
/// key, so no app is platform-signed) with no privileged / ephemeral / isolated-compute / sdk-sandbox / fromRunAs flag: a rule needing one of them never matches.
#[derive(Debug, Clone, Copy)]
pub struct AppFacts<'a> {
    pub uid: u32,
    pub package: &'a str,
    pub target_sdk: i32,
}

/// android_filesystem_config.h: AID_USER_OFFSET, AID_APP_START, AID_SDK_SANDBOX_PROCESS_START, AID_ISOLATED_START; android_seapp.c CAT_MAPPING_MAX_ID.
const AID_USER_OFFSET: u32 = 100_000;
const AID_APP_START: u32 = 10_000;
const AID_SDK_SANDBOX_PROCESS_START: u32 = 20_000;
const AID_ISOLATED_START: u32 = 90_000;
const CAT_MAPPING_MAX_ID: u32 = 0x1 << 16;

/// A parsed seapp_contexts file in libselinux's precedence order: what `seapp_context_lookup` (android_seapp.c, pinned external/selinux) walks for an app's domain.
#[derive(Debug, Clone)]
pub struct Seapp {
    rules: Vec<SeappRule>,
}

impl Seapp {
    /// android_seapp.c `selinux_android_seapp_context_reload` over one (platform) file: `key=value` tokens per line, `#` comment lines; an unknown key or value is an error, as there.
    /// `neverallow` lines are checkseapp's input, not rules (a device's installed file has none): skipped.
    pub fn parse(text: &str) -> Result<Seapp, Error> {
        let mut rules = Vec::new();
        for (i, l) in text.lines().enumerate() {
            let err = |why: String| Error::Contexts { line: i + 1, why };
            let l = l.trim_start();
            if l.is_empty() || l.starts_with('#') {
                continue;
            }
            let mut tokens = l.split([' ', '\t']).filter(|t| !t.is_empty()).peekable();
            if tokens.peek() == Some(&"neverallow") {
                continue;
            }
            let mut r = SeappRule::default();
            for t in tokens {
                let (k, v) = t
                    .split_once('=')
                    .ok_or_else(|| err(format!("not key=value: {t}")))?;
                let flag = |v: &str| match v.to_ascii_lowercase().as_str() {
                    "true" => Ok(true),
                    "false" => Ok(false),
                    _ => Err(err(format!("{k}: not true/false: {v}"))),
                };
                match k.to_ascii_lowercase().as_str() {
                    "issystemserver" => r.is_system_server = flag(v)?,
                    "isephemeralapp" => r.is_ephemeral_app = Some(flag(v)?),
                    "user" => r.user = Some(Selector::new(v)),
                    "seinfo" => r.seinfo = Some(v.to_owned()),
                    "name" => r.name = Some(Selector::new(v)),
                    "domain" => r.domain = Some(v.to_owned()),
                    "type" => {}
                    "levelfrom" => {
                        r.level_from = match v.to_ascii_lowercase().as_str() {
                            "none" => LevelFrom::None,
                            "app" => LevelFrom::App,
                            "user" => LevelFrom::User,
                            "all" => LevelFrom::All,
                            _ => return Err(err(format!("levelFrom: {v}"))),
                        }
                    }
                    "level" => r.level = Some(v.to_owned()),
                    "isprivapp" => r.is_priv_app = Some(flag(v)?),
                    "mintargetsdkversion" => {
                        r.min_target_sdk = v
                            .parse::<i32>()
                            .ok()
                            .filter(|n| *n >= 0)
                            .ok_or_else(|| err(format!("minTargetSdkVersion: {v}")))?
                    }
                    "fromrunas" => r.from_run_as = flag(v)?,
                    "isisolatedcomputeapp" => r.is_isolated_compute_app = flag(v)?,
                    "issdksandboxaudit" => r.is_sdk_sandbox_audit = flag(v)?,
                    "issdksandboxnext" => r.is_sdk_sandbox_next = flag(v)?,
                    _ => return Err(err(format!("unknown key {k}"))),
                }
            }
            rules.push(r);
        }
        // qsort with seapp_context_cmp; a stable sort keeps file order among equals (libselinux rejects equal-precedence rules that would select differently)
        rules.sort_by(seapp_cmp);
        Ok(Seapp { rules })
    }

    /// The app process's context (`seapp_context_lookup_internal`, kind SEAPP_DOMAIN, user `u`, role `r`, level `s0` before the rule's): None when no rule gives a domain (libselinux fails the spawn
    /// then: "Fail to prevent staying in the zygote's context") or the uid is no app uid.
    pub fn app_context(&self, app: AppFacts<'_>) -> Option<String> {
        let userid = app.uid / AID_USER_OFFSET;
        let appid = app.uid % AID_USER_OFFSET;
        // a uid below AID_APP_START is named by getpwuid there: no app process has one
        let (username, appid) = if appid < AID_APP_START {
            return None;
        } else if appid < AID_SDK_SANDBOX_PROCESS_START {
            ("_app", appid - AID_APP_START)
        } else if appid < AID_ISOLATED_START {
            ("_sdksandbox", appid - AID_SDK_SANDBOX_PROCESS_START)
        } else {
            ("_isolated", appid - AID_ISOLATED_START)
        };
        if appid >= CAT_MAPPING_MAX_ID || userid >= CAT_MAPPING_MAX_ID {
            return None;
        }
        let r = self.rules.iter().find(|r| {
            !r.is_system_server
                && r.is_ephemeral_app != Some(true)
                && r.user.as_ref().is_none_or(|u| u.matches(username))
                && r.seinfo
                    .as_ref()
                    .is_none_or(|s| s.eq_ignore_ascii_case("default"))
                && r.name.as_ref().is_none_or(|n| n.matches(app.package))
                && r.is_priv_app != Some(true)
                && r.min_target_sdk <= app.target_sdk
                && !r.from_run_as
                && !r.is_isolated_compute_app
                && !r.is_sdk_sandbox_audit
                && !r.is_sdk_sandbox_next
                && r.domain.is_some()
        })?;
        let level = match r.level_from {
            LevelFrom::None => r.level.clone().unwrap_or_else(|| "s0".to_owned()),
            LevelFrom::App => format!("s0:c{},c{}", appid & 0xff, 256 + (appid >> 8 & 0xff)),
            LevelFrom::User => format!(
                "s0:c{},c{}",
                512 + (userid & 0xff),
                768 + (userid >> 8 & 0xff)
            ),
            LevelFrom::All => format!(
                "s0:c{},c{},c{},c{}",
                appid & 0xff,
                256 + (appid >> 8 & 0xff),
                512 + (userid & 0xff),
                768 + (userid >> 8 & 0xff)
            ),
        };
        Some(format!("u:r:{}:{level}", r.domain.as_deref()?))
    }
}

/// android_seapp.c `seapp_context_cmp` (one platform file: the partition step never decides).
fn seapp_cmp(s1: &SeappRule, s2: &SeappRule) -> std::cmp::Ordering {
    use std::cmp::Ordering::{Equal, Greater, Less};
    // true first
    let first = |a: bool, b: bool| match (a, b) {
        (true, false) => Less,
        (false, true) => Greater,
        _ => Equal,
    };
    // a fixed string before a prefix, a longer prefix before a shorter
    let sel = |a: &Option<Selector>, b: &Option<Selector>| match (a, b) {
        (Some(a), Some(b)) => first(!a.prefix, !b.prefix).then(if a.prefix && b.prefix {
            b.text.len().cmp(&a.text.len())
        } else {
            Equal
        }),
        _ => first(a.is_some(), b.is_some()),
    };
    first(s1.is_system_server, s2.is_system_server)
        .then(first(
            s1.is_ephemeral_app.is_some(),
            s2.is_ephemeral_app.is_some(),
        ))
        .then(sel(&s1.user, &s2.user))
        .then(first(s1.seinfo.is_some(), s2.seinfo.is_some()))
        .then(sel(&s1.name, &s2.name))
        .then(first(s1.is_priv_app.is_some(), s2.is_priv_app.is_some()))
        .then(s2.min_target_sdk.cmp(&s1.min_target_sdk))
        .then(first(s1.from_run_as, s2.from_run_as))
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

    /// private/seapp_contexts at the pin, lines 174-224 minus the vendor-only and duplicate privileged rows; the rule order is reversed so the result rests on seapp_context_cmp, not file order.
    const SEAPP: &str = "\
neverallow user=_app domain=((?!x).)*
isSystemServer=true domain=system_server_startup
user=_app seinfo=platform name=com.android.traceur domain=traceur_app type=app_data_file levelFrom=all
user=_isolated domain=isolated_app levelFrom=user
user=_isolated isIsolatedComputeApp=true domain=isolated_compute_app levelFrom=user
user=_app seinfo=platform domain=platform_app type=app_data_file levelFrom=user
user=_app isEphemeralApp=true domain=ephemeral_app type=app_data_file levelFrom=all
user=_app isPrivApp=true domain=priv_app type=privapp_data_file levelFrom=user
user=_app isPrivApp=true name=com.android.rkpdapp domain=rkpdapp type=privapp_data_file levelFrom=all
user=_app minTargetSdkVersion=34 domain=untrusted_app type=app_data_file levelFrom=all
user=_app minTargetSdkVersion=32 domain=untrusted_app_32 type=app_data_file levelFrom=all
user=_app minTargetSdkVersion=30 domain=untrusted_app_30 type=app_data_file levelFrom=all
user=_app minTargetSdkVersion=29 domain=untrusted_app_29 type=app_data_file levelFrom=all
user=_app minTargetSdkVersion=28 domain=untrusted_app_27 type=app_data_file levelFrom=all
user=_app minTargetSdkVersion=26 domain=untrusted_app_27 type=app_data_file levelFrom=user
user=_app domain=untrusted_app_25 type=app_data_file levelFrom=user
user=_app minTargetSdkVersion=28 fromRunAs=true domain=runas_app levelFrom=all
user=_app fromRunAs=true domain=runas_app levelFrom=user
";

    fn reversed(text: &str) -> String {
        text.lines().rev().map(|l| format!("{l}\n")).collect()
    }

    /// Expected contexts: the domain from the rule seapp_context_cmp ranks first among those matching, the level from set_range_from_level (all: c<appid&0xff>,c<256+(appid>>8&0xff)>,
    /// c<512+(user&0xff)>,c<768+(user>>8&0xff)>; user: the last two), appid = uid % 100000 - 10000 (_app) or - 90000 (_isolated).
    #[test]
    fn seapp_selects_as_libselinux() {
        let s = Seapp::parse(&reversed(SEAPP)).expect("parses");
        let ctx = |uid, target_sdk| {
            s.app_context(AppFacts {
                uid,
                package: "org.example",
                target_sdk,
            })
        };
        let c = |s: &str| Some(s.to_owned());
        assert_eq!(
            ctx(10_123, 36),
            c("u:r:untrusted_app:s0:c123,c256,c512,c768")
        );
        assert_eq!(
            ctx(10_123, 34),
            c("u:r:untrusted_app:s0:c123,c256,c512,c768")
        );
        assert_eq!(
            ctx(10_123, 33),
            c("u:r:untrusted_app_32:s0:c123,c256,c512,c768")
        );
        assert_eq!(
            ctx(10_123, 28),
            c("u:r:untrusted_app_27:s0:c123,c256,c512,c768")
        );
        // minTargetSdkVersion=26: levelFrom=user
        assert_eq!(ctx(10_123, 27), c("u:r:untrusted_app_27:s0:c512,c768"));
        assert_eq!(ctx(10_123, 25), c("u:r:untrusted_app_25:s0:c512,c768"));
        // appid 300: 300 & 0xff = 44, 256 + 1; user 10: 512 + 10
        assert_eq!(
            ctx(1_010_300, 34),
            c("u:r:untrusted_app:s0:c44,c257,c522,c768")
        );
        assert_eq!(ctx(90_005, 34), c("u:r:isolated_app:s0:c512,c768"));
        // no app uid: getpwuid's names, never an app process
        assert_eq!(ctx(1000, 34), None);
        // name= rules need seinfo=platform or isPrivApp: never an ATL app
        let traceur = s.app_context(AppFacts {
            uid: 10_123,
            package: "com.android.traceur",
            target_sdk: 34,
        });
        assert_eq!(traceur, c("u:r:untrusted_app:s0:c123,c256,c512,c768"));
        // a file without a rule for plain apps: no domain (libselinux fails the spawn)
        let only_priv = Seapp::parse("user=_app isPrivApp=true domain=priv_app levelFrom=user\n")
            .expect("parses");
        assert_eq!(
            only_priv.app_context(AppFacts {
                uid: 10_123,
                package: "p",
                target_sdk: 34
            }),
            None
        );
    }

    #[test]
    fn seapp_refuses_what_libselinux_refuses() {
        assert!(Seapp::parse("user=_app domain\n").is_err());
        assert!(Seapp::parse("user=_app isPrivApp=maybe\n").is_err());
        assert!(Seapp::parse("user=_app colour=blue\n").is_err());
        assert!(Seapp::parse("user=_app minTargetSdkVersion=-1\n").is_err());
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
