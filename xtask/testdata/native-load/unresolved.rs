// SPDX-License-Identifier: MIT
//! Broken input of `xtask test native-load --self-test`: a library with one undefined function symbol.
//! Lazy binding would load it and fail only at the first call; the load stage must refuse it (RTLD_NOW / LD_BIND_NOW=1).

unsafe extern "C" {
    fn nlt_missing_symbol();
}

#[unsafe(no_mangle)]
pub extern "C" fn nlt_entry() {
    // SAFETY: never called; the reference alone leaves the symbol undefined in .dynsym.
    unsafe { nlt_missing_symbol() }
}
