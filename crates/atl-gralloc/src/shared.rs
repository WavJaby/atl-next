// SPDX-License-Identifier: MIT
//! The shared metadata page: one memfd page every process holding the buffer maps, so a `setMetadata` in one process is seen by the others ("Buffer metadata must be global", IMapper.h; the recommended mechanism is "an extra page of shared memory ... stored in the buffer's native_handle_t").
//! Holds the buffer name (written once by the allocator) and the fields the framework sets: dataspace, blend mode, SMPTE 2086 and CTA 861.3.
//! All access is by offset into a byte slice the mapper obtained from the mapping, so this module has no memory-model of its own: a field is a naturally aligned 4-byte store, the mapper's only synchronisation is that (IMapper.h: no explicit metadata synchronisation).
//! The page comes from another process: every read validates.

use crate::descriptor::NAME_MAX;

/// Bytes of the page: the unit `mmap` maps.
pub const PAGE_SIZE: usize = 4096;

/// 'ATLP'.
const MAGIC: u32 = u32::from_le_bytes(*b"ATLP");
const VERSION: u32 = 1;

const O_MAGIC: usize = 0;
const O_VERSION: usize = 4;
const O_NAME_LEN: usize = 8;
const O_NAME: usize = 12;
pub const O_DATASPACE: usize = O_NAME + NAME_MAX + 1; // 140
pub const O_BLEND: usize = O_DATASPACE + 4;
pub const O_SMPTE2086_PRESENT: usize = O_BLEND + 4;
pub const O_SMPTE2086: usize = O_SMPTE2086_PRESENT + 4;
pub const SMPTE2086_FLOATS: usize = 10;
pub const O_CTA_PRESENT: usize = O_SMPTE2086 + 4 * SMPTE2086_FLOATS;
pub const O_CTA: usize = O_CTA_PRESENT + 4;
pub const CTA_FLOATS: usize = 2;
/// Bytes of the page that carry data.
pub const USED: usize = O_CTA + 4 * CTA_FLOATS;

/// `android.hardware.graphics.common.Smpte2086`: three primaries and the white point (x, y each), then max and min luminance.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Smpte2086 {
    pub primary_red: [f32; 2],
    pub primary_green: [f32; 2],
    pub primary_blue: [f32; 2],
    pub white_point: [f32; 2],
    pub max_luminance: f32,
    pub min_luminance: f32,
}

/// `android.hardware.graphics.common.Cta861_3`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Cta861_3 {
    pub max_content_light_level: f32,
    pub max_frame_average_light_level: f32,
}

/// The page is unusable: too small, or not one we wrote.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BadPage;

impl std::fmt::Display for BadPage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "not an atl gralloc metadata page")
    }
}

impl std::error::Error for BadPage {}

fn u32_at(page: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(page[o..o + 4].try_into().expect("4 bytes"))
}

fn put_u32(page: &mut [u8], o: usize, v: u32) {
    page[o..o + 4].copy_from_slice(&v.to_le_bytes());
}

fn f32_at(page: &[u8], o: usize) -> f32 {
    f32::from_bits(u32_at(page, o))
}

/// Writes a fresh page: the name (cut at [`NAME_MAX`]), dataspace UNKNOWN (0), blend mode INVALID (0), no HDR metadata. `page` must be [`PAGE_SIZE`] long and zero-filled by the caller (a new memfd is).
pub fn init(page: &mut [u8], name: &str) -> Result<(), BadPage> {
    if page.len() < USED {
        return Err(BadPage);
    }
    let name = &name.as_bytes()[..name.len().min(NAME_MAX)];
    put_u32(page, O_MAGIC, MAGIC);
    put_u32(page, O_VERSION, VERSION);
    put_u32(page, O_NAME_LEN, name.len() as u32);
    page[O_NAME..O_NAME + name.len()].copy_from_slice(name);
    Ok(())
}

/// Whether `page` is one of ours (length, magic, version, name length).
pub fn validate(page: &[u8]) -> Result<(), BadPage> {
    if page.len() < USED
        || u32_at(page, O_MAGIC) != MAGIC
        || u32_at(page, O_VERSION) != VERSION
        || u32_at(page, O_NAME_LEN) as usize > NAME_MAX
    {
        return Err(BadPage);
    }
    Ok(())
}

/// The name bytes (validated page).
pub fn name(page: &[u8]) -> &[u8] {
    let n = (u32_at(page, O_NAME_LEN) as usize).min(NAME_MAX);
    &page[O_NAME..O_NAME + n]
}

/// `Dataspace` value (0 = UNKNOWN until set).
pub fn dataspace(page: &[u8]) -> i32 {
    u32_at(page, O_DATASPACE) as i32
}

pub fn set_dataspace(page: &mut [u8], v: i32) {
    put_u32(page, O_DATASPACE, v as u32);
}

/// `BlendMode` value (0 = INVALID until set).
pub fn blend_mode(page: &[u8]) -> i32 {
    u32_at(page, O_BLEND) as i32
}

