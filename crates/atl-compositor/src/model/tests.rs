// SPDX-License-Identifier: MIT
//! Model tests. Expected values are derived from the rules in the module docs (BBQ's accounting, SurfaceFlinger's z-order and release semantics), not from the model's own output.

use super::*;
use crate::parcel::{BINDER_TYPE_BINDER, FlatObject};
use crate::state::{BufferReleaseEndpoint, GraphicBuffer};

fn local(cookie: u64) -> StrongBinder {
    StrongBinder {
        obj: FlatObject {
            ty: BINDER_TYPE_BINDER,
            flags: 0,
            binder: cookie ^ 0xff,
            cookie,
        },
        stability: 0,
    }
}

fn key(b: &StrongBinder) -> BinderKey {
    b.key().expect("non-null")
}

fn gbuf(id: u64, w: i32, h: i32) -> GraphicBuffer {
    GraphicBuffer {
        width: w,
        height: h,
        stride: w,
        format: 1,
        layer_count: 1,
        usage: 0x33,
        id,
        generation_number: 0,
        fds: vec![FdObject::new(40 + id as i32, false)],
        ints: vec![],
    }
}

fn spec(handle: StrongBinder, id: i32, parent: Option<BinderKey>) -> LayerSpec {
    LayerSpec {
        handle,
        id,
        name: format!("layer{id}"),
        parent,
        fills_color: false,
    }
}

/// What BBQ's `acquireNextBufferLocked` puts in a transaction: the buffer (first time) or its cached id, frame number, release listener, callback registration, backpressure.
/// One argument per thing BBQ varies; a struct would only rename them at every call site.
#[allow(clippy::too_many_arguments)]
fn buffer_state(
    layer: &StrongBinder,
    listener: &StrongBinder,
    id: u64,
    frame: u64,
    w: i32,
    h: i32,
    cache_hit: bool,
    callback: i64,
) -> LayerState {
    let mut s = LayerState::new();
    s.surface = *layer;
    s.what = what::E_BUFFER_CHANGED
        | what::E_HAS_LISTENER_CALLBACKS_CHANGED
        | what::E_BUFFER_CROP_CHANGED;
    s.buffer_crop = Rect {
        left: 0,
        top: 0,
        right: w,
        bottom: h,
    };
    s.buffer_data = Some(BufferData {
        flags: buffer_data_change::CACHED_BUFFER_CHANGED | buffer_data_change::FRAME_NUMBER_CHANGED,
        buffer: (!cache_hit).then(|| gbuf(id, w, h)),
        acquire_fence: None,
        frame_number: frame,
        release_buffer_listener: *listener,
        release_buffer_endpoint: *listener,
        cached_buffer_token: *listener,
        cached_buffer_id: id,
        has_barrier: false,
        barrier_frame_number: 0,
        producer_id: 1,
        dequeue_time: -1,
    });
    s.listeners = vec![ListenerCallbacks {
        listener: *listener,
        callback_ids: vec![CallbackId {
            id: callback,
            ty: CallbackId::ON_COMPLETE,
        }],
    }];
    s
}

fn tx(states: Vec<LayerState>) -> TransactionState {
    let mut t = TransactionState::new();
    t.may_contain_buffer = true;
    t.composer_states = states;
    t
}

fn flags_state(layer: &StrongBinder, flags: u32, mask: u32) -> LayerState {
    let mut s = LayerState::new();
    s.surface = *layer;
    s.what = what::E_FLAGS_CHANGED;
    s.flags = flags;
    s.mask = mask;
    s
}

fn channel_state(layer: &StrongBinder, fd: i32) -> LayerState {
    let mut s = LayerState::new();
    s.surface = *layer;
    s.what = what::E_BUFFER_RELEASE_CHANNEL_CHANGED;
    s.buffer_release_channel = Some(BufferReleaseEndpoint {
        name: "ch".into(),
        fd: FdObject::new(fd, false),
    });
    s
}

fn releases(calls: &[ClientCall]) -> Vec<ReleaseCallbackId> {
    calls
        .iter()
        .filter_map(|c| match c {
            ClientCall::ReleaseBuffer { release, .. }
            | ClientCall::ReleaseOnChannel { release, .. } => Some(release.callback_id),
            _ => None,
        })
        .collect()
}

