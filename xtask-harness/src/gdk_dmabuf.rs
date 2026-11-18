// SPDX-License-Identifier: MIT
//! The dmabuf present verdict from GTK's own debug output (`GDK_DEBUG=dmabuf`; message texts of GTK 4.18 and 4.24 in `gdk/gdkdmabuf*.c`, `gdkgl*.c`, `gsk/gpu/gskvulkanimage.c`,
//! `gskgpurenderer.c`): every buffer is imported by the renderer (GskGLRenderer: an EGLImage; GskVulkanRenderer: a Vulkan image on the dma-buf's memory), none is downloaded on the CPU
//! (owner decision O3: no CPU fallback). GDK advertises the formats its mmap downloader handles alongside the GPU ones (`mmap advertises dmabuf format`), so a format check before the import
//! cannot tell the paths apart; this log can.

/// What the gated client runs with.
pub const GDK_DEBUG: &str = "dmabuf";

/// `Imported %dx%d %.4s:%#lx dmabuf as %s texture` (gdkdmabufegl.c): an EGL import, by GskGLRenderer or by GDK's EGL downloader.
const IMPORTED: (&str, &str) = ("Imported ", " dmabuf as ");
/// `Vulkan uploaded %zux%zu %.4s:%016llx %sdmabuf` (gskvulkanimage.c `gsk_vulkan_image_new_for_dmabuf`): GskVulkanRenderer imported the dma-buf's memory as a Vulkan image (despite the word, no copy).
const VULKAN_IMPORTED: &str = "Vulkan uploaded ";
/// `Used mmap for downloading ...` and `Used %s for downloading ...`: a CPU download of the dma-buf.
const DOWNLOADED: &str = " for downloading ";
/// Import failures: EGL `Import of %dx%d %.4s:%#lx dmabuf failed`, `Creating EGLImage for dmabuf failed`, `Failed to create dmabuf texture`; Vulkan (gskvulkanimage.c, GTK 4.24) the
/// `gsk_vulkan_image_new_for_dmabuf` refusals and failures.
const FAILED: [&str; 9] = [
    " dmabuf failed",
    "Creating EGLImage for dmabuf failed",
    "Failed to create dmabuf texture",
    "Vulkan does not support dmabufs",
    "GTK's Vulkan doesn't support fourcc",
    "Vulkan driver does not support format",
    "vkCreateImage() failed",
    "vkAllocateMemory() failed",
    "Vulkan failed to dup() fd",
];

/// `Exported GL texture to dmabuf (format: ...)` (gdkgltexture.c): GDK made a dma-buf of a GL texture it rendered itself. Under GTK 4.18 a `render_texture` result read with a
/// TextureDownloader (the window's capture) goes export -> one import -> one `Used GskGLRenderer for downloading`: the readback of GDK's own picture, never a scene buffer.
const EXPORTED: &str = "Exported GL texture to dmabuf";

/// `Creating dmabuf texture, format %.4s:%#lx, ...` (gdkdmabuftexture.c): any GdkDmabufTexture is built, a scene buffer or GDK's own render. GskVulkanRenderer's `render_texture` exports its
/// image this way only (gskvulkanimage.c `gsk_vulkan_image_to_dmabuf_texture` logs no export line); its readback is then create -> the EGL downloader's import -> download of that same
/// format, with no renderer import between (laptop, GTK 4.24.1: 4 captures as `AB24:0x100000000000009`, scene buffers `XB24:0` each `Vulkan uploaded`).
const CREATING: &str = "Creating dmabuf texture, format ";

/// Counts of each path in one GTK log.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Verdict {
    /// Scene buffers imported (an own render's re-import not counted).
    pub imported: usize,
    /// Scene buffers downloaded (an own render's readback not counted).
    pub downloaded: usize,
    pub failed: usize,
    /// [`NO_IMPORT`] lines: the renderer has no dma-buf import.
    pub no_import: usize,
    /// GDK's own renders read back (captures): each [`EXPORTED`] line (excusing the one import and the one download after it), and each texture created, imported and downloaded with
    /// nothing between (a Vulkan [`CREATING`]).
    pub own_readbacks: usize,
}

/// The `XB24:0` of `... format XB24:0, ...` / `... (format XB24:0)`.
fn format_of(line: &str) -> Option<&str> {
    let rest = line.split_once("format ")?.1;
    rest.split([',', ')', ' ']).next()
}