pub fn set_blend_mode(page: &mut [u8], v: i32) {
    put_u32(page, O_BLEND, v as u32);
}

pub fn smpte2086(page: &[u8]) -> Option<Smpte2086> {
    if u32_at(page, O_SMPTE2086_PRESENT) == 0 {
        return None;
    }
    let f = |i: usize| f32_at(page, O_SMPTE2086 + 4 * i);
    Some(Smpte2086 {
        primary_red: [f(0), f(1)],
        primary_green: [f(2), f(3)],
        primary_blue: [f(4), f(5)],
        white_point: [f(6), f(7)],
        max_luminance: f(8),
        min_luminance: f(9),
    })
}

pub fn set_smpte2086(page: &mut [u8], v: Option<Smpte2086>) {
    let Some(v) = v else {
        put_u32(page, O_SMPTE2086_PRESENT, 0);
        return;
    };
    let all = [
        v.primary_red[0],
        v.primary_red[1],
        v.primary_green[0],
        v.primary_green[1],
        v.primary_blue[0],
        v.primary_blue[1],
        v.white_point[0],
        v.white_point[1],
        v.max_luminance,
        v.min_luminance,
    ];
    for (i, f) in all.iter().enumerate() {
        put_u32(page, O_SMPTE2086 + 4 * i, f.to_bits());
    }
    // the flag last: a reader that sees it set sees the floats
    put_u32(page, O_SMPTE2086_PRESENT, 1);
}

pub fn cta861_3(page: &[u8]) -> Option<Cta861_3> {
    if u32_at(page, O_CTA_PRESENT) == 0 {
        return None;
    }
    Some(Cta861_3 {
        max_content_light_level: f32_at(page, O_CTA),
        max_frame_average_light_level: f32_at(page, O_CTA + 4),
    })
}

pub fn set_cta861_3(page: &mut [u8], v: Option<Cta861_3>) {
    let Some(v) = v else {
        put_u32(page, O_CTA_PRESENT, 0);
        return;
    };
    put_u32(page, O_CTA, v.max_content_light_level.to_bits());
    put_u32(page, O_CTA + 4, v.max_frame_average_light_level.to_bits());
    put_u32(page, O_CTA_PRESENT, 1);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh(name: &str) -> Vec<u8> {
        let mut p = vec![0u8; PAGE_SIZE];
        init(&mut p, name).expect("init");
        p
    }

    #[test]
    fn init_then_defaults() {
        let p = fresh("com.example/Main");
        assert_eq!(validate(&p), Ok(()));
        assert_eq!(name(&p), b"com.example/Main");
        assert_eq!((dataspace(&p), blend_mode(&p)), (0, 0));
        assert_eq!((smpte2086(&p), cta861_3(&p)), (None, None));
    }

    #[test]
    fn fields_round_trip_and_clear() {
        let mut p = fresh("");
        set_dataspace(&mut p, 142_671_872); // Dataspace.SRGB (Dataspace.aidl)
        set_blend_mode(&mut p, 2);
        let s = Smpte2086 {
            primary_red: [0.68, 0.32],
            primary_green: [0.265, 0.69],
            primary_blue: [0.15, 0.06],
            white_point: [0.3127, 0.329],
            max_luminance: 1000.0,
            min_luminance: 0.001,
        };
        let c = Cta861_3 {
            max_content_light_level: 800.0,
            max_frame_average_light_level: 400.0,
        };
        set_smpte2086(&mut p, Some(s));
        set_cta861_3(&mut p, Some(c));
        assert_eq!((dataspace(&p), blend_mode(&p)), (142_671_872, 2));
        assert_eq!((smpte2086(&p), cta861_3(&p)), (Some(s), Some(c)));
        set_smpte2086(&mut p, None);
        set_cta861_3(&mut p, None);
        assert_eq!((smpte2086(&p), cta861_3(&p)), (None, None));
    }

    #[test]
    fn long_name_is_cut_and_foreign_pages_fail() {
        let p = fresh(&"n".repeat(300));
        assert_eq!(name(&p).len(), NAME_MAX);
        assert_eq!(validate(&vec![0u8; PAGE_SIZE]), Err(BadPage));
        assert_eq!(validate(&p[..USED - 1]), Err(BadPage));
        let mut q = p.clone();
        put_u32(&mut q, O_NAME_LEN, 1000);
        assert_eq!(validate(&q), Err(BadPage));
        assert_eq!(init(&mut [0u8; 8], "x"), Err(BadPage));
    }

    /// The offsets are the layout the other process reads: pinned so a change is deliberate (the version is bumped with it).
    #[test]
    fn layout_numbers() {
        assert_eq!(
            (O_DATASPACE, O_BLEND, O_SMPTE2086, O_CTA, USED),
            (140, 144, 152, 196, 204)
        );
        const { assert!(USED <= PAGE_SIZE) };
    }
}
