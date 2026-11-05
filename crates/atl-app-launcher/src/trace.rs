// SPDX-License-Identifier: MIT
//! The app process's tracing subscriber: stderr, which atld points at the process's log (`log/<process>.log`), so the composer's and the platform layer's events sit beside liblog's records, stamped as they are (WallStamp).
//! Level from `RUST_LOG` (EnvFilter syntax); `info` when unset or unparsable. A directive list must start with its default level: `info,atl_composer=debug`, since a list without a bare level turns every target it does not name off (the platform layer's content-origin line went missing that way). Per-frame lines are debug: a normal run writes none, the UI-mode gates enable them (xtask atld_run.rs).

use std::time::{SystemTime, UNIX_EPOCH};

use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::time::FormatTime;

/// Wall clock as `<unix secs>.<ms>`: the stamp liblog's records and the services' lines carry, so one parser (xtask parse_stamp) orders every line of a run's logs.
struct WallStamp;

impl FormatTime for WallStamp {
    fn format_time(&self, w: &mut Writer<'_>) -> std::fmt::Result {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        write!(w, "{}.{:03}", now.as_secs(), now.subsec_millis())
    }
}

/// Installs the subscriber for this process. A process that already has one keeps it.
pub fn install() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_target(false)
        .with_timer(WallStamp)
        .try_init();
}
