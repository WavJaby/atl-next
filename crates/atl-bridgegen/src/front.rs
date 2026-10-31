// SPDX-License-Identifier: MIT
//! libclang front end: one source parsed once per target -> that arch's view. Every size, alignment,
//! offset and constant value is clang's, never computed here.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result, anyhow, bail, ensure};
use clang::diagnostic::Severity;
use clang::source::File;
use clang::{
    Clang, Entity, EntityKind, EvaluationResult, Index, Linkage, TranslationUnit, Type, TypeKind,
    Unsaved,
};

use crate::ir::{ArchView, CInt, Constant, Field, Nullability, Record, RecordKind, Signature, Ty};

/// One target to parse the source for.
pub struct Target {
    /// arches.toml key.
    pub arch: String,
    /// clang target triple.
    pub triple: String,
    /// Further compiler arguments: include directories, `-nostdlibinc` for a non-host sysroot.
    pub args: Vec<String>,
}

/// Language the source is parsed as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    /// GNU C11, as AOSP builds C: strict C hides POSIX types (siginfo_t) in glibc headers.
    C,
    /// GNU C++23: bionic's C++-only layouts (prop_area.h) include `<stdatomic.h>` next to `<atomic>`,
    /// which libc++ accepts only from C++23; layouts do not depend on the dialect.
    Cxx,
}

/// What to parse and which of its declarations form the interface.
pub struct Source<'a> {
    /// File parsed as the translation unit; its declarations belong to the interface.
    pub path: &'a Path,
    pub lang: Lang,
    /// Further files whose declarations belong to the interface (the headers a C++ source keeps its
    /// types in); each must be included by `path`.
    pub also: &'a [PathBuf],
    /// Object-like macros to evaluate: a name, or a `*` glob over the macros the interface's files
    /// define. Integer ones become constants, string literals strings; a name that is neither, or a
    /// glob matching nothing, fails.
    pub macros: &'a [&'a str],
}

impl<'a> Source<'a> {
    /// A C header, nothing else owned.
    pub fn c(path: &'a Path, macros: &'a [&'a str]) -> Self {
        Source {
            path,
            lang: Lang::C,
            also: &[],
            macros,
        }
    }
}

/// libclang allows one `Clang` per process at a time.
static CLANG: Mutex<()> = Mutex::new(());

/// Prefix of the probe variables that evaluate requested macros; nothing in a C header uses it.
const MACRO_PROBE: &str = "__atl_bridgegen_macro_";

/// The source's records, functions, typedefs and constants as each target sees them.
pub fn parse(src: &Source, targets: &[Target]) -> Result<BTreeMap<String, ArchView>> {
    let _one_at_a_time = CLANG
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let clang = Clang::new().map_err(|e| anyhow!("libclang: {e}"))?;
    let index = Index::new(&clang, false, false);
    targets
        .iter()
        .map(|t| {
            let view = parse_one(&index, src, t)
                .with_context(|| format!("{} for {}", src.path.display(), t.triple))?;
            Ok((t.arch.clone(), view))
        })
        .collect()
}

/// The file clang includes for `#include <name>` with these arguments: asks the compiler's own search
/// rules (e.g. where the distro keeps libc++), never a path of ours.
pub fn resolve_include(lang: Lang, target: &Target, name: &str) -> Result<PathBuf> {
    let _one_at_a_time = CLANG
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let clang = Clang::new().map_err(|e| anyhow!("libclang: {e}"))?;
    let index = Index::new(&clang, false, false);
    let probe = std::env::temp_dir().join(match lang {
        Lang::C => "__atl_bridgegen_include.c",
        Lang::Cxx => "__atl_bridgegen_include.cc",
    });
    let tu = index
        .parser(&probe)
        .arguments(&args(lang, target))
        .unsaved(&[Unsaved::new(&probe, format!("#include <{name}>\n"))])
        .detailed_preprocessing_record(true)
        .parse()
        .map_err(|e| anyhow!("libclang could not parse the include probe: {e:?}"))?;
    errors(&tu).with_context(|| format!("resolving <{name}>"))?;
    tu.get_entity()
        .get_children()
        .iter()
        .find(|e| e.get_kind() == EntityKind::InclusionDirective)
        .and_then(Entity::get_file)
        .map(|f| f.get_path())
        .with_context(|| format!("<{name}> resolved to no file"))
}

fn args(lang: Lang, t: &Target) -> Vec<String> {
    let (x, std) = match lang {
        Lang::C => ("c", "-std=gnu11"),
        Lang::Cxx => ("c++", "-std=gnu++23"),
    };
    let mut args = vec!["-x".to_owned(), x.to_owned(), std.to_owned()];
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

/// The files whose declarations belong to the interface: the source and `Source::also`.
struct Owned<'tu> {
    files: Vec<File<'tu>>,
}

impl Owned<'_> {
    /// By the file of the entity's own location (a declaration's name). Not libclang's "in main file",
    /// which says no for `bool f(void);` when `bool` is stdbool.h's macro.
    fn owns(&self, e: &Entity) -> bool {
        e.get_location()
            .and_then(|l| l.get_file_location().file)
            .is_some_and(|f| self.files.contains(&f))
    }
}

