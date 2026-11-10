// SPDX-License-Identifier: MIT
//! The engine C ABI's build-time facts, all from `crates/atl-engine-api/include/atl_engine.h`: the layouts and values clang reads from it (the route QEMU's CPU layouts take, `rust::layout_module`), and the version script of the engine `.so`.
//! Consumers: atl-qemu-engine's build script (the layout check against atl-engine-api's Rust mirror) and xtask's engine link and nm gate, which take [`TLS_EXPORTS`] and [`version_script`] as the one list.

use std::path::Path;

use anyhow::{Context, Result};

use crate::{Lang, Source, Target, buildscript, interface, rust};

/// The table's exported name.
pub const TABLE: &str = "atl_engine_v1";

/// The engine `.so`'s exports besides [`TABLE`]: QEMU's `__thread` state of the calling host thread, read by the engine with dlsym on its own handle (`thread_cpu`: the thread's CPUState; `helper_retaddr`: its fault slot). Everything else in the `.so` is local.
pub const TLS_EXPORTS: &[&str] = &["thread_cpu", "helper_retaddr"];

/// The header's facts, as (name in the generated Rust module, C expression): every size, offset and value the Rust mirror states.
const FACTS: &[(&str, &str)] = &[
    ("ATL_ABI_MAJOR", "ATL_ENGINE_ABI_MAJOR"),
    ("ATL_ABI_MINOR", "ATL_ENGINE_ABI_MINOR"),
    ("ATL_VALUE_PROT_READ", "ATL_PROT_READ"),
    ("ATL_VALUE_PROT_WRITE", "ATL_PROT_WRITE"),
    ("ATL_VALUE_PROT_EXEC", "ATL_PROT_EXEC"),
    ("ATL_VALUE_TRAP_BOOT", "ATL_TRAP_BOOT"),
    ("ATL_VALUE_TRAP_CALL", "ATL_TRAP_CALL"),
    ("ATL_VALUE_TRAP_BIND", "ATL_TRAP_BIND"),
    ("ATL_VALUE_TRAP_REPORT", "ATL_TRAP_REPORT"),
    ("ATL_SIZEOF_HOST_V1", "sizeof(atl_host_v1)"),
    ("ATL_OFFSET_HOST_MAJOR", "offsetof(atl_host_v1, major)"),
    ("ATL_OFFSET_HOST_MINOR", "offsetof(atl_host_v1, minor)"),
    ("ATL_OFFSET_HOST_SIZE", "offsetof(atl_host_v1, size)"),
    ("ATL_OFFSET_HOST_CTX", "offsetof(atl_host_v1, ctx)"),
    ("ATL_OFFSET_HOST_TRAP", "offsetof(atl_host_v1, trap)"),
    ("ATL_OFFSET_HOST_TRAP_EX", "offsetof(atl_host_v1, trap_ex)"),
    ("ATL_SIZEOF_LANES_V1", "sizeof(atl_lanes_v1)"),
    ("ATL_OFFSET_LANES_X", "offsetof(atl_lanes_v1, x)"),
    ("ATL_OFFSET_LANES_V", "offsetof(atl_lanes_v1, v)"),
    ("ATL_OFFSET_LANES_STACK", "offsetof(atl_lanes_v1, stack)"),
    ("ATL_SIZEOF_ENGINE_V1", "sizeof(atl_engine_v1)"),
    ("ATL_OFFSET_ENGINE_MAJOR", "offsetof(atl_engine_v1, major)"),
    ("ATL_OFFSET_ENGINE_MINOR", "offsetof(atl_engine_v1, minor)"),
    ("ATL_OFFSET_ENGINE_SIZE", "offsetof(atl_engine_v1, size)"),
    (
        "ATL_OFFSET_ENGINE_CREATE",
        "offsetof(atl_engine_v1, create)",
    ),
    (
        "ATL_OFFSET_ENGINE_DESTROY",
        "offsetof(atl_engine_v1, destroy)",
    ),
    ("ATL_OFFSET_ENGINE_MAP", "offsetof(atl_engine_v1, map)"),
    (
        "ATL_OFFSET_ENGINE_ATTACH",
        "offsetof(atl_engine_v1, attach)",
    ),
    (
        "ATL_OFFSET_ENGINE_DETACH",
        "offsetof(atl_engine_v1, detach)",
    ),
    ("ATL_OFFSET_ENGINE_CALL", "offsetof(atl_engine_v1, call)"),
];

/// The Rust module (`pub const ATL_...: u64`) of [`FACTS`] for the header at `header`, read by clang for `host`. The C unit goes into `out`.
pub fn layout_module(header: &Path, out: &Path, host: &str) -> Result<String> {
    let unit = out.join("engine_abi.c");
    let mut text = String::from("#include <stddef.h>\n");
    text.push_str(&format!("#include \"{}\"\n", header.display()));
    for (name, expr) in FACTS {
        text.push_str(&format!("#define {name} {expr}\n"));
    }
    std::fs::write(&unit, text)?;
    let macros: Vec<&str> = FACTS.iter().map(|&(n, _)| n).collect();
    let also = [header.to_path_buf()];
    let source = Source {
        path: &unit,
        lang: Lang::C,
        also: &also,
        macros: &macros,
    };
    // the host view: the engine `.so` runs in the host process, compiled by the host's clang
    let target = Target {
        arch: host.to_owned(),
        triple: buildscript::clang_triple_from_cargo()?,
        args: Vec::new(),
    };
    let iface = interface(
        "engine_abi",
        "atl-engine-api include/atl_engine.h (engine C ABI)",
        &source,
        std::slice::from_ref(&target),
    )
    .context("engine ABI layouts")?;
    rust::layout_module(&iface, host)
}

/// The engine `.so`'s version script: [`TABLE`] and `exports` global, everything else local. Callers pass [`TLS_EXPORTS`]; a break passes less.
pub fn version_script(exports: &[&str]) -> String {
    let mut s = format!("{{\n  global:\n    {TABLE};\n");
    for name in exports {
        s.push_str(&format!("    {name};\n"));
    }
    s.push_str("  local:\n    *;\n};\n");
    s
}