/// A BBQ-like producer against the model for 25 frames: its accounting (acquired = queued - released) must stay under `max + 2` the whole run, which only holds when every replaced buffer is released.
#[test]
fn every_acquired_buffer_is_released_and_the_producer_never_stalls() {
    let layer = local(1);
    let listener = local(2);
    let mut c = Compositor::new(CompositorConfig::default());
    c.create_layer(spec(layer, 7, None)).unwrap();
    c.queue_transaction(tx(vec![
        flags_state(
            &layer,
            layer_flags::ENABLE_BACKPRESSURE,
            layer_flags::ENABLE_BACKPRESSURE,
        ),
        channel_state(&layer, 90),
    ]));
    c.on_frame(1);

    let mut acquired = 0u64;
    let mut released = 0u64;
    let mut stall_limit_hit = false;
    for frame in 1..=25u64 {
        // the producer queues frame N when it is under its limit, as BBQ's acquireNextBufferLocked does
        let limit = u64::from(c.config().max_acquired_buffer_count) + 2;
        if acquired - released >= limit {
            stall_limit_hit = true;
        }
        let first_use = frame <= 3; // three buffers rotate: ids 101..=103
        let id = 100 + ((frame - 1) % 3) + 1;
        c.queue_transaction(tx(vec![buffer_state(
            &layer,
            &listener,
            id,
            frame,
            64,
            48,
            !first_use,
            frame as i64,
        )]));
        acquired += 1;
        let out = c.on_frame(frame as i64 * 16);
        released += releases(&out.calls).len() as u64;
        assert!(out.scene.is_some());
        assert_eq!(out.scene.as_ref().unwrap().quads.len(), 1);
    }
    assert!(
        !stall_limit_hit,
        "producer reached acquired >= max + 2: releases are missing"
    );
    // the buffer on screen is the only one not released
    assert_eq!(released, acquired - 1);
    assert_eq!(c.counters().buffers_acquired, 25);
    assert_eq!(c.counters().buffers_released, 24);
    assert_eq!(c.counters().cache_misses, 0);
    let end = c.destroy_layer(key(&layer));
    assert_eq!(
        releases(&end).len(),
        1,
        "destroying the layer releases the shown buffer"
    );
    assert_eq!(c.counters().buffers_released, 25);
}

/// Release ids are the buffer id and frame number of the replaced buffer, and with a channel registered they go to the channel, not the listener.
#[test]
fn release_goes_to_the_channel_with_the_replaced_buffers_id() {
    let layer = local(1);
    let listener = local(2);
    let mut c = Compositor::new(CompositorConfig::default());
    c.create_layer(spec(layer, 7, None)).unwrap();
    c.queue_transaction(tx(vec![channel_state(&layer, 90)]));
    c.queue_transaction(tx(vec![buffer_state(
        &layer, &listener, 201, 1, 8, 8, false, 1,
    )]));
    c.on_frame(10);
    c.queue_transaction(tx(vec![buffer_state(
        &layer, &listener, 202, 2, 8, 8, false, 2,
    )]));
    let out = c.on_frame(20);
    let rel: Vec<_> = out
        .calls
        .iter()
        .filter(|c| !matches!(c, ClientCall::TransactionCompleted { .. }))
        .collect();
    assert_eq!(rel.len(), 1);
    match rel[0] {
        ClientCall::ReleaseOnChannel { channel, release } => {
            assert_eq!(channel.fd, 90);
            assert_eq!(
                release.callback_id,
                ReleaseCallbackId {
                    buffer_id: 201,
                    frame_number: 1
                }
            );
            assert_eq!(release.current_max_acquired_buffer_count, 2);
        }
        other => panic!("expected a channel release, got {other:?}"),
    }
}

#[test]
fn release_goes_to_the_listener_without_a_channel() {
    let layer = local(1);
    let listener = local(2);
    let mut c = Compositor::new(CompositorConfig::default());
    c.create_layer(spec(layer, 7, None)).unwrap();
    c.queue_transaction(tx(vec![buffer_state(
        &layer, &listener, 201, 1, 8, 8, false, 1,
    )]));
    c.on_frame(10);
    c.queue_transaction(tx(vec![buffer_state(
        &layer, &listener, 202, 2, 8, 8, false, 2,
    )]));
    let out = c.on_frame(20);
    assert!(out.calls.iter().any(|c| matches!(c, ClientCall::ReleaseBuffer { listener: l, release } if *l == listener && release.callback_id.buffer_id == 201)));
}

