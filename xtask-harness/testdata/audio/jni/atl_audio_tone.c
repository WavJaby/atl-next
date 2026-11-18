// SPDX-License-Identifier: MIT
// The audio gate's native tones (xtask-harness/src/audio_app.rs SLES_*, AAUDIO_*): a sine played through OpenSL ES (wilhelm, the simple buffer queue Alto's and other NDK games use) and through AAudio (data callback, FMOD's route), each blocking until its frames were handed out; then a silent AAudio stream whose buffer an adaptive tuner grows on xruns. The gate checks the recording of the sink and the counters, never these return strings alone.
// ATL_TONE_SILENT (audio_app.rs `--break-tone`): both tones are zero amplitude, so their runs must be absent from the recording.
#include <dlfcn.h>
#include <jni.h>
#include <math.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

#include <SLES/OpenSLES.h>
#include <SLES/OpenSLES_Android.h>
#include <aaudio/AAudio.h>

#define CHANNELS 2
// one buffer of the OpenSL queue, in frames (audio_app.rs SLES_BUFFER_FRAMES)
#define SLES_BUFFER_FRAMES 1024
#define SLES_BUFFERS 2
// how long the caller waits for the last callback / the last frame, past the tone's own length
#define DRAIN_MS 2000
// after the last frame was handed out, how long the caller lets the track's buffer (a few thousand frames, under 0.1 s at 48 kHz) play before it destroys the player: a stopped track drains, but a destroyed one is not mixed again (AOSP destroyTrack_l)
#define PLAYOUT_MS 500

#ifdef ATL_TONE_SILENT
#define AMPLITUDE_SCALE 0.0
#else
#define AMPLITUDE_SCALE 1.0
#endif

// a tone being handed out: `frames` frames of a `hz` sine at `rate`, `amp` peak (16-bit units); `next` is the next frame index
struct tone {
    double hz, rate, amp;
    int32_t frames;
    volatile int32_t next;
};

static void tone_init(struct tone *t, jint hz, jint rate, jint frames, jdouble dbfs)
{
    t->hz = hz;
    t->rate = rate;
    t->amp = pow(10.0, dbfs / 20.0) * 32767.0 * AMPLITUDE_SCALE;
    t->frames = frames;
    t->next = 0;
}

// fills `n` stereo frames from the tone (silence past its end); returns the tone frames written
static int32_t tone_fill(struct tone *t, int16_t *out, int32_t n)
{
    int32_t k = 0;
    for (; k < n && t->next < t->frames; k++, t->next++) {
        int16_t v = (int16_t)lround(t->amp * sin(2.0 * M_PI * t->hz * t->next / t->rate));
        out[CHANNELS * k] = v;
        out[CHANNELS * k + 1] = v;
    }
    memset(out + CHANNELS * k, 0, (size_t)(n - k) * CHANNELS * sizeof(int16_t));
    return k;
}

static int64_t now_ms(void)
{
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (int64_t)ts.tv_sec * 1000 + ts.tv_nsec / 1000000;
}

// ---- OpenSL ES: engine, output mix, a player on a 2-buffer Android simple buffer queue; the callback refills the buffer that finished until the tone is out
static struct {
    struct tone tone;
    int16_t buf[SLES_BUFFERS][SLES_BUFFER_FRAMES * CHANNELS];
    volatile int enqueued, callbacks, failed, which;
} g_sles;

static void sles_enqueue(SLAndroidSimpleBufferQueueItf q)
{
    int16_t *b = g_sles.buf[g_sles.which];
    g_sles.which = (g_sles.which + 1) % SLES_BUFFERS;
    int32_t n = SLES_BUFFER_FRAMES;
    if (g_sles.tone.frames - g_sles.tone.next < n)
        n = g_sles.tone.frames - g_sles.tone.next;
    tone_fill(&g_sles.tone, b, n);
    if ((*q)->Enqueue(q, b, (SLuint32)n * CHANNELS * sizeof(int16_t)) == SL_RESULT_SUCCESS)
        g_sles.enqueued++;
    else
        g_sles.failed++;
}

