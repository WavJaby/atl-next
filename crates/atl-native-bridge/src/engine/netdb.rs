// SPDX-License-Identifier: MIT
//! Host sides of the guest resolver stubs (QEMU table, build.rs `QEMU_SHIMS`): bionic's `struct addrinfo` swaps `ai_canonname` and `ai_addr` against glibc's, and every AI_ / NI_ / EAI_ value differs (bionic netdb.h vs glibc netdb.h), so each call converts. The bionic side is generated from bionic's netdb.h (thread.rs `layout::qemu`), the glibc side is the libc crate; both gated below.
//! `struct hostent` is the same in both: `gethostbyname` is a plain glibc row (build.rs `GLIBC`), gated here.
//! `sockaddr` passes through: the kernel's layout, the same for both libcs.

use std::collections::HashSet;
use std::ffi::{c_char, c_int};
use std::mem::{offset_of, size_of};
use std::sync::Mutex;

use super::thread::layout::qemu as g;

// hostent: same layout, so the glibc row hands the guest glibc's own answer
const _: () = {
    assert!(g::hostent::SIZE == size_of::<libc::hostent>());
    assert!(g::hostent::h_name == offset_of!(libc::hostent, h_name));
    assert!(g::hostent::h_aliases == offset_of!(libc::hostent, h_aliases));
    assert!(g::hostent::h_addrtype == offset_of!(libc::hostent, h_addrtype));
    assert!(g::hostent::h_length == offset_of!(libc::hostent, h_length));
    assert!(g::hostent::h_addr_list == offset_of!(libc::hostent, h_addr_list));
};

// addrinfo: the int fields and ai_next agree; ai_canonname and ai_addr are where the copy converts
const _: () = {
    assert!(g::addrinfo::SIZE == size_of::<libc::addrinfo>());
    assert!(g::addrinfo::ai_flags == offset_of!(libc::addrinfo, ai_flags));
    assert!(g::addrinfo::ai_family == offset_of!(libc::addrinfo, ai_family));
    assert!(g::addrinfo::ai_socktype == offset_of!(libc::addrinfo, ai_socktype));
    assert!(g::addrinfo::ai_protocol == offset_of!(libc::addrinfo, ai_protocol));
    assert!(g::addrinfo::ai_addrlen == offset_of!(libc::addrinfo, ai_addrlen));
    assert!(g::addrinfo::ai_next == offset_of!(libc::addrinfo, ai_next));
};

/// Guest (bionic) flag -> host (glibc) flag, for every flag bionic's getaddrinfo accepts.
const AI: [(i32, c_int); 5] = [
    (g::AI_PASSIVE, libc::AI_PASSIVE),
    (g::AI_CANONNAME, libc::AI_CANONNAME),
    (g::AI_NUMERICHOST, libc::AI_NUMERICHOST),
    (g::AI_NUMERICSERV, libc::AI_NUMERICSERV),
    (g::AI_ADDRCONFIG, libc::AI_ADDRCONFIG),
];
// bionic rejects any flag outside AI_MASK (EAI_BADFLAGS); the table must cover exactly AI_MASK, so a flag bionic adds fails the build
const _: () = assert!(AI[0].0 | AI[1].0 | AI[2].0 | AI[3].0 | AI[4].0 == g::AI_MASK);

/// Guest flag -> host flag for getnameinfo; a guest bit outside these is EAI_BADFLAGS, as glibc answers for an unknown bit.
const NI: [(i32, c_int); 5] = [
    (g::NI_NOFQDN, libc::NI_NOFQDN),
    (g::NI_NUMERICHOST, libc::NI_NUMERICHOST),
    (g::NI_NAMEREQD, libc::NI_NAMEREQD),
    (g::NI_NUMERICSERV, libc::NI_NUMERICSERV),
    (g::NI_DGRAM, libc::NI_DGRAM),
];

