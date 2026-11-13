// SPDX-License-Identifier: MIT
//! Backend: the ABI-identity report. Two arches agree on a record when size, alignment and every
//! field's offset and type agree; that is what lets a value pass between them unconverted. Pass-through
//! is derived here, never annotated.

use serde::Serialize;

use crate::ir::{ArchView, Interface, Record, Signature, Ty};

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RecordDiff {
    pub record: String,
    pub identical: bool,
    /// Why not, one line per difference.
    pub differences: Vec<String>,
}

/// Every record of `a` and `b`, compared.
pub fn compare(iface: &Interface, a: &str, b: &str) -> Vec<RecordDiff> {
    let (Some(va), Some(vb)) = (iface.arches.get(a), iface.arches.get(b)) else {
        return Vec::new();
    };
    let names: std::collections::BTreeSet<&String> =
        va.records.keys().chain(vb.records.keys()).collect();
    names
        .into_iter()
        .map(|name| {
            let differences = match (va.records.get(name), vb.records.get(name)) {
                (Some(ra), Some(rb)) => record_differences(ra, rb, a, b),
                (Some(_), None) => vec![format!("only on {a}")],
                (None, _) => vec![format!("only on {b}")],
            };
            RecordDiff {
                record: name.clone(),
                identical: differences.is_empty(),
                differences,
            }
        })
        .collect()
}

fn record_differences(ra: &Record, rb: &Record, a: &str, b: &str) -> Vec<String> {
    let mut d = Vec::new();
    if ra.kind != rb.kind {
        d.push(format!("{:?} on {a}, {:?} on {b}", ra.kind, rb.kind));
    }
    if ra.size != rb.size {
        d.push(format!("size {} on {a}, {} on {b}", ra.size, rb.size));
    }
    if ra.align != rb.align {
        d.push(format!("align {} on {a}, {} on {b}", ra.align, rb.align));
    }
    if ra.fields.len() != rb.fields.len() {
        d.push(format!(
            "{} fields on {a}, {} on {b}",
            ra.fields.len(),
            rb.fields.len()
        ));
    }
    for (fa, fb) in ra.fields.iter().zip(&rb.fields) {
        if fa.name != fb.name {
            d.push(format!("field {} on {a} is {} on {b}", fa.name, fb.name));
        } else if fa.offset != fb.offset {
            d.push(format!(
                "{}: offset {} on {a}, {} on {b}",
                fa.name, fa.offset, fb.offset
            ));
        } else if fa.bits != fb.bits {
            d.push(format!(
                "{}: bits {:?} on {a}, {:?} on {b}",
                fa.name, fa.bits, fb.bits
            ));
        } else if !same_ty(&fa.ty, &fb.ty) {
            d.push(format!("{}: type differs", fa.name));
        }
    }
    d
}

/// Representation equality: widths and pointee structure. Typedef names, signedness, nullability and
/// constness do not change the bytes (plain `char` is signed on x86 and unsigned on Arm, same 8 bits).
fn same_ty(a: &Ty, b: &Ty) -> bool {
    let (a, b) = (a.resolved(), b.resolved());
    match (a, b) {
        (Ty::Int { bits: ba, .. }, Ty::Int { bits: bb, .. }) => ba == bb,
        (
            Ty::Pointer {
                bits: ba,
                pointee: pa,
                ..
            },
            Ty::Pointer {
                bits: bb,
                pointee: pb,
                ..
            },
        ) => ba == bb && same_ty(pa, pb),
        (
            Ty::Reference {
                bits: ba,
                pointee: pa,
                ..
            },
            Ty::Reference {
                bits: bb,
                pointee: pb,
                ..
            },
        ) => ba == bb && same_ty(pa, pb),
        (Ty::Function(sa), Ty::Function(sb)) => same_sig(sa, sb),
        (Ty::Array { elem: ea, len: la }, Ty::Array { elem: eb, len: lb }) => {
            la == lb && same_ty(ea, eb)
        }
        _ => a == b,
    }
}

