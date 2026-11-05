// SPDX-License-Identifier: MIT
//! android.os.Bundle: os/Bundle.java writeToParcel -> BaseBundle.writeToParcelInner (BaseBundle.java:1838-1882) / Bundle(Parcel) -> BaseBundle.readFromParcelInner (BaseBundle.java:1889-1945).
//! Wire: int32 length; 0 is the empty Bundle and nothing more. Else int32 magic (BUNDLE_MAGIC 'BNDL', or BUNDLE_MAGIC_NATIVE 'BNDN' when native code parcelled it, BaseBundle.java:60-61), `length` bytes of body (the ArrayMap of entries), int32 mHasIntent (writeBoolean: 0 or 1).
//! The body is kept as a sub-Parcel: BaseBundle.writeToParcelInner appends mParcelledData (`parcel.appendFrom(mParcelledData, 0, length)`), readFromParcelInner appends the range into a scratch parcel, so a body holds its binder and file descriptor objects
//! as AOSP's Bundles do (androidx puts a binder in broadcast extras). The codec copies the range with `append_from` (Parcel::appendFrom: bytes and object table entries, descriptors dup'd) and writes it back the same way: byte- and object-exact, never parsed.
//! Descriptors are not refused here. AOSP refuses them where it takes a Bundle from an app (`hasFileDescriptors()` in ActivityManagerService / BroadcastController: [`Bundle::has_file_descriptors`]); the services do the same at those call sites.
//! `entries` / `put_all` need the body as bytes and refuse (counted) a body with objects (Parcel::marshal, patches/aosp/frameworks/native/0004); `Bundle` has no `PartialEq`: a body with objects has no bytes to compare.

use std::sync::{Mutex, MutexGuard};

use binder::StatusCode;
use binder::binder_impl::Parcel;

use crate::parcel::*;
use crate::parts::*;

/// BaseBundle.BUNDLE_MAGIC and BUNDLE_MAGIC_NATIVE (BaseBundle.java:60-61): the reader takes either, the writer writes back the one it read.
pub const BUNDLE_MAGIC: i32 = 0x4C44_4E42;
const BUNDLE_MAGIC_NATIVE: i32 = 0x4C44_4E44;

/// android.os.Bundle as its parcelled form. `Default` is the empty Bundle: length 0 and nothing else.
#[derive(Debug, Clone, Default)]
pub struct Bundle {
    /// BUNDLE_MAGIC or BUNDLE_MAGIC_NATIVE; 0 when the body is empty.
    magic: i32,
    /// The map's bytes and object table, as read; data size a multiple of 4; size 0 = the empty Bundle.
    body: Body,
    /// mHasIntent. Java writes 0 or 1 only, so the codec refuses any other value.
    has_intent: bool,
}

/// A body Parcel behind a lock: a Parcel is `Send` but not `Sync` (its cursor is mutable state), and the generated AIDL stubs' async traits need every `&` argument to be `Sync`.
/// The codec only reads a body (`get_data_size`, `marshal`, `append_from` out of it); the lock serialises those.
#[derive(Debug, Default)]
struct Body(Mutex<Parcel>);

impl Body {
    fn new(parcel: Parcel) -> Self {
        Body(Mutex::new(parcel))
    }

    fn lock(&self) -> MutexGuard<'_, Parcel> {
        // a poisoned lock still guards a whole Parcel: no body code panics half-way through a call
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl Clone for Body {
    fn clone(&self) -> Self {
        Body::new(self.lock().clone())
    }
}

impl Part for Bundle {
    fn write(&self, p: &mut In<'_>) -> Result<()> {
        let body = self.body.lock();
        let length = body.get_data_size();
        if length == 0 {
            return w_i32(p, 0);
        }
        if !matches!(self.magic, BUNDLE_MAGIC | BUNDLE_MAGIC_NATIVE) || length % 4 != 0 {
            return Err(StatusCode::BAD_VALUE);
        }
        w_i32(p, length)?;
        w_i32(p, self.magic)?;
        p.append_from(&*body, 0, length)?;
        w_bool(p, self.has_intent)
    }

    fn read(p: &In<'_>) -> Result<Self> {
        let length = r_i32(p)?;
        if length < 0 {
            return Err(crate::refuse("android.os.Bundle (length)"));
        }
        Bundle::read_after_length(p, length)
    }
}

/// Parcel.writeBundle: -1 for null, else the Bundle's own writeToParcel.
pub fn w_bundle(p: &mut In<'_>, v: &Option<Bundle>) -> Result<()> {
    match v {
        None => w_null(p),
        Some(b) => b.write(p),
    }
}

/// Parcel.readBundle: a negative length is null (Parcel.java readBundle: `if (length < 0) return null`), else the Bundle.
pub fn r_bundle(p: &In<'_>) -> Result<Option<Bundle>> {
    let length = r_i32(p)?;
    if length < 0 {
        return Ok(None);
    }
    Bundle::read_after_length(p, length).map(Some)
}

/// A Bundle value of the kinds a service makes or merges (Parcel.writeValue's VAL_STRING, VAL_INTEGER, VAL_LONG, VAL_FLOAT, VAL_BOOLEAN, VAL_NULL); Float = manifest `<meta-data>` (Property.toBundle putFloat).
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    String(Option<String>),
    Int(i32),
    Long(i64),
    Float(f32),
    Bool(bool),
    Null,
}

