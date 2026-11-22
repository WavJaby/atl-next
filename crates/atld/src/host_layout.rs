// SPDX-License-Identifier: MIT
//! The data root's layout (`--data-root`, default `$XDG_DATA_HOME/atl`, kept across runs) and the one-time move of the old single device tree into it.
//! - `apps/<package>/`: one host root per package (maintainer layout 2026-10-29): `apk` (code, ro at `/data/app/<package>`), `data` (`/data/user/0/<package>`, its `cache`/`code_cache` inside as on AOSP),
//!   `data_de` (device-protected `/data/user_de/0/<package>`), `external/{data,obb}` (`/storage/emulated/0/Android/{data,obb}/<package>`). System packages' data lives here like any package's.
//! - `device/data/`: what is not yet per package: `system/` (SettingsProvider's /data/system), `dalvik-cache/`, `app/vmdl<session>.tmp` install staging (same filesystem as `apps/`, so commit is a rename).
//!   Later slices move them (system state -> `system/`, regenerable state -> the XDG cache root).
//! - `system/`: the services' own state, no Android path (atl-services `--system-dir`): `shortcut_service/`, user 0's shortcuts (atl_services::shortcut); `package-state.json`, user 0's component enabled settings (atl_services::package_state).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use atl_android_paths::valid_package_name;

/// One per-package dir: its name under `apps/<package>/` and where the old device tree (`device/`, before 2026-11-17's per-package layout) kept it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Item {
    Apk,
    Data,
    DataDe,
    External,
}

impl Item {
    pub const ALL: [Item; 4] = [Item::Apk, Item::Data, Item::DataDe, Item::External];

    pub fn name(self) -> &'static str {
        match self {
            Item::Apk => "apk",
            Item::Data => "data",
            Item::DataDe => "data_de",
            Item::External => "external",
        }
    }

    /// The parent dir of this item's per-package dirs in the old device tree, relative to the data root.
    fn legacy_parent(self) -> &'static str {
        match self {
            Item::Apk => "device/data/app",
            Item::Data => "device/data/user/0",
            Item::DataDe => "device/data/user_de/0",
            Item::External => "device/external",
        }
    }
}

/// `apps/<package>/<item>` under `data_root`.
pub fn package_dir(data_root: &Path, package: &str, item: Item) -> PathBuf {
    data_root.join("apps").join(package).join(item.name())
}

/// The tree not yet split per package: `device/data`, Android's /data minus the packages' dirs.
pub fn device_data(data_root: &Path) -> PathBuf {
    data_root.join("device").join("data")
}

/// `system`: the services' state dir (`--system-dir`), seen by no app.
pub fn system_dir(data_root: &Path) -> PathBuf {
    data_root.join("system")
}

/// What [`migrate`] did: per package, the items moved, and the packages refused because an item exists in both trees.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Migration {
    pub moved: Vec<(String, Vec<Item>)>,
    pub refused: Vec<(String, Vec<Item>)>,
}