static void sles_cb(SLAndroidSimpleBufferQueueItf q, void *ctx)
{
    g_sles.callbacks++;
    if (g_sles.tone.next < g_sles.tone.frames)
        sles_enqueue(q);
}

#define SLES_CHECK(step, expr)                                                    \
    do {                                                                          \
        SLresult r_ = (expr);                                                     \
        if (r_ != SL_RESULT_SUCCESS) {                                            \
            snprintf(msg, sizeof msg, "sles %s failed %u", step, (unsigned)r_); \
            goto out;                                                             \
        }                                                                         \
    } while (0)

// "sles enqueued=<n> callbacks=<n> failed=<n>": the buffers handed to Enqueue and the completion callbacks seen; the caller waits until every enqueued buffer called back (DRAIN_MS past the tone)
JNIEXPORT jstring JNICALL Java_com_example_atlaudio_Main_slesTone(JNIEnv *env, jclass cls, jint hz, jint rate, jint frames, jdouble dbfs)
{
    char msg[160];
    SLObjectItf engine_obj = NULL, mix = NULL, player = NULL;
    SLEngineItf engine;
    SLPlayItf play;
    SLAndroidSimpleBufferQueueItf queue;
    memset(&g_sles, 0, sizeof g_sles);
    tone_init(&g_sles.tone, hz, rate, frames, dbfs);
    SLES_CHECK("slCreateEngine", slCreateEngine(&engine_obj, 0, NULL, 0, NULL, NULL));
    SLES_CHECK("engine Realize", (*engine_obj)->Realize(engine_obj, SL_BOOLEAN_FALSE));
    SLES_CHECK("engine GetInterface", (*engine_obj)->GetInterface(engine_obj, SL_IID_ENGINE, &engine));
    SLES_CHECK("CreateOutputMix", (*engine)->CreateOutputMix(engine, &mix, 0, NULL, NULL));
    SLES_CHECK("mix Realize", (*mix)->Realize(mix, SL_BOOLEAN_FALSE));
    SLDataLocator_AndroidSimpleBufferQueue loc_q = {SL_DATALOCATOR_ANDROIDSIMPLEBUFFERQUEUE, SLES_BUFFERS};
    SLDataFormat_PCM pcm = {SL_DATAFORMAT_PCM, CHANNELS, (SLuint32)rate * 1000, SL_PCMSAMPLEFORMAT_FIXED_16, SL_PCMSAMPLEFORMAT_FIXED_16,
                            SL_SPEAKER_FRONT_LEFT | SL_SPEAKER_FRONT_RIGHT, SL_BYTEORDER_LITTLEENDIAN};
    SLDataSource src = {&loc_q, &pcm};
    SLDataLocator_OutputMix loc_mix = {SL_DATALOCATOR_OUTPUTMIX, mix};
    SLDataSink sink = {&loc_mix, NULL};
    const SLInterfaceID ids[1] = {SL_IID_ANDROIDSIMPLEBUFFERQUEUE};
    const SLboolean req[1] = {SL_BOOLEAN_TRUE};
    SLES_CHECK("CreateAudioPlayer", (*engine)->CreateAudioPlayer(engine, &player, &src, &sink, 1, ids, req));
    SLES_CHECK("player Realize", (*player)->Realize(player, SL_BOOLEAN_FALSE));
    SLES_CHECK("GetInterface PLAY", (*player)->GetInterface(player, SL_IID_PLAY, &play));
    SLES_CHECK("GetInterface BUFFERQUEUE", (*player)->GetInterface(player, SL_IID_ANDROIDSIMPLEBUFFERQUEUE, &queue));
    SLES_CHECK("RegisterCallback", (*queue)->RegisterCallback(queue, sles_cb, NULL));
    for (int i = 0; i < SLES_BUFFERS && g_sles.tone.next < g_sles.tone.frames; i++)
        sles_enqueue(queue);
    SLES_CHECK("SetPlayState", (*play)->SetPlayState(play, SL_PLAYSTATE_PLAYING));
    int64_t limit = now_ms() + (int64_t)frames * 1000 / rate + DRAIN_MS;
    while ((g_sles.tone.next < g_sles.tone.frames || g_sles.callbacks < g_sles.enqueued) && now_ms() < limit)
        usleep(10000);
    // a buffer's callback means the track took it, not that it played; wilhelm stops the track itself once its queue runs dry, and the stopped track drains: let it play out before Destroy
    usleep(PLAYOUT_MS * 1000);
    (*play)->SetPlayState(play, SL_PLAYSTATE_STOPPED);
    snprintf(msg, sizeof msg, "sles enqueued=%d callbacks=%d failed=%d", g_sles.enqueued, g_sles.callbacks, g_sles.failed);
out:
    if (player)
        (*player)->Destroy(player);
    if (mix)
        (*mix)->Destroy(mix);
    if (engine_obj)
        (*engine_obj)->Destroy(engine_obj);
    return (*env)->NewStringUTF(env, msg);
}

