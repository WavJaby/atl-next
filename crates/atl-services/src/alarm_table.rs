// SPDX-License-Identifier: MIT
//! The alarms the `alarm` service holds (alarm.rs): what AlarmManagerService keeps per set alarm, for matching, removal and delivery.
//! Matching is AlarmManagerService's: an alarm with an operation matches that operation (PendingIntent identity), one without matches its listener (Alarm.java:171-175). A set replaces the alarm already held for its key (setImplLocked:2271).
//! take_due is the due test and the re-arm of a repeating alarm (AlarmManagerService.java:4228-4243, the recurrence after a delivery). A one-shot leaves the table when it is due.
//! Clocks: RTC_WAKEUP and RTC triggers are wall-clock ms (CLOCK_REALTIME); ELAPSED_REALTIME(_WAKEUP) are CLOCK_BOOTTIME ms (SystemClock.elapsedRealtime). Both are read live at each take_due, so a clock set moves the RTC alarms with it: a forward set makes them due at once, a backward one holds them.
//! Generic over the key types, so the tests run without the binder; alarm.rs keys it by the operation's and the listener's binders.

/// AlarmManager's clock types (AlarmManager.java:118-139). setImpl refuses any other type (AlarmManagerService.java:2188).
pub const RTC_WAKEUP: i32 = 0;
pub const RTC: i32 = 1;
pub const ELAPSED_REALTIME_WAKEUP: i32 = 2;
pub const ELAPSED_REALTIME: i32 = 3;

/// The two time bases an alarm trigger is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Clock {
    /// CLOCK_REALTIME ms: RTC_WAKEUP, RTC
    Realtime,
    /// CLOCK_BOOTTIME ms: ELAPSED_REALTIME_WAKEUP, ELAPSED_REALTIME
    Boottime,
}

/// The clock an AlarmManager type reads; None for a type AlarmManager refuses.
pub fn clock_of(kind: i32) -> Option<Clock> {
    match kind {
        RTC_WAKEUP | RTC => Some(Clock::Realtime),
        ELAPSED_REALTIME_WAKEUP | ELAPSED_REALTIME => Some(Clock::Boottime),
        _ => None,
    }
}

/// Both clocks read at one instant, in ms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Now {
    pub realtime: i64,
    pub boottime: i64,
}

impl Now {
    fn at(self, clock: Clock) -> i64 {
        match clock {
            Clock::Realtime => self.realtime,
            Clock::Boottime => self.boottime,
        }
    }
}

/// One set alarm: the arguments of AlarmManagerService's set (:2684-2688), the creator uid and package, and the keys.
#[derive(Debug, Clone, PartialEq)]
pub struct Alarm<O, L> {
    /// the creator uid (`creatorUid`) and the package it was set for (`sourcePackage`)
    pub uid: u32,
    pub package: String,
    /// AlarmManager's type, the trigger in its clock's ms (as set, clamped to >= 0), the window (not used: delivery is at the trigger), the interval (0 = one-shot) and the flags
    pub kind: i32,
    pub trigger: i64,
    pub window: i64,
    pub interval: i64,
    pub flags: i32,
    /// the BroadcastOptions its PendingIntent send carries (setImpl's idleOptions); None = none. Never read for a listener alarm.
    pub idle: Option<IdleOptions>,
    pub operation: Option<O>,
    pub listener: Option<L>,
}

/// AlarmManager.FLAG_ALLOW_WHILE_IDLE (AlarmManager.java:216) and FLAG_PRIORITIZE (:254).
pub const FLAG_ALLOW_WHILE_IDLE: i32 = 1 << 2;
pub const FLAG_PRIORITIZE: i32 = 1 << 6;
/// AlarmManagerService.java:729 DEFAULT_ALLOW_WHILE_IDLE_ALLOWLIST_DURATION (`10 * 1000`): the temp allowlist an alarm's delivery grants.
pub const ALLOW_WHILE_IDLE_DURATION_MS: i64 = 10 * 1000;
/// PowerExemptionManager.REASON_ALARM_MANAGER_WHILE_IDLE (:293).
pub const REASON_ALARM_MANAGER_WHILE_IDLE: i32 = 302;

