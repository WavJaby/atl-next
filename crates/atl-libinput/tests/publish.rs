// SPDX-License-Identifier: MIT
//! The publish gate: events published through the adapters arrive at AOSP's InputConsumer with the fields they were published with, and the consumer's finished signals reach the publisher.
//! Expected values are the published inputs and AOSP's constants (android/input.h, android/keycodes.h, Input.h), never values read back from the code under test.

use atl_libinput::{Consumer, Finished, KeyEvent, MotionEvent, Pointer, Publisher, Received};
use rustix::net::{AddressFamily, SocketFlags, SocketType, socketpair};

/// AKEY_EVENT_ACTION_DOWN, AMOTION_EVENT_ACTION_DOWN.
const ACTION_DOWN: i32 = 0;
/// AKEYCODE_A.
const KEYCODE_A: i32 = 29;
/// AINPUT_SOURCE_KEYBOARD, AINPUT_SOURCE_TOUCHSCREEN.
const SOURCE_KEYBOARD: i32 = 0x0000_0101;
const SOURCE_TOUCHSCREEN: i32 = 0x0000_1002;
/// MotionEvent.TOOL_TYPE_FINGER (NDK input.h AMOTION_EVENT_TOOL_TYPE_FINGER = 1) and AXIS_VSCROLL (= 9): the tool type and an axis beyond X/Y must arrive as published.
const TOOL_TYPE_FINGER: i32 = 1;
const AXIS_VSCROLL: i32 = 9;
/// MotionClassification::NONE.
const CLASSIFICATION_NONE: u8 = 0;
/// The identity transform (a, b, c, d, tx, ty).
const IDENTITY: [f32; 6] = [1.0, 0.0, 0.0, 1.0, 0.0, 0.0];

#[test]
fn key_and_motion_reach_the_consumer_and_finish() {
    let (server, client) = socketpair(
        AddressFamily::UNIX,
        SocketType::SEQPACKET,
        SocketFlags::CLOEXEC,
        None,
    )
    .expect("socketpair");
    let mut publisher = Publisher::new(server, "publish-gate").expect("publisher");
    let mut consumer = Consumer::new(client, "publish-gate").expect("consumer");

    publisher
        .publish_key(&KeyEvent {
            seq: 1,
            event_id: 11,
            device_id: -1,
            source: SOURCE_KEYBOARD,
            display_id: -1,
            action: ACTION_DOWN,
            flags: 0,
            key_code: KEYCODE_A,
            scan_code: 30,
            meta_state: 0,
            repeat_count: 0,
            down_time: 1_000,
            event_time: 1_000,
        })
        .expect("publish key");
    publisher
        .publish_motion(&MotionEvent {
            seq: 2,
            event_id: 12,
            device_id: 1,
            source: SOURCE_TOUCHSCREEN,
            display_id: -1,
            action: ACTION_DOWN,
            action_button: 0,
            flags: 0,
            edge_flags: 0,
            meta_state: 0,
            button_state: 0,
            classification: CLASSIFICATION_NONE,
            transform: IDENTITY,
            raw_transform: IDENTITY,
            x_precision: 1.0,
            y_precision: 1.0,
            x_cursor: 0.0,
            y_cursor: 0.0,
            down_time: 2_000,
            event_time: 2_000,
            pointers: vec![Pointer {
                id: 3,
                tool_type: TOOL_TYPE_FINGER,
                x: 12.5,
                y: 40.0,
                axes: vec![(AXIS_VSCROLL, -1.5)],
            }],
        })
        .expect("publish motion");

    let key = consumer
        .consume()
        .expect("consume key")
        .expect("a key is pending");
    assert_eq!(
        key,
        Received::Key {
            seq: 1,
            action: ACTION_DOWN,
            key_code: KEYCODE_A,
            device_id: -1,
        }
    );
    consumer.finish(1, true).expect("finish key");
    assert_eq!(
        publisher.receive_finished().expect("receive key finish"),
        Some(Finished {
            seq: 1,
            handled: true,
        })
    );

    let motion = consumer
        .consume()
        .expect("consume motion")
        .expect("a motion is pending");
    assert_eq!(
        motion,
        Received::Motion {
            seq: 2,
            action: ACTION_DOWN,
            device_id: 1,
            pointer_id: 3,
            x: 12.5,
            y: 40.0,
            tool_type: TOOL_TYPE_FINGER,
            vscroll: -1.5,
        }
    );
    consumer.finish(2, false).expect("finish motion");
    assert_eq!(
        publisher.receive_finished().expect("receive motion finish"),
        Some(Finished {
            seq: 2,
            handled: false,
        })
    );
    assert_eq!(publisher.receive_finished().expect("receive none"), None);
}

/// A focus change crosses the channel as AOSP's FocusEvent (what ViewRootImpl turns into windowFocusChanged), gained then lost, in order.
#[test]
fn focus_reaches_the_consumer() {
    let (ours, theirs) = rustix::net::socketpair(
        rustix::net::AddressFamily::UNIX,
        rustix::net::SocketType::SEQPACKET,
        rustix::net::SocketFlags::CLOEXEC | rustix::net::SocketFlags::NONBLOCK,
        None,
    )
    .expect("socketpair");
    let mut publisher = Publisher::new(ours, "focus").expect("publisher");
    let mut consumer = Consumer::new(theirs, "focus").expect("consumer");
    publisher.publish_focus(1, 1, true).expect("publish focus");
    publisher.publish_focus(2, 2, false).expect("publish blur");
    for (seq, has_focus) in [(1, true), (2, false)] {
        let got = consumer.consume().expect("consume").expect("pending");
        assert_eq!(got, Received::Focus { seq, has_focus });
        consumer.finish(seq, true).expect("finish");
    }
}
