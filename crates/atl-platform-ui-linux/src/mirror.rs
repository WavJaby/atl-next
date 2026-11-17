// SPDX-License-Identifier: MIT
//! The UI thread's GLib main context, mirrored into its Android Looper (owner decision 2026-11-05, plan Q6): the Looper is the outer loop, so Java's `nativePollOnce` is where GLib runs, with no AOSP patch.
//! GLib's fds and its timeout (a timerfd) sit in the mirror's own epoll set; the Looper holds only that epoll fd. When it is readable, [`Mirror::pass`] collects every ready member: prepare, query, check with that readiness, dispatch (no GLib poll: the Looper polled), then the set is synced to GLib's new fd set.
//! Why a private set, not one Looper registration per GLib fd: registrations are keyed by fd number, and GLib users (WebKit, GIO) close fds and the number is reused before the next sync. Per-fd Looper registrations then (a) kept a reused number with unchanged events "registered" while the kernel had dropped the closed file's epoll entry, so the new fd never woke GLib (the window stopped painting for good), and (b) removed by number an fd that by then belonged to another Looper user (`Looper: Error removing epoll events ... Operation not permitted`). Here a sync re-arms every kept fd (`EPOLL_CTL_MOD`; ENOENT = closed and reused -> `ADD`), and a removal touches only the mirror's set.
//! Level-triggered: a fd still readable after its pass is reported again at once. [`Stats`] reports that (`passes` against what GLib did), the sync cost and the reused fds re-added (`readded`).
//! Thread-bound: the Looper, the context and the mirror are the UI thread's. The Looper holds the sink that holds the mirror, so the mirror lives for the process.
//! Raw FFI: GLib's `g_main_context_*` calls that glib-rs leaves unwrapped (prepare, query, check, dispatch); each names the owned context.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::ffi::c_int;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::rc::{Rc, Weak};
use std::time::{Duration, Instant};

use atl_platform_ui_api::{FdEvents, Looper, UiError};
use gtk4::glib::ffi::{self, GMainContext, GPollFD};
use rustix::event::epoll;
use rustix::io::read;
use rustix::time::{
    Itimerspec, TimerfdClockId, TimerfdFlags, TimerfdTimerFlags, Timespec, timerfd_create,
    timerfd_settime,
};

/// How often the counters are logged (`atl-platform-ui: mirror: ...`, debug level).
const REPORT_EVERY: Duration = Duration::from_secs(5);

/// What the mirror did since install: the spike's measurements.
#[derive(Default)]
struct Stats {
    /// Looper callbacks (the private set readable; GLib's pass each).
    passes: u64,
    /// Kept fds whose re-arm found them closed and the number reused: added again.
    readded: u64,
    timer_fires: u64,
    /// [] calls that synced (UI thread, outside a pass)
    resyncs: u64,
    syncs: u64,
    adds: u64,
    removes: u64,
    sync_ns: u128,
    sync_max_ns: u128,
    fds_max: usize,
}

/// The mirror of one thread's GLib context into its Looper.
pub struct Mirror {
    looper: Box<dyn Looper>,
    ctx: *mut GMainContext,
    timer: OwnedFd,
    /// The private epoll set (GLib's fds + `timer`), the Looper's only fd from this mirror; it lives as long as the mirror, so its number is never reused under the Looper.
    epoll: OwnedFd,
    /// The GLib fds in `epoll`, with the GLib events each was added for.
    registered: BTreeMap<RawFd, u16>,
    /// GLib's last query, in GLib's order: what `check` needs.
    query: Vec<GPollFD>,
    stats: Stats,
    report_at: Instant,
}

/// GLib's readiness bits (`G_IO_*`): what `query` asks for and `check` answers with.
const G_IN: u16 = ffi::G_IO_IN as u16;
const G_PRI: u16 = ffi::G_IO_PRI as u16;
const G_OUT: u16 = ffi::G_IO_OUT as u16;
const G_ERR: u16 = ffi::G_IO_ERR as u16;
const G_HUP: u16 = ffi::G_IO_HUP as u16;

/// GLib's events -> epoll's. PRI is read as input; ERR and HUP are reported on every poll, so they are not asked for.
fn epoll_events(glib_events: u16) -> u32 {
    let mut events = 0;
    if glib_events & (G_IN | G_PRI) != 0 {
        events |= libc::EPOLLIN as u32;
    }
    if glib_events & G_OUT != 0 {
        events |= libc::EPOLLOUT as u32;
    }
    events
}

