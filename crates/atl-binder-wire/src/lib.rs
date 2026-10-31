// SPDX-License-Identifier: MIT
//! Binder kernel uapi for host processes: the generated layouts and codes (`uapi`, from bionic's
//! `linux/android/binder.h` by atl-bridgegen, layout-asserted for the target), and safe codecs over
//! byte buffers for the BC command and BR return streams and the flat objects inside transaction data.
//! Every offset and size comes from the generated types; values are native-endian, as the ioctl ABI.
//! Hosts are 64-bit only (arches.toml), so `binder_uintptr_t`/`binder_size_t` are `u64`; the typed
//! constants below fail to compile otherwise.
#![forbid(unsafe_code)]

/// The generated uapi: `#[repr(C)]` records with rustc-vs-clang layout asserts, and every enumerator.
#[allow(
    non_upper_case_globals,
    clippy::all,
    reason = "generated from the C header, names and shapes are the header's"
)]
pub mod uapi {
    include!(concat!(env!("OUT_DIR"), "/binder.rs"));
}

use core::mem::{offset_of, size_of};

use uapi::*;

/// `binder_uintptr_t`: a user address (here: a region offset or an object pointer, opaque to binderd).
pub type Uptr = u64;
/// `binder_size_t`.
pub type Size = u64;

// Typed construction compiles only if every field has exactly the type the codecs below assume.
const ZP: Uptr = 0;
const ZS: Size = 0;
const _: binder_ptr_cookie = binder_ptr_cookie {
    ptr: ZP,
    cookie: ZP,
};
const _: binder_handle_cookie = binder_handle_cookie {
    handle: 0u32,
    cookie: ZP,
};
const _: binder_transaction_data = binder_transaction_data {
    target: binder_transaction_data_target { ptr: ZP },
    cookie: ZP,
    code: 0u32,
    flags: 0u32,
    sender_pid: 0i32,
    sender_euid: 0u32,
    data_size: ZS,
    offsets_size: ZS,
    data: binder_transaction_data_data {
        ptr: binder_transaction_data_data_ptr {
            buffer: ZP,
            offsets: ZP,
        },
    },
};
const _: flat_binder_object = flat_binder_object {
    hdr: binder_object_header { r#type: 0u32 },
    flags: 0u32,
    anon0: flat_binder_object_anon0 { binder: ZP },
    cookie: ZP,
};
const _: binder_fd_object = binder_fd_object {
    hdr: binder_object_header { r#type: 0u32 },
    pad_flags: 0u32,
    anon0: binder_fd_object_anon0 { pad_binder: ZP },
    cookie: ZP,
};

/// Payload size encoded in an ioctl-style code (`_IOC_SIZE`): what a stream walker skips for a command
/// it does not understand.
pub const fn ioc_size(code: u32) -> usize {
    ((code >> _IOC_SIZESHIFT) & _IOC_SIZEMASK as u32) as usize
}

// The codes embed their payload sizes; the codecs below rely on them matching the records.
const _: () = assert!(ioc_size(BC_TRANSACTION) == size_of::<binder_transaction_data>());
const _: () = assert!(ioc_size(BC_REPLY) == size_of::<binder_transaction_data>());
const _: () = assert!(ioc_size(BR_TRANSACTION) == size_of::<binder_transaction_data>());
const _: () = assert!(ioc_size(BR_REPLY) == size_of::<binder_transaction_data>());
const _: () = assert!(ioc_size(BC_INCREFS_DONE) == size_of::<binder_ptr_cookie>());
const _: () = assert!(ioc_size(BR_INCREFS) == size_of::<binder_ptr_cookie>());
const _: () = assert!(ioc_size(BC_REQUEST_DEATH_NOTIFICATION) == size_of::<binder_handle_cookie>());
const _: () = assert!(ioc_size(BC_FREE_BUFFER) == size_of::<Uptr>());
const _: () = assert!(ioc_size(BC_ACQUIRE) == size_of::<u32>());
const _: () = assert!(ioc_size(BR_DEAD_BINDER) == size_of::<Uptr>());
const _: () = assert!(ioc_size(BR_ERROR) == size_of::<i32>());

/// Size of one command/return code word.
pub const CODE_SIZE: usize = size_of::<u32>();

/// Largest single BR item binderd emits: code + `binder_transaction_data_secctx`. The kernel stops
/// filling a read buffer once less than this is left (`binder_thread_read`, `sizeof(tr) + 4`).
pub const MAX_RETURN_SIZE: usize = CODE_SIZE + size_of::<binder_transaction_data_secctx>();

/// A malformed stream or object.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireError {
    /// The stream ends inside the item starting at this byte.
    Truncated { at: usize },
}

fn rd<const N: usize>(b: &[u8], off: usize) -> [u8; N] {
    b[off..off + N]
        .try_into()
        .expect("caller checked the length")
}
fn rd_u32(b: &[u8], off: usize) -> u32 {
    u32::from_ne_bytes(rd(b, off))
}
fn rd_u64(b: &[u8], off: usize) -> u64 {
    u64::from_ne_bytes(rd(b, off))
}
fn wr(b: &mut [u8], off: usize, v: &[u8]) {
    b[off..off + v.len()].copy_from_slice(v);
}

/// `binder_ptr_cookie`: a node as its owner knows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PtrCookie {
    pub ptr: Uptr,
    pub cookie: Uptr,
}

