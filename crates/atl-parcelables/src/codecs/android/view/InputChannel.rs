// SPDX-License-Identifier: MIT
//! android.view.InputChannel (frameworks/base/core/java/android/view/InputChannel.java writeToParcel / readFromParcel, native side
//! frameworks/base/core/jni/android_view_InputChannel.cpp nativeWriteToParcel:198-215 / nativeReadFromParcel:180-195): the reply's client end of an input channel pair.
//! Layout: int32 isInitialized (0: no native channel, nothing follows), then android.os.InputChannelCore (utf8-in-C++ name as a String16, the ParcelFileDescriptor
//! (presence int 1, then the dup'd fd as an object), the token as a strong binder). Every field is modelled. The C++ side is InputTransport.cpp (InputChannel::openInputChannelPair:
//! socketpair SOCK_SEQPACKET, both ends 32 KiB SO_SNDBUF / SO_RCVBUF, one shared token).

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
        w_str16(p, &core.name)?;
        let dup = core
            .fd
            .try_clone()
            .map_err(|_| StatusCode::UNKNOWN_ERROR)?;
        p.write(&ParcelFileDescriptor::new(dup))?;
        w_binder(p, &core.token)
    }

    fn read(p: &In<'_>) -> Result<Self> {
        if !r_bool(p)? {
            return Ok(InputChannel { core: None });
        }
        let name = r_str16(p)?;
        let fd: ParcelFileDescriptor = p.read()?;
        let token = r_binder(p)?;
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
