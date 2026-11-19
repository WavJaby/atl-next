// SPDX-License-Identifier: MIT
//! Host sides of the guest resolver stubs, both engines (build.rs `SHIMS`): bionic's `struct addrinfo` swaps `ai_canonname` and `ai_addr` against glibc's, and every AI_ / NI_ / EAI_ value differs (bionic netdb.h vs glibc netdb.h), so each call converts. The bionic side is generated from bionic's netdb.h (thread.rs `layout::qemu`; `layout::identity` asserted equal below), the glibc side is the libc crate; both gated below.
//! Two guest views, chosen by the guest's pointer width ([`view`]): LP64 (identity + arm64, one netdb.h, asserted equal) and ILP32 (arm, `layout::qemu_arm`: 4-byte pointers, same constants, asserted). Every structure handed to the guest is built in its view in memory from the window allocator (`Active::reserve`, below 4 GiB for a 32-bit guest; counted in `RESERVED`), and given back on its free path: an addrinfo list at `freeaddrinfo`, a thread's hostent at its next lookup or at thread exit, the gai_strerror text never (one block per process).
//! `gai_strerror` answers bionic's own text (build.rs `gai_errlist_rs`), no host call.
//! `struct hostent`: glibc's answer (static, host pointers) copied into the guest view per thread, as bionic keeps its own per thread (`res_static`).
//! `sockaddr` passes through: the kernel's layout, the same for both libcs.

use std::collections::HashMap;
use std::ffi::{c_char, c_int, c_void};
use std::mem::{offset_of, size_of};
use std::sync::{Mutex, OnceLock};

use super::thread::layout::{identity, qemu as g, qemu_arm as a};

// the arm view's constants are bionic's one netdb.h: equal to the LP64 view's, so one set of tables serves both
const _: () = {
    assert!(a::AI_MASK == g::AI_MASK && a::AI_CANONNAME == g::AI_CANONNAME);
    assert!(a::AI_NUMERICHOST == g::AI_NUMERICHOST && a::AI_NUMERICSERV == g::AI_NUMERICSERV);
    assert!(a::AI_PASSIVE == g::AI_PASSIVE && a::AI_ADDRCONFIG == g::AI_ADDRCONFIG);
    assert!(a::NI_NOFQDN == g::NI_NOFQDN && a::NI_NUMERICHOST == g::NI_NUMERICHOST);
    assert!(a::NI_NAMEREQD == g::NI_NAMEREQD && a::NI_NUMERICSERV == g::NI_NUMERICSERV);
    assert!(a::NI_DGRAM == g::NI_DGRAM);
    assert!(a::EAI_ADDRFAMILY == g::EAI_ADDRFAMILY && a::EAI_AGAIN == g::EAI_AGAIN);
    assert!(a::EAI_BADFLAGS == g::EAI_BADFLAGS && a::EAI_FAIL == g::EAI_FAIL);
    assert!(a::EAI_FAMILY == g::EAI_FAMILY && a::EAI_MEMORY == g::EAI_MEMORY);
    assert!(a::EAI_NODATA == g::EAI_NODATA && a::EAI_NONAME == g::EAI_NONAME);
    assert!(a::EAI_SERVICE == g::EAI_SERVICE && a::EAI_SOCKTYPE == g::EAI_SOCKTYPE);
    assert!(a::EAI_SYSTEM == g::EAI_SYSTEM && a::EAI_OVERFLOW == g::EAI_OVERFLOW);
    // the int fields lead both views at the same offsets: written through the LP64 view's
    assert!(a::addrinfo::ai_flags == g::addrinfo::ai_flags);
    assert!(a::addrinfo::ai_family == g::addrinfo::ai_family);
    assert!(a::addrinfo::ai_socktype == g::addrinfo::ai_socktype);
    assert!(a::addrinfo::ai_protocol == g::addrinfo::ai_protocol);
    assert!(a::addrinfo::ai_addrlen == g::addrinfo::ai_addrlen);
};

