// SPDX-License-Identifier: MIT
//! The generated headers give the host libc bionic's header shape where AOSP code relies on it: each
//! case uses a name bionic's header brings in through an include (cited from bionic's header at the
//! pin), compiled as C against the host libc without the generated headers (must fail: the gap exists,
//! the probe responds) and with them (must compile).

use atl_bridgegen::Target;
use atl_bridgegen::front::{self, Lang};

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
    // libtimeinstate (bpf KernelUtils.h); the uapi headers name struct fields `__unused`, which bionic's
    // copies rename (libc/kernel/tools/defaults.py), so they must still compile after the definition
    (
        "sys/cdefs.h: #define __unused __attribute__((__unused__))",
        "#include <sys/types.h>\n#include <linux/sysctl.h>\n#include <linux/icmp.h>\nstatic int v __unused;\n",
    ),
    // libc_malloc_debug_backtrace MapData.cpp; bare, the call is an undeclared function
    (
        "elf.h: #include <bits/elf_common.h>, which defines IS_ELF(ehdr)",
        "#include <elf.h>\nextern const Elf64_Ehdr e;\nint v = sizeof(IS_ELF(e));\n",
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
