// SPDX-License-Identifier: MIT
//! AudioFlinger and AudioPolicyService in the app process (game audio A1a, A1b, A2): the two binders AOSP's audio clients look up by name. They answer the calls that give ids, the output's rate, frame count and latency, and the routing to the output; createTrack hands out a track (`track.rs`) with its control block and an IAudioTrack; the tracks are mixed into one PCM output (`output.rs`), whose granted rate and quantum are the served profile.
//! The interfaces are AOSP's AIDL (libaudioclient `aidl/`), compiled by the converter into `atl_audioflinger_stubs`. Calls not answered here stay UNKNOWN_TRANSACTION and are counted (atl-services counters).
//! Served by [`serve`] at launch, after `atl_service_view::install` (atl-app-launcher). Core crate: `unsafe` nowhere.
#![forbid(unsafe_code)]
#![cfg(feature = "converter")]

// the converter's rlibs, on the crate search path build.rs sets (no cargo dependency names them)
extern crate android_media_audio_common_types;
extern crate atl_audioflinger_stubs;
extern crate audioclient_types_aidl;
extern crate binder;
extern crate framework_permission_aidl;
extern crate shared_file_region_aidl;

mod flinger;
mod ids;
mod output;
mod policy;
mod track;

/// The name AudioSystem looks AudioFlinger up by (`checkService`).
pub const FLINGER_NAME: &str = "media.audio_flinger";
/// The name AudioSystem looks AudioPolicyService up by.
pub const POLICY_NAME: &str = "media.audio_policy";
/// The output's channel layout mask: `AUDIO_CHANNEL_OUT_STEREO` (front left | front right, system/media/audio/include/system/audio-hal-enums.h).
pub const OUTPUT_CHANNEL_MASK: i32 = 0x3;

/// Test switches of [`serve`]. The default serves normally.
#[derive(Clone, Copy, Debug, Default)]
pub struct Options {
    /// The audio gate's break (`--break-cblk`): each streaming track's region is one buffer short of its layout, so createTrack's size check refuses it and the track fails to initialise.
    pub break_cblk: bool,
    /// The audio gate's break (`--break-mix`): the output plays silence and the mixer does not run, so no track's sound reaches the sink.
    pub break_mix: bool,
}

/// The served PCM output. Hold it for the life of the process: dropping it stops the stream.
pub struct Served {
    _output: output::Output,
}

/// Opens the PCM output, starts it, and serves [`FLINGER_NAME`] and [`POLICY_NAME`] to this process. Refused before `atl_service_view::install`. The launcher skips the call under `--no-audio-service` (the audio gate's break).
pub fn serve(opts: Options) -> Result<Served, String> {
    let (output, profile, mixer) = output::open(opts)?;
    atl_service_view::serve(FLINGER_NAME, flinger::new_binder(opts, profile, mixer))?;
    atl_service_view::serve(POLICY_NAME, policy::new_binder())?;
    Ok(Served { _output: output })
}
