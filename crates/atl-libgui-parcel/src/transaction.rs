// SPDX-License-Identifier: Apache-2.0
// Derived from the Android Open Source Project: frameworks/native/libs/gui/TransactionState.cpp (writeToParcel / readFromParcel), frameworks/native/libs/gui/ISurfaceComposer.cpp (BpSurfaceComposer::setTransactionState,
// BnSurfaceComposer::onTransact SET_TRANSACTION_STATE), frameworks/native/libs/gui/include/gui/ISurfaceComposer.h (transaction code, flags)
//! `TransactionState`, the payload of `android.ui.ISurfaceComposer.setTransactionState` (a hand-written C++ interface: no AIDL, so the call is an interface token and this state in one Parcel).

use crate::parcel::{FdObject, ParcelError, Reader, StrongBinder, WireParcel};
use crate::state::{
    CallbackId, DisplayState, FrameTimelineInfo, InputWindowCommands, LayerState,
    ListenerCallbacks, SurfaceControlParcel, read_callback_ids, write_callback_ids,
};

type R<T> = Result<T, ParcelError>;

/// `ISurfaceComposer`'s interface descriptor (IMPLEMENT_META_INTERFACE(SurfaceComposer, "android.ui.ISurfaceComposer")).
pub const SURFACE_COMPOSER_DESCRIPTOR: &str = "android.ui.ISurfaceComposer";
/// `BnSurfaceComposer::SET_TRANSACTION_STATE`: `FIRST_CALL_TRANSACTION` (1) plus its position in `ISurfaceComposerTag` (BOOT_FINISHED, CREATE_CONNECTION, GET_STATIC_DISPLAY_INFO,
/// CREATE_DISPLAY_EVENT_CONNECTION, CREATE_VIRTUAL_DISPLAY, DESTROY_VIRTUAL_DISPLAY, GET_PHYSICAL_DISPLAY_TOKEN come first).
pub const SET_TRANSACTION_STATE: u32 = 8;
/// `ISurfaceComposer::eOneWay`: the call is transacted `FLAG_ONEWAY`.
pub const E_ONE_WAY: u32 = 0x20;

/// A `ComposerState`: one layer's changes (its `layer_state_t`).
pub type ComposerState = LayerState;

/// `client_cache_t` as written to the wire: the cache's token and a buffer id to drop from it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UncacheBuffer {
    /// The client cache's token.
    pub token: StrongBinder,
    /// The cached buffer id.
    pub id: u64,
}

/// `TransactionState`: everything one `SurfaceComposerClient::Transaction::apply` sends.
#[derive(Clone, Debug, PartialEq)]
pub struct TransactionState {
    /// The transaction id (updated at every apply).
    pub id: u64,
    /// `ISurfaceComposer::eOneWay` / `eEarlyWakeup*` / `eAnimation` bits.
    pub flags: u32,
    /// Desired present time, ns.
    pub desired_present_time: i64,
    /// Whether `desired_present_time` was filled in by the library.
    pub is_auto_timestamp: bool,
    /// The Choreographer vsync and input event.
    pub frame_timeline_info: FrameTimelineInfo,
    /// Transactions with the same token apply in order; null means the process-wide token.
    pub apply_token: StrongBinder,
    /// A hint that a buffer may be in this transaction.
    pub may_contain_buffer: bool,
    /// Debug call-point logging.
    pub log_call_points: bool,
    /// Display changes.
    pub display_states: Vec<DisplayState>,
    /// Layer changes.
    pub composer_states: Vec<ComposerState>,
    /// Input commands (focus requests ...).
    pub input_window_commands: InputWindowCommands,
    /// Buffers to drop from the client cache.
    pub uncache_buffers: Vec<UncacheBuffer>,
    /// The last transactions merged into this one, most recent first.
    pub merged_transaction_ids: Vec<u64>,
    /// True even when `listener_callbacks` is empty.
    pub has_listener_callbacks: bool,
    /// Transaction-completed listeners and their callbacks.
    pub listener_callbacks: Vec<ListenerCallbacks>,
}

