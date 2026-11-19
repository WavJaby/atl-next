// SPDX-License-Identifier: MIT
//! The UI thread's GLib main context, mirrored into its Android Looper (owner decision 2026-11-05, plan Q6): the Looper is the outer loop, so Java's `nativePollOnce` is where GLib runs, with no AOSP patch.
//! GLib's fds sit in the mirror's own epoll set (`set`); it and GLib's timeout (a timerfd) sit in a second one (`epoll`), the Looper's only fd from the mirror. When it is readable, [`Mirror::pass`] collects every ready member of `set`: prepare, query, check with that readiness, dispatch (no GLib poll: the Looper polled), then `set` is synced to GLib's new fd set.
//! Why a private set, not one Looper registration per GLib fd: registrations are keyed by fd number, and GLib users (WebKit, GIO) close fds and the number is reused before the next sync. Per-fd Looper registrations then (a) kept a reused number with unchanged events "registered" while the kernel had dropped the closed file's epoll entry, so the new fd never woke GLib (the window stopped painting for good), and (b) removed by number an fd that by then belonged to another Looper user (`Looper: Error removing epoll events ... Operation not permitted`). Here a sync re-arms every kept fd (`EPOLL_CTL_MOD`; ENOENT = closed and reused -> `ADD`), and a removal touches only the mirror's set.
//! Stale entries: epoll drops an entry only when its open file is released, not when the fd is closed. A GLib fd closed while a dup or a child keeps its file open (WebKit's pid socket, dup'd into the IPC connection) leaves an entry no fd number can reach (`DEL` by number finds the closed or reused fd's file), and it keeps reporting the old file's readiness.
//! So each entry's data is a token, not the fd number: readiness is handed to GLib only through a token still registered (keyed by number, the old file's readiness reached the source on the reused number: WebKit read EAGAIN and aborted), and a ready unknown token rebuilds `set` from GLib's fds (`stale`, `rebuilds`), the only way to drop an unreachable entry.
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
    /// Ready entries whose token is no longer registered (a closed fd's file kept open elsewhere): ignored, and `set` rebuilt.
    stale: u64,
    rebuilds: u64,
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
    /// `timer` + `set`, the Looper's only fd from this mirror; it lives as long as the mirror, so its number is never reused under the Looper.
    epoll: OwnedFd,
    /// GLib's fds; replaced whole by a rebuild.
    set: OwnedFd,
    /// The GLib fds in `set`, each with the token its entry carries as data: unique per add, never an fd number.
    registered: BTreeMap<RawFd, u64>,
    next_token: u64,
    /// A stale entry fired: the next sync rebuilds `set`.
    rebuild: bool,
    /// GLib's last query, in GLib's order: what `check` needs.
    query: Vec<GPollFD>,
    stats: Stats,
    report_at: Instant,
}

/// `epoll`'s members, by their data.
const TIMER: u64 = 0;
const SET: u64 = 1;

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

