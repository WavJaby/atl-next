// SPDX-License-Identifier: MIT
fn main() {
    // `--cfg atl_bridgegen_break`: the deliberate break of the layout fixtures (front::ty sizes a C++
    // reference by its referent, as clang's sizeof of a reference type does) -> tests/layout_fixtures fails
    println!("cargo::rustc-check-cfg=cfg(atl_bridgegen_break)");
}
