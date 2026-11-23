// SPDX-License-Identifier: MIT
//! `/etc/atl/vendor.prop`: this host's own vendor properties, build.prop syntax, applied after the bundled props so its values win.
//! AOSP's device states what only the device knows in its vendor build.prop; here the host states it in its system config (Linux convention).
//! Only the keys of [`HOST_KEYS`] are taken. A missing file changes nothing; a refused line costs that line only, and atld logs it.
//! Plumbing: atld reads the file once in `main`, logs the refusals, applies the accepted props over the bundled ones ([`apply`]) and logs the multiplier's source ([`hw_multiplier`]).

use std::io::ErrorKind;
use std::path::Path;

use atl_property_service::{Property, parse_build_prop};
use atl_services_types::hw_timeouts::HwTimeouts;

/// The host's file. A fixed path, not a flag or variable: the file is the host's own state, and no switch in product code selects it. Tests pass their own path to [`read`].
pub const HOST_VENDOR_PROP: &str = "/etc/atl/vendor.prop";

/// AOSP's scale of the framework's timeouts on hardware slower than a phone (Build.java:1744-1745).
pub const HW_MULTIPLIER: &str = "ro.hw_timeout_multiplier";

/// A key the host file may set, with the rule its value must pass.
pub struct HostKey {
    pub name: &'static str,
    pub valid: fn(&str) -> bool,
}

/// The only keys the host file may set. An allowlist, not a denylist: device identity (`ro.product.*`, `ro.build.*`, `ro.debuggable`) is the image's to state, so a host file cannot forge it.
/// A key added here needs its own value rule and a reason it is the host's to declare.
pub const HOST_KEYS: &[HostKey] = &[HostKey {
    name: HW_MULTIPLIER,
    valid: |v| HwTimeouts::parse_arg(v).is_ok(),
}];

/// What the host file contributed: its accepted props (one per name, the last line wins), and one message per refused line or unreadable file. Messages name keys, never values.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct HostProps {
    pub props: Vec<Property>,
    pub refused: Vec<String>,
}

/// The file at `path`. Missing: nothing, silently. Any other read error: one message, nothing taken.
pub fn read(path: &Path) -> HostProps {
    match std::fs::read(path) {
        Ok(bytes) => parse(&bytes),
        Err(e) if e.kind() == ErrorKind::NotFound => HostProps::default(),
        Err(e) => HostProps {
            props: Vec::new(),
            refused: vec![format!("unreadable, not taken: {e}")],
        },
    }
}

/// `bytes` as the host file. Each line is judged alone: an allowlisted key with a valid value is taken; anything else costs that line.
pub fn parse(bytes: &[u8]) -> HostProps {
    let mut out = HostProps::default();
    for (i, raw) in bytes.split(|&b| b == b'\n').enumerate() {
        let line = i + 1;
        let Ok(text) = std::str::from_utf8(raw) else {
            out.refused.push(format!("line {line}: not UTF-8, skipped"));
            continue;
        };
        let Ok(pairs) = parse_build_prop(text) else {
            out.refused
                .push(format!("line {line}: not name=value, skipped"));
            continue;
        };
        for (name, value) in pairs {
            let Some(key) = HOST_KEYS.iter().find(|k| k.name == name) else {
                out.refused
                    .push(format!("line {line}: {name}: not a host key, ignored"));
                continue;
            };
            if !(key.valid)(&value) {
                out.refused
                    .push(format!("line {line}: {name}: value rejected, skipped"));
                continue;
            }
            match out.props.iter_mut().find(|(n, _)| *n == name) {
                Some(taken) => taken.1 = value,
                None => out.props.push((name, value)),
            }
        }
    }
    out
}

/// The bundled props with the host's applied: a host value replaces a bundled one of the same name (init's later set wins); a new name is appended.
/// No name appears twice: bionic's property area refuses a duplicate, which would fail the whole area.
pub fn apply(mut bundled: Vec<Property>, host: &[Property]) -> Vec<Property> {
    bundled.retain(|(n, _)| !host.iter().any(|(h, _)| h == n));
    bundled.extend(host.iter().cloned());
    bundled
}

