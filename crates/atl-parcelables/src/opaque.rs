// SPDX-License-Identifier: MIT
//! Refusals: a parcelable (or a part of one) without a codec refuses to encode or decode, counted per Java name so a run shows what it met.
//! The impls expand libbinder_rs's impl_(de)serialize_for_unstructured_parcelable, which name `Option` / `StatusCode` unqualified: hence the imports in each `const _` block, and the
//! generator's `_` suffix on a Java class of such a name (bp2ninja aidlcrates.go rustName).

use std::collections::BTreeMap;
use std::io::Write;
use std::sync::Mutex;

use binder::StatusCode;

/// An opaque type's only field: libbinder_rs reads arrays in place only of types whose Option is no larger (Deserialize::ASSERT_UNINIT_SIZE_AND_ALIGNMENT), so a zero-size marker will not do.
pub type Niche = std::num::NonZeroU8;
pub const NICHE: Niche = Niche::MIN;

static REFUSED: Mutex<BTreeMap<&'static str, u64>> = Mutex::new(BTreeMap::new());

/// Counts one refusal of `what` (a Java class or field) and returns the status to fail with.
/// Every hit is also written to stderr as `parcelables: refused <what> <n> at=<stamp>` ([`wall_stamp`]): a refusal reaches the caller only as BAD_VALUE (Java: IllegalArgumentException), so the line is the one place that names what was refused.
/// `xtask test` reads these lines out of the services log (atld_run.rs `events`); a process killed at the end of a run prints no summary, so the last line per name is the total.
pub fn refuse(what: &'static str) -> StatusCode {
    // the BAD_VALUE this returns usually leaves on_transact next; the reply hook prints the cause on its line
    atl_diag::set_cause(what);
    // a poisoned counter still counts: the map holds plain numbers
    let mut map = REFUSED.lock().unwrap_or_else(|e| e.into_inner());
    let n = map.entry(what).or_insert(0);
    *n += 1;
    let n = *n;
    drop(map);
    let _ = writeln!(
        std::io::stderr(),
        "parcelables: refused {what} {n} at={}",
        wall_stamp()
    );
    StatusCode::BAD_VALUE
}

/// The refusals so far, by Java name.
pub fn refused() -> Vec<(&'static str, u64)> {
    let map = REFUSED.lock().unwrap_or_else(|e| e.into_inner());
    map.iter().map(|(k, v)| (*k, *v)).collect()
}

/// The wall clock as logcat stamps lines (`secs.mmm`): a copy of `atl_services_types::counters::wall_stamp`, since atl-parcelables has no dependency on atl-services-types; keep the two formats identical.
fn wall_stamp() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    format!("{}.{:03}", now.as_secs(), now.subsec_millis())
}

/// An unstructured parcelable without a codec: `opaque!(Name, "java.Name")` or `opaque!(Name<T>, "java.Name")`.
#[macro_export]
macro_rules! opaque {
    ($name:ident, $java:literal) => {
        #[doc = concat!("`", $java, "`: no codec; encoding or decoding one is refused and counted (`refused`).")]
        #[derive(Debug, Clone, PartialEq)]
        pub struct $name {
            _private: $crate::Niche,
        }
        impl std::default::Default for $name {
            fn default() -> Self {
                $name { _private: $crate::NICHE }
            }
        }
        const _: () = {
            use binder::StatusCode;
            use std::option::Option;
            impl binder::binder_impl::UnstructuredParcelable for $name {
                fn write_to_parcel(&self, _: &mut binder::binder_impl::BorrowedParcel<'_>) -> std::result::Result<(), StatusCode> {
                    Err($crate::refuse($java))
                }
                fn from_parcel(_: &binder::binder_impl::BorrowedParcel<'_>) -> std::result::Result<Self, StatusCode> {
                    Err($crate::refuse($java))
                }
            }
            binder::impl_serialize_for_unstructured_parcelable!($name);
            binder::impl_deserialize_for_unstructured_parcelable!($name);
        };
    };
    ($name:ident < $($t:ident),+ >, $java:literal) => {
        #[doc = concat!("`", $java, "`: no codec; encoding or decoding one is refused and counted (`refused`).")]
        #[derive(Debug, Clone, PartialEq)]
        // defaults: framework AIDL also names generic parcelables raw (`ParceledListSlice`, `AndroidFuture`), which Java allows
        pub struct $name<$($t = ()),+> {
            _private: ($crate::Niche, std::marker::PhantomData<($($t,)+)>),
        }
        impl<$($t),+> std::default::Default for $name<$($t),+> {
            fn default() -> Self {
                $name { _private: ($crate::NICHE, std::marker::PhantomData) }
            }
        }
        const _: () = {
            use binder::StatusCode;
            use std::option::Option;
            impl<$($t),+> binder::binder_impl::UnstructuredParcelable for $name<$($t),+> {
                fn write_to_parcel(&self, _: &mut binder::binder_impl::BorrowedParcel<'_>) -> std::result::Result<(), StatusCode> {
                    Err($crate::refuse($java))
                }
                fn from_parcel(_: &binder::binder_impl::BorrowedParcel<'_>) -> std::result::Result<Self, StatusCode> {
                    Err($crate::refuse($java))
                }
            }
            binder::impl_serialize_for_unstructured_parcelable!($name<$($t),+>);
            binder::impl_deserialize_for_unstructured_parcelable!($name<$($t),+>);
        };
    };
}

/// A hand-written codec (a `parts::Part`) as an unstructured parcelable: UnstructuredParcelable, Serialize, Deserialize.
#[macro_export]
macro_rules! codec {
    ($name:ident) => {
        const _: () = {
            use binder::StatusCode;
            use std::option::Option;
            impl binder::binder_impl::UnstructuredParcelable for $name {
                fn write_to_parcel(
                    &self,
                    p: &mut binder::binder_impl::BorrowedParcel<'_>,
                ) -> std::result::Result<(), StatusCode> {
                    $crate::parts::Part::write(self, p)
                }
                fn from_parcel(
                    p: &binder::binder_impl::BorrowedParcel<'_>,
                ) -> std::result::Result<Self, StatusCode> {
                    $crate::parts::Part::read(p)
                }
            }
            binder::impl_serialize_for_unstructured_parcelable!($name);
            binder::impl_deserialize_for_unstructured_parcelable!($name);
        };
    };
}
