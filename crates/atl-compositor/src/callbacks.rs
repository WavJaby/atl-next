// SPDX-License-Identifier: Apache-2.0
// Derived from the Android Open Source Project: frameworks/native/libs/gui/ITransactionCompletedListener.cpp (ListenerStats / TransactionStats / SurfaceStats / FrameEventHistoryStats / ReleaseCallbackId writeToParcel,
// BpTransactionCompletedListener tags), frameworks/native/libs/gui/BufferReleaseChannel.cpp (Message::flatten), frameworks/binder/include/binder/SafeInterface.h (callRemoteAsync argument writing)
//! What the compositor sends back to a client: `ITransactionCompletedListener.onTransactionCompleted` / `onReleaseBuffer` (hand-written C++, so raw one-way calls: interface token, then the arguments `SafeInterface` writes)
//! and the `BufferReleaseChannel` datagram a buffer release travels in when the layer registered a channel.

use crate::parcel::{FdObject, ParcelError, Reader, StrongBinder, WireParcel};
use crate::state::{CallbackId, Fence, read_callback_ids, write_callback_ids};

type R<T> = Result<T, ParcelError>;

/// `IMPLEMENT_META_INTERFACE(TransactionCompletedListener, "android.gui.ITransactionComposerListener")`: AOSP's descriptor does not match the interface's name.
pub const LISTENER_DESCRIPTOR: &str = "android.gui.ITransactionComposerListener";
/// `Tag::ON_TRANSACTION_COMPLETED` = `FIRST_CALL_TRANSACTION`.
pub const ON_TRANSACTION_COMPLETED: u32 = 1;
/// `Tag::ON_RELEASE_BUFFER`.
pub const ON_RELEASE_BUFFER: u32 = 2;
/// `ReleaseCallbackId::INVALID_ID` is `{0, 0}`.
pub const NO_RELEASE_CALLBACK: ReleaseCallbackId = ReleaseCallbackId {
    buffer_id: 0,
    frame_number: 0,
};

/// `ReleaseCallbackId`: a buffer and the frame it was queued for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ReleaseCallbackId {
    /// `GraphicBuffer::getId`.
    pub buffer_id: u64,
    /// BufferQueue frame number.
    pub frame_number: u64,
}

impl ReleaseCallbackId {
    fn write_parcelable(&self, p: &mut WireParcel) {
        p.write_parcelable_flag();
        p.write_u64(self.buffer_id);
        p.write_u64(self.frame_number);
    }

    fn read_parcelable(r: &mut Reader<'_>) -> R<Self> {
        r.read_parcelable_flag("ReleaseCallbackId")?;
        Ok(Self {
            buffer_id: r.read_u64()?,
            frame_number: r.read_u64()?,
        })
    }
}

/// `FrameEventHistoryStats`: per-frame timing the compositor reports (all zero for the software path).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FrameEventHistoryStats {
    /// The frame number.
    pub frame_number: u64,
    /// The previous frame number (libgui flag `frametimestamps_previousrelease`).
    pub previous_frame_number: u64,
    /// GPU composition done fence.
    pub gpu_composition_done_fence: Option<Fence>,
    /// Compositor deadline, ns.
    pub deadline: i64,
    /// Compositor interval, ns.
    pub interval: i64,
    /// Present latency, ns.
    pub present_latency: i64,
    /// Refresh start, ns.
    pub refresh_start_time: i64,
    /// Dequeue ready, ns.
    pub dequeue_ready_time: i64,
}

impl FrameEventHistoryStats {
    fn write(&self, p: &mut WireParcel) -> R<()> {
        p.write_u64(self.frame_number);
        p.write_u64(self.previous_frame_number);
        write_optional_fence(p, &self.gpu_composition_done_fence)?;
        p.write_i64(self.deadline);
        p.write_i64(self.interval);
        p.write_i64(self.present_latency);
        p.write_i64(self.refresh_start_time);
        p.write_i64(self.dequeue_ready_time);
        Ok(())
    }

    fn read(r: &mut Reader<'_>) -> R<Self> {
        Ok(Self {
            frame_number: r.read_u64()?,
            previous_frame_number: r.read_u64()?,
            gpu_composition_done_fence: read_optional_fence(r)?,
            deadline: r.read_i64()?,
            interval: r.read_i64()?,
            present_latency: r.read_i64()?,
            refresh_start_time: r.read_i64()?,
            dequeue_ready_time: r.read_i64()?,
        })
    }
}

