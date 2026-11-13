// SPDX-License-Identifier: MIT
//! The GPU device class: the device nodes a process rendering on one GPU opens, derived from what the kernel reports for that GPU (sysfs, procfs), never from the environment.
//! AOSP grants an app `gpu_device` per device (system/sepolicy private/app.te); flatpak's `--device=dri` binds a fixed vendor list for every GPU (flatpak-run.c `FLATPAK_CONTEXT_DEVICE_DRI`). Here: the chosen GPU's DRM nodes plus its kernel driver's control nodes, nothing of another GPU.

use std::io;
use std::path::{Path, PathBuf};

/// Where the DRM nodes live.
pub const DRM_DIR: &str = "/dev/dri";
const SYS_DRM: &str = "/sys/class/drm";

/// Control nodes a kernel driver's user-space GL/Vulkan driver opens besides the DRM nodes, keyed by the driver name sysfs reports (`<device>/driver`). Bound when present.
/// nvidia: flatpak-run.c `FLATPAK_CONTEXT_DEVICE_DRI` list (`/dev/nvidia-caps` is not in it); NVIDIA's EGL opens `nvidiactl` even to answer "not my device". The per-GPU `/dev/nvidia<minor>` comes from [`nvidia_minor`].
const DRIVER_NODES: &[(&str, &[&str])] = &[(
    "nvidia",
    &[
        "/dev/nvidiactl",
        "/dev/nvidia-modeset",
        "/dev/nvidia-uvm",
        "/dev/nvidia-uvm-tools",
    ],
)];

/// A render path a kernel module adds beside the DRM device classes: a GPU reached through a non-DRM node, so no [`device_class`] names it.
/// Candidate only: atld probes it (renderer differs from the baseline's and a context initialises) before apps get its nodes and variables; the baseline (no row) works everywhere and is the reference.
pub struct RenderPath {
    /// The kernel module (`/sys/module/<module>`) whose presence makes the row a candidate; also the tier's name in atld's log.
    pub module: &'static str,
    /// Device nodes the renderer opens; every one must exist.
    pub nodes: &'static [&'static str],
    /// Variables the host GL driver reads to render through it: Mesa picks llvmpipe by default and has no API to pick a gallium driver (eglinfo on WSL: every platform llvmpipe without it).
    pub env: &'static [(&'static str, &'static str)],
}

/// dxgkrnl (GPU paravirtualisation, e.g. WSL2): Mesa's d3d12 gallium driver renders through `/dev/dxg` (bwrap without it: d3d12 cannot initialise). Not gralloc's: GBM under d3d12 fails (`gbm_bo_create` ENOENT).
pub const RENDER_PATHS: &[RenderPath] = &[RenderPath {
    module: "dxgkrnl",
    nodes: &["/dev/dxg"],
    env: &[("GALLIUM_DRIVER", "d3d12")],
}];

/// The [`RENDER_PATHS`] rows this kernel offers: module loaded, every node present.
pub fn render_paths() -> Vec<&'static RenderPath> {
    paths_in(Path::new("/sys/module"), RENDER_PATHS)
}

fn paths_in(sys_module: &Path, rows: &'static [RenderPath]) -> Vec<&'static RenderPath> {
    rows.iter()
        .filter(|r| sys_module.join(r.module).is_dir() && r.nodes.iter().all(|n| Path::new(n).exists()))
        .collect()
}

/// The device class of the GPU behind DRM node `node` (`/dev/dri/renderD128`, or a `card*` node): every DRM node of the same device (its `drm/` dir in sysfs), and its kernel driver's control nodes ([`DRIVER_NODES`], [`nvidia_minor`]). Sorted, deduplicated.
pub fn device_class(node: &Path) -> io::Result<Vec<PathBuf>> {
    class_in(
        Path::new(SYS_DRM),
        Path::new(DRM_DIR),
        Path::new("/proc/driver/nvidia/gpus"),
        node,
    )
}

/// The union of every GPU's [`device_class`]: what a process that must see all GPUs opens (the display probe: NVIDIA's EGL needs its nodes even to decline).
pub fn all_device_classes() -> io::Result<Vec<PathBuf>> {
    let mut all = Vec::new();
    for e in std::fs::read_dir(DRM_DIR)? {
        let p = e?.path();
        if is_drm_node_name(&p) {
            all.extend(device_class(&p)?);
        }
    }
    all.sort();
    all.dedup();
    Ok(all)
}

fn is_drm_node_name(p: &Path) -> bool {
    p.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with("card") || n.starts_with("renderD"))
}

/// [`device_class`] against explicit roots: `sys_drm` (`/sys/class/drm`), `dev_dri` (`/dev/dri`), `nv_gpus` (`/proc/driver/nvidia/gpus`). A test builds them in a temp dir.
fn class_in(
    sys_drm: &Path,
    dev_dri: &Path,
    nv_gpus: &Path,
    node: &Path,
) -> io::Result<Vec<PathBuf>> {
    let name = node
        .file_name()
        .ok_or_else(|| io::Error::other(format!("{}: not a DRM node path", node.display())))?;
    let device = std::fs::canonicalize(sys_drm.join(name).join("device"))
        .map_err(|e| io::Error::other(format!("{}: no sysfs device ({e})", node.display())))?;
    let mut class = Vec::new();
    // the device's own DRM nodes: renderD* and its card* sibling (a vgem or virtual device may lack one)
    for e in std::fs::read_dir(device.join("drm"))? {
        let p = dev_dri.join(e?.file_name());
        if is_drm_node_name(&p) && p.exists() {
            class.push(p);
        }
    }
    // no driver link: no driver bound, no control nodes
    let driver = std::fs::read_link(device.join("driver"))
        .ok()
        .and_then(|l| l.file_name().map(|n| n.to_string_lossy().into_owned()));
    if let Some(driver) = driver {
        for (_, nodes) in DRIVER_NODES.iter().filter(|(d, _)| *d == driver) {
            class.extend(nodes.iter().map(PathBuf::from).filter(|p| p.exists()));
        }
        if driver == "nvidia" {
            let pci = device.file_name().unwrap_or_default();
            let minor = nvidia_minor(&nv_gpus.join(pci).join("information"))?;
            class.push(PathBuf::from(format!("/dev/nvidia{minor}")));
        }
    }
    class.sort();
    class.dedup();
    Ok(class)
}

