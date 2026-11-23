// SPDX-License-Identifier: MIT
//! The UI module cdylibs import no GTK/GDK/GSK name outside the GTK 4.14 baseline (gtk-baseline.toml). Two callers, one check ([`Baseline::check_lib`]):
//! `xtask check` step gtk-imports builds each cdylib in release for this out dir's host (x86_64 in check), as the harness stages it, so a runtime gate's build is reused;
//! a runtime run's staging (atld_run.rs, `stage_module` + `stage_webview`) checks the staged files, so a cross host's modules (riscv64, built only when a remote run stages them) are gated too and the run refuses a module importing a newer name.

use std::collections::BTreeSet;
use std::path::Path;

use anyhow::{Context, Result, ensure};
use object::read::elf::ElfFile64;
use object::{Endianness, Object, ObjectSymbol};

use crate::{cdylib, gtk_baseline, timing, unit_use};

/// The UI module cdylibs: the platform module the facade loads, and the web engine it dlopens on the first WebView call.
pub const MODULES: [&str; 2] = ["atl-platform-ui-linux", "atl-webview-linux"];

/// The cdylib file cargo writes (and the harness stages) for `package`.
pub fn lib_file(package: &str) -> String {
    format!("lib{}.so", package.replace('-', "_"))
}

/// The baseline's exported names and its version, loaded once per caller.
pub struct Baseline {
    names: BTreeSet<String>,
    version: String,
}

impl Baseline {
    pub fn load(root: &Path) -> Result<Self> {
        Ok(Self {
            names: gtk_baseline::symbols(root)?,
            version: gtk_baseline::load(root)?.version,
        })
    }

    /// Fails naming every GTK/GDK/GSK import of `lib` (cdylib of `package`) the baseline does not export; logs the count on a pass.
    pub fn check_lib(&self, package: &str, lib: &Path) -> Result<()> {
        let imports = gtk_imports(lib)?;
        let outside = outside_baseline(&imports, &self.names);
        ensure!(
            outside.is_empty(),
            "{package} imports outside GTK {} ({}): {}",
            self.version,
            gtk_baseline::FILE,
            outside.join(", ")
        );
        eprintln!(
            "gtk-imports: {package} ({}): {} GTK/GDK/GSK imports, all in the GTK {} set ({} names)",
            lib.display(),
            imports.len(),
            self.version,
            self.names.len()
        );
        Ok(())
    }
}

/// Builds each module and fails on the first one that imports a name the baseline does not export.
pub fn check(root: &Path, out: &Path) -> Result<()> {
    let baseline = Baseline::load(root)?;
    for package in MODULES {
        let (mut cmd, release) = cdylib::release_build(root, out, &[package])?;
        let status = timing::phase("cdylib build", || unit_use::status(&mut cmd))?;
        ensure!(status.success(), "building {package} failed");
        baseline.check_lib(package, &release.join(lib_file(package)))?;
    }
    Ok(())
}

/// Checks each module as staged in `dir` (file [`lib_file`]); a module missing there is an error, the staging put both.
pub fn check_staged(root: &Path, dir: &Path) -> Result<()> {
    let baseline = Baseline::load(root)?;
    for package in MODULES {
        baseline.check_lib(package, &dir.join(lib_file(package)))?;
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

    /// check_lib on real ELF files (the build machine's clang, as selfbind_gate.rs's test): a cdylib importing the 4.16 name is refused naming it; one importing only a baseline name passes.
    #[test]
    fn check_lib_refuses_a_built_library_importing_a_newer_name() {
        let dir = std::env::temp_dir().join(format!("gtk-imports-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let build = |name: &str, import: &str| {
            let src = dir.join(format!("{name}.c"));
            std::fs::write(
                &src,
                format!("void {import}(void); void call(void) {{ {import}(); }}\n"),
            )
            .unwrap();
            let so = dir.join(format!("{name}.so"));
            let status = std::process::Command::new("clang-19")
                .args(["-fuse-ld=lld", "-fPIC", "-shared"])
                .arg(&src)
                .arg("-o")
                .arg(&so)
                .status()
                .unwrap();
            assert!(status.success());
            so
        };
        let newer = build("newer", "gtk_graphics_offload_set_black_background");
        let ok = build("ok", "gtk_widget_show");
        let baseline = Baseline {
            names: set(&["gtk_graphics_offload_new", "gtk_widget_show"]),
            version: "4.14-test".to_owned(),
        };
        let refused = baseline.check_lib("newer", &newer).unwrap_err().to_string();
        let passed = baseline.check_lib("ok", &ok);
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(
            refused.ends_with(": gtk_graphics_offload_set_black_background"),
            "{refused}"
        );
        assert!(passed.is_ok());
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
