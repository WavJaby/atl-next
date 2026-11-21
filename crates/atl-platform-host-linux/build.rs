// SPDX-License-Identifier: MIT
//! Bindings of pipewire's SecurityContext extension (`pipewire/extensions/security-context.h`), generated from the installed header as pipewire-sys generates the core API: pipewire-sys has no binding for
//! the extension, and a hand-copied method table would call the wrong slot after a header change. spa types come from pipewire-sys's own spa_sys (blocklisted here, one definition).
use std::path::PathBuf;

fn main() {
    let lib = pkg_config::Config::new()
        .cargo_metadata(false)
        .probe("libpipewire-0.3")
        .expect("libpipewire-0.3 (pkg-config): the SecurityContext header");
    let out = PathBuf::from(std::env::var_os("OUT_DIR").expect("cargo sets OUT_DIR"));
    let builder = bindgen::Builder::default()
        .header_contents(
            "security_context.h",
            // the extension header leans on pipewire.h for spa_api_method_r, errno, spa_hook
            "#include <pipewire/pipewire.h>\n#include <pipewire/extensions/security-context.h>\n",
        )
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()))
        .allowlist_type("pw_security_context_methods")
        .allowlist_var("PW_SECURITY_CONTEXT_METHOD_.*")
        .allowlist_var("PW_VERSION_SECURITY_CONTEXT_METHODS")
        .blocklist_type("spa_.*")
        .raw_line("use pipewire::spa::sys::{spa_dict, spa_hook};")
        .clang_macro_fallback()
        .clang_macro_fallback_build_dir(&out);
    let builder = lib
        .include_paths
        .iter()
        .fold(builder, |b, p| b.clang_arg(format!("-I{}", p.display())));
    builder
        .generate()
        .expect("generating the SecurityContext bindings")
        .write_to_file(out.join("security_context.rs"))
        .expect("writing the SecurityContext bindings");
}
