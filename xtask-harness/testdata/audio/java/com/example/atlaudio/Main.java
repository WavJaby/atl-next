// SPDX-License-Identifier: MIT
package com.example.atlaudio;

import android.app.Activity;
import android.content.BroadcastReceiver;
import android.content.Context;
import android.content.Intent;
import android.content.IntentFilter;
import android.media.AudioAttributes;
import android.media.AudioFormat;
import android.media.AudioFocusRequest;
import android.media.AudioManager;
import android.media.AudioTrack;
import android.os.Bundle;
import android.os.Handler;
import android.os.HandlerThread;
import android.os.Looper;
import android.util.Log;
import java.io.IOException;
import java.io.InputStream;
import java.nio.charset.StandardCharsets;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicInteger;

/**
 * The audio gate (`xtask test audio-app`, xtask-harness/src/audio_app.rs). Tag AtlAudio, one line each, checked by the gate:
 * `session: <a> <b>`: AudioManager.generateAudioSessionId twice (AudioSystem.newAudioSessionId, AudioFlinger's ids).
 * `minbuf: <bytes>`: AudioTrack.getMinBufferSize for 44100 Hz, stereo, 16-bit (AudioFlinger's sample rate, frame count and latency through AudioPolicyService's output).
 * `track: state=<n> piid=<n> frames=<n>`: a track's constructor (createTrack, IAudioTrack, then PlayerBase's trackPlayer): its state, its player id, its frame count.
 * `fast: state=<n>`: a low-latency track; the server refuses FAST and the track must still initialise on the normal path.
 * `static: state=<n> frames=<n>`: a shared-buffer track (MODE_STATIC): the frames are the requested ones, not rounded.
 * `props: rate=<s> frames=<s>`: AudioManager's PROPERTY_OUTPUT_SAMPLE_RATE and PROPERTY_OUTPUT_FRAMES_PER_BUFFER (the primary output's rate and HAL frame count).
 * `group: media=<n> ringtone=<n>`: AudioManager.getVolumeGroupIdForAttributes of USAGE_MEDIA and of USAGE_NOTIFICATION_RINGTONE (the strategy list policy.rs serves).
 * `tts: flags=<n>`: the flags (AudioAttributes.getAllFlags, read reflectively) of a legacy TTS stream's attributes: FLAG_BEACON.
 * `pcm: ...`: the PCM tones (audio_app.rs TONE_*, STREAM_*): a static track of STATIC_TONE_HZ, then a streaming track of STREAM_TONE_HZ in FMOD's shape, then a float streaming track of FLOAT_TONE_HZ, then an OpenSL ES tone and an AAudio tone (native, `pcm: sles ...` / `pcm: aaudio ...`), one after the other; the gate checks the recording of the sink. Then the silent adaptive AAudio client (`pcm: aaudio-adaptive ...`).
 * `done`: the end of the run.
 * The volume run (asset `volume` present, audio_app.rs `--volume`) logs only `volume: ...` lines (volumeRun) and `done`.
 */
