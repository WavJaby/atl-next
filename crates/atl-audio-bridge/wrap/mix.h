// SPDX-License-Identifier: MIT
//! AOSP's server side of a track and its mixer, behind the cxx bridge in src/bridge.rs.
//! Track: AudioTrackServerProxy (streaming) or StaticAudioTrackServerProxy (shared buffer) over the control block Rust maps, read by the mixer through AudioBufferProvider (Tracks.cpp Track::getNextBuffer / releaseBuffer).
//! Mix: AudioMixerBase at the output's quantum and rate, float mixing, stereo out. AudioMixerBase has no setBufferProvider in this AOSP (AudioMixer, which has one, is not built: its BufferProviders need libaudiohal and external/sonic), so Mix sets its tracks' providers itself, as AudioMixer::setBufferProvider does.
//! Forward only: types are converted and calls forwarded; the protocol and the mix are AOSP's.
#pragma once

#include <cstddef>
#include <cstdint>
#include <memory>
#include <vector>

#include "media/AudioBufferProvider.h"
#include "media/AudioMixerBase.h"
#include "private/media/AudioTrackShared.h"
#include "rust/cxx.h"

namespace atl::audio {

/// One track's server side: the proxy over its control block and buffer, read as PCM 16-bit and handed to the mixer as float (Track::getNextBuffer).
struct Track final : public android::AudioBufferProvider {
    Track(android::sp<android::AudioTrackServerProxy> server, size_t channels_per_frame);
    android::status_t getNextBuffer(Buffer* buffer) override;
    void releaseBuffer(Buffer* buffer) override;

    android::sp<android::AudioTrackServerProxy> proxy;
    /// Interleaved channels of the server buffer (1 or 2).
    size_t channels;
    /// The float frames the mixer reads: one quantum, sized by mix_add outside the realtime fill.
    std::vector<float> floats;
    /// The server buffer the last getNextBuffer obtained; releaseBuffer gives it back.
    void* server_raw = nullptr;
};

/// The mixer: AudioMixerBase at `quantum_frames` and `rate`. `out` holds one quantum of mixed stereo float; `next` is the first frame of it not yet handed out.
struct Mix final : public android::AudioMixerBase {
    Mix(size_t quantum_frames, uint32_t rate);

    /// AudioMixer::setBufferProvider: the provider the mixer pulls track `name` through.
    void set_provider(int32_t name, android::AudioBufferProvider* provider);

    size_t quantum;
    std::vector<float> out;
    size_t next = 0;
};

/// sizeof(audio_track_cblk_t) on x86_64 (LP64), the layout the Rust side maps: checked against AudioTrackShared.h at compile time (the static_assert in mix.cpp).
size_t cblk_bytes();
/// `cblk` addresses cblk_bytes() bytes; `buffer` the track's frames (`frames` of `frame_bytes`): cblk + cblk_bytes() for a streaming track, the client's buffer for a static one.
std::unique_ptr<Track> new_track(uint8_t* cblk, uint8_t* buffer, size_t frames, size_t frame_bytes, bool is_static, uint32_t rate);
std::unique_ptr<Mix> new_mix(size_t quantum_frames, uint32_t rate);
/// AudioMixerBase::create (PCM 16-bit, `channel_mask`, `session`) under track name `name`, with the track's buffer provider and float mixing; the track starts disabled. False when AOSP refuses it.
bool mix_add(Mix& mix, Track& track, int32_t name, int32_t session, uint32_t channel_mask, uint32_t rate);
void mix_remove(Mix& mix, int32_t name);
/// On: starts the server side (AudioTrackServerProxy::start), follows the client's volume and enables the track. Off: disables it.
void mix_enable(Mix& mix, Track& track, int32_t name, bool on);
/// Fills `out` (interleaved stereo) with the next frames of the mix, one quantum at a time.
void mix_render(Mix& mix, rust::Slice<float> out);

}  // namespace atl::audio