impl PtrCookie {
    fn decode(b: &[u8]) -> Self {
        Self {
            ptr: rd_u64(b, offset_of!(binder_ptr_cookie, ptr)),
            cookie: rd_u64(b, offset_of!(binder_ptr_cookie, cookie)),
        }
    }
    fn encode(&self, out: &mut Vec<u8>) {
        let mut b = [0u8; size_of::<binder_ptr_cookie>()];
        wr(
            &mut b,
            offset_of!(binder_ptr_cookie, ptr),
            &self.ptr.to_ne_bytes(),
        );
        wr(
            &mut b,
            offset_of!(binder_ptr_cookie, cookie),
            &self.cookie.to_ne_bytes(),
        );
        out.extend_from_slice(&b);
    }
}

/// `binder_handle_cookie` (packed in the uapi).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HandleCookie {
    pub handle: u32,
    pub cookie: Uptr,
}

impl HandleCookie {
    fn decode(b: &[u8]) -> Self {
        Self {
            handle: rd_u32(b, offset_of!(binder_handle_cookie, handle)),
            cookie: rd_u64(b, offset_of!(binder_handle_cookie, cookie)),
        }
    }
    fn encode(&self, out: &mut Vec<u8>) {
        let mut b = [0u8; size_of::<binder_handle_cookie>()];
        wr(
            &mut b,
            offset_of!(binder_handle_cookie, handle),
            &self.handle.to_ne_bytes(),
        );
        wr(
            &mut b,
            offset_of!(binder_handle_cookie, cookie),
            &self.cookie.to_ne_bytes(),
        );
        out.extend_from_slice(&b);
    }
}

/// `binder_transaction_data`. `target` is the union read as the side that wrote it means it: a handle
/// in `BC_TRANSACTION`, a node pointer in `BR_TRANSACTION` (`TransactionData::target_handle`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TransactionData {
    /// `target.ptr`; for a handle use `target_handle` / `with_handle`.
    pub target: Uptr,
    pub cookie: Uptr,
    pub code: u32,
    pub flags: u32,
    pub sender_pid: i32,
    pub sender_euid: u32,
    pub data_size: Size,
    pub offsets_size: Size,
    /// `data.ptr.buffer`.
    pub buffer: Uptr,
    /// `data.ptr.offsets`.
    pub offsets: Uptr,
}

const TXN_SIZE: usize = size_of::<binder_transaction_data>();
const TARGET_HANDLE: usize = offset_of!(binder_transaction_data, target)
    + offset_of!(binder_transaction_data_target, handle);
const TARGET_PTR: usize =
    offset_of!(binder_transaction_data, target) + offset_of!(binder_transaction_data_target, ptr);
const DATA_BUFFER: usize = offset_of!(binder_transaction_data, data)
    + offset_of!(binder_transaction_data_data, ptr)
    + offset_of!(binder_transaction_data_data_ptr, buffer);
const DATA_OFFSETS: usize = offset_of!(binder_transaction_data, data)
    + offset_of!(binder_transaction_data_data, ptr)
    + offset_of!(binder_transaction_data_data_ptr, offsets);

impl TransactionData {
    /// The `target.handle` member of the union.
    pub fn target_handle(&self) -> u32 {
        let mut b = [0u8; TXN_SIZE];
        self.encode_into(&mut b);
        rd_u32(&b, TARGET_HANDLE)
    }