/// The multiplier in effect and its source, as atld logs it: `props` is the applied set, so its last `ro.hw_timeout_multiplier` is what readers see; `host` says whether the host file set it.
pub fn hw_multiplier(props: &[Property], host: &[Property]) -> (HwTimeouts, String) {
    let value = props
        .iter()
        .rev()
        .find(|(n, _)| n == HW_MULTIPLIER)
        .map(|(_, v)| v.as_str());
    let source = match value {
        None => "default",
        Some(_) if host.iter().any(|(n, _)| n == HW_MULTIPLIER) => "from /etc/atl/vendor.prop",
        Some(_) => "from build.prop",
    };
    (HwTimeouts::from_prop(value), format!("({source})"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair(name: &str, value: &str) -> Property {
        (name.to_owned(), value.to_owned())
    }

    #[test]
    fn allowlist_takes_only_the_declared_key() {
        let got = parse(b"ro.product.model=Pixel\nro.hw_timeout_multiplier=2\nro.build.id=x\n");
        assert_eq!(got.props, vec![pair(HW_MULTIPLIER, "2")]);
        // a refused line names its key; the value stays out of the log
        assert_eq!(
            got.refused,
            vec![
                "line 1: ro.product.model: not a host key, ignored",
                "line 3: ro.build.id: not a host key, ignored",
            ]
        );
    }

    #[test]
    fn value_must_pass_the_key_rule() {
        let got = parse(b"ro.hw_timeout_multiplier=two\nro.hw_timeout_multiplier=0\nro.hw_timeout_multiplier=1\n");
        // 1 is AOSP's unset value and the lowest multiplier: accepted
        assert_eq!(got.props, vec![pair(HW_MULTIPLIER, "1")]);
        assert_eq!(
            got.refused,
            vec![
                "line 1: ro.hw_timeout_multiplier: value rejected, skipped",
                "line 2: ro.hw_timeout_multiplier: value rejected, skipped",
            ]
        );
    }

    #[test]
    fn last_line_wins_within_the_file() {
        let got = parse(b"ro.hw_timeout_multiplier=2\nro.hw_timeout_multiplier=3\n");
        assert_eq!(got.props, vec![pair(HW_MULTIPLIER, "3")]);
        assert!(got.refused.is_empty());
    }

    #[test]
    fn malformed_lines_cost_only_themselves() {
        let got = parse(b"# host\n\nno equals sign\nro.hw_timeout_multiplier=3\n\xff=1\nro.hw_timeout_multiplier?=5\n");
        assert_eq!(got.props, vec![pair(HW_MULTIPLIER, "3")]);
        assert_eq!(
            got.refused,
            vec![
                "line 3: not name=value, skipped",
                "line 5: not UTF-8, skipped",
                "line 6: not name=value, skipped",
            ]
        );
    }

    #[test]
    fn missing_file_changes_nothing_and_says_nothing() {
        let path = std::env::temp_dir().join(format!("atld-no-vendor-prop-{}", std::process::id()));
        assert_eq!(read(&path), HostProps::default());
    }

    #[test]
    fn unreadable_file_is_one_message() {
        // a directory: the read fails with an error other than NotFound
        let got = read(&std::env::temp_dir());
        assert!(got.props.is_empty());
        assert_eq!(got.refused.len(), 1);
    }

    #[test]
    fn file_is_read_from_the_given_path() {
        let path = std::env::temp_dir().join(format!("atld-vendor-prop-{}", std::process::id()));
        std::fs::write(&path, "ro.hw_timeout_multiplier=2\n").expect("write the test file");
        let got = read(&path);
        std::fs::remove_file(&path).expect("remove the test file");
        assert_eq!(
            got,
            HostProps {
                props: vec![pair(HW_MULTIPLIER, "2")],
                refused: Vec::new(),
            }
        );
    }

    #[test]
    fn host_value_replaces_the_bundled_one() {
        let bundled = vec![pair("ro.build.id", "BP2A"), pair(HW_MULTIPLIER, "3")];
        let host = parse(b"ro.hw_timeout_multiplier=2\n").props;
        let props = apply(bundled, &host);
        assert_eq!(
            props,
            vec![pair("ro.build.id", "BP2A"), pair(HW_MULTIPLIER, "2")]
        );
        let (hw, source) = hw_multiplier(&props, &host);
        assert_eq!(hw.to_arg(), "2");
        assert_eq!(source, "(from /etc/atl/vendor.prop)");
    }

    #[test]
    fn bundled_value_stands_when_the_host_line_is_refused() {
        let host = parse(b"ro.hw_timeout_multiplier=two\n").props;
        let props = apply(vec![pair(HW_MULTIPLIER, "3")], &host);
        let (hw, source) = hw_multiplier(&props, &host);
        assert_eq!(hw.to_arg(), "3");
        assert_eq!(source, "(from build.prop)");
    }

    #[test]
    fn unset_is_the_default() {
        // AOSP's unset value, Build.java:1745
        let (hw, source) = hw_multiplier(&[], &[]);
        assert_eq!(hw.to_arg(), "1");
        assert_eq!(source, "(default)");
    }
}