/// glibc netdb.h `EAI_ADDRFAMILY` (under `__USE_GNU`), which the libc crate lacks.
const GLIBC_EAI_ADDRFAMILY: c_int = -9;

/// Host error -> guest error. A host code not here (glibc's asynchronous ones) is EAI_FAIL.
const EAI: [(c_int, i32); 12] = [
    (GLIBC_EAI_ADDRFAMILY, g::EAI_ADDRFAMILY),
    (libc::EAI_AGAIN, g::EAI_AGAIN),
    (libc::EAI_BADFLAGS, g::EAI_BADFLAGS),
    (libc::EAI_FAIL, g::EAI_FAIL),
    (libc::EAI_FAMILY, g::EAI_FAMILY),
    (libc::EAI_MEMORY, g::EAI_MEMORY),
    (libc::EAI_NODATA, g::EAI_NODATA),
    (libc::EAI_NONAME, g::EAI_NONAME),
    (libc::EAI_SERVICE, g::EAI_SERVICE),
    (libc::EAI_SOCKTYPE, g::EAI_SOCKTYPE),
    (libc::EAI_SYSTEM, g::EAI_SYSTEM),
    (libc::EAI_OVERFLOW, g::EAI_OVERFLOW),
];

/// `flags` through `table`; None when a bit has no entry.
fn map_flags(flags: i32, table: &[(i32, c_int)]) -> Option<c_int> {
    let mut rest = flags;
    let mut out = 0;
    for &(guest, host) in table {
        if flags & guest != 0 {
            out |= host;
            rest &= !guest;
        }
    }
    (rest == 0).then_some(out)
}

fn guest_error(host: c_int) -> c_int {
    if host == 0 {
        return 0;
    }
    EAI.iter()
        .find(|&&(h, _)| h == host)
        .map_or(g::EAI_FAIL, |&(_, guest)| guest)
}

/// Guest lists [`getaddrinfo`] made and [`freeaddrinfo`] has not freed: their heads.
static LISTS: Mutex<Option<HashSet<usize>>> = Mutex::new(None);

/// Where a guest node's two pointers go: break `dns_glibc_order` writes them at glibc's offsets (qemu-thread gate `r`).
const CANONNAME_AT: usize = if cfg!(atl_nb_dns_glibc_order) {
    offset_of!(libc::addrinfo, ai_canonname)
} else {
    g::addrinfo::ai_canonname
};
const ADDR_AT: usize = if cfg!(atl_nb_dns_glibc_order) {
    offset_of!(libc::addrinfo, ai_addr)
} else {
    g::addrinfo::ai_addr
};

/// One guest node of `h` (bionic layout): node, then the sockaddr, then the canonical name, in one malloc block; `ai_next` null.
///
/// # Safety
/// `h` is a node of a glibc getaddrinfo list.
unsafe fn guest_node(h: &libc::addrinfo) -> *mut u8 {
    let addr_len = if h.ai_addr.is_null() {
        0
    } else {
        h.ai_addrlen as usize
    };
    // SAFETY: a glibc canonical name is NUL-terminated.
    let name = (!h.ai_canonname.is_null())
        .then(|| unsafe { std::ffi::CStr::from_ptr(h.ai_canonname) }.to_bytes_with_nul());
    let size = g::addrinfo::SIZE + addr_len + name.map_or(0, <[u8]>::len);
    // SAFETY: plain allocation; zeroed so every field the copy skips is 0 / null.
    let p = unsafe { libc::calloc(1, size) }.cast::<u8>();
    if p.is_null() {
        return p;
    }
    // SAFETY: `p` holds `size` bytes laid out as above; int fields at the offsets gated equal, pointers at the generated ones (8-aligned in a calloc block).
    unsafe {
        p.add(g::addrinfo::ai_flags)
            .cast::<c_int>()
            .write(h.ai_flags);
        p.add(g::addrinfo::ai_family)
            .cast::<c_int>()
            .write(h.ai_family);
        p.add(g::addrinfo::ai_socktype)
            .cast::<c_int>()
            .write(h.ai_socktype);
        p.add(g::addrinfo::ai_protocol)
            .cast::<c_int>()
            .write(h.ai_protocol);
        let tail = p.add(g::addrinfo::SIZE);
        if addr_len != 0 {
            std::ptr::copy_nonoverlapping(h.ai_addr.cast::<u8>(), tail, addr_len);
            p.add(ADDR_AT).cast::<*mut u8>().write(tail);
        }
        p.add(g::addrinfo::ai_addrlen)
            .cast::<u32>()
            .write(addr_len as u32);
        if let Some(n) = name {
            let at = tail.add(addr_len);
            std::ptr::copy_nonoverlapping(n.as_ptr(), at, n.len());
            p.add(CANONNAME_AT).cast::<*mut u8>().write(at);
        }
    }
    p
}

