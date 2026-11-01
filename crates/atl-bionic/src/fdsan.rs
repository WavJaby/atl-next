// SPDX-License-Identifier: MIT
//! fdsan (bionic `fdsan.cpp`): a process-wide table fd -> owner tag, checked where an owner exchanges or
//! closes its descriptor, and the error level deciding what a failed check does.
//!
//! Deviation, the start level: bionic starts at `FATAL` (`__libc_init_fdsan`, unless `debug.fdsan`); here at
//! `DISABLED`. glibc's `close`, `close_range`, `dup2` and `fclose` clear no tag, so a descriptor closed
//! outside fdsan and reused keeps its old owner's tag -- on bionic that `close` is itself the check that
//! reports first. A report here can blame the wrong owner, so tags are kept and exchanged but reported only
//! once `android_fdsan_set_error_level` asks.
//! Deviation, the warn levels: bionic also raises `BIONIC_SIGNAL_DEBUGGER` for a tombstone; no debuggerd
//! handler is installed here and that signal's default action ends the process, so only the log line is
//! written. bionic's vfork-child exemption does not arise: nothing calls fdsan between `vfork` and `exec`.

use std::sync::OnceLock;
use std::sync::atomic::Ordering::SeqCst;
use std::sync::atomic::{AtomicU32, AtomicU64};

use crate::generated::fdsan::{
    ANDROID_FDSAN_ERROR_LEVEL_DISABLED, ANDROID_FDSAN_ERROR_LEVEL_FATAL,
    ANDROID_FDSAN_ERROR_LEVEL_WARN_ONCE,
};

static LEVEL: AtomicU32 = AtomicU32::new(ANDROID_FDSAN_ERROR_LEVEL_DISABLED);

/// Descriptors per lazily allocated chunk of the table.
const CHUNK: usize = 1024;
/// Chunks: CHUNK * CHUNKS = 2^20 descriptors, Linux's default `fs.nr_open` (the highest `RLIMIT_NOFILE` a
/// process may set). A higher fd is untracked -- unowned, never reported -- as bionic treats an fd past its
/// table.
const CHUNKS: usize = 1024;

static TABLE: [OnceLock<Box<[AtomicU64]>>; CHUNKS] = [const { OnceLock::new() }; CHUNKS];

