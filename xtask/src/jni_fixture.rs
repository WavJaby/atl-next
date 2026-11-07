// SPDX-License-Identifier: MIT
//! `xtask test jni-fixture`: the bridge-1 JNI fixture (xtask/testdata/jni-test), a stand-in for third-party app native code, built with the NDK's clang against bionic + libc++_shared exactly as an app's CMake/ndk-build would.
//! Two variants, because the TLS model depends on minSdk: [`EMULATED_TLS_API`] compiles `__thread` to emulated TLS (`__emutls_get_address` import, no PT_TLS), [`ELF_TLS_API`] (the first level with ELF TLS in bionic) to native TLS (`__tls_get_addr`, PT_TLS).
//! The M2 test APK (`xtask test app --fixture-api`) packages either variant into `lib/<abi>/`, its API level as the APK's minSdk.
//!
//! Gate, per variant:
//! - ELF: DT_NEEDED has `libc.so` (bionic's soname; glibc's is `libc.so.6`) and `libc++_shared.so`, the GNU verneed asks `libc.so` for version `LIBC`, the TLS shape is the variant's;
//! - glibc probe: host ld.so loading the fixture (LD_PRELOAD into a trivial host program; `dlopen(3)` would need unsafe, which xtask forbids, and preload maps through the same `_dl_map_object` path) must fail on the bionic ABI: `version `LIBC' not found`.
//!   The probe's library path maps the bionic sonames (`libc.so`, `libm.so`, `libdl.so`) to glibc's, so the load gets past name resolution (unmapped, glibc picks up `libm.so`, the GNU ld script, and fails for an unrelated reason) and reaches version binding, the failure bridge 1 removes.
//! - controls, so a pass means something: the probe loads a glibc library cleanly (it responds), and each variant fails the other variant's TLS expectation (the TLS check discriminates).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use anyhow::{Context, Result, bail};
use atl_layout::Layout;
use object::read::elf::{ElfFile64, FileHeader, ProgramHeader};
use object::{Endianness, Object, ObjectSymbol, elf};

use crate::atld_run::NativeLibs;
use crate::{arches, host_art, provided_gate};

#[derive(clap::Args)]
pub struct Args {
    /// The fixture's arch (an arches.toml row with `[guest]`); its first `android_abis` entry names the APK lib dir.
    #[arg(long)]
    arch: String,
    /// NDK root (default: <workspace>/tools/[`NDK_DIR`]).
    #[arg(long)]
    ndk: Option<PathBuf>,
    /// Output directory (default: <workspace>/out/jni-fixture).
    #[arg(long)]
    out: Option<PathBuf>,
}

/// The NDK release the fixture is built with (owner decision D1, bridge-1 plan); `--ndk` overrides.
const NDK_DIR: &str = "android-ndk-r30";
/// The fixture's sources.
const SOURCES: &str = "xtask/testdata/jni-test";
/// The emulated-TLS variant: bionic had no ELF TLS before 29, so clang emits emulated TLS below it.
pub const EMULATED_TLS_API: u32 = 21;
/// First API level whose bionic supports ELF TLS (Android 10); clang's default TLS model switches to native at this target.
pub const ELF_TLS_API: u32 = 29;
/// The library `System.loadLibrary("atl-jni-test")` maps to, and the sibling it dlopens by name.
const LIB: &str = "libatl-jni-test.so";
const SIBLING: &str = "libatl-jni-sibling.so";
/// libc++ the fixture links (`-stdlib=libc++`, NDK clang++'s shared default); packaged next to the fixture as an app packages it.
const CXX_SHARED: &str = "libc++_shared.so";
/// The C12 legacy fixture (atl_jni_legacy.cpp): C and C++ calls old NDK binaries make, linked to the guest's libstdc++.so and libz.so instead of libc++.
/// Its libc is an empty unversioned stub, as old NDK platforms shipped: no GNU verneed, imports left undefined for the loader (Happy Jump's libndksetup.so has this shape).
const LEGACY: &str = "libatl-jni-legacy.so";
/// DT_NEEDED the legacy fixture names: the guest libraries that answer its calls (atld_run.rs NB_LIBS rows).
const LEGACY_NEEDED: [&str; 2] = ["libstdc++.so", "libz.so"];
/// Imports the legacy fixture carries, each one a call its source makes at run time: a folded call drops its import, and the check fails.
const LEGACY_IMPORTS: [&str; 9] = [
    "__sF",
    "_ctype_",
    "__strlen_chk",
    "_Znwm",
    "_ZdlPv",
    "__cxa_guard_acquire",
    "crc32",
    "compress",
    "uncompress",
];

