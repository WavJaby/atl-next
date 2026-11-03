// SPDX-License-Identifier: MIT
//! java.io.FileDescriptor, the Java-only builtin (`in FileDescriptor` in framework AIDL): os/Parcel.java writeRawFileDescriptor / readRawFileDescriptor, Parcel.cpp writeDupFileDescriptor: one fd object in the parcel, no null flag and no leading int.
//! Write only: the Rust services send descriptors (IApplicationThread.bindApplication's applicationSharedMemoryFd), no Rust code reads one. libbinder_rs has no raw fd read or write, only `ParcelFileDescriptor`
//! (NDK AParcel_writeParcelFileDescriptor: int32 1 for non-null, then Parcel.cpp writeParcelFileDescriptor: int32 0, then the fd object): a write puts the object there and leaves the 8 leading bytes out when it is appended
//! (AParcel_appendFrom copies the fd object's table entry and dups the fd); a read would have to move the parcel's position past the object, which libbinder_rs allows only as `unsafe`.
//! No descriptor (`fd: None`, the Default) is written as nothing and read as nothing, consuming no bytes: Java's writeRawFileDescriptor has no null form either, so only a Rust peer that cannot read descriptors may be sent one
//! (`xtask test binder`'s client, a Rust IApplicationThread).

use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::Arc;

use binder::binder_impl::{
    BorrowedParcel, Deserialize, DeserializeOption, Parcel, Serialize, SerializeOption,
};
use binder::{ParcelFileDescriptor, StatusCode};

use crate::parcel::*;

/// What a `ParcelFileDescriptor` writes before its fd object: the non-null int32 (ndk/parcel.cpp AParcel_writeParcelFileDescriptor) and the comm-channel int32 0 (Parcel.cpp writeParcelFileDescriptor).
const PFD_LEAD: i32 = 8;

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
    fn deserialize(_: &BorrowedParcel<'_>) -> Result<Self> {
        Ok(FileDescriptor::default())
    }
}

impl DeserializeOption for FileDescriptor {
    /// Nothing is consumed: a parcel that holds a descriptor here is not read correctly (see the module doc).
    fn deserialize_option(_: &BorrowedParcel<'_>) -> Result<Option<Self>> {
        Ok(None)
    }
}
