// SPDX-License-Identifier: MIT
//! The preinstalled ("system") packages PackageManager scans at start: which directories, in which order, with which flags. One table read by atld (which runs the installer over each dir and
//! checks a scan request names one) and by the services (which ask for each and set the flags).
//! Order = InitAppsHelper.scanSystemDirs (services/core/java/com/android/server/pm/InitAppsHelper.java:317-350): per partition in PackagePartitions.SYSTEM_PARTITIONS order
//! (core/java/android/content/pm/PackagePartitions.java:68-87), `priv-app/` (SCAN_AS_PRIVILEGED) then `app/`; within a dir by name (AOSP's listFiles order is unspecified; sorted = the same ids every run).
//! Rows only where a consumer needs one (system, product, system_ext); vendor, odm and oem hold no APKs here. `/system/framework` (package `android`) is not scanned yet: its permission
//! definitions are a separate services input until it is.
//! Entries are directories `<partition>/{priv-app,app}/<Name>/` as Soong installs them (`<Name>/<Name>.apk` + splits); a bare `.apk` file (which scanDir also accepts) is skipped and counted.

use std::path::{Path, PathBuf};

/// ApplicationInfo.FLAG_SYSTEM (ApplicationInfo.java:189): every scanned package.
pub const FLAG_SYSTEM: i32 = 1;
/// ApplicationInfo.PRIVATE_FLAG_PRIVILEGED (:535): `priv-app/` (and `/system/framework`).
pub const PRIVATE_FLAG_PRIVILEGED: i32 = 1 << 3;
/// ApplicationInfo.PRIVATE_FLAG_PRODUCT (:667).
pub const PRIVATE_FLAG_PRODUCT: i32 = 1 << 19;
/// ApplicationInfo.PRIVATE_FLAG_SYSTEM_EXT (:681).
pub const PRIVATE_FLAG_SYSTEM_EXT: i32 = 1 << 21;

/// A scanned partition: its name under the runtime root (= its Android path without `/`) and the private flag ScanPackageUtils sets from its scan flag (system: none).
pub struct Partition {
    pub name: &'static str,
    pub private_flag: i32,
}

/// The scanned partitions, in PackagePartitions.SYSTEM_PARTITIONS order (every row has a priv-app dir: containsPrivApp is true for all three).
pub const PARTITIONS: [Partition; 3] = [
    Partition {
        name: "system",
        private_flag: 0,
    },
    Partition {
        name: "product",
        private_flag: PRIVATE_FLAG_PRODUCT,
    },
    Partition {
        name: "system_ext",
        private_flag: PRIVATE_FLAG_SYSTEM_EXT,
    },
];

/// One package dir to scan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanDir {
    /// Below the runtime root, e.g. `product/app/Fixture`; its Android path is `/` + this.
    pub rel: PathBuf,
    /// ApplicationInfo.flags to add.
    pub flags: i32,
    /// ApplicationInfo.privateFlags to add.
    pub private_flags: i32,
}

impl ScanDir {
    /// The dir as the sandbox sees it (`/product/app/Fixture`).
    pub fn android_path(&self) -> String {
        format!("/{}", self.rel.display())
    }
}

/// What a scan of `root` finds: the package dirs in scan order, and the entries skipped (not a directory).
#[derive(Debug, Default)]
pub struct Scan {
    pub dirs: Vec<ScanDir>,
    pub skipped: Vec<PathBuf>,
}

/// The package dirs under the runtime root `root` (host path), in scan order. A missing partition or folder adds nothing. Symlinks are followed (atld `--file` places links).
pub fn scan(root: &Path) -> std::io::Result<Scan> {
    let mut out = Scan::default();
    for p in &PARTITIONS {
        for (folder, privileged) in [("priv-app", PRIVATE_FLAG_PRIVILEGED), ("app", 0)] {
            let rel = Path::new(p.name).join(folder);
            let Ok(entries) = std::fs::read_dir(root.join(&rel)) else {
                continue;
            };
            let mut names = Vec::new();
            for e in entries {
                names.push(e?.file_name());
            }
            names.sort();
            for n in names {
                let rel = rel.join(&n);
                if root.join(&rel).is_dir() {
                    out.dirs.push(ScanDir {
                        rel,
                        flags: FLAG_SYSTEM,
                        private_flags: privileged | p.private_flag,
                    });
                } else {
                    out.skipped.push(rel);
                }
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Order and flags follow InitAppsHelper.scanSystemDirs: system before product before system_ext, priv-app before app, by name within a folder; a file entry is skipped.
    #[test]
    fn scan_order_and_flags() {
        let root = std::env::temp_dir().join(format!("atl-scan-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for d in [
            "system_ext/app/E",
            "product/app/B",
            "product/app/A",
            "product/priv-app/P",
            "system/app/S",
            "vendor/app/V",
        ] {
            std::fs::create_dir_all(root.join(d)).expect("mkdir");
        }
        std::fs::write(root.join("product/app/loose.apk"), b"").expect("file");
        let s = scan(&root).expect("scan");
        let got: Vec<(String, i32, i32)> = s
            .dirs
            .iter()
            .map(|d| (d.rel.display().to_string(), d.flags, d.private_flags))
            .collect();
        // flag values: ApplicationInfo.java FLAG_SYSTEM 1<<0, PRIVATE_FLAG_PRIVILEGED 1<<3, PRODUCT 1<<19, SYSTEM_EXT 1<<21
        let want = [
            ("system/app/S", 1, 0),
            ("product/priv-app/P", 1, (1 << 3) | (1 << 19)),
            ("product/app/A", 1, 1 << 19),
            ("product/app/B", 1, 1 << 19),
            ("system_ext/app/E", 1, 1 << 21),
        ];
        assert_eq!(
            got,
            want.iter()
                .map(|(p, f, pf)| (p.to_string(), *f, *pf))
                .collect::<Vec<_>>()
        );
        assert_eq!(s.skipped, vec![PathBuf::from("product/app/loose.apk")]);
        std::fs::remove_dir_all(&root).expect("cleanup");
    }
}
