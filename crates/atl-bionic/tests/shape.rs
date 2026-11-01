// SPDX-License-Identifier: MIT
//! The generated headers give the host libc bionic's header shape where AOSP code relies on it: each
//! case uses a name bionic's header brings in through an include, or a bionic-only header itself provides
//! (cited from bionic's header at the pin), compiled as C against the host libc without the generated headers (must fail: the gap exists,
//! the probe responds) and with them (must compile).

use std::path::{Path, PathBuf};

use atl_bridgegen::buildscript::{self, Upstreams};
use atl_bridgegen::front::{self, Lang};
use atl_bridgegen::{Source, Target};

/// (bionic's include that brings the name, unit using it). Function names are taken by address: unit
/// bodies are not parsed.
const CASES: &[(&str, &str)] = &[
    // libprotoutil, libimg_utils
    (
        "stdint.h: #include <stddef.h>",
        "#include <stdint.h>\nsize_t v;\n",
    ),
    // libnetd_client
    (
        "sys/types.h: #include <stdint.h>",
        "#include <sys/types.h>\nuint32_t v;\n",
    ),
    // libion, libdmabufheap
    (
        "fcntl.h: #include <sys/types.h>, which includes <stddef.h>",
        "#include <fcntl.h>\nsize_t v;\n",
    ),
    // libvulkanallocator
    (
        "stdlib.h: #include <malloc.h>",
        "#include <stdlib.h>\n__typeof__(malloc_usable_size) *v = malloc_usable_size;\n",
    ),
    // libnetdutils `::open`
    (
        "sys/eventfd.h: #include <fcntl.h>",
        "#include <sys/eventfd.h>\n__typeof__(open) *v = open;\n",
    ),
    // libnetdutils, libdmabufheap
    (
        "unistd.h: #include <bits/ioctl.h>, which declares ioctl",
        "#include <unistd.h>\n__typeof__(ioctl) *v = ioctl;\n",
    ),
    // libcore ojluni FileDescriptor_md.c (C, no _GNU_SOURCE); bare, glibc leaves it undefined (an undeclared call would only warn)
    (
        "unistd.h: #define TEMP_FAILURE_RETRY(exp)",
        "#include <errno.h>\n#include <unistd.h>\n#ifndef TEMP_FAILURE_RETRY\n#error\n#endif\nint f(int fd) { return TEMP_FAILURE_RETRY(close(fd)); }\n",
    ),
    // libtimeinstate (bpf KernelUtils.h); the uapi headers name struct fields `__unused`, which bionic's
    // copies rename (libc/kernel/tools/defaults.py), so they must still compile after the definition
    (
        "sys/cdefs.h: #define __unused __attribute__((__unused__))",
        "#include <sys/types.h>\n#include <linux/sysctl.h>\n#include <linux/icmp.h>\nstatic int v __unused;\n",
    ),
    // libmedia mediascanner.h `struct MediaAlbumArt {…} __packed;`; bare, `__packed` declares a
    // variable of the unpacked struct (8 bytes: `i` aligned to 4), so the size check fails
    (
        "sys/cdefs.h: #define __packed __attribute__((__packed__))",
        "#include <sys/types.h>\nstruct s { char c; int i; } __packed;\nint v[sizeof(struct s) == 5 ? 1 : -1];\n",
    ),
    // libc_malloc_debug_backtrace MapData.cpp; bare, the call is an undeclared function
    (
        "elf.h: #include <bits/elf_common.h>, which defines IS_ELF(ehdr)",
        "#include <elf.h>\nextern const Elf64_Ehdr e;\nint v = sizeof(IS_ELF(e));\n",
    ),
    // libpackagelistparser `gid > GID_MAX`
    (
        "limits.h: #define GID_MAX UINT_MAX",
        "#include <limits.h>\nint v[GID_MAX == UINT_MAX ? 1 : -1];\n",
    ),
    // libandroid_net through bionic's resolv_netid.h declarations
    (
        "sys/cdefs.h: #define __LIBC_HIDDEN__ __attribute__((__visibility__(\"hidden\")))",
        "#include <sys/cdefs.h>\nint f(void) __LIBC_HIDDEN__;\n",
    ),
    // libandroid_net net.c, libpackagelistparser; bare, the header is missing
    (
        "sys/limits.h: #include <limits.h>",
        "#include <sys/limits.h>\nint v = INT_MAX;\n",
    ),
    // libmediametrics ItemSerialize-*.cpp includes it; its own macros, values as bionic writes them
    (
        "sys/endian.h: #define _LITTLE_ENDIAN 1234, #define _BYTE_ORDER _LITTLE_ENDIAN",
        "#include <sys/endian.h>\nint v[_BYTE_ORDER == 1234 ? 1 : -1];\n",
    ),
    (
        "sys/endian.h: #define htonq(x) __swap64(x), __swap64 -> __builtin_bswap64",
        "#include <sys/endian.h>\nint v[htonq(1) == 0x0100000000000000 ? 1 : -1];\n",
    ),
    // libmediadrm DrmUtils.h, libmediandk; bare, an undeclared function (glibc names it be64toh)
    (
        "endian.h: #include <sys/endian.h>, which defines betoh64(x) __swap64(x)",
        "#include <endian.h>\nint v[betoh64(1) == 0x0100000000000000 ? 1 : -1];\n",
    ),
    // libmemunreachable ScopedPipe.h via LeakPipe.h; value from bionic's uapi asm-generic/fcntl.h
    (
        "sys/socket.h: #include <asm/fcntl.h>, which defines O_CLOEXEC 02000000",
        "#include <sys/socket.h>\nint v[O_CLOEXEC == 02000000 ? 1 : -1];\n",
    ),
    // hwui Bitmap.cpp `mallopt(M_PURGE, 0)`
    (
        "malloc.h: #define M_PURGE (-101)",
        "#include <malloc.h>\nint v[M_PURGE == -101 ? 1 : -1];\n",
    ),
    // libandroid_runtime NativeLibraryHelper.cpp; 0xEF53 is ext2/3/4's on-disk superblock magic (kernel ext4.h EXT4_SUPER_MAGIC)
    (
        "sys/statfs.h: #include <sys/vfs.h>, which includes <linux/magic.h>",
        "#include <sys/statfs.h>\nint v[EXT4_SUPER_MAGIC == 0xEF53 ? 1 : -1];\n",
    ),
    // libandroid_runtime AndroidRuntime.cpp setArgv0; bare, an undeclared function
    (
        "stdlib.h: void setprogname(const char*)",
        "#include <stdlib.h>\n__typeof__(setprogname) *v = setprogname;\n",
    ),
    // ART oat_file_manager.cc ShouldLoadAppImage (__ANDROID__); bare, an undeclared function
    (
        "stdlib.h: const char* getprogname(void)",
        "#include <stdlib.h>\nconst char* (*v)(void) = getprogname;\n",
    ),
    // libandroid_runtime android_os_Debug.cpp `mallopt(M_LOG_STATS, 0)`
    (
        "malloc.h: #define M_LOG_STATS (-205)",
        "#include <malloc.h>\nint v[M_LOG_STATS == -205 ? 1 : -1];\n",
    ),
    // libandroid_runtime android_util_Process.cpp `CPU_ALLOC_SIZE(n) / sizeof(__CPU_BITTYPE)`: glibc's
    // CPU_ALLOC_SIZE counts __cpu_mask elements, so the two must be one type
    (
        "sched.h (__USE_GNU): #define __CPU_BITTYPE unsigned long int",
        "#define _GNU_SOURCE\n#include <sched.h>\nint v[__builtin_types_compatible_p(__CPU_BITTYPE, __cpu_mask) ? 1 : -1];\n",
    ),
    // libmemunreachable ThreadCapture.cpp `offsetof(struct pt_regs, …)`; bare, an incomplete type
    (
        "sys/ptrace.h: #include <linux/ptrace.h>, which includes <asm/ptrace.h>: struct pt_regs",
        "#include <sys/ptrace.h>\nint v = sizeof(struct pt_regs);\n",
    ),
];

