// SPDX-License-Identifier: MIT
//! The app process's tracing subscriber: stderr, which atld points at the process's log (`log/<process>.log`), so the composer's and the platform layer's events sit beside liblog's records.
//! Level from `RUST_LOG` (EnvFilter syntax); `info` when unset or unparsable. A directive list must start with its default level: `info,atl_composer=debug`, since a list without a bare level turns every target it does not name off (the platform layer's content-origin line went missing that way). Per-frame lines are debug: a normal run writes none, the UI-mode gates enable them (xtask atld_run.rs).

use tracing_subscriber::EnvFilter;

/// Installs the subscriber for this process. A process that already has one keeps it.
pub fn install() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_target(false)
        .try_init();
}
