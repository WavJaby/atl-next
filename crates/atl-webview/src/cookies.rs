// SPDX-License-Identifier: MIT
//! CookieManager's synchronous copy (webview plan D5): the cookie store as last seen, read on any thread without touching the engine.
//! Writes (setCookie, removeAll*) land here at once and go to the engine; cookies a page sets arrive through the engine's change watch (a full list per change) -> a web-set cookie is
//! invisible until its change notification is processed (the accepted D5 cost, measured by the webview-cookies row).
//! Model and rules: RFC 6265 S5 (parsing S5.2, date S5.1.1, domain S5.1.3, path S5.1.4, storage S5.3, retrieval S5.4). No public-suffix list: a Domain attribute is accepted whenever it
//! domain-matches the host (the engine applies its own list when it stores the cookie, and the next change watch corrects the copy).

use atl_platform_ui::webview::Cookie;

/// The parts of a URL the cookie rules read.
#[derive(Debug, PartialEq, Eq)]
pub struct Url {
    /// Lower-case scheme.
    pub scheme: String,
    /// Lower-case host, no port, no brackets' content changed.
    pub host: String,
    /// The path, `/` when empty; no query or fragment.
    pub path: String,
}

impl Url {
    /// Parses `scheme://[userinfo@]host[:port][/path][?q][#f]`; None for anything else (CookieManager answers no cookie / a failed set for such a URL).
    pub fn parse(url: &str) -> Option<Self> {
        let (scheme, rest) = url.split_once("://")?;
        if scheme.is_empty()
            || !scheme
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c))
        {
            return None;
        }
        let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
        let (authority, tail) = rest.split_at(end);
        let hostport = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
        let host = if let Some(v6) = hostport.strip_prefix('[') {
            &hostport[..v6.find(']')? + 2]
        } else {
            hostport.split(':').next().unwrap_or("")
        };
        let scheme = scheme.to_ascii_lowercase();
        if host.is_empty() && scheme != "file" {
            return None;
        }
        let path = tail.split(['?', '#']).next().unwrap_or("");
        Some(Self {
            scheme,
            host: host.to_ascii_lowercase(),
            path: if path.is_empty() {
                "/".into()
            } else {
                path.into()
            },
        })
    }

    fn secure(&self) -> bool {
        matches!(self.scheme.as_str(), "https" | "wss")
    }
}

/// S5.1.3: `host` domain-matches `domain` (both lower case, `domain` without a leading dot).
fn domain_match(host: &str, domain: &str) -> bool {
    host == domain
        || (host.len() > domain.len()
            && host.ends_with(domain)
            && host.as_bytes()[host.len() - domain.len() - 1] == b'.'
            && host.parse::<std::net::IpAddr>().is_err())
}

/// S5.1.4 default-path of a request path.
fn default_path(path: &str) -> String {
    if !path.starts_with('/') {
        return "/".into();
    }
    match path.rfind('/') {
        Some(0) | None => "/".into(),
        Some(i) => path[..i].into(),
    }
}

/// S5.1.4 path-match.
fn path_match(request: &str, cookie: &str) -> bool {
    request == cookie
        || (request.starts_with(cookie)
            && (cookie.ends_with('/') || request.as_bytes().get(cookie.len()) == Some(&b'/')))
}

