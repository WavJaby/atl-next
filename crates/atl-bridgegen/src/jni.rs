// SPDX-License-Identifier: MIT
//! Lane descriptors of a JNI function table (Q6-3). Per slot: the lane class of each fixed parameter and of the result, and the variadic flag. A class is the bank that AAPCS64 (guest) and SysV (host) both assign the argument to: integers and pointers take an int lane, floats and doubles an fp lane, each bank counted in declaration order. The descriptors come from the IR that `jni_tables` reads (build.rs `jni_interface`), so a slot's lanes and its table position share one description.
//! A `va_list` parameter (the `...V` variants) has its own class: its argument is a pointer to a platform struct, not a lane value, so a route can refuse it by name.

use anyhow::{Context, Result, bail, ensure};

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
        "/// Lane class of a JNI parameter or result (`atl_bridgegen::objects::Class`).\n\
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

/// A function's parameter classes in declaration order (a proxy binding's descriptor, from the same prototype as its [`result_class`]).
pub fn param_classes(sig: &Signature) -> Result<Vec<Class>> {
    sig.params.iter().map(param).collect()
}

/// Why a host call of parameter classes `params` cannot cross the QEMU binding route, else `None` (R-1, Q7-4). That route (atl-native-bridge dispatch.rs `call_host`) passes the guest's lanes as one SysV image: x0..x7, then v0..v7 in xmm0..7, then the eight guest stack words. SysV takes integers 1-6 from registers and the rest from the stack in declaration order, interleaved with any fp argument past the eighth; AAPCS64 does the same with eight integer registers. So with at most eight fp arguments the host's stack words are integers 7.. = x6, x7, then the guest's stack words: exactly the image. A ninth fp argument sits among them on both sides at a position the image does not keep. More than 16 integers need a ninth guest stack word, past the eight a trap delivers. A `va_list` is a pointer to a platform struct, not a lane value.
pub fn lane_refusal(params: &[Class]) -> Option<&'static str> {
    let count = |c| params.iter().filter(|&&p| p == c).count();
    if count(Class::VaList) > 0 {
        Some("refused: R-1, a va_list parameter has no lane")
    } else if count(Class::Fp) > 8 {
        Some("refused: R-1, a ninth fp argument interleaves with the stack integers")
    } else if count(Class::Int) > 16 {
        Some("refused: R-1, more than 16 integers need a ninth stack word")
    } else {
        None
    }
}

/// jni.h's reference typedefs: a value of one is a reference in some `JNIEnv`'s tables, meaningful only next to that env.
const JNI_REFS: &[&str] = &[
    "jobject",
    "jclass",
    "jstring",
    "jarray",
    "jobjectArray",
    "jbooleanArray",
    "jbyteArray",
    "jcharArray",
    "jshortArray",
    "jintArray",
    "jlongArray",
    "jfloatArray",
    "jdoubleArray",
    "jthrowable",
    "jweak",
];

/// The integer lanes the env class forwards: SysV's six integer registers. Its host side (atl-native-bridge ndk.rs `call_env`) is one `extern "C" fn` of six words that calls the host function with them; a seventh would sit on the stack, which that function's frame does not forward.
pub const ENV_LANES: usize = 6;

/// The typedef name a type is spelled with, through `const` and nothing else.
fn typedef_name(ty: &Ty) -> Option<&str> {
    match ty {
        Ty::Typedef { name, .. } => Some(name),
        _ => None,
    }
}

/// `JNIEnv*` / `JavaVM*`: a pointer to that typedef.
fn points_to(ty: &Ty, name: &str) -> bool {
    matches!(ty.resolved(), Ty::Pointer { pointee, .. } if typedef_name(pointee) == Some(name))
}

/// How proxy function `name` with prototype `sig` binds when the prototype has JNI types (`JNIEnv*`, `JavaVM*`, a jni.h reference typedef). A generic row would hand the host library the guest's env, which the host cannot use (tr-first blocker 3: `ANativeWindow_fromSurface` -> SEGV in `android_view_Surface_getSurface`), so such a prototype binds only through the env class, and only when `listed` (the caller's reviewed list) names it.
/// Ok(None): no JNI type, the generic route applies. Ok(Some(i)): the env class, parameter `i` is the `JNIEnv*` its host side maps; every parameter an int lane, at most [`ENV_LANES`], the result an int lane or void. Err: why not, naming `name`.
pub fn env_binding(name: &str, sig: &Signature, listed: bool) -> Result<Option<usize>> {
    let env: Vec<usize> = (0..sig.params.len())
        .filter(|&i| points_to(&sig.params[i], "JNIEnv"))
        .collect();
    let vm = sig.params.iter().any(|p| points_to(p, "JavaVM"));
    let refs = sig
        .params
        .iter()
        .chain(std::iter::once(&sig.ret))
        .any(|p| typedef_name(p).is_some_and(|n| JNI_REFS.contains(&n)));
    if env.is_empty() && !vm && !refs {
        ensure!(
            !listed,
            "{name}: listed for the env class, but its prototype has no JNI type"
        );
        return Ok(None);
    }
    ensure!(
        listed,
        "{name}: a JNI-typed prototype (JNIEnv*, JavaVM* or a jni.h reference) bound generically would hand the host the guest's env; list it for the env class"
    );
    ensure!(!vm, "{name}: a JavaVM* parameter has no shim class");
    let [index] = env[..] else {
        bail!(
            "{name}: the env class maps exactly one JNIEnv* parameter, the prototype has {}",
            env.len()
        );
    };
    let params = param_classes(sig).with_context(|| format!("{name}: parameter classes"))?;
    ensure!(
        params.len() <= ENV_LANES && params.iter().all(|&c| c == Class::Int),
        "{name}: the env class forwards at most {ENV_LANES} int lanes, the prototype has {params:?}"
    );
    let ret = result_class(&sig.ret).with_context(|| format!("{name}: result class"))?;
    ensure!(
        matches!(ret, Class::Int | Class::Void),
        "{name}: the env class answers in an int lane, the result is {ret:?}"
    );
    Ok(Some(index))
}

