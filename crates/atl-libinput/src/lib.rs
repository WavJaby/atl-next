// SPDX-License-Identifier: MIT
//! AOSP libinput for ATL-next, run as the converter builds it: key and motion events published over an InputChannel, the consumer end the publish gate reads them with, and the KeyCharacterMap of a `.kcm` file in the Parcel form an InputDevice carries.
//! Without feature `converter` the crate is empty: libinput and its C++ come from the converter.
//! The C++ is the cxx glue (bridge.rs) and adapters (wrap/input.{h,cpp}) that convert types and call AOSP's public API; the logic is here.
#![cfg(feature = "converter")]

// The bridge takes scalars, not records: a C++ adapter cannot name a cxx shared struct, so publish_motion lists every field as an argument.
#[allow(clippy::too_many_arguments)]
mod bridge;

use std::os::fd::{AsFd, BorrowedFd, IntoRawFd, OwnedFd};

use bridge::ffi;

use rustix::io::Errno;

/// AOSP's `status_t` OK.
const OK: i32 = 0;
/// AOSP's `WOULD_BLOCK`: `-EAGAIN` (utils/Errors.h).
const WOULD_BLOCK: i32 = -(Errno::AGAIN.raw_os_error());
/// Message kinds the adapters report (wrap/input.h).
const KIND_KEY: i32 = 1;
const KIND_MOTION: i32 = 2;
const KIND_FINISHED: i32 = 3;
/// A focus event (FocusEvent), consume() only.
const KIND_FOCUS: i32 = 5;

/// An AOSP call that failed: its `status_t`, or a null object where AOSP returns none.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{call} failed with status {status}")]
    Status { call: &'static str, status: i32 },
    #[error("{call} returned no object")]
    Null { call: &'static str },
}

fn status(call: &'static str, status: i32) -> Result<(), Error> {
    if status == OK {
        Ok(())
    } else {
        Err(Error::Status { call, status })
    }
}

/// A key event to publish (InputPublisher::publishKeyEvent); the verifier's HMAC is zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyEvent {
    pub seq: u32,
    pub event_id: i32,
    pub device_id: i32,
    pub source: i32,
    pub display_id: i32,
    pub action: i32,
    pub flags: i32,
    pub key_code: i32,
    pub scan_code: i32,
    pub meta_state: i32,
    pub repeat_count: i32,
    pub down_time: i64,
    pub event_time: i64,
}

/// MotionEvent.AXIS_X, AXIS_Y (NDK input.h AMOTION_EVENT_AXIS_*).
pub const AXIS_X: i32 = 0;
pub const AXIS_Y: i32 = 1;

/// One pointer of a motion event: its id, tool type (MotionEvent.TOOL_TYPE_*), its (x, y) in the event's coordinates and any other axes as (MotionEvent.AXIS_*, value).
#[derive(Debug, Clone, PartialEq)]
pub struct Pointer {
    pub id: i32,
    pub tool_type: i32,
    pub x: f32,
    pub y: f32,
    pub axes: Vec<(i32, f32)>,
}

/// A motion event to publish (InputPublisher::publishMotionEvent). `transform` is the affine (a, b, c, d, tx, ty) taking the event's coordinates to the window (getX); `raw_transform` the one taking them to the screen (getRawX).
#[derive(Debug, Clone, PartialEq)]
pub struct MotionEvent {
    pub seq: u32,
    pub event_id: i32,
    pub device_id: i32,
    pub source: i32,
    pub display_id: i32,
    pub action: i32,
    pub action_button: i32,
    pub flags: i32,
    pub edge_flags: i32,
    pub meta_state: i32,
    pub button_state: i32,
    /// AOSP's MotionClassification (`enum class`, uint8_t).
    pub classification: u8,
    pub transform: [f32; 6],
    pub raw_transform: [f32; 6],
    pub x_precision: f32,
    pub y_precision: f32,
    pub x_cursor: f32,
    pub y_cursor: f32,
    pub down_time: i64,
    pub event_time: i64,
    pub pointers: Vec<Pointer>,
}

/// The publisher end of a channel: AOSP's InputPublisher over the InputChannel it owns.
pub struct Publisher(cxx::UniquePtr<ffi::Publisher>);

impl Publisher {
    /// Takes ownership of `fd`, one end of a SEQPACKET socket pair; `name` labels the channel.
    pub fn new(fd: OwnedFd, name: &str) -> Result<Self, Error> {
        let publisher = ffi::new_publisher(fd.into_raw_fd(), name);
        if publisher.is_null() {
            return Err(Error::Null {
                call: "InputChannel::create",
            });
        }
        Ok(Self(publisher))
    }