/// With backpressure a layer takes one new buffer per tick; the second transaction waits and the first buffer is released only when the second latches.
#[test]
fn backpressure_latches_one_buffer_per_tick() {
    let layer = local(1);
    let listener = local(2);
    let mut c = Compositor::new(CompositorConfig::default());
    c.create_layer(spec(layer, 7, None)).unwrap();
    c.queue_transaction(tx(vec![
        flags_state(
            &layer,
            layer_flags::ENABLE_BACKPRESSURE,
            layer_flags::ENABLE_BACKPRESSURE,
        ),
        channel_state(&layer, 90),
    ]));
    c.queue_transaction(tx(vec![buffer_state(
        &layer, &listener, 301, 1, 8, 8, false, 1,
    )]));
    c.queue_transaction(tx(vec![buffer_state(
        &layer, &listener, 302, 2, 8, 8, false, 2,
    )]));
    let out = c.on_frame(10);
    assert_eq!(
        c.pending_transactions(),
        1,
        "the second buffer waits for the next tick"
    );
    assert!(releases(&out.calls).is_empty());
    assert_eq!(
        out.scene.unwrap().quads[0].buffer().unwrap().frame_number,
        1
    );
    let out = c.on_frame(20);
    assert_eq!(c.pending_transactions(), 0);
    assert_eq!(
        releases(&out.calls),
        [ReleaseCallbackId {
            buffer_id: 301,
            frame_number: 1
        }]
    );
    assert_eq!(
        out.scene.unwrap().quads[0].buffer().unwrap().frame_number,
        2
    );
}

/// Without backpressure the later buffer replaces the earlier one within the tick, and the replaced buffer is still released.
#[test]
fn without_backpressure_the_replaced_buffer_is_released_in_the_same_tick() {
    let layer = local(1);
    let listener = local(2);
    let mut c = Compositor::new(CompositorConfig::default());
    c.create_layer(spec(layer, 7, None)).unwrap();
    c.queue_transaction(tx(vec![buffer_state(
        &layer, &listener, 301, 1, 8, 8, false, 1,
    )]));
    c.queue_transaction(tx(vec![buffer_state(
        &layer, &listener, 302, 2, 8, 8, false, 2,
    )]));
    let out = c.on_frame(10);
    assert_eq!(
        releases(&out.calls),
        [ReleaseCallbackId {
            buffer_id: 301,
            frame_number: 1
        }]
    );
    assert_eq!(
        out.scene.unwrap().quads[0].buffer().unwrap().frame_number,
        2
    );
}

/// Cache protocol: the first send carries the buffer, later ones only the id; a miss releases the buffer by its cached id at once.
#[test]
fn buffer_cache_hit_miss_and_uncache() {
    let layer = local(1);
    let listener = local(2);
    let mut c = Compositor::new(CompositorConfig::default());
    c.create_layer(spec(layer, 7, None)).unwrap();
    c.queue_transaction(tx(vec![buffer_state(
        &layer, &listener, 401, 1, 8, 8, false, 1,
    )]));
    c.on_frame(10);
    // cache hit: same id, no buffer
    c.queue_transaction(tx(vec![buffer_state(
        &layer, &listener, 401, 2, 8, 8, true, 2,
    )]));
    let out = c.on_frame(20);
    assert_eq!(out.scene.unwrap().quads[0].buffer().unwrap().buffer.id, 401);
    assert_eq!(c.counters().cache_misses, 0);
    // uncache then reuse the id: a miss
    let mut t = tx(vec![]);
    t.uncache_buffers = vec![crate::transaction::UncacheBuffer {
        token: listener,
        id: 401,
    }];
    c.queue_transaction(t);
    c.on_frame(30);
    c.queue_transaction(tx(vec![buffer_state(
        &layer, &listener, 401, 3, 8, 8, true, 3,
    )]));
    let out = c.on_frame(40);
    assert_eq!(c.counters().cache_misses, 1);
    let rel = releases(&out.calls);
    assert!(
        rel.contains(&ReleaseCallbackId {
            buffer_id: 401,
            frame_number: 3
        }),
        "the missed buffer is released by its cached id: {rel:?}"
    );
}