/// Parcel.java VAL_* type codes (Parcel.java:317-327).
const VAL_NULL: i32 = -1;
const VAL_STRING: i32 = 0;
const VAL_INTEGER: i32 = 1;
const VAL_LONG: i32 = 6;
const VAL_FLOAT: i32 = 7;
const VAL_BOOLEAN: i32 = 9;

/// String.hashCode: the order of an ArrayMap's entries, which Parcel.readArrayMap appends in the order written (Parcel.java:5688-5689, `sorted`).
fn java_hash(s: &str) -> i32 {
    s.encode_utf16()
        .fold(0i32, |h, c| h.wrapping_mul(31).wrapping_add(i32::from(c)))
}

/// A little-endian cursor over a body (the codec runs on little-endian hosts only, as `write` above hands the words to the parcel in native order).
struct Words<'a>(&'a [u8]);

impl Words<'_> {
    fn i32(&mut self) -> Result<i32> {
        let (w, rest) = self
            .0
            .split_first_chunk::<4>()
            .ok_or(StatusCode::NOT_ENOUGH_DATA)?;
        self.0 = rest;
        Ok(i32::from_ne_bytes(*w))
    }

    fn i64(&mut self) -> Result<i64> {
        let (w, rest) = self
            .0
            .split_first_chunk::<8>()
            .ok_or(StatusCode::NOT_ENOUGH_DATA)?;
        self.0 = rest;
        Ok(i64::from_ne_bytes(*w))
    }

    /// Parcel.writeString (writeString16): int32 UTF-16 unit count (-1 = null), the units and a NUL unit, padded to 4 bytes.
    fn string(&mut self) -> Result<Option<String>> {
        let n = self.i32()?;
        if n == -1 {
            return Ok(None);
        }
        let n =
            usize::try_from(n).map_err(|_| crate::refuse("android.os.Bundle (string length)"))?;
        let bytes = (n + 1) * 2;
        let padded = bytes.next_multiple_of(4);
        if self.0.len() < padded {
            return Err(StatusCode::NOT_ENOUGH_DATA);
        }
        let units: Vec<u16> = self.0[..n * 2]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_ne_bytes(*c))
            .collect();
        self.0 = &self.0[padded..];
        String::from_utf16(&units)
            .map(Some)
            .map_err(|_| crate::refuse("android.os.Bundle (string encoding)"))
    }
}

fn put_string(out: &mut Vec<u8>, s: &Option<String>) {
    let Some(s) = s else {
        out.extend_from_slice(&(-1i32).to_ne_bytes());
        return;
    };
    let units: Vec<u16> = s.encode_utf16().collect();
    // a Java String is far below i32::MAX units; a longer one cannot be a Bundle key or value
    out.extend_from_slice(&i32::try_from(units.len()).unwrap_or(i32::MAX).to_ne_bytes());
    for u in &units {
        out.extend_from_slice(&u.to_ne_bytes());
    }
    out.extend_from_slice(&[0, 0]);
    out.resize(out.len().next_multiple_of(4), 0);
}

impl Bundle {
    /// The rest of a Bundle whose `length` (a non-negative int32) was read: BaseBundle.readFromParcelInner(Parcel, length).
    fn read_after_length(p: &In<'_>, length: i32) -> Result<Bundle> {
        if length == 0 {
            return Ok(Bundle::default());
        }
        if length % 4 != 0 {
            return Err(crate::refuse("android.os.Bundle (length)"));
        }
        let magic = r_i32(p)?;
        if magic != BUNDLE_MAGIC && magic != BUNDLE_MAGIC_NATIVE {
            return Err(crate::refuse("android.os.Bundle (magic)"));
        }
        // the body's range with its object table entries (descriptors dup'd), then skipped in the source
        let start = p.get_data_position();
        let mut body = Parcel::new();
        body.append_from(p, start, length)?;
        p.set_data_position_checked(start + length)?;
        let has_intent = match r_i32(p)? {
            0 => false,
            1 => true,
            _ => return Err(crate::refuse("android.os.Bundle.mHasIntent")),
        };
        Ok(Bundle {
            magic,
            body: Body::new(body),
            has_intent,
        })
    }

    /// Bundle.hasFileDescriptors (Bundle.java:431-440): the parcelled data holds a file descriptor object. AOSP checks it before it takes a Bundle from an app, see the module doc.
    pub fn has_file_descriptors(&self) -> bool {
        self.body.lock().has_file_descriptors()
    }

