// SPDX-License-Identifier: MIT
//! Release-config flags a codec's format depends on, as aconfig itself evaluated them: the converter puts the flag dump of each `aconfig` declarations module (Cargo.toml) in OUT_DIR as
//! `<module>.aconfig.txt`, lines `<package>.<flag>:<permission>=<true|false>` (aconfig dump-cache). Only a READ_ONLY flag is a build constant (framework.jar compiles the same value in);
//! a missing flag or a READ_WRITE one fails the build here, at compile time.

/// The value of READ_ONLY flag `name` in `dump`; a compile-time panic otherwise.
pub const fn read_only_flag(dump: &str, name: &str) -> bool {
    let d = dump.as_bytes();
    let n = name.as_bytes();
    let mut line = 0;
    while line < d.len() {
        let mut end = line;
        while end < d.len() && d[end] != b'\n' {
            end += 1;
        }
        if starts_with(d, line, end, n) && line + n.len() < end && d[line + n.len()] == b':' {
            let rest = line + n.len() + 1;
            if equals(d, rest, end, b"READ_ONLY=true") {
                return true;
            }
            if equals(d, rest, end, b"READ_ONLY=false") {
                return false;
            }
            panic!("aconfig flag is not READ_ONLY: its value is not a build constant");
        }
        line = end + 1;
    }
    panic!("aconfig flag missing from the dump");
}

const fn starts_with(d: &[u8], from: usize, end: usize, p: &[u8]) -> bool {
    if end - from < p.len() {
        return false;
    }
    let mut i = 0;
    while i < p.len() {
        if d[from + i] != p[i] {
            return false;
        }
        i += 1;
    }
    true
}

const fn equals(d: &[u8], from: usize, end: usize, p: &[u8]) -> bool {
    end - from == p.len() && starts_with(d, from, end, p)
}

/// android.security (frameworks/base/AconfigFlags.bp `android.security.flags-aconfig`).
const SECURITY: &str = include_str!(concat!(
    env!("OUT_DIR"),
    "/android.security.flags-aconfig.aconfig.txt"
));

/// `android.security.Flags.preventIntentRedirect()`: Intent's creator-token block (codecs/android/content/Intent.rs).
pub const PREVENT_INTENT_REDIRECT: bool =
    read_only_flag(SECURITY, "android.security.prevent_intent_redirect");