fn same_sig(a: &Signature, b: &Signature) -> bool {
    a.variadic == b.variadic
        && a.params.len() == b.params.len()
        && same_ty(&a.ret, &b.ret)
        && a.params.iter().zip(&b.params).all(|(x, y)| same_ty(x, y))
}

/// One function that `a` or `b` declares, compared as a prototype.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FunctionDiff {
    pub name: String,
    /// Every place the two prototypes differ, ABI-neutral ones included: a marshaller still needs them.
    pub differences: Vec<Difference>,
}

impl FunctionDiff {
    /// A call passes between the views unconverted: no difference, or only ABI-neutral ones.
    pub fn abi_identical(&self) -> bool {
        self.differences.iter().all(|d| d.kind.abi_neutral())
    }
}

/// Where in a prototype two views differ, and how.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Difference {
    /// `return`, `parameter 2`, nested with ` -> ` (`parameter 0 -> pointee`, `parameter 1 -> parameter 0`).
    pub at: String,
    pub kind: TyDiff,
}

impl std::fmt::Display for Difference {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.kind {
            TyDiff::PointerVsInteger {
                pointer_bits,
                integer_bits,
            } => write!(
                f,
                "{}: pointer {pointer_bits} bits vs integer {integer_bits} bits",
                self.at
            ),
            TyDiff::Mismatch { what } => write!(f, "{}: {what}", self.at),
        }
    }
}

/// How two types differ.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TyDiff {
    /// A data pointer on one side, an integer of the same width on the other (EGLNativePixmapType: a
    /// pointer in bionic's eglplatform.h, `unsigned long` in glibc's). ABI-neutral: SysV x86-64 psABI
    /// §3.2.3 classifies both as INTEGER (same register or stack eightbyte), and §3.1.2 gives both size
    /// and alignment 8 in memory; the AAPCS64 equivalent puts both in an X register. Kept as its own kind
    /// so a marshaller between unequal ABIs (the QEMU bridge) still sees a pointer it may have to map.
    PointerVsInteger {
        pointer_bits: u32,
        integer_bits: u32,
    },
    /// Anything else: the call cannot pass unconverted.
    Mismatch { what: String },
}

impl TyDiff {
    fn abi_neutral(&self) -> bool {
        match self {
            TyDiff::PointerVsInteger {
                pointer_bits,
                integer_bits,
            } => pointer_bits == integer_bits,
            TyDiff::Mismatch { .. } => false,
        }
    }
}

/// Every function of `a` and `b`, compared: return and parameter classes, widths, signedness and
/// function-pointer-ness. A pointer to a record that is incomplete in either view is an opaque handle
/// and agrees with any pointer of the same width.
pub fn compare_functions(iface: &Interface, a: &str, b: &str) -> Vec<FunctionDiff> {
    let (Some(va), Some(vb)) = (iface.arches.get(a), iface.arches.get(b)) else {
        return Vec::new();
    };
    let names: std::collections::BTreeSet<&String> =
        va.functions.keys().chain(vb.functions.keys()).collect();
    names
        .into_iter()
        .map(|name| {
            let mut differences = Vec::new();
            match (va.functions.get(name), vb.functions.get(name)) {
                (Some(sa), Some(sb)) => sig_differences(sa, sb, va, vb, "", &mut differences),
                (Some(_), None) => mismatch(&mut differences, "", format!("only on {a}")),
                (None, _) => mismatch(&mut differences, "", format!("only on {b}")),
            }
            FunctionDiff {
                name: name.clone(),
                differences,
            }
        })
        .collect()
}

/// `prefix` + `part`, joined with ` -> ` below the top level.
fn path(prefix: &str, part: &str) -> String {
    match (prefix.is_empty(), part.is_empty()) {
        (true, _) => part.to_owned(),
        (false, true) => prefix.to_owned(),
        (false, false) => format!("{prefix} -> {part}"),
    }
}

fn mismatch(out: &mut Vec<Difference>, at: &str, what: String) {
    out.push(Difference {
        at: if at.is_empty() {
            "prototype".to_owned()
        } else {
            at.to_owned()
        },
        kind: TyDiff::Mismatch { what },
    });
}

