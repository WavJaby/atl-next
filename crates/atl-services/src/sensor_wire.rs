// SPDX-License-Identifier: MIT
//! `sensorservice`'s server on binder: the interfaces, codes and modes are libsensor's hand-written wire (atl-services-wire `sensor_wire`); this module serves them over sensor.rs's logic.
//! Codes not served (direct channels, operation parameters) are counted and answered UNKNOWN_TRANSACTION.

use std::sync::Arc;

use binder::binder_impl::{BorrowedParcel, TransactionCode};
use binder::{BinderFeatures, Interface, SpIBinder, StatusCode};

use crate::sensor::{Channel, SensorHub};
use atl_services_types::counters;
use atl_services_wire::sensor::MODE_HAL_BYPASS_REPLAY_DATA_INJECTION;
use atl_services_wire::sensor_wire::{
    BnSensorEventConnection, BnSensorServer, CREATE_SENSOR_EVENT_CONNECTION, DESTROY,
    ENABLE_DATA_INJECTION, ENABLE_DISABLE, ENABLE_HAL_BYPASS_REPLAY_DATA_INJECTION,
    ENABLE_REPLAY_DATA_INJECTION, GET_DYNAMIC_SENSOR_LIST, GET_RUNTIME_SENSOR_LIST,
    GET_SENSOR_CHANNEL, GET_SENSOR_LIST, IConnection, IHandWritten, MODE_DATA_INJECTION,
    MODE_NORMAL, MODE_REPLAY_DATA_INJECTION, SET_EVENT_RATE, read_string8,
};

/// The interface names counters use.
const SERVER: &str = "ISensorServer";
const CONNECTION: &str = "ISensorEventConnection";

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
                    // updateLooperRegistration(mLooper): the service reads what the client writes on its end; the creator's pid is the one its events are checked against (SensorHub::inject)
                    let pid = binder::ThreadState::get_calling_pid();
                    let (hub, ch) = (Arc::clone(&self.hub), Arc::clone(&channel));
                    std::thread::Builder::new()
                        .name("sensor-inject".to_owned())
                        .spawn(move || ch.read_injected(|r| hub.inject(&package, pid, r)))
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
