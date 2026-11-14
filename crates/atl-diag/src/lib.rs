// SPDX-License-Identifier: MIT
//! The one counter family: API outcomes of this process, keyed by (outcome, api). [`binder_reply`] is the aidl patch's `--rust_reply_hook` (patches/aosp/system/tools/aidl): every generated
//! `on_transact` calls it once per non-OK outcome it sends back, and hand-written dispatchers call it the same way ([`unknown_code`] for a code they do not serve); hand-written code counts
//! served-by-design calls with [`answered`], [`refused`], [`sent`], and an API it does not implement with [`unimplemented_api!`]. Free-text refusal lines are not written (xtask check step `diag-shape`).
//! Log policy (owner 2026-11-14, M60 R2): a line at the first hit and at each power-of-two count, so a SIGKILLed process still leaves first-seen and a lower bound within 2x without flooding
//! (50,708 hits of one key -> 16 lines); [`flush_totals`] writes the exact total per key on an orderly exit. Line shapes ([`summary`] parses them; outcome before api: an api may hold spaces, an
//! outcome never does): `diag: first <outcome> <api> 1 at=<stamp>[ cause=<what>]`, `diag: count <outcome> <api> <n> at=<stamp>[ cause=<what>]`, `diag: total <outcome> <api> <n> first=<stamp>`.
//! Outcomes: `unimplemented` (UNKNOWN_TRANSACTION), `exception:<NAME>`, `status:<NAME>`, `refused`, `refused-parcel`, `answered`, `sent`; the parser adds `missing-service` from upstream lines.
//! No dependencies (feature `serde` only for xtask's report): every converter-built AIDL crate, atl-parcelables and the services link the converter's one `libatl_diag` (bp2ninja aidlcrates.go
//! `reply_hook`), so a process holds one map and one cause; a second copy linked by cargo would split them.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::io::Write;
use std::sync::Mutex;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering::Relaxed;

pub mod summary;

/// `binder_exception_t` EX_TRANSACTION_FAILED (frameworks/native libs/binder/ndk/include_ndk/android/binder_status.h): the Status carries a transport status code instead of an exception.
pub const EX_TRANSACTION_FAILED: i32 = -129;
/// `binder_status_t` STATUS_UNKNOWN_TRANSACTION (binder_status.h: -EBADMSG): a code the service does not implement.
pub const STATUS_UNKNOWN_TRANSACTION: i32 = -74;
/// `binder_status_t` STATUS_BAD_VALUE (binder_status.h: -EINVAL): what an argument that fails to decode answers.
pub const STATUS_BAD_VALUE: i32 = -22;

/// Outcome of a call nothing implements (UNKNOWN_TRANSACTION).
pub const UNIMPLEMENTED: &str = "unimplemented";
/// Outcome of a call served on purpose but worth a count: a no-op, a null by design, a drop.
pub const ANSWERED: &str = "answered";
/// Outcome of a request a service cannot serve, answered in band (a result code, a false).
pub const REFUSED: &str = "refused";
/// Outcome of a parcelable (or a part of one) without a codec (atl-parcelables `refuse`); the api field names the Java class or field.
pub const REFUSED_PARCEL: &str = "refused-parcel";
/// Outcome of an outgoing call the services made, so `unimplemented` keeps meaning unimplemented.
pub const SENT: &str = "sent";
/// Outcome of a service lookup nobody serves; the api field is the name. Read from upstream's own log lines ([`summary::MISSING_SERVICE_LINES`]), not counted in process.
pub const MISSING_SERVICE: &str = "missing-service";

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
        (EX_TRANSACTION_FAILED, STATUS_UNKNOWN_TRANSACTION) => UNIMPLEMENTED.to_owned(),
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

/// One key's record: its count and the stamp of its first hit.
struct Record {
    n: u64,
    first: String,
}

