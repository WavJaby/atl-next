// SPDX-License-Identifier: MIT
//! atl-egl-probe (P0a of the system-EGL adapter): can the host's EGL render into a gralloc dma-buf that we read back ourselves?
//! Per (allocating DRM node, rendering EGL device) pair: allocate RGBA_8888 through gralloc's GBM backend, pre-fill it on the CPU with [`POISON`] (so a render that never lands reads as a mismatch),
//! import it as an EGLImage, bind it to a texture + FBO, draw [`FILLS`] as scissored clears, export a native fence, round-trip it (EGL import -> GPU wait -> CPU wait), poll it as a sync_file,
//! then map the dma-buf (atl-gralloc-dmabuf, DMA_BUF_IOCTL_SYNC bracket) and compare every pixel with what [`FILLS`] gives at that pixel (last fill wins; GL's origin is the buffer's first row).
//! Clear colours are c / 255, which GL's float -> unorm8 conversion (ES 3.2 §2.3.5.2, round(f x 255)) maps back to c, so the expected bytes are exact.
//! `--matrix` runs every pair, each in a child process of its own so one driver crash names its pair and the rest still run. Output: one `p0a:` line per pair.

#![forbid(unsafe_code)]
#![expect(
    clippy::disallowed_types,
    reason = "a measurement tool, not a runtime process: --matrix re-runs this binary per pair so a driver crash stays in its pair"
)]

use std::io::{self, Write};
use std::os::fd::{AsFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow, ensure};
use atl_egl_host::ffi::{GL_RENDERER, GL_VERSION};
use atl_egl_host::{Device, DmabufPlane, Host};
use atl_gralloc::descriptor::Descriptor;
use atl_gralloc::format::{DRM_FORMAT_ABGR8888, DRM_FORMAT_MOD_LINEAR, RGBA_8888};
use atl_gralloc::usage;
use atl_gralloc_dmabuf::{Mapping, SYNC_READ, SYNC_WRITE, begin_cpu_access, end_cpu_access};
use atl_gralloc_service::backend::{Backend, DRM_DIR};
use clap::Parser;

/// Buffer edge in pixels: the size the gralloc gate allocates.
const EDGE: i32 = 64;
/// The CPU's pre-fill byte; no fill colour has it in every channel.
const POISON: u8 = 0xAA;
/// (x, y, w, h, rgba bytes) in GL window coordinates, drawn in order. Asymmetric in both axes, so a flipped or transposed readback mismatches.
const FILLS: [(i32, i32, i32, i32, [u8; 4]); 4] = [
    (0, 0, EDGE, EDGE, [32, 64, 96, 255]),
    (0, 0, EDGE / 2, EDGE / 4, [255, 0, 0, 255]),
    (EDGE / 2, EDGE / 2, EDGE / 4, EDGE / 2, [0, 255, 0, 255]),
    (EDGE / 8, EDGE * 5 / 8, EDGE / 8, EDGE / 8, [0, 0, 255, 128]),
];
/// The fence extension AOSP's queue / dequeue fences ride on.
const NATIVE_FENCE: &str = "EGL_ANDROID_native_fence_sync";
/// How long a fence may take to signal before the pair fails.
const FENCE_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Parser)]
struct Args {
    /// DRM node gralloc allocates on (e.g. /dev/dri/renderD128).
    #[arg(long)]
    alloc: Option<PathBuf>,
    /// EGL device to render on: its render node path, or `software` for Mesa's software device.
    #[arg(long)]
    render: Option<String>,
    /// Run every (alloc node, EGL device) pair, each in a child process.
    #[arg(long)]
    matrix: bool,
    /// Allocate with GBM directly, with the modifiers the rendering display can render to (not gralloc's LINEAR), and judge by glReadPixels: tiled layouts are not CPU-indexable.
    /// Answers whether a GPU that refuses LINEAR render targets (NVIDIA lists LINEAR external-only) renders at all.
    #[arg(long)]
    native: bool,
}