impl TransactionState {
    /// Every file descriptor the layer states of this transaction refer to ([`LayerState::fds`]).
    pub fn fds(&self) -> Vec<FdObject> {
        self.composer_states
            .iter()
            .flat_map(LayerState::fds)
            .collect()
    }

    /// An empty transaction with AOSP's defaults.
    pub fn new() -> Self {
        Self {
            id: 0,
            flags: 0,
            desired_present_time: 0,
            is_auto_timestamp: true,
            frame_timeline_info: FrameTimelineInfo::default(),
            apply_token: StrongBinder::NULL,
            may_contain_buffer: false,
            log_call_points: false,
            display_states: Vec::new(),
            composer_states: Vec::new(),
            input_window_commands: InputWindowCommands::default(),
            uncache_buffers: Vec::new(),
            merged_transaction_ids: Vec::new(),
            has_listener_callbacks: false,
            listener_callbacks: Vec::new(),
        }
    }

    /// `TransactionState::writeToParcel`.
    pub fn write(&self, p: &mut WireParcel) -> R<()> {
        p.write_u64(self.id);
        p.write_u32(self.flags);
        p.write_i64(self.desired_present_time);
        p.write_bool(self.is_auto_timestamp);
        self.frame_timeline_info.write_parcelable(p)?;
        p.write_strong_binder(&self.apply_token);
        p.write_bool(self.may_contain_buffer);
        p.write_bool(self.log_call_points);
        p.write_len(self.display_states.len(), "display states")?;
        self.display_states.iter().for_each(|d| d.write(p));
        p.write_len(self.composer_states.len(), "composer states")?;
        for c in &self.composer_states {
            c.write(p)?;
        }
        self.input_window_commands.write(p)?;
        p.write_len(self.uncache_buffers.len(), "uncache buffers")?;
        for u in &self.uncache_buffers {
            p.write_strong_binder(&u.token);
            p.write_u64(u.id);
        }
        p.write_len(self.merged_transaction_ids.len(), "merged transaction ids")?;
        self.merged_transaction_ids
            .iter()
            .for_each(|&i| p.write_u64(i));
        p.write_bool(self.has_listener_callbacks);
        p.write_len(self.listener_callbacks.len(), "listener callbacks")?;
        for l in &self.listener_callbacks {
            l.write(p)?;
        }
        Ok(())
    }

    /// `TransactionState::readFromParcel`.
    pub fn read(r: &mut Reader<'_>) -> R<Self> {
        let id = r.read_u64()?;
        let flags = r.read_u32()?;
        let desired_present_time = r.read_i64()?;
        let is_auto_timestamp = r.read_bool()?;
        let frame_timeline_info = FrameTimelineInfo::read_parcelable(r)?;
        let apply_token = r.read_strong_binder(true, "applyToken")?;
        let may_contain_buffer = r.read_bool()?;
        let log_call_points = r.read_bool()?;
        let n = r.read_count("display state count")?;
        let display_states = (0..n)
            .map(|_| DisplayState::read(r))
            .collect::<R<Vec<_>>>()?;
        let n = r.read_count("composer state count")?;
        let composer_states = (0..n).map(|_| LayerState::read(r)).collect::<R<Vec<_>>>()?;
        let input_window_commands = InputWindowCommands::read(r)?;
        let n = r.read_count("uncache buffer count")?;
        let uncache_buffers = (0..n)
            .map(|_| {
                Ok(UncacheBuffer {
                    token: r.read_strong_binder(false, "uncache token")?,
                    id: r.read_u64()?,
                })
            })
            .collect::<R<Vec<_>>>()?;
        let n = r.read_count("merged transaction id count")?;
        let merged_transaction_ids = (0..n).map(|_| r.read_u64()).collect::<R<Vec<_>>>()?;
        let has_listener_callbacks = r.read_bool()?;
        let n = r.read_count("listener callback count")?;
        let listener_callbacks = (0..n)
            .map(|_| ListenerCallbacks::read(r, false))
            .collect::<R<Vec<_>>>()?;
        Ok(Self {
            id,
            flags,
            desired_present_time,
            is_auto_timestamp,
            frame_timeline_info,
            apply_token,
            may_contain_buffer,
            log_call_points,
            display_states,
            composer_states,
            input_window_commands,
            uncache_buffers,
            merged_transaction_ids,
            has_listener_callbacks,
            listener_callbacks,
        })
    }

