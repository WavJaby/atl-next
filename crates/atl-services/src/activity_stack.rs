// SPDX-License-Identifier: MIT
//! The activity lifecycle of the services (ATMS-lite), as a pure machine: per Task, which activity the app must pause, resume, start, stop or destroy next. Plain Rust, no binder, so its tests build without the converter feature.
//! Every decision comes back as [`Command`]s in decision order. The caller sends them under its lock: each is a oneway `scheduleTransaction`, so the app runs them in the order decided.
//! Nothing waits on an ack. `resumed` / `paused` / `stopped` move one record to its next phase and advance its task again, so the order of acks between records does not change what is sent.
//! Generic over the token `K` (the services use SpIBinder), the owner `O` (the app's thread and identity, cloned into each command) and the launch payload `L` (taken by the one Launch step).
//! A Task is shown or hidden ([`Machine::set_visible`]): hidden, every record of it stays stopped; shown again, its top restarts. A Task starts shown.
//! A result goes to its caller at once while the caller is resumed, else waits in the caller's results for its next resume ([`Machine::finish`], [`Machine::deliver`]).

use std::collections::{BTreeMap, BTreeSet};

use crate::tasks::TaskId;

/// Where an activity is in its lifecycle, as the app has acknowledged it. `Resuming`, `Pausing` and `Stopping` are sent and not yet acknowledged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Pushed, not yet advanced: only visible inside `push`, which advances at once.
    Created,
    Resuming,
    Resumed,
    Pausing,
    Paused,
    Stopping,
    Stopped,
}

/// One lifecycle item the app must run, in order. Each maps to one servertransaction item (the caller builds it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step<L, I, R> {
    /// LaunchActivityItem: the record's launch payload, taken once.
    Launch(L),
    /// NewIntentItem: `intents` delivered to onNewIntent. `resume`: the item itself ends resumed (ActivityRecord.java:4997 `mState == RESUMED`); false when a Resume step follows or the record stays paused.
    NewIntent { intents: Vec<I>, resume: bool },
    /// ActivityResultItem: the results of activities that finished for this one, in order. Sent before the NewIntent and Resume of the same resume (TaskFragment.java:1665-1668).
    Result(Vec<R>),
    /// StartActivityItem: a stopped activity comes back to the top.
    Start,
    /// ResumeActivityItem.
    Resume,
    /// PauseActivityItem. `finished`: the record is finishing. `user_leaving`: covered by a start, not finished.
    Pause { finished: bool, user_leaving: bool },
    /// StopActivityItem.
    Stop,
    /// DestroyActivityItem; only for a finishing record, which then leaves the table.
    Destroy { finished: bool },
}

/// The steps for one record, sent to its owner as one transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command<K, O, L, I, R> {
    pub token: K,
    pub owner: O,
    pub steps: Vec<Step<L, I, R>>,
}

/// A decision's commands, in decision order; the records of one Task, bottom -> top.
type Commands<K, O, L, I, R> = Vec<Command<K, O, L, I, R>>;
type Records<K, O, L, I, R> = Vec<Record<K, O, L, I, R>>;

/// The result a record owes its caller: ActivityRecord.resultTo, resultWho and requestCode. Held from the start until the record finishes (`finish`), or answered as cancelled when the start does not happen (`deliver`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResultTo<K> {
    pub token: K,
    pub who: Option<String>,
    pub request_code: i32,
}

struct Record<K, O, L, I, R> {
    token: K,
    owner: O,
    phase: Phase,
    /// `finish` was called: the record leaves the stack once stopped.
    finishing: bool,
    /// Taken by the Launch step.
    launch: Option<L>,
    /// The result owed to the caller, taken by `finish`.
    owed: Option<ResultTo<K>>,
    /// ActivityRecord.newIntents: intents not sent at once (record not resumed or paused), delivered before its next Resume (TaskFragment.java:1691-1693).
    pending: Vec<I>,
    /// ActivityRecord.results: results of records that finished while this one was not resumed, delivered before its next Resume (TaskFragment.java:1665-1668).
    results: Vec<R>,
}

impl<K: Clone, O: Clone, L, I, R> Record<K, O, L, I, R> {
    fn command(&self, steps: Vec<Step<L, I, R>>) -> Command<K, O, L, I, R> {
        Command {
            token: self.token.clone(),
            owner: self.owner.clone(),
            steps,
        }
    }
}

/// A record found by its token, with its Task.
type Found<'a, R> = Option<(TaskId, &'a R)>;
type FoundMut<'a, R> = Option<(TaskId, &'a mut R)>;

/// The stack of every Task: records bottom -> top, Tasks in id order. A Task with no records is dropped.
pub struct Machine<K, O, L, I, R> {
    tasks: BTreeMap<TaskId, Records<K, O, L, I, R>>,
    /// Tasks whose window is hidden (minimized or occluded: one path, `suspended`). Kept while the Task has no records: the window outlives them.
    hidden: BTreeSet<TaskId>,
}

impl<K, O, L, I, R> Default for Machine<K, O, L, I, R> {
    fn default() -> Self {
        Machine {
            tasks: BTreeMap::new(),
            hidden: BTreeSet::new(),
        }
    }
}

impl<K: Clone + PartialEq, O: Clone, L, I, R> Machine<K, O, L, I, R> {
    pub fn new() -> Self {
        Self::default()
    }

    /// A launch on top of `task`: the record is added and its task advanced at once.
    pub fn push(
        &mut self,
        task: TaskId,
        token: K,
        owner: O,
        launch: L,
    ) -> Vec<Command<K, O, L, I, R>> {
        self.push_owed(task, token, owner, launch, None)
    }