fn main() -> ExitCode {
    let args = Args::parse();
    let result = if args.matrix {
        matrix()
    } else {
        match (&args.alloc, &args.render) {
            (Some(a), Some(r)) => pair(a, r, args.native).map(|line| out(&format!("p0a: {line}"))),
            _ => Err(anyhow!("give --alloc and --render, or --matrix")),
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            out(&format!("p0a: FAIL {e:#}"));
            ExitCode::FAILURE
        }
    }
}

fn out(line: &str) {
    let _ = writeln!(io::stdout(), "{line}");
}

/// Every pair, each in a child; fails if any pair fails.
fn matrix() -> Result<()> {
    let host = Host::load()?;
    out(&format!(
        "p0a: client-extensions {}",
        host.client_extensions()
    ));
    let devices = host.devices()?;
    let mut names: Vec<String> = Vec::new();
    // NVIDIA lists its GPU twice (a CUDA-capable and a plain device, one render node); one run per node
    for n in devices.iter().map(device_name) {
        if !names.contains(&n) {
            names.push(n);
        }
    }
    for d in &devices {
        out(&format!(
            "p0a: device {} drm={:?} ext={}",
            device_name(d),
            d.drm_file,
            d.extensions
        ));
    }
    let mut nodes: Vec<PathBuf> = std::fs::read_dir(DRM_DIR)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("renderD") || n.starts_with("card"))
        })
        .collect();
    nodes.sort();
    let me = std::env::current_exe()?;
    let mut failed = 0;
    let runs = nodes.iter().flat_map(|n| {
        names
            .iter()
            .flat_map(move |r| [(n, r, false), (n, r, true)])
    });
    for (node, name, native) in runs.filter(|(_, r, native)| !(*native && *r == "software")) {
        {
            let mut cmd = Command::new(&me);
            cmd.arg("--alloc").arg(node).arg("--render").arg(name);
            if native {
                cmd.arg("--native");
            }
            let o = cmd.output()?;
            let text = String::from_utf8_lossy(&o.stdout);
            for info in text.lines().filter(|l| l.starts_with("p0a: info ")) {
                out(info);
            }
            let line = text
                .lines()
                .rfind(|l| l.starts_with("p0a: "))
                .unwrap_or("p0a: FAIL no output");
            let line = line.trim_start_matches("p0a: ");
            if !o.status.success() {
                failed += 1;
            }
            let status = if o.status.success() { "pass" } else { "FAIL" };
            let kind = if native { "native" } else { "gralloc" };
            out(&format!(
                "p0a: pair {kind} alloc={} render={name} {status} exit={:?} {}",
                node.display(),
                o.status.code(),
                line.trim_start_matches("FAIL ")
            ));
        }
    }
    ensure!(failed == 0, "{failed} pair(s) failed");
    Ok(())
}

fn device_name(d: &Device) -> String {
    if d.is_software() {
        return "software".into();
    }
    d.render_node
        .clone()
        .unwrap_or_else(|| "no-render-node".into())
}

