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
 * The audio gate (`xtask test audio-app`, xtask-harness/src/audio_app.rs). Tag AtlAudio, one line each, checked by the gate:
 * `session: <a> <b>`: AudioManager.generateAudioSessionId twice (AudioSystem.newAudioSessionId, AudioFlinger's ids).
 * `minbuf: <bytes>`: AudioTrack.getMinBufferSize for 44100 Hz, stereo, 16-bit (AudioFlinger's sample rate, frame count and latency through AudioPolicyService's output).
 * `track: state=<n> piid=<n> frames=<n>`: a track's constructor (createTrack, IAudioTrack, then PlayerBase's trackPlayer): its state, its player id, its frame count.
 * `fast: state=<n>`: a low-latency track; the server refuses FAST and the track must still initialise on the normal path.
 * `static: state=<n> frames=<n>`: a shared-buffer track (MODE_STATIC): the frames are the requested ones, not rounded.
 * `props: rate=<s> frames=<s>`: AudioManager's PROPERTY_OUTPUT_SAMPLE_RATE and PROPERTY_OUTPUT_FRAMES_PER_BUFFER (the primary output's rate and HAL frame count).
 * `pcm: ...`: the PCM tones (audio_app.rs TONE_*, STREAM_*): a static track of STATIC_TONE_HZ, then a streaming track of STREAM_TONE_HZ in FMOD's shape, one after the other; the gate checks the recording of the sink.
 * `done`: the end of the run.
 */
public class Main extends Activity {
    private static final String TAG = "AtlAudio";
    /** The rate the gate asks for; audio_app.rs works the expected minimum buffer for it. */
    private static final int RATE_HZ = 44100;
    /** The static track's buffer: 4096 bytes, 1024 stereo 16-bit frames (audio_app.rs STATIC_BYTES). */
    private static final int STATIC_BYTES = 4096;
    /** The PCM tones (audio_app.rs TONE_*): rate, frames, frequencies, peak level, and the gap between the two. */
    private static final int TONE_RATE_HZ = 44100;
    private static final int TONE_FRAMES = 22050;
    private static final int STATIC_TONE_HZ = 1000;
    private static final int STREAM_TONE_HZ = 500;
    private static final double TONE_DBFS = -12.0;
    private static final int TONE_GAP_MS = 500;

    /** The streaming tone in FMOD's shape (audio_app.rs STREAM_*): FMOD's rate seen in Temple Run (getMinFrameCount for 24000 Hz), half a second of it. */
    private static final int STREAM_RATE_HZ = 24000;
    private static final int STREAM_FRAMES = 12000;

    /** Stereo 16-bit samples of a sine of `hz`: `frames` frames at `rate`, TONE_DBFS peak. */
    private static short[] tone(int hz, int rate, int frames) {
        short[] s = new short[frames * 2];
        double amp = Math.pow(10.0, TONE_DBFS / 20.0) * 32767.0;
        for (int i = 0; i < frames; i++) {
            short v = (short) Math.round(amp * Math.sin(2.0 * Math.PI * hz * i / rate));
            s[2 * i] = v;
            s[2 * i + 1] = v;
        }
        return s;
    }

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
        // the output properties FMOD and OpenSL users size their buffers by (AudioSystem.getPrimaryOutput*; null when the server answers 0)
        Log.i(TAG, "props: rate=" + audio.getProperty(AudioManager.PROPERTY_OUTPUT_SAMPLE_RATE)
                + " frames=" + audio.getProperty(AudioManager.PROPERTY_OUTPUT_FRAMES_PER_BUFFER));
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
        AudioFormat toneFormat = new AudioFormat.Builder()
                .setSampleRate(TONE_RATE_HZ)
                .setChannelMask(AudioFormat.CHANNEL_OUT_STEREO)
                .setEncoding(AudioFormat.ENCODING_PCM_16BIT)
                .build();
        int toneMs = TONE_FRAMES * 1000 / TONE_RATE_HZ;
        try {
            // a static track (SoundPool's shape): the samples are written before play, then the track plays them once
            short[] samples = tone(STATIC_TONE_HZ, TONE_RATE_HZ, TONE_FRAMES);
            AudioTrack st = new AudioTrack(attributes, toneFormat, samples.length * 2,
                    AudioTrack.MODE_STATIC, 0);
            int wrote = st.write(samples, 0, samples.length);
            st.play();
            Log.i(TAG, "pcm: static wrote=" + wrote + " state=" + st.getState());
            Thread.sleep(toneMs + TONE_GAP_MS);
            st.release();
        } catch (Throwable e) {
            Log.i(TAG, "pcm: static " + e);
        }
        try {
            // a streaming track in FMOD's Java output shape (Unity games, org.fmod.FMODAudioDevice): stream-type constructor, STREAM_MUSIC, STREAM_RATE_HZ, its own minimum buffer, byte writes. Play, then write; the blocking write returns once the buffer has taken every byte
            int streamMin = AudioTrack.getMinBufferSize(
                    STREAM_RATE_HZ, AudioFormat.CHANNEL_OUT_STEREO, AudioFormat.ENCODING_PCM_16BIT);
            AudioTrack ts = new AudioTrack(AudioManager.STREAM_MUSIC, STREAM_RATE_HZ,
                    AudioFormat.CHANNEL_OUT_STEREO, AudioFormat.ENCODING_PCM_16BIT, streamMin,
                    AudioTrack.MODE_STREAM);
            ts.play();
            short[] s = tone(STREAM_TONE_HZ, STREAM_RATE_HZ, STREAM_FRAMES);
            byte[] bytes = new byte[s.length * 2];
            for (int i = 0; i < s.length; i++) {
                bytes[2 * i] = (byte) s[i];
                bytes[2 * i + 1] = (byte) (s[i] >> 8);
            }
            int wrote = ts.write(bytes, 0, bytes.length);
            Log.i(TAG, "pcm: stream wrote=" + wrote);
            Thread.sleep(TONE_GAP_MS);
            ts.release();
        } catch (Throwable e) {
            Log.i(TAG, "pcm: stream " + e);
        }
        Log.i(TAG, "done");
    }
}
