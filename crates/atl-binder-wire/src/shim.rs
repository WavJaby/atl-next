// SPDX-License-Identifier: MIT
//! The client side's stream rewriting (atl-binder-driver), safe over byte buffers: what a process sends and receives instead of the kernel's pointer-based ioctl.
//!
//! Outgoing (`outgoing`): `BC_TRANSACTION` / `BC_REPLY` name their data and offsets by user pointer; their bytes are copied into the request's payload and the two fields become payload offsets. `BC_FREE_BUFFER` names a buffer by its address in the region; it becomes the region offset. `BINDER_TYPE_FD` objects' fds are collected in stream and object order (they travel by `SCM_RIGHTS`).
//! Incoming (`incoming`): `BR_TRANSACTION` / `BR_REPLY` carry region offsets; they become addresses (region base + offset).
//! Every other command and return passes through unchanged. A malformed stream is left as it is from the first bad item on: binderd (outgoing) rejects it as the kernel would; incoming streams come from binderd.

use core::mem::offset_of;

use crate::uapi::*;
use crate::{CODE_SIZE, Object, Uptr, ioc_size, rd_u32, rd_u64, wr};

/// Byte offsets of `binder_transaction_data`'s pointer fields inside one command or return (after its code word).
const DATA_SIZE: usize = CODE_SIZE + offset_of!(binder_transaction_data, data_size);
const OFFSETS_SIZE: usize = CODE_SIZE + offset_of!(binder_transaction_data, offsets_size);
const BUFFER: usize = CODE_SIZE
    + offset_of!(binder_transaction_data, data)
    + offset_of!(binder_transaction_data_data, ptr)
    + offset_of!(binder_transaction_data_data_ptr, buffer);
const OFFSETS: usize = CODE_SIZE
    + offset_of!(binder_transaction_data, data)
    + offset_of!(binder_transaction_data_data, ptr)
    + offset_of!(binder_transaction_data_data_ptr, offsets);

/// Largest data or offsets array copied: the kernel's region cap (4 MiB); larger is left for binderd to refuse.
const MAX_ARRAY: u64 = 4 << 20;

/// What `outgoing` built: the request payload and the fds to pass, in order.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Outgoing {
    pub payload: Vec<u8>,
    pub fds: Vec<i32>,
}

/// Rewrites the BC stream `commands` in place (see the module doc). `read(ptr, len)` returns `len` bytes of the process's memory at `ptr`; `region` = (base address, length) of the process's mapped region, if any.
pub fn outgoing(
    commands: &mut [u8],
    region: Option<(Uptr, u64)>,
    read: &mut dyn FnMut(Uptr, usize) -> Vec<u8>,
) -> Outgoing {
    let mut out = Outgoing::default();
    let mut at = 0;
    while at + CODE_SIZE <= commands.len() {
        let code = rd_u32(commands, at);
        let end = at + CODE_SIZE + ioc_size(code);
        if end > commands.len() {
            break;
        }
        match code {
            BC_TRANSACTION | BC_REPLY => {
                let item = &mut commands[at..end];
                let data_size = rd_u64(item, DATA_SIZE);
                let offsets_size = rd_u64(item, OFFSETS_SIZE);
                if data_size > MAX_ARRAY || offsets_size > MAX_ARRAY {
                    break;
                }
                let data = read(rd_u64(item, BUFFER), data_size as usize);
                let offsets = read(rd_u64(item, OFFSETS), offsets_size as usize);
                for o in offsets.as_chunks::<8>().0 {
                    let off = u64::from_ne_bytes(*o);
                    if let Some(Object::Fd { fd, .. }) = usize::try_from(off)
                        .ok()
                        .and_then(|off| Object::read(&data, off))
                    {
                        out.fds.push(fd as i32);
                    }
                }
                wr(item, BUFFER, &(out.payload.len() as u64).to_ne_bytes());
                out.payload.extend_from_slice(&data);
                wr(item, OFFSETS, &(out.payload.len() as u64).to_ne_bytes());
                out.payload.extend_from_slice(&offsets);
            }
            BC_FREE_BUFFER => {
                let ptr = rd_u64(commands, at + CODE_SIZE);
                // an address outside the region stays as it is: binderd finds no buffer there and counts it
                if let Some((base, len)) = region
                    && ptr >= base
                    && ptr - base < len
                {
                    wr(commands, at + CODE_SIZE, &(ptr - base).to_ne_bytes());
                }
            }
            _ => {}
        }
        at = end;
    }
    out
}

