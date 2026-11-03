// SPDX-License-Identifier: MIT
//! The installed packages the services know and the process starts they expect. M2: given by the host (atld's broker later, from the APK parser); one user (0).

/// One installed package: what ApplicationInfo / PackageInfo / the launch need.
#[derive(Debug, Clone, PartialEq)]
pub struct PackageRecord {
    pub name: String,
    pub uid: u32,
    /// base.apk path (ApplicationInfo.sourceDir)
    pub source_dir: String,
    /// credential-protected storage (ApplicationInfo.dataDir, credentialProtectedDataDir: /data/user/<user>/<package>)
    pub data_dir: String,
    /// device-protected storage (ApplicationInfo.deviceProtectedDataDir: /data/user_de/<user>/<package>): the code cache dir, which graphics setup opens first
    pub device_protected_data_dir: String,
    pub version_code: i64,
    pub version_name: Option<String>,
    pub min_sdk: i32,
    pub target_sdk: i32,
    /// the launcher activity's class name
    pub launch_activity: String,
}

impl PackageRecord {
    /// The process name: the package name (ApplicationInfo.processName default when the manifest names none).
    pub fn process_name(&self) -> &str {
        &self.name
    }
}

/// A process start the services expect (ProcessList.mPendingStarts in AOSP): the broker spawned `package` as `uid` with `start_seq`.
#[derive(Debug, Clone, PartialEq)]
pub struct PendingStart {
    pub start_seq: i64,
    pub package: String,
    pub uid: u32,
}

/// The package set.
#[derive(Debug, Clone, Default)]
pub struct Packages {
    list: Vec<PackageRecord>,
}

impl Packages {
    pub fn new(list: Vec<PackageRecord>) -> Self {
        Packages { list }
    }

    pub fn by_name(&self, name: &str) -> Option<&PackageRecord> {
        self.list.iter().find(|p| p.name == name)
    }

    /// Packages whose uid has `app_id` (shared uids give several).
    pub fn by_app_id(&self, app_id: u32) -> impl Iterator<Item = &PackageRecord> {
        self.list
            .iter()
            .filter(move |p| crate::permission::app_id(p.uid) == app_id)
    }

    /// What `caller` may see of `package` (ComputerEngine shouldFilterApplication): the system everything, an app its own package only (no <queries> yet).
    pub fn visible(&self, caller: u32, package: &str) -> Option<&PackageRecord> {
        let p = self.by_name(package)?;
        let own = crate::permission::app_id(p.uid) == crate::permission::app_id(caller);
        (crate::permission::is_system_or_root(caller) || own).then_some(p)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(name: &str, uid: u32) -> PackageRecord {
        PackageRecord {
            name: name.into(),
            uid,
            source_dir: String::new(),
            data_dir: String::new(),
            device_protected_data_dir: String::new(),
            version_code: 1,
            version_name: None,
            min_sdk: 21,
            target_sdk: 34,
            launch_activity: String::new(),
        }
    }

    #[test]
    fn visibility() {
        let p = Packages::new(vec![record("a", 10_001), record("b", 10_002)]);
        assert!(p.visible(10_001, "a").is_some());
        assert!(p.visible(10_001, "b").is_none());
        assert!(p.visible(1000, "b").is_some());
        assert!(p.visible(1000, "c").is_none());
        assert_eq!(p.by_app_id(10_002).count(), 1);
    }
}