/// One guest view of the resolver records: the word (pointer and `size_t`) width and the pointer fields' offsets.
#[derive(Clone, Copy)]
struct View {
    word: usize,
    ai_size: usize,
    ai_canonname: usize,
    ai_addr: usize,
    ai_next: usize,
    he_size: usize,
    h_name: usize,
    h_aliases: usize,
    h_addrtype: usize,
    h_length: usize,
    h_addr_list: usize,
}

macro_rules! view {
    ($m:ident, $word:expr) => {
        View {
            word: $word,
            ai_size: $m::addrinfo::SIZE,
            // break `dns_glibc_order` writes the two pointers swapped: glibc's order (qemu-thread gate `r`)
            ai_canonname: if cfg!(atl_nb_dns_glibc_order) {
                $m::addrinfo::ai_addr
            } else {
                $m::addrinfo::ai_canonname
            },
            ai_addr: if cfg!(atl_nb_dns_glibc_order) {
                $m::addrinfo::ai_canonname
            } else {
                $m::addrinfo::ai_addr
            },
            ai_next: $m::addrinfo::ai_next,
            he_size: $m::hostent::SIZE,
            h_name: $m::hostent::h_name,
            h_aliases: $m::hostent::h_aliases,
            h_addrtype: $m::hostent::h_addrtype,
            h_length: $m::hostent::h_length,
            h_addr_list: $m::hostent::h_addr_list,
        }
    };
}

const LP64: View = view!(g, 8);
const ILP32: View = view!(a, 4);

/// The running guest's view.
fn view() -> View {
    if super::active().guest_pointer_bits() == 32 {
        ILP32
    } else {
        LP64
    }
}

/// Writes guest word `v` at `p` in view `v`'s width.
///
/// # Safety
/// `p` holds `v.word` writable bytes; `x` fits the word (window memory: below 4 GiB for a 32-bit guest).
unsafe fn write_word(v: View, p: *mut u8, x: usize) {
    // SAFETY: caller contract; unaligned writes, so no alignment assumption on the block layout.
    unsafe {
        if v.word == 4 {
            p.cast::<u32>().write_unaligned(x as u32);
        } else {
            p.cast::<u64>().write_unaligned(x as u64);
        }
    }
}

/// Window memory for one guest structure of `len` bytes (read + write for the guest); None when the window is exhausted.
fn reserve(len: usize) -> Option<usize> {
    super::active()
        .reserve(len, (libc::PROT_READ | libc::PROT_WRITE) as u32)
        .ok()
}

/// `n` rounded up to 8 (every block piece starts 8-aligned: a pointer field of either view).
const fn round8(n: usize) -> usize {
    (n + 7) & !7
}