/// One built variant: its API level and the libraries an APK carries for it.
pub struct Built {
    pub api: u32,
    pub dir: PathBuf,
    pub libs: Vec<PathBuf>,
}

/// The NDK's llvm prebuilt dir for this build machine.
fn toolchain(ndk: &Path) -> PathBuf {
    ndk.join("toolchains/llvm/prebuilt/linux-x86_64")
}

/// The default NDK root.
pub fn default_ndk(root: &Path) -> Result<PathBuf> {
    Ok(Layout::of(root)?.workspace().join("tools").join(NDK_DIR))
}

/// NDK clang for one guest arch at one API level, as an app's CMake/ndk-build drives it: `--target=<arches.toml clang triple><api>`.
pub struct NdkTarget {
    bin: PathBuf,
    target: String,
    /// The arch's `lib/<abi>/` name in an APK: its first arches.toml `android_abis` entry.
    pub abi: String,
    /// The NDK sysroot's per-triple lib dir (libc++_shared.so).
    sysroot_lib: PathBuf,
}

/// Compiler flags of every fixture object: PIC, hidden by default (exports are JNIEXPORT), warnings fatal.
pub const CFLAGS: &[&str] = &[
    "-fPIC",
    "-O2",
    "-fvisibility=hidden",
    "-Wall",
    "-Wextra",
    "-Werror",
    "-Wno-unused-parameter",
];

impl NdkTarget {
    pub fn new(root: &Path, ndk: &Path, arch: &str, api: u32) -> Result<NdkTarget> {
        let arches = arches::load(root)?;
        let guest = arches
            .get(arch)
            .and_then(|a| a.guest.as_ref())
            .with_context(|| format!("arches.toml has no [{arch}.guest] row"))?;
        let tc = toolchain(ndk);
        Ok(NdkTarget {
            bin: tc.join("bin"),
            target: format!("--target={}{api}", guest.clang),
            abi: guest
                .android_abis
                .first()
                .cloned()
                .with_context(|| format!("arches.toml [{arch}.guest] has no android_abis"))?,
            sysroot_lib: tc.join("sysroot/usr/lib").join(&guest.clang),
        })
    }

    /// `tool` (`clang` / `clang++`) with the target set.
    pub fn cmd(&self, tool: &str) -> Command {
        let mut c = Command::new(self.bin.join(tool));
        c.arg(&self.target);
        c
    }
}

