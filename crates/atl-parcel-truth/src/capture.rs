// SPDX-License-Identifier: MIT
//! Golden of ScreenCaptureResults' wire form, both directions: the codec in gui_aidl_types_rs (patch 0007) against libgui's own writeToParcel / readFromParcel,
//! reached through wrap/capture.{h,cpp} (adapters, no logic). Test-only.
//! Expected words come from the wire order (ScreenCaptureResults.cpp:25-52) and the header defaults (ScreenCaptureResults.h:29-43), never from the codec.
//! Boundary (cxx, raw AParcel pointers): `unsafe` only at the adapter calls; each call borrows its parcels for the call and keeps no pointer.

#[cxx::bridge(namespace = "atl::capture")]
mod ffi {
    unsafe extern "C++" {
        include!("wrap/capture.h");

        /// libbinder_ndk's AParcel.
        type AParcel;

        /// ScreenCaptureResults::writeToParcel of a result with these fields, the rest at the header's defaults, into the parcel's data at its position; status_t.
        /// fence_fd >= 0 is a valid Fence that takes ownership of the fd; otherwise fence_status is the fence status (NO_ERROR: no fence).
        unsafe fn write_result(
            parcel: *mut AParcel,
            fence_fd: i32,
            fence_status: i32,
            secure: bool,
            hdr: bool,
            dataspace: u32,
            hdr_sdr_ratio: f32,
        ) -> i32;

        /// ScreenCaptureResults::readFromParcel of `input` at its position, then writeToParcel of the result into `out`; the first failing status_t.
        unsafe fn reserialize(input: *const AParcel, out: *mut AParcel) -> i32;
    }
}

#[cfg(test)]
mod tests {
    use super::ffi;
    use crate::marshal;
    use binder::binder_impl::{Parcel, UnstructuredParcelable};
    use binder::unstable_api::AsNative;
    use gui_aidl_types_rs::{Flattened, ScreenCaptureResults};
    use std::fs::File;
    use std::os::fd::{IntoRawFd, OwnedFd};

    /// utils/Errors.h: NO_ERROR, and BAD_VALUE = -EINVAL.
    const NO_ERROR: i32 = 0;
    const BAD_VALUE: i32 = -22;
    /// Dataspace.aidl: V0_SRGB = STANDARD_BT709 | TRANSFER_SRGB | RANGE_FULL, the header default (ScreenCaptureResults.h:38).
    const V0_SRGB: u32 = (1 << 16) | (2 << 22) | (1 << 27);
    /// Dataspace.aidl: SRGB_LINEAR = STANDARD_BT709 | TRANSFER_LINEAR | RANGE_FULL.
    const SRGB_LINEAR: u32 = (1 << 16) | (1 << 22) | (1 << 27);
    /// IEEE-754 bits of 1.0 (the header's hdrSdrRatio default, ScreenCaptureResults.h:43) and of 1.5.
    const ONE: u32 = 0x3F80_0000;
    const ONE_AND_A_HALF: u32 = 0x3FC0_0000;
    /// GraphicBuffer::flatten's magic, 'GB01' (GraphicBuffer.cpp:472).
    const GB01: u32 = 0x4742_3031;
    /// BINDER_TYPE_FD = B_PACK_CHARS('f', 'd', '*', 0x85) (linux/android/binder.h).
    const BINDER_TYPE_FD: u32 = 0x6664_2A85;
    /// A valid fence's fd object follows the five words before it: hasBuffer, hasFence, the Fence's size, its fd count, its numFds word.
    const FENCE_OBJECT: usize = 5 * 4;
    /// flat_binder_object on LP64: type, flags, handle (the fd), cookie.
    const FLAT_BINDER_OBJECT: usize = 24;
    /// GraphicBuffer::flatten with no handle (GraphicBuffer.cpp:471-483): 'GB01', width, height, stride, format, layerCount, usage (low), mId (high, low),
    /// generation, numFds, numInts, usage (high). The values are a test vector; the layout is the one libgui writes and reads.
    const GRAPHIC_BUFFER: [u32; 13] = [GB01, 1920, 1080, 2048, 1, 1, 0x3, 0, 0x2A, 1, 0, 0, 0];
    /// The flattened size in bytes of GRAPHIC_BUFFER (its handle has no ints, so the size is the 13 words).
    const FLAT_BYTES: u32 = 4 * GRAPHIC_BUFFER.len() as u32;