/// Rewrites the BR stream `returns` in place: buffer and offsets of `BR_TRANSACTION` / `BR_REPLY` from region offsets to addresses at `base`.
pub fn incoming(returns: &mut [u8], base: Uptr) {
    let mut at = 0;
    while at + CODE_SIZE <= returns.len() {
        let code = rd_u32(returns, at);
        let end = at + CODE_SIZE + ioc_size(code);
        if end > returns.len() {
            break;
        }
        if code == BR_TRANSACTION || code == BR_REPLY {
            let item = &mut returns[at..end];
            for field in [BUFFER, OFFSETS] {
                let v = rd_u64(item, field).wrapping_add(base);
                wr(item, field, &v.to_ne_bytes());
            }
        }
        at = end;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Command, CommandWriter, Return, TransactionData, decode_command};

    /// A transaction with one fd object and one handle object: data and offsets land in the payload, fields become payload offsets, the fd is collected; FREE_BUFFER inside the region becomes an offset, outside it stays.
    #[test]
    fn outgoing_rewrites() {
        let mut data = vec![0u8; 48];
        Object::Fd { fd: 7, cookie: 0 }.write(&mut data, 0);
        Object::Handle {
            weak: false,
            flags: 0,
            handle: 3,
            cookie: 0,
        }
        .write(&mut data, 24);
        let offsets: Vec<u8> = [0u64, 24].iter().flat_map(|o| o.to_ne_bytes()).collect();
        let (data_ptr, offsets_ptr) = (0x1000, 0x2000);
        let mut w = CommandWriter::new();
        w.push(&Command::Transaction(
            TransactionData {
                data_size: 48,
                offsets_size: 16,
                buffer: data_ptr,
                offsets: offsets_ptr,
                ..Default::default()
            }
            .with_handle(0),
        ));
        w.push(&Command::FreeBuffer(0x9000_0040));
        w.push(&Command::FreeBuffer(0x10));
        let mut stream = w.0;
        let mut read = |ptr: Uptr, len: usize| -> Vec<u8> {
            match ptr {
                0x1000 => data[..len].to_vec(),
                0x2000 => offsets[..len].to_vec(),
                _ => panic!("read of {ptr:#x}"),
            }
        };
        let out = outgoing(&mut stream, Some((0x9000_0000, 0x1000)), &mut read);
        assert_eq!(out.fds, vec![7]);
        let mut want = data.clone();
        want.extend_from_slice(&offsets);
        assert_eq!(out.payload, want);
        let (c, next) = decode_command(&stream, 0).unwrap();
        let Command::Transaction(t) = c else {
            panic!("{c:?}")
        };
        assert_eq!((t.buffer, t.offsets), (0, 48));
        let (c, next) = decode_command(&stream, next).unwrap();
        assert_eq!(c, Command::FreeBuffer(0x40));
        assert_eq!(
            decode_command(&stream, next).unwrap().0,
            Command::FreeBuffer(0x10)
        );
    }

    #[test]
    fn incoming_adds_base() {
        let mut s = Vec::new();
        Return::Noop.encode(&mut s);
        Return::Reply(TransactionData {
            buffer: 0x40,
            offsets: 0x50,
            ..Default::default()
        })
        .encode(&mut s);
        incoming(&mut s, 0x7000_0000);
        let r = crate::decode_returns(&s).unwrap();
        let Return::Reply(t) = r[1] else {
            panic!("{r:?}")
        };
        assert_eq!((t.buffer, t.offsets), (0x7000_0040, 0x7000_0050));
    }
}