fn sig_differences(
    a: &Signature,
    b: &Signature,
    va: &ArchView,
    vb: &ArchView,
    at: &str,
    out: &mut Vec<Difference>,
) {
    if a.variadic != b.variadic {
        mismatch(out, at, "variadic differs".to_owned());
    }
    if a.params.len() != b.params.len() {
        mismatch(
            out,
            at,
            format!(
                "{} parameters on one view, {} on the other",
                a.params.len(),
                b.params.len()
            ),
        );
        return;
    }
    ty_difference(&a.ret, &b.ret, va, vb, &path(at, "return"), out);
    for (i, (x, y)) in a.params.iter().zip(&b.params).enumerate() {
        ty_difference(x, y, va, vb, &path(at, &format!("parameter {i}")), out);
    }
}

/// Appends to `out` where `a` (of view `va`) and `b` (of `vb`) differ, at `at`; nothing when they agree.
fn ty_difference(
    a: &Ty,
    b: &Ty,
    va: &ArchView,
    vb: &ArchView,
    at: &str,
    out: &mut Vec<Difference>,
) {
    let (a, b) = (a.resolved(), b.resolved());
    let what = match (a, b) {
        (Ty::Void, Ty::Void) | (Ty::Bool, Ty::Bool) => None,
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
        ) => (ba != bb || sa != sb)
            .then(|| format!("integer {ba} bits signed={sa} vs {bb} bits signed={sb}")),
        (Ty::Float { bits: ba }, Ty::Float { bits: bb }) => {
            (ba != bb).then(|| format!("float {ba} vs {bb} bits"))
        }
        (
            Ty::Pointer {
                bits: ba,
                pointee: pa,
                ..
            },
            Ty::Pointer {
                bits: bb,
                pointee: pb,
                ..
            },
        ) => {
            if ba != bb {
                Some(format!("pointer {ba} vs {bb} bits"))
            } else if is_opaque(pa) || is_opaque(pb) {
                None
            } else {
                return ty_difference(pa, pb, va, vb, &path(at, "pointee"), out);
            }
        }
        // a C++ reference is passed as a pointer of its width (ir.rs `Ty::Reference`): the same rule as a pointer (NDK surface_control.h `const ARect&`)
        (
            Ty::Reference {
                bits: ba,
                pointee: pa,
                ..
            },
            Ty::Reference {
                bits: bb,
                pointee: pb,
                ..
            },
        ) => {
            if ba != bb {
                Some(format!("reference {ba} vs {bb} bits"))
            } else if is_opaque(pa) || is_opaque(pb) {
                None
            } else {
                return ty_difference(pa, pb, va, vb, &path(at, "referent"), out);
            }
        }
        // an array (a parameter's `const uint8_t uuid[16]`, NDK NdkMediaCrypto.h, decays to a pointer; a field's is inline): the same length of the same element
        (Ty::Array { elem: ea, len: la }, Ty::Array { elem: eb, len: lb }) => {
            if la != lb {
                Some(format!("array of {la} vs {lb}"))
            } else {
                return ty_difference(ea, eb, va, vb, &path(at, "element"), out);
            }
        }
        // data pointer vs integer: its own kind (TyDiff::PointerVsInteger); a function pointer is code, never an integer
        (
            Ty::Pointer {
                bits: bp, pointee, ..
            },
            Ty::Int { bits: bi, .. },
        )
        | (
            Ty::Int { bits: bi, .. },
            Ty::Pointer {
                bits: bp, pointee, ..
            },
        ) => {
            if matches!(pointee.resolved(), Ty::Function(_)) {
                Some("function pointer vs integer".to_owned())
            } else {
                out.push(Difference {
                    at: at.to_owned(),
                    kind: TyDiff::PointerVsInteger {
                        pointer_bits: *bp,
                        integer_bits: *bi,
                    },
                });
                None
            }
        }
        (Ty::Function(sa), Ty::Function(sb)) => {
            return sig_differences(sa, sb, va, vb, at, out);
        }
        (Ty::Record { name: na }, Ty::Record { name: nb }) => {
            match (va.records.get(na), vb.records.get(nb)) {
                (Some(ra), Some(rb)) => (ra.size != rb.size || ra.align != rb.align).then(|| {
                    format!(
                        "record {na}: size {} align {} vs size {} align {}",
                        ra.size, ra.align, rb.size, rb.align
                    )
                }),
                _ => Some(format!("record {na} / {nb} missing on a view")),
            }
        }
        (Ty::Opaque { name: na, .. }, Ty::Opaque { name: nb, .. }) => {
            (na != nb).then(|| format!("opaque {na} vs {nb}"))
        }
        _ => Some(format!("{a:?} vs {b:?}")),
    };
    if let Some(what) = what {
        mismatch(out, at, what);
    }
}

