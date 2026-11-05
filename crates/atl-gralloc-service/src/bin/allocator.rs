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
use atl_gralloc::vintf;
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
}

fn main() -> Result<()> {
    let a = Args::parse();
    let backend =
        Backend::start(DRM_DIR.as_ref(), a.drm_node.as_deref()).context("gralloc backend")?;
    let node = backend.node().to_owned();
    let allocator = Arc::new(Allocator::new(backend));
    ProcessState::start_thread_pool();
    binder::add_service(
        &vintf::allocator_service_name(),
        service::new_binder(allocator),
    )
    .context("addService (is the instance declared in the VINTF manifest?)")?;
    writeln!(std::io::stderr(), "{}", allocator.counters_line())?;
    writeln!(std::io::stderr(), "gralloc: ready ({})", node.display())?;
    ProcessState::join_thread_pool();
    Ok(())
}