/// Moves each package's dirs from the old device tree to `apps/<package>/` by `rename` (one filesystem: both under `data_root`); never copies, merges or deletes.
/// Per package: an item present in both trees refuses the whole package (nothing of it moves; the caller refuses to start it); else every item still in the old tree moves.
/// Idempotent: a tree a crash left half-moved is finished by the next call. Install staging dirs (`vmdl<n>.tmp`) are not packages.
/// Delete this once the maintainer's tree is migrated and confirmed: the old tree then holds no package dirs.
pub fn migrate(data_root: &Path) -> Result<Migration> {
    let mut packages = std::collections::BTreeSet::new();
    for item in Item::ALL {
        let parent = data_root.join(item.legacy_parent());
        let Ok(entries) = std::fs::read_dir(&parent) else {
            continue;
        };
        for e in entries {
            let name = e?.file_name().to_string_lossy().into_owned();
            let staging = item == Item::Apk && name.starts_with("vmdl") && name.ends_with(".tmp");
            if valid_package_name(&name) && !staging {
                packages.insert(name);
            }
        }
    }
    let mut m = Migration::default();
    for package in packages {
        let pending: Vec<(Item, PathBuf, PathBuf)> = Item::ALL
            .into_iter()
            .map(|i| {
                let from = data_root.join(i.legacy_parent()).join(&package);
                (i, from, package_dir(data_root, &package, i))
            })
            .filter(|(_, from, _)| from.exists())
            .collect();
        let both: Vec<Item> = pending
            .iter()
            .filter(|(_, _, to)| to.exists())
            .map(|(i, _, _)| *i)
            .collect();
        if !both.is_empty() {
            m.refused.push((package, both));
            continue;
        }
        for (_, from, to) in &pending {
            let parent = to.parent().context("package dir has no parent")?;
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
            std::fs::rename(from, to)
                .with_context(|| format!("moving {} to {}", from.display(), to.display()))?;
        }
        m.moved
            .push((package, pending.into_iter().map(|(i, _, _)| i).collect()));
    }
    Ok(m)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("atl-host-layout-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn plant(root: &Path, rel: &str, text: &str) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }

    fn read(root: &Path, rel: &str) -> Option<String> {
        std::fs::read_to_string(root.join(rel)).ok()
    }

    /// The old tree as app-data-xdg left it: each item moves to its apps/ name, content intact; staging is not a package and stays.
    #[test]
    fn clean_move() {
        let r = scratch("clean");
        plant(&r, "device/data/app/p.q/base.apk", "code");
        plant(&r, "device/data/user/0/p.q/files/save.txt", "save");
        plant(&r, "device/data/user_de/0/p.q/de.txt", "de");
        plant(&r, "device/external/p.q/data/x", "ext");
        plant(&r, "device/data/app/vmdl3.tmp/base.apk", "staged");
        plant(&r, "device/data/system/users/0/settings_global.xml", "s");
        let m = migrate(&r).unwrap();
        assert_eq!(m.moved, vec![("p.q".to_owned(), Item::ALL.to_vec())]);
        assert!(m.refused.is_empty());
        assert_eq!(read(&r, "apps/p.q/apk/base.apk").as_deref(), Some("code"));
        assert_eq!(
            read(&r, "apps/p.q/data/files/save.txt").as_deref(),
            Some("save")
        );
        assert_eq!(read(&r, "apps/p.q/data_de/de.txt").as_deref(), Some("de"));
        assert_eq!(read(&r, "apps/p.q/external/data/x").as_deref(), Some("ext"));
        assert!(!r.join("device/data/user/0/p.q").exists());
        assert!(r.join("device/data/app/vmdl3.tmp/base.apk").exists());
        assert!(
            r.join("device/data/system/users/0/settings_global.xml")
                .exists()
        );
        // a rerun finds nothing left
        assert_eq!(migrate(&r).unwrap(), Migration::default());
        std::fs::remove_dir_all(&r).unwrap();
    }

    /// An item in both trees: the package is refused, nothing of it moves or is deleted; another package still moves.
    #[test]
    fn both_present_refuses_that_package() {
        let r = scratch("both");
        plant(&r, "device/data/user/0/p.q/files/a", "old");
        plant(&r, "device/external/p.q/data/e", "old-ext");
        plant(&r, "apps/p.q/data/files/b", "new");
        plant(&r, "device/data/user/0/o.k/f", "ok");
        let m = migrate(&r).unwrap();
        assert_eq!(m.refused, vec![("p.q".to_owned(), vec![Item::Data])]);
        assert_eq!(m.moved, vec![("o.k".to_owned(), vec![Item::Data])]);
        assert_eq!(
            read(&r, "device/data/user/0/p.q/files/a").as_deref(),
            Some("old")
        );
        assert_eq!(
            read(&r, "device/external/p.q/data/e").as_deref(),
            Some("old-ext")
        );
        assert_eq!(read(&r, "apps/p.q/data/files/b").as_deref(), Some("new"));
        assert!(!r.join("apps/p.q/data/files/a").exists());
        assert!(!r.join("apps/p.q/external").exists());
        assert_eq!(read(&r, "apps/o.k/data/f").as_deref(), Some("ok"));
        std::fs::remove_dir_all(&r).unwrap();
    }

    /// A crash after some items moved: the rest move on the next start, no refusal (the moved items are gone from the old tree).
    #[test]
    fn half_migrated_rerun_finishes() {
        let r = scratch("half");
        plant(&r, "apps/p.q/apk/base.apk", "code");
        plant(&r, "apps/p.q/data/files/save.txt", "save");
        plant(&r, "device/data/user_de/0/p.q/de.txt", "de");
        plant(&r, "device/external/p.q/obb/o", "obb");
        let m = migrate(&r).unwrap();
        assert_eq!(
            m.moved,
            vec![("p.q".to_owned(), vec![Item::DataDe, Item::External])]
        );
        assert!(m.refused.is_empty());
        assert_eq!(read(&r, "apps/p.q/data_de/de.txt").as_deref(), Some("de"));
        assert_eq!(read(&r, "apps/p.q/external/obb/o").as_deref(), Some("obb"));
        assert_eq!(
            read(&r, "apps/p.q/data/files/save.txt").as_deref(),
            Some("save")
        );
        std::fs::remove_dir_all(&r).unwrap();
    }

    /// No old tree at all (a new user, or already migrated): nothing happens, nothing is created.
    #[test]
    fn nothing_to_migrate() {
        let r = scratch("none");
        assert_eq!(migrate(&r).unwrap(), Migration::default());
        assert_eq!(std::fs::read_dir(&r).unwrap().count(), 0);
        std::fs::remove_dir_all(&r).unwrap();
    }

    /// A package with only device-protected data (e.g. a direct-boot-aware app that never wrote CE data) is still found and moved.
    #[test]
    fn package_only_in_user_de() {
        let r = scratch("de-only");
        plant(&r, "device/data/user_de/0/d.e/x", "de");
        let m = migrate(&r).unwrap();
        assert_eq!(m.moved, vec![("d.e".to_owned(), vec![Item::DataDe])]);
        assert_eq!(read(&r, "apps/d.e/data_de/x").as_deref(), Some("de"));
        assert!(!r.join("apps/d.e/data").exists());
        std::fs::remove_dir_all(&r).unwrap();
    }
}