/// outcome -> api -> record: nested so a hit looks both up by `&str` and allocates nothing (a flat `(String, String)` key needs owned strings to look up).
static COUNTS: Mutex<BTreeMap<String, BTreeMap<String, Record>>> = Mutex::new(BTreeMap::new());

/// Outcome of a property read that finds nothing, or finds an empty value (AOSP readers treat "" as unset); the api field is the property name, nothing else (plan M59: no thread, library, package or caller).
pub const PROP_MISS: &str = "prop:miss";
/// Outcome under which [`flush_totals`] reports [`count`] calls dropped because the thread was already inside one (a signal handler reading a property while its thread held the map).
pub const REENTRANT: &str = "diag:reentrant";
/// [`REENTRANT`]'s api field.
const REENTRANT_API: &str = "dropped";

/// Nested counts dropped: atomics only, so a signal handler can record without the lock or an allocation.
static DROPPED: AtomicU64 = AtomicU64::new(0);
/// Wall clock (ms since the epoch) of the first dropped count; 0 = none yet.
static DROPPED_FIRST_MS: AtomicU64 = AtomicU64::new(0);

thread_local! {
    static CAUSE: RefCell<Option<String>> = const { RefCell::new(None) };
    /// This thread is inside [`count`]: a nested call (signal handler) must not take the map lock it may already hold.
    static INSIDE: Cell<bool> = const { Cell::new(false) };
}

/// Held while a thread is inside [`count`]; none when it already was.
struct Inside;

impl Inside {
    fn enter() -> Option<Self> {
        // a const Cell has no destructor, so the slot is never torn down; try_with only guards the contract
        let first = INSIDE.try_with(|i| !i.replace(true)).unwrap_or(false);
        first.then_some(Self)
    }
}

impl Drop for Inside {
    fn drop(&mut self) {
        let _ = INSIDE.try_with(|i| i.set(false));
    }
}

fn drop_nested() {
    DROPPED.fetch_add(1, Relaxed);
    let _ = DROPPED_FIRST_MS.compare_exchange(0, wall_ms(), Relaxed, Relaxed);
}

/// Count calls dropped as nested so far (see [`REENTRANT`]).
pub fn reentrant() -> u64 {
    DROPPED.load(Relaxed)
}

/// Records why the next transport status this thread replies with happens (a refused parcelable, an unimplemented sub-case, a hand-written refusal); [`binder_reply`] takes it.
pub fn set_cause(what: &str) {
    CAUSE.with(|c| *c.borrow_mut() = Some(what.to_owned()));
}

/// Whether count `n` of a key writes a line: the first hit and each power of two.
fn logs(n: u64) -> bool {
    n.is_power_of_two()
}

/// Counts one `outcome` of `api`, writes its line when [`logs`] says so, returns the new count.
/// A call nested in another on the same thread (a signal handler) is dropped into [`REENTRANT`] and returns 0: taking the map lock again would deadlock a crashing app.
/// A hit of a known key allocates only when it writes a line.
pub fn count(outcome: &str, api: &str, cause: Option<&str>) -> u64 {
    let Some(_inside) = Inside::enter() else {
        drop_nested();
        return 0;
    };
    // a poisoned map still counts: it holds plain numbers
    let mut map = COUNTS.lock().unwrap_or_else(|e| e.into_inner());
    let (n, stamp) = match map.get_mut(outcome).and_then(|m| m.get_mut(api)) {
        Some(r) => {
            r.n += 1;
            (r.n, None)
        }
        None => {
            let stamp = wall_stamp();
            map.entry(outcome.to_owned()).or_default().insert(
                api.to_owned(),
                Record {
                    n: 1,
                    first: stamp.clone(),
                },
            );
            (1, Some(stamp))
        }
    };
    drop(map);
    if logs(n) {
        let stamp = stamp.unwrap_or_else(wall_stamp);
        let _ = writeln!(
            std::io::stderr(),
            "{}",
            line(outcome, api, n, &stamp, cause)
        );
    }
    n
}

