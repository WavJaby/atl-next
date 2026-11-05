// SPDX-License-Identifier: MIT
//! android.content.pm.ApplicationInfo: content/pm/ApplicationInfo.java writeToParcel / ApplicationInfo(Parcel), behind Parcel.maybeWriteSquashed / readSquashed.
//! Squashing: a parcel that allows it (PackageInfo, ProviderInfoList) writes a later identical ApplicationInfo as a back-reference offset; this codec writes 0 (no squash, which every reader accepts)
//! and refuses to read a back-reference. Not modelled, null only: splitDependencies (SparseArray), optionalSharedLibraryInfos.

use crate::parcel::*;
use crate::parts::*;

#[derive(Debug, Clone, Default)]
pub struct ApplicationInfo {
    pub item: PackageItemInfo,
    pub task_affinity: Option<String>,
    pub permission: Option<String>,
    pub process_name: Option<String>,
    pub class_name: Option<String>,
    pub theme: i32,
    pub flags: i32,
    pub private_flags: i32,
    pub private_flags_ext: i32,
    pub requires_smallest_width_dp: i32,
    pub compatible_width_limit_dp: i32,
    pub largest_width_limit_dp: i32,
    /// storageUuid (most, least significant bits)
    pub storage_uuid: Option<(i64, i64)>,
    pub scan_source_dir: Option<String>,
    pub scan_public_source_dir: Option<String>,
    pub source_dir: Option<String>,
    pub public_source_dir: Option<String>,
    pub split_names: Option<Vec<Option<String>>>,
    pub split_source_dirs: Option<Vec<Option<String>>>,
    pub split_public_source_dirs: Option<Vec<Option<String>>>,
    pub native_library_dir: Option<String>,
    pub secondary_native_library_dir: Option<String>,
    pub native_library_root_dir: Option<String>,
    pub native_library_root_requires_isa: bool,
    pub primary_cpu_abi: Option<String>,
    pub secondary_cpu_abi: Option<String>,
    pub resource_dirs: Option<Vec<Option<String>>>,
    pub overlay_paths: Option<Vec<Option<String>>>,
    pub se_info: Option<String>,
    pub se_info_user: Option<String>,
    pub shared_library_files: Option<Vec<Option<String>>>,
    pub shared_library_infos: Option<Vec<Option<SharedLibraryInfo>>>,
    pub data_dir: Option<String>,
    pub device_protected_data_dir: Option<String>,
    pub credential_protected_data_dir: Option<String>,
    pub uid: i32,
    pub min_sdk_version: i32,
    pub target_sdk_version: i32,
    pub long_version_code: i64,
    pub enabled: bool,
    pub enabled_setting: i32,
    pub install_location: i32,
    pub manage_space_activity_name: Option<String>,
    pub backup_agent_name: Option<String>,
    pub description_res: i32,
    pub ui_options: i32,
    pub full_backup_content: i32,
    pub data_extraction_rules_res: i32,
    pub cross_profile: bool,
    pub network_security_config_res: i32,
    pub category: i32,
    pub target_sandbox_version: i32,
    pub class_loader_name: Option<String>,
    pub split_class_loader_names: Option<Vec<Option<String>>>,
    pub compile_sdk_version: i32,
    pub compile_sdk_version_codename: Option<String>,
    pub app_component_factory: Option<String>,
    pub icon_res: i32,
    pub round_icon_res: i32,
    pub hidden_api_policy: i32,
    pub hidden_until_installed: bool,
    pub zygote_preload_name: Option<String>,
    pub gwp_asan_mode: i32,
    pub memtag_mode: i32,
    pub native_heap_zero_initialized: i32,
    pub request_raw_external_storage_access: Option<bool>,
    pub create_timestamp: i64,
    /// mAppClassNamesByProcess, in ArrayMap order (String16 pairs); empty = null
    pub app_class_names_by_process: Vec<(Option<String>, Option<String>)>,
    pub locale_config_res: i32,
    pub allow_cross_uid_activity_switch_from_below: bool,
    pub page_size_app_compat_flags: i32,
    /// mKnownActivityEmbeddingCerts; empty = null
    pub known_activity_embedding_certs: Vec<Option<String>>,
}

