// SPDX-License-Identifier: MIT
//! The buffered tier: the IIO char device `/dev/iio:deviceN` read as a stream of scans (kernel Documentation/iio/iio_devbuf.rst), where the environment enabled the buffer (udev rule 71). Nothing here writes sysfs.
//! [`probe`] reads the buffer's state in the design's order (layout, trigger, enabled, clock, node); the first failing step is the fallback label. [`Stream::open`] adds the fresh-data wait (P6).
//! Scans are decoded by the layout the kernel announces in `scan_elements`, never by a fixed offset. Stamps are kernel clock values mapped to CLOCK_BOOTTIME per read (`ts - (named_now - boottime_mid)`, see [`Stream::stamp`]): no root write, one path for all four clocks.

use std::os::fd::OwnedFd;
use std::path::Path;
use std::time::{Duration, Instant};

use atl_platform_host_api::{Fallback, Sample, SampleStream, SensorError, Tier};
use rustix::event::{PollFd, PollFlags, Timespec, poll};
use rustix::fs::{Mode, OFlags};
use rustix::io::Errno;
use rustix::time::{ClockId, clock_gettime};

use super::{AXES, Calibration, read_text};

const NS_PER_S: i64 = 1_000_000_000;
/// The oldest a fresh stamp may be: far above poll latency (ms), far below a stale ring (128 scans = 12.8 s at 10 Hz; design 5.1 P6). A stamp ahead of arrival is judged by the mapping's error instead, see [`Stream::stamp`].
const FRESH_AGE_NS: i64 = NS_PER_S;
/// Slack on a stamp ahead of arrival, beyond the read bracket: a coarse clocksource can read the same value twice, so the bracket can be zero while the error is a tick. 1 ms = one jiffy at HZ=1000.
const TICK_NS: i64 = 1_000_000;
/// Open-time wait for a fresh scan: this long, or [`FRESH_INTERVALS`] device intervals when longer (design 5.1 P6).
const FRESH_WAIT: Duration = Duration::from_millis(500);
const FRESH_INTERVALS: u32 = 3;
/// Bytes per read. A scan split across reads waits in `partial` for its rest.
const READ_CHUNK: usize = 4096;

/// `v` rounded up to a multiple of `to` (a storage size: 1, 2, 4 or 8 bytes).
fn align(v: usize, to: usize) -> usize {
    v.div_ceil(to) * to
}

/// One scan element type: `[be|le]:[s|u]bits/storagebits[>>shift]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Type {
    big_endian: bool,
    signed: bool,
    bits: u32,
    storage: usize,
    shift: u32,
}

/// `[be|le]:[s|u]bits/storagebits[>>shift]` (iio_devbuf.rst:116-124). `None` for anything else, or when the real bits and the shift do not fit the storage word.
fn parse_type(text: &str) -> Option<Type> {
    let (endian, rest) = text.split_once(':')?;
    let big_endian = match endian {
        "be" => true,
        "le" => false,
        _ => return None,
    };
    let signed = match rest.get(..1)? {
        "s" => true,
        "u" => false,
        _ => return None,
    };
    let (bits, rest) = rest.get(1..)?.split_once('/')?;
    let bits: u32 = bits.parse().ok()?;
    let (storage, shift): (&str, u32) = match rest.split_once(">>") {
        Some((storage, shift)) => (storage, shift.parse().ok()?),
        None => (rest, 0),
    };
    let storage: u32 = storage.parse().ok()?;
    if !matches!(storage, 8 | 16 | 32 | 64) || bits == 0 || bits + shift > storage {
        return None;
    }
    Some(Type {
        big_endian,
        signed,
        bits,
        storage: (storage / 8) as usize,
        shift,
    })
}

/// One channel's place in a scan and how to read it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Channel {
    offset: usize,
    ty: Type,
}

impl Channel {
    /// The value in one scan: the storage word in its byte order, shifted and masked to the real bits, sign-extended when signed.
    fn value(&self, scan: &[u8]) -> i64 {
        let bytes = &scan[self.offset..self.offset + self.ty.storage];
        let word = if self.ty.big_endian {
            bytes.iter().fold(0u64, |w, b| (w << 8) | u64::from(*b))
        } else {
            bytes
                .iter()
                .rev()
                .fold(0u64, |w, b| (w << 8) | u64::from(*b))
        };
        let mask = if self.ty.bits == 64 {
            u64::MAX
        } else {
            (1u64 << self.ty.bits) - 1
        };
        let v = (word >> self.ty.shift) & mask;
        if self.ty.signed && self.ty.bits < 64 && (v >> (self.ty.bits - 1)) & 1 == 1 {
            (v as i64).wrapping_sub(1i64 << self.ty.bits)
        } else {
            v as i64
        }
    }
}

