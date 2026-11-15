// SPDX-License-Identifier: Apache-2.0
//! Derived from the Android Open Source Project: frameworks/base/services/core/java/com/android/server/am/BroadcastController.java, BroadcastSkipPolicy.java, frameworks/base/core/java/android/content/IntentFilter.java,
//! frameworks/base/core/java/android/app/ActivityManager.java (checkComponentPermission).
//! Broadcast policy, P1 subset, no binder: runtime receivers (BroadcastController.mRegisteredReceivers / ReceiverList / BroadcastFilter), the sticky store (mStickyBroadcasts), intent-filter matching,
//! the register-time export flags, the per-receiver skip rules, and the per-process FIFO of deliveries waiting for a cold-started process to attach. Generic over the receiver handle `R`, the intent `I`
//! and the queued job `J`, so the binder side (activity.rs, broadcast_queue.rs) keeps its types and these rules are unit-tested without the converter.
//! Lives under the activity service's state lock; nothing here calls out.
//! Not modelled (P2 or later): ordered broadcasts, priorities, implicit manifest receivers (the sysconfig allow list applies there: BroadcastController.java:1100-1106 adds FLAG_RECEIVER_INCLUDE_BACKGROUND for
//! SystemConfig.getAllowImplicitBroadcasts actions, BroadcastSkipPolicy.disallowBackgroundStart:347-354 skips the rest at :241; an explicit-component broadcast never hits it), app ops, instant apps,
//! BroadcastOptions, multi-user beyond USER_ALL + user 0, the protected-broadcast list.

use std::collections::{BTreeMap, VecDeque};

use atl_services_types::permission;

use crate::pattern_matcher::{self, Pattern};

/// Context.RECEIVER_EXPORTED / RECEIVER_NOT_EXPORTED (Context.java:781, :795).
pub const RECEIVER_EXPORTED: i32 = 0x2;
pub const RECEIVER_NOT_EXPORTED: i32 = 0x4;
/// UserHandle.USER_ALL: a sticky or a registration for every user.
pub const USER_ALL: i32 = -1;
/// Intent.FLAG_RECEIVER_REGISTERED_ONLY (Intent.java:7582): no manifest receiver is resolved (BroadcastController.java:1516).
pub const FLAG_RECEIVER_REGISTERED_ONLY: i32 = 0x4000_0000;
/// ActivityManager.BROADCAST_STICKY_CANT_HAVE_PERMISSION.
pub const BROADCAST_STICKY_CANT_HAVE_PERMISSION: i32 = -1;
/// BroadcastController.DYNAMIC_RECEIVER_EXPLICIT_EXPORT_REQUIRED (:145-146): @EnabledSince(targetSdkVersion = UPSIDE_DOWN_CAKE), API 34.
const EXPLICIT_EXPORT_REQUIRED_SDK: i32 = 34;

/// IntentFilter.AuthorityEntry: `host` without the `*` of `wild` (the parcel keeps it stripped), `port` -1 = any port.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Authority {
    pub host: String,
    pub wild: bool,
    pub port: i32,
}

/// An intent filter reduced to what broadcast matching reads (a registered IntentFilter is converted into it): actions, categories, `<data>` schemes, scheme-specific parts, authorities, paths and MIME types.
/// None = the filter names none (IntentFilter's null lists), which matchData treats differently from an empty list.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Filter {
    pub actions: Vec<String>,
    pub categories: Option<Vec<String>>,
    pub schemes: Option<Vec<String>>,
    pub ssps: Option<Vec<Pattern>>,
    pub authorities: Option<Vec<Authority>>,
    pub paths: Option<Vec<Pattern>>,
    pub types: Option<Vec<String>>,
}

/// What matching and Intent.filterEquals read of an Intent. `resolved_type` is the caller's resolvedType (matching); filterEquals reads `r#type` (mType).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IntentKey {
    pub action: Option<String>,
    pub data: Option<String>,
    pub r#type: Option<String>,
    pub identifier: Option<String>,
    pub package: Option<String>,
    /// (package, class)
    pub component: Option<(String, String)>,
    /// sorted: filterEquals compares the category sets
    pub categories: Vec<String>,
    pub resolved_type: Option<String>,
}

impl IntentKey {
    /// Intent.filterEquals (Intent.java:11838-11849): action, data, type, identifier, package, component, categories.
    pub fn filter_equals(&self, o: &IntentKey) -> bool {
        (
            &self.action,
            &self.data,
            &self.r#type,
            &self.identifier,
            &self.package,
            &self.component,
            &self.categories,
        ) == (
            &o.action,
            &o.data,
            &o.r#type,
            &o.identifier,
            &o.package,
            &o.component,
            &o.categories,
        )
    }

    /// Uri.findSchemeSeparator, reduced: the first ':' when no '/', '?' or '#' precedes it. None = relative (no scheme).
    fn scheme_end(&self) -> Option<usize> {
        let data = self.data.as_deref()?;
        let end = data.find(':')?;
        (!data[..end].contains(['/', '?', '#'])).then_some(end)
    }

    /// Uri.getScheme of the data.
    fn scheme(&self) -> Option<&str> {
        let data = self.data.as_deref()?;
        self.scheme_end().map(|end| &data[..end])
    }

    /// Uri.getSchemeSpecificPart (decoded): from after the scheme separator up to '#'. None = no data.
    fn ssp(&self) -> Option<String> {
        let data = self.data.as_deref()?;
        let start = self.scheme_end().map_or(0, |end| end + 1);
        let end = data[start..].find('#').map_or(data.len(), |i| start + i);
        Some(uri_decode(&data[start..end]))
    }

