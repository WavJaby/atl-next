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
    /// A non-const pointer to one scalar or data pointer whose width differs (`size_t*`, `long*`, `void**`): a host-width temporary, read widened and written back narrowed.
    OutWord(Word),
    /// A const pointer to scalars or pointers whose width differs (`const EGLAttrib*`, `const char* const*`): an array whose length the header states in prose (a terminator, a count parameter), so no generic rule converts it.
    Array,
    /// A pointer to this record, whose layout differs: converted field by field ([`converters`]).
    Record(String),
    /// jni.h's `JNIEnv*` / `JavaVM*` (records `_JNIEnv`, `_JavaVM`): the guest's proxy object stands for the host's (P5's JNI tables), never a record copy.
    Jni,
    /// A pointer to an incomplete record or a handle typedef: a host handle the guest only names back.
    Opaque,
    /// A function pointer: a guest entry the host would call; its lanes as a JNI shorty.
    Callback(String),
    /// A `const char*` result: host text the guest reads.
    String,
}

/// The word behind an [`Kind::OutWord`] pointer, as the two sides hold it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Word {
    /// An integer of `guest` / `host` bytes: widened by its signedness, written back truncated.
    Int { guest: u8, host: u8, signed: bool },
    /// A data pointer: zero-extended, written back refused at or above 4 GiB.
    Ptr,
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
            Kind::OutWord(_) => "out-word",
            Kind::Array => "array",
            Kind::Record(_) => "record",
            Kind::Jni => "jni",
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
            Kind::OutWord(_) | Kind::Array => 4,
            Kind::Record(_) => 5,
            Kind::Jni => 6,
            Kind::Callback(_) => 7,
        }
    }

    /// The trap reason of a row whose heaviest kind is this one (bindings `TRAP_NAMES`): fixed text per kind, so a trap names the conversion it waits for.
    pub fn trap_reason(&self) -> &'static str {
        match self {
            Kind::Same | Kind::Widen { .. } | Kind::Ptr => "carried",
            Kind::PtrResult => "pending: ilp32 ptr-result",
            Kind::OutWord(_) => "pending: ilp32 out-word",
            Kind::Array => "pending: ilp32 array",
            Kind::Record(_) => "pending: ilp32 record",
            Kind::Jni => "pending: ilp32 jni (P5)",
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
            Kind::OutWord(Word::Int {
                guest,
                host,
                signed,
            }) => {
                format!("out-word:{}{guest}>{host}", if *signed { "s" } else { "u" })
            }
            Kind::OutWord(Word::Ptr) => "out-word:ptr".to_owned(),
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

/// `sig`'s parameter lane classes in view `v` ([`crate::jni::param_classes`]), plus the by-value one-pointer-wide union ([`word_union`], `Kind::Widen{signed:false}`): an unsigned int lane of the union's width, as the register carries it.
pub fn param_classes(sig: &Signature, v: &ArchView) -> Result<Vec<crate::jni::Class>> {
    sig.params
        .iter()
        .map(|p| match p.resolved() {
            Ty::Record { name } | Ty::Opaque { name, .. } if word_union(p, v) => {
                let bits = layout(v, name).map_or(0, |r| r.size * 8);
                Ok(crate::jni::Class::Int {
                    bits: u8::try_from(bits)?,
                    signed: false,
                })
            }
            _ => crate::jni::param_classes(&Signature {
                ret: Ty::Void,
                params: vec![p.clone()],
                variadic: false,
            })
            .map(|mut c| c.remove(0)),
        })
        .collect()
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
    if va_list(g) && va_list(h) {
        // by layout here (the resolved types escape `same_in`'s va_list rule): equal targets pass it, others have no rule
        ensure!(
            same(g.resolved(), views.guest, h.resolved(), views.host, true),
            "a va_list position whose layout differs: no rule crosses one (only the JNI V path converts)"
        );
        return Ok(Kind::Same);
    }
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
        // a signedness-only difference at equal width is carried (bionic arm `socklen_t` = int32, glibc's uint32): the register holds the same bits on both sides and neither callee extends a full-width argument
        (&Ty::Int { bits: gb, .. }, &Ty::Int { bits: hb, .. }) if gb == hb => Ok(Kind::Same),
        // a by-value union one pointer wide in each view ([`word_union`]: `union sigval`, 4 bytes on arm, 8 on x86_64) travels in one register (AAPCS32 parameter passing: a composite of at most 4 bytes takes one core register, its content in the low bytes; SysV x86_64: an 8-byte union of INTEGER class takes one general register), so it crosses as an unsigned word: zero-extended in (the int member is the low 4 bytes on both little-endian sides, a pointer member a guest window address), truncated out
        _ if !result && word_union(g, views.guest) && word_union(h, views.host) => {
            Ok(Kind::Widen { signed: false })
        }
        // a record by value whose layout differs (NdkMediaCodec.h `AMediaCodecOnAsyncNotifyCallback`, four fn pointers): converted field by field like one behind a pointer
        _ if both_records(g, h, views).is_some() => {
            Ok(Kind::Record(both_records(g, h, views).unwrap_or_default()))
        }
        // a parameter of array type is a pointer to its element (NDK NdkMediaCrypto.h `const uint8_t uuid[16]`)
        (Ty::Array { elem: ge, .. }, Ty::Array { elem: he, .. }) if !result => {
            pointee(ge, he, false, true, views)
        }
        (
            Ty::Pointer {
                pointee: gp,
                is_const,
                ..
            }
            | Ty::Reference {
                pointee: gp,
                is_const,
                ..
            },
            Ty::Pointer { pointee: hp, .. } | Ty::Reference { pointee: hp, .. },
        ) => pointee(gp, hp, result, *is_const, views),
        // a pointer in bionic, an integer in glibc (eglplatform.h `EGLNativePixmapType`): the guest passes a pointer
        (Ty::Pointer { pointee: gp, .. }, Ty::Int { .. })
            if !matches!(gp.resolved(), Ty::Function(_)) =>
        {
            Ok(if result { Kind::PtrResult } else { Kind::Ptr })
        }
        _ => bail!("unclassified ILP32 difference: guest {g:?} vs host {h:?}"),
    }
}

