// SPDX-License-Identifier: MIT
//! `eglGetProcAddress` (GS2.2 G1): the guest's name in, the address the guest calls that name by out.
//! A name with a row answers the row's exported entry, the address the guest's own import of that name resolves to (vdso.rs `intercept_symbol` records it), so a lookup and an import agree. The exported entry jumps to the row's stub, which is bound to the same host function as the row.
//! A name with no row goes to the host's libEGL: NULL stays NULL; a host answer is bound by its prototype descriptor (`bindings::prototypes`), once per host address; a host answer with no descriptor, or whose parameters the QEMU crossing refuses, gets a named trap (P-a: not NULL, never the raw host pointer).
//! Vulkan's `vkGet*ProcAddr` would need the same route through its own libvulkan rows (not built, G9).

use std::collections::BTreeMap;
use std::ffi::{CStr, CString, c_char, c_void};
use std::sync::{Mutex, PoisonError};

use super::bindings;
use super::objects::Class;
use super::{active, unsupported};

/// The libraries a name is looked up in, in order: a name two of them hold answers the first one's row.
const GL_LIBS: &[&str] = &[
    "libGLESv2.so",
    "libGLESv3.so",
    "libGLESv1_CM.so",
    "libEGL.so",
];
/// Named-trap slots: one per distinct name a fallback trapped. Past them the lookup aborts naming the name; each slot is one pool thunk, the pool holds about 2k free after init (`qemu/place.rs`).
const NAMED_TRAPS: usize = 32;

/// What init reported for a row: its exported entry, and the host address its stub is bound to (None while unbound).
#[derive(Default)]
struct Row {
    exported: usize,
    host: Option<usize>,
}

/// The GL rows init reported, by (library, name).
static ROWS: Mutex<BTreeMap<(String, String), Row>> = Mutex::new(BTreeMap::new());

/// Records the exported entry of row (`lib`, `name`): the address a guest import of it resolves to. The intercept reports it for the entry before the stub (`INIT_INTERCEPTABLE_STUB_FUNCTION` calls twice); the QEMU gate reports its rows the same way.
pub fn note_exported(lib: &str, name: &str, exported: usize) {
    if !GL_LIBS.contains(&lib) {
        return;
    }
    if let Ok(mut rows) = ROWS.lock() {
        rows.entry((lib.to_owned(), name.to_owned()))
            .or_default()
            .exported = exported;
    }
}

/// Records the host address row (`lib`, `name`)'s stub is bound to, after the bind succeeded.
pub fn note_bound(lib: &str, name: &str, host: usize) {
    if !GL_LIBS.contains(&lib) {
        return;
    }
    if let Ok(mut rows) = ROWS.lock() {
        rows.entry((lib.to_owned(), name.to_owned()))
            .or_default()
            .host = Some(host);
    }
}

/// The address `name` answers from its row: the exported entry of the first of [`GL_LIBS`] holding it bound.
fn row_address(name: &str) -> Option<usize> {
    let rows = ROWS.lock().ok()?;
    GL_LIBS.iter().find_map(|lib| {
        let row = rows.get(&((*lib).to_owned(), name.to_owned()))?;
        let host = row.host.filter(|_| row.exported != 0)?;
        // deliberate break (`ATL_NB_BREAK=egl_raw`): the host's own address, which the guest would run on its guest FS
        let answer = if cfg!(atl_nb_egl_raw) {
            host
        } else {
            row.exported
        };
        Some(answer)
    })
}