fn target(args: Vec<String>) -> Target {
    Target {
        arch: String::new(),
        triple: env!("ATL_BIONIC_CLANG_TRIPLE").to_owned(),
        args,
    }
}

#[test]
fn bionic_includes_reach_glibc_builds() {
    let include = concat!(env!("OUT_DIR"), "/capi/include");
    let units: Vec<String> = CASES.iter().map(|(_, u)| (*u).to_owned()).collect();
    let bare = front::source_errors(Lang::C, &target(Vec::new()), &units).unwrap();
    let wrapped =
        front::source_errors(Lang::C, &target(vec![format!("-isystem{include}")]), &units).unwrap();
    for (((why, unit), bare), wrapped) in CASES.iter().zip(&bare).zip(&wrapped) {
        assert!(
            !bare.is_empty(),
            "no gap to close without the wrappers ({why}):\n{unit}"
        );
        assert!(wrapped.is_empty(), "{why}:\n{unit}{wrapped:#?}");
    }
}

/// bionic defines `__CPU_BITTYPE` only under `__USE_GNU`: without `_GNU_SOURCE` the wrapper keeps it out,
/// with it the same unit sees it (the probe responds).
#[test]
fn gnu_only_names_keep_bionic_guard() {
    let include = concat!(env!("OUT_DIR"), "/capi/include");
    let unit = "#include <sched.h>\n#ifdef __CPU_BITTYPE\n#error __CPU_BITTYPE outside __USE_GNU\n#endif\n";
    let units = [unit.to_owned(), format!("#define _GNU_SOURCE\n{unit}")];
    let errs =
        front::source_errors(Lang::C, &target(vec![format!("-isystem{include}")]), &units).unwrap();
    assert!(errs[0].is_empty(), "{:#?}", errs[0]);
    assert!(!errs[1].is_empty(), "_GNU_SOURCE did not reach the wrapper");
}

