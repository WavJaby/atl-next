// SPDX-License-Identifier: MIT
//! A Transaction marshalled by AOSP's own libgui C++ writer (android-16.0.0_r1, captured by `xtask test compositor --golden`; testdata/transaction_libgui_writer.dump holds data + object table, `WireParcel::to_dump`).
//! Java built it with real `SurfaceControl.Transaction` setters (values in CompositorTruth.java `Fixture`); the codec must decode it, and encode the state again byte for byte.
//! Object fields (binder / cookie pointers) are those of the capturing process; they are carried, not interpreted.

use atl_compositor::parcel::WireParcel;
use atl_compositor::state::what;
use atl_compositor::transaction::TransactionState;

const DUMP: &[u8] = include_bytes!("../testdata/transaction_libgui_writer.dump");

/// Java's `Transaction.writeToParcel` = the int 1, `TransactionState::writeToParcel`, then libgui's listener suffix (a uint32 count and entries); the state part is what `setTransactionState` sends.
fn decode() -> (TransactionState, usize, WireParcel) {
    let p = WireParcel::from_dump(DUMP).expect("dump parses");
    let mut r = p.reader();
    assert_eq!(r.read_i32().expect("leading int"), 1);
    let state = TransactionState::read(&mut r).expect("libgui's bytes decode");
    let end = r.position();
    (state, end, p)
}

#[test]
fn libgui_written_state_decodes_to_what_java_set() {
    let (s, _, _) = decode();
    // values from CompositorTruth.java: setEarlyWakeupStart (0x8) + setAnimationTransaction (0x2), setDesiredPresentTimeNanos, setFrameTimelineVsync
    assert_eq!(s.flags, 0x0a);
    assert_eq!(s.desired_present_time, 987_654);
    assert!(!s.is_auto_timestamp);
    assert_eq!(s.frame_timeline_info.vsync_id, 1234);
    assert_eq!(s.composer_states.len(), 3);
    let l0 = &s.composer_states[0];
    assert_eq!(l0.layer_id, 11);
    assert_eq!((l0.x, l0.y, l0.z, l0.layer_stack), (10.5, -3.25, 7, 5));
    assert_eq!(l0.color[3], 0.25);
    assert_eq!(
        (
            l0.matrix_dsdx,
            l0.matrix_dtdx,
            l0.matrix_dtdy,
            l0.matrix_dsdy
        ),
        (2.0, 0.5, 0.75, 3.0)
    );
    assert_eq!(
        (l0.crop.left, l0.crop.top, l0.crop.right, l0.crop.bottom),
        (1.0, 2.0, 301.0, 202.0)
    );
    assert_eq!(l0.flags, 0x2 | 0x80 | 0x40);
    assert_eq!(l0.metadata, vec![(3, vec![77, 0, 0, 0])]);
    assert_eq!(l0.parent.as_ref().map(|p| p.layer_id), Some(14));
    assert_ne!(l0.what & what::E_POSITION_CHANGED, 0);
    let l1 = &s.composer_states[1];
    assert_eq!((l1.layer_id, l1.z), (12, -3));
    assert_eq!(l1.relative_layer.as_ref().map(|r| r.layer_id), Some(11));
    let l2 = &s.composer_states[2];
    assert!(
        l2.buffer_data.is_none(),
        "unsetBuffer resets the Transaction's buffer state only: no BufferData on the wire"
    );
}

#[test]
fn state_re_encodes_to_libgui_bytes() {
    let (s, end, p) = decode();
    let mut out = WireParcel::new();
    out.write_i32(1);
    s.write(&mut out).expect("encodes");
    assert_eq!(out.data(), &p.data()[..end], "the state's bytes");
    let objects: Vec<usize> = p.objects().iter().copied().filter(|o| *o < end).collect();
    assert_eq!(out.objects(), objects, "the state's object table");
}
