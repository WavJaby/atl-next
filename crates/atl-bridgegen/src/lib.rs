// SPDX-License-Identifier: MIT
//! C-ABI interface description: a header (or a C++ source and the headers holding its types) parsed by
//! libclang once per target arch -> the IR (every layout number is clang's) -> generated Rust types
//! with layout asserts, signature checks, layout constants, C declarations, and the ABI-identity report.
//! Also include wrappers giving one libc's headers another's header shape (`shape`).
//! Output is deterministic for the same sources and targets.
#![forbid(unsafe_code)]

pub mod buildscript;
pub mod c;
pub mod front;
pub mod ir;
pub mod report;
pub mod rust;
pub mod shape;

use anyhow::Result;

pub use front::{Lang, Source, Target};

/// `src` (shown as `label` in generated text) as seen by every target.
pub fn interface(
    name: &str,
    label: &str,
    src: &Source,
    targets: &[Target],
) -> Result<ir::Interface> {
    Ok(ir::Interface {
        name: name.to_owned(),
        header: label.to_owned(),
        arches: front::parse(src, targets)?,
    })
}

/// The IR as JSON: maps are ordered, so equal interfaces give equal bytes.
pub fn to_json(iface: &ir::Interface) -> Result<String> {
    Ok(serde_json::to_string_pretty(iface)? + "\n")
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::ir::{CInt, Ty};

    /// A header using only builtin types, so no sysroot is needed for any target.
    const HEADER: &str = "struct S {\n  char c;\n  long long ll;\n  long l;\n  void *p;\n  void (*cb)(int);\n  int arr[3];\n};\nstruct Opaque;\nstruct T {\n  struct S s;\n  const struct Opaque *o;\n};\nint f(struct T *t, const char *fmt, ...);\nstruct U {\n  int (*np)();\n};\n";

    struct Tmp(PathBuf);
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn tmp(name: &str) -> Tmp {
        let dir = std::env::temp_dir().join(format!("atl-bridgegen-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Tmp(dir)
    }

    fn target(arch: &str, triple: &str) -> Target {
        Target {
            arch: arch.into(),
            triple: triple.into(),
            args: vec!["-nostdlibinc".into()],
        }
    }

    fn parsed(dir: &Tmp) -> ir::Interface {
        let header = dir.0.join("t.h");
        std::fs::write(&header, HEADER).unwrap();
        let targets = [
            target("x86_64", "x86_64-unknown-linux-gnu"),
            target("arm64", "aarch64-linux-android"),
            target("x86", "i686-linux-android"),
            target("arm", "armv7a-linux-androideabi"),
        ];
        interface("t", "t.h", &Source::c(&header, &[]), &targets).unwrap()
    }

    /// Unions (anonymous members, unnamed field types, nested), packed/aligned attributes, enumerators
    /// and macros; builtin types only.
    const RECORDS: &str = "#define WIDTH 14\n#define MASK ((1 << WIDTH) - 1)\n#define BIG 3U << 9\nstruct V {\n  unsigned int kind;\n  union { unsigned long long p; unsigned int h; };\n  union { unsigned int x; struct { unsigned short lo, hi; } parts; unsigned char raw[8]; } u;\n  unsigned long long tail;\n};\nstruct P { unsigned int h; unsigned long long c; } __attribute__((packed));\nstruct A { int x; } __attribute__((aligned(16)));\nenum { NEG = -2, SMALL = 7 };\nenum big { HUGE = 0x80000000u };\n";

    fn parsed_records(dir: &Tmp) -> ir::Interface {
        let header = dir.0.join("r.h");
        std::fs::write(&header, RECORDS).unwrap();
        let targets = [
            target("x86_64", "x86_64-unknown-linux-gnu"),
            target("x86", "i686-linux-android"),
        ];
        interface(
            "r",
            "r.h",
            &Source::c(&header, &["WIDTH", "MASK", "BIG"]),
            &targets,
        )
        .unwrap()
    }

    #[test]
    fn unions_attributes_constants() {
        use crate::ir::RecordKind;
        let dir = tmp("records");
        let iface = parsed_records(&dir);
        let x = &iface.arches["x86_64"];
        fn layout(view: &ir::ArchView, r: &str) -> (u64, u64, Vec<(String, u64)>) {
            let rec = &view.records[r];
            let fields = rec
                .fields
                .iter()
                .map(|f| (f.name.clone(), f.offset))
                .collect();
            (rec.size, rec.align, fields)
        }
        let f = |v: &[(&str, u64)]| -> Vec<(String, u64)> {
            v.iter().map(|(n, o)| ((*n).to_owned(), *o)).collect()
        };
        // x86-64 SysV: the anonymous union holds a u64 -> 8-aligned at 8; `u` is 4-aligned (largest
        // member u32), 8 bytes (raw[8]) at 16; tail at 24
        assert_eq!(
            layout(x, "V"),
            (
                32,
                8,
                f(&[("kind", 0), ("anon0", 8), ("u", 16), ("tail", 24)])
            )
        );
        assert_eq!(x.records["V_anon0"].kind, RecordKind::Union);
        assert_eq!(
            layout(x, "V_u"),
            (8, 4, f(&[("x", 0), ("parts", 0), ("raw", 0)]))
        );
        assert_eq!(layout(x, "V_u_parts"), (4, 2, f(&[("lo", 0), ("hi", 2)])));
        // i386: long long is 4-aligned in structs, so everything after `kind` moves down
        assert_eq!(
            layout(&iface.arches["x86"], "V"),
            (
                28,
                4,
                f(&[("kind", 0), ("anon0", 4), ("u", 12), ("tail", 20)])
            )
        );
        // packed: no padding, alignment 1; aligned(16) raises size and alignment
        assert_eq!(layout(x, "P"), (12, 1, f(&[("h", 0), ("c", 4)])));
        assert!(x.records["P"].packed);
        assert_eq!(layout(x, "A"), (16, 16, f(&[("x", 0)])));
        assert!(x.records["A"].aligned);
        let c = |n: &str| (x.constants[n].ty.clone(), x.constants[n].bits);
        let int = |signed| Ty::Int {
            bits: 32,
            signed,
            c: if signed { CInt::Int } else { CInt::UnsignedInt },
        };
        // C: an enum whose values fit int has int enumerators; 0x80000000 forces unsigned int
        assert_eq!(c("NEG"), (int(true), 0xffff_fffe));
        assert_eq!(c("SMALL"), (int(true), 7));
        assert_eq!(c("HUGE"), (int(false), 0x8000_0000));
        // macros keep their C type: (1 << 14) - 1 is int, 3U << 9 unsigned
        assert_eq!(c("WIDTH"), (int(true), 14));
        assert_eq!(c("MASK"), (int(true), 16383));
        assert_eq!(c("BIG"), (int(false), 1536));
        let code = rust::host_module(&iface, "x86_64").unwrap();
        assert!(code.contains("pub const NEG: i32 = -2;"), "{code}");
        assert!(code.contains("#[repr(C, packed(1))]"), "{code}");
        assert!(code.contains("#[repr(C, align(16))]"), "{code}");
        assert!(code.contains("pub union V_anon0 {"), "{code}");
    }

    #[test]
    fn unknown_macro_fails() {
        let dir = tmp("nomacro");
        let header = dir.0.join("r.h");
        std::fs::write(&header, RECORDS).unwrap();
        let targets = [target("x86_64", "x86_64-unknown-linux-gnu")];
        let err =
            interface("r", "r.h", &Source::c(&header, &["NOT_DEFINED"]), &targets).unwrap_err();
        assert!(format!("{err:#}").contains("NOT_DEFINED"), "{err:#}");
    }

    fn offsets(iface: &ir::Interface, arch: &str) -> (u64, u64, Vec<u64>) {
        let s = &iface.arches[arch].records["S"];
        (s.size, s.align, s.fields.iter().map(|f| f.offset).collect())
    }

    #[test]
    fn layouts_follow_each_psabi() {
        let dir = tmp("layouts");
        let iface = parsed(&dir);
        // fields: char c, long long ll, long l, void *p, fn *cb, int arr[3]
        // LP64 (x86-64 SysV, AAPCS64): 8-byte long and pointers, long long aligned to 8
        assert_eq!(
            offsets(&iface, "x86_64"),
            (56, 8, vec![0, 8, 16, 24, 32, 40])
        );
        assert_eq!(
            offsets(&iface, "arm64"),
            (56, 8, vec![0, 8, 16, 24, 32, 40])
        );
        // i386 SysV: long long is 4-aligned inside structs
        assert_eq!(offsets(&iface, "x86"), (36, 4, vec![0, 4, 12, 16, 20, 24]));
        // AAPCS (arm32): long long is 8-aligned
        assert_eq!(offsets(&iface, "arm"), (40, 8, vec![0, 8, 16, 20, 24, 28]));
        // plain char is signed on x86, unsigned on Arm
        assert_eq!(
            iface.arches["x86_64"].records["S"].fields[0].ty,
            Ty::Int {
                bits: 8,
                signed: true,
                c: CInt::Char
            }
        );
        assert_eq!(
            iface.arches["arm64"].records["S"].fields[0].ty,
            Ty::Int {
                bits: 8,
                signed: false,
                c: CInt::Char
            }
        );
        let f = &iface.arches["x86_64"].functions["f"];
        assert!(f.variadic);
        assert_eq!(f.params.len(), 2);
        // `int (*np)()` is a no-parameter function, not a variadic one
        let np = &iface.arches["x86_64"].records["U"].fields[0].ty;
        let Ty::Pointer { pointee, .. } = np else {
            panic!("{np:?}")
        };
        let Ty::Function(sig) = pointee.as_ref() else {
            panic!("{pointee:?}")
        };
        assert!(sig.params.is_empty() && !sig.variadic);
    }

    #[test]
    fn identity_report() {
        let dir = tmp("report");
        let iface = parsed(&dir);
        let same = report::compare(&iface, "arm64", "x86_64");
        let s = same.iter().find(|d| d.record == "S").unwrap();
        // char signedness differs between the two, but the bytes are the same: pass-through
        assert!(s.identical, "{:?}", s.differences);
        let narrow = report::compare(&iface, "x86", "x86_64");
        assert!(
            narrow.iter().find(|d| d.record == "S").unwrap().differences[0].starts_with("size 36")
        );
    }

    #[test]
    fn deterministic() {
        let dir = tmp("det");
        let a = parsed(&dir);
        let b = parsed(&dir);
        assert_eq!(to_json(&a).unwrap(), to_json(&b).unwrap());
        assert_eq!(
            rust::host_module(&a, "x86_64").unwrap(),
            rust::host_module(&b, "x86_64").unwrap()
        );
    }

    /// The generated module compiles for the machine's own arch, so rustc agrees with clang on every
    /// asserted size, alignment and offset.
    #[test]
    fn generated_asserts_hold_under_rustc() {
        assert_eq!(
            std::env::consts::ARCH,
            "x86_64",
            "host triple for this test not set for this arch"
        );
        let dir = tmp("rustc");
        let iface = parsed(&dir);
        let code = rust::host_module(&iface, "x86_64").unwrap();
        let compiles = |code: &str| rustc(&dir, code);
        let out = compiles(&code);
        assert!(
            out.status.success(),
            "{}\n{code}",
            String::from_utf8_lossy(&out.stderr)
        );
        // the asserts can fail: a layout clang did not compute must not compile
        let wrong = code.replace("size_of::<S>() == 56", "size_of::<S>() == 57");
        assert_ne!(wrong, code);
        assert!(!compiles(&wrong).status.success());
        // unions, packed and aligned records: rustc agrees too
        let records = rust::host_module(&parsed_records(&dir), "x86_64").unwrap();
        let out = compiles(&records);
        assert!(
            out.status.success(),
            "{}\n{records}",
            String::from_utf8_lossy(&out.stderr)
        );
        let wrong = records.replace("offset_of!(P, c) == 4", "offset_of!(P, c) == 8");
        assert_ne!(wrong, records);
        assert!(!compiles(&wrong).status.success());
    }

    /// `code` as a `no_std` library, type-checked by rustc.
    fn rustc(dir: &Tmp, code: &str) -> std::process::Output {
        let src = dir.0.join("gen.rs");
        std::fs::write(&src, format!("#![no_std]\n{code}")).unwrap();
        std::process::Command::new("rustc")
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
            .unwrap()
    }

    /// An API header in the shape of bionic's: an opaque typedef, a forward-declared struct, callbacks,
    /// standard typedefs, integer and string macros, `long` next to `long long`.
    const API: &str = "#include <stdint.h>\n#include <stdbool.h>\n#include <stddef.h>\ntypedef struct item item;\nstruct timespec;\n#define LIMIT 92\n#define NAME \"/dev/x\"\n#define BIG 3000000000U\n#define NEG (-3)\nint set(const char *name, const char *value);\nconst item *find(const char *name);\nvoid visit(const item *it, void (*cb)(void *cookie, const char *name, uint32_t serial), void *cookie);\nbool wait_for(const item *it, uint32_t old, uint32_t *new_serial, const struct timespec *timeout);\nlong count(unsigned long n, long long m, size_t s, char c, signed char sc);\nint none(void);\n";
    const API_FUNCTIONS: &[&str] = &["set", "find", "visit", "wait_for", "count", "none"];
    const API_MACROS: &[&str] = &["LIMIT", "NAME", "BIG", "NEG"];

    /// clang's own freestanding stdint/stdbool/stddef: no sysroot needed.
    fn freestanding() -> Target {
        Target {
            arch: "x86_64".into(),
            triple: "x86_64-unknown-linux-gnu".into(),
            args: vec!["-nostdlibinc".into(), "-ffreestanding".into()],
        }
    }

    fn api(dir: &Tmp) -> ir::Interface {
        let header = dir.0.join("api.h");
        std::fs::write(&header, API).unwrap();
        interface(
            "api",
            "api.h",
            &Source::c(&header, API_MACROS),
            &[freestanding()],
        )
        .unwrap()
    }

    #[test]
    fn c_header_round_trip() {
        let dir = tmp("cheader");
        let iface = api(&dir);
        let view = &iface.arches["x86_64"];
        assert_eq!(view.includes, ["<stdint.h>", "<stdbool.h>", "<stddef.h>"]);
        assert_eq!(view.strings["NAME"], "/dev/x");
        let text = c::header(&iface, "x86_64", API_FUNCTIONS, API_MACROS).unwrap();
        for want in [
            "#include <stdbool.h>\n#include <stddef.h>\n#include <stdint.h>\n",
            "struct timespec;\ntypedef struct item item;\n",
            "#define BIG 3000000000U\n",
            "#define NEG (-3)\n",
            "#define NAME \"/dev/x\"\n",
            "const item * find(const char *);\n",
            "int none(void);\n",
        ] {
            assert!(text.contains(want), "{want:?} missing:\n{text}");
        }
        // what the printed header declares, read back in both languages, is what the source declared
        let printed = dir.0.join("printed.h");
        std::fs::write(&printed, &text).unwrap();
        for lang in [Lang::C, Lang::Cxx] {
            let src = Source {
                path: &printed,
                lang,
                also: &[],
                macros: API_MACROS,
            };
            let back = interface("back", "printed.h", &src, &[freestanding()]).unwrap();
            let bv = &back.arches["x86_64"];
            for f in API_FUNCTIONS {
                assert_eq!(bv.functions[*f], view.functions[*f], "{f} ({lang:?})");
            }
            assert_eq!(bv.constants, view.constants, "{lang:?}");
            assert_eq!(bv.strings, view.strings, "{lang:?}");
        }
    }

    #[test]
    fn c_header_without_standard_types_includes_nothing() {
        let dir = tmp("cfree");
        let iface = api(&dir);
        let text = c::header(&iface, "x86_64", &["none"], &["LIMIT"]).unwrap();
        // force-includable: no libc header runs before the source's own feature macros
        assert!(!text.contains("#include"), "{text}");
        let err = c::header(&iface, "x86_64", &["absent"], &[]).unwrap_err();
        assert!(format!("{err:#}").contains("absent"));
    }

    /// The implementation's signatures checked against the header: matching ones compile, a changed
    /// return type does not.
    #[test]
    fn api_signatures_checked_by_rustc() {
        assert_eq!(
            std::env::consts::ARCH,
            "x86_64",
            "host triple for this test not set for this arch"
        );
        let dir = tmp("apicheck");
        let iface = api(&dir);
        let checks = rust::api_module(&iface, "x86_64", &["set", "find", "visit"], "imp").unwrap();
        assert!(checks.contains("pub const LIMIT: i32 = 92;"), "{checks}");
        assert!(
            checks.contains("pub const NAME: &str = \"/dev/x\";"),
            "{checks}"
        );
        // x86-64: plain char is i8; an unannotated callback pointer may be null
        let imp = "mod imp {\n  use super::item;\n  pub unsafe extern \"C\" fn set(_: *const i8, _: *const i8) -> i32 { 0 }\n  pub unsafe extern \"C\" fn find(_: *const i8) -> *const item { core::ptr::null() }\n  pub unsafe extern \"C\" fn visit(_: *const item, _: Option<unsafe extern \"C\" fn(*mut core::ffi::c_void, *const i8, u32)>, _: *mut core::ffi::c_void) {}\n}\n";
        let out = rustc(&dir, &format!("{checks}{imp}"));
        assert!(
            out.status.success(),
            "{}\n{checks}{imp}",
            String::from_utf8_lossy(&out.stderr)
        );
        let wrong = imp.replace("_: *const i8) -> i32 { 0 }", "_: *const i8) -> i64 { 0 }");
        assert_ne!(wrong, imp);
        assert!(!rustc(&dir, &format!("{checks}{wrong}")).status.success());
    }

    /// A C++ source whose types live in a header it includes: classes, `static constexpr` members,
    /// `constexpr` globals, `extern "C"` declarations; internal functions are not interface.
    #[test]
    fn cxx_source_with_owned_header() {
        let dir = tmp("cxx");
        let header = dir.0.join("l.h");
        std::fs::write(&header, "struct node {\n  unsigned namelen;\n  unsigned prop;\n  char name[0];\n};\nclass area {\n public:\n  static constexpr unsigned kFlag = 1 << 16;\n  unsigned used() const { return used_; }\n private:\n  unsigned used_;\n  unsigned serial_;\n  unsigned reserved_[28];\n  char data_[0];\n};\n").unwrap();
        let source = dir.0.join("l.cc");
        std::fs::write(&source, "#include \"l.h\"\nconstexpr unsigned long SIZE = 128 * 1024;\nstatic constexpr const char kMessage[] = \"use the other one\";\nextern \"C\" int api(const area *a);\nstatic int helper(int x) { return x; }\n").unwrap();
        let also = [header];
        let src = Source {
            path: &source,
            lang: Lang::Cxx,
            also: &also,
            macros: &[],
        };
        let target = Target {
            args: vec!["-nostdlibinc".into()],
            ..freestanding()
        };
        let iface = interface("l", "l.cc", &src, &[target]).unwrap();
        let v = &iface.arches["x86_64"];
        // 2 + 28 u32 = 120 bytes, the zero-length array at the end
        let area = &v.records["area"];
        assert_eq!((area.size, area.align), (120, 4));
        assert_eq!(
            area.fields
                .iter()
                .map(|f| (f.name.as_str(), f.offset))
                .collect::<Vec<_>>(),
            [
                ("used_", 0),
                ("serial_", 4),
                ("reserved_", 8),
                ("data_", 120)
            ]
        );
        assert_eq!(v.records["node"].size, 8);
        assert_eq!(v.constants["SIZE"].bits, 128 * 1024);
        assert_eq!(v.constants["area::kFlag"].bits, 1 << 16);
        assert_eq!(v.strings["kMessage"], "use the other one");
        assert!(v.functions.contains_key("api"));
        assert!(!v.functions.contains_key("helper"));
        let layout = rust::layout_module(&iface, "x86_64").unwrap();
        for want in [
            "pub mod area {\n    pub const SIZE: usize = 120;\n",
            "    pub const data_: usize = 120;\n",
            "pub const area_kFlag: u32 = 65536;\n",
            "pub const SIZE: u64 = 131072;\n",
        ] {
            assert!(layout.contains(want), "{want:?} missing:\n{layout}");
        }
    }
}