fn parse_one(index: &Index, src: &Source, t: &Target) -> Result<ArchView> {
    let tu = index
        .parser(src.path)
        .arguments(&args(src.lang, t))
        .skip_function_bodies(true)
        .detailed_preprocessing_record(true)
        .parse()
        .map_err(|e| anyhow!("libclang could not parse: {e:?}"))?;
    errors(&tu)?;
    let owned = Owned {
        files: std::iter::once(src.path)
            .chain(src.also.iter().map(PathBuf::as_path))
            .map(|p| {
                tu.get_file(p)
                    .with_context(|| format!("{} is not included by the source", p.display()))
            })
            .collect::<Result<_>>()?,
    };

    let mut view = ArchView {
        triple: t.triple.clone(),
        records: BTreeMap::new(),
        functions: BTreeMap::new(),
        constants: BTreeMap::new(),
        strings: BTreeMap::new(),
        typedefs: BTreeMap::new(),
        includes: includes(&tu, src.path)?,
    };
    decls(&tu.get_entity(), &owned, &mut view)?;
    let macros = macro_names(&tu, &owned, src.macros)?;
    if !macros.is_empty() {
        for (name, value) in macro_values(index, src, &macros, t)? {
            insert_value(&mut view, name, value)?;
        }
    }
    Ok(view)
}

/// `#include` directives of `path` as written: `<name>` or `"name"`.
fn includes(tu: &TranslationUnit, path: &Path) -> Result<Vec<String>> {
    let file = tu
        .get_file(path)
        .with_context(|| format!("{}: not in its own translation unit", path.display()))?;
    file.get_includes()
        .iter()
        .map(|e| {
            let name = e.get_name().context("include directive without a name")?;
            let angled = e
                .get_range()
                .is_some_and(|r| r.tokenize().iter().any(|tok| tok.get_spelling() == "<"));
            Ok(if angled {
                format!("<{name}>")
            } else {
                format!("\"{name}\"")
            })
        })
        .collect()
}

