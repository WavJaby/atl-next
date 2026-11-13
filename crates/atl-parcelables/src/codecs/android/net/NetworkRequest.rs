// SPDX-License-Identifier: MIT
//! android.net.NetworkRequest (Connectivity framework/src/android/net/NetworkRequest.java writeToParcel / CREATOR): the capabilities in place, legacyType, requestId, and the Type enum by name.
//! ConnectivityManager keys its callbacks by NetworkRequest.equals (capabilities, legacyType, requestId, type), so the copy a callback message carries must equal the one requestNetwork returned.

use crate::codecs::android::net::NetworkCapabilities::NetworkCapabilities;
use crate::parcel::*;
use crate::parts::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkRequest {
    pub capabilities: NetworkCapabilities,
    pub legacy_type: i32,
    pub request_id: i32,
    /// NetworkRequest.Type's name (`TRACK_DEFAULT`, `LISTEN`, ...).
    pub request_type: String,
}

impl Part for NetworkRequest {
    fn write(&self, p: &mut In<'_>) -> Result<()> {
        self.capabilities.write(p)?;
        w_i32(p, self.legacy_type)?;
        w_i32(p, self.request_id)?;
        w_str16(p, &Some(self.request_type.clone()))
    }

    fn read(p: &In<'_>) -> Result<Self> {
        let capabilities = NetworkCapabilities::read(p)?;
        let legacy_type = r_i32(p)?;
        let request_id = r_i32(p)?;
        // Type.valueOf(null) throws
        let request_type =
            r_str16(p)?.ok_or_else(|| crate::refuse("android.net.NetworkRequest.type (null)"))?;
        Ok(NetworkRequest {
            capabilities,
            legacy_type,
            request_id,
            request_type,
        })
    }
}

crate::codec!(NetworkRequest);
