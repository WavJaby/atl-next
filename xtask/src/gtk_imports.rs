// SPDX-License-Identifier: MIT
//! `xtask check` step gtk-imports: the UI module cdylibs import no GTK/GDK/GSK name outside the GTK 4.14 baseline (gtk-baseline.toml). Each cdylib is built in release from this checkout's sources, as the harness stages it, so the step reads current binaries and a runtime gate's build is reused.

use std::collections::BTreeSet;
use std::path::Path;

use anyhow::{Context, Result, ensure};
use object::read::elf::ElfFile64;
use object::{Endianness, Object, ObjectSymbol};

use crate::{cdylib, gtk_baseline, timing, unit_use};

/// The UI module cdylibs: the platform module the facade loads, and the web engine it dlopens on the first WebView call.
pub const MODULES: [&str; 2] = ["atl-platform-ui-linux", "atl-webview-linux"];

/// Builds each module and fails on the first one that imports a name the baseline does not export.
pub fn check(root: &Path, out: &Path) -> Result<()> {
    let baseline = gtk_baseline::symbols(root)?;
    let version = gtk_baseline::load(root)?.version;
    for package in MODULES {
        let (mut cmd, release) = cdylib::release_build(root, out, package)?;
        let status = timing::phase("cdylib build", || unit_use::status(&mut cmd))?;
        ensure!(status.success(), "building {package} failed");
        let lib = release.join(format!("lib{}.so", package.replace('-', "_")));
        let imports = gtk_imports(&lib)?;
        let outside = outside_baseline(&imports, &baseline);
        ensure!(
            outside.is_empty(),
            "{package} imports outside GTK {version} ({}): {}",
            gtk_baseline::FILE,
            outside.join(", ")
        );
        eprintln!(
            "gtk-imports: {package}: {} GTK/GDK/GSK imports, all in the GTK {version} set ({} names)",
            imports.len(),
            baseline.len()
        );
    }
    Ok(())
}

/// The GTK, GDK and GSK names `lib` imports: its undefined dynamic symbols with those prefixes.
fn gtk_imports(lib: &Path) -> Result<BTreeSet<String>> {
    let data = std::fs::read(lib).with_context(|| lib.display().to_string())?;
    let elf = ElfFile64::<Endianness>::parse(&*data)
        .with_context(|| format!("{}: not a 64-bit ELF", lib.display()))?;
    let mut names = BTreeSet::new();
    for sym in elf.dynamic_symbols().filter(|s| s.is_undefined()) {
        let name = sym.name()?;
        if is_gtk_name(name) {
            names.insert(name.to_owned());
        }
    }
    Ok(names)
}

/// A name in the GTK, GDK or GSK API. `gdk_pixbuf_*` is gdk-pixbuf's, another library.
pub fn is_gtk_name(name: &str) -> bool {
    (name.starts_with("gtk_") || name.starts_with("gdk_") || name.starts_with("gsk_"))
        && !name.starts_with("gdk_pixbuf_")
}

/// The imports the baseline does not export, sorted.
pub fn outside_baseline(imports: &BTreeSet<String>, baseline: &BTreeSet<String>) -> Vec<String> {
    imports.difference(baseline).cloned().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
    }

    #[test]
    fn a_4_16_name_is_outside_the_4_14_set() {
        // gtk_graphics_offload_set_black_background is GTK 4.16 API (its gtk-doc says "Since: 4.16"); the 4.14 set holds the offload constructor only
        let baseline = set(&["gtk_graphics_offload_new", "gtk_widget_show"]);
        let imports = set(&[
            "gtk_graphics_offload_new",
            "gtk_graphics_offload_set_black_background",
        ]);
        assert_eq!(
            outside_baseline(&imports, &baseline),
            ["gtk_graphics_offload_set_black_background"]
        );
    }

    #[test]
    fn a_baseline_name_passes() {
        let baseline = set(&["gtk_graphics_offload_new", "gtk_widget_show"]);
        let imports = set(&["gtk_widget_show"]);
        assert!(outside_baseline(&imports, &baseline).is_empty());
    }

    #[test]
    fn only_gtk_gdk_gsk_names_are_checked() {
        assert!(is_gtk_name("gtk_widget_show"));
        assert!(is_gtk_name("gdk_dmabuf_texture_builder_new"));
        assert!(is_gtk_name("gsk_color_node_new"));
        assert!(!is_gtk_name("gdk_pixbuf_new_from_file"));
        assert!(!is_gtk_name("g_object_ref"));
        assert!(!is_gtk_name("webkit_web_view_new"));
    }
}
