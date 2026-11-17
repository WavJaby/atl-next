// SPDX-License-Identifier: MIT
//! The platform layer's host side (atld, xtask, the system services) as traits: the desktop's monitors, read once at start: size, refresh rate, scale. Following the window to another monitor or scale is the app side's (the window knows its monitor; atld has no window).
//! Shaped by the one implementation, GDK on Linux (atl-platform-host-linux): the connection is thread-affine, and a monitor that reports no refresh rate answers `None`.
//! The machine's motion sensors ([`Sensors`]): shaped by the one implementation, Linux IIO. A stream per open, served by the kernel buffer where the environment enabled it and by sysfs reads otherwise ([`Tier`]); the caller paces its reads.
//! The desktop's fonts ([`FontSource`]): which faces, in what order, for which language; read when atld starts.
//! The host's network ([`NetworkSource`]): the link its default route leaves through, its resolver, and the system's own verdict on reaching the Internet; read when the services start.
//! The host's memory ([`MemorySource`]): total and available RAM, read afresh per call.
//! The desktop's notification service ([`Notifications`]): shows, replaces and withdraws notifications atld posts on the apps' behalf; tiers probed per session ([`NotificationTier`]).
//! No OS or toolkit dependency, no unsafe; core crates use the `atl-platform-host` facade, not this crate or the implementation directly.
#![forbid(unsafe_code)]
#![deny(missing_docs)]

/// The desktop could not be reached: no display server answers, or the toolkit refused to start.
#[derive(Debug, thiserror::Error)]
#[error("desktop unavailable: {0}")]
pub struct HostError(
    /// The toolkit's reason.
    pub String,
);

/// The value types the app side shares (atld's monitor is the same type a window reports).
pub use atl_platform_types::{Monitor, PhysSize, Scale};

use std::path::{Path, PathBuf};

/// A connection to the desktop. Connect where the monitors are read; `monitor` answers from that connection.
pub trait Desktop: Sized {
    /// Connects on the calling thread.
    fn connect() -> Result<Self, HostError>;

    /// The first monitor (Wayland has no primary); `None` where the desktop lists none.
    fn monitor(&self) -> Option<Monitor>;
}

/// A sensor could not be read (the device went away, or answered something unparsable).
#[derive(Debug, thiserror::Error)]
#[error("sensor unavailable: {0}")]
pub struct SensorError(
    /// The source's reason.
    pub String,
);

/// One accelerometer of the machine, as listed by [`Sensors::accelerometers`].
#[derive(Debug, Clone, PartialEq)]
pub struct Accelerometer {
    /// Stable key for [`Sensors::open`] while the device exists.
    pub id: String,
    /// The device's own name.
    pub name: String,
    /// Largest magnitude one axis reports, m/s².
    pub max_range: f32,
    /// One raw step, m/s².
    pub resolution: f32,
    /// Shortest useful read interval of the tier in `tier`: the device's sample interval when buffered, the read span when polled.
    pub min_period: std::time::Duration,
    /// The tier [`Sensors::open`] serves, probed at listing; a stream opened later may fall back (see [`SampleStream::tier`]).
    pub tier: Tier,
}

/// The desktop's font configuration could not be read.
#[derive(Debug, thiserror::Error)]
#[error("fonts unavailable: {0}")]
pub struct FontError(
    /// The source's reason.
    pub String,
);

/// A question to the desktop's font configuration: a family name or generic (`sans-serif`, `monospace`, ...), optionally narrowed to faces covering a language and to a width.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FontQuery {
    /// Family name or generic alias, resolved by the desktop's rules; empty = any family.
    pub family: String,
    /// The desktop's language id (fontconfig's: `he`, `zh-tw`, `und-zsye`).
    pub lang: Option<String>,
    /// Width, 100 = normal, 75 = condensed (fontconfig's scale = OS/2 `usWidthClass` percentages).
    pub width: Option<i32>,
}

/// One face of a font file the desktop knows. What the face is (weight, axes, names) is read from the file itself.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Face {
    /// The font file.
    pub path: std::path::PathBuf,
    /// Face index inside a collection (`.ttc`); 0 for a single-face file.
    pub index: u32,
    /// The family name the desktop lists it under (first one).
    pub family: String,
    /// The desktop's width value of the face.
    pub width: i32,
}