/// S5.1.1: a cookie date to seconds since the epoch; None = not a date (the attribute is ignored).
fn parse_date(s: &str) -> Option<i64> {
    let delim = |c: char| {
        let b = c as u32;
        b == 0x09
            || (0x20..=0x2F).contains(&b)
            || (0x3B..=0x40).contains(&b)
            || (0x5B..=0x60).contains(&b)
            || (0x7B..=0x7E).contains(&b)
    };
    let (mut time, mut day, mut month, mut year) = (None, None, None, None);
    for tok in s.split(delim).filter(|t| !t.is_empty()) {
        let digits = |t: &str, lo: usize, hi: usize| {
            let n = t.bytes().take_while(u8::is_ascii_digit).count();
            (lo..=hi)
                .contains(&n)
                .then(|| t[..n].parse::<i64>().ok())
                .flatten()
        };
        if time.is_none() {
            let p: Vec<&str> = tok.splitn(3, ':').collect();
            if p.len() == 3
                && let (Some(h), Some(m), Some(sec)) =
                    (digits(p[0], 1, 2), digits(p[1], 1, 2), digits(p[2], 1, 2))
            {
                time = Some((h, m, sec));
                continue;
            }
        }
        if day.is_none()
            && let Some(d) = digits(tok, 1, 2)
        {
            day = Some(d);
            continue;
        }
        if month.is_none() && tok.len() >= 3 {
            const MONTHS: [&str; 12] = [
                "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
            ];
            if let Some(i) = MONTHS.iter().position(|m| tok[..3].eq_ignore_ascii_case(m)) {
                month = Some(i as i64 + 1);
                continue;
            }
        }
        if year.is_none()
            && let Some(y) = digits(tok, 2, 4)
        {
            year = Some(y);
        }
    }
    let (Some((h, mi, sec)), Some(d), Some(mo), Some(mut y)) = (time, day, month, year) else {
        return None;
    };
    if (70..=99).contains(&y) {
        y += 1900;
    } else if (0..=69).contains(&y) {
        y += 2000;
    }
    if !(1..=31).contains(&d) || y < 1601 || h > 23 || mi > 59 || sec > 59 {
        return None;
    }
    Some(days_from_civil(y, mo, d) * 86_400 + h * 3600 + mi * 60 + sec)
}

/// Days since 1970-01-01 of a proleptic Gregorian date (H. Hinnant's days_from_civil).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// S5.2 + S5.3: the cookie a `Set-Cookie` value sets for `url` at `now` (seconds since the epoch); None = ignored. An expiry in the past is kept (storing it deletes the stored one, S5.3 step 11).
pub fn parse_set_cookie(url: &Url, header: &str, now: i64) -> Option<Cookie> {
    let mut parts = header.split(';');
    let pair = parts.next()?;
    let (name, value) = pair.split_once('=')?;
    let (name, value) = (name.trim(), value.trim());
    if name.is_empty() {
        return None;
    }
    let (mut expires, mut max_age, mut domain, mut path, mut secure, mut http_only) =
        (None, None, None, None, false, false);
    for attr in parts {
        let (k, v) = attr.split_once('=').unwrap_or((attr, ""));
        let (k, v) = (k.trim(), v.trim());
        if k.eq_ignore_ascii_case("expires") {
            if let Some(t) = parse_date(v) {
                expires = Some(t);
            }
        } else if k.eq_ignore_ascii_case("max-age") {
            let ok = !v.is_empty()
                && v.trim_start_matches('-')
                    .bytes()
                    .all(|b| b.is_ascii_digit());
            if ok && let Ok(n) = v.parse::<i64>() {
                max_age = Some(if n <= 0 {
                    i64::MIN
                } else {
                    now.saturating_add(n)
                });
            }
        } else if k.eq_ignore_ascii_case("domain") {
            let d = v.trim_start_matches('.').to_ascii_lowercase();
            domain = (!d.is_empty()).then_some(d);
        } else if k.eq_ignore_ascii_case("path") {
            path = v.starts_with('/').then(|| v.to_owned());
        } else if k.eq_ignore_ascii_case("secure") {
            secure = true;
        } else if k.eq_ignore_ascii_case("httponly") {
            http_only = true;
        }
    }
    if secure && !url.secure() {
        return None; // S5.3 step 7 (Secure from an insecure origin), as browsers apply RFC 6265bis
    }
    let domain = match domain {
        Some(d) if domain_match(&url.host, &d) => format!(".{d}"),
        Some(_) => return None,
        None => url.host.clone(),
    };
    Some(Cookie {
        name: name.into(),
        value: value.into(),
        domain,
        path: path.unwrap_or_else(|| default_path(&url.path)),
        expires: max_age.or(expires),
        secure,
        http_only,
    })
}

/// The copy.
#[derive(Default)]
pub struct Jar {
    cookies: Vec<Cookie>,
    /// Writes sent to the engine and not yet confirmed: a full list from the engine may predate them, so it waits for the next one.
    pending: usize,
}

