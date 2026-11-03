// SPDX-License-Identifier: Apache-2.0
// Derived from the Android Open Source Project: frameworks/native/libs/gui/include/gui/DisplayEventReceiver.h (Event layout, DisplayEventType), frameworks/native/libs/gui/include/gui/VsyncEventData.h,
// frameworks/native/libs/gui/VsyncEventData.cpp (ParcelableVsyncEventData), frameworks/native/libs/gui/BitTube.cpp (one event per SOCK_SEQPACKET datagram)
//! The vsync side: `DisplayEventReceiver::Event` structs for a BitTube, and the `EventThread` that decides which connection gets one when the platform's frame clock ticks.
//! The frame clock is abstract: whoever owns it (the GTK frame clock, a test) calls [`EventThread::on_frame_tick`] and asks [`EventThread::wants_frame`] whether anyone is waiting.

use std::collections::BTreeMap;

use crate::parcel::{ParcelError, Reader, WireParcel};

/// `DisplayEventType::DISPLAY_EVENT_VSYNC` = fourcc('v','s','y','n') (c1 in the top byte).
pub const DISPLAY_EVENT_VSYNC: u32 = u32::from_be_bytes(*b"vsyn");
/// `sizeof(DisplayEventReceiver::Event)`, asserted in the header (`static_assert(sizeof(Event) == 224)`).
pub const EVENT_SIZE: usize = 224;
/// `VsyncEventData::kFrameTimelinesCapacity`.
pub const FRAME_TIMELINES_CAPACITY: usize = 7;

/// Byte offsets inside `Event` (Header: type u32 @0, displayId u64 @8, timestamp i64 @16; VSync payload @24: count u32, then `VsyncEventData` at the 8-aligned @32).
mod off {
    pub const TYPE: usize = 0;
    pub const DISPLAY_ID: usize = 8;
    pub const TIMESTAMP: usize = 16;
    pub const COUNT: usize = 24;
    pub const FRAME_INTERVAL: usize = 32;
    pub const PREFERRED_INDEX: usize = 40;
    pub const TIMELINES_LENGTH: usize = 44;
    pub const QUEUED_BUFFERS: usize = 48;
    /// `FrameTimeline` is `alignas(8)`: the array starts at the next multiple of 8 after `numberQueuedBuffers`.
    pub const TIMELINES: usize = 56;
    pub const TIMELINE_SIZE: usize = 24;
}

/// Where the app's frame starts relative to the vsync it was woken by, and when SurfaceFlinger latches it, in intervals.
/// A frame the app starts at vsync N is composed at N+1 and shown at N+2 (the classic two-stage pipeline AOSP's `expectedPresentationTime` models); the deadline is the latch, one interval before the present.
pub const PRESENT_DELAY_INTERVALS: i64 = 2;

/// One predicted presentation: `VsyncEventData::FrameTimeline`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameTimeline {
    /// The vsync id to pass back in `FrameTimelineInfo`.
    pub vsync_id: i64,
    /// The deadline the app must finish by, ns (CLOCK_MONOTONIC).
    pub deadline_timestamp: i64,
    /// The expected presentation time, ns.
    pub expected_presentation_time: i64,
}

/// `VsyncEventData`: the timelines Choreographer picks from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VsyncEventData {
    /// The frame interval when this frame was scheduled, ns.
    pub frame_interval: i64,
    /// Index into `frame_timelines` of the preferred timeline.
    pub preferred_frame_timeline_index: u32,
    /// The timelines, at most [`FRAME_TIMELINES_CAPACITY`].
    pub frame_timelines: Vec<FrameTimeline>,
    /// Queued-buffer count for buffer-stuffing detection (not part of `ParcelableVsyncEventData`).
    pub number_queued_buffers: u32,
}

impl VsyncEventData {
    /// `ParcelableVsyncEventData::writeToParcel`.
    pub fn write_parcelable_body(&self, p: &mut WireParcel) {
        p.write_i64(self.frame_interval);
        p.write_u32(self.preferred_frame_timeline_index);
        p.write_u32(self.frame_timelines.len() as u32);
        for t in &self.frame_timelines {
            p.write_i64(t.vsync_id);
            p.write_i64(t.deadline_timestamp);
            p.write_i64(t.expected_presentation_time);
        }
    }

