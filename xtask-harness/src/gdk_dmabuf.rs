// SPDX-License-Identifier: MIT
//! The dmabuf present verdict from GTK's own debug output (`GDK_DEBUG=dmabuf`, GTK 4.18 message texts in `gdk/gdkdmabuf*.c` / `gdkgl*.c`): every buffer is imported by the renderer (GL: an EGLImage), none is downloaded on the CPU (owner decision O3: no CPU fallback).
//! GDK advertises the formats its mmap downloader handles alongside the GPU ones (`mmap advertises dmabuf format`), so a format check before the import cannot tell the paths apart; this log can.

/// What the gated client runs with.
pub const GDK_DEBUG: &str = "dmabuf";

/// `Imported %dx%d %.4s:%#lx dmabuf as %s texture`.
const IMPORTED: (&str, &str) = ("Imported ", " dmabuf as ");
/// `Used mmap for downloading ...` and `Used %s for downloading ...`: a CPU download of the dma-buf.
const DOWNLOADED: &str = " for downloading ";
/// Import failures: `Import of %dx%d %.4s:%#lx dmabuf failed`, `Creating EGLImage for dmabuf failed`, `Failed to create dmabuf texture`.
const FAILED: [&str; 3] = [
    " dmabuf failed",
    "Creating EGLImage for dmabuf failed",
    "Failed to create dmabuf texture",
];

/// Counts of each path in one GTK log.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Verdict {
    pub imported: usize,
    pub downloaded: usize,
    pub failed: usize,
}

impl Verdict {
    pub fn of(log: &str) -> Self {
        let mut v = Self::default();
        for line in log.lines() {
            if line.contains(IMPORTED.0) && line.contains(IMPORTED.1) {
                v.imported += 1;
            } else if line.contains(DOWNLOADED) {
                v.downloaded += 1;
            } else if FAILED.iter().any(|f| line.contains(f)) {
                v.failed += 1;
            }
        }
        v
    }

    /// Imported at least once, never downloaded, no import failed.
    pub fn ok(&self) -> bool {
        self.imported > 0 && self.downloaded == 0 && self.failed == 0
    }

    /// [`Verdict::ok`], or the readback tier's presenter path (M58, plan gpu-readback S5 Q1): downloads allowed only when the same log has the driver's `buffer tier readback` line and a readback counter line > 0; still no failed import.
    pub fn ok_in(&self, log: &str) -> bool {
        self.ok() || (self.failed == 0 && self.downloaded > 0 && readback_frames(log) > 0)
    }
}

/// atl-egl-driver surface.rs: its tier line and its counter line (`READBACK_FRAMES_LINE`, logged at powers of two).
const READBACK_TIER: &str = "libGLES_atl: buffer tier readback";
const READBACK_FRAMES: &str = "libGLES_atl: readback_frames=";

/// The largest readback count the driver logged, 0 without its tier line.
pub fn readback_frames(log: &str) -> u64 {
    if !log.lines().any(|l| l.contains(READBACK_TIER)) {
        return 0;
    }
    log.lines()
        .filter_map(|l| l.split_once(READBACK_FRAMES))
        .filter_map(|(_, n)| n.trim().parse().ok())
        .max()
        .unwrap_or(0)
}

impl std::fmt::Display for Verdict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "imported={} downloaded={} failed={}",
            self.imported, self.downloaded, self.failed
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
                failed: 0
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

    /// mmap downloads pass only beside the driver's readback tier line and a counter > 0 (the rule's break: the same downloads without them are red).
    #[test]
    fn mmap_needs_counted_readback() {
        let mmap = "Gdk-DEBUG: Used mmap for downloading 562x900 dmabuf (format AB24:0)\n";
        let tier = "libGLES_atl: buffer tier readback (this device cannot import gralloc's buffer, EGL error 0x3003)\n";
        let counted = format!(
            "{tier}libGLES_atl: readback_frames=1\nlibGLES_atl: readback_frames=64\n{mmap}"
        );
        assert_eq!(readback_frames(&counted), 64);
        assert!(Verdict::of(&counted).ok_in(&counted));
        assert!(!Verdict::of(mmap).ok_in(mmap));
        let uncounted = format!("{tier}{mmap}");
        assert!(!Verdict::of(&uncounted).ok_in(&uncounted));
        let untiered = format!("libGLES_atl: readback_frames=8\n{mmap}");
        assert!(!Verdict::of(&untiered).ok_in(&untiered));
    }
}
