// SPDX-License-Identifier: MIT
//! Writes the Parcel form of a key character map: `atl-kcm-parcel <Virtual.kcm> <out>`.
//! The map loads under its file name, so the bytes do not depend on the directory it sits in. The parcel gate hands the bytes to its Java driver; the input service loads the same file itself.

use std::path::PathBuf;

use anyhow::{Context, Result};
use atl_libinput::KeyCharacterMap;
use clap::Parser;

#[derive(Parser)]
struct Args {
    /// The `.kcm` file (AOSP's frameworks/base/data/keyboards/Virtual.kcm).
    kcm: PathBuf,
    /// Where the Parcel form is written.
    out: PathBuf,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let name = args
        .kcm
        .file_name()
        .and_then(|n| n.to_str())
        .with_context(|| format!("{}: no UTF-8 file name", args.kcm.display()))?;
    let contents =
        std::fs::read(&args.kcm).with_context(|| format!("reading {}", args.kcm.display()))?;
    let map = KeyCharacterMap::load(name, &contents)
        .with_context(|| format!("loading {}", args.kcm.display()))?;
    std::fs::write(&args.out, map.parcel_bytes())
        .with_context(|| format!("writing {}", args.out.display()))?;
    Ok(())
}