/// The interface's declarations among `parent`'s children; `extern "C"` blocks are looked into.
fn decls(parent: &Entity, owned: &Owned, view: &mut ArchView) -> Result<()> {
    for e in parent.get_children() {
        if e.get_kind() == EntityKind::LinkageSpec {
            decls(&e, owned, view)?;
            continue;
        }
        if !owned.owns(&e) {
            continue;
        }
        match e.get_kind() {
            EntityKind::StructDecl | EntityKind::UnionDecl | EntityKind::ClassDecl
                if e.is_definition() =>
            {
                // an anonymous record is named by its typedef (below) or by the field using it
                if !e.is_anonymous()
                    && let Some(name) = e.get_name()
                {
                    record(&e, &name, owned, &mut view.records)?;
                    // C++ `static constexpr` members
                    for m in e.get_children() {
                        if m.get_kind() == EntityKind::VarDecl
                            && let Some((member, value)) = var_constant(&m, owned)?
                        {
                            insert_value(view, format!("{name}::{member}"), value)?;
                        }
                    }
                }
            }
            EntityKind::TypedefDecl => {
                let name = e.get_name().context("unnamed typedef")?;
                let under = e
                    .get_typedef_underlying_type()
                    .with_context(|| format!("{name}: no underlying type"))?;
                let decl = under.get_canonical_type().get_declaration();
                if let Some(d) = decl
                    && is_record(d.get_kind())
                    && d.is_definition()
                    && d.is_anonymous()
                {
                    record(&d, &name, owned, &mut view.records)?;
                }
                let target = typedef_target(&under, &name, owned).with_context(|| name.clone())?;
                view.typedefs.insert(name, target);
            }
            EntityKind::EnumDecl => {
                let int = e
                    .get_enum_underlying_type()
                    .context("enum without underlying type")?;
                let ty = ty(&int, owned)?.resolved().clone();
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
            // internal-linkage functions of a C++ source are not interface
            EntityKind::FunctionDecl if e.get_linkage() != Some(Linkage::Internal) => {
                let name = e.get_name().context("unnamed function")?;
                let t = e.get_type().with_context(|| format!("{name}: no type"))?;
                view.functions.insert(
                    name.clone(),
                    signature(&t, owned).with_context(|| name.clone())?,
                );
            }
            EntityKind::VarDecl => {
                if let Some((name, value)) = var_constant(&e, owned)? {
                    insert_value(view, name, value)?;
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn is_record(k: EntityKind) -> bool {
    matches!(
        k,
        EntityKind::StructDecl | EntityKind::UnionDecl | EntityKind::ClassDecl
    )
}

fn record_kind(k: EntityKind) -> RecordKind {
    match k {
        EntityKind::UnionDecl => RecordKind::Union,
        _ => RecordKind::Struct,
    }
}

/// A `const` variable clang can evaluate (C++ `constexpr`): an integer, or a `const char[]` string.
fn var_constant(e: &Entity, owned: &Owned) -> Result<Option<(String, MacroValue)>> {
    let Some(t) = e.get_type() else {
        return Ok(None);
    };
    // an array's constness is its element's
    let konst = t.is_const_qualified()
        || t.get_element_type()
            .is_some_and(|el| el.is_const_qualified());
    if !konst {
        return Ok(None);
    }
    let name = e.get_name().context("unnamed variable")?;
    let bits = match e.evaluate() {
        Some(EvaluationResult::SignedInteger(v)) => v as u64,
        Some(EvaluationResult::UnsignedInteger(v)) => v,
        Some(EvaluationResult::String(s)) if t.get_element_type().is_some() => {
            let s = s
                .into_string()
                .with_context(|| format!("{name}: not UTF-8"))?;
            return Ok(Some((name, MacroValue::Str(s))));
        }
        _ => return Ok(None),
    };
    let ty = ty(&t, owned)
        .with_context(|| name.clone())?
        .resolved()
        .clone();
    Ok(matches!(ty, Ty::Int { .. }).then_some((name, MacroValue::Int(Constant { ty, bits }))))
}

fn insert_value(view: &mut ArchView, name: String, value: MacroValue) -> Result<()> {
    match value {
        MacroValue::Int(c) => insert_constant(&mut view.constants, name, c.ty, c.bits),
        MacroValue::Str(s) => {
            if view.strings.insert(name.clone(), s).is_some() {
                bail!("{name} defined twice");
            }
            Ok(())
        }
    }
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

/// Requested macro names with globs expanded over the object-like macros the interface's files define.
fn macro_names(tu: &TranslationUnit, owned: &Owned, requested: &[&str]) -> Result<Vec<String>> {
    let defined: Vec<String> = tu
        .get_entity()
        .get_children()
        .into_iter()
        .filter(|e| {
            e.get_kind() == EntityKind::MacroDefinition
                && !e.is_function_like_macro()
                && owned.owns(e)
        })
        .filter_map(|e| e.get_name())
        .collect();
    let mut out = Vec::new();
    for r in requested {
        if !r.contains('*') {
            out.push((*r).to_owned());
            continue;
        }
        let before = out.len();
        out.extend(defined.iter().filter(|d| glob(r, d)).cloned());
        ensure!(out.len() > before, "macro glob {r} matches nothing");
    }
    out.sort();
    out.dedup();
    Ok(out)
}

/// `*` matches any run of characters; everything else itself.
fn glob(pattern: &str, s: &str) -> bool {
    let mut parts = pattern.split('*');
    let first = parts.next().unwrap_or_default();
    let Some(mut rest) = s.strip_prefix(first) else {
        return false;
    };
    let parts: Vec<&str> = parts.collect();
    let Some((last, middle)) = parts.split_last() else {
        return rest.is_empty();
    };
    for m in middle {
        match rest.find(m) {
            Some(i) => rest = &rest[i + m.len()..],
            None => return false,
        }
    }
    rest.len() >= last.len() && rest.ends_with(last)
}

enum MacroValue {
    Int(Constant),
    Str(String),
}

/// Each macro evaluated by clang in a probe unit that includes the source: a deduced type (`__auto_type`
/// in C, `auto` in C++) keeps the macro's own C type, so `3U << 9` stays unsigned and `(1 << 14) - 1`
/// stays int.
fn macro_values(
    index: &Index,
    src: &Source,
    macros: &[String],
    t: &Target,
) -> Result<Vec<(String, MacroValue)>> {
    let (probe_name, deduced) = match src.lang {
        Lang::C => ("__atl_bridgegen_macros.c", "__auto_type"),
        Lang::Cxx => ("__atl_bridgegen_macros.cc", "auto"),
    };
    let probe = src.path.with_file_name(probe_name);
    let mut text = format!("#include \"{}\"\n", src.path.display());
    for m in macros {
        // unparenthesised: libclang evaluates a string literal only as the initializer's direct operand
        text.push_str(&format!("static const {deduced} {MACRO_PROBE}{m} = {m};\n"));
    }
    let tu = index
        .parser(&probe)
        .arguments(&args(src.lang, t))
        .unsaved(&[Unsaved::new(&probe, &text)])
        .skip_function_bodies(true)
        .parse()
        .map_err(|e| anyhow!("libclang could not parse the macro probe: {e:?}"))?;
    errors(&tu).context("evaluating macros")?;
    let none = Owned { files: Vec::new() };
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
        let bits = match e.evaluate() {
            Some(EvaluationResult::SignedInteger(v)) => v as u64,
            Some(EvaluationResult::UnsignedInteger(v)) => v,
            Some(EvaluationResult::String(s)) => {
                let s = s
                    .into_string()
                    .with_context(|| format!("macro {name}: not UTF-8"))?;
                out.push((name, MacroValue::Str(s)));
                continue;
            }
            other => bail!("macro {name} is not an integer or string constant: {other:?}"),
        };
        let ty = ty(
            &e.get_type().with_context(|| format!("{name}: no type"))?,
            &none,
        )
        .with_context(|| name.clone())?
        .resolved()
        .clone();
        out.push((name, MacroValue::Int(Constant { ty, bits })));
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
fn record(
    e: &Entity,
    name: &str,
    owned: &Owned,
    records: &mut BTreeMap<String, Record>,
) -> Result<()> {
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
                record(&d, &rec, owned, records)?;
                Ty::Record { name: rec }
            }
            None => self::ty(&ft, owned).with_context(|| format!("{name}.{field}"))?,
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
            kind: record_kind(e.get_kind()),
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
    (d.is_anonymous() || is_record(parent.get_kind())).then_some(d)
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

/// What typedef `name` stands for; `typedef struct { … } name;` is the record known by `name`.
fn typedef_target(under: &Type, name: &str, owned: &Owned) -> Result<Ty> {
    let c = under.get_canonical_type();
    if c.get_kind() == TypeKind::Record
        && let Some(d) = c.get_declaration()
        && d.is_anonymous()
    {
        return Ok(if owned.owns(&d) && c.get_sizeof().is_ok() {
            Ty::Record {
                name: name.to_owned(),
            }
        } else {
            Ty::Opaque {
                name: name.to_owned(),
                record: record_kind(d.get_kind()),
            }
        });
    }
    ty(under, owned)
}

fn ty(t: &Type, owned: &Owned) -> Result<Ty> {
    // sugar: `struct x` looks through to the named type; a typedef name is kept, also when it is reached
    // through sugar libclang does not expose (C++ `using std::atomic_uint_least32_t;`: a using-type
    // with no declaration, only the typedef's name)
    if t.get_kind() == TypeKind::Unexposed
        && let Some(name) = t.get_typedef_name()
    {
        let target = ty(&t.get_canonical_type(), owned).with_context(|| name.clone())?;
        return Ok(Ty::Typedef {
            name,
            ty: Box::new(target),
        });
    }
    if t.get_kind() == TypeKind::Elaborated {
        let named = t
            .get_elaborated_type()
            .context("elaborated type without named type")?;
        return ty(&named, owned);
    }
    if let Some(decl) = t.get_declaration()
        && matches!(
            decl.get_kind(),
            EntityKind::TypedefDecl | EntityKind::TypeAliasDecl
        )
    {
        let name = decl.get_name().context("unnamed typedef")?;
        let under = decl
            .get_typedef_underlying_type()
            .with_context(|| format!("{name}: no underlying type"))?;
        return Ok(Ty::Typedef {
            ty: Box::new(typedef_target(&under, &name, owned).with_context(|| name.clone())?),
            name,
        });
    }
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
    let int = |signed, c| -> Result<Ty> {
        Ok(Ty::Int {
            bits: bits()?,
            signed,
            c,
        })
    };
    Ok(match c.get_kind() {
        TypeKind::Void => Ty::Void,
        TypeKind::Bool => Ty::Bool,
        TypeKind::CharS => int(true, CInt::Char)?,
        TypeKind::CharU => int(false, CInt::Char)?,
        TypeKind::SChar => int(true, CInt::SignedChar)?,
        TypeKind::UChar => int(false, CInt::UnsignedChar)?,
        TypeKind::Short => int(true, CInt::Short)?,
        TypeKind::UShort => int(false, CInt::UnsignedShort)?,
        TypeKind::Int => int(true, CInt::Int)?,
        TypeKind::UInt => int(false, CInt::UnsignedInt)?,
        TypeKind::Long => int(true, CInt::Long)?,
        TypeKind::ULong => int(false, CInt::UnsignedLong)?,
        TypeKind::LongLong => int(true, CInt::LongLong)?,
        TypeKind::ULongLong => int(false, CInt::UnsignedLongLong)?,
        TypeKind::Int128 => int(true, CInt::Int128)?,
        TypeKind::UInt128 => int(false, CInt::UnsignedInt128)?,
        TypeKind::Char16 => int(false, CInt::Char16)?,
        TypeKind::Char32 => int(false, CInt::Char32)?,
        TypeKind::Float | TypeKind::Double | TypeKind::LongDouble | TypeKind::Float128 => {
            Ty::Float { bits: bits()? }
        }
        TypeKind::Enum => {
            let decl = c.get_declaration().context("enum without declaration")?;
            let int = decl
                .get_enum_underlying_type()
                .context("enum without underlying type")?;
            ty(&int, owned)?.resolved().clone()
        }
        TypeKind::Pointer => {
            // the written pointee keeps typedef names and nullability; the canonical one is the fallback
            let pointee = t
                .get_pointee_type()
                .or_else(|| c.get_pointee_type())
                .context("pointer without pointee")?;
            Ty::Pointer {
                bits: bits()?,
                is_const: pointee.is_const_qualified(),
                pointee: Box::new(ty(&pointee, owned)?),
                nullability,
            }
        }
        TypeKind::Record => {
            let decl = c.get_declaration().context("record without declaration")?;
            let name = decl
                .get_name()
                .with_context(|| format!("anonymous record {}", c.get_display_name()))?;
            if owned.owns(&decl) && c.get_sizeof().is_ok() {
                Ty::Record { name }
            } else {
                Ty::Opaque {
                    name,
                    record: record_kind(decl.get_kind()),
                }
            }
        }
        TypeKind::FunctionPrototype | TypeKind::FunctionNoPrototype => {
            Ty::Function(Box::new(signature(t, owned)?))
        }
        TypeKind::ConstantArray => {
            // the written element type keeps typedef names
            let elem = t
                .get_element_type()
                .or_else(|| c.get_element_type())
                .context("array without element type")?;
            let len = c.get_size().context("array without length")?;
            Ty::Array {
                elem: Box::new(ty(&elem, owned)?),
                len: len as u64,
            }
        }
        other => bail!("{}: unsupported type kind {other:?}", t.get_display_name()),
    })
}

fn signature(t: &Type, owned: &Owned) -> Result<Signature> {
    let c = t.get_canonical_type();
    let ret = t
        .get_result_type()
        .or_else(|| c.get_result_type())
        .context("function without result type")?;
    // `()` in these headers means no parameters (they are written for C++ callers such as ART), not
    // C's unprototyped function
    if c.get_kind() == TypeKind::FunctionNoPrototype {
        return Ok(Signature {
            ret: ty(&ret, owned)?,
            params: Vec::new(),
            variadic: false,
        });
    }
    // written parameter types keep typedef names and nullability; fall back to canonical ones
    let params = t
        .get_argument_types()
        .or_else(|| c.get_argument_types())
        .unwrap_or_default();
    Ok(Signature {
        ret: ty(&ret, owned)?,
        params: params.iter().map(|p| ty(p, owned)).collect::<Result<_>>()?,
        variadic: c.is_variadic(),
    })
}

#[cfg(test)]
mod tests {
    use super::glob;

    #[test]
    fn globs() {
        assert!(glob("__ANDROID_API_*__", "__ANDROID_API_FUTURE__"));
        assert!(glob("__ANDROID_API_*__", "__ANDROID_API_U__"));
        // the level the build targets is not a level name: `__ANDROID_API_` + `_` leaves no `*__`
        assert!(!glob("__ANDROID_API_*__", "__ANDROID_API__"));
        assert!(glob("M_*", "M_PURGE"));
        assert!(!glob("M_*", "X_PURGE"));
        assert!(glob("a*b*c", "a__b__c"));
        assert!(!glob("a*b*c", "a__c"));
        assert!(glob("exact", "exact"));
        assert!(!glob("exact", "exactly"));
    }
}
