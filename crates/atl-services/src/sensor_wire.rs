// SPDX-License-Identifier: MIT
//! `sensorservice` on binder: libsensor's wire is hand-written C++ (ISensorServer.cpp, ISensorEventConnection.cpp), not AIDL, so both interfaces are declared here with libsensor's descriptors and codes; replies carry no status header (BnSensorServer::onTransact writes the values only).
//! The logic is sensor.rs. Codes not served (direct channels, operation parameters, data injection modes other than the queries) are counted and answered UNKNOWN_TRANSACTION.

use std::sync::Arc;

use binder::binder_impl::{Binder, BorrowedParcel, TransactionCode};
use binder::{BinderFeatures, Interface, SpIBinder, StatusCode, declare_binder_interface};

use crate::counters;
use crate::sensor::{Channel, SensorHub};

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

/// SensorService.h Mode NORMAL: the only mode createSensorEventConnection accepts while the service is not in an injection mode (:1584-1603).
const MODE_NORMAL: i32 = 0;

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

/// `Parcel::writeString8`'s form read back: int32 length, then length + 1 bytes (the NUL) padded to 4. Only skipped here: the package name is not used.
fn skip_string8(data: &BorrowedParcel<'_>) -> Result<(), StatusCode> {
    let len: i32 = data.read()?;
    if len < 0 {
        return Ok(());
    }
    let words = (len as usize + 1).div_ceil(4);
    for _ in 0..words {
        let _: i32 = data.read()?;
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
                skip_string8(data)?;
                let mode: i32 = data.read()?;
                // SensorService::createSensorEventConnection (:1581): a mode other than NORMAL needs the service in that injection mode, which it never is -> null
                if mode != MODE_NORMAL {
                    counters::count(SERVER, &format!("createSensorEventConnection mode {mode}"));
                    return reply.write(&None::<SpIBinder>);
                }
                let channel = Channel::open().map_err(|_| StatusCode::NO_MEMORY)?;
                let connection = EventConnection {
                    hub: Arc::clone(&self.hub),
                    id: self.hub.connection_id(),
                    channel: Arc::new(channel),
                };
                let b = BnSensorEventConnection::new_binder(connection, BinderFeatures::default());
                reply.write(&b.as_binder())
            }
            // SensorService::isDataInjectionEnabled (:1632) and its replay twins: the operating mode is always NORMAL
            ENABLE_DATA_INJECTION
            | ENABLE_REPLAY_DATA_INJECTION
            | ENABLE_HAL_BYPASS_REPLAY_DATA_INJECTION => reply.write(&0i32),
            _ => {
                counters::count(SERVER, &format!("code {code}"));
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
                counters::count(CONNECTION, &format!("code {code}"));
                Err(StatusCode::UNKNOWN_TRANSACTION)
            }
        }
    }
}

/// `sensorservice` over `hub`.
pub fn new_binder(hub: Arc<SensorHub>) -> SpIBinder {
    BnSensorServer::new_binder(SensorServer { hub }, BinderFeatures::default()).as_binder()
}