/// The completion answers each registered callback once, with the layer's handle and `eventStats.frameNumber` = the latched frame (BBQ's transactionCallback needs both).
#[test]
fn transaction_completed_carries_callback_ids_and_frame_numbers() {
    let layer = local(1);
    let listener = local(2);
    let mut c = Compositor::new(CompositorConfig::default());
    c.create_layer(spec(layer, 7, None)).unwrap();
    c.queue_transaction(tx(vec![buffer_state(
        &layer, &listener, 501, 11, 8, 8, false, 77,
    )]));
    let out = c.on_frame(1234);
    let [ClientCall::TransactionCompleted { listener: l, stats }] = out.calls.as_slice() else {
        panic!("expected one completion, got {:?}", out.calls)
    };
    assert_eq!(*l, listener);
    let ts = &stats.transaction_stats[0];
    assert_eq!(
        ts.callback_ids,
        [CallbackId {
            id: 77,
            ty: CallbackId::ON_COMPLETE
        }]
    );
    assert_eq!(ts.latch_time, 1234);
    assert_eq!(ts.surface_stats.len(), 1);
    assert_eq!(ts.surface_stats[0].surface_control, layer);
    assert_eq!(ts.surface_stats[0].event_stats.frame_number, 11);
    assert_eq!(ts.surface_stats[0].current_max_acquired_buffer_count, 2);
    // the next frame has nothing to answer
    assert!(c.on_frame(2000).calls.is_empty());
}

/// A transaction-level callback (no layer) completes with no surface stats; two transactions of one listener share one call.
#[test]
fn transaction_level_callbacks_and_grouping() {
    let listener = local(2);
    let mut c = Compositor::new(CompositorConfig::default());
    let mut t1 = tx(vec![]);
    t1.has_listener_callbacks = true;
    t1.listener_callbacks = vec![ListenerCallbacks {
        listener,
        callback_ids: vec![
            CallbackId {
                id: 1,
                ty: CallbackId::ON_COMMIT,
            },
            CallbackId {
                id: 2,
                ty: CallbackId::ON_COMPLETE,
            },
        ],
    }];
    let mut t2 = t1.clone();
    t2.listener_callbacks[0].callback_ids = vec![CallbackId {
        id: 3,
        ty: CallbackId::ON_COMPLETE,
    }];
    c.queue_transaction(t1);
    c.queue_transaction(t2);
    let out = c.on_frame(5);
    let [ClientCall::TransactionCompleted { stats, .. }] = out.calls.as_slice() else {
        panic!("{:?}", out.calls)
    };
    assert_eq!(stats.transaction_stats.len(), 2);
    assert_eq!(stats.transaction_stats[0].callback_ids.len(), 2);
    assert!(
        stats
            .transaction_stats
            .iter()
            .all(|t| t.surface_stats.is_empty())
    );
}

/// Fields the software path does not use are counted by AOSP's name, never dropped silently.
#[test]
fn ignored_state_is_counted_by_name() {
    let layer = local(1);
    let mut c = Compositor::new(CompositorConfig::default());
    c.create_layer(spec(layer, 7, None)).unwrap();
    let mut s = LayerState::new();
    s.surface = layer;
    s.what =
        what::E_SHADOW_RADIUS_CHANGED | what::E_CORNER_RADIUS_CHANGED | what::E_POSITION_CHANGED;
    c.queue_transaction(tx(vec![s.clone(), s]));
    c.on_frame(1);
    assert_eq!(
        c.counters().ignored.get("E_SHADOW_RADIUS_CHANGED"),
        Some(&2)
    );
    assert_eq!(
        c.counters().ignored.get("E_CORNER_RADIUS_CHANGED"),
        Some(&2)
    );
    assert_eq!(
        c.counters().ignored.get("E_POSITION_CHANGED"),
        None,
        "applied bits are not counted"
    );
}

/// Every bit of `layer_state_t::what` the header defines has a name in the table, once, as a single bit.
#[test]
fn what_table_is_complete_and_single_bit() {
    assert_eq!(what::ALL.len(), 55);
    let mut seen = 0u64;
    for &(bit, name) in what::ALL {
        assert_eq!(bit.count_ones(), 1, "{name}");
        assert_eq!(seen & bit, 0, "{name} repeated");
        seen |= bit;
    }
    // bits 0..=54 contiguous: the last defined bit is 0x40_0000_0000_0000 = 1 << 54
    assert_eq!(seen, (1u64 << 55) - 1);
}