// ---- AAudio: dlopen'd by name as FMOD does (the fixture's minSdk is below AAudio's 26), a 16-bit stereo output stream with a data callback that hands out the tone, then stops
static struct {
    struct tone tone;
    volatile int done;
} g_aaudio;

// the common app pattern: once the tone is out, the next callback returns AAUDIO_CALLBACK_RESULT_STOP and the stream stops itself; the stopped track plays out what was written. Not in the callback that writes the tail: AOSP discards the frames of a callback that returns STOP (AudioStreamLegacy processCallbackCommon)
static aaudio_data_callback_result_t aaudio_cb(AAudioStream *stream, void *user, void *audio, int32_t frames)
{
    if (g_aaudio.tone.next >= g_aaudio.tone.frames) {
        g_aaudio.done = 1;
        return AAUDIO_CALLBACK_RESULT_STOP;
    }
    tone_fill(&g_aaudio.tone, audio, frames);
    return AAUDIO_CALLBACK_RESULT_CONTINUE;
}

// "aaudio open=<rc> start=<rc> written=<tone frames handed out> stop=<rc> close=<rc>"; "aaudio dlopen null" without libaaudio
JNIEXPORT jstring JNICALL Java_com_example_atlaudio_Main_aaudioTone(JNIEnv *env, jclass cls, jint hz, jint rate, jint frames, jdouble dbfs)
{
    char msg[160];
    void *lib = dlopen("libaaudio.so", RTLD_NOW);
    if (!lib)
        return (*env)->NewStringUTF(env, "aaudio dlopen null");
    // AAudio.h's prototypes, typed by hand: NDK headers mark them unavailable below API 26 even inside __typeof__
    aaudio_result_t (*create)(AAudioStreamBuilder **) = dlsym(lib, "AAudio_createStreamBuilder");
    void (*set_rate)(AAudioStreamBuilder *, int32_t) = dlsym(lib, "AAudioStreamBuilder_setSampleRate");
    void (*set_channels)(AAudioStreamBuilder *, int32_t) = dlsym(lib, "AAudioStreamBuilder_setChannelCount");
    void (*set_format)(AAudioStreamBuilder *, aaudio_format_t) = dlsym(lib, "AAudioStreamBuilder_setFormat");
    void (*set_cb)(AAudioStreamBuilder *, AAudioStream_dataCallback, void *) = dlsym(lib, "AAudioStreamBuilder_setDataCallback");
    aaudio_result_t (*open)(AAudioStreamBuilder *, AAudioStream **) = dlsym(lib, "AAudioStreamBuilder_openStream");
    aaudio_result_t (*del)(AAudioStreamBuilder *) = dlsym(lib, "AAudioStreamBuilder_delete");
    aaudio_result_t (*start)(AAudioStream *) = dlsym(lib, "AAudioStream_requestStart");
    aaudio_result_t (*stop)(AAudioStream *) = dlsym(lib, "AAudioStream_requestStop");
    aaudio_result_t (*close)(AAudioStream *) = dlsym(lib, "AAudioStream_close");
    if (!create || !set_rate || !set_channels || !set_format || !set_cb || !open || !del || !start || !stop || !close) {
        dlclose(lib);
        return (*env)->NewStringUTF(env, "aaudio dlsym null");
    }
    memset(&g_aaudio, 0, sizeof g_aaudio);
    tone_init(&g_aaudio.tone, hz, rate, frames, dbfs);
    AAudioStreamBuilder *b = NULL;
    AAudioStream *s = NULL;
    aaudio_result_t rc_open = create(&b), rc_start = -1, rc_stop = -1, rc_close = -1;
    if (rc_open == AAUDIO_OK) {
        set_rate(b, rate);
        set_channels(b, CHANNELS);
        set_format(b, AAUDIO_FORMAT_PCM_I16);
        set_cb(b, aaudio_cb, NULL);
        rc_open = open(b, &s);
        del(b);
    }
    if (rc_open == AAUDIO_OK) {
        rc_start = start(s);
        int64_t limit = now_ms() + (int64_t)frames * 1000 / rate + DRAIN_MS;
        while (rc_start == AAUDIO_OK && !g_aaudio.done && now_ms() < limit)
            usleep(10000);
        // the stream stopped itself; its track drains: let it play out before close destroys it
        usleep(PLAYOUT_MS * 1000);
        rc_stop = stop(s);
        rc_close = close(s);
    }
    snprintf(msg, sizeof msg, "aaudio open=%d start=%d written=%d stop=%d close=%d", rc_open, rc_start, (int)g_aaudio.tone.next, rc_stop, rc_close);
    dlclose(lib);
    return (*env)->NewStringUTF(env, msg);
}

