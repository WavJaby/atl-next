// SPDX-License-Identifier: MIT
//! The atl-app-launcher executable: the package's loader (`loader.rs`) with the app runtime's entry `atl_app_main` (lib.rs `ffi::atl_app_main`).

mod loader;

fn main() {
    loader::main(c"atl_app_main", "atl-app-launcher")
}
