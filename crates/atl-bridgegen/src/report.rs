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
    pub identical: bool,
    /// Why not, one line per difference.
    pub differences: Vec<String>,
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
            let differences = match (va.functions.get(name), vb.functions.get(name)) {
                (Some(sa), Some(sb)) => sig_differences(sa, sb, va, vb),
                (Some(_), None) => vec![format!("only on {a}")],
                (None, _) => vec![format!("only on {b}")],
            };
            FunctionDiff {
                name: name.clone(),
                identical: differences.is_empty(),
                differences,
            }
        })
        .collect()
}

fn sig_differences(a: &Signature, b: &Signature, va: &ArchView, vb: &ArchView) -> Vec<String> {
    let mut d = Vec::new();
    if a.variadic != b.variadic {
        d.push("variadic differs".to_owned());
    }
    if a.params.len() != b.params.len() {
        d.push(format!(
            "{} parameters on one view, {} on the other",
            a.params.len(),
            b.params.len()
        ));
        return d;
    }
    if let Some(x) = ty_difference(&a.ret, &b.ret, va, vb) {
        d.push(format!("return: {x}"));
    }
    for (i, (x, y)) in a.params.iter().zip(&b.params).enumerate() {
        if let Some(x) = ty_difference(x, y, va, vb) {
            d.push(format!("parameter {i}: {x}"));
        }
    }
    d
}

/// None when `a` (of view `va`) and `b` (of `vb`) agree in the ABI; else what differs.
fn ty_difference(a: &Ty, b: &Ty, va: &ArchView, vb: &ArchView) -> Option<String> {
    let (a, b) = (a.resolved(), b.resolved());
    match (a, b) {
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
                return Some(format!("pointer {ba} vs {bb} bits"));
            }
            if is_opaque(pa) || is_opaque(pb) {
                return None;
            }
            ty_difference(pa, pb, va, vb).map(|d| format!("pointer to {d}"))
        }
        // a pointer and an integer of its width share the INTEGER class (SysV: one register, same bits); a function pointer is not an integer
        (
            Ty::Pointer {
                bits: bp,
                pointee,
                ..
            },
            Ty::Int { bits: bi, .. },
        )
        | (
            Ty::Int { bits: bi, .. },
            Ty::Pointer {
                bits: bp,
                pointee,
                ..
            },
        ) => {
            if matches!(pointee.resolved(), Ty::Function(_)) {
                Some("function pointer vs integer".to_owned())
            } else {
                (bp != bi).then(|| format!("pointer {bp} bits vs integer {bi} bits"))
            }
        }
        (Ty::Function(sa), Ty::Function(sb)) => {
            let d = sig_differences(sa, sb, va, vb);
            (!d.is_empty()).then(|| format!("function: {}", d.join("; ")))
        }
        (Ty::Record { name: na }, Ty::Record { name: nb }) => {
            let (Some(ra), Some(rb)) = (va.records.get(na), vb.records.get(nb)) else {
                return Some(format!("record {na} / {nb} missing on a view"));
            };
            (ra.size != rb.size || ra.align != rb.align).then(|| {
                format!(
                    "record {na}: size {} align {} vs size {} align {}",
                    ra.size, ra.align, rb.size, rb.align
                )
            })
        }
        (Ty::Opaque { name: na, .. }, Ty::Opaque { name: nb, .. }) => {
            (na != nb).then(|| format!("opaque {na} vs {nb}"))
        }
        _ => Some(format!("{a:?} vs {b:?}")),
    }
}

/// A type with no layout in this view: `void`, or a record defined only as a tag. Behind a pointer it is an opaque handle.
fn is_opaque(t: &Ty) -> bool {
    matches!(t.resolved(), Ty::Void | Ty::Opaque { .. })
}
