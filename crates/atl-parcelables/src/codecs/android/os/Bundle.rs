// SPDX-License-Identifier: MIT
//! android.os.Bundle: os/Bundle.java writeToParcel -> BaseBundle.writeToParcelInner (BaseBundle.java:1838-1882) / Bundle(Parcel) -> BaseBundle.readFromParcelInner (BaseBundle.java:1889-1945).
//! Wire: int32 length; 0 is the empty Bundle and nothing more. Else int32 magic (BUNDLE_MAGIC 'BNDL', or BUNDLE_MAGIC_NATIVE 'BNDN' when native code parcelled it, BaseBundle.java:60-61), `length` bytes of body (the ArrayMap of entries), int32 mHasIntent (writeBoolean: 0 or 1).
//! The body is a blob, copied verbatim and never parsed. A body that holds a binder or file descriptor is refused and counted: the object would be lost from the bytes. The copy is made as a parcel and marshalled, and Parcel::marshal refuses objects (patches/aosp/frameworks/native/0004).
//! AMS refuses the same in instrumentation results: ActivityManagerService.java:14917-14923 (finishInstrumentation, results.hasFileDescriptors()).

use binder::StatusCode;
use binder::binder_impl::Parcel;

use crate::parcel::*;
use crate::parts::*;

/// BaseBundle.BUNDLE_MAGIC and BUNDLE_MAGIC_NATIVE (BaseBundle.java:60-61): the reader takes either, the writer writes back the one it read.
const BUNDLE_MAGIC: i32 = 0x4C44_4E42;
const BUNDLE_MAGIC_NATIVE: i32 = 0x4C44_4E44;

/// android.os.Bundle as its blob. `Default` is the empty Bundle: length 0 and nothing else.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Bundle {
    /// BUNDLE_MAGIC or BUNDLE_MAGIC_NATIVE; 0 when the body is empty.
    pub magic: i32,
    /// The map's bytes, verbatim; a multiple of 4; empty = the empty Bundle.
    pub body: Vec<u8>,
    /// mHasIntent. Java writes 0 or 1 only, so the codec refuses any other value.
    pub has_intent: bool,
}

impl Part for Bundle {
    fn write(&self, p: &mut In<'_>) -> Result<()> {
        if self.body.is_empty() {
            return w_i32(p, 0);
        }
        if !matches!(self.magic, BUNDLE_MAGIC | BUNDLE_MAGIC_NATIVE)
            || !self.body.len().is_multiple_of(4)
        {
            return Err(StatusCode::BAD_VALUE);
        }
        w_i32(p, len_i32(self.body.len())?)?;
        w_i32(p, self.magic)?;
        for w in self.body.as_chunks::<4>().0 {
            w_i32(p, i32::from_ne_bytes(*w))?;
        }
        w_bool(p, self.has_intent)
    }

    fn read(p: &In<'_>) -> Result<Self> {
        let length = r_i32(p)?;
        if length == 0 {
            return Ok(Bundle::default());
        }
        if length < 0 || length % 4 != 0 {
            return Err(crate::refuse("android.os.Bundle (length)"));
        }
        let magic = r_i32(p)?;
        if magic != BUNDLE_MAGIC && magic != BUNDLE_MAGIC_NATIVE {
            return Err(crate::refuse("android.os.Bundle (magic)"));
        }
        let body = read_body(p, length)?;
        let has_intent = match r_i32(p)? {
            0 => false,
            1 => true,
            _ => return Err(crate::refuse("android.os.Bundle.mHasIntent")),
        };
        Ok(Bundle {
            magic,
            body,
            has_intent,
        })
    }
}

/// A Bundle value of the kinds a service makes or merges (Parcel.writeValue's VAL_STRING, VAL_INTEGER, VAL_LONG, VAL_BOOLEAN, VAL_NULL).
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    String(Option<String>),
    Int(i32),
    Long(i64),
    Bool(bool),
    Null,
}

/// Parcel.java VAL_* type codes (Parcel.java:317-327).
const VAL_NULL: i32 = -1;
const VAL_STRING: i32 = 0;
const VAL_INTEGER: i32 = 1;
const VAL_LONG: i32 = 6;
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
    /// The entries of the map body, in order. A value of a kind outside [`Value`] (a nested Bundle, an array, a Parcelable...) is refused and counted: its length is not known without a codec for it.
    pub fn entries(&self) -> Result<Vec<(String, Value)>> {
        if self.body.is_empty() {
            return Ok(Vec::new());
        }
        let mut w = Words(&self.body);
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
                Value::Bool(b) => {
                    body.extend_from_slice(&tag(VAL_BOOLEAN));
                    body.extend_from_slice(&i32::from(*b).to_ne_bytes());
                }
            }
        }
        Bundle {
            magic: BUNDLE_MAGIC,
            body,
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

    /// Bundle.putAll: `other`'s entries replace this one's of the same key. Refuses (counted) a Bundle with a value kind outside [`Value`]; this one is then unchanged.
    pub fn put_all(&mut self, other: &Bundle) -> Result<()> {
        let merged = Bundle::from_entries(self.entries()?.into_iter().chain(other.entries()?));
        *self = merged;
        Ok(())
    }
}

/// The body's `length` bytes, copied out and skipped. The copy is a parcel, marshalled: a binder or file descriptor in the range is in its object table, and marshal refuses it (counted).
fn read_body(p: &In<'_>, length: i32) -> Result<Vec<u8>> {
    let start = p.get_data_position();
    let mut copy = Parcel::new();
    copy.append_from(p, start, length)?;
    let body = copy.marshal().map_err(|e| {
        if e == StatusCode::INVALID_OPERATION {
            crate::refuse("android.os.Bundle (file descriptor or binder)")
        } else {
            e
        }
    })?;
    p.set_data_position_checked(start + length)?;
    Ok(body)
}

crate::codec!(Bundle);
