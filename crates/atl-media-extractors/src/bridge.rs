// SPDX-License-Identifier: MIT
//! The cxx bridge to wrap/extractors.h: one call into libstagefright, no data crosses.

#[cxx::bridge(namespace = "atl::extractors")]
pub(crate) mod ffi {
    unsafe extern "C++" {
        include!("wrap/extractors.h");

        /// MediaExtractorFactory::LoadExtractors: registers the plugins of `lib64/extractors` in the APEX, system and system_ext dirs; once per process, later calls return.
        fn load_extractors();
    }
}