/// A parameter's class. `va_list` is matched by name first: the IR keeps the typedef name, and its type differs by platform (an array on x86-64, a struct on AArch64).
fn param(ty: &Ty) -> Result<Class> {
    match ty {
        Ty::Typedef { name, .. } if name == "va_list" => Ok(Class::VaList),
        // C: a parameter of array type is a pointer (NDK NdkMediaCrypto.h `const uint8_t uuid[16]`)
        _ if matches!(ty.resolved(), Ty::Array { .. }) => Ok(Class::Int),
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

    /// R-1 at bind time (Q7-4), classes from GL-shaped prototypes. Fit: glTexImage2D (9 integers: one guest stack word), glUniform4f (int + 4 fp), 8 fp + 16 integers (the image's edge). Refused, each by its own reason: 9 floats, 17 integers, a va_list. Expected outcomes follow from the rule in `lane_refusal`'s doc (SysV 6 + AAPCS64 8 integer registers, 8 fp registers each, 8 trap stack words), not from the code.
    #[test]
    fn lane_refusal_names_the_r1_shapes() {
        let iface = parse(
            "r1",
            "typedef unsigned int GLenum;\ntypedef int GLint;\ntypedef int GLsizei;\ntypedef float GLfloat;\ntypedef __builtin_va_list va_list;\nvoid glTexImage2D(GLenum, GLint, GLint, GLsizei, GLsizei, GLint, GLenum, GLenum, const void *);\nvoid glUniform4f(GLint, GLfloat, GLfloat, GLfloat, GLfloat);\nvoid edge(float, float, float, float, float, float, float, float, int, int, int, int, int, int, int, int, int, int, int, int, int, int, int, int);\nvoid nine_fp(float, float, float, float, float, float, float, float, float);\nvoid ints17(int, int, int, int, int, int, int, int, int, int, int, int, int, int, int, int, int);\nvoid vl(int, va_list);\n",
        );
        let view = &iface.arches["x86_64"];
        let refusal = |name: &str| lane_refusal(&param_classes(&view.functions[name]).unwrap());
        assert_eq!(refusal("glTexImage2D"), None);
        assert_eq!(refusal("glUniform4f"), None);
        assert_eq!(refusal("edge"), None);
        let named = [
            ("nine_fp", "ninth fp"),
            ("ints17", "16 integers"),
            ("vl", "va_list"),
        ];
        for (name, why) in named {
            let r = refusal(name).unwrap_or_default();
            assert!(
                r.starts_with("refused: R-1") && r.contains(why),
                "{name}: {r:?}"
            );
        }
    }

    /// tr-first blocker 3, the build-time refusal: NDK-shaped prototypes in a fixture header. Expected outcomes are written from the C shapes and `env_binding`'s rule, not from running it: a JNI-typed prototype not listed fails naming itself; listed, the env class maps the `JNIEnv*` at its position; JavaVM*, two envs, an fp lane, seven lanes and a reference without an env each fail by their own reason; a plain prototype stays generic, and listing it is stale.
    #[test]
    fn env_binding_refuses_unlisted_jni_prototypes() {
        let iface = parse(
            "env",
            "typedef void *jobject;\ntypedef jobject jclass;\ntypedef const struct JNINativeInterface *JNIEnv;\ntypedef const struct JNIInvokeInterface *JavaVM;\nstruct ANativeWindow;\nstruct ANativeWindow *ANativeWindow_fromSurface(JNIEnv *env, jobject surface);\nint lock_at_two(void *a, void *b, JNIEnv *env, jobject o);\njobject to_java(JNIEnv *env, struct ANativeWindow *w);\nint from_vm(JavaVM *vm);\nint two_envs(JNIEnv *a, JNIEnv *b);\nint fp_lane(JNIEnv *env, float f);\nint seven(JNIEnv *e, int a, int b, int c, int d, int f, int g);\nint ref_only(jclass c);\nint plain(struct ANativeWindow *w);\n",
        );
        let view = &iface.arches["x86_64"];
        let bind = |name: &str, listed: bool| env_binding(name, &view.functions[name], listed);
        let err = format!(
            "{:#}",
            bind("ANativeWindow_fromSurface", false).unwrap_err()
        );
        assert!(
            err.starts_with("ANativeWindow_fromSurface: a JNI-typed prototype")
                && err.contains("list it for the env class"),
            "{err}"
        );
        assert_eq!(bind("ANativeWindow_fromSurface", true).unwrap(), Some(0));
        assert_eq!(bind("lock_at_two", true).unwrap(), Some(2));
        assert_eq!(bind("to_java", true).unwrap(), Some(0));
        for (name, why) in [
            ("from_vm", "JavaVM*"),
            ("two_envs", "exactly one JNIEnv*"),
            ("fp_lane", "int lanes"),
            ("seven", "at most 6"),
            ("ref_only", "exactly one JNIEnv*"),
        ] {
            let e = format!("{:#}", bind(name, true).unwrap_err());
            assert!(e.starts_with(name) && e.contains(why), "{name}: {e}");
        }
        assert_eq!(bind("plain", false).unwrap(), None);
        let stale = format!("{:#}", bind("plain", true).unwrap_err());
        assert!(stale.contains("no JNI type"), "{stale}");
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