/// The scan the kernel lays out for the enabled channels (`iio_compute_scan_bytes`, industrialio-buffer.c:768-802): data channels in index order, each aligned to its storage; the timestamp after them, aligned to its storage; the scan aligned to its largest storage.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Layout {
    axes: [Channel; 3],
    timestamp: Channel,
    size: usize,
}

impl Layout {
    /// One scan: the three axes as raw counts, and the timestamp.
    fn decode(&self, scan: &[u8]) -> ([f64; 3], i64) {
        let raw = self.axes.map(|c| c.value(scan) as f64);
        (raw, self.timestamp.value(scan))
    }
}

/// P1 from a reader of paths relative to the device dir. `None` when a channel is missing, disabled or unparsable, or two channels share an index.
fn layout_from(read: impl Fn(&str) -> Option<String>) -> Option<Layout> {
    let chan = |name: &str| -> Option<(usize, Type)> {
        let index: usize = read(&format!("scan_elements/{name}_index"))?.parse().ok()?;
        if read(&format!("scan_elements/{name}_en"))? != "1" {
            return None;
        }
        Some((
            index,
            parse_type(&read(&format!("scan_elements/{name}_type"))?)?,
        ))
    };
    let [Some(x), Some(y), Some(z)] = AXES.map(|a| chan(&format!("in_accel_{a}"))) else {
        return None;
    };
    let (ts_index, ts_type) = chan("in_timestamp")?;
    let mut indices = [x.0, y.0, z.0, ts_index];
    indices.sort_unstable();
    if indices.windows(2).any(|w| w[0] == w[1]) {
        return None;
    }
    let mut order = [(x.0, 0usize, x.1), (y.0, 1, y.1), (z.0, 2, z.1)];
    order.sort_by_key(|c| c.0);
    let mut slots: [Option<Channel>; 3] = [None; 3];
    let mut bytes = 0usize;
    let mut largest = 0usize;
    for (_, axis, ty) in order {
        bytes = align(bytes, ty.storage);
        slots[axis] = Some(Channel { offset: bytes, ty });
        bytes += ty.storage;
        largest = largest.max(ty.storage);
    }
    bytes = align(bytes, ts_type.storage);
    let timestamp = Channel {
        offset: bytes,
        ty: ts_type,
    };
    bytes += ts_type.storage;
    largest = largest.max(ts_type.storage);
    let [Some(x), Some(y), Some(z)] = slots else {
        return None;
    };
    Some(Layout {
        axes: [x, y, z],
        timestamp,
        size: align(bytes, largest),
    })
}

/// `CLOCK_*` for a `current_timestamp_clock` value (industrialio-core.c:1520-1531).
fn clock_of(text: &str) -> Option<ClockId> {
    Some(match text {
        "realtime" => ClockId::Realtime,
        "monotonic" => ClockId::Monotonic,
        "boottime" => ClockId::Boottime,
        "tai" => ClockId::Tai,
        _ => return None,
    })
}

/// `clock` now, in ns. The samples' clock is `clock_ns(ClockId::Boottime)`.
pub(super) fn clock_ns(clock: ClockId) -> i64 {
    let t = clock_gettime(clock);
    t.tv_sec.saturating_mul(NS_PER_S).saturating_add(t.tv_nsec)
}

/// P2: `trigger/current_trigger` names a trigger in the list (`triggerN/name`).
fn trigger_present(root: &Path, dir: &Path) -> bool {
    let Some(current) = read_text(&dir.join("trigger/current_trigger")) else {
        return false;
    };
    let Ok(entries) = std::fs::read_dir(root) else {
        return false;
    };
    entries
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().starts_with("trigger"))
        .any(|e| read_text(&e.path().join("name")).as_deref() == Some(current.as_str()))
}

/// What the stream decodes with: the scan layout and the clock its stamps are in.
#[derive(Debug, Clone)]
pub(super) struct Setup {
    layout: Layout,
    clock: ClockId,
}

