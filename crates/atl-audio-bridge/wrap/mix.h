// SPDX-License-Identifier: MIT
//! AOSP's server side of a track and its mixer, behind the cxx bridge in src/bridge.rs.
//! Track: AudioTrackServerProxy (streaming) or StaticAudioTrackServerProxy (shared buffer) over the control block Rust maps, read by the mixer through AudioBufferProvider (Tracks.cpp Track::getNextBuffer / releaseBuffer).
//! Mix: AudioMixerBase at the output's quantum and rate, float mixing, stereo out. AudioMixerBase has no setBufferProvider in this AOSP (AudioMixer, which has one, is not built: its BufferProviders need libaudiohal and external/sonic), so Mix sets its tracks' providers itself, as AudioMixer::setBufferProvider does.
//! Forward only: types are converted and calls forwarded; the protocol and the mix are AOSP's.
#pragma once

#include <cstddef>
#include <cstdint>
#include <map>
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
    /// A stopped streaming track still enabled until its written frames are mixed (AOSP STOPPED, still active); the fill ends the drain. No underrun is tallied while it drains.
    bool draining = false;
    /// The track's VolumeShapers (AOSP Track::mVolumeHandler, made at the track's rate: Tracks.cpp:909). applyVolumeShaper sets them; apply_volume reads their gain each quantum.
    android::sp<android::media::VolumeHandler> volume;
};

/// The mixer: AudioMixerBase at `quantum_frames` and `rate`. `out` holds one quantum of mixed stereo float; `next` is the first frame of it not yet handed out.
struct Mix final : public android::AudioMixerBase {
    Mix(size_t quantum_frames, uint32_t rate);

    /// AudioMixer::setBufferProvider: the provider the mixer pulls track `name` through.
    void set_provider(int32_t name, android::AudioBufferProvider* provider);

    size_t quantum;
    std::vector<float> out;
    size_t next = 0;
    /// The enabled tracks by name (draining ones included): each quantum reads their client volume, as AudioFlinger's prepareTracks does every cycle. Control calls add and remove; the fill only removes (a finished drain), which frees and never allocates.
    std::map<int32_t, Track*> enabled;
};

/// sizeof(audio_track_cblk_t) on x86_64 (LP64), the layout the Rust side maps: checked against AudioTrackShared.h at compile time (the static_assert in mix.cpp).
size_t cblk_bytes();
/// `cblk` addresses cblk_bytes() bytes; `buffer` the track's frames (`frames` of `frame_bytes`, samples in audio_format_t `format`): cblk + cblk_bytes() for a streaming track, the client's buffer for a static one.
std::unique_ptr<Track> new_track(uint8_t* cblk, uint8_t* buffer, uint32_t format, size_t frames, size_t frame_bytes, bool is_static,
                                 uint32_t rate);
std::unique_ptr<Mix> new_mix(size_t quantum_frames, uint32_t rate);
/// AudioMixerBase::create (the track's format, `channel_mask`, `session`) under track name `name`, with the track's buffer provider and float mixing; the track starts disabled. False when AOSP refuses it.
bool mix_add(Mix& mix, Track& track, int32_t name, int32_t session, uint32_t channel_mask, uint32_t rate);
/// Removes track `name` from the mixer. Returns the frames the track will never play: unread (AudioTrackServerProxy::framesReady, which first applies a pending client flush) plus discarded by flushes (ServerProxy::framesFlushed).
uint64_t mix_remove(Mix& mix, Track& track, int32_t name);
/// IAudioTrack.start: starts the server side (AudioTrackServerProxy::start), follows the client's volume and enables the track; a draining track plays on.
void mix_start(Mix& mix, Track& track, int32_t name);
/// IAudioTrack.pause: disables the track; its unread frames stay.
void mix_pause(Mix& mix, int32_t name);
/// IAudioTrack.stop: a streaming track plays out what the client wrote before its stop, then leaves the mix; a static one leaves at once.
void mix_stop(Mix& mix, Track& track, int32_t name);
/// IAudioTrack.flush: discards the unread frames of a track the mixer is not reading now; a draining one ends at its next quantum.
void mix_flush(Mix& mix, Track& track, int32_t name);
/// IAudioTrack.applyVolumeShaper on `track` (TrackHandle::applyVolumeShaper, Tracks.cpp:603-621): the configuration and operation as AIDL parcel bytes. Returns AOSP's status: a shaper id, or a negative status_t.
int32_t track_apply_volume_shaper(Track& track, rust::Slice<const uint8_t> configuration, rust::Slice<const uint8_t> operation);
/// Fills `out` (interleaved stereo) with the next frames of the mix, one quantum at a time.
void mix_render(Mix& mix, rust::Slice<float> out);
/// Frames the mixer took from tracks (getNextBuffer) since the last call; resets the count.
uint64_t take_track_frames();
/// Enabled tracks whose client volume was zero in the last quantum.
uint32_t muted_tracks();
/// Tracks enabled in the mix, draining ones included.
uint32_t active_tracks();
/// Underruns the tracks' proxies counted (AudioTrackServerProxy::tallyUnderrunFrames: one per start of an underrun) since the last call; resets the count.
uint64_t take_underruns();
/// Frames the tracks' proxies tallied as underrun since the last call; resets the count.
uint64_t take_underrun_frames();
/// The underruns `track`'s client reads (the cblk's mUnderrunCount, AudioTrack::getUnderrunCount); 0 for a static track, whose cblk keeps none.
uint32_t track_underruns(const Track& track);

}  // namespace atl::audio