    /// The fence a case carries: none, a valid fence (a fresh fd), or the status a failed capture carries.
    #[derive(Clone, Copy, Debug, PartialEq)]
    enum Fence {
        None,
        Fd,
        Status(i32),
    }

    /// The fields a case varies; the rest of ScreenCaptureResults stays at the header's defaults.
    #[derive(Clone, Copy, Debug, PartialEq)]
    struct Case {
        fence: Fence,
        secure: bool,
        hdr: bool,
        dataspace: u32,
        hdr_sdr_ratio: f32,
    }

    const DEFAULT: Case = Case {
        fence: Fence::None,
        secure: false,
        hdr: false,
        dataspace: V0_SRGB,
        hdr_sdr_ratio: 1.0,
    };

    /// Little-endian int32 words, the layout of the parcels here.
    fn le(words: &[u32]) -> Vec<u8> {
        words.iter().flat_map(|w| w.to_le_bytes()).collect()
    }

    fn fence_fd() -> OwnedFd {
        OwnedFd::from(File::open("/dev/null").expect("open /dev/null"))
    }

    fn bytes(p: &Parcel) -> Vec<u8> {
        marshal(p).expect("marshal")
    }

    /// libgui's writer: ScreenCaptureResults::writeToParcel of `c` into a fresh parcel.
    fn libgui_write(c: Case) -> Parcel {
        let mut p = Parcel::new();
        let (fd, status) = match c.fence {
            Fence::None => (-1, NO_ERROR),
            Fence::Fd => (fence_fd().into_raw_fd(), NO_ERROR),
            Fence::Status(s) => (-1, s),
        };
        // SAFETY: `p` is a live parcel borrowed for the call; the adapter keeps no pointer and takes ownership of `fd` when it is >= 0.
        let st = unsafe {
            ffi::write_result(
                p.as_native_mut().cast(),
                fd,
                status,
                c.secure,
                c.hdr,
                c.dataspace,
                c.hdr_sdr_ratio,
            )
        };
        assert_eq!(st, NO_ERROR, "libgui writeToParcel");
        p
    }

    /// The codec's value of `c`.
    fn codec_result(c: Case) -> ScreenCaptureResults {
        ScreenCaptureResults {
            fence: match c.fence {
                Fence::None => Ok(None),
                Fence::Fd => Ok(Some(fence_fd())),
                Fence::Status(s) => Err(s),
            },
            captured_secure_layers: c.secure,
            captured_hdr_layers: c.hdr,
            captured_dataspace: c.dataspace,
            hdr_sdr_ratio: c.hdr_sdr_ratio,
            ..Default::default()
        }
    }

    /// The codec's writer: `r` into a fresh parcel, in the raw form (UnstructuredParcelable::write_to_parcel). Serialize adds an AIDL non-null marker first; see the last test.
    fn codec_write(r: &ScreenCaptureResults) -> Parcel {
        let mut p = Parcel::new();
        UnstructuredParcelable::write_to_parcel(r, &mut p.borrowed()).expect("codec write");
        p
    }

    /// The codec's reader: `p` from its start, in the raw form (UnstructuredParcelable::from_parcel).
    fn codec_read(p: &mut Parcel) -> ScreenCaptureResults {
        p.set_data_position_checked(0).expect("rewind");
        <ScreenCaptureResults as UnstructuredParcelable>::from_parcel(p.borrowed_ref())
            .expect("codec read")
    }

    /// libgui's reader, then its writer: `input` from its start through ScreenCaptureResults into a fresh parcel; returns the status of the read.
    fn libgui_reserialize(input: &mut Parcel) -> (i32, Parcel) {
        input.set_data_position_checked(0).expect("rewind");
        let mut out = Parcel::new();
        // SAFETY: both parcels are live and borrowed for the call; the adapter keeps no pointer.
        let st = unsafe { ffi::reserialize(input.as_native().cast(), out.as_native_mut().cast()) };
        (st, out)
    }

    /// The case a codec result carries; its buffers are checked apart.
    fn case_of(r: &ScreenCaptureResults) -> Case {
        Case {
            fence: match &r.fence {
                Ok(Some(_)) => Fence::Fd,
                Ok(None) => Fence::None,
                Err(s) => Fence::Status(*s),
            },
            secure: r.captured_secure_layers,
            hdr: r.captured_hdr_layers,
            dataspace: r.captured_dataspace,
            hdr_sdr_ratio: r.hdr_sdr_ratio,
        }
    }

