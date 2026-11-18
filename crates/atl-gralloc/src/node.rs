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

/// The probe's `--renderer` line: what a GLES context on one EGL device reports, under the probe's environment (atld's render-path choice, atl-broker gpu::RENDER_PATHS).
pub const RENDERER_LINE: &str = "gpu-renderer:";

/// The GL extension whose presence makes a device declare `android.hardware.opengles.aep` (CDD 7.1.4.1; PackageManager.FEATURE_OPENGLES_EXTENSION_PACK).
pub const AEP_EXTENSION: &str = "GL_ANDROID_extension_pack_es31a";

/// What [`RENDERER_LINE`] reports of one GLES context.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Renderer {
    /// GL_VERSION
    pub version: String,
    /// GL_EXTENSIONS lists [`AEP_EXTENSION`]
    pub aep: bool,
    /// GL_RENDERER
    pub renderer: String,
}

/// `<prefix> version=<GL_VERSION> aep=<0|1> renderer=<GL_RENDERER>` (version first: neither string is quoted, the renderer runs to the end).
pub fn renderer_line(r: &Renderer) -> String {
    format!(
        "{RENDERER_LINE} version={} aep={} renderer={}",
        r.version,
        u8::from(r.aep),
        r.renderer
    )
}

/// The [`Renderer`] of the last well-formed [`RENDERER_LINE`] in `text`.
pub fn parse_renderer_line(text: &str) -> Option<Renderer> {
    let line = text
        .lines()
        .rev()
        .find_map(|l| l.strip_prefix(RENDERER_LINE))?;
    let (version, rest) = line
        .trim_start()
        .strip_prefix("version=")?
        .split_once(" aep=")?;
    let (aep, renderer) = rest.split_once(" renderer=")?;
    let aep = match aep {
        "0" => false,
        "1" => true,
        _ => return None,
    };
    Some(Renderer {
        version: version.to_owned(),
        aep,
        renderer: renderer.to_owned(),
    })
}

/// What `atl-egl-probe --trial-node` prints: `<prefix> node=<path>`, the node gralloc's own trial picks (first of /dev/dri that allocates), for runs whose display names none.
pub const TRIAL_LINE: &str = "gpu-trial:";

pub fn trial_line(node: &str) -> String {
    format!("{TRIAL_LINE} node={node}")
}

/// The node of the last well-formed [`TRIAL_LINE`].
pub fn parse_trial_line(text: &str) -> Option<String> {
    text.lines()
        .rev()
        .find_map(|l| {
            l.strip_prefix(TRIAL_LINE)?
                .trim_start()
                .strip_prefix("node=")
        })
        .filter(|n| !n.is_empty())
        .map(str::to_owned)
}

/// The kernel's udmabuf misc device (udmabuf.c `udmabuf_misc`, name "udmabuf"): gralloc's udmabuf tier allocates through it, atld checks it opens and binds it into gralloc's sandbox for that tier only.
pub const UDMABUF_DEVICE: &str = "/dev/udmabuf";

/// The probe's import lines (`--renderer`, run in the app processes' environment): `<prefix> device ok|<why>` for the EGL device the app's driver renders on, `<prefix> display ok|<why>` for the Wayland display GDK presents with.
pub const IMPORT_LINE: &str = "gpu-import:";

/// Whether each GPU of an app process imports a gralloc buffer (LINEAR, on the run's node): Err holds why not.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Imports {
    /// The app's EGL device: import + complete FBO (what the driver's `buffer tier import` needs).
    pub device: Result<(), String>,
    /// The desktop's Wayland EGL display: import + texture (what GDK's GL import needs).
    pub display: Result<(), String>,
}

const IMPORT_OK: &str = "ok";

/// The probe's line for one importer (`device` or `display`).
pub fn import_line(who: &str, r: &Result<(), String>) -> String {
    match r {
        Ok(()) => format!("{IMPORT_LINE} {who} {IMPORT_OK}"),
        Err(why) => format!("{IMPORT_LINE} {who} {}", why.replace('\n', " ")),
    }
}