fn expired(c: &Cookie, now: i64) -> bool {
    c.expires.is_some_and(|t| t <= now)
}

impl Jar {
    /// Stores `c` over the cookie with its name, domain and path; an expired `c` only deletes that one (S5.3 step 11).
    pub fn put(&mut self, c: Cookie, now: i64) {
        self.cookies
            .retain(|o| (&o.name, &o.domain, &o.path) != (&c.name, &c.domain, &c.path));
        if !expired(&c, now) {
            self.cookies.push(c);
        }
    }

    /// S5.4: the `Cookie` header for `url` (HttpOnly cookies included: CookieManager is not a script), longer paths first, then older first; None without a match.
    pub fn header(&self, url: &Url, now: i64) -> Option<String> {
        let mut hits: Vec<&Cookie> = self
            .cookies
            .iter()
            .filter(|c| {
                let host_ok = match c.domain.strip_prefix('.') {
                    Some(d) => domain_match(&url.host, d),
                    None => c.domain == url.host,
                };
                host_ok
                    && path_match(&url.path, &c.path)
                    && (!c.secure || url.secure())
                    && !expired(c, now)
            })
            .collect();
        hits.sort_by_key(|c| std::cmp::Reverse(c.path.len())); // stable: insertion order (oldest first) within one length
        (!hits.is_empty()).then(|| {
            hits.iter()
                .map(|c| format!("{}={}", c.name, c.value))
                .collect::<Vec<_>>()
                .join("; ")
        })
    }

    /// Any cookie not expired.
    pub fn any(&self, now: i64) -> bool {
        self.cookies.iter().any(|c| !expired(c, now))
    }

    /// Drops the expired ones.
    pub fn remove_expired(&mut self, now: i64) {
        self.cookies.retain(|c| !expired(c, now));
    }

    /// Drops the session cookies.
    pub fn remove_session(&mut self) {
        self.cookies.retain(|c| c.expires.is_some());
    }

    /// Drops all.
    pub fn clear(&mut self) {
        self.cookies.clear();
    }

    /// Every cookie, for a whole-store write.
    pub fn all(&self) -> &[Cookie] {
        &self.cookies
    }

    /// A write went to the engine.
    pub fn begin_write(&mut self) {
        self.pending += 1;
    }

    /// The engine confirmed a write.
    pub fn end_write(&mut self) {
        self.pending = self.pending.saturating_sub(1);
    }

    /// The engine's full list after a change: taken unless a write is still in flight (that write's own change brings a newer list). True = taken.
    pub fn received(&mut self, list: Vec<Cookie>) -> bool {
        if self.pending > 0 {
            return false;
        }
        self.cookies = list;
        true
    }
}

// ---- the process's CookieManager: the copy plus the engine ----

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard};

use atl_platform_ui::PlatformWebEngine;
use atl_platform_ui::webview::{CookiePolicy, WebEngine as _};

static JAR: Mutex<Jar> = Mutex::new(Jar {
    cookies: Vec::new(),
    pending: 0,
});
/// CookieManager.setAcceptCookie; true by default (Chromium's).
static ACCEPT: AtomicBool = AtomicBool::new(true);
/// Live WebViews whose setAcceptThirdPartyCookies is true: the engine has one policy for the whole store, so third-party cookies are accepted while any such view lives.
static THIRD_PARTY_VIEWS: AtomicUsize = AtomicUsize::new(0);
/// CookieManager.setAcceptFileSchemeCookies; false by default (CookieManager.java javadoc).
static FILE_SCHEME: AtomicBool = AtomicBool::new(false);

fn jar() -> MutexGuard<'static, Jar> {
    JAR.lock().unwrap_or_else(|p| p.into_inner())
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// Starts the engine's change watch and pushes the policy, once per process (the first CookieManager or WebView); a failure (no UI thread) is logged and the copy stays app-only.
pub fn start() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let watch = PlatformWebEngine::watch_cookies(Box::new(|list| {
            jar().received(list);
        }));
        if let Err(e) = watch {
            tracing::warn!("webview: cookie watch: {e}");
        }
        push_policy();
    });
}