/// The kind of a pointer position, by what it points to; `konst`: a `const` pointee.
fn pointee(gp: &Ty, hp: &Ty, result: bool, konst: bool, views: &Views) -> Result<Kind> {
    let data = if result { Kind::PtrResult } else { Kind::Ptr };
    Ok(match (gp.resolved(), hp.resolved()) {
        (Ty::Function(sig), Ty::Function(_)) => Kind::Callback(crate::jni::shorty(sig)?),
        (Ty::Record { name } | Ty::Opaque { name, .. }, _)
            if JNI_RECORDS.contains(&name.as_str()) =>
        {
            Kind::Jni
        }
        _ if untyped(gp, views.guest) && !matches!(gp.resolved(), Ty::Void)
            || untyped(hp, views.host) && !matches!(hp.resolved(), Ty::Void) =>
        {
            Kind::Opaque
        }
        (Ty::Int { bits: 8, .. }, Ty::Int { bits: 8, .. }) if result => Kind::String,
        _ if same(gp, views.guest, hp, views.host, false) => data,
        _ if both_records(gp, hp, views).is_some() => {
            Kind::Record(both_records(gp, hp, views).unwrap_or_default())
        }
        (Ty::Int { .. } | Ty::Pointer { .. }, Ty::Int { .. } | Ty::Pointer { .. }) if konst => {
            Kind::Array
        }
        (
            &Ty::Int {
                bits: g, signed, ..
            },
            &Ty::Int { bits: h, .. },
        ) => Kind::OutWord(Word::Int {
            guest: (g / 8) as u8,
            host: (h / 8) as u8,
            signed,
        }),
        (Ty::Pointer { pointee: gi, .. }, Ty::Pointer { pointee: hi, .. }) => {
            out_pointer(gp, gi, hi, views)
        }
        // a pointer in bionic, an integer in glibc (eglplatform.h `EGLNativeWindowType*`): the guest's word is a pointer, compared against itself
        (Ty::Pointer { pointee: gi, .. }, Ty::Int { .. }) => out_pointer(gp, gi, gi, views),
        _ => bail!("unclassified ILP32 pointee: guest {gp:?} vs host {hp:?}"),
    })
}

