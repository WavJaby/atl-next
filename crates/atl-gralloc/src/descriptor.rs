// SPDX-License-Identifier: MIT
//! The allocation request (`android.hardware.graphics.allocator.BufferDescriptorInfo`) and its validation against what the allocator supports.

use crate::format::{self, FormatInfo};
use crate::usage;

/// Longest buffer name kept: BufferDescriptorInfo.name is `byte[128]`, null-terminated, and "IAllocator implementations proactively do `name[127] = 0`" (BufferDescriptorInfo.aidl).
pub const NAME_MAX: usize = 127;

/// Largest width / height accepted. The dumb / GBM allocators of every driver take 16384 or more; 16384 keeps `pitch * height` far below `u32::MAX * 4` and a request this large is no window buffer.
/// Not a hardware limit: the backend still refuses what the GPU cannot hold.
pub const DIMENSION_MAX: u32 = 16384;

/// Largest BLOB (bytes): [`DIMENSION_MAX`] rows of [`format::BLOB_ROW_BYTES`] = 64 MiB, the backing buffer's largest shape. Codec2's linear blocks are KiB to a few MiB.
pub const BLOB_MAX: u32 = DIMENSION_MAX * format::BLOB_ROW_BYTES;

/// A request, as the AIDL parcelable carries it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Descriptor {
    /// Raw `name` bytes (up to the first NUL, cut to [`NAME_MAX`]).
    pub name: Vec<u8>,
    pub width: i32,
    pub height: i32,
    pub layer_count: i32,
    pub format: i32,
    pub usage: u64,
    pub reserved_size: i64,
    /// `additionalOptions` names: any is unrecognised (no option exists in our set).
    pub option_names: Vec<String>,
    /// DRM modifiers the buffer may use, any one; empty = LINEAR only. Not in the AIDL: the modifier set is device policy the allocator adds.
    pub modifiers: Vec<u64>,
}

/// Why a request cannot be served: `AllocationError::UNSUPPORTED` on the wire, and `isSupported()` = false (IAllocator.aidl: reject unrecognised usage and options).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// Zero, negative or too large dimension, or layer count other than 1.
    BadShape,
    UnsupportedFormat(i32),
    /// The usage bits outside [`usage::SUPPORTED`].
    UnsupportedUsage(u64),
    /// A reserved region was requested (`getReservedRegion` is not offered).
    ReservedRegion,
    UnrecognisedOption(String),
    /// No modifier of the set is one the backend allocates (LINEAR only).
    UnsupportedModifiers(Vec<u64>),
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadShape => write!(f, "bad width / height / layer count"),
            Self::UnsupportedFormat(p) => write!(f, "pixel format {p} is not supported"),
            Self::UnsupportedUsage(u) => write!(f, "usage bits {u:#x} are not supported"),
            Self::ReservedRegion => write!(f, "reserved regions are not supported"),
            Self::UnrecognisedOption(n) => write!(f, "unrecognised option {n}"),
            Self::UnsupportedModifiers(m) => write!(f, "no supported modifier in {m:#x?}"),
        }
    }
}

impl std::error::Error for Refusal {}

/// A validated request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checked {
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub usage: u64,
    pub format: &'static FormatInfo,
    /// The allowed modifiers, never empty (an empty request means `[LINEAR]`).
    pub modifiers: Vec<u64>,
}

impl Descriptor {
    /// The request's name as text: bytes up to the first NUL, at most [`NAME_MAX`], non-ASCII replaced (the name is a debugging label).
    pub fn name_text(&self) -> String {
        let end = self
            .name
            .iter()
            .position(|&b| b == 0)
            .unwrap_or(self.name.len())
            .min(NAME_MAX);
        String::from_utf8_lossy(&self.name[..end]).into_owned()
    }

