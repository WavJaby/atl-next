// SPDX-License-Identifier: MIT
//! atl-platform-ui-api on GTK4 (M3 slice 6): the toolkit session and main loop ([`GtkUi`]), the window with its frame clock and pointer input ([`GtkWindow`]).
//! A presented `Scene` becomes one GSK render node tree with a `GdkDmabufTexture` per buffer quad (`scene`, `dmabuf`), drawn by the window's one widget (`view`); GSK's GL or Vulkan renderer imports the dma-bufs (no CPU copy, owner decision O3).
//! No `unsafe` here except the GLib mirror (`mirror`: the raw `g_main_context_*` calls glib-rs leaves unwrapped), the dmabuf texture builder (`dmabuf`: GDK borrows the fds) and the PCM output (`audio`: pipewire-rs's unsafe thread-loop constructor, `sched_getscheduler`).
//! `audio` is the PCM output on PipeWire (plan A0): [`PipeWireHost`] and [`PipeWireOutput`] implement the `atl-platform-ui-api` audio traits.
#![deny(unsafe_code)]

#[allow(unsafe_code)]
mod audio;
#[allow(unsafe_code)]
mod dmabuf;
mod geometry;
mod input;
#[allow(unsafe_code)]
pub(crate) mod mirror;
mod outputs;
mod scene;
mod text_input;
mod ui;
mod view;
mod window;

pub use audio::{PipeWireHost, PipeWireOutput};
pub use text_input::GtkTextInput;
pub use ui::GtkUi;
pub use window::GtkWindow;
