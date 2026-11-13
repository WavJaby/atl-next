// SPDX-License-Identifier: MIT
//! android.net.NetworkCapabilities (Connectivity framework/src/android/net/NetworkCapabilities.java writeToParcel:2404-2426 / CREATOR:2431-2464), field for field.
//! Not modelled, null only: the network specifier and the transport info (each a writeParcelable of an app-defined class). The uid set is UidRange parcelables (UidRange.java writeToParcel: start, stop).

use crate::codecs::android::net::Network::Network;
use crate::parcel::*;
use crate::parts::*;

/// NetworkCapabilities.NET_CAPABILITY_* bits the services set (NetworkCapabilities.java:528-754).
pub const NET_CAPABILITY_NOT_METERED: u32 = 11;
pub const NET_CAPABILITY_INTERNET: u32 = 12;
pub const NET_CAPABILITY_NOT_RESTRICTED: u32 = 13;
pub const NET_CAPABILITY_TRUSTED: u32 = 14;
pub const NET_CAPABILITY_NOT_VPN: u32 = 15;
pub const NET_CAPABILITY_VALIDATED: u32 = 16;
pub const NET_CAPABILITY_CAPTIVE_PORTAL: u32 = 17;
pub const NET_CAPABILITY_NOT_ROAMING: u32 = 18;
pub const NET_CAPABILITY_FOREGROUND: u32 = 19;
pub const NET_CAPABILITY_NOT_CONGESTED: u32 = 20;
pub const NET_CAPABILITY_NOT_SUSPENDED: u32 = 21;
pub const NET_CAPABILITY_NOT_VCN_MANAGED: u32 = 28;
pub const NET_CAPABILITY_NOT_BANDWIDTH_CONSTRAINED: u32 = 37;
/// NetworkCapabilities.TRANSPORT_* (NetworkCapabilities.java:1313-1367).
pub const TRANSPORT_WIFI: u32 = 1;
pub const TRANSPORT_ETHERNET: u32 = 3;
/// SIGNAL_STRENGTH_UNSPECIFIED (Integer.MIN_VALUE) and Process.INVALID_UID, the constructor's values (clearAll).
pub const SIGNAL_STRENGTH_UNSPECIFIED: i32 = i32::MIN;
pub const INVALID_UID: i32 = -1;

/// UidRange's class name, as writeParcelable writes it in the uid set.
const UID_RANGE: &str = "android.net.UidRange";

/// android.net.UidRange: an inclusive uid interval.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UidRange {
    pub start: i32,
    pub stop: i32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkCapabilities {
    /// Bit n = NET_CAPABILITY n.
    pub capabilities: u64,
    pub forbidden: u64,
    /// Bit n = TRANSPORT n.
    pub transports: u64,
    pub link_up_kbps: i32,
    pub link_down_kbps: i32,
    pub signal_strength: i32,
    /// None = null (no uid restriction).
    pub uids: Option<Vec<UidRange>>,
    pub allowed_uids: Vec<i32>,
    pub ssid: Option<String>,
    pub private_dns_broken: bool,
    pub administrator_uids: Vec<i32>,
    pub owner_uid: i32,
    pub requestor_uid: i32,
    pub requestor_package_name: Option<String>,
    pub sub_ids: Vec<i32>,
    pub underlying_networks: Option<Vec<Option<Network>>>,
    pub enterprise_id: i32,
    pub reservation_id: i32,
    pub match_non_thread_local_networks: bool,
}

impl NetworkCapabilities {
    /// What `new NetworkCapabilities()` holds: clearAll's values plus DEFAULT_CAPABILITIES (NOT_RESTRICTED, TRUSTED, NOT_VPN, and NOT_BANDWIDTH_CONSTRAINED from SDK 35, NetworkCapabilities.java:800-810; this platform is 36).
    pub fn new() -> Self {
        NetworkCapabilities {
            capabilities: bits(&[
                NET_CAPABILITY_NOT_RESTRICTED,
                NET_CAPABILITY_TRUSTED,
                NET_CAPABILITY_NOT_VPN,
                NET_CAPABILITY_NOT_BANDWIDTH_CONSTRAINED,
            ]),
            forbidden: 0,
            transports: 0,
            link_up_kbps: 0,
            link_down_kbps: 0,
            signal_strength: SIGNAL_STRENGTH_UNSPECIFIED,
            uids: None,
            allowed_uids: Vec::new(),
            ssid: None,
            private_dns_broken: false,
            administrator_uids: Vec::new(),
            owner_uid: INVALID_UID,
            requestor_uid: INVALID_UID,
            requestor_package_name: None,
            sub_ids: Vec::new(),
            underlying_networks: None,
            enterprise_id: 0,
            reservation_id: 0,
            match_non_thread_local_networks: false,
        }
    }

