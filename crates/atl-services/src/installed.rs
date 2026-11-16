// SPDX-License-Identifier: MIT
//! The installer's output (atl-services-types `installer_output`) decoded into the upstream types: the PackageInfo, and per component the parsed intent filters PackageInfo does not carry.
//! All or nothing: a frame defect, a parcel a codec refuses, bytes left after a value, a missing or second PackageInfo or uses-library frame, or an intents frame naming no component of its kind is an error naming it.

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

/// One `<uses-library>` as the parse keeps it: a library declared both ways is required only (ParsingPackageUtils.parseUsesLibrary).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsesLibrary {
    pub name: String,
    pub required: bool,
}

/// SharedLibraryNames.ORG_APACHE_HTTP_LEGACY (services/core/java/com/android/server/pm/parsing/library/SharedLibraryNames.java:35).
pub const ORG_APACHE_HTTP_LEGACY: &str = "org.apache.http.legacy";
/// Build.VERSION_CODES.P (frameworks/base/core/java/android/os/Build.java:1207): targetSdk below it gets the legacy library implicitly.
pub const VERSION_CODES_P: i32 = 28;

/// The libraries a package uses with the implicit one of PackageBackwardCompatibility's OrgApacheHttpLegacyUpdater (scan time, ScanPackageUtils.java:978):
/// an app targeting below P gets `org.apache.http.legacy` first among its required libraries (OrgApacheHttpLegacyUpdater.java:45-47, PackageSharedLibraryUpdater.java:88-97
/// prefixRequiredLibrary), unless it names the library already, required or optional (isLibraryPresent :58-62). Nothing else changes.
pub fn with_implicit_libraries(
    mut libraries: Vec<UsesLibrary>,
    target_sdk: i32,
) -> Vec<UsesLibrary> {
    let named = libraries.iter().any(|l| l.name == ORG_APACHE_HTTP_LEGACY);
    if target_sdk < VERSION_CODES_P && !named {
        libraries.insert(
            0,
            UsesLibrary {
                name: ORG_APACHE_HTTP_LEGACY.to_owned(),
                required: true,
            },
        );
    }
    libraries
}

#[cfg(test)]
mod implicit_library_tests {
    use super::*;

    fn lib(name: &str, required: bool) -> UsesLibrary {
        UsesLibrary {
            name: name.to_owned(),
            required,
        }
    }

    /// targetSdk 27 (below P) and no declaration: the library comes first, required (prefixRequiredLibrary's addUsesLibrary(0, ...)).
    #[test]
    fn below_p_without_declaration_gets_the_library_first_and_required() {
        assert_eq!(
            with_implicit_libraries(Vec::new(), 27),
            [lib("org.apache.http.legacy", true)]
        );
    }

    /// targetSdk 28 is P itself: the rule's `< P` test fails, nothing is added (AOSP: `targetSdkVersion < Build.VERSION_CODES.P`).
    #[test]
    fn at_p_nothing_is_added() {
        assert!(with_implicit_libraries(Vec::new(), 28).is_empty());
    }

    /// An optional declaration already names the library: isLibraryPresent checks both lists, so nothing changes.
    #[test]
    fn optional_declaration_below_p_is_kept_as_declared() {
        let declared = vec![lib("org.apache.http.legacy", false)];
        assert_eq!(with_implicit_libraries(declared.clone(), 27), declared);
    }

    /// A declaration below P also stays where the manifest put it: the required list is not reordered.
    #[test]
    fn required_declaration_below_p_is_kept_in_place() {
        let declared = vec![lib("a.req", true), lib("org.apache.http.legacy", true)];
        assert_eq!(with_implicit_libraries(declared.clone(), 27), declared);
    }

    /// At P a declared library is only what the manifest says, same as below P without the rule.
    #[test]
    fn at_p_declared_libraries_are_unchanged() {
        let declared = vec![lib("a.req", true), lib("b.opt", false)];
        assert_eq!(with_implicit_libraries(declared.clone(), 28), declared);
    }

    /// Below P the implicit library goes in front of the manifest's libraries, which keep their order (required first, as the installer lists them).
    #[test]
    fn below_p_prefixes_before_the_manifest_order() {
        let declared = vec![lib("b.req", true), lib("a.opt", false)];
        assert_eq!(
            with_implicit_libraries(declared, 27),
            [
                lib("org.apache.http.legacy", true),
                lib("b.req", true),
                lib("a.opt", false)
            ]
        );
    }
}