/// The host's network could not be read (a kernel table or resolver file answered something unparsable).
#[derive(Debug, thiserror::Error)]
#[error("network state unavailable: {0}")]
pub struct NetworkError(
    /// The source's reason.
    pub String,
);

/// The kind of link the default route leaves through, as the host's driver reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkKind {
    /// A wireless LAN interface.
    Wifi,
    /// Any other interface (wired, virtual): a desktop's default.
    Ethernet,
}

/// The system's verdict on reaching the Internet over the default route, as the desktop's network monitor reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Connectivity {
    /// A route exists but the Internet is known not to be reachable.
    Limited,
    /// A captive portal intercepts traffic.
    Portal,
    /// The Internet is reachable (or the monitor has no check and a default route exists).
    Full,
}

/// The host's default network: the link its default route leaves through.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DefaultNetwork {
    /// The interface name (`eth0`, `wlp2s0`).
    pub interface: String,
    /// Its kind.
    pub kind: LinkKind,
    /// Its hardware address as the kernel prints it (`aa:bb:..`); `None` where it has none.
    pub hw_address: Option<String>,
    /// Its MTU; `None` where unreadable.
    pub mtu: Option<u32>,
    /// The default routes' gateways through it, IPv4 first; an on-link default route has none.
    pub gateways: Vec<std::net::IpAddr>,
    /// The resolver's servers, in the host resolver's order.
    pub dns: Vec<std::net::IpAddr>,
    /// The resolver's search domains, space-separated; `None` without any.
    pub search: Option<String>,
    /// Whether the Internet is reachable over it.
    pub connectivity: Connectivity,
    /// Whether the system considers it metered.
    pub metered: bool,
}

/// The host's network state, as a native desktop program sees it. Shaped by its one implementation: the kernel's route tables, the resolver file, GIO's network monitor.
pub trait NetworkSource: Send + Sync {
    /// The default network now; `None` when the host has no default route (offline).
    fn default_network(&self) -> Result<Option<DefaultNetwork>, NetworkError>;
}

/// The host's memory could not be read (the kernel's report was missing or unparsable).
#[derive(Debug, thiserror::Error)]
#[error("memory state unavailable: {0}")]
pub struct MemoryError(
    /// The source's reason.
    pub String,
);

/// The host's memory now, in bytes, as the kernel accounts it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostMemory {
    /// Usable RAM: physical memory less what the kernel reserved at boot.
    pub total: u64,
    /// The kernel's estimate of memory available to start new programs without swapping (free + reclaimable caches).
    pub available: u64,
}

/// The host's memory, as a native program reads it. Shaped by its one implementation: the kernel's /proc/meminfo.
pub trait MemorySource: Send + Sync {
    /// The memory figures now; each call reads afresh.
    fn memory(&self) -> Result<HostMemory, MemoryError>;
}

/// The desktop's font configuration (plan C7: Android's font list follows the desktop). Shaped by its one implementation, fontconfig: a sorted preference list per query and a plain listing; named instances of a variable font are not separate faces.
pub trait FontSource {
    /// Faces in the desktop's preference order for `q` (best first); fonts not matching `q` follow, as the desktop's fallback order.
    fn sorted(&self, q: &FontQuery) -> Result<Vec<Face>, FontError>;

    /// Every face that matches `q` exactly (family name, covered language, width), unordered.
    fn list(&self, q: &FontQuery) -> Result<Vec<Face>, FontError>;
}

/// The machine's motion sensors. Values are Android's `SensorEvent` frame of the display's natural orientation (x right, y up the screen, z out of the screen; at rest the axis pointing up reads +g), m/s², gravity included: the source has applied its scale, mounting and frame conversion.
/// Only sensors in the display count (a laptop base's accelerometer is not the display's without hinge-angle compensation). Values never follow the display rotation: apps remap with `Display.getRotation()`.
pub trait Sensors: Send + Sync {
    /// The accelerometers present now; empty when there are none.
    fn accelerometers(&self) -> Vec<Accelerometer>;

    /// A stream of accelerometer `id`. An unusable buffer is not an error: the stream falls back to the baseline tier and says why.
    fn open(&self, id: &str) -> Result<Box<dyn SampleStream>, SensorError>;
}