    /// Checks the request against the supported set.
    pub fn check(&self) -> Result<Checked, Refusal> {
        let dim = |v: i32, max: u32| u32::try_from(v).ok().filter(|v| (1..=max).contains(v));
        // a BLOB is width bytes x 1 (PixelFormat.aidl BLOB)
        let (width_max, height_max) = if self.format == format::BLOB {
            (BLOB_MAX, 1)
        } else {
            (DIMENSION_MAX, DIMENSION_MAX)
        };
        let (Some(width), Some(height)) =
            (dim(self.width, width_max), dim(self.height, height_max))
        else {
            return Err(Refusal::BadShape);
        };
        if self.layer_count != 1 {
            return Err(Refusal::BadShape);
        }
        let format = format::lookup(self.format).ok_or(Refusal::UnsupportedFormat(self.format))?;
        if self.usage & !usage::SUPPORTED != 0 {
            return Err(Refusal::UnsupportedUsage(self.usage & !usage::SUPPORTED));
        }
        if self.reserved_size != 0 {
            return Err(Refusal::ReservedRegion);
        }
        if let Some(n) = self.option_names.first() {
            return Err(Refusal::UnrecognisedOption(n.clone()));
        }
        let modifiers = if self.modifiers.is_empty() {
            vec![format::DRM_FORMAT_MOD_LINEAR]
        } else {
            self.modifiers.clone()
        };
        if !modifiers.contains(&format::DRM_FORMAT_MOD_LINEAR) {
            return Err(Refusal::UnsupportedModifiers(modifiers));
        }
        Ok(Checked {
            name: self.name_text(),
            width,
            height,
            usage: self.usage,
            format,
            modifiers,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok() -> Descriptor {
        Descriptor {
            name: b"win\0garbage".to_vec(),
            width: 64,
            height: 32,
            layer_count: 1,
            format: format::RGBA_8888,
            usage: 0x33 | usage::GPU_TEXTURE,
            reserved_size: 0,
            option_names: vec![],
            modifiers: vec![],
        }
    }

    /// Empty = LINEAR; a set naming LINEAR is kept; a set without it is refused (the backend allocates LINEAR only).
    #[test]
    fn modifiers_default_to_linear() {
        const X_TILED: u64 = (1 << 56) | 1; // I915_FORMAT_MOD_X_TILED (drm_fourcc.h: vendor INTEL = 1, value 1)
        let c = ok().check().expect("accepted");
        assert_eq!(c.modifiers, [format::DRM_FORMAT_MOD_LINEAR]);
        let mut d = ok();
        d.modifiers = vec![X_TILED, format::DRM_FORMAT_MOD_LINEAR];
        assert_eq!(d.check().expect("accepted").modifiers, d.modifiers);
        d.modifiers = vec![X_TILED];
        assert_eq!(d.check(), Err(Refusal::UnsupportedModifiers(vec![X_TILED])));
    }

    #[test]
    fn accepts_the_window_buffer_shape() {
        let c = ok().check().expect("accepted");
        assert_eq!((c.width, c.height, c.name.as_str()), (64, 32, "win"));
    }

    #[test]
    fn each_refusal_fires() {
        let mut d = ok();
        d.width = 0;
        assert_eq!(d.check(), Err(Refusal::BadShape));
        let mut d = ok();
        d.height = i32::MAX;
        assert_eq!(d.check(), Err(Refusal::BadShape));
        let mut d = ok();
        d.layer_count = 2;
        assert_eq!(d.check(), Err(Refusal::BadShape));
        let mut d = ok();
        d.format = 4;
        assert_eq!(d.check(), Err(Refusal::UnsupportedFormat(4)));
        let mut d = ok();
        d.usage |= 1 << 14;
        assert_eq!(d.check(), Err(Refusal::UnsupportedUsage(1 << 14)));
        let mut d = ok();
        d.reserved_size = 4096;
        assert_eq!(d.check(), Err(Refusal::ReservedRegion));
        let mut d = ok();
        d.option_names = vec!["x".into()];
        assert_eq!(d.check(), Err(Refusal::UnrecognisedOption("x".into())));
    }

    #[test]
    fn name_is_cut_at_nul_and_limit() {
        let mut d = ok();
        d.name = vec![b'a'; 200];
        assert_eq!(d.name_text().len(), NAME_MAX);
        d.name = Vec::new();
        assert_eq!(d.name_text(), "");
    }
}