// bionic_thread_{identity,qemu}: the identity guest is the host arch's bionic, the QEMU guest arm64's. Both LP64 and one netdb.h, so one implementation serves both engines; equal here, not assumed.
const _: () = {
    assert!(identity::addrinfo::SIZE == g::addrinfo::SIZE);
    assert!(identity::addrinfo::ai_flags == g::addrinfo::ai_flags);
    assert!(identity::addrinfo::ai_family == g::addrinfo::ai_family);
    assert!(identity::addrinfo::ai_socktype == g::addrinfo::ai_socktype);
    assert!(identity::addrinfo::ai_protocol == g::addrinfo::ai_protocol);
    assert!(identity::addrinfo::ai_addrlen == g::addrinfo::ai_addrlen);
    assert!(identity::addrinfo::ai_addr == g::addrinfo::ai_addr);
    assert!(identity::addrinfo::ai_canonname == g::addrinfo::ai_canonname);
    assert!(identity::addrinfo::ai_next == g::addrinfo::ai_next);
    assert!(identity::AI_MASK == g::AI_MASK);
    assert!(identity::AI_PASSIVE == g::AI_PASSIVE);
    assert!(identity::AI_CANONNAME == g::AI_CANONNAME);
    assert!(identity::AI_NUMERICHOST == g::AI_NUMERICHOST);
    assert!(identity::AI_NUMERICSERV == g::AI_NUMERICSERV);
    assert!(identity::AI_ADDRCONFIG == g::AI_ADDRCONFIG);
    assert!(identity::NI_NOFQDN == g::NI_NOFQDN);
    assert!(identity::NI_NUMERICHOST == g::NI_NUMERICHOST);
    assert!(identity::NI_NAMEREQD == g::NI_NAMEREQD);
    assert!(identity::NI_NUMERICSERV == g::NI_NUMERICSERV);
    assert!(identity::NI_DGRAM == g::NI_DGRAM);
    assert!(identity::EAI_ADDRFAMILY == g::EAI_ADDRFAMILY);
    assert!(identity::EAI_AGAIN == g::EAI_AGAIN);
    assert!(identity::EAI_BADFLAGS == g::EAI_BADFLAGS);
    assert!(identity::EAI_FAIL == g::EAI_FAIL);
    assert!(identity::EAI_FAMILY == g::EAI_FAMILY);
    assert!(identity::EAI_MEMORY == g::EAI_MEMORY);
    assert!(identity::EAI_NODATA == g::EAI_NODATA);
    assert!(identity::EAI_NONAME == g::EAI_NONAME);
    assert!(identity::EAI_SERVICE == g::EAI_SERVICE);
    assert!(identity::EAI_SOCKTYPE == g::EAI_SOCKTYPE);
    assert!(identity::EAI_SYSTEM == g::EAI_SYSTEM);
    assert!(identity::EAI_OVERFLOW == g::EAI_OVERFLOW);
};

// bionic's text table, indexed by its EAI_* values: one entry per value 0..=EAI_MAX (netdb.h: EAI_MAX = EAI_OVERFLOW + 1)
include!(concat!(env!("OUT_DIR"), "/gai_errlist.rs"));
const _: () = assert!(GAI_ERRLIST.len() == g::EAI_OVERFLOW as usize + 2);

// hostent: the LP64 view is glibc's layout (the copy keeps glibc's field values unchanged)
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
/// (Break `dns_eai_raw` returns the host's code unmapped.)
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
    if cfg!(atl_nb_dns_eai_raw) {
        return host;
    }
    EAI.iter()
        .find(|&&(h, _)| h == host)
        .map_or(g::EAI_FAIL, |&(_, guest)| guest)
}

/// A failed lookup's guest code, as Android answers it. A name lookup that fails on the DNS side is EAI_NODATA: EAI_NONAME (unknown host), EAI_AGAIN (no connectivity, no answer), EAI_FAIL. On a device netd makes the lookup, and every failure of its reply (DnsProxyListener.cpp GetAddrInfoHandler::run, rv) reaches bionic's proxy client, which reads the code, discards it and answers EAI_NODATA (getaddrinfo.c:455-459, :557). EAI_NONAME stays for the numeric-only flags (AI_NUMERICHOST :730, AI_NUMERICSERV :1190-1191) and for a NULL host and service (:616); other codes are [`guest_error`]'s.
fn lookup_error(host: c_int, guest_flags: i32, nameless: bool) -> c_int {
    let numeric_only = guest_flags & (g::AI_NUMERICHOST | g::AI_NUMERICSERV) != 0;
    let dns_failure = match host {
        libc::EAI_NONAME => !(nameless || numeric_only),
        libc::EAI_AGAIN | libc::EAI_FAIL => true,
        _ => false,
    };
    if dns_failure {
        g::EAI_NODATA
    } else {
        guest_error(host)
    }
}

/// Index into `GAI_ERRLIST` of bionic's text for `ecode`; out of range = EAI_MAX's entry (bionic clamps the same way).
fn gai_index(ecode: c_int) -> usize {
    let last = GAI_ERRLIST.len() - 1;
    usize::try_from(ecode).map_or(last, |e| e.min(last))
}

/// The text table copied once into window memory, one guest address per entry; None when the window refused it.
static GAI_GUEST: OnceLock<Option<Vec<usize>>> = OnceLock::new();

