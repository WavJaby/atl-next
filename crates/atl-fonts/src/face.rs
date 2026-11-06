// SPDX-License-Identifier: MIT
//! What a face is, read from the file's own tables (read-fonts): OS/2 `usWeightClass` + italic bit, `post.isFixedPitch`, `fvar` axes, name ID 6.
//! fontconfig is not asked: its weights are its own scale (regular 80, bold 200) and the conversion to OpenType lives only in its C code.

use crate::Error;

/// One `fvar` axis.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Axis {
    pub tag: [u8; 4],
    pub min: f32,
    pub max: f32,
}

/// The tables FontListParser's attributes come from.
#[derive(Debug, Clone, PartialEq)]
pub struct FaceInfo {
    pub weight: u16,
    pub italic: bool,
    pub monospaced: bool,
    /// Name ID 6; the file stem when the font has none.
    pub postscript: Option<String>,
    pub axes: Vec<Axis>,
}

impl FaceInfo {
    pub fn axis(&self, tag: &[u8; 4]) -> Option<Axis> {
        self.axes.iter().copied().find(|a| &a.tag == tag)
    }
}

/// Reads face `index` of `data`.
pub fn read(data: &[u8], index: u32) -> Result<FaceInfo, Error> {
    use read_fonts::TableProvider;
    use read_fonts::tables::os2::SelectionFlags;
    let bad = |e: read_fonts::ReadError| Error::Face(e.to_string());
    let f = read_fonts::FontRef::from_index(data, index).map_err(bad)?;
    let postscript = f.name().ok().and_then(|n| {
        n.name_record()
            .iter()
            .filter(|r| r.name_id() == read_fonts::types::NameId::POSTSCRIPT_NAME)
            .find_map(|r| {
                r.string(n.string_data())
                    .ok()
                    .map(|s| s.chars().collect::<String>())
            })
    });
    // no fvar = a static face
    let axes = match f.fvar() {
        Ok(fvar) => fvar
            .axes()
            .map_err(bad)?
            .iter()
            .map(|a| Axis {
                tag: a.axis_tag().to_be_bytes(),
                min: a.min_value().to_f32(),
                max: a.max_value().to_f32(),
            })
            .collect(),
        Err(_) => vec![],
    };
    let os2 = f.os2().map_err(bad)?;
    Ok(FaceInfo {
        weight: os2.us_weight_class(),
        italic: os2.fs_selection().contains(SelectionFlags::ITALIC),
        monospaced: f.post().map_err(bad)?.is_fixed_pitch() != 0,
        postscript,
        axes,
    })
}
