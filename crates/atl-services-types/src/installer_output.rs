// SPDX-License-Identifier: MIT
//! The installer's output (plan item 4b): upstream parcels in frames, on its stdout. A frame is `u32 kind`, `u32 length` (little-endian), then `length` bytes of a marshalled Parcel:
//! - [`Kind::Package`], once: `PackageInfo.writeToParcel` of PackageInfoCommonUtils.generate's PackageInfo, the installer's ABI choice set on its ApplicationInfo (primaryCpuAbi, nativeLibraryDir), as PMS's
//!   PackageInfoUtils sets them from the package setting.
//! - [`Kind::ActivityIntents`] .. [`Kind::ProviderIntents`], one per component with intent filters (PackageInfo carries none): the first and the intents fields of ParsedComponentImpl.writeToParcel
//!   (frameworks/base core/java/com/android/internal/pm/pkg/component/ParsedComponentImpl.java:159-169): the class name as writeString, then writeTypedList of its ParsedIntentInfoImpl.
//!
//! The frame layer only splits; atl-services decodes the parcels (`installed`).

use std::fmt;
use std::io::{self, Write};

/// A frame's payload kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Package = 1,
    ActivityIntents = 2,
    ReceiverIntents = 3,
    ServiceIntents = 4,
    ProviderIntents = 5,
}

impl Kind {
    pub const ALL: [Kind; 5] = [
        Kind::Package,
        Kind::ActivityIntents,
        Kind::ReceiverIntents,
        Kind::ServiceIntents,
        Kind::ProviderIntents,
    ];

    fn from_u32(v: u32) -> Option<Kind> {
        Kind::ALL.into_iter().find(|k| *k as u32 == v)
    }
}

/// One frame of the output.
#[derive(Debug, PartialEq, Eq)]
pub struct Frame<'a> {
    pub kind: Kind,
    pub parcel: &'a [u8],
}

/// Why the output does not split into frames; `at` is the byte offset of the frame's header.
#[derive(Debug, PartialEq, Eq)]
pub enum FrameError {
    /// fewer than 8 header bytes left
    TruncatedHeader {
        at: usize,
    },
    UnknownKind {
        at: usize,
        kind: u32,
    },
    /// the length runs past the end of the output
    LengthPastEnd {
        at: usize,
        length: u32,
        left: usize,
    },
}

impl fmt::Display for FrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FrameError::TruncatedHeader { at } => {
                write!(f, "installer output: truncated frame header at byte {at}")
            }
            FrameError::UnknownKind { at, kind } => write!(
                f,
                "installer output: unknown frame kind {kind} at byte {at}"
            ),
            FrameError::LengthPastEnd { at, length, left } => {
                write!(
                    f,
                    "installer output: frame at byte {at} claims {length} bytes, {left} left"
                )
            }
        }
    }
}

impl std::error::Error for FrameError {}

/// Writes one frame.
pub fn write_frame(out: &mut impl Write, kind: Kind, parcel: &[u8]) -> io::Result<()> {
    let length = u32::try_from(parcel.len()).map_err(|_| io::Error::other("frame over 4 GiB"))?;
    out.write_all(&(kind as u32).to_le_bytes())?;
    out.write_all(&length.to_le_bytes())?;
    out.write_all(parcel)
}

/// The frames of a whole output, or the first defect; never a partial list.
pub fn frames(data: &[u8]) -> Result<Vec<Frame<'_>>, FrameError> {
    let mut out = Vec::new();
    let mut at = 0;
    while at < data.len() {
        let header = data
            .get(at..at + 8)
            .ok_or(FrameError::TruncatedHeader { at })?;
        let (kind, length) = header.split_at(4);
        let kind = u32::from_le_bytes(kind.try_into().expect("4 bytes"));
        let length = u32::from_le_bytes(length.try_into().expect("4 bytes"));
        let kind = Kind::from_u32(kind).ok_or(FrameError::UnknownKind { at, kind })?;
        let start = at + 8;
        let left = data.len() - start;
        let end = usize::try_from(length)
            .ok()
            .filter(|n| *n <= left)
            .map(|n| start + n)
            .ok_or(FrameError::LengthPastEnd { at, length, left })?;
        out.push(Frame {
            kind,
            parcel: &data[start..end],
        });
        at = end;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn two_frames() -> Vec<u8> {
        let mut out = Vec::new();
        write_frame(&mut out, Kind::Package, &[1, 2, 3, 4]).unwrap();
        write_frame(&mut out, Kind::ServiceIntents, &[]).unwrap();
        out
    }

    #[test]
    fn frames_round_trip() {
        let data = two_frames();
        // header 8 + 4 payload, then header 8 + 0: the layout the module doc gives
        assert_eq!(data.len(), 20);
        assert_eq!(
            frames(&data).unwrap(),
            [
                Frame {
                    kind: Kind::Package,
                    parcel: &[1, 2, 3, 4]
                },
                Frame {
                    kind: Kind::ServiceIntents,
                    parcel: &[]
                },
            ]
        );
    }

    #[test]
    fn corrupt_length_is_refused_by_name() {
        let mut data = two_frames();
        // the first frame's length: 4 -> 400; 20 bytes - its 8-byte header = 12 left
        data[4..8].copy_from_slice(&400u32.to_le_bytes());
        assert_eq!(
            frames(&data),
            Err(FrameError::LengthPastEnd {
                at: 0,
                length: 400,
                left: 12
            })
        );
    }

    #[test]
    fn truncated_output_is_refused_by_name() {
        let data = two_frames();
        // cut inside the second header (it starts at byte 12)
        assert_eq!(
            frames(&data[..15]),
            Err(FrameError::TruncatedHeader { at: 12 })
        );
        // cut inside the first payload
        assert_eq!(
            frames(&data[..10]),
            Err(FrameError::LengthPastEnd {
                at: 0,
                length: 4,
                left: 2
            })
        );
    }

    #[test]
    fn unknown_kind_is_refused_by_name() {
        let mut data = two_frames();
        data[0..4].copy_from_slice(&9u32.to_le_bytes());
        assert_eq!(
            frames(&data),
            Err(FrameError::UnknownKind { at: 0, kind: 9 })
        );
    }
}
