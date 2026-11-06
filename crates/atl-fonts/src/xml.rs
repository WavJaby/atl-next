// SPDX-License-Identifier: MIT
//! The model as FontListParser's XML (frameworks/base/graphics/java/android/graphics/FontListParser.java).
//! font_fallback.xml takes a variable family in the shapes SystemFonts.resolveVarFamilyType accepts (one font wght[,ital], or upright + italic fonts wght): `supportedAxes` (readFont 397-407), weight from the axis at draw time.
//! Every other variable face, and every variable face in the legacy fonts.xml (whose readers know no supportedAxes), becomes static instances at each multiple of 100 inside its wght range, each with a fixed `<axis tag="wght" stylevalue>` (readAxis 460-469), as AOSP's own fonts.xml lays out Roboto.
//! `postScriptName` is always written: its default is the file text minus 4 chars (readFont 409-413), wrong for a `<k>/` path.

use std::fmt::Write;

use crate::{Family, Font, Model};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Flavor {
    Fallback,
    Legacy,
}

const WGHT: [u8; 4] = *b"wght";
const ITAL: [u8; 4] = *b"ital";

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn path(f: &Font) -> String {
    esc(&format!("{}/{}", f.dir, f.file))
}

fn postscript(f: &Font) -> String {
    let stem = f.file.rsplit_once('.').map_or(f.file.as_str(), |(s, _)| s);
    esc(f.info.postscript.as_deref().unwrap_or(stem))
}

fn style(italic: bool) -> &'static str {
    if italic { "italic" } else { "normal" }
}

fn index(f: &Font) -> String {
    if f.index == 0 {
        String::new()
    } else {
        format!(" index=\"{}\"", f.index)
    }
}

/// The supportedAxes value of each font when the family fits resolveVarFamilyType's variable shapes, else `None`.
fn variable_shape(fam: &Family) -> Option<Vec<&'static str>> {
    let all_wght = fam.fonts.iter().all(|f| f.info.axis(&WGHT).is_some());
    let ital = |f: &Font| f.info.axis(&ITAL).is_some();
    match fam.fonts.as_slice() {
        [f] if all_wght => Some(vec![if ital(f) { "wght,ital" } else { "wght" }]),
        [a, b] if all_wght && !ital(a) && !ital(b) && a.info.italic != b.info.italic => {
            Some(vec!["wght", "wght"])
        }
        _ => None,
    }
}

fn font_lines(out: &mut String, f: &Font, axes: Option<&str>) {
    let (p, ps, ix) = (path(f), postscript(f), index(f));
    if let Some(axes) = axes {
        let _ = writeln!(
            out,
            "        <font style=\"{}\"{ix} postScriptName=\"{ps}\" supportedAxes=\"{axes}\">{p}</font>",
            style(f.info.italic)
        );
        return;
    }
    let Some(w) = f.info.axis(&WGHT) else {
        let _ = writeln!(
            out,
            "        <font weight=\"{}\" style=\"{}\"{ix} postScriptName=\"{ps}\">{p}</font>",
            f.info.weight,
            style(f.info.italic)
        );
        return;
    };
    let italics: &[bool] = match f.info.axis(&ITAL) {
        Some(a) if a.max >= 1.0 => &[false, true],
        _ => &[f.info.italic],
    };
    for &it in italics {
        for weight in (100..=900)
            .step_by(100)
            .filter(|&x| (w.min..=w.max).contains(&(x as f32)))
        {
            let ital_axis = if f.info.axis(&ITAL).is_some() {
                format!(
                    "\n            <axis tag=\"ital\" stylevalue=\"{}\"/>",
                    u8::from(it)
                )
            } else {
                String::new()
            };
            let _ = writeln!(
                out,
                "        <font weight=\"{weight}\" style=\"{}\"{ix} postScriptName=\"{ps}\">{p}\n            <axis tag=\"wght\" stylevalue=\"{weight}\"/>{ital_axis}\n        </font>",
                style(it)
            );
        }
    }
}

pub fn write(m: &Model, flavor: Flavor) -> String {
    let mut out = String::from("<?xml version=\"1.0\" encoding=\"utf-8\"?>\n");
    out.push_str(match flavor {
        Flavor::Fallback => "<familyset>\n",
        Flavor::Legacy => "<familyset version=\"23\">\n",
    });
    for fam in &m.families {
        match (&fam.name, &fam.lang) {
            (Some(n), _) => {
                let _ = writeln!(out, "    <family name=\"{}\">", esc(n));
            }
            (None, Some(l)) => {
                let _ = writeln!(out, "    <family lang=\"{}\">", esc(l));
            }
            (None, None) => out.push_str("    <family>\n"),
        }
        let shape = if flavor == Flavor::Fallback {
            variable_shape(fam)
        } else {
            None
        };
        for (i, f) in fam.fonts.iter().enumerate() {
            font_lines(&mut out, f, shape.as_ref().map(|s| s[i]));
        }
        out.push_str("    </family>\n");
    }
    for a in &m.aliases {
        let w = a
            .weight
            .as_ref()
            .map(|w| format!(" weight=\"{}\"", esc(w)))
            .unwrap_or_default();
        let _ = writeln!(
            out,
            "    <alias name=\"{}\" to=\"{}\"{w} />",
            esc(&a.name),
            esc(&a.to)
        );
    }
    out.push_str("</familyset>\n");
    out
}
