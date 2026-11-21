// SPDX-License-Identifier: MIT
//! libselinux's kernel half, answered by ATL in the kernel's place (service-access plan D3; providers.toml row `libselinux`, `srcs`): the six libselinux files that read selinuxfs or procattr are
//! replaced by this crate, the rest of libselinux (avc, checkAccess, labels, android.c) is AOSP's, unpatched. What each replaced file's functions answer here:
//! - `enabled.c`, `getenforce.c`, `sestatus.c`: SELinux enabled, MLS, enforcing; a status page that never changes (no policy reload, no setenforce); unknown classes / permissions denied.
//! - `compute_av.c`: the access vector from the service access table (atl-sepolicy, `atl_android_paths::SERVICE_ACCESS`), derived at build time from the compiled pinned policy.
//!   A source or target the table lacks gets nothing allowed; the avc's own denial line (`avc:  denied ...`) is its count.
//! - `stringrep.c`: class and permission values of the table's classes (`atl_sepolicy::CLASSES`): class = 1-based position, permission = bit of its position, which is how selinuxfs numbers them.
//!   Both ends of a value are this crate, so no kernel numbering has to match. `selinux_set_mapping` is not honoured (mapping.c is libselinux-internal); no ATL consumer calls it.
//! - `procattr.c`: the process's own context from the file the broker places in every sandbox (`atl_android_paths::SELINUX_CONTEXT`); another process's context is not readable (EACCES, as
//!   for a domain without `process getattr` on it); the exec / fscreate / sockcreate / keycreate attributes read unset, and setting any context is refused (EACCES: no table row grants a
//!   `process` permission, so no domain may set one).
//!
//! Boundary crate (C ABI exports in [`exports`]): every export's signature is checked at compile time against libselinux's own declaration (build.rs bindings); the logic is the safe code here.
#![deny(unsafe_code)]

use std::ffi::{CStr, CString, c_int};
use std::sync::OnceLock;

use atl_sepolicy::{CLASSES, Table};

/// libselinux's declarations of the replaced functions, `struct av_decision` and the id types (build.rs).
#[allow(
    non_camel_case_types,
    non_upper_case_globals,
    dead_code,
    unsafe_code,
    clippy::upper_case_acronyms
)]
mod sys {
    include!(concat!(env!("OUT_DIR"), "/selinux.rs"));
}

/// A class value as `string_to_security_class` returns it; 0 = no such class (selinuxfs `class/<name>/index` never reads 0).
type Class = u16;
/// A permission set of one class, one bit per permission.
type Vector = u32;

/// The table's class and permission names as C strings, for the functions that hand out `const char *`: made once, never freed.
struct Names {
    classes: Vec<(CString, Vec<CString>)>,
}

fn names() -> &'static Names {
    static NAMES: OnceLock<Names> = OnceLock::new();
    NAMES.get_or_init(|| Names {
        classes: CLASSES
            .iter()
            .map(|(c, perms)| {
                let c = CString::new(*c).expect("class names hold no NUL");
                let p = perms
                    .iter()
                    .map(|p| CString::new(*p).expect("permission names hold no NUL"))
                    .collect();
                (c, p)
            })
            .collect(),
    })
}

/// `name`'s class value, 0 when the table has no such class.
fn class_value(name: &[u8]) -> Class {
    CLASSES
        .iter()
        .position(|(c, _)| c.as_bytes() == name)
        .map_or(0, |i| {
            Class::try_from(i + 1).expect("CLASSES fits a class value")
        })
}

/// The permission names of class `class`, None for no such class.
fn class_perms(class: Class) -> Option<(&'static str, &'static [&'static str])> {
    let i = usize::from(class).checked_sub(1)?;
    CLASSES.get(i).copied()
}

/// The bit of permission `name` of `class`, 0 when either is unknown.
fn perm_bit(class: Class, name: &[u8]) -> Vector {
    class_perms(class)
        .and_then(|(_, perms)| perms.iter().position(|p| p.as_bytes() == name))
        .map_or(0, |i| 1 << i)
}

/// The name of the lowest permission bit set in `av` of `class` (libselinux scans from bit 0), None for none.
fn perm_name(class: Class, av: Vector) -> Option<&'static CStr> {
    let i = usize::from(class).checked_sub(1)?;
    let perms = &names().classes.get(i)?.1;
    (0..perms.len())
        .find(|b| av & (1 << b) != 0)
        .map(|b| perms[b].as_c_str())
}

