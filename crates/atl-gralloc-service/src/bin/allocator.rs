// SPDX-License-Identifier: MIT
//! atl-gralloc-allocator: registers `android.hardware.graphics.allocator.IAllocator/default` with servicemanager and serves it on libbinder's thread pool until killed.
//! Runs as the system uid (servicemanager accepts addService from it) and needs the VINTF manifest to declare the instance (atl_gralloc::vintf::device_hals): servicemanager refuses to register an undeclared @VintfStability service.
//! Prints `gralloc: ready (<DRM node>)` once registered; refusals and allocation failures print on stderr.
#![forbid(unsafe_code)]

// the converter's rlib, on the crate search path build.rs sets (no cargo dependency names it)
extern crate binder;

use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use atl_gralloc::format::DRM_FORMAT_MOD_LINEAR;
use atl_gralloc::{node, vintf};
use atl_gralloc_service::allocator::Allocator;
use atl_gralloc_service::backend::{Backend, DRM_DIR};
use atl_gralloc_service::service;
use binder::ProcessState;
use clap::Parser;

#[derive(Parser)]
struct Args {
    /// The DRM node to allocate from (default: the first of /dev/dri's render then card nodes that allocates a linear buffer).
    #[arg(long)]
    drm_node: Option<PathBuf>,
    /// The modifier set atld's display probe found for one fourcc, `<FOURCC>=<hex>,...` (atl_gralloc::node::modifier_arg), repeatable. GPU-only buffers of that fourcc allocate inside it (policy.rs `modifiers_for`); a fourcc without one stays LINEAR.
    #[arg(long, value_parser = parse_modifiers)]
    modifiers: Vec<(u32, Vec<u64>)>,
}

fn parse_modifiers(s: &str) -> Result<(u32, Vec<u64>), String> {
    node::parse_modifier_arg(s).ok_or_else(|| format!("{s}: want <FOURCC>=0x<hex>,..."))
}

fn main() -> Result<()> {
    let a = Args::parse();
    // tier per fourcc: the probe's list, else the linear baseline (no display node: WSLg, headless)
    for fourcc in node::probed_fourccs() {
        let line = match a.modifiers.iter().find(|(f, _)| *f == fourcc) {
            Some((_, mods)) => format!("{} (probe)", node::modifier_arg(fourcc, mods)),
            None => format!(
                "{} (no probe list)",
                node::modifier_arg(fourcc, &[DRM_FORMAT_MOD_LINEAR])
            ),
        };
        writeln!(std::io::stderr(), "gralloc: modifiers {line}")?;
    }
    let backend =
        Backend::start(DRM_DIR.as_ref(), a.drm_node.as_deref()).context("gralloc backend")?;
    let node = backend.node().to_owned();
    let allocator = Arc::new(Allocator::new(backend));
    ProcessState::start_thread_pool();
    binder::add_service(
        &vintf::allocator_service_name(),
        service::new_binder(Arc::clone(&allocator), a.modifiers),
    )
    .context("addService (is the instance declared in the VINTF manifest?)")?;
    writeln!(std::io::stderr(), "{}", allocator.counters_line())?;
    writeln!(std::io::stderr(), "gralloc: ready ({})", node.display())?;
    ProcessState::join_thread_pool();
    Ok(())
}
