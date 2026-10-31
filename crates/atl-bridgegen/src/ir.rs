// SPDX-License-Identifier: MIT
//! The C-ABI IR: one view per arch, every size/align/offset as clang computed it for that arch.

use std::collections::BTreeMap;

use serde::Serialize;

/// One interface (a header) seen from every requested arch.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Interface {
    pub name: String,
    /// Header path relative to the upstream source tree.
    pub header: String,
    /// View label -> that target's view. The label is the arches.toml arch; when one arch is seen from
    /// both sides (glibc host and bionic guest), the caller tells them apart, e.g. `guest-x86_64`.
    pub arches: BTreeMap<String, ArchView>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ArchView {
    /// clang target triple the view was computed for.
    pub triple: String,
    /// Records (structs) defined by the interface's header, by tag name.
    pub records: BTreeMap<String, Record>,
    /// Functions declared by the interface's header.
    pub functions: BTreeMap<String, Signature>,
    /// Integer constants: the header's enumerators plus the object-like macros the caller asked for,
    /// each as clang evaluated it for this target (ioctl numbers embed sizes, so they are per arch).
    pub constants: BTreeMap<String, Constant>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Record {
    pub kind: RecordKind,
    pub size: u64,
    pub align: u64,
    /// `__attribute__((packed))`: fields may sit below their natural alignment.
    pub packed: bool,
    /// `__attribute__((aligned(n)))`: alignment raised above the natural one.
    pub aligned: bool,
    /// Declaration order. An anonymous member (`union { ... };`) is a field named `anon<i>` whose
    /// record is `<parent>_anon<i>`; an unnamed type of a named field is `<parent>_<field>`.
    pub fields: Vec<Field>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordKind {
    Struct,
    Union,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Constant {
    /// Always `Ty::Int`.
    pub ty: Ty,
    /// Two's-complement bits of the value, truncated to the type's width.
    pub bits: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Field {
    pub name: String,
    pub offset: u64,
    pub ty: Ty,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Signature {
    pub ret: Ty,
    pub params: Vec<Ty>,
    pub variadic: bool,
}

/// A C type after typedefs are resolved; widths are this arch's.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Ty {
    Void,
    Bool,
    Int {
        bits: u32,
        signed: bool,
    },
    Float {
        bits: u32,
    },
    Pointer {
        /// This arch's pointer width.
        bits: u32,
        pointee: Box<Ty>,
        /// `const` pointee.
        is_const: bool,
        nullability: Nullability,
    },
    /// A record defined by this interface, by value.
    Record {
        name: String,
    },
    /// A record defined elsewhere (or never): only usable behind a pointer.
    Opaque {
        name: String,
    },
    Function(Box<Signature>),
    Array {
        elem: Box<Ty>,
        len: u64,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Nullability {
    NonNull,
    Nullable,
    Unspecified,
}