/// One pair: the summary line on success.
fn pair(alloc: &Path, render: &str, native: bool) -> Result<String> {
    let host = Host::load()?;
    let device = host
        .devices()?
        .into_iter()
        .find(|d| device_name(d) == render)
        .ok_or_else(|| anyhow!("no EGL device {render}"))?;
    let display = device.display()?;
    let mods = display.modifiers(DRM_FORMAT_ABGR8888)?;
    let linear = match mods.iter().find(|(m, _)| *m == DRM_FORMAT_MOD_LINEAR) {
        Some((_, true)) => "external-only",
        Some((_, false)) => "listed",
        None => "absent",
    };
    let buf = if native {
        native_buffer(alloc, &mods)?
    } else {
        gralloc_buffer(alloc)?
    };
    let ctx = display.create_context()?;
    let gl = ctx.make_current()?;
    let renderer = gl.string(GL_RENDERER);
    let version = gl.string(GL_VERSION);
    let image = display
        .import_dmabuf(&DmabufPlane {
            fd: buf.fd.as_fd(),
            fourcc: DRM_FORMAT_ABGR8888,
            width: EDGE.unsigned_abs(),
            height: EDGE.unsigned_abs(),
            offset: buf.offset,
            pitch: buf.pitch,
            modifier: buf.modifier,
        })
        .with_context(|| format!("import (modifier {:#x}, linear {linear})", buf.modifier))?;
    out(&format!(
        "p0a: info alloc={} render={render} modifiers={:x?}",
        alloc.display(),
        mods
    ));
    // a texture first (what platform_android.c binds); a renderbuffer where the driver refuses that
    let target = match gl.texture_from_image(&image) {
        Ok(t) => t,
        Err(e) => {
            out(&format!(
                "p0a: info texture refused ({e:#}), trying a renderbuffer"
            ));
            gl.renderbuffer_from_image(&image)?
        }
    };
    let fb = gl.framebuffer(&target)?;
    gl.bind(&fb);
    gl.viewport(EDGE, EDGE);
    for (x, y, w, h, c) in FILLS {
        gl.fill(x, y, w, h, c.map(|v| f32::from(v) / 255.0));
    }
    gl.check("fills")?;
    // without the fence extension the pixels are still judged (after glFinish), and the line says the fence is absent
    let fence = if display.has_extension(NATIVE_FENCE) {
        let fence = display.export_fence()?;
        display
            .wait_fence(fence.try_clone()?)
            .context("fence round trip")?;
        ensure!(
            poll_signalled(&fence)?,
            "exported fence not signalled after {FENCE_TIMEOUT:?}"
        );
        "ok"
    } else {
        gl.finish();
        "absent"
    };

    // gralloc's LINEAR buffer: our own mapping; a native (tiled) one: glReadPixels, rows bottom-up like the buffer's own
    let (bytes, pitch, readback) = if native {
        (
            gl.read_rgba(EDGE, EDGE)?,
            usize::try_from(EDGE)? * 4,
            "glReadPixels",
        )
    } else {
        let len = usize::try_from(buf.size)?;
        begin_cpu_access(buf.fd.as_fd(), SYNC_READ)?;
        let bytes = Mapping::read_only(buf.fd.as_fd(), len)
            .context("map for readback")?
            .read();
        end_cpu_access(buf.fd.as_fd(), SYNC_READ)?;
        (bytes, usize::try_from(buf.pitch)?, "mapping")
    };
    let mut bad = 0;
    let mut first = None;
    for y in 0..EDGE {
        for x in 0..EDGE {
            let want = expected(x, y);
            let at = usize::try_from(y)? * pitch + usize::try_from(x)? * 4;
            let got = &bytes[at..at + 4];
            if got != want {
                bad += 1;
                first.get_or_insert((x, y, got.to_vec(), want));
            }
        }
    }
    let summary = format!(
        "alloc={} render={render} egl={}.{} vendor={:?} gl={version:?} renderer={renderer:?} pitch={} modifier={:#x} linear={linear} mods={} target={} readback={readback} fence={fence} swap_damage={} image_native_buffer={} partial_update={}",
        alloc.display(),
        display.version.0,
        display.version.1,
        display.vendor,
        buf.pitch,
        buf.modifier,
        mods.len(),
        if target.renderbuffer {
            "renderbuffer"
        } else {
            "texture"
        },
        display.has_extension("EGL_KHR_swap_buffers_with_damage"),
        display.has_extension("EGL_ANDROID_image_native_buffer"),
        display.has_extension("EGL_KHR_partial_update"),
    );
    if let Some((x, y, got, want)) = first {
        return Err(anyhow!(
            "{summary} pixels={bad}/{} bad, first ({x},{y}) got {got:?} want {want:?}",
            EDGE * EDGE
        ));
    }
    Ok(format!("{summary} pixels=exact"))
}

/// One plane-0 dma-buf to render into.
struct Buffer {
    fd: OwnedFd,
    offset: u32,
    pitch: u32,
    size: u64,
    modifier: u64,
}

