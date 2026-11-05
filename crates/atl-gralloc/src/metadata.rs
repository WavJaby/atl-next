// SPDX-License-Identifier: MIT
//! IMapper v5 standard metadata: which `StandardMetadataType` values the mapper answers and sets, and the byte encoding of each.
//! The encoding is AOSP's (hardware/interfaces/graphics/mapper/stable-c/implutils/include/android/hardware/graphics/mapper/utils/IMapperMetadataTypes.h, `MetadataWriter` / `MetadataReader` / `MetadataValue`): every value starts with a header, the string "android.hardware.graphics.common.StandardMetadataType" (an i64 length, then the bytes) and the type as i64;
//! then the payload in host byte order: integers at their width, floats as f32, strings as i64 length + bytes, `ExtendableType` as string + i64, vectors as an i64 count + the elements.
//! libui decodes with that header's C++, so the gate's ground truth is AOSP's own reader; the tests here restate the byte layout from the header independently of the encoder.

use crate::handle::Params;
use crate::shared::{self, Cta861_3, Smpte2086};

/// The header name of every standard type (IMapperMetadataTypes.h `DEFINE_TYPE` -> `Header::name`; AIMapper_MetadataType.name).
pub const STANDARD_METADATA_NAME: &str = "android.hardware.graphics.common.StandardMetadataType";

/// `StandardMetadataType` values (StandardMetadataType.aidl; the header static-asserts value == index).
pub mod ty {
    pub const INVALID: i64 = 0;
    pub const BUFFER_ID: i64 = 1;
    pub const NAME: i64 = 2;
    pub const WIDTH: i64 = 3;
    pub const HEIGHT: i64 = 4;
    pub const LAYER_COUNT: i64 = 5;
    pub const PIXEL_FORMAT_REQUESTED: i64 = 6;
    pub const PIXEL_FORMAT_FOURCC: i64 = 7;
    pub const PIXEL_FORMAT_MODIFIER: i64 = 8;
    pub const USAGE: i64 = 9;
    pub const ALLOCATION_SIZE: i64 = 10;
    pub const PROTECTED_CONTENT: i64 = 11;
    pub const COMPRESSION: i64 = 12;
    pub const INTERLACED: i64 = 13;
    pub const CHROMA_SITING: i64 = 14;
    pub const PLANE_LAYOUTS: i64 = 15;
    pub const CROP: i64 = 16;
    pub const DATASPACE: i64 = 17;
    pub const BLEND_MODE: i64 = 18;
    pub const SMPTE2086: i64 = 19;
    pub const CTA861_3: i64 = 20;
    pub const SMPTE2094_40: i64 = 21;
    pub const SMPTE2094_10: i64 = 22;
    pub const STRIDE: i64 = 23;
}

/// Gettable and settable standard types, the mapper's `listSupportedMetadataTypes`: every type but INVALID is gettable; the settable ones are the four the framework sets ("DATASPACE, SMPTE2086, CTA861_3, and BLEND_MODE", IMapper.h `setMetadata`).
/// SMPTE2094_10 / _40 get as absent and are not settable ("required to support setting those if it is possible to get them").
pub const SUPPORTED: &[(i64, bool)] = &[
    (ty::BUFFER_ID, false),
    (ty::NAME, false),
    (ty::WIDTH, false),
    (ty::HEIGHT, false),
    (ty::LAYER_COUNT, false),
    (ty::PIXEL_FORMAT_REQUESTED, false),
    (ty::PIXEL_FORMAT_FOURCC, false),
    (ty::PIXEL_FORMAT_MODIFIER, false),
    (ty::USAGE, false),
    (ty::ALLOCATION_SIZE, false),
    (ty::PROTECTED_CONTENT, false),
    (ty::COMPRESSION, false),
    (ty::INTERLACED, false),
    (ty::CHROMA_SITING, false),
    (ty::PLANE_LAYOUTS, false),
    (ty::CROP, false),
    (ty::DATASPACE, true),
    (ty::BLEND_MODE, true),
    (ty::SMPTE2086, true),
    (ty::CTA861_3, true),
    (ty::SMPTE2094_40, false),
    (ty::SMPTE2094_10, false),
    (ty::STRIDE, false),
];

