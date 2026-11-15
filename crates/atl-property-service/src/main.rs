// SPDX-License-Identifier: MIT
//! Writes the default property area (crate docs: the set and its sources) to --out, then prints `ATL_PROP_AREA=<--out>` on stdout: the variable an app process maps its area from (atl-android-paths `PROPERTY_AREA_ENV`), for the caller to put in that process's environment.
//! The file is written beside --out and renamed over it, so a process mapping --out never sees a partial area.
//! `--dump <area>` instead prints an area file's properties as `getprop` does (`[name]: [value]`, sorted), for a diff against a device's `adb shell getprop`. Read-only.
#![forbid(unsafe_code)]

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicU32;

use anyhow::{Context, Result, anyhow, bail};
use atl_property_service::LinearTier;
use clap::Parser;

#[derive(Parser)]
#[command(about, long_about = None)]
struct Args {
    /// The converter's build.prop (`<native out>/gen/build.prop`, written by `xtask native`).
    #[arg(long, required_unless_present = "dump")]
    build_prop: Option<PathBuf>,
    /// The area file to write.
    #[arg(long, required_unless_present = "dump")]
    out: Option<PathBuf>,
    /// Codec2's linear allocator tier instead of the probed one (`dmabuf-heap`, `gralloc-blob`): for a gate's break, e.g. the heap tier on a host without a heap must fail allocation.
    #[arg(long, value_parser = ["dmabuf-heap", "gralloc-blob"])]
    linear_tier: Option<String>,
    /// Print this area file (e.g. a run's `<root>/properties.d/<process>`) in getprop's format and exit; writes nothing.
    #[arg(long, conflicts_with_all = ["build_prop", "out", "linear_tier"])]
    dump: Option<PathBuf>,
}

fn main() -> Result<()> {
    let args = Args::parse();
    if let Some(area) = &args.dump {
        return dump(area);
    }
    let (Some(build_prop), Some(out)) = (&args.build_prop, &args.out) else {
        bail!("--build-prop and --out are required without --dump");
    };
    let text = std::fs::read_to_string(build_prop)
        .with_context(|| format!("reading {}", build_prop.display()))?;
    let tier = match args.linear_tier.as_deref() {
        Some("dmabuf-heap") => LinearTier::DmaBufHeap,
        Some(_) => LinearTier::GrallocBlob,
        None => LinearTier::probe(),
    };
    // the tier log line: which Codec2 linear allocator this host's apps get
    writeln!(
        std::io::stderr(),
        "atl-property-service: linear allocator tier {}",
        tier.name()
    )?;
    let props = atl_property_service::defaults(&text, tier)
        .with_context(|| build_prop.display().to_string())?;
    let bytes = atl_property_service::area(&props)?;
    let mut tmp = out.clone().into_os_string();
    tmp.push(".tmp");
    std::fs::write(&tmp, bytes).with_context(|| format!("writing {}", out.display()))?;
    std::fs::rename(&tmp, out).with_context(|| format!("renaming to {}", out.display()))?;
    let out = std::path::absolute(out)?;
    writeln!(
        std::io::stdout(),
        "{}={}",
        atl_android_paths::PROPERTY_AREA_ENV,
        out.display()
    )?;
    Ok(())
}

/// `--dump`: the area's properties, sorted by name as getprop sorts them.
fn dump(path: &Path) -> Result<()> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let words: Vec<AtomicU32> = bytes
        .chunks(4)
        .map(|c| {
            let mut w = [0u8; 4];
            w[..c.len()].copy_from_slice(c);
            AtomicU32::new(u32::from_ne_bytes(w))
        })
        .collect();
    let area = atl_bionic::props::Area::new(&words)
        .map_err(|e| anyhow!("{}: not a property area ({e:?})", path.display()))?;
    let Some(mut entries) = area.entries() else {
        bail!("{}: corrupt property area", path.display());
    };
    entries.sort();
    let mut out = std::io::stdout().lock();
    for (name, value) in entries {
        writeln!(out, "[{name}]: [{value}]")?;
    }
    Ok(())
}
