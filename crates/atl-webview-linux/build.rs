// SPDX-License-Identifier: MIT
//! Links the module with `-z defs`: every symbol it imports must resolve in the libraries it links (WebKitGTK, GTK, libsoup, GLib, libc), or the link fails.
//! A cross build links the sysroot pinned at deps.toml's floors ([gtk4], [webkitgtk] min), so a call to a newer API fails that build instead of the target host's dlopen.
fn main() {
    println!("cargo:rustc-cdylib-link-arg=-Wl,-z,defs");
}
