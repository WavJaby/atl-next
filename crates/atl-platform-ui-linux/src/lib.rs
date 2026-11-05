// SPDX-License-Identifier: MIT
//! atl-platform-ui-api on GTK4 (M3 slice 6): the toolkit session and main loop ([`GtkUi`]), the window with its frame clock and pointer input ([`GtkWindow`]).
//! A presented `Scene` becomes one GSK render node tree with a `GdkMemoryTexture` per quad (`scene`), drawn by the window's one widget (`view`); GSK's renderer puts it on the Wayland surface (wl_shm with the cairo renderer, which is what the step-1 gate runs; step 2 adds dmabuf textures).
//! No `unsafe` here except the GLib mirror (`mirror`: the raw `g_main_context_*` calls glib-rs leaves unwrapped). A quad's fd is mapped and sync'd for CPU reads by atl-scene-buffer.
#![deny(unsafe_code)]

mod geometry;
mod input;
#[allow(unsafe_code)]
pub(crate) mod mirror;
mod pacing;
mod scene;
mod text_input;
mod ui;
mod view;
mod window;

pub use text_input::GtkTextInput;
pub use ui::GtkUi;
pub use window::GtkWindow;