public class Main extends Activity {
    private static final String TAG = "AtlAudio";
    /** The volume run's asset: the music index to set, written by audio_app.rs `--volume`. */
    private static final String VOLUME_ASSET = "volume";
    /** How long the app's own mute lasts: long enough for the gate to read it on the node (app_volume.rs polls every 50 ms, within STEP_LIMIT). */
    private static final long APP_MUTE_MS = 1500;
    /** How long the focus probe waits for the first listener's LOSS_TRANSIENT before it abandons: a bound, so a dispatcher that sends none reads as lost=0. */
    private static final long FOCUS_WAIT_MS = 5000;
    /** AudioManager's hidden broadcast actions and extras (AudioManager.java:204, :246, :288, :298, :306, :312, :338); the fixture compiles against the public SDK. */
    private static final String VOLUME_CHANGED_ACTION = "android.media.VOLUME_CHANGED_ACTION";
    private static final String STREAM_MUTE_CHANGED_ACTION = "android.media.STREAM_MUTE_CHANGED_ACTION";
    private static final String EXTRA_VOLUME_STREAM_TYPE = "android.media.EXTRA_VOLUME_STREAM_TYPE";
    private static final String EXTRA_VOLUME_STREAM_TYPE_ALIAS = "android.media.EXTRA_VOLUME_STREAM_TYPE_ALIAS";
    private static final String EXTRA_VOLUME_STREAM_VALUE = "android.media.EXTRA_VOLUME_STREAM_VALUE";
    private static final String EXTRA_PREV_VOLUME_STREAM_VALUE = "android.media.EXTRA_PREV_VOLUME_STREAM_VALUE";
    private static final String EXTRA_STREAM_VOLUME_MUTED = "android.media.EXTRA_STREAM_VOLUME_MUTED";
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
    /** The float tone (audio_app.rs FLOAT_TONE_HZ): Angry Birds 2's FMOD AAudio stream shape, float at STREAM_RATE_HZ, STREAM_FRAMES of it. */
    private static final int FLOAT_TONE_HZ = 750;
    /** The native tones (audio_app.rs SLES_TONE_HZ, AAUDIO_TONE_HZ, NATIVE_*): 16-bit stereo at the output's rate, NATIVE_FRAMES each, TONE_DBFS peak. */
    private static final int SLES_TONE_HZ = 1500;
    private static final int AAUDIO_TONE_HZ = 250;
    private static final int NATIVE_RATE_HZ = 48000;
    private static final int NATIVE_FRAMES = 72000;
    /** AudioSystem.STREAM_TTS (AudioSystem.java:107, @hide): the speaker stream the flags probe builds from. */
    private static final int STREAM_TTS = 9;

    /** OpenSL ES simple buffer queue player (jni/atl_audio_tone.c): "sles enqueued=<n> callbacks=<n> failed=<n>". */
    private static native String slesTone(int hz, int rate, int frames, double dbfs);
    /** AAudio data-callback stream: "aaudio open=<rc> start=<rc> written=<n> stop=<rc> close=<rc>". */
    private static native String aaudioTone(int hz, int rate, int frames, double dbfs);
    /** The adaptive AAudio client (audio_app.rs ADAPTIVE_*): ends after ADAPTIVE_STABLE_MS without an xrun, or at ADAPTIVE_MAX_MS. */
    private static final int ADAPTIVE_MAX_MS = 8000;
    private static final int ADAPTIVE_STABLE_MS = 1000;
    /** A silent LOW_LATENCY stream that starts at one burst and grows by one per xrun rise: "aaudio-adaptive ... first=<n> final=<n> xruns=<n> stable=<ms> ...". */
    private static native String aaudioAdaptive(int rate, int maxMs, int stableMs);

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

    /** The volume run's index (audio_app.rs `--volume` puts it in asset VOLUME_ASSET); -1: the PCM run. */
    private int volumeIndex() {
        try (InputStream in = getAssets().open(VOLUME_ASSET)) {
            return Integer.parseInt(new String(in.readAllBytes(), StandardCharsets.US_ASCII).trim());
        } catch (IOException | NumberFormatException e) {
            return -1;
        }
    }

