// SPDX-License-Identifier: MIT
//! The sensor service's logic (`sensorservice`, AOSP frameworks/native/services/sensorservice; the binder side is sensor_wire.rs): the sensor list, each client's event channel and the sources polled for them.
//! Sensors come from the platform layer ([`atl_platform_host::Sensors`]): one accelerometer per device it lists; none listed -> an empty list, which SensorManager takes as "no sensors" (getDefaultSensor null, registerListener false).
//! AOSP behaviour kept:
//! - list order = handle order, handles from 1 (0 is SENSOR_TYPE_META_DATA's sensor field in a flush event)
//! - enable: unknown handle -> BAD_VALUE; period clamped to [minDelay, maxDelay] (SensorService.cpp `enable` :2124-2131, `setEventRate` :2244-2247)
//! - one source read per sensor, at the shortest period any connection asked for, every event to every connection with the sensor enabled (SensorService::threadLoop fans one HAL stream out)
//! - events are `ASensorEvent` records sent whole on the connection's SOCK_SEQPACKET channel without blocking; a full channel drops the event (AOSP caches it in mEventCache: not done, counted)
//! - timestamps on CLOCK_BOOTTIME, the base of SystemClock.elapsedRealtimeNanos (SensorEvent.timestamp docs)

use std::collections::BTreeMap;
use std::os::fd::OwnedFd;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use atl_platform_host::{Accelerometer, Sensors};
use rustix::net::{AddressFamily, SendFlags, SocketFlags, SocketType};

use crate::counters;

/// `ASensorEvent` as clang lays it out from the pinned android/sensor.h (build.rs).
#[allow(clippy::all)]
mod layout {
    include!(concat!(env!("OUT_DIR"), "/sensor_event.rs"));
}

/// The interface name counters use.
const INTERFACE: &str = "ISensorServer";
/// One event record.
pub const EVENT_SIZE: usize = layout::ASENSOR_EVENT_SIZE as usize;

/// status_t values the wire answers with (utils/Errors.h: negated errno).
pub const OK: i32 = 0;
pub const BAD_VALUE: i32 = -(rustix::io::Errno::INVAL.raw_os_error());

/// Sensor.h / hardware/sensors-base.h: SENSOR_STRING_TYPE_ACCELEROMETER (Sensor.cpp ctor :95-98 sets it with SENSOR_FLAG_CONTINUOUS_MODE, which is 0).
const STRING_TYPE_ACCELEROMETER: &str = "android.sensor.accelerometer";
/// What the sensor record names as vendor.
const VENDOR: &str = "ATL (Linux IIO)";
/// Sensor.cpp's version of a HAL 1.x+ sensor record.
const SENSOR_VERSION: i32 = 1;

/// SensorService.h SOCKET_BUFFER_SIZE_NON_BATCHED: each channel's receive and send buffer (no FIFO here: no batching).
const SOCKET_BUFFER_SIZE: usize = 4 * 1024;
/// BitTube.cpp DEFAULT_SOCKET_BUFFER_SIZE: the unused return direction (acks of wake-up sensors only).
const RETURN_BUFFER_SIZE: usize = 4 * 1024;

/// One sensor as the list reports it (libsensor `Sensor` fields).
#[derive(Debug, Clone, PartialEq)]
pub struct SensorInfo {
    pub handle: i32,
    pub name: String,
    pub sensor_type: i32,
    pub string_type: &'static str,
    pub max_range: f32,
    pub resolution: f32,
    /// mA; the source states none.
    pub power: f32,
    /// µs, > 0: a continuous sensor.
    pub min_delay_us: i32,
    /// µs, 0: no limit.
    pub max_delay_us: i32,
    source: String,
}

impl SensorInfo {
    fn accelerometer(handle: i32, a: &Accelerometer) -> SensorInfo {
        SensorInfo {
            handle,
            name: a.name.clone(),
            sensor_type: layout::ASENSOR_TYPE_ACCELEROMETER_ID,
            string_type: STRING_TYPE_ACCELEROMETER,
            max_range: a.max_range,
            resolution: a.resolution,
            power: 0.0,
            min_delay_us: i32::try_from(a.min_period.as_micros().max(1)).unwrap_or(i32::MAX),
            max_delay_us: 0,
            source: a.id.clone(),
        }
    }

    fn min_delay_ns(&self) -> i64 {
        i64::from(self.min_delay_us) * 1000
    }

