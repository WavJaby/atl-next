// SPDX-License-Identifier: MIT
//! atl-platform-ui-api on GTK4 (M3 slice 6): the toolkit session and main loop ([`GtkUi`]), the window with its frame clock and pointer input ([`GtkWindow`]).
//! A presented `Scene` becomes one GSK render node tree with a `GdkMemoryTexture` per quad (`scene`), drawn by the window's one widget (`view`); GSK's renderer puts it on the Wayland surface (wl_shm with the cairo renderer, which is what the step-1 gate runs; step 2 adds dmabuf textures).
//! `unsafe` only in `buffer` (a quad's fd mapped and sync'd for CPU reads); the crate denies it everywhere else.
#![deny(unsafe_code)]

#[allow(unsafe_code)]
mod buffer;
mod geometry;
mod input;
mod scene;
mod ui;
mod view;
mod window;

pub use ui::{FdWatch, GtkUi};
pub use window::GtkWindow;
