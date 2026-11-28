// SPDX-License-Identifier: MIT
//! The real-desktop font gate (`xtask test host-fonts`, a check gates row): a CJK and an emoji code point each resolve to a real font file through the product's own font list, built as atld builds it (atl_fonts::build over the platform layer's fontconfig, the desktop's own config, AOSP's alias/fallback data at the pin).
//! Resolution = minikin's fallback walk over font_fallback.xml: families in document order, the first face whose cmap maps the code point; for an Emoji_Presentation code point a family whose lang has script Zsye first (FontCollection's emoji-style score), so a text font's monochrome glyph (DejaVu Sans maps U+1F431) does not answer for the emoji font. Other locale scoring is not modelled. The cmap is read here with read-fonts, not through atl-fonts.
//! Three verdicts: ok (family + file per code point); FAIL = the desktop has a face for the code point (fontconfig lists it, for emoji under lang `und-zsye`; cmap read here) but the product's list resolves none; "host lacks a <group> font" = no listed face: a skip check names, not a product failure and not a pass.
//! Breaks (each must leave the ok verdict): `empty-dir` re-runs the gate under a fontconfig config whose only dir is empty (this harness's fixture, FONTCONFIG_FILE on the child) -> host lacks both; `no-fallback` builds the list with an empty fallback order -> FAIL (the host still has the faces).

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use atl_layout::Layout;
use atl_platform_host::{FontQuery, FontSource, PlatformFonts};

use crate::tree;

/// One checked code point: the group the "host lacks" verdict names (with its article), and for an emoji-presentation point the AOSP script its family's lang must carry + fontconfig's lang id for the desktop probe.
pub struct Point {
    pub c: char,
    pub group: &'static str,
    pub article: &'static str,
    pub style: Option<(&'static str, &'static str)>,
}

/// U+8C93 貓 (cat): the Traditional-only form (Simplified 猫) of Battle Cats TW's title 貓咪大戰爭, a CJK app whose text rendered as boxes when the desktop's CJK faces did not reach the font list; only a CJK font maps it.
/// U+1F431 🐱 (cat face): Unicode 6.0, Emoji_Presentation=Yes (emoji-data.txt); AOSP's emoji fallback is lang `und-Zsye` (fallback_order.json), fontconfig's id for it `und-zsye` (fc-lang und_zsye.orth).
pub const POINTS: [Point; 2] = [
    Point {
        c: '\u{8C93}',
        group: "CJK",
        article: "a",
        style: None,
    },
    Point {
        c: '\u{1F431}',
        group: "emoji",
        article: "an",
        style: Some(("Zsye", "und-zsye")),
    },
];

/// AOSP's font data at the pin, as atld's `--font-data`.
const FONT_DATA: &str = "frameworks/base/data/fonts";

#[derive(clap::Args)]
pub struct Args {
    /// Deliberate break that must leave the ok verdict: `empty-dir` (host lacks both) or `no-fallback` (FAIL on both).
    #[arg(long = "break", value_enum)]
    brk: Option<Break>,
    /// Work dir for the empty-dir fixture (default `<workspace>/out/host-fonts`).
    #[arg(long)]
    out: Option<PathBuf>,
}

#[derive(Clone, Copy, PartialEq, clap::ValueEnum)]
enum Break {
    EmptyDir,
    NoFallback,
}

/// One code point's outcome.
enum Outcome {
    /// family description, file, face index
    Resolved(String, PathBuf, u32),
    /// a desktop face covering it that the product's list does not reach
    Unreached(PathBuf),
    HostLacks,
}

/// `xtask test host-fonts`: prints the verdict line; FAIL exits nonzero, "host lacks" exits 0 and says so.
pub fn run(root: &Path, args: &Args) -> Result<std::process::ExitCode> {
    if args.brk == Some(Break::EmptyDir) {
        let dir = match &args.out {
            Some(d) => d.clone(),
            None => Layout::of(root)?.workspace().join("out/host-fonts"),
        };
        let conf = empty_conf(&dir)?;
        // fontconfig reads FONTCONFIG_FILE once, at FcInit: a fresh process is the only way to give it another config
        let status = Command::new(std::env::current_exe()?)
            .args(["test", "host-fonts"])
            .env("FONTCONFIG_FILE", &conf)
            .status()
            .context("re-running host-fonts under the empty config")?;
        return Ok(std::process::ExitCode::from(
            u8::try_from(status.code().unwrap_or(1)).unwrap_or(1),
        ));
    }
    // "host lacks": the row's skip line (check_gates.rs reads it) + an ok verdict naming it
    if let Some(why) = check(root, args.brk == Some(Break::NoFallback))? {
        println!("{}{why}", xtask::harness::SKIP_PREFIX);
        println!("host-fonts: ok (skipped: {why})");
    }
    Ok(std::process::ExitCode::SUCCESS)
}