    /// `ParcelableVsyncEventData::readFromParcel`; unlike AOSP's reader it refuses a timeline count above the capacity.
    pub fn read_parcelable_body(r: &mut Reader<'_>) -> Result<Self, ParcelError> {
        let frame_interval = r.read_i64()?;
        let preferred_frame_timeline_index = r.read_u32()?;
        let n = r.read_u32()? as usize;
        if n > FRAME_TIMELINES_CAPACITY {
            return Err(ParcelError::BadValue("frame timeline count above capacity"));
        }
        let frame_timelines = (0..n)
            .map(|_| {
                Ok(FrameTimeline {
                    vsync_id: r.read_i64()?,
                    deadline_timestamp: r.read_i64()?,
                    expected_presentation_time: r.read_i64()?,
                })
            })
            .collect::<Result<Vec<_>, ParcelError>>()?;
        Ok(Self {
            frame_interval,
            preferred_frame_timeline_index,
            frame_timelines,
            number_queued_buffers: 0,
        })
    }
}

/// A `DISPLAY_EVENT_VSYNC` event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VsyncEvent {
    /// The `PhysicalDisplayId`.
    pub display_id: u64,
    /// The vsync time, ns.
    pub timestamp: i64,
    /// The vsync counter.
    pub count: u32,
    /// The timelines.
    pub data: VsyncEventData,
}

impl VsyncEvent {
    /// The [`EVENT_SIZE`] bytes one BitTube datagram carries (unused tail of the timelines and padding zero).
    pub fn to_bytes(&self) -> [u8; EVENT_SIZE] {
        let mut b = [0u8; EVENT_SIZE];
        let mut put = |at: usize, v: &[u8]| b[at..at + v.len()].copy_from_slice(v);
        put(off::TYPE, &DISPLAY_EVENT_VSYNC.to_le_bytes());
        put(off::DISPLAY_ID, &self.display_id.to_le_bytes());
        put(off::TIMESTAMP, &self.timestamp.to_le_bytes());
        put(off::COUNT, &self.count.to_le_bytes());
        put(off::FRAME_INTERVAL, &self.data.frame_interval.to_le_bytes());
        put(
            off::PREFERRED_INDEX,
            &self.data.preferred_frame_timeline_index.to_le_bytes(),
        );
        let n = self
            .data
            .frame_timelines
            .len()
            .min(FRAME_TIMELINES_CAPACITY);
        put(off::TIMELINES_LENGTH, &(n as u32).to_le_bytes());
        put(
            off::QUEUED_BUFFERS,
            &self.data.number_queued_buffers.to_le_bytes(),
        );
        for (i, t) in self.data.frame_timelines.iter().take(n).enumerate() {
            let at = off::TIMELINES + i * off::TIMELINE_SIZE;
            put(at, &t.vsync_id.to_le_bytes());
            put(at + 8, &t.deadline_timestamp.to_le_bytes());
            put(at + 16, &t.expected_presentation_time.to_le_bytes());
        }
        b
    }

    /// Decodes one datagram; anything but a whole `Event` of type vsync is refused.
    pub fn from_bytes(b: &[u8]) -> Result<Self, ParcelError> {
        if b.len() != EVENT_SIZE {
            return Err(ParcelError::BadValue("an Event is exactly 224 bytes"));
        }
        let u32_at = |at: usize| u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]]);
        let i64_at = |at: usize| {
            i64::from_le_bytes([
                b[at],
                b[at + 1],
                b[at + 2],
                b[at + 3],
                b[at + 4],
                b[at + 5],
                b[at + 6],
                b[at + 7],
            ])
        };
        if u32_at(off::TYPE) != DISPLAY_EVENT_VSYNC {
            return Err(ParcelError::BadValue("event type is not vsync"));
        }
        let n = u32_at(off::TIMELINES_LENGTH) as usize;
        if n > FRAME_TIMELINES_CAPACITY {
            return Err(ParcelError::BadValue("frame timeline count above capacity"));
        }
        let frame_timelines = (0..n)
            .map(|i| {
                let at = off::TIMELINES + i * off::TIMELINE_SIZE;
                FrameTimeline {
                    vsync_id: i64_at(at),
                    deadline_timestamp: i64_at(at + 8),
                    expected_presentation_time: i64_at(at + 16),
                }
            })
            .collect();
        Ok(Self {
            display_id: i64_at(off::DISPLAY_ID) as u64,
            timestamp: i64_at(off::TIMESTAMP),
            count: u32_at(off::COUNT),
            data: VsyncEventData {
                frame_interval: i64_at(off::FRAME_INTERVAL),
                preferred_frame_timeline_index: u32_at(off::PREFERRED_INDEX),
                frame_timelines,
                number_queued_buffers: u32_at(off::QUEUED_BUFFERS),
            },
        })
    }
}

/// One tick of the platform's frame clock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameTick {
    /// A id for this vsync, increasing by one per tick (the first timeline's `vsync_id`).
    pub vsync_id: i64,
    /// The vsync time, ns, on the clock the app's Choreographer uses (CLOCK_MONOTONIC).
    pub timestamp: i64,
    /// The interval to the next tick, ns.
    pub interval: i64,
}

