// SPDX-License-Identifier: MIT
//! `sensorservice` on binder: libsensor's wire is hand-written C++ (ISensorServer.cpp, ISensorEventConnection.cpp), not AIDL, so both interfaces are declared here with libsensor's descriptors and codes; replies carry no status header (BnSensorServer::onTransact writes the values only).
//! The logic is sensor.rs. Codes not served (direct channels, operation parameters) are counted and answered UNKNOWN_TRANSACTION.
//! [`Injector`] is the client side of data injection (SystemSensorManager InjectEventQueue + SensorEventQueue::injectSensorEvent), for an in-ATL injector such as the composer's key tilt.

use std::os::fd::OwnedFd;
use std::sync::Arc;

use binder::binder_impl::{Binder, BorrowedParcel, IBinderInternal, TransactionCode};
use binder::{BinderFeatures, Interface, SpIBinder, StatusCode, Strong, declare_binder_interface};

use crate::counters;
use crate::sensor::{
    Channel, EVENT_SIZE, MODE_HAL_BYPASS_REPLAY_DATA_INJECTION, SensorHub, encode_event,
};

/// ISensorServer.cpp:220 IMPLEMENT_META_INTERFACE(SensorServer, "android.gui.SensorServer").
const SERVER_DESCRIPTOR: &str = "android.gui.SensorServer";
/// ISensorEventConnection.cpp:117.
const CONNECTION_DESCRIPTOR: &str = "android.gui.SensorEventConnection";
const SERVER: &str = "ISensorServer";
const CONNECTION: &str = "ISensorEventConnection";

/// ISensorServer.cpp:38-48, from IBinder::FIRST_CALL_TRANSACTION (1).
const GET_SENSOR_LIST: TransactionCode = 1;
const CREATE_SENSOR_EVENT_CONNECTION: TransactionCode = 2;
const ENABLE_DATA_INJECTION: TransactionCode = 3;
const GET_DYNAMIC_SENSOR_LIST: TransactionCode = 4;
const GET_RUNTIME_SENSOR_LIST: TransactionCode = 7;
const ENABLE_REPLAY_DATA_INJECTION: TransactionCode = 8;
const ENABLE_HAL_BYPASS_REPLAY_DATA_INJECTION: TransactionCode = 9;

/// ISensorEventConnection.cpp:34-41.
const GET_SENSOR_CHANNEL: TransactionCode = 1;
const ENABLE_DISABLE: TransactionCode = 2;
const SET_EVENT_RATE: TransactionCode = 3;
const DESTROY: TransactionCode = 6;

/// SensorService.h Mode: createSensorEventConnection (:1581-1625) takes NORMAL and the injection modes; DATA_INJECTION only while the service is in it (never here: it needs a HAL), REPLAY / HAL bypass always, the allow list checked per injected event.
const MODE_NORMAL: i32 = 0;
const MODE_DATA_INJECTION: i32 = 1;
const MODE_REPLAY_DATA_INJECTION: i32 = 3;
/// The name libsensor's SensorManager waits for.
const SERVICE: &str = "sensorservice";

/// One hand-written interface: the code and the parcels after the interface token (the NDK checks it before on_transact).
pub trait IHandWritten: Interface {
    fn transact(
        &self,
        code: TransactionCode,
        data: &BorrowedParcel<'_>,
        reply: &mut BorrowedParcel<'_>,
    ) -> Result<(), StatusCode>;
}

declare_binder_interface! {
    IHandWritten[SERVER_DESCRIPTOR] {
        native: BnSensorServer(server_on_transact),
        proxy: BpSensorServer,
    }
}

fn server_on_transact(
    service: &dyn IHandWritten,
    code: TransactionCode,
    data: &BorrowedParcel<'_>,
    reply: &mut BorrowedParcel<'_>,
) -> Result<(), StatusCode> {
    service.transact(code, data, reply)
}

impl IHandWritten for BpSensorServer {
    fn transact(
        &self,
        _: TransactionCode,
        _: &BorrowedParcel<'_>,
        _: &mut BorrowedParcel<'_>,
    ) -> Result<(), StatusCode> {
        Err(StatusCode::INVALID_OPERATION)
    }
}

