// SPDX-License-Identifier: MIT
//! aarch64 guest thunks for the QEMU engine (QEMU bridge plan Q4). A binding thunk is two words, `svc #(BIND | id)` then `ret`; the svc immediate carries the binding id, which atl-qemu-engine's trap.rs decodes. `BIND` here and there must agree: the spike gate (l) fails when they do not.
//! The pool is a page-aligned run of thunks in an executable section, slot k holding binding k. [`pool_asm`] is assembly text for a guest build only (xtask writes it into the fixture build and the NDK clang assembles it); no host code links it.

use std::fmt::Write;

/// `svc #0`: the base encoding, imm16 at bits 20:5.
pub const SVC: u32 = 0xD400_0001;
/// `ret` (through x30).
pub const RET: u32 = 0xD65F_03C0;
/// Bit 15 of the svc immediate: a binding (`atl_qemu_engine::trap::BIND`). The low 15 bits are the id, so 32768 bindings.
pub const BIND: u32 = 0x8000;
/// The placeholder body of an intercept stub as the NB sources ship it: `ldr x3, =0; blr x3`, assembled to `mov x3, #0; blr x3`. In-place writes replace it.
pub const PLACEHOLDER: [u32; 2] = [0xD280_0003, 0xD63F_0060];

/// The two words of binding `id`'s thunk. Panics when `id` does not fit the immediate (`id >= BIND`).
pub fn place_words(id: u32) -> [u32; 2] {
    assert!(
        id < BIND,
        "binding id {id:#x} does not fit the svc immediate"
    );
    [SVC | ((BIND | id) << 5), RET]
}

/// Assembly for a page-aligned pool of `entries` thunks, symbol `atl_bind_pool`, section `.atl_thunks` (executable, not writable). Slot `i` carries binding `id_of(i)`; the production pool is the identity.
/// Panics when an id does not fit the immediate.
pub fn pool_asm(entries: u32, id_of: impl Fn(u32) -> u32) -> String {
    let mut s = String::from(
        ".section .atl_thunks,\"ax\",@progbits\n.p2align 12\n.globl atl_bind_pool\n.type atl_bind_pool,@function\natl_bind_pool:\n",
    );
    for i in 0..entries {
        let [svc, ret] = place_words(id_of(i));
        writeln!(s, "    .inst {svc:#010x}\n    .inst {ret:#010x}").expect("writing to a String");
    }
    writeln!(s, ".size atl_bind_pool, . - atl_bind_pool").expect("writing to a String");
    s
}