/// `ExtendableType` names (the AIDL enum's package-qualified name, as libgralloctypes' `Compression_NONE` & co. spell them).
const COMPRESSION_NAME: &str = "android.hardware.graphics.common.Compression";
const INTERLACED_NAME: &str = "android.hardware.graphics.common.Interlaced";
const CHROMA_SITING_NAME: &str = "android.hardware.graphics.common.ChromaSiting";
const PLANE_LAYOUT_COMPONENT_TYPE_NAME: &str =
    "android.hardware.graphics.common.PlaneLayoutComponentType";

/// A buffer as the mapper sees it: the handle's parameters and the shared page.
pub struct View<'a> {
    pub params: &'a Params,
    pub page: &'a [u8],
}

/// The type is unknown or has no value to give.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Unsupported;

/// Why a set was refused, as the two `AIMapper_Error`s the interface allows for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetError {
    /// Fixed at allocation (BUFFER_ID, NAME, WIDTH, HEIGHT, LAYER_COUNT, PIXEL_FORMAT_REQUESTED, USAGE), or the bytes are not a valid value (IMapper.h: `BAD_VALUE` "when the field is constant", and `applyStandardMetadata` maps a failed decode to it).
    BadValue,
    /// Not settable, or unknown.
    Unsupported,
}

/// Little-endian appender of the header's `MetadataWriter` (this crate's targets are all little-endian; the writer copies host order).
struct Enc(Vec<u8>);

impl Enc {
    fn new(ty: i64) -> Self {
        let mut e = Self(Vec::new());
        e.str(STANDARD_METADATA_NAME);
        e.i64(ty);
        e
    }
    fn i64(&mut self, v: i64) -> &mut Self {
        self.0.extend_from_slice(&v.to_ne_bytes());
        self
    }
    fn u64(&mut self, v: u64) -> &mut Self {
        self.0.extend_from_slice(&v.to_ne_bytes());
        self
    }
    fn i32(&mut self, v: i32) -> &mut Self {
        self.0.extend_from_slice(&v.to_ne_bytes());
        self
    }
    fn u32(&mut self, v: u32) -> &mut Self {
        self.0.extend_from_slice(&v.to_ne_bytes());
        self
    }
    fn f32(&mut self, v: f32) -> &mut Self {
        self.0.extend_from_slice(&v.to_ne_bytes());
        self
    }
    fn str(&mut self, s: &str) -> &mut Self {
        self.bytes(s.as_bytes())
    }
    fn bytes(&mut self, b: &[u8]) -> &mut Self {
        self.i64(b.len() as i64);
        self.0.extend_from_slice(b);
        self
    }
    fn extendable(&mut self, name: &str, value: i64) -> &mut Self {
        self.str(name).i64(value)
    }
}

/// The header's `MetadataReader`: bounds-checked, `ok()` goes false at the first short read and stays false.
struct Dec<'a> {
    src: &'a [u8],
    ok: bool,
}

impl<'a> Dec<'a> {
    fn new(src: &'a [u8]) -> Self {
        Self { src, ok: true }
    }
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        if self.ok && self.src.len() >= n {
            let (a, b) = self.src.split_at(n);
            self.src = b;
            Some(a)
        } else {
            self.ok = false;
            None
        }
    }
    fn i64(&mut self) -> Option<i64> {
        self.take(8)
            .map(|b| i64::from_ne_bytes(b.try_into().expect("8 bytes")))
    }
    fn i32(&mut self) -> Option<i32> {
        self.take(4)
            .map(|b| i32::from_ne_bytes(b.try_into().expect("4 bytes")))
    }
    fn f32(&mut self) -> Option<f32> {
        self.take(4)
            .map(|b| f32::from_ne_bytes(b.try_into().expect("4 bytes")))
    }
    fn string(&mut self) -> Option<&'a [u8]> {
        let n = usize::try_from(self.i64()?).ok()?;
        self.take(n)
    }
    /// `checkHeader<Header>()`: name and type must match.
    fn header(&mut self, ty: i64) {
        if self.string() != Some(STANDARD_METADATA_NAME.as_bytes()) || self.i64() != Some(ty) {
            self.ok = false;
        }
    }
}