impl Verdict {
    pub fn of(log: &str) -> Self {
        let mut v = Self::default();
        // an export's re-import and readback still to come
        let (mut own_import, mut own_download) = (0usize, 0usize);
        // the texture just created, and whether the EGL import of it came next: a download of it next is the readback of an own render
        let mut created: Option<(&str, bool)> = None;
        // the renderer is GskVulkanRenderer: it has imported a scene buffer
        let mut vulkan = false;
        for line in log.lines() {
            if line.contains(EXPORTED) {
                v.own_readbacks += 1;
                own_import += 1;
                own_download += 1;
                created = None;
            } else if line.contains(CREATING) {
                created = format_of(line).map(|f| (f, false));
            } else if line.contains(VULKAN_IMPORTED) {
                v.imported += 1;
                vulkan = true;
                created = None;
            } else if line.contains(IMPORTED.0) && line.contains(IMPORTED.1) {
                if own_import > 0 {
                    own_import -= 1;
                } else {
                    v.imported += 1;
                    // under GskVulkanRenderer an EGL import is only ever the downloader's
                    created = created.filter(|_| vulkan).map(|(f, _)| (f, true));
                }
            } else if line.contains(DOWNLOADED) {
                if own_download > 0 && own_import == 0 {
                    own_download -= 1;
                } else if let Some((f, true)) = created
                    && format_of(line) == Some(f)
                {
                    // its import was the downloader's, not the renderer's
                    v.imported -= 1;
                    v.own_readbacks += 1;
                } else {
                    v.downloaded += 1;
                }
                created = None;
            } else if FAILED.iter().any(|f| line.contains(f)) {
                v.failed += 1;
                created = None;
            } else if line.contains(NO_IMPORT) {
                v.no_import += 1;
            }
        }
        v
    }

    /// Imported at least once, never downloaded, no import failed.
    pub fn ok(&self) -> bool {
        self.imported > 0 && self.downloaded == 0 && self.failed == 0
    }

    /// The present tier the log shows, None when it fits none:
    /// `import` = [`Verdict::ok`];
    /// `gdk-no-import` = GDK's GL renderer itself lacks dma-buf import (its [`NO_IMPORT`] line; Mesa d3d12 in the app process, whether the driver's buffer tier is import or readback): nothing imported, every failed import one of those lines, and downloads -- the host toolkit's own path, as for any native GTK program there.
    /// Keyed on GDK's line only, never on the driver's tier: the copy is GDK's, and a host whose GDK can import must import.
    pub fn tier(&self) -> Option<&'static str> {
        if self.ok() {
            Some("import")
        } else if self.imported == 0
            && self.downloaded > 0
            && self.failed > 0
            && self.failed == self.no_import
        {
            Some("gdk-no-import")
        } else {
            None
        }
    }
}

/// GDK 4.18 `gdk_dmabuf_egl_import_dmabuf` (gdkdmabufegl.c): `Can't import dmabufs into GL, missing EGL or EGL_EXT_image_dma_buf_import_modifiers`, printed before each `Import of ... failed` when the GL context cannot import at all.
const NO_IMPORT: &str = "Can't import dmabufs into GL";

