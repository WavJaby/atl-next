// SPDX-License-Identifier: MIT
//! The sensor service's logic (`sensorservice`, AOSP frameworks/native/services/sensorservice; the binder side is sensor_wire.rs): the sensor list, each client's event channel and the sources polled for them.
//! Sensors come from the platform layer ([`atl_platform_host::Sensors`]): one accelerometer per device it lists; none listed -> an empty list, which SensorManager takes as "no sensors" (getDefaultSensor null, registerListener false).
//! AOSP behaviour kept:
//! - list order = handle order, handles from 1 (0 is SENSOR_TYPE_META_DATA's sensor field in a flush event)
//! - enable: unknown handle -> BAD_VALUE; period clamped to [minDelay, maxDelay] (SensorService.cpp `enable` :2124-2131, `setEventRate` :2244-2247)
//! - one source read per sensor, at the shortest period any connection asked for, every event to every connection with the sensor enabled (SensorService::threadLoop fans one HAL stream out)
//! - events are `ASensorEvent` records sent whole on the connection's SOCK_SEQPACKET channel without blocking; a full channel drops the event (AOSP caches it in mEventCache: not done, counted)
//! - timestamps on CLOCK_BOOTTIME, the base of SystemClock.elapsedRealtimeNanos (SensorEvent.timestamp docs)
//!
//! Data injection: only HAL_BYPASS_REPLAY_DATA_INJECTION (SensorService.h Mode 4), the one mode that needs no HAL: injected events go to every app with the sensor enabled (SensorDevice::injectSensorData queues them, poll() plays them back, SensorDevice.cpp :370, :799).
//! - set at start, as AOSP's `dumpsys sensorservice hal_bypass_replay_data_injection <package>` sets it (changeOperatingMode :2430): global, one allow-listed package matched by `contains` (isAllowListedPackage :2538)
//! - refused while the platform lists a real accelerometer (probed at start, logged + counted): an injecting client would otherwise replace real readings for every app (in bypass mode poll() reads only the injected queue)
//! - the "HAL" here lists no sensor without a device, so the mode lists one accelerometer of its own ([`INJECTED_NAME`]) for injected events to land on; it has no source thread
//! - not AOSP: an event is accepted only from the process owning the focused window ([`FocusedProcess`]), checked per event; the name check alone lets any process that claims the allow-listed name (the op package is the caller's own string) steer every app's accelerometer

use std::collections::BTreeMap;
use std::os::fd::OwnedFd;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use atl_platform_host::{Accelerometer, SampleStream, Sensors};
use rustix::net::{AddressFamily, SendFlags, SocketFlags, SocketType};

use crate::counters;
use crate::focus::FocusedProcess;

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

/// android/sensor.h ASENSOR_TYPE_ACCELEROMETER.
pub const TYPE_ACCELEROMETER: i32 = layout::ASENSOR_TYPE_ACCELEROMETER_ID;
/// SensorService.h Mode HAL_BYPASS_REPLAY_DATA_INJECTION (SensorManager.java HAL_BYPASS_REPLAY_DATA_INJECTION).
pub const MODE_HAL_BYPASS_REPLAY_DATA_INJECTION: i32 = 4;
/// The package the composer's key tilt injects as and atld allow-lists (`--tilt-keys`): not an app package, so no app's own name matches it.
pub const TILT_INJECTOR_PACKAGE: &str = "atl.tilt-keys";
/// The accelerometer the injection mode lists.
pub const INJECTED_NAME: &str = "ATL injected accelerometer";
/// Its range: +-2 g, a phone accelerometer's smallest common range; injected values are +-1 g.
const INJECTED_MAX_RANGE: f32 = 2.0 * 9.806_65;
/// float32 resolution: the values are computed, not quantised.
const INJECTED_RESOLUTION: f32 = 1.0e-6;
/// SENSOR_DELAY_FASTEST's floor on most phones, 200 Hz: the injector's rate is its own business.
const INJECTED_MIN_PERIOD: Duration = Duration::from_millis(5);

