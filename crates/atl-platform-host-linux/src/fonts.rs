// SPDX-License-Identifier: MIT
//! The desktop's fonts through fontconfig (crate `fontconfig`, linked: fontconfig is a required system library). The config is the one fontconfig itself picks (`FONTCONFIG_FILE`, else the system's `fonts.conf`), read once at [`Fontconfig::new`].
//! Named instances of a variable font come back as extra patterns with the instance number in the index's high 16 bits; they are folded into their face (the file's own `fvar` describes them).
//! Languages are matched by listing with `lang=` (FcFontList keeps faces whose langset holds it), never by `Pattern::lang_set`: crate 0.11.0 does not destroy the FcStrSet that call creates.

use std::collections::BTreeSet;
use std::ffi::CString;
use std::path::PathBuf;

use atl_platform_host_api::{Face, FontError, FontQuery, FontSource};
use fontconfig::{
    FC_FAMILY, FC_FILE, FC_INDEX, FC_LANG, FC_WIDTH, Fontconfig, ObjectSet, Pattern,
    UnicodeCoverage,
};

/// fontconfig initialised with the config it finds.
pub struct FcFonts {
    fc: Fontconfig,
}

impl FcFonts {
    /// Initialises fontconfig. `Err`: it refused (unreadable config).
    pub fn new() -> Result<Self, FontError> {
        Fontconfig::new()
            .map(|fc| Self { fc })
            .ok_or_else(|| FontError("FcInit failed".into()))
    }

    fn pattern(&self, q: &FontQuery) -> Result<Pattern<'_>, FontError> {
        let mut p = Pattern::new(&self.fc).map_err(err)?;
        if !q.family.is_empty() {
            p.add_string(FC_FAMILY, &cstr(&q.family)?).map_err(err)?;
        }
        if let Some(l) = &q.lang {
            p.add_string(FC_LANG, &cstr(l)?).map_err(err)?;
        }
        if let Some(w) = q.width {
            p.add_integer(FC_WIDTH, w).map_err(err)?;
        }
        Ok(p)
    }
}

fn err(e: impl std::fmt::Display) -> FontError {
    FontError(e.to_string())
}

fn cstr(s: &str) -> Result<CString, FontError> {
    CString::new(s).map_err(err)
}

/// The face of a pattern; `None` for one without a file (fontconfig lists only file-backed fonts from a config's dirs, but a pattern may lack the object).
fn face(p: &Pattern<'_>) -> Option<Face> {
    let path = PathBuf::from(p.get_string(FC_FILE).ok()?);
    let index = u32::try_from(p.get_int(FC_INDEX).unwrap_or(0)).ok()? & 0xffff;
    let family = p.get_string(FC_FAMILY).unwrap_or_default().to_owned();
    let width = p.get_int(FC_WIDTH).unwrap_or(100);
    Some(Face {
        path,
        index,
        family,
        width,
    })
}

/// Faces in order, each (file, face index) once: named instances fold into their face.
fn unique(faces: impl Iterator<Item = Face>) -> Vec<Face> {
    let mut seen = BTreeSet::new();
    faces
        .filter(|f| seen.insert((f.path.clone(), f.index)))
        .collect()
}

impl FontSource for FcFonts {
    fn sorted(&self, q: &FontQuery) -> Result<Vec<Face>, FontError> {
        let mut p = self.pattern(q)?;
        let set = p.sort_fonts(UnicodeCoverage::NoTrim).map_err(err)?;
        Ok(unique(set.iter().filter_map(|p| face(&p))))
    }

    fn list(&self, q: &FontQuery) -> Result<Vec<Face>, FontError> {
        let p = self.pattern(q)?;
        let mut os = ObjectSet::new(&self.fc).map_err(err)?;
        for o in [FC_FILE, FC_INDEX, FC_FAMILY, FC_WIDTH] {
            os.add(o).map_err(err)?;
        }
        let set = fontconfig::list_fonts(&p, Some(&os)).map_err(err)?;
        let mut faces = unique(set.iter().filter_map(|p| face(&p)));
        faces.sort();
        Ok(faces)
    }
}
