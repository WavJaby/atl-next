// SPDX-License-Identifier: MIT
//! android.net.Network (packages/modules/Connectivity framework/src/android/net/Network.java writeToParcel / CREATOR): the netId, one int.

use crate::parcel::*;
use crate::parts::*;

/// Java writeParcelable's class name of a Network (NetworkCapabilities' underlying networks are typed, a Bundle value is not).
pub const CLASS: &str = "android.net.Network";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Network {
    pub net_id: i32,
}

impl Part for Network {
    fn write(&self, p: &mut In<'_>) -> Result<()> {
        w_i32(p, self.net_id)
    }

    fn read(p: &In<'_>) -> Result<Self> {
        Ok(Network { net_id: r_i32(p)? })
    }
}

crate::codec!(Network);