/// A type with no layout in this view: `void`, or a record defined only as a tag. Behind a pointer it is an opaque handle.
fn is_opaque(t: &Ty) -> bool {
    matches!(t.resolved(), Ty::Void | Ty::Opaque { .. })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{CInt, Nullability};

    fn ulong() -> Ty {
        Ty::Int {
            bits: 64,
            signed: false,
            c: CInt::UnsignedLong,
        }
    }

    fn ptr(pointee: Ty) -> Ty {
        Ty::Pointer {
            bits: 64,
            pointee: Box::new(pointee),
            is_const: false,
            nullability: Nullability::Unspecified,
        }
    }

    /// `f(param)` on guest and host, compared.
    fn diff(guest: Ty, host: Ty) -> FunctionDiff {
        let view = |p: Ty| ArchView {
            triple: String::new(),
            records: Default::default(),
            functions: [(
                "f".to_owned(),
                Signature {
                    ret: Ty::Void,
                    params: vec![p],
                    variadic: false,
                },
            )]
            .into(),
            weak: Default::default(),
            constants: Default::default(),
            strings: Default::default(),
            typedefs: Default::default(),
            includes: Vec::new(),
            function_macros: Default::default(),
        };
        let iface = Interface {
            name: "t".to_owned(),
            header: "t.h".to_owned(),
            arches: [
                ("guest".to_owned(), view(guest)),
                ("host".to_owned(), view(host)),
            ]
            .into(),
        };
        compare_functions(&iface, "guest", "host").remove(0)
    }

    #[test]
    fn data_pointer_vs_same_width_integer_is_its_own_neutral_kind() {
        // EGLNativePixmapType: bionic `egl_native_pixmap_t*`, glibc `unsigned long`
        let d = diff(ptr(Ty::Void), ulong());
        assert_eq!(
            d.differences,
            vec![Difference {
                at: "parameter 0".to_owned(),
                kind: TyDiff::PointerVsInteger {
                    pointer_bits: 64,
                    integer_bits: 64
                }
            }]
        );
        assert!(d.abi_identical());
    }

    #[test]
    fn pointer_vs_narrower_integer_is_not_identical() {
        let narrow = Ty::Int {
            bits: 32,
            signed: false,
            c: CInt::UnsignedInt,
        };
        assert!(!diff(ptr(Ty::Void), narrow).abi_identical());
    }

    #[test]
    fn function_pointer_vs_integer_is_a_mismatch() {
        let fnptr = ptr(Ty::Function(Box::new(Signature {
            ret: Ty::Void,
            params: Vec::new(),
            variadic: false,
        })));
        let d = diff(fnptr, ulong());
        assert!(matches!(
            d.differences[..],
            [Difference {
                kind: TyDiff::Mismatch { .. },
                ..
            }]
        ));
        assert!(!d.abi_identical());
    }

    #[test]
    fn widened_integer_names_the_parameter() {
        let d = diff(
            ulong(),
            Ty::Int {
                bits: 32,
                signed: false,
                c: CInt::UnsignedInt,
            },
        );
        assert!(!d.abi_identical());
        assert!(
            d.differences[0]
                .to_string()
                .starts_with("parameter 0: integer 64"),
            "{}",
            d.differences[0]
        );
    }
}