    /// libsensor `Sensor::flatten` (Sensor.cpp:548-574): field order and widths as there; strings `uint32 len` + bytes padded to 4; the bool is 1 byte, so the fields after it are unaligned.
    pub fn flatten(&self) -> Vec<u8> {
        fn string(out: &mut Vec<u8>, s: &str) {
            out.extend((s.len() as u32).to_ne_bytes());
            out.extend(s.as_bytes());
            out.resize(out.len().next_multiple_of(4), 0);
        }
        let mut o = Vec::new();
        string(&mut o, &self.name);
        string(&mut o, VENDOR);
        o.extend(SENSOR_VERSION.to_ne_bytes());
        o.extend(self.handle.to_ne_bytes());
        o.extend(self.sensor_type.to_ne_bytes());
        // min value: Sensor.cpp ctor :56 sets 0 for every HAL sensor
        o.extend(0f32.to_ne_bytes());
        o.extend(self.max_range.to_ne_bytes());
        o.extend(self.resolution.to_ne_bytes());
        o.extend(self.power.to_ne_bytes());
        o.extend(self.min_delay_us.to_ne_bytes());
        // FIFO reserved / max event count: no FIFO
        o.extend(0u32.to_ne_bytes());
        o.extend(0u32.to_ne_bytes());
        string(&mut o, self.string_type);
        // required permission: none for an accelerometer
        string(&mut o, "");
        // mRequiredPermissionRuntime (bool)
        o.push(0);
        // mRequiredAppOp: none (-1, Sensor.cpp ctor :41)
        o.extend((-1i32).to_ne_bytes());
        o.extend(self.max_delay_us.to_ne_bytes());
        // flags: SENSOR_FLAG_CONTINUOUS_MODE (0), not wake-up
        o.extend(0u32.to_ne_bytes());
        // uuid: none; id 0 (makeUuidsIntoIdsForSensorList maps a zero uuid to id 0)
        o.extend([0u8; 16]);
        o.extend(0i32.to_ne_bytes());
        o
    }
}

/// One `ASensorEvent` (the data union's `float data[16]` from its first element).
pub fn encode_event(
    handle: i32,
    sensor_type: i32,
    timestamp_ns: i64,
    values: &[f32],
) -> [u8; EVENT_SIZE] {
    let mut e = [0u8; EVENT_SIZE];
    let mut put = |off: u64, b: &[u8]| {
        let off = off as usize;
        e[off..off + b.len()].copy_from_slice(b);
    };
    // version: sizeof(struct ASensorEvent)
    put(
        layout::ASENSOR_EVENT_VERSION,
        &(EVENT_SIZE as i32).to_ne_bytes(),
    );
    put(layout::ASENSOR_EVENT_SENSOR, &handle.to_ne_bytes());
    put(layout::ASENSOR_EVENT_TYPE, &sensor_type.to_ne_bytes());
    put(layout::ASENSOR_EVENT_TIMESTAMP, &timestamp_ns.to_ne_bytes());
    for (i, v) in values
        .iter()
        .take(layout::ASENSOR_EVENT_DATA_LEN as usize)
        .enumerate()
    {
        put(layout::ASENSOR_EVENT_DATA + 4 * i as u64, &v.to_ne_bytes());
    }
    put(layout::ASENSOR_EVENT_FLAGS, &0u32.to_ne_bytes());
    e
}

/// A `sensor: ` line on the services' stderr (the gate reads the started / stopped lines).
fn log(msg: &str) {
    use std::io::Write;
    let _ = writeln!(std::io::stderr(), "sensor: {msg}");
}

/// CLOCK_BOOTTIME, ns.
fn boottime_ns() -> i64 {
    let t = rustix::time::clock_gettime(rustix::time::ClockId::Boottime);
    t.tv_sec * 1_000_000_000 + t.tv_nsec
}

/// The server end of a client's event channel (BitTube: `socketpair(AF_UNIX, SOCK_SEQPACKET)`, both ends non-blocking, BitTube.cpp `init` :68-85).
#[derive(Debug)]
pub struct Channel {
    send: OwnedFd,
    receive: Mutex<Option<OwnedFd>>,
    dead: AtomicBool,
}