    /// Uri.parseAuthority: the text after the `//` that follows the scheme separator, up to '/', '\', '?' or '#'. None = no authority.
    fn authority(&self) -> Option<&str> {
        let data = self.data.as_deref()?;
        let start = self.scheme_end().map_or(0, |end| end + 1);
        let after = data[start..].strip_prefix("//")?;
        Some(
            after
                .split(['/', '\\', '?', '#'])
                .next()
                .unwrap_or_default(),
        )
    }

    /// Uri.getHost (decoded): after the last '@', before the port.
    fn host(&self) -> Option<String> {
        let authority = self.authority()?;
        let start = authority.rfind('@').map_or(0, |i| i + 1);
        let end = port_separator(authority).unwrap_or(authority.len());
        Some(uri_decode(authority.get(start..end).unwrap_or_default()))
    }

    /// Uri.getPort: the digits after the port separator; -1 when there are none or they are not an int.
    fn port(&self) -> i32 {
        let Some(authority) = self.authority() else {
            return -1;
        };
        let Some(sep) = port_separator(authority) else {
            return -1;
        };
        authority[sep + 1..].parse().unwrap_or(-1)
    }

    /// Uri.getPath (decoded): the path after the authority, cut at '?' / '#'. None = opaque URI (no '/' after the scheme separator).
    fn path(&self) -> Option<String> {
        let data = self.data.as_deref()?;
        let rest = match self.scheme_end() {
            Some(end) if data[end + 1..].starts_with('/') => &data[end + 1..],
            Some(_) => return None,
            None => data,
        };
        // after the authority the path starts at its '/' or '\'; a '?' or '#' first leaves it empty
        let path = match rest.strip_prefix("//") {
            Some(after) => match after.find(['/', '\\', '?', '#']) {
                Some(i) if matches!(after.as_bytes()[i], b'/' | b'\\') => &after[i..],
                _ => "",
            },
            None => rest,
        };
        let end = path.find(['?', '#']).unwrap_or(path.len());
        Some(uri_decode(&path[..end]))
    }
}

/// IntentFilter.match(action, type, scheme, data, categories) >= 0 for the parts the filter models: actions, categories, schemes, scheme-specific parts, authorities, paths and types.
/// Relative-reference filter groups are not modelled (the codec refuses them).
/// matchAction:872-891 (a null action matches nothing), matchData:1742 (no types and no schemes: only a type-less, data-less intent; schemes: the intent's scheme, "" for none, then the data parts; no schemes: only
/// none / "" / content / file; types: the resolved type by findMimeType, else a typed intent fails), matchCategories:1904 (every intent category in the filter).
pub fn matches(f: &Filter, k: &IntentKey) -> bool {
    match_value(f, k).is_some()
}

/// IntentFilter.MATCH_CATEGORY_* and MATCH_ADJUSTMENT_NORMAL (IntentFilter.java:246-279).
pub const MATCH_CATEGORY_EMPTY: i32 = 0x0100000;
pub const MATCH_CATEGORY_SCHEME: i32 = 0x0200000;
pub const MATCH_CATEGORY_HOST: i32 = 0x0300000;
pub const MATCH_CATEGORY_PORT: i32 = 0x0400000;
pub const MATCH_CATEGORY_PATH: i32 = 0x0500000;
pub const MATCH_CATEGORY_SCHEME_SPECIFIC_PART: i32 = 0x0580000;
pub const MATCH_CATEGORY_TYPE: i32 = 0x0600000;
pub const MATCH_ADJUSTMENT_NORMAL: i32 = 0x8000;

/// IntentFilter.match's positive result (what ResolveInfo.match carries), None for any NO_MATCH_*; the rules [`matches`] documents. matchData:1747-1832 starts at MATCH_CATEGORY_EMPTY, a matched
/// scheme raises it to SCHEME, the data parts (scheme-specific part, host, port, path: [`data_category`]) raise it to theirs, a matched type to TYPE, then adds MATCH_ADJUSTMENT_NORMAL.
pub fn match_value(f: &Filter, k: &IntentKey) -> Option<i32> {
    let action = k.action.as_ref()?;
    if !f.actions.contains(action) {
        return None;
    }
    let scheme = k.scheme();
    let resolved = k.resolved_type.as_deref();
    let mut category = MATCH_CATEGORY_EMPTY;
    if f.types.is_none() && f.schemes.is_none() {
        if resolved.is_some() || k.data.is_some() {
            return None;
        }
    } else {
        match &f.schemes {
            Some(schemes) => {
                if !schemes.iter().any(|s| s == scheme.unwrap_or("")) {
                    return None;
                }
                category = data_category(f, k)?;
            }
            None => {
                if !matches!(scheme, None | Some("" | "content" | "file")) {
                    return None;
                }
            }
        }
        match (&f.types, resolved) {
            (Some(types), Some(t)) => {
                if !types.iter().any(|ft| mime_matches(ft, t)) {
                    return None;
                }
                category = MATCH_CATEGORY_TYPE;
            }
            (Some(_), None) | (None, Some(_)) => return None,
            (None, None) => {}
        }
    }
    let categories = match &f.categories {
        None => k.categories.is_empty(),
        Some(cats) => k.categories.iter().all(|c| cats.contains(c)),
    };
    categories.then_some(category + MATCH_ADJUSTMENT_NORMAL)
}