/// A client of the EventThread (`IDisplayEventConnection`): its id keys the BitTube the platform owns.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ConnectionId(pub u32);

/// `EventThread::VsyncRequest` without the throttling states the software compositor has no use for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum VsyncRequest {
    None,
    Single,
    Periodic,
}

/// Decides who gets a vsync event per tick, and builds it.
#[derive(Debug)]
pub struct EventThread {
    display_id: u64,
    next_connection: u32,
    connections: BTreeMap<ConnectionId, VsyncRequest>,
    count: u32,
    latest: Option<VsyncEventData>,
}

impl EventThread {
    /// An event thread for the display `display_id`.
    pub fn new(display_id: u64) -> Self {
        Self {
            display_id,
            next_connection: 1,
            connections: BTreeMap::new(),
            count: 0,
            latest: None,
        }
    }

    /// `createDisplayEventConnection`: a connection with vsync off.
    pub fn create_connection(&mut self) -> ConnectionId {
        let id = ConnectionId(self.next_connection);
        self.next_connection += 1;
        self.connections.insert(id, VsyncRequest::None);
        id
    }

    /// The connection's last reference went away.
    pub fn remove_connection(&mut self, id: ConnectionId) {
        self.connections.remove(&id);
    }

    /// `IDisplayEventConnection.setVsyncRate(count)`: 0 stops events; any count of 1 or more delivers every vsync (AOSP's EventThread treats the count as on/off, `vsyncRequest = count >= 1 ? Periodic : None`).
    pub fn set_vsync_rate(&mut self, id: ConnectionId, count: u32) {
        if let Some(r) = self.connections.get_mut(&id) {
            *r = if count >= 1 {
                VsyncRequest::Periodic
            } else {
                VsyncRequest::None
            };
        }
    }

    /// `IDisplayEventConnection.requestNextVsync()`: one event, unless the connection already gets every vsync.
    pub fn request_next_vsync(&mut self, id: ConnectionId) {
        if let Some(r @ VsyncRequest::None) = self.connections.get_mut(&id) {
            *r = VsyncRequest::Single;
        }
    }

    /// Whether any connection waits for a vsync: the platform runs its frame clock only then.
    pub fn wants_frame(&self) -> bool {
        self.connections.values().any(|r| *r != VsyncRequest::None)
    }

    /// The timelines of the last tick, or `None` before the first (`getLatestVsyncEventData`).
    pub fn latest_vsync_event_data(&self) -> Option<&VsyncEventData> {
        self.latest.as_ref()
    }

    /// The timelines a tick predicts: [`FRAME_TIMELINES_CAPACITY`] successive vsyncs, the n-th shown [`PRESENT_DELAY_INTERVALS`] + n intervals after the tick.
    fn timelines(tick: &FrameTick) -> VsyncEventData {
        let frame_timelines = (0..FRAME_TIMELINES_CAPACITY as i64)
            .map(|n| {
                let expected = tick.timestamp + (PRESENT_DELAY_INTERVALS + n) * tick.interval;
                FrameTimeline {
                    vsync_id: tick.vsync_id + n,
                    deadline_timestamp: expected - tick.interval,
                    expected_presentation_time: expected,
                }
            })
            .collect();
        VsyncEventData {
            frame_interval: tick.interval,
            preferred_frame_timeline_index: 0,
            frame_timelines,
            number_queued_buffers: 0,
        }
    }

