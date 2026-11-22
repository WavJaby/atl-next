// SPDX-License-Identifier: MIT
//! The activity service's delayed messages: AOSP's Handler.sendMessageAtTime / removeMessages on uptime (ActivityTaskSupervisor's KILL_TASK_PROCESSES_TIMEOUT_MSG is one), run by one thread instead of a sleeper thread per timeout.
//! [`DelayQueue`] is the rule and reads no clock: deadline order, FIFO at an equal deadline (MessageQueue.enqueueMessage), due when `at <= now` (MessageQueue.next).
//! [`AmHandler`] is the thread: it pops each message when due and calls `dispatch` with no lock of its own held, so the dispatch may take the service's state lock.
//! Monotonic time only (`Instant`): alarms need the wall clock and suspend semantics, which alarm_timer.rs owns; AOSP's activity timeouts are uptime too.

use std::collections::BTreeMap;
use std::io;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Instant;

/// Messages by deadline. `seq` breaks ties in post order, so messages with an equal deadline pop FIFO.
pub struct DelayQueue<K, M> {
    seq: u64,
    due: BTreeMap<(Instant, u64), (K, M)>,
}

impl<K, M> Default for DelayQueue<K, M> {
    fn default() -> Self {
        DelayQueue {
            seq: 0,
            due: BTreeMap::new(),
        }
    }
}

impl<K, M> DelayQueue<K, M> {
    /// Handler.sendMessageAtTime: due at `at`, after every message already posted for the same instant.
    pub fn post(&mut self, key: K, msg: M, at: Instant) {
        self.due.insert((at, self.seq), (key, msg));
        self.seq += 1;
    }

    /// The earliest message, removed, if it is due at or before `now`.
    pub fn pop_due(&mut self, now: Instant) -> Option<M> {
        let entry = self.due.first_entry()?;
        if entry.key().0 > now {
            return None;
        }
        Some(entry.remove().1)
    }

    /// When the earliest message falls due: the thread's wait bound.
    pub fn next_due(&self) -> Option<Instant> {
        self.due.first_key_value().map(|(&(at, _), _)| at)
    }
}

impl<K: PartialEq, M> DelayQueue<K, M> {
    /// Handler.removeMessages(key): every message posted with `key` leaves the queue.
    pub fn remove(&mut self, key: &K) {
        self.due.retain(|_, (k, _)| *k != *key);
    }
}

/// What the owner and its thread share: the queue, and whether the owner has been dropped.
struct Shared<K, M> {
    pending: Mutex<Pending<K, M>>,
    wake: Condvar,
}

struct Pending<K, M> {
    queue: DelayQueue<K, M>,
    closed: bool,
}

/// One thread for a service's delayed messages. Dropping the handler ends the thread; messages not yet due are discarded.
pub struct AmHandler<K, M> {
    shared: Arc<Shared<K, M>>,
}

impl<K: PartialEq + Send + 'static, M: Send + 'static> AmHandler<K, M> {
    /// Starts thread `name`. `dispatch` runs each message on it when due. The owner must reach its state through a weak reference captured by `dispatch`: the thread must not keep the owner alive.
    pub fn spawn(name: &str, dispatch: impl Fn(M) + Send + 'static) -> io::Result<Self> {
        let shared = Arc::new(Shared {
            pending: Mutex::new(Pending {
                queue: DelayQueue::default(),
                closed: false,
            }),
            wake: Condvar::new(),
        });
        let thread = Arc::clone(&shared);
        std::thread::Builder::new()
            .name(name.into())
            .spawn(move || run(&thread, dispatch))?;
        Ok(AmHandler { shared })
    }

    /// Handler.sendMessageAtTime (see [`DelayQueue::post`]).
    pub fn post(&self, key: K, msg: M, at: Instant) {
        self.lock().queue.post(key, msg, at);
        self.shared.wake.notify_one();
    }

    /// Handler.removeMessages (see [`DelayQueue::remove`]). No wake: a removed head only makes the thread wake early and sleep again.
    pub fn remove(&self, key: &K) {
        self.lock().queue.remove(key);
    }

    fn lock(&self) -> MutexGuard<'_, Pending<K, M>> {
        self.shared
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }
}

impl<K, M> Drop for AmHandler<K, M> {
    fn drop(&mut self) {
        let mut pending = self
            .shared
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        pending.closed = true;
        drop(pending);
        self.shared.wake.notify_one();
    }
}

