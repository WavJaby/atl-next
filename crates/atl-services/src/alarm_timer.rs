// SPDX-License-Identifier: MIT
//! The alarm service's one timer source (alarm.rs): a timerfd on CLOCK_BOOTTIME for the elapsed-realtime alarms and one on CLOCK_REALTIME for the wall-clock ones.
//! Each is absolute and armed to the earliest trigger of its clock, or disarmed. The realtime one is CANCEL_ON_SET: a clock set cancels it, its read fails with ECANCELED and the wait returns, so the caller re-evaluates (alarm_table's due test reads both clocks live).
//! No suspend wake: a BOOTTIME timer does not wake a suspended host (BOOTTIME_ALARM would). A desktop program does not wake the machine.
//! A trigger at or before now fires at once. A timerfd whose value is zero is disarmed, so an armed value is at least MIN_ARM_MS.

use std::io;
use std::mem::size_of;
use std::os::fd::OwnedFd;

use rustix::event::{PollFd, PollFlags, poll};
use rustix::io::{Errno, read};
use rustix::time::{
    ClockId, Itimerspec, TimerfdClockId, TimerfdFlags, TimerfdTimerFlags, clock_gettime,
    timerfd_create, timerfd_settime,
};

use crate::alarm_table::Now;

const MS_PER_S: i64 = 1_000;
const NS_PER_MS: i64 = 1_000_000;
/// The least value a timer is armed to: zero disarms it, and a past trigger fires at once whatever its value.
const MIN_ARM_MS: i64 = 1;
/// A timerfd read returns the expiration count, a u64.
const EXPIRATIONS_BYTES: usize = size_of::<u64>();

/// Both clocks, read now, in ms.
pub fn now() -> Now {
    let realtime = clock_gettime(ClockId::Realtime);
    let boottime = clock_gettime(ClockId::Boottime);
    Now {
        realtime: realtime.tv_sec * MS_PER_S + realtime.tv_nsec / NS_PER_MS,
        boottime: boottime.tv_sec * MS_PER_S + boottime.tv_nsec / NS_PER_MS,
    }
}

/// The two timers. Armed from any thread (timerfd_settime works per fd); waited on by one thread.
pub struct Timers {
    boottime: OwnedFd,
    realtime: OwnedFd,
}

impl Timers {
    pub fn new() -> io::Result<Self> {
        let flags = TimerfdFlags::CLOEXEC | TimerfdFlags::NONBLOCK;
        Ok(Timers {
            boottime: timerfd_create(TimerfdClockId::Boottime, flags)?,
            realtime: timerfd_create(TimerfdClockId::Realtime, flags)?,
        })
    }

    /// Arms each timer to the absolute trigger in ms on its clock (the earliest one held there); None disarms it.
    pub fn arm(&self, boottime: Option<i64>, realtime: Option<i64>) -> io::Result<()> {
        settime(&self.boottime, TimerfdTimerFlags::ABSTIME, boottime)?;
        settime(
            &self.realtime,
            TimerfdTimerFlags::ABSTIME | TimerfdTimerFlags::CANCEL_ON_SET,
            realtime,
        )
    }

    /// Blocks until a timer is readable (expired, or its clock was set), then drains the readable ones. The caller re-evaluates in every case.
    pub fn wait(&self) -> io::Result<()> {
        let mut fds = [
            PollFd::new(&self.boottime, PollFlags::IN),
            PollFd::new(&self.realtime, PollFlags::IN),
        ];
        loop {
            match poll(&mut fds, None) {
                Ok(_) => break,
                Err(Errno::INTR) => {}
                Err(e) => return Err(e.into()),
            }
        }
        let ready = [
            (&self.boottime, fds[0].revents()),
            (&self.realtime, fds[1].revents()),
        ];
        for (fd, revents) in ready {
            if revents.contains(PollFlags::IN) {
                drain(fd)?;
            }
        }
        Ok(())
    }
}

/// Arms `fd` to the absolute trigger `at_ms` on its clock, or disarms it.
fn settime(fd: &OwnedFd, flags: TimerfdTimerFlags, at_ms: Option<i64>) -> io::Result<()> {
    let mut value = Itimerspec {
        it_interval: Default::default(),
        it_value: Default::default(),
    };
    if let Some(ms) = at_ms {
        let ms = ms.max(MIN_ARM_MS);
        value.it_value.tv_sec = ms / MS_PER_S;
        value.it_value.tv_nsec = (ms % MS_PER_S) * NS_PER_MS;
    }
    timerfd_settime(fd, flags, &value)?;
    Ok(())
}

/// Reads a readable timer to clear it. ECANCELED is a clock set (the timer is cancelled) and EAGAIN a count already taken: neither is an error here.
fn drain(fd: &OwnedFd) -> io::Result<()> {
    let mut buf = [0u8; EXPIRATIONS_BYTES];
    match read(fd, &mut buf[..]) {
        Ok(_) | Err(Errno::CANCELED) | Err(Errno::AGAIN) => Ok(()),
        Err(e) => Err(e.into()),
    }
}
