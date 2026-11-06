// SPDX-License-Identifier: MIT
//! com.android.internal.infra.AndroidFuture (core/java/com/android/internal/infra/AndroidFuture.java writeToParcel:513-546, AndroidFuture(Parcel):92-108): a future that crosses binder.
//! Wire: bool done; not done -> strong binder of an IAndroidFuture the receiver completes (oneway complete(AndroidFuture done-form)); done -> bool failed; failed -> writeThrowable (:560-589);
//! else Parcel.writeValue(result): int32 type, and for VAL_PARCELABLE an int32 length prefix, the class name String16 (writeParcelable) and the parcelable's writeToParcel.
//! Modelled: pending with its binder; done with null or a SurroundingText (IRemoteInputConnection.getSurroundingText, atl-ime). A failed future or another value type is refused: a re-encoded
//! throwable carries a new stack trace, so no truth case could check it. AIDL names the class raw, so `T` is only a marker (as ParceledListSlice).

use binder::StatusCode;

use crate::codecs::android::view::inputmethod::SurroundingText::SurroundingText;
use crate::parcel::*;
use crate::parts::*;

/// Parcel.VAL_NULL / VAL_PARCELABLE (Parcel.java:317, :322); VAL_PARCELABLE is length-prefixed (isLengthPrefixed).
const VAL_NULL: i32 = -1;
const VAL_PARCELABLE: i32 = 4;
/// The class name writeParcelable writes before a SurroundingText.
const SURROUNDING_TEXT: &str = "android.view.inputmethod.SurroundingText";

/// A completed future's value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FutureValue {
    Null,
    SurroundingText(SurroundingText),
}

#[derive(Debug, Clone, PartialEq)]
pub enum FutureState {
    /// Not done: the sender's IAndroidFuture, completed by the receiver.
    Pending(Option<binder::SpIBinder>),
    Done(FutureValue),
}

pub struct AndroidFuture<T = ()> {
    pub state: FutureState,
    _value: std::marker::PhantomData<fn() -> T>,
}

impl<T> AndroidFuture<T> {
    pub fn new(state: FutureState) -> Self {
        AndroidFuture {
            state,
            _value: std::marker::PhantomData,
        }
    }
}

impl<T> Default for AndroidFuture<T> {
    fn default() -> Self {
        Self::new(FutureState::Pending(None))
    }
}

impl<T> Clone for AndroidFuture<T> {
    fn clone(&self) -> Self {
        Self::new(self.state.clone())
    }
}

impl<T> PartialEq for AndroidFuture<T> {
    fn eq(&self, other: &Self) -> bool {
        self.state == other.state
    }
}

impl<T> std::fmt::Debug for AndroidFuture<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AndroidFuture")
            .field("state", &self.state)
            .finish()
    }
}

impl<T> Part for AndroidFuture<T> {
    fn write(&self, p: &mut In<'_>) -> Result<()> {
        match &self.state {
            FutureState::Pending(binder) => {
                w_bool(p, false)?;
                w_binder(p, binder)
            }
            FutureState::Done(value) => {
                w_bool(p, true)?;
                w_bool(p, false)?;
                match value {
                    FutureValue::Null => w_i32(p, VAL_NULL),
                    FutureValue::SurroundingText(t) => {
                        w_i32(p, VAL_PARCELABLE)?;
                        let size_at = p.get_data_position();
                        w_i32(p, 0)?;
                        let start = p.get_data_position();
                        w_str16(p, &Some(SURROUNDING_TEXT.to_owned()))?;
                        t.write(p)?;
                        let end = p.get_data_position();
                        p.set_data_position_checked(size_at)?;
                        w_i32(p, end - start)?;
                        p.set_data_position_checked(end)
                    }
                }
            }
        }
    }

    fn read(p: &In<'_>) -> Result<Self> {
        if !r_bool(p)? {
            return Ok(Self::new(FutureState::Pending(r_binder(p)?)));
        }
        if r_bool(p)? {
            return Err(crate::refuse(
                "com.android.internal.infra.AndroidFuture (failed)",
            ));
        }
        let value = match r_i32(p)? {
            VAL_NULL => FutureValue::Null,
            VAL_PARCELABLE => {
                let size = r_i32(p)?;
                let start = p.get_data_position();
                if r_str16(p)?.as_deref() != Some(SURROUNDING_TEXT) {
                    return Err(crate::refuse(
                        "com.android.internal.infra.AndroidFuture (value class)",
                    ));
                }
                let t = SurroundingText::read(p)?;
                if p.get_data_position() - start != size {
                    return Err(StatusCode::BAD_VALUE);
                }
                FutureValue::SurroundingText(t)
            }
            _ => {
                return Err(crate::refuse(
                    "com.android.internal.infra.AndroidFuture (value type)",
                ));
            }
        };
        Ok(Self::new(FutureState::Done(value)))
    }
}

// crate::codec! for a generic type (its names are those of the macro's non-generic arm)
const _: () = {
    use std::option::Option;
    impl<T> binder::binder_impl::UnstructuredParcelable for AndroidFuture<T> {
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
    binder::impl_serialize_for_unstructured_parcelable!(AndroidFuture<T>);
    binder::impl_deserialize_for_unstructured_parcelable!(AndroidFuture<T>);
};