impl IHandWritten for Binder<BnSensorServer> {
    fn transact(
        &self,
        code: TransactionCode,
        data: &BorrowedParcel<'_>,
        reply: &mut BorrowedParcel<'_>,
    ) -> Result<(), StatusCode> {
        self.0.transact(code, data, reply)
    }
}

/// The connection object: same trait shape, its own descriptor.
pub trait IConnection: Interface {
    fn transact(
        &self,
        code: TransactionCode,
        data: &BorrowedParcel<'_>,
        reply: &mut BorrowedParcel<'_>,
    ) -> Result<(), StatusCode>;
}

declare_binder_interface! {
    IConnection[CONNECTION_DESCRIPTOR] {
        native: BnSensorEventConnection(connection_on_transact),
        proxy: BpSensorEventConnection,
    }
}

fn connection_on_transact(
    service: &dyn IConnection,
    code: TransactionCode,
    data: &BorrowedParcel<'_>,
    reply: &mut BorrowedParcel<'_>,
) -> Result<(), StatusCode> {
    service.transact(code, data, reply)
}

impl IConnection for BpSensorEventConnection {
    fn transact(
        &self,
        _: TransactionCode,
        _: &BorrowedParcel<'_>,
        _: &mut BorrowedParcel<'_>,
    ) -> Result<(), StatusCode> {
        Err(StatusCode::INVALID_OPERATION)
    }
}

impl IConnection for Binder<BnSensorEventConnection> {
    fn transact(
        &self,
        code: TransactionCode,
        data: &BorrowedParcel<'_>,
        reply: &mut BorrowedParcel<'_>,
    ) -> Result<(), StatusCode> {
        self.0.transact(code, data, reply)
    }
}

/// `Parcel::writeString8`'s form read back: int32 length, then length + 1 bytes (the NUL) padded to 4. A negative length (null) reads as "".
fn read_string8(data: &BorrowedParcel<'_>) -> Result<String, StatusCode> {
    let len: i32 = data.read()?;
    let Ok(len) = usize::try_from(len) else {
        return Ok(String::new());
    };
    let mut bytes = Vec::with_capacity(len + 4);
    for _ in 0..(len + 1).div_ceil(4) {
        let w: i32 = data.read()?;
        bytes.extend(w.to_ne_bytes());
    }
    bytes.truncate(len);
    String::from_utf8(bytes).map_err(|_| StatusCode::BAD_VALUE)
}

/// `Parcel::writeString8`: int32 length, the bytes, a NUL, padded to 4, written as native-order int32 words.
fn write_string8(data: &mut BorrowedParcel<'_>, s: &str) -> Result<(), StatusCode> {
    data.write(&i32::try_from(s.len()).map_err(|_| StatusCode::BAD_VALUE)?)?;
    let mut bytes = s.as_bytes().to_vec();
    bytes.push(0);
    for chunk in bytes.chunks(4) {
        let mut w = [0u8; 4];
        w[..chunk.len()].copy_from_slice(chunk);
        data.write(&i32::from_ne_bytes(w))?;
    }
    Ok(())
}

/// `Parcel::write(const LightFlattenable<T>&)` of a variable-size one (`Sensor`; read side Parcel.h `read(LightFlattenable<T>&)` :1540-1558): int32 size, then the payload padded to 4 (readInplace). Written as int32 words of the native byte order, which is the parcel's.
fn write_light_flattenable(reply: &mut BorrowedParcel<'_>, bytes: &[u8]) -> Result<(), StatusCode> {
    reply.write(&(bytes.len() as i32))?;
    for chunk in bytes.chunks(4) {
        let mut w = [0u8; 4];
        w[..chunk.len()].copy_from_slice(chunk);
        reply.write(&i32::from_ne_bytes(w))?;
    }
    Ok(())
}

struct SensorServer {
    hub: Arc<SensorHub>,
}

impl Interface for SensorServer {}