/// The check step: `Some(why)` = host lacks a font (named skip); an error = FAIL.
pub fn check(root: &Path, no_fallback: bool) -> Result<Option<String>> {
    let data_dir = tree::shared_tree(&Layout::of(root)?).join(FONT_DATA);
    let data = |f: &str| {
        let p = data_dir.join(f);
        std::fs::read_to_string(&p).with_context(|| p.display().to_string())
    };
    let src = PlatformFonts::new()?;
    let order = if no_fallback {
        Vec::new()
    } else {
        atl_fonts_table::parse_order(&data("fallback_order.json")?)?
    };
    let model = match atl_fonts::build(
        &src,
        &atl_fonts::FileReader,
        &atl_fonts_table::Table::parse(atl_fonts_table::FONTS_TOML)?,
        atl_fonts_table::parse_aliases(&data("alias.json")?)?,
        &order,
    ) {
        Ok(m) => m,
        // atld refuses to start (no sans-serif face): no family resolves anything, and the desktop probe below tells a fontless host from a product miss
        Err(e @ atl_fonts::Error::Required(..)) => {
            eprintln!("host-fonts: font list not built: {e}");
            atl_fonts::Model {
                families: Vec::new(),
                aliases: Vec::new(),
                dirs: Vec::new(),
                missing: atl_fonts::Missing::default(),
            }
        }
        Err(e) => return Err(e.into()),
    };
    let mut ok = Vec::new();
    let mut fails = Vec::new();
    let mut lacks = Vec::new();
    for p in &POINTS {
        let (c, group) = (p.c, p.group);
        let cp = format!("{group} U+{:04X}", u32::from(c));
        match resolve(&model, &src, p)? {
            Outcome::Resolved(fam, file, index) => {
                ok.push(format!("{cp} -> {fam} {}#{index}", file.display()));
            }
            Outcome::Unreached(file) => fails.push(format!(
                "{cp}: the desktop's {} maps it, but no family of the product's font list does",
                file.display()
            )),
            Outcome::HostLacks => lacks.push(format!(
                "host lacks {} {group} font (no face fontconfig lists{} maps U+{:04X})",
                p.article,
                p.style
                    .map(|(_, l)| format!(" for lang {l}"))
                    .unwrap_or_default(),
                u32::from(c)
            )),
        }
    }
    if !fails.is_empty() {
        bail!("host-fonts: FAIL {}", fails.join("; "));
    }
    if !ok.is_empty() {
        eprintln!("host-fonts: ok {}", ok.join("; "));
    }
    Ok((!lacks.is_empty()).then(|| lacks.join("; ")))
}

/// minikin's walk over the product's list (an emoji-style point: its script's families only); on a miss, whether a desktop face (for emoji: one fontconfig lists for its lang) maps the point.
fn resolve(model: &atl_fonts::Model, src: &PlatformFonts, p: &Point) -> Result<Outcome> {
    let c = p.c;
    let styled = |fam: &atl_fonts::Family| match p.style {
        None => true,
        Some((script, _)) => fam.lang.as_deref().is_some_and(|l| {
            l.split(',')
                .any(|t| t.trim().split('-').any(|s| s == script))
        }),
    };
    for fam in model.families.iter().filter(|f| styled(f)) {
        for f in &fam.fonts {
            let file = model.dirs[f.dir].join(&f.file);
            if maps(&file, f.index, c)? {
                let desc = match (&fam.name, &fam.lang) {
                    (Some(n), _) => format!("family {n}"),
                    (None, Some(l)) => format!("fallback lang={l}"),
                    (None, None) => "unnamed family".into(),
                };
                return Ok(Outcome::Resolved(desc, file, f.index));
            }
        }
    }
    let all = src.list(&FontQuery {
        family: String::new(),
        lang: p.style.map(|(_, l)| l.to_owned()),
        width: None,
    })?;
    for f in all {
        if maps(&f.path, f.index, c)? {
            return Ok(Outcome::Unreached(f.path));
        }
    }
    Ok(Outcome::HostLacks)
}

/// Whether face `index` of `file` maps `c` (cmap); a non-sfnt file (Type 1, which minikin cannot load) maps nothing.
fn maps(file: &Path, index: u32, c: char) -> Result<bool> {
    use read_fonts::{FileRef, TableProvider};
    let data = std::fs::read(file).with_context(|| file.display().to_string())?;
    let face = match FileRef::new(&data) {
        Ok(FileRef::Font(f)) => f,
        Ok(FileRef::Collection(col)) => match col.get(index) {
            Ok(f) => f,
            Err(_) => return Ok(false),
        },
        Err(_) => return Ok(false),
    };
    Ok(face.cmap().ok().and_then(|m| m.map_codepoint(c)).is_some())
}

/// A fontconfig config whose only font dir is empty: fontconfig lists no face.
fn empty_conf(dir: &Path) -> Result<PathBuf> {
    let fonts = dir.join("empty-fonts");
    std::fs::create_dir_all(&fonts)?;
    let conf = dir.join("empty-fonts.conf");
    std::fs::write(
        &conf,
        format!(
            "<?xml version=\"1.0\"?>\n<!DOCTYPE fontconfig SYSTEM \"urn:fontconfig:fonts.dtd\">\n<!-- generated by xtask-harness host_fonts.rs, do not edit -->\n<fontconfig>\n  <dir>{}</dir>\n  <cachedir>{}</cachedir>\n</fontconfig>\n",
            fonts.display(),
            dir.join("empty-cache").display()
        ),
    )?;
    Ok(conf)
}