/// `epoll_ctl(op)` of `fd` in `epoll` with `data`. Raw fds: GLib owns them, and a closed one is a valid (failing) argument here.
fn epoll_ctl(epoll: &OwnedFd, op: c_int, fd: RawFd, events: u32, data: u64) -> std::io::Result<()> {
    let mut event = libc::epoll_event { events, u64: data };
    // SAFETY: an epoll fd this mirror owns, any fd number (the kernel validates it), and an event that lives across the call
    let rc = unsafe { libc::epoll_ctl(epoll.as_raw_fd(), op, fd, &raw mut event) };
    if rc < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// The ready members of `epoll` now, without waiting: (data, epoll events).
fn ready_now(epoll: &OwnedFd) -> Vec<(u64, u32)> {
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
    events[..n].iter().map(|e| (e.u64, e.events)).collect()
}

fn new_epoll() -> Result<OwnedFd, UiError> {
    epoll::create(epoll::CreateFlags::CLOEXEC)
        .map_err(|e| UiError::Looper(format!("epoll_create: {e}")))
}

impl Mirror {
    /// The mirror of `ctx` (acquired by the caller) with its private set holding the timer; not yet on the Looper.
    fn new(looper: Box<dyn Looper>, ctx: *mut GMainContext) -> Result<Self, UiError> {
        let timer = timerfd_create(
            TimerfdClockId::Monotonic,
            TimerfdFlags::CLOEXEC | TimerfdFlags::NONBLOCK,
        )
        .map_err(|e| UiError::Looper(format!("timerfd_create: {e}")))?;
        let (epoll, set) = (new_epoll()?, new_epoll()?);
        for (fd, data, what) in [
            (timer.as_raw_fd(), TIMER, "the timerfd"),
            (set.as_raw_fd(), SET, "the fd set"),
        ] {
            epoll_ctl(&epoll, libc::EPOLL_CTL_ADD, fd, libc::EPOLLIN as u32, data)
                .map_err(|e| UiError::Looper(format!("adding {what}: {e}")))?;
        }
        Ok(Mirror {
            looper,
            ctx,
            timer,
            epoll,
            set,
            registered: BTreeMap::new(),
            next_token: 0,
            rebuild: false,
            query: Vec::new(),
            stats: Stats::default(),
            report_at: Instant::now(),
        })
    }

    /// One pass, from the Looper's callback on the private set: GLib's non-blocking iteration with the readiness of every ready member, then the sync.
    fn pass(&mut self) {
        self.stats.passes += 1;
        if ready_now(&self.epoll)
            .iter()
            .any(|&(data, _)| data == TIMER)
        {
            self.stats.timer_fires += 1;
            // level-triggered: an unread expiry count would report again at once
            let mut count = [0u8; 8];
            let _ = read(&self.timer, &mut count);
        }
        let mut revents: BTreeMap<RawFd, u16> = BTreeMap::new();
        for (token, events) in ready_now(&self.set) {
            // a handful of fds: a scan, not a second map
            match self.registered.iter().find(|&(_, &t)| t == token) {
                Some((&fd, _)) => *revents.entry(fd).or_insert(0) |= glib_revents(events),
                None => {
                    self.stats.stale += 1;
                    self.rebuild = true;
                }
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
        if self.rebuild {
            self.rebuild_set();
        }
        let gone: Vec<RawFd> = self
            .registered
            .keys()
            .filter(|fd| !want.contains_key(fd))
            .copied()
            .collect();
        for fd in gone {
            self.registered.remove(&fd);
            // DEL fails for a closed fd (EBADF) or a reused number's new file (ENOENT/EPERM); the old entry then survives only while its file is open elsewhere, and its token is no longer registered
            if epoll_ctl(&self.set, libc::EPOLL_CTL_DEL, fd, 0, 0).is_ok() {
                self.stats.removes += 1;
            }
        }
        for (&fd, &events) in &want {
            if let Some(&token) = self.registered.get(&fd) {
                // re-armed every sync: a number whose file was closed and reused is no longer in the set (ENOENT), whatever its events
                match epoll_ctl(
                    &self.set,
                    libc::EPOLL_CTL_MOD,
                    fd,
                    epoll_events(events),
                    token,
                ) {
                    Ok(()) => continue,
                    Err(e) if e.raw_os_error() == Some(libc::ENOENT) => self.stats.readded += 1,
                    Err(e) => {
                        tracing::warn!("atl-platform-ui: mirror: re-arming fd {fd}: {e}");
                        self.registered.remove(&fd);
                        continue;
                    }
                }
            }
            // a new token: the closed file's entry may live on with the old one
            let token = self.next_token;
            self.next_token += 1;
            if let Err(e) = epoll_ctl(
                &self.set,
                libc::EPOLL_CTL_ADD,
                fd,
                epoll_events(events),
                token,
            ) {
                tracing::warn!("atl-platform-ui: mirror: adding fd {fd}: {e}");
                self.registered.remove(&fd);
                continue;
            }
            self.registered.insert(fd, token);
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

    /// A fresh, empty `set` in place of the old one, which may hold entries no fd number reaches; the sync then adds every GLib fd anew. On failure the old set stays (its stale entry keeps waking passes).
    fn rebuild_set(&mut self) {
        let fresh = new_epoll().and_then(|fresh| {
            epoll_ctl(
                &self.epoll,
                libc::EPOLL_CTL_ADD,
                fresh.as_raw_fd(),
                libc::EPOLLIN as u32,
                SET,
            )
            .map(|()| fresh)
            .map_err(|e| UiError::Looper(format!("adding the fd set: {e}")))
        });
        match fresh {
            Ok(fresh) => {
                // the old set is this mirror's own fd, open until the swap: DEL by its number finds it
                let _ = epoll_ctl(&self.epoll, libc::EPOLL_CTL_DEL, self.set.as_raw_fd(), 0, 0);
                self.set = fresh;
                self.registered.clear();
                self.rebuild = false;
                self.stats.rebuilds += 1;
                tracing::info!(
                    "atl-platform-ui: mirror: a closed fd's entry fired (its file open elsewhere): fd set rebuilt, stale={} rebuilds={}",
                    self.stats.stale,
                    self.stats.rebuilds
                );
            }
            Err(e) => tracing::warn!("atl-platform-ui: mirror: rebuilding the fd set: {e}"),
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
            "atl-platform-ui: mirror: passes={} readded={} stale={} rebuilds={} timer_fires={} resyncs={} syncs={} sync_avg_ns={avg} sync_max_ns={} fds={} fds_max={} adds={} removes={}",
            s.passes,
            s.readded,
            s.stale,
            s.rebuilds,
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
    install_on(looper, unsafe { ffi::g_main_context_default() })
}

/// [`install`] on `ctx` (tests: a context of their own, pushed as the thread default, since the default context has one owner thread per process).
fn install_on(looper: Box<dyn Looper>, ctx: *mut GMainContext) -> Result<(), UiError> {
    // SAFETY: the context; acquiring makes this thread its owner for every later call
    let acquired = unsafe { ffi::g_main_context_acquire(ctx) };
    if acquired == 0 {
        return Err(UiError::Unavailable(
            "the GLib main context is owned by another thread".to_owned(),
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
    use gtk4::glib::{ControlFlow, IOCondition, MainContext, Priority, Source};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

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
            // AOSP Looper::addFd: MOD an existing number, ADD on ENOENT; the fd number as data
            let data = u64::try_from(fd).expect("an fd number");
            epoll_ctl(&self.epoll, libc::EPOLL_CTL_MOD, fd, e, data)
                .or_else(|_| epoll_ctl(&self.epoll, libc::EPOLL_CTL_ADD, fd, e, data))
                .map_err(|err| UiError::Looper(err.to_string()))
        }
        fn remove_fd(&self, fd: RawFd) -> Result<(), UiError> {
            epoll_ctl(&self.epoll, libc::EPOLL_CTL_DEL, fd, 0, 0)
                .map_err(|err| UiError::Looper(err.to_string()))
        }
    }

    /// Runs `body` with a mirror installed on a context of its own (the default context has one owner thread per process, and tests run on several), the Looper and the context handed in.
    fn with_mirror(body: impl FnOnce(&EpollLooper, &MainContext)) {
        use gtk4::glib::translate::ToGlibPtr;
        let looper = EpollLooper {
            epoll: Rc::new(epoll::create(epoll::CreateFlags::CLOEXEC).expect("epoll")),
            sink: Rc::new(RefCell::new(None)),
        };
        let ctx = MainContext::new();
        install_on(Box::new(looper.clone()), ctx.to_glib_none().0).expect("install");
        body(&looper, &ctx);
    }

    /// `old`'s number now names `new`'s file: `old` closed and its number reused in one step (dup3), so no other test thread takes the number in between.
    fn reuse(old: OwnedFd, new: OwnedFd) -> OwnedFd {
        use std::os::fd::{FromRawFd, IntoRawFd};
        let number = old.into_raw_fd();
        // SAFETY: `new` is open; `number` was `old`'s, owned here (into_raw_fd), and dup3 closes its file atomically
        let rc = unsafe { libc::dup3(new.as_raw_fd(), number, libc::O_CLOEXEC) };
        assert_eq!(rc, number, "dup3");
        // SAFETY: dup3 installed a fresh fd at `number`, owned from here on
        unsafe { OwnedFd::from_raw_fd(number) }
    }

    /// A GLib source on `fd` for input, attached to `ctx`.
    fn watch(
        ctx: &MainContext,
        fd: RawFd,
        func: impl FnMut(RawFd, IOCondition) -> ControlFlow + Send + 'static,
    ) -> Source {
        let source = glib_unix::fd_source_new(fd, IOCondition::IN, None, Priority::DEFAULT, func);
        source.attach(Some(ctx));
        source
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
        with_mirror(closed_and_reused);
    }

    fn closed_and_reused(looper: &EpollLooper, ctx: &MainContext) {
        let (r1, w1) = pipe();
        let number = r1.as_raw_fd();
        let first = watch(ctx, number, |_, _| ControlFlow::Continue);
        resync();
        first.destroy();
        drop(w1);
        let (r2, w2) = pipe();
        let r2 = reuse(r1, r2);
        let ran = Arc::new(AtomicBool::new(false));
        let seen = Arc::clone(&ran);
        let _second = watch(ctx, r2.as_raw_fd(), move |_, _| {
            seen.store(true, Ordering::Relaxed);
            ControlFlow::Break
        });
        resync();
        rustix::io::write(&w2, b"x").expect("write");
        for _ in 0..10 {
            looper.poll_once(50);
            if ran.load(Ordering::Relaxed) {
                break;
            }
        }
        assert!(
            ran.load(Ordering::Relaxed),
            "the source on the reused fd never ran"
        );
    }

    /// A GLib fd closed while a dup keeps its file open (WebKit's pid socket: `ProcessLauncherGLib.cpp` hands a dup to the IPC connection, then closes the original), its number reused by a new GLib fd.
    /// The old file's epoll entry outlives the close, so its readiness came back under the reused number: the new fd's source ran with G_IO_IN and nothing to read, and WebKit's `recvmsg` got EAGAIN -> `g_error` ->
    /// SIGTRAP (AB2's ad WebView). The new source must run only for its own data, and the stale entry must stop firing (level-triggered, it would wake the Looper forever).
    #[test]
    fn a_dup_kept_file_does_not_wake_its_reused_number() {
        with_mirror(dup_kept_file);
    }

    fn dup_kept_file(looper: &EpollLooper, ctx: &MainContext) {
        let (r1, w1) = pipe();
        let number = r1.as_raw_fd();
        let first = watch(ctx, number, |_, _| ControlFlow::Continue);
        resync();
        let kept = r1.try_clone().expect("dup");
        first.destroy();
        let (r2, w2) = pipe();
        let r2 = reuse(r1, r2);
        let (runs, empties) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let (seen, empty) = (Arc::clone(&runs), Arc::clone(&empties));
        let reader = r2.try_clone().expect("dup");
        let _second = watch(ctx, number, move |_, _| {
            seen.fetch_add(1, Ordering::Relaxed);
            if read(&reader, &mut [0u8; 8]).is_err() {
                empty.fetch_add(1, Ordering::Relaxed);
            }
            ControlFlow::Continue
        });
        let ran = || runs.load(Ordering::Relaxed);
        let spurious = || empties.load(Ordering::Relaxed);
        resync();
        // the old file readable, never drained: its entry (if any survived) fires on every poll
        rustix::io::write(&w1, b"x").expect("write old");
        for _ in 0..5 {
            looper.poll_once(20);
        }
        assert_eq!(
            spurious(),
            0,
            "the reused number's source ran with nothing to read"
        );
        assert_eq!(ran(), 0, "the reused number's source ran for the old file");
        // probe responds: the new fd's own data runs its source
        rustix::io::write(&w2, b"y").expect("write new");
        for _ in 0..5 {
            looper.poll_once(20);
            if ran() > 0 {
                break;
            }
        }
        assert_eq!(
            (ran(), spurious()),
            (1, 0),
            "the new fd's data must run its source once"
        );
        // the stale entry is gone: once GLib's own pending work (its wakeup eventfd) is drained, nothing keeps the Looper's set readable; a surviving stale entry would (the old pipe stays unread)
        for _ in 0..3 {
            looper.poll_once(20);
        }
        assert_eq!(ran(), 1, "the new fd's source ran again without data");
        let mut events = [libc::epoll_event { events: 0, u64: 0 }; 4];
        // SAFETY: an epoll fd the looper owns and a writable array of the stated length; timeout 0
        let n = unsafe { libc::epoll_wait(looper.epoll.as_raw_fd(), events.as_mut_ptr(), 4, 0) };
        let ready = MIRROR.with_borrow(|m| {
            let m = m.upgrade().expect("installed");
            let m = m.borrow();
            (ready_now(&m.epoll), ready_now(&m.set), m.registered.clone())
        });
        assert_eq!(
            n, 0,
            "the Looper's set stays readable: a stale entry still fires: {ready:?}"
        );
        let (stale, rebuilds) = MIRROR.with_borrow(|m| {
            let m = m.upgrade().expect("installed");
            let s = &m.borrow().stats;
            (s.stale, s.rebuilds)
        });
        // one stale event (the first poll after the old file turned readable), one rebuild that dropped it
        assert_eq!((stale, rebuilds), (1, 1), "stale, rebuilds");
        drop((kept, w1, w2));
    }
}