fn class_name(class: Class) -> Option<&'static CStr> {
    let i = usize::from(class).checked_sub(1)?;
    names().classes.get(i).map(|(c, _)| c.as_c_str())
}

/// stringrep.c `security_av_string`: `null` for no permission, else `{ p1 p2 }` (unknown bits left out).
fn av_string(class: Class, av: Vector) -> String {
    if av == 0 {
        return "null".to_owned();
    }
    let mut s = "{ ".to_owned();
    for b in (0..Vector::BITS).filter(|b| av & (1 << b) != 0) {
        if let Some(n) = perm_name(class, 1 << b) {
            s += &n.to_string_lossy();
            s.push(' ');
        }
    }
    s.push('}');
    s
}

/// stringrep.c `print_access_vector`: ` null`, or ` { p1 p2 }` with the unnamed rest as ` 0x<hex>`.
fn av_print(class: Class, mut av: Vector) -> String {
    if av == 0 {
        return " null".to_owned();
    }
    let mut s = " {".to_owned();
    for b in 0..Vector::BITS {
        let bit = 1 << b;
        if av & bit != 0 {
            let Some(n) = perm_name(class, bit) else {
                break;
            };
            s += &format!(" {}", n.to_string_lossy());
            av &= !bit;
        }
    }
    if av != 0 {
        s += &format!(" 0x{av:x}");
    }
    s += " }";
    s
}

/// The service access table, read once; None when the image lacks it or it does not parse (every query then fails ENOENT: nothing is allowed by accident).
fn table() -> Option<&'static Table> {
    static TABLE: OnceLock<Option<Table>> = OnceLock::new();
    TABLE
        .get_or_init(|| {
            let text = std::fs::read_to_string(atl_android_paths::SERVICE_ACCESS).ok()?;
            Table::parse(&text).ok()
        })
        .as_ref()
}

/// The process's own context, read once from the broker's file: its first line, without the line end.
fn own_context() -> Option<&'static CStr> {
    static OWN: OnceLock<Option<CString>> = OnceLock::new();
    OWN.get_or_init(|| {
        let text = std::fs::read_to_string(atl_android_paths::SELINUX_CONTEXT).ok()?;
        own_context_of(&text)
    })
    .as_deref()
}

/// The context a context file holds: its first line, None when that is empty or holds a NUL.
fn own_context_of(text: &str) -> Option<CString> {
    let line = text.lines().next()?.trim_end();
    (!line.is_empty()).then(|| CString::new(line).ok())?
}

/// What `security_compute_av_flags_raw` answers: the allowed vector of `class` from `scon` to `tcon`; Err(errno).
fn allowed(table: Option<&Table>, scon: &str, tcon: &str, class: Class) -> Result<Vector, c_int> {
    let (name, perms) = class_perms(class).ok_or(libc::EINVAL)?;
    let table = table.ok_or(libc::ENOENT)?;
    Ok(perms
        .iter()
        .enumerate()
        .filter(|(_, p)| table.check(scon, tcon, name, p).allowed())
        .fold(0, |v, (i, _)| v | 1 << i))
}

/// The C side: each function of the six replaced files, its signature checked against libselinux's declaration.
#[allow(unsafe_code)]
pub mod exports {
    use std::ffi::{CStr, c_char, c_int};

    use super::set_errno;
    use super::sys::{access_vector_t, av_decision, mode_t, pid_t, security_class_t};

