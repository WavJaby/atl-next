// SPDX-License-Identifier: MIT
//! The bionic provider for harnesses that start a guest on the stub libc (qemu_dlopen, qemu_main).

use std::path::Path;

/// Loads the bionic provider every ART process loads (`libatl_bionic`, built next to the NB output at `<out>/crates/atl-bionic/lib`, `<nb>` = `<out>/nb/<arch>`). The stubs bind its property functions by name with RTLD_NOLOAD (bindings.rs `Host::Provider`), so it must be in the process before the guest starts; without it those stubs stay unbound and the guest's call to one jumps to 0 (an LP64 guest's libc init calls them; the arm guest reads properties with its own bionic, NBS patch 0012).
pub fn load_provider(nb: &Path) -> Result<(), String> {
    let lib = nb
        .ancestors()
        .map(|a| a.join("crates/atl-bionic/lib/libatl_bionic.so.0.0.0"))
        .find(|p| p.is_file())
        .ok_or("no crates/atl-bionic/lib/libatl_bionic.so.0.0.0 above the NB directory (<out>/nb/<arch>, <out>/nb-graph-<arch>/nb/<arch>)")?;
    let path =
        std::ffi::CString::new(lib.as_os_str().as_encoded_bytes()).map_err(|e| e.to_string())?;
    // SAFETY: dlopen of a NUL-terminated path; the provider's constructors are its own.
    let h = unsafe { libc::dlopen(path.as_ptr(), libc::RTLD_NOW | libc::RTLD_GLOBAL) };
    if h.is_null() {
        return Err(format!("dlopen {}: failed", lib.display()));
    }
    Ok(())
}
