// SPDX-License-Identifier: MIT
//! libclang front end: one header parsed once per target -> that arch's view. Every size, alignment,
//! offset and constant value is clang's, never computed here.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Mutex;

use anyhow::{Context, Result, anyhow, bail, ensure};
use clang::diagnostic::Severity;
use clang::{
    Clang, Entity, EntityKind, EvaluationResult, Index, TranslationUnit, Type, TypeKind, Unsaved,
};

use crate::ir::{ArchView, Constant, Field, Nullability, Record, RecordKind, Signature, Ty};

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

/// Prefix of the probe variables that evaluate requested macros; nothing in a C header uses it.
const MACRO_PROBE: &str = "__atl_bridgegen_macro_";

/// The header's records, functions and constants as each target sees them. `macros`: object-like
/// macros (from the header or anything it includes) to evaluate as integer constants; a name that is
/// not an integer constant expression fails.
pub fn parse(
    header: &Path,
    macros: &[&str],
    targets: &[Target],
) -> Result<BTreeMap<String, ArchView>> {
    let _one_at_a_time = CLANG
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let clang = Clang::new().map_err(|e| anyhow!("libclang: {e}"))?;
    let index = Index::new(&clang, false, false);
    targets
        .iter()
        .map(|t| {
            let view = parse_one(&index, header, macros, t)
                .with_context(|| format!("{} for {}", header.display(), t.triple))?;
            Ok((t.arch.clone(), view))
        })
        .collect()
}

fn args(t: &Target) -> Vec<String> {
    // GNU dialect as AOSP builds use: strict C hides POSIX types (siginfo_t) in glibc headers
    let mut args = vec!["-x".to_owned(), "c".to_owned(), "-std=gnu11".to_owned()];
    args.push(format!("--target={}", t.triple));
    args.extend(t.args.iter().cloned());
    args
}

fn errors(tu: &TranslationUnit) -> Result<()> {
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
    Ok(())
}