// ---- AAudio adaptive client (audio_app.rs ADAPTIVE_*): Alto's Odyssey's Wwise shape (48 kHz stereo, LOW_LATENCY, data callback; the server refuses FAST, so a normal track) with the buffer tuned as the NDK AAudio guide's "Tuning buffers" sample does
// (developer.android.com/ndk/guides/audio/aaudio/aaudio; Oboe's LatencyTuner::tune is the same rule): start small (here one burst, Wwise's start), and on every rise of AAudioStream_getXRunCount grow the buffer by one burst while below capacity.
// The callback writes silence: the recording's tone runs stay the five above. Each mixer cycle that comes up short starts an underrun (the server tallies it once the track delivered anything that cycle), so `stable` ms without a rise = that many ms of full cycles.
static volatile int64_t g_adaptive_frames;

static aaudio_data_callback_result_t adaptive_cb(AAudioStream *stream, void *user, void *audio, int32_t frames)
{
    memset(audio, 0, (size_t)frames * CHANNELS * sizeof(int16_t));
    g_adaptive_frames += frames;
    return AAUDIO_CALLBACK_RESULT_CONTINUE;
}

// "aaudio-adaptive open=<rc> start=<rc> burst=<frames> capacity=<frames> first=<size> final=<size> xruns=<n> stable=<ms> frames=<n> stop=<rc> close=<rc>": first = the size the one-burst request got, final = the size when the
// tuner ended, xruns = the stream's xrun count then, stable = ms since the last rise when it ended (it ends after `stable_ms` without one, or at `max_ms`)
JNIEXPORT jstring JNICALL Java_com_example_atlaudio_Main_aaudioAdaptive(JNIEnv *env, jclass cls, jint rate, jint max_ms, jint stable_ms)
{
    char msg[256];
    void *lib = dlopen("libaaudio.so", RTLD_NOW);
    if (!lib)
        return (*env)->NewStringUTF(env, "aaudio-adaptive dlopen null");
    aaudio_result_t (*create)(AAudioStreamBuilder **) = dlsym(lib, "AAudio_createStreamBuilder");
    void (*set_rate)(AAudioStreamBuilder *, int32_t) = dlsym(lib, "AAudioStreamBuilder_setSampleRate");
    void (*set_channels)(AAudioStreamBuilder *, int32_t) = dlsym(lib, "AAudioStreamBuilder_setChannelCount");
    void (*set_format)(AAudioStreamBuilder *, aaudio_format_t) = dlsym(lib, "AAudioStreamBuilder_setFormat");
    void (*set_perf)(AAudioStreamBuilder *, aaudio_performance_mode_t) = dlsym(lib, "AAudioStreamBuilder_setPerformanceMode");
    void (*set_cb)(AAudioStreamBuilder *, AAudioStream_dataCallback, void *) = dlsym(lib, "AAudioStreamBuilder_setDataCallback");
    aaudio_result_t (*open)(AAudioStreamBuilder *, AAudioStream **) = dlsym(lib, "AAudioStreamBuilder_openStream");
    aaudio_result_t (*del)(AAudioStreamBuilder *) = dlsym(lib, "AAudioStreamBuilder_delete");
    int32_t (*burst_of)(AAudioStream *) = dlsym(lib, "AAudioStream_getFramesPerBurst");
    int32_t (*capacity_of)(AAudioStream *) = dlsym(lib, "AAudioStream_getBufferCapacityInFrames");
    aaudio_result_t (*set_size)(AAudioStream *, int32_t) = dlsym(lib, "AAudioStream_setBufferSizeInFrames");
    int32_t (*xruns_of)(AAudioStream *) = dlsym(lib, "AAudioStream_getXRunCount");
    aaudio_result_t (*start)(AAudioStream *) = dlsym(lib, "AAudioStream_requestStart");
    aaudio_result_t (*stop)(AAudioStream *) = dlsym(lib, "AAudioStream_requestStop");
    aaudio_result_t (*close)(AAudioStream *) = dlsym(lib, "AAudioStream_close");
    if (!create || !set_rate || !set_channels || !set_format || !set_perf || !set_cb || !open || !del || !burst_of || !capacity_of || !set_size || !xruns_of || !start || !stop ||
        !close) {
        dlclose(lib);
        return (*env)->NewStringUTF(env, "aaudio-adaptive dlsym null");
    }
    g_adaptive_frames = 0;
    AAudioStreamBuilder *b = NULL;
    AAudioStream *s = NULL;
    aaudio_result_t rc_open = create(&b), rc_start = -1, rc_stop = -1, rc_close = -1;
    int32_t burst = 0, capacity = 0, first = 0, size = 0, xruns = 0;
    int64_t stable = 0;
    if (rc_open == AAUDIO_OK) {
        set_rate(b, rate);
        set_channels(b, CHANNELS);
        set_format(b, AAUDIO_FORMAT_PCM_I16);
        set_perf(b, AAUDIO_PERFORMANCE_MODE_LOW_LATENCY);
        set_cb(b, adaptive_cb, NULL);
        rc_open = open(b, &s);
        del(b);
    }
    if (rc_open == AAUDIO_OK) {
        burst = burst_of(s);
        capacity = capacity_of(s);
        first = size = set_size(s, burst);
        rc_start = start(s);
        int64_t begin = now_ms(), last_rise = begin, now = begin;
        while (rc_start == AAUDIO_OK && now - last_rise < stable_ms && now - begin < max_ms) {
            usleep(10000);
            now = now_ms();
            int32_t x = xruns_of(s);
            if (x > xruns) {
                xruns = x;
                last_rise = now;
                if (size < capacity)
                    size = set_size(s, size + burst);
            }
        }
        stable = now - last_rise;
        // a stopped track drains what was written: let it play out before close destroys it (the gate counts dropped frames)
        rc_stop = stop(s);
        usleep(PLAYOUT_MS * 1000);
        rc_close = close(s);
    }
    snprintf(msg, sizeof msg, "aaudio-adaptive open=%d start=%d burst=%d capacity=%d first=%d final=%d xruns=%d stable=%lld frames=%lld stop=%d close=%d", rc_open, rc_start, burst, capacity, first, size, xruns,
             (long long)stable, (long long)g_adaptive_frames, rc_stop, rc_close);
    dlclose(lib);
    return (*env)->NewStringUTF(env, msg);
}