#[test]
fn unknown_layer_and_null_handle_are_refused() {
    let mut c = Compositor::new(CompositorConfig::default());
    assert_eq!(
        c.create_layer(spec(StrongBinder::NULL, 1, None)),
        Err(ModelError::NullHandle)
    );
    let l = local(1);
    c.create_layer(spec(l, 1, None)).unwrap();
    assert_eq!(
        c.create_layer(spec(l, 2, None)),
        Err(ModelError::DuplicateLayer(key(&l)))
    );
    // a buffer for a layer that does not exist is released, not lost
    let ghost = local(9);
    let listener = local(2);
    c.queue_transaction(tx(vec![buffer_state(
        &ghost, &listener, 601, 1, 8, 8, false, 1,
    )]));
    let out = c.on_frame(1);
    assert_eq!(c.counters().unknown_layer, 1);
    assert_eq!(
        releases(&out.calls),
        [ReleaseCallbackId {
            buffer_id: 601,
            frame_number: 1
        }]
    );
}

fn pos_state(layer: &StrongBinder, x: f32, y: f32) -> LayerState {
    let mut s = LayerState::new();
    s.surface = *layer;
    s.what = what::E_POSITION_CHANGED;
    s.x = x;
    s.y = y;
    s
}

fn z_state(layer: &StrongBinder, z: i32) -> LayerState {
    let mut s = LayerState::new();
    s.surface = *layer;
    s.what = what::E_LAYER_CHANGED;
    s.z = z;
    s
}

fn show(c: &mut Compositor, layer: &StrongBinder, id: u64, w: i32, h: i32) {
    let listener = local(99);
    c.queue_transaction(tx(vec![buffer_state(
        layer, &listener, id, 1, w, h, false, id as i64,
    )]));
}

/// Z-order and geometry: a root window with a SurfaceView child below it (z < 0) and a popup above (z >= 0), positions accumulate, a crop clips children, alpha multiplies.
#[test]
fn scene_orders_children_around_their_parent_and_places_them() {
    let (root, below, above) = (local(1), local(2), local(3));
    let mut c = Compositor::new(CompositorConfig::default());
    c.create_layer(spec(root, 1, None)).unwrap();
    c.create_layer(spec(below, 2, Some(key(&root)))).unwrap();
    c.create_layer(spec(above, 3, Some(key(&root)))).unwrap();
    let mut crop = LayerState::new();
    crop.surface = root;
    crop.what = what::E_CROP_CHANGED;
    crop.crop = FloatRect {
        left: 0.0,
        top: 0.0,
        right: 100.0,
        bottom: 80.0,
    };
    let mut alpha = LayerState::new();
    alpha.surface = root;
    alpha.what = what::E_ALPHA_CHANGED;
    alpha.color = [0.0, 0.0, 0.0, 0.5];
    c.queue_transaction(tx(vec![
        pos_state(&root, 10.0, 20.0),
        pos_state(&below, 5.0, 5.0),
        pos_state(&above, 50.0, 0.0),
        z_state(&below, -1),
        z_state(&above, 1),
        crop,
        alpha,
    ]));
    show(&mut c, &root, 11, 100, 80);
    show(&mut c, &below, 12, 40, 40);
    show(&mut c, &above, 13, 90, 90);
    let scene = c.on_frame(1).scene.unwrap();
    let ids: Vec<i32> = scene.quads.iter().map(|q| q.layer_id).collect();
    assert_eq!(ids, [2, 1, 3], "child below, parent, child above");
    let by = |id: i32| scene.quads.iter().find(|q| q.layer_id == id).unwrap();
    // root at (10,20); its 100x80 buffer
    assert_eq!(
        by(1).dst,
        FloatRect {
            left: 10.0,
            top: 20.0,
            right: 110.0,
            bottom: 100.0
        }
    );
    // child positions are relative to the parent's origin
    assert_eq!(
        by(2).dst,
        FloatRect {
            left: 15.0,
            top: 25.0,
            right: 55.0,
            bottom: 65.0
        }
    );
    assert_eq!(
        by(3).dst,
        FloatRect {
            left: 60.0,
            top: 20.0,
            right: 150.0,
            bottom: 110.0
        }
    );
    // the root's crop (layer space 0..100 x 0..80) is its clip in screen space and clips the children too
    let clip = FloatRect {
        left: 10.0,
        top: 20.0,
        right: 110.0,
        bottom: 100.0,
    };
    assert_eq!(by(3).clip, Some(clip));
    assert_eq!(by(1).clip, Some(clip));
    // alpha 0.5 on the root applies to everything below it
    assert!(
        scene
            .quads
            .iter()
            .all(|q| (q.alpha - 0.5).abs() < f32::EPSILON)
    );
}