/// `const char* gai_strerror(int)`: bionic's text for its own code. No host call. A 32-bit guest cannot hold the address of the host static, so its answer points into a window copy of the table (made on first use, kept for the process).
pub(super) extern "C" fn gai_strerror(ecode: c_int) -> *const c_char {
    let i = gai_index(ecode);
    if view().word == 8 {
        return GAI_ERRLIST[i].as_ptr();
    }
    let table = GAI_GUEST.get_or_init(|| {
        let len: usize = GAI_ERRLIST.iter().map(|s| s.to_bytes_with_nul().len()).sum();
        let base = reserve(len)?;
        let mut at = base;
        Some(
            GAI_ERRLIST
                .iter()
                .map(|s| {
                    let b = s.to_bytes_with_nul();
                    // SAFETY: `[base, base + len)` is fresh window memory holding every entry back to back.
                    unsafe { std::ptr::copy_nonoverlapping(b.as_ptr(), at as *mut u8, b.len()) };
                    at += b.len();
                    at - b.len()
                })
                .collect(),
        )
    });
    table
        .as_ref()
        .map_or(std::ptr::null(), |t| t[i] as *const c_char)
}

/// Guest lists [`getaddrinfo`] made and [`freeaddrinfo`] has not freed: head (= its window block) -> block length.
static LISTS: Mutex<Option<HashMap<usize, usize>>> = Mutex::new(None);

/// The sockaddr and canonical name of glibc node `h`: (sockaddr bytes, name with its NUL).
///
/// # Safety
/// `h` is a node of a glibc getaddrinfo list, alive while the answer is used.
unsafe fn node_parts<'a>(h: &'a libc::addrinfo) -> (&'a [u8], &'a [u8]) {
    let addr: &[u8] = if h.ai_addr.is_null() {
        &[]
    } else {
        // SAFETY: glibc's ai_addr holds ai_addrlen bytes.
        unsafe { std::slice::from_raw_parts(h.ai_addr.cast(), h.ai_addrlen as usize) }
    };
    let name: &[u8] = if h.ai_canonname.is_null() {
        &[]
    } else {
        // SAFETY: a glibc canonical name is NUL-terminated.
        unsafe { std::ffi::CStr::from_ptr(h.ai_canonname) }.to_bytes_with_nul()
    };
    (addr, name)
}

/// Bytes of the guest copy of one node in view `v`: node, sockaddr, name, each piece 8-aligned.
fn node_len(v: View, addr: &[u8], name: &[u8]) -> usize {
    round8(v.ai_size) + round8(addr.len()) + round8(name.len())
}

/// Writes the guest copy of glibc node `h` at `p` in view `v` (pieces as [`node_len`]); `ai_next` left 0 (fresh window memory is zeroed). Answers the byte after it.
///
/// # Safety
/// `p` is window memory holding [`node_len`] writable bytes for `h`'s parts; `h` as [`node_parts`].
unsafe fn write_node(v: View, h: &libc::addrinfo, p: *mut u8) -> *mut u8 {
    // SAFETY: caller contract.
    let (addr, name) = unsafe { node_parts(h) };
    // SAFETY: `p` holds the node, then the sockaddr, then the name (node_len); int fields at offsets asserted equal in both views, pointers at the view's.
    unsafe {
        p.add(g::addrinfo::ai_flags)
            .cast::<c_int>()
            .write_unaligned(h.ai_flags);
        p.add(g::addrinfo::ai_family)
            .cast::<c_int>()
            .write_unaligned(h.ai_family);
        p.add(g::addrinfo::ai_socktype)
            .cast::<c_int>()
            .write_unaligned(h.ai_socktype);
        p.add(g::addrinfo::ai_protocol)
            .cast::<c_int>()
            .write_unaligned(h.ai_protocol);
        p.add(g::addrinfo::ai_addrlen)
            .cast::<u32>()
            .write_unaligned(addr.len() as u32);
        let at = p.add(round8(v.ai_size));
        if !addr.is_empty() {
            std::ptr::copy_nonoverlapping(addr.as_ptr(), at, addr.len());
            write_word(v, p.add(v.ai_addr), at as usize);
        }
        let at = at.add(round8(addr.len()));
        if !name.is_empty() {
            std::ptr::copy_nonoverlapping(name.as_ptr(), at, name.len());
            write_word(v, p.add(v.ai_canonname), at as usize);
        }
        at.add(round8(name.len()))
    }
}