/// The BroadcastOptions temp allowlist an alarm's PendingIntent send carries: duration, type (uid_state TEMP_TYPE_*), reason (AlarmManagerService mOptsWithFgs :346, mOptsWithoutFgs :348).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IdleOptions {
    pub duration_ms: i64,
    pub kind: i32,
    pub reason: i32,
}

/// Which idleOptions AlarmManagerService.setImpl gives an alarm (:2745-2820), reduced. `exact` = windowLength 0 (:2750). `allow_while_idle` and `prioritized` are the flags after
/// the core-caller conversion (:2735-2748: a core caller's both are cleared). `exact_change` = REQUIRE_EXACT_ALARM_PERMISSION applies, i.e. targetSdk >= S (AlarmManager.java :265-266).
/// With the change on, an exact alarm gets the FGS-allowed grant (mOptsWithFgs) and an inexact allow-while-idle alarm the FGS-not-allowed one, whose REASON_DENIED grants nothing.
/// With it off, only an allow-while-idle alarm gets the FGS-allowed grant. Not modelled: the exact-alarm grant and the exempt-app branch (:2818), alarm clocks (refused by atl-parcelables).
pub fn idle_options(
    exact: bool,
    allow_while_idle: bool,
    prioritized: bool,
    exact_change: bool,
) -> Option<IdleOptions> {
    if prioritized || !(exact || allow_while_idle) {
        return None;
    }
    let fgs = IdleOptions {
        duration_ms: ALLOW_WHILE_IDLE_DURATION_MS,
        kind: crate::uid_state::TEMP_TYPE_FGS_ALLOWED,
        reason: REASON_ALARM_MANAGER_WHILE_IDLE,
    };
    if !exact_change {
        return allow_while_idle.then_some(fgs);
    }
    if exact {
        Some(fgs)
    } else {
        Some(IdleOptions {
            kind: crate::uid_state::TEMP_TYPE_FGS_NOT_ALLOWED,
            reason: crate::uid_state::REASON_DENIED,
            ..fgs
        })
    }
}

/// The alarms in set order. A key holds at most one alarm: a set replaces the one it matches.
#[derive(Debug)]
pub struct AlarmTable<O, L> {
    alarms: Vec<Alarm<O, L>>,
}

impl<O, L> Default for AlarmTable<O, L> {
    fn default() -> Self {
        AlarmTable { alarms: Vec::new() }
    }
}

impl<O: PartialEq, L: PartialEq> AlarmTable<O, L> {
    /// Alarms held.
    pub fn len(&self) -> usize {
        self.alarms.len()
    }

    /// No alarm held.
    pub fn is_empty(&self) -> bool {
        self.alarms.is_empty()
    }

    /// setImplLocked (:2257-2271): the alarm held for this operation or listener goes first, then this one is added.
    pub fn set(&mut self, alarm: Alarm<O, L>) {
        self.remove(alarm.operation.as_ref(), alarm.listener.as_ref());
        self.alarms.push(alarm);
    }

    /// removeLocked (:3989-3997): drops each alarm the operation or the listener matches; with neither given, none. Returns how many were dropped.
    pub fn remove(&mut self, operation: Option<&O>, listener: Option<&L>) -> usize {
        let before = self.alarms.len();
        self.alarms.retain(|a| !a.matches(operation, listener));
        before - self.alarms.len()
    }

    /// removeAll (:2911-2927): drops the alarms `package` set under `uid`. Returns how many were dropped.
    pub fn remove_all(&mut self, uid: u32, package: &str) -> usize {
        let before = self.alarms.len();
        self.alarms.retain(|a| a.uid != uid || a.package != package);
        before - self.alarms.len()
    }

