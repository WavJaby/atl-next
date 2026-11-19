// SPDX-License-Identifier: MIT
//! ILP32 row classes (phase4-32bit P4b): a prototype seen by a guest bionic view and the host view, each parameter and the result put in one [`Kind`] -- what a crossing between the two data models must do to that value. Every layout fact comes from the two views (sizes, offsets, pointer widths as clang computed them), never from a typed-in list; the one named input is the caller's handle-typedef set (an opaque handle spelled as a data pointer, e.g. `EGLDisplay`), each name checked to exist in the guest view and to be used.
//! A difference no rule names fails with the function and position: there is no default kind.
//! Equal data models (arm64 guest, LP64 host) classify every position [`Kind::Same`] or fail: the identity rule `report::abi_identical` held, in classifier form.

use std::collections::BTreeSet;

use anyhow::{Context, Result, bail, ensure};

use crate::ir::{ArchView, Signature, Ty};

/// What a crossing does to one parameter or the result.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    /// ABI-identical in both views: passes unchanged.
    Same,
    /// An integer narrower in the guest (`long`, `size_t`, `off_t`, `intptr_t`, `GLsizeiptr`, `EGLAttrib` by value): sign- or zero-extended in, truncated out.
    Widen { signed: bool },
    /// A data pointer value the host reads or writes through, its pointee identical in both views: zero-extended in.
    Ptr,
    /// A data pointer the host answers: must point into guest-addressable memory.
    PtrResult,
    /// A pointer to a scalar or pointer whose width differs (`size_t*`, `long*`, `void**`, `EGLAttrib*` arrays): the words are copied and resized.
    OutWord,
    /// A pointer to this record, whose layout differs: converted field by field.
    Record(String),
    /// A pointer to an incomplete record or a handle typedef: a host handle the guest only names back.
    Opaque,
    /// A function pointer: a guest entry the host would call; its lanes as a JNI shorty.
    Callback(String),
    /// A `const char*` result: host text the guest reads.
    String,
}

impl Kind {
    /// Converted by its lane class alone (the class's width and signedness, a pointer zero-extended into the guest's window): a row of only these kinds binds today.
    pub fn carried(&self) -> bool {
        matches!(self, Kind::Same | Kind::Widen { .. } | Kind::Ptr)
    }

    /// The kind's name in `ilp32_rows.txt` and in a trap reason.
    pub fn label(&self) -> &'static str {
        match self {
            Kind::Same => "same",
            Kind::Widen { .. } => "widen",
            Kind::Ptr => "ptr",
            Kind::PtrResult => "ptr-result",
            Kind::OutWord => "out-word",
            Kind::Record(_) => "record",
            Kind::Opaque => "opaque",
            Kind::Callback(_) => "callback",
            Kind::String => "string",
        }
    }

    /// How much a row of this kind still needs, for picking the one a trap names: a callback (a guest entry, P5's down shells) above a record (a converter per record) above the word and handle tables.
    pub fn weight(&self) -> u8 {
        match self {
            Kind::Same | Kind::Widen { .. } | Kind::Ptr => 0,
            Kind::String => 1,
            Kind::PtrResult => 2,
            Kind::Opaque => 3,
            Kind::OutWord => 4,
            Kind::Record(_) => 5,
            Kind::Callback(_) => 6,
        }
    }

    /// The trap reason of a row whose heaviest kind is this one (bindings `TRAP_NAMES`): fixed text per kind, so a trap names the conversion it waits for.
    pub fn trap_reason(&self) -> &'static str {
        match self {
            Kind::Same | Kind::Widen { .. } | Kind::Ptr => "carried",
            Kind::PtrResult => "pending: ilp32 ptr-result",
            Kind::OutWord => "pending: ilp32 out-word",
            Kind::Record(_) => "pending: ilp32 record",
            Kind::Opaque => "pending: ilp32 opaque",
            Kind::Callback(_) => "pending: ilp32 callback",
            Kind::String => "pending: ilp32 string",
        }
    }

    /// `label`, with the record name or shorty a kind carries (`record:ANativeWindow_Buffer`, `widen:s`).
    pub fn describe(&self) -> String {
        match self {
            Kind::Widen { signed } => format!("widen:{}", if *signed { "s" } else { "u" }),
            Kind::Record(name) => format!("record:{name}"),
            Kind::Callback(shorty) => format!("callback:{shorty}"),
            k => k.label().to_owned(),
        }
    }
}

