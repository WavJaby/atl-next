// SPDX-License-Identifier: MIT
//! libsensor's event record and the constants both ends of `sensorservice` agree on: the service (atl-services sensor.rs) and the injector ([`crate::sensor_wire::Injector`]). Plain Rust.

/// `ASensorEvent` as clang lays it out from the pinned android/sensor.h (build.rs): field offsets for the encoder here and the service's reading of injected records.
#[allow(clippy::all)]
pub mod layout {
    include!(concat!(env!("OUT_DIR"), "/sensor_event.rs"));
}

/// One event record.
pub const EVENT_SIZE: usize = layout::ASENSOR_EVENT_SIZE as usize;
/// android/sensor.h ASENSOR_TYPE_ACCELEROMETER.
pub const TYPE_ACCELEROMETER: i32 = layout::ASENSOR_TYPE_ACCELEROMETER_ID;
/// SensorService.h Mode HAL_BYPASS_REPLAY_DATA_INJECTION (SensorManager.java HAL_BYPASS_REPLAY_DATA_INJECTION).
pub const MODE_HAL_BYPASS_REPLAY_DATA_INJECTION: i32 = 4;
/// The package the composer's key tilt injects as and atld allow-lists (`--tilt-keys`): not an app package, so no app's own name matches it.
pub const TILT_INJECTOR_PACKAGE: &str = "atl.tilt-keys";

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