/// RGBA_8888 through gralloc's GBM backend, as the allocator service allocates (LINEAR), pre-filled with [`POISON`] on the CPU.
fn gralloc_buffer(alloc: &Path) -> Result<Buffer> {
    let backend = Backend::start(Path::new(DRM_DIR), Some(alloc))
        .map_err(|e| anyhow!("gralloc backend on {}: {e}", alloc.display()))?;
    let checked = Descriptor {
        name: b"p0a".to_vec(),
        width: EDGE,
        height: EDGE,
        layer_count: 1,
        format: RGBA_8888,
        usage: usage::CPU_MASK | usage::GPU_RENDER_TARGET | usage::GPU_TEXTURE,
        reserved_size: 0,
        option_names: vec![],
        modifiers: vec![],
    }
    .check()
    .map_err(|e| anyhow!("descriptor refused: {e:?}"))?;
    let buf = backend
        .allocate(&checked)
        .map_err(|e| anyhow!("allocate: {e}"))?;
    let len = usize::try_from(buf.size)?;
    let map = Mapping::new(buf.fd.as_fd(), len).context("map for pre-fill")?;
    begin_cpu_access(buf.fd.as_fd(), SYNC_WRITE)?;
    map.write(0, &vec![POISON; len]);
    end_cpu_access(buf.fd.as_fd(), SYNC_WRITE)?;
    Ok(Buffer {
        fd: buf.fd,
        offset: 0,
        pitch: buf.pitch,
        size: buf.size,
        modifier: buf.modifier,
    })
}

/// ABGR8888 straight from GBM on `alloc`, restricted to the modifiers the display renders to (`external_only` false), GBM_BO_USE_RENDERING.
fn native_buffer(alloc: &Path, mods: &[(u64, bool)]) -> Result<Buffer> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(alloc)?;
    let dev = gbm::Device::new(file).context("gbm device")?;
    let renderable: Vec<u64> = mods
        .iter()
        .filter(|(_, ext)| !ext)
        .map(|(m, _)| *m)
        .collect();
    ensure!(
        !renderable.is_empty(),
        "the display renders to no ABGR8888 modifier"
    );
    let bo = dev
        .create_buffer_object_with_modifiers2::<()>(
            EDGE.unsigned_abs(),
            EDGE.unsigned_abs(),
            gbm::Format::Abgr8888,
            renderable.iter().map(|m| gbm::Modifier::from(*m)),
            gbm::BufferObjectFlags::RENDERING,
        )
        .with_context(|| format!("gbm_bo_create_with_modifiers2 {renderable:x?}"))?;
    ensure!(
        bo.plane_count() == 1,
        "{} planes (compression metadata?)",
        bo.plane_count()
    );
    let fd = bo.fd().context("gbm_bo_get_fd")?;
    let size = rustix::fs::seek(&fd, rustix::fs::SeekFrom::End(0))?;
    Ok(Buffer {
        fd,
        offset: bo.offset(0),
        pitch: bo.stride(),
        size,
        modifier: u64::from(bo.modifier()),
    })
}

/// The bytes [`FILLS`] leave at (x, y): the last fill covering it.
fn expected(x: i32, y: i32) -> [u8; 4] {
    FILLS
        .iter()
        .rev()
        .find(|(fx, fy, w, h, _)| (*fx..fx + w).contains(&x) && (*fy..fy + h).contains(&y))
        .map_or([POISON; 4], |f| f.4)
}

/// Whether the sync_file signals within [`FENCE_TIMEOUT`] (POLLIN on a sync_file = signalled).
fn poll_signalled(fence: &impl AsFd) -> Result<bool> {
    use rustix::event::{PollFd, PollFlags, Timespec, poll};
    let mut fds = [PollFd::new(fence, PollFlags::IN)];
    let timeout = Timespec {
        tv_sec: i64::try_from(FENCE_TIMEOUT.as_secs())?,
        tv_nsec: 0,
    };
    let n = poll(&mut fds, Some(&timeout))?;
    Ok(n == 1 && fds[0].revents().contains(PollFlags::IN))
}
