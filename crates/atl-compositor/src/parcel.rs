// SPDX-License-Identifier: Apache-2.0
// Derived from the Android Open Source Project: frameworks/native/libs/binder/Parcel.cpp (primitive writers/readers, flattenBinder, writeFileDescriptor, write(Flattenable), writeInterfaceToken)
//! A Parcel as libbinder's kernel transport holds it: little-endian words in a byte buffer padded to 4, plus the table of offsets at which a `flat_binder_object` (binder, handle or fd) sits inline.
//! Binders and fds stay objects ([`StrongBinder`], [`FdObject`]): this crate never owns or closes a descriptor, the front-end that converts to and from a real Parcel does.
//! Writers are infallible except where a length may not fit the wire's int32; readers fail with [`ParcelError`] instead of reading past the data.

use std::fmt;

/// `BINDER_TYPE_BINDER` = B_PACK_CHARS('s','b','*',0x85): a node of the sending process.
pub const BINDER_TYPE_BINDER: u32 = 0x7362_2a85;
/// `BINDER_TYPE_HANDLE` = B_PACK_CHARS('s','h','*',0x85): a reference to another process's node.
pub const BINDER_TYPE_HANDLE: u32 = 0x7368_2a85;
/// `BINDER_TYPE_FD` = B_PACK_CHARS('f','d','*',0x85).
pub const BINDER_TYPE_FD: u32 = 0x6664_2a85;
/// `sizeof(flat_binder_object)` on a 64-bit target: type u32, flags u32, binder/handle union u64, cookie u64.
pub const FLAT_OBJECT_SIZE: usize = 24;

/// BBinder's `kHeader` for the interface token under the kernel transport (Parcel.cpp: B_PACK_CHARS('S','Y','S','T')).
const INTERFACE_HEADER: i32 = 0x5359_5354;
/// `STRICT_MODE_PENALTY_GATHER` (1 << 31), or-ed into the strict-mode word of every interface token.
const STRICT_MODE_PENALTY_GATHER: i32 = i32::MIN;
/// `IPCThreadState::kUnsetWorkSource`.
const UNSET_WORK_SOURCE: i32 = -1;

/// Why a Parcel could not be read or a length could not be written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParcelError {
    /// `need` bytes wanted at `pos`, fewer remain.
    OutOfData { pos: usize, need: usize },
    /// A value the wire does not allow (negative size, unknown tag, non-zero reserved bits ...).
    BadValue(&'static str),
    /// A binder or fd was read at a position that holds no object.
    NotAnObject { pos: usize },
    /// An object of another type than the field takes.
    BadObjectType { pos: usize, ty: u32 },
    /// A binder that the field does not allow to be null was null.
    UnexpectedNull(&'static str),
    /// A write whose length does not fit int32.
    TooLarge(&'static str),
    /// A reader that had to consume the whole parcel stopped short.
    TrailingData { pos: usize, len: usize },
}

impl fmt::Display for ParcelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OutOfData { pos, need } => {
                write!(f, "parcel: {need} bytes wanted at {pos}, data ends sooner")
            }
            Self::BadValue(what) => write!(f, "parcel: bad value: {what}"),
            Self::NotAnObject { pos } => write!(f, "parcel: no object at offset {pos}"),
            Self::BadObjectType { pos, ty } => write!(
                f,
                "parcel: object type {ty:#x} at offset {pos} is not accepted here"
            ),
            Self::UnexpectedNull(what) => write!(f, "parcel: unexpected null {what}"),
            Self::TooLarge(what) => write!(f, "parcel: {what} does not fit int32"),
            Self::TrailingData { pos, len } => {
                write!(f, "parcel: read stopped at {pos} of {len} bytes")
            }
        }
    }
}

impl std::error::Error for ParcelError {}

/// One `flat_binder_object` as it sits in the data, fields as read (the union is kept whole: `binder` is the node address for a binder, the handle in the low 4 bytes for a handle or fd).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct FlatObject {
    /// `hdr.type`.
    pub ty: u32,
    /// `flags` (scheduling inheritance bits, `FLAT_BINDER_FLAG_ACCEPTS_FDS`).
    pub flags: u32,
    /// The `binder` / `handle` union.
    pub binder: u64,
    /// `cookie`: the local `IBinder*` for a binder, ownership (1 = the parcel closes it) for an fd.
    pub cookie: u64,
}

