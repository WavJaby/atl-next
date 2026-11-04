// SPDX-License-Identifier: MIT
//! The pixel formats the allocator supports: a table keyed by `android.hardware.graphics.common.PixelFormat`.

/// `PixelFormat.RGBA_8888` (hardware/interfaces/graphics/common/aidl PixelFormat.aidl `RGBA_8888 = 0x1`).
pub const RGBA_8888: i32 = 0x1;

/// `PixelFormat.RGBX_8888` (PixelFormat.aidl `RGBX_8888 = 0x2`): what BLASTBufferQueue::convertBufferFormat turns PIXEL_FORMAT_OPAQUE into, so every opaque window asks for it.
pub const RGBX_8888: i32 = 0x2;

/// DRM_FORMAT_ABGR8888, fourcc 'AB24' (drm_fourcc.h `fourcc_code('A','B','2','4')`): little-endian word A,B,G,R = memory bytes R,G,B,A, which is RGBA_8888's byte order.
pub const DRM_FORMAT_ABGR8888: u32 = u32::from_le_bytes(*b"AB24");

/// DRM_FORMAT_XBGR8888, fourcc 'XB24' (drm_fourcc.h): ABGR8888 with the top byte, memory byte 3, unused.
pub const DRM_FORMAT_XBGR8888: u32 = u32::from_le_bytes(*b"XB24");

/// What the allocator knows about a pixel format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FormatInfo {
    /// `PixelFormat` value.
    pub pixel_format: i32,
    /// DRM fourcc of the same memory layout.
    pub fourcc: u32,
    /// Bytes of one pixel (single plane, packed).
    pub bytes_per_pixel: u32,
    /// Bit offset and size of each component in a pixel, in memory order: (`PlaneLayoutComponentType` value, offset, size).
    pub components: &'static [(i64, i64, i64)],
}

/// `PlaneLayoutComponentType` values (PlaneLayoutComponentType.aidl): R = 1 << 10, G = 1 << 11, B = 1 << 12, A = 1 << 30.
pub const COMPONENT_R: i64 = 1 << 10;
pub const COMPONENT_G: i64 = 1 << 11;
pub const COMPONENT_B: i64 = 1 << 12;
pub const COMPONENT_A: i64 = 1 << 30;

/// Supported formats. Add a row when a consumer first needs one (AGENTS.md "Rows are added when a consumer first needs them"): libui's software path (`Surface::lock`) allocates RGBA_8888, an opaque window's BLAST queue RGBX_8888. RGBX has no alpha component: byte 3 is padding, its value undefined.
const FORMATS: &[FormatInfo] = &[
    FormatInfo {
        pixel_format: RGBA_8888,
        fourcc: DRM_FORMAT_ABGR8888,
        bytes_per_pixel: 4,
        components: &[
            (COMPONENT_R, 0, 8),
            (COMPONENT_G, 8, 8),
            (COMPONENT_B, 16, 8),
            (COMPONENT_A, 24, 8),
        ],
    },
    FormatInfo {
        pixel_format: RGBX_8888,
        fourcc: DRM_FORMAT_XBGR8888,
        bytes_per_pixel: 4,
        components: &[
            (COMPONENT_R, 0, 8),
            (COMPONENT_G, 8, 8),
            (COMPONENT_B, 16, 8),
        ],
    },
];

/// The row of `pixel_format`, if supported.
pub fn lookup(pixel_format: i32) -> Option<&'static FormatInfo> {
    FORMATS.iter().find(|f| f.pixel_format == pixel_format)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 'X'=0x58 'B'=0x42 '2'=0x32 '4'=0x34, least significant byte first (drm_fourcc.h `fourcc_code('X', 'B', '2', '4')`).
    #[test]
    fn xbgr_fourcc_value() {
        assert_eq!(DRM_FORMAT_XBGR8888, 0x3432_4258);
    }

    /// 'A'=0x41 'B'=0x42 '2'=0x32 '4'=0x34, least significant byte first: 0x34324241 (the value gbm.h prints for GBM_FORMAT_ABGR8888, measured with libgbm on this host: stride 256 for 64 px, i.e. 4 bytes per pixel).
    #[test]
    fn fourcc_value() {
        assert_eq!(DRM_FORMAT_ABGR8888, 0x3432_4241);
    }

    #[test]
    fn lookup_known_and_unknown() {
        assert_eq!(lookup(RGBA_8888).map(|f| f.bytes_per_pixel), Some(4));
        assert_eq!(lookup(RGBX_8888).map(|f| f.bytes_per_pixel), Some(4));
        // RGB_565 (4) and BGRA_8888 (5) have no row yet
        for f in [0, 4, 5, -1] {
            assert!(lookup(f).is_none(), "{f}");
        }
    }

    #[test]
    fn components_cover_the_pixel() {
        let f = lookup(RGBA_8888).expect("row");
        let bits: i64 = f.components.iter().map(|c| c.2).sum();
        assert_eq!(bits, i64::from(f.bytes_per_pixel) * 8);
    }

    /// Same memory layout as RGBA_8888 minus the alpha component (24 of the 32 bits are colour).
    #[test]
    fn rgbx_is_rgba_without_alpha() {
        let (a, x) = (
            lookup(RGBA_8888).expect("row"),
            lookup(RGBX_8888).expect("row"),
        );
        assert_eq!(x.components, &a.components[..3]);
    }
}
