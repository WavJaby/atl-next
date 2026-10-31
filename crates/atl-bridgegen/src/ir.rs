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
    /// Integer constants: the header's enumerators, its `const` integer variables (C++ `constexpr`;
    /// a class member as `<record>::<name>`), plus the object-like macros the caller asked for, each as
    /// clang evaluated it for this target (ioctl numbers embed sizes, so they are per arch).
    pub constants: BTreeMap<String, Constant>,
    /// String constants: requested object-like macros that are string literals (`PROP_DIRNAME`) and
    /// `const char[]` variables, named as `constants` are.
    pub strings: BTreeMap<String, String>,
    /// Typedefs the interface declares: name -> the type it names (`prop_info` -> opaque struct).
    pub typedefs: BTreeMap<String, Ty>,
    /// `#include` directives of the parsed file, in order, as written (`<stdint.h>`).
    pub includes: Vec<String>,
    /// Function-like macros the interface's files define, as written (`NAME(args) body`).
    pub function_macros: BTreeMap<String, String>,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
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

/// A C type; widths are this arch's. Typedef names are kept (`Typedef`) for printers that write C;
/// layout and identity questions look through them (`resolved`).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Ty {
    Void,
    Bool,
    Int {
        bits: u32,
        signed: bool,
        /// The builtin type: same bits are distinct types to C++ (`long` / `long long` on LP64, plain
        /// `char` / `signed char`), so a printed declaration must name this one.
        c: CInt,
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
        /// `struct` or `union`: C names the type by it.
        record: RecordKind,
    },
    Function(Box<Signature>),
    Array {
        elem: Box<Ty>,
        len: u64,
    },
    /// A typedef name as written (`uint32_t`, `prop_info`) and the type it stands for.
    Typedef {
        name: String,
        ty: Box<Ty>,
    },
}

impl Ty {
    /// The type behind any typedef names.
    pub fn resolved(&self) -> &Ty {
        match self {
            Ty::Typedef { ty, .. } => ty.resolved(),
            t => t,
        }
    }
}

/// C builtin integer types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CInt {
    Char,
    SignedChar,
    UnsignedChar,
    Short,
    UnsignedShort,
    Int,
    UnsignedInt,
    Long,
    UnsignedLong,
    LongLong,
    UnsignedLongLong,
    Int128,
    UnsignedInt128,
    Char16,
    Char32,
}

impl CInt {
    /// The type's C spelling.
    pub fn c_name(self) -> &'static str {
        match self {
            CInt::Char => "char",
            CInt::SignedChar => "signed char",
            CInt::UnsignedChar => "unsigned char",
            CInt::Short => "short",
            CInt::UnsignedShort => "unsigned short",
            CInt::Int => "int",
            CInt::UnsignedInt => "unsigned int",
            CInt::Long => "long",
            CInt::UnsignedLong => "unsigned long",
            CInt::LongLong => "long long",
            CInt::UnsignedLongLong => "unsigned long long",
            CInt::Int128 => "__int128",
            CInt::UnsignedInt128 => "unsigned __int128",
            CInt::Char16 => "char16_t",
            CInt::Char32 => "char32_t",
        }
    }

    /// Suffix that gives an integer literal this type, where C has one.
    pub fn literal_suffix(self) -> Option<&'static str> {
        match self {
            CInt::Int => Some(""),
            CInt::UnsignedInt => Some("U"),
            CInt::Long => Some("L"),
            CInt::UnsignedLong => Some("UL"),
            CInt::LongLong => Some("LL"),
            CInt::UnsignedLongLong => Some("ULL"),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Nullability {
    NonNull,
    Nullable,
    Unspecified,
}
