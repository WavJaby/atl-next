// SPDX-License-Identifier: MIT
package com.example.atlaudio;

import android.app.Activity;
import android.media.AudioAttributes;
import android.media.AudioFormat;
import android.media.AudioManager;
import android.media.AudioTrack;
import android.os.Bundle;
import android.util.Log;

/**
 * The audio gate (`xtask test audio-app`, xtask/src/audio_app.rs). Tag AtlAudio, one line each, checked by the gate:
 * `session: <a> <b>`: AudioManager.generateAudioSessionId twice (AudioSystem.newAudioSessionId, AudioFlinger's ids).
 * `minbuf: <bytes>`: AudioTrack.getMinBufferSize for 44100 Hz, stereo, 16-bit (AudioFlinger's sample rate, frame count and latency through AudioPolicyService's output).
 * `track: state=<n> piid=<n> frames=<n>`: a track's constructor (createTrack, IAudioTrack, then PlayerBase's trackPlayer): its state, its player id, its frame count.
 * `fast: state=<n>`: a low-latency track; the server refuses FAST and the track must still initialise on the normal path.
 * `static: state=<n> frames=<n>`: a shared-buffer track (MODE_STATIC): the frames are the requested ones, not rounded.
 * `done`: the end of the run.
 */
public class Main extends Activity {
    private static final String TAG = "AtlAudio";
    /** The rate the gate asks for; audio_app.rs works the expected minimum buffer for it. */
    private static final int RATE_HZ = 44100;
    /** The static track's buffer: 4096 bytes, 1024 stereo 16-bit frames (audio_app.rs STATIC_BYTES). */
    private static final int STATIC_BYTES = 4096;

    @Override
    protected void onCreate(Bundle state) {
        super.onCreate(state);
        AudioManager audio = (AudioManager) getSystemService(AUDIO_SERVICE);
        int a = audio.generateAudioSessionId();
        int b = audio.generateAudioSessionId();
        Log.i(TAG, "session: " + a + " " + b);
        int min = AudioTrack.getMinBufferSize(
                RATE_HZ, AudioFormat.CHANNEL_OUT_STEREO, AudioFormat.ENCODING_PCM_16BIT);
        Log.i(TAG, "minbuf: " + min);
        AudioAttributes attributes = new AudioAttributes.Builder()
                .setUsage(AudioAttributes.USAGE_GAME).build();
        AudioFormat format = new AudioFormat.Builder()
                .setSampleRate(RATE_HZ)
                .setChannelMask(AudioFormat.CHANNEL_OUT_STEREO)
                .setEncoding(AudioFormat.ENCODING_PCM_16BIT)
                .build();
        try {
            AudioTrack track = new AudioTrack(attributes, format, min, AudioTrack.MODE_STREAM, a);
            // getBufferSizeInFrames throws on an uninitialised track: read it only then
            boolean initialized = track.getState() == AudioTrack.STATE_INITIALIZED;
            // PlayerBase.getPlayerIId is hidden API (the fixture compiles against the public SDK): read it reflectively
            Object piid = null;
            try {
                piid = AudioTrack.class.getMethod("getPlayerIId").invoke(track);
            } catch (Throwable t) {
                Log.i(TAG, "track: piid unreadable: " + t);
            }
            Log.i(TAG, "track: state=" + track.getState() + " piid=" + piid
                    + " frames=" + (initialized ? track.getBufferSizeInFrames() : 0));
            track.release();
        } catch (Throwable e) {
            Log.i(TAG, "track: " + e);
        }
        try {
            AudioTrack fast = new AudioTrack.Builder()
                    .setAudioAttributes(attributes)
                    .setAudioFormat(format)
                    .setBufferSizeInBytes(min)
                    .setPerformanceMode(AudioTrack.PERFORMANCE_MODE_LOW_LATENCY)
                    .build();
            Log.i(TAG, "fast: state=" + fast.getState());
            fast.release();
        } catch (Throwable e) {
            Log.i(TAG, "fast: " + e);
        }
        try {
            // a static track: its buffer is the client's (MODE_STATIC, the shared-buffer path SoundPool uses)
            AudioTrack st = new AudioTrack(attributes, format, STATIC_BYTES, AudioTrack.MODE_STATIC, a);
            // built = native setup succeeded (STATE_NO_STATIC_DATA until data is written)
            boolean built = st.getState() != AudioTrack.STATE_UNINITIALIZED;
            Log.i(TAG, "static: state=" + st.getState() + " frames="
                    + (built ? st.getBufferSizeInFrames() : 0));
            st.release();
        } catch (Throwable e) {
            Log.i(TAG, "static: " + e);
        }
        Log.i(TAG, "done");
    }
}
