// SPDX-License-Identifier: MIT
//! AudioFlinger and AudioPolicyService in the app process (game audio A1a, A1b, A2): the two binders AOSP's audio clients look up by name. They answer the calls that give ids, the output's rate, frame count and latency, and the routing to the output; createTrack hands out a track (`track.rs`) with its control block and an IAudioTrack; the tracks are mixed into one PCM output (`output.rs`), whose granted rate and quantum the served profile is.
//! The interfaces are AOSP's AIDL (libaudioclient `aidl/`), compiled by the converter into `atl_audioflinger_stubs`. Calls not answered here stay UNKNOWN_TRANSACTION and are counted (atl-services counters).
//! Served by [`serve`] at launch, after `atl_service_view::install` (atl-app-launcher). The PCM output opens on its first use, so an unreachable audio server fails tracks, never the launch. Core crate: `unsafe` nowhere.
#![forbid(unsafe_code)]
#![cfg(feature = "converter")]

// the converter's rlibs, on the crate search path build.rs sets (no cargo dependency names them)
extern crate android_media_audio_common_types;
extern crate atl_audioflinger_stubs;
extern crate atl_diag;
extern crate atl_parcelables;
extern crate audioclient_types_aidl;
extern crate binder;
extern crate framework_permission_aidl;
extern crate shared_file_region_aidl;

mod dispatcher;
mod flinger;
mod ids;
mod output;
mod policy;
mod track;
mod volume;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use atl_services_wire::audio_volume::{MAX_STREAM_VOLUME, STREAM_MUSIC};

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

/// Serves [`FLINGER_NAME`] and [`POLICY_NAME`] to this process, with the PCM output connecting to the audio server socket `remote` (granted by atld; None: none granted) on its first use. Refused before `atl_service_view::install`. The launcher skips the call under `--no-audio-service` (the audio gate's break).
/// `identity`: the app the output's stream belongs to (the desktop's mixer row and saved-volume key); None: a process of no app. The stream's host volume is the app volume (volume.rs), counted in AOSP's music steps
/// and registered with AudioService as STREAM_MUSIC's absolute-volume device (dispatcher.rs), so the app's AudioManager calls drive it and the host's changes reach AudioService.
/// Fails only when the services cannot be served: a server that is unreachable is not an error here, and its tracks fail when made (output.rs).
pub fn serve(
    remote: Option<&Path>,
    identity: Option<atl_platform_ui::audio::StreamIdentity<'_>>,
    opts: Options,
) -> Result<(), String> {
    let target = output::Target {
        remote: remote.map(PathBuf::from),
        identity: identity.map(|i| i.id.to_owned()),
    };
    let music = usize::try_from(STREAM_MUSIC).map_err(|e| e.to_string())?;
    let max = u32::try_from(MAX_STREAM_VOLUME[music]).map_err(|e| e.to_string())?;
    let volume = Arc::new(volume::AppVolume::new(max));
    let (reports, reported) = std::sync::mpsc::channel();
    let output = output::Opener::spawn(target, opts, Arc::clone(&volume), reports)?;
    dispatcher::register(output.clone(), volume.current(), max, reported)?;
    atl_service_view::serve(FLINGER_NAME, flinger::new_binder(opts, output))?;
    atl_service_view::serve(POLICY_NAME, policy::new_binder())?;
    Ok(())
}