impl SensorServer {
    /// BnSensorServer GET_*_SENSOR_LIST: uint32 count, then each `Sensor` (a LightFlattenable).
    fn write_list(
        &self,
        reply: &mut BorrowedParcel<'_>,
        list: &[crate::sensor::SensorInfo],
    ) -> Result<(), StatusCode> {
        reply.write(&(list.len() as i32))?;
        for s in list {
            write_light_flattenable(reply, &s.flatten())?;
        }
        Ok(())
    }
}

impl IHandWritten for SensorServer {
    fn transact(
        &self,
        code: TransactionCode,
        data: &BorrowedParcel<'_>,
        reply: &mut BorrowedParcel<'_>,
    ) -> Result<(), StatusCode> {
        match code {
            // SensorService::getSensorList (:1522); the op package name only selects rate caps, none here
            GET_SENSOR_LIST => {
                let _op_package: Option<String> = data.read()?;
                self.write_list(reply, self.hub.sensor_list())
            }
            // SensorService::getDynamicSensorList (:1554) / getRuntimeSensorList (:1567): no dynamic sensors, no virtual devices
            GET_DYNAMIC_SENSOR_LIST | GET_RUNTIME_SENSOR_LIST => self.write_list(reply, &[]),
            CREATE_SENSOR_EVENT_CONNECTION => {
                let package = read_string8(data)?;
                let mode: i32 = data.read()?;
                // SensorService::createSensorEventConnection (:1581): unknown modes and DATA_INJECTION outside that mode -> null
                let injecting = match mode {
                    MODE_NORMAL => false,
                    MODE_REPLAY_DATA_INJECTION | MODE_HAL_BYPASS_REPLAY_DATA_INJECTION => true,
                    _ => {
                        let what = if mode == MODE_DATA_INJECTION {
                            "createSensorEventConnection DATA_INJECTION: not in that mode"
                        } else {
                            "createSensorEventConnection: invalid mode"
                        };
                        counters::refused(SERVER, what);
                        return reply.write(&None::<SpIBinder>);
                    }
                };
                let channel = Arc::new(Channel::open().map_err(|_| StatusCode::NO_MEMORY)?);
                if injecting {
                    // updateLooperRegistration(mLooper): the service reads what the client writes on its end
                    let (hub, ch) = (Arc::clone(&self.hub), Arc::clone(&channel));
                    std::thread::Builder::new()
                        .name("sensor-inject".to_owned())
                        .spawn(move || ch.read_injected(|r| hub.inject(&package, r)))
                        .map_err(|_| StatusCode::NO_MEMORY)?;
                }
                let connection = EventConnection {
                    hub: Arc::clone(&self.hub),
                    id: self.hub.connection_id(),
                    channel,
                };
                let b = BnSensorEventConnection::new_binder(connection, BinderFeatures::default());
                reply.write(&b.as_binder())
            }
            // SensorService::isDataInjectionEnabled (:1632) / isReplayDataInjectionEnabled (:1637): those modes need a HAL, never set
            ENABLE_DATA_INJECTION | ENABLE_REPLAY_DATA_INJECTION => reply.write(&0i32),
            // isHalBypassReplayDataInjectionEnabled (:1642)
            ENABLE_HAL_BYPASS_REPLAY_DATA_INJECTION => {
                reply.write(&i32::from(self.hub.hal_bypass_injection()))
            }
            _ => {
                counters::count_unimplemented(SERVER, &format!("code {code}"));
                Err(StatusCode::UNKNOWN_TRANSACTION)
            }
        }
    }
}

/// SensorService::SensorEventConnection: one client queue's channel and the sensors it enabled.
struct EventConnection {
    hub: Arc<SensorHub>,
    id: u64,
    channel: Arc<Channel>,
}

impl Interface for EventConnection {}

impl Drop for EventConnection {
    /// The client's last reference is gone (BpSensorEventConnection::onLastStrongRef also sends DESTROY, which the NDK refuses before on_transact: it carries no interface token).
    fn drop(&mut self) {
        self.hub.drop_connection(self.id);
    }
}

