// SPDX-License-Identifier: MIT
//! android.graphics.Rect as a parcelable of its own: the layout parts.rs defines (frameworks/base/graphics/java/android/graphics/Rect.java writeToParcel: four int32s, left top right
//! bottom). Services' stubs name it as a parcelable type (IWindowSession's frames and insets), so it is a codec here, not an opaque type.

pub use crate::parts::Rect;

crate::codec!(Rect);
