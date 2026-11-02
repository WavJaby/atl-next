// SPDX-License-Identifier: MIT
//! `android_set_abort_message`: bionic keeps the first message of the process for debuggerd ("the first
//! crash is the one most worth reporting"; null is stored as `(null)`), and `async_safe_fatal` sets it
//! too. Stored here, in this process-wide library: `message()` is the reader, for the crash handler
//! (none yet; AOSP's libdebuggerd_client fails every dump request before any signal: no tombstoned). A handler in another
//! shared object reads it through a C export added with it -- bionic's own reader is libc-internal
//! (`__libc_shared_globals()->abort_msg`).
//!
//! bionic copies into a fresh anonymous mapping, which a corrupted heap cannot stop; this copies into
//! the heap -- its callers (liblog's and libbase's aborters) have just built the message on it.

use std::sync::OnceLock;

/// One process's abort message.
pub struct AbortMessage(OnceLock<Box<[u8]>>);

/// bionic's text for a null message.
const NULL_TEXT: &[u8] = b"(null)";

impl AbortMessage {
    pub const fn new() -> Self {
        Self(OnceLock::new())
    }

    /// Keeps `msg` (none = a null pointer) unless a message is already kept.
    pub fn set(&self, msg: Option<&[u8]>) {
        let _ = self.0.set(msg.unwrap_or(NULL_TEXT).into());
    }

    /// The kept message, without its NUL.
    pub fn get(&self) -> Option<&[u8]> {
        self.0.get().map(|m| &**m)
    }
}

impl Default for AbortMessage {
    fn default() -> Self {
        Self::new()
    }
}

static PROCESS: AbortMessage = AbortMessage::new();

/// `android_set_abort_message` of this process.
pub fn set(msg: Option<&[u8]>) {
    PROCESS.set(msg);
}

/// The message a crash report of this process includes, if one was set.
pub fn message() -> Option<&'static [u8]> {
    PROCESS.get()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// bionic android_set_abort_message.cpp: a later call while a message exists returns at once;
    /// a null message is stored as "(null)".
    #[test]
    fn first_message_kept() {
        let m = AbortMessage::new();
        assert_eq!(m.get(), None);
        m.set(Some(b"first"));
        m.set(Some(b"second"));
        assert_eq!(m.get(), Some(&b"first"[..]));
        let null = AbortMessage::new();
        null.set(None);
        null.set(Some(b"later"));
        assert_eq!(null.get(), Some(&b"(null)"[..]));
    }
}
