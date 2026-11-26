// SPDX-License-Identifier: MIT
//! The volume-change seam (A23 slice 5): AudioService (atl-services, the system side) tells an app process's policy (atl-audioflinger) about each committed stream change, over the guest Binder seam.
//! `IAudioVolumeListener` (descriptor `atl.IAudioVolumeListener`) is the app's binder: VOLUME_COMMITTED (oneway) carries the written stream and the change's flags, the setStreamVolume / adjustStreamVolume flags as given.
//! `IAudioVolumeRegistry` (descriptor `atl.IAudioVolumeRegistry`, registered as `atl.audio_volume`: atl-services volume_listeners) is AudioService's: REGISTER hands in the calling process's listener; the calling (uid, pid) keys it.
//! No group id is on this seam: the policy maps the stream to AOSP's engine group (atl-audio-bridge), so the system side holds no group catalogue. The pinned IAudioVolumeChangeDispatcher carries a group, so it would need that catalogue here.

use binder::binder_impl::{Binder, BorrowedParcel, IBinderInternal, TransactionCode};
use binder::{Interface, StatusCode, Strong, declare_binder_interface};

/// The listener's interface descriptor.
pub const LISTENER_DESCRIPTOR: &str = "atl.IAudioVolumeListener";
/// The registry's interface descriptor.
pub const REGISTRY_DESCRIPTOR: &str = "atl.IAudioVolumeRegistry";
/// The written stream and the change's flags in (oneway: the change does not wait for the app); nothing out.
pub const VOLUME_COMMITTED: TransactionCode = 1;
/// The calling process's listener in; nothing out.
pub const REGISTER: TransactionCode = 1;

/// The app process's side: one call per committed change of the stream the app wrote.
pub trait IAudioVolumeListener: Interface {
    /// `stream` changed its step or mute on a write of the caller's own volumes, with `flags` as the write gave them.
    fn volume_committed(&self, stream: i32, flags: i32) -> Result<(), StatusCode>;
}

declare_binder_interface! {
    IAudioVolumeListener[LISTENER_DESCRIPTOR] {
        native: BnAudioVolumeListener(on_listener),
        proxy: BpAudioVolumeListener,
    }
}

fn on_listener(
    service: &dyn IAudioVolumeListener,
    code: TransactionCode,
    data: &BorrowedParcel<'_>,
    _reply: &mut BorrowedParcel<'_>,
) -> Result<(), StatusCode> {
    match code {
        VOLUME_COMMITTED => {
            let stream: i32 = data.read()?;
            let flags: i32 = data.read()?;
            service.volume_committed(stream, flags)
        }
        _ => Err(StatusCode::UNKNOWN_TRANSACTION),
    }
}

impl IAudioVolumeListener for BpAudioVolumeListener {
    fn volume_committed(&self, stream: i32, flags: i32) -> Result<(), StatusCode> {
        let binder = self.as_binder();
        let mut data = binder.prepare_transact()?;
        data.write(&stream)?;
        data.write(&flags)?;
        binder.submit_transact(VOLUME_COMMITTED, data, binder::binder_impl::FLAG_ONEWAY)?;
        Ok(())
    }
}

impl IAudioVolumeListener for Binder<BnAudioVolumeListener> {
    fn volume_committed(&self, stream: i32, flags: i32) -> Result<(), StatusCode> {
        self.0.volume_committed(stream, flags)
    }
}

/// AudioService's side: the registry the app processes register their listeners with.
pub trait IAudioVolumeRegistry: Interface {
    /// The calling process's listener. A process that registered already keeps its first listener (AudioPolicyService.registerClient); a dead listener's process is dropped.
    fn register(&self, listener: Strong<dyn IAudioVolumeListener>) -> Result<(), StatusCode>;
}

declare_binder_interface! {
    IAudioVolumeRegistry[REGISTRY_DESCRIPTOR] {
        native: BnAudioVolumeRegistry(on_registry),
        proxy: BpAudioVolumeRegistry,
    }
}

fn on_registry(
    service: &dyn IAudioVolumeRegistry,
    code: TransactionCode,
    data: &BorrowedParcel<'_>,
    _reply: &mut BorrowedParcel<'_>,
) -> Result<(), StatusCode> {
    match code {
        REGISTER => {
            let listener: Strong<dyn IAudioVolumeListener> = data.read()?;
            service.register(listener)
        }
        _ => Err(StatusCode::UNKNOWN_TRANSACTION),
    }
}

impl IAudioVolumeRegistry for BpAudioVolumeRegistry {
    fn register(&self, listener: Strong<dyn IAudioVolumeListener>) -> Result<(), StatusCode> {
        let binder = self.as_binder();
        let mut data = binder.prepare_transact()?;
        data.write(&listener)?;
        binder.submit_transact(REGISTER, data, 0)?;
        Ok(())
    }
}

impl IAudioVolumeRegistry for Binder<BnAudioVolumeRegistry> {
    fn register(&self, listener: Strong<dyn IAudioVolumeListener>) -> Result<(), StatusCode> {
        self.0.register(listener)
    }
}