    /// A launch whose start owes `owed` to the record below it (ActivityStarter's resultTo): the result goes out when the new record finishes.
    pub fn push_owed(
        &mut self,
        task: TaskId,
        token: K,
        owner: O,
        launch: L,
        owed: Option<ResultTo<K>>,
    ) -> Vec<Command<K, O, L, I, R>> {
        self.tasks.entry(task).or_default().push(Record {
            token,
            owner,
            phase: Phase::Created,
            finishing: false,
            launch: Some(launch),
            owed,
            pending: Vec::new(),
            results: Vec::new(),
        });
        self.advance(task)
    }

    /// Sends `item` to the record `to` (ActivityRecord.sendResult): at once when it is resumed (Resuming counts, as in new_intent), else kept in its results for its next resume
    /// (addResultLocked, ActivityRecord.java:4920). A record that is gone gets nothing.
    pub fn deliver(&mut self, to: &ResultTo<K>, item: R) -> Vec<Command<K, O, L, I, R>> {
        match self.find_mut(&to.token) {
            Some((_, r)) if matches!(r.phase, Phase::Resumed | Phase::Resuming) => {
                vec![r.command(vec![Step::Result(vec![item])])]
            }
            Some((_, r)) => {
                r.results.push(item);
                Vec::new()
            }
            None => Vec::new(),
        }
    }

    /// The top record of `task` that is not finishing (ActivityStarter.java:2442 topRunningNonDelayedActivityLocked): what a singleTop start reuses.
    pub fn top(&self, task: TaskId) -> Option<(&K, &O)> {
        self.tasks
            .get(&task)?
            .iter()
            .rev()
            .find(|r| !r.finishing)
            .map(|r| (&r.token, &r.owner))
    }

    /// The newest non-finishing record of `task` that `matches` (Task.findActivityInHistory, Task.java clearTopActivities): what a singleTask start reuses.
    pub fn find_in_task(&self, task: TaskId, matches: impl Fn(&O) -> bool) -> Option<&K> {
        self.tasks
            .get(&task)?
            .iter()
            .rev()
            .find(|r| !r.finishing && matches(&r.owner))
            .map(|r| &r.token)
    }

    /// A start reuses `token`'s record (ActivityStarter.deliverNewIntent -> ActivityRecord.deliverNewIntentLocked). `clear_top`: every record above it finishes first
    /// (Task.clearTopActivities, finishActivityAbove), as singleTask implies (ActivityStarter.java:2510-2533). Then (ActivityRecord.java:4990-5011) a Resumed / Resuming record gets the
    /// intent at once and stays resumed (AOSP sets RESUMED when it sends the resume, so Resuming counts), a Paused one at once without resume; any other keeps it pending for its next Resume.
    /// None when the token names no record.
    pub fn new_intent(
        &mut self,
        token: &K,
        intent: I,
        clear_top: bool,
    ) -> Option<Commands<K, O, L, I, R>> {
        let task = self.task_of(token)?;
        let records = self.tasks.get_mut(&task)?;
        let at = records.iter().position(|r| &r.token == token)?;
        if clear_top {
            for r in &mut records[at + 1..] {
                r.finishing = true;
            }
        }
        let r = &mut records[at];
        let mut out = Vec::new();
        match r.phase {
            Phase::Resumed | Phase::Resuming => out.push(r.command(vec![Step::NewIntent {
                intents: vec![intent],
                resume: true,
            }])),
            Phase::Paused => out.push(r.command(vec![Step::NewIntent {
                intents: vec![intent],
                resume: false,
            }])),
            _ => r.pending.push(intent),
        }
        out.extend(self.advance(task));
        Some(out)
    }

    /// The app acknowledged a resume. In `Pausing` the phase stays: the resume ran before the pause, so its ack comes first and the pause ack is still to come.
    pub fn resumed(&mut self, token: &K) -> Vec<Command<K, O, L, I, R>> {
        let Some((task, record)) = self.find_mut(token) else {
            return Vec::new();
        };
        if record.phase == Phase::Resuming {
            record.phase = Phase::Resumed;
        }
        self.advance(task)
    }

    /// The app acknowledged a pause (`activityPaused`).
    pub fn paused(&mut self, token: &K) -> Vec<Command<K, O, L, I, R>> {
        let Some((task, record)) = self.find_mut(token) else {
            return Vec::new();
        };
        if record.phase == Phase::Pausing {
            record.phase = Phase::Paused;
        }
        self.advance(task)
    }

    /// The app acknowledged a stop (`activityStopped`).
    pub fn stopped(&mut self, token: &K) -> Vec<Command<K, O, L, I, R>> {
        let Some((task, record)) = self.find_mut(token) else {
            return Vec::new();
        };
        if record.phase == Phase::Stopping {
            record.phase = Phase::Stopped;
        }
        self.advance(task)
    }

    /// `finishActivity`: the record is finishing and its result goes out first (ActivityRecord.finishActivityResults, before the record pauses). `result` builds it from the
    /// owed request. None when the token names no record.
    pub fn finish(
        &mut self,
        token: &K,
        result: impl FnOnce(&ResultTo<K>) -> R,
    ) -> Option<Commands<K, O, L, I, R>> {
        let (task, record) = self.find_mut(token)?;
        record.finishing = true;
        record.results.clear();
        let owed = record.owed.take();
        let mut out = match owed {
            Some(owed) => {
                let item = result(&owed);
                self.deliver(&owed, item)
            }
            None => Vec::new(),
        };
        out.extend(self.advance(task));
        Some(out)
    }

    pub fn phase(&self, token: &K) -> Option<Phase> {
        self.find(token).map(|(_, r)| r.phase)
    }

    pub fn owner(&self, token: &K) -> Option<&O> {
        self.find(token).map(|(_, r)| &r.owner)
    }

