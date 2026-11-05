// SPDX-License-Identifier: MIT
//! The buffer handle: a `native_handle_t` of two fds (the dma-buf, the shared metadata page) and [`NUM_INTS`] ints.
//! The allocator builds it, the mapper parses it in the importing process; both sides call the same functions, so the layout has one definition.
//! Everything the mapper needs to answer the allocation-time metadata is in the ints; what any process may change (dataspace, ...) lives in the page ([`crate::shared`]).

use crate::format::{self, FormatInfo};

/// Index of the dma-buf in `native_handle_t::data`.
pub const FD_DMABUF: usize = 0;
/// Index of the shared metadata page (a memfd of [`crate::shared::PAGE_SIZE`] bytes).
pub const FD_PAGE: usize = 1;
/// `native_handle_t::numFds`.
pub const NUM_FDS: usize = 2;

/// 'ATLG'.
const MAGIC: i32 = i32::from_le_bytes(*b"ATLG");
/// Bumped when the int layout changes; an importer of another version refuses the handle.
const VERSION: i32 = 1;

// int indices
const I_MAGIC: usize = 0;
const I_VERSION: usize = 1;
const I_WIDTH: usize = 2;
const I_HEIGHT: usize = 3;
const I_PIXEL_FORMAT: usize = 4;
const I_FOURCC: usize = 5;
const I_PITCH: usize = 6;
const I_SIZE: usize = 7; // lo, hi
const I_USAGE: usize = 9; // lo, hi
const I_MODIFIER: usize = 11; // lo, hi
const I_ID: usize = 13; // lo, hi
/// `native_handle_t::numInts`.
pub const NUM_INTS: usize = 15;

/// What the allocator decided about one buffer, as the handle's ints carry it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Params {
    pub width: u32,
    pub height: u32,
    pub pixel_format: i32,
    pub fourcc: u32,
    /// Bytes from the start of one row to the next.
    pub pitch: u32,
    /// Bytes of the dma-buf.
    pub size: u64,
    /// `BufferUsage` bits the buffer was allocated for.
    pub usage: u64,
    /// DRM format modifier of the memory (`DRM_FORMAT_MOD_LINEAR` = 0 for the memory the CPU path maps).
    pub modifier: u64,
    /// Unique among live buffers of the system: allocator pid << 32 | its counter.
    pub id: u64,
}

/// Why a handle is not one of ours.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HandleError {
    /// Wrong fd or int count.
    Counts { fds: usize, ints: usize },
    /// Wrong magic or version.
    Foreign,
    /// Fields contradict each other (unsupported format, pitch under one row, size under the rows, ...).
    Inconsistent(&'static str),
}

impl std::fmt::Display for HandleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Counts { fds, ints } => write!(
                f,
                "handle has {fds} fds and {ints} ints, want {NUM_FDS} and {NUM_INTS}"
            ),
            Self::Foreign => write!(f, "handle is not an atl gralloc handle"),
            Self::Inconsistent(why) => write!(f, "handle fields inconsistent: {why}"),
        }
    }
}

impl std::error::Error for HandleError {}

fn split(v: u64) -> [i32; 2] {
    // two's complement reinterpretation of each half
    [(v as u32) as i32, ((v >> 32) as u32) as i32]
}

fn join(lo: i32, hi: i32) -> u64 {
    u64::from(lo as u32) | (u64::from(hi as u32) << 32)
}