/// Builds the fixture for `arch` at `api` into `out/android-<api>`.
/// `legacy_no_libc`: the legacy library loses DT_NEEDED `libc.so`, the marker the NativeBridge core routes on (`xtask test app --legacy-no-libc`).
pub fn build(
    root: &Path,
    ndk: &Path,
    arch: &str,
    api: u32,
    out: &Path,
    legacy_no_libc: bool,
) -> Result<Built> {
    let t = NdkTarget::new(root, ndk, arch, api)?;
    let dir = out.join(format!("android-{api}"));
    if dir.exists() {
        std::fs::remove_dir_all(&dir)?;
    }
    std::fs::create_dir_all(&dir)?;
    let src = root.join(SOURCES);
    let compile = |cc: &str, out_obj: &Path, file: &str, extra: &[&str]| {
        host_art::exec(
            t.cmd(cc)
                .arg("-c")
                .args(CFLAGS)
                .args(extra)
                .arg(src.join(file))
                .arg("-o")
                .arg(out_obj),
            &format!("NDK {cc} {file}"),
        )
    };
    let sibling = dir.join(SIBLING);
    host_art::exec(
        t.cmd("clang")
            .arg("-shared")
            .args(CFLAGS)
            .arg(src.join("atl_jni_sibling.c"))
            .arg(format!("-Wl,-soname,{SIBLING}"))
            .arg("-o")
            .arg(&sibling),
        "NDK clang sibling",
    )?;
    let c_obj = dir.join("atl_jni_test.o");
    let cxx_obj = dir.join("atl_jni_cxx.o");
    compile("clang", &c_obj, "atl_jni_test.c", &["-std=c11"])?;
    compile("clang++", &cxx_obj, "atl_jni_cxx.cpp", &["-std=c++17"])?;
    let lib = dir.join(LIB);
    host_art::exec(
        t.cmd("clang++")
            .args(["-shared", "-static-libgcc"])
            .arg(&c_obj)
            .arg(&cxx_obj)
            .arg(format!("-Wl,-soname,{LIB}"))
            .arg("-llog")
            .arg("-lEGL")
            .arg("-lGLESv2")
            .arg("-o")
            .arg(&lib),
        "NDK clang++ link",
    )?;
    std::fs::remove_file(&c_obj)?;
    std::fs::remove_file(&cxx_obj)?;
    let legacy = dir.join(LEGACY);
    let legacy_obj = dir.join("atl_jni_legacy.o");
    // Old NDK default: C++11 without sized operator delete. The guest's libstdc++ has no `_ZdlPvm` (bionic/libstdc++/include/new: TODO), so clang's C++14 default would make a modern-ABI import this fixture must not carry.
    compile(
        "clang++",
        &legacy_obj,
        "atl_jni_legacy.cpp",
        &[
            "-std=c++11",
            "-fno-sized-deallocation",
            "-D_FORTIFY_SOURCE=2",
        ],
    )?;
    let stubs = dir.join("legacy-stubs");
    std::fs::create_dir_all(&stubs)?;
    host_art::exec(
        t.cmd("clang")
            .args(["-shared", "-nostdlib", "-Wl,-soname,libc.so"])
            .args(["-x", "c", "/dev/null", "-o"])
            .arg(stubs.join("libc.so")),
        "NDK clang legacy libc stub",
    )?;
    // the stub defines nothing, so --as-needed drops it from DT_NEEDED
    let libc_needed = if legacy_no_libc {
        "-Wl,--as-needed"
    } else {
        "-Wl,--no-as-needed"
    };
    host_art::exec(
        t.cmd("clang++")
            .args(["-shared", "-nostdlib++", libc_needed])
            .arg(format!("-L{}", stubs.display()))
            .arg(&legacy_obj)
            .arg(format!("-Wl,-soname,{LEGACY}"))
            .args(["-lstdc++", "-lz"])
            .arg("-o")
            .arg(&legacy),
        "NDK clang++ legacy link",
    )?;
    std::fs::remove_file(&legacy_obj)?;
    std::fs::remove_dir_all(&stubs)?;
    let cxx = t.sysroot_lib.join(CXX_SHARED);
    let cxx_copy = dir.join(CXX_SHARED);
    std::fs::copy(&cxx, &cxx_copy).with_context(|| format!("copying {}", cxx.display()))?;
    Ok(Built {
        api,
        dir,
        libs: vec![lib, sibling, cxx_copy, legacy],
    })
}

/// What the gate reads from one ELF.
pub struct Shape {
    pub needed: Vec<String>,
    /// (file, version) pairs of the GNU verneed.
    verneed: BTreeSet<(String, String)>,
    pub imports: BTreeSet<String>,
    pt_tls: bool,
}

pub fn shape(path: &Path) -> Result<Shape> {
    let data = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let file = ElfFile64::<Endianness>::parse(&*data)
        .with_context(|| format!("{}: not a 64-bit ELF", path.display()))?;
    let endian = file.endian();
    let sections = file.elf_section_table();
    let mut verneed = BTreeSet::new();
    if let Some((mut it, link)) = sections.gnu_verneed(endian, &*data)? {
        let strings = sections.strings(endian, &*data, link)?;
        while let Some((need, mut aux)) = it.next()? {
            let f = String::from_utf8_lossy(
                strings
                    .get(need.vn_file.get(endian))
                    .map_err(|()| anyhow::anyhow!("bad vn_file"))?,
            )
            .into_owned();
            while let Some(a) = aux.next()? {
                let v = strings
                    .get(a.vna_name.get(endian))
                    .map_err(|()| anyhow::anyhow!("bad vna_name"))?;
                verneed.insert((f.clone(), String::from_utf8_lossy(v).into_owned()));
            }
        }
    }
    let pt_tls = file
        .elf_header()
        .program_headers(endian, &*data)?
        .iter()
        .any(|p| p.p_type(endian) == elf::PT_TLS);
    let imports = file
        .dynamic_symbols()
        .filter(|s| s.is_undefined())
        .filter_map(|s| s.name().ok().map(str::to_owned))
        .collect();
    Ok(Shape {
        needed: provided_gate::needed(path)?,
        verneed,
        imports,
        pt_tls,
    })
}