fn parse_one(index: &Index, header: &Path, macros: &[&str], t: &Target) -> Result<ArchView> {
    let tu = index
        .parser(header)
        .arguments(&args(t))
        .skip_function_bodies(true)
        .parse()
        .map_err(|e| anyhow!("libclang could not parse: {e:?}"))?;
    errors(&tu)?;

    let mut view = ArchView {
        triple: t.triple.clone(),
        records: BTreeMap::new(),
        functions: BTreeMap::new(),
        constants: BTreeMap::new(),
    };
    for e in tu.get_entity().get_children() {
        if !e.is_in_main_file() {
            continue;
        }
        match e.get_kind() {
            EntityKind::StructDecl | EntityKind::UnionDecl if e.is_definition() => {
                // an anonymous record is named by its typedef (below) or by the field using it
                if !e.is_anonymous()
                    && let Some(name) = e.get_name()
                {
                    record(&e, &name, &mut view.records)?;
                }
            }
            EntityKind::TypedefDecl => {
                let decl = e
                    .get_typedef_underlying_type()
                    .and_then(|u| u.get_canonical_type().get_declaration());
                if let (Some(d), Some(name)) = (decl, e.get_name())
                    && matches!(d.get_kind(), EntityKind::StructDecl | EntityKind::UnionDecl)
                    && d.is_definition()
                    && d.is_anonymous()
                {
                    record(&d, &name, &mut view.records)?;
                }
            }
            EntityKind::EnumDecl => {
                let int = e
                    .get_enum_underlying_type()
                    .context("enum without underlying type")?;
                let ty = ty(&int)?;
                for c in e.get_children() {
                    if c.get_kind() != EntityKind::EnumConstantDecl {
                        continue;
                    }
                    let name = c.get_name().context("unnamed enumerator")?;
                    let (_, bits) = c
                        .get_enum_constant_value()
                        .with_context(|| format!("{name}: no value"))?;
                    insert_constant(&mut view.constants, name, ty.clone(), bits)?;
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
    if !macros.is_empty() {
        for (name, c) in macro_values(index, header, macros, t)? {
            insert_constant(&mut view.constants, name, c.ty, c.bits)?;
        }
    }
    Ok(view)
}

fn insert_constant(
    constants: &mut BTreeMap<String, Constant>,
    name: String,
    ty: Ty,
    bits: u64,
) -> Result<()> {
    let Ty::Int { bits: width, .. } = ty else {
        bail!("{name}: not an integer constant");
    };
    let mask = if width >= 64 {
        u64::MAX
    } else {
        (1u64 << width) - 1
    };
    let c = Constant {
        ty,
        bits: bits & mask,
    };
    if let Some(old) = constants.insert(name.clone(), c) {
        bail!("{name} defined twice (first as {old:?})");
    }
    Ok(())
}

/// Each macro evaluated by clang in a probe unit that includes the header: `__auto_type` keeps the
/// macro's own C type, so `3U << 9` stays unsigned and `(1 << 14) - 1` stays int.
fn macro_values(
    index: &Index,
    header: &Path,
    macros: &[&str],
    t: &Target,
) -> Result<Vec<(String, Constant)>> {
    let probe = header.with_file_name("__atl_bridgegen_macros.c");
    let mut text = format!("#include \"{}\"\n", header.display());
    for m in macros {
        text.push_str(&format!(
            "static const __auto_type {MACRO_PROBE}{m} = ({m});\n"
        ));
    }
    let tu = index
        .parser(&probe)
        .arguments(&args(t))
        .unsaved(&[Unsaved::new(&probe, &text)])
        .skip_function_bodies(true)
        .parse()
        .map_err(|e| anyhow!("libclang could not parse the macro probe: {e:?}"))?;
    errors(&tu).context("evaluating macros")?;
    let mut out = Vec::new();
    for e in tu.get_entity().get_children() {
        let Some(name) = e
            .get_name()
            .and_then(|n| n.strip_prefix(MACRO_PROBE).map(str::to_owned))
        else {
            continue;
        };
        if e.get_kind() != EntityKind::VarDecl {
            continue;
        }
        let ty = ty(&e.get_type().with_context(|| format!("{name}: no type"))?)
            .with_context(|| name.clone())?;
        let bits = match e.evaluate() {
            Some(EvaluationResult::SignedInteger(v)) => v as u64,
            Some(EvaluationResult::UnsignedInteger(v)) => v,
            other => bail!("macro {name} is not an integer constant: {other:?}"),
        };
        out.push((name, Constant { ty, bits }));
    }
    ensure!(
        out.len() == macros.len(),
        "evaluated {} of {} macros",
        out.len(),
        macros.len()
    );
    Ok(out)
}

/// The record `e` under `name`, plus every record it defines inline (anonymous members, unnamed field
/// types, nested tagged definitions), into `records`.
fn record(e: &Entity, name: &str, records: &mut BTreeMap<String, Record>) -> Result<()> {
    if records.contains_key(name) {
        return Ok(());
    }
    let t = e.get_type().with_context(|| format!("{name}: no type"))?;
    let size = t
        .get_sizeof()
        .map_err(|err| anyhow!("{name}: sizeof: {err:?}"))?;
    let align = t
        .get_alignof()
        .map_err(|err| anyhow!("{name}: alignof: {err:?}"))?;
    let kind = match e.get_kind() {
        EntityKind::UnionDecl => RecordKind::Union,
        _ => RecordKind::Struct,
    };
    let children = e.get_children();
    let has_attr = |k| children.iter().any(|c| c.get_kind() == k);
    let mut fields = Vec::new();
    let mut anon = 0;
    for c in &children {
        let (field, ft) = match c.get_kind() {
            EntityKind::FieldDecl => {
                let field = c
                    .get_name()
                    .with_context(|| format!("{name}: unnamed field"))?;
                ensure!(
                    !c.is_bit_field(),
                    "{name}.{field}: bit-fields are not supported"
                );
                let ft = c
                    .get_type()
                    .with_context(|| format!("{name}.{field}: no type"))?;
                (field, ft)
            }
            EntityKind::StructDecl | EntityKind::UnionDecl if c.is_anonymous_record_decl() => {
                let field = format!("anon{anon}");
                anon += 1;
                let ft = c
                    .get_type()
                    .with_context(|| format!("{name}.{field}: no type"))?;
                (field, ft)
            }
            _ => continue,
        };
        let offset = field_offset(&t, c, &field).with_context(|| format!("{name}.{field}"))?;
        let ty = match inline_record(&ft) {
            Some(d) => {
                let rec = if d.is_anonymous() {
                    let rec = format!("{name}_{field}");
                    ensure!(
                        !records.contains_key(&rec),
                        "{rec}: synthesized name collides"
                    );
                    rec
                } else {
                    d.get_name().context("nested record without name")?
                };
                record(&d, &rec, records)?;
                Ty::Record { name: rec }
            }
            None => self::ty(&ft).with_context(|| format!("{name}.{field}"))?,
        };
        fields.push(Field {
            name: field,
            offset,
            ty,
        });
    }
    records.insert(
        name.to_owned(),
        Record {
            kind,
            size: size as u64,
            align: align as u64,
            packed: has_attr(EntityKind::PackedAttr),
            aligned: has_attr(EntityKind::AlignedAttr),
            fields,
        },
    );
    Ok(())
}

/// A field's record type when it is defined inside the enclosing record (and so has no top-level
/// declaration of its own).
fn inline_record<'tu>(ft: &Type<'tu>) -> Option<Entity<'tu>> {
    let c = ft.get_canonical_type();
    if c.get_kind() != TypeKind::Record {
        return None;
    }
    let d = c.get_declaration()?;
    let parent = d.get_lexical_parent()?;
    (d.is_anonymous()
        || matches!(
            parent.get_kind(),
            EntityKind::StructDecl | EntityKind::UnionDecl
        ))
    .then_some(d)
}

/// Byte offset of a member in its parent. An anonymous member has no name to ask clang about; its
/// first leaf field is reachable by name from the parent (C11 anonymous members) and sits at the
/// member's own offset.
fn field_offset(parent: &Type, member: &Entity, name: &str) -> Result<u64> {
    let leaf = if member.get_kind() == EntityKind::FieldDecl {
        name.to_owned()
    } else {
        first_leaf(member).context("anonymous member without named fields")?
    };
    let bits = parent
        .get_offsetof(&leaf)
        .map_err(|err| anyhow!("offsetof({leaf}): {err:?}"))?;
    ensure!(bits % 8 == 0, "offset is not whole bytes");
    Ok((bits / 8) as u64)
}

fn first_leaf(record: &Entity) -> Option<String> {
    record
        .get_children()
        .iter()
        .find_map(|c| match c.get_kind() {
            EntityKind::FieldDecl => c.get_name(),
            EntityKind::StructDecl | EntityKind::UnionDecl if c.is_anonymous_record_decl() => {
                first_leaf(c)
            }
            _ => None,
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