    /// The Parcel `BpSurfaceComposer::setTransactionState` sends: the interface token, then the state.
    pub fn encode_call(&self) -> R<WireParcel> {
        let mut p = WireParcel::new();
        p.write_interface_token(SURFACE_COMPOSER_DESCRIPTOR)?;
        self.write(&mut p)?;
        Ok(p)
    }

    /// `BnSurfaceComposer::onTransact(SET_TRANSACTION_STATE)`: enforce the interface, read the state, and require the parcel consumed.
    pub fn decode_call(p: &WireParcel) -> R<Self> {
        let mut r = p.reader();
        r.enforce_interface(SURFACE_COMPOSER_DESCRIPTOR)?;
        let s = Self::read(&mut r)?;
        r.finish()?;
        Ok(s)
    }
}

impl Default for TransactionState {
    fn default() -> Self {
        Self::new()
    }
}

/// One listener entry of a client `Transaction`'s parcel: the listener, its callback ids, and the surfaces those callbacks cover (`CallbackInfo`).
#[derive(Clone, Debug, PartialEq)]
pub struct ClientCallbacks {
    /// The `ITransactionCompletedListener` binder.
    pub listener: StrongBinder,
    /// Its callback ids.
    pub callback_ids: Vec<CallbackId>,
    /// The surfaces the callbacks cover (`SurfaceControl::writeToParcel` each).
    pub surfaces: Vec<SurfaceControlParcel>,
}

/// `SurfaceComposerClient::Transaction` as it parcels (`Transaction::writeToParcel` / `readFromParcel`, SurfaceComposerClient.cpp:865-930): the `TransactionState`, then a uint32 count of
/// listener entries. Java's `SurfaceControl.Transaction.writeToParcel` (SurfaceControl.java:5276-5283) writes its own non-null int before these bytes; that int is the caller's to read.
/// The parcel carries no length: its end is known only by reading it whole, which is why the window service frames a post-draw Transaction with this same reader.
#[derive(Clone, Debug, PartialEq)]
pub struct Transaction {
    /// The state `setTransactionState` would send.
    pub state: TransactionState,
    /// `mListenerCallbacks`.
    pub listener_callbacks: Vec<ClientCallbacks>,
}

impl Transaction {
    /// `Transaction::readFromParcel`.
    pub fn read(r: &mut Reader<'_>) -> R<Self> {
        let state = TransactionState::read(r)?;
        let n = r.read_count("listener count")?;
        let listener_callbacks = (0..n)
            .map(|_| {
                let listener = r.read_strong_binder(true, "listener")?;
                let callback_ids = read_callback_ids(r)?;
                let m = r.read_count("surface count")?;
                let surfaces = (0..m)
                    .map(|_| SurfaceControlParcel::read(r))
                    .collect::<R<Vec<_>>>()?;
                Ok(ClientCallbacks {
                    listener,
                    callback_ids,
                    surfaces,
                })
            })
            .collect::<R<Vec<_>>>()?;
        Ok(Self {
            state,
            listener_callbacks,
        })
    }