impl Params {
    /// The format row of this buffer.
    pub fn format(&self) -> &'static FormatInfo {
        format::lookup(self.pixel_format)
            .expect("Params only come from a checked handle or allocation")
    }

    /// Pixels from the start of one row to the next (`StandardMetadataType::STRIDE`, `AllocationResult.stride`).
    pub fn pixel_stride(&self) -> u32 {
        self.layout().pixel_stride()
    }

    /// The handle's ints.
    pub fn to_ints(&self) -> [i32; NUM_INTS] {
        let mut v = [0i32; NUM_INTS];
        v[I_MAGIC] = MAGIC;
        v[I_VERSION] = VERSION;
        v[I_WIDTH] = self.width as i32;
        v[I_HEIGHT] = self.height as i32;
        v[I_PIXEL_FORMAT] = self.pixel_format;
        v[I_FOURCC] = self.fourcc as i32;
        v[I_PITCH] = self.pitch as i32;
        v[I_SIZE..I_SIZE + 2].copy_from_slice(&split(self.size));
        v[I_USAGE..I_USAGE + 2].copy_from_slice(&split(self.usage));
        v[I_MODIFIER..I_MODIFIER + 2].copy_from_slice(&split(self.modifier));
        v[I_ID..I_ID + 2].copy_from_slice(&split(self.id));
        v
    }

    /// Parses and validates the ints of a handle with `num_fds` fds. A handle crosses processes, so the importer trusts none of it: layout and consistency are checked here once.
    pub fn from_handle(num_fds: usize, ints: &[i32]) -> Result<Self, HandleError> {
        if num_fds != NUM_FDS || ints.len() != NUM_INTS {
            return Err(HandleError::Counts {
                fds: num_fds,
                ints: ints.len(),
            });
        }
        if ints[I_MAGIC] != MAGIC || ints[I_VERSION] != VERSION {
            return Err(HandleError::Foreign);
        }
        let p = Self {
            width: ints[I_WIDTH] as u32,
            height: ints[I_HEIGHT] as u32,
            pixel_format: ints[I_PIXEL_FORMAT],
            fourcc: ints[I_FOURCC] as u32,
            pitch: ints[I_PITCH] as u32,
            size: join(ints[I_SIZE], ints[I_SIZE + 1]),
            usage: join(ints[I_USAGE], ints[I_USAGE + 1]),
            modifier: join(ints[I_MODIFIER], ints[I_MODIFIER + 1]),
            id: join(ints[I_ID], ints[I_ID + 1]),
        };
        let f = format::lookup(p.pixel_format).ok_or(HandleError::Inconsistent("pixel format"))?;
        if p.fourcc != f.fourcc {
            return Err(HandleError::Inconsistent("fourcc"));
        }
        if p.width == 0 || p.height == 0 {
            return Err(HandleError::Inconsistent("empty"));
        }
        let row = u64::from(p.width) * u64::from(f.bytes_per_pixel);
        if u64::from(p.pitch) < row || !p.pitch.is_multiple_of(f.bytes_per_pixel) {
            return Err(HandleError::Inconsistent("pitch"));
        }
        if p.size < u64::from(p.pitch) * u64::from(p.height) {
            return Err(HandleError::Inconsistent("size"));
        }
        Ok(p)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Params {
        Params {
            width: 64,
            height: 64,
            pixel_format: format::RGBA_8888,
            fourcc: format::DRM_FORMAT_ABGR8888,
            pitch: 256,
            size: 16384,
            usage: 0x1_0000_0033,
            modifier: 0,
            id: (1234u64 << 32) | 7,
        }
    }

    #[test]
    fn round_trip_including_64_bit_halves() {
        let p = sample();
        let ints = p.to_ints();
        assert_eq!(Params::from_handle(NUM_FDS, &ints), Ok(p));
        // usage 0x1_0000_0033: lo 0x33, hi 1; id lo 7, hi 1234
        assert_eq!((ints[I_USAGE], ints[I_USAGE + 1]), (0x33, 1));
        assert_eq!((ints[I_ID], ints[I_ID + 1]), (7, 1234));
    }

    #[test]
    fn pixel_stride_is_pitch_over_bytes_per_pixel() {
        assert_eq!(sample().pixel_stride(), 64);
        let mut p = sample();
        p.pitch = 320;
        assert_eq!(p.pixel_stride(), 80);
    }

    #[test]
    fn each_rejection_fires() {
        let good = sample().to_ints();
        assert_eq!(
            Params::from_handle(1, &good),
            Err(HandleError::Counts {
                fds: 1,
                ints: NUM_INTS
            })
        );
        assert_eq!(
            Params::from_handle(NUM_FDS, &good[..NUM_INTS - 1]),
            Err(HandleError::Counts {
                fds: NUM_FDS,
                ints: NUM_INTS - 1
            })
        );
        let mut v = good;
        v[I_MAGIC] = 0;
        assert_eq!(Params::from_handle(NUM_FDS, &v), Err(HandleError::Foreign));
        let mut v = good;
        v[I_VERSION] = 2;
        assert_eq!(Params::from_handle(NUM_FDS, &v), Err(HandleError::Foreign));
        for (idx, val, why) in [
            (I_PIXEL_FORMAT, 4, "pixel format"),
            (I_FOURCC, 0, "fourcc"),
            (I_WIDTH, 0, "empty"),
            (I_PITCH, 255, "pitch"), // under one row (64 px * 4) and not a pixel multiple
            (I_SIZE, 16383, "size"),
        ] {
            let mut v = good;
            v[idx] = val;
            assert_eq!(
                Params::from_handle(NUM_FDS, &v),
                Err(HandleError::Inconsistent(why)),
                "{why}"
            );
        }
    }
}
