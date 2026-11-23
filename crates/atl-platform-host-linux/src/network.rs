// SPDX-License-Identifier: MIT
//! The host's default network as a native Linux program sees it: the default route from the kernel's route tables (/proc/net/route, /proc/net/ipv6_route: lowest metric, IPv4 first), the link's kind, address and MTU from sysfs,
//! the resolver servers and search domains from /etc/resolv.conf (what glibc's stub resolver uses), and the Internet verdict and metered flag from GIO's GNetworkMonitor, which asks NetworkManager (or the portal in a sandbox) where present
//! and otherwise answers "full" whenever a default route exists (its netlink base). GIO's LOCAL means no route to the Internet: no default network then, whatever the tables say.
//! Changes: `watch` reports rtnetlink link, address and route events as bursts. It says only that something changed; the caller re-reads `default_network` (GIO's monitor is read there, not on the watch thread).

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::Path;
use std::sync::mpsc::Sender;

use atl_platform_host_api::{Connectivity, DefaultNetwork, LinkKind, NetworkError, NetworkSource};
use gtk4::gio;
use gtk4::gio::prelude::*;
use nix::errno::Errno;
use nix::sys::socket::{
    AddressFamily, MsgFlags, NetlinkAddr, SockFlag, SockProtocol, SockType, bind, recv, socket,
};

const ROUTE_V4: &str = "/proc/net/route";
const ROUTE_V6: &str = "/proc/net/ipv6_route";
const RESOLV_CONF: &str = "/etc/resolv.conf";
const SYS_NET: &str = "/sys/class/net";
/// RTF_UP and RTF_REJECT (linux/route.h): a usable route, an unreachable one.
const RTF_UP: u32 = 0x1;
const RTF_REJECT: u32 = 0x200;
/// rtnetlink groups the watch joins (linux/rtnetlink.h RTMGRP_LINK | IPV4_IFADDR | IPV4_ROUTE | IPV6_IFADDR | IPV6_ROUTE): link state, addresses, routes.
const WATCH_GROUPS: u32 = 0x1 | 0x10 | 0x40 | 0x100 | 0x400;
/// nlmsghdr (linux/netlink.h): 16 bytes (len u32, type u16, flags u16, seq u32, pid u32, native byte order); messages padded to 4 (NLMSG_ALIGNTO).
const NLMSG_HDRLEN: usize = 16;
/// Framing messages, not events: NLMSG_NOOP, NLMSG_DONE.
const NLMSG_NOOP: u16 = 0x1;
const NLMSG_DONE: u16 = 0x3;
/// The receive buffer: a longer datagram is cut short, fails the length check and counts as a change. The largest datagram of the recorded capture (testdata) is 1428 bytes.
const DATAGRAM_MAX: usize = 64 * 1024;

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

    fn watch(&self, changed: Sender<()>) -> Result<(), NetworkError> {
        let sock = socket(
            AddressFamily::Netlink,
            SockType::Raw,
            SockFlag::SOCK_CLOEXEC,
            SockProtocol::NetlinkRoute,
        )
        .map_err(|e| NetworkError(format!("netlink socket: {e}")))?;
        bind(sock.as_raw_fd(), &NetlinkAddr::new(0, WATCH_GROUPS))
            .map_err(|e| NetworkError(format!("netlink bind: {e}")))?;
        std::thread::Builder::new()
            .name("net-watch".into())
            .spawn(move || run(sock, changed))
            .map_err(|e| NetworkError(format!("net-watch thread: {e}")))?;
        Ok(())
    }
}

/// One burst of watch events: the datagrams one wake-up takes, reduced to whether the default network may have changed.
#[derive(Debug, Default)]
struct Burst {
    changed: bool,
}

impl Burst {
    /// Any message but the framing ones is a change; a datagram the framing cannot read is one too.
    fn feed(&mut self, datagram: &[u8]) {
        self.changed |= match message_types(datagram) {
            Some(types) => types.iter().any(|&t| t != NLMSG_NOOP && t != NLMSG_DONE),
            None => true,
        };
    }

    /// ENOBUFS: events were dropped, so the state is unknown.
    fn overflow(&mut self) {
        self.changed = true;
    }

    /// The burst's verdict; the next burst starts empty.
    fn take(&mut self) -> bool {
        std::mem::take(&mut self.changed)
    }
}

