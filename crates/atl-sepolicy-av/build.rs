// SPDX-License-Identifier: MIT
//! Bindings of libsepol's access-vector query (`sepol/sepol.h`, `sepol/policydb/services.h`), generated from the installed headers: struct sepol_av_decision and the id types are taken from the
//! header, not hand-copied. Linked dynamically; build time only (deps.toml row libsepol): nothing of libsepol reaches a shipped binary.
use std::path::PathBuf;

fn main() {
    let lib = pkg_config::Config::new()
        .atleast_version("3.8.1")
        .probe("libsepol")
        .expect("libsepol (pkg-config): the access-vector query of the service access table");
    let out = PathBuf::from(std::env::var_os("OUT_DIR").expect("cargo sets OUT_DIR"));
    let builder = bindgen::Builder::default()
        .header_contents("av.h", "#include <stdio.h>\n#include <sepol/sepol.h>\n#include <sepol/policydb/services.h>\n")
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()))
        .allowlist_function("sepol_set_policydb_from_file")
        .allowlist_function("sepol_context_to_sid")
        .allowlist_function("sepol_compute_av")
        .allowlist_function("sepol_string_to_security_class")
        .allowlist_function("sepol_string_to_av_perm")
        .allowlist_type("sepol_av_decision")
        // FILE is passed through as an opaque pointer from libc::fopen
        .blocklist_type("FILE")
        .blocklist_type("_IO_FILE")
        .raw_line("pub type FILE = libc::FILE;");
    let builder = lib
        .include_paths
        .iter()
        .fold(builder, |b, p| b.clang_arg(format!("-I{}", p.display())));
    builder
        .generate()
        .expect("generating the libsepol bindings")
        .write_to_file(out.join("sepol.rs"))
        .expect("writing the libsepol bindings");
}
