// SPDX-License-Identifier: MIT
//! The `audio` service (Context.AUDIO_SERVICE): the converter's IAudioService stub. View.performClick reaches it on every click: ViewRootImpl.playSoundEffect -> getAudioManager ->
//! AudioManager.areNavigationRepeatSoundEffectsEnabled, which throws NullPointerException without the service (AudioManager.getService does not check the lookup).
//! ATL has no audio policy yet, so the two click calls are answered and no sound plays; every other method stays counted and answered UNKNOWN_TRANSACTION.

use atl_aidl::aidl::android::media::IAudioService::{
    BnAudioService, EnforcePermission, IAudioService,
};
use binder::{BinderFeatures, ExceptionCode, Interface, SpIBinder, Status, ThreadState};

use crate::counters;
use crate::permission::Permissions;
use std::sync::Arc;

/// The interface name counters use.
const INTERFACE: &str = "IAudioService";

struct AudioService {
    permissions: Arc<Permissions>,
}

impl Interface for AudioService {}

impl IAudioService for AudioService {
    /// @EnforcePermission: root / system, or a caller whose install-time grants hold the permissions (Permissions::holds, rule A).
    fn r#enforce_permission(
        &self,
        method: &str,
        permission: EnforcePermission<'_>,
    ) -> binder::Result<()> {
        let (perms, any): (&[&str], bool) = match &permission {
            EnforcePermission::Single(p) => (std::slice::from_ref(p), false),
            EnforcePermission::AllOf(p) => (p, false),
            EnforcePermission::AnyOf(p) => (p, true),
        };
        if self
            .permissions
            .holds(ThreadState::get_calling_uid(), perms, any)
        {
            return Ok(());
        }
        Err(Status::new_exception_str(
            ExceptionCode::SECURITY,
            Some(format!("{method}: caller lacks {permission:?}")),
        ))
    }

    fn r#unimplemented(&self, method: &'static str) -> Status {
        counters::unimplemented(INTERFACE, method)
    }

    /// Repeat sounds for navigation (fast scroll) stay off: there is no sound effect player.
    fn r#areNavigationRepeatSoundEffectsEnabled(&self) -> binder::Result<bool> {
        Ok(false)
    }

    /// The click sound: nothing plays yet. Counted, so a run shows the call.
    fn r#playSoundEffect(&self, _effect_type: i32, _user_id: i32) -> binder::Result<()> {
        counters::count(INTERFACE, "playSoundEffect");
        Ok(())
    }
}

/// The service's binder, for `addService`.
pub fn new_binder(permissions: Arc<Permissions>) -> SpIBinder {
    BnAudioService::new_binder(AudioService { permissions }, BinderFeatures::default()).as_binder()
}