/// epoll's readiness -> GLib's `revents`.
fn glib_revents(events: u32) -> u16 {
    let mut revents = 0;
    for (e, g) in [
        (libc::EPOLLIN, G_IN),
        (libc::EPOLLOUT, G_OUT),
        (libc::EPOLLERR, G_ERR),
        (libc::EPOLLHUP, G_HUP),
    ] {
        if events & e as u32 != 0 {
            revents |= g;
        }
    }
    revents
}

/// `epoll_ctl(op)` of `fd` in `epoll`, the fd number as its data. Raw fds: GLib owns them, and a closed one is a valid (failing) argument here.
fn epoll_ctl(epoll: &OwnedFd, op: c_int, fd: RawFd, events: u32) -> std::io::Result<()> {
    let mut event = libc::epoll_event {
        events,
        u64: u64::try_from(fd).unwrap_or(u64::MAX),
    };
    // SAFETY: an epoll fd this mirror owns, any fd number (the kernel validates it), and an event that lives across the call
    let rc = unsafe { libc::epoll_ctl(epoll.as_raw_fd(), op, fd, &raw mut event) };
    if rc < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// The ready members of `epoll` now, without waiting: (fd, epoll events).
fn ready_now(epoll: &OwnedFd) -> Vec<(RawFd, u32)> {
    let mut events = [libc::epoll_event { events: 0, u64: 0 }; 64];
    // SAFETY: an epoll fd this mirror owns and a writable array of the stated length; timeout 0 never blocks
    let n = unsafe {
        libc::epoll_wait(
            epoll.as_raw_fd(),
            events.as_mut_ptr(),
            c_int::try_from(events.len()).unwrap_or(c_int::MAX),
            0,
        )
    };
    let n = usize::try_from(n).unwrap_or(0);
    events[..n]
        .iter()
        .map(|e| (RawFd::try_from(e.u64).unwrap_or(-1), e.events))
        .collect()
}

impl Mirror {
    /// The mirror of `ctx` (acquired by the caller) with its private set holding the timer; not yet on the Looper.
    fn new(looper: Box<dyn Looper>, ctx: *mut GMainContext) -> Result<Self, UiError> {
        let timer = timerfd_create(
            TimerfdClockId::Monotonic,
            TimerfdFlags::CLOEXEC | TimerfdFlags::NONBLOCK,
        )
        .map_err(|e| UiError::Looper(format!("timerfd_create: {e}")))?;
        let epoll = epoll::create(epoll::CreateFlags::CLOEXEC)
            .map_err(|e| UiError::Looper(format!("epoll_create: {e}")))?;
        epoll_ctl(
            &epoll,
            libc::EPOLL_CTL_ADD,
            timer.as_raw_fd(),
            libc::EPOLLIN as u32,
        )
        .map_err(|e| UiError::Looper(format!("adding the timerfd: {e}")))?;
        Ok(Mirror {
            looper,
            ctx,
            timer,
            epoll,
            registered: BTreeMap::new(),
            query: Vec::new(),
            stats: Stats::default(),
            report_at: Instant::now(),
        })
    }

    /// One pass, from the Looper's callback on the private set: GLib's non-blocking iteration with the readiness of every ready member, then the sync.
    fn pass(&mut self) {
        self.stats.passes += 1;
        let ready = ready_now(&self.epoll);
        let mut revents: BTreeMap<RawFd, u16> = BTreeMap::new();
        for (fd, events) in ready {
            if fd == self.timer.as_raw_fd() {
                self.stats.timer_fires += 1;
                // level-triggered: an unread expiry count would report again at once
                let mut count = [0u8; 8];
                let _ = read(&self.timer, &mut count);
            } else {
                revents.insert(fd, glib_revents(events));
            }
        }
        let (mut priority, mut timeout) = (0, -1);
        let n = self.query(&mut priority, &mut timeout);
        for poll in &mut self.query[..n] {
            poll.revents = revents
                .get(&poll.fd)
                .map_or(0, |r| r & (poll.events | G_ERR | G_HUP));
        }
        // SAFETY: the owned context (acquired in `install`), the priority and the array `query` filled for this iteration
        let ready = unsafe {
            ffi::g_main_context_check(
                self.ctx,
                priority,
                self.query.as_mut_ptr(),
                c_int::try_from(n).unwrap_or(c_int::MAX),
            )
        };
        if ready != 0 {
            // SAFETY: the owned context, acquired by this thread
            unsafe { ffi::g_main_context_dispatch(self.ctx) };
        }
        self.sync();
    }

    /// GLib's prepare + query into `self.query`; returns how many entries are valid. GLib names the count it needs when the array is short, so it is grown and asked again.
    fn query(&mut self, priority: &mut c_int, timeout: &mut c_int) -> usize {
        // SAFETY: the owned context; prepare's ready flag is not needed here (check decides readiness)
        unsafe { ffi::g_main_context_prepare(self.ctx, priority) };
        loop {
            let capacity = self.query.len();
            // SAFETY: the owned context and a writable array of `capacity` entries, as the count says
            let needed = unsafe {
                ffi::g_main_context_query(
                    self.ctx,
                    *priority,
                    timeout,
                    self.query.as_mut_ptr(),
                    c_int::try_from(capacity).unwrap_or(c_int::MAX),
                )
            };
            let needed = usize::try_from(needed).unwrap_or(0);
            if needed <= capacity {
                return needed;
            }
            self.query.resize_with(needed, || GPollFD {
                fd: -1,
                events: 0,
                revents: 0,
            });
        }
    }

    /// Makes the private set GLib's current set: removes what is gone, re-arms what is kept (a closed and reused number is added again), adds what is new, and arms the timerfd for GLib's timeout (none: disarmed).
    fn sync(&mut self) {
        let started = Instant::now();
        let (mut priority, mut timeout) = (0, -1);
        let n = self.query(&mut priority, &mut timeout);
        let mut want: BTreeMap<RawFd, u16> = BTreeMap::new();
        for poll in &self.query[..n] {
            *want.entry(poll.fd).or_insert(0) |= poll.events;
        }
        let gone: Vec<RawFd> = self
            .registered
            .keys()
            .filter(|fd| !want.contains_key(fd))
            .copied()
            .collect();
        for fd in gone {
            self.registered.remove(&fd);
            // a closed fd left the set with its file (ENOENT/EBADF; a reused number's new file: EPERM or ENOENT): nothing else to undo
            if epoll_ctl(&self.epoll, libc::EPOLL_CTL_DEL, fd, 0).is_ok() {
                self.stats.removes += 1;
            }
        }
        for (&fd, &events) in &want {
            if self.registered.contains_key(&fd) {
                // re-armed every sync: a number whose file was closed and reused is no longer in the set (ENOENT), whatever its events
                match epoll_ctl(&self.epoll, libc::EPOLL_CTL_MOD, fd, epoll_events(events)) {
                    Ok(()) => {
                        self.registered.insert(fd, events);
                        continue;
                    }
                    Err(e) if e.raw_os_error() == Some(libc::ENOENT) => self.stats.readded += 1,
                    Err(e) => {
                        tracing::warn!("atl-platform-ui: mirror: re-arming fd {fd}: {e}");
                        self.registered.remove(&fd);
                        continue;
                    }
                }
            }
            if let Err(e) = epoll_ctl(&self.epoll, libc::EPOLL_CTL_ADD, fd, epoll_events(events)) {
                tracing::warn!("atl-platform-ui: mirror: adding fd {fd}: {e}");
                self.registered.remove(&fd);
                continue;
            }
            self.registered.insert(fd, events);
            self.stats.adds += 1;
        }
        // query's prepare took GDK's Wayland read intent; only check releases it. Left held, a roundtrip on this thread (EGL's, in a window's realise) waits on its own reader: the stall of 2026-11-05. No fd is reported ready here, so nothing is read; sources due by time are dispatched, and a dispatch may add sources, so the next pass re-syncs at once.
        for poll in &mut self.query[..n] {
            poll.revents = 0;
        }
        // SAFETY: the owned context (acquired in `install`), the priority prepare set, and the array `query` filled for `n` entries, with revents all zero
        let ready = unsafe {
            ffi::g_main_context_check(
                self.ctx,
                priority,
                self.query.as_mut_ptr(),
                c_int::try_from(n).unwrap_or(c_int::MAX),
            )
        };
        if ready != 0 {
            // SAFETY: the owned context, acquired by this thread
            unsafe { ffi::g_main_context_dispatch(self.ctx) };
            timeout = 0;
        }
        self.arm(timeout);
        let took = started.elapsed().as_nanos();
        self.stats.syncs += 1;
        self.stats.sync_ns += took;
        self.stats.sync_max_ns = self.stats.sync_max_ns.max(took);
        self.stats.fds_max = self.stats.fds_max.max(n);
        if self.report_at.elapsed() >= REPORT_EVERY {
            self.report();
            self.report_at = Instant::now();
        }
    }

    /// The timerfd fires at GLib's timeout; 0 (ready now) fires at once; negative (no timeout) disarms it.
    fn arm(&self, timeout_ms: c_int) {
        let value = match u64::try_from(timeout_ms) {
            Err(_) => Timespec {
                tv_sec: 0,
                tv_nsec: 0,
            },
            Ok(0) => Timespec {
                tv_sec: 0,
                tv_nsec: 1,
            },
            Ok(ms) => {
                let t = Duration::from_millis(ms);
                Timespec {
                    tv_sec: i64::try_from(t.as_secs()).unwrap_or(i64::MAX),
                    tv_nsec: i64::from(t.subsec_nanos()),
                }
            }
        };
        let spec = Itimerspec {
            it_interval: Timespec {
                tv_sec: 0,
                tv_nsec: 0,
            },
            it_value: value,
        };
        if let Err(e) = timerfd_settime(&self.timer, TimerfdTimerFlags::empty(), &spec) {
            tracing::warn!("atl-platform-ui: mirror: timerfd_settime: {e}");
        }
    }

    fn report(&self) {
        let s = &self.stats;
        let avg = s.sync_ns.checked_div(u128::from(s.syncs)).unwrap_or(0);
        tracing::debug!(
            "atl-platform-ui: mirror: passes={} readded={} timer_fires={} resyncs={} syncs={} sync_avg_ns={avg} sync_max_ns={} fds={} fds_max={} adds={} removes={}",
            s.passes,
            s.readded,
            s.timer_fires,
            s.resyncs,
            s.syncs,
            s.sync_max_ns,
            self.registered.len(),
            s.fds_max,
            s.adds,
            s.removes,
        );
    }
}

thread_local! {
    /// The UI thread's mirror, for [`resync`]; empty on every other thread.
    static MIRROR: RefCell<Weak<RefCell<Mirror>>> = const { RefCell::new(Weak::new()) };
}

/// Syncs the Looper's registrations to GLib now, when called on the UI thread outside a pass; a no-op inside a pass (the pass syncs when it ends) and on any other thread (GLib wakes the owner itself).
/// Needed after any GLib change the UI thread makes outside a pass (Java calling in from the Looper, e.g. the app's vsync request): a source attached by the context's owner writes no wakeup (GLib `g_source_attach_unlocked` wakes only another owner thread), so its fd or timeout would otherwise never reach the Looper and never fire.
/// During thread-local teardown (a window the process drops from its own thread-local destructor at exit) the slot may already be gone: then nothing syncs, the loop is ending (`with` would abort there).
pub fn resync() {
    let mirror = MIRROR.try_with(|m| m.borrow().upgrade()).ok().flatten();
    if let Some(mirror) = mirror
        && let Ok(mut m) = mirror.try_borrow_mut()
    {
        m.stats.resyncs += 1;
        m.sync();
    }
}

/// Installs the mirror on `looper`, the calling thread's Looper, after GTK init on this thread: the default context is acquired here, so every call on it is this thread's, and its fds are registered now.
pub fn install(looper: Box<dyn Looper>) -> Result<(), UiError> {
    // SAFETY: no arguments; the process's default context, created on first use
    let ctx = unsafe { ffi::g_main_context_default() };
    // SAFETY: the context; acquiring makes this thread its owner for every later call
    let acquired = unsafe { ffi::g_main_context_acquire(ctx) };
    if acquired == 0 {
        return Err(UiError::Unavailable(
            "the default GLib main context is owned by another thread".to_owned(),
        ));
    }
    let mirror = Rc::new(RefCell::new(Mirror::new(looper, ctx)?));
    let sink = Rc::clone(&mirror);
    // one registration, the private set's: every readiness of it is one pass
    mirror
        .borrow()
        .looper
        .set_sink(Box::new(move |_, _| sink.borrow_mut().pass()));
    MIRROR.with_borrow_mut(|slot| *slot = Rc::downgrade(&mirror));
    let mut m = mirror.borrow_mut();
    m.looper.add_fd(
        m.epoll.as_raw_fd(),
        FdEvents {
            input: true,
            ..FdEvents::default()
        },
    )?;
    m.sync();
    // one line at install: a UI-mode process shows its mirror in the app log even when no callback ever fires
    m.report();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use gtk4::glib::{ControlFlow, IOCondition};
    use std::cell::Cell;

    type Sink = RefCell<Option<Box<dyn FnMut(RawFd, FdEvents)>>>;

    /// A Looper as AOSP's behaves for this purpose: registrations keyed by fd number in one epoll set, a callback per ready fd.
    #[derive(Clone)]
    struct EpollLooper {
        epoll: Rc<OwnedFd>,
        sink: Rc<Sink>,
    }

    impl Looper for EpollLooper {
        fn set_sink(&self, sink: Box<dyn FnMut(RawFd, FdEvents)>) {
            *self.sink.borrow_mut() = Some(sink);
        }
        fn add_fd(&self, fd: RawFd, events: FdEvents) -> Result<(), UiError> {
            let mut e = 0;
            if events.input {
                e |= libc::EPOLLIN as u32;
            }
            if events.output {
                e |= libc::EPOLLOUT as u32;
            }
            // AOSP Looper::addFd: MOD an existing number, ADD on ENOENT
            epoll_ctl(&self.epoll, libc::EPOLL_CTL_MOD, fd, e)
                .or_else(|_| epoll_ctl(&self.epoll, libc::EPOLL_CTL_ADD, fd, e))
                .map_err(|err| UiError::Looper(err.to_string()))
        }
        fn remove_fd(&self, fd: RawFd) -> Result<(), UiError> {
            epoll_ctl(&self.epoll, libc::EPOLL_CTL_DEL, fd, 0)
                .map_err(|err| UiError::Looper(err.to_string()))
        }
    }

    impl EpollLooper {
        /// One `pollOnce`: waits up to `ms`, then runs the sink for each ready fd.
        fn poll_once(&self, ms: c_int) {
            let mut events = [libc::epoll_event { events: 0, u64: 0 }; 16];
            // SAFETY: an epoll fd this looper owns and a writable array of the stated length
            let n =
                unsafe { libc::epoll_wait(self.epoll.as_raw_fd(), events.as_mut_ptr(), 16, ms) };
            for e in &events[..usize::try_from(n).unwrap_or(0)] {
                let fd = RawFd::try_from(e.u64).expect("an fd number");
                let ready = FdEvents {
                    input: e.events & libc::EPOLLIN as u32 != 0,
                    output: e.events & libc::EPOLLOUT as u32 != 0,
                    error: e.events & libc::EPOLLERR as u32 != 0,
                    hangup: e.events & libc::EPOLLHUP as u32 != 0,
                };
                if let Some(sink) = self.sink.borrow_mut().as_mut() {
                    sink(fd, ready);
                }
            }
        }
    }

    fn pipe() -> (OwnedFd, OwnedFd) {
        use std::os::fd::FromRawFd;
        let mut fds = [0; 2];
        // SAFETY: a writable array of two fds
        let rc = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) };
        assert_eq!(rc, 0, "pipe2");
        // SAFETY: two fresh fds pipe2 returned, owned from here on
        unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) }
    }

    /// A GLib fd closed and its number reused for a new fd with the same events between two syncs (WebKit/GIO do this): the new fd must still wake GLib.
    /// Per-fd Looper registrations kept the number "registered" while the kernel had dropped the closed file's epoll entry, so the new source never ran.
    #[test]
    fn a_closed_and_reused_fd_still_wakes_glib() {
        let looper = EpollLooper {
            epoll: Rc::new(epoll::create(epoll::CreateFlags::CLOEXEC).expect("epoll")),
            sink: Rc::new(RefCell::new(None)),
        };
        install(Box::new(looper.clone())).expect("install");
        let (r1, w1) = pipe();
        let number = r1.as_raw_fd();
        let first =
            glib_unix::unix_fd_add_local(number, IOCondition::IN, |_, _| ControlFlow::Continue);
        resync();
        first.remove();
        drop((r1, w1));
        let (r2, w2) = pipe();
        assert_eq!(r2.as_raw_fd(), number, "the lowest free number is reused");
        let ran = Rc::new(Cell::new(false));
        let seen = Rc::clone(&ran);
        let _second = glib_unix::unix_fd_add_local(r2.as_raw_fd(), IOCondition::IN, move |_, _| {
            seen.set(true);
            ControlFlow::Break
        });
        resync();
        rustix::io::write(&w2, b"x").expect("write");
        for _ in 0..10 {
            looper.poll_once(50);
            if ran.get() {
                break;
            }
        }
        assert!(ran.get(), "the source on the reused fd never ran");
    }
}