fn push_policy() {
    let policy = if !ACCEPT.load(Ordering::Relaxed) {
        CookiePolicy::Never
    } else if THIRD_PARTY_VIEWS.load(Ordering::Relaxed) > 0 {
        CookiePolicy::Always
    } else {
        CookiePolicy::NoThirdParty
    };
    if let Err(e) = PlatformWebEngine::set_cookie_policy(policy) {
        tracing::warn!("webview: cookie policy: {e}");
    }
}

/// A URL the store serves: parsed, and a `file:` one only while file-scheme cookies are on.
fn served(url: &str) -> Option<Url> {
    Url::parse(url).filter(|u| u.scheme != "file" || FILE_SCHEME.load(Ordering::Relaxed))
}

/// CookieManager.setCookie: the copy has it on return; `done(true)` once the engine has it, `done(false)` for a value that sets nothing.
pub fn set(url: &str, value: &str, done: Box<dyn FnOnce(bool) + Send>) {
    let now = now();
    let Some(c) = served(url).and_then(|u| parse_set_cookie(&u, value, now)) else {
        return done(false);
    };
    let mut j = jar();
    j.put(c.clone(), now);
    j.begin_write();
    drop(j);
    let sent = PlatformWebEngine::add_cookie(
        &c,
        Box::new(move |ok| {
            jar().end_write();
            done(ok);
        }),
    );
    if let Err(e) = sent {
        tracing::warn!("webview: setCookie: {e}");
        jar().end_write();
    }
}

/// CookieManager.getCookie: from the copy, any thread.
pub fn get(url: &str) -> Option<String> {
    served(url).and_then(|u| jar().header(&u, now()))
}

/// CookieManager.hasCookies.
pub fn any() -> bool {
    jar().any(now())
}

/// CookieManager.removeExpiredCookie: the engine drops expired cookies itself; the copy follows.
pub fn remove_expired() {
    jar().remove_expired(now());
}

/// CookieManager.removeAllCookies (`all`) / removeSessionCookies: the copy at once, then the engine gets the copy's whole content; `done(true)` when something was removed (the javadoc's callback value).
pub fn remove(all: bool, done: Box<dyn FnOnce(bool) + Send>) {
    let mut j = jar();
    let before = j.all().len();
    if all {
        j.clear();
    } else {
        j.remove_session();
    }
    let removed = j.all().len() != before;
    let keep = j.all().to_vec();
    j.begin_write();
    drop(j);
    let sent = PlatformWebEngine::replace_cookies(
        &keep,
        Box::new(move |_| {
            jar().end_write();
            done(removed);
        }),
    );
    if let Err(e) = sent {
        tracing::warn!("webview: remove cookies: {e}");
        jar().end_write();
    }
}

/// CookieManager.setAcceptCookie.
pub fn set_accept(on: bool) {
    ACCEPT.store(on, Ordering::Relaxed);
    push_policy();
}

/// CookieManager.acceptCookie.
pub fn accept() -> bool {
    ACCEPT.load(Ordering::Relaxed)
}

/// A WebView's third-party flag turned on (`true`) or off (or the view went away while on).
pub fn third_party(on: bool) {
    if on {
        THIRD_PARTY_VIEWS.fetch_add(1, Ordering::Relaxed);
    } else {
        let _ = THIRD_PARTY_VIEWS
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_sub(1));
    }
    push_policy();
}

/// CookieManager.setAcceptFileSchemeCookies.
pub fn set_file_scheme(on: bool) {
    FILE_SCHEME.store(on, Ordering::Relaxed);
}