/// P1-P4: the buffer's state, read only. The first failing step names the fallback.
pub(super) fn probe(root: &Path, dir: &Path) -> Result<Setup, Fallback> {
    let layout = layout_from(|rel| read_text(&dir.join(rel))).ok_or(Fallback::Layout)?;
    if !trigger_present(root, dir) {
        return Err(Fallback::Trigger);
    }
    if read_text(&dir.join("buffer/enable")).as_deref() != Some("1") {
        return Err(Fallback::NotEnabled);
    }
    let clock = read_text(&dir.join("current_timestamp_clock"))
        .and_then(|c| clock_of(&c))
        .ok_or(Fallback::Clock)?;
    Ok(Setup { layout, clock })
}

/// P5: the char device opens for reading without blocking. Open sets and close clears the device's busy bit (industrialio-core.c:1859, 1894): a reader holds the device while its fd lives.
pub(super) fn claim(node: &Path) -> Result<OwnedFd, Fallback> {
    rustix::fs::open(
        node,
        OFlags::RDONLY | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|e| match e {
        Errno::BUSY => Fallback::NodeBusy,
        _ => Fallback::NodeDenied,
    })
}

/// The buffered stream: whole scans from the char device, the newest one returned per call, stamped on CLOCK_BOOTTIME.
pub(super) struct Stream {
    fd: OwnedFd,
    layout: Layout,
    clock: ClockId,
    calib: Calibration,
    /// The fresh sample found by the open-time wait, returned by the first `next`.
    pending: Option<Sample>,
    /// Bytes of a scan not yet complete.
    partial: Vec<u8>,
    /// Scans not fresh (stale, or stamped ahead of arrival); each took the arrival time (ts-fallback).
    ts_fallbacks: u64,
}

/// One drained batch's newest scan, stamped.
struct Stamped {
    sample: Sample,
    freshness: Freshness,
}

/// Where a stamp sits against arrival: `Fresh` in the window, `Ahead` past the mapping's error (not in the named clock), `Stale` older than [`FRESH_AGE_NS`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Freshness {
    Fresh,
    Ahead,
    Stale,
}

impl Stream {
    /// P1-P6: the buffered stream, or the first failing step. P6 waits for a fresh scan; stale ring scans are skipped, never returned.
    pub(super) fn open(
        root: &Path,
        dir: &Path,
        node: &Path,
        interval: Duration,
        calib: Calibration,
    ) -> Result<Stream, Fallback> {
        let setup = probe(root, dir)?;
        let fd = claim(node)?;
        let mut stream = Stream {
            fd,
            layout: setup.layout,
            clock: setup.clock,
            calib,
            pending: None,
            partial: Vec::new(),
            ts_fallbacks: 0,
        };
        stream.first_fresh(FRESH_WAIT.max(interval * FRESH_INTERVALS))?;
        Ok(stream)
    }