/// The generated `struct pt_regs` the `<sys/ptrace.h>` wrapper brings has the layout of bionic's uapi
/// `<asm/ptrace.h>` for this arch, read by clang in both languages: size, alignment, every field's name,
/// offset and type.
#[test]
fn pt_regs_has_bionic_uapi_layout() {
    let checkout = buildscript::checkout(Path::new(env!("CARGO_MANIFEST_DIR"))).unwrap();
    let up = Upstreams::of(&checkout).unwrap();
    let arches = std::fs::read_to_string(checkout.join("arches.toml")).unwrap();
    let host = buildscript::host_arch(&arches, env!("ATL_BIONIC_TARGET")).unwrap();
    let bionic_t = buildscript::bionic_libc_target(&up, &host, None);
    let uapi = front::resolve_include(Lang::C, &bionic_t, "asm/ptrace.h").unwrap();
    let bionic = front::parse(&Source::c(&uapi, &[]), &[bionic_t]).unwrap();
    let want = &bionic[&host.arch].records["pt_regs"];
    assert!(!want.fields.is_empty());

    let include = PathBuf::from(concat!(env!("OUT_DIR"), "/capi/include"));
    let generated = [include.join("atl-bionic/uapi/asm/ptrace.h")];
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
    for (lang, name) in [(Lang::C, "pt_regs.c"), (Lang::Cxx, "pt_regs.cc")] {
        let unit = dir.join(name);
        std::fs::write(&unit, "#include <sys/ptrace.h>\n").unwrap();
        let src = Source {
            path: &unit,
            lang,
            also: &generated,
            macros: &[],
        };
        let mut t = target(vec![format!("-isystem{}", include.display())]);
        t.arch = host.arch.clone();
        let got = front::parse(&src, &[t]).unwrap();
        assert_eq!(&got[&host.arch].records["pt_regs"], want, "{lang:?}");
    }
}
