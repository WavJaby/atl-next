// SPDX-License-Identifier: MIT
//! Rust types of the Java-only parcelables framework AIDL passes, so ATL's Rust services can implement framework interfaces (IApplicationThread, IActivityManager, ...).
//! Rung 6 of "AOSP's own code first": these types exist only as Java `Parcelable`s (writeToParcel / createFromParcel); no AOSP C++ or Rust implementation reads them, and the Rust services need them.
//!
//! Built by the converter as rust_library `libatl_parcelables` (Cargo.toml `rust_library`; bp2ninja aidlcrates.go), with `parcelables.rs` generated into OUT_DIR: one item per unstructured
//! `parcelable X;` under the AIDL crate's include dirs and per Java-only builtin, in modules mirroring the Java packages (the stubs name them through rust_type `atl_parcelables::<java path>`):
//! - a hand-written codec where `src/codecs/<java path>/<Name>.rs` exists: the AOSP writeToParcel / createFromParcel order, field for field;
//! - else `opaque!`: a type that refuses to encode or decode and counts the refusal (`refused`); null still passes (the NULL flag is the caller's).
//!
//! A codec models the values M2 sends; a non-null value of a part it does not model (Spanned text, a typed array it has no codec for, ...) is refused and counted the same way.
//! Ground truth: `xtask test parcel` round-trips each codec against Java's own Parcelable on host ART (Java bytes -> Rust decode + encode -> byte-equal; Rust bytes -> Java `equals`).
//! Cargo builds nothing here (`cfg(soong)`, which only the converter passes): plain workspace builds need no converter output.
#![cfg(soong)]
#![forbid(unsafe_code)]

pub mod aconfig;
pub mod codecs;
mod opaque;
pub mod parcel;
pub mod parts;
pub mod squash;

pub use opaque::{NICHE, Niche, refuse, refused};

/// The generated items: Java package and class names as Rust paths (nested classes `Outer_Inner`), hence the naming allowances.
#[allow(non_camel_case_types, non_snake_case)]
mod generated {
    include!(concat!(env!("OUT_DIR"), "/parcelables.rs"));
}
pub use generated::*;
