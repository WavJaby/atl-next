// SPDX-License-Identifier: MIT
//! Links the module with `-z defs`: every symbol it imports must resolve in the libraries it links (GTK, GLib, PipeWire, libc), or the link fails.
//! A cross build links the sysroot pinned at deps.toml's GTK floor ([gtk4] min), so a call to a newer GTK fails that build instead of the target host's dlopen.
fn main() {
    println!("cargo:rustc-cdylib-link-arg=-Wl,-z,defs");
}
