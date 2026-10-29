// SPDX-License-Identifier: MIT
//! libclang front end: one header parsed once per target -> that arch's view. Every size, alignment
//! and offset is clang's, never computed here.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Mutex;

use anyhow::{Context, Result, anyhow, bail, ensure};
use clang::diagnostic::Severity;
use clang::{Clang, Entity, EntityKind, Index, Type, TypeKind};

use crate::ir::{ArchView, Field, Nullability, Record, Signature, Ty};

/// One target to parse the header for.
pub struct Target {
    /// arches.toml key.
    pub arch: String,
    /// clang target triple.
    pub triple: String,
    /// Further compiler arguments: include directories, `-nostdlibinc` for a non-host sysroot.
    pub args: Vec<String>,
}

/// libclang allows one `Clang` per process at a time.
static CLANG: Mutex<()> = Mutex::new(());

/// The header's records and functions as each target sees them.
pub fn parse(header: &Path, targets: &[Target]) -> Result<BTreeMap<String, ArchView>> {
    let _one_at_a_time = CLANG
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let clang = Clang::new().map_err(|e| anyhow!("libclang: {e}"))?;
    let index = Index::new(&clang, false, false);
    targets
        .iter()
        .map(|t| {
            let view = parse_one(&index, header, t)
                .with_context(|| format!("{} for {}", header.display(), t.triple))?;
            Ok((t.arch.clone(), view))
        })
        .collect()
}

fn parse_one(index: &Index, header: &Path, t: &Target) -> Result<ArchView> {
    // GNU dialect as AOSP builds use: strict C hides POSIX types (siginfo_t) in glibc headers
    let mut args = vec!["-x".to_owned(), "c".to_owned(), "-std=gnu11".to_owned()];
    args.push(format!("--target={}", t.triple));
    args.extend(t.args.iter().cloned());
    let tu = index
        .parser(header)
        .arguments(&args)
        .skip_function_bodies(true)
        .parse()
        .map_err(|e| anyhow!("libclang could not parse: {e:?}"))?;
    let errors: Vec<String> = tu
        .get_diagnostics()
        .iter()
        .filter(|d| matches!(d.get_severity(), Severity::Error | Severity::Fatal))
        .map(|d| {
            let (file, line, _) = d.get_location().get_presumed_location();
            format!("{file}:{line}: {}", d.get_text())
        })
        .collect();
    ensure!(
        errors.is_empty(),
        "clang errors:\n  {}",
        errors.join("\n  ")
    );

    let mut view = ArchView {
        triple: t.triple.clone(),
        records: BTreeMap::new(),
        functions: BTreeMap::new(),
    };
    for e in tu.get_entity().get_children() {
        if !e.is_in_main_file() {
            continue;
        }
        match e.get_kind() {
            EntityKind::StructDecl if e.is_definition() => {
                // an anonymous struct is named by its typedef, below
                if let Some(name) = e.get_name() {
                    view.records.insert(name, record(&e)?);
                }
            }
            EntityKind::TypedefDecl => {
                let decl = e
                    .get_typedef_underlying_type()
                    .and_then(|u| u.get_canonical_type().get_declaration());
                if let (Some(d), Some(name)) = (decl, e.get_name())
                    && d.get_kind() == EntityKind::StructDecl
                    && d.is_definition()
                    && d.get_name().is_none()
                {
                    view.records.insert(name, record(&d)?);
                }
            }
            EntityKind::FunctionDecl => {
                let name = e.get_name().context("unnamed function")?;
                let t = e.get_type().with_context(|| format!("{name}: no type"))?;
                view.functions
                    .insert(name.clone(), signature(&t).with_context(|| name.clone())?);
            }
            _ => {}
        }
    }
    Ok(view)
}

fn record(e: &Entity) -> Result<Record> {
    let name = e.get_name().unwrap_or_else(|| "(anonymous)".to_owned());
    let t = e.get_type().with_context(|| format!("{name}: no type"))?;
    let size = t
        .get_sizeof()
        .map_err(|err| anyhow!("{name}: sizeof: {err:?}"))?;
    let align = t
        .get_alignof()
        .map_err(|err| anyhow!("{name}: alignof: {err:?}"))?;
    let mut fields = Vec::new();
    for f in e
        .get_children()
        .iter()
        .filter(|c| c.get_kind() == EntityKind::FieldDecl)
    {
        let field = f
            .get_name()
            .with_context(|| format!("{name}: unnamed field"))?;
        ensure!(
            !f.is_bit_field(),
            "{name}.{field}: bit-fields are not supported"
        );
        let bits = t
            .get_offsetof(&field)
            .map_err(|err| anyhow!("{name}.{field}: offsetof: {err:?}"))?;
        ensure!(bits % 8 == 0, "{name}.{field}: offset is not whole bytes");
        let ft = f
            .get_type()
            .with_context(|| format!("{name}.{field}: no type"))?;
        let ty = ty(&ft).with_context(|| format!("{name}.{field}"))?;
        fields.push(Field {
            name: field,
            offset: (bits / 8) as u64,
            ty,
        });
    }
    Ok(Record {
        size: size as u64,
        align: align as u64,
        fields,
    })
}

