// SPDX-License-Identifier: MIT
//! The app volume's link to AudioService: this process's host stream is STREAM_MUSIC's absolute-volume device (AOSP: AudioDeviceVolumeManager.setDeviceAbsoluteVolumeBehavior, the controller of
//! an HDMI / AVRCP device). [`register`] names the device with the controller's current index (registerDeviceVolumeDispatcherForAbsoluteVolume, behaviour ABSOLUTE, adjustments left to
//! AudioService); AudioService's dispatchDeviceVolumeChanged sets the app volume, which the output's thread writes to the stream. Host changes go back as setStreamVolume /
//! adjustStreamVolume(ADJUST_MUTE / ADJUST_UNMUTE) with FLAG_ABSOLUTE_VOLUME, so AudioService updates and broadcasts without dispatching back. Those calls run on the reporter thread, never
//! on the PipeWire connection thread that observes the change.

use std::sync::mpsc::Receiver;
use std::thread;

use atl_aidl::aidl::android::media::IAudioDeviceVolumeDispatcher::{
    BnAudioDeviceVolumeDispatcher, IAudioDeviceVolumeDispatcher,
};
use atl_aidl::aidl::android::media::IAudioService::IAudioService;
use atl_parcelables::android::media::{AudioDeviceAttributes, VolumeInfo};
use atl_services_wire::audio_volume::{
    ADJUST_MUTE, ADJUST_UNMUTE, DEVICE_OUT_SPEAKER, DEVICE_VOLUME_BEHAVIOR_ABSOLUTE,
    FLAG_ABSOLUTE_VOLUME, ROLE_OUTPUT, STREAM_MUSIC, TYPE_BUILTIN_SPEAKER,
};
use binder::{BinderFeatures, Interface, Strong};

use crate::output::Opener;
use crate::volume::Report;

/// Context.AUDIO_SERVICE: the name AudioService is registered under.
const AUDIO_SERVICE: &str = "audio";
const INTERFACE: &str = "AppVolume";

/// The device STREAM_MUSIC plays to here (atl-services-wire): the speaker role this process's host stream takes.
fn device() -> AudioDeviceAttributes {
    AudioDeviceAttributes {
        role: ROLE_OUTPUT,
        device_type: TYPE_BUILTIN_SPEAKER,
        native_type: DEVICE_OUT_SPEAKER,
        ..AudioDeviceAttributes::default()
    }
}

/// AudioService's side: its dispatches set the app volume.
struct Dispatcher {
    opener: Opener,
    max: u32,
}

impl Interface for Dispatcher {}

impl IAudioDeviceVolumeDispatcher for Dispatcher {
    /// The index arrives in the device's range, which [`register`] declared as 0..=max, and the mute as its mute command; the output's thread writes both to the stream.
    fn r#dispatchDeviceVolumeChanged(
        &self,
        _device: Option<&AudioDeviceAttributes>,
        vol: Option<&VolumeInfo>,
    ) -> binder::Result<()> {
        let Some(vol) = vol else {
            atl_diag::refused(INTERFACE, "dispatchDeviceVolumeChanged (null VolumeInfo)");
            return Ok(());
        };
        let index = u32::try_from(vol.index).unwrap_or(0).min(self.max);
        if let Err(e) = self
            .opener
            .set_volume(index, vol.has_mute_command && vol.muted)
        {
            tracing::warn!(error = %e, "app volume not set");
        }
        Ok(())
    }
}

/// Registers this process's host stream with AudioService as STREAM_MUSIC's absolute-volume device at `current` (index, mute) of `max`, and starts the reporter that sends `reports` on.
/// A missing AudioService or a refused registration is counted (`refused AppVolume.register`) and leaves the volume app-local: the stream still plays, AudioService keeps its own index.
pub(crate) fn register(
    opener: Opener,
    current: (u32, bool),
    max: u32,
    reports: Receiver<Report>,
) -> Result<(), String> {
    let service: Strong<dyn IAudioService> = match binder::check_interface(AUDIO_SERVICE) {
        Ok(s) => s,
        Err(e) => {
            atl_diag::refused(INTERFACE, "register");
            tracing::warn!(error = %e, "no AudioService: the app volume stays local");
            return Ok(());
        }
    };
    let to_i32 = |v: u32| i32::try_from(v).map_err(|e| e.to_string());
    let volume = VolumeInfo {
        stream_type: STREAM_MUSIC,
        has_mute_command: false,
        muted: false,
        index: to_i32(current.0)?,
        min_index: 0,
        max_index: to_i32(max)?,
    };
    let dispatcher = BnAudioDeviceVolumeDispatcher::new_binder(
        Dispatcher { opener, max },
        BinderFeatures::default(),
    );
    if let Err(e) = service.r#registerDeviceVolumeDispatcherForAbsoluteVolume(
        true,
        Some(&dispatcher),
        None,
        Some(&device()),
        Some(&[Some(volume)]),
        false,
        DEVICE_VOLUME_BEHAVIOR_ABSOLUTE,
    ) {
        atl_diag::refused(INTERFACE, "register");
        tracing::warn!(error = %e, "app volume not registered: it stays local");
        return Ok(());
    }
    thread::Builder::new()
        .name("atl-app-volume".to_owned())
        .spawn(move || report(&service, &reports))
        .map_err(|e| format!("starting the app volume's reporter: {e}"))?;
    Ok(())
}

/// The reporter thread: each host change as the calls [`Report::calls`] names, flagged as the device's own. A failed call is counted (`refused AppVolume.report`).
fn report(service: &Strong<dyn IAudioService>, reports: &Receiver<Report>) {
    for r in reports {
        let (index, mute) = r.calls();
        let mut sent = Ok(());
        if let Some(i) = index {
            sent = service.r#setStreamVolume(
                STREAM_MUSIC,
                i32::try_from(i).unwrap_or(i32::MAX),
                FLAG_ABSOLUTE_VOLUME,
                None,
            );
        }
        if let (Ok(()), Some(m)) = (&sent, mute) {
            let direction = if m { ADJUST_MUTE } else { ADJUST_UNMUTE };
            sent =
                service.r#adjustStreamVolume(STREAM_MUSIC, direction, FLAG_ABSOLUTE_VOLUME, None);
        }
        if let Err(e) = sent {
            atl_diag::refused(INTERFACE, "report");
            tracing::warn!(error = %e, "host volume change not reported to AudioService");
        }
    }
}
