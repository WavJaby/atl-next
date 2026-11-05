// SPDX-License-Identifier: MIT
//! A buffer's dma-buf layout: what the mapper answers for `PIXEL_FORMAT_FOURCC`, `PIXEL_FORMAT_MODIFIER`, `PLANE_LAYOUTS` and `STRIDE`, and what an importer (GTK's dmabuf texture, an EGL image, the CPU reader) needs.
//! One definition: the metadata encoder and every importer read it from here, so the two cannot disagree.

use crate::handle::{FD_DMABUF, HandleError, Params};

/// One plane of the memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Plane {
    /// Index of the plane's dma-buf among the handle's fds.
    pub fd_index: usize,
    /// Bytes from the start of the dma-buf to the plane's first row.
    pub offset: u64,
    /// Bytes from the start of one row to the next.
    pub stride_bytes: u32,
}

/// The layout of one buffer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    pub width: u32,
    pub height: u32,
    /// DRM fourcc.
    pub fourcc: u32,
    /// DRM format modifier, one for all planes.
    pub modifier: u64,
    /// Bytes of one pixel of a packed single-plane format (`PLANE_LAYOUTS` sampleIncrement, `STRIDE` divisor).
    pub bytes_per_pixel: u32,
    /// The planes; one for every format in the table now.
    pub planes: Vec<Plane>,
}

impl Layout {
    /// `StandardMetadataType::STRIDE`: pixels per row of plane 0.
    pub fn pixel_stride(&self) -> u32 {
        self.planes[0].stride_bytes / self.bytes_per_pixel
    }
}

impl Params {
    /// The layout these handle ints describe.
    pub fn layout(&self) -> Layout {
        Layout {
            width: self.width,
            height: self.height,
            fourcc: self.fourcc,
            modifier: self.modifier,
            bytes_per_pixel: self.format().bytes_per_pixel,
            planes: vec![Plane {
                fd_index: FD_DMABUF,
                offset: 0,
                stride_bytes: self.pitch,
            }],
        }
    }
}

/// The layout of a handle with `num_fds` fds and `ints`, validated as [`Params::from_handle`] does.
pub fn from_handle(num_fds: usize, ints: &[i32]) -> Result<Layout, HandleError> {
    Params::from_handle(num_fds, ints).map(|p| p.layout())
}