    /// `Transaction::writeToParcel`.
    pub fn write(&self, p: &mut WireParcel) -> R<()> {
        self.state.write(p)?;
        p.write_len(self.listener_callbacks.len(), "listener entries")?;
        for l in &self.listener_callbacks {
            p.write_strong_binder(&l.listener);
            write_callback_ids(p, &l.callback_ids)?;
            p.write_len(l.surfaces.len(), "surfaces")?;
            for s in &l.surfaces {
                s.write(p)?;
            }
        }
        Ok(())
    }

    /// The state `Transaction::apply` sends (SurfaceComposerClient.cpp:1120-1145): each listener's callbacks go to the state when no surface is named, else to each named layer's state
    /// (`TransactionState::getLayerState`, TransactionState.cpp:230-247: the layer's existing entry, else a new one for its handle and id), marked `eHasListenerCallbacksChanged`.
    pub fn into_state(self) -> TransactionState {
        let mut state = self.state;
        state.has_listener_callbacks = !self.listener_callbacks.is_empty();
        for l in self.listener_callbacks {
            if l.callback_ids.is_empty() {
                continue;
            }
            if l.surfaces.is_empty() {
                state.listener_callbacks.push(ListenerCallbacks {
                    listener: l.listener,
                    callback_ids: l.callback_ids,
                });
                continue;
            }
            for sc in &l.surfaces {
                let at = match state
                    .composer_states
                    .iter()
                    .position(|s| s.surface == sc.handle)
                {
                    Some(at) => at,
                    None => {
                        let mut s = LayerState::new();
                        s.surface = sc.handle;
                        s.layer_id = sc.layer_id;
                        state.composer_states.push(s);
                        state.composer_states.len() - 1
                    }
                };
                let s = &mut state.composer_states[at];
                s.what |= crate::state::what::E_HAS_LISTENER_CALLBACKS_CHANGED;
                s.listeners.push(ListenerCallbacks {
                    listener: l.listener,
                    callback_ids: l.callback_ids.clone(),
                });
            }
        }
        state
    }

    /// A Transaction inside a libbinder parcel's raw data (`data`, `object_count` objects inline, as `data_with_objects` gives them) starting at byte `start`: its end, and the offsets of the
    /// objects inside it. For a reader that only forwards the bytes (the window service).
    pub fn frame(
        data: &[u8],
        object_count: usize,
        start: usize,
    ) -> Result<(usize, Vec<usize>), String> {
        let offsets = crate::wire::scan_objects(data, object_count).map_err(|e| e.to_string())?;
        let p = WireParcel::from_wire(data.to_vec(), offsets).map_err(|e| e.to_string())?;
        let end = Self::end_in(&p, start).map_err(|e| e.to_string())?;
        let inside = p
            .objects()
            .iter()
            .copied()
            .filter(|&o| o >= start && o < end)
            .collect();
        Ok((end, inside))
    }

