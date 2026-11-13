// SPDX-License-Identifier: MIT
//! android.net.LinkProperties (Connectivity framework/src/android/net/LinkProperties.java writeToParcel / CREATOR), field for field, with its parts written in place:
//! LinkAddress (LinkAddress.java: address bytes, prefix length, flags, scope, deprecation and expiration times), RouteInfo (RouteInfo.java: an IpPrefix by writeParcelable, gateway bytes, interface, type, MTU),
//! IpPrefix (IpPrefix.java: address bytes, prefix length). An InetAddress is writeAddress's form: the bytes (4 or 16), and for IPv6 a scope flag (+ the scope id).
//! Not modelled, empty or null only: validated private DNS servers, P-CSCF servers, the DHCP server address, the HTTP proxy, the NAT64 prefix, stacked links, the captive portal URL and data. A scoped IPv6 address is refused.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use crate::parcel::*;
use crate::parts::*;

const LINK_ADDRESS: &str = "android.net.LinkAddress";
const ROUTE_INFO: &str = "android.net.RouteInfo";
const IP_PREFIX: &str = "android.net.IpPrefix";

/// RouteInfo.RTN_UNICAST (RouteInfo.java): a route to a gateway or a directly connected network.
pub const RTN_UNICAST: i32 = 1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkAddress {
    pub address: IpAddr,
    pub prefix_length: i32,
    /// IFA_F_* flags.
    pub flags: i32,
    /// RT_SCOPE_* scope.
    pub scope: i32,
    pub deprecation_time: i64,
    pub expiration_time: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteInfo {
    /// The destination prefix: address and length.
    pub destination: (IpAddr, i32),
    pub gateway: Option<IpAddr>,
    pub interface: Option<String>,
    pub route_type: i32,
    pub mtu: i32,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LinkProperties {
    pub interface_name: Option<String>,
    pub link_addresses: Vec<LinkAddress>,
    pub dnses: Vec<IpAddr>,
    pub use_private_dns: bool,
    pub private_dns_server_name: Option<String>,
    pub domains: Option<String>,
    pub mtu: i32,
    pub tcp_buffer_sizes: Option<String>,
    pub routes: Vec<RouteInfo>,
    pub wake_on_lan_supported: bool,
}

fn octets(a: &IpAddr) -> Vec<u8> {
    match a {
        IpAddr::V4(v) => v.octets().to_vec(),
        IpAddr::V6(v) => v.octets().to_vec(),
    }
}

fn addr_of(bytes: &[u8], what: &'static str) -> Result<IpAddr> {
    if let Ok(b) = <[u8; 4]>::try_from(bytes) {
        Ok(IpAddr::V4(Ipv4Addr::from(b)))
    } else if let Ok(b) = <[u8; 16]>::try_from(bytes) {
        Ok(IpAddr::V6(Ipv6Addr::from(b)))
    } else {
        Err(crate::refuse(what))
    }
}

/// writeAddress: the bytes, then for IPv6 hasScopeId (false: no scope modelled).
fn w_addr(p: &mut In<'_>, a: Option<&IpAddr>) -> Result<()> {
    w_bytes(p, a.map(octets).as_deref())?;
    if matches!(a, Some(IpAddr::V6(_))) {
        w_bool(p, false)?;
    }
    Ok(())
}

/// readAddress (LinkProperties.java:1777-1790): null bytes = null; 16 bytes carry the scope flag.
fn r_addr(p: &In<'_>) -> Result<Option<IpAddr>> {
    let Some(bytes) = r_bytes(p)? else {
        return Ok(None);
    };
    let a = addr_of(&bytes, "android.net.LinkProperties (address length)")?;
    if a.is_ipv6() && r_bool(p)? {
        return Err(crate::refuse(
            "android.net.LinkProperties (scoped IPv6 address)",
        ));
    }
    Ok(Some(a))
}

fn w_addrs(p: &mut In<'_>, v: &[IpAddr]) -> Result<()> {
    w_i32(p, len_i32(v.len())?)?;
    v.iter().try_for_each(|a| w_addr(p, Some(a)))
}

fn r_addrs(p: &In<'_>) -> Result<Vec<IpAddr>> {
    let n = r_i32(p)?;
    (0..n.max(0))
        .map(|_| {
            r_addr(p)?.ok_or_else(|| crate::refuse("android.net.LinkProperties (null address)"))
        })
        .collect()
}

/// A writeParcelable element of a list Java never fills with null (the CREATOR adds each with addLinkAddress / addRoute).
fn r_element(p: &In<'_>, class: &str, what: &'static str) -> Result<()> {
    r_class(p, class, what)?.ok_or_else(|| crate::refuse(what))
}

impl Part for LinkAddress {
    fn write(&self, p: &mut In<'_>) -> Result<()> {
        w_bytes(p, Some(&octets(&self.address)))?;
        w_i32(p, self.prefix_length)?;
        w_i32(p, self.flags)?;
        w_i32(p, self.scope)?;
        w_i64(p, self.deprecation_time)?;
        w_i64(p, self.expiration_time)
    }

    fn read(p: &In<'_>) -> Result<Self> {
        let bytes =
            r_bytes(p)?.ok_or_else(|| crate::refuse("android.net.LinkAddress (null address)"))?;
        Ok(LinkAddress {
            address: addr_of(&bytes, "android.net.LinkAddress (address length)")?,
            prefix_length: r_i32(p)?,
            flags: r_i32(p)?,
            scope: r_i32(p)?,
            deprecation_time: r_i64(p)?,
            expiration_time: r_i64(p)?,
        })
    }
}

impl Part for RouteInfo {
    fn write(&self, p: &mut In<'_>) -> Result<()> {
        w_class(p, Some(IP_PREFIX))?;
        w_bytes(p, Some(&octets(&self.destination.0)))?;
        w_i32(p, self.destination.1)?;
        w_bytes(p, self.gateway.as_ref().map(octets).as_deref())?;
        w_str16(p, &self.interface)?;
        w_i32(p, self.route_type)?;
        w_i32(p, self.mtu)
    }

    fn read(p: &In<'_>) -> Result<Self> {
        // the CREATOR's `new RouteInfo(dest, ...)` takes a null destination as the default route; the writer here always sends one
        r_element(p, IP_PREFIX, "android.net.RouteInfo.mDestination")?;
        let bytes =
            r_bytes(p)?.ok_or_else(|| crate::refuse("android.net.IpPrefix (null address)"))?;
        let destination = (
            addr_of(&bytes, "android.net.IpPrefix (address length)")?,
            r_i32(p)?,
        );
        let gateway = r_bytes(p)?
            .map(|b| addr_of(&b, "android.net.RouteInfo.mGateway (length)"))
            .transpose()?;
        Ok(RouteInfo {
            destination,
            gateway,
            interface: r_str16(p)?,
            route_type: r_i32(p)?,
            mtu: r_i32(p)?,
        })
    }
}

impl Part for LinkProperties {
    fn write(&self, p: &mut In<'_>) -> Result<()> {
        w_str16(p, &self.interface_name)?;
        w_i32(p, len_i32(self.link_addresses.len())?)?;
        for a in &self.link_addresses {
            w_class(p, Some(LINK_ADDRESS))?;
            a.write(p)?;
        }
        w_addrs(p, &self.dnses)?;
        // mValidatedPrivateDnses
        w_addrs(p, &[])?;
        w_bool(p, self.use_private_dns)?;
        w_str16(p, &self.private_dns_server_name)?;
        // mPcscfs
        w_addrs(p, &[])?;
        w_str16(p, &self.domains)?;
        // mDhcpServerAddress
        w_addr(p, None)?;
        w_i32(p, self.mtu)?;
        w_str16(p, &self.tcp_buffer_sizes)?;
        w_i32(p, len_i32(self.routes.len())?)?;
        for r in &self.routes {
            w_class(p, Some(ROUTE_INFO))?;
            r.write(p)?;
        }
        // no HTTP proxy (writeByte 0), no NAT64 prefix, no stacked links (writeList of an empty list)
        w_i32(p, 0)?;
        w_class(p, None)?;
        w_i32(p, 0)?;
        w_bool(p, self.wake_on_lan_supported)?;
        // captive portal URL and data
        w_class(p, None)?;
        w_class(p, None)
    }

    fn read(p: &In<'_>) -> Result<Self> {
        let interface_name = r_str16(p)?;
        let n = r_i32(p)?;
        let mut link_addresses = Vec::new();
        for _ in 0..n.max(0) {
            r_element(
                p,
                LINK_ADDRESS,
                "android.net.LinkProperties.mLinkAddresses (element)",
            )?;
            link_addresses.push(LinkAddress::read(p)?);
        }
        let dnses = r_addrs(p)?;
        if !r_addrs(p)?.is_empty() {
            return Err(crate::refuse(
                "android.net.LinkProperties.mValidatedPrivateDnses",
            ));
        }
        let use_private_dns = r_bool(p)?;
        let private_dns_server_name = r_str16(p)?;
        if !r_addrs(p)?.is_empty() {
            return Err(crate::refuse("android.net.LinkProperties.mPcscfs"));
        }
        let domains = r_str16(p)?;
        if r_addr(p)?.is_some() {
            return Err(crate::refuse(
                "android.net.LinkProperties.mDhcpServerAddress",
            ));
        }
        let mtu = r_i32(p)?;
        let tcp_buffer_sizes = r_str16(p)?;
        let n = r_i32(p)?;
        let mut routes = Vec::new();
        for _ in 0..n.max(0) {
            r_element(
                p,
                ROUTE_INFO,
                "android.net.LinkProperties.mRoutes (element)",
            )?;
            routes.push(RouteInfo::read(p)?);
        }
        if r_i32(p)? != 0 {
            return Err(crate::refuse("android.net.LinkProperties.mHttpProxy"));
        }
        if r_str16(p)?.is_some() {
            return Err(crate::refuse("android.net.LinkProperties.mNat64Prefix"));
        }
        // readList: -1 null, else the count; only an empty list is modelled
        if r_i32(p)? > 0 {
            return Err(crate::refuse("android.net.LinkProperties.mStackedLinks"));
        }
        let wake_on_lan_supported = r_bool(p)?;
        if r_str16(p)?.is_some() || r_str16(p)?.is_some() {
            return Err(crate::refuse("android.net.LinkProperties (captive portal)"));
        }
        Ok(LinkProperties {
            interface_name,
            link_addresses,
            dnses,
            use_private_dns,
            private_dns_server_name,
            domains,
            mtu,
            tcp_buffer_sizes,
            routes,
            wake_on_lan_supported,
        })
    }
}

crate::codec!(LinkProperties);