/// How a stream gets its samples. The kernel buffer is the faster tier, used where the environment set it up; sysfs reads are the baseline every machine has.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    /// The kernel buffer: one scan per device sample, stamped by the kernel.
    Buffered,
    /// Sysfs reads, and why the buffer was not used.
    Polled(Fallback),
}

/// Why the buffered tier is not used; the label is the tier counter's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fallback {
    /// The scan layout is not one the decoder reads.
    Layout,
    /// No trigger, or the named trigger is not on the machine.
    Trigger,
    /// The buffer is off: nothing has enabled it.
    NotEnabled,
    /// The timestamp clock is not one the kernel offers, or scan stamps are ahead of arrival (not in that clock).
    Clock,
    /// This process may not open the char device.
    NodeDenied,
    /// Another reader holds the char device.
    NodeBusy,
    /// The buffer is on but no fresh scan arrives.
    NoData,
}

impl Fallback {
    /// The reason's label in the tier counter.
    pub fn label(self) -> &'static str {
        match self {
            Fallback::Layout => "layout",
            Fallback::Trigger => "trigger",
            Fallback::NotEnabled => "not-enabled",
            Fallback::Clock => "clock",
            Fallback::NodeDenied => "node-denied",
            Fallback::NodeBusy => "node-busy",
            Fallback::NoData => "no-data",
        }
    }
}

impl std::fmt::Display for Tier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Tier::Buffered => f.write_str("buffered"),
            Tier::Polled(why) => write!(f, "polled ({})", why.label()),
        }
    }
}

/// One sample: the frame of [`Sensors`] and the instant it was measured.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Sample {
    /// CLOCK_BOOTTIME, ns: Android's `elapsedRealtimeNanos`.
    pub boottime_ns: i64,
    /// x, y, z in the frame of [`Sensors`], m/s².
    pub values: [f32; 3],
}

/// One open reading of an accelerometer; dropping it releases the device.
pub trait SampleStream: Send {
    /// The tier serving this stream, decided when it was opened.
    fn tier(&self) -> Tier;

    /// Samples so far whose stamp was not in the stream's clock, each taken at arrival instead (the `ts-fallback` counter). Zero where stamps never leave their clock.
    fn ts_fallbacks(&self) -> u64;

    /// The newest sample not returned before. Returns as soon as one is ready, or `Ok(None)` after `timeout` with none; the polled tier returns after one read (its span is its interval). The caller paces one call per period and repeats its last sample on `None`.
    fn next(&mut self, timeout: std::time::Duration) -> Result<Option<Sample>, SensorError>;
}

/// One notification as the desktop shows it, already mapped from Android's (the system services own that mapping, notifications plan N4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesktopNotification {
    /// The source the desktop names (the app's label, else its package).
    pub app_name: String,
    /// The app's desktop file id without `.desktop` (`atl.<package>`): the desktop's per-app identity.
    pub app_id: String,
    /// The headline.
    pub summary: String,
    /// The text below it, plain (no markup).
    pub body: String,
    /// How insistently the desktop presents it.
    pub urgency: Urgency,
    /// It stays until withdrawn (Android ongoing / no-clear): the desktop never expires it.
    pub resident: bool,
    /// No sound for this post.
    pub silent: bool,
}

/// The desktop's urgency levels Android's importances map to (freedesktop Desktop Notifications 1.3 hint `urgency`; critical comes with full-screen intents, notifications plan S4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Urgency {
    /// Shade-only content.
    Low,
    /// The ordinary banner.
    Normal,
}

/// Which desktop service shows notifications: the capability tier the implementation probed, named by the tier log line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotificationTier {
    /// The freedesktop notification service (`org.freedesktop.Notifications`): the baseline every desktop with a notification daemon has.
    Fdo,
    /// No service: nothing is shown, every post is counted.
    None,
}

impl NotificationTier {
    /// The tier's name in log lines and counters.
    pub fn label(self) -> &'static str {
        match self {
            NotificationTier::Fdo => "fdo",
            NotificationTier::None => "none",
        }
    }
}

/// What became of one post.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Posted {
    /// A new notification is on the desktop.
    Shown,
    /// The key's earlier notification was replaced in place.
    Updated,
    /// No desktop service shows notifications now: nothing was sent.
    NoService,
}

