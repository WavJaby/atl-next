// SPDX-License-Identifier: MIT
//! atl-binder-sweep, `xtask test binder` stage `sweep`: transactions of every size up to libbinder's limit, in both directions and oneway, through binderd.
//! `server` (uid 1000) registers `atl.sweep` and serves. `client` (an app uid) runs each direction's grid, finds its limit by bisection and checks the rows around it.
//! put: the request carries the bytes, the reply is their FNV-1a. get: the reply carries `n` bytes of [`pattern`]. sink: oneway, the server counts it and keeps its sequence number, length and FNV-1a (`state`).
//! Past the limit a call fails with FAILED_TRANSACTION (binderd refuses the buffer: BR_FAILED_REPLY). Any other failure, a wrong byte or a dead binderd fails the client.

extern crate atl_parcelables;
extern crate binder;

use std::io::Write;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail, ensure};
use binder::binder_impl::{
    BorrowedParcel, FIRST_CALL_TRANSACTION, FLAG_ONEWAY, IBinderInternal, TransactionCode,
};
use binder::{
    BinderFeatures, Interface, ProcessState, Status, StatusCode, Strong, declare_binder_interface,
};
use clap::{Parser, Subcommand};

const NAME: &str = "atl.sweep";
const PUT: TransactionCode = FIRST_CALL_TRANSACTION;
const GET: TransactionCode = FIRST_CALL_TRANSACTION + 1;
const SINK: TransactionCode = FIRST_CALL_TRANSACTION + 2;
const STATE: TransactionCode = FIRST_CALL_TRANSACTION + 3;
const PING: TransactionCode = FIRST_CALL_TRANSACTION + 4;

/// The mapping every libbinder process makes: BINDER_VM_SIZE = 1 MiB - 2 pages (ProcessState.cpp:47). A transaction's data (interface token, length prefix, payload) must fit in it.
const REGION: usize = (1 << 20) - 2 * 4096;
/// A one-way transaction's data must fit the async half of the region (binder_alloc.c: free_async_space = size / 2).
const ASYNC_REGION: usize = REGION / 2;
/// Sizes this far past a limit are refused: the payload alone exceeds the region, and binderd refuses a buffer whose aligned size exceeds the free space.
const PAST: usize = 8;
/// Bytes a measured limit may sit below its region: the token, the length prefix and the status word are a few dozen bytes; 512 leaves room for a longer descriptor.
const HEADER_ALLOWANCE: usize = 512;
/// Sizes every direction runs first, ascending. 65536 is the first size whose message needs three 32 KiB fragments (proto.rs FRAGMENT); 61440 is the last that needs two.
const GRID: &[usize] = &[
    0, 4, 4096, 32768, 61440, 65536, 98304, 131072, 262144, 524288,
];
/// How long a one-way call may take to reach the server's state: it is delivered on the server's own thread.
const SINK_WAIT: Duration = Duration::from_secs(10);
const SINK_POLL: Duration = Duration::from_millis(5);
/// After the server counts a one-way call, binderd frees its async buffer when the server's reply to it is read; the next one-way call needs that space back.
const FREE_SETTLE: Duration = Duration::from_millis(20);

/// The bytes a payload of `n` carries: position-dependent, so a shifted or truncated copy fails. Any fixed generator will do.
fn pattern(n: usize) -> Vec<u8> {
    (0..n)
        .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
        .collect()
}

/// FNV-1a 64 (offset basis 0xcbf29ce484222325, prime 0x100000001b3): the checksum a put answers and the sink state keeps.
fn fnv1a(data: &[u8]) -> i64 {
    data.iter().fold(0xcbf2_9ce4_8422_2325_u64, |h, &b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    }) as i64
}

pub trait ISweep: Interface {
    /// Answers the FNV-1a of `data`.
    fn put(&self, data: &[u8]) -> binder::Result<i64>;
    /// Answers `n` bytes of [`pattern`].
    fn get(&self, n: i32) -> binder::Result<Vec<u8>>;
    /// Oneway: counts `data` as sequence `seq`, keeps its length and FNV-1a.
    fn sink(&self, seq: i32, data: &[u8]) -> binder::Result<()>;
    /// [one-way calls counted, last sequence number, last length, last FNV-1a].
    fn state(&self) -> binder::Result<Vec<i64>>;
    /// Answers 1.
    fn ping(&self) -> binder::Result<i32>;
}

declare_binder_interface! {
    ISweep["atl.sweep.ISweep"] {
        native: BnSweep(on_transact),
        proxy: BpSweep,
    }
}

