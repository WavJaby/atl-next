// SPDX-License-Identifier: MIT
//! `android_dlopen_ext`'s extended flags (bionic `linker.cpp` `do_dlopen`): which ones a load here takes,
//! and bionic's refusal text for the rest. Only `ANDROID_DLEXT_USE_NAMESPACE` is taken -- the one flag the
//! native closure passes (2026-10-31: libEGL Loader.cpp, libvulkan driver.cpp, libstagefright MediaCodec /
//! MediaExtractorFactory, libvndksupport). Reserved addresses, RELRO sharing, loading from a descriptor
//! and forced reloads need bionic's own loader; glibc's `dlopen` has no equivalent, so they are refused.

use crate::generated::dlext::{
    ANDROID_DLEXT_USE_LIBRARY_FD, ANDROID_DLEXT_USE_LIBRARY_FD_OFFSET, ANDROID_DLEXT_USE_NAMESPACE,
    ANDROID_DLEXT_VALID_FLAG_BITS,
};

/// Why a load with these extended flags does not happen; `Display` is the `dlerror` detail after
/// bionic's `dlopen failed: `.
#[derive(Debug, PartialEq, Eq)]
pub enum Refusal {
    /// Bits outside `ANDROID_DLEXT_VALID_FLAG_BITS` (bionic's text).
    Invalid(u64),
    /// `USE_LIBRARY_FD_OFFSET` without `USE_LIBRARY_FD` (bionic's text).
    OffsetWithoutFd(u64),
    /// `USE_NAMESPACE` with a null namespace (bionic's text).
    NullNamespace,
    /// Valid in bionic, not implemented here (our text, in bionic's form).
    Unsupported(u64),
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Refusal::Invalid(flags) => {
                write!(
                    f,
                    "invalid extended flags to android_dlopen_ext: {flags:#x}"
                )
            }
            Refusal::OffsetWithoutFd(flags) => write!(
                f,
                "invalid extended flag combination (ANDROID_DLEXT_USE_LIBRARY_FD_OFFSET without ANDROID_DLEXT_USE_LIBRARY_FD): {flags:#x}"
            ),
            Refusal::NullNamespace => write!(
                f,
                "ANDROID_DLEXT_USE_NAMESPACE is set but extinfo->library_namespace is null"
            ),
            Refusal::Unsupported(flags) => write!(
                f,
                "unsupported extended flags to android_dlopen_ext: {flags:#x} (only ANDROID_DLEXT_USE_NAMESPACE is)"
            ),
        }
    }
}

/// Checks `flags` in bionic's order; `has_namespace`: `library_namespace` is not null. Ok: load as
/// plain `dlopen` (in the given namespace when `USE_NAMESPACE` is set).
pub fn check(flags: u64, has_namespace: bool) -> Result<(), Refusal> {
    // the enumerators are `unsigned int` (C gives the enum that type)
    let bit = |b: u32| u64::from(b);
    if flags & !bit(ANDROID_DLEXT_VALID_FLAG_BITS) != 0 {
        return Err(Refusal::Invalid(flags));
    }
    if flags & bit(ANDROID_DLEXT_USE_LIBRARY_FD) == 0
        && flags & bit(ANDROID_DLEXT_USE_LIBRARY_FD_OFFSET) != 0
    {
        return Err(Refusal::OffsetWithoutFd(flags));
    }
    if flags & bit(ANDROID_DLEXT_USE_NAMESPACE) != 0 && !has_namespace {
        return Err(Refusal::NullNamespace);
    }
    if flags & !bit(ANDROID_DLEXT_USE_NAMESPACE) != 0 {
        return Err(Refusal::Unsupported(flags));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // bit values from bionic's android/dlext.h
    const RESERVED_ADDRESS: u64 = 0x1;
    const USE_LIBRARY_FD: u64 = 0x10;
    const USE_LIBRARY_FD_OFFSET: u64 = 0x20;
    const USE_NAMESPACE: u64 = 0x200;
    const RESERVED_ADDRESS_RECURSIVE: u64 = 0x400;

    #[test]
    fn taken_flags() {
        assert_eq!(check(0, false), Ok(()));
        assert_eq!(check(0, true), Ok(()));
        assert_eq!(check(USE_NAMESPACE, true), Ok(()));
    }

    #[test]
    fn bionic_refusals_in_bionic_order() {
        // 0x800 is the first bit past bionic's last flag (RESERVED_ADDRESS_RECURSIVE); bionic checks
        // validity before the namespace
        let r = check(0x800 | USE_NAMESPACE, false).unwrap_err();
        assert_eq!(
            r.to_string(),
            "invalid extended flags to android_dlopen_ext: 0xa00"
        );
        let r = check(USE_LIBRARY_FD_OFFSET, true).unwrap_err();
        assert_eq!(
            r.to_string(),
            "invalid extended flag combination (ANDROID_DLEXT_USE_LIBRARY_FD_OFFSET without ANDROID_DLEXT_USE_LIBRARY_FD): 0x20"
        );
        let r = check(USE_NAMESPACE, false).unwrap_err();
        assert_eq!(
            r.to_string(),
            "ANDROID_DLEXT_USE_NAMESPACE is set but extinfo->library_namespace is null"
        );
    }

    #[test]
    fn bionic_only_flags_refused() {
        for flags in [
            RESERVED_ADDRESS,
            USE_LIBRARY_FD | USE_LIBRARY_FD_OFFSET,
            RESERVED_ADDRESS_RECURSIVE | USE_NAMESPACE,
        ] {
            assert_eq!(check(flags, true), Err(Refusal::Unsupported(flags)));
        }
    }
}
