// SPDX-License-Identifier: MIT
// Guest side of the Q7-4 GL lanes gate (xtask `test qemu-gl`): a static executable. main hands the host a table of entries through BOOT; the host calls each with x0 = the pool address of one GL proxy binding, and the entry calls it through the NDK header's PFN type, so the arguments sit where NDK clang puts them for that GL prototype (fp in s-registers, the ninth integer on the stack).
// The host's recorder checks every argument bit for bit against the literals in tests/qemu_gl.rs (which repeat them). Each entry returns 0, except qg_path_length: the bits of the float the binding answered.
#include <GLES2/gl2.h>
#include <GLES2/gl2ext.h>
#include <stdint.h>
#include <string.h>

// Where a host call returns: LR points here; the hook leaves the innermost cpu_loop.
__asm__(
    ".globl qg_ret_stub\n"
    ".type qg_ret_stub,%function\n"
    "qg_ret_stub:\n"
    "    svc #0x7a11\n"
    "    b qg_ret_stub\n");
void qg_ret_stub(void);

// Pointer arguments: values only, never dereferenced by the recorder.
#define TEX_PIXELS ((const void *)(uintptr_t)0x00007e5712345670ULL)
#define SUB_PIXELS ((const void *)(uintptr_t)0x00005ab500009ab0ULL)
#define ETC_DATA ((const void *)(uintptr_t)0x0000ec1000c0ffe0ULL)

uint64_t qg_clear_color(PFNGLCLEARCOLORPROC f) {
    f(0.125f, -2.5f, 0.0625f, 3.75f);
    return 0;
}

uint64_t qg_uniform4f(PFNGLUNIFORM4FPROC f) {
    f(-7, 1.5f, -0.25f, 1024.5f, 0.375f);
    return 0;
}

uint64_t qg_depth_rangef(PFNGLDEPTHRANGEFPROC f) {
    f(0.1875f, 0.8125f);
    return 0;
}

uint64_t qg_tex_parameterf(PFNGLTEXPARAMETERFPROC f) {
    f(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, (GLfloat)GL_LINEAR);
    return 0;
}

uint64_t qg_vertex_attrib4f(PFNGLVERTEXATTRIB4FPROC f) {
    f(5, 2.0f, -3.0f, 4.5f, -5.25f);
    return 0;
}

// 9 integer-class arguments: x0..x7, the ninth (pixels) the first guest stack word
uint64_t qg_tex_image_2d(PFNGLTEXIMAGE2DPROC f) {
    f(GL_TEXTURE_2D, 2, GL_RGBA, 64, 32, 0, GL_RGBA, GL_UNSIGNED_BYTE, TEX_PIXELS);
    return 0;
}

uint64_t qg_tex_sub_image_2d(PFNGLTEXSUBIMAGE2DPROC f) {
    f(GL_TEXTURE_2D, 1, 3, 5, 16, 8, GL_RGB, GL_UNSIGNED_SHORT_5_6_5, SUB_PIXELS);
    return 0;
}

// 8 integer-class arguments: an exact fit of x0..x7
uint64_t qg_compressed_tex_image_2d(PFNGLCOMPRESSEDTEXIMAGE2DPROC f) {
    f(GL_TEXTURE_2D, 0, GL_ETC1_RGB8_OES, 128, 64, 0, 4096, ETC_DATA);
    return 0;
}

// the one fp-returning proxy: the float comes back in s0; its bits are the answer
uint64_t qg_path_length(PFNGLGETPATHLENGTHNVPROC f) {
    GLfloat r = f(11, 2, 3);
    uint32_t bits;
    memcpy(&bits, &r, sizeof bits);
    return bits;
}

// GS2.2 G1, eglGetProcAddress: `gpa` is the QEMU binding of eglGetProcAddress. Each entry asks it for a name and returns the answer; with `call` set it also calls the answer (tests/qemu_gl.rs `egl_row`: the answer is compared first, so a host address is refused before any guest call).
typedef void *(*qg_gpa_fn)(const char *name);

