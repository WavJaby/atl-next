// SPDX-License-Identifier: MIT
//! The atl-app-launcher executable: the library's run, linked by `xtask native` against the converter's libsigchain (lib.rs, build.rs).
#![forbid(unsafe_code)]

fn main() {
    atl_app_launcher::main()
}