    /**
     * The volume run (app_volume.rs): AudioManager's own calls against the app's host stream, silent. Receivers log VOLUME_CHANGED_ACTION / STREAM_MUTE_CHANGED_ACTION of STREAM_MUSIC with their
     * extras and AudioManager's reading; a silent track keeps the stream up; setStreamVolume(STREAM_MUSIC, index) is the app-side write. After the host's mute and unmute, the app mutes
     * (adjustStreamVolume ADJUST_MUTE) and APP_MUTE_MS later unmutes, the app-side mute the gate reads on the node.
     */
    private void volumeRun(AudioManager audio, int index) {
        Context app = getApplicationContext();
        app.registerReceiver(new BroadcastReceiver() {
            @Override
            public void onReceive(Context c, Intent i) {
                int stream = i.getIntExtra(EXTRA_VOLUME_STREAM_TYPE, -1);
                if (stream != AudioManager.STREAM_MUSIC) {
                    return;
                }
                Log.i(TAG, "volume: changed stream=" + stream
                        + " value=" + i.getIntExtra(EXTRA_VOLUME_STREAM_VALUE, -1)
                        + " prev=" + i.getIntExtra(EXTRA_PREV_VOLUME_STREAM_VALUE, -1)
                        + " alias=" + i.getIntExtra(EXTRA_VOLUME_STREAM_TYPE_ALIAS, -1)
                        + " get=" + audio.getStreamVolume(stream));
            }
        }, new IntentFilter(VOLUME_CHANGED_ACTION), Context.RECEIVER_EXPORTED);
        int[] seen = {0, 0};
        Handler main = new Handler(Looper.getMainLooper());
        app.registerReceiver(new BroadcastReceiver() {
            @Override
            public void onReceive(Context c, Intent i) {
                int stream = i.getIntExtra(EXTRA_VOLUME_STREAM_TYPE, -1);
                if (stream != AudioManager.STREAM_MUSIC) {
                    return;
                }
                boolean muted = i.getBooleanExtra(EXTRA_STREAM_VOLUME_MUTED, !audio.isStreamMute(stream));
                Log.i(TAG, "volume: mute stream=" + stream + " muted=" + muted
                        + " is=" + audio.isStreamMute(stream));
                // seen[1] counts unmutes: the host's (1st) starts the app's own mute; the app's own unmute (2nd) ends the run
                if (!muted && ++seen[1] == 1) {
                    audio.adjustStreamVolume(stream, AudioManager.ADJUST_MUTE, 0);
                    Log.i(TAG, "volume: app mute");
                    main.postDelayed(new Runnable() {
                        @Override
                        public void run() {
                            audio.adjustStreamVolume(stream, AudioManager.ADJUST_UNMUTE, 0);
                            Log.i(TAG, "volume: app unmute");
                        }
                    }, APP_MUTE_MS);
                } else if (!muted && seen[1] == 2) {
                    Log.i(TAG, "done");
                }
            }
        }, new IntentFilter(STREAM_MUTE_CHANGED_ACTION), Context.RECEIVER_EXPORTED);
        Thread silence = new Thread(new Runnable() {
            @Override
            public void run() {
                AudioAttributes attributes = new AudioAttributes.Builder()
                        .setUsage(AudioAttributes.USAGE_GAME).build();
                AudioFormat format = new AudioFormat.Builder()
                        .setSampleRate(RATE_HZ)
                        .setChannelMask(AudioFormat.CHANNEL_OUT_STEREO)
                        .setEncoding(AudioFormat.ENCODING_PCM_16BIT)
                        .build();
                int min = AudioTrack.getMinBufferSize(
                        RATE_HZ, AudioFormat.CHANNEL_OUT_STEREO, AudioFormat.ENCODING_PCM_16BIT);
                AudioTrack track = new AudioTrack(attributes, format, min, AudioTrack.MODE_STREAM, 0);
                track.play();
                short[] zeros = new short[min / 2];
                // the gate ends the run; until then the stream (and its host node) stays
                while (track.write(zeros, 0, zeros.length) > 0) {
                }
            }
        }, "silence");
        silence.setDaemon(true);
        silence.start();
        audio.setStreamVolume(AudioManager.STREAM_MUSIC, index, 0);
        Log.i(TAG, "volume: set index=" + index + " get=" + audio.getStreamVolume(AudioManager.STREAM_MUSIC));
    }