/// The TLS shape expected at `api`: emulated below [`ELF_TLS_API`], native from it.
fn tls_failures(s: &Shape, api: u32) -> Vec<String> {
    let mut f = Vec::new();
    let emulated = s.imports.contains("__emutls_get_address");
    let native = s.pt_tls && s.imports.contains("__tls_get_addr");
    if api < ELF_TLS_API {
        if !emulated || s.pt_tls {
            f.push(format!("android-{api}: expected emulated TLS (__emutls_get_address import, no PT_TLS), got emutls={emulated} PT_TLS={}", s.pt_tls));
        }
    } else if !native || emulated {
        f.push(format!("android-{api}: expected ELF TLS (PT_TLS + __tls_get_addr), got PT_TLS={} __tls_get_addr={} emutls={emulated}", s.pt_tls, s.imports.contains("__tls_get_addr")));
    }
    f
}

/// ELF expectations of the fixture library itself (the sibling and libc++ need libc.so too; the probe covers their load).
fn elf_failures(s: &Shape, api: u32) -> Vec<String> {
    let mut f = Vec::new();
    for n in ["libc.so", CXX_SHARED] {
        if !s.needed.iter().any(|x| x == n) {
            f.push(format!(
                "android-{api}: DT_NEEDED lacks {n} (has {:?})",
                s.needed
            ));
        }
    }
    if !s
        .verneed
        .contains(&("libc.so".to_owned(), "LIBC".to_owned()))
    {
        f.push(format!(
            "android-{api}: no verneed libc.so/LIBC (has {:?})",
            s.verneed
        ));
    }
    f.extend(tls_failures(s, api));
    f
}

/// Host ld.so loading `lib` with `libdir` first on its path; its stderr, and whether the program ran cleanly.
fn ld_load(lib: &Path, libdir: &Path) -> Result<(bool, String)> {
    let out = Command::new("/bin/true")
        .env("LD_PRELOAD", lib)
        .env("LD_LIBRARY_PATH", libdir)
        .output()
        .context("running /bin/true under LD_PRELOAD")?;
    let err = String::from_utf8_lossy(&out.stderr).trim().to_owned();
    Ok((out.status.success() && err.is_empty(), err))
}

/// The bionic sonames the probe maps to glibc's; the rest of DT_NEEDED is the fixture's own dir.
const SONAME_MAP: &[(&str, &str)] = &[
    ("libc.so", "/lib/x86_64-linux-gnu/libc.so.6"),
    ("libm.so", "/lib/x86_64-linux-gnu/libm.so.6"),
    ("libdl.so", "/lib/x86_64-linux-gnu/libdl.so.2"),
];

/// The probe's library dir: the soname map, then the fixture's own libraries.
fn probe_dir(built: &Built) -> Result<PathBuf> {
    let dir = built.dir.join("probe-libs");
    if dir.exists() {
        std::fs::remove_dir_all(&dir)?;
    }
    std::fs::create_dir_all(&dir)?;
    for (bionic, glibc) in SONAME_MAP {
        std::os::unix::fs::symlink(glibc, dir.join(bionic))?;
    }
    for l in &built.libs {
        std::os::unix::fs::symlink(l, dir.join(l.file_name().context("lib name")?))?;
    }
    Ok(dir)
}