impl Channel {
    pub fn open() -> rustix::io::Result<Channel> {
        let (receive, send) = rustix::net::socketpair(
            AddressFamily::UNIX,
            SocketType::SEQPACKET,
            SocketFlags::NONBLOCK | SocketFlags::CLOEXEC,
            None,
        )?;
        rustix::net::sockopt::set_socket_recv_buffer_size(&receive, SOCKET_BUFFER_SIZE)?;
        rustix::net::sockopt::set_socket_send_buffer_size(&send, SOCKET_BUFFER_SIZE)?;
        rustix::net::sockopt::set_socket_send_buffer_size(&receive, RETURN_BUFFER_SIZE)?;
        rustix::net::sockopt::set_socket_recv_buffer_size(&send, RETURN_BUFFER_SIZE)?;
        Ok(Channel {
            send,
            receive: Mutex::new(Some(receive)),
            dead: AtomicBool::new(false),
        })
    }

    /// The client end, once: BitTube::writeToParcel closes its copy after the reply took a dup (:131-140), so a second ask has none.
    pub fn take_receive(&self) -> Option<OwnedFd> {
        // a poisoned lock still holds a plain fd
        self.receive
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
    }

    /// BitTube::sendObjects: one packet, never blocking. false once the client end is gone.
    fn send(&self, event: &[u8]) -> bool {
        match rustix::net::send(&self.send, event, SendFlags::DONTWAIT | SendFlags::NOSIGNAL) {
            Ok(_) => true,
            Err(rustix::io::Errno::AGAIN) => {
                counters::answered(INTERFACE, "event dropped: channel full");
                true
            }
            Err(_) => {
                self.dead.store(true, Ordering::Relaxed);
                false
            }
        }
    }
}

/// One enabled sensor: its subscribers and the stop flag of its poll thread.
struct Active {
    /// connection id -> (period ns, channel)
    subscribers: BTreeMap<u64, (i64, Weak<Channel>)>,
    stop: Arc<AtomicBool>,
}

struct State {
    active: BTreeMap<i32, Active>,
}

/// The sensors and their enabled state, shared by all connections.
pub struct SensorHub {
    sensors: Vec<SensorInfo>,
    source: Arc<dyn Sensors>,
    state: Mutex<State>,
    next_connection: AtomicU64,
}

impl SensorHub {
    /// The sensors `source` lists now, handles from 1 (SensorService `registerSensor` at start; no hot-plug).
    pub fn new(source: Arc<dyn Sensors>) -> Arc<SensorHub> {
        let sensors = source
            .accelerometers()
            .iter()
            .zip(1..)
            .map(|(a, h)| SensorInfo::accelerometer(h, a))
            .collect();
        Arc::new(SensorHub {
            sensors,
            source,
            state: Mutex::new(State {
                active: BTreeMap::new(),
            }),
            next_connection: AtomicU64::new(1),
        })
    }

    /// A poisoned lock still holds consistent maps (every update is one insert or remove).
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// SensorService::getSensorList (:1522): every sensor; no capped sensors (the rate cap is for mic-adjacent types).
    pub fn sensor_list(&self) -> &[SensorInfo] {
        &self.sensors
    }

    /// A new connection's id.
    pub fn connection_id(&self) -> u64 {
        self.next_connection.fetch_add(1, Ordering::Relaxed)
    }

    fn sensor(&self, handle: i32) -> Option<&SensorInfo> {
        self.sensors.iter().find(|s| s.handle == handle)
    }

    /// The period `enable` / `setEventRate` use: clamped to the sensor's limits (SensorService.cpp :2124-2131).
    fn clamp(s: &SensorInfo, period_ns: i64) -> i64 {
        let max = i64::from(s.max_delay_us) * 1000;
        let p = if max > 0 && period_ns > max {
            max
        } else {
            period_ns
        };
        p.max(s.min_delay_ns())
    }

    /// SensorService::enable for one connection: subscribes it and starts the source when it is the first. BAD_VALUE for a handle not in the list.
    pub fn enable(
        self: &Arc<Self>,
        connection: u64,
        channel: &Arc<Channel>,
        handle: i32,
        period_ns: i64,
    ) -> i32 {
        let Some(s) = self.sensor(handle) else {
            return BAD_VALUE;
        };
        let period = Self::clamp(s, period_ns);
        let mut st = self.lock();
        let start = !st.active.contains_key(&handle);
        let a = st.active.entry(handle).or_insert_with(|| Active {
            subscribers: BTreeMap::new(),
            stop: Arc::new(AtomicBool::new(false)),
        });
        a.subscribers
            .insert(connection, (period, Arc::downgrade(channel)));
        if start {
            let stop = Arc::clone(&a.stop);
            let hub = Arc::clone(self);
            let s = s.clone();
            log(&format!("{} (handle {handle}) started", s.name));
            std::thread::spawn(move || hub.poll(s, stop));
        }
        OK
    }