/// matchData:1754-1801 after the scheme check: the scheme-specific part, else authority then path, else SCHEME (the filter names none of them). None = no match.
fn data_category(f: &Filter, k: &IntentKey) -> Option<i32> {
    // a missed SSP still goes on to the authorities (AOSP keeps match = NO_MATCH_DATA, which a matching authority overwrites); it is a miss only without them
    let mut ssp_missed = false;
    if let (Some(ssps), Some(ssp)) = (&f.ssps, k.ssp()) {
        if ssps.iter().any(|p| pattern_matcher::matches(p, &ssp)) {
            return Some(MATCH_CATEGORY_SCHEME_SPECIFIC_PART);
        }
        ssp_missed = true;
    }
    let Some(authorities) = &f.authorities else {
        return (!ssp_missed).then_some(MATCH_CATEGORY_SCHEME);
    };
    let (host, port) = (k.host(), k.port());
    let category = authorities
        .iter()
        .find_map(|a| match_authority(a, host.as_deref(), port))?;
    let Some(paths) = &f.paths else {
        return Some(category);
    };
    let path = k.path()?;
    paths
        .iter()
        .any(|p| pattern_matcher::matches(p, &path))
        .then_some(MATCH_CATEGORY_PATH)
}

/// AuthorityEntry.match(data, false), IntentFilter.java:1263-1293: PORT when the entry names a port, else HOST. None = no match.
fn match_authority(a: &Authority, host: Option<&str>, port: i32) -> Option<i32> {
    let mut host = host?;
    if a.wild {
        // `*.example.com` keeps ".example.com": the host's tail of that many chars must equal it, so a shorter host cannot match
        let tail = host.chars().count().checked_sub(a.host.chars().count())?;
        let whole = host;
        host = whole
            .char_indices()
            .nth(tail)
            .map_or("", |(i, _)| &whole[i..]);
    }
    if !eq_ignore_case(host, &a.host) {
        return None;
    }
    if a.port >= 0 {
        return (a.port == port).then_some(MATCH_CATEGORY_PORT);
    }
    Some(MATCH_CATEGORY_HOST)
}

/// String.compareToIgnoreCase == 0, per char (Java's CaseInsensitiveComparator): equal, or equal once upper-cased, or once lower-cased after upper-casing.
fn eq_ignore_case(a: &str, b: &str) -> bool {
    a.chars().count() == b.chars().count() && a.chars().zip(b.chars()).all(|(x, y)| fold_eq(x, y))
}

fn fold_eq(x: char, y: char) -> bool {
    // Character.toUpperCase / toLowerCase: a single-char mapping, else the char itself
    fn single(mut mapped: impl Iterator<Item = char>, c: char) -> char {
        match (mapped.next(), mapped.next()) {
            (Some(m), None) => m,
            _ => c,
        }
    }
    let (ux, uy) = (single(x.to_uppercase(), x), single(y.to_uppercase(), y));
    x == y || ux == uy || single(ux.to_lowercase(), ux) == single(uy.to_lowercase(), uy)
}

/// Uri.findPortSeparator: the last ':' when only ASCII digits follow it, else None.
fn port_separator(authority: &str) -> Option<usize> {
    for (i, b) in authority.bytes().enumerate().rev() {
        if b == b':' {
            return Some(i);
        }
        if !b.is_ascii_digit() {
            return None;
        }
    }
    None
}

/// Uri.decode = UriCodec.decode(s, no '+' conversion, UTF-8, no throw): %XX bytes decoded as UTF-8, U+FFFD where AOSP writes it.
/// A bad digit writes U+FFFD and the byte built so far (AOSP breaks out of the loop, then puts it); a truncated escape writes only U+FFFD.
fn uri_decode(s: &str) -> String {
    fn flush(out: &mut String, pending: &mut Vec<u8>) {
        if !pending.is_empty() {
            out.push_str(&String::from_utf8_lossy(pending));
            pending.clear();
        }
    }
    let mut out = String::with_capacity(s.len());
    let mut pending = Vec::new();
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '%' {
            flush(&mut out, &mut pending);
            out.push(c);
            continue;
        }
        let mut byte = 0u8;
        for _ in 0..2 {
            let Some(d) = chars.next() else {
                flush(&mut out, &mut pending);
                out.push(char::REPLACEMENT_CHARACTER);
                return out;
            };
            let Some(v) = d.to_digit(16) else {
                flush(&mut out, &mut pending);
                out.push(char::REPLACEMENT_CHARACTER);
                break;
            };
            byte = byte * 16 + v as u8;
        }
        pending.push(byte);
    }
    flush(&mut out, &mut pending);
    out
}

/// IntentFilter.findMimeType, reduced: equal, `*/*` / `*`, or a partial type against `<base>/<sub>`. A partial type is `<base>/*` as the manifest writes it, or `<base>` as a parcelled
/// IntentFilter holds it (addDataType stores `image/*` as `image` with mHasPartialTypes).
fn mime_matches(filter_type: &str, t: &str) -> bool {
    if filter_type == t || filter_type == "*/*" || filter_type == "*" {
        return true;
    }
    match (filter_type.split_once('/'), t.split_once('/')) {
        (Some((fb, "*")), Some((tb, _))) => fb == tb,
        (None, Some((tb, _))) => filter_type == tb,
        _ => false,
    }
}

/// The export decision of registerReceiverWithFeatureTraced (:428-472).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Export {
    pub exported: bool,
    /// Neither flag given for a receiver at targetSdk >= 34: AOSP throws SecurityException (:457-460) unless every action is a protected broadcast (`isProtectedBroadcast`). The protected-broadcast list is not
    /// loaded, so the registration is taken as exported (the pre-34 default) and the caller counts it.
    pub flag_missing_unchecked: bool,
}

