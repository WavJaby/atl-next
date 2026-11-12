// SPDX-License-Identifier: MIT
//! The DRM node gralloc allocates on, as atld hands it to every app process: the node and how it was chosen, two system properties.
//! One node per atld run (gpu-selection D4): gralloc allocates there, the app's GLES driver renders on the EGL device of that node, so no buffer crosses GPUs.

/// The node's path (`/dev/dri/renderD128`): gralloc's ready line names it, atld copies it.
pub const RENDER_NODE_PROPERTY: &str = "ro.atl.gpu.render_node";
/// How the node was chosen: a [`NodeSource`] name.
pub const NODE_SOURCE_PROPERTY: &str = "ro.atl.gpu.node_source";

/// How the run's node was chosen; one path with two capability tiers, picked by probing the display.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NodeSource {
    /// The desktop's Wayland EGL display named its device's render node: the app must render on that node's EGL device, nothing else.
    Display,
    /// The display named none (no DRM device behind it, e.g. WSLg's d3d12, or a headless run): gralloc's own trial picked the first node that allocates; a node with no EGL device (vgem) only allocates, the app renders on the software device.
    Trial,
}

impl NodeSource {
    pub fn name(self) -> &'static str {
        match self {
            NodeSource::Display => "display",
            NodeSource::Trial => "trial",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        [NodeSource::Display, NodeSource::Trial]
            .into_iter()
            .find(|n| n.name() == s)
    }
}

/// The device number of DRM node `path` (`st_rdev`): what the allocator writes into every handle and what an importer compares it with (handle::Params::check_device). Not a character device, or a number past 32 bits: an error naming the path.
pub fn device_number(path: &std::path::Path) -> std::io::Result<crate::handle::DeviceNumber> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    let meta = std::fs::metadata(path)?;
    if !meta.file_type().is_char_device() {
        return Err(std::io::Error::other(format!(
            "{}: not a character device",
            path.display()
        )));
    }
    crate::handle::DeviceNumber::from_rdev(meta.rdev()).ok_or_else(|| {
        std::io::Error::other(format!(
            "{}: device number {:#x} past 32 bits",
            path.display(),
            meta.rdev()
        ))
    })
}

/// The node an app process's GPU drivers must use, as atld names it: path, how it was chosen, its st_rdev.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcessNode {
    pub path: String,
    pub source: NodeSource,
    pub device: crate::handle::DeviceNumber,
}

impl std::fmt::Display for ProcessNode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({}, {})", self.path, self.device, self.source.name())
    }
}

/// The process's node from its two properties; `get` reads one (bionic `__system_property_get` in an app process, None when unset). Every driver reads the node through this, so they agree on one GPU.
pub fn process_node(
    get: impl Fn(&str) -> std::io::Result<Option<String>>,
) -> std::io::Result<ProcessNode> {
    let read = |name: &str| {
        get(name)?.ok_or_else(|| {
            std::io::Error::other(format!(
                "{name} is unset: no gralloc node in this process (atld names it with --gralloc)"
            ))
        })
    };
    let path = read(RENDER_NODE_PROPERTY)?;
    let source = read(NODE_SOURCE_PROPERTY)?;
    let source = NodeSource::parse(&source).ok_or_else(|| {
        std::io::Error::other(format!("{NODE_SOURCE_PROPERTY}={source}: unknown"))
    })?;
    let device = device_number(std::path::Path::new(&path))
        .map_err(|e| std::io::Error::other(format!("gralloc's node {path}: {e}")))?;
    Ok(ProcessNode {
        path,
        source,
        device,
    })
}

/// What `atl-egl-probe --display-node` prints and atld reads: `<prefix> node=<path>|none renderer=<GL_RENDERER>`.
pub const DISPLAY_LINE: &str = "gpu-probe:";
/// The `node=` value when the display's device has no render node.
const NO_NODE: &str = "none";