/// The two views of one prototype set and the caller's handle typedefs.
pub struct Views<'a> {
    pub guest: &'a ArchView,
    pub host: &'a ArchView,
    /// Typedef names that spell an opaque handle as a data pointer (`EGLDisplay = void*`); a position spelled with one is [`Kind::Opaque`].
    pub handles: &'a [&'a str],
}

impl Views<'_> {
    /// Every handle name not declared as a typedef by the guest view: a stale or misspelt row.
    pub fn undeclared_handles(&self) -> Vec<&str> {
        self.handles
            .iter()
            .copied()
            .filter(|h| !self.guest.typedefs.contains_key(*h))
            .collect()
    }
}

/// One prototype's kinds: the result first, then each parameter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub kinds: Vec<Kind>,
}

impl Row {
    /// Every position carried by its class: the row binds as before.
    pub fn carried(&self) -> bool {
        self.kinds.iter().all(Kind::carried)
    }

    /// The heaviest kind of the row (its trap reason).
    pub fn heaviest(&self) -> &Kind {
        self.kinds
            .iter()
            .max_by_key(|k| k.weight())
            .expect("a row has a result position")
    }

    /// Every position `Same`.
    pub fn same(&self) -> bool {
        self.kinds.iter().all(|k| *k == Kind::Same)
    }

    /// `result=<kind> p0=<kind> ...` with each kind's detail.
    pub fn describe(&self) -> String {
        self.kinds
            .iter()
            .enumerate()
            .map(|(i, k)| match i {
                0 => format!("result={}", k.describe()),
                p => format!("p{}={}", p - 1, k.describe()),
            })
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// Classifies function `name`: `guest` in `views.guest`, `host` in `views.host`. Records each handle name it used in `used`. Err names the function and the position no rule covers.
pub fn classify(
    name: &str,
    guest: &Signature,
    host: &Signature,
    views: &Views,
    used: &mut BTreeSet<String>,
) -> Result<Row> {
    ensure!(
        guest.variadic == host.variadic && guest.params.len() == host.params.len(),
        "{name}: arity or variadic differs between the guest and host views"
    );
    let mut kinds = vec![
        position(&guest.ret, &host.ret, true, views, used)
            .with_context(|| format!("{name}: result"))?,
    ];
    for (i, (g, h)) in guest.params.iter().zip(&host.params).enumerate() {
        kinds.push(
            position(g, h, false, views, used).with_context(|| format!("{name}: parameter {i}"))?,
        );
    }
    Ok(Row { kinds })
}

/// The handle typedef a type is spelled with, through a typedef chain (`EGLDisplay`, or a typedef of one).
fn handle<'a>(ty: &'a Ty, handles: &[&str]) -> Option<&'a str> {
    match ty {
        Ty::Typedef { name, ty } => {
            if handles.contains(&name.as_str()) {
                Some(name)
            } else {
                handle(ty, handles)
            }
        }
        _ => None,
    }
}

/// The first typedef name in `ty`'s chain that stands for `void*` (`EGLDisplay`, `GLeglImageOES`).
fn void_pointer_typedef(ty: &Ty) -> Option<&str> {
    match ty {
        Ty::Typedef { name, ty: inner } => match ty.resolved() {
            Ty::Pointer { pointee, .. } if matches!(pointee.resolved(), Ty::Void) => Some(name),
            _ => void_pointer_typedef(inner),
        },
        _ => None,
    }
}