fn write_optional_fence(p: &mut WireParcel, f: &Option<Fence>) -> R<()> {
    match f {
        Some(f) => {
            p.write_bool(true);
            f.write(p)
        }
        None => {
            p.write_bool(false);
            Ok(())
        }
    }
}

fn read_optional_fence(r: &mut Reader<'_>) -> R<Option<Fence>> {
    if r.read_bool()? {
        Fence::read(r).map(Some)
    } else {
        Ok(None)
    }
}

/// `SurfaceStats::acquireTimeOrFence`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AcquireTimeOrFence {
    /// The acquire fence.
    Fence(Fence),
    /// The acquire time, ns.
    Time(i64),
}

/// `SurfaceStats`: what happened to one layer in the transaction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SurfaceStats {
    /// The layer handle binder (the one the client set the state with).
    pub surface_control: StrongBinder,
    /// When the buffer was acquired, or its fence.
    pub acquire_time_or_fence: AcquireTimeOrFence,
    /// The fence signalled when the previous buffer may be reused.
    pub previous_release_fence: Option<Fence>,
    /// The transform hint, if the compositor sets one.
    pub transform_hint: Option<u32>,
    /// How many buffers the client may hold acquired.
    pub current_max_acquired_buffer_count: u32,
    /// Frame timing.
    pub event_stats: FrameEventHistoryStats,
    /// The previous buffer that this latch released, `NO_RELEASE_CALLBACK` when none.
    pub previous_release_callback_id: ReleaseCallbackId,
}

impl SurfaceStats {
    fn write(&self, p: &mut WireParcel) -> R<()> {
        p.write_strong_binder(&self.surface_control);
        match &self.acquire_time_or_fence {
            AcquireTimeOrFence::Fence(f) => {
                p.write_bool(true);
                f.write(p)?;
            }
            AcquireTimeOrFence::Time(t) => {
                p.write_bool(false);
                p.write_i64(*t);
            }
        }
        write_optional_fence(p, &self.previous_release_fence)?;
        p.write_bool(self.transform_hint.is_some());
        if let Some(h) = self.transform_hint {
            p.write_u32(h);
        }
        p.write_u32(self.current_max_acquired_buffer_count);
        p.write_parcelable_flag();
        self.event_stats.write(p)?;
        self.previous_release_callback_id.write_parcelable(p);
        Ok(())
    }

    fn read(r: &mut Reader<'_>) -> R<Self> {
        let surface_control = r.read_strong_binder(false, "SurfaceStats.surfaceControl")?;
        let acquire_time_or_fence = if r.read_bool()? {
            AcquireTimeOrFence::Fence(Fence::read(r)?)
        } else {
            AcquireTimeOrFence::Time(r.read_i64()?)
        };
        let previous_release_fence = read_optional_fence(r)?;
        let transform_hint = if r.read_bool()? {
            Some(r.read_u32()?)
        } else {
            None
        };
        let current_max_acquired_buffer_count = r.read_u32()?;
        r.read_parcelable_flag("eventStats")?;
        let event_stats = FrameEventHistoryStats::read(r)?;
        let previous_release_callback_id = ReleaseCallbackId::read_parcelable(r)?;
        Ok(Self {
            surface_control,
            acquire_time_or_fence,
            previous_release_fence,
            transform_hint,
            current_max_acquired_buffer_count,
            event_stats,
            previous_release_callback_id,
        })
    }
}

/// `TransactionStats`: the outcome of the callbacks one transaction registered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TransactionStats {
    /// The callbacks this completes.
    pub callback_ids: Vec<CallbackId>,
    /// When the transaction was latched, ns.
    pub latch_time: i64,
    /// The present fence.
    pub present_fence: Option<Fence>,
    /// Per-layer stats.
    pub surface_stats: Vec<SurfaceStats>,
}

impl TransactionStats {
    fn write(&self, p: &mut WireParcel) -> R<()> {
        write_callback_ids(p, &self.callback_ids)?;
        p.write_i64(self.latch_time);
        write_optional_fence(p, &self.present_fence)?;
        p.write_len(self.surface_stats.len(), "surface stats")?;
        for s in &self.surface_stats {
            p.write_parcelable_flag();
            s.write(p)?;
        }
        Ok(())
    }

    fn read(r: &mut Reader<'_>) -> R<Self> {
        let callback_ids = read_callback_ids(r)?;
        let latch_time = r.read_i64()?;
        let present_fence = read_optional_fence(r)?;
        let n = r.read_count("surface stats count")?;
        let surface_stats = (0..n)
            .map(|_| {
                r.read_parcelable_flag("SurfaceStats")?;
                SurfaceStats::read(r)
            })
            .collect::<R<Vec<_>>>()?;
        Ok(Self {
            callback_ids,
            latch_time,
            present_fence,
            surface_stats,
        })
    }
}