/// glibc's list `host` copied into one window block in the guest view: its head, or None when the window refused it. Registered in [`LISTS`] for [`freeaddrinfo`].
///
/// # Safety
/// `host` is a non-empty glibc getaddrinfo list.
unsafe fn guest_list(host: *const libc::addrinfo) -> Option<usize> {
    let v = view();
    let mut len = 0;
    let mut h = host;
    while !h.is_null() {
        // SAFETY: a node of glibc's list.
        let (addr, name) = unsafe { node_parts(&*h) };
        len += node_len(v, addr, name);
        // SAFETY: as above.
        h = unsafe { (*h).ai_next };
    }
    let head = reserve(len)?;
    let mut p = head as *mut u8;
    let mut prev: *mut u8 = std::ptr::null_mut();
    let mut h = host;
    while !h.is_null() {
        if !prev.is_null() {
            // SAFETY: `prev` is the node written before, in the block.
            unsafe { write_word(v, prev.add(v.ai_next), p as usize) };
        }
        prev = p;
        // SAFETY: `p` has this node's bytes left of the block sized above.
        p = unsafe { write_node(v, &*h, p) };
        // SAFETY: glibc's list.
        h = unsafe { (*h).ai_next };
    }
    if let Ok(mut l) = LISTS.lock() {
        l.get_or_insert_with(HashMap::new).insert(head, len);
    }
    Some(head)
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
    let mut guest_flags = 0;
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
        guest_flags = flags;
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
        return lookup_error(rc, guest_flags, node.is_null() && service.is_null());
    }
    // SAFETY: glibc's list of a successful call: non-empty.
    let head = unsafe { guest_list(host_res) };
    // SAFETY: glibc's own list, freed once.
    unsafe { libc::freeaddrinfo(host_res) };
    let Some(head) = head else {
        return g::EAI_MEMORY;
    };
    // SAFETY: the guest's out parameter (checked non-null): one guest word.
    unsafe { write_word(view(), res.cast(), head) };
    0
}

/// libandroid net.c `kHandleMagic`: the low 32 bits of every network handle but NETWORK_UNSPECIFIED (0); the netid is the high 32.
const HANDLE_MAGIC: u64 = 0xcafe_d00d;

/// libandroid `int android_getaddrinfofornetwork(net_handle_t, const char*, const char*, const struct addrinfo*, struct addrinfo**)` (build.rs `PROXY_SHIMS`): the host libandroid would answer glibc's list, so it converts as [`getaddrinfo`]. A handle net.c `getnetidfromhandle` refuses answers EAI_SYSTEM with the guest's errno EINVAL, as there. A valid netid selects nothing: the host has one resolver configuration (behaviour decision 3).
pub(super) extern "C" fn getaddrinfo_for_network(
    network: u64,
    node: *const c_char,
    service: *const c_char,
    hints: *const u8,
    res: *mut *mut u8,
) -> c_int {
    if network != 0 && network & 0xffff_ffff != HANDLE_MAGIC {
        // net.c `getnetidfromhandle` false: `errno = EINVAL; return EAI_SYSTEM;` (bionic EINVAL = Linux's 22)
        super::thread::set_guest_errno(libc::EINVAL);
        return g::EAI_SYSTEM;
    }
    getaddrinfo(node, service, hints, res)
}