fn position(
    g: &Ty,
    h: &Ty,
    result: bool,
    views: &Views,
    used: &mut BTreeSet<String>,
) -> Result<Kind> {
    if same(g, views.guest, h, views.host, true) {
        return Ok(Kind::Same);
    }
    if let Some(n) = handle(g, views.handles) {
        used.insert(n.to_owned());
        return Ok(Kind::Opaque);
    }
    // a `void*` typedef names either a handle or a buffer, and the views cannot tell which: it must be listed
    if let Some(n) = void_pointer_typedef(g) {
        bail!(
            "{n} is a void* typedef not in the handle list: list it as a handle, or spell the buffer as void*"
        );
    }
    match (g.resolved(), h.resolved()) {
        (
            &Ty::Int {
                bits: gb, signed, ..
            },
            &Ty::Int { bits: hb, .. },
        ) if gb < hb => Ok(Kind::Widen { signed }),
        // a record by value whose layout differs (NdkMediaCodec.h `AMediaCodecOnAsyncNotifyCallback`, four fn pointers): converted field by field like one behind a pointer
        (Ty::Record { name }, Ty::Record { .. }) => Ok(Kind::Record(name.clone())),
        // a parameter of array type is a pointer to its element (NDK NdkMediaCrypto.h `const uint8_t uuid[16]`)
        (Ty::Array { elem: ge, .. }, Ty::Array { elem: he, .. }) if !result => {
            pointee(ge, he, false, views)
        }
        (
            Ty::Pointer { pointee: gp, .. } | Ty::Reference { pointee: gp, .. },
            Ty::Pointer { pointee: hp, .. } | Ty::Reference { pointee: hp, .. },
        ) => pointee(gp, hp, result, views),
        // a pointer in bionic, an integer in glibc (eglplatform.h `EGLNativePixmapType`): the guest passes a pointer
        (Ty::Pointer { pointee: gp, .. }, Ty::Int { .. })
            if !matches!(gp.resolved(), Ty::Function(_)) =>
        {
            Ok(if result { Kind::PtrResult } else { Kind::Ptr })
        }
        _ => bail!("unclassified ILP32 difference: guest {g:?} vs host {h:?}"),
    }
}

/// The kind of a pointer position, by what it points to.
fn pointee(gp: &Ty, hp: &Ty, result: bool, views: &Views) -> Result<Kind> {
    let data = if result { Kind::PtrResult } else { Kind::Ptr };
    Ok(match (gp.resolved(), hp.resolved()) {
        (Ty::Function(sig), Ty::Function(_)) => Kind::Callback(crate::jni::shorty(sig)?),
        (Ty::Opaque { .. }, _) | (_, Ty::Opaque { .. }) => Kind::Opaque,
        (Ty::Int { bits: 8, .. }, Ty::Int { bits: 8, .. }) if result => Kind::String,
        _ if same(gp, views.guest, hp, views.host, false) => data,
        (Ty::Record { name }, Ty::Record { .. }) => Kind::Record(name.clone()),
        (
            Ty::Int { .. } | Ty::Pointer { .. } | Ty::Typedef { .. },
            Ty::Int { .. } | Ty::Pointer { .. },
        ) => Kind::OutWord,
        _ => bail!("unclassified ILP32 pointee: guest {gp:?} vs host {hp:?}"),
    })
}

/// `a` (of `va`) and `b` (of `vb`) are ABI-identical: same widths, same record layouts all the way down; behind a pointer (`top` false) an integer's signedness is not part of the ABI (plain `char` is unsigned on arm, signed on x86), at the top it says how a register is extended.
fn same(a: &Ty, va: &ArchView, b: &Ty, vb: &ArchView, top: bool) -> bool {
    same_in(a, va, b, vb, top, &mut Vec::new())
}