    /// `self` with `target.handle = handle` (the rest of the union zero).
    pub fn with_handle(mut self, handle: u32) -> Self {
        let mut b = [0u8; TXN_SIZE];
        wr(&mut b, TARGET_HANDLE, &handle.to_ne_bytes());
        self.target = rd_u64(&b, TARGET_PTR);
        self
    }

    fn decode(b: &[u8]) -> Self {
        Self {
            target: rd_u64(b, TARGET_PTR),
            cookie: rd_u64(b, offset_of!(binder_transaction_data, cookie)),
            code: rd_u32(b, offset_of!(binder_transaction_data, code)),
            flags: rd_u32(b, offset_of!(binder_transaction_data, flags)),
            sender_pid: i32::from_ne_bytes(rd(b, offset_of!(binder_transaction_data, sender_pid))),
            sender_euid: rd_u32(b, offset_of!(binder_transaction_data, sender_euid)),
            data_size: rd_u64(b, offset_of!(binder_transaction_data, data_size)),
            offsets_size: rd_u64(b, offset_of!(binder_transaction_data, offsets_size)),
            buffer: rd_u64(b, DATA_BUFFER),
            offsets: rd_u64(b, DATA_OFFSETS),
        }
    }

    fn encode_into(&self, b: &mut [u8; TXN_SIZE]) {
        wr(b, TARGET_PTR, &self.target.to_ne_bytes());
        wr(
            b,
            offset_of!(binder_transaction_data, cookie),
            &self.cookie.to_ne_bytes(),
        );
        wr(
            b,
            offset_of!(binder_transaction_data, code),
            &self.code.to_ne_bytes(),
        );
        wr(
            b,
            offset_of!(binder_transaction_data, flags),
            &self.flags.to_ne_bytes(),
        );
        wr(
            b,
            offset_of!(binder_transaction_data, sender_pid),
            &self.sender_pid.to_ne_bytes(),
        );
        wr(
            b,
            offset_of!(binder_transaction_data, sender_euid),
            &self.sender_euid.to_ne_bytes(),
        );
        wr(
            b,
            offset_of!(binder_transaction_data, data_size),
            &self.data_size.to_ne_bytes(),
        );
        wr(
            b,
            offset_of!(binder_transaction_data, offsets_size),
            &self.offsets_size.to_ne_bytes(),
        );
        wr(b, DATA_BUFFER, &self.buffer.to_ne_bytes());
        wr(b, DATA_OFFSETS, &self.offsets.to_ne_bytes());
    }

    fn encode(&self, out: &mut Vec<u8>) {
        let mut b = [0u8; TXN_SIZE];
        self.encode_into(&mut b);
        out.extend_from_slice(&b);
    }
}

/// One `BC_*` command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    Transaction(TransactionData),
    Reply(TransactionData),
    FreeBuffer(Uptr),
    IncRefs(u32),
    Acquire(u32),
    Release(u32),
    DecRefs(u32),
    IncRefsDone(PtrCookie),
    AcquireDone(PtrCookie),
    RegisterLooper,
    EnterLooper,
    ExitLooper,
    RequestDeathNotification(HandleCookie),
    ClearDeathNotification(HandleCookie),
    DeadBinderDone(Uptr),
    /// A uapi command binderd does not implement (scatter-gather, attempt-acquire, freeze
    /// notifications); payload skipped by its `_IOC_SIZE`.
    Unsupported(u32),
    /// Not a uapi command.
    Unknown(u32),
}

