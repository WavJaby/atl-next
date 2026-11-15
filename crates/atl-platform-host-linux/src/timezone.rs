// SPDX-License-Identifier: MIT
//! The desktop's time zone as a native Linux program resolves it (`TZ`, else `/etc/localtime`); the pure rule is `atl_platform_host_api::zone_id_from`.

use std::path::Path;

/// The zone id this host's native programs run in; `None` where glibc names no zone file.
pub fn time_zone() -> Option<String> {
    atl_platform_host_api::zone_id_from(
        std::env::var_os("TZ").map(|tz| tz.to_string_lossy().into_owned()),
        std::fs::canonicalize("/etc/localtime").ok(),
        |id| Path::new("/usr/share/zoneinfo").join(id).is_file(),
    )
}