    /// P6: a fresh scan within `window`, kept as the first sample. A stamp ahead of arrival means the stamps are not in the named clock (`clock`); no fresh scan otherwise, or a read failure, is `no-data`.
    fn first_fresh(&mut self, window: Duration) -> Result<(), Fallback> {
        let deadline = Instant::now() + window;
        let mut ahead = false;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(if ahead {
                    Fallback::Clock
                } else {
                    Fallback::NoData
                });
            }
            match self.read_within(left) {
                Ok(Some(s)) => match s.freshness {
                    Freshness::Fresh => {
                        self.pending = Some(s.sample);
                        return Ok(());
                    }
                    Freshness::Ahead => ahead = true,
                    Freshness::Stale => {}
                },
                Ok(None) => {}
                Err(_) => return Err(Fallback::NoData),
            }
        }
    }

    /// Waits up to `timeout` for data, then reads all of it. `None` when nothing came in time, or only part of a scan did.
    /// The read runs even when poll saw nothing: a FIFO whose writer has gone reports end of file to read but no POLLHUP to a reader opened after the close.
    fn read_within(&mut self, timeout: Duration) -> Result<Option<Stamped>, SensorError> {
        let limit = Timespec {
            tv_sec: i64::try_from(timeout.as_secs()).unwrap_or(i64::MAX),
            tv_nsec: i64::from(timeout.subsec_nanos()),
        };
        let mut fds = [PollFd::new(&self.fd, PollFlags::IN)];
        match poll(&mut fds, Some(&limit)) {
            Ok(_) | Err(Errno::INTR) => {}
            Err(e) => return Err(SensorError(format!("iio buffer poll: {e}"))),
        }
        self.drain()
    }

    /// Reads everything available without blocking and keeps the newest whole scan. End of file with nothing to show is an error: the writer is gone.
    fn drain(&mut self) -> Result<Option<Stamped>, SensorError> {
        let mut chunk = [0u8; READ_CHUNK];
        let mut eof = false;
        loop {
            match rustix::io::read(&self.fd, &mut chunk) {
                Ok(0) => {
                    eof = true;
                    break;
                }
                Ok(n) => self.partial.extend_from_slice(&chunk[..n]),
                Err(Errno::AGAIN) => break,
                Err(Errno::INTR) => {}
                Err(e) => return Err(SensorError(format!("iio buffer read: {e}"))),
            }
        }
        let size = self.layout.size;
        let whole = self.partial.len() / size * size;
        let newest = self.partial[..whole]
            .chunks_exact(size)
            .last()
            .map(|scan| self.layout.decode(scan));
        self.partial.drain(..whole);
        match newest {
            Some((raw, ts)) => Ok(Some(self.stamp(raw, ts))),
            None if eof => Err(SensorError("iio buffer: end of stream".to_owned())),
            None => Ok(None),
        }
    }

    /// The stamp mapped to CLOCK_BOOTTIME: `ts - (named_now - boottime_mid)`. The named clock is read between two boottime reads, so the mapping is off by at most their gap plus [`TICK_NS`]; the midpoint is the arrival reference. A stamp ahead of arrival by more than that is not in the named clock: arrival time is used and the scan is not fresh.
    fn stamp(&self, raw: [f64; 3], ts: i64) -> Stamped {
        let before = clock_ns(ClockId::Boottime);
        let named = clock_ns(self.clock);
        let after = clock_ns(ClockId::Boottime);
        let arrival = (before + after) / 2;
        let mapped = ts - (named - arrival);
        let ahead_limit = (after - before) + TICK_NS;
        let age = arrival - mapped;
        let freshness = if age < -ahead_limit {
            Freshness::Ahead
        } else if age > FRESH_AGE_NS {
            Freshness::Stale
        } else {
            Freshness::Fresh
        };
        Stamped {
            sample: Sample {
                boottime_ns: if freshness == Freshness::Fresh {
                    mapped
                } else {
                    arrival
                },
                values: self.calib.frame(raw),
            },
            freshness,
        }
    }

    /// The first ts-fallback is logged, so a wrong clock shows once; every one is counted.
    fn count_fallback(&mut self) {
        if self.ts_fallbacks == 0 {
            tracing::warn!(
                clock = ?self.clock,
                "iio buffer: scan stamp outside the clock's window, arrival time used (ts-fallback)"
            );
        }
        self.ts_fallbacks += 1;
    }
}

impl SampleStream for Stream {
    fn tier(&self) -> Tier {
        Tier::Buffered
    }

    fn ts_fallbacks(&self) -> u64 {
        self.ts_fallbacks
    }