/// Both flags: IllegalArgumentException (:429-434). No flag: exported, whether the receiver is null (a sticky read, :454-455) or the rule above was not checked.
pub fn register_export(flags: i32, has_receiver: bool, target_sdk: i32) -> Result<Export, String> {
    if flags & RECEIVER_EXPORTED != 0 && flags & RECEIVER_NOT_EXPORTED != 0 {
        return Err(
            "Receiver can't specify both RECEIVER_EXPORTED and RECEIVER_NOT_EXPORTEDflag".into(),
        );
    }
    if flags & (RECEIVER_EXPORTED | RECEIVER_NOT_EXPORTED) == 0 {
        return Ok(Export {
            exported: true,
            flag_missing_unchecked: has_receiver && target_sdk >= EXPLICIT_EXPORT_REQUIRED_SDK,
        });
    }
    Ok(Export {
        exported: flags & RECEIVER_EXPORTED != 0,
        flag_missing_unchecked: false,
    })
}

/// ActivityManager.checkComponentPermission:5226-5258 (no isolated uids exist): root / system, then the owning app, then exported, then the permission.
pub fn component_permission(
    permission: Option<&str>,
    uid: u32,
    owning_uid: u32,
    exported: bool,
    holds: impl Fn(&str) -> bool,
) -> bool {
    if permission::is_system_or_root(uid) {
        return true;
    }
    if permission::app_id(uid) == permission::app_id(owning_uid)
        && permission::user_id(uid) == permission::user_id(owning_uid)
    {
        return true;
    }
    if !exported {
        return false;
    }
    permission.is_none_or(holds)
}

/// Why broadcastIntentLocked refuses a sticky send (:1415-1440), checked in AOSP's order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StickyRefusal {
    /// SecurityException
    Security(String),
    /// BROADCAST_STICKY_CANT_HAVE_PERMISSION returned
    CantHavePermission,
}

/// The sticky send checks: BROADCAST_STICKY held (:1417-1429), no requiredPermissions (:1430-1436), no component (:1437-1440).
pub fn sticky_send(
    holds_sticky: bool,
    pid: i32,
    uid: u32,
    has_required: bool,
    has_component: bool,
) -> Result<(), StickyRefusal> {
    if !holds_sticky {
        return Err(StickyRefusal::Security(format!(
            "Permission Denial: broadcastIntent() requesting a sticky broadcast from pid={pid}, uid={uid} requires {}",
            permission::BROADCAST_STICKY
        )));
    }
    if has_required {
        return Err(StickyRefusal::CantHavePermission);
    }
    if has_component {
        return Err(StickyRefusal::Security(
            "Sticky broadcasts can't target a specific component".into(),
        ));
    }
    Ok(())
}

/// A registered filter (BroadcastFilter, reduced).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BroadcastFilter {
    pub filter: Filter,
    /// the registering package (BroadcastFilter.packageName): an intent with a package reaches only its filters
    pub package: String,
    /// what a sender must hold (registerReceiver's broadcastPermission)
    pub required_permission: Option<String>,
    pub exported: bool,
}

/// One IIntentReceiver's registrations (ReceiverList).
#[derive(Debug, Clone)]
pub struct ReceiverList<R> {
    pub receiver: R,
    pub pid: i32,
    pub uid: u32,
    pub user: i32,
    pub filters: Vec<BroadcastFilter>,
}

/// A stored sticky broadcast (BroadcastController.StickyBroadcast, reduced).
#[derive(Debug, Clone)]
pub struct Sticky<I> {
    pub key: IntentKey,
    pub intent: I,
    pub calling_uid: u32,
}

/// The skip rules of BroadcastSkipPolicy.shouldSkipMessage for a registered receiver, in its order: the filter's permission held by the sender (:388), each requiredPermission held by the receiver
/// (:457-465), the export rule (:580-592). Some(reason) = skipped.
pub fn skip_registered(
    bf: &BroadcastFilter,
    sender_uid: u32,
    receiver_uid: u32,
    required: &[String],
    sender_holds: impl Fn(&str) -> bool,
    receiver_holds: impl Fn(&str) -> bool,
) -> Option<String> {
    if let Some(p) = &bf.required_permission
        && !component_permission(Some(p), sender_uid, receiver_uid, true, &sender_holds)
    {
        return Some(format!(
            "Permission Denial: broadcasting from uid {sender_uid} requires {p} due to registered receiver of {}",
            bf.package
        ));
    }
    if let Some(p) = required.iter().find(|p| !receiver_holds(p)) {
        return Some(format!(
            "Permission Denial: receiving by {} (uid {receiver_uid}) requires {p} due to sender uid {sender_uid}",
            bf.package
        ));
    }
    if !bf.exported && !component_permission(None, sender_uid, receiver_uid, false, |_| false) {
        return Some(format!(
            "Exported Denial: sending from uid {sender_uid} due to receiver of {} (uid {receiver_uid}) not specifying RECEIVER_EXPORTED",
            bf.package
        ));
    }
    None
}

/// A manifest receiver as the skip rules read it (ResolveInfo.activityInfo, reduced).
pub struct ManifestReceiver<'a> {
    /// `<package>/<class>`, for the messages
    pub component: &'a str,
    pub permission: Option<&'a str>,
    pub exported: bool,
    /// its application's uid
    pub uid: u32,
}

/// The manifest-receiver skip rules of shouldSkip (:133-140, :308-312): checkComponentPermission of the receiver's permission and export for the sender, then each requiredPermission held by the receiver.
pub fn skip_manifest(
    r: &ManifestReceiver<'_>,
    sender_uid: u32,
    required: &[String],
    sender_holds: impl Fn(&str) -> bool,
    receiver_holds: impl Fn(&str) -> bool,
) -> Option<String> {
    let (component, permission, exported, receiver_uid) =
        (r.component, r.permission, r.exported, r.uid);
    if !component_permission(permission, sender_uid, receiver_uid, exported, sender_holds) {
        return Some(match (exported, permission) {
            (false, _) => format!(
                "Permission Denial: broadcasting from uid {sender_uid} is not exported from uid {receiver_uid} due to receiver {component}"
            ),
            (true, p) => format!(
                "Permission Denial: broadcasting from uid {sender_uid} requires {} due to receiver {component}",
                p.unwrap_or("?")
            ),
        });
    }
    if let Some(p) = required.iter().find(|p| !receiver_holds(p)) {
        return Some(format!(
            "Permission Denial: receiving by {component} (uid {receiver_uid}) requires {p} due to sender uid {sender_uid}"
        ));
    }
    None
}

