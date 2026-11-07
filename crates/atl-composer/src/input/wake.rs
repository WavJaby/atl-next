// SPDX-License-Identifier: MIT
//! The input loop's wake-up. Messages from any thread queue on an mpsc channel; one byte on a socket pair wakes the loop when the queue goes from idle to busy, and the loop polls the socket's read end with every window's channel ([`InputReceiver::wait`], no timeout in production), so it sleeps without a timer.
//! Protocol: a send pushes its message, then swaps `armed` to true and writes the byte only if it was false. The loop waits, drains the socket, swaps `armed` back to false ([`InputReceiver::ack`]), and only then takes the queue.
//! A send that swapped false is covered by its own byte. A send that swapped true is covered by the next ack, which clears `armed` only after that send's push. No counter: a count the loop decrements per message races the send that increments it.

use std::io;
use std::os::fd::{BorrowedFd, OwnedFd};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, SendError, TryIter};

use rustix::event::{PollFd, PollFlags, poll};
use rustix::io::Errno;
use rustix::net::{AddressFamily, SocketFlags, SocketType, socketpair};
use rustix::time::Timespec;

/// The state both ends share.
struct Shared {
    /// True from a send's swap until the loop's ack: at most one byte per ack.
    armed: AtomicBool,
    /// The write end of the socket pair.
    write: OwnedFd,
    /// false: the deliberate-failure break (start.rs BREAK_INPUT_NO_WAKE). Sends still queue; none writes the byte.
    wake_enabled: bool,
}

/// The sending end, `Clone + Send + Sync`: any thread may send.
pub struct InputSender<M> {
    tx: mpsc::Sender<M>,
    shared: Arc<Shared>,
}

impl<M> Clone for InputSender<M> {
    fn clone(&self) -> Self {
        Self {
            tx: self.tx.clone(),
            shared: Arc::clone(&self.shared),
        }
    }
}

/// The loop's end: only the loop holds it.
pub struct InputReceiver<M> {
    rx: mpsc::Receiver<M>,
    /// The read end: [`InputReceiver::wait`] polls it, [`InputReceiver::ack`] drains it.
    read: OwnedFd,
    shared: Arc<Shared>,
}

/// One wait's result: whether the wake end was readable, and each channel that reported events (POLLIN, POLLHUP, POLLERR, POLLNVAL), in the order given.
pub struct Round<K> {
    pub wake: bool,
    pub channels: Vec<(K, PollFlags)>,
}

/// A queue with its wake pair. `wake_enabled` false: sends queue their messages and write no byte, so a loop sleeps through them.
pub fn channel<M>(wake_enabled: bool) -> io::Result<(InputSender<M>, InputReceiver<M>)> {
    let (read, write) = socketpair(
        AddressFamily::UNIX,
        SocketType::STREAM,
        SocketFlags::CLOEXEC | SocketFlags::NONBLOCK,
        None,
    )?;
    let shared = Arc::new(Shared {
        armed: AtomicBool::new(false),
        write,
        wake_enabled,
    });
    let (tx, rx) = mpsc::channel();
    Ok((
        InputSender {
            tx,
            shared: Arc::clone(&shared),
        },
        InputReceiver { rx, read, shared },
    ))
}

impl<M> InputSender<M> {
    /// Queues `message`, and wakes the loop if it was idle. Errs once the loop has dropped its receiver.
    pub fn send(&self, message: M) -> Result<(), SendError<M>> {
        self.tx.send(message)?;
        if !self.shared.armed.swap(true, Ordering::AcqRel) && self.shared.wake_enabled {
            write_wake(&self.shared.write);
        }
        Ok(())
    }
}

impl<M> InputReceiver<M> {
    /// Drains the wake bytes, then clears `armed`. Call it after a wait that reported the wake end, then take the queue with [`InputReceiver::try_iter`].
    pub fn ack(&self) {
        let mut buf = [0_u8; 64];
        loop {
            match rustix::io::read(&self.read, &mut buf) {
                Ok(0) | Err(Errno::AGAIN) => break,
                Ok(_) | Err(Errno::INTR) => {}
                Err(err) => {
                    tracing::warn!("input: wake read: {err:?}");
                    break;
                }
            }
        }
        self.shared.armed.swap(false, Ordering::AcqRel);
    }

