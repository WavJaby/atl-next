// SPDX-License-Identifier: MIT
// GLES2 fixture library: stands in for a game's native renderer (Cut the Rope's libctro: GL calls from libGLESv2 only, no EGL; the context comes from Java's GLSurfaceView).
// Imports only libGLESv2, liblog, libc (+ libdl, which NDK clang always links); xtask-harness/src/gles2_fixture.rs gates the NEEDED set and the gl* import list against this file.
//
// Scene, in a 64x64 viewport at the window origin (Main.java derives its expected pixels from this, not from running it):
// - state: blend, depth test, scissor, dither off -> written colour = fragment colour
// - clear colour (64, 128, 192, 255) / 255
// - triangle (-1,-1) (1,-1) (-1,1), solid uniform colour (1, 0, 0, 1)
// - quad [0,1]^2 (triangle strip), fragment = texture2D(tex, ndc position), 2x2 RGBA8 texture, GL_NEAREST, clamp to edge; rows bottom first:
//   row 0 (t = 0): green (0,255,0,255), blue (0,0,255,255); row 1: white (255,255,255,255), yellow (255,255,0,255)
//
// Worker (the window gate's balance sub-gate): nativeInit starts a pthread that attaches to the VM, makes one JNI call, detaches and exits, and joins it -> the bridge logs one thread exit before init returns, as a game's loader/audio thread does.
// Deliberate break (built with -DATL_GLES2_BREAK_NO_EXIT, window `--break-no-detach`): the worker parks forever after its call and is not joined -> no exit line -> balance must fail.

#include <GLES2/gl2.h>
#include <android/log.h>
#include <jni.h>
#include <pthread.h>
#include <unistd.h>

#define TAG "AtlGles2"
#define VIEWPORT 64

static const char VS[] = "attribute vec2 a_pos;\n"
                         "varying vec2 v_tc;\n"
                         "void main() { v_tc = a_pos; gl_Position = vec4(a_pos, 0.0, 1.0); }\n";
static const char FS_SOLID[] = "precision mediump float;\n"
                               "uniform vec4 u_color;\n"
                               "void main() { gl_FragColor = u_color; }\n";
static const char FS_TEX[] = "precision mediump float;\n"
                             "varying vec2 v_tc;\n"
                             "uniform sampler2D u_tex;\n"
                             "void main() { gl_FragColor = texture2D(u_tex, v_tc); }\n";

static const GLfloat TRIANGLE[] = {-1.f, -1.f, 1.f, -1.f, -1.f, 1.f};
static const GLfloat QUAD[] = {0.f, 0.f, 1.f, 0.f, 0.f, 1.f, 1.f, 1.f};
static const GLubyte TEXELS[] = {
    0, 255, 0, 255, 0, 0, 255, 255,         // row 0
    255, 255, 255, 255, 255, 255, 0, 255,   // row 1
};

static GLuint solid, textured, texture;
static GLint u_color, u_tex;

static GLuint shader(GLenum kind, const char *src) {
    GLuint s = glCreateShader(kind);
    glShaderSource(s, 1, &src, NULL);
    glCompileShader(s);
    GLint ok = 0;
    glGetShaderiv(s, GL_COMPILE_STATUS, &ok);
    if (!ok) {
        char log[512];
        glGetShaderInfoLog(s, sizeof log, NULL, log);
        __android_log_print(ANDROID_LOG_ERROR, TAG, "shader compile failed: %s", log);
        return 0;
    }
    return s;
}

static GLuint program(const char *fs) {
    GLuint v = shader(GL_VERTEX_SHADER, VS), f = shader(GL_FRAGMENT_SHADER, fs);
    if (!v || !f) {
        return 0;
    }
    GLuint p = glCreateProgram();
    glAttachShader(p, v);
    glAttachShader(p, f);
    glBindAttribLocation(p, 0, "a_pos");
    glLinkProgram(p);
    GLint ok = 0;
    glGetProgramiv(p, GL_LINK_STATUS, &ok);
    if (!ok) {
        __android_log_print(ANDROID_LOG_ERROR, TAG, "program link failed");
        return 0;
    }
    return p;
}

static JavaVM *vm;