    pub fn task_of(&self, token: &K) -> Option<TaskId> {
        self.find(token).map(|(task, _)| task)
    }

    pub fn owner_mut(&mut self, token: &K) -> Option<&mut O> {
        self.find_mut(token).map(|(_, r)| &mut r.owner)
    }

    /// The records of `task`, bottom -> top: their tokens and owners.
    pub fn records(&self, task: TaskId) -> impl Iterator<Item = (&K, &O)> {
        self.tasks
            .get(&task)
            .into_iter()
            .flatten()
            .map(|r| (&r.token, &r.owner))
    }

    /// The owner process died (ActivityManagerService.handleAppDiedLocked): its records leave the stack at once, nothing is sent; a Task left empty is dropped.
    pub fn drop_owned(&mut self, dead: impl Fn(&O) -> bool) {
        for records in self.tasks.values_mut() {
            records.retain(|r| !dead(&r.owner));
        }
        self.tasks.retain(|_, records| !records.is_empty());
    }

    pub fn is_finishing(&self, token: &K) -> Option<bool> {
        self.find(token).map(|(_, r)| r.finishing)
    }

    /// The steps a relaunch of `token` ends with (the relaunch item runs the lifecycle again). A resumed record resumes: Resume, phase Resuming, its ack awaited as any resume.
    /// Any other record keeps its phase, so no Resume: a paused or stopped activity is not resumed by a relaunch.
    pub fn relaunch(&mut self, token: &K) -> Vec<Step<L, I, R>> {
        let Some((_, record)) = self.find_mut(token) else {
            return Vec::new();
        };
        match record.phase {
            Phase::Resumed | Phase::Resuming => {
                record.phase = Phase::Resuming;
                vec![Step::Resume]
            }
            _ => Vec::new(),
        }
    }

    /// At rest: no record is Created, Resuming, Pausing or Stopping, so nothing sent is still unacknowledged. The services log each change of this (activity_task.rs); run-apk waits for it.
    pub fn settled(&self) -> bool {
        self.tasks.values().flatten().all(|r| {
            !matches!(
                r.phase,
                Phase::Created | Phase::Resuming | Phase::Pausing | Phase::Stopping
            )
        })
    }

    /// The window of `task` is shown or hidden. Hidden: its records pause and stop, the top included, and none is resumed until shown (each step waits for its ack).
    /// Shown: the top restarts from Stopped or Paused, as any record does. The commands come back in decision order; a Task with no records only changes its flag.
    pub fn set_visible(&mut self, task: TaskId, visible: bool) -> Vec<Command<K, O, L, I, R>> {
        if visible {
            self.hidden.remove(&task);
        } else {
            self.hidden.insert(task);
        }
        self.advance(task)
    }

    pub fn is_visible(&self, task: TaskId) -> bool {
        !self.hidden.contains(&task)
    }