/// CookieManager.allowFileSchemeCookies.
pub fn file_scheme() -> bool {
    FILE_SCHEME.load(Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    /// 2015-10-21T07:28:00Z, computed independently: 16729 days (1970-01-01..2015-10-21) * 86400 + 7*3600 + 28*60.
    const T_2015: i64 = 16_729 * 86_400 + 7 * 3600 + 28 * 60;

    #[test]
    fn url_parts() {
        assert_eq!(
            url("HTTPS://user@Ads.Example.com:8443/a/b?x=1#f"),
            Url {
                scheme: "https".into(),
                host: "ads.example.com".into(),
                path: "/a/b".into()
            }
        );
        assert_eq!(url("http://example.com").path, "/");
        assert!(Url::parse("not a url").is_none());
    }

    #[test]
    fn dates_rfc6265_forms() {
        // RFC 6265 S4.1.1's example form, RFC 850 form, asctime form: the same instant
        assert_eq!(parse_date("Wed, 21 Oct 2015 07:28:00 GMT"), Some(T_2015));
        assert_eq!(
            parse_date("Wednesday, 21-Oct-15 07:28:00 GMT"),
            Some(T_2015)
        );
        assert_eq!(parse_date("Wed Oct 21 07:28:00 2015"), Some(T_2015));
        assert_eq!(parse_date("tomorrow"), None);
    }

    #[test]
    fn set_cookie_attributes() {
        let u = url("https://www.example.com/dir/page");
        let c = parse_set_cookie(
            &u,
            "id=a3fWa; Max-Age=60; Domain=.Example.com; Secure; HttpOnly",
            1000,
        )
        .unwrap();
        assert_eq!(c.domain, ".example.com");
        assert_eq!(c.path, "/dir"); // S5.1.4 default-path of /dir/page
        assert_eq!(c.expires, Some(1060));
        assert!(c.secure && c.http_only);
        // Max-Age wins over Expires (S5.3 step 3)
        assert_eq!(
            parse_set_cookie(
                &u,
                "a=b; Expires=Wed, 21 Oct 2015 07:28:00 GMT; Max-Age=5",
                0
            )
            .unwrap()
            .expires,
            Some(5)
        );
        // a foreign Domain is ignored as a whole cookie (S5.3 step 6)
        assert!(parse_set_cookie(&u, "a=b; Domain=other.com", 0).is_none());
        // no Domain: host-only
        assert_eq!(
            parse_set_cookie(&u, "a=b", 0).unwrap().domain,
            "www.example.com"
        );
        // Secure from http: dropped
        assert!(parse_set_cookie(&url("http://example.com/"), "a=b; Secure", 0).is_none());
    }

    #[test]
    fn header_matches_domain_path_secure() {
        let mut j = Jar::default();
        let set = |j: &mut Jar, u: &str, h: &str| {
            let c = parse_set_cookie(&url(u), h, 0).unwrap();
            j.put(c, 0);
        };
        set(&mut j, "https://example.com/", "root=1; Domain=example.com");
        set(&mut j, "https://example.com/a/x", "deep=2; Path=/a");
        set(&mut j, "https://example.com/", "host=3");
        set(&mut j, "https://example.com/", "sec=4; Secure");
        assert_eq!(
            j.header(&url("https://example.com/a/b"), 0).as_deref(),
            Some("deep=2; root=1; host=3; sec=4")
        );
        // subdomain: the Domain cookie only; http: no Secure one; /ab does not path-match /a
        assert_eq!(
            j.header(&url("http://sub.example.com/ab"), 0).as_deref(),
            Some("root=1")
        );
        assert_eq!(j.header(&url("https://other.com/"), 0), None);
    }

    #[test]
    fn put_replaces_and_expired_deletes() {
        let u = url("https://example.com/");
        let mut j = Jar::default();
        j.put(parse_set_cookie(&u, "a=1", 0).unwrap(), 0);
        j.put(parse_set_cookie(&u, "a=2", 0).unwrap(), 0);
        assert_eq!(j.header(&u, 0).as_deref(), Some("a=2"));
        j.put(parse_set_cookie(&u, "a=x; Max-Age=0", 0).unwrap(), 0);
        assert!(!j.any(0));
    }

    #[test]
    fn engine_list_waits_for_pending_writes() {
        let u = url("https://example.com/");
        let mut j = Jar::default();
        j.begin_write();
        j.put(parse_set_cookie(&u, "mine=1", 0).unwrap(), 0);
        assert!(!j.received(vec![]));
        assert!(j.any(0));
        j.end_write();
        assert!(j.received(vec![]));
        assert!(!j.any(0));
    }
}