    /// The earliest trigger held on `clock`, the next time the timer must wake for it (alarm_timer.rs); None when no alarm is held on it.
    pub fn next_due(&self, clock: Clock) -> Option<i64> {
        self.alarms
            .iter()
            .filter(|a| clock_of(a.kind) == Some(clock))
            .map(|a| a.trigger)
            .min()
    }
}

impl<O: Clone + PartialEq, L: Clone + PartialEq> AlarmTable<O, L> {
    /// The alarms due at `now`, in the order they fell due (boot-equivalent ms, then set order). A one-shot leaves the table. A repeating one stays, its trigger moved to its next recurrence after `now`; the delivered copy keeps the trigger it was due at.
    pub fn take_due(&mut self, now: Now) -> Vec<Alarm<O, L>> {
        let mut due = Vec::new();
        let mut kept = Vec::with_capacity(self.alarms.len());
        for mut alarm in std::mem::take(&mut self.alarms) {
            let Some(clock) = clock_of(alarm.kind) else {
                kept.push(alarm);
                continue;
            };
            if alarm.trigger > now.at(clock) {
                kept.push(alarm);
            } else if alarm.interval > 0 {
                due.push(alarm.clone());
                alarm.trigger = next_recurrence(alarm.trigger, alarm.interval, now.at(clock));
                kept.push(alarm);
            } else {
                due.push(alarm);
            }
        }
        self.alarms = kept;
        due.sort_by_key(|a| boot_equivalent(now, a));
        due
    }
}

/// The first recurrence after `now`, from the trigger `requested` the alarm was due at: count = 1 + (now - requested) / interval, next = requested + count * interval (AlarmManagerService.java:4233-4236). AOSP's count accumulates over deliveries; from the latest requested time it is the same.
fn next_recurrence(requested: i64, interval: i64, now: i64) -> i64 {
    let count = (now - requested) / interval + 1;
    requested + count * interval
}

/// A trigger on the boot clock: a wall-clock trigger is shifted by the realtime-boottime offset now (AlarmManagerService.convertToElapsed). The order key of take_due, not a deadline.
fn boot_equivalent<O, L>(now: Now, alarm: &Alarm<O, L>) -> i64 {
    match clock_of(alarm.kind) {
        Some(Clock::Realtime) => alarm.trigger - (now.realtime - now.boottime),
        _ => alarm.trigger,
    }
}

