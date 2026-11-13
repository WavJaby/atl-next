// SPDX-License-Identifier: MIT
//! The GL driver each app process gets: AOSP's per-package switch (GraphicsEnvironment.queryAngleChoice reads `angle_gl_driver_selection_pkgs` / `_values`), decided by the owner list (gl-drivers.toml) and the install-time ES1 rule, then handed to app processes as coreSettings at bindApplication (CoreSettingsObserver's job in AOSP).
//! The install step stores the decision per package (atl-services `PackageRecord::gl_driver`); the bundle lists only the `angle` packages, so an unlisted package gets AOSP's default (native).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// The owner list, compiled in. The install step and `xtask check` read this same text.
pub const OWNER_TABLE: &str = include_str!("../../../gl-drivers.toml");
/// The Settings.Global keys (Settings.java:17016, :17024) the core settings carry, as CoreSettingsObserver mirrors them.
pub const SELECTION_PKGS: &str = "angle_gl_driver_selection_pkgs";
pub const SELECTION_VALUES: &str = "angle_gl_driver_selection_values";
/// AOSP's value for ANGLE (GraphicsEnvironment ANGLE_GL_DRIVER_CHOICE_ANGLE).
const ANGLE_VALUE: &str = "angle";
/// The library whose DT_NEEDED selects ANGLE: ES1 is served by ANGLE only, our driver has no ES1.
pub const ES1_LIB: &str = "libGLESv1_CM.so";

/// The driver an app process loads as its native EGL.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GlDriver {
    /// AOSP's default: libEGL loads our driver (`ro.hardware.egl=atl`).
    #[default]
    Native,
    /// libEGL loads ANGLE (Loader.cpp `load_angle`), whose GL backend runs over our driver.
    Angle,
}

#[derive(Deserialize)]
struct Table {
    #[serde(default)]
    package: Vec<Row>,
}

#[derive(Deserialize)]
struct Row {
    name: String,
    driver: GlDriver,
    reason: String,
}

/// Parses an owner list into package -> driver. Errors name the table and the bad row.
pub fn owner_table(text: &str) -> Result<BTreeMap<String, GlDriver>, String> {
    let table: Table = toml::from_str(text).map_err(|e| format!("gl-drivers.toml: {e}"))?;
    let mut rows = BTreeMap::new();
    for row in table.package {
        if !valid_package(&row.name) {
            return Err(format!(
                "gl-drivers.toml: `{}` is not a package name",
                row.name
            ));
        }
        if row.reason.trim().is_empty() {
            return Err(format!("gl-drivers.toml: {} has no reason", row.name));
        }
        if rows.insert(row.name.clone(), row.driver).is_some() {
            return Err(format!("gl-drivers.toml: {} listed twice", row.name));
        }
    }
    Ok(rows)
}

/// Lower-case Android package syntax: at least two dot-separated segments, each starting with a letter.
fn valid_package(name: &str) -> bool {
    let segments: Vec<&str> = name.split('.').collect();
    segments.len() >= 2
        && segments.iter().all(|s| {
            let mut c = s.chars();
            c.next().is_some_and(|f| f.is_ascii_lowercase())
                && c.all(|x| x.is_ascii_lowercase() || x.is_ascii_digit() || x == '_')
        })
}

/// The driver of package `name`: its owner row, else ANGLE when `needs_es1` (a native library DT_NEEDs [`ES1_LIB`]), else native.
pub fn select(owner: &BTreeMap<String, GlDriver>, name: &str, needs_es1: bool) -> GlDriver {
    owner.get(name).copied().unwrap_or(if needs_es1 {
        GlDriver::Angle
    } else {
        GlDriver::Native
    })
}

/// The values of [`SELECTION_PKGS`] and [`SELECTION_VALUES`] for bindApplication: the `angle` packages, index-matched (GraphicsEnvironment splits both on ','). None when no package is ANGLE, so no key is set.
pub fn selection<'a>(
    packages: impl IntoIterator<Item = (&'a str, GlDriver)>,
) -> Option<(String, String)> {
    let names: Vec<&str> = packages
        .into_iter()
        .filter(|(_, d)| *d == GlDriver::Angle)
        .map(|(n, _)| n)
        .collect();
    if names.is_empty() {
        return None;
    }
    Some((names.join(","), vec![ANGLE_VALUE; names.len()].join(",")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(name: &str, driver: &str, reason: &str) -> String {
        format!("[[package]]\nname = \"{name}\"\ndriver = \"{driver}\"\nreason = \"{reason}\"\n")
    }

    #[test]
    fn compiled_owner_table_parses() {
        let rows = owner_table(OWNER_TABLE).expect("gl-drivers.toml parses");
        assert_eq!(rows.get("com.noodlecake.happyjump"), Some(&GlDriver::Angle));
    }

    #[test]
    fn owner_table_rejects_bad_rows() {
        assert!(
            owner_table(&row("Bad.Name", "angle", "r")).is_err(),
            "upper case"
        );
        assert!(
            owner_table(&row("single", "angle", "r")).is_err(),
            "one segment"
        );
        assert!(
            owner_table(&row("a.b", "gles", "r")).is_err(),
            "unknown driver"
        );
        assert!(owner_table(&row("a.b", "angle", " ")).is_err(), "no reason");
        let twice = [row("a.b", "angle", "r"), row("a.b", "native", "r")].concat();
        assert!(owner_table(&twice).is_err(), "listed twice");
    }

    #[test]
    fn owner_row_overrides_es1_rule() {
        let owner = owner_table(&row("a.b", "native", "r")).expect("one row");
        assert_eq!(select(&owner, "a.b", true), GlDriver::Native);
        assert_eq!(select(&owner, "c.d", true), GlDriver::Angle);
        assert_eq!(select(&owner, "c.d", false), GlDriver::Native);
    }
}
