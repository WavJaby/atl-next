// SPDX-License-Identifier: MIT
//! android.content.pm.ParceledListSlice: content/pm/BaseParceledListSlice.java writeToParcel (:172-195). Wire: int32 N; for N > 0 the element class name (writeParcelableCreator: writeString of the class name),
//! then N times int32 1 and the element's writeToParcel. ParceledListSlice never splits a list (mInlineCountLimit is Integer.MAX_VALUE), but BaseParceledListSlice's size limit (:185) would; a list whose
//! elements outgrow `IBinder.getSuggestedMaxIpcSizeBytes()` is refused here rather than half-written.
//! Only what the services return so far is modelled: a list of InstrumentationInfo (IPackageManager.queryInstrumentationAsUser). The Java class is generic and AIDL names it raw, so one Rust type stands for every
//! element class; another element kind is another field here. Reading takes the inline form of the same kind.

use super::InstrumentationInfo::InstrumentationInfo;
use crate::parcel::*;
use crate::parts::*;

/// `IBinder.getSuggestedMaxIpcSizeBytes()`: 64 KiB (IBinder.java).
const MAX_IPC_SIZE: i32 = 64 * 1024;
/// The class name `writeParcelableCreator` writes for the elements.
const INSTRUMENTATION_INFO: &str = "android.content.pm.InstrumentationInfo";

/// AIDL names it raw (`ParceledListSlice`) and typed (`ParceledListSlice<ApplicationStartInfo>`) alike, so the type parameter is only a marker (default `()`, as `opaque!` does for the generic ones): the elements are
/// whatever field below is set. Impls are written out because derives would bound `T`.
pub struct ParceledListSlice<T = ()> {
    /// The elements, when they are InstrumentationInfos.
    pub instrumentation: Vec<InstrumentationInfo>,
    _element: std::marker::PhantomData<fn() -> T>,
}

impl<T> ParceledListSlice<T> {
    pub fn new(instrumentation: Vec<InstrumentationInfo>) -> Self {
        ParceledListSlice {
            instrumentation,
            _element: std::marker::PhantomData,
        }
    }
}

impl<T> Default for ParceledListSlice<T> {
    fn default() -> Self {
        Self::new(Vec::new())
    }
}

impl<T> Clone for ParceledListSlice<T> {
    fn clone(&self) -> Self {
        Self::new(self.instrumentation.clone())
    }
}

impl<T> PartialEq for ParceledListSlice<T> {
    fn eq(&self, other: &Self) -> bool {
        self.instrumentation == other.instrumentation
    }
}

impl<T> std::fmt::Debug for ParceledListSlice<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ParceledListSlice")
            .field("instrumentation", &self.instrumentation)
            .finish()
    }
}

impl<T> Part for ParceledListSlice<T> {
    fn write(&self, p: &mut In<'_>) -> Result<()> {
        w_i32(p, len_i32(self.instrumentation.len())?)?;
        if self.instrumentation.is_empty() {
            return Ok(());
        }
        w_str16(p, &Some(INSTRUMENTATION_INFO.to_owned()))?;
        for item in &self.instrumentation {
            w_i32(p, 1)?;
            item.write(p)?;
        }
        if p.get_data_size() > MAX_IPC_SIZE {
            return Err(crate::refuse("android.content.pm.ParceledListSlice (size)"));
        }
        Ok(())
    }

    /// The inline form only: every element follows (the int32 1 before each). A list the sender split (an int32 0 and a binder to fetch the rest) or of another element class is refused.
    fn read(p: &In<'_>) -> Result<Self> {
        let n = r_i32(p)?;
        if n <= 0 {
            return Ok(ParceledListSlice::default());
        }
        if r_str16(p)?.as_deref() != Some(INSTRUMENTATION_INFO) {
            return Err(crate::refuse(
                "android.content.pm.ParceledListSlice (element class)",
            ));
        }
        let instrumentation = (0..n)
            .map(|_| match r_i32(p)? {
                1 => InstrumentationInfo::read(p),
                _ => Err(crate::refuse(
                    "android.content.pm.ParceledListSlice (split list)",
                )),
            })
            .collect::<Result<_>>()?;
        Ok(ParceledListSlice::new(instrumentation))
    }
}

// crate::codec! for a generic type (its names are those of the macro's non-generic arm)
const _: () = {
    use binder::StatusCode;
    use std::option::Option;
    impl<T> binder::binder_impl::UnstructuredParcelable for ParceledListSlice<T> {
        fn write_to_parcel(
            &self,
            p: &mut binder::binder_impl::BorrowedParcel<'_>,
        ) -> std::result::Result<(), StatusCode> {
            Part::write(self, p)
        }
        fn from_parcel(
            p: &binder::binder_impl::BorrowedParcel<'_>,
        ) -> std::result::Result<Self, StatusCode> {
            Part::read(p)
        }
    }
    binder::impl_serialize_for_unstructured_parcelable!(ParceledListSlice<T>);
    binder::impl_deserialize_for_unstructured_parcelable!(ParceledListSlice<T>);
};
