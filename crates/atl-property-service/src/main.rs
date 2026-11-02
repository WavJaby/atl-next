// SPDX-License-Identifier: MIT
//! Writes the default property area (crate docs: the set and its sources) to --out, then prints `ATL_PROP_AREA=<--out>` on stdout: the variable an app process maps its area from (atl-bionic `AREA_ENV`), for the caller to put in that process's environment.
//! The file is written beside --out and renamed over it, so a process mapping --out never sees a partial area.
#![forbid(unsafe_code)]

use std::io::Write;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;

#[derive(Parser)]
#[command(about, long_about = None)]
struct Args {
    /// The converter's build.prop (`<native out>/gen/build.prop`, written by `xtask native`).
    #[arg(long)]
    build_prop: PathBuf,
    /// The area file to write.
    #[arg(long)]
    out: PathBuf,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let text = std::fs::read_to_string(&args.build_prop)
        .with_context(|| format!("reading {}", args.build_prop.display()))?;
    let props = atl_property_service::defaults(&text)
        .with_context(|| args.build_prop.display().to_string())?;
    let bytes = atl_property_service::area(&props)?;
    let mut tmp = args.out.clone().into_os_string();
    tmp.push(".tmp");
    std::fs::write(&tmp, bytes).with_context(|| format!("writing {}", args.out.display()))?;
    std::fs::rename(&tmp, &args.out)
        .with_context(|| format!("renaming to {}", args.out.display()))?;
    let out = std::path::absolute(&args.out)?;
    writeln!(
        std::io::stdout(),
        "{}={}",
        atl_property_service::AREA_ENV,
        out.display()
    )?;
    Ok(())
}