    /// Defines each C export and asserts its type equals the header's declaration (a mismatch fails to compile).
    macro_rules! export {
        ($($(#[$doc:meta])* fn $name:ident($($arg:ident: $ty:ty),*) -> $ret:ty $body:block)*) => {
            $(
                $(#[$doc])*
                ///
                /// # Safety
                /// C callers only: every pointer argument as `selinux/avc.h` / `selinux.h` declares it for this function (valid, NUL-terminated strings, writable out-parameters).
                // SAFETY: the symbol is libselinux's (its defining file is not built: providers.toml `srcs`), nothing else in the process defines it; callers pass what the declaration says.
                #[unsafe(no_mangle)]
                pub unsafe extern "C" fn $name($($arg: $ty),*) -> $ret $body
                const _: unsafe extern "C" fn($($ty),*) -> $ret = super::sys::$name;
            )*
        };
    }

    /// A C string argument as a Rust `&CStr`, None for NULL.
    ///
    /// # Safety
    /// `p` is NULL or a NUL-terminated string that outlives the call.
    unsafe fn arg<'a>(p: *const c_char) -> Option<&'a CStr> {
        // SAFETY: the caller's contract above.
        (!p.is_null()).then(|| unsafe { CStr::from_ptr(p) })
    }

    /// `*out = strdup(s)`: the caller frees with freecon (= free). -1 with ENOMEM when malloc fails.
    ///
    /// # Safety
    /// `out` is a valid `char **`.
    unsafe fn give(out: *mut *mut c_char, s: Option<&CStr>) -> c_int {
        if out.is_null() {
            set_errno(libc::EINVAL);
            return -1;
        }
        let p = match s {
            // SAFETY: s is a NUL-terminated string.
            Some(s) => unsafe { libc::strdup(s.as_ptr()) },
            None => std::ptr::null_mut(),
        };
        if s.is_some() && p.is_null() {
            set_errno(libc::ENOMEM);
            return -1;
        }
        // SAFETY: out is a valid char ** (caller contract).
        unsafe { *out = p };
        0
    }

    /// The own context into `out`; -1 with ENOENT when the sandbox has no context file (a process the broker did not start).
    ///
    /// # Safety
    /// `out` is a valid `char **`.
    unsafe fn own(out: *mut *mut c_char) -> c_int {
        match super::own_context() {
            // SAFETY: forwarded caller contract.
            Some(c) => unsafe { give(out, Some(c)) },
            None => {
                set_errno(libc::ENOENT);
                -1
            }
        }
    }

    /// Setting a process attribute: clearing (NULL) succeeds, any context is refused (EACCES).
    fn set_attr(c: *const c_char) -> c_int {
        if c.is_null() {
            0
        } else {
            set_errno(libc::EACCES);
            -1
        }
    }

    /// The access vector query of compute_av.c.
    ///
    /// # Safety
    /// `scon`, `tcon` NUL-terminated or NULL; `avd` NULL or a valid `struct av_decision *`.
    unsafe fn compute(
        scon: *const c_char,
        tcon: *const c_char,
        tclass: security_class_t,
        avd: *mut av_decision,
        flags: bool,
    ) -> c_int {
        // SAFETY: forwarded caller contract.
        let (Some(s), Some(t)) = (unsafe { arg(scon) }, unsafe { arg(tcon) }) else {
            set_errno(libc::EINVAL);
            return -1;
        };
        if avd.is_null() {
            set_errno(libc::EINVAL);
            return -1;
        }
        let (Ok(s), Ok(t)) = (s.to_str(), t.to_str()) else {
            set_errno(libc::EINVAL);
            return -1;
        };
        match super::allowed(super::table(), s, t, tclass) {
            Ok(allowed) => {
                // SAFETY: avd is a valid struct av_decision * (checked non-NULL above). Every permission decided, every denial audited, no allow audited, no reload (seqno 0),
                // enforcing (no SELINUX_AVD_FLAGS_PERMISSIVE); the non-flags variants leave `flags` untouched (compute_av.c: binary compatibility).
                unsafe {
                    (*avd).allowed = allowed;
                    (*avd).decided = access_vector_t::MAX;
                    (*avd).auditallow = 0;
                    (*avd).auditdeny = access_vector_t::MAX;
                    (*avd).seqno = 0;
                    if flags {
                        (*avd).flags = 0;
                    }
                }
                0
            }
            Err(e) => {
                set_errno(e);
                -1
            }
        }
    }

    export! {
        /// enabled.c: ATL is the kernel, and it enforces.
        fn is_selinux_enabled() -> c_int {
            1
        }
        /// enabled.c: AOSP's policy is an MLS policy (`private/mls`).
        fn is_selinux_mls_enabled() -> c_int {
            1
        }
        /// getenforce.c: enforcing.
        fn security_getenforce() -> c_int {
            1
        }
        /// sestatus.c: the status page is always there (the kernel's mapped page: 0, never the netlink fallback's 1).
        fn selinux_status_open(_fallback: c_int) -> c_int {
            0
        }
        fn selinux_status_close() -> () {}
        /// sestatus.c: no setenforce, no policy reload.
        fn selinux_status_updated() -> c_int {
            0
        }
        fn selinux_status_getenforce() -> c_int {
            1
        }
        fn selinux_status_policyload() -> c_int {
            0
        }
        /// sestatus.c: unknown classes and permissions are denied (deny_unknown.c, still upstream, answers the same through its missing selinuxfs file).
        fn selinux_status_deny_unknown() -> c_int {
            1
        }
        fn security_compute_av_flags_raw(scon: *const c_char, tcon: *const c_char, tclass: security_class_t, _requested: access_vector_t, avd: *mut av_decision) -> c_int {
            // SAFETY: forwarded caller contract.
            unsafe { compute(scon, tcon, tclass, avd, true) }
        }
        fn security_compute_av_raw(scon: *const c_char, tcon: *const c_char, tclass: security_class_t, _requested: access_vector_t, avd: *mut av_decision) -> c_int {
            // SAFETY: forwarded caller contract.
            unsafe { compute(scon, tcon, tclass, avd, false) }
        }
        /// No context translation on Android (no mcstrans): the raw query.
        fn security_compute_av_flags(scon: *const c_char, tcon: *const c_char, tclass: security_class_t, _requested: access_vector_t, avd: *mut av_decision) -> c_int {
            // SAFETY: forwarded caller contract.
            unsafe { compute(scon, tcon, tclass, avd, true) }
        }
        fn security_compute_av(scon: *const c_char, tcon: *const c_char, tclass: security_class_t, _requested: access_vector_t, avd: *mut av_decision) -> c_int {
            // SAFETY: forwarded caller contract.
            unsafe { compute(scon, tcon, tclass, avd, false) }
        }
        fn string_to_security_class(s: *const c_char) -> security_class_t {
            // SAFETY: s NUL-terminated or NULL (caller contract).
            let c = unsafe { arg(s) }.map_or(0, |s| super::class_value(s.to_bytes()));
            if c == 0 {
                set_errno(libc::EINVAL);
            }
            c
        }
        /// stringrep.c: the file class of a mode; none of them is a class the table holds, so 0 with EINVAL, as for an unknown mode.
        fn mode_to_security_class(m: mode_t) -> security_class_t {
            let name: &[u8] = match m & libc::S_IFMT {
                libc::S_IFREG => b"file",
                libc::S_IFDIR => b"dir",
                libc::S_IFCHR => b"chr_file",
                libc::S_IFBLK => b"blk_file",
                libc::S_IFIFO => b"fifo_file",
                libc::S_IFLNK => b"lnk_file",
                libc::S_IFSOCK => b"sock_file",
                _ => b"",
            };
            let c = super::class_value(name);
            if c == 0 {
                set_errno(libc::EINVAL);
            }
            c
        }
        fn string_to_av_perm(tclass: security_class_t, s: *const c_char) -> access_vector_t {
            // SAFETY: s NUL-terminated or NULL (caller contract).
            let v = unsafe { arg(s) }.map_or(0, |s| super::perm_bit(tclass, s.to_bytes()));
            if v == 0 {
                set_errno(libc::EINVAL);
            }
            v
        }
        fn security_class_to_string(tclass: security_class_t) -> *const c_char {
            super::class_name(tclass).map_or(std::ptr::null(), CStr::as_ptr)
        }
        fn security_av_perm_to_string(tclass: security_class_t, av: access_vector_t) -> *const c_char {
            super::perm_name(tclass, av).map_or(std::ptr::null(), CStr::as_ptr)
        }
        fn security_av_string(tclass: security_class_t, av: access_vector_t, res: *mut *mut c_char) -> c_int {
            let s = std::ffi::CString::new(super::av_string(tclass, av)).expect("permission names hold no NUL");
            // SAFETY: res is a valid char ** (caller contract).
            unsafe { give(res, Some(&s)) }
        }
        fn print_access_vector(tclass: security_class_t, av: access_vector_t) -> () {
            use std::io::Write;
            // printf's stdout, unflushed as there
            let _ = std::io::stdout().write_all(super::av_print(tclass, av).as_bytes());
        }
        /// stringrep.c drops its selinuxfs class cache; the table's classes never change.
        fn selinux_flush_class_cache() -> () {}
        /// procattr.c: the own context (the broker's file).
        fn getcon(c: *mut *mut c_char) -> c_int {
            // SAFETY: forwarded caller contract.
            unsafe { own(c) }
        }
        fn getcon_raw(c: *mut *mut c_char) -> c_int {
            // SAFETY: forwarded caller contract.
            unsafe { own(c) }
        }
        /// procattr.c `prev`: the context before the last exec; the process was started in its context, never transitioned: the same.
        fn getprevcon(c: *mut *mut c_char) -> c_int {
            // SAFETY: forwarded caller contract.
            unsafe { own(c) }
        }
        fn getprevcon_raw(c: *mut *mut c_char) -> c_int {
            // SAFETY: forwarded caller contract.
            unsafe { own(c) }
        }
        fn setcon(c: *const c_char) -> c_int {
            set_attr(c)
        }
        fn setcon_raw(c: *const c_char) -> c_int {
            set_attr(c)
        }
        fn getexeccon(c: *mut *mut c_char) -> c_int {
            // SAFETY: forwarded caller contract.
            unsafe { give(c, None) }
        }
        fn getexeccon_raw(c: *mut *mut c_char) -> c_int {
            // SAFETY: forwarded caller contract.
            unsafe { give(c, None) }
        }
        fn setexeccon(c: *const c_char) -> c_int {
            set_attr(c)
        }
        fn setexeccon_raw(c: *const c_char) -> c_int {
            set_attr(c)
        }
        fn getfscreatecon(c: *mut *mut c_char) -> c_int {
            // SAFETY: forwarded caller contract.
            unsafe { give(c, None) }
        }
        fn getfscreatecon_raw(c: *mut *mut c_char) -> c_int {
            // SAFETY: forwarded caller contract.
            unsafe { give(c, None) }
        }
        fn setfscreatecon(c: *const c_char) -> c_int {
            set_attr(c)
        }
        fn setfscreatecon_raw(c: *const c_char) -> c_int {
            set_attr(c)
        }
        fn getsockcreatecon(c: *mut *mut c_char) -> c_int {
            // SAFETY: forwarded caller contract.
            unsafe { give(c, None) }
        }
        fn getsockcreatecon_raw(c: *mut *mut c_char) -> c_int {
            // SAFETY: forwarded caller contract.
            unsafe { give(c, None) }
        }
        fn setsockcreatecon(c: *const c_char) -> c_int {
            set_attr(c)
        }
        fn setsockcreatecon_raw(c: *const c_char) -> c_int {
            set_attr(c)
        }
        fn getkeycreatecon(c: *mut *mut c_char) -> c_int {
            // SAFETY: forwarded caller contract.
            unsafe { give(c, None) }
        }
        fn getkeycreatecon_raw(c: *mut *mut c_char) -> c_int {
            // SAFETY: forwarded caller contract.
            unsafe { give(c, None) }
        }
        fn setkeycreatecon(c: *const c_char) -> c_int {
            set_attr(c)
        }
        fn setkeycreatecon_raw(c: *const c_char) -> c_int {
            set_attr(c)
        }
        /// procattr.c: pid <= 0 is EINVAL; the own pid answers the own context; any other process is not readable from a sandbox (EACCES).
        fn getpidcon(pid: pid_t, c: *mut *mut c_char) -> c_int {
            // SAFETY: forwarded caller contract.
            unsafe { pid_context(pid, c) }
        }
        fn getpidcon_raw(pid: pid_t, c: *mut *mut c_char) -> c_int {
            // SAFETY: forwarded caller contract.
            unsafe { pid_context(pid, c) }
        }
        fn getpidprevcon(pid: pid_t, c: *mut *mut c_char) -> c_int {
            // SAFETY: forwarded caller contract.
            unsafe { pid_context(pid, c) }
        }
        fn getpidprevcon_raw(pid: pid_t, c: *mut *mut c_char) -> c_int {
            // SAFETY: forwarded caller contract.
            unsafe { pid_context(pid, c) }
        }
    }

    /// # Safety
    /// `c` is a valid `char **`.
    unsafe fn pid_context(pid: pid_t, c: *mut *mut c_char) -> c_int {
        if pid <= 0 {
            set_errno(libc::EINVAL);
            -1
        } else if pid == std::process::id() as pid_t {
            // SAFETY: forwarded caller contract.
            unsafe { own(c) }
        } else {
            set_errno(libc::EACCES);
            -1
        }
    }
}

/// Sets the calling thread's errno, as the replaced C functions report failure.
#[allow(unsafe_code)]
fn set_errno(e: c_int) {
    // SAFETY: __errno_location returns the calling thread's errno slot, valid for the thread's lifetime.
    unsafe { *libc::__errno_location() = e };
}

#[cfg(test)]
mod tests {
    use super::*;

    /// private/access_vectors at the pin: `class service_manager { add find list }`; selinuxfs numbers permissions from bit 0 in declaration order.
    #[test]
    fn values_number_as_selinuxfs() {
        assert_eq!(class_value(b"service_manager"), 1);
        assert_eq!(class_value(b"file"), 0);
        assert_eq!(perm_bit(1, b"add"), 0b001);
        assert_eq!(perm_bit(1, b"find"), 0b010);
        assert_eq!(perm_bit(1, b"list"), 0b100);
        assert_eq!(perm_bit(1, b"call"), 0);
        assert_eq!(perm_bit(0, b"find"), 0);
        assert_eq!(perm_name(1, 0b110), Some(c"find"));
        assert_eq!(class_name(1), Some(c"service_manager"));
        assert_eq!(class_name(2), None);
    }

    /// stringrep.c's formats: `null` / `{ a b }` (security_av_string), ` null` / ` { a b }` / trailing ` 0x<rest>` (print_access_vector).
    #[test]
    fn strings_format_as_stringrep() {
        assert_eq!(av_string(1, 0), "null");
        assert_eq!(av_string(1, 0b011), "{ add find }");
        assert_eq!(av_print(1, 0), " null");
        assert_eq!(av_print(1, 0b101), " { add list }");
        assert_eq!(av_print(1, 0b1010), " { find 0x8 }");
    }

    #[test]
    fn allowed_vector_from_the_table() {
        let mut t = Table::new(
            ["untrusted_app".into()].into(),
            ["activity_service".into(), "hal_keymint_service".into()].into(),
        );
        t.grant(
            "untrusted_app",
            "activity_service",
            "service_manager",
            "find",
        );
        let app = "u:r:untrusted_app:s0:c123,c256,c512,c768";
        let q = |tcon, class| allowed(Some(&t), app, tcon, class);
        assert_eq!(q("u:object_r:activity_service:s0", 1), Ok(0b010));
        assert_eq!(q("u:object_r:hal_keymint_service:s0", 1), Ok(0));
        // a type or domain the table lacks: nothing allowed
        assert_eq!(q("u:object_r:no_such_service:s0", 1), Ok(0));
        assert_eq!(
            allowed(
                Some(&t),
                "u:r:no_such_app:s0",
                "u:object_r:activity_service:s0",
                1
            ),
            Ok(0)
        );
        assert_eq!(q("u:object_r:activity_service:s0", 2), Err(libc::EINVAL));
        assert_eq!(
            allowed(None, app, "u:object_r:activity_service:s0", 1),
            Err(libc::ENOENT)
        );
    }

    #[test]
    fn context_file_first_line() {
        assert_eq!(
            own_context_of("u:r:servicemanager:s0\n").as_deref(),
            Some(c"u:r:servicemanager:s0")
        );
        assert_eq!(own_context_of("\n"), None);
        assert_eq!(own_context_of(""), None);
    }
}