/// Frees a guest list made by [`guest_node`]s.
///
/// # Safety
/// `head` is such a list (or null).
unsafe fn free_list(mut head: *mut u8) {
    while !head.is_null() {
        // SAFETY: caller contract; ai_next at the generated offset.
        let next = unsafe { head.add(g::addrinfo::ai_next).cast::<*mut u8>().read() };
        // SAFETY: a calloc block of guest_node.
        unsafe { libc::free(head.cast()) };
        head = next;
    }
}

/// `int getaddrinfo(const char*, const char*, const struct addrinfo*, struct addrinfo**)`: guest hints converted to glibc's, glibc's list copied into a guest-layout list ([`freeaddrinfo`] frees it), the error code converted. A hints flag outside bionic's AI_MASK is EAI_BADFLAGS, as bionic answers.
pub(super) extern "C" fn getaddrinfo(
    node: *const c_char,
    service: *const c_char,
    hints: *const u8,
    res: *mut *mut u8,
) -> c_int {
    if res.is_null() {
        return g::EAI_FAIL;
    }
    // SAFETY: zeroed is a valid empty addrinfo (bionic and glibc both read null hints fields as unset).
    let mut host_hints: libc::addrinfo = unsafe { std::mem::zeroed() };
    if !hints.is_null() {
        // SAFETY: the guest's hints, bionic layout (generated offsets); bionic reads only these four fields of hints.
        let (flags, family, socktype, protocol) = unsafe {
            (
                hints.add(g::addrinfo::ai_flags).cast::<c_int>().read(),
                hints.add(g::addrinfo::ai_family).cast::<c_int>().read(),
                hints.add(g::addrinfo::ai_socktype).cast::<c_int>().read(),
                hints.add(g::addrinfo::ai_protocol).cast::<c_int>().read(),
            )
        };
        let Some(f) = map_flags(flags, &AI) else {
            return g::EAI_BADFLAGS;
        };
        host_hints.ai_flags = f;
        host_hints.ai_family = family;
        host_hints.ai_socktype = socktype;
        host_hints.ai_protocol = protocol;
    }
    let mut host_res: *mut libc::addrinfo = std::ptr::null_mut();
    // SAFETY: node / service are the guest's C strings (or null); hints is ours or null.
    let rc = unsafe {
        libc::getaddrinfo(
            node,
            service,
            if hints.is_null() {
                std::ptr::null()
            } else {
                &host_hints
            },
            &mut host_res,
        )
    };
    if rc != 0 {
        return guest_error(rc);
    }
    let mut head: *mut u8 = std::ptr::null_mut();
    let mut tail: *mut u8 = std::ptr::null_mut();
    let mut h = host_res;
    let mut out_of_memory = false;
    while !h.is_null() {
        // SAFETY: a node of glibc's list.
        let node = unsafe { guest_node(&*h) };
        if node.is_null() {
            out_of_memory = true;
            break;
        }
        if tail.is_null() {
            head = node;
        } else {
            // SAFETY: tail is a guest node; ai_next at the generated offset.
            unsafe { tail.add(g::addrinfo::ai_next).cast::<*mut u8>().write(node) };
        }
        tail = node;
        // SAFETY: glibc's list.
        h = unsafe { (*h).ai_next };
    }
    // SAFETY: glibc's own list, freed once.
    unsafe { libc::freeaddrinfo(host_res) };
    if out_of_memory {
        // SAFETY: the nodes made so far.
        unsafe { free_list(head) };
        return g::EAI_MEMORY;
    }
    if let Ok(mut l) = LISTS.lock() {
        l.get_or_insert_with(HashSet::new).insert(head as usize);
    }
    // SAFETY: the guest's out parameter (checked non-null).
    unsafe { res.write(head) };
    0
}