/// `void freeaddrinfo(struct addrinfo*)`: frees a list [`getaddrinfo`] made. A pointer it did not hand out aborts, named: freeing it would free memory this side never allocated.
pub(super) extern "C" fn freeaddrinfo(head: *mut u8) {
    if head.is_null() {
        return;
    }
    let len = LISTS
        .lock()
        .ok()
        .and_then(|mut l| l.as_mut().and_then(|s| s.remove(&(head as usize))));
    let Some(len) = len else {
        super::unsupported(&format!(
            "guest freeaddrinfo({head:p}): not a list this getaddrinfo returned"
        ))
    };
    // break `dns_keep_list` (qemu-main gate `n`): the block stays reserved, so the window count does not return
    if !cfg!(atl_nb_dns_keep_list) {
        // a list's one block, removed from LISTS above, so given back once
        super::active().release(head as usize, len);
    }
}

thread_local! {
    /// This thread's guest hostent block (address, length), replaced by its next lookup, given back at thread exit.
    static HOSTENT: HostentBlock = const { HostentBlock(std::cell::Cell::new((0, 0))) };
}

struct HostentBlock(std::cell::Cell<(usize, usize)>);

impl HostentBlock {
    fn replace(&self, block: (usize, usize)) {
        let (at, len) = self.0.replace(block);
        if at != 0 {
            super::active().release(at, len);
        }
    }
}

impl Drop for HostentBlock {
    fn drop(&mut self) {
        self.replace((0, 0));
    }
}

/// glibc's answer `h` copied into a window block in the guest view: the struct, then the alias pointer array, the address pointer array, the name, alias strings and addresses. Null for null (h_errno stays glibc's: `__get_h_errno` row) or a refused window.
///
/// # Safety
/// `h` is null or glibc's hostent answer, alive until the copy returns.
unsafe fn guest_hostent(h: *const libc::hostent) -> *mut c_void {
    if h.is_null() {
        return std::ptr::null_mut();
    }
    let v = view();
    // SAFETY: caller contract.
    let h = unsafe { &*h };
    let cstr = |p: *const c_char| -> &[u8] {
        // SAFETY: glibc's strings are NUL-terminated.
        unsafe { std::ffi::CStr::from_ptr(p) }.to_bytes_with_nul()
    };
    let list = |p: *mut *mut c_char| -> Vec<*mut c_char> {
        let mut out = Vec::new();
        if !p.is_null() {
            // SAFETY: glibc's NULL-terminated pointer arrays.
            unsafe {
                let mut q = p;
                while !(*q).is_null() {
                    out.push(*q);
                    q = q.add(1);
                }
            }
        }
        out
    };
    let name: &[u8] = if h.h_name.is_null() { &[] } else { cstr(h.h_name) };
    let aliases = list(h.h_aliases);
    let addrs = list(h.h_addr_list);
    let addr_len = usize::try_from(h.h_length).unwrap_or(0);
    let alias_bytes: usize = aliases.iter().map(|&s| round8(cstr(s).len())).sum();
    let len = round8(v.he_size)
        + round8((aliases.len() + 1) * v.word)
        + round8((addrs.len() + 1) * v.word)
        + round8(name.len())
        + alias_bytes
        + addrs.len() * round8(addr_len);
    let Some(base) = reserve(len) else {
        return std::ptr::null_mut();
    };
    let p = base as *mut u8;
    // SAFETY: `[base, base + len)` is fresh zeroed window memory laid out as summed above.
    unsafe {
        p.add(v.h_addrtype).cast::<c_int>().write_unaligned(h.h_addrtype);
        p.add(v.h_length).cast::<c_int>().write_unaligned(h.h_length);
        let alias_arr = p.add(round8(v.he_size));
        let addr_arr = alias_arr.add(round8((aliases.len() + 1) * v.word));
        let mut at = addr_arr.add(round8((addrs.len() + 1) * v.word));
        if !h.h_name.is_null() {
            std::ptr::copy_nonoverlapping(name.as_ptr(), at, name.len());
            write_word(v, p.add(v.h_name), at as usize);
            at = at.add(round8(name.len()));
        }
        write_word(v, p.add(v.h_aliases), alias_arr as usize);
        for (i, &s) in aliases.iter().enumerate() {
            let b = cstr(s);
            std::ptr::copy_nonoverlapping(b.as_ptr(), at, b.len());
            write_word(v, alias_arr.add(i * v.word), at as usize);
            at = at.add(round8(b.len()));
        }
        write_word(v, p.add(v.h_addr_list), addr_arr as usize);
        for (i, &s) in addrs.iter().enumerate() {
            std::ptr::copy_nonoverlapping(s.cast::<u8>(), at, addr_len);
            write_word(v, addr_arr.add(i * v.word), at as usize);
            at = at.add(round8(addr_len));
        }
    }
    HOSTENT.with(|b| b.replace((base, len)));
    p.cast()
}

