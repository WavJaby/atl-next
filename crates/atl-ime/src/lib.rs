// SPDX-License-Identifier: MIT
//! The app process's in-app input method (IME plan route R1): IMMS-lite binds the Rust `IInputMethod` this crate serves, and its session drives the focused field's `InputConnection` from the desktop's input method (`atl-platform-ui` TextInput).
//! So far: [`units`], the one place text indices change units.
#![forbid(unsafe_code)]

pub mod units;