    /**
     * The policy probe: AudioManager's volume group of USAGE_MEDIA and of USAGE_NOTIFICATION_RINGTONE, through the strategy list policy.rs serves (AudioProductStrategy.java:184, :353);
     * and the flags of a legacy TTS stream's attributes (AudioAttributes.java:1223 sets FLAG_BEACON). getAllFlags is @SystemApi (AudioAttributes.java:656): read reflectively.
     * Each part reports its own failure, so the focus probe and the tracks still run.
     */
    private void policyProbe(AudioManager audio) {
        try {
            AudioAttributes media = new AudioAttributes.Builder()
                    .setUsage(AudioAttributes.USAGE_MEDIA).build();
            AudioAttributes ringtone = new AudioAttributes.Builder()
                    .setUsage(AudioAttributes.USAGE_NOTIFICATION_RINGTONE).build();
            Log.i(TAG, "group: media=" + audio.getVolumeGroupIdForAttributes(media)
                    + " ringtone=" + audio.getVolumeGroupIdForAttributes(ringtone));
        } catch (Throwable e) {
            Log.i(TAG, "group: " + e);
        }
        try {
            AudioAttributes tts = new AudioAttributes.Builder()
                    .setLegacyStreamType(STREAM_TTS).build();
            Object flags = AudioAttributes.class.getMethod("getAllFlags").invoke(tts);
            Log.i(TAG, "tts: flags=" + flags);
        } catch (Throwable e) {
            Log.i(TAG, "tts: " + e);
        }
    }

    /**
     * The focus probe, public AudioManager API only: a foreground media gain is GRANTED, its identical repeat is GRANTED, a DELAY_OK
     * request with no lock is GRANTED (never DELAYED), and each abandon succeeds. The first listener must receive LOSS_TRANSIENT when the
     * delayed transient gain takes focus. Its callback runs on a handler thread; the probe waits for it (bounded) before abandoning the top,
     * since an abandon unregisters the listener. The listeners log what they receive; the summary's lost and restored are those changes, or 0 when none came.
     */
    private void focusProbe(AudioManager audio) {
        AudioAttributes media = new AudioAttributes.Builder()
                .setUsage(AudioAttributes.USAGE_MEDIA).build();
        HandlerThread thread = new HandlerThread("atl-focus");
        thread.start();
        Handler callbacks = new Handler(thread.getLooper());
        CountDownLatch lossSeen = new CountDownLatch(1);
        CountDownLatch regainSeen = new CountDownLatch(1);
        AtomicInteger lost = new AtomicInteger(AudioManager.AUDIOFOCUS_NONE);
        AtomicInteger regained = new AtomicInteger(AudioManager.AUDIOFOCUS_NONE);
        AudioManager.OnAudioFocusChangeListener first = new AudioManager.OnAudioFocusChangeListener() {
            public void onAudioFocusChange(int change) {
                Log.i(TAG, "focus: first change=" + change);
                if (change == AudioManager.AUDIOFOCUS_LOSS_TRANSIENT) {
                    lost.set(change);
                    lossSeen.countDown();
                } else if (change == AudioManager.AUDIOFOCUS_GAIN && lossSeen.getCount() == 0) {
                    regained.set(change);
                    regainSeen.countDown();
                }
            }
        };
        AudioManager.OnAudioFocusChangeListener second = new AudioManager.OnAudioFocusChangeListener() {
            public void onAudioFocusChange(int change) {
                Log.i(TAG, "focus: second change=" + change);
            }
        };
        AudioFocusRequest request = new AudioFocusRequest.Builder(AudioManager.AUDIOFOCUS_GAIN)
                .setAudioAttributes(media)
                .setOnAudioFocusChangeListener(first, callbacks)
                .build();
        int granted = audio.requestAudioFocus(request);
        int repeat = audio.requestAudioFocus(request);
        AudioFocusRequest delayed = new AudioFocusRequest.Builder(AudioManager.AUDIOFOCUS_GAIN_TRANSIENT)
                .setAudioAttributes(media)
                .setAcceptsDelayedFocusGain(true)
                .setOnAudioFocusChangeListener(second)
                .build();
        int delay = audio.requestAudioFocus(delayed);
        try {
            lossSeen.await(FOCUS_WAIT_MS, TimeUnit.MILLISECONDS);
        } catch (InterruptedException e) {
            Thread.currentThread().interrupt();
        }
        // the top (delayed) leaves: the first listener, now on top again, gets GAIN
        int abandon = audio.abandonAudioFocusRequest(delayed);
        try {
            regainSeen.await(FOCUS_WAIT_MS, TimeUnit.MILLISECONDS);
        } catch (InterruptedException e) {
            Thread.currentThread().interrupt();
        }
        int released = audio.abandonAudioFocusRequest(request);
        int again = audio.abandonAudioFocusRequest(request);
        Log.i(TAG, "focus: first=" + granted + " repeat=" + repeat + " delayed=" + delay
                + " abandon=" + abandon + " released=" + released + " again=" + again
                + " lost=" + lost.get() + " restored=" + regained.get());
        thread.quitSafely();
    }

