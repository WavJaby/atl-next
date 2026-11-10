// SPDX-License-Identifier: MIT
//! Lane descriptors of a JNI function table (Q6-3). Per slot: the lane class of each fixed parameter and of the result, and the variadic flag. A class is the bank that AAPCS64 (guest) and SysV (host) both assign the argument to: integers and pointers take an int lane, floats and doubles an fp lane, each bank counted in declaration order. The descriptors come from the IR that `jni_tables` reads (build.rs `jni_interface`), so a slot's lanes and its table position share one description.
//! A `va_list` parameter (the `...V` variants) has its own class: its argument is a pointer to a platform struct, not a lane value, so a route can refuse it by name.

use anyhow::{Context, Result, bail};

use crate::ir::{ArchView, Signature, Ty};

/// The lane class of one parameter or result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    /// Integer or pointer: an int lane.
    Int,
    /// `float` or `double`: an fp lane, its bits as the register holds them.
    Fp,
    /// No value (a `void` result).
    Void,
    /// `va_list` (the `...V` variants).
    VaList,
}

/// One function slot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lane {
    /// The field name as jni.h spells it (`CallDoubleMethod`).
    pub name: String,
    /// The fixed parameters in order. A variadic slot's `...` has no class here: it follows them.
    pub params: Vec<Class>,
    pub ret: Class,
    pub variadic: bool,
}

/// `record`'s fields in declaration order: a function field's lane, `None` for a field that is not a function (`reserved0`..`reserved3`). Fails naming the field when a parameter or result has no lane class (a record by value), so a jni.h change of that kind stops the build.
pub fn lanes(view: &ArchView, record: &str) -> Result<Vec<Option<Lane>>> {
    let rec = view
        .records
        .get(record)
        .with_context(|| format!("no record {record}"))?;
    rec.fields
        .iter()
        .map(|f| match function(&f.ty) {
            None => Ok(None),
            Some(sig) => lane(&f.name, sig)
                .with_context(|| format!("{record}.{}", f.name))
                .map(Some),
        })
        .collect()
}

/// `jni_lanes.rs`: the lane types, then one module per `(name, lanes)` with `LANES` indexed by slot. Each module's array length is the table length, so a table that disagrees with its slot list fails to compile.
pub fn rust(tables: &[(&str, Vec<Option<Lane>>)]) -> String {
    let mut out = String::from(
        "/// Lane class of a JNI parameter or result (`atl_bridgegen::jni::Class`).\n\
         #[allow(dead_code)]\n\
         #[derive(Clone, Copy, Debug, PartialEq, Eq)]\n\
         pub enum Class {\n    Int,\n    Fp,\n    Void,\n    VaList,\n}\n\n\
         /// A function slot's lanes (`atl_bridgegen::jni::Lane`).\n\
         #[allow(dead_code)]\n\
         #[derive(Clone, Copy, Debug)]\n\
         pub struct Lane {\n    pub name: &'static str,\n    pub params: &'static [Class],\n    pub ret: Class,\n    pub variadic: bool,\n}\n\n",
    );
    for (module, lanes) in tables {
        out.push_str(&format!(
            "/// `{module}`: the lane of each slot, `None` for a field that is not a function.\n#[allow(dead_code)]\npub mod {module} {{\n    use super::{{Class, Lane}};\n\n    pub const LANES: [Option<Lane>; {}] = [\n",
            lanes.len()
        ));
        for lane in lanes {
            match lane {
                None => out.push_str("        None,\n"),
                Some(l) => {
                    let params: Vec<String> =
                        l.params.iter().map(|c| format!("Class::{c:?}")).collect();
                    out.push_str(&format!(
                        "        Some(Lane {{ name: {:?}, params: &[{}], ret: Class::{:?}, variadic: {} }}),\n",
                        l.name,
                        params.join(", "),
                        l.ret,
                        l.variadic
                    ));
                }
            }
        }
        out.push_str("    ];\n}\n\n");
    }
    out
}

/// The signature of a function-pointer type, else `None`.
fn function(ty: &Ty) -> Option<&Signature> {
    match ty.resolved() {
        Ty::Pointer { pointee, .. } => match pointee.resolved() {
            Ty::Function(sig) => Some(sig.as_ref()),
            _ => None,
        },
        _ => None,
    }
}

fn lane(name: &str, sig: &Signature) -> Result<Lane> {
    let params = sig.params.iter().map(param).collect::<Result<Vec<_>>>()?;
    let ret = result_class(&sig.ret)?;
    Ok(Lane {
        name: name.to_owned(),
        params,
        ret,
        variadic: sig.variadic,
    })
}

/// A result's class: `void` is [`Class::Void`], else the class of its type. Shared by a JNI slot's result and a proxy binding's (bindings.rs `Entry::Generic`), so both answer in the same register.
pub fn result_class(ty: &Ty) -> Result<Class> {
    match ty.resolved() {
        Ty::Void => Ok(Class::Void),
        _ => scalar(ty),
    }
}

/// A parameter's class. `va_list` is matched by name first: the IR keeps the typedef name, and its type differs by platform (an array on x86-64, a struct on AArch64).
fn param(ty: &Ty) -> Result<Class> {
    match ty {
        Ty::Typedef { name, .. } if name == "va_list" => Ok(Class::VaList),
        _ => scalar(ty),
    }
}

