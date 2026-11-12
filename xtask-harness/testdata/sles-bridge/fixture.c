// SPDX-License-Identifier: MIT
// Gate sles-bridge fixture (opensl-es plan O1): an NDK x86_64 library linked against libOpenSLES.so, driven step by step by crates/atl-native-bridge/tests/sles_bridge.rs through the identity engine.
// Each step returns an SLresult (0 = success) or a step-local error code; the test reads the exported globals for the values its checks compare.
#include <SLES/OpenSLES.h>
#include <SLES/OpenSLES_Android.h>
#include <stddef.h>

// what the checks expect: callbacks the recorder fires in step d, players step e creates
const int sles_callbacks = 8;
const int sles_players = 50;

SLObjectItf sles_engine_obj;
SLEngineItf sles_engine;
SLObjectItf sles_mix;
SLObjectItf sles_player;
SLPlayItf sles_play;
SLAndroidSimpleBufferQueueItf sles_queue;

// step d: what the callback saw
volatile int sles_cb_count;
volatile int sles_cb_bad_caller;
volatile int sles_cb_bad_ctx;
volatile int sles_cb_bad_enqueue;
// the context registered with the callback: its address must come back unchanged
int sles_cb_context = 0x51e5;

static short buffer[64];

static SLresult realize(SLObjectItf o) { return (*o)->Realize(o, SL_BOOLEAN_FALSE); }

// (a) slCreateEngine + Realize
SLresult sles_a_engine(void) {
    SLresult r = slCreateEngine(&sles_engine_obj, 0, NULL, 0, NULL, NULL);
    if (r != SL_RESULT_SUCCESS) return r;
    return realize(sles_engine_obj);
}

// (b) the Engine interface
SLresult sles_b_interface(void) {
    return (*sles_engine_obj)->GetInterface(sles_engine_obj, SL_IID_ENGINE, &sles_engine);
}

static SLresult create_player(SLObjectItf *out) {
    SLDataLocator_AndroidSimpleBufferQueue loc_bq = {SL_DATALOCATOR_ANDROIDSIMPLEBUFFERQUEUE, 2};
    SLDataFormat_PCM pcm = {SL_DATAFORMAT_PCM, 2, SL_SAMPLINGRATE_48, SL_PCMSAMPLEFORMAT_FIXED_16,
                            SL_PCMSAMPLEFORMAT_FIXED_16, SL_SPEAKER_FRONT_LEFT | SL_SPEAKER_FRONT_RIGHT,
                            SL_BYTEORDER_LITTLEENDIAN};
    SLDataSource src = {&loc_bq, &pcm};
    SLDataLocator_OutputMix loc_mix = {SL_DATALOCATOR_OUTPUTMIX, sles_mix};
    SLDataSink snk = {&loc_mix, NULL};
    const SLInterfaceID ids[2] = {SL_IID_ANDROIDSIMPLEBUFFERQUEUE, SL_IID_PLAY};
    const SLboolean req[2] = {SL_BOOLEAN_TRUE, SL_BOOLEAN_TRUE};
    SLresult r = (*sles_engine)->CreateAudioPlayer(sles_engine, out, &src, &snk, 2, ids, req);
    if (r != SL_RESULT_SUCCESS) return r;
    return realize(*out);
}

// (c) output mix + a buffer-queue player whose sink is that mix, with its Play and queue interfaces
SLresult sles_c_player(void) {
    SLresult r = (*sles_engine)->CreateOutputMix(sles_engine, &sles_mix, 0, NULL, NULL);
    if (r == SL_RESULT_SUCCESS) r = realize(sles_mix);
    if (r == SL_RESULT_SUCCESS) r = create_player(&sles_player);
    if (r == SL_RESULT_SUCCESS) r = (*sles_player)->GetInterface(sles_player, SL_IID_PLAY, &sles_play);
    if (r == SL_RESULT_SUCCESS)
        r = (*sles_player)->GetInterface(sles_player, SL_IID_ANDROIDSIMPLEBUFFERQUEUE, &sles_queue);
    return r;
}

// runs on the recorder's host thread: checks its arguments, re-enters Enqueue every time
static void on_buffer(SLAndroidSimpleBufferQueueItf caller, void *context) {
    sles_cb_count++;
    if (caller != sles_queue) sles_cb_bad_caller++;
    if (context != &sles_cb_context) sles_cb_bad_ctx++;
    if ((*caller)->Enqueue(caller, buffer, sizeof buffer) != SL_RESULT_SUCCESS) sles_cb_bad_enqueue++;
}

// (d) callback registered, one buffer primed, playback started (the recorder thread then fires sles_callbacks callbacks)
SLresult sles_d_start(void) {
    SLresult r = (*sles_queue)->RegisterCallback(sles_queue, on_buffer, &sles_cb_context);
    if (r == SL_RESULT_SUCCESS) r = (*sles_queue)->Enqueue(sles_queue, buffer, sizeof buffer);
    if (r == SL_RESULT_SUCCESS) r = (*sles_play)->SetPlayState(sles_play, SL_PLAYSTATE_PLAYING);
    return r;
}

static void on_event(SLPlayItf caller, void *context, SLuint32 event) {
    (void)caller;
    (void)context;
    (void)event;
}

// (e) sles_players players, each registering the same Play callback, then all destroyed
SLresult sles_e_players(void) {
    SLObjectItf players[50];
    SLresult r = SL_RESULT_SUCCESS;
    int made = 0;
    for (; made < sles_players && made < 50; made++) {
        SLPlayItf play;
        r = create_player(&players[made]);
        if (r != SL_RESULT_SUCCESS) break;
        r = (*players[made])->GetInterface(players[made], SL_IID_PLAY, &play);
        if (r == SL_RESULT_SUCCESS) r = (*play)->RegisterCallback(play, on_event, NULL);
        if (r != SL_RESULT_SUCCESS) {
            made++;
            break;
        }
    }
    for (int i = 0; i < made; i++) (*players[i])->Destroy(players[i]);
    return r;
}

// (f) the guest's SL_IID_PLAY value
const void *sles_f_iid_play(void) { return SL_IID_PLAY; }

// (g) an IID no SL header defines
static const struct SLInterfaceID_ unknown_iid = {0x0badc0de, 0x0001, 0x0002, 0x0003, {1, 2, 3, 4, 5, 6}};
SLresult sles_g_unknown(void) {
    void *itf = NULL;
    return (*sles_engine_obj)->GetInterface(sles_engine_obj, &unknown_iid, &itf);
}