/// `ListenerStats`: the transactions' outcomes for one listener (the listener itself is the call's target and is not written).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ListenerStats {
    /// One entry per transaction completed.
    pub transaction_stats: Vec<TransactionStats>,
}

impl ListenerStats {
    fn write(&self, p: &mut WireParcel) -> R<()> {
        p.write_len(self.transaction_stats.len(), "transaction stats")?;
        for t in &self.transaction_stats {
            p.write_parcelable_flag();
            t.write(p)?;
        }
        Ok(())
    }

    fn read(r: &mut Reader<'_>) -> R<Self> {
        let n = r.read_count("transaction stats count")?;
        let transaction_stats = (0..n)
            .map(|_| {
                r.read_parcelable_flag("TransactionStats")?;
                TransactionStats::read(r)
            })
            .collect::<R<Vec<_>>>()?;
        Ok(Self { transaction_stats })
    }

    /// The one-way call `BpTransactionCompletedListener::onTransactionCompleted(stats)` sends: code [`ON_TRANSACTION_COMPLETED`] and this Parcel.
    pub fn encode_call(&self) -> R<WireParcel> {
        let mut p = WireParcel::new();
        p.write_interface_token(LISTENER_DESCRIPTOR)?;
        p.write_parcelable_flag();
        self.write(&mut p)?;
        Ok(p)
    }

    /// The reader `callLocalAsync(onTransactionCompleted)` runs.
    pub fn decode_call(p: &WireParcel) -> R<Self> {
        let mut r = p.reader();
        r.enforce_interface(LISTENER_DESCRIPTOR)?;
        r.read_parcelable_flag("ListenerStats")?;
        let s = Self::read(&mut r)?;
        r.finish()?;
        Ok(s)
    }
}

/// `onReleaseBuffer(callbackId, releaseFence, currentMaxAcquiredBufferCount)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReleaseBuffer {
    /// The buffer and frame released.
    pub callback_id: ReleaseCallbackId,
    /// Signalled when the compositor is done reading the buffer; an invalid fence when it already is.
    pub release_fence: Fence,
    /// How many buffers the client may hold acquired now.
    pub current_max_acquired_buffer_count: u32,
}

impl ReleaseBuffer {
    /// The one-way call `BpTransactionCompletedListener::onReleaseBuffer` sends: code [`ON_RELEASE_BUFFER`] and this Parcel.
    pub fn encode_call(&self) -> R<WireParcel> {
        let mut p = WireParcel::new();
        p.write_interface_token(LISTENER_DESCRIPTOR)?;
        self.callback_id.write_parcelable(&mut p);
        self.release_fence.write(&mut p)?;
        p.write_u32(self.current_max_acquired_buffer_count);
        Ok(p)
    }

    /// The reader `callLocalAsync(onReleaseBuffer)` runs.
    pub fn decode_call(p: &WireParcel) -> R<Self> {
        let mut r = p.reader();
        r.enforce_interface(LISTENER_DESCRIPTOR)?;
        let callback_id = ReleaseCallbackId::read_parcelable(&mut r)?;
        let release_fence = Fence::read(&mut r)?;
        let current_max_acquired_buffer_count = r.read_u32()?;
        r.finish()?;
        Ok(Self {
            callback_id,
            release_fence,
            current_max_acquired_buffer_count,
        })
    }