/// The desktop's notification service failed a call it answered.
#[derive(Debug, thiserror::Error)]
#[error("notification service: {0}")]
pub struct NotificationError(
    /// The service's reason.
    pub String,
);

/// The desktop's notification service. Shaped by its one implementation, the freedesktop service over the session bus: the implementation keeps each key's desktop id, so an update replaces in place.
/// A key is the caller's stable name for one notification (Android's StatusBarNotification key); keys of all apps share one space.
pub trait Notifications: Send + Sync {
    /// Connects to the user's session and probes which tier serves; never fails: no session or no service is tier [`NotificationTier::None`].
    fn connect() -> Self
    where
        Self: Sized;

    /// The tier the last probe or call found.
    fn tier(&self) -> NotificationTier;

    /// Shows `n` under `key`, replacing the key's earlier one.
    fn post(&self, key: &str, n: &DesktopNotification) -> Result<Posted, NotificationError>;

    /// Removes `key`'s notification; false when none of it is shown.
    fn withdraw(&self, key: &str) -> Result<bool, NotificationError>;
}

/// The UI language of a native program: glibc's `LC_MESSAGES` (POSIX XBD 8.2), the first non-empty of `LC_ALL`, `LC_MESSAGES`, `LANG`, as one BCP-47 tag.
/// `LANGUAGE` (gettext's fallback list) is not read. `C`, `POSIX`, all unset, or a name [`bcp47_of_posix`] refuses: `None`, and the caller's default applies.
/// `lookup` reads one environment variable; the Linux crate passes `std::env::var`.
pub fn ui_locale_from(lookup: impl Fn(&str) -> Option<String>) -> Option<String> {
    ["LC_ALL", "LC_MESSAGES", "LANG"]
        .into_iter()
        .find_map(|name| lookup(name).filter(|value| !value.is_empty()))
        .and_then(|posix| bcp47_of_posix(&posix))
}

/// A POSIX locale name as a BCP-47 tag: `language[_territory][.codeset][@modifier]` -> `language[-TERRITORY]`, codeset and modifier dropped.
/// `None` where the language is not 2-3 ASCII letters (so `C` and `POSIX` give none) or the territory is not 2 letters or 3 digits (RFC 5646 region).
pub fn bcp47_of_posix(name: &str) -> Option<String> {
    let base = name.split_once(['.', '@']).map_or(name, |(head, _)| head);
    let (language, territory) = match base.split_once('_') {
        Some((language, territory)) => (language, Some(territory)),
        None => (base, None),
    };
    if !(2..=3).contains(&language.len()) || !language.bytes().all(|b| b.is_ascii_alphabetic()) {
        return None;
    }
    let mut tag = language.to_ascii_lowercase();
    if let Some(territory) = territory {
        let letters = territory.len() == 2 && territory.bytes().all(|b| b.is_ascii_alphabetic());
        let digits = territory.len() == 3 && territory.bytes().all(|b| b.is_ascii_digit());
        if !(letters || digits) {
            return None;
        }
        tag.push('-');
        tag.push_str(&territory.to_ascii_uppercase());
    }
    Some(tag)
}

/// The zone id a native Linux program runs in (glibc `tzset`): `TZ` when present, else the zone `/etc/localtime` links to.
/// `zoneinfo`: the host zoneinfo dir; `zone_file_exists(name)`: is `zoneinfo/<name>` a file (the Linux crate's check). A leading `:` is dropped.
/// An empty name is UTC: glibc reads `TZ=""` and `TZ=:` as UTC (offset 0) and never falls back to `/etc/localtime` (measured in a private mount namespace).
/// A name is a zone iff glibc opens `zoneinfo/<name>`; the id drops a leading `posix/` or `right/` (libcore's name for the same zone). `..` names none; a POSIX string (`CST-8`) names no file, so none.
/// An absolute name is a file for glibc: it names a zone only under `zoneinfo` (its path there). Elsewhere it names none, though glibc would read that file: libcore has no name for it.
pub fn zone_id_from(
    tz: Option<String>,
    localtime_target: Option<PathBuf>,
    zoneinfo: &Path,
    zone_file_exists: impl Fn(&str) -> bool,
) -> Option<String> {
    match tz {
        Some(tz) => {
            let spec = tz.strip_prefix(':').unwrap_or(tz.as_str());
            if spec.is_empty() {
                return Some("UTC".to_owned());
            }
            let rel = if spec.starts_with('/') {
                Path::new(spec).strip_prefix(zoneinfo).ok()?.to_str()?
            } else {
                spec
            };
            let name = zone_name(rel)?;
            zone_file_exists(rel).then_some(name)
        }
        None => zone_from_path(&localtime_target?),
    }
}