    /// SensorService::setEventRate (:2230): a negative period is BAD_VALUE; a sensor this connection has not enabled is BAD_VALUE too (SensorDevice::setDelay finds no batch params).
    pub fn set_rate(&self, connection: u64, handle: i32, period_ns: i64) -> i32 {
        let Some(s) = self.sensor(handle) else {
            return BAD_VALUE;
        };
        if period_ns < 0 {
            return BAD_VALUE;
        }
        let period = Self::clamp(s, period_ns);
        let mut st = self.lock();
        match st
            .active
            .get_mut(&handle)
            .and_then(|a| a.subscribers.get_mut(&connection))
        {
            Some(sub) => {
                sub.0 = period;
                OK
            }
            None => BAD_VALUE,
        }
    }

    /// SensorService::disable: unsubscribes; the last one stops the source. BAD_VALUE when it was not enabled (SensorService.cpp `disable` :2185 -> cleanupWithoutDisableLocked finds no record).
    pub fn disable(&self, connection: u64, handle: i32) -> i32 {
        let mut st = self.lock();
        let Some(a) = st.active.get_mut(&handle) else {
            return BAD_VALUE;
        };
        if a.subscribers.remove(&connection).is_none() {
            return BAD_VALUE;
        }
        if a.subscribers.is_empty() {
            a.stop.store(true, Ordering::Relaxed);
            st.active.remove(&handle);
        }
        OK
    }

    /// The connection is gone (DESTROY or its binder object dropped): every sensor it enabled is disabled.
    pub fn drop_connection(&self, connection: u64) {
        let handles: Vec<i32> = {
            let st = self.lock();
            st.active
                .iter()
                .filter(|(_, a)| a.subscribers.contains_key(&connection))
                .map(|(h, _)| *h)
                .collect()
        };
        for h in handles {
            self.disable(connection, h);
        }
    }

    /// Handles `connection` has enabled.
    pub fn enabled(&self, connection: u64) -> Vec<i32> {
        let st = self.lock();
        st.active
            .iter()
            .filter(|(_, a)| a.subscribers.contains_key(&connection))
            .map(|(h, _)| *h)
            .collect()
    }

