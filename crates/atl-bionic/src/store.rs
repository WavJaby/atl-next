// SPDX-License-Identifier: MIT
//! The process's property store: one area, mapped on first use.

use std::path::PathBuf;
use std::sync::OnceLock;

use rustix::io::Errno;

use crate::generated::system_properties::PROP_DIRNAME;
use crate::props::{Area, PropInfo, VALUE_MAX, value_len};

/// Environment variable naming the area file; unset = bionic's `PROP_DIRNAME`. Set by whoever starts
/// the process until the broker puts the file at `PROP_DIRNAME` inside the sandbox.
pub const AREA_ENV: &str = "ATL_PROP_AREA";

static AREA: OnceLock<Option<Area<'static>>> = OnceLock::new();

/// The area, or none: no file (a desktop without the sandbox), or not bionic's format. Without an area
/// every property is unset, as on bionic before `__system_properties_init`.
pub fn area() -> Option<Area<'static>> {
    *AREA.get_or_init(|| {
        let path =
            std::env::var_os(AREA_ENV).map_or_else(|| PathBuf::from(PROP_DIRNAME), PathBuf::from);
        Area::new(crate::map::map_shared(&path).ok()?).ok()
    })
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