impl IConnection for EventConnection {
    fn transact(
        &self,
        code: TransactionCode,
        data: &BorrowedParcel<'_>,
        reply: &mut BorrowedParcel<'_>,
    ) -> Result<(), StatusCode> {
        match code {
            // SensorEventConnection::getSensorChannel (:677) + BitTube::writeToParcel: the receive end once; later asks get -EINVAL and no fd (BitTube(Parcel) then logs its dup failure)
            GET_SENSOR_CHANNEL => match self.channel.take_receive() {
                Some(fd) => reply
                    .write_dup_file_descriptor(&fd)
                    .map_err(|_| StatusCode::BAD_VALUE),
                None => Ok(()),
            },
            // SensorEventConnection::enableDisable (:682) -> SensorService::enable / disable
            ENABLE_DISABLE => {
                let handle: i32 = data.read()?;
                let enabled: i32 = data.read()?;
                let period: i64 = data.read()?;
                let _max_batch_latency: i64 = data.read()?;
                let _reserved_flags: i32 = data.read()?;
                let status = if enabled != 0 {
                    self.hub.enable(self.id, &self.channel, handle, period)
                } else {
                    self.hub.disable(self.id, handle)
                };
                reply.write(&status)
            }
            // SensorEventConnection::setEventRate (:730)
            SET_EVENT_RATE => {
                let handle: i32 = data.read()?;
                let period: i64 = data.read()?;
                reply.write(&self.hub.set_rate(self.id, handle, period))
            }
            DESTROY => {
                self.hub.drop_connection(self.id);
                Ok(())
            }
            _ => {
                // FLUSH_SENSOR (flush-complete meta events), CONFIGURE_CHANNEL (direct channels only: INVALID_OPERATION in AOSP too)
                counters::count_unimplemented(CONNECTION, &format!("code {code}"));
                Err(StatusCode::UNKNOWN_TRANSACTION)
            }
        }
    }
}

/// `sensorservice` over `hub`.
pub fn new_binder(hub: Arc<SensorHub>) -> SpIBinder {
    BnSensorServer::new_binder(SensorServer { hub }, BinderFeatures::default()).as_binder()
}

/// A data injection client of the accelerometer (SystemSensorManager.initDataInjectionImpl + injectSensorDataImpl, HAL_BYPASS_REPLAY_DATA_INJECTION).
pub struct Injector {
    /// The connection: dropping it ends the service's side.
    _connection: SpIBinder,
    /// The client end of the connection's BitTube.
    tube: OwnedFd,
    handle: i32,
    sensor_type: i32,
}

/// SensorManager.injectSensorData's failure forms, as the injector reports them.
#[derive(Debug)]
pub struct InjectError(pub String);

impl std::fmt::Display for InjectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

fn err(what: &str) -> impl Fn(StatusCode) -> InjectError + '_ {
    move |s| InjectError(format!("{what}: {s:?}"))
}

