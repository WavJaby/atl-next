// SPDX-License-Identifier: MIT
//! android.view.InputChannel (frameworks/base/core/java/android/view/InputChannel.java writeToParcel / readFromParcel, native side
//! frameworks/base/core/jni/android_view_InputChannel.cpp nativeWriteToParcel:198-215 / nativeReadFromParcel:180-195): the reply's client end of an input channel pair.
//! Layout: int32 isInitialized (0: no native channel, nothing follows), then android.os.InputChannelCore, an AIDL C++ parcelable: its int32 size (the whole parcelable, the size field
//! included, back-patched after the fields: InputChannelCore.cpp writeToParcel), the name (String16), the ParcelFileDescriptor (presence int 1, then the dup'd fd as an object), the token
//! (strong binder). The reader takes the size as the parcelable's end: fields past it are refused, bytes before it that no field reads are skipped (AIDL readers skip a later writer's).
//! The C++ side is InputTransport.cpp (InputChannel::openInputChannelPair: socketpair SOCK_SEQPACKET, both ends 32 KiB SO_SNDBUF / SO_RCVBUF, one shared token).

use std::os::fd::OwnedFd;
use std::sync::Arc;

use binder::{ParcelFileDescriptor, SpIBinder, StatusCode};

use crate::parcel::*;
use crate::parts::*;

/// android.os.InputChannelCore: the name, the descriptor (shared, so a copy dups it), the token.
#[derive(Debug, Clone)]
pub struct InputChannelCore {
    pub name: Option<String>,
    pub fd: Arc<OwnedFd>,
    pub token: Option<SpIBinder>,
}

/// The Java InputChannel: `core` None when it holds no native channel (mPtr == 0).
#[derive(Debug, Clone, Default)]
pub struct InputChannel {
    pub core: Option<InputChannelCore>,
}

impl Part for InputChannel {
    fn write(&self, p: &mut In<'_>) -> Result<()> {
        let Some(core) = &self.core else {
            return w_i32(p, 0);
        };
        w_i32(p, 1)?;
        let size_at = p.get_data_position();
        w_i32(p, 0)?;
        w_str16(p, &core.name)?;
        let dup = core.fd.try_clone().map_err(|_| StatusCode::UNKNOWN_ERROR)?;
        p.write(&ParcelFileDescriptor::new(dup))?;
        w_binder(p, &core.token)?;
        let end = p.get_data_position();
        p.set_data_position_checked(size_at)?;
        w_i32(p, end - size_at)?;
        p.set_data_position_checked(end)
    }

    fn read(p: &In<'_>) -> Result<Self> {
        if !r_bool(p)? {
            return Ok(InputChannel { core: None });
        }
        let start = p.get_data_position();
        let size = r_i32(p)?;
        if size < 4 {
            return Err(StatusCode::BAD_VALUE);
        }
        let name = r_str16(p)?;
        let fd: ParcelFileDescriptor = p.read()?;
        let token = r_binder(p)?;
        if p.get_data_position() - start > size {
            return Err(StatusCode::BAD_VALUE);
        }
        p.set_data_position_checked(start + size)?;
        Ok(InputChannel {
            core: Some(InputChannelCore {
                name,
                fd: Arc::new(OwnedFd::from(fd)),
                token,
            }),
        })
    }
}

crate::codec!(InputChannel);
