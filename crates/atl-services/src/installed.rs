// SPDX-License-Identifier: MIT
//! The installer's output (atl-services-types `installer_output`) decoded into the upstream types: the PackageInfo, and per component the parsed intent filters PackageInfo does not carry.
//! All or nothing: a frame defect, a parcel a codec refuses, bytes left after a value, a second PackageInfo or an intents frame naming no component of its kind is an error naming it.

use std::fmt;

use atl_parcelables::android::content::pm::PackageInfo;
use atl_parcelables::parts::{ParsedIntentInfo, r_typed_list};
use atl_services_types::installer_output::{FrameError, Kind, frames};
use binder::StatusCode;
use binder::binder_impl::{Parcel, UnstructuredParcelable};

/// One component's `<intent-filter>`s, in manifest order.
#[derive(Debug, Clone)]
pub struct ComponentIntents {
    /// ActivityIntents .. ProviderIntents
    pub kind: Kind,
    /// the component's class name, as in its PackageInfo entry
    pub name: String,
    pub intents: Vec<ParsedIntentInfo>,
}

/// A decoded installer output.
#[derive(Debug, Clone)]
pub struct Installed {
    pub package: PackageInfo,
    pub intents: Vec<ComponentIntents>,
}

/// Why an output does not decode; `frame` is the frame's index.
#[derive(Debug)]
pub enum DecodeError {
    Frame(FrameError),
    /// a codec refused the parcel (`refused`: the refusal names it counted, atl-parcelables `refuse`) or the parcel ended early
    Parcel {
        frame: usize,
        kind: Kind,
        status: StatusCode,
        refused: Vec<&'static str>,
    },
    /// the value ended before its frame did
    Trailing {
        frame: usize,
        kind: Kind,
        consumed: i32,
        size: i32,
    },
    NoPackage,
    SecondPackage {
        frame: usize,
    },
    /// an intents frame without a class name, with a null list or a null element
    NullInIntents {
        frame: usize,
    },
    UnknownComponent {
        frame: usize,
        kind: Kind,
        name: String,
    },
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DecodeError::Frame(e) => e.fmt(f),
            DecodeError::Parcel {
                frame,
                kind,
                status,
                refused,
            } => {
                write!(
                    f,
                    "installer output: frame {frame} ({kind:?}) refused: {status:?}, refused {refused:?}"
                )
            }
            DecodeError::Trailing {
                frame,
                kind,
                consumed,
                size,
            } => {
                write!(
                    f,
                    "installer output: frame {frame} ({kind:?}) decoded {consumed} of {size} bytes"
                )
            }
            DecodeError::NoPackage => write!(f, "installer output: no PackageInfo frame"),
            DecodeError::SecondPackage { frame } => {
                write!(f, "installer output: a second PackageInfo at frame {frame}")
            }
            DecodeError::NullInIntents { frame } => write!(
                f,
                "installer output: frame {frame}: null class name or intent"
            ),
            DecodeError::UnknownComponent { frame, kind, name } => {
                write!(
                    f,
                    "installer output: frame {frame}: no {kind:?} component {name}"
                )
            }
        }
    }
}

impl std::error::Error for DecodeError {}