    /// The poll loop of one enabled sensor: read, stamp, send to each subscriber, sleep the shortest asked period.
    fn poll(&self, s: SensorInfo, stop: Arc<AtomicBool>) {
        while !stop.load(Ordering::Relaxed) {
            let (period, channels): (i64, Vec<(u64, Arc<Channel>)>) = {
                let st = self.lock();
                let Some(a) = st.active.get(&s.handle) else {
                    break;
                };
                let period = a
                    .subscribers
                    .values()
                    .map(|(p, _)| *p)
                    .min()
                    .unwrap_or(s.min_delay_ns());
                let ch = a
                    .subscribers
                    .iter()
                    .filter_map(|(c, (_, w))| w.upgrade().map(|ch| (*c, ch)))
                    .collect();
                (period, ch)
            };
            match self.source.read_accel(&s.source) {
                Ok(v) => {
                    let event = encode_event(s.handle, s.sensor_type, boottime_ns(), &v);
                    for (c, ch) in &channels {
                        if ch.dead.load(Ordering::Relaxed) || !ch.send(&event) {
                            self.disable(*c, s.handle);
                        }
                    }
                }
                Err(e) => {
                    counters::answered(INTERFACE, &format!("source read failed: {e}"));
                }
            }
            std::thread::sleep(Duration::from_nanos(u64::try_from(period).unwrap_or(0)));
        }
        log(&format!("{} (handle {}) stopped", s.name, s.handle));
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use atl_platform_host::SensorError;

    use super::*;

    /// A test double of the platform source: one accelerometer with fixed values.
    struct Fixed;

    impl Sensors for Fixed {
        fn accelerometers(&self) -> Vec<Accelerometer> {
            vec![Accelerometer {
                id: "dev0".into(),
                name: "fake accel".into(),
                max_range: 19.6,
                resolution: 0.01,
                min_period: Duration::from_millis(5),
            }]
        }
        fn read_accel(&self, _: &str) -> Result<[f32; 3], SensorError> {
            Ok([1.0, -2.0, 9.5])
        }
    }

    struct Nothing;

    impl Sensors for Nothing {
        fn accelerometers(&self) -> Vec<Accelerometer> {
            Vec::new()
        }
        fn read_accel(&self, id: &str) -> Result<[f32; 3], SensorError> {
            Err(SensorError(id.into()))
        }
    }

    fn recv_event(ch: &Channel, fd: &OwnedFd) -> [u8; EVENT_SIZE] {
        let _ = ch;
        let mut buf = [0u8; EVENT_SIZE];
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match rustix::net::recv(fd, &mut buf, rustix::net::RecvFlags::DONTWAIT) {
                Ok((n, _)) => {
                    assert_eq!(n, EVENT_SIZE, "one whole record per packet");
                    return buf;
                }
                Err(rustix::io::Errno::AGAIN) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(2))
                }
                Err(e) => panic!("no event: {e}"),
            }
        }
    }

    #[test]
    fn empty_source_lists_nothing_and_refuses_enable() {
        let hub = SensorHub::new(Arc::new(Nothing));
        assert!(hub.sensor_list().is_empty());
        let ch = Arc::new(Channel::open().unwrap());
        assert_eq!(hub.enable(1, &ch, 1, 20_000_000), BAD_VALUE);
    }

    #[test]
    fn flatten_matches_sensor_cpp_field_widths() {
        let hub = SensorHub::new(Arc::new(Fixed));
        let s = &hub.sensor_list()[0];
        assert_eq!(s.handle, 1);
        // fixed (Sensor::getFlattenedSize): 4 version + 4 handle + 4 type + 4x4 floats + 4 minDelay + 2x4 fifo + 1 bool + 4 appOp + 4 maxDelay + 4 flags + 16 uuid + 4 id = 73;
        // strings: name "fake accel" 4+12, vendor "ATL (Linux IIO)" 4+16, string type (28 chars) 4+28, permission "" 4
        assert_eq!(s.flatten().len(), 73 + 16 + 20 + 32 + 4);
        // min delay: 5 ms in µs at the offset after name, vendor and seven 4-byte fields
        let off = 16 + 20 + 7 * 4;
        assert_eq!(&s.flatten()[off..off + 4], &5000i32.to_ne_bytes());
    }

    #[test]
    fn events_reach_the_channel_with_aosp_layout_and_stop_on_last_disable() {
        let hub = SensorHub::new(Arc::new(Fixed));
        let ch = Arc::new(Channel::open().unwrap());
        let rx = ch.take_receive().unwrap();
        assert!(ch.take_receive().is_none(), "the client end is given once");
        let before = boottime_ns();
        // asked faster than minDelay: clamped, still accepted
        assert_eq!(hub.enable(7, &ch, 1, 1_000), OK);
        assert_eq!(hub.enable(8, &ch, 1, 20_000_000), OK);
        let e = recv_event(&ch, &rx);
        // offsets from android/sensor.h: version @0, sensor @4, type @8, timestamp @16, data @24
        assert_eq!(&e[0..4], &104i32.to_ne_bytes());
        assert_eq!(&e[4..8], &1i32.to_ne_bytes());
        assert_eq!(&e[8..12], &1i32.to_ne_bytes());
        let ts = i64::from_ne_bytes(e[16..24].try_into().unwrap());
        assert!(ts >= before && ts <= boottime_ns());
        let v: Vec<f32> = (0..3)
            .map(|i| f32::from_ne_bytes(e[24 + 4 * i..28 + 4 * i].try_into().unwrap()))
            .collect();
        assert_eq!(v, [1.0, -2.0, 9.5]);
        assert_eq!(hub.disable(7, 1), OK);
        assert_eq!(hub.enabled(8), vec![1]);
        assert_eq!(hub.disable(7, 1), BAD_VALUE, "not enabled any more");
        assert_eq!(hub.set_rate(8, 1, -1), BAD_VALUE);
        assert_eq!(hub.set_rate(8, 1, 0), OK);
        hub.drop_connection(8);
        assert!(hub.enabled(8).is_empty());
        assert!(
            hub.state.lock().unwrap().active.is_empty(),
            "the last disable stops the source"
        );
    }

    #[test]
    fn clamp_raises_to_min_delay() {
        let hub = SensorHub::new(Arc::new(Fixed));
        let s = &hub.sensor_list()[0];
        assert_eq!(SensorHub::clamp(s, 1), 5_000_000);
        assert_eq!(SensorHub::clamp(s, 20_000_000), 20_000_000);
    }
}