/// The glibc probe of one variant; failure messages.
fn probe_failures(built: &Built) -> Result<Vec<String>> {
    let dir = probe_dir(built)?;
    let mut f = Vec::new();
    for lib in [LIB, SIBLING] {
        let (clean, err) = ld_load(&built.dir.join(lib), &dir)?;
        let line = err.lines().next().unwrap_or("");
        if clean {
            f.push(format!("android-{}: glibc loaded {lib} cleanly", built.api));
        } else if !err.contains("version `LIBC' not found") {
            f.push(format!(
                "android-{}: glibc rejected {lib} for another reason: {line}",
                built.api
            ));
        } else {
            println!("pass glibc probe android-{} {lib}: {line}", built.api);
        }
    }
    std::fs::remove_dir_all(&dir)?;
    Ok(f)
}

/// The NativeBridge core's `isSupported` verdict (atl-native-bridge `elf::classify`) on the fixture: ours; with glibc's libc as control: not ours.
fn nb_failures(lib: &Path, api: u32) -> Result<Vec<String>> {
    use atl_native_bridge::elf::{Verdict, classify, host_machine};
    let host = host_machine().map_err(anyhow::Error::msg)?;
    let mut fails = Vec::new();
    match classify(&std::fs::read(lib)?, host) {
        Verdict::Bridged => println!("pass nb android-{api}: isSupported takes it"),
        v => fails.push(format!("nb android-{api}: isSupported refuses it ({v:?})")),
    }
    let glibc = Path::new(SONAME_MAP[1].1);
    match classify(&std::fs::read(glibc)?, host) {
        Verdict::NotBionic => {}
        v => fails.push(format!(
            "nb control: {} judged {v:?}, want NotBionic",
            glibc.display()
        )),
    }
    Ok(fails)
}

pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    let ndk = match &args.ndk {
        Some(n) => n.clone(),
        None => default_ndk(root)?,
    };
    let out = match &args.out {
        Some(o) => o.clone(),
        None => Layout::of(root)?.workspace().join("out/jni-fixture"),
    };
    let mut fails = Vec::new();
    // control: the probe responds -- a glibc library loads without a word
    let (clean, err) = ld_load(Path::new(SONAME_MAP[1].1), Path::new("/nonexistent"))?;
    if clean {
        println!(
            "pass probe control: glibc loads {} cleanly",
            SONAME_MAP[1].1
        );
    } else {
        fails.push(format!(
            "probe control: glibc failed to load {}: {err}",
            SONAME_MAP[1].1
        ));
    }
    let mut shapes = Vec::new();
    for api in [EMULATED_TLS_API, ELF_TLS_API] {
        let built = build(root, &ndk, &args.arch, api, &out, false)?;
        let s = shape(&built.dir.join(LIB))?;
        let f = elf_failures(&s, api);
        if f.is_empty() {
            println!(
                "pass elf android-{api}: NEEDED {:?}, verneed libc.so/LIBC, TLS {}",
                s.needed,
                if api < ELF_TLS_API { "emulated" } else { "ELF" }
            );
        }
        fails.extend(f);
        fails.extend(probe_failures(&built)?);
        fails.extend(nb_failures(&built.dir.join(LIB), api)?);
        fails.extend(nb_failures(&built.dir.join(LEGACY), api)?);
        shapes.push((api, s));
    }
    // control: each variant fails the other's TLS expectation
    for ((api, s), (other, _)) in shapes.iter().zip(shapes.iter().rev()) {
        if tls_failures(s, *other).is_empty() {
            fails.push(format!(
                "TLS control: android-{api} also passes android-{other}'s TLS expectation"
            ));
        } else {
            println!("pass TLS control: android-{api} fails android-{other}'s TLS expectation");
        }
    }
    for f in &fails {
        println!("FAIL {f}");
    }
    println!("{}", crate::verdict::gate("jni-fixture", &fails));
    if fails.is_empty() {
        Ok(ExitCode::SUCCESS)
    } else {
        Ok(ExitCode::FAILURE)
    }
}

