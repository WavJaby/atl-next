// SPDX-License-Identifier: MIT
//! java.io.FileDescriptor, the Java-only builtin (`in FileDescriptor` in framework AIDL): os/Parcel.java writeRawFileDescriptor / readRawFileDescriptor, Parcel.cpp writeDupFileDescriptor: one fd object in the parcel, no null flag and no leading int.
//! Write: the object is a ParcelFileDescriptor's (NDK AParcel_writeParcelFileDescriptor: int32 1 for non-null, then Parcel.cpp writeParcelFileDescriptor: int32 0, then the fd object) with those 8 leading bytes left out; AParcel_appendFrom copies the object's table entry and dups the fd.
//! Read: the same object, read through a scratch parcel that gets the 8 leading bytes and a window of the input (AParcel_appendFrom again), so ParcelFileDescriptor's own read does the work. The input then advances by the object's length as that read consumed it (24 bytes with 64-bit IPC, 16 with 32-bit): derived, not assumed.
//! No descriptor (`fd: None`, the Default) is written as nothing. Java has no null form either, so no reader meets one; the Rust services always send a descriptor.

use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::Arc;

use binder::binder_impl::{
    BorrowedParcel, Deserialize, DeserializeOption, Parcel, Serialize, SerializeOption,
};
use binder::{ParcelFileDescriptor, StatusCode};

use crate::parcel::*;

/// What a `ParcelFileDescriptor` writes before its fd object: the non-null int32 (ndk/parcel.cpp AParcel_writeParcelFileDescriptor) and the comm-channel int32 0 (Parcel.cpp writeParcelFileDescriptor).
const PFD_LEAD: i32 = 8;
/// Bytes of input a read copies: room for one flat_binder_object (24 bytes with 64-bit IPC, 16 with 32-bit). Only the first object is read; a second one in the window is copied too and closes with the scratch parcel.
const WINDOW: i32 = 32;

#[derive(Debug, Clone, Default)]
pub struct FileDescriptor {
    /// None = no descriptor: nothing is written.
    pub fd: Option<Arc<OwnedFd>>,
}

impl FileDescriptor {
    pub fn new(fd: OwnedFd) -> Self {
        FileDescriptor {
            fd: Some(Arc::new(fd)),
        }
    }

    /// One descriptor object at the position, consumed (Parcel.readRawFileDescriptor).
    fn read_object(p: &BorrowedParcel<'_>) -> Result<OwnedFd> {
        let at = p.get_data_position();
        let window = WINDOW.min(p.get_data_size() - at);
        let mut scratch = Parcel::new();
        w_i32(&mut scratch.borrowed(), 1)?;
        w_i32(&mut scratch.borrowed(), 0)?;
        scratch.append_from(p, at, window)?;
        // appending moved the position past the window: the read starts from the lead bytes again
        scratch.set_data_position_checked(0)?;
        let pfd = scratch
            .borrowed_ref()
            .read::<Option<ParcelFileDescriptor>>()?
            .ok_or(StatusCode::BAD_VALUE)?;
        // the object's length, as the read consumed it: everything past the 8 leading bytes
        let object = scratch.borrowed_ref().get_data_position() - PFD_LEAD;
        if object <= 0 || object % 4 != 0 || object > window {
            return Err(StatusCode::BAD_VALUE);
        }
        // a seek, not int reads: libbinder refuses a non-object read over an object (validateReadData)
        p.set_data_position_checked(at + object)?;
        Ok(pfd.into())
    }
}

impl PartialEq for FileDescriptor {
    fn eq(&self, other: &Self) -> bool {
        self.fd.as_ref().map(|f| f.as_raw_fd()) == other.fd.as_ref().map(|f| f.as_raw_fd())
    }
}

impl Serialize for FileDescriptor {
    fn serialize(&self, p: &mut BorrowedParcel<'_>) -> Result<()> {
        let Some(fd) = self.fd.as_ref() else {
            return Ok(());
        };
        let dup = fd.try_clone().map_err(|_| StatusCode::UNKNOWN_ERROR)?;
        let mut tmp = Parcel::new();
        tmp.borrowed().write(&ParcelFileDescriptor::new(dup))?;
        let size = tmp.borrowed_ref().get_data_size();
        p.append_from(&tmp, PFD_LEAD, size - PFD_LEAD)
    }
}

impl SerializeOption for FileDescriptor {
    fn serialize_option(this: Option<&Self>, p: &mut BorrowedParcel<'_>) -> Result<()> {
        match this {
            Some(f) => f.serialize(p),
            None => Ok(()),
        }
    }
}

impl Deserialize for FileDescriptor {
    type UninitType = Option<Self>;
    fn uninit() -> Self::UninitType {
        None
    }
    fn from_init(value: Self) -> Self::UninitType {
        Some(value)
    }
    fn deserialize(p: &BorrowedParcel<'_>) -> Result<Self> {
        Ok(FileDescriptor::new(FileDescriptor::read_object(p)?))
    }
}

impl DeserializeOption for FileDescriptor {
    fn deserialize_option(p: &BorrowedParcel<'_>) -> Result<Option<Self>> {
        Self::deserialize(p).map(Some)
    }
}