/// The libcore name of a zoneinfo-relative name: a leading `posix/` or `right/` (the same zones in other trees) dropped; empty or `..` names none.
fn zone_name(rel: &str) -> Option<String> {
    let name = rel
        .strip_prefix("posix/")
        .or_else(|| rel.strip_prefix("right/"))
        .unwrap_or(rel);
    (!name.is_empty() && !name.split('/').any(|c| c == "..")).then(|| name.to_owned())
}

/// The zone a canonical zoneinfo path names: the part after its first `/zoneinfo/`, as [`zone_name`].
fn zone_from_path(path: &Path) -> Option<String> {
    let (_, rel) = path.to_str()?.split_once("/zoneinfo/")?;
    zone_name(rel)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A lookup over fixed variables; a name not listed is unset.
    fn env<'a>(vars: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name| {
            vars.iter()
                .find(|(n, _)| *n == name)
                .map(|(_, v)| (*v).to_owned())
        }
    }

    #[test]
    fn posix_names_map_to_bcp47_tags() {
        // XBD 8.2 grammar; RFC 5646 region: 2 letters or 3 digits
        for (name, tag) in [
            ("zh_TW.UTF-8", "zh-TW"),
            ("en_US", "en-US"),
            ("de", "de"),
            ("sr_RS@latin", "sr-RS"),
            ("es_419.UTF-8", "es-419"),
        ] {
            assert_eq!(bcp47_of_posix(name).as_deref(), Some(tag), "{name}");
        }
    }

    #[test]
    fn c_posix_and_malformed_names_give_no_tag() {
        for name in [
            "C", "C.UTF-8", "POSIX", "", "e_US", "en_U", "en_USA", "en_1234",
        ] {
            assert_eq!(bcp47_of_posix(name), None, "{name:?}");
        }
    }

    #[test]
    fn lc_messages_precedence_and_empty_is_unset() {
        let fr = env(&[("LC_ALL", ""), ("LC_MESSAGES", "fr_FR"), ("LANG", "de_DE")]);
        assert_eq!(ui_locale_from(fr).as_deref(), Some("fr-FR"));
        let ja = env(&[
            ("LC_ALL", "ja_JP"),
            ("LC_MESSAGES", "fr_FR"),
            ("LANG", "de_DE"),
        ]);
        assert_eq!(ui_locale_from(ja).as_deref(), Some("ja-JP"));
        // LC_ALL=C is set, so no tag: LANG is not consulted
        assert_eq!(
            ui_locale_from(env(&[("LC_ALL", "C"), ("LANG", "zh_TW.UTF-8")])),
            None
        );
        // WSL's usual default
        assert_eq!(ui_locale_from(env(&[("LANG", "C.UTF-8")])), None);
        assert_eq!(ui_locale_from(env(&[])), None);
    }

    #[test]
    fn tz_names_its_zone_file_else_localtime_names_it() {
        let zoneinfo = Path::new("/usr/share/zoneinfo");
        let exists =
            |name: &str| ["Asia/Taipei", "Europe/Berlin", "America/New_York"].contains(&name);
        let new_york = || Some(PathBuf::from("/usr/share/zoneinfo/America/New_York"));
        // a leading ':' is dropped; the file must exist
        assert_eq!(
            zone_id_from(Some(":Asia/Taipei".into()), new_york(), zoneinfo, exists).as_deref(),
            Some("Asia/Taipei")
        );
        assert_eq!(
            zone_id_from(Some("Europe/Berlin".into()), new_york(), zoneinfo, exists).as_deref(),
            Some("Europe/Berlin")
        );
        // a POSIX string names no file, and glibc does not fall through to /etc/localtime
        assert_eq!(
            zone_id_from(
                Some("CST-8".into()),
                Some(PathBuf::from("/usr/share/zoneinfo/Asia/Tokyo")),
                zoneinfo,
                exists
            ),
            None
        );
        // unset: the zone /etc/localtime links to; posix/ and right/ name the same zones
        assert_eq!(
            zone_id_from(None, new_york(), zoneinfo, exists).as_deref(),
            Some("America/New_York")
        );
        assert_eq!(
            zone_id_from(
                None,
                Some(PathBuf::from("/usr/share/zoneinfo/posix/Asia/Taipei")),
                zoneinfo,
                exists
            )
            .as_deref(),
            Some("Asia/Taipei")
        );
        assert_eq!(
            zone_id_from(
                None,
                Some(PathBuf::from("/usr/share/zoneinfo/right/Europe/Berlin")),
                zoneinfo,
                exists
            )
            .as_deref(),
            Some("Europe/Berlin")
        );
        // a copied /etc/localtime is no link into zoneinfo; no /etc/localtime at all
        assert_eq!(
            zone_id_from(
                None,
                Some(PathBuf::from("/etc/localtime")),
                zoneinfo,
                exists
            ),
            None
        );
        assert_eq!(zone_id_from(None, None, zoneinfo, exists), None);
    }

    #[test]
    fn non_zone_names_are_refused_before_the_file_check() {
        // the closure accepts everything: only the guards can refuse these
        let zoneinfo = Path::new("/usr/share/zoneinfo");
        let any = |_: &str| true;
        for tz in [
            "../../etc/passwd",
            "Asia/../../etc/passwd",
            "/usr/share/zoneinfo/../../etc/passwd",
            "/usr/share/zoneinfo/posix/../../etc/passwd",
        ] {
            assert_eq!(
                zone_id_from(Some(tz.into()), None, zoneinfo, any),
                None,
                "{tz:?}"
            );
        }
    }

    #[test]
    fn empty_tz_is_utc() {
        // glibc: TZ="" and TZ=: read as UTC (`date +%Z:%z`: Universal:+0000 and UTC:+0000 in a private mount namespace), never /etc/localtime (New York there)
        let zoneinfo = Path::new("/usr/share/zoneinfo");
        let new_york = || Some(PathBuf::from("/usr/share/zoneinfo/America/New_York"));
        assert_eq!(
            zone_id_from(Some(String::new()), new_york(), zoneinfo, |_| false).as_deref(),
            Some("UTC")
        );
        assert_eq!(
            zone_id_from(Some(":".into()), new_york(), zoneinfo, |_| false).as_deref(),
            Some("UTC")
        );
    }

    #[test]
    fn absolute_tz_under_zoneinfo_names_the_zone_there() {
        // glibc reads an absolute TZ as a file; under zoneinfo its id is the path relative to it, posix/ and right/ dropped
        let zoneinfo = Path::new("/usr/share/zoneinfo");
        let exists = |name: &str| ["Asia/Taipei", "posix/Asia/Taipei"].contains(&name);
        let zone = |tz: &str| zone_id_from(Some(tz.into()), None, zoneinfo, exists);
        assert_eq!(
            zone("/usr/share/zoneinfo/Asia/Taipei").as_deref(),
            Some("Asia/Taipei")
        );
        assert_eq!(
            zone(":/usr/share/zoneinfo/Asia/Taipei").as_deref(),
            Some("Asia/Taipei")
        );
        assert_eq!(
            zone("/usr/share/zoneinfo/posix/Asia/Taipei").as_deref(),
            Some("Asia/Taipei")
        );
        // the file must exist
        assert_eq!(zone("/usr/share/zoneinfo/Asia/Tokyo"), None);
    }

    #[test]
    fn absolute_tz_outside_zoneinfo_names_none() {
        // glibc would read these files; libcore has no zone name for one outside zoneinfo. The path prefix decides, not the file.
        let zoneinfo = Path::new("/usr/share/zoneinfo");
        for tz in [
            "/etc/localtime",
            "/home/u/zoneinfo/Asia/Taipei",
            "/usr/share/zoneinfo-backup/Asia/Taipei",
            "/",
        ] {
            assert_eq!(
                zone_id_from(Some(tz.into()), None, zoneinfo, |_| true),
                None,
                "{tz}"
            );
        }
    }
}
