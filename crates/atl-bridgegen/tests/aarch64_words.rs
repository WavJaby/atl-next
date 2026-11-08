// SPDX-License-Identifier: MIT
//! aarch64 thunk words against the ARM ARM encodings. Expected values are literals, checked with clang-19's `llvm-mc -show-encoding`, never derived from the emitter under test.

use atl_bridgegen::aarch64::{PLACEHOLDER, place_words, pool_asm};

#[test]
fn thunk_words() {
    // svc #0x8000 = d4100001, svc #0x8003 = d4100061, svc #0x8fff = d411ffe1, ret = d65f03c0
    assert_eq!(place_words(0), [0xD410_0001, 0xD65F_03C0]);
    assert_eq!(place_words(3), [0xD410_0061, 0xD65F_03C0]);
    assert_eq!(place_words(0x0FFF), [0xD411_FFE1, 0xD65F_03C0]);
}

#[test]
fn placeholder_words() {
    // mov x3, #0 = d2800003; blr x3 = d63f0060
    assert_eq!(PLACEHOLDER, [0xD280_0003, 0xD63F_0060]);
}

#[test]
#[should_panic(expected = "does not fit the svc immediate")]
fn id_outside_the_immediate_is_refused() {
    place_words(0x8000);
}

#[test]
fn pool_text_layout() {
    // slot 0 carries id 5 (svc #0x8005 = d41000a1), slot 1 id 6 (svc #0x8006 = d41000c1)
    let want = concat!(
        ".section .atl_thunks,\"ax\",@progbits\n",
        ".p2align 12\n",
        ".globl atl_bind_pool\n",
        ".type atl_bind_pool,@function\n",
        "atl_bind_pool:\n",
        "    .inst 0xd41000a1\n",
        "    .inst 0xd65f03c0\n",
        "    .inst 0xd41000c1\n",
        "    .inst 0xd65f03c0\n",
        ".size atl_bind_pool, . - atl_bind_pool\n",
    );
    assert_eq!(pool_asm(2, |i| i + 5), want);
}