    /// Blanks a valid fence's fd object. Its type is checked; the rest is libbinder's (the fd number differs per dup, and the flags and cookie are its own).
    fn blank_fence_object(mut b: Vec<u8>) -> Vec<u8> {
        let ty = u32::from_le_bytes(
            b[FENCE_OBJECT..FENCE_OBJECT + 4]
                .try_into()
                .expect("one word"),
        );
        assert_eq!(ty, BINDER_TYPE_FD, "fence fd object at byte {FENCE_OBJECT}");
        b[FENCE_OBJECT..FENCE_OBJECT + FLAT_BINDER_OBJECT].fill(0);
        b
    }

    /// One case, both directions: libgui writes `c` as `words` say, the codec writes the same bytes, the codec reads libgui's parcel,
    /// and libgui reads the codec's parcel and writes the same bytes back.
    fn golden(c: Case, words: &[u32]) {
        let blank = |b: Vec<u8>| {
            if c.fence == Fence::Fd {
                blank_fence_object(b)
            } else {
                b
            }
        };
        let mut theirs = libgui_write(c);
        let mut mine = codec_write(&codec_result(c));
        assert_eq!(blank(bytes(&theirs)), le(words), "libgui's wire order");
        assert_eq!(
            blank(bytes(&mine)),
            blank(bytes(&theirs)),
            "codec write vs libgui write"
        );
        assert_eq!(
            case_of(&codec_read(&mut theirs)),
            c,
            "codec read of libgui's parcel"
        );
        let (st, back) = libgui_reserialize(&mut mine);
        assert_eq!(st, NO_ERROR, "libgui readFromParcel of the codec's parcel");
        assert_eq!(
            blank(bytes(&back)),
            blank(bytes(&mine)),
            "libgui write of the codec's parcel"
        );
    }

    #[test]
    fn default_result() {
        golden(DEFAULT, &[0, 0, 0, 0, 0, V0_SRGB, 0, ONE]);
    }

    /// Ignored: a fence's fd is a binder object, and libbinder's ProcessState opens /dev/binder (or binderd's inherited fd) before it writes one.
    #[test]
    #[ignore = "needs binderd: a fence fd is a binder object; unit tests have no /dev/binder"]
    fn valid_fence() {
        // hasBuffer, hasFence, size 4, fd count 1, numFds 1, the object (blanked: zeros), secure, hdr, dataspace, hasGainMap, ratio
        golden(
            Case {
                fence: Fence::Fd,
                ..DEFAULT
            },
            &[0, 1, 4, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, V0_SRGB, 0, ONE],
        );
    }

    #[test]
    fn error_status_in_place_of_a_fence() {
        golden(
            Case {
                fence: Fence::Status(BAD_VALUE),
                ..DEFAULT
            },
            &[0, 0, BAD_VALUE as u32, 0, 0, V0_SRGB, 0, ONE],
        );
    }

    /// Asymmetric flags: swapping captured_secure_layers and captured_hdr_layers in the codec fails this case.
    #[test]
    fn secure_layers_only() {
        golden(
            Case {
                secure: true,
                ..DEFAULT
            },
            &[0, 0, 0, 1, 0, V0_SRGB, 0, ONE],
        );
    }

    #[test]
    fn hdr_layers_only() {
        golden(
            Case {
                hdr: true,
                ..DEFAULT
            },
            &[0, 0, 0, 0, 1, V0_SRGB, 0, ONE],
        );
    }

    #[test]
    fn secure_and_hdr_layers() {
        golden(
            Case {
                secure: true,
                hdr: true,
                ..DEFAULT
            },
            &[0, 0, 0, 1, 1, V0_SRGB, 0, ONE],
        );
    }

    #[test]
    fn non_zero_dataspace() {
        golden(
            Case {
                dataspace: SRGB_LINEAR,
                ..DEFAULT
            },
            &[0, 0, 0, 0, 0, SRGB_LINEAR, 0, ONE],
        );
    }

    #[test]
    fn hdr_sdr_ratio_of_1_5() {
        golden(
            Case {
                hdr_sdr_ratio: 1.5,
                ..DEFAULT
            },
            &[0, 0, 0, 0, 0, V0_SRGB, 0, ONE_AND_A_HALF],
        );
    }