/// The reply hook (aidl `--rust_reply_hook`): counts one non-OK outcome of `descriptor`'s `method` (None: unknown `code`), returns the new count.
/// A pending cause is taken on every call (so it never outlives the reply it was set for) and printed only with a transport status, the replies its setters produce (an exception carries its own message).
/// Hand-written dispatchers (no generated `on_transact`) call it too: for every `Err` leaving their `on_transact` and every exception they write, one site per outcome as in generated code.
pub fn binder_reply(
    descriptor: &str,
    method: Option<&str>,
    code: u32,
    exception: i32,
    status: i32,
) -> u64 {
    let cause = CAUSE.with(|c| c.borrow_mut().take());
    count(
        &outcome(exception, status),
        &api(descriptor, method, code),
        kept_cause(exception, cause.as_deref()),
    )
}

/// A hand-written dispatcher's reply to a code it does not serve: [`binder_reply`] with UNKNOWN_TRANSACTION; the caller returns that status.
pub fn unknown_code(interface: &str, code: u32) -> u64 {
    binder_reply(
        interface,
        None,
        code,
        EX_TRANSACTION_FAILED,
        STATUS_UNKNOWN_TRANSACTION,
    )
}

/// The cause a reply prints: only a transport status (EX_TRANSACTION_FAILED) takes one.
fn kept_cause(exception: i32, cause: Option<&str>) -> Option<&str> {
    cause.filter(|_| exception == EX_TRANSACTION_FAILED)
}

/// A call served on purpose but worth a count (`interface`.`what`).
pub fn answered(interface: &str, what: &str) -> u64 {
    count(ANSWERED, &format!("{interface}.{what}"), None)
}

/// A request `interface` cannot serve, answered in band.
pub fn refused(interface: &str, what: &str) -> u64 {
    count(REFUSED, &format!("{interface}.{what}"), None)
}

/// An outgoing call the services made on `interface`.
pub fn sent(interface: &str, call: &str) -> u64 {
    count(SENT, &format!("{interface}.{call}"), None)
}

/// Hand-written Rust's entry for an API it does not implement (AGENTS "Lints", plan F4/F7): counts `unimplemented` under the api string; the caller returns the API's default.
#[macro_export]
macro_rules! unimplemented_api {
    ($api:expr) => {
        $crate::count($crate::UNIMPLEMENTED, $api, None)
    };
}

/// The counts so far, in key order: (outcome, api, count).
pub fn counts() -> Vec<(String, String, u64)> {
    let map = COUNTS.lock().unwrap_or_else(|e| e.into_inner());
    map.iter()
        .flat_map(|(o, apis)| apis.iter().map(move |(a, r)| (o.clone(), a.clone(), r.n)))
        .collect()
}

/// Writes the exact total of every key (`diag: total ...`), then `diag:reentrant dropped` if any count was dropped: call on an orderly exit.
pub fn flush_totals() {
    let map = COUNTS.lock().unwrap_or_else(|e| e.into_inner());
    let mut err = std::io::stderr().lock();
    for (o, apis) in map.iter() {
        for (a, r) in apis {
            let _ = writeln!(err, "diag: total {o} {a} {} first={}", r.n, r.first);
        }
    }
    let dropped = reentrant();
    if dropped > 0 {
        let first = stamp_of(DROPPED_FIRST_MS.load(Relaxed));
        let _ = writeln!(
            err,
            "diag: total {REENTRANT} {REENTRANT_API} {dropped} first={first}"
        );
    }
}

fn line(outcome: &str, api: &str, n: u64, stamp: &str, cause: Option<&str>) -> String {
    let kind = if n == 1 { "first" } else { "count" };
    let cause = cause.map_or_else(String::new, |c| format!(" cause={c}"));
    format!("diag: {kind} {outcome} {api} {n} at={stamp}{cause}")
}

/// The wall clock as logcat stamps lines (`secs.mmm`), the app log's clock.
pub fn wall_stamp() -> String {
    stamp_of(wall_ms())
}

