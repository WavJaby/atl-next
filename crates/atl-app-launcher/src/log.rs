// SPDX-License-Identifier: MIT
//! The sink of this process's log records (atl-services-types `logcat`): stderr for now (journald later, only through the platform layer).

use std::io::Write;

pub use atl_services_types::logcat::{LIBLOG, Level, Record, format};

/// Writes `r` to stderr in one write. A failed write is dropped: a logger has nowhere to report it.
pub fn write(r: &Record) {
    write_line(&format(r));
}

/// Writes one formatted record (several lines for a multi-line message) to stderr in one write.
pub fn write_line(text: &str) {
    let _ = std::io::stderr().lock().write_all(text.as_bytes());
}

#[cfg(test)]
mod tests {
    use crate::generated::log as liblog;
    use atl_services_types::logcat::priority;

    /// The shared crate's priorities are android/log.h's (this crate's bindgen of the header).
    #[test]
    fn priorities_match_the_header() {
        let header = [
            liblog::ANDROID_LOG_VERBOSE,
            liblog::ANDROID_LOG_DEBUG,
            liblog::ANDROID_LOG_INFO,
            liblog::ANDROID_LOG_WARN,
            liblog::ANDROID_LOG_ERROR,
            liblog::ANDROID_LOG_FATAL,
            liblog::ANDROID_LOG_SILENT,
        ]
        .map(|p| i32::try_from(p).unwrap());
        let ours = [
            priority::VERBOSE,
            priority::DEBUG,
            priority::INFO,
            priority::WARN,
            priority::ERROR,
            priority::FATAL,
            priority::SILENT,
        ];
        assert_eq!(header, ours);
    }
}
