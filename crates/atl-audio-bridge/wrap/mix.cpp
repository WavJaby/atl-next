// SPDX-License-Identifier: MIT
#include "wrap/mix.h"

#include <algorithm>
#include <new>

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

}  // namespace

Track::Track(android::sp<android::AudioTrackServerProxy> server) : proxy(std::move(server)) {}

// Track::getNextBuffer (Tracks.cpp:1252): the proxy hands out what the client has written.
android::status_t Track::getNextBuffer(Buffer* buffer) {
    android::ServerProxy::Buffer buf;
    buf.mFrameCount = buffer->frameCount;
    android::status_t status = proxy->obtainBuffer(&buf);
    buffer->frameCount = buf.mFrameCount;
    buffer->raw = buf.mRaw;
    return status;
}

// TrackBase::releaseBuffer: the frames the mixer consumed go back to the proxy.
void Track::releaseBuffer(Buffer* buffer) {
    android::ServerProxy::Buffer buf;
    buf.mFrameCount = buffer->frameCount;
    buf.mRaw = buffer->raw;
    buf.mNonContig = 0;
    buffer->frameCount = 0;
    buffer->raw = nullptr;
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
std::unique_ptr<Track> new_track(uint8_t* cblk_mem, uint8_t* buffer, size_t frames, size_t frame_bytes,
                                 bool is_static, uint32_t rate) {
    auto* cblk = new (cblk_mem) android::audio_track_cblk_t();
    android::sp<android::AudioTrackServerProxy> server;
    if (is_static) {
        server = new android::StaticAudioTrackServerProxy(cblk, buffer, frames, frame_bytes, rate);
    } else {
        // clientInServer is false for an AudioTrack's track (Tracks.cpp: !isExternalTrack())
        server = new android::AudioTrackServerProxy(cblk, buffer, frames, frame_bytes,
                                                    /*clientInServer=*/false, rate);
    }
    return std::make_unique<Track>(std::move(server));
}

std::unique_ptr<Mix> new_mix(size_t quantum_frames, uint32_t rate) {
    return std::make_unique<Mix>(quantum_frames, rate);
}

// AudioMixerBase::create and the parameters AudioFlinger's prepareTracks sets (Threads.cpp, the mixer block).
bool mix_add(Mix& mix, Track& track, int32_t name, int32_t session, uint32_t channel_mask, uint32_t rate) {
    if (mix.exists(name)) {
        return false;
    }
    if (mix.create(name, channel_mask, AUDIO_FORMAT_PCM_16_BIT, session) != android::OK) {
        return false;
    }
    mix.setParameter(name, android::AudioMixerBase::TRACK, android::AudioMixerBase::MIXER_FORMAT,
                     as_value(AUDIO_FORMAT_PCM_FLOAT));
    mix.setParameter(name, android::AudioMixerBase::TRACK, android::AudioMixerBase::MAIN_BUFFER,
                     mix.out.data());
    mix.setParameter(name, android::AudioMixerBase::RESAMPLE, android::AudioMixerBase::SAMPLE_RATE,
                     as_value(rate));
    mix.set_provider(name, &track);
    return true;
}

void mix_remove(Mix& mix, int32_t name) {
    if (!mix.exists(name)) {
        return;
    }
    mix.disable(name);
    mix.destroy(name);
}

void mix_enable(Mix& mix, Track& track, int32_t name, bool on) {
    if (!on) {
        mix.disable(name);
        return;
    }
    track.proxy->start();
    // the client's volume (cblk mVolumeLR, gain_minifloat) as AudioFlinger reads it for a track
    const gain_minifloat_packed_t volume = track.proxy->getVolumeLR();
    float left = float_from_gain(gain_minifloat_unpack_left(volume));
    float right = float_from_gain(gain_minifloat_unpack_right(volume));
    mix.setParameter(name, android::AudioMixerBase::VOLUME, android::AudioMixerBase::VOLUME0, &left);
    mix.setParameter(name, android::AudioMixerBase::VOLUME, android::AudioMixerBase::VOLUME1, &right);
    mix.enable(name);
}

// AudioMixerBase::process() makes one quantum; the output takes it frame by frame, so a request of any size is served from it.
void mix_render(Mix& mix, rust::Slice<float> out) {
    const size_t frames = out.size() / kOutChannels;
    for (size_t done = 0; done < frames;) {
        if (mix.next == mix.quantum) {
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

}  // namespace atl::audio