unsafe extern "C" {
    /// glibc netdb.h (the libc crate lacks both)
    fn gethostbyname(name: *const c_char) -> *mut libc::hostent;
    fn gethostbyaddr(addr: *const c_void, len: libc::socklen_t, kind: c_int) -> *mut libc::hostent;
}

/// `struct hostent* gethostbyname(const char*)`: glibc's answer copied into this thread's guest block ([`guest_hostent`]).
pub(super) extern "C" fn gethostbyname_shim(name: *const c_char) -> *mut c_void {
    // SAFETY: the guest's C string; glibc's static answer copied before this thread's next lookup.
    unsafe { guest_hostent(gethostbyname(name)) }
}

/// `struct hostent* gethostbyaddr(const void*, socklen_t, int)`: as [`gethostbyname_shim`]. bionic arm's `socklen_t` is `int32_t`, glibc's `uint32_t`: the low 32 bits of the lane, as both read them.
pub(super) extern "C" fn gethostbyaddr_shim(addr: *const c_void, len: usize, kind: c_int) -> *mut c_void {
    // SAFETY: the guest's address of `len` bytes; answer as above.
    unsafe { guest_hostent(gethostbyaddr(addr, len as u32, kind)) }
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

    #[test]
    fn lookup_failures_answer_bionic_codes() {
        // bionic netdb.h values (spec literals, getaddrinfo.c:455-459 and :557 for the proxy's EAI_NODATA): EAI_FAIL 4, EAI_NODATA 7, EAI_NONAME 8, EAI_SERVICE 9; guest AI_NUMERICHOST 4, AI_NUMERICSERV 8
        assert_eq!(
            lookup_error(libc::EAI_NONAME, 0, false),
            7,
            "host not found"
        );
        assert_eq!(
            lookup_error(libc::EAI_NONAME, 4, false),
            8,
            "AI_NUMERICHOST"
        );
        assert_eq!(
            lookup_error(libc::EAI_NONAME, 8, false),
            8,
            "AI_NUMERICSERV"
        );
        assert_eq!(
            lookup_error(libc::EAI_NONAME, 0, true),
            8,
            "NULL host and service (:616)"
        );
        assert_eq!(
            lookup_error(libc::EAI_AGAIN, 0, false),
            7,
            "no connectivity: the device's EAI_NODATA"
        );
        assert_eq!(
            lookup_error(libc::EAI_FAIL, 0, false),
            7,
            "DNS failure: the device's EAI_NODATA"
        );
        assert_eq!(
            lookup_error(libc::EAI_AGAIN, 4, false),
            7,
            "numeric flags do not keep a DNS failure"
        );
        assert_eq!(guest_error(libc::EAI_SERVICE), 9, "EAI_SERVICE");
    }

    #[test]
    fn gai_strerror_is_bionic_text() {
        // bionic getaddrinfo.c ai_errlist, spec literals; out of range answers EAI_MAX's text
        let text = |c: c_int| {
            GAI_ERRLIST[gai_index(c)].to_str().expect("ASCII text")
        };
        assert_eq!(text(0), "Success");
        assert_eq!(text(7), "No address associated with hostname");
        assert_eq!(text(8), "hostname nor servname provided, or not known");
        assert_eq!(text(15), "Unknown error");
        assert_eq!(text(-1), "Unknown error");
        assert_eq!(text(99), "Unknown error");
    }
}