/// Decodes a whole output.
pub fn decode(data: &[u8]) -> Result<Installed, DecodeError> {
    let frames = frames(data).map_err(DecodeError::Frame)?;
    let mut package = None;
    let mut intents = Vec::new();
    for (frame, f) in frames.iter().enumerate() {
        let parcel = |status| parcel_error(frame, f.kind, status, Vec::new());
        let p = Parcel::unmarshal(f.parcel).map_err(parcel)?;
        let before = atl_parcelables::refused();
        let decoded = read(&p, f.kind);
        let decoded =
            decoded.map_err(|status| parcel_error(frame, f.kind, status, new_refusals(&before)))?;
        if p.get_data_position() != p.get_data_size() {
            return Err(DecodeError::Trailing {
                frame,
                kind: f.kind,
                consumed: p.get_data_position(),
                size: p.get_data_size(),
            });
        }
        match decoded {
            Value::Package(info) => {
                if package.replace(*info).is_some() {
                    return Err(DecodeError::SecondPackage { frame });
                }
            }
            Value::Intents(name, list) => {
                let name = name.ok_or(DecodeError::NullInIntents { frame })?;
                let list = list
                    .into_iter()
                    .collect::<Option<Vec<_>>>()
                    .ok_or(DecodeError::NullInIntents { frame })?;
                intents.push((
                    frame,
                    ComponentIntents {
                        kind: f.kind,
                        name,
                        intents: list,
                    },
                ));
            }
        }
    }
    let package = package.ok_or(DecodeError::NoPackage)?;
    let intents = intents
        .into_iter()
        .map(|(frame, c)| {
            if component_names(&package, c.kind).any(|n| n == c.name) {
                Ok(c)
            } else {
                Err(DecodeError::UnknownComponent {
                    frame,
                    kind: c.kind,
                    name: c.name,
                })
            }
        })
        .collect::<Result<_, _>>()?;
    Ok(Installed { package, intents })
}

enum Value {
    Package(Box<PackageInfo>),
    Intents(Option<String>, Vec<Option<ParsedIntentInfo>>),
}

fn read(p: &Parcel, kind: Kind) -> Result<Value, StatusCode> {
    let b = p.borrowed_ref();
    Ok(match kind {
        Kind::Package => Value::Package(Box::new(PackageInfo::from_parcel(b)?)),
        _ => {
            let name: Option<String> = b.read()?;
            let list = r_typed_list(b)?.ok_or(StatusCode::UNEXPECTED_NULL)?;
            Value::Intents(name, list)
        }
    })
}

fn parcel_error(
    frame: usize,
    kind: Kind,
    status: StatusCode,
    refused: Vec<&'static str>,
) -> DecodeError {
    DecodeError::Parcel {
        frame,
        kind,
        status,
        refused,
    }
}

