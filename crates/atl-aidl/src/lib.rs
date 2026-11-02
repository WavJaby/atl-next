// SPDX-License-Identifier: MIT
//! Rust stubs of the AIDL interfaces ATL's Rust services implement or call, generated at build time by the converter's host `aidl --lang=rust` (build.rs) against AOSP's libbinder_rs (`binder`).
//! Both exist only after `xtask native`, which builds this crate with feature `converter`; without it the crate is empty.
//! Layout as an aidl_interface Rust crate: `aidl::<package path>::<File>::<Type>`, and `mangled`, through which the stubs name each other.
#![forbid(unsafe_code)]
// module names are the AIDL file names (aidl_rust_glue.py's crate root allows the same)
#![cfg_attr(feature = "converter", allow(non_snake_case))]

#[cfg(feature = "converter")]
extern crate binder;

#[cfg(feature = "converter")]
include!(concat!(env!("OUT_DIR"), "/aidl.rs"));
