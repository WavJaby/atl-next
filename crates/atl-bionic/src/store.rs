// SPDX-License-Identifier: MIT
//! The process's property store: one area, mapped by `__system_properties_init` or on first use.
//!
//! bionic maps the area in `__libc_init_common` (`SystemProperties::Init`); a process here has no such
//! hook, so the first reader maps it. A failed first use is kept (no open per lookup on a desktop without
//! the area); an explicit `init` retries until one succeeds, as bionic's `Init` does (it sets
//! `initialized_` only on success). After success, `init` is a no-op returning success.

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering::{Acquire, Release};
use std::sync::{Mutex, OnceLock};

use rustix::io::Errno;

use crate::generated::system_properties::PROP_DIRNAME;
use crate::props::{Area, PropInfo, VALUE_MAX, value_len};

/// Environment variable naming the area file; unset = bionic's `PROP_DIRNAME`. Set by whoever starts
/// the process until the broker puts the file at `PROP_DIRNAME` inside the sandbox.
pub const AREA_ENV: &str = "ATL_PROP_AREA";

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
    let path =
        std::env::var_os(AREA_ENV).map_or_else(|| PathBuf::from(PROP_DIRNAME), PathBuf::from);
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

/// bionic `SystemProperties::Get`: the value of `name` into `out` (empty if unset), its length.
pub fn get(name: &[u8], out: &mut [u8; VALUE_MAX]) -> usize {
    match area().and_then(|a| Some((a, a.find(name)?))) {
        Some((a, pi)) => read(a, pi, out),
        None => {
            out[0] = 0;
            0
        }
    }
}

/// bionic `SystemProperties::Read` without the name: the value's length.
pub fn read(area: Area, pi: PropInfo, out: &mut [u8; VALUE_MAX]) -> usize {
    value_len(area.read(pi, out))
}

/// `__system_property_set`: no property service yet (atld's setprop path), so it fails as bionic does
/// when its service socket is absent: `connect` reports ENOENT.
pub fn set(_name: &[u8], _value: &[u8]) -> Result<(), Errno> {
    Err(Errno::NOENT)
}