    @Override
    protected void onCreate(Bundle state) {
        super.onCreate(state);
        AudioManager audio = (AudioManager) getSystemService(AUDIO_SERVICE);
        int volume = volumeIndex();
        if (volume >= 0) {
            volumeRun(audio, volume);
            return;
        }
        int a = audio.generateAudioSessionId();
        int b = audio.generateAudioSessionId();
        Log.i(TAG, "session: " + a + " " + b);
        int min = AudioTrack.getMinBufferSize(
                RATE_HZ, AudioFormat.CHANNEL_OUT_STEREO, AudioFormat.ENCODING_PCM_16BIT);
        Log.i(TAG, "minbuf: " + min);
        // the output properties FMOD and OpenSL users size their buffers by (AudioSystem.getPrimaryOutput*; null when the server answers 0)
        Log.i(TAG, "props: rate=" + audio.getProperty(AudioManager.PROPERTY_OUTPUT_SAMPLE_RATE)
                + " frames=" + audio.getProperty(AudioManager.PROPERTY_OUTPUT_FRAMES_PER_BUFFER));
        policyProbe(audio);
        focusProbe(audio);
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
        try {
            // a float streaming track in Angry Birds 2's FMOD shape (ENCODING_PCM_FLOAT, STREAM_RATE_HZ, stereo); the same samples as the 16-bit tones, scaled to full scale 1.0
            AudioFormat floatFormat = new AudioFormat.Builder()
                    .setSampleRate(STREAM_RATE_HZ)
                    .setChannelMask(AudioFormat.CHANNEL_OUT_STEREO)
                    .setEncoding(AudioFormat.ENCODING_PCM_FLOAT)
                    .build();
            int floatMin = AudioTrack.getMinBufferSize(
                    STREAM_RATE_HZ, AudioFormat.CHANNEL_OUT_STEREO, AudioFormat.ENCODING_PCM_FLOAT);
            AudioTrack tf = new AudioTrack(attributes, floatFormat, floatMin, AudioTrack.MODE_STREAM, 0);
            tf.play();
            short[] s = tone(FLOAT_TONE_HZ, STREAM_RATE_HZ, STREAM_FRAMES);
            float[] f = new float[s.length];
            for (int i = 0; i < s.length; i++) {
                f[i] = s[i] / 32768.0f;
            }
            int wrote = tf.write(f, 0, f.length, AudioTrack.WRITE_BLOCKING);
            Log.i(TAG, "pcm: float wrote=" + wrote + " state=" + tf.getState());
            Thread.sleep(TONE_GAP_MS);
            tf.release();
        } catch (Throwable e) {
            Log.i(TAG, "pcm: float " + e);
        }
        try {
            // the native tones (jni/atl_audio_tone.c): OpenSL ES's buffer queue, then AAudio's data callback; each call blocks until its tone was handed out
            System.loadLibrary("atl_audio_tone");
            Log.i(TAG, "pcm: " + slesTone(SLES_TONE_HZ, NATIVE_RATE_HZ, NATIVE_FRAMES, TONE_DBFS));
            Thread.sleep(TONE_GAP_MS);
            Log.i(TAG, "pcm: " + aaudioTone(AAUDIO_TONE_HZ, NATIVE_RATE_HZ, NATIVE_FRAMES, TONE_DBFS));
            Thread.sleep(TONE_GAP_MS);
            Log.i(TAG, "pcm: " + aaudioAdaptive(NATIVE_RATE_HZ, ADAPTIVE_MAX_MS, ADAPTIVE_STABLE_MS));
        } catch (Throwable e) {
            Log.i(TAG, "pcm: native " + e);
        }
        Log.i(TAG, "done");
    }
}
