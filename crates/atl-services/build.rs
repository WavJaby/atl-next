// SPDX-License-Identifier: MIT
//! Always: the `ASensorEvent` layout (`OUT_DIR/sensor_event.rs`) the sensor service writes into each event channel, as clang lays it out from the pinned NDK header in bionic's view of the host arch (atl-bridgegen probe constants, as atl-bionic's timespec), so a header change fails the build.
//! With feature `converter` (`xtask native` turns it on): links the converter's libbinder_rs and libatl_parcelables (Cargo.toml `[package.metadata.atl]`).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use atl_bridgegen::{Source, buildscript, converter};

/// The NDK headers: `android/sensor.h` declares the event record libsensor's SensorEventQueue reads (the HAL's sensors_event_t is tied to it: LINT.ThenChange there).
const INCLUDE: &str = "frameworks/native/include";
const HEADER: &str = "aosp/frameworks/native/include/android/sensor.h";

/// The layout facts sensor.rs encodes an event with.
const PROBE: &str = "\
#include <stddef.h>
#include <android/sensor.h>
#define ASENSOR_EVENT_SIZE sizeof(ASensorEvent)
#define ASENSOR_EVENT_VERSION offsetof(ASensorEvent, version)
#define ASENSOR_EVENT_SENSOR offsetof(ASensorEvent, sensor)
#define ASENSOR_EVENT_TYPE offsetof(ASensorEvent, type)
#define ASENSOR_EVENT_TIMESTAMP offsetof(ASensorEvent, timestamp)
#define ASENSOR_EVENT_DATA offsetof(ASensorEvent, data)
#define ASENSOR_EVENT_DATA_LEN (sizeof(((ASensorEvent *)0)->data) / sizeof(float))
#define ASENSOR_EVENT_FLAGS offsetof(ASensorEvent, flags)
#define ASENSOR_TYPE_ACCELEROMETER_ID ASENSOR_TYPE_ACCELEROMETER
";

fn main() -> Result<()> {
    // libbinder_rs's declare_binder_interface! (token.rs, settings.rs) tests these cfgs in the expanding crate (libs/binder/rust/src/binder.rs); unset here, as for a host build
    println!("cargo::rustc-check-cfg=cfg(android_vndk, android_ndk)");
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR")?);
    sensor_event(&manifest)?;
    if std::env::var_os("CARGO_FEATURE_CONVERTER").is_none() {
        return Ok(());
    }
    for l in converter::link_package(&manifest)? {
        println!("{l}");
    }
    Ok(())
}

fn sensor_event(manifest: &Path) -> Result<()> {
    let checkout = buildscript::checkout(manifest)?;
    let up = buildscript::Upstreams::of(&checkout)?;
    let header = up.file(HEADER)?;
    let arches = checkout.join("arches.toml");
    let host = buildscript::host_arch(
        &std::fs::read_to_string(&arches)?,
        &std::env::var("TARGET")?,
    )?;
    let out = PathBuf::from(std::env::var("OUT_DIR")?);
    let unit = out.join("sensor_event_layout.c");
    std::fs::write(&unit, PROBE)?;
    let mut target = buildscript::bionic_libc_target(&up, &host, None);
    target
        .args
        .push(format!("-isystem{}", up.aosp(INCLUDE).display()));
    let src = Source::c(&unit, &["ASENSOR_EVENT_*", "ASENSOR_TYPE_ACCELEROMETER_ID"]);
    let generate = || -> Result<String> {
        let mut iface = atl_bridgegen::interface(
            "sensor_event",
            "frameworks/native/include/android/sensor.h ASensorEvent",
            &src,
            std::slice::from_ref(&target),
        )?;
        // only the probe's constants: the header's records are not what the encoder reads
        for view in iface.arches.values_mut() {
            view.records.clear();
        }
        atl_bridgegen::rust::layout_module(&iface, &host.arch)
    };
    let first = generate()?;
    ensure!(
        first == generate()?,
        "atl-bridgegen output differs between two runs on the same input"
    );
    std::fs::write(out.join("sensor_event.rs"), first).context("writing sensor_event.rs")?;
    println!("cargo::rerun-if-changed={}", header.display());
    println!("cargo::rerun-if-changed={}", arches.display());
    Ok(())
}