    fn find(&self, token: &K) -> Found<'_, Record<K, O, L, I, R>> {
        self.tasks.iter().find_map(|(task, records)| {
            records
                .iter()
                .find(|r| &r.token == token)
                .map(|r| (*task, r))
        })
    }

    fn find_mut(&mut self, token: &K) -> FoundMut<'_, Record<K, O, L, I, R>> {
        self.tasks.iter_mut().find_map(|(task, records)| {
            records
                .iter_mut()
                .find(|r| &r.token == token)
                .map(|r| (*task, r))
        })
    }

    /// The rules, applied to one task in order. D = the top record that is not finishing: the one to resume, unless the Task is hidden.
    fn advance(&mut self, task: TaskId) -> Vec<Command<K, O, L, I, R>> {
        let mut out = Vec::new();
        let hidden = !self.is_visible(task);
        let Some(records) = self.tasks.get_mut(&task) else {
            return out;
        };
        let top = records.iter().rposition(|r| !r.finishing);
        // (1) every record other than D that is Resumed or Resuming is paused first; a hidden Task pauses D too
        for (i, r) in records.iter_mut().enumerate() {
            if (Some(i) != top || hidden) && matches!(r.phase, Phase::Resumed | Phase::Resuming) {
                r.phase = Phase::Pausing;
                let steps = vec![Step::Pause {
                    finished: r.finishing,
                    user_leaving: !r.finishing,
                }];
                out.push(r.command(steps));
            }
        }
        // (2) a hidden Task's created D is launched stopped: Launch, then Stop, so it never resumes unseen (its stop ack comes back as any other).
        if let Some(i) = top.filter(|&i| hidden && records[i].phase == Phase::Created) {
            let r = &mut records[i];
            let launch = r
                .launch
                .take()
                .expect("a Created record holds its launch payload");
            r.phase = Phase::Stopping;
            out.push(r.command(vec![Step::Launch(launch), Step::Stop]));
        }
        // (2') D comes back when its Task is shown: launched, restarted from Stopped, or resumed from Paused. Pausing/Resuming/Stopping wait for their ack.
        if let Some(i) = top.filter(|_| !hidden) {
            let r = &mut records[i];
            let steps = match r.phase {
                Phase::Created => Some(vec![
                    Step::Launch(
                        r.launch
                            .take()
                            .expect("a Created record holds its launch payload"),
                    ),
                    Step::Resume,
                ]),
                Phase::Stopped => Some(vec![Step::Start, Step::Resume]),
                Phase::Paused => Some(vec![Step::Resume]),
                _ => None,
            };
            if let Some(mut steps) = steps {
                // results, then intents, go before the Resume that ends the steps (TaskFragment.java:1665-1693: results first, then new intents, ahead of its ResumeActivityItem)
                let at = steps.len() - 1;
                if !r.pending.is_empty() {
                    let intents = std::mem::take(&mut r.pending);
                    steps.insert(
                        at,
                        Step::NewIntent {
                            intents,
                            resume: false,
                        },
                    );
                }
                if !r.results.is_empty() {
                    let results = std::mem::take(&mut r.results);
                    steps.insert(at, Step::Result(results));
                }
                r.phase = Phase::Resuming;
                out.push(r.command(steps));
            }
        }
        // (3) paused records other than D are stopped once D is Resumed, or once no D is left; a hidden Task stops every paused record, D included
        let ready = hidden
            || match top {
                None => true,
                Some(i) => records[i].phase == Phase::Resumed,
            };
        if ready {
            for (i, r) in records.iter_mut().enumerate() {
                if (Some(i) != top || hidden) && r.phase == Phase::Paused {
                    r.phase = Phase::Stopping;
                    out.push(r.command(vec![Step::Stop]));
                }
            }
        }
        // (4) finishing and stopped: destroyed, and gone from the table
        let mut i = 0;
        while i < records.len() {
            if records[i].finishing && records[i].phase == Phase::Stopped {
                let r = records.remove(i);
                out.push(r.command(vec![Step::Destroy { finished: true }]));
            } else {
                i += 1;
            }
        }
        if records.is_empty() {
            self.tasks.remove(&task);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type Machine = super::Machine<u32, &'static str, &'static str, &'static str, &'static str>;
    type Command = super::Command<u32, &'static str, &'static str, &'static str, &'static str>;
    type Step = super::Step<&'static str, &'static str, &'static str>;

    const SPLASH: u32 = 1;
    const MAIN: u32 = 2;
    const OTHER: u32 = 3;
    const UNKNOWN: u32 = 99;
    const APP: &str = "app";

    fn task(raw: i32) -> TaskId {
        TaskId::from_raw(raw).expect("a positive task id")
    }

    /// (token, steps) of each command, in order.
    fn sent(cmds: &[Command]) -> Vec<(u32, Vec<Step>)> {
        cmds.iter().map(|c| (c.token, c.steps.clone())).collect()
    }

    fn pause(finished: bool, user_leaving: bool) -> Step {
        Step::Pause {
            finished,
            user_leaving,
        }
    }

    fn new_intent(resume: bool) -> Step {
        Step::NewIntent {
            intents: vec!["again"],
            resume,
        }
    }

    /// singleTop on top, resumed: the intent alone, resume true; nothing else is sent (ActivityRecord.java:4990-4997).
    #[test]
    fn new_intent_to_the_resumed_top_is_sent_at_once_and_resumes() {
        let mut m = Machine::new();
        let t = task(1);
        m.push(t, MAIN, APP, "main");
        m.resumed(&MAIN);
        let cmds = m.new_intent(&MAIN, "again", false).expect("known token");
        assert_eq!(sent(&cmds), vec![(MAIN, vec![new_intent(true)])]);
        assert_eq!(m.phase(&MAIN), Some(Phase::Resumed));
        assert!(m.new_intent(&UNKNOWN, "again", false).is_none());
    }

    /// singleTask under a top: the records above finish (clear top), the stopped target comes back with the intent between Start and Resume, as TransactionExecutor cycles a stopped
    /// activity to ON_START before NewIntentItem's post state ON_RESUME; the cleared record is paused, stopped, destroyed.
    #[test]
    fn new_intent_with_clear_top_finishes_above_and_delivers_before_the_resume() {
        let mut m = Machine::new();
        let t = task(1);
        m.push(t, MAIN, APP, "main");
        m.resumed(&MAIN);
        m.push(t, OTHER, APP, "other");
        m.paused(&MAIN);
        m.resumed(&OTHER);
        m.stopped(&MAIN);
        assert_eq!(m.phase(&MAIN), Some(Phase::Stopped));
        let cmds = m.new_intent(&MAIN, "again", true).expect("known token");
        assert_eq!(
            sent(&cmds),
            vec![
                (OTHER, vec![pause(true, false)]),
                (MAIN, vec![Step::Start, new_intent(false), Step::Resume]),
            ]
        );
        assert_eq!(m.top(t).map(|(k, _)| *k), Some(MAIN));
        m.paused(&OTHER);
        assert_eq!(sent(&m.resumed(&MAIN)), vec![(OTHER, vec![Step::Stop])]);
        assert_eq!(
            sent(&m.stopped(&OTHER)),
            vec![(OTHER, vec![Step::Destroy { finished: true }])]
        );
    }

    /// A record in transit (Pausing) keeps the intent: none is sent, the next Resume carries it.
    #[test]
    fn new_intent_to_a_pausing_record_waits_for_its_resume() {
        let mut m = Machine::new();
        let t = task(1);
        m.push(t, MAIN, APP, "main");
        m.resumed(&MAIN);
        m.push(t, OTHER, APP, "other");
        assert_eq!(m.phase(&MAIN), Some(Phase::Pausing));
        let cmds = m.new_intent(&MAIN, "again", true).expect("known token");
        assert_eq!(sent(&cmds), vec![(OTHER, vec![pause(true, false)])]);
        assert_eq!(
            sent(&m.paused(&MAIN)),
            vec![(MAIN, vec![new_intent(false), Step::Resume])]
        );
    }

    #[test]
    fn push_launches_and_resumes_the_new_top() {
        let mut m = Machine::new();
        let cmds = m.push(task(1), SPLASH, APP, "splash");
        assert_eq!(
            sent(&cmds),
            vec![(SPLASH, vec![Step::Launch("splash"), Step::Resume])]
        );
        assert_eq!(m.phase(&SPLASH), Some(Phase::Resuming));
        assert!(m.resumed(&SPLASH).is_empty());
        assert_eq!(m.phase(&SPLASH), Some(Phase::Resumed));
        assert_eq!(m.owner(&SPLASH), Some(&APP));
        assert_eq!(m.task_of(&SPLASH), Some(task(1)));
    }

    /// The Splash -> Main flow: Splash starts Main and finishes itself in onCreate, so its resume ack arrives while it is Pausing.
    #[test]
    fn splash_starts_main_and_finishes_itself() {
        let mut m = Machine::new();
        let t = task(1);
        m.push(t, SPLASH, APP, "splash");
        // onCreate: startActivity(Main) -> pause Splash, launch Main
        let cmds = m.push(t, MAIN, APP, "main");
        assert_eq!(
            sent(&cmds),
            vec![
                (SPLASH, vec![pause(false, true)]),
                (MAIN, vec![Step::Launch("main"), Step::Resume]),
            ]
        );
        // onCreate: finish() -> nothing to send, Main is still resuming
        assert!(sent(&m.finish(&SPLASH, |_| "ok").expect("known token")).is_empty());
        assert!(m.resumed(&SPLASH).is_empty());
        assert_eq!(m.phase(&SPLASH), Some(Phase::Pausing));
        assert!(m.paused(&SPLASH).is_empty());
        assert_eq!(m.phase(&SPLASH), Some(Phase::Paused));
        // Main resumed: Splash stops, then is destroyed and gone
        assert_eq!(sent(&m.resumed(&MAIN)), vec![(SPLASH, vec![Step::Stop])]);
        assert_eq!(
            sent(&m.stopped(&SPLASH)),
            vec![(SPLASH, vec![Step::Destroy { finished: true }])]
        );
        assert_eq!(m.phase(&SPLASH), None);
        assert_eq!(m.phase(&MAIN), Some(Phase::Resumed));
    }

    #[test]
    fn a_resumed_record_under_a_new_top_is_paused_first() {
        let mut m = Machine::new();
        let t = task(1);
        m.push(t, SPLASH, APP, "splash");
        m.resumed(&SPLASH);
        assert_eq!(
            sent(&m.push(t, MAIN, APP, "main")),
            vec![
                (SPLASH, vec![pause(false, true)]),
                (MAIN, vec![Step::Launch("main"), Step::Resume]),
            ]
        );
    }

    /// Rule (3): the record below stops only after the new top is Resumed, not while it is still Resuming.
    #[test]
    fn stop_waits_for_the_new_top_to_be_resumed() {
        let mut m = Machine::new();
        let t = task(1);
        m.push(t, SPLASH, APP, "splash");
        m.resumed(&SPLASH);
        m.push(t, MAIN, APP, "main");
        assert!(m.paused(&SPLASH).is_empty());
        assert_eq!(m.phase(&MAIN), Some(Phase::Resuming));
        assert_eq!(sent(&m.resumed(&MAIN)), vec![(SPLASH, vec![Step::Stop])]);
        assert_eq!(m.phase(&SPLASH), Some(Phase::Stopping));
    }

    /// Finishing the top restarts the stopped record below it; the finished one is paused first, stopped once the restart resumes, then destroyed.
    #[test]
    fn finishing_the_top_restarts_the_stopped_record_below() {
        let mut m = Machine::new();
        let t = task(1);
        m.push(t, SPLASH, APP, "splash");
        m.resumed(&SPLASH);
        m.push(t, MAIN, APP, "main");
        m.paused(&SPLASH);
        m.resumed(&MAIN);
        m.stopped(&SPLASH);
        assert_eq!(m.phase(&SPLASH), Some(Phase::Stopped));

        let cmds = m.finish(&MAIN, |_| "ok").expect("known token");
        assert_eq!(
            sent(&cmds),
            vec![
                (MAIN, vec![pause(true, false)]),
                (SPLASH, vec![Step::Start, Step::Resume]),
            ]
        );
        assert!(m.paused(&MAIN).is_empty());
        assert_eq!(sent(&m.resumed(&SPLASH)), vec![(MAIN, vec![Step::Stop])]);
        assert_eq!(
            sent(&m.stopped(&MAIN)),
            vec![(MAIN, vec![Step::Destroy { finished: true }])]
        );
        assert_eq!(m.phase(&MAIN), None);
        assert_eq!(m.phase(&SPLASH), Some(Phase::Resumed));
    }

    #[test]
    fn a_stopped_record_that_is_not_finishing_is_kept() {
        let mut m = Machine::new();
        let t = task(1);
        m.push(t, SPLASH, APP, "splash");
        m.resumed(&SPLASH);
        m.push(t, MAIN, APP, "main");
        m.paused(&SPLASH);
        m.resumed(&MAIN);
        assert!(m.stopped(&SPLASH).is_empty());
        assert_eq!(m.phase(&SPLASH), Some(Phase::Stopped));
    }

    /// Rule (1) on the finishing top with no record left below it: it is paused, stopped and destroyed, and the task disappears.
    #[test]
    fn finishing_the_only_activity_pauses_stops_and_destroys_it() {
        let mut m = Machine::new();
        let t = task(1);
        m.push(t, MAIN, APP, "main");
        m.resumed(&MAIN);
        assert_eq!(
            sent(&m.finish(&MAIN, |_| "ok").expect("known token")),
            vec![(MAIN, vec![pause(true, false)])]
        );
        assert_eq!(sent(&m.paused(&MAIN)), vec![(MAIN, vec![Step::Stop])]);
        assert_eq!(
            sent(&m.stopped(&MAIN)),
            vec![(MAIN, vec![Step::Destroy { finished: true }])]
        );
        assert_eq!(m.phase(&MAIN), None);
        assert_eq!(
            sent(&m.push(t, OTHER, APP, "other")),
            vec![(OTHER, vec![Step::Launch("other"), Step::Resume])]
        );
    }

    /// A pause sent while the resume is still in flight: the resume ack keeps Pausing, the pause ack makes Paused, and the stop waits for the new top.
    #[test]
    fn resume_ack_while_pausing_keeps_the_pause() {
        let mut m = Machine::new();
        let t = task(1);
        m.push(t, SPLASH, APP, "splash");
        let cmds = m.push(t, MAIN, APP, "main");
        assert_eq!(sent(&cmds)[0], (SPLASH, vec![pause(false, true)]));
        assert!(m.resumed(&SPLASH).is_empty());
        assert_eq!(m.phase(&SPLASH), Some(Phase::Pausing));
        assert!(m.paused(&SPLASH).is_empty());
        assert_eq!(m.phase(&SPLASH), Some(Phase::Paused));
        assert_eq!(sent(&m.resumed(&MAIN)), vec![(SPLASH, vec![Step::Stop])]);
    }

    /// Acks that do not match the record's phase change nothing and send nothing.
    #[test]
    fn an_ack_in_the_wrong_phase_changes_nothing() {
        let mut m = Machine::new();
        m.push(task(1), SPLASH, APP, "splash");
        m.resumed(&SPLASH);
        assert!(m.paused(&SPLASH).is_empty());
        assert!(m.stopped(&SPLASH).is_empty());
        assert_eq!(m.phase(&SPLASH), Some(Phase::Resumed));
    }

    /// Each task advances alone: a launch on task 2 leaves task 1's resumed record alone.
    #[test]
    fn tasks_advance_separately() {
        let mut m = Machine::new();
        m.push(task(1), SPLASH, APP, "splash");
        m.resumed(&SPLASH);
        let cmds = m.push(task(2), OTHER, APP, "other");
        assert_eq!(
            sent(&cmds),
            vec![(OTHER, vec![Step::Launch("other"), Step::Resume])]
        );
        assert_eq!(m.phase(&SPLASH), Some(Phase::Resumed));
    }

    /// A relaunch resumes a resumed record only; a paused one keeps its phase and sends no Resume.
    #[test]
    fn relaunch_resumes_only_a_resumed_record() {
        let mut m = Machine::new();
        let t = task(1);
        m.push(t, SPLASH, APP, "splash");
        assert_eq!(m.relaunch(&SPLASH), vec![Step::Resume]);
        assert_eq!(m.phase(&SPLASH), Some(Phase::Resuming));
        m.resumed(&SPLASH);
        m.push(t, MAIN, APP, "main");
        m.paused(&SPLASH);
        assert!(m.relaunch(&SPLASH).is_empty());
        assert_eq!(m.phase(&SPLASH), Some(Phase::Paused));
        assert!(m.relaunch(&UNKNOWN).is_empty());
    }

    /// A start is pending from its push until the new top is resumed and the paused record is stopped: the probe must not pass in between.
    #[test]
    fn settled_only_when_no_record_is_in_transit() {
        let mut m = Machine::new();
        assert!(m.settled());
        m.push(task(1), SPLASH, APP, "splash");
        assert!(!m.settled(), "launch sent, not acknowledged");
        m.resumed(&SPLASH);
        assert!(m.settled());
        m.push(task(1), MAIN, APP, "main");
        assert!(!m.settled(), "start pending: Splash pausing, Main resuming");
        m.paused(&SPLASH);
        m.resumed(&MAIN);
        assert!(!m.settled(), "Splash still to be stopped");
        m.stopped(&SPLASH);
        assert!(m.settled());
    }

    /// A dead owner's records leave the stack and are no longer listed; the others stay.
    #[test]
    fn drop_owned_removes_only_the_dead_owners_records() {
        let mut m = Machine::new();
        m.push(task(1), SPLASH, APP, "splash");
        m.push(task(2), OTHER, "other", "other");
        assert_eq!(m.records(task(1)).count(), 1);
        m.drop_owned(|o| *o == APP);
        assert_eq!(m.records(task(1)).count(), 0);
        assert!(m.phase(&SPLASH).is_none());
        assert_eq!(
            m.records(task(2)).map(|(k, _)| *k).collect::<Vec<_>>(),
            vec![OTHER]
        );
    }

    #[test]
    fn unknown_tokens_are_ignored() {
        let mut m = Machine::new();
        m.push(task(1), SPLASH, APP, "splash");
        assert!(m.resumed(&UNKNOWN).is_empty());
        assert!(m.paused(&UNKNOWN).is_empty());
        assert!(m.stopped(&UNKNOWN).is_empty());
        assert!(m.finish(&UNKNOWN, |_| "ok").is_none());
        assert_eq!(m.phase(&UNKNOWN), None);
        assert_eq!(m.is_finishing(&SPLASH), Some(false));
    }

    /// A Task starts shown; hiding one leaves another Task's resumed top resumed.
    #[test]
    fn hiding_one_task_leaves_the_others_resumed() {
        let mut m = Machine::new();
        m.push(task(1), MAIN, APP, "main");
        m.resumed(&MAIN);
        m.push(task(2), OTHER, APP, "other");
        m.resumed(&OTHER);
        assert!(m.is_visible(task(1)));
        let cmds = m.set_visible(task(1), false);
        assert_eq!(sent(&cmds), vec![(MAIN, vec![pause(false, true)])]);
        assert_eq!(m.phase(&OTHER), Some(Phase::Resumed));
        assert!(m.is_visible(task(2)));
    }

    /// Hiding a resumed top: it pauses first, as a cover (`user_leaving`, not finished), and stops only after the pause ack; the stop ack ends it Stopped with nothing sent.
    #[test]
    fn hiding_pauses_the_resumed_top_then_stops_it() {
        let mut m = Machine::new();
        let t = task(1);
        m.push(t, MAIN, APP, "main");
        m.resumed(&MAIN);
        let cmds = m.set_visible(t, false);
        assert_eq!(sent(&cmds), vec![(MAIN, vec![pause(false, true)])]);
        assert_eq!(m.phase(&MAIN), Some(Phase::Pausing));
        assert_eq!(sent(&m.paused(&MAIN)), vec![(MAIN, vec![Step::Stop])]);
        assert!(m.stopped(&MAIN).is_empty());
        assert_eq!(m.phase(&MAIN), Some(Phase::Stopped));
        assert!(!m.is_visible(t));
    }

    /// The resume of a launch is still unacknowledged when the Task is hidden: the pause goes after it, its ack keeps the pause (as `resume_ack_while_pausing_keeps_the_pause`), then the stop.
    #[test]
    fn hiding_a_resuming_top_pauses_it_after_its_resume_ack() {
        let mut m = Machine::new();
        let t = task(1);
        m.push(t, MAIN, APP, "main");
        assert_eq!(m.phase(&MAIN), Some(Phase::Resuming));
        let cmds = m.set_visible(t, false);
        assert_eq!(sent(&cmds), vec![(MAIN, vec![pause(false, true)])]);
        assert!(m.resumed(&MAIN).is_empty());
        assert_eq!(m.phase(&MAIN), Some(Phase::Pausing));
        assert_eq!(sent(&m.paused(&MAIN)), vec![(MAIN, vec![Step::Stop])]);
    }

    /// Hiding a Task with two records: the resumed top pauses, and the paused record below it is stopped by the pause ack's advance. Both end Stopped.
    #[test]
    fn hiding_stops_every_record_of_the_task() {
        let mut m = Machine::new();
        let t = task(1);
        m.push(t, MAIN, APP, "main");
        m.resumed(&MAIN);
        m.push(t, OTHER, APP, "other");
        m.paused(&MAIN);
        m.resumed(&OTHER);
        m.stopped(&MAIN);
        assert_eq!(m.phase(&MAIN), Some(Phase::Stopped));
        assert_eq!(m.phase(&OTHER), Some(Phase::Resumed));
        let cmds = m.set_visible(t, false);
        assert_eq!(sent(&cmds), vec![(OTHER, vec![pause(false, true)])]);
        assert_eq!(sent(&m.paused(&OTHER)), vec![(OTHER, vec![Step::Stop])]);
        assert!(m.stopped(&OTHER).is_empty());
        assert_eq!(m.phase(&OTHER), Some(Phase::Stopped));
        assert_eq!(m.phase(&MAIN), Some(Phase::Stopped));
    }

    /// A launch into a hidden Task: created and stopped in one transaction (Launch, Stop), never resumed; the stop ack ends it Stopped.
    #[test]
    fn a_launch_into_a_hidden_task_stays_stopped() {
        let mut m = Machine::new();
        let t = task(1);
        assert!(m.set_visible(t, false).is_empty());
        let cmds = m.push(t, MAIN, APP, "main");
        assert_eq!(
            sent(&cmds),
            vec![(MAIN, vec![Step::Launch("main"), Step::Stop])]
        );
        assert_eq!(m.phase(&MAIN), Some(Phase::Stopping));
        assert!(m.stopped(&MAIN).is_empty());
        assert_eq!(m.phase(&MAIN), Some(Phase::Stopped));
    }

    /// Restore: a shown Task's stopped top gets Start then Resume, the rule of any stop; the resume ack makes it Resumed.
    #[test]
    fn showing_a_hidden_task_restarts_its_top_with_start_then_resume() {
        let mut m = Machine::new();
        let t = task(1);
        m.set_visible(t, false);
        m.push(t, MAIN, APP, "main");
        m.stopped(&MAIN);
        let cmds = m.set_visible(t, true);
        assert_eq!(sent(&cmds), vec![(MAIN, vec![Step::Start, Step::Resume])]);
        assert_eq!(m.phase(&MAIN), Some(Phase::Resuming));
        assert!(m.is_visible(t));
        m.resumed(&MAIN);
        assert_eq!(m.phase(&MAIN), Some(Phase::Resumed));
    }

    /// Shown while the hide's stop is unacknowledged: nothing is sent; the stop ack then restarts the top.
    #[test]
    fn showing_while_stopping_waits_for_the_stop_ack() {
        let mut m = Machine::new();
        let t = task(1);
        m.push(t, MAIN, APP, "main");
        m.resumed(&MAIN);
        m.set_visible(t, false);
        m.paused(&MAIN);
        assert_eq!(m.phase(&MAIN), Some(Phase::Stopping));
        assert!(m.set_visible(t, true).is_empty());
        assert_eq!(
            sent(&m.stopped(&MAIN)),
            vec![(MAIN, vec![Step::Start, Step::Resume])]
        );
    }

    /// An intent to a hidden Task's stopped top waits (pending) and goes between Start and Resume when the Task is shown, as ActivityRecord.newIntents.
    #[test]
    fn an_intent_to_a_hidden_stopped_top_waits_for_the_show() {
        let mut m = Machine::new();
        let t = task(1);
        m.push(t, MAIN, APP, "main");
        m.resumed(&MAIN);
        m.set_visible(t, false);
        m.paused(&MAIN);
        m.stopped(&MAIN);
        let cmds = m.new_intent(&MAIN, "again", false).expect("known token");
        assert!(cmds.is_empty());
        assert_eq!(
            sent(&m.set_visible(t, true)),
            vec![(MAIN, vec![Step::Start, new_intent(false), Step::Resume])]
        );
    }

    fn owed(to: u32, request_code: i32) -> super::ResultTo<u32> {
        super::ResultTo {
            token: to,
            who: None,
            request_code,
        }
    }

    /// The child finishes while its caller is resumed in another Task: the result is sent at once (ActivityRecord.java:4873-4887), before the child pauses.
    #[test]
    fn a_result_to_a_resumed_caller_is_sent_at_once() {
        let mut m = Machine::new();
        m.push(task(1), MAIN, APP, "main");
        m.resumed(&MAIN);
        m.push_owed(task(2), OTHER, APP, "other", Some(owed(MAIN, 7)));
        m.resumed(&OTHER);
        let cmds = m
            .finish(&OTHER, |to| {
                assert_eq!((to.token, to.request_code), (MAIN, 7));
                "ok"
            })
            .expect("known token");
        assert_eq!(
            sent(&cmds),
            vec![
                (MAIN, vec![Step::Result(vec!["ok"])]),
                (OTHER, vec![pause(true, false)]),
            ]
        );
        assert_eq!(m.phase(&MAIN), Some(Phase::Resumed));
    }

    /// The caller is stopped when its child finishes: the result waits in the caller's results and goes between its Start and its Resume (TaskFragment.java:1665-1693).
    #[test]
    fn a_result_to_a_stopped_caller_rides_its_restart() {
        let mut m = Machine::new();
        let t = task(1);
        m.push(t, SPLASH, APP, "splash");
        m.resumed(&SPLASH);
        m.push_owed(t, MAIN, APP, "main", Some(owed(SPLASH, 7)));
        m.paused(&SPLASH);
        m.resumed(&MAIN);
        m.stopped(&SPLASH);
        assert_eq!(m.phase(&SPLASH), Some(Phase::Stopped));
        let cmds = m.finish(&MAIN, |_| "ok").expect("known token");
        assert_eq!(
            sent(&cmds),
            vec![
                (MAIN, vec![pause(true, false)]),
                (
                    SPLASH,
                    vec![Step::Start, Step::Result(vec!["ok"]), Step::Resume]
                ),
            ]
        );
        assert_eq!(m.phase(&SPLASH), Some(Phase::Resuming));
    }

    /// Results go before the intents that wait for the same resume (TaskFragment.java:1665-1693).
    #[test]
    fn a_result_goes_before_the_intents_of_the_same_resume() {
        let mut m = Machine::new();
        let t = task(1);
        m.push(t, MAIN, APP, "main");
        m.resumed(&MAIN);
        m.push_owed(t, OTHER, APP, "other", Some(owed(MAIN, 7)));
        m.paused(&MAIN);
        m.resumed(&OTHER);
        m.stopped(&MAIN);
        assert!(
            m.new_intent(&MAIN, "again", false)
                .expect("known token")
                .is_empty()
        );
        let cmds = m.finish(&OTHER, |_| "ok").expect("known token");
        assert_eq!(
            sent(&cmds),
            vec![
                (OTHER, vec![pause(true, false)]),
                (
                    MAIN,
                    vec![
                        Step::Start,
                        Step::Result(vec!["ok"]),
                        new_intent(false),
                        Step::Resume,
                    ]
                ),
            ]
        );
    }

    /// A caller whose process died is gone: nothing is sent for the result, and the child's own finish still pauses the child.
    #[test]
    fn a_result_to_a_gone_caller_is_dropped() {
        let mut m = Machine::new();
        m.push(task(1), MAIN, APP, "main");
        m.resumed(&MAIN);
        m.push_owed(task(2), OTHER, "other", "other", Some(owed(MAIN, 7)));
        m.resumed(&OTHER);
        m.drop_owned(|o| *o == APP);
        assert_eq!(m.phase(&MAIN), None);
        let cmds = m.finish(&OTHER, |_| "ok").expect("known token");
        assert_eq!(sent(&cmds), vec![(OTHER, vec![pause(true, false)])]);
    }

    /// singleTop reuse with a result owed to the top: the cancelled result goes out before the new intent (ActivityStarter.java:2469-2476).
    #[test]
    fn a_reuse_with_a_result_sends_the_cancel_before_the_intent() {
        let mut m = Machine::new();
        m.push(task(1), MAIN, APP, "main");
        m.resumed(&MAIN);
        let mut cmds = m.deliver(&owed(MAIN, 7), "cancel");
        cmds.extend(m.new_intent(&MAIN, "again", false).expect("known token"));
        assert_eq!(
            sent(&cmds),
            vec![
                (MAIN, vec![Step::Result(vec!["cancel"])]),
                (MAIN, vec![new_intent(true)]),
            ]
        );
        assert_eq!(m.phase(&MAIN), Some(Phase::Resumed));
    }

    /// A cancelled start to a paused caller waits in its results and goes with the caller's next resume, not before it.
    #[test]
    fn a_cancel_to_a_paused_caller_waits_for_its_resume() {
        let mut m = Machine::new();
        let t = task(1);
        m.push(t, SPLASH, APP, "splash");
        m.resumed(&SPLASH);
        m.push(t, MAIN, APP, "main");
        m.paused(&SPLASH);
        assert!(m.deliver(&owed(SPLASH, 7), "cancel").is_empty());
        assert_eq!(sent(&m.resumed(&MAIN)), vec![(SPLASH, vec![Step::Stop])]);
        m.stopped(&SPLASH);
        let cmds = m.finish(&MAIN, |_| "unused").expect("known token");
        assert_eq!(
            sent(&cmds),
            vec![
                (MAIN, vec![pause(true, false)]),
                (
                    SPLASH,
                    vec![Step::Start, Step::Result(vec!["cancel"]), Step::Resume]
                ),
            ]
        );
    }
}
