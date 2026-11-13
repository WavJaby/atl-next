// SPDX-License-Identifier: MIT
/*
 * GS2.2 G1 identity gate fixture (`xtask test egl-bridge`, tests/egl_bridge.rs): NDK x86_64, DT_NEEDED libEGL.so and libGLESv2.so (the guest proxies).
 * Each entry asks eglGetProcAddress for a name and calls the answer; the test reads the globals and the recorder's last call.
 * qe_a / qe_f import their GL function, so the guest's import address is taken beside the answer.
 * Prototypes are declared here (the recorder has the Khronos ones; GL_APIENTRY is empty on x86_64).
 */
#include <stdint.h>

typedef void (*qe_gl_fn)(void);
void *eglGetProcAddress(const char *name);
void glVertexAttrib4f(unsigned index, float x, float y, float z, float w);
void glTexImage2D(unsigned target, int level, int internalformat, int width, int height, int border, unsigned format, unsigned type, const void *pixels);

uintptr_t qe_a_import, qe_a_answer;
uintptr_t qe_f_import, qe_f_answer;
uintptr_t qe_b_answer;
uintptr_t qe_e_first, qe_e_second;
uint32_t qe_e_result;
unsigned char qe_pixels[64 * 32 * 4];

/* (a): glVertexAttrib4f's import and its answer; with `call` set the answer is called with fp arguments (the test compares the two addresses between the calls, so a bad answer is refused before any call to it) */
void qe_a(int call) {
    typedef void (*fn)(unsigned, float, float, float, float);
    qe_a_import = (uintptr_t)&glVertexAttrib4f;
    fn f = (fn)eglGetProcAddress("glVertexAttrib4f");
    qe_a_answer = (uintptr_t)f;
    if (call && f) f(5, 2.0f, -3.0f, 4.5f, -5.25f);
}

/* (f): glTexImage2D's import and its answer; with `call` set the answer is called with nine integer-class arguments, the ninth on the stack */
void qe_f(int call) {
    typedef void (*fn)(unsigned, int, int, int, int, int, unsigned, unsigned, const void *);
    qe_f_import = (uintptr_t)&glTexImage2D;
    fn f = (fn)eglGetProcAddress("glTexImage2D");
    qe_f_answer = (uintptr_t)f;
    if (call && f) f(0x0DE1u, 2, 0x1908, 64, 32, 0, 0x1908u, 0x1401u, qe_pixels);
}

/* (b): glFinish, a row this fixture never imports: the answer is called */
void qe_b(int call) {
    qe_gl_fn f = (qe_gl_fn)eglGetProcAddress("glFinish");
    qe_b_answer = (uintptr_t)f;
    if (call && f) f();
}

/* (d): a name with no row and no host answer: NULL */
uintptr_t qe_d(void) {
    return (uintptr_t)eglGetProcAddress("glNoSuchFunctionAtl");
}

/* (e): two answers for a name the host describes and no row holds (eglQueryDevicesEXT: no NBS row in either engine): equal, and the answer's call returns the host's result */
void qe_e(void) {
    typedef unsigned (*fn)(int, void *, int *);
    void *a = eglGetProcAddress("eglQueryDevicesEXT");
    void *b = eglGetProcAddress("eglQueryDevicesEXT");
    qe_e_first = (uintptr_t)a;
    qe_e_second = (uintptr_t)b;
    if (a) qe_e_result = ((fn)a)(4, 0, 0);
}

/* (c), trap run only: a name no prototype describes; its answer is a named trap, so this call aborts the process */
void qe_c(void) {
    qe_gl_fn f = (qe_gl_fn)eglGetProcAddress("glAtlNoDescriptor");
    if (f) f();
}