    fn next(&mut self, timeout: Duration) -> Result<Option<Sample>, SensorError> {
        if let Some(sample) = self.pending.take() {
            return Ok(Some(sample));
        }
        match self.read_within(timeout)? {
            Some(s) => {
                if s.freshness != Freshness::Fresh {
                    self.count_fallback();
                }
                Ok(Some(s.sample))
            }
            None => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;

    use atl_platform_host_api::Sensors;

    use super::*;
    use crate::iio::tests::{Fake, LAPTOP_CASES, RAW, close};

    /// The owner's laptop, `scan_elements` as read 2026-11-08 (`run`): x, y, z `le:s32/32>>0` at indices 0-2, the timestamp `le:s64/64>>0` at index 3.
    const LAPTOP_LAYOUT: [(&str, &str); 12] = [
        ("scan_elements/in_accel_x_index", "0"),
        ("scan_elements/in_accel_x_type", "le:s32/32>>0"),
        ("scan_elements/in_accel_x_en", "1"),
        ("scan_elements/in_accel_y_index", "1"),
        ("scan_elements/in_accel_y_type", "le:s32/32>>0"),
        ("scan_elements/in_accel_y_en", "1"),
        ("scan_elements/in_accel_z_index", "2"),
        ("scan_elements/in_accel_z_type", "le:s32/32>>0"),
        ("scan_elements/in_accel_z_en", "1"),
        ("scan_elements/in_timestamp_index", "3"),
        ("scan_elements/in_timestamp_type", "le:s64/64>>0"),
        ("scan_elements/in_timestamp_en", "1"),
    ];
    /// The buffer on, as udev rule 71 leaves it: the laptop's trigger, realtime stamps, 10 Hz.
    const BUFFER_ON: [(&str, &str); 4] = [
        ("buffer/enable", "1"),
        ("trigger/current_trigger", "accel_3d-dev0"),
        ("current_timestamp_clock", "realtime"),
        ("in_accel_sampling_frequency", "10"),
    ];
    /// The laptop's hwdb matrix: canonical = (x, y, -z).
    const LAPTOP_MATRIX: [[f64; 3]; 3] = [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, -1.0]];

    /// The laptop's files, with `change` replacing or adding keys and `remove` dropping keys.
    fn files(
        change: &[(&'static str, &'static str)],
        remove: &[&str],
    ) -> Vec<(&'static str, &'static str)> {
        let mut v: Vec<(&'static str, &'static str)> = RAW
            .iter()
            .chain(LAPTOP_LAYOUT.iter())
            .chain(BUFFER_ON.iter())
            .copied()
            .filter(|(k, _)| !remove.contains(k))
            .collect();
        for (k, val) in change {
            v.retain(|(kk, _)| kk != k);
            v.push((k, val));
        }
        v
    }

    fn laptop_layout() -> Layout {
        let map: HashMap<&str, &str> = LAPTOP_LAYOUT.into_iter().collect();
        layout_from(|rel| map.get(rel).map(|v| v.to_string())).expect("laptop layout parses")
    }

    fn calib() -> Calibration {
        Calibration {
            scale: [0.01; 3],
            offset: [0.0; 3],
            matrix: [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
        }
    }

    /// One scan as the kernel writes it: x `value` (s32 at 0), y and z zero, the stamp (s64 at 16).
    fn scan(value: i32, stamp: i64) -> [u8; 24] {
        let mut s = [0u8; 24];
        s[0..4].copy_from_slice(&value.to_le_bytes());
        s[16..24].copy_from_slice(&stamp.to_le_bytes());
        s
    }

    fn make_fifo(path: &Path) {
        rustix::fs::mknodat(
            rustix::fs::CWD,
            path,
            rustix::fs::FileType::Fifo,
            Mode::from_raw_mode(0o600),
            0,
        )
        .expect("mkfifo");
    }

    /// Reader-side stand-in for the device: a FIFO kept open by one reader, so a writer can write and close before the stream opens. Returns that reader's fd.
    fn hold_fifo(node: &Path) -> OwnedFd {
        make_fifo(node);
        rustix::fs::open(node, OFlags::RDONLY | OFlags::NONBLOCK, Mode::empty()).expect("reader")
    }

    fn writer(node: &Path) -> std::fs::File {
        std::fs::OpenOptions::new()
            .write(true)
            .open(node)
            .expect("writer")
    }

    /// G1: the offsets the kernel gives this layout (industrialio-buffer.c:768-802 on x, y, z as s32, then the s64 stamp): 0, 4, 8, then 16; the scan is 24 bytes.
    #[test]
    fn laptop_layout_is_offsets_0_4_8_16_size_24() {
        let l = laptop_layout();
        let offsets = [
            l.axes[0].offset,
            l.axes[1].offset,
            l.axes[2].offset,
            l.timestamp.offset,
        ];
        assert_eq!(offsets, [0, 4, 8, 16]);
        assert_eq!(l.size, 24);
    }

    /// G1: swapping the y and z indices (the break `order`) puts z at 4 and y at 8: the decoder follows `_index`, not the axis name.
    #[test]
    fn indices_decide_the_order_not_the_names() {
        let mut map: HashMap<&str, &str> = LAPTOP_LAYOUT.into_iter().collect();
        map.insert("scan_elements/in_accel_y_index", "2");
        map.insert("scan_elements/in_accel_z_index", "1");
        let l = layout_from(|rel| map.get(rel).map(|v| v.to_string())).expect("parses");
        assert_eq!([l.axes[1].offset, l.axes[2].offset], [8, 4]);
        // two channels on one index: no layout
        map.insert("scan_elements/in_accel_z_index", "0");
        assert!(layout_from(|rel| map.get(rel).map(|v| v.to_string())).is_none());
    }

    /// G1: `le:s12/16>>4` (iio_devbuf.rst:116-124): the 12 real bits sit at bit 4 of a 16-bit little-endian word; the low nibble is not the value. -5 in 12 bits is 0xffb.
    #[test]
    fn masked_channel_reads_its_bits_and_sign_extends() {
        let ch = Channel {
            offset: 0,
            ty: parse_type("le:s12/16>>4").expect("parses"),
        };
        // word 0xffb7: value 0xffb (-5), low nibble 0x7 is other data
        assert_eq!(ch.value(&[0xb7, 0xff]), -5);
        // word 0x0123 >> 4 = 0x012 = 18
        assert_eq!(ch.value(&[0x23, 0x01]), 18);
        // big-endian unsigned 16: bytes 0x12 0x34 = 4660
        let be = Channel {
            offset: 0,
            ty: parse_type("be:u16/16>>0").expect("parses"),
        };
        assert_eq!(be.value(&[0x12, 0x34]), 4660);
    }

    #[test]
    fn unsupported_types_are_refused() {
        // real bits + shift exceed the storage word
        assert!(parse_type("le:s12/12>>4").is_none());
        assert!(parse_type("xe:s32/32>>0").is_none());
        // storage is not 1, 2, 4 or 8 bytes
        assert!(parse_type("le:s24/24>>0").is_none());
        assert!(parse_type("le:x32/32>>0").is_none());
    }

    /// G2: scans packed as the kernel writes them decode to the frame the sysfs path gives for the same raw counts, and to the laptop table (expected values by hand, `iio/mod.rs` tests).
    #[test]
    fn buffered_decode_equals_polled_values() {
        let layout = laptop_layout();
        let calibration = Calibration {
            scale: [0.01; 3],
            offset: [0.0; 3],
            matrix: LAPTOP_MATRIX,
        };
        let db = "E:ACCEL_MOUNT_MATRIX=1, 0, 0; 0, 1, 0; 0, 0, -1\n";
        for (raw, want) in LAPTOP_CASES {
            let mut s = [0u8; 24];
            for (i, v) in raw.iter().enumerate() {
                let v: i32 = v.parse().expect("raw");
                s[4 * i..4 * i + 4].copy_from_slice(&v.to_le_bytes());
            }
            s[16..24].copy_from_slice(&1_234_567_890i64.to_le_bytes());
            let (r, ts) = layout.decode(&s);
            assert_eq!(ts, 1_234_567_890);
            let buffered = calibration.frame(r);
            assert!(
                close(buffered, want),
                "{raw:?}: buffered {buffered:?}, want {want:?}"
            );
            // the polled path on the same raw counts (no buffer files: it says `not-enabled`)
            let f = Fake::new(
                "g2",
                &[(
                    &files(
                        &[
                            ("in_accel_x_raw", raw[0]),
                            ("in_accel_y_raw", raw[1]),
                            ("in_accel_z_raw", raw[2]),
                        ],
                        &["buffer/enable"],
                    ),
                    Some(db),
                )],
            );
            let polled = f
                .sensors()
                .open("iio:device0")
                .expect("opens")
                .next(Duration::ZERO)
                .expect("reads")
                .expect("a sample")
                .values;
            assert!(
                close(buffered, polled),
                "{raw:?}: buffered {buffered:?}, polled {polled:?}"
            );
        }
    }

    /// G3: each probe step's failure gives its label (design 5.1). The steps run in order: the first failure is the reason.
    #[test]
    fn probe_names_the_first_failing_step() {
        let reason = |files: &[(&str, &str)]| {
            let f = Fake::new("probe", &[(files, None)]);
            probe(&f.0.join("sys"), &f.dir(0)).map(|_| ()).err()
        };
        assert_eq!(reason(&files(&[], &[])), None);
        assert_eq!(
            reason(&files(&[], &["scan_elements/in_accel_y_en"])),
            Some(Fallback::Layout)
        );
        assert_eq!(
            reason(&files(&[("trigger/current_trigger", "missing")], &[])),
            Some(Fallback::Trigger)
        );
        assert_eq!(
            reason(&files(&[("buffer/enable", "0")], &[])),
            Some(Fallback::NotEnabled)
        );
        assert_eq!(
            reason(&files(&[("current_timestamp_clock", "tai2")], &[])),
            Some(Fallback::Clock)
        );
    }

    /// G3: a node the process may not open is `node-denied` (mode 000; the test runs as an ordinary user).
    #[test]
    fn unopenable_node_is_node_denied() {
        let f = Fake::new("denied", &[(&files(&[], &[]), None)]);
        let node = f.node(0);
        make_fifo(&node);
        std::fs::set_permissions(&node, std::fs::Permissions::from_mode(0o000)).expect("chmod");
        assert_eq!(claim(&node).err(), Some(Fallback::NodeDenied));
    }

    /// G3: the whole path through [`IioSensors::open`]: a buffer that is on but has no writer gives the polled tier with `no-data` after the open-time wait.
    #[test]
    fn enabled_buffer_without_data_is_no_data() {
        let f = Fake::new("nodata", &[(&files(&[], &[]), None)]);
        let node = f.node(0);
        // a live writer that never writes: the open waits its window (without a writer a FIFO reads end-of-file at once)
        let _reader = hold_fifo(&node);
        let _w = writer(&node);
        let sensors = f.sensors();
        let started = Instant::now();
        let st = sensors.open("iio:device0").expect("opens");
        assert_eq!(st.tier(), Tier::Polled(Fallback::NoData));
        assert!(
            started.elapsed() >= FRESH_WAIT,
            "waits the open-time window"
        );
    }

    /// G4: newest scan wins; the end of a writer's stream is an error. The writer writes and closes before the stream opens, so the open-time wait reads all three.
    #[test]
    fn newest_scan_wins_and_a_closed_writer_is_an_error() {
        let f = Fake::new("fifo-newest", &[(&files(&[], &[]), None)]);
        let node = f.node(0);
        let _reader = hold_fifo(&node);
        let mut w = writer(&node);
        for v in [1, 2, 3] {
            w.write_all(&scan(v, clock_ns(ClockId::Realtime)))
                .expect("write");
        }
        drop(w);
        let mut s = Stream::open(
            &f.0.join("sys"),
            &f.dir(0),
            &node,
            Duration::from_millis(100),
            calib(),
        )
        .expect("opens on the fresh scans");
        let first = s
            .next(Duration::from_millis(100))
            .expect("reads")
            .expect("a sample");
        // x = 3 counts at 0.01 m/s^2, Android x negated
        assert!(
            close([first.values[0], 0.0, 0.0], [-0.03, 0.0, 0.0]),
            "{:?}",
            first.values
        );
        // nothing left, and the writer has gone: an error, not a silent repeat
        assert!(s.next(Duration::from_millis(50)).is_err());
    }

    /// G4: a stale scan (stamp 5 s old, as the ring keeps while disabled) is not the open's sample; a fresh one is. The stale-only case gives `no-data`.
    #[test]
    fn stale_scans_are_skipped_at_open() {
        let f = Fake::new("fifo-stale", &[(&files(&[], &[]), None)]);
        let node = f.node(0);
        let _reader = hold_fifo(&node);
        let mut w = writer(&node);
        let now = clock_ns(ClockId::Realtime);
        w.write_all(&scan(7, now - 5 * NS_PER_S)).expect("write");
        // stale only: the open waits out its window and reports no-data
        let stale_only = Stream::open(
            &f.0.join("sys"),
            &f.dir(0),
            &node,
            Duration::from_millis(100),
            calib(),
        );
        assert!(matches!(stale_only, Err(Fallback::NoData)));
        // stale, then a fresh scan
        w.write_all(&scan(7, now - 5 * NS_PER_S)).expect("write");
        w.write_all(&scan(8, clock_ns(ClockId::Realtime)))
            .expect("write");
        let mut s = Stream::open(
            &f.0.join("sys"),
            &f.dir(0),
            &node,
            Duration::from_millis(100),
            calib(),
        )
        .expect("opens on the fresh scan");
        let first = s
            .next(Duration::from_millis(50))
            .expect("reads")
            .expect("a sample");
        assert!(
            close([first.values[0], 0.0, 0.0], [-0.08, 0.0, 0.0]),
            "{:?}",
            first.values
        );
        let age = clock_ns(ClockId::Boottime) - first.boottime_ns;
        assert!(age.abs() < FRESH_AGE_NS, "fresh stamp, age {age} ns");
    }

    /// G4: a writer that pauses (fd open, no scans) gives `None` after the asked timeout, not an error; the scan written before the pause is still delivered first.
    #[test]
    fn paused_writer_gives_none_after_the_timeout() {
        let f = Fake::new("fifo-pause", &[(&files(&[], &[]), None)]);
        let node = f.node(0);
        let _reader = hold_fifo(&node);
        let mut w = writer(&node);
        w.write_all(&scan(4, clock_ns(ClockId::Realtime)))
            .expect("write");
        let mut s = Stream::open(
            &f.0.join("sys"),
            &f.dir(0),
            &node,
            Duration::from_millis(100),
            calib(),
        )
        .expect("opens");
        assert!(s.next(Duration::from_millis(100)).expect("reads").is_some());
        let started = Instant::now();
        assert!(s.next(Duration::from_millis(100)).expect("reads").is_none());
        let waited = started.elapsed();
        assert!(
            waited >= Duration::from_millis(90) && waited < Duration::from_secs(1),
            "waited {waited:?}"
        );
        drop(w);
    }

    /// The `ts-fallback` count: after a fresh open, a scan stamped 5 s ago is returned at arrival time and counted once.
    #[test]
    fn stamp_outside_the_clock_counts_a_ts_fallback() {
        let f = Fake::new("fifo-fallback", &[(&files(&[], &[]), None)]);
        let node = f.node(0);
        let _reader = hold_fifo(&node);
        let mut w = writer(&node);
        w.write_all(&scan(1, clock_ns(ClockId::Realtime)))
            .expect("write");
        let mut s = Stream::open(
            &f.0.join("sys"),
            &f.dir(0),
            &node,
            Duration::from_millis(100),
            calib(),
        )
        .expect("opens");
        assert!(s.next(Duration::from_millis(100)).expect("reads").is_some());
        assert_eq!(s.ts_fallbacks(), 0);
        w.write_all(&scan(2, clock_ns(ClockId::Realtime) - 5 * NS_PER_S))
            .expect("write");
        let got = s
            .next(Duration::from_millis(100))
            .expect("reads")
            .expect("a sample");
        assert!(
            (got.boottime_ns - clock_ns(ClockId::Boottime)).abs() < FRESH_AGE_NS,
            "arrival time used"
        );
        assert_eq!(s.ts_fallbacks(), 1);
        drop(w);
    }

    /// The stamp mapping for `realtime`: a scan stamped 300 ms before the realtime clock lands 300 ms before CLOCK_BOOTTIME. A stamp ahead is not in the clock: see `stamp_ahead_of_arrival_is_not_fresh_and_opens_as_clock`.
    #[test]
    fn realtime_stamps_map_to_boottime() {
        let f = Fake::new("fifo-clock", &[(&files(&[], &[]), None)]);
        let node = f.node(0);
        let _reader = hold_fifo(&node);
        let mut w = writer(&node);
        w.write_all(&scan(1, clock_ns(ClockId::Realtime) - 300_000_000))
            .expect("write");
        let mut s = Stream::open(
            &f.0.join("sys"),
            &f.dir(0),
            &node,
            Duration::from_millis(100),
            calib(),
        )
        .expect("opens");
        let got = s
            .next(Duration::from_millis(100))
            .expect("reads")
            .expect("a sample");
        let behind = clock_ns(ClockId::Boottime) - got.boottime_ns;
        assert!(
            (behind - 300_000_000).abs() < 100_000_000,
            "mapped {behind} ns behind"
        );
        drop(w);
    }

    /// A stamp a second ahead of arrival is not in the realtime clock: the open finds no fresh scan and falls back as `clock`.
    #[test]
    fn stamp_ahead_of_arrival_is_not_fresh_and_opens_as_clock() {
        let f = Fake::new("fifo-ahead", &[(&files(&[], &[]), None)]);
        let node = f.node(0);
        let _reader = hold_fifo(&node);
        let mut w = writer(&node);
        w.write_all(&scan(1, clock_ns(ClockId::Realtime) + NS_PER_S))
            .expect("write");
        let sensors = f.sensors();
        let st = sensors.open("iio:device0").expect("opens");
        assert_eq!(st.tier(), Tier::Polled(Fallback::Clock));
        drop(w);
    }

    /// A scan stamped ahead of arrival after the open is returned at arrival time and counted, as a stale one is.
    #[test]
    fn ahead_scan_after_open_is_a_ts_fallback() {
        let f = Fake::new("fifo-ahead-run", &[(&files(&[], &[]), None)]);
        let node = f.node(0);
        let _reader = hold_fifo(&node);
        let mut w = writer(&node);
        w.write_all(&scan(1, clock_ns(ClockId::Realtime)))
            .expect("write");
        let mut s = Stream::open(
            &f.0.join("sys"),
            &f.dir(0),
            &node,
            Duration::from_millis(100),
            calib(),
        )
        .expect("opens");
        assert!(s.next(Duration::from_millis(100)).expect("reads").is_some());
        w.write_all(&scan(2, clock_ns(ClockId::Realtime) + NS_PER_S))
            .expect("write");
        let got = s
            .next(Duration::from_millis(100))
            .expect("reads")
            .expect("a sample");
        assert!(
            (got.boottime_ns - clock_ns(ClockId::Boottime)).abs() < FRESH_AGE_NS,
            "arrival time used"
        );
        assert_eq!(s.ts_fallbacks(), 1);
        drop(w);
    }
}