/// A hidden layer takes its subtree out of the scene; destroying a parent does the same for what is left of the children.
#[test]
fn hidden_and_destroyed_layers_leave_the_scene() {
    let (root, child) = (local(1), local(2));
    let mut c = Compositor::new(CompositorConfig::default());
    c.create_layer(spec(root, 1, None)).unwrap();
    c.create_layer(spec(child, 2, Some(key(&root)))).unwrap();
    show(&mut c, &root, 11, 10, 10);
    show(&mut c, &child, 12, 10, 10);
    assert_eq!(c.on_frame(1).scene.unwrap().quads.len(), 2);
    c.queue_transaction(tx(vec![flags_state(
        &root,
        layer_flags::HIDDEN,
        layer_flags::HIDDEN,
    )]));
    assert_eq!(c.on_frame(2).scene.unwrap().quads.len(), 0);
    c.queue_transaction(tx(vec![flags_state(&root, 0, layer_flags::HIDDEN)]));
    assert_eq!(c.on_frame(3).scene.unwrap().quads.len(), 2);
    let calls = c.destroy_layer(key(&root));
    assert_eq!(
        releases(&calls).len(),
        1,
        "the root's shown buffer is released to its listener (no channel registered)"
    );
    let scene = c.on_frame(4).scene.unwrap();
    assert!(scene.quads.is_empty(), "the orphaned child is not drawn");
}

/// A relative layer sorts among the children of the layer it is relative to, whatever its parent.
#[test]
fn relative_layer_orders_against_its_anchor() {
    let (root, a, b) = (local(1), local(2), local(3));
    let mut c = Compositor::new(CompositorConfig::default());
    c.create_layer(spec(root, 1, None)).unwrap();
    c.create_layer(spec(a, 2, Some(key(&root)))).unwrap();
    c.create_layer(spec(b, 3, Some(key(&root)))).unwrap();
    let mut rel = LayerState::new();
    rel.surface = b;
    rel.what = what::E_RELATIVE_LAYER_CHANGED;
    rel.z = -5;
    rel.relative_layer = Some(crate::state::SurfaceControlParcel {
        client: local(50),
        handle: a,
        layer_id: 2,
        name: "a".into(),
        transform_hint: 0,
        width: 0,
        height: 0,
        format: 0,
    });
    c.queue_transaction(tx(vec![z_state(&a, 1), rel]));
    for (l, id) in [(&root, 11), (&a, 12), (&b, 13)] {
        show(&mut c, l, id, 4, 4);
    }
    let ids: Vec<i32> = c
        .on_frame(1)
        .scene
        .unwrap()
        .quads
        .iter()
        .map(|q| q.layer_id)
        .collect();
    // b is drawn below a (z -5 under its anchor), and a is above root (z 1)
    assert_eq!(ids, [1, 3, 2]);
}

/// Destination frame, buffer crop and ROT_90: the layer's rect is the destination frame, else the buffer crop's size, with width and height swapped under a 90 degree transform.
#[test]
fn quad_geometry_from_crop_destination_and_transform() {
    let (l1, l2, l3) = (local(1), local(2), local(3));
    let mut c = Compositor::new(CompositorConfig::default());
    for (l, id) in [(&l1, 1), (&l2, 2), (&l3, 3)] {
        c.create_layer(spec(*l, id, None)).unwrap();
    }
    let mut dest = LayerState::new();
    dest.surface = l1;
    dest.what = what::E_DESTINATION_FRAME_CHANGED;
    dest.destination_frame = Rect {
        left: 0,
        top: 0,
        right: 200,
        bottom: 100,
    };
    let mut rot = LayerState::new();
    rot.surface = l3;
    rot.what = what::E_BUFFER_TRANSFORM_CHANGED;
    rot.buffer_transform = 4;
    c.queue_transaction(tx(vec![dest, rot]));
    show(&mut c, &l1, 21, 64, 64);
    show(&mut c, &l2, 22, 64, 32);
    show(&mut c, &l3, 23, 64, 32);
    let scene = c.on_frame(1).scene.unwrap();
    let by = |id: i32| scene.quads.iter().find(|q| q.layer_id == id).unwrap();
    assert_eq!(
        by(1).dst,
        FloatRect {
            left: 0.0,
            top: 0.0,
            right: 200.0,
            bottom: 100.0
        }
    );
    assert_eq!(
        by(1).buffer().unwrap().src,
        FloatRect {
            left: 0.0,
            top: 0.0,
            right: 64.0,
            bottom: 64.0
        },
        "buffer crop equals the buffer here"
    );
    assert_eq!(
        by(2).dst,
        FloatRect {
            left: 0.0,
            top: 0.0,
            right: 64.0,
            bottom: 32.0
        }
    );
    assert_eq!(
        by(3).dst,
        FloatRect {
            left: 0.0,
            top: 0.0,
            right: 32.0,
            bottom: 64.0
        },
        "rot90 swaps"
    );
    assert_eq!(by(3).buffer().unwrap().buffer_transform, 4);
}