/// How a process names a binder: a node of this process by its cookie, or a handle to a remote one.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum BinderKey {
    /// `BINDER_TYPE_BINDER`: the sender's own node, identified by the `IBinder*` it put in `cookie`.
    Local(u64),
    /// `BINDER_TYPE_HANDLE`: a reference by handle number.
    Handle(u32),
}

/// What `Parcel::writeStrongBinder` writes: the object, then the binder's stability word (`finishFlattenBinder`).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct StrongBinder {
    /// The inline object.
    pub obj: FlatObject,
    /// `Stability::getRepr`: 0 undeclared, or the declared level.
    pub stability: i32,
}

impl StrongBinder {
    /// The null binder: a `BINDER_TYPE_BINDER` with zero node and cookie, and the scheduling bits libbinder ORs into every flattened binder
    /// (`schedPolicyMask(SCHED_NORMAL = 0, priority 19)` = 19 = 0x13, Parcel.cpp flattenBinder), as libgui writes it.
    pub const NULL: StrongBinder = StrongBinder {
        obj: FlatObject {
            ty: BINDER_TYPE_BINDER,
            flags: 0x13,
            binder: 0,
            cookie: 0,
        },
        stability: 0,
    };

    /// libbinder reads a `BINDER_TYPE_BINDER` as `IBinder*` = cookie, so a zero cookie is null.
    pub fn is_null(&self) -> bool {
        self.obj.ty == BINDER_TYPE_BINDER && self.obj.cookie == 0
    }

    /// The identity of a non-null binder, `None` for null.
    pub fn key(&self) -> Option<BinderKey> {
        if self.is_null() {
            None
        } else if self.obj.ty == BINDER_TYPE_HANDLE {
            // the handle is the union's low 4 bytes
            Some(BinderKey::Handle(self.obj.binder as u32))
        } else {
            Some(BinderKey::Local(self.obj.cookie))
        }
    }
}

/// A file descriptor object: `fd` is a number valid in the process that reads the parcel (the driver translated it).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct FdObject {
    /// The descriptor number (`handle`, the union's low 4 bytes; the high 4 are zero).
    pub fd: i32,
    /// `flags`: 0 as `writeFileDescriptor` writes it.
    pub flags: u32,
    /// `cookie`: 1 when the parcel owns the descriptor, else 0.
    pub cookie: u64,
}

impl FdObject {
    /// An fd object as `Parcel::writeFileDescriptor(fd, takeOwnership)` makes it.
    pub fn new(fd: i32, take_ownership: bool) -> Self {
        Self {
            fd,
            flags: 0,
            cookie: u64::from(take_ownership),
        }
    }

    fn flat(&self) -> FlatObject {
        FlatObject {
            ty: BINDER_TYPE_FD,
            flags: self.flags,
            binder: u64::from(self.fd as u32),
            cookie: self.cookie,
        }
    }
}

/// A flattened object's payload and the fd objects that follow it (`Parcel::write(Flattenable)`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Flattened<'a> {
    /// The payload bytes, unpadded.
    pub payload: &'a [u8],
    /// The `fd_count` fd objects written after the payload.
    pub fds: Vec<FdObject>,
}

fn pad4(len: usize) -> usize {
    (len + 3) & !3
}

/// The data and the object offsets of one Parcel; also its writer (append only).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WireParcel {
    data: Vec<u8>,
    objects: Vec<usize>,
}

impl WireParcel {
    /// An empty parcel.
    pub fn new() -> Self {
        Self::default()
    }

    /// A parcel from transported bytes and object offsets: offsets must ascend and each hold a whole object inside the data.
    pub fn from_wire(data: Vec<u8>, objects: Vec<usize>) -> Result<Self, ParcelError> {
        let mut prev_end = 0usize;
        for &off in &objects {
            let end = off
                .checked_add(FLAT_OBJECT_SIZE)
                .ok_or(ParcelError::BadValue("object offset overflows"))?;
            if off < prev_end || end > data.len() {
                return Err(ParcelError::BadValue(
                    "object offsets must ascend and lie inside the data",
                ));
            }
            prev_end = end;
        }
        Ok(Self { data, objects })
    }

    /// The data bytes.
    pub fn data(&self) -> &[u8] {
        &self.data
    }

    /// The offsets of the inline objects, ascending.
    pub fn objects(&self) -> &[usize] {
        &self.objects
    }