/// The wall clock in ms since the epoch (clock_gettime: no lock, no allocation).
fn wall_ms() -> u64 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    u64::try_from(now.as_millis()).unwrap_or(u64::MAX)
}

fn stamp_of(ms: u64) -> String {
    format!("{}.{:03}", ms / 1000, ms % 1000)
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
    fn counts_per_outcome_and_api_and_cause_taken_by_any_reply() {
        let d = "t.ICounted";
        assert_eq!(binder_reply(d, Some("a"), 1, -1, 0), 1);
        assert_eq!(binder_reply(d, Some("a"), 1, -1, 0), 2);
        assert_eq!(binder_reply(d, Some("a"), 1, -129, -74), 1);
        // a cause set before an unrelated outcome is dropped, not carried to the next BAD_VALUE
        set_cause("x");
        binder_reply(d, Some("b"), 2, -1, 0);
        assert!(CAUSE.with(|c| c.borrow().is_none()));
    }

    /// binder_status.h: EX_TRANSACTION_FAILED -129 carries a status (PERMISSION_DENIED -1 here: a hand-written refusal), EX_SECURITY -1 is an exception with its own message.
    #[test]
    fn cause_rides_any_transport_status_never_an_exception() {
        assert_eq!(kept_cause(-129, Some("owns no task")), Some("owns no task"));
        assert_eq!(kept_cause(-1, Some("owns no task")), None);
    }

    /// The policy: hits 1..=5 write lines at 1, 2 and 4 (powers of two); the total is 5. Break check: logging every hit makes the first assertion 5 lines.
    #[test]
    fn five_hits_log_three_lines_and_total_five() {
        let logged: Vec<u64> = (1..=5).filter(|&n| logs(n)).collect();
        assert_eq!(logged, [1, 2, 4]);
        for _ in 0..5 {
            answered("t.IPolicy", "m");
        }
        assert!(counts().contains(&(ANSWERED.to_owned(), "t.IPolicy.m".to_owned(), 5)));
    }

    #[test]
    fn line_shape() {
        assert_eq!(
            line(
                "status:BAD_VALUE",
                "IMessenger.send",
                1,
                "100.000",
                Some("android.os.Message")
            ),
            "diag: first status:BAD_VALUE IMessenger.send 1 at=100.000 cause=android.os.Message"
        );
        assert_eq!(
            line("answered", "I.a b", 4, "100.000", None),
            "diag: count answered I.a b 4 at=100.000"
        );
    }

    /// A signal handler's count while its thread is inside `count` (the guard taken, the map locked): dropped into `diag:reentrant`, 1 more, the key never created.
    /// Break check: without the guard test in `count`, the nested call locks the map this thread holds and the test hangs (or std panics on the relock).
    #[test]
    fn nested_count_is_dropped_not_deadlocked() {
        let before = reentrant();
        let inside = Inside::enter().expect("not inside count yet");
        let held = COUNTS.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(count(PROP_MISS, "t.nested", None), 0);
        drop(held);
        drop(inside);
        assert_eq!(reentrant(), before + 1);
        assert!(!counts().iter().any(|(_, a, _)| a == "t.nested"));
        // the guard was released: the next call counts
        assert_eq!(count(PROP_MISS, "t.nested", None), 1);
    }

    /// 1794661778402 ms = the S4 example stamp `1794661778.402`; a value under 100 ms keeps its leading zeros.
    #[test]
    fn stamp_of_ms() {
        assert_eq!(stamp_of(1_794_661_778_402), "1794661778.402");
        assert_eq!(stamp_of(5_007), "5.007");
    }

    /// The stamp is logcat's shape: `secs.mmm`, three digits of milliseconds.
    #[test]
    fn wall_stamp_is_logcat_shaped() {
        let s = wall_stamp();
        let (secs, ms) = s.split_once('.').expect("secs.mmm");
        assert!(secs.parse::<u64>().is_ok());
        assert_eq!(ms.len(), 3);
    }
}