    /// The queued messages, in send order, without waiting.
    pub fn try_iter(&self) -> TryIter<'_, M> {
        self.rx.try_iter()
    }

    /// Waits until the wake end or a channel is readable, hung up or failed. `timeout` None waits without limit: the loop passes None, tests bound it.
    /// Errs when the wake end itself reports a hang-up or error, which means no sender can reach the loop. Interrupted waits restart.
    pub fn wait<K: Clone>(
        &self,
        channels: &[(K, BorrowedFd<'_>)],
        timeout: Option<&Timespec>,
    ) -> io::Result<Round<K>> {
        let mut fds = Vec::with_capacity(channels.len() + 1);
        fds.push(PollFd::new(&self.read, PollFlags::IN));
        fds.extend(
            channels
                .iter()
                .map(|(_, fd)| PollFd::new(fd, PollFlags::IN)),
        );
        loop {
            match poll(&mut fds, timeout) {
                Ok(_) => break,
                Err(Errno::INTR) => {}
                Err(err) => return Err(err.into()),
            }
        }
        let wake = fds[0].revents();
        if wake.intersects(PollFlags::ERR | PollFlags::HUP | PollFlags::NVAL) {
            return Err(io::Error::other(format!("input wake end: {wake:?}")));
        }
        let channels = channels
            .iter()
            .zip(&fds[1..])
            .filter_map(|((key, _), fd)| {
                let revents = fd.revents();
                (!revents.is_empty()).then(|| (key.clone(), revents))
            })
            .collect();
        Ok(Round {
            wake: wake.contains(PollFlags::IN),
            channels,
        })
    }
}

/// One wake byte. EAGAIN: the buffer holds a byte already, so the loop wakes anyway. EPIPE: the loop is gone.
fn write_wake(write: &OwnedFd) {
    loop {
        match rustix::io::write(write, &[1]) {
            Err(Errno::INTR) => {}
            Err(err) if err != Errno::AGAIN && err != Errno::PIPE => {
                tracing::warn!("input: wake write: {err:?}");
                return;
            }
            _ => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::thread;

    use super::*;

    /// The bound a wait gets in tests: a wake that is coming arrives long before it; a missing one fails the test.
    const BOUND: Timespec = Timespec {
        tv_sec: 5,
        tv_nsec: 0,
    };

    /// Wake bytes waiting on the read end, read without blocking.
    fn wake_bytes<M>(rx: &InputReceiver<M>) -> usize {
        let mut buf = [0_u8; 64];
        let mut total = 0;
        loop {
            match rustix::io::read(&rx.read, &mut buf) {
                Ok(n) if n > 0 => total += n,
                _ => return total,
            }
        }
    }

    /// Three sends wake the loop with one byte. An ack clears it; the next send wakes again; the messages come out in send order.
    #[test]
    fn one_wake_per_ack_in_send_order() {
        let (tx, rx) = channel::<u32>(true).expect("wake pair");
        for n in 1..=3 {
            tx.send(n).expect("queue");
        }
        assert_eq!(wake_bytes(&rx), 1);
        rx.ack();
        assert_eq!(rx.try_iter().collect::<Vec<_>>(), [1, 2, 3]);
        tx.send(4).expect("queue");
        assert_eq!(wake_bytes(&rx), 1);
    }

    /// The break queues the message without a byte: a short bounded wait reports no wake, and the message is there after the ack.
    #[test]
    fn the_break_writes_no_wake() {
        let short = Timespec {
            tv_sec: 0,
            tv_nsec: 50_000_000,
        };
        let (tx, rx) = channel::<u32>(false).expect("wake pair");
        tx.send(1).expect("queue");
        assert_eq!(wake_bytes(&rx), 0);
        let round = rx.wait::<()>(&[], Some(&short)).expect("wait");
        assert!(!round.wake);
        rx.ack();
        assert_eq!(rx.try_iter().collect::<Vec<_>>(), [1]);
    }

    /// Four threads send 1000 messages each while the loop waits with a bound: all 4000 arrive, none stays queued behind a sleeping loop.
    #[test]
    fn concurrent_sends_all_arrive() {
        const SENDERS: usize = 4;
        const EACH: usize = 1000;
        let (tx, rx) = channel::<usize>(true).expect("wake pair");
        let threads: Vec<_> = (0..SENDERS)
            .map(|_| {
                let tx = tx.clone();
                thread::spawn(move || {
                    for n in 0..EACH {
                        tx.send(n).expect("queue");
                    }
                })
            })
            .collect();
        let mut seen = 0;
        while seen < SENDERS * EACH {
            let round = rx.wait::<()>(&[], Some(&BOUND)).expect("wait");
            assert!(
                round.wake,
                "no wake in 5 s with {seen} of {} delivered",
                SENDERS * EACH
            );
            rx.ack();
            seen += rx.try_iter().count();
        }
        for thread in threads {
            thread.join().expect("sender");
        }
    }
}