/// `void freeaddrinfo(struct addrinfo*)`: frees a list [`getaddrinfo`] made. A pointer it did not hand out aborts, named: freeing it would free memory this side never allocated.
pub(super) extern "C" fn freeaddrinfo(head: *mut u8) {
    if head.is_null() {
        return;
    }
    let known = LISTS
        .lock()
        .ok()
        .and_then(|mut l| l.as_mut().map(|s| s.remove(&(head as usize))))
        .unwrap_or(false);
    if !known {
        super::unsupported(&format!(
            "guest freeaddrinfo({head:p}): not a list this getaddrinfo returned"
        ));
    }
    // SAFETY: a list of guest_node blocks, removed from LISTS above, so freed once.
    unsafe { free_list(head) };
}

/// `int getnameinfo(const struct sockaddr*, socklen_t, char*, size_t, char*, size_t, int)` (bionic: `size_t` buffer lengths; glibc: `socklen_t`, clamped): flags and error code converted.
pub(super) extern "C" fn getnameinfo(
    sa: *const libc::sockaddr,
    salen: libc::socklen_t,
    host: *mut c_char,
    hostlen: usize,
    serv: *mut c_char,
    servlen: usize,
    flags: c_int,
) -> c_int {
    let Some(f) = map_flags(flags, &NI) else {
        return g::EAI_BADFLAGS;
    };
    let clamp = |n: usize| libc::socklen_t::try_from(n).unwrap_or(libc::socklen_t::MAX);
    // SAFETY: the guest's sockaddr and buffers with their lengths (kernel sockaddr layout, same for both libcs).
    let rc = unsafe { libc::getnameinfo(sa, salen, host, clamp(hostlen), serv, clamp(servlen), f) };
    guest_error(rc)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every guest flag value of the tables is bionic's netdb.h number and every host one glibc's (both headers read for Q7-7): the generated and libc constants are the code under test, these literals the spec.
    #[test]
    fn tables_carry_the_header_values() {
        // bionic netdb.h: AI_PASSIVE 1, AI_CANONNAME 2, AI_NUMERICHOST 4, AI_NUMERICSERV 8, AI_ADDRCONFIG 0x400; glibc: 1, 2, 4, 0x400, 0x20
        assert_eq!(AI, [(1, 1), (2, 2), (4, 4), (8, 0x400), (0x400, 0x20)]);
        // bionic: NI_NOFQDN 1, NUMERICHOST 2, NAMEREQD 4, NUMERICSERV 8, DGRAM 0x10; glibc: 4, 1, 8, 2, 16
        assert_eq!(NI, [(1, 4), (2, 1), (4, 8), (8, 2), (0x10, 16)]);
        // glibc EAI_* are negative, bionic's positive (netdb.h of each)
        assert_eq!(guest_error(-2), 8, "EAI_NONAME");
        assert_eq!(guest_error(-3), 2, "EAI_AGAIN");
        assert_eq!(guest_error(0), 0);
    }

    #[test]
    fn unknown_flag_bits_are_refused() {
        // bionic AI_V4MAPPED (0x800) is outside AI_MASK: bionic answers EAI_BADFLAGS
        assert_eq!(map_flags(0x800, &AI), None);
        assert_eq!(map_flags(1 | 8, &AI), Some(1 | 0x400));
    }
}