/// An integer, pointer or floating type's class; anything else has no lane.
fn scalar(ty: &Ty) -> Result<Class> {
    match ty.resolved() {
        Ty::Float { .. } => Ok(Class::Fp),
        Ty::Int { .. } | Ty::Bool | Ty::Pointer { .. } | Ty::Reference { .. } => Ok(Class::Int),
        other => bail!("no lane class for {other:?}"),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::{Source, Target, interface};

    /// jni.h's shapes in miniature: a reserved data pointer, a function with one fixed parameter, float and double in parameters and results, a variadic double result, a `va_list` variant, a boolean result.
    const TABLE: &str = "typedef int jint;\ntypedef unsigned char jboolean;\ntypedef long long jlong;\ntypedef float jfloat;\ntypedef double jdouble;\ntypedef void *jobject;\ntypedef void *jfieldID;\ntypedef void *jmethodID;\ntypedef __builtin_va_list va_list;\ntypedef const struct JNINativeInterface *JNIEnv;\nstruct JNINativeInterface {\n  void *reserved0;\n  jint (*GetVersion)(JNIEnv *);\n  void (*SetDoubleField)(JNIEnv *, jobject, jfieldID, jdouble);\n  jdouble (*CallDoubleMethod)(JNIEnv *, jobject, jmethodID, ...);\n  jdouble (*CallDoubleMethodV)(JNIEnv *, jobject, jmethodID, va_list);\n  jfloat (*GetFloatField)(JNIEnv *, jobject, jfieldID);\n  jboolean (*IsSameObject)(JNIEnv *, jobject, jobject);\n  jlong (*NewLong)(JNIEnv *, jlong);\n};\n";

    struct Tmp(PathBuf);
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn parse(name: &str, header: &str) -> crate::ir::Interface {
        let dir =
            Tmp(std::env::temp_dir()
                .join(format!("atl-bridgegen-jni-{name}-{}", std::process::id())));
        std::fs::create_dir_all(&dir.0).unwrap();
        let path = dir.0.join("jni.h");
        std::fs::write(&path, header).unwrap();
        let target = Target {
            arch: "x86_64".into(),
            triple: "x86_64-unknown-linux-gnu".into(),
            args: vec!["-nostdlibinc".into()],
        };
        interface(
            "jni",
            "jni.h",
            &Source::c(&path, &[]),
            std::slice::from_ref(&target),
        )
        .unwrap()
    }

    fn lane(name: &str, params: &[Class], ret: Class, variadic: bool) -> Option<Lane> {
        Some(Lane {
            name: name.into(),
            params: params.to_vec(),
            ret,
            variadic,
        })
    }

    /// Expected lanes written from the C shapes above, not from the code under test.
    #[test]
    fn classes_follow_the_lane_banks() {
        use Class::{Fp, Int, VaList, Void};
        let iface = parse("classes", TABLE);
        assert_eq!(
            lanes(&iface.arches["x86_64"], "JNINativeInterface").unwrap(),
            vec![
                None,
                lane("GetVersion", &[Int], Int, false),
                lane("SetDoubleField", &[Int, Int, Int, Fp], Void, false),
                lane("CallDoubleMethod", &[Int, Int, Int], Fp, true),
                lane("CallDoubleMethodV", &[Int, Int, Int, VaList], Fp, false),
                lane("GetFloatField", &[Int, Int, Int], Fp, false),
                lane("IsSameObject", &[Int, Int, Int], Int, false),
                lane("NewLong", &[Int, Int], Int, false),
            ]
        );
    }

    #[test]
    fn record_by_value_fails_naming_the_field() {
        let iface = parse(
            "byval",
            "struct Pt { int x, y; };\nstruct JNINativeInterface {\n  void (*Take)(struct Pt);\n};\n",
        );
        let err = format!(
            "{:#}",
            lanes(&iface.arches["x86_64"], "JNINativeInterface").unwrap_err()
        );
        assert!(err.contains("JNINativeInterface.Take"), "{err}");
        assert!(err.contains("no lane class"), "{err}");
    }

    /// Expected classes written from the C shapes, not from the code under test: void, an enum and a boolean (int lanes), a pointer (int), float and double (fp).
    #[test]
    fn result_classes_follow_the_return_banks() {
        use Class::{Fp, Int, Void};
        let iface = parse(
            "results",
            "typedef unsigned int GLenum;\ntypedef unsigned char GLboolean;\ntypedef float GLfloat;\ntypedef double GLdouble;\nvoid f_void(void);\nGLenum f_enum(void);\nGLboolean f_bool(void);\nvoid *f_ptr(void);\nGLfloat f_float(void);\nGLdouble f_double(void);\n",
        );
        let view = &iface.arches["x86_64"];
        let got: Vec<Class> = ["f_void", "f_enum", "f_bool", "f_ptr", "f_float", "f_double"]
            .iter()
            .map(|name| result_class(&view.functions[*name].ret).unwrap())
            .collect();
        assert_eq!(got, [Void, Int, Int, Int, Fp, Fp]);
    }

    /// The emitted text type-checks under rustc, with one 8-slot table.
    #[test]
    fn generated_rust_compiles() {
        let iface = parse("rust", TABLE);
        let code = rust(&[(
            "env",
            lanes(&iface.arches["x86_64"], "JNINativeInterface").unwrap(),
        )]);
        assert!(
            code.contains("pub const LANES: [Option<Lane>; 8] = ["),
            "{code}"
        );
        let dir = Tmp(
            std::env::temp_dir().join(format!("atl-bridgegen-jni-rust-{}", std::process::id()))
        );
        std::fs::create_dir_all(&dir.0).unwrap();
        let src = dir.0.join("gen.rs");
        std::fs::write(&src, format!("#![no_std]\n{code}")).unwrap();
        let out = std::process::Command::new("rustc")
            .args([
                "--edition",
                "2024",
                "--crate-type",
                "lib",
                "--emit",
                "metadata",
                "-o",
            ])
            .arg(dir.0.join("gen.rmeta"))
            .arg(&src)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}