/// SurfaceView's background (SurfaceView.java updateSurface + updateRelativeZ): an effect layer, child of a cropped container, ordered relative to the window at Integer.MIN_VALUE, coloured by setColor.
/// The scene shows it as a colour quad over the container's crop, below the window; with no crop anywhere it is skipped and counted; created without the effect flag it never fills.
#[test]
fn colour_layer_fills_its_parent_crop_below_the_window() {
    let (window, container, background, plain, bare) =
        (local(1), local(2), local(3), local(4), local(5));
    let mut c = Compositor::new(CompositorConfig::default());
    c.create_layer(spec(window, 1, None)).unwrap();
    c.create_layer(spec(container, 2, Some(key(&window))))
        .unwrap();
    c.create_layer(LayerSpec {
        fills_color: true,
        ..spec(background, 3, Some(key(&container)))
    })
    .unwrap();
    // a buffer-state layer under the same crop, coloured: never fills (createLayer gave it eNoColorFill), until setColor makes every component >= 0
    c.create_layer(spec(plain, 4, Some(key(&container))))
        .unwrap();
    c.create_layer(LayerSpec {
        fills_color: true,
        ..spec(bare, 5, None)
    })
    .unwrap();
    let mut crop = LayerState::new();
    crop.surface = container;
    crop.what = what::E_POSITION_CHANGED | what::E_CROP_CHANGED;
    (crop.x, crop.y) = (10.0, 20.0);
    crop.crop = FloatRect {
        left: 0.0,
        top: 0.0,
        right: 30.0,
        bottom: 40.0,
    };
    let mut colour = LayerState::new();
    colour.surface = background;
    colour.what = what::E_COLOR_CHANGED | what::E_RELATIVE_LAYER_CHANGED;
    colour.color = [0.25, 0.5, 0.75, 0.0];
    colour.z = i32::MIN;
    colour.relative_layer = Some(crate::state::SurfaceControlParcel {
        client: local(50),
        handle: window,
        layer_id: 1,
        name: "window".into(),
        transform_hint: 0,
        width: 0,
        height: 0,
        format: 0,
    });
    c.queue_transaction(tx(vec![crop, colour]));
    show(&mut c, &window, 11, 64, 64);
    let scene = c.on_frame(1).scene.unwrap();
    let names: Vec<i32> = scene.quads.iter().map(|q| q.layer_id).collect();
    assert_eq!(
        names,
        [3, 1],
        "background below the window; layer 4 has no fill, layer 5 no bounds"
    );
    let bounds = FloatRect {
        left: 10.0,
        top: 20.0,
        right: 40.0,
        bottom: 60.0,
    };
    assert_eq!(scene.quads[0].content, Content::Color([0.25, 0.5, 0.75]));
    assert_eq!(
        (scene.quads[0].dst, scene.quads[0].clip),
        (bounds, Some(bounds))
    );
    assert_eq!(
        scene.quads[0].alpha, 1.0,
        "eColorChanged leaves the alpha alone"
    );
    assert_eq!(c.counters().unbounded_color_skipped, 1);
    let mut lit = LayerState::new();
    lit.surface = plain;
    lit.what = what::E_COLOR_CHANGED;
    lit.color = [1.0, 0.0, 0.0, 0.0];
    c.queue_transaction(tx(vec![lit]));
    let scene = c.on_frame(2).scene.unwrap();
    assert!(
        scene
            .quads
            .iter()
            .any(|q| q.layer_id == 4 && q.content == Content::Color([1.0, 0.0, 0.0])),
        "setColor with components >= 0 makes a bufferless layer fill (RequestedLayerState::fillsColor)"
    );
}