    /// Publishes a key event (InputPublisher::publishKeyEvent).
    pub fn publish_key(&mut self, e: &KeyEvent) -> Result<(), Error> {
        let s = ffi::publish_key(
            self.0.pin_mut(),
            e.seq,
            e.event_id,
            e.device_id,
            e.source,
            e.display_id,
            e.action,
            e.flags,
            e.key_code,
            e.scan_code,
            e.meta_state,
            e.repeat_count,
            e.down_time,
            e.event_time,
        );
        status("InputPublisher::publishKeyEvent", s)
    }

    /// Publishes a motion event (InputPublisher::publishMotionEvent).
    pub fn publish_motion(&mut self, e: &MotionEvent) -> Result<(), Error> {
        let ids: Vec<i32> = e.pointers.iter().map(|p| p.id).collect();
        let tools: Vec<i32> = e.pointers.iter().map(|p| p.tool_type).collect();
        // X and Y first, then the pointer's other axes; the pointer index is always in range, so the C++ loop indexes no pointer it was not given
        let mut axis_pointers = Vec::new();
        let mut axis_ids = Vec::new();
        let mut axis_values = Vec::new();
        for (index, p) in (0_u32..).zip(&e.pointers) {
            for (axis, value) in [(AXIS_X, p.x), (AXIS_Y, p.y)]
                .into_iter()
                .chain(p.axes.iter().copied())
            {
                axis_pointers.push(index);
                axis_ids.push(axis);
                axis_values.push(value);
            }
        }
        let s = ffi::publish_motion(
            self.0.pin_mut(),
            e.seq,
            e.event_id,
            e.device_id,
            e.source,
            e.display_id,
            e.action,
            e.action_button,
            e.flags,
            e.edge_flags,
            e.meta_state,
            e.button_state,
            e.classification,
            &e.transform,
            &e.raw_transform,
            e.x_precision,
            e.y_precision,
            e.x_cursor,
            e.y_cursor,
            e.down_time,
            e.event_time,
            &ids,
            &tools,
            &axis_pointers,
            &axis_ids,
            &axis_values,
        );
        status("InputPublisher::publishMotionEvent", s)
    }

    /// Publishes a focus change (InputPublisher::publishFocusEvent): ViewRootImpl's onFocusEvent -> windowFocusChanged.
    pub fn publish_focus(&mut self, seq: u32, event_id: i32, has_focus: bool) -> Result<(), Error> {
        status(
            "InputPublisher::publishFocusEvent",
            ffi::publish_focus(self.0.pin_mut(), seq, event_id, has_focus),
        )
    }

    /// The next finished signal from the consumer (InputPublisher::receiveConsumerResponse); None when none is pending. Timeline responses are not returned.
    pub fn receive_finished(&mut self) -> Result<Option<Finished>, Error> {
        let mut message = ffi::new_message();
        let s = ffi::receive_finished(self.0.pin_mut(), message.pin_mut());
        if s == WOULD_BLOCK {
            return Ok(None);
        }
        status("InputPublisher::receiveConsumerResponse", s)?;
        Ok((message.kind() == KIND_FINISHED).then(|| Finished {
            seq: message.seq(),
            handled: message.handled(),
        }))
    }
}

impl AsFd for Publisher {
    /// The channel's fd, for the composer's poll. Borrowed from the publisher, which owns the InputChannel holding it.
    fn as_fd(&self) -> BorrowedFd<'_> {
        let fd = ffi::publisher_fd(&self.0);
        // SAFETY: the InputChannel keeps fd open until the publisher drops, and the borrow cannot outlive `self`.
        unsafe { BorrowedFd::borrow_raw(fd) }
    }
}

/// The consumer's answer to one dispatched event (InputPublisher::Finished).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Finished {
    pub seq: u32,
    pub handled: bool,
}

/// One message a consumer received: a key or motion event, as AOSP's InputConsumer delivers it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Received {
    Key {
        seq: u32,
        action: i32,
        key_code: i32,
        device_id: i32,
    },
    Motion {
        seq: u32,
        action: i32,
        device_id: i32,
        /// The first pointer's id, (x, y), tool type and AXIS_VSCROLL.
        pointer_id: i32,
        x: f32,
        y: f32,
        tool_type: i32,
        vscroll: f32,
    },
    /// A focus change (FocusEvent).
    Focus { seq: u32, has_focus: bool },
    /// Any other message kind; `seq` is what the consumer must answer.
    Other { seq: u32 },
}