    /// The pieces, for transport.
    pub fn into_parts(self) -> (Vec<u8>, Vec<usize>) {
        (self.data, self.objects)
    }

    /// A reader at the start.
    pub fn reader(&self) -> Reader<'_> {
        Reader {
            parcel: self,
            pos: 0,
        }
    }

    // ---- writers

    /// `writeInt32`.
    pub fn write_i32(&mut self, v: i32) {
        self.data.extend_from_slice(&v.to_le_bytes());
    }

    /// `writeUint32`.
    pub fn write_u32(&mut self, v: u32) {
        self.data.extend_from_slice(&v.to_le_bytes());
    }

    /// `writeInt64`.
    pub fn write_i64(&mut self, v: i64) {
        self.data.extend_from_slice(&v.to_le_bytes());
    }

    /// `writeUint64`.
    pub fn write_u64(&mut self, v: u64) {
        self.data.extend_from_slice(&v.to_le_bytes());
    }

    /// `writeFloat`.
    pub fn write_f32(&mut self, v: f32) {
        self.data.extend_from_slice(&v.to_le_bytes());
    }

    /// `writeBool`: an int32.
    pub fn write_bool(&mut self, v: bool) {
        self.write_i32(i32::from(v));
    }

    /// `writeByte`: an int32.
    pub fn write_byte(&mut self, v: i8) {
        self.write_i32(i32::from(v));
    }

    /// `write(void*, len)` / `writeInplace`: the bytes, then zeros up to the next multiple of 4.
    pub fn write_padded(&mut self, bytes: &[u8]) {
        self.data.extend_from_slice(bytes);
        self.data
            .resize(self.data.len() + pad4(bytes.len()) - bytes.len(), 0);
    }

    /// An int32 length that must fit.
    pub fn write_len(&mut self, len: usize, what: &'static str) -> Result<(), ParcelError> {
        self.write_i32(i32::try_from(len).map_err(|_| ParcelError::TooLarge(what))?);
        Ok(())
    }

    /// `writeUtf8AsUtf16`: UTF-16 unit count, the units and a NUL unit, padded.
    pub fn write_utf8_as_utf16(&mut self, s: &str) -> Result<(), ParcelError> {
        let units: Vec<u16> = s.encode_utf16().collect();
        self.write_len(units.len(), "string")?;
        let mut bytes = Vec::with_capacity((units.len() + 1) * 2);
        for u in units.iter().chain([&0u16]) {
            bytes.extend_from_slice(&u.to_le_bytes());
        }
        self.write_padded(&bytes);
        Ok(())
    }

    /// `writeInterfaceToken`: strict-mode word, work-source uid, 'SYST', then the descriptor as a String16 (same layout as [`Self::write_utf8_as_utf16`]).
    pub fn write_interface_token(&mut self, descriptor: &str) -> Result<(), ParcelError> {
        self.write_i32(STRICT_MODE_PENALTY_GATHER);
        self.write_i32(UNSET_WORK_SOURCE);
        self.write_i32(INTERFACE_HEADER);
        self.write_utf8_as_utf16(descriptor)
    }

    /// `Parcel::writeObject(obj, nullMetaData)`: the object's 24 bytes, and its offset in the object table unless it is a null-ish binder (`binder` word 0 and `nullMetaData` false),
    /// which libbinder leaves out of the table: the kernel has nothing to translate in it.
    fn write_object(&mut self, obj: &FlatObject, always_listed: bool) {
        if always_listed || obj.binder != 0 {
            self.objects.push(self.data.len());
        }
        self.write_u32(obj.ty);
        self.write_u32(obj.flags);
        self.write_u64(obj.binder);
        self.write_u64(obj.cookie);
    }

    /// `writeStrongBinder`: the object, then the stability word.
    pub fn write_strong_binder(&mut self, b: &StrongBinder) {
        self.write_object(&b.obj, false);
        self.write_i32(b.stability);
    }

    /// `writeFileDescriptor`: one fd object, always in the object table.
    pub fn write_fd(&mut self, fd: &FdObject) {
        self.write_object(&fd.flat(), true);
    }

    /// The transport form test fixtures use: data length (u32), the data, offset count (u32), the offsets (u32 each); all little-endian.
    pub fn to_dump(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(8 + self.data.len() + 4 * self.objects.len());
        out.extend_from_slice(&(self.data.len() as u32).to_le_bytes());
        out.extend_from_slice(&self.data);
        out.extend_from_slice(&(self.objects.len() as u32).to_le_bytes());
        for &o in &self.objects {
            out.extend_from_slice(&(o as u32).to_le_bytes());
        }
        out
    }

    /// The inverse of [`Self::to_dump`]; the dump must be consumed exactly.
    pub fn from_dump(dump: &[u8]) -> Result<Self, ParcelError> {
        let word = |at: usize| -> Result<usize, ParcelError> {
            let b = dump
                .get(at..at + 4)
                .ok_or(ParcelError::BadValue("dump ends inside a length"))?;
            Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize)
        };
        let len = word(0)?;
        let data = dump
            .get(4..4 + len)
            .ok_or(ParcelError::BadValue("dump data is cut short"))?
            .to_vec();
        let n = word(4 + len)?;
        if dump.len() != 8 + len + 4 * n {
            return Err(ParcelError::BadValue(
                "dump length disagrees with its counts",
            ));
        }
        let objects = (0..n)
            .map(|i| word(8 + len + 4 * i))
            .collect::<Result<Vec<_>, _>>()?;
        Self::from_wire(data, objects)
    }

    /// `write(Flattenable)`: payload size, fd count, the padded payload, then the fd objects.
    pub fn write_flattenable(
        &mut self,
        payload: &[u8],
        fds: &[FdObject],
    ) -> Result<(), ParcelError> {
        self.write_len(payload.len(), "flattenable")?;
        self.write_len(fds.len(), "flattenable fd count")?;
        self.write_padded(payload);
        for fd in fds {
            self.write_fd(fd);
        }
        Ok(())
    }

    /// `write(LightFlattenable)` of a variable-size type: payload size, then the padded payload.
    pub fn write_light_flattenable(&mut self, payload: &[u8]) -> Result<(), ParcelError> {
        self.write_len(payload.len(), "light flattenable")?;
        self.write_padded(payload);
        Ok(())
    }

    /// `writeParcelable` header: the non-null flag.
    pub fn write_parcelable_flag(&mut self) {
        self.write_i32(1);
    }

    /// Starts an AIDL parcelable body: a size placeholder; pass the result to [`Self::end_aidl_parcelable`].
    pub fn begin_aidl_parcelable(&mut self) -> usize {
        let start = self.data.len();
        self.write_i32(0);
        start
    }

    /// Fills the size placeholder `begin_aidl_parcelable` left with the bytes written since (the placeholder included).
    pub fn end_aidl_parcelable(&mut self, start: usize) -> Result<(), ParcelError> {
        let size = i32::try_from(self.data.len() - start)
            .map_err(|_| ParcelError::TooLarge("parcelable"))?;
        self.data[start..start + 4].copy_from_slice(&size.to_le_bytes());
        Ok(())
    }
}