/// The libraries the M2 test APK carries in `lib/<abi>/`: the `api` variant, built into `out`; `api` is also the APK's minSdk.
pub fn apk_libs(
    root: &Path,
    arch: &str,
    api: u32,
    out: &Path,
    legacy_no_libc: bool,
) -> Result<NativeLibs> {
    let ndk = default_ndk(root)?;
    let abi = NdkTarget::new(root, &ndk, arch, api)?.abi;
    let built = build(root, &ndk, arch, api, out, legacy_no_libc)?;
    if built.libs.is_empty() {
        bail!("no fixture libraries built");
    }
    Ok(NativeLibs {
        abi,
        libs: built.libs,
        min_sdk: api,
    })
}

/// The C12 legacy fixture's ELF expectations: DT_NEEDED names the guest libraries that answer its calls, and each import is one its source calls at run time.
fn legacy_shape_failures(s: &Shape) -> Vec<String> {
    let mut f = Vec::new();
    // libstdc++.so's own stub stays versioned: only libc's is the old-NDK shape
    if s.verneed.iter().any(|(file, _)| file == "libc.so") {
        f.push(format!(
            "{LEGACY}: want no verneed on libc.so, got {:?}",
            s.verneed
        ));
    }
    for n in LEGACY_NEEDED {
        if !s.needed.iter().any(|x| x == n) {
            f.push(format!(
                "{LEGACY}: DT_NEEDED lacks {n} (has {:?})",
                s.needed
            ));
        }
    }
    for i in LEGACY_IMPORTS {
        if !s.imports.contains(i) {
            f.push(format!("{LEGACY}: no import {i}"));
        }
    }
    f
}

/// The legacy fixture among the APK's `libs` (apk_libs), checked by `legacy_shape_failures`.
pub fn legacy_failures(libs: &[PathBuf]) -> Result<Vec<String>> {
    let lib = libs
        .iter()
        .find(|l| l.file_name().is_some_and(|n| n == LEGACY))
        .with_context(|| format!("no {LEGACY} among the fixture libraries"))?;
    Ok(legacy_shape_failures(&shape(lib)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_shape_wants_its_guest_libs_and_calls() {
        let shape_of = |needed: &[&str], drop: Option<&str>| Shape {
            needed: needed.iter().map(|&n| n.to_owned()).collect(),
            verneed: BTreeSet::new(),
            imports: LEGACY_IMPORTS
                .iter()
                .filter(|&&i| Some(i) != drop)
                .map(|&i| i.to_owned())
                .collect(),
            pt_tls: false,
        };
        let all = ["libc.so", "libstdc++.so", "libz.so"];
        assert!(legacy_shape_failures(&shape_of(&all, None)).is_empty());
        let mut versioned = shape_of(&all, None);
        versioned.verneed.insert(("libc.so".into(), "LIBC".into()));
        assert_eq!(legacy_shape_failures(&versioned).len(), 1);
        assert_eq!(
            legacy_shape_failures(&shape_of(&["libc.so", "libstdc++.so"], None)).len(),
            1
        );
        assert_eq!(
            legacy_shape_failures(&shape_of(&all, Some("_Znwm"))).len(),
            1
        );
    }

    fn s(imports: &[&str], pt_tls: bool) -> Shape {
        Shape {
            needed: vec!["libc.so".into(), CXX_SHARED.into()],
            verneed: BTreeSet::from([("libc.so".into(), "LIBC".into())]),
            imports: imports.iter().map(|&i| i.to_owned()).collect(),
            pt_tls,
        }
    }

    #[test]
    fn tls_shape_follows_api() {
        let emu = s(&["__emutls_get_address"], false);
        let elf = s(&["__tls_get_addr"], true);
        assert!(elf_failures(&emu, EMULATED_TLS_API).is_empty());
        assert!(elf_failures(&elf, ELF_TLS_API).is_empty());
        assert!(!tls_failures(&emu, ELF_TLS_API).is_empty());
        assert!(!tls_failures(&elf, EMULATED_TLS_API).is_empty());
    }

    #[test]
    fn glibc_soname_is_not_bionic() {
        let mut x = s(&["__emutls_get_address"], false);
        x.needed = vec!["libc.so.6".into(), CXX_SHARED.into()];
        x.verneed = BTreeSet::from([("libc.so.6".into(), "GLIBC_2.2.5".into())]);
        assert_eq!(elf_failures(&x, EMULATED_TLS_API).len(), 2);
    }
}