impl<O: PartialEq, L: PartialEq> Alarm<O, L> {
    /// Alarm.matches (Alarm.java:171-175): an alarm with an operation matches only that operation; one without matches its listener. An alarm with neither matches nothing (AOSP would throw).
    fn matches(&self, operation: Option<&O>, listener: Option<&L>) -> bool {
        match (&self.operation, &self.listener) {
            (Some(op), _) => operation == Some(op),
            (None, Some(l)) => listener == Some(l),
            (None, None) => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The keys are plain numbers here: the table never looks inside them.
    fn alarm(uid: u32, package: &str, op: Option<u32>, listener: Option<u32>) -> Alarm<u32, u32> {
        Alarm {
            uid,
            package: package.to_owned(),
            kind: 0,
            trigger: 0,
            window: 0,
            interval: 0,
            flags: 0,
            idle: None,
            operation: op,
            listener,
        }
    }

    #[test]
    fn idle_options_follow_setimpl_by_target_sdk() {
        // exact change on (targetSdk >= S): exact -> FGS-allowed grant; inexact allow-while-idle -> FGS-not-allowed, REASON_DENIED (grants nothing)
        let fgs = IdleOptions {
            duration_ms: 10_000,
            kind: 0,
            reason: 302,
        };
        assert_eq!(idle_options(true, false, false, true), Some(fgs));
        assert_eq!(idle_options(true, true, false, true), Some(fgs));
        assert_eq!(
            idle_options(false, true, false, true),
            Some(IdleOptions {
                duration_ms: 10_000,
                kind: 1,
                reason: -1,
            })
        );
        // exact change off (targetSdk < S): only allow-while-idle gets the FGS-allowed grant
        assert_eq!(idle_options(true, false, false, false), None);
        assert_eq!(idle_options(false, true, false, false), Some(fgs));
        // neither exact nor allow-while-idle, and PRIORITIZE: no options
        assert_eq!(idle_options(false, false, false, true), None);
        assert_eq!(idle_options(true, false, true, true), None);
    }

    /// An operation alarm of `kind` due at `trigger` ms on its clock, repeating every `interval` ms (0 = one-shot).
    fn timed(op: u32, kind: i32, trigger: i64, interval: i64) -> Alarm<u32, u32> {
        Alarm {
            kind,
            trigger,
            interval,
            ..alarm(10123, "com.example", Some(op), None)
        }
    }

    /// A boot clock reading with the realtime clock at `realtime`.
    fn now(realtime: i64, boottime: i64) -> Now {
        Now { realtime, boottime }
    }

    /// Operations of the delivered alarms, in delivery order.
    fn ops(due: &[Alarm<u32, u32>]) -> Vec<Option<u32>> {
        due.iter().map(|a| a.operation).collect()
    }

    /// set, then remove by the same operation: one alarm dropped, the table empty.
    #[test]
    fn set_then_remove_empties_the_table() {
        let mut t = AlarmTable::default();
        t.set(alarm(10123, "com.example", Some(1), None));
        assert_eq!(t.len(), 1);
        assert_eq!(t.remove(Some(&1), None), 1);
        assert!(t.is_empty());
    }

    /// A second set for the same operation replaces the first (setImplLocked removes the match before it adds).
    #[test]
    fn same_operation_replaces_the_alarm() {
        let mut t = AlarmTable::default();
        t.set(alarm(10123, "com.example", Some(1), None));
        t.set(alarm(10123, "com.example", Some(1), None));
        assert_eq!(t.len(), 1);
    }

    /// A listener alarm matches its own listener only: another operation or another listener drops nothing.
    #[test]
    fn listener_alarm_matches_its_listener_only() {
        let mut t = AlarmTable::default();
        t.set(alarm(10123, "com.example", None, Some(7)));
        assert_eq!(t.remove(Some(&1), None), 0);
        assert_eq!(t.remove(None, Some(&8)), 0);
        assert_eq!(t.remove(None, Some(&7)), 1);
        assert!(t.is_empty());
    }

    /// Neither key given: nothing matches (removeLocked returns early).
    #[test]
    fn remove_without_a_key_drops_nothing() {
        let mut t = AlarmTable::default();
        t.set(alarm(10123, "com.example", Some(1), None));
        assert_eq!(t.remove(None, None), 0);
        assert_eq!(t.len(), 1);
    }

    /// removeAll drops the package's alarms set under the caller's uid only: two of three here.
    #[test]
    fn remove_all_is_per_package_and_uid() {
        let mut t = AlarmTable::default();
        t.set(alarm(10123, "com.example", Some(1), None));
        t.set(alarm(10123, "com.example", Some(2), None));
        t.set(alarm(10124, "com.example", Some(3), None));
        t.set(alarm(10123, "com.other", Some(4), None));
        assert_eq!(t.remove_all(10123, "com.example"), 2);
        assert_eq!(t.len(), 2);
    }

    /// Due alarms come out in the order their triggers fall due, whatever the set order: 100, 200, 300 ms of boot time.
    #[test]
    fn due_alarms_come_out_in_trigger_order() {
        let mut t = AlarmTable::default();
        t.set(timed(1, ELAPSED_REALTIME, 300, 0));
        t.set(timed(2, ELAPSED_REALTIME, 100, 0));
        t.set(timed(3, ELAPSED_REALTIME, 200, 0));
        let due = t.take_due(now(0, 1000));
        assert_eq!(ops(&due), vec![Some(2), Some(3), Some(1)]);
        assert!(t.is_empty(), "one-shots leave the table when due");
    }

    /// An alarm is due at its trigger, not before (AOSP delivers when whenElapsed <= now).
    #[test]
    fn alarm_is_due_at_its_trigger_and_not_before() {
        let mut t = AlarmTable::default();
        t.set(timed(1, ELAPSED_REALTIME, 2000, 0));
        assert!(t.take_due(now(0, 1999)).is_empty());
        assert_eq!(ops(&t.take_due(now(0, 2000))), vec![Some(1)]);
    }

    /// A cancelled alarm is never delivered: removed before its trigger, the clock past it changes nothing.
    #[test]
    fn cancelled_alarm_is_never_delivered() {
        let mut t = AlarmTable::default();
        t.set(timed(1, RTC, 2000, 0));
        assert_eq!(t.remove(Some(&1), None), 1);
        assert!(t.take_due(now(5000, 5000)).is_empty());
    }

    /// A repeating alarm stays after its delivery, re-armed one interval on: trigger 1000 ms, interval 60000 ms, delivered on time -> next 61000 ms.
    #[test]
    fn repeating_alarm_rearms_one_interval_after_an_on_time_delivery() {
        let mut t = AlarmTable::default();
        t.set(timed(1, RTC_WAKEUP, 1_000, 60_000));
        assert_eq!(ops(&t.take_due(now(1_000, 0))), vec![Some(1)]);
        assert_eq!(t.len(), 1);
        assert_eq!(t.next_due(Clock::Realtime), Some(61_000));
    }

    /// A late repeating delivery fires once and re-arms to the first recurrence after now: trigger 1000, interval 60000, now 151000 (2.5 intervals on) -> next 181000 (3 intervals on).
    #[test]
    fn late_repeating_delivery_fires_once_then_rearms_after_now() {
        let mut t = AlarmTable::default();
        t.set(timed(1, RTC, 1_000, 60_000));
        assert_eq!(ops(&t.take_due(now(151_000, 0))).len(), 1);
        assert_eq!(t.next_due(Clock::Realtime), Some(181_000));
        assert!(t.take_due(now(151_000, 0)).is_empty());
    }

    /// A wall-clock set forward makes an RTC alarm due at once; a boot-clock alarm is not moved by it.
    #[test]
    fn realtime_clock_set_forward_makes_rtc_alarm_due() {
        let mut t = AlarmTable::default();
        t.set(timed(1, RTC, 5000, 0));
        t.set(timed(2, ELAPSED_REALTIME, 5000, 0));
        assert!(t.take_due(now(1000, 1000)).is_empty());
        assert_eq!(ops(&t.take_due(now(9000, 1000))), vec![Some(1)]);
        assert_eq!(t.next_due(Clock::Boottime), Some(5000));
    }

    /// A wall-clock set back does not re-deliver a repeating alarm: it re-armed to 65000 ms after its delivery at 6000 ms (trigger 5000, interval 60000), and 4000 ms is not due.
    #[test]
    fn realtime_clock_set_back_does_not_redeliver() {
        let mut t = AlarmTable::default();
        t.set(timed(1, RTC, 5000, 60_000));
        assert_eq!(ops(&t.take_due(now(6000, 0))), vec![Some(1)]);
        assert!(t.take_due(now(4000, 0)).is_empty());
        assert_eq!(t.next_due(Clock::Realtime), Some(65_000));
    }

    /// next_due reads each clock separately; an alarm of the other clock is not its trigger.
    #[test]
    fn next_due_is_per_clock() {
        let mut t = AlarmTable::default();
        t.set(timed(1, RTC_WAKEUP, 700, 0));
        t.set(timed(2, ELAPSED_REALTIME_WAKEUP, 900, 0));
        assert_eq!(t.next_due(Clock::Realtime), Some(700));
        assert_eq!(t.next_due(Clock::Boottime), Some(900));
    }
}
