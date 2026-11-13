// SPDX-License-Identifier: MIT
//! Binder reply outcomes of this process. [`binder_reply`] is the aidl patch's `--rust_reply_hook` (patches/aosp/system/tools/aidl): every generated `on_transact` calls it once per non-OK outcome it sends back.
//! Each hit writes `services: <outcome> <Interface>.<method> <n> at=<stamp>[ cause=<what>]`, the line shape xtask's atld_run.rs `events` reads; outcome `unimplemented` is UNKNOWN_TRANSACTION, else `exception:<NAME>` or `status:<NAME>`.
//! No dependencies: every converter-built AIDL crate and atl-parcelables link it (bp2ninja aidlcrates.go `reply_hook`), cargo-built crates too.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::io::Write;
use std::sync::Mutex;

/// `binder_exception_t` EX_TRANSACTION_FAILED (frameworks/native libs/binder/ndk/include_ndk/android/binder_status.h): the Status carries a transport status code instead of an exception.
pub const EX_TRANSACTION_FAILED: i32 = -129;
/// `binder_status_t` STATUS_UNKNOWN_TRANSACTION (binder_status.h: -EBADMSG): a code the service does not implement.
pub const STATUS_UNKNOWN_TRANSACTION: i32 = -74;
/// `binder_status_t` STATUS_BAD_VALUE (binder_status.h: -EINVAL): what an argument that fails to decode answers.
pub const STATUS_BAD_VALUE: i32 = -22;

/// Exception names by `binder_exception_t` value (binder_status.h).
const EXCEPTIONS: &[(i32, &str)] = &[
    (-1, "SECURITY"),
    (-2, "BAD_PARCELABLE"),
    (-3, "ILLEGAL_ARGUMENT"),
    (-4, "NULL_POINTER"),
    (-5, "ILLEGAL_STATE"),
    (-6, "NETWORK_MAIN_THREAD"),
    (-7, "UNSUPPORTED_OPERATION"),
    (-8, "SERVICE_SPECIFIC"),
    (-9, "PARCELABLE"),
    (EX_TRANSACTION_FAILED, "TRANSACTION_FAILED"),
];

/// Status names by `binder_status_t` value (binder_status.h, errno-based and INT32_MIN-based codes).
const STATUSES: &[(i32, &str)] = &[
    (i32::MIN, "UNKNOWN_ERROR"),
    (-12, "NO_MEMORY"),
    (-38, "INVALID_OPERATION"),
    (STATUS_BAD_VALUE, "BAD_VALUE"),
    (i32::MIN + 1, "BAD_TYPE"),
    (-2, "NAME_NOT_FOUND"),
    (-1, "PERMISSION_DENIED"),
    (-19, "NO_INIT"),
    (-17, "ALREADY_EXISTS"),
    (-32, "DEAD_OBJECT"),
    (i32::MIN + 2, "FAILED_TRANSACTION"),
    (-75, "BAD_INDEX"),
    (-61, "NOT_ENOUGH_DATA"),
    (-11, "WOULD_BLOCK"),
    (-110, "TIMED_OUT"),
    (STATUS_UNKNOWN_TRANSACTION, "UNKNOWN_TRANSACTION"),
    (i32::MIN + 7, "FDS_NOT_ALLOWED"),
    (i32::MIN + 8, "UNEXPECTED_NULL"),
];

