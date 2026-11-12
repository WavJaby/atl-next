// SPDX-License-Identifier: MIT
//! The UI thread's GLib main context, mirrored into its Android Looper (owner decision 2026-11-05, plan Q6): the Looper is the outer loop, so Java's `nativePollOnce` is where GLib runs, with no AOSP patch.
//! GLib's fds and its timeout are registered with the Looper: each fd with `add_fd` (events as GLib polls them), the timeout as a timerfd. A readable one reaches [`Mirror::pass`]: prepare, query, check with the fd's readiness, dispatch (no GLib poll: the Looper polled), then the registrations are synced to GLib's new fd set.
//! Level-triggered: a fd still readable after its pass is reported again at once. [`Stats`] reports that (`passes` against what GLib did), the sync cost, and reports for fds removed earlier in the same Looper batch (`stale`).
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
    /// Looper callbacks (one per readable fd or timer expiry; GLib's pass each).
    passes: u64,
    /// Callbacks for an fd that is no longer registered: its response was queued in the same Looper batch before its removal.
    stale: u64,
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
    /// The fds registered with the Looper, with the GLib events each was registered for.
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

/// GLib's events -> the Looper's. PRI is read as input; ERR and HUP are reported on every poll, so they are not asked for.
fn looper_events(glib_events: u16) -> FdEvents {
    FdEvents {
        input: glib_events & (G_IN | G_PRI) != 0,
        output: glib_events & G_OUT != 0,
        ..FdEvents::default()
    }
}

/// The Looper's readiness -> GLib's `revents`.
fn glib_revents(events: FdEvents) -> u16 {
    let mut revents = 0;
    if events.input {
        revents |= G_IN;
    }
    if events.output {
        revents |= G_OUT;
    }
    if events.error {
        revents |= G_ERR;
    }
    if events.hangup {
        revents |= G_HUP;
    }
    revents
}

impl Mirror {
    /// One pass, from the Looper's callback: GLib's non-blocking iteration with the readiness the Looper reported, then the sync.
    fn pass(&mut self, fd: RawFd, events: FdEvents) {
        self.stats.passes += 1;
        if fd == self.timer.as_raw_fd() {
            self.stats.timer_fires += 1;
            // level-triggered: an unread expiry count would report again at once
            let mut count = [0u8; 8];
            let _ = read(&self.timer, &mut count);
        } else if !self.registered.contains_key(&fd) {
            self.stats.stale += 1;
        }
        let (mut priority, mut timeout) = (0, -1);
        let n = self.query(&mut priority, &mut timeout);
        let revents = glib_revents(events);
        for poll in &mut self.query[..n] {
            poll.revents = if poll.fd == fd { revents } else { 0 };
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

    /// Makes the Looper's registrations GLib's current set: removes what is gone or changed, adds what is new, and arms the timerfd for GLib's timeout (none: disarmed).
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
            .iter()
            .filter(|&(fd, events)| want.get(fd) != Some(events))
            .map(|(fd, _)| *fd)
            .collect();
        for fd in gone {
            self.registered.remove(&fd);
            // a failed removal of a fd the Looper does not hold is not fatal to the mirror
            if self.looper.remove_fd(fd).is_ok() {
                self.stats.removes += 1;
            }
        }
        for (&fd, &events) in &want {
            if self.registered.contains_key(&fd) {
                continue;
            }
            if let Err(e) = self.looper.add_fd(fd, looper_events(events)) {
                tracing::warn!("atl-platform-ui: mirror: {e}");
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
            "atl-platform-ui: mirror: passes={} stale={} timer_fires={} resyncs={} syncs={} sync_avg_ns={avg} sync_max_ns={} fds={} fds_max={} adds={} removes={}",
            s.passes,
            s.stale,
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
    let timer = timerfd_create(
        TimerfdClockId::Monotonic,
        TimerfdFlags::CLOEXEC | TimerfdFlags::NONBLOCK,
    )
    .map_err(|e| UiError::Looper(format!("timerfd_create: {e}")))?;
    let timer_fd = timer.as_raw_fd();
    let mirror = Rc::new(RefCell::new(Mirror {
        looper,
        ctx,
        timer,
        registered: BTreeMap::new(),
        query: Vec::new(),
        stats: Stats::default(),
        report_at: Instant::now(),
    }));
    let sink = Rc::clone(&mirror);
    mirror.borrow().looper.set_sink(Box::new(move |fd, events| {
        sink.borrow_mut().pass(fd, events)
    }));
    MIRROR.with_borrow_mut(|slot| *slot = Rc::downgrade(&mirror));
    let mut m = mirror.borrow_mut();
    m.looper.add_fd(
        timer_fd,
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
