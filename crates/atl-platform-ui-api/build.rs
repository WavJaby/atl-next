// SPDX-License-Identifier: MIT
//! `abi::ABI_ID`: FNV-1a 64 over the sources whose definitions cross the platform module's C ABI (this crate's `src/`, atl-platform-types') and the pinned toolchain (`rust-toolchain.toml`). The facade and the module compile this crate in one build, so equal ids mean one build; a module from another build is refused by the loader before any other vtable field is read.

use std::path::{Path, PathBuf};

/// FNV-1a 64 offset basis and prime (the published FNV parameters).
const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

fn fnv(mut h: u64, bytes: &[u8]) -> u64 {
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(FNV_PRIME);
    }
    h
}

fn sources(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .expect("read src dir")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "rs"))
        .collect();
    files.sort();
    files
}

fn main() {
    let manifest =
        PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let mut files = sources(&manifest.join("src"));
    files.extend(sources(&manifest.join("../atl-platform-types/src")));
    let mut h = FNV_OFFSET;
    for f in &files {
        println!("cargo:rerun-if-changed={}", f.display());
        h = fnv(h, f.file_name().expect("file name").as_encoded_bytes());
        h = fnv(h, &std::fs::read(f).expect("read source"));
    }
    // the pinned toolchain: one rustc for both artifacts, and a pin bump changes the id
    let toolchain = manifest.join("../../rust-toolchain.toml");
    println!("cargo:rerun-if-changed={}", toolchain.display());
    h = fnv(
        h,
        &std::fs::read(&toolchain).expect("read rust-toolchain.toml"),
    );
    let out = PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR"));
    std::fs::write(out.join("abi_id.rs"), format!("0x{h:016x}")).expect("write abi_id.rs");
}