/// The `/dev/nvidia<minor>` of one NVIDIA GPU: `Device Minor:` of its `/proc/driver/nvidia/gpus/<pci>/information` (the NVIDIA kernel module's per-GPU file). Derived per GPU, not flatpak's `nvidia0..19` sweep.
fn nvidia_minor(information: &Path) -> io::Result<u32> {
    let text = std::fs::read_to_string(information)
        .map_err(|e| io::Error::other(format!("{}: {e}", information.display())))?;
    text.lines()
        .find_map(|l| l.strip_prefix("Device Minor:"))
        .and_then(|v| v.trim().parse().ok())
        .ok_or_else(|| {
            io::Error::other(format!(
                "{}: no `Device Minor:` line",
                information.display()
            ))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    /// A sysfs/devfs/procfs stand-in: Intel `xe` at 0000:00:02.0 (card0, renderD128), NVIDIA at 0000:2c:00.0 (card1, renderD129, minor 0), as measured on the owner's laptop (gpu-selection.md S1).
    fn fixture(root: &Path, nvidia_info: Option<&str>) -> (PathBuf, PathBuf, PathBuf) {
        let sys = root.join("sys/class/drm");
        let dev = root.join("dev/dri");
        let nv = root.join("proc/nvidia/gpus");
        std::fs::create_dir_all(&sys).unwrap();
        std::fs::create_dir_all(&dev).unwrap();
        for (pci, driver, nodes) in [
            ("0000:00:02.0", "xe", ["card0", "renderD128"]),
            ("0000:2c:00.0", "nvidia", ["card1", "renderD129"]),
        ] {
            let d = root.join("sys/devices").join(pci);
            std::fs::create_dir_all(d.join("drm")).unwrap();
            let drv = root.join("sys/bus/pci/drivers").join(driver);
            std::fs::create_dir_all(&drv).unwrap();
            symlink(&drv, d.join("driver")).unwrap();
            for n in nodes {
                std::fs::create_dir_all(d.join("drm").join(n)).unwrap();
                std::fs::create_dir_all(sys.join(n)).unwrap();
                symlink(&d, sys.join(n).join("device")).unwrap();
                std::fs::write(dev.join(n), "").unwrap();
            }
        }
        if let Some(info) = nvidia_info {
            std::fs::create_dir_all(nv.join("0000:2c:00.0")).unwrap();
            std::fs::write(nv.join("0000:2c:00.0/information"), info).unwrap();
        }
        (sys, dev, nv)
    }

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("atl-broker-gpu-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn intel_class_is_its_two_nodes_only() {
        let root = tmp("intel");
        let (sys, dev, nv) = fixture(&root, Some("Model: X\nDevice Minor: 0\n"));
        let class = class_in(&sys, &dev, &nv, &dev.join("renderD128")).unwrap();
        assert_eq!(class, vec![dev.join("card0"), dev.join("renderD128")]);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn nvidia_class_adds_its_minor_node() {
        let root = tmp("nv");
        // minor 3 is not the index of the GPU's card node: the minor comes from the information file
        let (sys, dev, nv) = fixture(&root, Some("Model: RTX\nIRQ: 1\nDevice Minor: 3\n"));
        let class = class_in(&sys, &dev, &nv, &dev.join("renderD129")).unwrap();
        assert!(
            class.contains(&dev.join("card1")) && class.contains(&dev.join("renderD129")),
            "{class:?}"
        );
        assert!(class.contains(&PathBuf::from("/dev/nvidia3")), "{class:?}");
        assert!(!class.contains(&dev.join("renderD128")), "{class:?}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn nvidia_without_information_is_named() {
        let root = tmp("noinfo");
        let (sys, dev, nv) = fixture(&root, None);
        let e = class_in(&sys, &dev, &nv, &dev.join("renderD129")).unwrap_err();
        assert!(e.to_string().contains("information"), "{e}");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A row is a candidate only with its module loaded and every node present (`/` exists everywhere; the second row's node never does).
    #[test]
    fn render_path_needs_module_and_nodes() {
        static ROWS: &[RenderPath] = &[
            RenderPath { module: "m1", nodes: &["/"], env: &[] },
            RenderPath { module: "m2", nodes: &["/atl-no-such-node"], env: &[] },
            RenderPath { module: "m3", nodes: &["/"], env: &[] },
        ];
        let root = tmp("modules");
        std::fs::create_dir_all(root.join("m1")).unwrap();
        std::fs::create_dir_all(root.join("m2")).unwrap();
        let found: Vec<_> = paths_in(&root, ROWS).iter().map(|r| r.module).collect();
        assert_eq!(found, ["m1"]);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn unknown_node_is_named() {
        let root = tmp("unknown");
        let (sys, dev, nv) = fixture(&root, None);
        let e = class_in(&sys, &dev, &nv, &dev.join("renderD200")).unwrap_err();
        assert!(e.to_string().contains("renderD200"), "{e}");
        let _ = std::fs::remove_dir_all(&root);
    }
}
