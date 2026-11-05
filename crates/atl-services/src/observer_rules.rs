// SPDX-License-Identifier: MIT
//! Which registered content observers hear a notification: ContentService.ObserverNode.collectMyObserversLocked (ContentService.java:1858-1904), flattened from AOSP's per-segment tree into one list of [`Watch`]es.
//! Segments are compared as written; AOSP compares decoded ones, and the M2 URIs carry no escapes.

/// ContentResolver.NOTIFY_SKIP_NOTIFY_FOR_DESCENDANTS (ContentResolver.java:685): a leaf notification skips the observers that asked for descendants.
pub const SKIP_NOTIFY_FOR_DESCENDANTS: i32 = 1 << 1;
/// UserHandle.USER_ALL (UserHandle.java:52): every user, as a registration's or a notification's user.
pub const USER_ALL: i32 = -1;

/// The segments AOSP counts for a URI (ObserverNode.countUriSegments): the authority, then each non-empty path segment. The query and fragment are not segments.
pub fn segments(uri: &str) -> Vec<&str> {
    let uri = uri.split(['?', '#']).next().unwrap_or_default();
    let body = uri.split_once(':').map_or(uri, |(_, rest)| rest);
    let body = body.strip_prefix("//").unwrap_or(body);
    let mut parts = body.split('/');
    let authority = parts.next().unwrap_or_default();
    std::iter::once(authority)
        .chain(parts.filter(|s| !s.is_empty()))
        .collect()
}

/// One registration: the URI's segments, whether its observer asked for descendants, and the user it watches (USER_ALL: every user).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Watch {
    pub segments: Vec<String>,
    pub descendants: bool,
    pub user: i32,
}

impl Watch {
    /// The registration of `uri` (see [`segments`]).
    pub fn new(uri: &str, descendants: bool, user: i32) -> Self {
        Watch {
            segments: segments(uri).into_iter().map(str::to_owned).collect(),
            descendants,
            user,
        }
    }

    /// Whether a notification of `notified` at `flags`, for user `target`, reaches this observer. The users match or one is USER_ALL. The registered URI is the notified one or a
    /// segment-wise prefix of it. At the same URI the observer hears it unless the notification skips descendants' observers and this one asked for them; above it, only if it asked.
    pub fn hears(&self, notified: &[&str], flags: i32, target: i32) -> bool {
        let user = target == USER_ALL || self.user == USER_ALL || target == self.user;
        let prefix = self.segments.len() <= notified.len()
            && self
                .segments
                .iter()
                .zip(notified)
                .all(|(s, n)| s.as_str() == *n);
        if !user || !prefix {
            return false;
        }
        if self.segments.len() == notified.len() {
            return !(self.descendants && (flags & SKIP_NOTIFY_FOR_DESCENDANTS) != 0);
        }
        self.descendants
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONFIG: &str = "content://settings/config";

    // expected values are read off ContentService.java's rules, not off `hears`

    #[test]
    fn segments_are_authority_then_path() {
        assert_eq!(
            segments("content://settings/config/ns"),
            ["settings", "config", "ns"]
        );
        assert_eq!(
            segments("content://settings/config/"),
            ["settings", "config"]
        );
        assert_eq!(
            segments("content://settings/config?x=1#f"),
            ["settings", "config"]
        );
        assert_eq!(segments("content://settings"), ["settings"]);
    }

    #[test]
    fn same_uri_hears_unless_skipped_for_descendants() {
        let leaf = segments(CONFIG);
        assert!(Watch::new(CONFIG, false, 0).hears(&leaf, 0, 0));
        assert!(Watch::new(CONFIG, false, 0).hears(&leaf, SKIP_NOTIFY_FOR_DESCENDANTS, 0));
        assert!(Watch::new(CONFIG, true, 0).hears(&leaf, 0, 0));
        assert!(!Watch::new(CONFIG, true, 0).hears(&leaf, SKIP_NOTIFY_FOR_DESCENDANTS, 0));
    }

    #[test]
    fn below_the_uri_hears_only_when_asked() {
        let below = segments("content://settings/config/ns/key");
        assert!(Watch::new(CONFIG, true, 0).hears(&below, 0, 0));
        assert!(!Watch::new(CONFIG, false, 0).hears(&below, 0, 0));
        // the skip flag drops leaf observers only
        assert!(Watch::new(CONFIG, true, 0).hears(&below, SKIP_NOTIFY_FOR_DESCENDANTS, 0));
    }

    #[test]
    fn siblings_and_ancestors_do_not_hear() {
        assert!(!Watch::new(CONFIG, true, 0).hears(&segments("content://settings/global"), 0, 0));
        assert!(!Watch::new(CONFIG, true, 0).hears(&segments("content://other/config"), 0, 0));
        assert!(!Watch::new("content://settings/config/ns", true, 0).hears(
            &segments(CONFIG),
            0,
            0
        ));
    }

    #[test]
    fn users_match_or_are_all() {
        let leaf = segments(CONFIG);
        assert!(Watch::new(CONFIG, false, 10).hears(&leaf, 0, 10));
        assert!(!Watch::new(CONFIG, false, 10).hears(&leaf, 0, 0));
        assert!(Watch::new(CONFIG, false, USER_ALL).hears(&leaf, 0, 0));
        assert!(Watch::new(CONFIG, false, 0).hears(&leaf, 0, USER_ALL));
    }
}