/// Both importers from the probe's output (last line of each wins); None when either is missing.
pub fn parse_import_lines(text: &str) -> Option<Imports> {
    let find = |who: &str| {
        text.lines()
            .rev()
            .filter_map(|l| l.strip_prefix(IMPORT_LINE))
            .find_map(|l| l.trim_start().strip_prefix(who)?.strip_prefix(' '))
            .map(|v| {
                if v == IMPORT_OK {
                    Ok(())
                } else {
                    Err(v.to_owned())
                }
            })
    };
    Some(Imports {
        device: find("device")?,
        display: find("display")?,
    })
}

/// gralloc's allocation tier: one allocator, two backings behind its one `allocate` (atl-gralloc-service backend.rs).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AllocatorTier {
    /// GBM on the run's DRM node: GPU-importable memory (zero-copy where a GPU imports it). The baseline: works on every host gralloc runs on.
    Gbm,
    /// memfd pages through `/dev/udmabuf`: cached system memory, for hosts where no GPU in the process imports the buffer and every access is a CPU access.
    Udmabuf,
}

impl AllocatorTier {
    pub fn name(self) -> &'static str {
        match self {
            AllocatorTier::Gbm => "gbm",
            AllocatorTier::Udmabuf => "udmabuf",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        [AllocatorTier::Gbm, AllocatorTier::Udmabuf]
            .into_iter()
            .find(|t| t.name() == s)
    }
}

/// The tier line's prefix: `allocator tier <name> (<why>)`, printed by gralloc (`gralloc: ` before it) and by atld.
pub const TIER_LINE: &str = "allocator tier ";

pub fn tier_line(tier: AllocatorTier, why: &str) -> String {
    format!("{TIER_LINE}{} ({why})", tier.name())
}

/// The (tier, why) of the last well-formed tier line in `text`; a line that only contains the words (atld's `linear allocator tier <codec2 tier>`) is not one.
pub fn parse_tier_line(text: &str) -> Option<(AllocatorTier, String)> {
    text.lines().rev().find_map(|l| {
        let at = l.find(TIER_LINE)?;
        let (name, why) = l[at + TIER_LINE.len()..].split_once(" (")?;
        Some((
            AllocatorTier::parse(name)?,
            why.strip_suffix(')')?.to_owned(),
        ))
    })
}

/// The tier atld asks gralloc for: udmabuf only when neither GPU of an app process imports gralloc's buffer and `/dev/udmabuf` opens (`udmabuf`: Err = why not); otherwise GBM, today's path. `imports` None: no probe answered, nothing known -> GBM.
pub fn choose_allocator_tier(
    imports: Option<&Imports>,
    udmabuf: Result<(), String>,
) -> (AllocatorTier, String) {
    let Some(i) = imports else {
        return (AllocatorTier::Gbm, "no import probe".to_owned());
    };
    match (&i.device, &i.display) {
        (Ok(()), _) => (AllocatorTier::Gbm, "the app's GPU imports".to_owned()),
        (_, Ok(())) => (AllocatorTier::Gbm, "the display imports".to_owned()),
        (Err(dev), Err(disp)) => {
            let none = format!("no GPU in the process imports: device {dev}; display {disp}");
            match udmabuf {
                Ok(()) => (AllocatorTier::Udmabuf, none),
                Err(why) => (
                    AllocatorTier::Gbm,
                    format!("{none}; udmabuf unavailable: {why}"),
                ),
            }
        }
    }
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

/// The set for a fourcc whose explicit set ([`modifier_set`]) is `explicit`: that set where it is non-empty; else `[DRM_FORMAT_MOD_INVALID]` (the implicit tier: the driver's own layout, no modifier named) when `implicit` -- the probe's trial of a modifier-less GBM buffer imported on the device (as an FBO) and on the presenter -- succeeded; else empty (refused).
/// INVALID as a set member is the Linux convention: linux-dmabuf's format table and GDK's `GdkDmabufFormats` list it for "implicit modifier", and EXT_image_dma_buf_import_modifiers treats it as no modifier given. Only meaningful on the device that allocated it: the probe allocates on the display's node, the node the app renders on.
/// Explicit wins: a host that lists modifiers keeps exactly its explicit set (EGL lists none on implicit-only drivers: radeonsi before GFX9, virgl).
pub fn with_implicit(explicit: Vec<u64>, implicit: &Result<(), String>) -> Vec<u64> {
    if !explicit.is_empty() {
        return explicit;
    }
    match implicit {
        Ok(()) => vec![crate::format::DRM_FORMAT_MOD_INVALID],
        Err(_) => explicit,
    }
}

/// The modifier tier a probed set names, for atld's `modifier tier <FOURCC> <name>` line: `implicit` (`[INVALID]`), `explicit` (named modifiers), `none` (empty: GPU-only buffers refused).
pub fn modifier_tier_name(mods: &[u64]) -> &'static str {
    match mods {
        [] => "none",
        [crate::format::DRM_FORMAT_MOD_INVALID] => "implicit",
        _ => "explicit",
    }
}

