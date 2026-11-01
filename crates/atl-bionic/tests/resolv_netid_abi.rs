// SPDX-License-Identifier: MIT
//! `android_getaddrinfofornet` as libandroid_net calls it, compiled against the host libc. Expected values: bionic's contract
//! (getaddrinfo.c: `getaddrinfo` is this function with `NETID_UNSET`, `MARK_UNSET`; single network here, so every netid is that
//! network), the host's `getaddrinfo` as the oracle, and constants from glibc's `<netdb.h>` / `<sys/socket.h>` and POSIX.

use std::ffi::{CStr, c_char, c_int, c_uint, c_void};
use std::ptr::{null, null_mut};

// the exports live in the crate; nothing else of it is named here
use atl_bionic as _;

/// glibc `<netdb.h>` `struct addrinfo` (field order differs from bionic's: `ai_addr` before `ai_canonname`).
#[repr(C)]
struct AddrInfo {
    ai_flags: c_int,
    ai_family: c_int,
    ai_socktype: c_int,
    ai_protocol: c_int,
    ai_addrlen: u32,
    ai_addr: *mut c_void,
    ai_canonname: *mut c_char,
    ai_next: *mut AddrInfo,
}

// glibc <netdb.h> / <bits/socket.h> values
const AI_NUMERICHOST: c_int = 0x0004;
const AI_NUMERICSERV: c_int = 0x0400;
const AF_INET: c_int = 2;
const SOCK_STREAM: c_int = 1;
const EAI_NONAME: c_int = -2;
/// bionic resolv_netid.h
const NETID_UNSET: c_uint = 0;
const MARK_UNSET: c_uint = 0;

unsafe extern "C" {
    fn android_getaddrinfofornet(
        node: *const c_char,
        service: *const c_char,
        hints: *const AddrInfo,
        netid: c_uint,
        mark: c_uint,
        res: *mut *mut AddrInfo,
    ) -> c_int;
    fn getaddrinfo(
        node: *const c_char,
        service: *const c_char,
        hints: *const AddrInfo,
        res: *mut *mut AddrInfo,
    ) -> c_int;
    fn freeaddrinfo(res: *mut AddrInfo);
}

fn hints() -> AddrInfo {
    AddrInfo {
        ai_flags: AI_NUMERICHOST | AI_NUMERICSERV,
        ai_family: AF_INET,
        ai_socktype: SOCK_STREAM,
        ai_protocol: 0,
        ai_addrlen: 0,
        ai_addr: null_mut(),
        ai_canonname: null_mut(),
        ai_next: null_mut(),
    }
}

/// Return value and each result's sockaddr bytes, the list freed.
fn resolve(node: &CStr, service: &CStr, netid: Option<(c_uint, c_uint)>) -> (c_int, Vec<Vec<u8>>) {
    let hints = hints();
    let mut res = null_mut();
    // SAFETY: NUL-terminated strings, valid hints, an out pointer; the list is read before freeaddrinfo
    unsafe {
        let rc = match netid {
            Some((id, mark)) => android_getaddrinfofornet(
                node.as_ptr(),
                service.as_ptr(),
                &hints,
                id,
                mark,
                &mut res,
            ),
            None => getaddrinfo(node.as_ptr(), service.as_ptr(), &hints, &mut res),
        };
        let mut addrs = Vec::new();
        let mut p = res;
        while !p.is_null() {
            let ai = &*p;
            addrs.push(
                std::slice::from_raw_parts(ai.ai_addr.cast::<u8>(), ai.ai_addrlen as usize)
                    .to_vec(),
            );
            p = ai.ai_next;
        }
        if !res.is_null() {
            freeaddrinfo(res);
        }
        (rc, addrs)
    }
}

#[test]
fn numeric_address_resolves_on_every_netid() {
    // POSIX sockaddr_in: sa_family (host order), port 80 big-endian, 127.0.0.1, 8 zero bytes; 16 bytes
    let mut want = Vec::new();
    want.extend_from_slice(&(AF_INET as u16).to_ne_bytes());
    want.extend_from_slice(&80u16.to_be_bytes());
    want.extend_from_slice(&[127, 0, 0, 1]);
    want.extend_from_slice(&[0; 8]);
    for netid in [(NETID_UNSET, MARK_UNSET), (100, 0x1_0064)] {
        assert_eq!(
            resolve(c"127.0.0.1", c"80", Some(netid)),
            (0, vec![want.clone()]),
            "{netid:?}"
        );
    }
    assert_eq!(
        resolve(c"127.0.0.1", c"80", None),
        (0, vec![want]),
        "host getaddrinfo"
    );
}

#[test]
fn failure_is_the_host_result() {
    // AI_NUMERICHOST with a name: glibc EAI_NONAME, no list
    let want = (EAI_NONAME, Vec::new());
    assert_eq!(
        resolve(c"host.invalid", c"80", None),
        want,
        "host getaddrinfo"
    );
    assert_eq!(resolve(c"host.invalid", c"80", Some((7, 0))), want);
    // a null node and service: EAI_NONAME from both
    let hints = hints();
    let mut res = null_mut();
    // SAFETY: null node and service are getaddrinfo's documented failure input
    let rc = unsafe {
        android_getaddrinfofornet(null(), null(), &hints, NETID_UNSET, MARK_UNSET, &mut res)
    };
    assert_eq!(rc, EAI_NONAME);
}