/// Where a sensor's events come from.
#[derive(Debug, Clone, PartialEq)]
enum Source {
    /// A platform device (its id), polled.
    Platform(String),
    /// Injected by a client (HAL bypass mode); no thread.
    Injected,
}

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
    source: Source,
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
            source: Source::Platform(a.id.clone()),
        }
    }

    fn injected(handle: i32) -> SensorInfo {
        SensorInfo {
            handle,
            name: INJECTED_NAME.to_owned(),
            sensor_type: layout::ASENSOR_TYPE_ACCELEROMETER_ID,
            string_type: STRING_TYPE_ACCELEROMETER,
            max_range: INJECTED_MAX_RANGE,
            resolution: INJECTED_RESOLUTION,
            power: 0.0,
            min_delay_us: i32::try_from(INJECTED_MIN_PERIOD.as_micros().max(1)).unwrap_or(i32::MAX),
            max_delay_us: 0,
            source: Source::Injected,
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

    /// The events an injecting client writes on its end (SensorEventConnection::handleEvent :846-878, ALOOPER_EVENT_INPUT): each whole record to `on_event`, until the client end closes. Blocks: run on its own thread.
    /// A packet of another size (AOSP: a 4-byte wake-lock ack) is skipped.
    pub fn read_injected(&self, mut on_event: impl FnMut(&[u8; EVENT_SIZE])) {
        use rustix::event::{PollFd, PollFlags};
        let mut buf = [0u8; EVENT_SIZE];
        loop {
            let mut fds = [PollFd::new(&self.send, PollFlags::IN)];
            match rustix::event::poll(&mut fds, None) {
                Ok(_) | Err(rustix::io::Errno::INTR) => {}
                Err(_) => return,
            }
            let revents = fds[0].revents();
            if revents.contains(PollFlags::IN) {
                match rustix::net::recv(&self.send, &mut buf, rustix::net::RecvFlags::DONTWAIT) {
                    Ok((n, _)) if n == EVENT_SIZE => on_event(&buf),
                    // the client closed its end
                    Ok((0, _)) => return,
                    Ok(_) | Err(rustix::io::Errno::AGAIN) => {}
                    Err(_) => return,
                }
            } else if revents.intersects(PollFlags::HUP | PollFlags::ERR | PollFlags::NVAL) {
                return;
            }
        }
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
    /// The allow-listed package while in HAL bypass injection mode (SensorService mAllowListedPackage); None: NORMAL.
    injection: Option<String>,
    /// The focus owner an injected event must come from; None: no focus check (test only, the tilt gate's `focus` break).
    focus: Option<Arc<FocusedProcess>>,
}

impl SensorHub {
    /// The sensors `source` lists now, handles from 1 (SensorService `registerSensor` at start; no hot-plug).
    /// `injection`: start in HAL bypass injection mode with this allow-listed package, unless the source lists a real accelerometer (then NORMAL, logged and counted). `focus`: see [`SensorHub::inject`].
    pub fn new(
        source: Arc<dyn Sensors>,
        injection: Option<&str>,
        focus: Option<Arc<FocusedProcess>>,
    ) -> Arc<SensorHub> {
        let listed = source.accelerometers();
        for a in &listed {
            counters::answered(INTERFACE, &format!("accelerometer tier: {}", a.tier));
        }
        let mut sensors: Vec<SensorInfo> = listed
            .iter()
            .zip(1..)
            .map(|(a, h)| SensorInfo::accelerometer(h, a))
            .collect();
        let injection = match injection {
            Some(_) if !sensors.is_empty() => {
                log(&format!(
                    "injection off: {} real accelerometer(s)",
                    sensors.len()
                ));
                counters::refused(INTERFACE, "data injection: real accelerometer present");
                None
            }
            Some(package) => {
                sensors.push(SensorInfo::injected(1));
                log(&format!(
                    "injection on: hal_bypass_replay_data_injection for {package:?}"
                ));
                counters::answered(INTERFACE, "data injection: hal bypass mode");
                Some(package.to_owned())
            }
            None => None,
        };
        Arc::new(SensorHub {
            sensors,
            source,
            state: Mutex::new(State {
                active: BTreeMap::new(),
            }),
            next_connection: AtomicU64::new(1),
            injection,
            focus,
        })
    }

    /// SensorService::isHalBypassReplayDataInjectionEnabled (:1642).
    pub fn hal_bypass_injection(&self) -> bool {
        self.injection.is_some()
    }

    /// SensorService::isAllowListedPackage (:2538): the allow-listed name is a substring of `package`.
    pub fn is_allow_listed(&self, package: &str) -> bool {
        self.injection
            .as_deref()
            .is_some_and(|allowed| package.contains(allowed))
    }

    /// One record an injecting client wrote (SensorEventConnection::handleEvent -> SensorDevice::injectSensorData, HAL bypass: queued, then fanned out as a HAL event).
    /// Dropped (counted) outside the mode, for a package not allow-listed, from a `pid` (the connection's creator) not owning the focused window, or for an unknown handle (AOSP: getSensorInterfaceFromHandle null).
    pub fn inject(&self, package: &str, pid: i32, record: &[u8; EVENT_SIZE]) {
        if !self.is_allow_listed(package) {
            counters::refused(INTERFACE, "injected event: not allowed");
            return;
        }
        if self.focus.as_ref().is_some_and(|f| !f.is(pid)) {
            counters::refused(INTERFACE, "injected event: caller has no focused window");
            return;
        }
        let word = |off: u64| {
            let off = off as usize;
            i32::from_ne_bytes(record[off..off + 4].try_into().unwrap_or_default())
        };
        let handle = word(layout::ASENSOR_EVENT_SENSOR);
        let Some(s) = self.sensor(handle) else {
            counters::refused(INTERFACE, "injected event: unknown handle");
            return;
        };
        let mut event = *record;
        // handleEvent: `sensor_event.type = si->getSensor().getType()`
        let off = layout::ASENSOR_EVENT_TYPE as usize;
        event[off..off + 4].copy_from_slice(&s.sensor_type.to_ne_bytes());
        let channels: Vec<(u64, Arc<Channel>)> = {
            let st = self.lock();
            let Some(a) = st.active.get(&handle) else {
                return;
            };
            a.subscribers
                .iter()
                .filter_map(|(c, (_, w))| w.upgrade().map(|ch| (*c, ch)))
                .collect()
        };
        for (c, ch) in &channels {
            if ch.dead.load(Ordering::Relaxed) || !ch.send(&event) {
                self.disable(*c, handle);
            }
        }
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
            log(&format!("{} (handle {handle}) started", s.name));
            if let Source::Platform(id) = &s.source {
                let stop = Arc::clone(&a.stop);
                let hub = Arc::clone(self);
                let (s, id) = (s.clone(), id.clone());
                std::thread::spawn(move || hub.poll(s, id, stop));
            }
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
            if self
                .sensor(handle)
                .is_some_and(|s| s.source == Source::Injected)
            {
                log(&format!("{INJECTED_NAME} (handle {handle}) stopped"));
            }
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

    /// The poll loop of one enabled sensor: one `next` per period, each sample to every subscriber. A silent source repeats the last sample with a fresh stamp, so the events keep the asked rate (Android's accelerometer reports continuously).
    /// The stream opens on the first pass and is reopened after a failure; the loop paces itself to the shortest asked period.
    fn poll(&self, s: SensorInfo, id: String, stop: Arc<AtomicBool>) {
        let mut stream: Option<Box<dyn SampleStream>> = None;
        let mut last: Option<[f32; 3]> = None;
        // the open stream's ts-fallbacks already counted
        let mut ts_seen = 0u64;
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
            let wait = Duration::from_nanos(u64::try_from(period).unwrap_or(0));
            let started = Instant::now();
            if stream.is_none() {
                match self.source.open(&id) {
                    Ok(st) => {
                        counters::answered(
                            INTERFACE,
                            &format!("accelerometer tier: {}", st.tier()),
                        );
                        stream = Some(st);
                        ts_seen = 0;
                    }
                    Err(e) => {
                        counters::answered(INTERFACE, &format!("source open failed: {e}"));
                    }
                }
            }
            let outcome = stream.as_mut().map(|st| st.next(wait));
            if let Some(st) = stream.as_ref() {
                for _ in ts_seen..st.ts_fallbacks() {
                    counters::answered(INTERFACE, "accelerometer ts-fallback");
                }
                ts_seen = st.ts_fallbacks();
            }
            let sample = match outcome {
                Some(Ok(Some(sm))) => {
                    last = Some(sm.values);
                    Some((sm.boottime_ns, sm.values))
                }
                Some(Ok(None)) => last.map(|v| (boottime_ns(), v)),
                Some(Err(e)) => {
                    counters::answered(INTERFACE, &format!("source read failed: {e}"));
                    stream = None;
                    None
                }
                None => None,
            };
            if let Some((ts, v)) = sample {
                let event = encode_event(s.handle, s.sensor_type, ts, &v);
                for (c, ch) in &channels {
                    if ch.dead.load(Ordering::Relaxed) || !ch.send(&event) {
                        self.disable(*c, s.handle);
                    }
                }
            }
            std::thread::sleep(wait.saturating_sub(started.elapsed()));
        }
        log(&format!("{} (handle {}) stopped", s.name, s.handle));
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use atl_platform_host::{Sample, SensorError, Tier};

    use super::*;

    /// A test double of the platform source: one accelerometer whose stream answers at once with fixed values.
    struct Fixed;

    struct FixedStream;

    impl SampleStream for FixedStream {
        fn tier(&self) -> Tier {
            Tier::Buffered
        }
        fn ts_fallbacks(&self) -> u64 {
            0
        }
        fn next(&mut self, _: Duration) -> Result<Option<Sample>, SensorError> {
            Ok(Some(Sample {
                boottime_ns: boottime_ns(),
                values: [1.0, -2.0, 9.5],
            }))
        }
    }

    impl Sensors for Fixed {
        fn accelerometers(&self) -> Vec<Accelerometer> {
            vec![Accelerometer {
                id: "dev0".into(),
                name: "fake accel".into(),
                max_range: 19.6,
                resolution: 0.01,
                min_period: Duration::from_millis(5),
                tier: Tier::Buffered,
            }]
        }
        fn open(&self, _: &str) -> Result<Box<dyn SampleStream>, SensorError> {
            Ok(Box::new(FixedStream))
        }
    }

    /// The writer paused: one sample, then nothing; each silent call takes its whole timeout.
    struct Paused {
        sent: bool,
    }

    impl SampleStream for Paused {
        fn tier(&self) -> Tier {
            Tier::Polled(atl_platform_host::Fallback::NoData)
        }
        fn ts_fallbacks(&self) -> u64 {
            0
        }
        fn next(&mut self, timeout: Duration) -> Result<Option<Sample>, SensorError> {
            if !self.sent {
                self.sent = true;
                return Ok(Some(Sample {
                    boottime_ns: boottime_ns(),
                    values: [3.0, 3.0, 3.0],
                }));
            }
            std::thread::sleep(timeout);
            Ok(None)
        }
    }

    struct PausedSource;

    impl Sensors for PausedSource {
        fn accelerometers(&self) -> Vec<Accelerometer> {
            Fixed.accelerometers()
        }
        fn open(&self, _: &str) -> Result<Box<dyn SampleStream>, SensorError> {
            Ok(Box::new(Paused { sent: false }))
        }
    }

    struct Nothing;

    impl Sensors for Nothing {
        fn accelerometers(&self) -> Vec<Accelerometer> {
            Vec::new()
        }
        fn open(&self, id: &str) -> Result<Box<dyn SampleStream>, SensorError> {
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
        let hub = SensorHub::new(Arc::new(Nothing), None, None);
        assert!(hub.sensor_list().is_empty());
        let ch = Arc::new(Channel::open().unwrap());
        assert_eq!(hub.enable(1, &ch, 1, 20_000_000), BAD_VALUE);
    }

    /// A real accelerometer refuses the mode; without one the injected sensor is listed and an allow-listed client's records reach the subscribers with the type set, others' are dropped;
    /// so are the allow-listed name's from a process without the focused window (pid 5 focused, 6 not).
    #[test]
    fn injection_needs_no_real_sensor_an_allow_listed_package_and_focus() {
        let real = SensorHub::new(Arc::new(Fixed), Some("atl.tilt"), None);
        assert!(!real.hal_bypass_injection());
        assert_eq!(real.sensor_list().len(), 1);
        let focus = Arc::new(FocusedProcess::default());
        focus.gained(5);
        let hub = SensorHub::new(Arc::new(Nothing), Some("atl.tilt"), Some(focus));
        assert!(hub.hal_bypass_injection());
        let s = &hub.sensor_list()[0];
        assert_eq!((s.handle, s.name.as_str()), (1, INJECTED_NAME));
        let ch = Arc::new(Channel::open().unwrap());
        let rx = ch.take_receive().unwrap();
        assert_eq!(hub.enable(3, &ch, 1, 20_000_000), OK);
        // type 0 on the wire: the service sets the sensor's
        let record = encode_event(1, 0, 42, &[0.5, 0.0, 9.0]);
        hub.inject("com.example:atl.tilt", 5, &record);
        let e = recv_event(&ch, &rx);
        assert_eq!(&e[8..12], &1i32.to_ne_bytes());
        assert_eq!(&e[16..24], &42i64.to_ne_bytes());
        hub.inject("other", 5, &record);
        hub.inject("com.example:atl.tilt", 6, &record);
        let mut buf = [0u8; EVENT_SIZE];
        assert!(rustix::net::recv(&rx, &mut buf, rustix::net::RecvFlags::DONTWAIT).is_err());
    }

    #[test]
    fn flatten_matches_sensor_cpp_field_widths() {
        let hub = SensorHub::new(Arc::new(Fixed), None, None);
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
        let hub = SensorHub::new(Arc::new(Fixed), None, None);
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

    /// The writer paused (G4 gate): events keep coming at the asked period with the last value, and their stamps move forward.
    #[test]
    fn silent_source_repeats_the_last_sample_at_the_asked_rate() {
        let hub = SensorHub::new(Arc::new(PausedSource), None, None);
        let ch = Arc::new(Channel::open().unwrap());
        let rx = ch.take_receive().unwrap();
        let started = Instant::now();
        assert_eq!(hub.enable(1, &ch, 1, 20_000_000), OK);
        let mut stamps = Vec::new();
        for _ in 0..5 {
            let e = recv_event(&ch, &rx);
            let v: Vec<f32> = (0..3)
                .map(|i| f32::from_ne_bytes(e[24 + 4 * i..28 + 4 * i].try_into().unwrap()))
                .collect();
            assert_eq!(v, [3.0, 3.0, 3.0]);
            stamps.push(i64::from_ne_bytes(e[16..24].try_into().unwrap()));
        }
        assert!(
            stamps.windows(2).all(|w| w[1] > w[0]),
            "stamps move forward: {stamps:?}"
        );
        // five events at 20 ms: the first at once, the next four one period apart (not a burst)
        assert!(
            started.elapsed() >= Duration::from_millis(60),
            "{:?}",
            started.elapsed()
        );
        hub.drop_connection(1);
    }

    #[test]
    fn clamp_raises_to_min_delay() {
        let hub = SensorHub::new(Arc::new(Fixed), None, None);
        let s = &hub.sensor_list()[0];
        assert_eq!(SensorHub::clamp(s, 1), 5_000_000);
        assert_eq!(SensorHub::clamp(s, 20_000_000), 20_000_000);
    }
}
