// SPDX-License-Identifier: MIT
#include "wrap/mix.h"

#include <algorithm>
#include <atomic>
#include <new>

#include <binder/Parcel.h>

#include "audio_utils/format.h"
#include "audio_utils/minifloat.h"
#include "system/audio.h"

namespace atl::audio {

namespace {

/// The Rust side's mirror of audio_track_cblk_t. Per x86_64 LP64, measured from AudioTrackShared.h at the pinned frameworks/av; the static_assert keeps it honest at every build.
constexpr size_t kCblkBytes = 232;
static_assert(sizeof(android::audio_track_cblk_t) == kCblkBytes,
              "audio_track_cblk_t differs from the mirror in atl-audio-bridge (AudioTrackShared.h)");

/// Interleaved channels of the output: AUDIO_CHANNEL_OUT_STEREO.
constexpr size_t kOutChannels = 2;

void* as_value(uint32_t v) {
    return reinterpret_cast<void*>(static_cast<uintptr_t>(v));
}

/// A parcelable from its bytes as AOSP reads one (engine.cpp attributes_from): setData, then readParcelable.
template <typename T>
android::status_t read_parcelable(rust::Slice<const uint8_t> bytes, T& out) {
    android::Parcel parcel;
    if (android::status_t status = parcel.setData(bytes.data(), bytes.size()); status != android::NO_ERROR) {
        return status;
    }
    parcel.setDataPosition(0);
    return parcel.readParcelable(&out);
}

/// Counters for the output's line (atl-audioflinger output.rs): written by the realtime fill, read by the output's thread.
std::atomic<uint64_t> g_track_frames{0};
std::atomic<uint32_t> g_muted{0};
/// Enabled tracks: a drain ends in the fill, so the level is kept here, not by the control calls.
std::atomic<uint32_t> g_active{0};
/// Underruns the tracks' proxies counted (steps of the cblk's mUnderrunCount) and the frames they tallied as underrun.
std::atomic<uint64_t> g_underruns{0};
std::atomic<uint64_t> g_underrun_frames{0};

/// Disables track `name` and forgets it as enabled (a pause, a static stop, a finished drain).
void disable(Mix& mix, int32_t name) {
    mix.disable(name);
    if (const auto it = mix.enabled.find(name); it != mix.enabled.end()) {
        it->second->draining = false;
        mix.enabled.erase(it);
    }
    g_active.store(static_cast<uint32_t>(mix.enabled.size()), std::memory_order_relaxed);
}

/// The client's volume (cblk mVolumeLR, gain_minifloat) times the track's shaper gain on both sides, applied to track `name` as AudioFlinger's prepareTracks does every cycle (Threads.cpp:5860-5862: `volume *= vh`, then the client's LR).
/// True when the client's volume is zero on both sides (the shaper is not counted: muted_tracks reads the client's volume).
bool apply_volume(Mix& mix, Track& track, int32_t name) {
    const gain_minifloat_packed_t volume = track.proxy->getVolumeLR();
    float left = float_from_gain(gain_minifloat_unpack_left(volume));
    float right = float_from_gain(gain_minifloat_unpack_right(volume));
    const bool muted = left == 0.0f && right == 0.0f;
    const float shaper = track.volume->getVolume(track.proxy->framesReleased()).first;
    left *= shaper;
    right *= shaper;
    mix.setParameter(name, android::AudioMixerBase::VOLUME, android::AudioMixerBase::VOLUME0, &left);
    mix.setParameter(name, android::AudioMixerBase::VOLUME, android::AudioMixerBase::VOLUME1, &right);
    return muted;
}

}  // namespace

Track::Track(android::sp<android::AudioTrackServerProxy> server, audio_format_t track_format, size_t channels_per_frame,
             bool shared_buffer, uint32_t rate)
    : proxy(std::move(server)), format(track_format), channels(channels_per_frame), is_static(shared_buffer),
      volume(new android::media::VolumeHandler(rate)) {}

// Track::getNextBuffer (Tracks.cpp:1252): the proxy hands out what the client has written, in the track's format. The mixer reads float (AudioMixerBase::create sets mMixerInFormat to PCM_FLOAT; nothing changes it), so the frames go through AOSP's conversion: the copy ReformatBufferProvider::copyFrames makes (BufferProviders.cpp:412), which AudioMixer inserts for any track format other than the mixer's. That class is not linked: its object needs libaudiohal (EffectsFactoryHalInterface) and libsonic.
// A float track is copied as is (memcpy_by_audio_format of equal formats is a memcpy), still into `floats`: one buffer shape for every format.
// The underrun tally is Track::getNextBuffer's (Tracks.cpp:1260-1272): nothing obtained for a track the mixer still plays -> the whole request is tallied, anything obtained -> 0, which ends the underrun so the next one counts again. The cblk count it keeps is what the client reads as underruns
// (AudioTrack::getUnderrunCount, AAudioStream_getXRunCount), and adaptive clients grow their buffer on it. AOSP skips stopping, pausing, stopped, paused and offloaded tracks; here only an enabled track is pulled (a pause disables it at once), so a draining (stopped) one is the case left.
// prepareTracks_l's own tally (Threads.cpp:6281-6286) is for a track it does not mix that round (framesReady below a full period); this mixer pulls every enabled track every quantum, so the shortfall reaches this call as an empty obtain for the rest of the quantum.
android::status_t Track::getNextBuffer(Buffer* buffer) {
    android::ServerProxy::Buffer buf;
    const size_t desired = buffer->frameCount;
    buf.mFrameCount = desired;
    android::status_t status = proxy->obtainBuffer(&buf);
    const uint32_t underruns = is_static ? 0 : proxy->getUnderrunCount();
    if (buf.mFrameCount == 0 && !draining) {
        proxy->tallyUnderrunFrames(static_cast<uint32_t>(desired));
        // a static track's cblk has no underrun count (its union holds mStatic): StaticAudioTrackServerProxy only flags the underrun
        if (!is_static) {
            g_underruns.fetch_add(proxy->getUnderrunCount() - underruns, std::memory_order_relaxed);
            g_underrun_frames.fetch_add(desired, std::memory_order_relaxed);
        }
    } else {
        proxy->tallyUnderrunFrames(0);
    }
    server_raw = buf.mRaw;
    // the mixer asks for at most one quantum, and mix_add sized `floats` for that: the realtime fill allocates nothing
    const size_t frames = std::min<size_t>(buf.mFrameCount, floats.size() / channels);
    buffer->frameCount = frames;
    if (frames == 0 || server_raw == nullptr) {
        buffer->frameCount = 0;
        buffer->raw = nullptr;
        return status;
    }
    g_track_frames.fetch_add(frames, std::memory_order_relaxed);
    memcpy_by_audio_format(floats.data(), AUDIO_FORMAT_PCM_FLOAT, server_raw, format, frames * channels);
    buffer->raw = floats.data();
    return status;
}

// TrackBase::releaseBuffer: the frames the mixer consumed go back to the proxy, from the server buffer obtained above.
void Track::releaseBuffer(Buffer* buffer) {
    android::ServerProxy::Buffer buf;
    buf.mFrameCount = buffer->frameCount;
    buf.mRaw = server_raw;
    buf.mNonContig = 0;
    buffer->frameCount = 0;
    buffer->raw = nullptr;
    server_raw = nullptr;
    proxy->releaseBuffer(&buf);
}

Mix::Mix(size_t quantum_frames, uint32_t rate)
    : android::AudioMixerBase(quantum_frames, rate), quantum(quantum_frames), out(quantum_frames * kOutChannels) {}

void Mix::set_provider(int32_t name, android::AudioBufferProvider* provider) {
    mTracks.find(name)->second->bufferProvider = provider;
}

size_t cblk_bytes() {
    return kCblkBytes;
}

// TrackBase's placement construction (Tracks.cpp:209), then the proxy the server reads the track through (Tracks.cpp:943-946).
std::unique_ptr<Track> new_track(uint8_t* cblk_mem, uint8_t* buffer, uint32_t format, size_t frames, size_t frame_bytes,
                                 bool is_static, uint32_t rate) {
    const auto track_format = static_cast<audio_format_t>(format);
    auto* cblk = new (cblk_mem) android::audio_track_cblk_t();
    android::sp<android::AudioTrackServerProxy> server;
    if (is_static) {
        server = new android::StaticAudioTrackServerProxy(cblk, buffer, frames, frame_bytes, rate);
    } else {
        // clientInServer is false for an AudioTrack's track (Tracks.cpp: !isExternalTrack())
        server = new android::AudioTrackServerProxy(cblk, buffer, frames, frame_bytes,
                                                    /*clientInServer=*/false, rate);
    }
    // one sample per channel: frame_bytes is channels x audio_bytes_per_sample(format) (createTrack_l's frameSize)
    return std::make_unique<Track>(std::move(server), track_format, frame_bytes / audio_bytes_per_sample(track_format),
                                   is_static, rate);
}

std::unique_ptr<Mix> new_mix(size_t quantum_frames, uint32_t rate) {
    return std::make_unique<Mix>(quantum_frames, rate);
}

// AudioMixerBase::create and the parameters AudioFlinger's prepareTracks sets (Threads.cpp, the mixer block).
bool mix_add(Mix& mix, Track& track, int32_t name, int32_t session, uint32_t channel_mask, uint32_t rate) {
    if (mix.exists(name)) {
        return false;
    }
    if (mix.create(name, static_cast<audio_channel_mask_t>(channel_mask), track.format, session) != android::OK) {
        return false;
    }
    // the conversion target for one quantum of frames (control call: outside the realtime fill)
    track.floats.resize(mix.quantum * track.channels);
    mix.setParameter(name, android::AudioMixerBase::TRACK, android::AudioMixerBase::MIXER_FORMAT,
                     as_value(AUDIO_FORMAT_PCM_FLOAT));
    mix.setParameter(name, android::AudioMixerBase::TRACK, android::AudioMixerBase::MAIN_BUFFER,
                     mix.out.data());
    mix.setParameter(name, android::AudioMixerBase::RESAMPLE, android::AudioMixerBase::SAMPLE_RATE,
                     as_value(rate));
    mix.set_provider(name, &track);
    return true;
}

// A removed track is not mixed again, draining or not: AOSP's destroyTrack_l terminates it (Threads.cpp:3021-3026) and prepareTracks_l mixes no terminated track.
// A static track's buffer stays the client's to replay, so it never counts as dropped (Tracks.cpp:1321-1325 gives it no frames once stopped).
uint64_t mix_remove(Mix& mix, Track& track, int32_t name) {
    uint64_t dropped = 0;
    if (!track.is_static) {
        const uint64_t unread = track.proxy->framesReady();
        dropped = unread + static_cast<uint64_t>(std::max<int64_t>(track.proxy->framesFlushed(), 0));
    }
    if (mix.exists(name)) {
        disable(mix, name);
        mix.destroy(name);
    }
    return dropped;
}

void mix_start(Mix& mix, Track& track, int32_t name) {
    // a draining track resumes: start lifts the client's stop limit (AudioTrackServerProxy::start)
    track.proxy->start();
    track.draining = false;
    apply_volume(mix, track, name);
    mix.enabled[name] = &track;
    mix.enable(name);
    g_active.store(static_cast<uint32_t>(mix.enabled.size()), std::memory_order_relaxed);
}

void mix_pause(Mix& mix, int32_t name) {
    disable(mix, name);
}

// Track::stop (Tracks.cpp:1550-1553): a normal mixed track goes to STOPPED and stays active; prepareTracks_l keeps mixing it with minFrames 1 (Threads.cpp:5976-5980) until framesReady, bounded by the client's stop position (AudioTrackServerProxy::getRear), is 0, then removes it (:6297-6309).
// A static track has no frames once stopped (Tracks.cpp:1321-1325): it leaves the mix now.
void mix_stop(Mix& mix, Track& track, int32_t name) {
    if (track.is_static || mix.enabled.count(name) == 0) {
        disable(mix, name);
        return;
    }
    track.draining = true;
}

// Track::flush (Tracks.cpp:1646-1648): a track the mixer is not reading is flushed now, so the client can refill it while stopped or paused. A draining track's flush is applied by its next framesReady in the fill, which then ends the drain.
void mix_flush(Mix& mix, Track& track, int32_t name) {
    if (mix.enabled.count(name) == 0) {
        track.proxy->flushBufferIfNeeded();
    }
}

// TrackHandle::applyVolumeShaper (Tracks.cpp:603-621): both parcelables are read before the handler sees them; a read that fails is the status and leaves the handler untouched.
int32_t track_apply_volume_shaper(Track& track, rust::Slice<const uint8_t> configuration, rust::Slice<const uint8_t> operation) {
    android::media::VolumeShaperConfiguration conf_parcel;
    if (android::status_t status = read_parcelable(configuration, conf_parcel); status != android::NO_ERROR) {
        return status;
    }
    android::media::VolumeShaperOperation op_parcel;
    if (android::status_t status = read_parcelable(operation, op_parcel); status != android::NO_ERROR) {
        return status;
    }
    android::sp<android::media::VolumeShaper::Configuration> conf = new android::media::VolumeShaper::Configuration();
    if (android::status_t status = conf->readFromParcelable(conf_parcel); status != android::NO_ERROR) {
        return status;
    }
    android::sp<android::media::VolumeShaper::Operation> op = new android::media::VolumeShaper::Operation();
    if (android::status_t status = op->readFromParcelable(op_parcel); status != android::NO_ERROR) {
        return status;
    }
    return track.volume->applyVolumeShaper(conf, op);
}

// AudioMixerBase::process() makes one quantum; the output takes it frame by frame, so a request of any size is served from it.
void mix_render(Mix& mix, rust::Slice<float> out) {
    const size_t frames = out.size() / kOutChannels;
    for (size_t done = 0; done < frames;) {
        if (mix.next == mix.quantum) {
            // a stopped track whose frames up to its stop position are all mixed leaves the mix (prepareTracks_l, Threads.cpp:6297-6309)
            for (auto it = mix.enabled.begin(); it != mix.enabled.end();) {
                const auto [name, track] = *it++;  // advanced first: disable erases this entry
                if (track->draining && track->proxy->framesReady() == 0) {
                    disable(mix, name);
                }
            }
            uint32_t muted = 0;
            for (auto& [name, track] : mix.enabled) {
                muted += apply_volume(mix, *track, name) ? 1 : 0;
            }
            g_muted.store(muted, std::memory_order_relaxed);
            std::fill(mix.out.begin(), mix.out.end(), 0.0f);
            mix.process();
            mix.next = 0;
        }
        const size_t n = std::min(frames - done, mix.quantum - mix.next);
        std::copy_n(mix.out.data() + mix.next * kOutChannels, n * kOutChannels,
                    out.data() + done * kOutChannels);
        mix.next += n;
        done += n;
    }
}

uint64_t take_track_frames() {
    return g_track_frames.exchange(0, std::memory_order_relaxed);
}

uint32_t muted_tracks() {
    return g_muted.load(std::memory_order_relaxed);
}

uint32_t active_tracks() {
    return g_active.load(std::memory_order_relaxed);
}

uint64_t take_underruns() {
    return g_underruns.exchange(0, std::memory_order_relaxed);
}

uint64_t take_underrun_frames() {
    return g_underrun_frames.exchange(0, std::memory_order_relaxed);
}

uint32_t track_underruns(const Track& track) {
    return track.is_static ? 0 : track.proxy->getUnderrunCount();
}

}  // namespace atl::audio
