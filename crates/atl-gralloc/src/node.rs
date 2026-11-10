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

#[cfg(test)]
mod tests {
    use super::*;

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
