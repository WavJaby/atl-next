// SPDX-License-Identifier: MIT
//! The system features SystemConfig reads (frameworks/base/services/core/java/com/android/server/SystemConfig.java) and PackageManagerService answers `hasSystemFeature` / `getSystemAvailableFeatures` from.
//! Source: every `*.xml` in `<partition>/etc/{sysconfig,permissions}/` of [`PARTITIONS`] (readAllPermissionsFromXml :684-777); each partition's flags allow features, so all count alike.
//! Per file (readPermissionsFromXml): the root is `<permissions>` or `<config>` (else the file is refused); `<feature name version>` adds the feature, a repeat keeps the higher version (addFeature :1850);
//! `<unavailable-feature name>` removes it after every file is read (readAllPermissions :679). Union, max and the final removal commute, so file and directory order (platform.xml last) do not change the set.
//! Not modelled: `notLowRam` (only filters when `ro.config.low_ram` is true, which ATL never sets), SKU subdirectories (`ro.boot.product.*.sku` unset), apex dirs (ATL ships no apex permission files).
//! A file AOSP would log and skip (bad root, unparsable XML) is an error here: every file under these dirs is ATL's own placement, so a bad one is a build bug, not a device quirk.
//! [`permissions_xml`] writes the same format, for the features atld derives from the host at start (vendor partition, as a device's hardware features come from its vendor build).

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{Context as _, Result, bail};

/// The partitions SystemConfig reads, under the runtime root (Environment.getRootDirectory, getVendorDirectory, getOdmDirectory, getOemDirectory, getProductDirectory, getSystemExtDirectory).
pub const PARTITIONS: [&str; 6] = ["system", "vendor", "odm", "oem", "product", "system_ext"];
/// The directories of each partition that hold permission files.
pub const DIRS: [&str; 2] = ["etc/sysconfig", "etc/permissions"];

/// The available features: name -> version (FeatureInfo.version; 0 when the file gives none).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Features(BTreeMap<String, i32>);

impl Features {
    /// Every permission file under `root`'s partitions; a missing partition or directory adds nothing (SystemConfig.readPermissions skips it).
    pub fn read(root: &Path) -> Result<Features> {
        let mut acc = Accumulator::default();
        for part in PARTITIONS {
            for dir in DIRS {
                let d = root.join(part).join(dir);
                let Ok(entries) = std::fs::read_dir(&d) else {
                    continue;
                };
                for e in entries {
                    let path = e?.path();
                    // readPermissions: only regular `.xml` files (metadata follows the symlinks atld places)
                    if path.extension().is_none_or(|x| x != "xml") || !path.is_file() {
                        continue;
                    }
                    let text = std::fs::read_to_string(&path)
                        .with_context(|| format!("reading {}", path.display()))?;
                    acc.file(&text)
                        .with_context(|| format!("permission file {}", path.display()))?;
                }
            }
        }
        Ok(acc.finish())
    }

    /// The set from file texts, as [`Features::read`] combines them.
    pub fn from_texts<'a>(texts: impl IntoIterator<Item = &'a str>) -> Result<Features> {
        let mut acc = Accumulator::default();
        for t in texts {
            acc.file(t)?;
        }
        Ok(acc.finish())
    }

    /// PackageManagerService.hasSystemFeature:2954: present at `version` or higher.
    pub fn has(&self, name: &str, version: i32) -> bool {
        self.0.get(name).is_some_and(|v| *v >= version)
    }

    /// (name, version) in name order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, i32)> {
        self.0.iter().map(|(n, v)| (n.as_str(), *v))
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

#[derive(Default)]
struct Accumulator {
    available: BTreeMap<String, i32>,
    unavailable: BTreeSet<String>,
}

impl Accumulator {
    fn file(&mut self, text: &str) -> Result<()> {
        let doc = roxmltree::Document::parse(text)?;
        let root = doc.root_element();
        let tag = root.tag_name().name();
        if tag != "permissions" && tag != "config" {
            bail!("root <{tag}>, expected <permissions> or <config>");
        }
        for e in root.children().filter(roxmltree::Node::is_element) {
            match e.tag_name().name() {
                "feature" => {
                    let name = e.attribute("name").context("<feature> without name")?;
                    // XmlUtils.readIntAttribute: absent or not an int -> the default 0
                    let version = e
                        .attribute("version")
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(0);
                    let v = self.available.entry(name.to_owned()).or_insert(version);
                    *v = (*v).max(version);
                }
                "unavailable-feature" => {
                    let name = e
                        .attribute("name")
                        .context("<unavailable-feature> without name")?;
                    self.unavailable.insert(name.to_owned());
                }
                // libraries, permissions, privapp lists, ...: other SystemConfig tables, not features
                _ => {}
            }
        }
        Ok(())
    }

    fn finish(mut self) -> Features {
        for name in &self.unavailable {
            self.available.remove(name);
        }
        Features(self.available)
    }
}

