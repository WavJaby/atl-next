// SPDX-License-Identifier: MIT
//! The host's default network as a native Linux program sees it: the default route from the kernel's route tables (/proc/net/route, /proc/net/ipv6_route: lowest metric, IPv4 first), the link's kind, address and MTU from sysfs,
//! the resolver servers and search domains from /etc/resolv.conf (what glibc's stub resolver uses), and the Internet verdict and metered flag from GIO's GNetworkMonitor, which asks NetworkManager (or the portal in a sandbox) where present
//! and otherwise answers "full" whenever a default route exists (its netlink base). GIO's LOCAL means no route to the Internet: no default network then, whatever the tables say.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::Path;

use atl_platform_host_api::{Connectivity, DefaultNetwork, LinkKind, NetworkError, NetworkSource};
use gtk4::gio;
use gtk4::gio::prelude::*;

const ROUTE_V4: &str = "/proc/net/route";
const ROUTE_V6: &str = "/proc/net/ipv6_route";
const RESOLV_CONF: &str = "/etc/resolv.conf";
const SYS_NET: &str = "/sys/class/net";
/// RTF_UP and RTF_REJECT (linux/route.h): a usable route, an unreachable one.
const RTF_UP: u32 = 0x1;
const RTF_REJECT: u32 = 0x200;

/// The host's network through the kernel tables, sysfs, resolv.conf and GIO's monitor (module doc).
#[derive(Debug, Default)]
pub struct HostNetwork;

/// One default route: interface, gateway (None on-link), metric.
#[derive(Debug, Clone, PartialEq, Eq)]
struct DefaultRoute {
    interface: String,
    gateway: Option<IpAddr>,
    metric: u32,
}

fn bad(what: &str, line: &str) -> NetworkError {
    NetworkError(format!("{what}: unparsable line {line:?}"))
}

/// /proc/net/route (net/ipv4/fib_trie.c fib_route_seq_show): a header line, then `Iface Destination Gateway Flags RefCnt Use Metric Mask MTU Window IRTT`, addresses as 8 hex digits of the address in memory order (little-endian hosts print it reversed).
/// Default = destination and mask 0, flags RTF_UP and not RTF_REJECT.
fn v4_defaults(table: &str) -> Result<Vec<DefaultRoute>, NetworkError> {
    let mut v = Vec::new();
    for line in table.lines().skip(1) {
        let f: Vec<&str> = line.split_whitespace().collect();
        let [iface, dest, gw, flags, _, _, metric, mask, ..] = f.as_slice() else {
            return Err(bad(ROUTE_V4, line));
        };
        let hex = |s: &str| u32::from_str_radix(s, 16).map_err(|_| bad(ROUTE_V4, line));
        let flags = hex(flags)?;
        if hex(dest)? != 0 || hex(mask)? != 0 || flags & RTF_UP == 0 || flags & RTF_REJECT != 0 {
            continue;
        }
        let gw = hex(gw)?;
        v.push(DefaultRoute {
            interface: (*iface).to_owned(),
            // the kernel prints the __be32 as a host-order int: its native bytes are the address bytes
            gateway: (gw != 0).then(|| IpAddr::V4(Ipv4Addr::from(gw.to_ne_bytes()))),
            metric: metric.parse().map_err(|_| bad(ROUTE_V4, line))?,
        });
    }
    Ok(v)
}

/// /proc/net/ipv6_route (net/ipv6/route.c rt6_info_seq_show): `dest plen src splen nexthop metric refcnt use flags iface`, addresses as 32 hex digits in network order, numbers in hex.
/// Default = dest ::/0, flags RTF_UP and not RTF_REJECT, not through `lo` (the kernel's catch-all unreachable entry sits there).
fn v6_defaults(table: &str) -> Result<Vec<DefaultRoute>, NetworkError> {
    let mut v = Vec::new();
    for line in table.lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        let [dest, plen, _, _, nexthop, metric, _, _, flags, iface] = f.as_slice() else {
            return Err(bad(ROUTE_V6, line));
        };
        let hex = |s: &str| u32::from_str_radix(s, 16).map_err(|_| bad(ROUTE_V6, line));
        let flags = hex(flags)?;
        let addr = |s: &str| {
            u128::from_str_radix(s, 16)
                .map(Ipv6Addr::from)
                .map_err(|_| bad(ROUTE_V6, line))
        };
        if !addr(dest)?.is_unspecified()
            || hex(plen)? != 0
            || flags & RTF_UP == 0
            || flags & RTF_REJECT != 0
            || *iface == "lo"
        {
            continue;
        }
        let gw = addr(nexthop)?;
        v.push(DefaultRoute {
            interface: (*iface).to_owned(),
            gateway: (!gw.is_unspecified()).then_some(IpAddr::V6(gw)),
            metric: hex(metric)?,
        });
    }
    Ok(v)
}

