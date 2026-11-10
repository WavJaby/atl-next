// SPDX-License-Identifier: MIT
//! x86-64 encodings against the Intel SDM Vol. 2 forms (MOVABS REX.W B8+rd io, JMP FF /4 with REX.B, CALL E8 cd, INT3 CC), checked with GNU as 2.44 + objdump. Expected values are literals, never the emitter's output.

use atl_bridgegen::x86::{constants_rust, down_stub, entries_asm, jump};

#[test]
fn jump_is_movabs_r11_then_jmp_r11() {
    // movabs $0x1122334455667788, %r11 = 49 bb 88 77 66 55 44 33 22 11; jmp *%r11 = 41 ff e3
    assert_eq!(
        jump(0x1122_3344_5566_7788),
        [
            0x49, 0xBB, 0x88, 0x77, 0x66, 0x55, 0x44, 0x33, 0x22, 0x11, 0x41, 0xFF, 0xE3
        ]
    );
}

#[test]
fn down_stub_is_two_movabs_then_jmp_r11_then_int3() {
    // movabs $target, %r10 = 49 ba + imm64; movabs $entry, %r11 = 49 bb + imm64; jmp *%r11 = 41 ff e3; int3 = cc
    assert_eq!(
        down_stub(0x1122_3344_5566_7788, 0x8877_6655_4433_2211),
        [
            0x49, 0xBA, 0x88, 0x77, 0x66, 0x55, 0x44, 0x33, 0x22, 0x11, //
            0x49, 0xBB, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, //
            0x41, 0xFF, 0xE3, 0xCC,
        ]
    );
}

#[test]
fn templates_are_the_zero_immediate_forms() {
    let src = constants_rust();
    assert!(
        src.contains(
            "pub const JUMP: [u8; 13] = [0x49, 0xbb, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x41, 0xff, 0xe3];"
        ),
        "{src}"
    );
    assert!(
        src.contains(
            "pub const DOWN: [u8; 24] = [0x49, 0xba, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x49, 0xbb, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x41, 0xff, 0xe3, 0xcc];"
        ),
        "{src}"
    );
    assert!(
        src.contains("pub const STUB_PREFIX: [u8; 2] = [0x49, 0xbb];"),
        "{src}"
    );
    assert!(
        src.contains("pub const HOOK_PLACEHOLDER: [u8; 2] = [0x0f, 0x0b];"),
        "{src}"
    );
}

#[test]
fn entries_text_layout() {
    // slot k at pool + 8k: `e8`, then rel32 = target - (slot + 5) written as `target - . - 4` (`.` = slot + 1)
    let want = concat!(
        ".text\n",
        ".globl atl_nb_cross_to_host\n",
        ".type atl_nb_cross_to_host,@function\n",
        "atl_nb_cross_to_host:\n",
        "    pop %r11\n",
        "    lea atl_nb_host_thunks+5(%rip), %r10\n",
        "    sub %r10, %r11\n",
        "    lea atl_nb_host_targets(%rip), %r10\n",
        "    mov (%r10,%r11), %r11\n",
        "    jmp atl_nb_up\n",
        ".size atl_nb_cross_to_host, .-atl_nb_cross_to_host\n",
        ".globl atl_nb_jni_up\n",
        ".type atl_nb_jni_up,@function\n",
        "atl_nb_jni_up:\n",
        "    pop %r11\n",
        "    lea atl_nb_jni_stubs+5(%rip), %r10\n",
        "    sub %r10, %r11\n",
        "    mov 8(%rdi), %rdi\n",
        "    mov (%rdi), %r10\n",
        "    mov (%r10,%r11), %r11\n",
        "    jmp atl_nb_up\n",
        ".size atl_nb_jni_up, .-atl_nb_jni_up\n",
        ".p2align 3\n",
        ".globl atl_nb_host_thunks\n",
        "atl_nb_host_thunks:\n",
        "    .byte 0xe8\n",
        "    .long atl_nb_cross_to_host - . - 4\n",
        "    .p2align 3\n",
        "    .byte 0xe8\n",
        "    .long atl_nb_cross_to_host - . - 4\n",
        "    .p2align 3\n",
        ".p2align 3\n",
        ".globl atl_nb_jni_stubs\n",
        "atl_nb_jni_stubs:\n",
        "    .byte 0xe8\n",
        "    .long atl_nb_jni_up - . - 4\n",
        "    .p2align 3\n",
    );
    assert_eq!(entries_asm(2, 1, 0), want);
}

#[test]
fn displacement_break_is_one_byte_short() {
    // disp_bias 1: `target - . - 3` gives rel32 = target - (slot + 4), so the call lands at target + 1
    let text = entries_asm(1, 1, 1);
    assert!(
        text.contains("    .long atl_nb_cross_to_host - . - 3\n"),
        "{text}"
    );
    assert!(text.contains("    .long atl_nb_jni_up - . - 3\n"), "{text}");
    assert!(!text.contains("- 4\n"), "{text}");
}

#[test]
#[should_panic(expected = "leaves no displacement end")]
fn bias_past_the_displacement_is_refused() {
    // CALL_LEN 5: a bias of 4 would leave the displacement with no bytes to its end
    entries_asm(1, 1, 4);
}