/// The broadcast state: registrations, stickies by user then action, and per-process FIFOs of jobs for processes that have not attached.
#[derive(Debug)]
pub struct Broadcasts<R, I, J> {
    lists: Vec<ReceiverList<R>>,
    sticky: BTreeMap<i32, BTreeMap<String, Vec<Sticky<I>>>>,
    pending: BTreeMap<i32, VecDeque<J>>,
}

impl<R, I, J> Default for Broadcasts<R, I, J> {
    fn default() -> Self {
        Broadcasts {
            lists: Vec::new(),
            sticky: BTreeMap::new(),
            pending: BTreeMap::new(),
        }
    }
}

impl<R: Clone + PartialEq, I: Clone, J> Broadcasts<R, I, J> {
    /// registerReceiverWithFeatureTraced:530-579: a new ReceiverList, or the existing one of the same caller (another uid / pid / user is IllegalArgumentException); an equal filter already there is not
    /// added again (logged by AOSP). Ok(true) = added.
    pub fn register(
        &mut self,
        receiver: R,
        pid: i32,
        uid: u32,
        user: i32,
        bf: BroadcastFilter,
    ) -> Result<bool, String> {
        let i = match self.lists.iter().position(|l| l.receiver == receiver) {
            Some(i) => i,
            None => {
                self.lists.push(ReceiverList {
                    receiver,
                    pid,
                    uid,
                    user,
                    filters: Vec::new(),
                });
                self.lists.len() - 1
            }
        };
        let l = &mut self.lists[i];
        if l.uid != uid {
            return Err(format!(
                "Receiver requested to register for uid {uid} was previously registered for uid {}",
                l.uid
            ));
        }
        if l.pid != pid {
            return Err(format!(
                "Receiver requested to register for pid {pid} was previously registered for pid {}",
                l.pid
            ));
        }
        if l.user != user {
            return Err(format!(
                "Receiver requested to register for user {user} was previously registered for user {}",
                l.user
            ));
        }
        if l.filters.iter().any(|f| f.filter == bf.filter) {
            return Ok(false);
        }
        l.filters.push(bf);
        Ok(true)
    }

    /// unregisterReceiverTraced:647: the receiver's list and its filters go. True when it was registered.
    pub fn unregister(&mut self, receiver: &R) -> bool {
        let before = self.lists.len();
        self.lists.retain(|l| l.receiver != *receiver);
        self.lists.len() != before
    }

    /// The process died (ProcessRecord.mReceivers -> removeReceiverLocked; its queue goes with it): its registrations and its waiting jobs; the number of jobs dropped.
    pub fn process_died(&mut self, pid: i32) -> usize {
        self.lists.retain(|l| l.pid != pid);
        self.pending.remove(&pid).map_or(0, |q| q.len())
    }

    /// The registered filters `key` reaches for `user` (mReceiverResolver.queryIntent): per matching filter, as IntentResolver lists them; an intent with a package only reaches that package's filters.
    pub fn receivers(&self, key: &IntentKey, user: i32) -> Vec<(R, i32, u32, BroadcastFilter)> {
        let mut out = Vec::new();
        for l in &self.lists {
            if !(l.user == USER_ALL || user == USER_ALL || l.user == user) {
                continue;
            }
            for bf in &l.filters {
                if key.package.as_ref().is_some_and(|p| *p != bf.package) {
                    continue;
                }
                if matches(&bf.filter, key) {
                    out.push((l.receiver.clone(), l.pid, l.uid, bf.clone()));
                }
            }
        }
        out
    }

    /// Adds a sticky under `user` and its action (:1466-1495; the USER_ALL sweep of per-user copies at :1444-1465 is multi-user, not modelled): one with an equal intent (filterEquals) is replaced in place, else appended. An action-less intent is not stored (AOSP keys by action).
    pub fn put_sticky(&mut self, user: i32, key: IntentKey, intent: I, calling_uid: u32) {
        let Some(action) = key.action.clone() else {
            return;
        };
        let list = self
            .sticky
            .entry(user)
            .or_default()
            .entry(action)
            .or_default();
        let s = Sticky {
            key,
            intent,
            calling_uid,
        };
        match list.iter_mut().find(|o| o.key.filter_equals(&s.key)) {
            Some(o) => *o = s,
            None => list.push(s),
        }
    }

    /// unbroadcastIntent:1756-1790: the sticky with an equal intent under `user` is removed. True when there was one.
    pub fn remove_sticky(&mut self, user: i32, key: &IntentKey) -> bool {
        let Some(action) = &key.action else {
            return false;
        };
        let Some(list) = self.sticky.get_mut(&user).and_then(|m| m.get_mut(action)) else {
            return false;
        };
        let before = list.len();
        list.retain(|s| !s.key.filter_equals(key));
        before != list.len()
    }