#[test]
fn wants_frame_tracks_pending_work() {
    let layer = local(1);
    let mut c = Compositor::new(CompositorConfig::default());
    assert!(!c.wants_frame());
    c.create_layer(spec(layer, 1, None)).unwrap();
    assert!(c.wants_frame());
    c.on_frame(1);
    assert!(!c.wants_frame());
    c.queue_transaction(tx(vec![]));
    assert!(c.wants_frame());
}

#[test]
fn shutdown_releases_what_is_held() {
    let (layer, listener) = (local(1), local(2));
    let mut c = Compositor::new(CompositorConfig::default());
    c.create_layer(spec(layer, 1, None)).unwrap();
    c.queue_transaction(tx(vec![buffer_state(
        &layer, &listener, 701, 1, 8, 8, false, 1,
    )]));
    c.on_frame(1);
    let calls = c.shutdown();
    assert_eq!(
        releases(&calls),
        [ReleaseCallbackId {
            buffer_id: 701,
            frame_number: 1
        }]
    );
    assert_eq!(c.layer_count(), 0);
}

/// The descriptors the core names, step by step: queued in a transaction, shown by a layer, kept by the client cache after the layer lets go, gone with the cache entry. Buffer id N carries fd 40 + N (`gbuf`).
#[test]
fn referenced_fds_follow_queue_layer_and_cache() {
    use crate::transaction::UncacheBuffer;
    let (layer, listener) = (local(1), local(2));
    let mut c = Compositor::new(CompositorConfig::default());
    assert!(c.referenced_fds().is_empty());
    c.create_layer(spec(layer, 1, None)).unwrap();
    c.queue_transaction(tx(vec![channel_state(&layer, 90)]));
    c.queue_transaction(tx(vec![buffer_state(
        &layer, &listener, 201, 1, 8, 8, false, 1,
    )]));
    // queued, not applied: the transactions name both
    assert_eq!(c.referenced_fds(), BTreeSet::from([90, 241]));
    c.on_frame(10);
    assert_eq!(c.referenced_fds(), BTreeSet::from([90, 241]));
    c.queue_transaction(tx(vec![buffer_state(
        &layer, &listener, 202, 2, 8, 8, false, 2,
    )]));
    assert_eq!(c.referenced_fds(), BTreeSet::from([90, 241, 242]));
    // 201 is replaced and released, but the client's cache still holds it until it is uncached
    c.on_frame(20);
    assert_eq!(c.referenced_fds(), BTreeSet::from([90, 241, 242]));
    let mut uncache = TransactionState::new();
    uncache.uncache_buffers.push(UncacheBuffer {
        token: listener,
        id: 201,
    });
    c.queue_transaction(uncache);
    c.on_frame(30);
    assert_eq!(c.referenced_fds(), BTreeSet::from([90, 242]));
    // the layer goes: its channel and shown buffer with it, the cache entry of 202 stays
    c.destroy_layer(key(&layer));
    assert_eq!(c.referenced_fds(), BTreeSet::from([242]));
    let mut uncache = TransactionState::new();
    uncache.uncache_buffers.push(UncacheBuffer {
        token: listener,
        id: 202,
    });
    c.queue_transaction(uncache);
    c.on_frame(40);
    assert!(c.referenced_fds().is_empty());
}

/// A second window of the app (a toast) is placed by the window manager, not by the app: set_position moves its root layer, the next scene shows it there; an unknown id is refused.
#[test]
fn window_manager_places_a_root_layer() {
    let (window, toast) = (local(1), local(2));
    let mut c = Compositor::new(CompositorConfig::default());
    c.create_layer(spec(window, 1, None)).unwrap();
    c.create_layer(spec(toast, 2, None)).unwrap();
    show(&mut c, &window, 11, 960, 600);
    show(&mut c, &toast, 12, 200, 48);
    c.on_frame(1);
    assert!(c.set_position(2, 380.0, 488.0));
    assert!(!c.set_position(99, 0.0, 0.0));
    assert!(c.wants_frame());
    let scene = c.on_frame(2).scene.unwrap();
    let toast_quad = scene.quads.iter().find(|q| q.layer_id == 2).unwrap();
    assert_eq!(
        toast_quad.dst,
        FloatRect {
            left: 380.0,
            top: 488.0,
            right: 580.0,
            bottom: 536.0
        }
    );
}