/// The command at `stream[at..]` and where the next one starts. An unknown code decodes without its
/// payload (its size is not trusted); the caller stops there as the kernel does (`-EINVAL`).
pub fn decode_command(stream: &[u8], at: usize) -> Result<(Command, usize), WireError> {
    let truncated = WireError::Truncated { at };
    let body = at + CODE_SIZE;
    if stream.len() < body {
        return Err(truncated);
    }
    let code = rd_u32(stream, at);
    let known = matches!(
        code,
        BC_TRANSACTION
            | BC_REPLY
            | BC_ACQUIRE_RESULT
            | BC_FREE_BUFFER
            | BC_INCREFS
            | BC_ACQUIRE
            | BC_RELEASE
            | BC_DECREFS
            | BC_INCREFS_DONE
            | BC_ACQUIRE_DONE
            | BC_ATTEMPT_ACQUIRE
            | BC_REGISTER_LOOPER
            | BC_ENTER_LOOPER
            | BC_EXIT_LOOPER
            | BC_REQUEST_DEATH_NOTIFICATION
            | BC_CLEAR_DEATH_NOTIFICATION
            | BC_DEAD_BINDER_DONE
            | BC_TRANSACTION_SG
            | BC_REPLY_SG
            | BC_REQUEST_FREEZE_NOTIFICATION
            | BC_CLEAR_FREEZE_NOTIFICATION
            | BC_FREEZE_NOTIFICATION_DONE
    );
    if !known {
        return Ok((Command::Unknown(code), body));
    }
    let end = body + ioc_size(code);
    if stream.len() < end {
        return Err(truncated);
    }
    let p = &stream[body..end];
    let cmd = match code {
        BC_TRANSACTION => Command::Transaction(TransactionData::decode(p)),
        BC_REPLY => Command::Reply(TransactionData::decode(p)),
        BC_FREE_BUFFER => Command::FreeBuffer(rd_u64(p, 0)),
        BC_INCREFS => Command::IncRefs(rd_u32(p, 0)),
        BC_ACQUIRE => Command::Acquire(rd_u32(p, 0)),
        BC_RELEASE => Command::Release(rd_u32(p, 0)),
        BC_DECREFS => Command::DecRefs(rd_u32(p, 0)),
        BC_INCREFS_DONE => Command::IncRefsDone(PtrCookie::decode(p)),
        BC_ACQUIRE_DONE => Command::AcquireDone(PtrCookie::decode(p)),
        BC_REGISTER_LOOPER => Command::RegisterLooper,
        BC_ENTER_LOOPER => Command::EnterLooper,
        BC_EXIT_LOOPER => Command::ExitLooper,
        BC_REQUEST_DEATH_NOTIFICATION => Command::RequestDeathNotification(HandleCookie::decode(p)),
        BC_CLEAR_DEATH_NOTIFICATION => Command::ClearDeathNotification(HandleCookie::decode(p)),
        BC_DEAD_BINDER_DONE => Command::DeadBinderDone(rd_u64(p, 0)),
        other => Command::Unsupported(other),
    };
    Ok((cmd, end))
}

/// Builds a `BC_*` stream (tests, the client shim, in-process clients).
#[derive(Debug, Default, Clone)]
pub struct CommandWriter(pub Vec<u8>);

impl CommandWriter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, cmd: &Command) -> &mut Self {
        let out = &mut self.0;
        let code = |out: &mut Vec<u8>, c: u32| out.extend_from_slice(&c.to_ne_bytes());
        match cmd {
            Command::Transaction(t) => {
                code(out, BC_TRANSACTION);
                t.encode(out);
            }
            Command::Reply(t) => {
                code(out, BC_REPLY);
                t.encode(out);
            }
            Command::FreeBuffer(p) => {
                code(out, BC_FREE_BUFFER);
                out.extend_from_slice(&p.to_ne_bytes());
            }
            Command::IncRefs(h)
            | Command::Acquire(h)
            | Command::Release(h)
            | Command::DecRefs(h) => {
                code(
                    out,
                    match cmd {
                        Command::IncRefs(_) => BC_INCREFS,
                        Command::Acquire(_) => BC_ACQUIRE,
                        Command::Release(_) => BC_RELEASE,
                        _ => BC_DECREFS,
                    },
                );
                out.extend_from_slice(&h.to_ne_bytes());
            }
            Command::IncRefsDone(pc) => {
                code(out, BC_INCREFS_DONE);
                pc.encode(out);
            }
            Command::AcquireDone(pc) => {
                code(out, BC_ACQUIRE_DONE);
                pc.encode(out);
            }
            Command::RegisterLooper => code(out, BC_REGISTER_LOOPER),
            Command::EnterLooper => code(out, BC_ENTER_LOOPER),
            Command::ExitLooper => code(out, BC_EXIT_LOOPER),
            Command::RequestDeathNotification(hc) => {
                code(out, BC_REQUEST_DEATH_NOTIFICATION);
                hc.encode(out);
            }
            Command::ClearDeathNotification(hc) => {
                code(out, BC_CLEAR_DEATH_NOTIFICATION);
                hc.encode(out);
            }
            Command::DeadBinderDone(c) => {
                code(out, BC_DEAD_BINDER_DONE);
                out.extend_from_slice(&c.to_ne_bytes());
            }
            // the raw code with a zero payload of its encoded size
            Command::Unsupported(c) | Command::Unknown(c) => {
                code(out, *c);
                out.resize(out.len() + ioc_size(*c), 0);
            }
        }
        self
    }
}

