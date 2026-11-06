// SPDX-License-Identifier: MIT
//! android.view.SurfaceControl.Transaction (SurfaceControl.java:5276-5296 writeToParcel / readFromParcel): an int (0 = no native transaction), then libgui's
//! `SurfaceComposerClient::Transaction::writeToParcel` bytes (SurfaceComposerClient.cpp:903). Kept as those bytes and their objects, never decoded here: the window service forwards them
//! to the app's composer, which decodes them (atl-libgui-parcel), as WMS hands a post-draw Transaction on to SurfaceFlinger.
//! The bytes carry no length, so their end is found by libgui's reader. This crate is a converter rust_library and links only Soong rustlibs, not atl-libgui-parcel, so the reader is
//! installed at startup by its user ([`set_framer`]); without one a Transaction is refused, counted. Writing one is refused: nothing sends a Transaction yet.

use std::os::fd::OwnedFd;
use std::sync::{Arc, OnceLock};

use binder::{ParcelFileDescriptor, SpIBinder};

use crate::opaque::refuse;
use crate::parcel::*;
use crate::parts::*;

/// The Java name refusals count under.
const JAVA: &str = "android.view.SurfaceControl.Transaction";
/// binder.h `BINDER_TYPE_FD` (B_PACK_CHARS('f', 'd', '*', B_TYPE_LARGE)).
const BINDER_TYPE_FD: u32 = 0x6664_2a85;

/// Finds a libgui Transaction in a parcel's data (`data`, holding `object_count` objects inline) that starts at byte `start`: its end and the offsets of the objects inside it.
pub type Framer = fn(
    data: &[u8],
    object_count: usize,
    start: usize,
) -> std::result::Result<(usize, Vec<usize>), String>;

static FRAMER: OnceLock<Framer> = OnceLock::new();

/// Installs the reader that finds where a Transaction ends; false if one was installed already.
pub fn set_framer(f: Framer) -> bool {
    FRAMER.set(f).is_ok()
}

/// An object a Transaction's bytes carry, in the order of its offset.
#[derive(Debug, Clone)]
pub enum TransactionObject {
    Binder(Option<SpIBinder>),
    Fd(Arc<OwnedFd>),
}

/// A Transaction as libgui parcelled it: `data` holds its bytes (object slots as the sender's libbinder wrote them), `objects` each object's offset in `data` and the live object.
/// `present` is Java's leading int: false for a Transaction with no native object (no bytes follow).
#[allow(non_camel_case_types)]
#[derive(Debug, Clone, Default)]
pub struct SurfaceControl_Transaction {
    pub present: bool,
    pub data: Vec<u8>,
    pub objects: Vec<(usize, TransactionObject)>,
}

impl PartialEq for SurfaceControl_Transaction {
    /// Same bytes and offsets; binders compare by identity, fds by the shared descriptor.
    fn eq(&self, o: &Self) -> bool {
        self.present == o.present
            && self.data == o.data
            && self.objects.len() == o.objects.len()
            && self.objects.iter().zip(&o.objects).all(|(a, b)| {
                a.0 == b.0
                    && match (&a.1, &b.1) {
                        (TransactionObject::Binder(x), TransactionObject::Binder(y)) => x == y,
                        (TransactionObject::Fd(x), TransactionObject::Fd(y)) => Arc::ptr_eq(x, y),
                        _ => false,
                    }
            })
    }
}

impl Part for SurfaceControl_Transaction {
    fn write(&self, _: &mut In<'_>) -> Result<()> {
        Err(refuse(JAVA))
    }

    fn read(p: &In<'_>) -> Result<Self> {
        if r_i32(p)? == 0 {
            return Ok(Self::default());
        }
        let Some(frame) = FRAMER.get() else {
            return Err(refuse(JAVA));
        };
        let start = usize::try_from(p.get_data_position()).map_err(|_| refuse(JAVA))?;
        let (data, count) = p.data_with_objects();
        let (end, offsets) = frame(&data, count, start).map_err(|_| refuse(JAVA))?;
        if end < start || end > data.len() || offsets.iter().any(|&o| o < start || o >= end) {
            return Err(refuse(JAVA));
        }
        let mut objects = Vec::with_capacity(offsets.len());
        for &o in &offsets {
            p.set_data_position_checked(i32::try_from(o).map_err(|_| refuse(JAVA))?)?;
            let ty = u32::from_le_bytes([data[o], data[o + 1], data[o + 2], data[o + 3]]);
            let object = if ty == BINDER_TYPE_FD {
                let fd: ParcelFileDescriptor = p.read()?;
                TransactionObject::Fd(Arc::new(fd.into()))
            } else {
                TransactionObject::Binder(p.read()?)
            };
            objects.push((o - start, object));
        }
        p.set_data_position_checked(i32::try_from(end).map_err(|_| refuse(JAVA))?)?;
        Ok(Self {
            present: true,
            data: data[start..end].to_vec(),
            objects,
        })
    }
}

crate::codec!(SurfaceControl_Transaction);