uint64_t qg_egl_vertex_attrib4f(qg_gpa_fn gpa, uint64_t call) {
    PFNGLVERTEXATTRIB4FPROC f = (PFNGLVERTEXATTRIB4FPROC)gpa("glVertexAttrib4f");
    if (call && f) f(5, 2.0f, -3.0f, 4.5f, -5.25f);
    return (uint64_t)(uintptr_t)f;
}

// 9 integer-class arguments through the answer, as qg_tex_image_2d
uint64_t qg_egl_tex_image_2d(qg_gpa_fn gpa, uint64_t call) {
    PFNGLTEXIMAGE2DPROC f = (PFNGLTEXIMAGE2DPROC)gpa("glTexImage2D");
    if (call && f) f(GL_TEXTURE_2D, 2, GL_RGBA, 64, 32, 0, GL_RGBA, GL_UNSIGNED_BYTE, TEX_PIXELS);
    return (uint64_t)(uintptr_t)f;
}

// a row the fixture never imports: its only argument is the mark the recorder checks
uint64_t qg_egl_finish(qg_gpa_fn gpa, uint64_t call) {
    PFNGLFINISHPROC f = (PFNGLFINISHPROC)gpa("glFinish");
    if (call && f) f();
    return (uint64_t)(uintptr_t)f;
}

// a name no row and no host answer has: NULL
uint64_t qg_egl_unknown(qg_gpa_fn gpa) {
    return (uint64_t)(uintptr_t)gpa("glNoSuchFunctionAtl");
}

// GS2.2 G1 fallback: a name with no row and a host answer. Both answers of glGetPathLengthNV are one address; the answer's fp result comes back bit for bit (`qg_path_length`).
uint64_t qg_egl_dedup(qg_gpa_fn gpa) {
    void *a = gpa("glGetPathLengthNV");
    void *b = gpa("glGetPathLengthNV");
    return a != 0 && a == b;
}

uint64_t qg_egl_path(qg_gpa_fn gpa) {
    void *f = gpa("glGetPathLengthNV");
    if (!f) return 0;
    return qg_path_length((PFNGLGETPATHLENGTHNVPROC)f);
}

// GS2.2 G1 named trap, its own run: a name no prototype describes; its answer aborts when called
uint64_t qg_egl_trap(qg_gpa_fn gpa) {
    void (*f)(void) = (void (*)(void))gpa("glAtlNoDescriptor");
    if (f) f();
    return 0;
}

// The entries BOOT carries (x0 = this table), in tests/qemu_gl.rs `calls` order (0 to 8), then the eglGetProcAddress entries (9 to 15, `egl_checks`, `egl_fallback_checks`, `egl_trap_run`).
static void *const qg_table[16] = {
    (void *)qg_clear_color,  (void *)qg_uniform4f,        (void *)qg_depth_rangef,
    (void *)qg_tex_parameterf, (void *)qg_vertex_attrib4f, (void *)qg_tex_image_2d,
    (void *)qg_tex_sub_image_2d, (void *)qg_compressed_tex_image_2d, (void *)qg_path_length,
    (void *)qg_egl_vertex_attrib4f, (void *)qg_egl_tex_image_2d, (void *)qg_egl_finish,
    (void *)qg_egl_unknown, (void *)qg_egl_dedup, (void *)qg_egl_path,
    (void *)qg_egl_trap,
};

// BOOT: x0 = the table, x1 = the return stub. The host's answer is this process's exit status; the boot thread never resumes after BOOT.
int main(void) {
    register uint64_t x0 __asm__("x0") = (uint64_t)(uintptr_t)qg_table;
    register uint64_t x1 __asm__("x1") = (uint64_t)qg_ret_stub;
    __asm__ volatile("svc #0x7a13" : "+r"(x0) : "r"(x1) : "memory");
    return (int)x0;
}
