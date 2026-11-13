// SPDX-License-Identifier: MIT
//! android.content.pm.ParceledListSlice: content/pm/BaseParceledListSlice.java writeToParcel (:172-195) / BaseParceledListSlice(Parcel) (:60-100). Wire: int32 N; for N > 0 the element class name
//! (writeParcelableCreator: writeString of the class name), then N times int32 1 and the element's writeToParcel. ParceledListSlice never splits a list (mInlineCountLimit is Integer.MAX_VALUE), but
//! BaseParceledListSlice's size limit (:185) would; a list whose elements outgrow `IBinder.getSuggestedMaxIpcSizeBytes()` is refused here rather than half-written.
//! The Java class is generic and AIDL names it raw, so the stubs decode it without knowing the element type: the class name on the wire picks the element codec from [`elements!`]'s table. A class with no
//! row is refused (counted, and named on stderr): inline elements carry no length, so an element no codec reads cannot be skipped. Adding an element kind is one row.

use std::io::Write;

use super::InstrumentationInfo::InstrumentationInfo;
use crate::codecs::android::app::NotificationChannel::NotificationChannel;
use crate::parcel::*;
use crate::parts::*;

/// `IBinder.getSuggestedMaxIpcSizeBytes()`: 64 KiB (IBinder.java).
const MAX_IPC_SIZE: i32 = 64 * 1024;

/// An element class a list can carry: its Java class name (what writeParcelableCreator writes) and its place in [`Element`].
pub trait ListElement: Part + Clone {
    const CLASS: &'static str;
    fn wrap(self) -> Element;
    fn unwrap(e: &Element) -> Option<&Self>;
}

/// The element table: one row per modelled element class (Rust type => Java class name).
macro_rules! elements {
    ($($ty:ident => $class:literal),* $(,)?) => {
        /// One element of a list, of whichever modelled class the list carries.
        #[derive(Debug, Clone)]
        pub enum Element {
            $($ty($ty),)*
        }

        impl Element {
            fn class(&self) -> &'static str {
                match self {
                    $(Element::$ty(_) => $class,)*
                }
            }
            fn write(&self, p: &mut In<'_>) -> Result<()> {
                match self {
                    $(Element::$ty(e) => e.write(p),)*
                }
            }
            /// The reader of `class`'s elements; None for a class with no row.
            fn reader(class: &str) -> Option<fn(&In<'_>) -> Result<Element>> {
                match class {
                    $($class => Some(|p| $ty::read(p).map(Element::$ty)),)*
                    _ => None,
                }
            }
        }

        $(impl ListElement for $ty {
            const CLASS: &'static str = $class;
            fn wrap(self) -> Element {
                Element::$ty(self)
            }
            fn unwrap(e: &Element) -> Option<&Self> {
                match e {
                    Element::$ty(x) => Some(x),
                    #[allow(unreachable_patterns)]
                    _ => None,
                }
            }
        })*
    };
}

elements! {
    // IPackageManager.queryInstrumentationAsUser's reply
    InstrumentationInfo => "android.content.pm.InstrumentationInfo",
    // INotificationManager.createNotificationChannels / getNotificationChannels
    NotificationChannel => "android.app.NotificationChannel",
}

/// AIDL names it raw (`ParceledListSlice`) and typed (`ParceledListSlice<ApplicationStartInfo>`) alike, so the type parameter is only a marker (default `()`, as `opaque!` does for the generic ones): the
/// elements are [`Element`]s, all of one class as Java requires (writeToParcel throws on a mixed list). Impls are written out because derives would bound `T`.
pub struct ParceledListSlice<T = ()> {
    items: Vec<Element>,
    _element: std::marker::PhantomData<fn() -> T>,
}

impl<T> ParceledListSlice<T> {
    /// A list of `items`, one class.
    pub fn new<E: ListElement>(items: Vec<E>) -> Self {
        ParceledListSlice {
            items: items.into_iter().map(ListElement::wrap).collect(),
            _element: std::marker::PhantomData,
        }
    }

    /// The elements of class `E`; an element of another class is left out (a list holds one class, so a list of another class yields nothing).
    pub fn of<E: ListElement>(&self) -> impl Iterator<Item = &E> {
        self.items.iter().filter_map(E::unwrap)
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

impl<T> Default for ParceledListSlice<T> {
    fn default() -> Self {
        ParceledListSlice {
            items: Vec::new(),
            _element: std::marker::PhantomData,
        }
    }
}

impl<T> Clone for ParceledListSlice<T> {
    fn clone(&self) -> Self {
        ParceledListSlice {
            items: self.items.clone(),
            _element: std::marker::PhantomData,
        }
    }
}

impl<T> std::fmt::Debug for ParceledListSlice<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ParceledListSlice")
            .field("items", &self.items)
            .finish()
    }
}

impl<T> Part for ParceledListSlice<T> {
    fn write(&self, p: &mut In<'_>) -> Result<()> {
        w_i32(p, len_i32(self.items.len())?)?;
        let Some(first) = self.items.first() else {
            return Ok(());
        };
        let class = first.class();
        w_str16(p, &Some(class.to_owned()))?;
        for item in &self.items {
            if item.class() != class {
                return Err(crate::refuse(
                    "android.content.pm.ParceledListSlice (mixed classes)",
                ));
            }
            w_i32(p, 1)?;
            item.write(p)?;
        }
        if p.get_data_size() > MAX_IPC_SIZE {
            return Err(crate::refuse("android.content.pm.ParceledListSlice (size)"));
        }
        Ok(())
    }

    /// The inline form only: every element follows (the int32 1 before each). A list the sender split (an int32 0 and a binder to fetch the rest) or of a class with no row is refused.
    fn read(p: &In<'_>) -> Result<Self> {
        let n = r_i32(p)?;
        if n <= 0 {
            return Ok(ParceledListSlice::default());
        }
        let class = r_str16(p)?.unwrap_or_default();
        let Some(read) = Element::reader(&class) else {
            let _ = writeln!(
                std::io::stderr(),
                "parcelables: ParceledListSlice element class {class} has no codec"
            );
            return Err(crate::refuse(
                "android.content.pm.ParceledListSlice (element class)",
            ));
        };
        let items = (0..n)
            .map(|_| match r_i32(p)? {
                1 => read(p),
                _ => Err(crate::refuse(
                    "android.content.pm.ParceledListSlice (split list)",
                )),
            })
            .collect::<Result<_>>()?;
        Ok(ParceledListSlice {
            items,
            _element: std::marker::PhantomData,
        })
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