/// [`same`], `open` the record pairs being compared further up: a record reached again through its own pointer field (a list node) is assumed equal there, its fields being checked by the outer comparison.
fn same_in(
    a: &Ty,
    va: &ArchView,
    b: &Ty,
    vb: &ArchView,
    top: bool,
    open: &mut Vec<(String, String)>,
) -> bool {
    match (a.resolved(), b.resolved()) {
        (Ty::Void, Ty::Void) | (Ty::Bool, Ty::Bool) => true,
        (
            Ty::Int {
                bits: ba,
                signed: sa,
                ..
            },
            Ty::Int {
                bits: bb,
                signed: sb,
                ..
            },
        ) => ba == bb && (!top || sa == sb),
        (Ty::Float { bits: ba }, Ty::Float { bits: bb }) => ba == bb,
        (
            Ty::Pointer {
                bits: ba,
                pointee: pa,
                ..
            }
            | Ty::Reference {
                bits: ba,
                pointee: pa,
                ..
            },
            Ty::Pointer {
                bits: bb,
                pointee: pb,
                ..
            }
            | Ty::Reference {
                bits: bb,
                pointee: pb,
                ..
            },
        ) => {
            ba == bb
                && (matches!(pa.resolved(), Ty::Void | Ty::Opaque { .. })
                    || matches!(pb.resolved(), Ty::Void | Ty::Opaque { .. })
                    || same_in(pa, va, pb, vb, false, open))
        }
        // data pointer vs integer of the same width (report.rs TyDiff::PointerVsInteger)
        (Ty::Pointer { bits, pointee, .. }, Ty::Int { bits: i, .. })
        | (Ty::Int { bits: i, .. }, Ty::Pointer { bits, pointee, .. }) => {
            bits == i && !matches!(pointee.resolved(), Ty::Function(_))
        }
        (Ty::Array { elem: ea, len: la }, Ty::Array { elem: eb, len: lb }) => {
            la == lb && same_in(ea, va, eb, vb, false, open)
        }
        (Ty::Function(sa), Ty::Function(sb)) => {
            sa.variadic == sb.variadic
                && sa.params.len() == sb.params.len()
                && same_in(&sa.ret, va, &sb.ret, vb, true, open)
                && sa
                    .params
                    .iter()
                    .zip(&sb.params)
                    .all(|(x, y)| same_in(x, va, y, vb, true, open))
        }
        (Ty::Record { name: na }, Ty::Record { name: nb }) => {
            let key = (na.clone(), nb.clone());
            if open.contains(&key) {
                return true;
            }
            match (va.records.get(na), vb.records.get(nb)) {
                (Some(ra), Some(rb)) => {
                    open.push(key);
                    let equal = ra.size == rb.size
                        && ra.align == rb.align
                        && ra.fields.len() == rb.fields.len()
                        && ra.fields.iter().zip(&rb.fields).all(|(fa, fb)| {
                            fa.offset == fb.offset
                                && fa.bits == fb.bits
                                && same_in(&fa.ty, va, &fb.ty, vb, false, open)
                        });
                    open.pop();
                    equal
                }
                _ => false,
            }
        }
        (Ty::Opaque { name: na, .. }, Ty::Opaque { name: nb, .. }) => na == nb,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::{Source, Target, interface};

    /// bionic arm32 / glibc x86_64 declarations in miniature, each transcribed from its header with the field order kept; the expected sizes in the tests come from the AAPCS32 data model and those headers, not from the classifier.
    /// `off_t`, `size_t`, `time_t`: `long` / `unsigned long` in bionic (`__kernel_off_t`, `__kernel_size_t`, `__kernel_time_t`, uapi asm-generic/posix_types.h). `pthread_attr_t`: bionic libc/include/bits/pthread_types.h. `ANativeWindow_Buffer`: nativewindow native_window.h. `ARect`: arect rect.h. `EGLAttrib`: EGL/egl.h `intptr_t`. `EGLDisplay`: `void*`.
    const HEADER: &str = "typedef long off_t;\ntypedef long long off64_t;\ntypedef unsigned long size_t;\ntypedef long time_t;\ntypedef long intptr_t;\n\
struct timespec { time_t tv_sec; long tv_nsec; };\n\
typedef struct { unsigned int flags; void* stack_base; size_t stack_size; size_t guard_size; int sched_policy; int sched_priority; } pthread_attr_t;\n\
typedef struct ANativeWindow_Buffer { int width; int height; int stride; int format; void* bits; unsigned int reserved[6]; } ANativeWindow_Buffer;\n\
typedef struct ARect { int left; int top; int right; int bottom; } ARect;\n\
typedef struct AAsset AAsset;\ntypedef struct ANativeWindow ANativeWindow;\n\
typedef void *EGLDisplay;\ntypedef intptr_t EGLAttrib;\ntypedef void *EGLImage;\ntypedef unsigned int EGLenum;\ntypedef void *EGLClientBuffer;\ntypedef void *EGLContext;\n\
typedef int (*ALooper_callbackFunc)(int fd, int events, void* data);\n\
off_t AAsset_getLength(AAsset* asset);\n\
off64_t AAsset_getLength64(AAsset* asset);\n\
int ALooper_pollOnce(int timeoutMillis, int* outFd, int* outEvents, void** outData);\n\
EGLImage eglCreateImage(EGLDisplay dpy, EGLContext ctx, EGLenum target, EGLClientBuffer buffer, const EGLAttrib* attrib_list);\n\
int ANativeWindow_lock(ANativeWindow* window, ANativeWindow_Buffer* outBuffer, ARect* inOutDirtyBounds);\n\
int nanosleep(const struct timespec* req, struct timespec* rem);\n\
int pthread_attr_getstacksize(const pthread_attr_t* attr, size_t* size);\n\
void* memcpy(void* dst, const void* src, size_t n);\n\
const char* AAsset_name(AAsset* asset);\n\
int ALooper_addFd(int fd, int ident, int events, ALooper_callbackFunc callback, void* data);\n\
int sum(int a, double b);\n";

    const HANDLES: &[&str] = &["EGLDisplay", "EGLImage", "EGLContext", "EGLClientBuffer"];

    struct Tmp(PathBuf);
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// `HEADER` in the bionic arm view (`guest`) and an LP64 view (`host`). `host_triple`: the red control passes the host's own triple for the guest, so every position reads `Same`.
    fn views(name: &str, guest_triple: &str) -> crate::ir::Interface {
        let dir =
            Tmp(std::env::temp_dir()
                .join(format!("atl-bridgegen-ilp32-{name}-{}", std::process::id())));
        std::fs::create_dir_all(&dir.0).unwrap();
        let path = dir.0.join("h.h");
        std::fs::write(&path, HEADER).unwrap();
        let target = |arch: &str, triple: &str| Target {
            arch: arch.into(),
            triple: triple.into(),
            args: vec!["-nostdlibinc".into()],
        };
        interface(
            "ilp32",
            "h.h",
            &Source::c(&path, &[]),
            &[
                target("guest", guest_triple),
                target("host", "x86_64-unknown-linux-gnu"),
            ],
        )
        .unwrap()
    }

    const ARM: &str = "armv7a-linux-androideabi";

    fn row(iface: &crate::ir::Interface, name: &str) -> Row {
        let (g, h) = (&iface.arches["guest"], &iface.arches["host"]);
        let views = Views {
            guest: g,
            host: h,
            handles: HANDLES,
        };
        classify(
            name,
            &g.functions[name],
            &h.functions[name],
            &views,
            &mut BTreeSet::new(),
        )
        .unwrap()
    }

    #[test]
    fn arm_data_model_from_the_view() {
        let i = views("model", ARM);
        let g = &i.arches["guest"];
        let size = |t: &str| match g.typedefs[t].resolved() {
            Ty::Int { bits, .. } => bits / 8,
            _ => panic!("{t}"),
        };
        // AAPCS32 / bionic ILP32: long, size_t, off_t, time_t 4 bytes; off64_t 8
        assert_eq!(
            [
                size("off_t"),
                size("size_t"),
                size("time_t"),
                size("off64_t")
            ],
            [4, 4, 4, 8]
        );
        // time_t 4 + long 4
        assert_eq!(g.records["timespec"].size, 8);
        // u32 + ptr 4 + size_t 4 + size_t 4 + i32 + i32
        let attr = g
            .records
            .values()
            .find(|r| r.fields.first().is_some_and(|f| f.name == "flags"))
            .unwrap();
        assert_eq!(attr.size, 24);
        // 4 x int32 + void* 4 + 6 x uint32 = 44 (x86_64: 16 + 8 + 24 = 48)
        assert_eq!(g.records["ANativeWindow_Buffer"].size, 44);
        assert_eq!(i.arches["host"].records["ANativeWindow_Buffer"].size, 48);
    }

    #[test]
    fn kinds_of_the_brief_rows() {
        let i = views("kinds", ARM);
        // off_t result: long, narrower in the guest
        assert_eq!(
            row(&i, "AAsset_getLength").kinds[0],
            Kind::Widen { signed: true }
        );
        // off64_t is 8 bytes on both: the result is Same, the asset handle Opaque
        assert_eq!(
            row(&i, "AAsset_getLength64").kinds,
            vec![Kind::Same, Kind::Opaque]
        );
        // void** outData: a pointer-width word behind a pointer
        assert_eq!(row(&i, "ALooper_pollOnce").kinds[4], Kind::OutWord);
        // int* outFd: 4-byte int behind a pointer, same in both
        assert_eq!(row(&i, "ALooper_pollOnce").kinds[2], Kind::Ptr);
        let img = row(&i, "eglCreateImage");
        // EGLImage result and EGLDisplay: handle typedefs; EGLenum Same; const EGLAttrib*: an intptr_t array
        assert_eq!(
            img.kinds,
            vec![
                Kind::Opaque,
                Kind::Opaque,
                Kind::Opaque,
                Kind::Same,
                Kind::Opaque,
                Kind::OutWord
            ]
        );
        let lock = row(&i, "ANativeWindow_lock");
        // ANativeWindow_Buffer 44 vs 48 bytes: a record; ARect 16/16: a plain data pointer
        assert_eq!(
            lock.kinds[2],
            Kind::Record("ANativeWindow_Buffer".to_owned())
        );
        assert_eq!(lock.kinds[3], Kind::Ptr);
        assert_eq!(lock.kinds[1], Kind::Opaque);
        assert_eq!(
            row(&i, "nanosleep").kinds[1],
            Kind::Record("timespec".to_owned())
        );
        let memcpy = row(&i, "memcpy");
        assert_eq!(
            memcpy.kinds,
            vec![
                Kind::PtrResult,
                Kind::Ptr,
                Kind::Ptr,
                Kind::Widen { signed: false }
            ]
        );
        assert_eq!(row(&i, "pthread_attr_getstacksize").kinds[2], Kind::OutWord);
        assert_eq!(row(&i, "AAsset_name").kinds[0], Kind::String);
        assert_eq!(
            row(&i, "ALooper_addFd").kinds[4],
            Kind::Callback("IIIL".to_owned())
        );
        assert!(row(&i, "sum").same());
        assert!(!row(&i, "ANativeWindow_lock").carried());
        assert!(row(&i, "ALooper_pollOnce").heaviest() == &Kind::OutWord);
    }

    /// Red control: the guest parsed as the host's own data model -> every row Same, so the ILP32 expectations above fail.
    #[test]
    fn host_model_guest_reads_same() {
        let i = views("lp64", "x86_64-unknown-linux-gnu");
        for name in [
            "AAsset_getLength",
            "memcpy",
            "ANativeWindow_lock",
            "eglCreateImage",
        ] {
            assert!(row(&i, name).same(), "{name}");
        }
    }

    /// Red control of the handle list: a `void*` typedef dropped from it fails, naming the function and the typedef.
    #[test]
    fn unlisted_handle_fails() {
        let i = views("unlisted", ARM);
        let (g, h) = (&i.arches["guest"], &i.arches["host"]);
        let views = Views {
            guest: g,
            host: h,
            handles: &["EGLImage", "EGLContext", "EGLClientBuffer"],
        };
        let e = classify(
            "eglCreateImage",
            &g.functions["eglCreateImage"],
            &h.functions["eglCreateImage"],
            &views,
            &mut BTreeSet::new(),
        )
        .unwrap_err();
        let e = format!("{e:#}");
        assert!(
            e.contains("eglCreateImage") && e.contains("EGLDisplay"),
            "{e}"
        );
        assert_eq!(
            Views {
                guest: g,
                host: h,
                handles: &["EGLNoSuchHandle"]
            }
            .undeclared_handles(),
            vec!["EGLNoSuchHandle"]
        );
    }
}
