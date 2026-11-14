// SPDX-License-Identifier: MIT
//! logd's write socket for this process: `/dev/socket/logdw`, where a liblog without our logger sends (`__android_log_logd_logger`, the `__ANDROID__` default, logger_write.cpp).
//! Such a liblog is the native-bridge guest's: AOSP liblog built for the guest (NBS `NATIVE_BRIDGE_ORIG_GUEST_LIBS`), whose logger we cannot set from the host. Its records go to the same sink as the
//! host liblog's ([`log::write`]), so one app log holds both. The receiver is atl-services-types `logcat::serve_logdw`.
//! The broker makes `/dev/socket` (lib.rs SOCKET_DIR); a launcher started without it says so once and runs without the socket: guest log lines are then lost, as on Android without logd.

use std::path::Path;

use anyhow::Result;

use crate::log;

/// liblog's socket path.
pub const PATH: &str = atl_services_types::logcat::LOGDW;

/// Binds [`PATH`] and serves it on a thread for the process's lifetime.
pub fn start() -> Result<()> {
    atl_services_types::logcat::serve_logdw(Path::new(PATH), log::write_line)
}