JNIEXPORT jint JNI_OnLoad(JavaVM *v, void *reserved) {
    vm = v;
    return JNI_VERSION_1_6;
}

static void *worker(void *arg) {
    JNIEnv *env = NULL;
    if ((*vm)->AttachCurrentThread(vm, &env, NULL) != JNI_OK) {
        __android_log_print(ANDROID_LOG_ERROR, TAG, "worker: AttachCurrentThread failed");
        return NULL;
    }
    __android_log_print(ANDROID_LOG_INFO, TAG, "worker JNI version 0x%x", (*env)->GetVersion(env));
#ifdef ATL_GLES2_BREAK_NO_EXIT
    for (;;) {
        pause();
    }
#endif
    (*vm)->DetachCurrentThread(vm);
    return NULL;
}

/// Starts the worker and (unless broken) joins it: its exit, and so the bridge's detach line, comes before init returns.
static int run_worker(void) {
    pthread_t t;
    if (pthread_create(&t, NULL, worker, NULL) != 0) {
        __android_log_print(ANDROID_LOG_ERROR, TAG, "worker: pthread_create failed");
        return 0;
    }
#ifndef ATL_GLES2_BREAK_NO_EXIT
    pthread_join(t, NULL);
#endif
    return 1;
}

JNIEXPORT jboolean JNICALL Java_com_example_atlgles2_Main_nativeInit(JNIEnv *env, jclass cls) {
    if (!run_worker()) {
        return JNI_FALSE;
    }
    solid = program(FS_SOLID);
    textured = program(FS_TEX);
    if (!solid || !textured) {
        return JNI_FALSE;
    }
    u_color = glGetUniformLocation(solid, "u_color");
    u_tex = glGetUniformLocation(textured, "u_tex");
    glGenTextures(1, &texture);
    glBindTexture(GL_TEXTURE_2D, texture);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_S, GL_CLAMP_TO_EDGE);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_T, GL_CLAMP_TO_EDGE);
    // rows are 8 bytes: the default GL_UNPACK_ALIGNMENT of 4 needs no padding
    glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA, 2, 2, 0, GL_RGBA, GL_UNSIGNED_BYTE, TEXELS);
    GLenum err = glGetError();
    if (err != GL_NO_ERROR) {
        __android_log_print(ANDROID_LOG_ERROR, TAG, "init: GL error 0x%x", err);
        return JNI_FALSE;
    }
    __android_log_print(ANDROID_LOG_INFO, TAG, "init done");
    return JNI_TRUE;
}

JNIEXPORT void JNICALL Java_com_example_atlgles2_Main_nativeDraw(JNIEnv *env, jclass cls) {
    glDisable(GL_BLEND);
    glDisable(GL_DEPTH_TEST);
    glDisable(GL_SCISSOR_TEST);
    glDisable(GL_DITHER);
    glViewport(0, 0, VIEWPORT, VIEWPORT);
    glClearColor(64.f / 255.f, 128.f / 255.f, 192.f / 255.f, 1.f);
    glClear(GL_COLOR_BUFFER_BIT);
    glEnableVertexAttribArray(0);

    glUseProgram(solid);
    glUniform4f(u_color, 1.f, 0.f, 0.f, 1.f);
    glVertexAttribPointer(0, 2, GL_FLOAT, GL_FALSE, 0, TRIANGLE);
    glDrawArrays(GL_TRIANGLES, 0, 3);

    glUseProgram(textured);
    glBindTexture(GL_TEXTURE_2D, texture);
    glUniform1i(u_tex, 0);
    glVertexAttribPointer(0, 2, GL_FLOAT, GL_FALSE, 0, QUAD);
    glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);

    GLenum err = glGetError();
    if (err != GL_NO_ERROR) {
        __android_log_print(ANDROID_LOG_ERROR, TAG, "draw: GL error 0x%x", err);
    }
}

JNIEXPORT jint JNICALL Java_com_example_atlgles2_Main_nativeReadPixel(JNIEnv *env, jclass cls, jint x, jint y) {
    GLubyte p[4] = {0, 0, 0, 0};
    glReadPixels(x, y, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, p);
    return (jint)(((unsigned)p[0] << 24) | ((unsigned)p[1] << 16) | ((unsigned)p[2] << 8) | p[3]);
}
