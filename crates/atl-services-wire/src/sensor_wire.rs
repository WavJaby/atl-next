// SPDX-License-Identifier: MIT
//! `sensorservice` on binder: libsensor's wire is hand-written C++ (ISensorServer.cpp, ISensorEventConnection.cpp), not AIDL, so both interfaces are declared here with libsensor's descriptors and codes; replies carry no status header (BnSensorServer::onTransact writes the values only).
//! The server (atl-services sensor_wire.rs) implements the traits; [`Injector`] is the client side of data injection (SystemSensorManager InjectEventQueue + SensorEventQueue::injectSensorEvent), for an in-ATL injector such as the composer's key tilt.

use std::os::fd::OwnedFd;

use binder::binder_impl::{Binder, BorrowedParcel, IBinderInternal, TransactionCode};
use binder::{Interface, SpIBinder, StatusCode, Strong, declare_binder_interface};

use crate::sensor::{
    EVENT_SIZE, MODE_HAL_BYPASS_REPLAY_DATA_INJECTION, TYPE_ACCELEROMETER, encode_event,
};

/// ISensorServer.cpp:220 IMPLEMENT_META_INTERFACE(SensorServer, "android.gui.SensorServer").
const SERVER_DESCRIPTOR: &str = "android.gui.SensorServer";
/// ISensorEventConnection.cpp:117.
const CONNECTION_DESCRIPTOR: &str = "android.gui.SensorEventConnection";

/// ISensorServer.cpp:38-48, from IBinder::FIRST_CALL_TRANSACTION (1).
pub const GET_SENSOR_LIST: TransactionCode = 1;
pub const CREATE_SENSOR_EVENT_CONNECTION: TransactionCode = 2;
pub const ENABLE_DATA_INJECTION: TransactionCode = 3;
pub const GET_DYNAMIC_SENSOR_LIST: TransactionCode = 4;
pub const GET_RUNTIME_SENSOR_LIST: TransactionCode = 7;
pub const ENABLE_REPLAY_DATA_INJECTION: TransactionCode = 8;
pub const ENABLE_HAL_BYPASS_REPLAY_DATA_INJECTION: TransactionCode = 9;

/// ISensorEventConnection.cpp:34-41.
pub const GET_SENSOR_CHANNEL: TransactionCode = 1;
pub const ENABLE_DISABLE: TransactionCode = 2;
pub const SET_EVENT_RATE: TransactionCode = 3;
pub const DESTROY: TransactionCode = 6;

/// SensorService.h Mode: createSensorEventConnection (:1581-1625) takes NORMAL and the injection modes; DATA_INJECTION only while the service is in it (never here: it needs a HAL), REPLAY / HAL bypass always, the allow list checked per injected event.
pub const MODE_NORMAL: i32 = 0;
pub const MODE_DATA_INJECTION: i32 = 1;
pub const MODE_REPLAY_DATA_INJECTION: i32 = 3;
/// The name libsensor's SensorManager waits for.
pub const SERVICE: &str = "sensorservice";

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

/// The names the reply hook counts under (ISensorServer.cpp / ISensorEventConnection.cpp class names).
const SERVER: &str = "ISensorServer";
const CONNECTION: &str = "ISensorEventConnection";

/// ISensorServer.cpp's name of `code`, for the reply hook.
fn server_method(code: TransactionCode) -> Option<&'static str> {
    [
        (GET_SENSOR_LIST, "getSensorList"),
        (
            CREATE_SENSOR_EVENT_CONNECTION,
            "createSensorEventConnection",
        ),
        (ENABLE_DATA_INJECTION, "isDataInjectionEnabled"),
        (GET_DYNAMIC_SENSOR_LIST, "getDynamicSensorList"),
        (GET_RUNTIME_SENSOR_LIST, "getRuntimeSensorList"),
        (ENABLE_REPLAY_DATA_INJECTION, "isReplayDataInjectionEnabled"),
        (
            ENABLE_HAL_BYPASS_REPLAY_DATA_INJECTION,
            "isHalBypassReplayDataInjectionEnabled",
        ),
    ]
    .into_iter()
    .find_map(|(c, n)| (c == code).then_some(n))
}

/// ISensorEventConnection.cpp's name of `code`, for the reply hook.
fn connection_method(code: TransactionCode) -> Option<&'static str> {
    [
        (GET_SENSOR_CHANNEL, "getSensorChannel"),
        (ENABLE_DISABLE, "enableDisable"),
        (SET_EVENT_RATE, "setEventRate"),
        (DESTROY, "destroy"),
    ]
    .into_iter()
    .find_map(|(c, n)| (c == code).then_some(n))
}

/// Every `Err` leaving the service's `transact` passes the reply hook once, as a generated `on_transact`'s does.
fn server_on_transact(
    service: &dyn IHandWritten,
    code: TransactionCode,
    data: &BorrowedParcel<'_>,
    reply: &mut BorrowedParcel<'_>,
) -> Result<(), StatusCode> {
    service.transact(code, data, reply).inspect_err(|e| {
        atl_diag::binder_reply(
            SERVER,
            server_method(code),
            code,
            atl_diag::EX_TRANSACTION_FAILED,
            *e as i32,
        );
    })
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

/// As [`server_on_transact`].
fn connection_on_transact(
    service: &dyn IConnection,
    code: TransactionCode,
    data: &BorrowedParcel<'_>,
    reply: &mut BorrowedParcel<'_>,
) -> Result<(), StatusCode> {
    service.transact(code, data, reply).inspect_err(|e| {
        atl_diag::binder_reply(
            CONNECTION,
            connection_method(code),
            code,
            atl_diag::EX_TRANSACTION_FAILED,
            *e as i32,
        );
    })
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
pub fn read_string8(data: &BorrowedParcel<'_>) -> Result<String, StatusCode> {
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
pub fn write_string8(data: &mut BorrowedParcel<'_>, s: &str) -> Result<(), StatusCode> {
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
        if sensor_type == TYPE_ACCELEROMETER {
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