/// Where `fd`'s tag lives.
enum Entry {
    /// Untracked: negative or past the table.
    None,
    /// In a chunk never written: unowned.
    Unwritten(&'static OnceLock<Box<[AtomicU64]>>, usize),
    Tag(&'static AtomicU64),
}

fn entry(fd: i32) -> Entry {
    let Some(slot) = usize::try_from(fd)
        .ok()
        .and_then(|fd| Some((TABLE.get(fd / CHUNK)?, fd % CHUNK)))
    else {
        return Entry::None;
    };
    match slot.0.get() {
        Some(chunk) => Entry::Tag(&chunk[slot.1]),
        None => Entry::Unwritten(slot.0, slot.1),
    }
}

/// The tag of an unwritten entry, allocating its chunk.
fn written(slot: &'static OnceLock<Box<[AtomicU64]>>, i: usize) -> &'static AtomicU64 {
    &slot.get_or_init(|| (0..CHUNK).map(|_| AtomicU64::new(0)).collect())[i]
}

/// bionic `android_fdsan_create_owner_tag`: the type in the top byte over the value's low 56 bits; 0 stays 0.
/// A type wider than a byte is fatal, as in bionic.
pub fn create_owner_tag(owner_type: u32, tag: u64) -> u64 {
    if tag == 0 {
        return 0;
    }
    if owner_type & 0xff != owner_type {
        fatal(
            b"",
            &format!("invalid android_fdsan_owner_type value: {owner_type:x}"),
        );
    }
    (u64::from(owner_type) << 56) | (tag & ((1 << 56) - 1))
}

/// bionic `android_fdsan_get_owner_tag`.
pub fn get_owner_tag(fd: i32) -> u64 {
    match entry(fd) {
        Entry::Tag(t) => t.load(SeqCst),
        Entry::None | Entry::Unwritten(..) => 0,
    }
}

/// bionic `android_fdsan_exchange_owner_tag`: `new_tag` if the owner is `expected`, else a report.
pub fn exchange_owner_tag(fd: i32, expected: u64, new_tag: u64) {
    let tag = match entry(fd) {
        Entry::None => return,
        // all unowned: nothing to write for 0 -> 0
        Entry::Unwritten(..) if expected == 0 && new_tag == 0 => return,
        Entry::Unwritten(slot, i) => written(slot, i),
        Entry::Tag(t) => t,
    };
    if let Err(actual) = tag.compare_exchange(expected, new_tag, SeqCst, SeqCst) {
        report(&exchange_message(fd, expected, actual));
    }
}

/// bionic `android_fdsan_close_with_tag`: clears the owner (a report if it is not `expected`, the tag then
/// left as it is), then `close`s; with an owner expected, `EBADF` is a double close. `close` returns
/// (return value, errno); so does this.
pub fn close_with_tag(fd: i32, expected: u64, close: impl FnOnce(i32) -> (i32, i32)) -> (i32, i32) {
    let mismatch = match entry(fd) {
        Entry::None => None,
        Entry::Unwritten(..) => (expected != 0).then_some(0),
        Entry::Tag(t) => t.compare_exchange(expected, 0, SeqCst, SeqCst).err(),
    };
    if let Some(actual) = mismatch {
        report(&close_message(fd, expected, actual));
    }
    let (rc, errno) = close(fd);
    if expected != 0 && rc == -1 && errno == EBADF {
        report(&format!("double-close of file descriptor {fd} detected"));
    }
    (rc, errno)
}

/// Linux `EBADF`, the same on every arch.
const EBADF: i32 = 9;

/// bionic `android_fdsan_set_error_level`: the previous level (an atomic exchange there too).
pub fn set_error_level(level: u32) -> u32 {
    LEVEL.swap(level, SeqCst)
}

/// bionic `fdsan_error`, at the current level.
fn report(msg: &str) {
    match LEVEL.load(SeqCst) {
        ANDROID_FDSAN_ERROR_LEVEL_DISABLED => {}
        ANDROID_FDSAN_ERROR_LEVEL_FATAL => fatal(b"fdsan: ", msg),
        level => {
            let _ = crate::async_safe::write_log(ANDROID_LOG_ERROR, b"fdsan", msg.as_bytes());
            if level == ANDROID_FDSAN_ERROR_LEVEL_WARN_ONCE {
                // bionic: a compare-exchange, so a level set meanwhile is kept
                let _ = LEVEL.compare_exchange(
                    level,
                    ANDROID_FDSAN_ERROR_LEVEL_DISABLED,
                    SeqCst,
                    SeqCst,
                );
            }
        }
    }
}

/// liblog's `ANDROID_LOG_ERROR` (`android_LogPriority`).
const ANDROID_LOG_ERROR: i32 = 6;

/// `async_safe_fatal` (`prefix` = its tag argument) then abort.
fn fatal(prefix: &[u8], msg: &str) -> ! {
    crate::async_safe::fatal_text(prefix, msg.as_bytes());
    std::process::abort()
}

/// bionic `android_fdsan_get_tag_type`: the owner kind the top byte names (fdsan.h's owner types).
fn tag_type(tag: u64) -> &'static str {
    match tag >> 56 {
        1 => "FILE*",
        2 => "DIR*",
        3 => "unique_fd",
        4 => "sqlite",
        5 => "FileInputStream",
        6 => "FileOutputStream",
        7 => "RandomAccessFile",
        8 => "ParcelFileDescriptor",
        9 => "ART FdFile",
        10 => "DatagramSocketImpl",
        11 => "SocketImpl",
        12 => "ZipArchive",
        13 => "native_handle_t",
        14 => "Parcel",
        // GENERIC_FF: a sign-extended native pointer has bits 48..63 all set
        255 if tag >> 48 != 0xffff => "Java object of unknown type",
        _ => "native object of unknown type",
    }
}

/// bionic `android_fdsan_get_tag_value`: the low 56 bits, sign-extended.
fn tag_value(tag: u64) -> u64 {
    (((tag << 8) as i64) >> 8) as u64
}

fn owner(tag: u64) -> String {
    format!("{} 0x{:x}", tag_type(tag), tag_value(tag))
}

/// bionic's texts for a failed exchange (`expected != actual`).
fn exchange_message(fd: i32, expected: u64, actual: u64) -> String {
    let head = format!("failed to exchange ownership of file descriptor: fd {fd} is");
    match (expected, actual) {
        (0, a) => format!("{head} owned by {}, was expected to be unowned", owner(a)),
        (e, 0) => format!("{head} unowned, was expected to be owned by {}", owner(e)),
        (e, a) => format!(
            "{head} owned by {}, was expected to be owned by {}",
            owner(a),
            owner(e)
        ),
    }
}

/// bionic's texts for a close by the wrong owner (`expected != actual`).
fn close_message(fd: i32, expected: u64, actual: u64) -> String {
    let head = format!("attempted to close file descriptor {fd},");
    match (expected, actual) {
        (0, a) => format!(
            "{head} expected to be unowned, actually owned by {}",
            owner(a)
        ),
        (e, 0) => format!(
            "{head} expected to be owned by {}, actually unowned",
            owner(e)
        ),
        (e, a) => format!(
            "{head} expected to be owned by {}, actually owned by {}",
            owner(e),
            owner(a)
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Tag encoding from bionic's fdsan.cpp: type << 56 | (value & (2^56 - 1)), value read back sign-extended
    // from bit 55. Owner types from fdsan.h: UNIQUE_FD = 3, NATIVE_HANDLE = 13, GENERIC_FF = 255.

    #[test]
    fn tag_encoding() {
        assert_eq!(create_owner_tag(3, 0), 0);
        assert_eq!(create_owner_tag(3, 0x1234), 0x0300_0000_0000_1234);
        assert_eq!(
            create_owner_tag(13, 0xffff_7f00_dead_beef),
            0x0dff_7f00_dead_beef
        );
        assert_eq!(create_owner_tag(255, u64::MAX), u64::MAX);
        assert_eq!(tag_value(0x0300_0000_0000_1234), 0x1234);
        assert_eq!(tag_value(0x0380_0000_0000_0000), 0xff80_0000_0000_0000);
        assert_eq!(tag_type(0x0300_0000_0000_1234), "unique_fd");
        assert_eq!(tag_type(u64::MAX), "native object of unknown type");
        assert_eq!(
            tag_type(0xff00_0000_0000_0001),
            "Java object of unknown type"
        );
    }

    #[test]
    fn messages_are_bionics() {
        assert_eq!(
            close_message(7, 0, 0x0300_0000_0000_0010),
            "attempted to close file descriptor 7, expected to be unowned, actually owned by unique_fd 0x10"
        );
        assert_eq!(
            exchange_message(7, 0x0300_0000_0000_0010, 0),
            "failed to exchange ownership of file descriptor: fd 7 is unowned, was expected to be owned by unique_fd 0x10"
        );
    }

    /// The table; a mismatch never stops the operation (FATAL is never set in these tests). fds far above
    /// any the test process opens, one per test, so tests running in parallel share no entry.
    #[test]
    fn owner_table() {
        let fd = 900_000;
        let a = create_owner_tag(3, 0xa);
        let b = create_owner_tag(3, 0xb);
        assert_eq!(get_owner_tag(fd), 0);
        exchange_owner_tag(fd, 0, a);
        assert_eq!(get_owner_tag(fd), a);
        // wrong expected owner: unchanged (bionic's compare-exchange)
        exchange_owner_tag(fd, b, 0);
        assert_eq!(get_owner_tag(fd), a);
        exchange_owner_tag(fd, a, b);
        assert_eq!(get_owner_tag(fd), b);
        // a close by the wrong owner still closes, and leaves the tag
        let mut closed = Vec::new();
        let r = close_with_tag(fd, a, |f| {
            closed.push(f);
            (0, 0)
        });
        assert_eq!((r, closed.as_slice()), ((0, 0), &[fd][..]));
        assert_eq!(get_owner_tag(fd), b);
        close_with_tag(fd, b, |_| (0, 0));
        assert_eq!(get_owner_tag(fd), 0);
        // untracked: unowned, nothing kept
        let past = i32::try_from(CHUNK * CHUNKS).expect("fits");
        exchange_owner_tag(past, 0, a);
        assert_eq!(get_owner_tag(past), 0);
        assert_eq!(get_owner_tag(-1), 0);
    }

    #[test]
    fn concurrent_exchange_has_one_winner() {
        let fd = 900_001;
        let winners: usize = std::thread::scope(|s| {
            let hs: Vec<_> = (1..=8u64)
                .map(|i| {
                    s.spawn(move || {
                        let t = create_owner_tag(3, i);
                        exchange_owner_tag(fd, 0, t);
                        usize::from(get_owner_tag(fd) == t)
                    })
                })
                .collect();
            hs.into_iter().map(|h| h.join().expect("thread")).sum()
        });
        assert_eq!(winners, 1);
    }

    #[test]
    fn set_level_returns_previous() {
        // enumerator values from bionic's fdsan.h: DISABLED, WARN_ONCE, WARN_ALWAYS, FATAL = 0..3 in order;
        // the start level is this crate's DISABLED (module doc). FATAL is never set: the other tests'
        // deliberate mismatches would abort.
        assert_eq!(set_error_level(2), 0);
        assert_eq!(set_error_level(1), 2);
        assert_eq!(set_error_level(0), 1);
    }
}