/// A read cursor over a [`WireParcel`].
#[derive(Clone, Debug)]
pub struct Reader<'a> {
    parcel: &'a WireParcel,
    pos: usize,
}

impl<'a> Reader<'a> {
    /// The cursor position, in bytes.
    pub fn position(&self) -> usize {
        self.pos
    }

    /// Bytes left.
    pub fn remaining(&self) -> usize {
        self.parcel.data.len() - self.pos
    }

    /// Fails unless every byte was consumed (the truth tests require it).
    pub fn finish(&self) -> Result<(), ParcelError> {
        if self.pos == self.parcel.data.len() {
            Ok(())
        } else {
            Err(ParcelError::TrailingData {
                pos: self.pos,
                len: self.parcel.data.len(),
            })
        }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], ParcelError> {
        let end = self
            .pos
            .checked_add(n)
            .filter(|&e| e <= self.parcel.data.len())
            .ok_or(ParcelError::OutOfData {
                pos: self.pos,
                need: n,
            })?;
        let s = &self.parcel.data[self.pos..end];
        self.pos = end;
        Ok(s)
    }

    fn word<const N: usize>(&mut self) -> Result<[u8; N], ParcelError> {
        let mut a = [0u8; N];
        a.copy_from_slice(self.take(N)?);
        Ok(a)
    }

    /// `readInt32`.
    pub fn read_i32(&mut self) -> Result<i32, ParcelError> {
        Ok(i32::from_le_bytes(self.word()?))
    }

