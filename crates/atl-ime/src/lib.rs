// SPDX-License-Identifier: MIT
//! The app process's in-app input method (IME plan route R1): IMMS-lite binds the Rust `IInputMethod` this crate serves (`new_binder`, handed out by the app's UI agent), and its session drives
//! the focused field's `InputConnection` from the desktop's input method (`atl-platform-ui` TextInput, `attach`).
//! Modules: `method` (the binder servers, binder threads), `field` (UI thread: TextInput events -> IRemoteInputConnection), [`content`] (EditorInfo -> purpose/hints), [`units`] (index units).
#![forbid(unsafe_code)]

// the converter's rlibs, on the crate search path build.rs sets (no cargo dependency names them)
#[cfg(feature = "converter")]
extern crate atl_parcelables;
#[cfg(feature = "converter")]
extern crate binder;

pub mod content;
pub mod units;

#[cfg(feature = "converter")]
mod field;
#[cfg(feature = "converter")]
mod method;

#[cfg(feature = "converter")]
pub use field::attach;
#[cfg(feature = "converter")]
pub use method::new_binder;