impl Part for ApplicationInfo {
    fn write(&self, p: &mut In<'_>) -> Result<()> {
        w_i32(p, 0)?; // maybeWriteSquashed: not squashed
        self.item.write(p)?;
        w_str8(p, &self.task_affinity)?;
        w_str8(p, &self.permission)?;
        w_str8(p, &self.process_name)?;
        w_str8(p, &self.class_name)?;
        w_i32(p, self.theme)?;
        w_i32(p, self.flags)?;
        w_i32(p, self.private_flags)?;
        w_i32(p, self.private_flags_ext)?;
        w_i32(p, self.requires_smallest_width_dp)?;
        w_i32(p, self.compatible_width_limit_dp)?;
        w_i32(p, self.largest_width_limit_dp)?;
        match self.storage_uuid {
            None => w_i32(p, 0)?,
            Some((most, least)) => {
                w_i32(p, 1)?;
                w_i64(p, most)?;
                w_i64(p, least)?;
            }
        }
        w_str8(p, &self.scan_source_dir)?;
        w_str8(p, &self.scan_public_source_dir)?;
        w_str8(p, &self.source_dir)?;
        w_str8(p, &self.public_source_dir)?;
        w_str8_array(p, &self.split_names)?;
        w_str8_array(p, &self.split_source_dirs)?;
        w_str8_array(p, &self.split_public_source_dirs)?;
        w_null(p)?; // splitDependencies
        w_str8(p, &self.native_library_dir)?;
        w_str8(p, &self.secondary_native_library_dir)?;
        w_str8(p, &self.native_library_root_dir)?;
        w_bool(p, self.native_library_root_requires_isa)?;
        w_str8(p, &self.primary_cpu_abi)?;
        w_str8(p, &self.secondary_cpu_abi)?;
        w_str8_array(p, &self.resource_dirs)?;
        w_str8_array(p, &self.overlay_paths)?;
        w_str8(p, &self.se_info)?;
        w_str8(p, &self.se_info_user)?;
        w_str8_array(p, &self.shared_library_files)?;
        w_typed_list(p, &self.shared_library_infos)?;
        w_null(p)?; // optionalSharedLibraryInfos
        w_str8(p, &self.data_dir)?;
        w_str8(p, &self.device_protected_data_dir)?;
        w_str8(p, &self.credential_protected_data_dir)?;
        w_i32(p, self.uid)?;
        w_i32(p, self.min_sdk_version)?;
        w_i32(p, self.target_sdk_version)?;
        w_i64(p, self.long_version_code)?;
        w_bool(p, self.enabled)?;
        w_i32(p, self.enabled_setting)?;
        w_i32(p, self.install_location)?;
        w_str8(p, &self.manage_space_activity_name)?;
        w_str8(p, &self.backup_agent_name)?;
        w_i32(p, self.description_res)?;
        w_i32(p, self.ui_options)?;
        w_i32(p, self.full_backup_content)?;
        w_i32(p, self.data_extraction_rules_res)?;
        w_bool(p, self.cross_profile)?;
        w_i32(p, self.network_security_config_res)?;
        w_i32(p, self.category)?;
        w_i32(p, self.target_sandbox_version)?;
        w_str8(p, &self.class_loader_name)?;
        w_str8_array(p, &self.split_class_loader_names)?;
        w_i32(p, self.compile_sdk_version)?;
        w_str8(p, &self.compile_sdk_version_codename)?;
        w_str8(p, &self.app_component_factory)?;
        w_i32(p, self.icon_res)?;
        w_i32(p, self.round_icon_res)?;
        w_i32(p, self.hidden_api_policy)?;
        w_bool(p, self.hidden_until_installed)?;
        w_str8(p, &self.zygote_preload_name)?;
        w_i32(p, self.gwp_asan_mode)?;
        w_i32(p, self.memtag_mode)?;
        w_i32(p, self.native_heap_zero_initialized)?;
        w_opt_bool(p, self.request_raw_external_storage_access)?;
        w_i64(p, self.create_timestamp)?;
        w_i32(p, len_i32(self.app_class_names_by_process.len())?)?;
        for (k, v) in &self.app_class_names_by_process {
            w_str16(p, k)?;
            w_str16(p, v)?;
        }
        w_i32(p, self.locale_config_res)?;
        w_bool(p, self.allow_cross_uid_activity_switch_from_below)?;
        w_i32(p, self.page_size_app_compat_flags)?;
        w_string_set(p, &self.known_activity_embedding_certs)
    }