/// jni.h's env and vm records (C++: `struct _JNIEnv`, `struct _JavaVM`): [`Kind::Jni`].
const JNI_RECORDS: &[&str] = &["_JNIEnv", "_JavaVM"];

/// The kind of a non-const pointer to a pointer (`gp`, pointing to `gi` / `hi`): [`Word::Ptr`] when what it points to reads the same on both sides or is `void`; else the word the host writes names host memory the guest cannot read as it is -- a handle typedef, an incomplete record, a function, host records of the host's layout -- which is a host object the guest only names back ([`Kind::Opaque`], P4b s4's handles).
fn out_pointer(gp: &Ty, gi: &Ty, hi: &Ty, views: &Views) -> Kind {
    let void = matches!(gi.resolved(), Ty::Void);
    // an incomplete record or a function reads `same` by name: still a host object
    let object = !void && untyped(gi, views.guest) || matches!(gi.resolved(), Ty::Function(_));
    let readable = void || same(gi, views.guest, hi, views.host, false);
    if handle(gp, views.handles).is_none() && !object && readable {
        Kind::OutWord(Word::Ptr)
    } else {
        Kind::Opaque
    }
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
    // inside a record or a callback type: each side's own va_list (`struct __va_list` on arm64, `__va_list_tag[1]` on x86_64) -- a cursor no layout comparison can equate; the V calls that receive one are converted by the JNI path (dispatch.rs `call_object`), and a classified position refuses it ([`position`])
    if va_list(a) && va_list(b) {
        return true;
    }
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
            ba == bb && (untyped(pa, va) || untyped(pb, vb) || same_in(pa, va, pb, vb, false, open))
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
        // by layout, never by name: `union sigval` is one name for 4 bytes on arm and 8 on x86_64
        (
            Ty::Record { name: na } | Ty::Opaque { name: na, .. },
            Ty::Record { name: nb } | Ty::Opaque { name: nb, .. },
        ) => {
            let key = (na.clone(), nb.clone());
            if open.contains(&key) {
                return true;
            }
            match (layout(va, na), layout(vb, nb)) {
                // both incomplete: a handle neither side looks into
                (None, None) => na == nb,
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
        _ => false,
    }
}

/// `ty` is a complete union in `v` of integers of at most 32 bits and at least one data pointer, every member at offset 0, no bit-field: its size is then exactly one pointer (pointers are at least 4 bytes on every target here).
fn word_union(ty: &Ty, v: &ArchView) -> bool {
    let name = match ty.resolved() {
        Ty::Record { name } | Ty::Opaque { name, .. } => name,
        _ => return false,
    };
    layout(v, name).is_some_and(|r| {
        r.kind == crate::ir::RecordKind::Union
            && r.fields
                .iter()
                .any(|f| matches!(f.ty.resolved(), Ty::Pointer { .. }))
            && r.fields.iter().all(|f| {
                f.offset == 0
                    && f.bits.is_none()
                    && match f.ty.resolved() {
                        Ty::Int { bits, .. } => *bits <= 32,
                        Ty::Pointer { .. } => true,
                        _ => false,
                    }
            })
    })
}

/// The layout of record `name` in `v`: defined by the interface or by a header it includes; `None` = incomplete.
fn layout<'v>(v: &'v ArchView, name: &str) -> Option<&'v crate::ir::Record> {
    v.records.get(name).or_else(|| v.foreign.get(name))
}