    /// The body's bytes. A body with binder or file descriptor objects has none (Parcel::marshal refuses it): refused and counted.
    fn body_bytes(&self) -> Result<Vec<u8>> {
        self.body.lock().marshal().map_err(|e| {
            if e == StatusCode::INVALID_OPERATION {
                crate::refuse(
                    "android.os.Bundle (entries of a body with binder or file descriptor objects)",
                )
            } else {
                e
            }
        })
    }

    /// A body parcel of `bytes`: object-free, written as words in native order (the codec runs on little-endian hosts only, see `Words`).
    fn parcel_of(bytes: &[u8]) -> Parcel {
        let mut body = Parcel::new();
        for w in bytes.as_chunks::<4>().0 {
            body.write(&i32::from_ne_bytes(*w))
                .expect("writing an int to a new Parcel");
        }
        body
    }

    /// The entries of the map body, in order. A value of a kind outside [`Value`] (a nested Bundle, an array, a Parcelable...) is refused and counted: its length is not known without a codec for it.
    pub fn entries(&self) -> Result<Vec<(String, Value)>> {
        if self.body.lock().get_data_size() == 0 {
            return Ok(Vec::new());
        }
        let bytes = self.body_bytes()?;
        let mut w = Words(&bytes);
        let n = w.i32()?;
        (0..n.max(0))
            .map(|_| {
                let key = w
                    .string()?
                    .ok_or_else(|| crate::refuse("android.os.Bundle (null key)"))?;
                let value = match w.i32()? {
                    VAL_NULL => Value::Null,
                    VAL_STRING => Value::String(w.string()?),
                    VAL_INTEGER => Value::Int(w.i32()?),
                    VAL_LONG => Value::Long(w.i64()?),
                    // writeFloat: the IEEE bits in one word
                    VAL_FLOAT => Value::Float(f32::from_bits(w.i32()? as u32)),
                    VAL_BOOLEAN => Value::Bool(w.i32()? != 0),
                    _ => return Err(crate::refuse("android.os.Bundle (value type)")),
                };
                Ok((key, value))
            })
            .collect()
    }

    /// A Bundle of `entries` (later ones replace earlier ones of the same key, as Bundle.put* do), written as Java writes it: sorted by key hash.
    pub fn from_entries(entries: impl IntoIterator<Item = (String, Value)>) -> Bundle {
        let mut map: Vec<(String, Value)> = Vec::new();
        for (k, v) in entries {
            match map.iter_mut().find(|(have, _)| *have == k) {
                Some(slot) => slot.1 = v,
                None => map.push((k, v)),
            }
        }
        map.sort_by_key(|(k, _)| java_hash(k));
        if map.is_empty() {
            return Bundle::default();
        }
        let mut body = Vec::new();
        body.extend_from_slice(&i32::try_from(map.len()).unwrap_or(i32::MAX).to_ne_bytes());
        for (k, v) in &map {
            put_string(&mut body, &Some(k.clone()));
            let tag = |t: i32| t.to_ne_bytes();
            match v {
                Value::Null => body.extend_from_slice(&tag(VAL_NULL)),
                Value::String(s) => {
                    body.extend_from_slice(&tag(VAL_STRING));
                    put_string(&mut body, s);
                }
                Value::Int(i) => {
                    body.extend_from_slice(&tag(VAL_INTEGER));
                    body.extend_from_slice(&i.to_ne_bytes());
                }
                Value::Long(l) => {
                    body.extend_from_slice(&tag(VAL_LONG));
                    body.extend_from_slice(&l.to_ne_bytes());
                }
                Value::Float(f) => {
                    body.extend_from_slice(&tag(VAL_FLOAT));
                    body.extend_from_slice(&f.to_ne_bytes());
                }
                Value::Bool(b) => {
                    body.extend_from_slice(&tag(VAL_BOOLEAN));
                    body.extend_from_slice(&i32::from(*b).to_ne_bytes());
                }
            }
        }
        Bundle {
            magic: BUNDLE_MAGIC,
            body: Body::new(Bundle::parcel_of(&body)),
            has_intent: false,
        }
    }

    /// A Bundle of string entries (Bundle.putString per pair).
    pub fn from_strings(pairs: &[(&str, &str)]) -> Bundle {
        Bundle::from_entries(
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_owned(), Value::String(Some((*v).to_owned())))),
        )
    }

    /// Bundle.putAll: `other`'s entries replace this one's of the same key. Refuses (counted) a Bundle with a value kind outside [`Value`], or with binder or file descriptor objects; this one is then unchanged.
    pub fn put_all(&mut self, other: &Bundle) -> Result<()> {
        let merged = Bundle::from_entries(self.entries()?.into_iter().chain(other.entries()?));
        *self = merged;
        Ok(())
    }
}

crate::codec!(Bundle);
