// SPDX-License-Identifier: MIT
//! AOSP's audio policy engine (libaudiopolicyenginedefault) behind [`PolicyEngine`]: the queries of IAudioPolicyService that atl-audioflinger's policy serves. Each query answers AOSP's status (0 = NO_ERROR) with its result; a list is the AIDL parcel of an int32 count and each parcelable (wrap/engine.cpp).
//! Attributes go in as the AudioAttributes parcelable's bytes, so the AIDL conversion is AOSP's. No strategy table or matcher lives here.

use cxx::UniquePtr;

use crate::bridge::ffi;

/// status_t NO_ERROR (utils/Errors.h).
const NO_ERROR: i32 = 0;

/// The engine, loaded with its default config. Sent between threads, never shared: the owner serialises the queries, as AOSP's service holds mMutex around them.
pub struct PolicyEngine {
    inner: UniquePtr<ffi::PolicyEngine>,
}

// SAFETY: the engine has no thread affinity and the queries only read it; the owner gives it to one caller at a time.
unsafe impl Send for PolicyEngine {}

impl PolicyEngine {
    /// The engine with its default config. Err: AOSP's load status.
    pub fn load() -> Result<Self, i32> {
        let inner = ffi::new_policy_engine();
        check(ffi::policy_engine_status(
            inner.as_ref().expect("new_policy_engine returns an engine"),
        ))?;
        Ok(Self { inner })
    }

    fn engine(&self) -> &ffi::PolicyEngine {
        self.inner
            .as_ref()
            .expect("new_policy_engine returns an engine")
    }

    /// listAudioProductStrategies: the parcel of the strategies.
    pub fn product_strategies(&self) -> Result<Vec<u8>, i32> {
        let mut parcel = Vec::new();
        check(ffi::list_product_strategies(self.engine(), &mut parcel))?;
        Ok(parcel)
    }

    /// getProductStrategyFromAudioAttributes: the strategy id of `attributes` (an AudioAttributes parcelable's bytes).
    pub fn product_strategy_for_attributes(
        &self,
        attributes: &[u8],
        fallback_on_default: bool,
    ) -> Result<i32, i32> {
        let mut strategy = 0;
        check(ffi::product_strategy_for_attributes(
            self.engine(),
            attributes,
            fallback_on_default,
            &mut strategy,
        ))?;
        Ok(strategy)
    }

    /// getStrategyForStream: the strategy id of `stream` (AudioStreamType's value).
    pub fn product_strategy_for_stream(&self, stream: i32) -> Result<i32, i32> {
        let mut strategy = 0;
        check(ffi::product_strategy_for_stream(
            self.engine(),
            stream,
            &mut strategy,
        ))?;
        Ok(strategy)
    }

    /// listAudioVolumeGroups: the parcel of the groups.
    pub fn volume_groups(&self) -> Result<Vec<u8>, i32> {
        let mut parcel = Vec::new();
        check(ffi::list_volume_groups(self.engine(), &mut parcel))?;
        Ok(parcel)
    }

    /// getVolumeGroupFromAudioAttributes: the group id of `attributes` (an AudioAttributes parcelable's bytes).
    pub fn volume_group_for_attributes(
        &self,
        attributes: &[u8],
        fallback_on_default: bool,
    ) -> Result<i32, i32> {
        let mut group = 0;
        check(ffi::volume_group_for_attributes(
            self.engine(),
            attributes,
            fallback_on_default,
            &mut group,
        ))?;
        Ok(group)
    }
}

/// AOSP's status as a result: NO_ERROR is Ok, anything else is the status.
fn check(status: i32) -> Result<(), i32> {
    if status == NO_ERROR {
        Ok(())
    } else {
        Err(status)
    }
}
