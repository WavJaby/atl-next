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
}