impl std::fmt::Display for Verdict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "imported={} downloaded={} failed={} no_import={} own_readbacks={}",
            self.imported, self.downloaded, self.failed, self.no_import, self.own_readbacks
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Lines in GTK 4.18's formats (the format strings above, filled in as GDK prints them).
    #[test]
    fn counts_each_path() {
        let log = "Gdk-DEBUG: Importing dmabuf (format: AB24:0, planes: 1) into GL\n\
                   Gdk-DEBUG: Imported 256x192 AB24:0 dmabuf as GL_TEXTURE_2D texture\n\
                   Gdk-DEBUG: mmap advertises dmabuf format AB24::0000000000000000\n";
        let v = Verdict::of(log);
        assert_eq!(
            v,
            Verdict {
                imported: 1,
                downloaded: 0,
                failed: 0,
                no_import: 0,
                own_readbacks: 0
            }
        );
        assert!(v.ok());
        let bad = Verdict::of(
            "Gdk-DEBUG: Used mmap for downloading 256x192 dmabuf (format AB24:0)\n\
             Gdk-DEBUG: Import of 4x4 ZZ99:0 dmabuf failed\n",
        );
        assert_eq!((bad.downloaded, bad.failed), (1, 1));
        assert!(!bad.ok());
    }

    /// A capture's round trip as GTK 4.18 printed it (job rgm platform-ui --verbose): export, the export's import, its readback. Only the scene buffer before it counts.
    #[test]
    fn a_capture_readback_is_not_a_scene_download() {
        let scene = "Imported 320x240 XB24:0 dmabuf as GL_TEXTURE_2D texture\n";
        let capture = "Exporting GL texture to dmabuf\n\
                       Exported GL texture to dmabuf (format: AB24:0, planes: 1)\n\
                       Creating dmabuf texture, format AB24:0,  premultiplied, 1 planes, memory format 2\n\
                       Imported 320x240 AB24:0 dmabuf as GL_TEXTURE_2D texture\n\
                       Used GskGLRenderer for downloading 320x240 dmabuf (format AB24:0)\n";
        let v = Verdict::of(&format!("{scene}{capture}").repeat(3));
        assert_eq!((v.imported, v.downloaded, v.own_readbacks), (3, 0, 3));
        assert!(v.ok());
        // a scene download beside captures still counts: one export excuses one readback only
        let mmap = "Used mmap for downloading 320x240 dmabuf (format XB24:0)\n";
        let v = Verdict::of(&format!("{capture}{mmap}"));
        assert_eq!((v.downloaded, v.ok()), (1, false));
        // a download before the export's re-import is not its readback
        let early = "Exported GL texture to dmabuf (format: AB24:0, planes: 1)\nUsed mmap for downloading 320x240 dmabuf (format XB24:0)\n";
        assert_eq!(Verdict::of(early).downloaded, 1);
    }

    /// GskVulkanRenderer as GTK 4.24.1 printed it on the laptop (job rwgb-lw3): scene buffers created then `Vulkan uploaded`; each capture's own render created, imported by the EGL
    /// downloader and read back. 157 scene imports and 4 captures there; 2 + 2 here.
    #[test]
    fn vulkan_renderer_imports_and_its_captures() {
        let scene = "Creating dmabuf texture, format XB24:0,  premultiplied, 1 planes, memory format RGBX8\n\
                     Vulkan uploaded 960x600 XB24:0000000000000000 dmabuf\n";
        let capture = "Creating dmabuf texture, format AB24:0x100000000000009,  premultiplied, 1 planes, memory format RGBA8p\n\
                       Importing dmabuf (format: AB24:0x100000000000009, planes: 1) into GL\n\
                       Imported 960x600 AB24:0x100000000000009 dmabuf as GL_TEXTURE_2D texture\n\
                       Used GskGLRenderer for downloading 960x600 dmabuf (format AB24:0x100000000000009)\n";
        let v = Verdict::of(&format!("{scene}{scene}{capture}{capture}"));
        assert_eq!((v.imported, v.downloaded, v.own_readbacks), (2, 0, 2));
        assert_eq!(v.tier(), Some("import"));
        // a scene buffer the Vulkan renderer refused, then downloaded: red
        let refused = "Creating dmabuf texture, format XB24:0,  premultiplied, 1 planes, memory format RGBX8\n\
                       Vulkan driver does not support format XB24::0000000000000000 with 1 planes\n\
                       Used mmap for downloading 960x600 dmabuf (format XB24:0)\n";
        let v = Verdict::of(&format!("{scene}{refused}"));
        assert_eq!((v.downloaded, v.failed, v.tier()), (1, 1, None));
        // a download of another format than the texture just created is no readback of it
        let other = "Creating dmabuf texture, format AB24:0x100000000000009,  premultiplied, 1 planes, memory format RGBA8p\n\
                     Imported 960x600 AB24:0x100000000000009 dmabuf as GL_TEXTURE_2D texture\n\
                     Used mmap for downloading 960x600 dmabuf (format XB24:0)\n";
        assert_eq!(Verdict::of(&format!("{scene}{other}")).downloaded, 1);
        // the same round trip under GskGLRenderer (no Vulkan import seen): the EGL import is the renderer's, the download a scene download
        assert_eq!(Verdict::of(capture).downloaded, 1);
    }

    /// GDK's renderer without import (lines as the WSL dxgkrnl run printed them, on either driver buffer tier): downloads pass only when every failed import follows GDK's no-import line and nothing imported.
    #[test]
    fn gdk_without_import_downloads() {
        let frame = "Creating dmabuf texture, format XB24:0,  premultiplied, 1 planes, memory format 31\n\
                     Can't import dmabufs into GL, missing EGL or EGL_EXT_image_dma_buf_import_modifiers\n\
                     Import of 1440x900 XB24:0 dmabuf failed\n\
                     Used mmap for downloading 1440x900 dmabuf (format XB24:0)\n";
        let tier_of = |log: &str| Verdict::of(log).tier();
        let three = frame.repeat(3);
        assert_eq!(tier_of(&three), Some("gdk-no-import"));
        // the driver's readback tier does not change GDK's verdict (job tr-play-m1: every download beside GDK's no-import line)
        let readback = "libEGL_atl: buffer tier readback (this device cannot import gralloc's buffer, EGL error 0x3003)\nlibEGL_atl: readback_frames=64 ms=1003 frames=40 sync_us=12/40 read_us=2100/3500\n";
        assert_eq!(
            tier_of(&format!("{readback}{three}")),
            Some("gdk-no-import")
        );
        // ... nor admits downloads GDK does not explain: the driver tier alone is red
        let mmap = "Gdk-DEBUG: Used mmap for downloading 562x900 dmabuf (format AB24:0)\n";
        assert_eq!(tier_of(&format!("{readback}{mmap}")), None);
        // a failure GDK does not explain by missing import: red
        let other = format!("{three}Import of 4x4 ZZ99:0 dmabuf failed\n");
        assert_eq!(tier_of(&other), None);
        // the renderer imported once, so it can: the downloads are a fallback, red
        let mixed = format!("{three}Imported 1440x900 XB24:0 dmabuf as GL_TEXTURE_2D texture\n");
        assert_eq!(tier_of(&mixed), None);
        // downloads without GDK's no-import line: red
        let bare = "Used mmap for downloading 1440x900 dmabuf (format XB24:0)\n";
        assert_eq!(tier_of(bare), None);
    }
}