/// A permission file declaring `features` ((name, version); version 0 is written as no attribute, as AOSP's own files do). Names are Java identifiers and dots: nothing to escape, checked.
pub fn permissions_xml(features: &[(&str, i32)]) -> Result<String> {
    let mut s = String::from("<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<permissions>\n");
    for (name, version) in features {
        if name.is_empty()
            || !name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_')
        {
            bail!("feature name {name:?}: not a feature name");
        }
        if *version == 0 {
            s.push_str(&format!("    <feature name=\"{name}\" />\n"));
        } else {
            s.push_str(&format!(
                "    <feature name=\"{name}\" version=\"{version}\" />\n"
            ));
        }
    }
    s.push_str("</permissions>\n");
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// AOSP's android.hardware.touchscreen.xml body (frameworks/native/data/etc), license header dropped.
    const TOUCHSCREEN: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<!-- comment -->
<permissions>
    <feature name="android.hardware.touchscreen" />
    <feature name="android.hardware.faketouch" />
</permissions>"#;

    #[test]
    fn aosp_file_reads_its_features() {
        let f = Features::from_texts([TOUCHSCREEN]).unwrap();
        assert_eq!(
            f.iter().collect::<Vec<_>>(),
            [
                ("android.hardware.faketouch", 0),
                ("android.hardware.touchscreen", 0)
            ]
        );
        assert!(f.has("android.hardware.touchscreen", 0));
        // hasSystemFeature(name, 1) on a version-0 feature: false (0 >= 1 fails)
        assert!(!f.has("android.hardware.touchscreen", 1));
        assert!(!f.has("android.hardware.touchscreen.multitouch", 0));
    }

    /// Vulkan level files as a device ships two (level-0 and level-1): the higher version stays, whichever file comes first.
    #[test]
    fn repeat_keeps_the_higher_version_in_any_order() {
        let l0 = r#"<permissions><feature name="android.hardware.vulkan.level" version="0" /></permissions>"#;
        let l1 = r#"<permissions><feature name="android.hardware.vulkan.level" version="1" /></permissions>"#;
        for texts in [[l0, l1], [l1, l0]] {
            let f = Features::from_texts(texts).unwrap();
            assert!(f.has("android.hardware.vulkan.level", 1));
            assert!(!f.has("android.hardware.vulkan.level", 2));
        }
    }

    #[test]
    fn unavailable_removes_after_all_files_and_bad_version_is_zero() {
        let drop = r#"<config><unavailable-feature name="android.hardware.faketouch" /></config>"#;
        let bad = r#"<permissions><feature name="x.y" version="one" /></permissions>"#;
        // the removal comes first in file order and still wins
        let f = Features::from_texts([drop, TOUCHSCREEN, bad]).unwrap();
        assert!(!f.has("android.hardware.faketouch", 0));
        assert!(f.has("android.hardware.touchscreen", 0));
        assert!(f.has("x.y", 0) && !f.has("x.y", 1));
    }

    #[test]
    fn foreign_root_and_nameless_feature_are_refused() {
        assert!(Features::from_texts(["<manifest><feature name=\"a\"/></manifest>"]).is_err());
        assert!(Features::from_texts(["<permissions><feature /></permissions>"]).is_err());
        assert!(Features::from_texts(["<permissions>"]).is_err());
    }

    #[test]
    fn written_file_reads_back() {
        let text =
            permissions_xml(&[("android.hardware.opengles.aep", 0), ("a.level", 2)]).unwrap();
        let f = Features::from_texts([text.as_str()]).unwrap();
        assert_eq!(
            f.iter().collect::<Vec<_>>(),
            [("a.level", 2), ("android.hardware.opengles.aep", 0)]
        );
        assert!(permissions_xml(&[("a\" />", 0)]).is_err());
    }

    /// The directory walk: partitions and both dirs, non-xml files and missing partitions skipped.
    #[test]
    fn read_walks_the_partitions() {
        let root = std::env::temp_dir().join(format!("atl-system-config-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let sys = root.join("system/etc/permissions");
        let ven = root.join("vendor/etc/sysconfig");
        std::fs::create_dir_all(&sys).unwrap();
        std::fs::create_dir_all(&ven).unwrap();
        std::fs::write(sys.join("android.hardware.touchscreen.xml"), TOUCHSCREEN).unwrap();
        std::fs::write(sys.join("notes.txt"), "<not xml").unwrap();
        std::fs::write(
            ven.join("atl.xml"),
            permissions_xml(&[("android.hardware.opengles.aep", 0)]).unwrap(),
        )
        .unwrap();
        let f = Features::read(&root).unwrap();
        assert_eq!(f.len(), 3);
        assert!(f.has("android.hardware.opengles.aep", 0));
        std::fs::remove_dir_all(&root).unwrap();
    }
}
