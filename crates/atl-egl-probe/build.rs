// SPDX-License-Identifier: MIT
//! atl-egl-driver-probe exports `__system_property_get`: the driver it loads resolves the symbol from the process's global scope (an app process has bionic's there), and an executable's own symbols reach that scope only when exported.

fn main() {
    println!(
        "cargo::rustc-link-arg-bin=atl-egl-driver-probe=-Wl,--export-dynamic-symbol=__system_property_get"
    );
}