    /// Both directions against libgui for a buffer and a gain map. Ignored: libgui's GraphicBufferMapper is a binder service (see valid_fence).
    #[test]
    #[ignore = "needs binderd: GraphicBuffer's mapper is a binder service; unit tests have no /dev/binder"]
    fn buffer_and_gain_map_framing() {
        let flat = le(&GRAPHIC_BUFFER);
        let r = ScreenCaptureResults {
            buffer: Some(Flattened {
                payload: flat.clone(),
                fds: Vec::new(),
            }),
            optional_gain_map: Some(Flattened {
                payload: flat.clone(),
                fds: Vec::new(),
            }),
            ..Default::default()
        };
        let mut mine = codec_write(&r);
        // hasBuffer, size, fd count, words; NO_FENCE with status 0; secure, hdr, dataspace; hasGainMap, size, fd count, words; ratio
        let mut expect = vec![1, FLAT_BYTES, 0];
        expect.extend_from_slice(&GRAPHIC_BUFFER);
        expect.extend_from_slice(&[0, NO_ERROR as u32, 0, 0, V0_SRGB, 1, FLAT_BYTES, 0]);
        expect.extend_from_slice(&GRAPHIC_BUFFER);
        expect.push(ONE);
        assert_eq!(bytes(&mine), le(&expect), "codec wire order");
        let (st, mut back) = libgui_reserialize(&mut mine);
        assert_eq!(st, NO_ERROR, "libgui readFromParcel of the codec's parcel");
        assert_eq!(
            bytes(&back),
            le(&expect),
            "libgui write of the codec's parcel"
        );
        let read = codec_read(&mut back);
        assert_eq!(
            read.buffer.as_ref().map(|f| &f.payload),
            Some(&flat),
            "buffer payload"
        );
        assert_eq!(
            read.optional_gain_map.as_ref().map(|f| &f.payload),
            Some(&flat),
            "gain map payload"
        );
        assert_eq!(case_of(&read), DEFAULT, "scalar fields");
    }

    /// The codec's buffer and gain-map framing with no binder object: the wire order (derived), and the codec's own reader returns the payloads.
    #[test]
    fn buffer_and_gain_map_wire_order() {
        let flat = le(&GRAPHIC_BUFFER);
        let r = ScreenCaptureResults {
            buffer: Some(Flattened {
                payload: flat.clone(),
                fds: Vec::new(),
            }),
            optional_gain_map: Some(Flattened {
                payload: flat.clone(),
                fds: Vec::new(),
            }),
            ..Default::default()
        };
        let mut mine = codec_write(&r);
        // hasBuffer, size, fd count, words; NO_FENCE with status 0; secure, hdr, dataspace; hasGainMap, size, fd count, words; ratio
        let mut expect = vec![1, FLAT_BYTES, 0];
        expect.extend_from_slice(&GRAPHIC_BUFFER);
        expect.extend_from_slice(&[0, NO_ERROR as u32, 0, 0, V0_SRGB, 1, FLAT_BYTES, 0]);
        expect.extend_from_slice(&GRAPHIC_BUFFER);
        expect.push(ONE);
        assert_eq!(bytes(&mine), le(&expect), "codec wire order");
        let read = codec_read(&mut mine);
        assert_eq!(
            read.buffer.as_ref().map(|f| &f.payload),
            Some(&flat),
            "buffer payload"
        );
        assert_eq!(
            read.optional_gain_map.as_ref().map(|f| &f.payload),
            Some(&flat),
            "gain map payload"
        );
        assert_eq!(case_of(&read), DEFAULT, "scalar fields");
    }

    /// Serialize (what AIDL's typed-object path calls) writes a non-null marker, 1, then the raw form, as writeTypedObject does for a non-null object;
    /// libgui's readTypedObject expects it. The marker is AIDL's framing, not the codec's, so this test pins the composition.
    #[test]
    fn aidl_serialize_writes_non_null_marker_then_raw_form() {
        let mut p = Parcel::new();
        p.write(&codec_result(DEFAULT)).expect("serialize");
        let mut expect = le(&[1]);
        expect.extend(bytes(&libgui_write(DEFAULT)));
        assert_eq!(bytes(&p), expect, "marker, then libgui's wire form");
        p.set_data_position_checked(0).expect("rewind");
        let back: ScreenCaptureResults = p.read().expect("deserialize");
        assert_eq!(
            case_of(&back),
            DEFAULT,
            "deserialize reads the marker and the raw form"
        );
    }
}