/// The refusal names whose count grew since `before`.
fn new_refusals(before: &[(&'static str, u64)]) -> Vec<&'static str> {
    atl_parcelables::refused()
        .into_iter()
        .filter(|(name, n)| {
            before
                .iter()
                .find(|(b, _)| b == name)
                .is_none_or(|(_, m)| n > m)
        })
        .map(|(name, _)| name)
        .collect()
}

/// The class names of the PackageInfo's components of an intents kind.
pub fn component_names(package: &PackageInfo, kind: Kind) -> Box<dyn Iterator<Item = &str> + '_> {
    fn names<'a, T: 'a>(
        list: &'a Option<Vec<Option<T>>>,
        name: fn(&T) -> &Option<String>,
    ) -> Box<dyn Iterator<Item = &'a str> + 'a> {
        Box::new(
            list.iter()
                .flatten()
                .flatten()
                .filter_map(move |c| name(c).as_deref()),
        )
    }
    match kind {
        Kind::ActivityIntents => names(&package.activities, |c| &c.component.item.name),
        Kind::ReceiverIntents => names(&package.receivers, |c| &c.component.item.name),
        Kind::ServiceIntents => names(&package.services, |c| &c.component.item.name),
        Kind::ProviderIntents => names(&package.providers, |c| &c.component.item.name),
        Kind::Package => Box::new(std::iter::empty()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atl_parcelables::android::content::pm::ApplicationInfo;
    use atl_parcelables::parts::Part;
    use atl_services_types::installer_output::write_frame;

    fn app() -> ApplicationInfo {
        let mut ai = ApplicationInfo::default();
        ai.item.package_name = Some("com.example.app".into());
        ai.target_sdk_version = 35;
        ai
    }

    /// A PackageInfo with one activity sharing the package's ApplicationInfo, so the activity's copy is a back-reference.
    fn package_bytes() -> Vec<u8> {
        let mut activity = atl_parcelables::android::content::pm::ActivityInfo::default();
        activity.component.item.name = Some("com.example.app.Main".into());
        activity.component.application_info = app();
        let info = PackageInfo {
            package_name: Some("com.example.app".into()),
            application_info: Some(app()),
            activities: Some(vec![Some(activity)]),
            ..Default::default()
        };
        let mut p = Parcel::new();
        info.write(&mut p.borrowed()).expect("encode");
        p.marshal().expect("marshal")
    }

    fn output(package: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        write_frame(&mut out, Kind::Package, package).expect("frame");
        out
    }

    #[test]
    fn decodes_a_squashed_package() {
        let got = decode(&output(&package_bytes())).expect("decode");
        let activity = got.package.activities.unwrap().remove(0).unwrap();
        // the back-reference resolved to the package's own ApplicationInfo
        assert_eq!(activity.component.application_info.target_sdk_version, 35);
        assert_eq!(
            activity
                .component
                .application_info
                .item
                .package_name
                .as_deref(),
            Some("com.example.app")
        );
    }

    #[test]
    fn truncated_parcel_is_refused() {
        let bytes = package_bytes();
        let cut = &bytes[..bytes.len() - 8];
        assert!(matches!(
            decode(&output(cut)),
            Err(DecodeError::Parcel {
                frame: 0,
                kind: Kind::Package,
                ..
            })
        ));
    }

    #[test]
    fn corrupt_frame_length_is_refused() {
        let mut out = output(&package_bytes());
        out[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(matches!(
            decode(&out),
            Err(DecodeError::Frame(FrameError::LengthPastEnd { at: 0, .. }))
        ));
    }

    #[test]
    fn back_reference_to_no_copy_is_refused() {
        let mut bytes = package_bytes();
        // the activity's ApplicationInfo is the last back-reference offset written: a positive int that is not 0. Find it by re-encoding with the activity's copy unsquashed
        // (a different target SDK) -- the two encodings differ from that int on.
        let first_diff = {
            let mut activity = atl_parcelables::android::content::pm::ActivityInfo::default();
            activity.component.item.name = Some("com.example.app.Main".into());
            let mut other = app();
            other.target_sdk_version = 34;
            activity.component.application_info = other;
            let info = PackageInfo {
                package_name: Some("com.example.app".into()),
                application_info: Some(app()),
                activities: Some(vec![Some(activity)]),
                ..Default::default()
            };
            let mut p = Parcel::new();
            info.write(&mut p.borrowed()).expect("encode");
            let full = p.marshal().expect("marshal");
            bytes
                .iter()
                .zip(&full)
                .position(|(a, b)| a != b)
                .expect("encodings differ")
        };
        // the differing int, aligned: the offset (squashed) vs 0 (full copy)
        let at = first_diff / 4 * 4;
        let offset = i32::from_ne_bytes(bytes[at..at + 4].try_into().expect("4 bytes"));
        assert!(
            offset > 0,
            "expected a back-reference at byte {at}, found {offset}"
        );
        // point it 4 bytes further back: no copy starts there
        bytes[at..at + 4].copy_from_slice(&(offset + 4).to_ne_bytes());
        match decode(&output(&bytes)) {
            Err(DecodeError::Parcel { refused, .. }) => assert!(
                refused.contains(
                    &"android.content.pm.ApplicationInfo (back-reference to no earlier copy)"
                ),
                "{refused:?}"
            ),
            other => panic!("expected a refused back-reference, got {other:?}"),
        }
    }

    #[test]
    fn intents_of_an_unknown_component_are_refused() {
        let mut out = output(&package_bytes());
        let mut p = Parcel::new();
        p.write(&Some("com.example.app.Other".to_owned()))
            .expect("name");
        p.write(&0i32).expect("empty list");
        write_frame(
            &mut out,
            Kind::ActivityIntents,
            &p.marshal().expect("marshal"),
        )
        .expect("frame");
        assert!(matches!(
            decode(&out),
            Err(DecodeError::UnknownComponent { frame: 1, .. })
        ));
    }
}