fn ty(t: &Type) -> Result<Ty> {
    let nullability = match t.get_nullability() {
        Some(clang::Nullability::NonNull) => Nullability::NonNull,
        Some(clang::Nullability::Nullable) => Nullability::Nullable,
        _ => Nullability::Unspecified,
    };
    let c = t.get_canonical_type();
    let bits = || -> Result<u32> {
        let bytes = c
            .get_sizeof()
            .map_err(|e| anyhow!("{}: sizeof: {e:?}", c.get_display_name()))?;
        Ok(u32::try_from(bytes * 8)?)
    };
    Ok(match c.get_kind() {
        TypeKind::Void => Ty::Void,
        TypeKind::Bool => Ty::Bool,
        TypeKind::CharS
        | TypeKind::SChar
        | TypeKind::Short
        | TypeKind::Int
        | TypeKind::Long
        | TypeKind::LongLong
        | TypeKind::Int128 => Ty::Int {
            bits: bits()?,
            signed: true,
        },
        TypeKind::CharU
        | TypeKind::UChar
        | TypeKind::UShort
        | TypeKind::UInt
        | TypeKind::ULong
        | TypeKind::ULongLong
        | TypeKind::UInt128
        | TypeKind::Char16
        | TypeKind::Char32 => Ty::Int {
            bits: bits()?,
            signed: false,
        },
        TypeKind::Float | TypeKind::Double | TypeKind::LongDouble | TypeKind::Float128 => {
            Ty::Float { bits: bits()? }
        }
        TypeKind::Enum => {
            let decl = c.get_declaration().context("enum without declaration")?;
            let int = decl
                .get_enum_underlying_type()
                .context("enum without underlying type")?;
            ty(&int)?
        }
        TypeKind::Pointer => {
            // the written pointee keeps nullability annotations; the canonical one is the fallback
            let pointee = t
                .get_pointee_type()
                .or_else(|| c.get_pointee_type())
                .context("pointer without pointee")?;
            Ty::Pointer {
                bits: bits()?,
                is_const: pointee.is_const_qualified(),
                pointee: Box::new(ty(&pointee)?),
                nullability,
            }
        }
        TypeKind::Record => {
            let decl = c.get_declaration().context("record without declaration")?;
            let name = decl
                .get_name()
                .with_context(|| format!("anonymous record {}", c.get_display_name()))?;
            if decl.is_in_main_file() && c.get_sizeof().is_ok() {
                Ty::Record { name }
            } else {
                Ty::Opaque { name }
            }
        }
        TypeKind::FunctionPrototype | TypeKind::FunctionNoPrototype => {
            Ty::Function(Box::new(signature(t)?))
        }
        TypeKind::ConstantArray => {
            let elem = c.get_element_type().context("array without element type")?;
            let len = c.get_size().context("array without length")?;
            Ty::Array {
                elem: Box::new(ty(&elem)?),
                len: len as u64,
            }
        }
        other => bail!("{}: unsupported type kind {other:?}", t.get_display_name()),
    })
}

fn signature(t: &Type) -> Result<Signature> {
    let c = t.get_canonical_type();
    let ret = c
        .get_result_type()
        .context("function without result type")?;
    // `()` in these headers means no parameters (they are written for C++ callers such as ART), not
    // C's unprototyped function
    if c.get_kind() == TypeKind::FunctionNoPrototype {
        return Ok(Signature {
            ret: ty(&ret)?,
            params: Vec::new(),
            variadic: false,
        });
    }
    // written parameter types keep nullability; fall back to canonical ones
    let params = t
        .get_argument_types()
        .or_else(|| c.get_argument_types())
        .unwrap_or_default();
    Ok(Signature {
        ret: ty(&ret)?,
        params: params.iter().map(ty).collect::<Result<_>>()?,
        variadic: c.is_variadic(),
    })
}
