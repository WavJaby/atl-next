// SPDX-License-Identifier: MIT
//! android.net.NetworkInfo (Connectivity framework/src/android/net/NetworkInfo.java writeToParcel / CREATOR), field for field: type, subtype, the two names, State and DetailedState by enum name,
//! failover / available / roaming as int flags, reason and extra info.

use crate::parcel::*;
use crate::parts::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkInfo {
    pub network_type: i32,
    pub subtype: i32,
    pub type_name: Option<String>,
    pub subtype_name: Option<String>,
    /// NetworkInfo.State's name (`CONNECTED`, ...).
    pub state: String,
    /// NetworkInfo.DetailedState's name.
    pub detailed_state: String,
    pub is_failover: bool,
    pub is_available: bool,
    pub is_roaming: bool,
    pub reason: Option<String>,
    pub extra_info: Option<String>,
}

impl Part for NetworkInfo {
    fn write(&self, p: &mut In<'_>) -> Result<()> {
        w_i32(p, self.network_type)?;
        w_i32(p, self.subtype)?;
        w_str16(p, &self.type_name)?;
        w_str16(p, &self.subtype_name)?;
        w_str16(p, &Some(self.state.clone()))?;
        w_str16(p, &Some(self.detailed_state.clone()))?;
        w_bool(p, self.is_failover)?;
        w_bool(p, self.is_available)?;
        w_bool(p, self.is_roaming)?;
        w_str16(p, &self.reason)?;
        w_str16(p, &self.extra_info)
    }

    fn read(p: &In<'_>) -> Result<Self> {
        let network_type = r_i32(p)?;
        let subtype = r_i32(p)?;
        let type_name = r_str16(p)?;
        let subtype_name = r_str16(p)?;
        // State.valueOf / DetailedState.valueOf of null throw: the writer never sends one
        let (Some(state), Some(detailed_state)) = (r_str16(p)?, r_str16(p)?) else {
            return Err(crate::refuse("android.net.NetworkInfo (null state)"));
        };
        Ok(NetworkInfo {
            network_type,
            subtype,
            type_name,
            subtype_name,
            state,
            detailed_state,
            is_failover: r_bool(p)?,
            is_available: r_bool(p)?,
            is_roaming: r_bool(p)?,
            reason: r_str16(p)?,
            extra_info: r_str16(p)?,
        })
    }
}

crate::codec!(NetworkInfo);