/// The probe's line for the display's render node (None: it reported none) and renderer.
pub fn display_line(node: Option<&str>, renderer: &str) -> String {
    format!(
        "{DISPLAY_LINE} node={} renderer={renderer}",
        node.unwrap_or(NO_NODE)
    )
}

/// The display's (node, renderer) from the probe's output: its last [`DISPLAY_LINE`]; None when no well-formed line is there.
pub fn parse_display_line(text: &str) -> Option<(Option<String>, String)> {
    let line = text
        .lines()
        .rev()
        .find_map(|l| l.strip_prefix(DISPLAY_LINE))?;
    let rest = line.trim_start().strip_prefix("node=")?;
    let (node, renderer) = rest.split_once(" renderer=")?;
    let node = (node != NO_NODE).then(|| node.to_owned());
    Some((node, renderer.to_owned()))
}

/// The probe's per-fourcc modifier line after [`DISPLAY_LINE`]: `<prefix> <FOURCC>=<hex>,<hex>...` (nothing after `=`: no modifier qualifies). Its own prefix, so [`parse_display_line`] never reads it.
pub const MODIFIERS_LINE: &str = "gpu-modifiers:";

/// The fourccs gralloc allocates for GPU use: every format row but BLOB's (BLOB is CPU memory, LINEAR by usage).
pub fn probed_fourccs() -> impl Iterator<Item = u32> {
    crate::format::all()
        .iter()
        .filter(|f| f.pixel_format != crate::format::BLOB)
        .map(|f| f.fourcc)
}

/// The allocation set for `fourcc`: modifiers the rendering device renders to (`external_only` false on its EGL device display) that the presenter's display imports at all (any `external_only`), restricted to 1-plane layouts (the handle carries one plane; `planes` = gbm_device_get_format_modifier_plane_count, None = unknown -> excluded). Device order kept.
pub fn modifier_set(
    device: &[(u64, bool)],
    presenter: &[(u64, bool)],
    planes: impl Fn(u64) -> Option<u32>,
) -> Vec<u64> {
    device
        .iter()
        .filter(|(_, external_only)| !external_only)
        .map(|(m, _)| *m)
        .filter(|m| presenter.iter().any(|(p, _)| p == m))
        .filter(|m| planes(*m) == Some(1))
        .collect()
}

/// `AB24` from 0x34324241 (drm_fourcc.h: first character in the least significant byte).
pub fn fourcc_name(fourcc: u32) -> String {
    String::from_utf8_lossy(&fourcc.to_le_bytes()).into_owned()
}

/// `<FOURCC>=<hex>,...`: the value of the allocator's `--modifiers` and the tail of [`MODIFIERS_LINE`].
pub fn modifier_arg(fourcc: u32, mods: &[u64]) -> String {
    let list: Vec<String> = mods.iter().map(|m| format!("{m:#x}")).collect();
    format!("{}={}", fourcc_name(fourcc), list.join(","))
}

/// Inverse of [`modifier_arg`]; None when malformed (fourcc not 4 bytes, a modifier not `0x` hex).
pub fn parse_modifier_arg(s: &str) -> Option<(u32, Vec<u64>)> {
    let (name, list) = s.split_once('=')?;
    let fourcc = u32::from_le_bytes(name.as_bytes().try_into().ok()?);
    let mods = list
        .split(',')
        .filter(|m| !m.is_empty())
        .map(|m| u64::from_str_radix(m.strip_prefix("0x")?, 16).ok())
        .collect::<Option<Vec<_>>>()?;
    Some((fourcc, mods))
}

/// The probe's line for one fourcc.
pub fn modifiers_line(fourcc: u32, mods: &[u64]) -> String {
    format!("{MODIFIERS_LINE} {}", modifier_arg(fourcc, mods))
}