/// The encoded value of standard type `t` of `view` (empty for an absent optional value, as the header's `encode` returns 0 bytes for `nullopt`).
pub fn get_standard(view: &View<'_>, t: i64) -> Result<Vec<u8>, Unsupported> {
    let p = view.params;
    let mut e = Enc::new(t);
    match t {
        ty::BUFFER_ID => {
            e.u64(p.id);
        }
        ty::NAME => {
            e.bytes(shared::name(view.page));
        }
        ty::WIDTH => {
            e.u64(u64::from(p.width));
        }
        ty::HEIGHT => {
            e.u64(u64::from(p.height));
        }
        // one layer is the only shape allocated (Descriptor::check)
        ty::LAYER_COUNT => {
            e.u64(1);
        }
        ty::PIXEL_FORMAT_REQUESTED => {
            e.i32(p.pixel_format);
        }
        ty::PIXEL_FORMAT_FOURCC => {
            e.u32(p.layout().fourcc);
        }
        ty::PIXEL_FORMAT_MODIFIER => {
            e.u64(p.layout().modifier);
        }
        ty::USAGE => {
            e.u64(p.usage);
        }
        ty::ALLOCATION_SIZE => {
            e.u64(p.size);
        }
        ty::PROTECTED_CONTENT => {
            e.u64(0);
        }
        ty::COMPRESSION => {
            e.extendable(COMPRESSION_NAME, 0);
        }
        ty::INTERLACED => {
            e.extendable(INTERLACED_NAME, 0);
        }
        ty::CHROMA_SITING => {
            e.extendable(CHROMA_SITING_NAME, 0);
        }
        ty::PLANE_LAYOUTS => {
            let f = p.format();
            let l = p.layout();
            let bpp = i64::from(l.bytes_per_pixel);
            e.i64(l.planes.len() as i64);
            for plane in &l.planes {
                e.i64(f.components.len() as i64);
                for &(kind, offset, size) in f.components {
                    e.extendable(PLANE_LAYOUT_COMPONENT_TYPE_NAME, kind)
                        .i64(offset)
                        .i64(size);
                }
                let stride = i64::from(plane.stride_bytes);
                e.i64(plane.offset as i64) // offsetInBytes
                    .i64(bpp * 8) // sampleIncrementInBits
                    .i64(stride) // strideInBytes
                    .i64(i64::from(l.width)) // widthInSamples
                    .i64(i64::from(l.height)) // heightInSamples
                    .i64(stride * i64::from(l.height)) // totalSizeInBytes
                    .i64(1) // horizontalSubsampling
                    .i64(1); // verticalSubsampling
            }
        }
        // one rect per plane: the whole buffer
        ty::CROP => {
            e.i64(1)
                .i32(0)
                .i32(0)
                .i32(p.width as i32)
                .i32(p.height as i32);
        }
        ty::DATASPACE => {
            e.i32(shared::dataspace(view.page));
        }
        ty::BLEND_MODE => {
            e.i32(shared::blend_mode(view.page));
        }
        ty::SMPTE2086 => match shared::smpte2086(view.page) {
            None => return Ok(Vec::new()),
            Some(s) => {
                for v in [
                    s.primary_red[0],
                    s.primary_red[1],
                    s.primary_green[0],
                    s.primary_green[1],
                    s.primary_blue[0],
                    s.primary_blue[1],
                    s.white_point[0],
                    s.white_point[1],
                    s.max_luminance,
                    s.min_luminance,
                ] {
                    e.f32(v);
                }
            }
        },
        ty::CTA861_3 => match shared::cta861_3(view.page) {
            None => return Ok(Vec::new()),
            Some(c) => {
                e.f32(c.max_content_light_level)
                    .f32(c.max_frame_average_light_level);
            }
        },
        // never present: nothing sets them
        ty::SMPTE2094_40 | ty::SMPTE2094_10 => return Ok(Vec::new()),
        ty::STRIDE => {
            e.u32(p.pixel_stride());
        }
        _ => return Err(Unsupported),
    }
    Ok(e.0)
}

/// The byte ranges (offset, length) of the page that a set of `t` changes, in the order to write them: an optional value's floats before its presence flag, so a reader that sees the flag sees the floats. Empty for a type that is not settable.
pub fn page_spans(t: i64) -> &'static [(usize, usize)] {
    match t {
        ty::DATASPACE => &[(shared::O_DATASPACE, 4)],
        ty::BLEND_MODE => &[(shared::O_BLEND, 4)],
        ty::SMPTE2086 => &[
            (shared::O_SMPTE2086, 4 * shared::SMPTE2086_FLOATS),
            (shared::O_SMPTE2086_PRESENT, 4),
        ],
        ty::CTA861_3 => &[
            (shared::O_CTA, 4 * shared::CTA_FLOATS),
            (shared::O_CTA_PRESENT, 4),
        ],
        _ => &[],
    }
}

