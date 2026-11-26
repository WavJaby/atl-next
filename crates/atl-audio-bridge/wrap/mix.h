// SPDX-License-Identifier: MIT
//! AOSP's server side of a track and its mixer, behind the cxx bridge in src/bridge.rs.
//! Track: AudioTrackServerProxy (streaming) or StaticAudioTrackServerProxy (shared buffer) over the control block Rust maps, read by the mixer through AudioBufferProvider (Tracks.cpp Track::getNextBuffer / releaseBuffer), and the track's VolumeHandler (Tracks.cpp:909).
//! Mix: AudioMixerBase at the output's quantum and rate, float mixing, stereo out. The mixer's state and its quantum loop are Rust's (src/lib.rs `Mix`); the functions here forward to AOSP.
//! AudioMixerBase has no setBufferProvider in this AOSP (AudioMixer, which has one, is not built: its BufferProviders need libaudiohal and external/sonic), so Mix sets its tracks' providers itself, as AudioMixer::setBufferProvider does.
//! Forward only: types are converted and calls forwarded; the protocol and the mix are AOSP's. getNextBuffer / releaseBuffer stay here: AOSP's process() calls them.
#pragma once

#include <cstddef>
#include <cstdint>
#include <memory>
#include <vector>

#include "media/AudioBufferProvider.h"
#include "media/AudioMixerBase.h"
#include "media/VolumeShaper.h"
#include "private/media/AudioTrackShared.h"
#include "rust/cxx.h"

namespace atl::audio {

/// One track's server side: the proxy over its control block and buffer, read in the track's format and handed to the mixer as float (Track::getNextBuffer).
struct Track final : public android::AudioBufferProvider {
    Track(android::sp<android::AudioTrackServerProxy> server, audio_format_t track_format, size_t channels_per_frame,
          bool shared_buffer, uint32_t rate);
    android::status_t getNextBuffer(Buffer* buffer) override;
    void releaseBuffer(Buffer* buffer) override;

    android::sp<android::AudioTrackServerProxy> proxy;
    /// The client's sample format (linear PCM, one AudioMixerBase::isValidFormat takes).
    audio_format_t format;
    /// Interleaved channels of the server buffer (1 or 2).
    size_t channels;
    /// The float frames the mixer reads: one quantum, sized by mix_add outside the realtime fill.
    std::vector<float> floats;
    /// A shared-buffer (static) track: StaticAudioTrackServerProxy over the client's buffer.
    bool is_static;
    /// The server buffer the last getNextBuffer obtained; releaseBuffer gives it back.
    void* server_raw = nullptr;
    /// A stopped streaming track still enabled until its written frames are mixed (AOSP STOPPED, still active). Rust sets it (track_set_draining); getNextBuffer reads it for the underrun tally.
    bool draining = false;
    /// The track's VolumeShapers (AOSP Track::mVolumeHandler, made at the track's rate: Tracks.cpp:909). Rust reads their gain each quantum (track_shaper_gain).
    android::sp<android::media::VolumeHandler> volume;
};

/// The mixer: AudioMixerBase at `quantum_frames` and `rate`. Its state is Rust's; `quantum` is what mix_add sizes a track's floats by.
struct Mix final : public android::AudioMixerBase {
    Mix(size_t quantum_frames, uint32_t rate);

    /// AudioMixer::setBufferProvider: the provider the mixer pulls track `name` through.
    void set_provider(int32_t name, android::AudioBufferProvider* provider);

    size_t quantum;
};

/// sizeof(audio_track_cblk_t) on x86_64 (LP64), the layout the Rust side maps: checked against AudioTrackShared.h at compile time (the static_assert in mix.cpp).
size_t cblk_bytes();
/// `cblk` addresses cblk_bytes() bytes; `buffer` the track's frames (`frames` of `frame_bytes`, samples in audio_format_t `format`): cblk + cblk_bytes() for a streaming track, the client's buffer for a static one.
std::unique_ptr<Track> new_track(uint8_t* cblk, uint8_t* buffer, uint32_t format, size_t frames, size_t frame_bytes, bool is_static,
                                 uint32_t rate);
std::unique_ptr<Mix> new_mix(size_t quantum_frames, uint32_t rate);
/// AudioMixerBase::create (the track's format, `channel_mask`, `session`) under track name `name`, with the track's buffer provider, float mixing, and `main` (one quantum of stereo floats, Rust's) as its main buffer. The track starts disabled. False when AOSP refuses it.
bool mix_add(Mix& mix, Track& track, int32_t name, int32_t session, uint32_t channel_mask, uint32_t rate, float* main);
/// AudioMixerBase::exists / destroy / enable / disable / process, and the track's volume parameters (VOLUME0 left, VOLUME1 right).
bool mix_exists(Mix& mix, int32_t name);
void mix_destroy(Mix& mix, int32_t name);
void mix_enable(Mix& mix, int32_t name);
void mix_disable(Mix& mix, int32_t name);
void mix_set_volume(Mix& mix, int32_t name, float left, float right);
void mix_process(Mix& mix);
/// AudioTrackServerProxy calls: start, flushBufferIfNeeded, framesReady (applies a pending flush first), framesFlushed, framesReleased.
void track_start(Track& track);
void track_flush_if_needed(Track& track);
uint64_t track_frames_ready(Track& track);
int64_t track_frames_flushed(Track& track);
int64_t track_frames_released(Track& track);
/// The client's volume, unpacked from the control block (getVolumeLR; float_from_gain of each side).
void track_client_gain(Track& track, float& left, float& right);
/// VolumeHandler::getVolume(frame).first: the track's shaper gain at `frame` (Threads.cpp:5860-5862).
float track_shaper_gain(Track& track, int64_t frame);
bool track_draining(Track& track);
void track_set_draining(Track& track, bool draining);
/// IAudioTrack.applyVolumeShaper on `track` (TrackHandle::applyVolumeShaper, Tracks.cpp:603-621): the configuration and operation as AIDL parcel bytes. Returns AOSP's status: a shaper id, or a negative status_t.
int32_t track_apply_volume_shaper(Track& track, rust::Slice<const uint8_t> configuration, rust::Slice<const uint8_t> operation);
/// IAudioTrack.getVolumeShaperState (VolumeHandler::getVolumeShaperState): false when shaper `id` is not on the track (AOSP's null).
bool track_volume_shaper_state(Track& track, int32_t id, float& volume, float& x_offset);
/// Frames the mixer took from tracks (getNextBuffer) since the last call; resets the count.
uint64_t take_track_frames();
/// Underruns the tracks' proxies counted (AudioTrackServerProxy::tallyUnderrunFrames: one per start of an underrun) since the last call; resets the count.
uint64_t take_underruns();
/// Frames the tracks' proxies tallied as underrun since the last call; resets the count.
uint64_t take_underrun_frames();
/// The underruns `track`'s client reads (the cblk's mUnderrunCount, AudioTrack::getUnderrunCount); 0 for a static track, whose cblk keeps none.
uint32_t track_underruns(const Track& track);

}  // namespace atl::audio
