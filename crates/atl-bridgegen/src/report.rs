// SPDX-License-Identifier: MIT
//! Backend: the ABI-identity report. Two arches agree on a record when size, alignment and every
//! field's offset and type agree; that is what lets a value pass between them unconverted. Pass-through
//! is derived here, never annotated.

use serde::Serialize;

use crate::ir::{Interface, Record, Signature, Ty};

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
        } else if !same_ty(&fa.ty, &fb.ty) {
            d.push(format!("{}: type differs", fa.name));
        }
    }
    d
}

/// Representation equality: widths and pointee structure. Signedness, nullability and constness do
/// not change the bytes (plain `char` is signed on x86 and unsigned on Arm, same 8 bits).
fn same_ty(a: &Ty, b: &Ty) -> bool {
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