    /// The frame clock ticked: the event for every connection that asked for one (`Single` requests are spent), to be written to that connection's BitTube.
    pub fn on_frame_tick(&mut self, tick: FrameTick) -> Vec<(ConnectionId, VsyncEvent)> {
        self.count = self.count.wrapping_add(1);
        let data = Self::timelines(&tick);
        self.latest = Some(data.clone());
        let event = VsyncEvent {
            display_id: self.display_id,
            timestamp: tick.timestamp,
            count: self.count,
            data,
        };
        let mut out = Vec::new();
        for (id, req) in &mut self.connections {
            match *req {
                VsyncRequest::None => {}
                VsyncRequest::Single => {
                    *req = VsyncRequest::None;
                    out.push((*id, event.clone()));
                }
                VsyncRequest::Periodic => out.push((*id, event.clone())),
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tick(n: i64) -> FrameTick {
        FrameTick {
            vsync_id: 100 + n,
            timestamp: 1_000_000 + n * 16_666_667,
            interval: 16_666_667,
        }
    }

    /// Offsets from the C++ layout rules: Header 24 bytes (type, pad, 8-aligned id and timestamp), VSync at 24: count, pad, int64 interval @32, three uint32 @40..52, pad to 56 for the alignas(8) timelines, 7 x 24 = 168, total 224.
    #[test]
    fn layout_matches_the_header_arithmetic() {
        assert_eq!(
            24 + 8 + 8 + 12 + 4 + FRAME_TIMELINES_CAPACITY * 24,
            EVENT_SIZE
        );
        assert_eq!(
            off::TIMELINES + FRAME_TIMELINES_CAPACITY * off::TIMELINE_SIZE,
            EVENT_SIZE
        );
        assert_eq!(
            DISPLAY_EVENT_VSYNC, 0x7673_796e,
            "'v' 's' 'y' 'n' with the first character in the top byte"
        );
    }

    #[test]
    fn event_bytes_round_trip() {
        let mut et = EventThread::new(0x1234);
        let c = et.create_connection();
        et.set_vsync_rate(c, 1);
        let evs = et.on_frame_tick(tick(0));
        assert_eq!(evs.len(), 1);
        let bytes = evs[0].1.to_bytes();
        assert_eq!(&bytes[..4], b"nysv", "little-endian store of 0x7673796e");
        assert_eq!(VsyncEvent::from_bytes(&bytes).unwrap(), evs[0].1);
        assert_eq!(evs[0].1.data.frame_timelines.len(), 7);
        assert!(VsyncEvent::from_bytes(&bytes[..223]).is_err());
        let mut bad = bytes;
        bad[0] ^= 1;
        assert!(VsyncEvent::from_bytes(&bad).is_err());
    }

    #[test]
    fn single_request_is_spent_and_periodic_is_not() {
        let mut et = EventThread::new(1);
        let once = et.create_connection();
        let always = et.create_connection();
        let idle = et.create_connection();
        et.request_next_vsync(once);
        et.set_vsync_rate(always, 1);
        assert!(et.wants_frame());
        let ids: Vec<_> = et
            .on_frame_tick(tick(0))
            .into_iter()
            .map(|(c, _)| c)
            .collect();
        assert_eq!(ids, [once, always]);
        let ids: Vec<_> = et
            .on_frame_tick(tick(1))
            .into_iter()
            .map(|(c, _)| c)
            .collect();
        assert_eq!(ids, [always]);
        et.set_vsync_rate(always, 0);
        assert!(!et.wants_frame());
        assert!(et.on_frame_tick(tick(2)).is_empty());
        // a request on a periodic connection does not downgrade it
        et.set_vsync_rate(idle, 1);
        et.request_next_vsync(idle);
        assert_eq!(et.on_frame_tick(tick(3)).len(), 1);
        assert_eq!(et.on_frame_tick(tick(4)).len(), 1);
    }

    #[test]
    fn timelines_follow_the_tick() {
        let mut et = EventThread::new(1);
        let c = et.create_connection();
        et.set_vsync_rate(c, 1);
        let t = tick(5);
        let ev = &et.on_frame_tick(t)[0].1;
        let tl = &ev.data.frame_timelines;
        assert_eq!(tl[0].vsync_id, t.vsync_id);
        assert_eq!(
            tl[0].expected_presentation_time,
            t.timestamp + 2 * t.interval
        );
        assert_eq!(tl[0].deadline_timestamp, t.timestamp + t.interval);
        assert_eq!(tl[6].vsync_id, t.vsync_id + 6);
        assert_eq!(
            tl[6].expected_presentation_time,
            t.timestamp + 8 * t.interval
        );
        assert_eq!(ev.count, 1);
        assert_eq!(et.on_frame_tick(tick(6))[0].1.count, 2);
    }

    #[test]
    fn parcelable_vsync_data_round_trip_and_bound() {
        let mut et = EventThread::new(1);
        let c = et.create_connection();
        et.set_vsync_rate(c, 1);
        et.on_frame_tick(tick(0));
        let d = et.latest_vsync_event_data().unwrap().clone();
        let mut p = WireParcel::new();
        d.write_parcelable_body(&mut p);
        let mut r = p.reader();
        let back = VsyncEventData::read_parcelable_body(&mut r).unwrap();
        assert_eq!(back.frame_timelines, d.frame_timelines);
        r.finish().unwrap();
        // 8 (interval) + 4 + 4 + 7 * 24
        assert_eq!(p.data().len(), 8 + 4 + 4 + 7 * 24);
        let mut over = WireParcel::new();
        over.write_i64(1);
        over.write_u32(0);
        over.write_u32(8);
        assert!(VsyncEventData::read_parcelable_body(&mut over.reader()).is_err());
    }
}
