// SPDX-License-Identifier: MIT
//! The atl-installer executable: the package's loader (`loader.rs`) with the installer's entry `atl_installer_main` (lib.rs `installer`): AOSP's package parsing in a VM of its own, one process per install.

#[path = "../loader.rs"]
mod loader;

fn main() {
    loader::main(c"atl_installer_main", "atl-installer")
}
