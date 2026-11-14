// SPDX-License-Identifier: MIT
//! The process's property store: one area, mapped by `__system_properties_init` or on first use.
//!
//! bionic maps the area in `__libc_init_common` (`SystemProperties::Init`); a process here has no such
//! hook, so the first reader maps it. A failed first use is kept (no open per lookup on a desktop without
//! the area); an explicit `init` retries until one succeeds, as bionic's `Init` does (it sets
//! `initialized_` only on success). After success, `init` is a no-op returning success.

use std::borrow::Cow;
use std::fmt::Write;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering::{Acquire, Release};
use std::sync::{Mutex, OnceLock};

use atl_android_paths::PROPERTY_AREA_ENV;
use rustix::io::Errno;

use crate::generated::system_properties::{PROP_DIRNAME, PROP_NAME_MAX};
use crate::props::{Area, PropInfo, VALUE_MAX, value_len};

static AREA: OnceLock<Area<'static>> = OnceLock::new();
/// A first use already tried and failed.
static TRIED: AtomicBool = AtomicBool::new(false);
/// Serialises mapping attempts: two would leak a mapping (never unmapped).
static MAPPING: Mutex<()> = Mutex::new(());

/// Maps the area unless mapped; the area, or none: no file, or not bionic's format.
fn map() -> Option<Area<'static>> {
    let _guard = MAPPING.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(a) = AREA.get() {
        return Some(*a);
    }
    let path = std::env::var_os(PROPERTY_AREA_ENV)
        .map_or_else(|| PathBuf::from(PROP_DIRNAME), PathBuf::from);
    let area = Area::new(crate::map::map_shared(&path).ok()?).ok()?;
    Some(*AREA.get_or_init(|| area))
}

/// The area, or none. Without one every property is unset, as on bionic before `__system_properties_init`.
pub fn area() -> Option<Area<'static>> {
    if let Some(a) = AREA.get() {
        return Some(*a);
    }
    if TRIED.load(Acquire) {
        return None;
    }
    let area = map();
    TRIED.store(true, Release);
    area
}

/// bionic `SystemProperties::Init`: true once the area is mapped (now or before).
pub fn init() -> bool {
    map().is_some()
}

/// bionic `SystemProperties::Find`: the area and `name`'s record. The one lookup every reader takes (`__system_property_find`, [`get`]), so a name nothing sets is counted
/// here once per read as `prop:miss <name>` -- also without an area, where every property is unset.
pub fn find(name: &[u8]) -> Option<(Area<'static>, PropInfo)> {
    lookup(area(), name)
}

fn lookup<'a>(area: Option<Area<'a>>, name: &[u8]) -> Option<(Area<'a>, PropInfo)> {
    let found = area.and_then(|a| Some((a, a.find(name)?)));
    if found.is_none() {
        miss(name);
    }
    found
}

/// bionic `SystemProperties::Get`: the value of `name` into `out` (empty if unset), its length.
pub fn get(name: &[u8], out: &mut [u8; VALUE_MAX]) -> usize {
    match find(name) {
        Some((a, pi)) => read(a, pi, out),
        None => {
            out[0] = 0;
            0
        }
    }
}

/// bionic `SystemProperties::Read` without the name: the value's length. The one value delivery of the copying readers; an empty value counts as a miss of its name (AOSP readers treat
/// "" as unset), and a read follows a successful find, so no read is counted twice.
pub fn read(area: Area, pi: PropInfo, out: &mut [u8; VALUE_MAX]) -> usize {
    let len = value_len(area.read(pi, out));
    delivered(area, pi, len == 0);
    len
}

/// Counts a delivered value that is `empty` as a miss of `pi`'s name: [`read`]'s rule, for readers that hand the value out without copying it (`__system_property_read_callback`).
pub fn delivered(area: Area, pi: PropInfo, empty: bool) {
    if empty {
        let mut name = [0u8; NAME_MAX];
        let n = area.strlcpy(pi.name_offset(), &mut name).min(NAME_MAX - 1);
        miss(&name[..n]);
    }
}

/// `PROP_NAME_MAX`, the name buffer bionic's readers fill.
const NAME_MAX: usize = PROP_NAME_MAX as usize;

/// Counts `prop:miss <name>`; the count. Key = the name only (plan M59).
fn miss(name: &[u8]) -> u64 {
    atl_diag::count(atl_diag::PROP_MISS, &key(name), None)
}

/// The name as a line field: borrowed when every byte is printable ASCII other than space and `\` (every bionic-valid name), else those bytes as `\xHH`, so the line parser stays unambiguous.
fn key(name: &[u8]) -> Cow<'_, str> {
    let plain = |b: u8| b.is_ascii_graphic() && b != b'\\';
    match std::str::from_utf8(name) {
        Ok(s) if name.iter().all(|&b| plain(b)) => Cow::Borrowed(s),
        _ => Cow::Owned(name.iter().fold(String::new(), |mut s, &b| {
            if plain(b) {
                s.push(char::from(b));
            } else {
                let _ = write!(s, "\\x{b:02x}");
            }
            s
        })),
    }
}

/// `__system_property_set`: no property service yet (atld's setprop path), so it fails as bionic does
/// when its service socket is absent: `connect` reports ENOENT.
pub fn set(_name: &[u8], _value: &[u8]) -> Result<(), Errno> {
    Err(Errno::NOENT)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image::Image;
    use crate::props::tests::words;

    fn misses(name: &str) -> u64 {
        atl_diag::counts()
            .into_iter()
            .find(|(o, a, _)| o == atl_diag::PROP_MISS && a == name)
            .map_or(0, |(_, _, n)| n)
    }

    /// Area `a.b=x`, `a.e=""`: 5 finds of the unset `a.c` -> 5 (lines at 1, 2, 4: atl-diag's `five_hits_log_three_lines_and_total_five`); a read of the empty `a.e` -> 1; `a.b` found and
    /// read with a value -> none. Break check: drop the count in `lookup` -> `a.c` reads 0.
    #[test]
    fn unset_and_empty_count_set_does_not() {
        let mut image = Image::new();
        image.add("a.b", "x").expect("fits");
        image.add("a.e", "").expect("fits");
        let w = words(&image.into_bytes());
        let area = Area::new(&w).expect("bionic's format");
        for _ in 0..5 {
            assert!(lookup(Some(area), b"a.c").is_none());
        }
        assert_eq!(misses("a.c"), 5);
        let mut out = [0u8; VALUE_MAX];
        let (a, pi) = lookup(Some(area), b"a.e").expect("a.e is set");
        assert_eq!(read(a, pi, &mut out), 0);
        assert_eq!(misses("a.e"), 1);
        let (a, pi) = lookup(Some(area), b"a.b").expect("a.b is set");
        assert_eq!(read(a, pi, &mut out), 1);
        assert_eq!(misses("a.b"), 0);
        // no area: every property is unset, so a miss
        assert!(lookup(None, b"a.noarea").is_none());
        assert_eq!(misses("a.noarea"), 1);
    }

    /// S4's escaping: space, `\` and non-ASCII bytes as `\xHH` (0x20, 0x5c, 0xff); a bionic-valid name borrowed.
    #[test]
    fn key_escapes_what_the_parser_splits_on() {
        assert!(matches!(key(b"ro.a_b-c:d@e"), Cow::Borrowed("ro.a_b-c:d@e")));
        assert_eq!(key(b"a b\\c\xff"), "a\\x20b\\x5cc\\xff");
    }
}