    /// The `BufferReleaseChannel::Message` datagram for the same release (what `ProducerEndpoint::writeReleaseFence` sends): the flattened fence (a uint32 fd count), then
    /// buffer id low, high, frame number low, high, max acquired count, each uint32; and the fence fd as the SCM_RIGHTS payload when valid.
    pub fn channel_datagram(&self) -> (Vec<u8>, Option<FdObject>) {
        let mut b = Vec::with_capacity(24);
        b.extend_from_slice(&u32::from(self.release_fence.0.is_some()).to_le_bytes());
        for w in [
            self.callback_id.buffer_id as u32,
            (self.callback_id.buffer_id >> 32) as u32,
            self.callback_id.frame_number as u32,
            (self.callback_id.frame_number >> 32) as u32,
            self.current_max_acquired_buffer_count,
        ] {
            b.extend_from_slice(&w.to_le_bytes());
        }
        (b, self.release_fence.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parcel::BinderKey;

    fn handle(h: u32) -> StrongBinder {
        StrongBinder {
            obj: crate::parcel::FlatObject {
                ty: crate::parcel::BINDER_TYPE_HANDLE,
                flags: 0,
                binder: u64::from(h),
                cookie: 0,
            },
            stability: 0,
        }
    }

    fn sample_stats() -> ListenerStats {
        ListenerStats {
            transaction_stats: vec![TransactionStats {
                callback_ids: vec![CallbackId {
                    id: 9,
                    ty: CallbackId::ON_COMPLETE,
                }],
                latch_time: 1_000,
                present_fence: None,
                surface_stats: vec![SurfaceStats {
                    surface_control: handle(7),
                    acquire_time_or_fence: AcquireTimeOrFence::Time(5),
                    previous_release_fence: None,
                    transform_hint: Some(2),
                    current_max_acquired_buffer_count: 3,
                    event_stats: FrameEventHistoryStats::default(),
                    previous_release_callback_id: ReleaseCallbackId {
                        buffer_id: 0x1_0000_0002,
                        frame_number: 4,
                    },
                }],
            }],
        }
    }

    #[test]
    fn listener_stats_round_trip_and_consumed() {
        let s = sample_stats();
        let p = s.encode_call().unwrap();
        assert_eq!(ListenerStats::decode_call(&p).unwrap(), s);
        // the surface-control binder is the only object
        assert_eq!(p.objects().len(), 1);
        let mut r = p.reader();
        r.enforce_interface(LISTENER_DESCRIPTOR).unwrap();
        assert_eq!(handle(7).key(), Some(BinderKey::Handle(7)));
    }

    /// Hand-derived layout of the smallest ListenerStats: token (3 words + descriptor), the non-null flag and an empty transaction list.
    #[test]
    fn empty_listener_stats_bytes() {
        let p = ListenerStats::default().encode_call().unwrap();
        let descriptor = LISTENER_DESCRIPTOR;
        let mut want = Vec::new();
        want.extend_from_slice(&i32::MIN.to_le_bytes()); // strict mode gather
        want.extend_from_slice(&(-1i32).to_le_bytes()); // work source unset
        want.extend_from_slice(&0x5359_5354i32.to_le_bytes()); // 'SYST'
        want.extend_from_slice(&(descriptor.len() as i32).to_le_bytes());
        let mut units: Vec<u8> = descriptor
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect();
        units.extend_from_slice(&[0, 0]); // NUL unit
        while !units.len().is_multiple_of(4) {
            units.push(0);
        }
        want.extend_from_slice(&units);
        want.extend_from_slice(&1i32.to_le_bytes()); // writeParcelable: non-null
        want.extend_from_slice(&0i32.to_le_bytes()); // zero transaction stats
        assert_eq!(p.data(), want);
    }

    /// A wrong field order is caught: a reader that swaps latch time and the callback ids does not accept the writer's bytes.
    #[test]
    fn wrong_field_order_fails() {
        let p = sample_stats().encode_call().unwrap();
        let mut r = p.reader();
        r.enforce_interface(LISTENER_DESCRIPTOR).unwrap();
        r.read_parcelable_flag("x").unwrap();
        let n = r.read_count("n").unwrap();
        assert_eq!(n, 1);
        r.read_parcelable_flag("x").unwrap();
        // TransactionStats starts with the callback id vector; reading latch time (an int64) first would take the vector's size + flag as its value
        let wrong_latch = r.read_i64().unwrap();
        assert_ne!(wrong_latch, 1_000);
    }

    #[test]
    fn release_buffer_round_trip_and_datagram() {
        let rb = ReleaseBuffer {
            callback_id: ReleaseCallbackId {
                buffer_id: 0xAABB_CCDD_1122_3344,
                frame_number: 0x1_0000_0005,
            },
            release_fence: Fence(Some(FdObject::new(11, false))),
            current_max_acquired_buffer_count: 3,
        };
        let p = rb.encode_call().unwrap();
        assert_eq!(ReleaseBuffer::decode_call(&p).unwrap(), rb);
        let (bytes, fd) = rb.channel_datagram();
        assert_eq!(fd, Some(FdObject::new(11, false)));
        let words: Vec<u32> = bytes
            .chunks(4)
            .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect();
        assert_eq!(words, [1, 0x1122_3344, 0xAABB_CCDD, 5, 1, 3]);
        let (nf, _) = ReleaseBuffer {
            release_fence: Fence(None),
            ..rb
        }
        .channel_datagram();
        assert_eq!(u32::from_le_bytes([nf[0], nf[1], nf[2], nf[3]]), 0);
    }
}