/// What a name with no row answers, from the host's answer (0: none) and the name's prototype descriptor (return class, the QEMU lane refusal).
#[derive(Debug, PartialEq, Eq)]
enum Answer {
    Null,
    /// bind the host address with this result class
    Bind(usize, Class),
    /// a named trap, with why
    Trap(&'static str),
}

fn decide(host: usize, proto: Option<(Class, Option<&'static str>)>) -> Answer {
    if host == 0 {
        return Answer::Null;
    }
    match proto {
        Some((ret, None)) => Answer::Bind(host, ret),
        Some((_, Some(why))) => Answer::Trap(why),
        None => Answer::Trap("no prototype descriptor"),
    }
}

/// Host answers bound so far: host address -> guest thunk.
static BOUND: Mutex<BTreeMap<usize, usize>> = Mutex::new(BTreeMap::new());

/// The guest address of host function `host` (result class `ret`), bound by `bind` once per host address. Break `egl_nodedup` binds on every call.
fn bind_host(
    bound: &mut BTreeMap<usize, usize>,
    host: usize,
    ret: Class,
    bind: impl FnOnce(usize, Class) -> Option<usize>,
) -> Option<usize> {
    #[cfg(not(atl_nb_egl_nodedup))]
    if let Some(&thunk) = bound.get(&host) {
        return Some(thunk);
    }
    let thunk = bind(host, ret)?;
    bound.insert(host, thunk);
    Some(thunk)
}

/// The named traps made so far: (name, why, guest address). Slot `i` is `NAMED_FNS[i]`.
static NAMED: Mutex<Vec<(String, &'static str, usize)>> = Mutex::new(Vec::new());

/// The trap of slot `I`: aborts, naming the function it was made for.
extern "C" fn named_trap<const I: usize>() {
    let what = NAMED
        .lock()
        .ok()
        .and_then(|named| {
            named
                .get(I)
                .map(|(name, why, _)| format!("eglGetProcAddress({name}): no host binding ({why})"))
        })
        .unwrap_or_else(|| format!("eglGetProcAddress: named trap {I} has no name"));
    unsupported(&what)
}

macro_rules! named_traps {
    ($($i:literal)*) => {
        [$(named_trap::<$i> as extern "C" fn()),*]
    };
}
/// `named_trap::<i>` per slot; [`NAMED_TRAPS`] entries.
const NAMED_FNS: [extern "C" fn(); NAMED_TRAPS] = named_traps!(
    0 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20 21 22 23 24 25 26 27 28 29 30 31
);

/// The guest address of the named trap for `name` (`why` says why it has no binding), once per name.
fn named_trap_for(name: &str, why: &'static str) -> Result<usize, String> {
    let mut named = NAMED.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some((_, _, at)) = named.iter().find(|(n, _, _)| n.as_str() == name) {
        return Ok(*at);
    }
    let slot = named.len();
    let target = *NAMED_FNS
        .get(slot)
        .ok_or_else(|| format!("named-trap slots used up ({NAMED_TRAPS})"))?;
    let at = active()
        .bind_result(target as usize, Class::Int)
        .ok_or("the binding pool is used up")?;
    named.push((name.to_owned(), why, at));
    Ok(at)
}

/// The host libEGL's answer for `name`: 0 when there is no host libEGL, or it answers NULL.
fn host_answer(name: &str) -> usize {
    let Some(gpa) = bindings::system_symbol("libEGL.so", "eglGetProcAddress") else {
        return 0;
    };
    let Ok(c) = CString::new(name) else {
        return 0;
    };
    // SAFETY: the host libEGL's eglGetProcAddress has its EGL signature (EGL 1.5 §3.10); `c` is NUL-terminated and lives for the call.
    unsafe {
        let f =
            std::mem::transmute::<usize, unsafe extern "C" fn(*const c_char) -> *const c_void>(gpa);
        f(c.as_ptr()) as usize
    }
}

/// The prototype descriptor of `name` in the process's engine table (build.rs `PROTOTYPES`).
fn prototype(name: &str) -> Option<(Class, Option<&'static str>)> {
    bindings::prototypes()
        .iter()
        .find(|(n, _, _)| *n == name)
        .map(|&(_, ret, why)| (ret, why))
}

/// libEGL's `eglGetProcAddress` (bindings.rs `shim`): the guest's name in, the address its guest calls the name by out.
pub(super) extern "C" fn get_proc_address(name: *const c_char) -> *const c_void {
    if name.is_null() {
        return std::ptr::null();
    }
    // SAFETY: EGL 1.5 §3.10: a NUL-terminated name. Guest memory is host memory at guest_base 0 (engine/qemu), so the guest's string is readable here, as intercepts.rs and jni.rs read guest strings.
    let name = unsafe { CStr::from_ptr(name) }
        .to_string_lossy()
        .into_owned();
    if let Some(at) = row_address(&name) {
        return at as *const c_void;
    }
    let at = match decide(host_answer(&name), prototype(&name)) {
        Answer::Null => 0,
        Answer::Bind(host, ret) => {
            let mut bound = BOUND.lock().unwrap_or_else(PoisonError::into_inner);
            bind_host(&mut bound, host, ret, |h, r| active().bind_result(h, r)).unwrap_or_else(
                || {
                    unsupported(&format!(
                        "eglGetProcAddress({name}): the binding pool is used up"
                    ))
                },
            )
        }
        Answer::Trap(why) => named_trap_for(&name, why)
            .unwrap_or_else(|e| unsupported(&format!("eglGetProcAddress({name}): {e}"))),
    };
    at as *const c_void
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    /// A name with no row: no host answer gives NULL; a described host answer is bound by its descriptor; an undescribed or refused one is trapped.
    #[test]
    fn fallback_decision() {
        assert_eq!(decide(0, Some((Class::Void, None))), Answer::Null);
        assert_eq!(
            decide(0x1000, Some((Class::Void, None))),
            Answer::Bind(0x1000, Class::Void)
        );
        assert_eq!(
            decide(0x1000, None),
            Answer::Trap("no prototype descriptor")
        );
        assert_eq!(
            decide(0x1000, Some((Class::Int, Some("refused")))),
            Answer::Trap("refused")
        );
    }

    /// One binding per host address: a second answer for the same address reuses the first thunk and binds nothing (break `egl_nodedup` makes both fail).
    #[test]
    fn fallback_dedup() {
        let mut bound = BTreeMap::new();
        let made = Cell::new(0usize);
        let bind = |_: usize, _: Class| {
            made.set(made.get() + 1);
            Some(0x9000 + made.get() * 8)
        };
        let first = bind_host(&mut bound, 0x7000, Class::Void, bind);
        let second = bind_host(&mut bound, 0x7000, Class::Void, bind);
        assert_eq!(first, Some(0x9008));
        assert_eq!(second, first);
        assert_eq!(made.get(), 1);
    }

    /// A row answers its exported entry once its stub is bound; a row whose stub is not bound answers nothing.
    #[test]
    fn row_answers_exported_entry() {
        note_exported("libGLESv2.so", "glProcTestBound", 0x2000);
        note_bound("libGLESv2.so", "glProcTestBound", 0x3000);
        note_exported("libGLESv2.so", "glProcTestUnbound", 0x2100);
        assert_eq!(row_address("glProcTestBound"), Some(0x2000));
        assert_eq!(row_address("glProcTestUnbound"), None);
    }

    /// The shim on the host side, no guest: a bound row answers its exported entry; a name with no row and no host libEGL (none at SYSTEM_LIB here) answers NULL.
    #[test]
    fn shim_answers_rows_and_null() {
        note_exported("libGLESv2.so", "glShimTestRow", 0x4000);
        note_bound("libGLESv2.so", "glShimTestRow", 0x5000);
        assert_eq!(get_proc_address(c"glShimTestRow".as_ptr()) as usize, 0x4000);
        assert!(get_proc_address(c"glShimNoSuchName".as_ptr()).is_null());
    }

    /// A named trap is made once per name: the same name gives the same address, another name a new one.
    #[test]
    fn named_trap_once_per_name() {
        let a = named_trap_for("glProcTrapA", "no prototype descriptor").expect("a slot");
        let b = named_trap_for("glProcTrapB", "no prototype descriptor").expect("a slot");
        assert_ne!(a, b);
        assert_eq!(
            named_trap_for("glProcTrapA", "no prototype descriptor"),
            Ok(a)
        );
    }
}
