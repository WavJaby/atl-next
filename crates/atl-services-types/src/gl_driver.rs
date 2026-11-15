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

/// What ANGLE's GL backend exposes to an app routed to it, beyond what the host GL answers. atld applies it to the device-wide features.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GlCeiling {
    /// android.hardware.opengles.aep is advertised only when true.
    pub aep: bool,
}

/// The ceiling of ANGLE's GL backend at the sources.toml pin. AEP is withheld: ANGLE's `CanSupportAEP` (Context.cpp) needs OES_sample_shading, EXT_copy_image, EXT_tessellation_shader and more, and the GL backend (renderer/gl/*.cpp) never sets sampleShadingOES, copyImageEXT, tessellationShaderEXT or primitiveBoundingBoxEXT.
/// Condition: patch 0011 (patches/aosp/frameworks/native) forces ANGLE's OpenGL ES platform, so any app may be routed to ANGLE, and AOSP keeps one device-wide feature list: an advertised feature the app's driver lacks breaks it, an unadvertised one only sends it to its fallback.
/// GLES version is not capped (owner decision 2026-11-16): GenerateCaps (renderergl_utils.cpp) starts from ES 3.1 and nothing raises it, so an ES1-path app on an ES 3.2 host is told 3.2 and gets 3.1. Capping the device-wide version would withhold 3.2 from every host-GL app.
/// Retire when ANGLE runs its Vulkan backend: then delete this constant and its uses in the same change.
pub const ANGLE_GL_CEILING: GlCeiling = GlCeiling { aep: false };

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

/// Why a package got its driver.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reason {
    /// Its gl-drivers.toml row.
    Owner,
    /// The ES1 rule: this native library of it DT_NEEDs [`ES1_LIB`].
    Es1(String),
    /// No row, no ES1 library: AOSP's default.
    Default,
    /// The GL selection gate's break (`--no-gl-selection`).
    SelectionOff,
}

/// A package's driver and why: what the services log per app process.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Selection {
    pub driver: GlDriver,
    pub reason: Reason,
}

impl std::fmt::Display for Selection {
    /// `ANGLE (es1: libX.so needs libGLESv1_CM.so)`, `native (default)`, ...
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let driver = match self.driver {
            GlDriver::Native => "native",
            GlDriver::Angle => "ANGLE",
        };
        match &self.reason {
            Reason::Owner => write!(f, "{driver} (gl-drivers.toml row)"),
            Reason::Es1(lib) => write!(f, "{driver} (es1: {lib} needs {ES1_LIB})"),
            Reason::Default => write!(f, "{driver} (default)"),
            Reason::SelectionOff => write!(f, "{driver} (selection off)"),
        }
    }
}

/// The driver of package `name`: its owner row, else ANGLE when `es1_library` names a native library of it that DT_NEEDs [`ES1_LIB`], else native.
pub fn select(
    owner: &BTreeMap<String, GlDriver>,
    name: &str,
    es1_library: Option<&str>,
) -> Selection {
    match (owner.get(name), es1_library) {
        (Some(&driver), _) => Selection {
            driver,
            reason: Reason::Owner,
        },
        (None, Some(lib)) => Selection {
            driver: GlDriver::Angle,
            reason: Reason::Es1(lib.to_owned()),
        },
        (None, None) => Selection {
            driver: GlDriver::Native,
            reason: Reason::Default,
        },
    }
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

    /// Patch 0011's added line forces the GL platform; a patch that stops doing so fails here. Dropping or renaming the file breaks the build (include_str!). The `+` filter: the commit message also names the token.
    #[test]
    fn angle_gl_backend_forced_by_patch_0011() {
        const PATCH: &str = include_str!(
            "../../../patches/aosp/frameworks/native/0011-feat-egl-request-ANGLE-s-OpenGL-ES-platform-on-Andro.patch"
        );
        assert!(
            PATCH.lines().any(|l| l.starts_with('+') && l.contains("EGL_PLATFORM_ANGLE_TYPE_OPENGLES_ANGLE")),
            "patch 0011 no longer adds the GL platform"
        );
    }

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
        let es1 = Some("libgame.so");
        assert_eq!(select(&owner, "a.b", es1).driver, GlDriver::Native);
        assert_eq!(select(&owner, "c.d", es1).driver, GlDriver::Angle);
        assert_eq!(select(&owner, "c.d", None).driver, GlDriver::Native);
    }

    /// The logged line names the driver and the rule, and for ES1 the library that matched.
    #[test]
    fn selection_names_its_reason() {
        let owner = owner_table(&row("a.b", "native", "r")).expect("one row");
        assert_eq!(
            select(&owner, "c.d", Some("libDoodleJump.so")).to_string(),
            "ANGLE (es1: libDoodleJump.so needs libGLESv1_CM.so)"
        );
        assert_eq!(select(&owner, "c.d", None).to_string(), "native (default)");
        assert_eq!(
            select(&owner, "a.b", None).to_string(),
            "native (gl-drivers.toml row)"
        );
    }
}