/// `ty` is the compiler's variadic cursor (clang's `__builtin_va_list` typedef, the one spelling every target's va_list goes through).
fn va_list(ty: &Ty) -> bool {
    match ty {
        Ty::Typedef { name, ty } => name == "__builtin_va_list" || va_list(ty),
        _ => false,
    }
}

/// A pointee the host never looks into: `void`, an incomplete record, or a record without fields (a C++ tag class: jni.h `class _jobject {}` makes `jobject` a handle, not memory).
fn untyped(ty: &Ty, v: &ArchView) -> bool {
    match ty.resolved() {
        Ty::Void => true,
        Ty::Opaque { name, .. } | Ty::Record { name } => {
            layout(v, name).is_none_or(|r| r.fields.is_empty())
        }
        _ => false,
    }
}

/// `g` and `h` both name a record complete in their view: the guest's name.
fn both_records(g: &Ty, h: &Ty, views: &Views) -> Option<String> {
    let complete = |ty: &Ty, v: &ArchView| match ty.resolved() {
        Ty::Record { name } | Ty::Opaque { name, .. } if layout(v, name).is_some() => {
            Some(name.clone())
        }
        _ => None,
    };
    complete(h, views.host)?;
    complete(g, views.guest)
}

/// How one field crosses between the guest's and the host's layout of a record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Op {
    /// The same bytes on both sides (same layout all the way down).
    Copy { bytes: u64 },
    /// An integer of `guest` / `host` bytes: widened by signedness in, truncated out.
    Int { guest: u64, host: u64, signed: bool },
    /// A data pointer whose pointee reads the same on both sides (or `void`): zero-extended in, refused at or above 4 GiB out.
    Ptr { guest: u64, host: u64 },
    /// A nested record by value whose layout differs: its own converter.
    Record(String),
    /// `len` elements converted one by one, each `guest` / `host` bytes apart.
    Array {
        len: u64,
        elem: Box<Op>,
        guest: u64,
        host: u64,
    },
    /// A pointer to data whose layout differs (a list's next node, a `char**` list): a deep copy whose length and ownership the header states in prose, so the generic converter writes NULL and only a hand shim that walks it may use the record ([`Conv::deep`]).
    Deep { guest: u64, host: u64 },
}

impl Conv {
    /// A field is [`Op::Deep`], here or in a nested record: the lane-mapped route refuses the record.
    pub fn deep(&self, all: &[Conv]) -> bool {
        fn op_deep(op: &Op, all: &[Conv]) -> bool {
            match op {
                Op::Deep { .. } => true,
                Op::Record(n) => all.iter().any(|c| c.name == *n && c.deep(all)),
                Op::Array { elem, .. } => op_deep(elem, all),
                _ => false,
            }
        }
        self.fields.iter().any(|f| op_deep(&f.op, all))
    }
}

/// One field of a [`Conv`]: its offsets on both sides, its alignment on both sides (the view's, for the build-time check), and how it crosses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldConv {
    pub name: String,
    pub guest_offset: u64,
    pub host_offset: u64,
    pub guest_align: u64,
    pub host_align: u64,
    pub op: Op,
}

/// A record's converter, derived from the two views: guest and host size, and every field in declaration order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conv {
    pub name: String,
    pub guest_size: u64,
    pub host_size: u64,
    pub fields: Vec<FieldConv>,
}

/// The converters for record `name` and every nested record it holds by value whose layout differs, nested ones first. Err names the field no rule converts: a union (which member is live is the program's knowledge), a bit-field, a function pointer (a guest entry: a down shell, not a copy), or a field set that differs. A pointer to data whose layout differs is [`Op::Deep`].
pub fn converters(name: &str, views: &Views) -> Result<Vec<Conv>> {
    let mut out = Vec::new();
    conv_into(name, views, &mut out)?;
    Ok(out)
}