    /// Where a Transaction that starts at byte `start` of `p` ends: the position after reading it whole (the read is the only way to know, see [`Transaction`]).
    pub fn end_in(p: &WireParcel, start: usize) -> R<usize> {
        let mut r = p.reader();
        r.skip_to(start)?;
        Self::read(&mut r)?;
        Ok(r.position())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parcel::{BINDER_TYPE_BINDER, FdObject, FlatObject};
    use crate::state::{BufferData, BufferReleaseEndpoint, CallbackId, GraphicBuffer, what};

    fn local(cookie: u64) -> StrongBinder {
        StrongBinder {
            obj: FlatObject {
                ty: BINDER_TYPE_BINDER,
                flags: 0,
                binder: cookie + 1,
                cookie,
            },
            stability: 0,
        }
    }

    /// Size of the interface token: 3 header words, the length word, and "android.ui.ISurfaceComposer" (27 UTF-16 units + NUL = 56 bytes, already a multiple of 4).
    const TOKEN_BYTES: usize = 12 + 4 + 56;

    /// An empty transaction is, field by field: id 8, flags 4, present time 8, isAuto 4, FrameTimelineInfo (flag 4 + size word 4 + six fields 8+4+8+4+8+8), null token 24+4,
    /// two bools 8, display count 4, composer count 4, focus count 4, window-info listener count 4, uncache count 4, merged count 4, hasListenerCallbacks 4, callback count 4.
    #[test]
    fn empty_transaction_size_matches_the_field_list() {
        let want = 8
            + 4
            + 8
            + 4
            + (4 + 4 + 8 + 4 + 8 + 4 + 8 + 8)
            + (24 + 4)
            + 8
            + 4
            + 4
            + 4
            + 4
            + 4
            + 4
            + 4
            + 4;
        assert_eq!(want, 140);
        let p = TransactionState::new().encode_call().unwrap();
        assert_eq!(p.data().len(), TOKEN_BYTES + want);
        assert!(
            p.objects().is_empty(),
            "the null apply token is not in the object table (libbinder omits binder-word-0 objects)"
        );
        assert_eq!(
            TransactionState::decode_call(&p).unwrap(),
            TransactionState::new()
        );
    }

    /// layer_state_t starts: surface binder (24 + stability 4), layerId i32, what u64, x f32, y f32, z i32, layerStack u32, flags u32, mask u32, matrix dsdx dtdx dtdy dsdy.
    #[test]
    fn layer_state_field_order_prefix() {
        let mut s = LayerState::new();
        s.surface = local(5);
        s.layer_id = 9;
        s.what = what::E_POSITION_CHANGED | what::E_BUFFER_CHANGED;
        s.x = 1.5;
        s.y = -2.0;
        s.z = 3;
        s.layer_stack = 4;
        s.flags = 5;
        s.mask = 6;
        let mut p = WireParcel::new();
        s.write(&mut p).unwrap();
        let mut want = Vec::new();
        want.extend_from_slice(&0x7362_2a85u32.to_le_bytes());
        want.extend_from_slice(&0u32.to_le_bytes());
        want.extend_from_slice(&6u64.to_le_bytes());
        want.extend_from_slice(&5u64.to_le_bytes());
        want.extend_from_slice(&0i32.to_le_bytes());
        want.extend_from_slice(&9i32.to_le_bytes());
        want.extend_from_slice(&(0x1u64 | 0x20_0000).to_le_bytes());
        want.extend_from_slice(&1.5f32.to_le_bytes());
        want.extend_from_slice(&(-2.0f32).to_le_bytes());
        for v in [3i32, 4, 5, 6] {
            want.extend_from_slice(&v.to_le_bytes());
        }
        for v in [1.0f32, 0.0, 0.0, 1.0] {
            want.extend_from_slice(&v.to_le_bytes());
        }
        assert_eq!(&p.data()[..want.len()], want);
        // the self-test: the same bytes with x and y swapped (a wrong field order) are not what the writer produces, and decode to other values
        let mut swapped = want.clone();
        let (x_at, y_at) = (4 + 4 + 8 + 8 + 4 + 4 + 8, 4 + 4 + 8 + 8 + 4 + 4 + 8 + 4);
        let (x, y) = (
            swapped[x_at..x_at + 4].to_vec(),
            swapped[y_at..y_at + 4].to_vec(),
        );
        swapped[x_at..x_at + 4].copy_from_slice(&y);
        swapped[y_at..y_at + 4].copy_from_slice(&x);
        assert_ne!(&p.data()[..swapped.len()], swapped);
    }

    fn bbq_transaction() -> TransactionState {
        let layer = local(0x10);
        let listener = local(0x20);
        let mut s = LayerState::new();
        s.surface = layer;
        s.layer_id = 3;
        s.what = what::E_BUFFER_CHANGED
            | what::E_BUFFER_CROP_CHANGED
            | what::E_HAS_LISTENER_CALLBACKS_CHANGED
            | what::E_BUFFER_RELEASE_CHANNEL_CHANGED
            | what::E_ALPHA_CHANGED;
        s.color = [0.0, 0.0, 0.0, 0.75];
        s.buffer_crop = crate::state::Rect {
            left: 0,
            top: 0,
            right: 64,
            bottom: 48,
        };
        s.buffer_release_channel = Some(BufferReleaseEndpoint {
            name: "bbq#1".into(),
            fd: FdObject::new(21, false),
        });
        s.buffer_data = Some(BufferData {
            flags: 0x06,
            buffer: Some(GraphicBuffer {
                width: 64,
                height: 48,
                stride: 64,
                format: 1,
                layer_count: 1,
                usage: 0x1_0000_0033,
                id: 0xAABB_0000_0001,
                generation_number: 0,
                fds: vec![FdObject::new(30, false), FdObject::new(31, false)],
                ints: vec![7, 8, 9],
            }),
            acquire_fence: Some(crate::state::Fence(None)),
            frame_number: 12,
            release_buffer_listener: listener,
            release_buffer_endpoint: listener,
            cached_buffer_token: listener,
            cached_buffer_id: 0xAABB_0000_0001,
            has_barrier: true,
            barrier_frame_number: 11,
            producer_id: 4,
            dequeue_time: 123_456,
        });
        s.listeners = vec![ListenerCallbacks {
            listener,
            callback_ids: vec![CallbackId {
                id: 5,
                ty: CallbackId::ON_COMPLETE,
            }],
        }];
        let mut t = TransactionState::new();
        t.id = 0x1_0000_0002;
        t.flags = E_ONE_WAY;
        t.is_auto_timestamp = false;
        t.desired_present_time = 99;
        t.apply_token = local(0x30);
        t.may_contain_buffer = true;
        t.composer_states = vec![s];
        t.uncache_buffers = vec![UncacheBuffer {
            token: listener,
            id: 77,
        }];
        t.merged_transaction_ids = vec![1, 2];
        t.has_listener_callbacks = true;
        t.listener_callbacks = vec![ListenerCallbacks {
            listener,
            callback_ids: vec![CallbackId {
                id: 6,
                ty: CallbackId::ON_COMMIT,
            }],
        }];
        t
    }

    #[test]
    fn bbq_like_transaction_round_trips_with_every_object() {
        let t = bbq_transaction();
        let p = t.encode_call().unwrap();
        assert_eq!(TransactionState::decode_call(&p).unwrap(), t);
        // by hand: apply token 1; layer surface 1; layer listener 1; buffer fds 2; BufferData binders (listener, endpoint, cache token) 3; release channel fd 1; uncache token 1; transaction listener 1.
        // The null trusted-presentation listener is written but, its binder word being 0, not listed.
        assert_eq!(p.objects().len(), 11);
    }

    #[test]
    fn decode_requires_the_whole_parcel_and_the_right_interface() {
        let p = bbq_transaction().encode_call().unwrap();
        let (mut data, objects) = p.clone().into_parts();
        data.extend_from_slice(&[0; 4]);
        assert!(
            TransactionState::decode_call(&WireParcel::from_wire(data, objects).unwrap()).is_err(),
            "trailing data"
        );
        let mut other = WireParcel::new();
        other
            .write_interface_token("android.ui.ISomethingElse")
            .unwrap();
        assert!(TransactionState::decode_call(&other).is_err());
    }

    /// A parcel cut short anywhere fails to decode, never panics.
    #[test]
    fn truncation_is_an_error_at_every_length() {
        let p = bbq_transaction().encode_call().unwrap();
        let (data, objects) = p.into_parts();
        for cut in 0..data.len() {
            let objs: Vec<usize> = objects.iter().copied().filter(|o| o + 24 <= cut).collect();
            let q = WireParcel::from_wire(data[..cut].to_vec(), objs).unwrap();
            assert!(TransactionState::decode_call(&q).is_err(), "cut at {cut}");
        }
    }
}