    /// The stickies `filter` matches for a caller of `user` (:378-395, :474-509): per filter action, USER_ALL's then the user's, kept when the filter matches.
    pub fn stickies(&self, filter: &Filter, user: i32) -> Vec<Sticky<I>> {
        let mut out = Vec::new();
        for action in &filter.actions {
            for u in [USER_ALL, user] {
                if let Some(list) = self.sticky.get(&u).and_then(|m| m.get(action)) {
                    out.extend(list.iter().filter(|s| matches(filter, &s.key)).cloned());
                }
            }
        }
        out
    }

    /// Queues `job` for process `pid` until it attaches.
    pub fn push_pending(&mut self, pid: i32, job: J) {
        self.pending.entry(pid).or_default().push_back(job);
    }

    /// The jobs waiting for `pid`, in order, leaving none.
    pub fn take_pending(&mut self, pid: i32) -> VecDeque<J> {
        self.pending.remove(&pid).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(action: &str) -> IntentKey {
        IntentKey {
            action: Some(action.into()),
            ..Default::default()
        }
    }

    fn filter(actions: &[&str]) -> Filter {
        Filter {
            actions: actions.iter().map(|a| a.to_string()).collect(),
            ..Default::default()
        }
    }

    fn bf(actions: &[&str], package: &str, exported: bool) -> BroadcastFilter {
        BroadcastFilter {
            filter: filter(actions),
            package: package.into(),
            required_permission: None,
            exported,
        }
    }

    #[test]
    fn action_categories_and_data_match_as_intent_filter() {
        let f = filter(&["a.X"]);
        assert!(matches(&f, &key("a.X")));
        assert!(!matches(&f, &key("a.Y")));
        assert!(
            !matches(&f, &IntentKey::default()),
            "a null action matches nothing"
        );
        // categories: every intent category must be in the filter; a filter with none takes only category-less intents
        let mut k = key("a.X");
        k.categories = vec!["c.A".into()];
        assert!(!matches(&f, &k));
        let f2 = Filter {
            categories: Some(vec!["c.A".into(), "c.B".into()]),
            ..filter(&["a.X"])
        };
        assert!(matches(&f2, &k));
        // data: a filter without schemes or types takes no data; one with schemes takes its scheme; none + content is fine only with a type part
        let mut d = key("a.X");
        d.data = Some("package:com.x".into());
        assert!(!matches(&f, &d));
        let fs = Filter {
            schemes: Some(vec!["package".into()]),
            ..filter(&["a.X"])
        };
        assert!(matches(&fs, &d));
        assert!(
            !matches(&fs, &key("a.X")),
            "a scheme filter needs the scheme"
        );
        let ft = Filter {
            types: Some(vec!["image/*".into()]),
            ..filter(&["a.X"])
        };
        let mut t = key("a.X");
        t.resolved_type = Some("image/png".into());
        assert!(matches(&ft, &t));
        let parcelled = Filter {
            types: Some(vec!["image".into()]),
            ..filter(&["a.X"])
        };
        assert!(matches(&parcelled, &t), "a parcelled partial type");
        t.data = Some("content://p/1".into());
        assert!(matches(&ft, &t), "content data passes a type-only filter");
        t.data = Some("http://h/1".into());
        assert!(!matches(&ft, &t));
        assert!(!matches(
            &f,
            &IntentKey {
                resolved_type: Some("image/png".into()),
                ..key("a.X")
            }
        ));
    }

    fn data_key(data: &str) -> IntentKey {
        IntentKey {
            action: Some("a.X".into()),
            data: Some(data.into()),
            ..Default::default()
        }
    }

    fn auth(host: &str, wild: bool, port: i32) -> Authority {
        Authority {
            host: host.into(),
            wild,
            port,
        }
    }

    fn pat(pattern: &str, kind: i32) -> Pattern {
        Pattern {
            pattern: pattern.into(),
            kind,
            parsed: Vec::new(),
        }
    }

    /// scheme https with the given authorities and paths
    fn https(authorities: Vec<Authority>, paths: Option<Vec<Pattern>>) -> Filter {
        Filter {
            actions: vec!["a.X".into()],
            schemes: Some(vec!["https".into()]),
            authorities: Some(authorities),
            paths,
            ..Default::default()
        }
    }

    #[test]
    fn data_parts_match_as_match_data() {
        // PATH 0x500000, HOST 0x300000, PORT 0x400000, SSP 0x580000, SCHEME 0x200000, + MATCH_ADJUSTMENT_NORMAL 0x8000 (IntentFilter.java:246-275)
        let with_path = https(
            vec![auth("example.com", false, -1)],
            Some(vec![pat("/app", pattern_matcher::PATTERN_PREFIX)]),
        );
        assert_eq!(
            match_value(&with_path, &data_key("https://example.com/app/x")),
            Some(0x508000)
        );
        assert_eq!(
            match_value(&with_path, &data_key("https://other.com/app/x")),
            None
        );
        assert_eq!(
            match_value(&with_path, &data_key("https://example.com/other")),
            None
        );
        assert_eq!(
            match_value(&with_path, &data_key("https://EXAMPLE.com/app")),
            Some(0x508000),
            "compareToIgnoreCase"
        );
        let host_only = https(vec![auth("example.com", false, -1)], None);
        assert_eq!(
            match_value(&host_only, &data_key("https://example.com/z")),
            Some(0x308000)
        );
        let with_port = https(vec![auth("example.com", false, 8080)], None);
        assert_eq!(
            match_value(&with_port, &data_key("https://example.com:8080/")),
            Some(0x408000)
        );
        assert_eq!(
            match_value(&with_port, &data_key("https://example.com:9090/")),
            None
        );
        assert_eq!(
            match_value(&with_port, &data_key("https://example.com/")),
            None,
            "no port reads as -1"
        );
        // parcel host `.example.com` with wild: `*.example.com`
        let wild = https(vec![auth(".example.com", true, -1)], None);
        assert_eq!(
            match_value(&wild, &data_key("https://m.example.com/")),
            Some(0x308000)
        );
        assert_eq!(match_value(&wild, &data_key("https://example.org/")), None);
        // non-ASCII case folding as Character.toUpperCase: 'Ü' == 'ü'
        let umlaut = https(vec![auth("bücher.de", false, -1)], None);
        assert_eq!(
            match_value(&umlaut, &data_key("https://BÜCHER.de/")),
            Some(0x308000)
        );
        let tel = Filter {
            actions: vec!["a.X".into()],
            schemes: Some(vec!["tel".into()]),
            ssps: Some(vec![pat("123", pattern_matcher::PATTERN_LITERAL)]),
            ..Default::default()
        };
        assert_eq!(match_value(&tel, &data_key("tel:123")), Some(0x588000));
        assert_eq!(match_value(&tel, &data_key("tel:456")), None);
        // the path is percent-decoded before matching
        let decoded = https(
            vec![auth("example.com", false, -1)],
            Some(vec![pat("/a b", pattern_matcher::PATTERN_LITERAL)]),
        );
        assert_eq!(
            match_value(&decoded, &data_key("https://example.com/a%20b")),
            Some(0x508000)
        );
        // scheme only: the filter names no data part
        let scheme_only = Filter {
            actions: vec!["a.X".into()],
            schemes: Some(vec!["https".into()]),
            ..Default::default()
        };
        assert_eq!(
            match_value(&scheme_only, &data_key("https://x/y")),
            Some(0x208000)
        );
    }

    #[test]
    fn a_missed_ssp_falls_through_to_the_authority() {
        // matchData:1763-1797: a missed SSP still tries the authorities; without them it is a miss
        let f = Filter {
            actions: vec!["a.X".into()],
            schemes: Some(vec!["https".into()]),
            ssps: Some(vec![pat("x", pattern_matcher::PATTERN_LITERAL)]),
            authorities: Some(vec![auth("example.com", false, -1)]),
            ..Default::default()
        };
        assert_eq!(
            match_value(&f, &data_key("https://example.com/")),
            Some(0x308000)
        );
        let no_authority = Filter {
            authorities: None,
            ..f.clone()
        };
        assert_eq!(
            match_value(&no_authority, &data_key("https://example.com/")),
            None
        );
    }

    #[test]
    fn authority_and_path_follow_uri_parsing() {
        let with_port = https(vec![auth("example.com", false, 8080)], None);
        // userinfo ends at the last '@'; the port follows the host
        assert_eq!(
            match_value(&with_port, &data_key("https://u:p@example.com:8080")),
            Some(0x408000)
        );
        let host = https(vec![auth("example.com", false, -1)], None);
        assert_eq!(
            match_value(&host, &data_key("https://example.com:")),
            Some(0x308000),
            "an empty port reads as -1"
        );
        assert_eq!(
            match_value(&host, &data_key("https://example.com?x=/")),
            Some(0x308000),
            "'?' ends the host"
        );
        // opaque URI: no authority, so an authority filter misses
        let mailto = Filter {
            actions: vec!["a.X".into()],
            schemes: Some(vec!["mailto".into()]),
            authorities: Some(vec![auth("example.com", false, -1)]),
            ..Default::default()
        };
        assert_eq!(
            match_value(&mailto, &data_key("mailto:x@example.com")),
            None
        );
        // no path after the host reads as "": PREFIX "/" misses it, PREFIX "" takes it
        let prefix_slash = https(
            vec![auth("example.com", false, -1)],
            Some(vec![pat("/", pattern_matcher::PATTERN_PREFIX)]),
        );
        assert_eq!(
            match_value(&prefix_slash, &data_key("https://example.com")),
            None
        );
        assert_eq!(
            match_value(&prefix_slash, &data_key("https://example.com?q")),
            None
        );
        assert_eq!(
            match_value(&prefix_slash, &data_key("https://example.com/?q")),
            Some(0x508000)
        );
        let any_path = https(
            vec![auth("example.com", false, -1)],
            Some(vec![pat("", pattern_matcher::PATTERN_PREFIX)]),
        );
        assert_eq!(
            match_value(&any_path, &data_key("https://example.com")),
            Some(0x508000)
        );
    }

    #[test]
    fn uri_decode_follows_uri_codec() {
        assert_eq!(uri_decode("a%20b%41%C3%A9+"), "a bAé+");
        // a truncated escape is U+FFFD; a bad digit is U+FFFD plus the byte built so far, and the bad digit is consumed (UriCodec.java:128)
        assert_eq!(uri_decode("%4"), "\u{FFFD}");
        assert_eq!(uri_decode("%4z"), "\u{FFFD}\u{4}");
        assert_eq!(uri_decode("%FF"), "\u{FFFD}");
    }

    #[test]
    fn export_flags_follow_register_receiver() {
        assert!(register_export(RECEIVER_EXPORTED | RECEIVER_NOT_EXPORTED, true, 36).is_err());
        assert_eq!(
            register_export(RECEIVER_NOT_EXPORTED, true, 36),
            Ok(Export {
                exported: false,
                flag_missing_unchecked: false
            })
        );
        assert_eq!(
            register_export(0, true, 36),
            Ok(Export {
                exported: true,
                flag_missing_unchecked: true
            })
        );
        // a sticky read (null receiver) and a pre-34 app need no flag
        assert_eq!(
            register_export(0, false, 36),
            Ok(Export {
                exported: true,
                flag_missing_unchecked: false
            })
        );
        assert_eq!(
            register_export(0, true, 33),
            Ok(Export {
                exported: true,
                flag_missing_unchecked: false
            })
        );
    }

    #[test]
    fn unexported_receivers_take_only_their_app_and_the_system() {
        let b = bf(&["a.X"], "p.a", false);
        // sender 10124 is another app; 10123 is the receiver's own uid; 1000 is the system
        assert!(
            skip_registered(&b, 10_124, 10_123, &[], |_| true, |_| true)
                .is_some_and(|r| r.starts_with("Exported Denial"))
        );
        assert!(skip_registered(&b, 10_123, 10_123, &[], |_| false, |_| false).is_none());
        assert!(skip_registered(&b, 1000, 10_123, &[], |_| false, |_| false).is_none());
        assert!(
            skip_registered(
                &bf(&["a.X"], "p.a", true),
                10_124,
                10_123,
                &[],
                |_| false,
                |_| false
            )
            .is_none()
        );
    }

    #[test]
    fn permissions_gate_both_ends() {
        let mut b = bf(&["a.X"], "p.a", true);
        b.required_permission = Some("p.SEND".into());
        assert!(skip_registered(&b, 10_124, 10_123, &[], |_| false, |_| true).is_some());
        assert!(skip_registered(&b, 10_124, 10_123, &[], |p| p == "p.SEND", |_| true).is_none());
        let need = ["p.RECV".to_owned()];
        let open = bf(&["a.X"], "p.a", true);
        assert!(skip_registered(&open, 10_124, 10_123, &need, |_| true, |_| false).is_some());
        assert!(
            skip_registered(&open, 10_124, 10_123, &need, |_| true, |p| p == "p.RECV").is_none()
        );
        // manifest: unexported refuses other apps, a receiver permission must be held by the sender
        let closed = ManifestReceiver {
            component: "p/.R",
            permission: None,
            exported: false,
            uid: 10_123,
        };
        assert!(skip_manifest(&closed, 10_124, &[], |_| true, |_| true).is_some());
        assert!(skip_manifest(&closed, 10_123, &[], |_| false, |_| true).is_none());
        let guarded = ManifestReceiver {
            permission: Some("p.P"),
            exported: true,
            ..closed
        };
        assert!(skip_manifest(&guarded, 10_124, &[], |_| false, |_| true).is_some());
        assert!(skip_manifest(&guarded, 10_124, &[], |_| true, |_| true).is_none());
        let need = ["p.RECV".to_owned()];
        assert!(skip_manifest(&guarded, 10_124, &need, |_| true, |_| false).is_some());
    }

    #[test]
    fn sticky_send_checks_in_aosp_order() {
        assert!(
            matches!(sticky_send(false, 1, 10_123, true, true), Err(StickyRefusal::Security(m)) if m.contains(permission::BROADCAST_STICKY))
        );
        assert_eq!(
            sticky_send(true, 1, 10_123, true, true),
            Err(StickyRefusal::CantHavePermission)
        );
        assert!(matches!(
            sticky_send(true, 1, 10_123, false, true),
            Err(StickyRefusal::Security(_))
        ));
        assert_eq!(sticky_send(true, 1, 10_123, false, false), Ok(()));
    }

    #[test]
    fn sticky_store_replaces_by_filter_equals_and_answers_a_filter() {
        let mut b: Broadcasts<u32, &str, ()> = Broadcasts::default();
        b.put_sticky(0, key("a.S"), "first", 10_123);
        b.put_sticky(0, key("a.S"), "second", 10_123);
        let mut other = key("a.S");
        other.identifier = Some("id".into());
        b.put_sticky(USER_ALL, other.clone(), "all-users", 10_123);
        let got: Vec<&str> = b
            .stickies(&filter(&["a.S"]), 0)
            .iter()
            .map(|s| s.intent)
            .collect();
        assert_eq!(
            got,
            ["all-users", "second"],
            "USER_ALL first, then the user's; equal intents replaced"
        );
        assert!(b.stickies(&filter(&["a.T"]), 0).is_empty());
        assert!(b.remove_sticky(0, &key("a.S")));
        assert!(!b.remove_sticky(0, &key("a.S")));
        assert_eq!(b.stickies(&filter(&["a.S"]), 0).len(), 1);
    }

    #[test]
    fn registrations_are_per_receiver_and_die_with_the_process() {
        let mut b: Broadcasts<u32, (), &str> = Broadcasts::default();
        assert_eq!(
            b.register(7, 100, 10_123, 0, bf(&["a.X"], "p.a", true)),
            Ok(true)
        );
        assert_eq!(
            b.register(7, 100, 10_123, 0, bf(&["a.X"], "p.a", true)),
            Ok(false),
            "equal filter not added twice"
        );
        assert!(
            b.register(7, 101, 10_123, 0, bf(&["a.Y"], "p.a", true))
                .is_err(),
            "another pid"
        );
        assert_eq!(
            b.register(8, 200, 10_124, 0, bf(&["a.X"], "p.b", true)),
            Ok(true)
        );
        assert_eq!(b.receivers(&key("a.X"), 0).len(), 2);
        let mut only_b = key("a.X");
        only_b.package = Some("p.b".into());
        assert_eq!(
            b.receivers(&only_b, 0)
                .iter()
                .map(|r| r.0)
                .collect::<Vec<_>>(),
            [8]
        );
        assert!(b.unregister(&8) && !b.unregister(&8));
        b.push_pending(100, "one");
        b.push_pending(100, "two");
        assert_eq!(b.process_died(100), 2);
        assert!(b.receivers(&key("a.X"), 0).is_empty());
        b.push_pending(300, "x");
        b.push_pending(300, "y");
        assert_eq!(b.take_pending(300), ["x", "y"]);
        assert!(b.take_pending(300).is_empty());
    }
}