/// One `BR_*` return.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Return {
    Error(i32),
    Ok,
    Transaction(TransactionData),
    Reply(TransactionData),
    DeadReply,
    TransactionComplete,
    IncRefs(PtrCookie),
    Acquire(PtrCookie),
    Release(PtrCookie),
    DecRefs(PtrCookie),
    Noop,
    SpawnLooper,
    Finished,
    DeadBinder(Uptr),
    ClearDeathNotificationDone(Uptr),
    FailedReply,
    /// Any other uapi return code; payload skipped.
    Other(u32),
}

impl Return {
    /// Encoded size: code + payload.
    pub fn len(&self) -> usize {
        CODE_SIZE + ioc_size(self.code())
    }

    /// Never empty: every return carries its code.
    pub fn is_empty(&self) -> bool {
        false
    }

    pub fn code(&self) -> u32 {
        match self {
            Return::Error(_) => BR_ERROR,
            Return::Ok => BR_OK,
            Return::Transaction(_) => BR_TRANSACTION,
            Return::Reply(_) => BR_REPLY,
            Return::DeadReply => BR_DEAD_REPLY,
            Return::TransactionComplete => BR_TRANSACTION_COMPLETE,
            Return::IncRefs(_) => BR_INCREFS,
            Return::Acquire(_) => BR_ACQUIRE,
            Return::Release(_) => BR_RELEASE,
            Return::DecRefs(_) => BR_DECREFS,
            Return::Noop => BR_NOOP,
            Return::SpawnLooper => BR_SPAWN_LOOPER,
            Return::Finished => BR_FINISHED,
            Return::DeadBinder(_) => BR_DEAD_BINDER,
            Return::ClearDeathNotificationDone(_) => BR_CLEAR_DEATH_NOTIFICATION_DONE,
            Return::FailedReply => BR_FAILED_REPLY,
            Return::Other(c) => *c,
        }
    }

    pub fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.code().to_ne_bytes());
        match self {
            Return::Error(e) => out.extend_from_slice(&e.to_ne_bytes()),
            Return::Transaction(t) | Return::Reply(t) => t.encode(out),
            Return::IncRefs(pc)
            | Return::Acquire(pc)
            | Return::Release(pc)
            | Return::DecRefs(pc) => pc.encode(out),
            Return::DeadBinder(c) | Return::ClearDeathNotificationDone(c) => {
                out.extend_from_slice(&c.to_ne_bytes())
            }
            Return::Other(c) => out.resize(out.len() + ioc_size(*c), 0),
            _ => {}
        }
    }
}

/// Every return in a `BR_*` stream, in order.
pub fn decode_returns(stream: &[u8]) -> Result<Vec<Return>, WireError> {
    let mut out = Vec::new();
    let mut at = 0;
    while at < stream.len() {
        let truncated = WireError::Truncated { at };
        if stream.len() < at + CODE_SIZE {
            return Err(truncated);
        }
        let code = rd_u32(stream, at);
        let body = at + CODE_SIZE;
        let end = body + ioc_size(code);
        if stream.len() < end {
            return Err(truncated);
        }
        let p = &stream[body..end];
        out.push(match code {
            BR_ERROR => Return::Error(i32::from_ne_bytes(rd(p, 0))),
            BR_OK => Return::Ok,
            BR_TRANSACTION => Return::Transaction(TransactionData::decode(p)),
            BR_REPLY => Return::Reply(TransactionData::decode(p)),
            BR_DEAD_REPLY => Return::DeadReply,
            BR_TRANSACTION_COMPLETE => Return::TransactionComplete,
            BR_INCREFS => Return::IncRefs(PtrCookie::decode(p)),
            BR_ACQUIRE => Return::Acquire(PtrCookie::decode(p)),
            BR_RELEASE => Return::Release(PtrCookie::decode(p)),
            BR_DECREFS => Return::DecRefs(PtrCookie::decode(p)),
            BR_NOOP => Return::Noop,
            BR_SPAWN_LOOPER => Return::SpawnLooper,
            BR_FINISHED => Return::Finished,
            BR_DEAD_BINDER => Return::DeadBinder(rd_u64(p, 0)),
            BR_CLEAR_DEATH_NOTIFICATION_DONE => Return::ClearDeathNotificationDone(rd_u64(p, 0)),
            BR_FAILED_REPLY => Return::FailedReply,
            other => Return::Other(other),
        });
        at = end;
    }
    Ok(out)
}