/// The thread: pop the earliest message once due, else wait for its deadline, a post or the owner's drop. `dispatch` runs with the queue lock released.
fn run<K, M>(shared: &Shared<K, M>, dispatch: impl Fn(M)) {
    loop {
        let msg = {
            let mut pending = shared.pending.lock().unwrap_or_else(|e| e.into_inner());
            loop {
                if pending.closed {
                    return;
                }
                let now = Instant::now();
                if let Some(msg) = pending.queue.pop_due(now) {
                    break msg;
                }
                pending = match pending.queue.next_due() {
                    Some(at) => {
                        shared
                            .wake
                            .wait_timeout(pending, at.saturating_duration_since(now))
                            .unwrap_or_else(|e| e.into_inner())
                            .0
                    }
                    None => shared.wake.wait(pending).unwrap_or_else(|e| e.into_inner()),
                };
            }
        };
        dispatch(msg);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc::{self, RecvTimeoutError};
    use std::time::{Duration, Instant};

    use super::{AmHandler, DelayQueue};

    /// How long a thread test waits for a dispatch: far above the deadlines used here, so a loaded host does not fail it.
    const WAIT: Duration = Duration::from_secs(5);

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    // The expected orders are MessageQueue's (AOSP frameworks/base/core/java/android/os/MessageQueue.java: enqueueMessage, next), not read back from DelayQueue.

    #[test]
    fn pops_in_deadline_order_whatever_the_post_order() {
        let t0 = Instant::now();
        let mut q = DelayQueue::default();
        q.post("a", 'a', t0 + ms(30));
        q.post("b", 'b', t0 + ms(10));
        q.post("c", 'c', t0 + ms(20));
        let now = t0 + ms(100);
        assert_eq!(q.pop_due(now), Some('b'));
        assert_eq!(q.pop_due(now), Some('c'));
        assert_eq!(q.pop_due(now), Some('a'));
        assert_eq!(q.pop_due(now), None);
    }

    #[test]
    fn equal_deadline_is_fifo() {
        let t0 = Instant::now();
        let mut q = DelayQueue::default();
        q.post(0, 'x', t0 + ms(5));
        q.post(1, 'y', t0 + ms(5));
        q.post(2, 'z', t0 + ms(5));
        q.post(3, 'e', t0 + ms(4));
        let now = t0 + ms(5);
        assert_eq!(q.pop_due(now), Some('e'));
        assert_eq!(q.pop_due(now), Some('x'));
        assert_eq!(q.pop_due(now), Some('y'));
        assert_eq!(q.pop_due(now), Some('z'));
        assert_eq!(q.pop_due(now), None);
    }

    #[test]
    fn due_at_its_deadline_not_before() {
        let t0 = Instant::now();
        let mut q = DelayQueue::default();
        q.post(0, 'a', t0 + ms(10));
        assert_eq!(q.next_due(), Some(t0 + ms(10)));
        assert_eq!(q.pop_due(t0 + ms(9)), None);
        assert_eq!(q.pop_due(t0 + ms(10)), Some('a'));
        assert_eq!(q.next_due(), None);
    }

    #[test]
    fn remove_takes_every_message_with_the_key() {
        let t0 = Instant::now();
        let mut q = DelayQueue::default();
        q.post(1, 'a', t0 + ms(10));
        q.post(2, 'b', t0 + ms(10));
        q.post(1, 'c', t0 + ms(20));
        q.remove(&1);
        let now = t0 + ms(100);
        assert_eq!(q.pop_due(now), Some('b'));
        assert_eq!(q.pop_due(now), None);
    }

    #[test]
    fn repost_after_remove_takes_the_new_deadline() {
        let t0 = Instant::now();
        let mut q = DelayQueue::default();
        q.post(7, "old", t0 + ms(10));
        q.remove(&7);
        q.post(7, "new", t0 + ms(20));
        assert_eq!(q.pop_due(t0 + ms(15)), None);
        assert_eq!(q.pop_due(t0 + ms(25)), Some("new"));
        assert_eq!(q.pop_due(t0 + ms(25)), None);
    }

    #[test]
    fn thread_dispatches_in_deadline_order() {
        let (tx, rx) = mpsc::channel();
        let handler = AmHandler::<u8, u32>::spawn("am-handler-test", move |m| {
            let _ = tx.send(m);
        })
        .expect("spawning the test thread");
        let t0 = Instant::now();
        handler.post(0, 1, t0 + ms(150));
        handler.post(0, 2, t0 + ms(50));
        handler.post(0, 3, t0 + ms(50));
        let got: Vec<u32> = (0..3)
            .map(|_| rx.recv_timeout(WAIT).expect("a dispatch"))
            .collect();
        assert_eq!(got, [2, 3, 1]);
    }

    #[test]
    fn thread_never_dispatches_a_removed_message() {
        let (tx, rx) = mpsc::channel();
        let handler = AmHandler::<u8, u32>::spawn("am-handler-test", move |m| {
            let _ = tx.send(m);
        })
        .expect("spawning the test thread");
        let t0 = Instant::now();
        handler.post(1, 10, t0 + ms(50));
        handler.remove(&1);
        handler.post(2, 20, t0 + ms(100));
        // 10 was due first: unless it was removed, it would arrive before 20
        assert_eq!(rx.recv_timeout(WAIT), Ok(20));
    }

    #[test]
    fn dropping_the_handler_ends_the_thread() {
        let (tx, rx) = mpsc::channel::<u32>();
        let handler = AmHandler::<u8, u32>::spawn("am-handler-test", move |m| {
            let _ = tx.send(m);
        })
        .expect("spawning the test thread");
        handler.post(0, 1, Instant::now() + ms(60_000));
        drop(handler);
        // the thread drops its dispatch, and with it the sender, when it ends; the message 60 s out is never sent
        assert_eq!(rx.recv_timeout(WAIT), Err(RecvTimeoutError::Disconnected));
    }
}
