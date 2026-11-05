// SPDX-License-Identifier: MIT
//! android.os.storage.StorageVolume as a parcelable of its own (frameworks/base/core/java/android/os/storage/StorageVolume.java writeToParcel, StorageVolume(Parcel)): the layout is
//! `parts::StorageVolume`. IStorageManager.getVolumeList returns it, so the services' stubs name it as a parcelable type and it is a codec here, not an opaque type.

pub use crate::parts::StorageVolume;

crate::codec!(StorageVolume);