/// A flat object inside transaction data, at an offset listed in the offsets array.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Object {
    /// `BINDER_TYPE_BINDER` / `_WEAK_BINDER`: a node of the sending (or, after translation, receiving)
    /// process.
    Binder {
        weak: bool,
        flags: u32,
        ptr: Uptr,
        cookie: Uptr,
    },
    /// `BINDER_TYPE_HANDLE` / `_WEAK_HANDLE`.
    Handle {
        weak: bool,
        flags: u32,
        handle: u32,
        cookie: Uptr,
    },
    /// `BINDER_TYPE_FD`.
    Fd { fd: u32, cookie: Uptr },
    /// `BINDER_TYPE_PTR` / `_FDA` (scatter-gather; libbinder never sends them) or an unknown type.
    Other { kind: u32 },
}

const FBO: usize = size_of::<flat_binder_object>();
const FDO: usize = size_of::<binder_fd_object>();
const HDR_TYPE: usize = offset_of!(binder_object_header, r#type);
const FBO_TYPE: usize = offset_of!(flat_binder_object, hdr) + HDR_TYPE;
const FBO_FLAGS: usize = offset_of!(flat_binder_object, flags);
const FBO_BINDER: usize =
    offset_of!(flat_binder_object, anon0) + offset_of!(flat_binder_object_anon0, binder);
const FBO_HANDLE: usize =
    offset_of!(flat_binder_object, anon0) + offset_of!(flat_binder_object_anon0, handle);
const FBO_COOKIE: usize = offset_of!(flat_binder_object, cookie);
const FDO_TYPE: usize = offset_of!(binder_fd_object, hdr) + HDR_TYPE;
const FDO_FD: usize = offset_of!(binder_fd_object, anon0) + offset_of!(binder_fd_object_anon0, fd);
const FDO_COOKIE: usize = offset_of!(binder_fd_object, cookie);

/// Byte offset of the fd number inside a `binder_fd_object`: where a receiver's fd gets written.
pub const FD_OBJECT_FD_OFFSET: usize = FDO_FD;

impl Object {
    /// Encoded size of this object's record.
    pub fn len(&self) -> usize {
        match self {
            Object::Fd { .. } => FDO,
            _ => FBO,
        }
    }

    /// Never empty.
    pub fn is_empty(&self) -> bool {
        false
    }

    /// The object at `data[off..]`, or `None` when `off` is misaligned, the header or the record does
    /// not fit, or the type is not one a flat object can be (kernel `binder_get_object`).
    pub fn read(data: &[u8], off: usize) -> Option<Object> {
        if !off.is_multiple_of(size_of::<u32>()) || data.len() < off.checked_add(HDR_TYPE + 4)? {
            return None;
        }
        let kind = rd_u32(data, off + HDR_TYPE);
        let size = match kind {
            BINDER_TYPE_BINDER
            | BINDER_TYPE_WEAK_BINDER
            | BINDER_TYPE_HANDLE
            | BINDER_TYPE_WEAK_HANDLE => FBO,
            BINDER_TYPE_FD => FDO,
            BINDER_TYPE_PTR => size_of::<binder_buffer_object>(),
            BINDER_TYPE_FDA => size_of::<binder_fd_array_object>(),
            _ => return Some(Object::Other { kind }),
        };
        if data.len() < off + size {
            return None;
        }
        let b = &data[off..off + size];
        Some(match kind {
            BINDER_TYPE_BINDER | BINDER_TYPE_WEAK_BINDER => Object::Binder {
                weak: kind == BINDER_TYPE_WEAK_BINDER,
                flags: rd_u32(b, FBO_FLAGS),
                ptr: rd_u64(b, FBO_BINDER),
                cookie: rd_u64(b, FBO_COOKIE),
            },
            BINDER_TYPE_HANDLE | BINDER_TYPE_WEAK_HANDLE => Object::Handle {
                weak: kind == BINDER_TYPE_WEAK_HANDLE,
                flags: rd_u32(b, FBO_FLAGS),
                handle: rd_u32(b, FBO_HANDLE),
                cookie: rd_u64(b, FBO_COOKIE),
            },
            BINDER_TYPE_FD => Object::Fd {
                fd: rd_u32(b, FDO_FD),
                cookie: rd_u64(b, FDO_COOKIE),
            },
            _ => Object::Other { kind },
        })
    }

    /// Writes the object's record at `data[off..]` (the caller checked it fits: `off + len()`).
    pub fn write(&self, data: &mut [u8], off: usize) {
        let b = &mut data[off..off + self.len()];
        b.fill(0);
        match *self {
            Object::Binder {
                weak,
                flags,
                ptr,
                cookie,
            } => {
                let kind = if weak {
                    BINDER_TYPE_WEAK_BINDER
                } else {
                    BINDER_TYPE_BINDER
                };
                wr(b, FBO_TYPE, &kind.to_ne_bytes());
                wr(b, FBO_FLAGS, &flags.to_ne_bytes());
                wr(b, FBO_BINDER, &ptr.to_ne_bytes());
                wr(b, FBO_COOKIE, &cookie.to_ne_bytes());
            }
            Object::Handle {
                weak,
                flags,
                handle,
                cookie,
            } => {
                let kind = if weak {
                    BINDER_TYPE_WEAK_HANDLE
                } else {
                    BINDER_TYPE_HANDLE
                };
                wr(b, FBO_TYPE, &kind.to_ne_bytes());
                wr(b, FBO_FLAGS, &flags.to_ne_bytes());
                wr(b, FBO_HANDLE, &handle.to_ne_bytes());
                wr(b, FBO_COOKIE, &cookie.to_ne_bytes());
            }
            Object::Fd { fd, cookie } => {
                wr(b, FDO_TYPE, &BINDER_TYPE_FD.to_ne_bytes());
                wr(b, FDO_FD, &fd.to_ne_bytes());
                wr(b, FDO_COOKIE, &cookie.to_ne_bytes());
            }
            Object::Other { kind } => wr(b, FBO_TYPE, &kind.to_ne_bytes()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Expected values from the uapi definitions themselves (asm-generic/ioctl.h: nr bits 0-7, type
    // 8-15, size 16-29, dir 30-31 with _IOC_WRITE = 1, _IOC_READ = 2), computed by hand here.

    #[test]
    fn codes_follow_ioctl_encoding() {
        // _IOW('c', 0, struct binder_transaction_data): dir 1, size 64 (LP64), type 0x63, nr 0
        assert_eq!(BC_TRANSACTION, (1 << 30) | (64 << 16) | (0x63 << 8));
        // _IOR('r', 3, struct binder_transaction_data)
        assert_eq!(BR_REPLY, (2 << 30) | (64 << 16) | (0x72 << 8) | 3);
        // _IO('r', 12)
        assert_eq!(BR_NOOP, (0x72 << 8) | 12);
        // _IOWR('b', 1, struct binder_write_read): 6 x u64 = 48 bytes
        assert_eq!(BINDER_WRITE_READ, (3 << 30) | (48 << 16) | (0x62 << 8) | 1);
        assert_eq!(ioc_size(BC_TRANSACTION), 64);
        // binder_handle_cookie is packed: u32 + u64 = 12
        assert_eq!(ioc_size(BC_REQUEST_DEATH_NOTIFICATION), 12);
        // B_PACK_CHARS('s', 'b', '*', 0x85)
        assert_eq!(BINDER_TYPE_BINDER, 0x7362_2a85);
        assert_eq!(BINDER_CURRENT_PROTOCOL_VERSION, 8);
    }

    #[test]
    fn transaction_bytes_match_uapi_layout() {
        // binder_transaction_data on LP64: target@0 (8), cookie@8, code@16, flags@20, sender_pid@24,
        // sender_euid@28, data_size@32, offsets_size@40, data.ptr.buffer@48, data.ptr.offsets@56
        let t = TransactionData {
            target: 0,
            cookie: 0x1111,
            code: 7,
            flags: TF_ONE_WAY,
            sender_pid: -1,
            sender_euid: 10001,
            data_size: 16,
            offsets_size: 8,
            buffer: 0x40,
            offsets: 0x50,
        }
        .with_handle(3);
        let mut w = CommandWriter::new();
        w.push(&Command::Transaction(t));
        let b = &w.0;
        assert_eq!(b.len(), 4 + 64);
        let at = |o: usize, n: usize| &b[4 + o..4 + o + n];
        assert_eq!(at(0, 4), 3u32.to_ne_bytes());
        assert_eq!(at(8, 8), 0x1111u64.to_ne_bytes());
        assert_eq!(at(16, 4), 7u32.to_ne_bytes());
        assert_eq!(at(20, 4), 1u32.to_ne_bytes());
        assert_eq!(at(24, 4), (-1i32).to_ne_bytes());
        assert_eq!(at(28, 4), 10001u32.to_ne_bytes());
        assert_eq!(at(32, 8), 16u64.to_ne_bytes());
        assert_eq!(at(40, 8), 8u64.to_ne_bytes());
        assert_eq!(at(48, 8), 0x40u64.to_ne_bytes());
        assert_eq!(at(56, 8), 0x50u64.to_ne_bytes());
        let (cmd, next) = decode_command(b, 0).unwrap();
        assert_eq!(next, b.len());
        let Command::Transaction(back) = cmd else {
            panic!("{cmd:?}")
        };
        assert_eq!(back, t);
        assert_eq!(back.target_handle(), 3);
    }

    #[test]
    fn stream_walk_and_truncation() {
        let mut w = CommandWriter::new();
        w.push(&Command::EnterLooper)
            .push(&Command::Acquire(5))
            .push(&Command::RequestDeathNotification(HandleCookie {
                handle: 5,
                cookie: 9,
            }));
        let b = w.0.clone();
        // 4 + (4 + 4) + (4 + 12)
        assert_eq!(b.len(), 28);
        let (c1, n1) = decode_command(&b, 0).unwrap();
        let (c2, n2) = decode_command(&b, n1).unwrap();
        let (c3, n3) = decode_command(&b, n2).unwrap();
        assert_eq!(
            (c1, c2, c3, n3),
            (
                Command::EnterLooper,
                Command::Acquire(5),
                Command::RequestDeathNotification(HandleCookie {
                    handle: 5,
                    cookie: 9
                }),
                28
            )
        );
        assert_eq!(
            decode_command(&b[..27], n2),
            Err(WireError::Truncated { at: n2 })
        );
        // not a BC code at all: no payload trusted
        let junk = 0xdead_beefu32.to_ne_bytes();
        assert_eq!(
            decode_command(&junk, 0),
            Ok((Command::Unknown(0xdead_beef), 4))
        );
    }

    #[test]
    fn objects_round_trip_at_uapi_offsets() {
        let mut data = vec![0u8; 48];
        let o = Object::Handle {
            weak: false,
            flags: 0x17f,
            handle: 2,
            cookie: 0,
        };
        o.write(&mut data, 0);
        // flat_binder_object: type@0, flags@4, handle@8, cookie@16; 24 bytes
        assert_eq!(&data[0..4], &BINDER_TYPE_HANDLE.to_ne_bytes());
        assert_eq!(&data[4..8], &0x17fu32.to_ne_bytes());
        assert_eq!(&data[8..12], &2u32.to_ne_bytes());
        assert_eq!(Object::read(&data, 0), Some(o));
        let fd = Object::Fd { fd: 4, cookie: 0 };
        fd.write(&mut data, 24);
        // binder_fd_object: fd@8
        assert_eq!(FD_OBJECT_FD_OFFSET, 8);
        assert_eq!(Object::read(&data, 24), Some(fd));
        // misaligned, or a record running past the data
        assert_eq!(Object::read(&data, 2), None);
        assert_eq!(Object::read(&data[..40], 24), None);
    }

    #[test]
    fn returns_decode() {
        let mut b = Vec::new();
        for r in [
            Return::Noop,
            Return::IncRefs(PtrCookie { ptr: 1, cookie: 2 }),
            Return::TransactionComplete,
            Return::DeadBinder(7),
        ] {
            r.encode(&mut b);
        }
        // 4 + (4 + 16) + 4 + (4 + 8)
        assert_eq!(b.len(), 40);
        assert_eq!(
            decode_returns(&b).unwrap(),
            vec![
                Return::Noop,
                Return::IncRefs(PtrCookie { ptr: 1, cookie: 2 }),
                Return::TransactionComplete,
                Return::DeadBinder(7)
            ]
        );
        assert!(decode_returns(&b[..39]).is_err());
    }
}