fn conv_into(name: &str, views: &Views, out: &mut Vec<Conv>) -> Result<()> {
    if out.iter().any(|c| c.name == name) {
        return Ok(());
    }
    let (Some(g), Some(h)) = (layout(views.guest, name), layout(views.host, name)) else {
        bail!("{name}: incomplete in one view");
    };
    ensure!(
        g.kind == crate::ir::RecordKind::Struct,
        "{name}: a union, whose live member only the program knows"
    );
    ensure!(
        g.fields.len() == h.fields.len(),
        "{name}: {} guest fields, {} host fields",
        g.fields.len(),
        h.fields.len()
    );
    let mut fields = Vec::new();
    // by name: the two libcs may declare one record's fields in another order (bionic `addrinfo` ai_canonname before ai_addr, glibc after)
    for gf in &g.fields {
        let hf = h
            .fields
            .iter()
            .find(|hf| hf.name == gf.name)
            .with_context(|| format!("{name}.{}: no such host field", gf.name))?;
        ensure!(
            gf.bits.is_none() && hf.bits.is_none(),
            "{name}.{}: a bit-field",
            gf.name
        );
        let op =
            field_op(&gf.ty, &hf.ty, views, out).with_context(|| format!("{name}.{}", gf.name))?;
        fields.push(FieldConv {
            name: gf.name.clone(),
            guest_offset: gf.offset,
            host_offset: hf.offset,
            guest_align: size_align(&gf.ty, views.guest)?.1,
            host_align: size_align(&hf.ty, views.host)?.1,
            op,
        });
    }
    out.push(Conv {
        name: name.to_owned(),
        guest_size: g.size,
        host_size: h.size,
        fields,
    });
    Ok(())
}

/// The op of a field typed `g` in the guest view and `h` in the host view.
fn field_op(g: &Ty, h: &Ty, views: &Views, out: &mut Vec<Conv>) -> Result<Op> {
    if same(g, views.guest, h, views.host, false) {
        let (gs, _) = size_align(g, views.guest)?;
        return Ok(Op::Copy { bytes: gs });
    }
    Ok(match (g.resolved(), h.resolved()) {
        (
            &Ty::Int {
                bits: gb, signed, ..
            },
            &Ty::Int { bits: hb, .. },
        ) => Op::Int {
            guest: u64::from(gb / 8),
            host: u64::from(hb / 8),
            signed,
        },
        (
            Ty::Pointer {
                pointee: gp,
                bits: gb,
                ..
            },
            Ty::Pointer {
                pointee: hp,
                bits: hb,
                ..
            },
        ) => {
            ensure!(
                !matches!(gp.resolved(), Ty::Function(_)),
                "a function pointer (a guest entry)"
            );
            let readable = untyped(gp, views.guest)
                || untyped(hp, views.host)
                || same(gp, views.guest, hp, views.host, false);
            let (guest, host) = (u64::from(gb / 8), u64::from(hb / 8));
            if readable {
                Op::Ptr { guest, host }
            } else {
                Op::Deep { guest, host }
            }
        }
        (Ty::Record { name } | Ty::Opaque { name, .. }, Ty::Record { .. } | Ty::Opaque { .. }) => {
            conv_into(name, views, out)?;
            Op::Record(name.clone())
        }
        (Ty::Array { elem: ge, len: gl }, Ty::Array { elem: he, len: hl }) if gl == hl => {
            Op::Array {
                len: *gl,
                elem: Box::new(field_op(ge, he, views, out)?),
                guest: size_align(ge, views.guest)?.0,
                host: size_align(he, views.host)?.0,
            }
        }
        _ => bail!("no field rule: guest {g:?} vs host {h:?}"),
    })
}

/// Size and alignment of `ty` in `v`: scalars are their own size and alignment (AAPCS32 and SysV x86_64 alike: natural alignment, 8-byte types 8-aligned), records the view's layout.
fn size_align(ty: &Ty, v: &ArchView) -> Result<(u64, u64)> {
    Ok(match ty.resolved() {
        Ty::Bool => (1, 1),
        Ty::Int { bits, .. }
        | Ty::Float { bits }
        | Ty::Pointer { bits, .. }
        | Ty::Reference { bits, .. } => (u64::from(bits / 8), u64::from(bits / 8)),
        Ty::Record { name } | Ty::Opaque { name, .. } => {
            let r = layout(v, name).with_context(|| format!("{name}: incomplete"))?;
            (r.size, r.align)
        }
        Ty::Array { elem, len } => {
            let (s, a) = size_align(elem, v)?;
            (s * len, a)
        }
        t => bail!("no size for {t:?}"),
    })
}