    /// `readUint32`.
    pub fn read_u32(&mut self) -> Result<u32, ParcelError> {
        Ok(u32::from_le_bytes(self.word()?))
    }

    /// `readInt64`.
    pub fn read_i64(&mut self) -> Result<i64, ParcelError> {
        Ok(i64::from_le_bytes(self.word()?))
    }

    /// `readUint64`.
    pub fn read_u64(&mut self) -> Result<u64, ParcelError> {
        Ok(u64::from_le_bytes(self.word()?))
    }

    /// `readFloat`.
    pub fn read_f32(&mut self) -> Result<f32, ParcelError> {
        Ok(f32::from_le_bytes(self.word()?))
    }

    /// `readBool`: any non-zero int32 is true.
    pub fn read_bool(&mut self) -> Result<bool, ParcelError> {
        Ok(self.read_i32()? != 0)
    }

    /// `readByte`: an int32 truncated to int8.
    pub fn read_byte(&mut self) -> Result<i8, ParcelError> {
        Ok(self.read_i32()? as i8)
    }

    /// An element count that must be non-negative and cannot exceed the bytes left (`SAFE_PARCEL_READ_SIZE` bounds it by the data size; each element here takes at least one byte).
    pub fn read_count(&mut self, what: &'static str) -> Result<usize, ParcelError> {
        let n = self.read_i32()?;
        let n = usize::try_from(n).map_err(|_| ParcelError::BadValue(what))?;
        if n > self.remaining() {
            return Err(ParcelError::BadValue(what));
        }
        Ok(n)
    }