    fn read(p: &In<'_>) -> Result<Self> {
        if r_i32(p)? != 0 {
            return Err(crate::refuse(
                "android.content.pm.ApplicationInfo (squashed back-reference)",
            ));
        }
        let item = PackageItemInfo::read(p)?;
        let task_affinity = r_str8(p)?;
        let permission = r_str8(p)?;
        let process_name = r_str8(p)?;
        let class_name = r_str8(p)?;
        let theme = r_i32(p)?;
        let flags = r_i32(p)?;
        let private_flags = r_i32(p)?;
        let private_flags_ext = r_i32(p)?;
        let requires_smallest_width_dp = r_i32(p)?;
        let compatible_width_limit_dp = r_i32(p)?;
        let largest_width_limit_dp = r_i32(p)?;
        let storage_uuid = if r_i32(p)? != 0 {
            Some((r_i64(p)?, r_i64(p)?))
        } else {
            None
        };
        let scan_source_dir = r_str8(p)?;
        let scan_public_source_dir = r_str8(p)?;
        let source_dir = r_str8(p)?;
        let public_source_dir = r_str8(p)?;
        let split_names = r_str8_array(p)?;
        let split_source_dirs = r_str8_array(p)?;
        let split_public_source_dirs = r_str8_array(p)?;
        r_null(p, "android.content.pm.ApplicationInfo.splitDependencies")?;
        let native_library_dir = r_str8(p)?;
        let secondary_native_library_dir = r_str8(p)?;
        let native_library_root_dir = r_str8(p)?;
        let native_library_root_requires_isa = r_bool(p)?;
        let primary_cpu_abi = r_str8(p)?;
        let secondary_cpu_abi = r_str8(p)?;
        let resource_dirs = r_str8_array(p)?;
        let overlay_paths = r_str8_array(p)?;
        let se_info = r_str8(p)?;
        let se_info_user = r_str8(p)?;
        let shared_library_files = r_str8_array(p)?;
        let shared_library_infos = r_typed_list(p)?;
        r_null(
            p,
            "android.content.pm.ApplicationInfo.optionalSharedLibraryInfos",
        )?;
        let data_dir = r_str8(p)?;
        let device_protected_data_dir = r_str8(p)?;
        let credential_protected_data_dir = r_str8(p)?;
        let uid = r_i32(p)?;
        let min_sdk_version = r_i32(p)?;
        let target_sdk_version = r_i32(p)?;
        let long_version_code = r_i64(p)?;
        let enabled = r_bool(p)?;
        let enabled_setting = r_i32(p)?;
        let install_location = r_i32(p)?;
        let manage_space_activity_name = r_str8(p)?;
        let backup_agent_name = r_str8(p)?;
        let description_res = r_i32(p)?;
        let ui_options = r_i32(p)?;
        let full_backup_content = r_i32(p)?;
        let data_extraction_rules_res = r_i32(p)?;
        let cross_profile = r_bool(p)?;
        let network_security_config_res = r_i32(p)?;
        let category = r_i32(p)?;
        let target_sandbox_version = r_i32(p)?;
        let class_loader_name = r_str8(p)?;
        let split_class_loader_names = r_str8_array(p)?;
        let compile_sdk_version = r_i32(p)?;
        let compile_sdk_version_codename = r_str8(p)?;
        let app_component_factory = r_str8(p)?;
        let icon_res = r_i32(p)?;
        let round_icon_res = r_i32(p)?;
        let hidden_api_policy = r_i32(p)?;
        let hidden_until_installed = r_bool(p)?;
        let zygote_preload_name = r_str8(p)?;
        let gwp_asan_mode = r_i32(p)?;
        let memtag_mode = r_i32(p)?;
        let native_heap_zero_initialized = r_i32(p)?;
        let request_raw_external_storage_access = r_opt_bool(p)?;
        let create_timestamp = r_i64(p)?;
        let n = r_i32(p)?;
        let app_class_names_by_process = (0..n.max(0))
            .map(|_| Ok((r_str16(p)?, r_str16(p)?)))
            .collect::<Result<Vec<_>>>()?;
        Ok(ApplicationInfo {
            item,
            task_affinity,
            permission,
            process_name,
            class_name,
            theme,
            flags,
            private_flags,
            private_flags_ext,
            requires_smallest_width_dp,
            compatible_width_limit_dp,
            largest_width_limit_dp,
            storage_uuid,
            scan_source_dir,
            scan_public_source_dir,
            source_dir,
            public_source_dir,
            split_names,
            split_source_dirs,
            split_public_source_dirs,
            native_library_dir,
            secondary_native_library_dir,
            native_library_root_dir,
            native_library_root_requires_isa,
            primary_cpu_abi,
            secondary_cpu_abi,
            resource_dirs,
            overlay_paths,
            se_info,
            se_info_user,
            shared_library_files,
            shared_library_infos,
            data_dir,
            device_protected_data_dir,
            credential_protected_data_dir,
            uid,
            min_sdk_version,
            target_sdk_version,
            long_version_code,
            enabled,
            enabled_setting,
            install_location,
            manage_space_activity_name,
            backup_agent_name,
            description_res,
            ui_options,
            full_backup_content,
            data_extraction_rules_res,
            cross_profile,
            network_security_config_res,
            category,
            target_sandbox_version,
            class_loader_name,
            split_class_loader_names,
            compile_sdk_version,
            compile_sdk_version_codename,
            app_component_factory,
            icon_res,
            round_icon_res,
            hidden_api_policy,
            hidden_until_installed,
            zygote_preload_name,
            gwp_asan_mode,
            memtag_mode,
            native_heap_zero_initialized,
            request_raw_external_storage_access,
            create_timestamp,
            app_class_names_by_process,
            locale_config_res: r_i32(p)?,
            allow_cross_uid_activity_switch_from_below: r_bool(p)?,
            page_size_app_compat_flags: r_i32(p)?,
            known_activity_embedding_certs: r_string_set(p)?,
        })
    }
}

crate::codec!(ApplicationInfo);