fn on_transact(
    s: &dyn ISweep,
    code: TransactionCode,
    data: &BorrowedParcel<'_>,
    reply: &mut BorrowedParcel<'_>,
) -> Result<(), StatusCode> {
    match code {
        PUT => {
            let v: Vec<u8> = data.read()?;
            reply.write(&s.put(&v).map_err(|_| StatusCode::UNKNOWN_ERROR)?)
        }
        GET => {
            let n: i32 = data.read()?;
            reply.write(&s.get(n).map_err(|_| StatusCode::UNKNOWN_ERROR)?)
        }
        SINK => {
            let seq: i32 = data.read()?;
            let v: Vec<u8> = data.read()?;
            s.sink(seq, &v).map_err(|_| StatusCode::UNKNOWN_ERROR)
        }
        STATE => reply.write(&s.state().map_err(|_| StatusCode::UNKNOWN_ERROR)?),
        PING => reply.write(&s.ping().map_err(|_| StatusCode::UNKNOWN_ERROR)?),
        _ => Err(StatusCode::UNKNOWN_TRANSACTION),
    }
}

impl ISweep for BpSweep {
    fn put(&self, data: &[u8]) -> binder::Result<i64> {
        let r = self
            .binder
            .transact(PUT, 0, |mut p| p.write(&data.to_vec()))?;
        Ok(r.read()?)
    }
    fn get(&self, n: i32) -> binder::Result<Vec<u8>> {
        let r = self.binder.transact(GET, 0, |mut p| p.write(&n))?;
        Ok(r.read()?)
    }
    fn sink(&self, seq: i32, data: &[u8]) -> binder::Result<()> {
        self.binder.transact(SINK, FLAG_ONEWAY, |mut p| {
            p.write(&seq)?;
            p.write(&data.to_vec())
        })?;
        Ok(())
    }
    fn state(&self) -> binder::Result<Vec<i64>> {
        let r = self.binder.transact(STATE, 0, |_| Ok(()))?;
        Ok(r.read()?)
    }
    fn ping(&self) -> binder::Result<i32> {
        let r = self.binder.transact(PING, 0, |_| Ok(()))?;
        Ok(r.read()?)
    }
}

impl ISweep for binder::binder_impl::Binder<BnSweep> {
    fn put(&self, data: &[u8]) -> binder::Result<i64> {
        self.0.put(data)
    }
    fn get(&self, n: i32) -> binder::Result<Vec<u8>> {
        self.0.get(n)
    }
    fn sink(&self, seq: i32, data: &[u8]) -> binder::Result<()> {
        self.0.sink(seq, data)
    }
    fn state(&self) -> binder::Result<Vec<i64>> {
        self.0.state()
    }
    fn ping(&self) -> binder::Result<i32> {
        self.0.ping()
    }
}

/// The server's state: one-way calls counted, and the last one's sequence number, length and FNV-1a.
#[derive(Default)]
struct Sweep {
    last: Mutex<[i64; 4]>,
}

impl Interface for Sweep {}

impl ISweep for Sweep {
    fn put(&self, data: &[u8]) -> binder::Result<i64> {
        Ok(fnv1a(data))
    }
    fn get(&self, n: i32) -> binder::Result<Vec<u8>> {
        let n = usize::try_from(n).map_err(|_| StatusCode::BAD_VALUE)?;
        Ok(pattern(n))
    }
    fn sink(&self, seq: i32, data: &[u8]) -> binder::Result<()> {
        let mut st = self.last.lock().expect("sweep state");
        st[0] += 1;
        st[1] = i64::from(seq);
        st[2] = data.len() as i64;
        st[3] = fnv1a(data);
        Ok(())
    }
    fn state(&self) -> binder::Result<Vec<i64>> {
        Ok(self.last.lock().expect("sweep state").to_vec())
    }
    fn ping(&self) -> binder::Result<i32> {
        Ok(1)
    }
}

/// Writes one line to stdout (the harness reads the log; the workspace denies print!).
fn say(line: &str) {
    let _ = writeln!(std::io::stdout(), "{line}");
}

fn word(ok: bool) -> &'static str {
    if ok {
        "ok"
    } else {
        "refused (FAILED_TRANSACTION)"
    }
}

/// libbinder's Status as an error line, with what was being called.
fn bx<T>(r: binder::Result<T>, what: &str) -> Result<T> {
    r.map_err(|e| anyhow!("{what}: {e}"))
}

/// binderd refused the buffer: libbinder reports BR_FAILED_REPLY as FAILED_TRANSACTION.
fn refused(e: &Status) -> bool {
    e.transaction_error() == StatusCode::FAILED_TRANSACTION
}

/// Ok(true): the call passed and its bytes checked out. Ok(false): refused with FAILED_TRANSACTION. Any other status, or wrong bytes, is an error.
fn put_probe(s: &dyn ISweep, n: usize) -> Result<bool> {
    let data = pattern(n);
    match s.put(&data) {
        Ok(sum) => {
            ensure!(
                sum == fnv1a(&data),
                "put {n}: the server's checksum is {sum}"
            );
            Ok(true)
        }
        Err(e) if refused(&e) => Ok(false),
        Err(e) => bail!("put {n}: {e}"),
    }
}

fn get_probe(s: &dyn ISweep, n: usize) -> Result<bool> {
    match s.get(i32::try_from(n)?) {
        Ok(bytes) => {
            ensure!(
                bytes == pattern(n),
                "get {n}: {} bytes, not the pattern",
                bytes.len()
            );
            Ok(true)
        }
        Err(e) if refused(&e) => Ok(false),
        Err(e) => bail!("get {n}: {e}"),
    }
}