/// The probe's line when a fourcc's explicit set is empty: `<prefix> <FOURCC> ok|<why>`, the implicit trial's result (informational; the set on the [`MODIFIERS_LINE`] carries the decision).
pub const IMPLICIT_LINE: &str = "gpu-implicit:";

pub fn implicit_line(fourcc: u32, r: &Result<(), String>) -> String {
    let fourcc = fourcc_name(fourcc);
    match r {
        Ok(()) => format!("{IMPLICIT_LINE} {fourcc} {IMPORT_OK}"),
        Err(why) => format!("{IMPLICIT_LINE} {fourcc} {}", why.replace('\n', " ")),
    }
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

    /// Strings as WSL's d3d12 reports them (gpu-readback slice 1): spaces and parentheses in both survive the round trip.
    #[test]
    fn renderer_line_round_trips() {
        for aep in [false, true] {
            let r = Renderer {
                version: "OpenGL ES 3.1 Mesa 25.0.7-2".to_owned(),
                aep,
                renderer: "D3D12 (NVIDIA GeForce GTX 1080 Ti)".to_owned(),
            };
            let text = format!("noise\n{}\n", renderer_line(&r));
            assert_eq!(parse_renderer_line(&text), Some(r));
        }
        assert_eq!(parse_renderer_line("gpu-renderer: FAIL x"), None);
        // an aep value other than 0 / 1, or a line without the field, is not well-formed
        assert_eq!(
            parse_renderer_line("gpu-renderer: version=v aep=yes renderer=r"),
            None
        );
        assert_eq!(
            parse_renderer_line("gpu-renderer: version=v renderer=r"),
            None
        );
    }

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

    /// The implicit tier: only an empty explicit set with a working implicit trial gives `[INVALID]` (rvvm virgl and sifive radeonsi GFX8 probed `XB24=` empty); a non-empty explicit set is kept whatever the trial says; a failed trial stays empty.
    #[test]
    fn implicit_tier_only_when_nothing_explicit_and_the_trial_imports() {
        const INVALID: u64 = crate::format::DRM_FORMAT_MOD_INVALID;
        let ok = Ok(());
        let fail = Err("EGL error 0x3003".to_owned());
        assert_eq!(with_implicit(vec![], &ok), [INVALID]);
        assert_eq!(with_implicit(vec![], &fail), Vec::<u64>::new());
        assert_eq!(with_implicit(vec![NV_BL2, NV_BL], &ok), [NV_BL2, NV_BL]);
        assert_eq!(with_implicit(vec![0], &fail), [0]);
        assert_eq!(modifier_tier_name(&[INVALID]), "implicit");
        assert_eq!(modifier_tier_name(&[]), "none");
        assert_eq!(modifier_tier_name(&[0]), "explicit");
        assert_eq!(modifier_tier_name(&[0, INVALID]), "explicit");
        // INVALID = 0x00ff_ffff_ffff_ffff (drm_fourcc.h) survives the --modifiers round trip
        let xb24 = u32::from_le_bytes(*b"XB24");
        let arg = modifier_arg(xb24, &[INVALID]);
        assert_eq!(arg, "XB24=0xffffffffffffff");
        assert_eq!(parse_modifier_arg(&arg), Some((xb24, vec![INVALID])));
        assert_eq!(
            implicit_line(xb24, &fail),
            "gpu-implicit: XB24 EGL error 0x3003"
        );
        assert_eq!(implicit_line(xb24, &ok), "gpu-implicit: XB24 ok");
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

    /// The WSL case as the app log reports it (driver: EGL error 0x3003; GDK: no import extension), the laptop case (device imports), and the two udmabuf answers.
    #[test]
    fn tier_is_udmabuf_only_when_nothing_imports_and_udmabuf_opens() {
        let fail = |w: &str| Err(w.to_owned());
        let wsl = Imports {
            device: fail("EGL error 0x3003"),
            display: fail("no EGL_EXT_image_dma_buf_import"),
        };
        let (t, why) = choose_allocator_tier(Some(&wsl), Ok(()));
        assert_eq!(t, AllocatorTier::Udmabuf);
        assert!(
            why.contains("0x3003") && why.contains("display no EGL"),
            "{why}"
        );
        let (t, why) = choose_allocator_tier(Some(&wsl), fail("/dev/udmabuf: Permission denied"));
        assert_eq!(t, AllocatorTier::Gbm);
        assert!(
            why.contains("udmabuf unavailable: /dev/udmabuf: Permission denied"),
            "{why}"
        );
        for imports in [
            Imports {
                device: Ok(()),
                display: Ok(()),
            },
            Imports {
                device: Ok(()),
                display: fail("x"),
            },
            Imports {
                device: fail("x"),
                display: Ok(()),
            },
        ] {
            assert_eq!(
                choose_allocator_tier(Some(&imports), Ok(())).0,
                AllocatorTier::Gbm
            );
        }
        assert_eq!(choose_allocator_tier(None, Ok(())).0, AllocatorTier::Gbm);
    }

    #[test]
    fn trial_line_round_trips() {
        let text = format!("noise\n{}\n", trial_line("/dev/dri/card0"));
        assert_eq!(parse_trial_line(&text), Some("/dev/dri/card0".to_owned()));
        assert_eq!(parse_trial_line("gpu-trial: FAIL no node"), None);
        assert_eq!(parse_trial_line("gpu-trial: node="), None);
    }

    #[test]
    fn import_and_tier_lines_round_trip() {
        let i = Imports {
            device: Err("EGL error 0x3003\nsecond".to_owned()),
            display: Ok(()),
        };
        let text = format!(
            "noise\n{}\n{}\n",
            import_line("device", &i.device),
            import_line("display", &i.display)
        );
        assert_eq!(
            parse_import_lines(&text),
            Some(Imports {
                device: Err("EGL error 0x3003 second".to_owned()),
                display: Ok(())
            })
        );
        assert_eq!(parse_import_lines("gpu-import: device ok\n"), None);
        let line = format!("gralloc: {}", tier_line(AllocatorTier::Udmabuf, "a (b)"));
        assert_eq!(
            parse_tier_line(&line),
            Some((AllocatorTier::Udmabuf, "a (b)".to_owned()))
        );
        assert_eq!(parse_tier_line("allocator tier vram (x)"), None);
        // atld's Codec2 line after the real one (as the WSL atld.log orders them, reversed) does not hide it
        let atld = "atld: allocator tier udmabuf (why)\natld: linear allocator tier gralloc-blob\n";
        assert_eq!(
            parse_tier_line(atld),
            Some((AllocatorTier::Udmabuf, "why".to_owned()))
        );
    }

    #[test]
    fn names_round_trip_and_unknown_is_refused() {
        assert_eq!(NodeSource::parse("display"), Some(NodeSource::Display));
        assert_eq!(NodeSource::parse("trial"), Some(NodeSource::Trial));
        assert_eq!(NodeSource::parse("Display"), None);
        assert_eq!(NodeSource::parse(""), None);
    }
}
