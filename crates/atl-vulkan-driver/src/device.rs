// SPDX-License-Identifier: MIT
//! Which physical device the process sees: exactly the one on gralloc's node (atl_gralloc::node), so allocation and rendering agree on one GPU (vulkan plan S2 ideal 2, tier table row "device").
//! Tiers, probed per device: `drm` -- the device has `VK_EXT_physical_device_drm` and its render node is gralloc's st_rdev; `cpu-device` -- no device is on the node, gralloc chose its node by trial (a node with no rendering device behind it, e.g. vgem, only allocates) and the device is a CPU device: the software renderer, as the GL driver does on that tier.
//! Anything else -> no device: instances still create, enumeration returns 0 devices, and the refusal names gralloc's node and every device's.

use atl_gralloc::handle::DeviceNumber;
use atl_gralloc::node::{NodeSource, ProcessNode};

/// A physical device as the ICD describes it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    pub name: String,
    pub kind: &'static str,
    pub cpu: bool,
    /// Its render node (`VK_EXT_physical_device_drm`); None: no extension or no render node.
    pub render: Option<DeviceNumber>,
    /// What finds it again on another instance of the same ICD: vendor, device id, name (Vulkan 1.0 properties, so an app's 1.0 instance can be matched too). Two identical GPUs share it: the first one enumerated wins.
    pub key: Key,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Key {
    pub vendor: u32,
    pub device: u32,
    pub name: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tier {
    Drm,
    CpuDevice,
}

impl Tier {
    pub fn name(self) -> &'static str {
        match self {
            Tier::Drm => "drm",
            Tier::CpuDevice => "cpu-device",
        }
    }
}

/// `VK_EXT_physical_device_drm`'s (major, minor) as st_rdev: glibc `gnu_dev_makedev`, restricted to 32 bits as [`DeviceNumber`] is (the inverse of its `major`/`minor`).
pub fn dev_number(major: i64, minor: i64) -> DeviceNumber {
    let (major, minor) = (major as u32, minor as u32);
    DeviceNumber(((major & 0xfff) << 8) | (minor & 0xff) | ((minor & !0xff) << 12))
}

/// The device to expose and its tier, or the refusal naming both sides.
pub fn pick(node: &ProcessNode, devices: &[Candidate]) -> Result<(usize, Tier), String> {
    if let Some(i) = devices.iter().position(|d| d.render == Some(node.device)) {
        return Ok((i, Tier::Drm));
    }
    if node.source == NodeSource::Trial
        && let Some(i) = devices.iter().position(|d| d.cpu && d.render.is_none())
    {
        return Ok((i, Tier::CpuDevice));
    }
    let listed: Vec<String> = devices
        .iter()
        .map(|d| {
            let render = d
                .render
                .map_or_else(|| "none".to_owned(), |r| r.to_string());
            format!("{} type {} node {render}", d.name, d.kind)
        })
        .collect();
    Err(format!(
        "no device on gralloc's node {node}; devices: [{}]",
        listed.join("; ")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 226 = the DRM char major (linux/major.h DRM_MAJOR); render minors start at 128 (drm_drv.c).
    const D128: DeviceNumber = DeviceNumber((226 << 8) | 128);
    const D129: DeviceNumber = DeviceNumber((226 << 8) | 129);

    fn node(device: DeviceNumber, source: NodeSource) -> ProcessNode {
        ProcessNode {
            path: format!("/dev/dri/renderD{}", device.minor()),
            source,
            device,
        }
    }

    fn dev(name: &str, cpu: bool, render: Option<DeviceNumber>) -> Candidate {
        Candidate {
            name: name.into(),
            kind: if cpu { "cpu" } else { "discrete" },
            cpu,
            render,
            key: Key {
                vendor: 0,
                device: 0,
                name: name.into(),
            },
        }
    }

    #[test]
    fn dev_number_inverts_major_minor() {
        assert_eq!(dev_number(226, 128), D128);
        let d = dev_number(226, 300);
        assert_eq!((d.major(), d.minor()), (226, 300));
    }

    #[test]
    fn node_match_wins_on_both_sources() {
        let devs = [
            dev("llvmpipe", true, None),
            dev("intel", false, Some(D128)),
            dev("nvidia", false, Some(D129)),
        ];
        for source in [NodeSource::Display, NodeSource::Trial] {
            assert_eq!(pick(&node(D129, source), &devs), Ok((2, Tier::Drm)));
        }
    }

    #[test]
    fn cpu_device_only_on_trial() {
        let devs = [dev("gpu-no-ext", false, None), dev("llvmpipe", true, None)];
        assert_eq!(
            pick(&node(D128, NodeSource::Trial), &devs),
            Ok((1, Tier::CpuDevice))
        );
        let err = pick(&node(D128, NodeSource::Display), &devs).unwrap_err();
        // both sides named: gralloc's node and every device's
        assert!(
            err.contains("/dev/dri/renderD128 (226:128, display)"),
            "{err}"
        );
        assert!(err.contains("llvmpipe type cpu node none"), "{err}");
        assert!(err.contains("gpu-no-ext type discrete node none"), "{err}");
    }

    #[test]
    fn gpu_on_another_node_is_refused() {
        let devs = [dev("nvidia", false, Some(D129))];
        let err = pick(&node(D128, NodeSource::Trial), &devs).unwrap_err();
        assert!(err.contains("nvidia type discrete node 226:129"), "{err}");
    }
}