/// resolv.conf (resolv_conf.c): `nameserver <address>` lines in order (an IPv6 `%scope` suffix dropped), and the last `search` or `domain` line's domains.
fn resolver(text: &str) -> (Vec<IpAddr>, Option<String>) {
    let mut dns = Vec::new();
    let mut search = None;
    for line in text.lines() {
        let mut w = line.split_whitespace();
        match w.next() {
            Some("nameserver") => {
                if let Some(a) = w.next().and_then(|a| a.split('%').next()?.parse().ok()) {
                    dns.push(a);
                }
            }
            Some("search" | "domain") => {
                let d: Vec<&str> = w.collect();
                search = (!d.is_empty()).then(|| d.join(" "));
            }
            _ => {}
        }
    }
    (dns, search)
}

fn read_opt(path: &Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_owned())
}

impl NetworkSource for HostNetwork {
    fn default_network(&self) -> Result<Option<DefaultNetwork>, NetworkError> {
        let monitor = gio::NetworkMonitor::default();
        let connectivity = match monitor.connectivity() {
            gio::NetworkConnectivity::Local => return Ok(None),
            gio::NetworkConnectivity::Limited => Connectivity::Limited,
            gio::NetworkConnectivity::Portal => Connectivity::Portal,
            _ => Connectivity::Full,
        };
        let read =
            |p: &str| std::fs::read_to_string(p).map_err(|e| NetworkError(format!("{p}: {e}")));
        let v4 = v4_defaults(&read(ROUTE_V4)?)?;
        // a kernel without IPv6 has no table
        let v6 = std::fs::read_to_string(ROUTE_V6).map_or(Ok(Vec::new()), |t| v6_defaults(&t))?;
        let best = |v: &[DefaultRoute]| v.iter().min_by_key(|r| r.metric).cloned();
        let Some(route) = best(&v4).or_else(|| best(&v6)) else {
            return Ok(None);
        };
        let gateways = v4
            .iter()
            .chain(&v6)
            .filter(|r| r.interface == route.interface)
            .filter_map(|r| r.gateway)
            .collect();
        let sys = Path::new(SYS_NET).join(&route.interface);
        // the wireless extensions' dir, present for every cfg80211 interface
        let kind = if sys.join("wireless").exists() {
            LinkKind::Wifi
        } else {
            LinkKind::Ethernet
        };
        let (dns, search) = resolver(&std::fs::read_to_string(RESOLV_CONF).unwrap_or_default());
        Ok(Some(DefaultNetwork {
            hw_address: read_opt(&sys.join("address")).filter(|a| !a.is_empty()),
            mtu: read_opt(&sys.join("mtu")).and_then(|m| m.parse().ok()),
            interface: route.interface,
            kind,
            gateways,
            dns,
            search,
            connectivity,
            metered: monitor.is_network_metered(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Lines as `cat /proc/net/route` prints them on x86_64 (WSL, gateway 172.26.240.1 = bytes ac 1a f0 01, printed in memory order 01F01AAC): one default route, one subnet route.
    #[test]
    fn v4_default_route() {
        let table = "Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT\n\
                     eth0\t00000000\t01F01AAC\t0003\t0\t0\t0\t00000000\t0\t0\t0\n\
                     eth0\t00F01AAC\t00000000\t0001\t0\t0\t0\t00F0FFFF\t0\t0\t0\n";
        assert_eq!(
            v4_defaults(table).expect("parse"),
            [DefaultRoute {
                interface: "eth0".into(),
                gateway: Some(IpAddr::V4(Ipv4Addr::new(172, 26, 240, 1))),
                metric: 0,
            }]
        );
    }

    /// `cat /proc/net/ipv6_route`: a default via fe80::1 on wlan0 (metric 0x258 = 600, flags RTF_UP|RTF_GATEWAY 0x3), and the kernel's unreachable default on lo (flags 0x00200200: RTF_REJECT).
    #[test]
    fn v6_default_route() {
        let table = "00000000000000000000000000000000 00 00000000000000000000000000000000 00 fe800000000000000000000000000001 00000258 00000001 00000000 00000003 wlan0\n\
                     00000000000000000000000000000000 00 00000000000000000000000000000000 00 00000000000000000000000000000000 ffffffff 00000001 00000000 00200200       lo\n";
        assert_eq!(
            v6_defaults(table).expect("parse"),
            [DefaultRoute {
                interface: "wlan0".into(),
                gateway: Some(IpAddr::V6("fe80::1".parse().expect("addr"))),
                metric: 600,
            }]
        );
    }

    #[test]
    fn resolver_servers_and_search() {
        let text = "# generated\nnameserver 10.255.255.254\nnameserver fe80::1%eth0\nsearch lan example.org\n";
        assert_eq!(
            resolver(text),
            (
                vec![
                    IpAddr::V4(Ipv4Addr::new(10, 255, 255, 254)),
                    IpAddr::V6("fe80::1".parse().expect("addr"))
                ],
                Some("lan example.org".into())
            )
        );
    }
}
