// SPDX-License-Identifier: MIT
//! android.content.pm.InstrumentationInfo: content/pm/InstrumentationInfo.java writeToParcel / InstrumentationInfo(Parcel), after the PackageItemInfo base (parts.rs). Not modelled, null only: splitDependencies (a SparseArray; writeSparseArray writes -1 for null).
//! handleProfiling / functionalTest: writeInt(0|1) on write, readInt() != 0 on read (w_bool / r_bool).

use crate::parcel::*;
use crate::parts::*;

#[derive(Debug, Clone, Default)]
pub struct InstrumentationInfo {
    pub item: PackageItemInfo,
    pub target_package: Option<String>,
    pub target_processes: Option<String>,
    pub source_dir: Option<String>,
    pub public_source_dir: Option<String>,
    pub split_names: Option<Vec<Option<String>>>,
    pub split_source_dirs: Option<Vec<Option<String>>>,
    pub split_public_source_dirs: Option<Vec<Option<String>>>,
    pub data_dir: Option<String>,
    pub device_protected_data_dir: Option<String>,
    pub credential_protected_data_dir: Option<String>,
    pub primary_cpu_abi: Option<String>,
    pub secondary_cpu_abi: Option<String>,
    pub native_library_dir: Option<String>,
    pub secondary_native_library_dir: Option<String>,
    pub handle_profiling: bool,
    pub functional_test: bool,
}

impl Part for InstrumentationInfo {
    fn write(&self, p: &mut In<'_>) -> Result<()> {
        self.item.write(p)?;
        w_str8(p, &self.target_package)?;
        w_str8(p, &self.target_processes)?;
        w_str8(p, &self.source_dir)?;
        w_str8(p, &self.public_source_dir)?;
        w_str8_array(p, &self.split_names)?;
        w_str8_array(p, &self.split_source_dirs)?;
        w_str8_array(p, &self.split_public_source_dirs)?;
        w_null(p)?; // splitDependencies
        w_str8(p, &self.data_dir)?;
        w_str8(p, &self.device_protected_data_dir)?;
        w_str8(p, &self.credential_protected_data_dir)?;
        w_str8(p, &self.primary_cpu_abi)?;
        w_str8(p, &self.secondary_cpu_abi)?;
        w_str8(p, &self.native_library_dir)?;
        w_str8(p, &self.secondary_native_library_dir)?;
        w_bool(p, self.handle_profiling)?;
        w_bool(p, self.functional_test)
    }

    fn read(p: &In<'_>) -> Result<Self> {
        let item = PackageItemInfo::read(p)?;
        let target_package = r_str8(p)?;
        let target_processes = r_str8(p)?;
        let source_dir = r_str8(p)?;
        let public_source_dir = r_str8(p)?;
        let split_names = r_str8_array(p)?;
        let split_source_dirs = r_str8_array(p)?;
        let split_public_source_dirs = r_str8_array(p)?;
        r_null(
            p,
            "android.content.pm.InstrumentationInfo.splitDependencies",
        )?;
        let data_dir = r_str8(p)?;
        let device_protected_data_dir = r_str8(p)?;
        let credential_protected_data_dir = r_str8(p)?;
        let primary_cpu_abi = r_str8(p)?;
        let secondary_cpu_abi = r_str8(p)?;
        let native_library_dir = r_str8(p)?;
        let secondary_native_library_dir = r_str8(p)?;
        let handle_profiling = r_bool(p)?;
        let functional_test = r_bool(p)?;
        Ok(InstrumentationInfo {
            item,
            target_package,
            target_processes,
            source_dir,
            public_source_dir,
            split_names,
            split_source_dirs,
            split_public_source_dirs,
            data_dir,
            device_protected_data_dir,
            credential_protected_data_dir,
            primary_cpu_abi,
            secondary_cpu_abi,
            native_library_dir,
            secondary_native_library_dir,
            handle_profiling,
            functional_test,
        })
    }
}

crate::codec!(InstrumentationInfo);
