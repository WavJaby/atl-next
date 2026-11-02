// SPDX-License-Identifier: MIT
//! Proof that a cargo-built binary links the converter's AOSP binder: `ServiceDebugInfo` (a structured parcelable of IServiceManager's AIDL, stub from atl-aidl) round-trips through AOSP's C++ Parcel (libbinder_rs -> libbinder_ndk AParcel -> libbinder Parcel).
//! No binder driver: a Parcel needs none (`ProcessState` would open /dev/binder and abort). Exit 0 = equal value back and the byte count Parcel's encoding gives; otherwise the error.
//! `--want-bytes <n>` replaces the expected byte count, so `xtask test parcel` can show the size check fires.
//! `--enforce`: the aidl patch's fail-closed permission hook, on ISerialManager (`@EnforcePermission("SERIAL_PORT")` getSerialPorts): a getSerialPorts transaction through a local
//! binder's on_transact must return the hook's SECURITY Status without running the method when it denies, and the method's result when it allows; the hook must see exactly
//! `("getSerialPorts", Single("SERIAL_PORT"))`. Writing the interface token reaches libbinder's IPCThreadState, so this mode needs a driver: `xtask test parcel` runs it under binderd.

// the converter's rlib, on the crate search path build.rs sets (no cargo dependency names it)
extern crate binder;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result, bail, ensure};
use atl_aidl::aidl::android::hardware::ISerialManager::{
    BnSerialManager, EnforcePermission, ISerialManager, transactions,
};
use atl_aidl::aidl::android::os::ServiceDebugInfo::ServiceDebugInfo;
use binder::binder_impl::{IBinderInternal, Parcel};
use binder::{BinderFeatures, ExceptionCode, Interface, Status};

/// The service name written: 14 UTF-16 code units.
const NAME: &str = "servicemanager";
/// `NAME` in Parcel's encoding (Parcel.cpp writeString16): int32 length 14, then (14 + 1 terminator) * 2 = 30 bytes padded to 4 -> 32; 36 in all.
const NAME_BYTES: i32 = 4 + 32;
/// Bytes of the parcel: the non-null parcelable marker (libbinder_rs `SerializeOption`, int32), the size header of the structured parcelable (`sized_write`, int32), the name, `debugPid` (int32).
const PARCEL_BYTES: i32 = 4 + 4 + NAME_BYTES + 4;

enum Mode {
    /// the round trip, expecting this many bytes
    RoundTrip(i32),
    Enforce,
}

fn mode() -> Result<Mode> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.as_slice() {
        [] => Ok(Mode::RoundTrip(PARCEL_BYTES)),
        [flag, n] if flag == "--want-bytes" => Ok(Mode::RoundTrip(
            n.parse().context("--want-bytes: an integer")?,
        )),
        [flag] if flag == "--enforce" => Ok(Mode::Enforce),
        _ => bail!("usage: atl-aidl-probe [--want-bytes <n> | --enforce]"),
    }
}

fn main() -> Result<()> {
    match mode()? {
        Mode::RoundTrip(want) => round_trip(want),
        Mode::Enforce => enforce(),
    }
}

/// The port the allowing service answers.
const PORT: &str = "/dev/ttyATL0";

/// An ISerialManager service whose hook allows only `allow`, and only for exactly what aidl parsed; it records whether a method body ran.
struct Serial {
    allow: bool,
    ran: Arc<AtomicBool>,
}

impl Interface for Serial {}

impl ISerialManager for Serial {
    fn r#enforce_permission(
        &self,
        method: &str,
        permission: EnforcePermission<'_>,
    ) -> binder::Result<()> {
        if self.allow
            && method == "getSerialPorts"
            && permission == EnforcePermission::Single("SERIAL_PORT")
        {
            Ok(())
        } else {
            Err(Status::new_exception(ExceptionCode::SECURITY, None))
        }
    }
    // openSerialPort keeps its default body (aidl --rust_unimplemented_hook), which answers through here
    fn r#unimplemented(&self, _method: &'static str) -> Status {
        binder::StatusCode::UNKNOWN_TRANSACTION.into()
    }
    fn r#getSerialPorts(&self) -> binder::Result<Option<Vec<Option<String>>>> {
        self.ran.store(true, Ordering::SeqCst);
        Ok(Some(vec![Some(PORT.to_owned())]))
    }
}

/// The ports a reply carries (aidl --rust_java_nullability: the list and each String nullable).
type Ports = Option<Vec<Option<String>>>;

/// One getSerialPorts transaction through the local binder's on_transact: the reply's Status, its ports when ok, whether the body ran.
fn call(allow: bool) -> Result<(Status, Option<Ports>, bool)> {
    let ran = Arc::new(AtomicBool::new(false));
    let service = BnSerialManager::new_binder(
        Serial {
            allow,
            ran: ran.clone(),
        },
        BinderFeatures::default(),
    );
    let binder = service.as_binder();
    let data = binder.prepare_transact()?;
    let reply = binder.submit_transact(transactions::r#getSerialPorts, data, 0)?;
    let status: Status = reply.read()?;
    let ports = if status.is_ok() {
        Some(reply.read()?)
    } else {
        None
    };
    Ok((status, ports, ran.load(Ordering::SeqCst)))
}

fn enforce() -> Result<()> {
    let (status, _, ran) = call(false)?;
    ensure!(
        status.exception_code() == ExceptionCode::SECURITY && !ran,
        "denying hook: reply {status:?}, body ran = {ran} (want SECURITY, not run)"
    );
    let (status, ports, ran) = call(true)?;
    ensure!(
        status.is_ok() && ran && ports == Some(Some(vec![Some(PORT.to_owned())])),
        "allowing hook: reply {status:?}, ports {ports:?}, body ran = {ran} (want ok, [{PORT}], run)"
    );
    Ok(())
}

fn round_trip(want: i32) -> Result<()> {
    let sent = ServiceDebugInfo {
        name: Some(NAME.to_owned()),
        // any value; distinct from the default 0 so a dropped field shows
        debugPid: 4242,
    };
    let mut parcel = Parcel::new();
    parcel.write(&sent)?;
    ensure!(
        parcel.get_data_size() == want,
        "parcel holds {} bytes, Parcel's encoding gives {want}",
        parcel.get_data_size()
    );
    // SAFETY: position 0 lies inside every parcel (libbinder_rs's own ParcelableHolder relies on the same); the reads below stay within the bytes written
    unsafe { parcel.set_data_position(0)? };
    let got: ServiceDebugInfo = parcel.read()?;
    // AIDL parcelables derive no PartialEq without @JavaDerive(equals=true): field by field
    ensure!(
        got.name == sent.name && got.debugPid == sent.debugPid,
        "read back {got:?}, wrote {sent:?}"
    );
    ensure!(
        parcel.get_data_position() == want,
        "read stopped at {}, not at the end ({want})",
        parcel.get_data_position()
    );
    Ok(())
}