/// Every well-formed [`MODIFIERS_LINE`] in the probe's output, in order; a `FAIL` line or a malformed one is no entry.
pub fn parse_modifiers_lines(text: &str) -> Vec<(u32, Vec<u64>)> {
    text.lines()
        .filter_map(|l| l.strip_prefix(MODIFIERS_LINE))
        .filter_map(|l| parse_modifier_arg(l.trim()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// AB24 lists from the laptop (gralloc-modifiers S1): NVIDIA renderD129 lists LINEAR external-only + block-linear renderable; GTK offload env imports `0` and the 12 block-linear ones.
    const NV_BL: u64 = 0x0300_0000_00e0_8014;
    const NV_BL2: u64 = 0x0300_0000_0060_6010;
    const INTEL_X: u64 = 0x0100_0000_0000_0001;

    #[test]
    fn set_drops_external_only_unimportable_and_multiplane() {
        let device = [(0, true), (NV_BL2, false), (NV_BL, false), (INTEL_X, false)];
        let presenter = [(0, false), (NV_BL2, false), (NV_BL, false)];
        // LINEAR: external-only on the device; INTEL_X: presenter lacks it
        assert_eq!(
            modifier_set(&device, &presenter, |_| Some(1)),
            [NV_BL2, NV_BL]
        );
        // a 2-plane layout (compression metadata) and an unknown count are out
        let planes = |m| (m == NV_BL).then_some(2).or((m == NV_BL2).then_some(1));
        assert_eq!(modifier_set(&device, &presenter, planes), [NV_BL2]);
        // LINEAR renderable (Intel) stays
        assert_eq!(modifier_set(&[(0, false)], &[(0, true)], |_| Some(1)), [0]);
    }

    #[test]
    fn modifiers_line_round_trips_and_skips_noise() {
        let ab24 = u32::from_le_bytes(*b"AB24");
        let xb24 = u32::from_le_bytes(*b"XB24");
        let text = format!(
            "{}\n{}\n{}\n{MODIFIERS_LINE} FAIL x\n",
            display_line(Some("/dev/dri/renderD129"), "NVIDIA"),
            modifiers_line(ab24, &[NV_BL2, NV_BL]),
            modifiers_line(xb24, &[]),
        );
        assert!(text.contains("gpu-modifiers: AB24=0x300000000606010,0x300000000e08014"));
        assert_eq!(
            parse_modifiers_lines(&text),
            [(ab24, vec![NV_BL2, NV_BL]), (xb24, vec![])]
        );
        // the modifier lines do not disturb the display line
        assert_eq!(
            parse_display_line(&text).map(|d| d.0),
            Some(Some("/dev/dri/renderD129".into()))
        );
        assert_eq!(parse_modifier_arg("AB2=0x0"), None);
        assert_eq!(parse_modifier_arg("AB24=12"), None);
    }

    #[test]
    fn probed_fourccs_are_the_gpu_rows() {
        let names: Vec<String> = probed_fourccs().map(fourcc_name).collect();
        assert_eq!(names, ["AB24", "XB24"]);
    }

    #[test]
    fn display_line_round_trips() {
        let text = format!(
            "noise\n{}\n",
            display_line(Some("/dev/dri/renderD128"), "Mesa Intel(R) Graphics (LNL)")
        );
        assert_eq!(
            parse_display_line(&text),
            Some((
                Some("/dev/dri/renderD128".to_owned()),
                "Mesa Intel(R) Graphics (LNL)".to_owned()
            ))
        );
        assert_eq!(
            parse_display_line(&display_line(None, "D3D12 (x)")),
            Some((None, "D3D12 (x)".to_owned()))
        );
        assert_eq!(parse_display_line("gpu-probe: FAIL eglInitialize"), None);
    }

    #[test]
    fn names_round_trip_and_unknown_is_refused() {
        assert_eq!(NodeSource::parse("display"), Some(NodeSource::Display));
        assert_eq!(NodeSource::parse("trial"), Some(NodeSource::Trial));
        assert_eq!(NodeSource::parse("Display"), None);
        assert_eq!(NodeSource::parse(""), None);
    }
}