/// Sets standard type `t` from its encoded `data` into `page`.
pub fn set_standard(page: &mut [u8], t: i64, data: &[u8]) -> Result<(), SetError> {
    match t {
        ty::BUFFER_ID
        | ty::NAME
        | ty::WIDTH
        | ty::HEIGHT
        | ty::LAYER_COUNT
        | ty::PIXEL_FORMAT_REQUESTED
        | ty::USAGE => Err(SetError::BadValue),
        ty::DATASPACE => {
            let mut d = Dec::new(data);
            d.header(t);
            let v = d.i32();
            if !d.ok {
                return Err(SetError::BadValue);
            }
            shared::set_dataspace(page, v.expect("ok"));
            Ok(())
        }
        ty::BLEND_MODE => {
            let mut d = Dec::new(data);
            d.header(t);
            let v = d.i32();
            // BlendMode.aidl: INVALID 0, NONE 1, PREMULTIPLIED 2, COVERAGE 3
            match v {
                Some(v @ 0..=3) if d.ok => {
                    shared::set_blend_mode(page, v);
                    Ok(())
                }
                _ => Err(SetError::BadValue),
            }
        }
        ty::SMPTE2086 => {
            if data.is_empty() {
                shared::set_smpte2086(page, None);
                return Ok(());
            }
            let mut d = Dec::new(data);
            d.header(t);
            let mut f = [0f32; 10];
            for v in &mut f {
                *v = d.f32().unwrap_or(0.0);
            }
            if !d.ok {
                return Err(SetError::BadValue);
            }
            shared::set_smpte2086(
                page,
                Some(Smpte2086 {
                    primary_red: [f[0], f[1]],
                    primary_green: [f[2], f[3]],
                    primary_blue: [f[4], f[5]],
                    white_point: [f[6], f[7]],
                    max_luminance: f[8],
                    min_luminance: f[9],
                }),
            );
            Ok(())
        }
        ty::CTA861_3 => {
            if data.is_empty() {
                shared::set_cta861_3(page, None);
                return Ok(());
            }
            let mut d = Dec::new(data);
            d.header(t);
            let (a, b) = (d.f32(), d.f32());
            if !d.ok {
                return Err(SetError::BadValue);
            }
            shared::set_cta861_3(
                page,
                Some(Cta861_3 {
                    max_content_light_level: a.expect("ok"),
                    max_frame_average_light_level: b.expect("ok"),
                }),
            );
            Ok(())
        }
        _ => Err(SetError::Unsupported),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::{self, DRM_FORMAT_ABGR8888, RGBA_8888};

    fn params() -> Params {
        Params {
            width: 64,
            height: 48,
            pixel_format: RGBA_8888,
            fourcc: DRM_FORMAT_ABGR8888,
            pitch: 256,
            size: 256 * 48,
            usage: 0x33,
            modifier: 0,
            id: 0x0000_04d2_0000_0007,
        }
    }

    fn page() -> Vec<u8> {
        let mut p = vec![0u8; shared::PAGE_SIZE];
        shared::init(&mut p, "SurfaceView[x]").expect("init");
        p
    }

    /// The header the layout in IMapperMetadataTypes.h prescribes, restated: i64 length 53 ("android.hardware.graphics.common.StandardMetadataType" is 7+1+8+1+8+1+6+1+20 chars), the name, i64 type.
    fn header(ty: i64) -> Vec<u8> {
        let mut v = 53i64.to_ne_bytes().to_vec();
        v.extend_from_slice(b"android.hardware.graphics.common.StandardMetadataType");
        v.extend_from_slice(&ty.to_ne_bytes());
        v
    }

    fn with(ty: i64, payload: &[u8]) -> Vec<u8> {
        let mut v = header(ty);
        v.extend_from_slice(payload);
        v
    }

    fn get(ty: i64) -> Vec<u8> {
        let p = params();
        let pg = page();
        get_standard(
            &View {
                params: &p,
                page: &pg,
            },
            ty,
        )
        .expect("supported")
    }

    #[test]
    fn header_length_is_53() {
        assert_eq!(STANDARD_METADATA_NAME.len(), 53);
    }

    #[test]
    fn integers_at_their_width() {
        assert_eq!(
            get(ty::BUFFER_ID),
            with(1, &0x0000_04d2_0000_0007u64.to_ne_bytes())
        );
        assert_eq!(get(ty::WIDTH), with(3, &64u64.to_ne_bytes()));
        assert_eq!(get(ty::HEIGHT), with(4, &48u64.to_ne_bytes()));
        assert_eq!(get(ty::LAYER_COUNT), with(5, &1u64.to_ne_bytes()));
        // PixelFormat is an int-backed enum
        assert_eq!(
            get(ty::PIXEL_FORMAT_REQUESTED),
            with(6, &1i32.to_ne_bytes())
        );
        assert_eq!(
            get(ty::PIXEL_FORMAT_FOURCC),
            with(7, &0x3432_4241u32.to_ne_bytes())
        );
        assert_eq!(get(ty::PIXEL_FORMAT_MODIFIER), with(8, &0u64.to_ne_bytes()));
        // BufferUsage is a long-backed enum
        assert_eq!(get(ty::USAGE), with(9, &0x33u64.to_ne_bytes()));
        assert_eq!(get(ty::ALLOCATION_SIZE), with(10, &12288u64.to_ne_bytes()));
        assert_eq!(get(ty::PROTECTED_CONTENT), with(11, &0u64.to_ne_bytes()));
        assert_eq!(get(ty::STRIDE), with(23, &64u32.to_ne_bytes()));
        assert_eq!(get(ty::DATASPACE), with(17, &0i32.to_ne_bytes()));
        assert_eq!(get(ty::BLEND_MODE), with(18, &0i32.to_ne_bytes()));
    }

    #[test]
    fn strings_and_extendables() {
        let mut name = 14i64.to_ne_bytes().to_vec();
        name.extend_from_slice(b"SurfaceView[x]");
        assert_eq!(get(ty::NAME), with(2, &name));

        let ext = |name: &str| {
            let mut v = (name.len() as i64).to_ne_bytes().to_vec();
            v.extend_from_slice(name.as_bytes());
            v.extend_from_slice(&0i64.to_ne_bytes());
            v
        };
        assert_eq!(
            get(ty::COMPRESSION),
            with(12, &ext("android.hardware.graphics.common.Compression"))
        );
        assert_eq!(
            get(ty::INTERLACED),
            with(13, &ext("android.hardware.graphics.common.Interlaced"))
        );
        assert_eq!(
            get(ty::CHROMA_SITING),
            with(14, &ext("android.hardware.graphics.common.ChromaSiting"))
        );
    }

    #[test]
    fn plane_layout_and_crop() {
        let mut want = Vec::new();
        want.extend_from_slice(&1i64.to_ne_bytes()); // planes
        want.extend_from_slice(&4i64.to_ne_bytes()); // components
        // R, G, B, A: (type, offset bits, size bits)
        for (kind, off) in [
            (1i64 << 10, 0i64),
            (1 << 11, 8),
            (1 << 12, 16),
            (1 << 30, 24),
        ] {
            let n = "android.hardware.graphics.common.PlaneLayoutComponentType";
            want.extend_from_slice(&(n.len() as i64).to_ne_bytes());
            want.extend_from_slice(n.as_bytes());
            want.extend_from_slice(&kind.to_ne_bytes());
            want.extend_from_slice(&off.to_ne_bytes());
            want.extend_from_slice(&8i64.to_ne_bytes());
        }
        // offsetInBytes, sampleIncrementInBits, strideInBytes, widthInSamples, heightInSamples, totalSizeInBytes, hsub, vsub
        for v in [0i64, 32, 256, 64, 48, 256 * 48, 1, 1] {
            want.extend_from_slice(&v.to_ne_bytes());
        }
        assert_eq!(get(ty::PLANE_LAYOUTS), with(15, &want));

        let mut crop = 1i64.to_ne_bytes().to_vec();
        for v in [0i32, 0, 64, 48] {
            crop.extend_from_slice(&v.to_ne_bytes());
        }
        assert_eq!(get(ty::CROP), with(16, &crop));
    }

    #[test]
    fn absent_optionals_are_empty_and_unknown_is_unsupported() {
        for t in [
            ty::SMPTE2086,
            ty::CTA861_3,
            ty::SMPTE2094_10,
            ty::SMPTE2094_40,
        ] {
            assert!(get(t).is_empty(), "{t}");
        }
        let p = params();
        let pg = page();
        let v = View {
            params: &p,
            page: &pg,
        };
        for t in [ty::INVALID, 24, -1, i64::MAX] {
            assert_eq!(get_standard(&v, t), Err(Unsupported), "{t}");
        }
    }

    #[test]
    fn every_listed_type_is_gettable() {
        let p = params();
        let pg = page();
        let v = View {
            params: &p,
            page: &pg,
        };
        for &(t, _) in SUPPORTED {
            assert!(get_standard(&v, t).is_ok(), "{t}");
        }
        assert_eq!(SUPPORTED.len(), 23);
    }

    #[test]
    fn set_dataspace_blend_and_hdr_then_get() {
        let mut pg = page();
        let p = params();
        set_standard(
            &mut pg,
            ty::DATASPACE,
            &with(17, &142_671_872i32.to_ne_bytes()),
        )
        .expect("dataspace");
        set_standard(&mut pg, ty::BLEND_MODE, &with(18, &2i32.to_ne_bytes())).expect("blend");
        let mut hdr = Vec::new();
        for f in [
            0.68f32, 0.32, 0.265, 0.69, 0.15, 0.06, 0.3127, 0.329, 1000.0, 0.001,
        ] {
            hdr.extend_from_slice(&f.to_ne_bytes());
        }
        set_standard(&mut pg, ty::SMPTE2086, &with(19, &hdr)).expect("smpte2086");
        let mut cta = Vec::new();
        for f in [800.0f32, 400.0] {
            cta.extend_from_slice(&f.to_ne_bytes());
        }
        set_standard(&mut pg, ty::CTA861_3, &with(20, &cta)).expect("cta");
        let v = View {
            params: &p,
            page: &pg,
        };
        assert_eq!(
            get_standard(&v, ty::DATASPACE),
            Ok(with(17, &142_671_872i32.to_ne_bytes()))
        );
        assert_eq!(
            get_standard(&v, ty::BLEND_MODE),
            Ok(with(18, &2i32.to_ne_bytes()))
        );
        assert_eq!(get_standard(&v, ty::SMPTE2086), Ok(with(19, &hdr)));
        assert_eq!(get_standard(&v, ty::CTA861_3), Ok(with(20, &cta)));
        // an empty value clears an optional one
        set_standard(&mut pg, ty::SMPTE2086, &[]).expect("clear");
        let v = View {
            params: &p,
            page: &pg,
        };
        assert!(get_standard(&v, ty::SMPTE2086).expect("get").is_empty());
    }

    #[test]
    fn set_refusals() {
        let mut pg = page();
        let good = with(17, &0i32.to_ne_bytes());
        // constants
        for t in [
            ty::BUFFER_ID,
            ty::NAME,
            ty::WIDTH,
            ty::HEIGHT,
            ty::LAYER_COUNT,
            ty::PIXEL_FORMAT_REQUESTED,
            ty::USAGE,
        ] {
            assert_eq!(
                set_standard(&mut pg, t, &good),
                Err(SetError::BadValue),
                "{t}"
            );
        }
        // not settable / unknown
        for t in [ty::STRIDE, ty::CROP, ty::SMPTE2094_40, ty::INVALID, 99] {
            assert_eq!(
                set_standard(&mut pg, t, &good),
                Err(SetError::Unsupported),
                "{t}"
            );
        }
        // malformed: short, wrong header type, trailing garbage is tolerated (the reader stops at the value), bad blend mode
        assert_eq!(
            set_standard(&mut pg, ty::DATASPACE, &good[..good.len() - 1]),
            Err(SetError::BadValue)
        );
        assert_eq!(
            set_standard(&mut pg, ty::DATASPACE, &with(18, &0i32.to_ne_bytes())),
            Err(SetError::BadValue)
        );
        assert_eq!(
            set_standard(&mut pg, ty::BLEND_MODE, &with(18, &9i32.to_ne_bytes())),
            Err(SetError::BadValue)
        );
        assert_eq!(
            set_standard(&mut pg, ty::SMPTE2086, &with(19, &[0u8; 39])),
            Err(SetError::BadValue)
        );
        assert_eq!(
            set_standard(&mut pg, ty::CTA861_3, &with(20, &[0u8; 7])),
            Err(SetError::BadValue)
        );
        assert_eq!(shared::dataspace(&pg), 0, "a refused set changes nothing");
    }

    #[test]
    fn format_table_feeds_the_layout() {
        // the plane layout's component bits sum to the sample increment
        let f = format::lookup(RGBA_8888).expect("row");
        assert_eq!(f.components.iter().map(|c| c.2).sum::<i64>(), 32);
    }
}