/// A one-way call returns before the server runs it: the probe waits until the server's state shows this sequence number.
fn sink_probe(s: &dyn ISweep, seq: i32, n: usize) -> Result<bool> {
    let data = pattern(n);
    match s.sink(seq, &data) {
        Ok(()) => {
            let start = Instant::now();
            while start.elapsed() < SINK_WAIT {
                let st = bx(s.state(), "state")?;
                let [_, last_seq, len, sum] =
                    <[i64; 4]>::try_from(st.as_slice()).context("state is not four words")?;
                if last_seq == i64::from(seq) {
                    ensure!(
                        len == i64::try_from(n)? && sum == fnv1a(&data),
                        "sink {seq}: the server kept {len} bytes, FNV-1a {sum}"
                    );
                    std::thread::sleep(FREE_SETTLE);
                    return Ok(true);
                }
                std::thread::sleep(SINK_POLL);
            }
            bail!("sink {seq} ({n} bytes): the server never counted it")
        }
        Err(e) if refused(&e) => Ok(false),
        Err(e) => bail!("sink {n}: {e}"),
    }
}

/// Largest `n` in [lo, hi) that passes, given `ok(lo)` and not `ok(hi)`: the probes must be monotone (a size that passes lies below every size refused).
fn bisect(lo: usize, hi: usize, mut ok: impl FnMut(usize) -> Result<bool>) -> Result<usize> {
    let (mut lo, mut hi) = (lo, hi);
    ensure!(
        ok(lo)? && !ok(hi)?,
        "the bounds {lo} / {hi} do not bracket the limit"
    );
    while hi - lo > 1 {
        let mid = lo + (hi - lo) / 2;
        if ok(mid)? {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    Ok(lo)
}

/// One direction: the grid (printed), the limit by bisection from 0 to `top` + PAST, the grid checked against that limit, and the rows around the limit. Returns the limit.
fn direction(
    name: &str,
    top: usize,
    probe: &mut dyn FnMut(usize) -> Result<bool>,
) -> Result<usize> {
    let mut grid = Vec::new();
    for &n in GRID {
        let ok = probe(n)?;
        say(&format!("sweep: {name} {n} {}", word(ok)));
        grid.push((n, ok));
    }
    let limit = bisect(0, top + PAST, &mut *probe)?;
    for (n, ok) in grid {
        ensure!(
            ok == (n <= limit),
            "{name} {n}: {} but the limit is {limit}",
            word(ok)
        );
    }
    ensure!(
        limit <= top && limit + HEADER_ALLOWANCE >= top,
        "{name}: limit {limit} is not within {HEADER_ALLOWANCE} bytes below {top}"
    );
    for n in [limit, limit + 1, 1 << 20, 2 << 20] {
        let ok = probe(n)?;
        ensure!(
            ok == (n <= limit),
            "{name} {n}: {} at the limit {limit}",
            word(ok)
        );
        say(&format!("sweep: {name} {n} {} (limit rows)", word(ok)));
    }
    Ok(limit)
}

fn client() -> Result<()> {
    ProcessState::start_thread_pool();
    let s: Strong<dyn ISweep> = binder::wait_for_interface(NAME).context("lookup")?;
    ensure!(bx(s.ping(), "ping")? == 1, "ping answered the wrong value");
    let put = direction("put", REGION, &mut |n| put_probe(&*s, n))?;
    let get = direction("get", REGION, &mut |n| get_probe(&*s, n))?;
    let mut seq = 0;
    let mut delivered = 0;
    let sink = direction("sink", ASYNC_REGION, &mut |n: usize| -> Result<bool> {
        seq += 1;
        let ok = sink_probe(&*s, seq, n)?;
        delivered += i64::from(ok);
        Ok(ok)
    })?;
    let st = bx(s.state(), "state")?;
    ensure!(
        st.first() == Some(&delivered),
        "sink: the server counted {:?} one-way calls, {delivered} passed",
        st.first()
    );
    ensure!(bx(s.ping(), "ping")? == 1, "ping after the sweep");
    say(&format!(
        "sweep: limit put={put} get={get} sink={sink} (region {REGION}, async half {ASYNC_REGION})"
    ));
    say("sweep: pass");
    Ok(())
}

#[derive(Parser)]
struct Args {
    #[command(subcommand)]
    mode: Mode,
}

#[derive(Subcommand)]
enum Mode {
    /// Registers `atl.sweep` and serves until killed.
    Server,
    /// Runs the sweep; fails on the first row that does not behave as the limit says.
    Client,
}

fn main() -> Result<()> {
    match Args::parse().mode {
        Mode::Server => {
            ProcessState::start_thread_pool();
            let b = BnSweep::new_binder(Sweep::default(), BinderFeatures::default());
            binder::add_service(NAME, b.as_binder()).context("addService")?;
            ProcessState::join_thread_pool();
            Ok(())
        }
        Mode::Client => client(),
    }
}