/// A decoded installer output.
#[derive(Debug, Clone)]
pub struct Installed {
    pub package: PackageInfo,
    pub intents: Vec<ComponentIntents>,
    /// required ones first, each list in manifest order
    pub libraries: Vec<UsesLibrary>,
    /// manifest `forceQueryable` (AndroidPackage.isForceQueryable)
    pub force_queryable: bool,
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
        refused: Vec<String>,
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
    NoPackageFields,
    SecondPackageFields {
        frame: usize,
    },
    /// a uses-library list that is null or holds a null name
    NullInLibraries {
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
            DecodeError::NoPackageFields => write!(f, "installer output: no uses-library frame"),
            DecodeError::SecondPackageFields { frame } => {
                write!(
                    f,
                    "installer output: a second uses-library frame at frame {frame}"
                )
            }
            DecodeError::NullInLibraries { frame } => write!(
                f,
                "installer output: frame {frame}: null uses-library list or name"
            ),
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
    let mut fields = None;
    let mut intents = Vec::new();
    for (frame, f) in frames.iter().enumerate() {
        let parcel = |status| parcel_error(frame, f.kind, status, Vec::new());
        let p = Parcel::unmarshal(f.parcel).map_err(parcel)?;
        let before = parcel_refusals();
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
            Value::Fields(required, optional, force_queryable) => {
                let mut list = Vec::new();
                for (names, required) in [(required, true), (optional, false)] {
                    for name in names.ok_or(DecodeError::NullInLibraries { frame })? {
                        let name = name.ok_or(DecodeError::NullInLibraries { frame })?;
                        list.push(UsesLibrary { name, required });
                    }
                }
                if fields.replace((list, force_queryable)).is_some() {
                    return Err(DecodeError::SecondPackageFields { frame });
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
    let (libraries, force_queryable) = fields.ok_or(DecodeError::NoPackageFields)?;
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
    Ok(Installed {
        package,
        intents,
        libraries,
        force_queryable,
    })
}

type StringList = Option<Vec<Option<String>>>;

enum Value {
    Package(Box<PackageInfo>),
    Intents(Option<String>, Vec<Option<ParsedIntentInfo>>),
    Fields(StringList, StringList, bool),
}

fn read(p: &Parcel, kind: Kind) -> Result<Value, StatusCode> {
    let b = p.borrowed_ref();
    Ok(match kind {
        Kind::Package => Value::Package(Box::new(PackageInfo::from_parcel(b)?)),
        // Parcel.writeStringList: the count (-1 for null), then writeString per element -- the binder crate's Option<Vec<Option<String>>> layout
        Kind::PackageFields => Value::Fields(b.read()?, b.read()?, b.read()?),
        _ => {
            let name: Option<String> = b.read()?;
            let list = r_typed_list(b)?.ok_or(StatusCode::UNEXPECTED_NULL)?;
            Value::Intents(name, list)
        }
    })
}

fn parcel_error(frame: usize, kind: Kind, status: StatusCode, refused: Vec<String>) -> DecodeError {
    DecodeError::Parcel {
        frame,
        kind,
        status,
        refused,
    }
}

/// This process's parcelable refusals so far (atl-parcelables `refuse` counts them as `refused-parcel <what>`): (what, count).
fn parcel_refusals() -> Vec<(String, u64)> {
    atl_diag::counts()
        .into_iter()
        .filter(|(outcome, _, _)| outcome == atl_diag::REFUSED_PARCEL)
        .map(|(_, what, n)| (what, n))
        .collect()
}

/// The refusal names whose count grew since `before`.
fn new_refusals(before: &[(String, u64)]) -> Vec<String> {
    parcel_refusals()
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
        Kind::Package | Kind::PackageFields => Box::new(std::iter::empty()),
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

    /// A package-fields frame as the installer writes it: two string lists, then forceQueryable.
    fn fields_bytes(required: &[&str], optional: &[&str], force_queryable: bool) -> Vec<u8> {
        let list = |v: &[&str]| Some(v.iter().map(|s| Some((*s).to_owned())).collect::<Vec<_>>());
        let mut p = Parcel::new();
        p.write(&list(required)).expect("required");
        p.write(&list(optional)).expect("optional");
        p.write(&force_queryable).expect("forceQueryable");
        p.marshal().expect("marshal")
    }

    fn libraries_bytes(required: &[&str], optional: &[&str]) -> Vec<u8> {
        fields_bytes(required, optional, false)
    }

    #[test]
    fn decodes_force_queryable() {
        for want in [false, true] {
            let mut out = Vec::new();
            write_frame(&mut out, Kind::Package, &package_bytes()).expect("frame");
            write_frame(&mut out, Kind::PackageFields, &fields_bytes(&[], &[], want))
                .expect("frame");
            assert_eq!(decode(&out).expect("decode").force_queryable, want);
        }
    }

    fn output(package: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        write_frame(&mut out, Kind::Package, package).expect("frame");
        write_frame(&mut out, Kind::PackageFields, &libraries_bytes(&[], &[])).expect("frame");
        out
    }

    #[test]
    fn decodes_uses_libraries_required_first() {
        let mut out = Vec::new();
        write_frame(&mut out, Kind::Package, &package_bytes()).expect("frame");
        write_frame(
            &mut out,
            Kind::PackageFields,
            &libraries_bytes(&["a.req"], &["b.opt", "c.opt"]),
        )
        .expect("frame");
        let got = decode(&out).expect("decode").libraries;
        let lib = |name: &str, required| UsesLibrary {
            name: name.to_owned(),
            required,
        };
        assert_eq!(
            got,
            [lib("a.req", true), lib("b.opt", false), lib("c.opt", false)]
        );
    }

    #[test]
    fn missing_or_second_uses_libraries_is_refused() {
        let mut out = Vec::new();
        write_frame(&mut out, Kind::Package, &package_bytes()).expect("frame");
        assert!(matches!(decode(&out), Err(DecodeError::NoPackageFields)));
        let mut out = output(&package_bytes());
        write_frame(&mut out, Kind::PackageFields, &libraries_bytes(&[], &[])).expect("frame");
        assert!(matches!(
            decode(&out),
            Err(DecodeError::SecondPackageFields { frame: 2 })
        ));
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
                refused.iter().any(|r| r
                    == "android.content.pm.ApplicationInfo (back-reference to no earlier copy)"),
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
            Err(DecodeError::UnknownComponent { frame: 2, .. })
        ));
    }
}
