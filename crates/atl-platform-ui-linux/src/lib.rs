// SPDX-License-Identifier: MIT
//! atl-platform-ui-api on GTK4 (M3 slice 6): the toolkit session and main loop ([`GtkUi`]), the window with its frame clock and pointer input ([`GtkWindow`]).
//! A presented `Scene` becomes one GSK render node tree with a `GdkDmabufTexture` per buffer quad (`scene`, `dmabuf`), drawn by the window's one widget (`view`); GSK's GL or Vulkan renderer imports the dma-bufs (no CPU copy, maintainer decision O3).
//! [`GtkUi`] starts the display thread, which owns GTK and runs GLib's default main loop; every other thread reaches it through the session's invoker.
//! No `unsafe` here except the ping hold (`ping`: the exported `wl_proxy_add_listener` and GDK's saved listener), the dmabuf texture builder (`dmabuf`: GDK borrows the fds) the PCM output (`audio`: pipewire-rs's unsafe thread-loop constructor, `sched_getscheduler`) and the roundtrip on the window surface's queue (`surface_sync`: two libwayland-client calls).
//! `audio` is the PCM output on PipeWire (plan A0): [`PipeWireHost`] and [`PipeWireOutput`] implement the `atl-platform-ui-api` audio traits.
//! Built only as the cdylib the facade loads `RTLD_LOCAL`; `module` exports every type here through the `abi` vtable, the one route in.
#![deny(unsafe_code)]

#[allow(unsafe_code)]
mod audio;
mod count;
#[allow(unsafe_code)]
mod dmabuf;
mod geometry;
mod input;
#[allow(unsafe_code)]
mod module;
mod outputs;
mod paint_stats;
#[allow(unsafe_code)]
mod ping;
mod presentation;
mod scene;
mod sideband;
#[allow(unsafe_code)]
mod surface_sync;
mod text_input;
mod ui;
mod view;
mod window;

pub use audio::{PipeWireHost, PipeWireOutput};
pub use text_input::GtkTextInput;
pub use ui::GtkUi;
pub use window::GtkWindow;