    /// `len` bytes then the padding up to a multiple of 4 (padding content is not checked, as libbinder does not).
    pub fn read_padded(&mut self, len: usize) -> Result<&'a [u8], ParcelError> {
        let padded = pad4(len);
        let all = self.take(padded)?;
        Ok(&all[..len])
    }

    /// `readUtf8FromUtf16` of a non-null string.
    pub fn read_utf8_from_utf16(&mut self) -> Result<String, ParcelError> {
        let units = usize::try_from(self.read_i32()?)
            .map_err(|_| ParcelError::BadValue("null or negative string length"))?;
        let nbytes = units
            .checked_add(1)
            .and_then(|n| n.checked_mul(2))
            .ok_or(ParcelError::BadValue("string length"))?;
        let bytes = self.read_padded(nbytes)?;
        if bytes[nbytes - 2..] != [0, 0] {
            return Err(ParcelError::BadValue("string lacks its NUL terminator"));
        }
        // nbytes is even, so no remainder
        let units: Vec<u16> = bytes[..nbytes - 2]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_le_bytes(*c))
            .collect();
        String::from_utf16(&units).map_err(|_| ParcelError::BadValue("string is not valid UTF-16"))
    }

    /// `enforceInterface`: the three header words, then the descriptor, which must equal `expected`.
    pub fn enforce_interface(&mut self, expected: &str) -> Result<(), ParcelError> {
        let _strict_mode = self.read_i32()?;
        let _work_source = self.read_i32()?;
        if self.read_i32()? != INTERFACE_HEADER {
            return Err(ParcelError::BadValue("interface token header is not SYST"));
        }
        if self.read_utf8_from_utf16()? != expected {
            return Err(ParcelError::BadValue("interface descriptor differs"));
        }
        Ok(())
    }

    /// `Parcel::readObject(nullMetaData)`: the 24 bytes at the cursor, which must be in the object table; with `unlisted_null_ok` (false for fds) an object whose binder and cookie words are both 0 passes
    /// without being listed, as `writeObject(obj, false)` leaves it out of the table.
    fn read_object(&mut self, unlisted_null_ok: bool) -> Result<FlatObject, ParcelError> {
        let pos = self.pos;
        let listed = self.parcel.objects.binary_search(&pos).is_ok();
        let ty = self.read_u32()?;
        let flags = self.read_u32()?;
        let binder = self.read_u64()?;
        let cookie = self.read_u64()?;
        if !listed && !(unlisted_null_ok && binder == 0 && cookie == 0) {
            self.pos = pos;
            return Err(ParcelError::NotAnObject { pos });
        }
        Ok(FlatObject {
            ty,
            flags,
            binder,
            cookie,
        })
    }

    /// `readStrongBinder` (`allow_null` false) or `readNullableStrongBinder`: a binder or handle object and the stability word.
    pub fn read_strong_binder(
        &mut self,
        allow_null: bool,
        what: &'static str,
    ) -> Result<StrongBinder, ParcelError> {
        let pos = self.pos;
        let obj = self.read_object(true)?;
        if obj.ty != BINDER_TYPE_BINDER && obj.ty != BINDER_TYPE_HANDLE {
            return Err(ParcelError::BadObjectType { pos, ty: obj.ty });
        }
        let stability = self.read_i32()?;
        let b = StrongBinder { obj, stability };
        if !allow_null && b.is_null() {
            return Err(ParcelError::UnexpectedNull(what));
        }
        Ok(b)
    }

    /// `readFileDescriptor`: an fd object.
    pub fn read_fd(&mut self) -> Result<FdObject, ParcelError> {
        let pos = self.pos;
        let obj = self.read_object(false)?;
        if obj.ty != BINDER_TYPE_FD {
            return Err(ParcelError::BadObjectType { pos, ty: obj.ty });
        }
        if obj.binder >> 32 != 0 {
            return Err(ParcelError::BadValue("fd object has bits above the handle"));
        }
        Ok(FdObject {
            fd: obj.binder as u32 as i32,
            flags: obj.flags,
            cookie: obj.cookie,
        })
    }

    /// `read(Flattenable)`: payload size, fd count, the payload, the fd objects.
    pub fn read_flattenable(&mut self) -> Result<Flattened<'a>, ParcelError> {
        let len = usize::try_from(self.read_i32()?)
            .map_err(|_| ParcelError::BadValue("flattenable size"))?;
        let nfds = usize::try_from(self.read_i32()?)
            .map_err(|_| ParcelError::BadValue("flattenable fd count"))?;
        if nfds > self.remaining() / FLAT_OBJECT_SIZE {
            return Err(ParcelError::BadValue("flattenable fd count"));
        }
        let payload = self.read_padded(len)?;
        let fds = (0..nfds)
            .map(|_| self.read_fd())
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Flattened { payload, fds })
    }

    /// `read(LightFlattenable)` of a variable-size type: payload size, then the payload.
    pub fn read_light_flattenable(&mut self) -> Result<&'a [u8], ParcelError> {
        let len = usize::try_from(self.read_i32()?)
            .map_err(|_| ParcelError::BadValue("light flattenable size"))?;
        self.read_padded(len)
    }

    /// `readParcelable`'s header: the non-null flag; 0 (a null parcelable) is refused by every field here.
    pub fn read_parcelable_flag(&mut self, what: &'static str) -> Result<(), ParcelError> {
        if self.read_i32()? == 0 {
            return Err(ParcelError::UnexpectedNull(what));
        }
        Ok(())
    }

    /// Opens an AIDL parcelable body: its size; returns the position of the body's end. Fields are read while the cursor is before it ([`Self::more_before`]), then [`Self::skip_to`] it.
    pub fn begin_aidl_parcelable(&mut self) -> Result<usize, ParcelError> {
        let start = self.pos;
        let size = usize::try_from(self.read_i32()?)
            .map_err(|_| ParcelError::BadValue("parcelable size"))?;
        if size < 4 {
            return Err(ParcelError::BadValue(
                "parcelable size below its own header",
            ));
        }
        let end = start
            .checked_add(size)
            .filter(|&e| e <= self.parcel.data.len())
            .ok_or(ParcelError::BadValue("parcelable size exceeds the data"))?;
        Ok(end)
    }

    /// Whether an AIDL body that ends at `end` still has fields (AIDL's forward-compatible reader stops at the declared end).
    pub fn more_before(&self, end: usize) -> bool {
        self.pos < end
    }

    /// Moves to `end`, where the AIDL parcelable's declared size says the body stops; fails if fields ran past it.
    pub fn skip_to(&mut self, end: usize) -> Result<(), ParcelError> {
        if self.pos > end {
            return Err(ParcelError::BadValue(
                "parcelable fields ran past the declared size",
            ));
        }
        self.pos = end;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Values derived from the wire rules (little-endian words, pad to 4), not from the writer.
    #[test]
    fn primitives_layout() {
        let mut p = WireParcel::new();
        p.write_i32(-2);
        p.write_u64(0x0102_0304_0506_0708);
        p.write_bool(true);
        p.write_byte(-1);
        p.write_f32(1.0);
        assert_eq!(
            p.data(),
            [
                0xfe, 0xff, 0xff, 0xff, // -2
                8, 7, 6, 5, 4, 3, 2, 1, // u64 little-endian
                1, 0, 0, 0, // bool is an int32
                0xff, 0xff, 0xff, 0xff, // int8 -1 widened to int32
                0, 0, 0x80, 0x3f, // 1.0f
            ]
        );
    }

    /// "ab": 2 units, units + NUL unit = 6 bytes, padded to 8.
    #[test]
    fn utf16_string_layout() {
        let mut p = WireParcel::new();
        p.write_utf8_as_utf16("ab").unwrap();
        assert_eq!(p.data(), [2, 0, 0, 0, b'a', 0, b'b', 0, 0, 0, 0, 0]);
        assert_eq!(p.reader().read_utf8_from_utf16().unwrap(), "ab");
        let mut e = WireParcel::new();
        e.write_utf8_as_utf16("").unwrap();
        assert_eq!(
            e.data(),
            [0, 0, 0, 0, 0, 0, 0, 0],
            "empty: length 0, one NUL unit padded to 4"
        );
    }

    /// A non-BMP character is two UTF-16 units.
    #[test]
    fn utf16_surrogates() {
        let mut p = WireParcel::new();
        p.write_utf8_as_utf16("\u{1f600}").unwrap();
        assert_eq!(&p.data()[..4], [2, 0, 0, 0]);
        assert_eq!(p.reader().read_utf8_from_utf16().unwrap(), "\u{1f600}");
    }

    #[test]
    fn interface_token_layout() {
        let mut p = WireParcel::new();
        p.write_interface_token("a").unwrap();
        assert_eq!(
            p.data(),
            [
                0, 0, 0, 0x80, // strict mode: PENALTY_GATHER
                0xff, 0xff, 0xff, 0xff, // work source unset
                b'T', b'S', b'Y', b'S', // 'SYST' as an int32 0x53595354
                1, 0, 0, 0, b'a', 0, 0, 0, // "a" + NUL unit, padded
            ]
        );
        let mut r = p.reader();
        r.enforce_interface("a").unwrap();
        r.finish().unwrap();
        assert!(p.reader().enforce_interface("b").is_err());
    }

    #[test]
    fn strong_binder_and_fd_objects() {
        let mut p = WireParcel::new();
        p.write_u32(7);
        let b = StrongBinder {
            obj: FlatObject {
                ty: BINDER_TYPE_HANDLE,
                flags: 0x7f,
                binder: 5,
                cookie: 0,
            },
            stability: 12,
        };
        p.write_strong_binder(&b);
        p.write_fd(&FdObject::new(9, true));
        assert_eq!(p.objects(), [4, 4 + 24 + 4]);
        // the first object: type, flags, binder/handle union, cookie, then the stability word
        assert_eq!(&p.data()[4..8], 0x7368_2a85u32.to_le_bytes());
        assert_eq!(&p.data()[8..12], 0x7fu32.to_le_bytes());
        assert_eq!(&p.data()[12..20], 5u64.to_le_bytes());
        assert_eq!(&p.data()[28..32], 12i32.to_le_bytes());
        let mut r = p.reader();
        assert_eq!(r.read_u32().unwrap(), 7);
        let got = r.read_strong_binder(false, "t").unwrap();
        assert_eq!(got, b);
        assert_eq!(got.key(), Some(BinderKey::Handle(5)));
        assert_eq!(r.read_fd().unwrap(), FdObject::new(9, true));
        r.finish().unwrap();
        // a read where no object sits is refused, not reinterpreted
        let mut r2 = p.reader();
        assert!(matches!(
            r2.read_strong_binder(true, "t"),
            Err(ParcelError::NotAnObject { pos: 0 })
        ));
    }

    /// libbinder lists an object only when its binder word is non-zero (null binders, handle 0) or it is an fd; a reader accepts the unlisted zero object, and refuses an unlisted live one.
    #[test]
    fn null_binder_is_not_listed_but_readable() {
        let mut p = WireParcel::new();
        p.write_strong_binder(&StrongBinder::NULL);
        assert!(p.objects().is_empty());
        let handle0 = StrongBinder {
            obj: FlatObject {
                ty: BINDER_TYPE_HANDLE,
                flags: 0,
                binder: 0,
                cookie: 0,
            },
            stability: 0,
        };
        p.write_strong_binder(&handle0);
        assert!(
            p.objects().is_empty(),
            "handle 0 has a zero binder word too"
        );
        let mut r = p.reader();
        assert!(r.read_strong_binder(true, "n").unwrap().is_null());
        assert_eq!(
            r.read_strong_binder(false, "h0").unwrap().key(),
            Some(BinderKey::Handle(0)),
            "handle 0 is the context object, not null"
        );
        // a live object cut out of the table is refused
        let mut live = WireParcel::new();
        live.write_strong_binder(&StrongBinder {
            obj: FlatObject {
                ty: BINDER_TYPE_BINDER,
                flags: 0,
                binder: 9,
                cookie: 8,
            },
            stability: 0,
        });
        let (data, _) = live.into_parts();
        let unlisted = WireParcel::from_wire(data, vec![]).unwrap();
        assert!(matches!(
            unlisted.reader().read_strong_binder(true, "x"),
            Err(ParcelError::NotAnObject { pos: 0 })
        ));
        // an fd is always listed and never read unlisted
        let mut f = WireParcel::new();
        f.write_fd(&FdObject::new(0, false));
        assert_eq!(f.objects(), [0]);
    }

    #[test]
    fn dump_round_trip() {
        let mut p = WireParcel::new();
        p.write_i32(3);
        p.write_fd(&FdObject::new(5, true));
        p.write_utf8_as_utf16("x").unwrap();
        let d = p.to_dump();
        assert_eq!(WireParcel::from_dump(&d).unwrap(), p);
        assert!(WireParcel::from_dump(&d[..d.len() - 1]).is_err());
        let mut longer = d.clone();
        longer.push(0);
        assert!(WireParcel::from_dump(&longer).is_err());
    }

    #[test]
    fn null_binder_is_refused_unless_allowed() {
        let mut p = WireParcel::new();
        p.write_strong_binder(&StrongBinder::NULL);
        assert_eq!(
            p.reader().read_strong_binder(true, "x").unwrap(),
            StrongBinder::NULL
        );
        assert_eq!(
            p.reader().read_strong_binder(false, "x"),
            Err(ParcelError::UnexpectedNull("x"))
        );
    }

    /// size, fd count, payload padded, then the fd objects.
    #[test]
    fn flattenable_layout() {
        let mut p = WireParcel::new();
        p.write_flattenable(&[1, 2, 3], &[FdObject::new(4, false)])
            .unwrap();
        assert_eq!(&p.data()[..12], [3, 0, 0, 0, 1, 0, 0, 0, 1, 2, 3, 0]);
        assert_eq!(p.objects(), [12]);
        let mut r = p.reader();
        let f = r.read_flattenable().unwrap();
        assert_eq!(f.payload, [1, 2, 3]);
        assert_eq!(f.fds, [FdObject::new(4, false)]);
        r.finish().unwrap();
    }

    #[test]
    fn aidl_parcelable_size_counts_its_own_header() {
        let mut p = WireParcel::new();
        let s = p.begin_aidl_parcelable();
        p.write_i64(5);
        p.end_aidl_parcelable(s).unwrap();
        assert_eq!(&p.data()[..4], 12i32.to_le_bytes(), "4 header + 8 payload");
        let mut r = p.reader();
        let end = r.begin_aidl_parcelable().unwrap();
        assert_eq!(r.read_i64().unwrap(), 5);
        assert!(!r.more_before(end));
        r.skip_to(end).unwrap();
        r.finish().unwrap();
    }

    #[test]
    fn reader_refuses_reading_past_the_end_and_bad_counts() {
        let mut p = WireParcel::new();
        p.write_i32(1000);
        assert!(matches!(
            p.reader().read_i64(),
            Err(ParcelError::OutOfData { .. })
        ));
        assert!(matches!(
            p.reader().read_count("n"),
            Err(ParcelError::BadValue("n"))
        ));
        let mut neg = WireParcel::new();
        neg.write_i32(-1);
        assert!(neg.reader().read_count("n").is_err());
    }

    #[test]
    fn from_wire_validates_object_offsets() {
        assert!(WireParcel::from_wire(vec![0; 23], vec![0]).is_err());
        assert!(
            WireParcel::from_wire(vec![0; 48], vec![0, 8]).is_err(),
            "overlapping objects"
        );
        assert!(WireParcel::from_wire(vec![0; 48], vec![0, 24]).is_ok());
    }
}
