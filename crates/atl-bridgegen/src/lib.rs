// SPDX-License-Identifier: MIT
//! C-ABI interface description: a header parsed by libclang once per target arch -> the IR (every
//! layout number is clang's) -> generated Rust types with layout asserts, and the ABI-identity report.
//! Output is deterministic for the same headers and targets.
#![forbid(unsafe_code)]

pub mod front;
pub mod ir;
pub mod report;
pub mod rust;

use std::path::Path;

use anyhow::Result;

pub use front::Target;

/// `header` (shown as `label` in generated text) as seen by every target.
pub fn interface(
    name: &str,
    header: &Path,
    label: &str,
    targets: &[Target],
) -> Result<ir::Interface> {
    Ok(ir::Interface {
        name: name.to_owned(),
        header: label.to_owned(),
        arches: front::parse(header, targets)?,
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
    use crate::ir::Ty;

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
        interface("t", &header, "t.h", &targets).unwrap()
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
        // char is signed on x86, unsigned on Arm
        assert_eq!(
            iface.arches["x86_64"].records["S"].fields[0].ty,
            Ty::Int {
                bits: 8,
                signed: true
            }
        );
        assert_eq!(
            iface.arches["arm64"].records["S"].fields[0].ty,
            Ty::Int {
                bits: 8,
                signed: false
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
        let compiles = |code: &str| {
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
        };
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
    }
}
