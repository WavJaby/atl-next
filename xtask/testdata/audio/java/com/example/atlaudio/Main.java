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
 * The A1a gate (`xtask test audio-app`, xtask/src/audio_app.rs). Tag AtlAudio, one line each, checked by the gate:
 * `session: <a> <b>`: AudioManager.generateAudioSessionId twice (AudioSystem.newAudioSessionId, AudioFlinger's ids).
 * `minbuf: <bytes>`: AudioTrack.getMinBufferSize for 44100 Hz, stereo, 16-bit (AudioFlinger's sample rate, frame count and latency through AudioPolicyService's output).
 * `track: ...`: a track's constructor, which runs PlayerBase's trackPlayer before createTrack (A1b, not yet served; the line says what it threw).
 * `done`: the end of the run.
 */
public class Main extends Activity {
    private static final String TAG = "AtlAudio";
    /** The rate the gate asks for; audio_app.rs works the expected minimum buffer for it. */
    private static final int RATE_HZ = 44100;

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
        try {
            AudioTrack track = new AudioTrack(
                    new AudioAttributes.Builder().setUsage(AudioAttributes.USAGE_GAME).build(),
                    new AudioFormat.Builder()
                            .setSampleRate(RATE_HZ)
                            .setChannelMask(AudioFormat.CHANNEL_OUT_STEREO)
                            .setEncoding(AudioFormat.ENCODING_PCM_16BIT)
                            .build(),
                    min, AudioTrack.MODE_STREAM, a);
            Log.i(TAG, "track: built");
            track.release();
        } catch (Throwable e) {
            Log.i(TAG, "track: " + e);
        }
        Log.i(TAG, "done");
    }
}