/// One transaction on `binder` (the token prepare_transact writes, then `fill`).
fn call(
    binder: &SpIBinder,
    code: TransactionCode,
    what: &str,
    fill: impl FnOnce(&mut BorrowedParcel<'_>) -> Result<(), StatusCode>,
) -> Result<binder::binder_impl::Parcel, InjectError> {
    let mut data = binder.prepare_transact().map_err(err(what))?;
    fill(&mut data.borrowed()).map_err(err(what))?;
    binder.submit_transact(code, data, 0).map_err(err(what))
}

/// The first accelerometer of a GET_SENSOR_LIST reply: (handle, type). Each `Sensor` = int32 size + payload words; the payload starts name, vendor (string: uint32 length + bytes padded to 4), version, handle, type (Sensor::flatten :548).
fn first_accelerometer(reply: &BorrowedParcel<'_>) -> Result<Option<(i32, i32)>, StatusCode> {
    let count: i32 = reply.read()?;
    for _ in 0..count.max(0) {
        let size: i32 = reply.read()?;
        let size = usize::try_from(size).map_err(|_| StatusCode::BAD_VALUE)?;
        let mut b = Vec::with_capacity(size + 4);
        for _ in 0..size.div_ceil(4) {
            let w: i32 = reply.read()?;
            b.extend(w.to_ne_bytes());
        }
        let word = |off: usize| -> Option<i32> {
            Some(i32::from_ne_bytes(b.get(off..off + 4)?.try_into().ok()?))
        };
        let skip_string = |off: usize| -> Option<usize> {
            let len = usize::try_from(word(off)?).ok()?;
            Some(off + 4 + len.next_multiple_of(4))
        };
        let Some(after_strings) = skip_string(0).and_then(skip_string) else {
            return Err(StatusCode::BAD_VALUE);
        };
        let (Some(handle), Some(sensor_type)) = (word(after_strings + 4), word(after_strings + 8))
        else {
            return Err(StatusCode::BAD_VALUE);
        };
        if sensor_type == crate::sensor::TYPE_ACCELEROMETER {
            return Ok(Some((handle, sensor_type)));
        }
    }
    Ok(None)
}

impl Injector {
    /// Connects as `package`. None when the service is not in HAL bypass injection mode (initDataInjectionImpl: "The correct Data Injection mode has not been enabled") or lists no accelerometer.
    pub fn connect(package: &str) -> Result<Option<Injector>, InjectError> {
        let server: Strong<dyn IHandWritten> = binder::check_service(SERVICE)
            .ok_or_else(|| InjectError(format!("{SERVICE}: not registered")))?
            .into_interface()
            .map_err(err(SERVICE))?;
        let server = server.as_binder();
        let on: i32 = call(
            &server,
            ENABLE_HAL_BYPASS_REPLAY_DATA_INJECTION,
            "isHalBypassReplayDataInjectionEnabled",
            |_| Ok(()),
        )?
        .borrowed_ref()
        .read()
        .map_err(err("isHalBypassReplayDataInjectionEnabled"))?;
        if on == 0 {
            return Ok(None);
        }
        let list = call(&server, GET_SENSOR_LIST, "getSensorList", |d| {
            d.write(package)
        })?;
        let Some((handle, sensor_type)) =
            first_accelerometer(list.borrowed_ref()).map_err(err("getSensorList"))?
        else {
            return Ok(None);
        };
        let reply = call(
            &server,
            CREATE_SENSOR_EVENT_CONNECTION,
            "createSensorEventConnection",
            |d| {
                write_string8(d, package)?;
                d.write(&MODE_HAL_BYPASS_REPLAY_DATA_INJECTION)?;
                d.write(package)?;
                d.write("")
            },
        )?;
        let connection: Option<SpIBinder> = reply
            .borrowed_ref()
            .read()
            .map_err(err("createSensorEventConnection"))?;
        let connection = connection
            .ok_or_else(|| InjectError("createSensorEventConnection: null".to_owned()))?;
        let connection_if: Strong<dyn IConnection> = connection
            .clone()
            .into_interface()
            .map_err(err("ISensorEventConnection"))?;
        let connection = connection_if.as_binder();
        let channel = call(&connection, GET_SENSOR_CHANNEL, "getSensorChannel", |_| {
            Ok(())
        })?;
        let tube = channel
            .borrowed_ref()
            .read_dup_file_descriptor()
            .map_err(err("getSensorChannel"))?;
        Ok(Some(Injector {
            _connection: connection,
            tube,
            handle,
            sensor_type,
        }))
    }

    /// SensorEventQueue::injectSensorEvent: one `ASensorEvent` on the tube, never blocking (a full tube drops it, Err).
    pub fn inject(&self, timestamp_ns: i64, values: &[f32; 3]) -> Result<(), InjectError> {
        let event: [u8; EVENT_SIZE] =
            encode_event(self.handle, self.sensor_type, timestamp_ns, values);
        rustix::net::send(
            &self.tube,
            &event,
            rustix::net::SendFlags::DONTWAIT | rustix::net::SendFlags::NOSIGNAL,
        )
        .map(|_| ())
        .map_err(|e| InjectError(format!("injectSensorEvent: {e}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn string8_round_trips_as_the_c_form() {
        let mut p = binder::binder_impl::Parcel::new();
        write_string8(&mut p.borrowed(), "atl.tilt").unwrap();
        // 4 length + 8 bytes + NUL -> 12 padded = 16
        assert_eq!(p.get_data_size(), 16);
        p.set_data_position_checked(0).unwrap();
        assert_eq!(read_string8(&p.borrowed_ref()).unwrap(), "atl.tilt");
    }
}