    pub fn has(&self, capability: u32) -> bool {
        self.capabilities & (1 << capability) != 0
    }
}

impl Default for NetworkCapabilities {
    fn default() -> Self {
        NetworkCapabilities::new()
    }
}

/// The bit set of `numbers` (capabilities or transports).
pub fn bits(numbers: &[u32]) -> u64 {
    numbers.iter().fold(0, |b, n| b | (1 << n))
}

fn w_int_list(p: &mut In<'_>, v: &[i32]) -> Result<()> {
    w_i32_array(p, &Some(v.to_vec()))
}

/// createIntArray where Java dereferences the result (`ensureCapacity(allowedUids.length)`, requireNonNull): null is refused.
fn r_int_list(p: &In<'_>, what: &'static str) -> Result<Vec<i32>> {
    r_i32_array(p)?.ok_or_else(|| crate::refuse(what))
}

impl Part for NetworkCapabilities {
    fn write(&self, p: &mut In<'_>) -> Result<()> {
        // writeLong of the masked bit sets: the masks keep only defined bits, which these are
        w_i64(p, self.capabilities as i64)?;
        w_i64(p, self.forbidden as i64)?;
        w_i64(p, self.transports as i64)?;
        w_i32(p, self.link_up_kbps)?;
        w_i32(p, self.link_down_kbps)?;
        w_class(p, None)?;
        w_class(p, None)?;
        w_i32(p, self.signal_strength)?;
        match &self.uids {
            None => w_i32(p, -1)?,
            Some(ranges) => {
                w_i32(p, len_i32(ranges.len())?)?;
                for r in ranges {
                    w_class(p, Some(UID_RANGE))?;
                    w_i32(p, r.start)?;
                    w_i32(p, r.stop)?;
                }
            }
        }
        w_int_list(p, &self.allowed_uids)?;
        w_str16(p, &self.ssid)?;
        w_bool(p, self.private_dns_broken)?;
        w_int_list(p, &self.administrator_uids)?;
        w_i32(p, self.owner_uid)?;
        w_i32(p, self.requestor_uid)?;
        w_str16(p, &self.requestor_package_name)?;
        w_int_list(p, &self.sub_ids)?;
        w_typed_list(p, &self.underlying_networks)?;
        w_i32(p, self.enterprise_id)?;
        w_i32(p, self.reservation_id)?;
        w_bool(p, self.match_non_thread_local_networks)
    }

    fn read(p: &In<'_>) -> Result<Self> {
        let capabilities = r_i64(p)? as u64;
        let forbidden = r_i64(p)? as u64;
        let transports = r_i64(p)? as u64;
        let link_up_kbps = r_i32(p)?;
        let link_down_kbps = r_i32(p)?;
        r_null(p, "android.net.NetworkCapabilities.mNetworkSpecifier")?;
        r_null(p, "android.net.NetworkCapabilities.mTransportInfo")?;
        let signal_strength = r_i32(p)?;
        let n = r_i32(p)?;
        let uids = if n < 0 {
            None
        } else {
            let mut v = Vec::new();
            for _ in 0..n {
                if r_class(
                    p,
                    UID_RANGE,
                    "android.net.NetworkCapabilities.mUids (element class)",
                )?
                .is_none()
                {
                    return Err(crate::refuse(
                        "android.net.NetworkCapabilities.mUids (null element)",
                    ));
                }
                v.push(UidRange {
                    start: r_i32(p)?,
                    stop: r_i32(p)?,
                });
            }
            Some(v)
        };
        Ok(NetworkCapabilities {
            capabilities,
            forbidden,
            transports,
            link_up_kbps,
            link_down_kbps,
            signal_strength,
            uids,
            allowed_uids: r_int_list(p, "android.net.NetworkCapabilities.mAllowedUids (null)")?,
            ssid: r_str16(p)?,
            private_dns_broken: r_bool(p)?,
            // setAdministratorUids(null) throws (Objects.requireNonNull)
            administrator_uids: r_int_list(
                p,
                "android.net.NetworkCapabilities.mAdministratorUids (null)",
            )?,
            owner_uid: r_i32(p)?,
            requestor_uid: r_i32(p)?,
            requestor_package_name: r_str16(p)?,
            sub_ids: r_int_list(p, "android.net.NetworkCapabilities.mSubIds (null)")?,
            underlying_networks: r_typed_list(p)?,
            enterprise_id: r_i32(p)?,
            reservation_id: r_i32(p)?,
            match_non_thread_local_networks: r_bool(p)?,
        })
    }
}

crate::codec!(NetworkCapabilities);
