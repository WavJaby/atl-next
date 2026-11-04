// SPDX-License-Identifier: MIT
//! The vsync channel: libgui's BitTube is a non-blocking SOCK_SEQPACKET socketpair (`BitTube::init`); the client reads whole `DisplayEventReceiver::Event` structs from the end it steals (`stealReceiveChannel`),
//! the compositor writes one event per datagram to the other. Both ends are non-blocking, and the status flags belong to the open file description, so the client's end stays non-blocking after it crosses the parcel.

use std::io;
use std::os::fd::{AsFd, OwnedFd};

use atl_compositor::event::VsyncEvent;
use rustix::net::{AddressFamily, SendFlags, SocketFlags, SocketType, send, socketpair};

/// A channel: (the end the compositor writes, the end the client reads).
pub fn channel() -> io::Result<(OwnedFd, OwnedFd)> {
    Ok(socketpair(
        AddressFamily::UNIX,
        SocketType::SEQPACKET,
        SocketFlags::CLOEXEC | SocketFlags::NONBLOCK,
        None,
    )?)
}

/// Writes `bytes` as one datagram, never blocking. A full socket (a client that does not read) is `WouldBlock`.
pub fn send_datagram(socket: impl AsFd, bytes: &[u8]) -> io::Result<()> {
    let sent = send(socket, bytes, SendFlags::DONTWAIT | SendFlags::NOSIGNAL)?;
    if sent != bytes.len() {
        return Err(io::Error::other("short write on a SEQPACKET socket"));
    }
    Ok(())
}

/// Writes `event` as one datagram. A full socket is `WouldBlock`: the event is dropped, as the real EventThread drops one for a connection whose BitTube is full.
pub fn send_event(tube: impl AsFd, event: &VsyncEvent) -> io::Result<()> {
    send_datagram(tube, &event.to_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use atl_compositor::event::{EVENT_SIZE, FrameTimeline, VsyncEventData};
    use rustix::net::{RecvFlags, recv};

    fn event(count: u32) -> VsyncEvent {
        VsyncEvent {
            display_id: 7,
            timestamp: 1_000_000 * i64::from(count),
            count,
            data: VsyncEventData {
                frame_interval: 16_666_667,
                preferred_frame_timeline_index: 0,
                frame_timelines: vec![FrameTimeline {
                    vsync_id: i64::from(count),
                    deadline_timestamp: 5,
                    expected_presentation_time: 9,
                }],
                number_queued_buffers: 0,
            },
        }
    }

    /// Two events are two datagrams in order; the reader gets each whole and decodes it; with nothing queued the non-blocking read says so.
    #[test]
    fn one_event_per_datagram_in_order() {
        let (tx, rx) = channel().expect("socketpair");
        send_event(&tx, &event(1)).expect("send");
        send_event(&tx, &event(2)).expect("send");
        let mut buf = [0u8; 4 * EVENT_SIZE];
        for count in [1, 2] {
            let (n, _) = recv(&rx, &mut buf, RecvFlags::empty()).expect("recv");
            assert_eq!(n, EVENT_SIZE, "a datagram is one Event");
            let got = VsyncEvent::from_bytes(&buf[..n]).expect("decode");
            assert_eq!(got.count, count);
            assert_eq!(got.timestamp, 1_000_000 * i64::from(count));
        }
        let err = recv(&rx, &mut buf, RecvFlags::empty()).expect_err("empty");
        assert_eq!(err, rustix::io::Errno::AGAIN);
    }

    /// A client that never reads fills the socket: the write then fails WouldBlock instead of blocking the compositor.
    #[test]
    fn a_full_channel_drops_instead_of_blocking() {
        let (tx, _rx) = channel().expect("socketpair");
        let mut result = Ok(());
        // far more events than the default socket buffer holds (hundreds of KiB), so some write must hit the limit
        for count in 0..100_000 {
            result = send_event(&tx, &event(count));
            if result.is_err() {
                break;
            }
        }
        let err = result.expect_err("the buffer fills");
        assert_eq!(err.kind(), io::ErrorKind::WouldBlock);
    }
}