/// The message types of one datagram, in order; `None` when a header is cut short or a length runs past the datagram.
fn message_types(mut rest: &[u8]) -> Option<Vec<u16>> {
    let mut types = Vec::new();
    while !rest.is_empty() {
        let header = rest.get(..NLMSG_HDRLEN)?;
        let len = usize::try_from(u32::from_ne_bytes([
            header[0], header[1], header[2], header[3],
        ]))
        .ok()?;
        if !(NLMSG_HDRLEN..=rest.len()).contains(&len) {
            return None;
        }
        types.push(u16::from_ne_bytes([header[4], header[5]]));
        rest = rest.get((len + 3) & !3..).unwrap_or_default();
    }
    Some(types)
}

/// The watch thread: block for a datagram, take the ones already queued, send one `()` if the burst changed anything.
/// Ends when the receiver is gone (found at the next send) or recv fails (one last `()`, so the caller re-reads what it may have missed).
fn run(sock: OwnedFd, changed: Sender<()>) {
    let fd = sock.as_raw_fd();
    let mut buf = vec![0u8; DATAGRAM_MAX];
    let mut burst = Burst::default();
    loop {
        let mut flags = MsgFlags::empty();
        loop {
            match recv(fd, &mut buf, flags) {
                Ok(n) => burst.feed(&buf[..n]),
                Err(Errno::ENOBUFS) => burst.overflow(),
                Err(Errno::EINTR) => continue,
                Err(Errno::EAGAIN) => break,
                Err(e) => {
                    tracing::warn!("network watch ends: {e}");
                    let _ = changed.send(());
                    return;
                }
            }
            flags = MsgFlags::MSG_DONTWAIT;
        }
        if burst.take() && changed.send(()).is_err() {
            return;
        }
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

    // linux/netlink.h and linux/rtnetlink.h: the types of the messages the watched groups deliver
    const RTM_NEWLINK: u16 = 16;
    const RTM_NEWADDR: u16 = 20;
    const RTM_DELADDR: u16 = 21;
    const RTM_NEWROUTE: u16 = 24;
    const RTM_DELROUTE: u16 = 25;

    /// One message framed as the kernel frames it: nlmsghdr (len, type, zero flags/seq/pid, native order) + payload, padded to 4.
    fn message(ty: u16, payload: &[u8]) -> Vec<u8> {
        let len = NLMSG_HDRLEN + payload.len();
        let mut m = u32::try_from(len)
            .expect("short message")
            .to_ne_bytes()
            .to_vec();
        m.extend_from_slice(&ty.to_ne_bytes());
        m.extend_from_slice(&[0; 10]);
        m.extend_from_slice(payload);
        m.resize((len + 3) & !3, 0);
        m
    }

    /// ifinfomsg (16 bytes): AF_UNSPEC, ARPHRD_ETHER (1), the index, IFF_UP | IFF_RUNNING (0x41), no change mask.
    fn link(index: i32) -> Vec<u8> {
        let mut p = vec![0u8, 0];
        p.extend_from_slice(&1u16.to_ne_bytes());
        p.extend_from_slice(&index.to_ne_bytes());
        p.extend_from_slice(&0x41u32.to_ne_bytes());
        p.extend_from_slice(&0u32.to_ne_bytes());
        p
    }

    /// ifaddrmsg (8 bytes): AF_INET, /24, scope universe, the index.
    fn ifaddr(index: u32) -> Vec<u8> {
        let mut p = vec![2u8, 24, 0, 0];
        p.extend_from_slice(&index.to_ne_bytes());
        p
    }

    /// rtmsg (12 bytes): an IPv4 unicast default route in table main (254), protocol boot (3).
    fn default_route() -> Vec<u8> {
        vec![2, 0, 0, 0, 254, 3, 0, 1, 0, 0, 0, 0]
    }

    /// A fixture line's hex digits as bytes.
    fn hex_bytes(line: &str) -> Vec<u8> {
        (0..line.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&line[i..i + 2], 16).expect("hex digit pair"))
            .collect()
    }

    #[test]
    fn a_link_event_is_a_change() {
        let mut b = Burst::default();
        b.feed(&message(RTM_NEWLINK, &link(2)));
        assert!(b.take());
        assert!(!b.take(), "a taken burst starts empty");
    }

    #[test]
    fn framing_alone_is_no_change() {
        let mut b = Burst::default();
        b.feed(&message(NLMSG_NOOP, &[]));
        b.feed(&message(NLMSG_DONE, &0i32.to_ne_bytes()));
        assert!(!b.take());
    }

    #[test]
    fn one_wake_up_is_one_change() {
        // the kernel sends each notification as its own datagram; one wake-up takes the ones queued
        let mut b = Burst::default();
        b.feed(&message(RTM_NEWADDR, &ifaddr(2)));
        b.feed(&message(RTM_NEWROUTE, &default_route()));
        b.feed(&message(RTM_DELROUTE, &default_route()));
        assert!(b.take());
        assert!(!b.take());
    }

    #[test]
    fn a_datagram_holds_several_messages_in_order() {
        let mut d = message(RTM_NEWLINK, &link(2));
        d.extend(message(RTM_NEWROUTE, &default_route()));
        assert_eq!(message_types(&d), Some(vec![RTM_NEWLINK, RTM_NEWROUTE]));
    }

    #[test]
    fn a_malformed_datagram_is_a_change() {
        // a length below the 16-byte header
        let mut short = message(RTM_NEWLINK, &link(2));
        short[..4].copy_from_slice(&8u32.to_ne_bytes());
        // a length past the datagram: a read cut short by the buffer
        let mut cut = message(RTM_NEWLINK, &link(2));
        cut.truncate(20);
        for d in [short, cut] {
            let mut b = Burst::default();
            b.feed(&d);
            assert!(b.take());
        }
    }

    #[test]
    fn an_overflow_is_a_change() {
        let mut b = Burst::default();
        b.overflow();
        assert!(b.take());
    }

    #[test]
    fn the_watch_joins_link_address_and_route_groups() {
        // RTMGRP_LINK 0x1, IPV4_IFADDR 0x10, IPV4_ROUTE 0x40, IPV6_IFADDR 0x100, IPV6_ROUTE 0x400
        assert_eq!(WATCH_GROUPS, 0x551);
    }

    #[test]
    fn recorded_capture_is_framed_and_its_events_are_changes() {
        let datagrams: Vec<Vec<u8>> = include_str!("../testdata/rtnetlink-events.hex")
            .lines()
            .filter(|l| !l.starts_with('#'))
            .map(hex_bytes)
            .collect();
        let mut types = Vec::new();
        for d in &datagrams {
            // one message per datagram: its length padded to 4 is the datagram's size (NLMSG_ALIGN)
            let len = u32::from_ne_bytes([d[0], d[1], d[2], d[3]]) as usize;
            assert_eq!((len + 3) & !3, d.len());
            types.extend(message_types(d).expect("recorded datagrams are well framed"));
        }
        assert_eq!(
            types,
            [
                RTM_NEWLINK,
                RTM_NEWADDR,
                RTM_NEWROUTE,
                RTM_NEWROUTE,
                RTM_NEWROUTE,
                RTM_NEWADDR,
                RTM_NEWROUTE,
                RTM_NEWADDR,
                RTM_NEWROUTE,
                RTM_NEWROUTE,
                RTM_NEWROUTE,
                RTM_NEWROUTE,
                RTM_NEWROUTE,
                RTM_NEWADDR,
                RTM_NEWADDR,
                RTM_NEWROUTE,
                RTM_DELROUTE,
                RTM_NEWLINK,
                RTM_DELROUTE,
                RTM_DELROUTE,
                RTM_DELROUTE,
                RTM_DELADDR,
                RTM_DELADDR,
            ]
        );
        let mut b = Burst::default();
        for d in &datagrams {
            b.feed(d);
        }
        assert!(b.take());
    }

    /// The socket path end to end. Needs a network namespace of its own where a link changes while it runs: run the built
    /// test binary as `unshare -Urn sh -c '<bin> --ignored watch_reports_a_link_change & sleep 2; ip link set lo up; wait'`.
    #[test]
    #[ignore = "needs a private network namespace with a link change during the test"]
    fn watch_reports_a_link_change() {
        let (tx, rx) = std::sync::mpsc::channel();
        HostNetwork.watch(tx).expect("the watch starts");
        rx.recv_timeout(std::time::Duration::from_secs(10))
            .expect("the link change is reported");
    }
}