/// The consumer end of a channel: AOSP's InputConsumer over the other SEQPACKET end.
pub struct Consumer(cxx::UniquePtr<ffi::Consumer>);

impl Consumer {
    /// Takes ownership of `fd`, the other end of the socket pair; `name` labels the channel.
    pub fn new(fd: OwnedFd, name: &str) -> Result<Self, Error> {
        let consumer = ffi::new_consumer(fd.into_raw_fd(), name);
        if consumer.is_null() {
            return Err(Error::Null {
                call: "InputChannel::create",
            });
        }
        Ok(Self(consumer))
    }

    /// The next message (InputConsumer::consume); None when none is pending.
    pub fn consume(&mut self) -> Result<Option<Received>, Error> {
        let mut message = ffi::new_message();
        let s = ffi::consume(self.0.pin_mut(), message.pin_mut());
        if s == WOULD_BLOCK {
            return Ok(None);
        }
        status("InputConsumer::consume", s)?;
        let seq = message.seq();
        Ok(Some(match message.kind() {
            KIND_KEY => Received::Key {
                seq,
                action: message.action(),
                key_code: message.key_code(),
                device_id: message.device_id(),
            },
            KIND_MOTION => Received::Motion {
                seq,
                action: message.action(),
                device_id: message.device_id(),
                pointer_id: message.pointer_id(),
                x: message.x(),
                y: message.y(),
                tool_type: message.tool_type(),
                vscroll: message.vscroll(),
            },
            KIND_FOCUS => Received::Focus {
                seq,
                has_focus: message.has_focus(),
            },
            _ => Received::Other { seq },
        }))
    }

    /// Answers the message `seq` (InputConsumer::sendFinishedSignal).
    pub fn finish(&mut self, seq: u32, handled: bool) -> Result<(), Error> {
        status(
            "InputConsumer::sendFinishedSignal",
            ffi::finish(self.0.pin_mut(), seq, handled),
        )
    }
}

/// AOSP's KeyCharacterMap, loaded from a `.kcm` file's contents.
pub struct KeyCharacterMap(cxx::UniquePtr<ffi::KeyCharacterMap>);

impl KeyCharacterMap {
    /// Loads `contents` (format ANY, as validatekeymaps loads a map). `file_name` is the load name the Parcel form carries: the bare file name, `Virtual.kcm`, keeps the bytes independent of the directory.
    pub fn load(file_name: &str, contents: &[u8]) -> Result<Self, Error> {
        let map = ffi::load_kcm(file_name, contents);
        if map.is_null() {
            return Err(Error::Null {
                call: "KeyCharacterMap::loadContents",
            });
        }
        Ok(Self(map))
    }

    /// The Parcel form (KeyCharacterMap::writeToParcel): the bytes InputDevice carries after its device id and present flag.
    pub fn parcel_bytes(&self) -> Vec<u8> {
        ffi::kcm_parcel_bytes(&self.0)
    }
}

/// AOSP's KeyLayoutMap: linux scan codes to Android keycodes, from a `.kl` file.
pub struct KeyLayoutMap(cxx::UniquePtr<ffi::KeyLayoutMap>);

impl KeyLayoutMap {
    /// KeyLayoutMap::load of the file at `path` (libinput reads it).
    pub fn load(path: &str) -> Result<Self, Error> {
        let map = ffi::load_key_layout(path);
        if map.is_null() {
            return Err(Error::Null {
                call: "KeyLayoutMap::load",
            });
        }
        Ok(Self(map))
    }

    /// The keycode and policy flags of `scan_code` (KeyLayoutMap::mapKey, no HID usage); None for a code the layout does not list.
    pub fn map_key(&self, scan_code: i32) -> Option<(i32, u32)> {
        let (mut key_code, mut flags) = (0, 0);
        (ffi::map_key(&self.0, scan_code, &mut key_code, &mut flags) == OK)
            .then_some((key_code, flags))
    }
}

/// android::updateMetaState: the meta state after `key_code` went down or up (modifier bits set or cleared, lock bits toggled on down), normalized.
pub fn update_meta_state(key_code: i32, down: bool, old_meta_state: i32) -> i32 {
    ffi::update_meta_state(key_code, down, old_meta_state)
}

/// android::normalizeMetaState: the side bits imply the generic one (SHIFT_LEFT_ON -> SHIFT_ON).
pub fn normalize_meta_state(meta_state: i32) -> i32 {
    ffi::normalize_meta_state(meta_state)
}