/// `converters` as Rust consts for the engine (`engine/convert.rs` types `Conv`, `Field`, `Op`): one `pub const C_<record>: Conv` each, then a const assert per record that its fields sit in declaration order on each side, within the size and at their alignment (C11 6.7.2.1p15: members at increasing addresses) -- the check a swapped or hand-edited offset fails at build time. `swap`: the cfg name of the red control that swaps the first two guest offsets of the first record with two fields.
pub fn converters_rs(convs: &[Conv], swap: &str) -> String {
    fn op_rs(op: &Op) -> String {
        match op {
            Op::Copy { bytes } => format!("Op::Copy({bytes})"),
            Op::Int {
                guest,
                host,
                signed,
            } => format!("Op::Int {{ guest: {guest}, host: {host}, signed: {signed} }}"),
            Op::Ptr { guest, host } => format!("Op::Ptr {{ guest: {guest}, host: {host} }}"),
            Op::Deep { guest, host } => format!("Op::Deep {{ guest: {guest}, host: {host} }}"),
            Op::Record(n) => format!("Op::Record(&C_{n})"),
            Op::Array {
                len,
                elem,
                guest,
                host,
            } => {
                format!(
                    "Op::Array {{ len: {len}, elem: &{}, guest: {guest}, host: {host} }}",
                    op_rs(elem)
                )
            }
        }
    }
    let mut s = String::new();
    let swapped = convs.iter().position(|c| c.fields.len() >= 2);
    for (ci, c) in convs.iter().enumerate() {
        s.push_str(&format!(
            "/// `{}`: guest {} bytes, host {} bytes (bridgegen ilp32 `converters`).\n#[allow(non_upper_case_globals, reason = \"named by the C record, which a grep finds\")]\npub const C_{}: Conv = Conv {{\n    name: {:?},\n    guest_size: {},\n    host_size: {},\n    fields: &[\n",
            c.name, c.guest_size, c.host_size, c.name, c.name, c.guest_size, c.host_size
        ));
        for (fi, f) in c.fields.iter().enumerate() {
            let guest_offset = if Some(ci) == swapped && fi < 2 {
                let other = c.fields[1 - fi].guest_offset;
                format!(
                    "if cfg!({swap}) {{ {other} }} else {{ {} }}",
                    f.guest_offset
                )
            } else {
                f.guest_offset.to_string()
            };
            s.push_str(&format!(
                "        Field {{ name: {:?}, guest_offset: {guest_offset}, host_offset: {}, guest_align: {}, host_align: {}, op: {} }},\n",
                f.name,
                f.host_offset,
                f.guest_align,
                f.host_align,
                op_rs(&f.op)
            ));
        }
        s.push_str(&format!(
            "    ],\n}};\nconst _: () = assert!(C_{0}.ordered(true) && C_{0}.ordered(false), \"C_{0}: a field out of declaration order, overlapping, past the size or misaligned\");\n\n",
            c.name
        ));
    }
    s
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
int sum(int a, double b);\n\
#include \"other.h\"\n\
int sigqueue(int pid, int sig, const union sigval value);\n\
int wide_user(union wide w);\n\
int getnameinfo(const struct sockaddr* sa, socklen_t sa_len, char* host, size_t host_len, char* serv, size_t serv_len, int flags);\n\
struct VaUser { int (*f)(void* p, __builtin_va_list ap); };\n\
int va_user(struct VaUser* u);\n\
int use_object(jobject o);\n\
int va_fn(const char* fmt, __builtin_va_list ap);\n\
int out_display(EGLDisplay* out);\n\
int out_asset(AAsset** out);\n\
struct _JNIEnv { const void* functions; };\nint env_user(struct _JNIEnv* env);\n";

    /// Defined outside the interface (`Ty::Opaque` + `ArchView::foreign`). `union sigval`: bionic uapi asm-generic/siginfo.h `{ int sival_int; void* sival_ptr; }`. `struct sockaddr`: bionic linux/socket.h `{ sa_family_t sa_family; char sa_data[14]; }`, 16 bytes on both. `socklen_t`: bionic sys/types.h `__kernel_socklen_t` = int; glibc bits/types.h `__socklen_t` = unsigned int.
    const OTHER: &str = "union sigval { int sival_int; void* sival_ptr; };\n\
union wide { long long w; void* p; };\n\
struct sockaddr { unsigned short sa_family; char sa_data[14]; };\n\
struct _jobject {};\ntypedef struct _jobject* jobject;\n\
#if defined(__arm__)\ntypedef int socklen_t;\n#else\ntypedef unsigned int socklen_t;\n#endif\n";

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
        std::fs::write(dir.0.join("other.h"), OTHER).unwrap();
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
        // void** outData: a pointer-width word behind a non-const pointer
        assert_eq!(
            row(&i, "ALooper_pollOnce").kinds[4],
            Kind::OutWord(Word::Ptr)
        );
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
                Kind::Array
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
        // size_t*: 4 bytes unsigned in bionic arm, 8 in glibc x86_64
        assert_eq!(
            row(&i, "pthread_attr_getstacksize").kinds[2],
            Kind::OutWord(Word::Int {
                guest: 4,
                host: 8,
                signed: false
            })
        );
        // EGLDisplay*: a handle the host writes (P4b s4)
        assert_eq!(row(&i, "out_display").kinds[1], Kind::Opaque);
        // AAsset**: an incomplete record the host hands out
        assert_eq!(row(&i, "out_asset").kinds[1], Kind::Opaque);
        assert_eq!(row(&i, "env_user").kinds[1], Kind::Jni);
        assert_eq!(row(&i, "AAsset_name").kinds[0], Kind::String);
        assert_eq!(
            row(&i, "ALooper_addFd").kinds[4],
            Kind::Callback("IIIL".to_owned())
        );
        assert!(row(&i, "sum").same());
        assert!(!row(&i, "ANativeWindow_lock").carried());
        assert!(row(&i, "ALooper_pollOnce").heaviest() == &Kind::OutWord(Word::Ptr));
    }

    /// The brief's gate numbers: `ANativeWindow_Buffer` guest offsets {0,4,8,12,16,20} size 44 (four int32, a 4-byte pointer, uint32[6]: AAPCS32 natural alignment) vs host {0,4,8,12,16,24} size 48 (the pointer 8-aligned and 8 wide); `bits` crosses as a pointer, the rest as copies.
    #[test]
    fn window_buffer_converter() {
        let i = views("conv", ARM);
        let (g, h) = (&i.arches["guest"], &i.arches["host"]);
        let v = Views {
            guest: g,
            host: h,
            handles: HANDLES,
        };
        let c = converters("ANativeWindow_Buffer", &v).unwrap();
        assert_eq!(c.len(), 1);
        let c = &c[0];
        assert_eq!((c.guest_size, c.host_size), (44, 48));
        let offs: Vec<(u64, u64)> = c
            .fields
            .iter()
            .map(|f| (f.guest_offset, f.host_offset))
            .collect();
        assert_eq!(offs, [(0, 0), (4, 4), (8, 8), (12, 12), (16, 16), (20, 24)]);
        assert_eq!(c.fields[4].op, Op::Ptr { guest: 4, host: 8 });
        assert_eq!(c.fields[5].op, Op::Copy { bytes: 24 });
        // timespec {time_t, long}: two widened longs (4 -> 8, signed)
        let t = &converters("timespec", &v).unwrap()[0];
        assert_eq!(
            t.fields[1].op,
            Op::Int {
                guest: 4,
                host: 8,
                signed: true
            }
        );
        // a union has no field rule
        assert!(converters("sigval", &v).is_err());
        let rs = converters_rs(std::slice::from_ref(c), "swap_cfg");
        assert!(
            rs.contains("guest_offset: if cfg!(swap_cfg) { 4 } else { 0 }"),
            "{rs}"
        );
    }

    /// Records another header defines compare by layout, never by name: `union sigval` is 4 bytes on arm (int / 4-byte pointer) and 8 on x86_64, one pointer wide in each, so by value it is one register: an unsigned word (`word_union`); `struct sockaddr` is 16 bytes in both, so a pointer to it is a plain data pointer.
    #[test]
    fn foreign_records_compare_by_layout() {
        let i = views("foreign", ARM);
        let (g, h) = (&i.arches["guest"], &i.arches["host"]);
        assert_eq!((g.foreign["sigval"].size, h.foreign["sigval"].size), (4, 8));
        assert!(!g.records.contains_key("sigval"));
        assert_eq!(row(&i, "sigqueue").kinds[3], Kind::Widen { signed: false });
        // its lane: one unsigned word of the guest's 32-bit pointer (AAPCS32: one core register); the 64-bit union has none
        assert_eq!(
            param_classes(&g.functions["sigqueue"], g).unwrap()[2],
            crate::jni::Class::Int {
                bits: 32,
                signed: false
            }
        );
        assert!(param_classes(&g.functions["wide_user"], g).is_err());
        // a pointer to it still converts field by field, and a union has no field rule
        assert!(
            converters(
                "sigval",
                &Views {
                    guest: g,
                    host: h,
                    handles: &[]
                }
            )
            .is_err()
        );
        // two words on arm (a 64-bit member): not one register, stays a record
        assert_eq!(
            row(&i, "wide_user").kinds[1],
            Kind::Record("wide".to_owned())
        );
        assert_eq!(row(&i, "getnameinfo").kinds[1], Kind::Ptr);
        // a field-less tag record (jni.h `_jobject`, here C's empty struct) stays a handle
        assert_eq!(row(&i, "use_object").kinds[1], Kind::Opaque);
    }

    /// A signedness-only difference at equal width is carried: `socklen_t` int32 (bionic arm) vs uint32 (glibc x86_64).
    #[test]
    fn equal_width_signedness_is_carried() {
        let i = views("signedness", ARM);
        let (g, h) = (&i.arches["guest"], &i.arches["host"]);
        let signed = |v: &ArchView| match v.functions["getnameinfo"].params[1].resolved() {
            Ty::Int {
                bits: 32, signed, ..
            } => *signed,
            t => panic!("{t:?}"),
        };
        assert_eq!((signed(g), signed(h)), (true, false));
        let gni = row(&i, "getnameinfo");
        assert_eq!(gni.kinds[2], Kind::Same);
        assert!(gni.carried());
    }

    /// va_list (arm64 `struct __va_list` 32 B, x86_64 `__va_list_tag[1]` 24 B): inside a callback type each side's own (a JNIEnv-like table stays a plain pointer); as a classified position it is refused unless the layouts agree.
    #[test]
    fn va_list_inside_callbacks_only() {
        let i = views("valist", "aarch64-linux-android");
        assert_eq!(row(&i, "va_user").kinds[1], Kind::Same);
        let (g, h) = (&i.arches["guest"], &i.arches["host"]);
        let v = Views {
            guest: g,
            host: h,
            handles: HANDLES,
        };
        let e = classify(
            "va_fn",
            &g.functions["va_fn"],
            &h.functions["va_fn"],
            &v,
            &mut BTreeSet::new(),
        )
        .unwrap_err();
        assert!(format!("{e:#}").contains("va_list"), "{e:#}");
        // equal targets: the same va_list passes
        let x = views("valist-x86", "x86_64-unknown-linux-gnu");
        assert!(row(&x, "va_fn").same());
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