fn name(table: &[(i32, &'static str)], v: i32) -> String {
    table
        .iter()
        .find(|(k, _)| *k == v)
        .map_or_else(|| v.to_string(), |(_, n)| (*n).to_owned())
}

/// The outcome label of a reply: `unimplemented` for UNKNOWN_TRANSACTION, `status:<NAME>` for another transport status, `exception:<NAME>` otherwise.
pub fn outcome(exception: i32, status: i32) -> String {
    match (exception, status) {
        (EX_TRANSACTION_FAILED, STATUS_UNKNOWN_TRANSACTION) => "unimplemented".to_owned(),
        (EX_TRANSACTION_FAILED, s) => format!("status:{}", name(STATUSES, s)),
        (e, _) => format!("exception:{}", name(EXCEPTIONS, e)),
    }
}

/// The API part of a line: `<Interface>.<method>`, the interface being the descriptor's last segment; a code no method has is `code=<n>`.
fn api(descriptor: &str, method: Option<&str>, code: u32) -> String {
    let iface = descriptor.rsplit('.').next().unwrap_or(descriptor);
    match method {
        Some(m) => format!("{iface}.{m}"),
        None => format!("{iface}.code={code}"),
    }
}

static COUNTS: Mutex<BTreeMap<(String, String), u64>> = Mutex::new(BTreeMap::new());

thread_local! {
    static CAUSE: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// Records why the next BAD_VALUE this thread replies with happens (a refused parcelable); [`binder_reply`] takes it.
pub fn set_cause(what: &str) {
    CAUSE.with(|c| *c.borrow_mut() = Some(what.to_owned()));
}

/// The reply hook (aidl `--rust_reply_hook`): counts one non-OK outcome of `descriptor`'s `method` (None: unknown `code`), writes its line, returns the new count.
/// A pending cause is taken on every call (so it never outlives the reply it was set for) and printed only with BAD_VALUE, the status a refusal produces.
pub fn binder_reply(
    descriptor: &str,
    method: Option<&str>,
    code: u32,
    exception: i32,
    status: i32,
) -> u64 {
    let cause = CAUSE.with(|c| c.borrow_mut().take());
    let outcome = outcome(exception, status);
    let api = api(descriptor, method, code);
    // a poisoned map still counts: it holds plain numbers
    let mut map = COUNTS.lock().unwrap_or_else(|e| e.into_inner());
    let n = map.entry((outcome.clone(), api.clone())).or_insert(0);
    *n += 1;
    let n = *n;
    drop(map);
    let cause = match cause {
        Some(c) if exception == EX_TRANSACTION_FAILED && status == STATUS_BAD_VALUE => {
            format!(" cause={c}")
        }
        _ => String::new(),
    };
    let _ = writeln!(
        std::io::stderr(),
        "{}",
        line(&outcome, &api, n, &wall_stamp(), &cause)
    );
    n
}

fn line(outcome: &str, api: &str, n: u64, stamp: &str, cause: &str) -> String {
    format!("services: {outcome} {api} {n} at={stamp}{cause}")
}

/// The wall clock as logcat stamps lines (`secs.mmm`), the app log's clock.
pub fn wall_stamp() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    format!("{}.{:03}", now.as_secs(), now.subsec_millis())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Values from binder_status.h: EX_SECURITY -1, EX_ILLEGAL_ARGUMENT -3, STATUS_BAD_VALUE -EINVAL = -22, STATUS_UNKNOWN_TRANSACTION -EBADMSG = -74.
    #[test]
    fn outcome_names_follow_binder_status_h() {
        assert_eq!(outcome(-129, -74), "unimplemented");
        assert_eq!(outcome(-129, -22), "status:BAD_VALUE");
        assert_eq!(outcome(-1, 0), "exception:SECURITY");
        assert_eq!(outcome(-3, 0), "exception:ILLEGAL_ARGUMENT");
        assert_eq!(outcome(-77, 0), "exception:-77");
    }

    #[test]
    fn api_is_short_interface_and_method_or_code() {
        assert_eq!(
            api("android.os.IMessenger", Some("send"), 1),
            "IMessenger.send"
        );
        assert_eq!(api("android.os.IMessenger", None, 99), "IMessenger.code=99");
    }

    #[test]
    fn counts_per_outcome_and_api_and_cause_only_on_bad_value() {
        let d = "t.ICounted";
        assert_eq!(binder_reply(d, Some("a"), 1, -1, 0), 1);
        assert_eq!(binder_reply(d, Some("a"), 1, -1, 0), 2);
        assert_eq!(binder_reply(d, Some("a"), 1, -129, -74), 1);
        // a cause set before an unrelated outcome is dropped, not carried to the next BAD_VALUE
        set_cause("x");
        binder_reply(d, Some("b"), 2, -1, 0);
        assert!(CAUSE.with(|c| c.borrow().is_none()));
    }

    #[test]
    fn line_shape() {
        assert_eq!(
            line(
                "status:BAD_VALUE",
                "IMessenger.send",
                1,
                "100.000",
                " cause=android.os.Message"
            ),
            "services: status:BAD_VALUE IMessenger.send 1 at=100.000 cause=android.os.Message"
        );
    }
}
