// SPDX-License-Identifier: MIT
//! The activity lifecycle of the services (ATMS-lite), as a pure machine: per Task, which activity the app must pause, resume, start, stop or destroy next. Plain Rust, no binder, so its tests build without the converter feature.
//! Every decision comes back as [`Command`]s in decision order. The caller sends them under its lock: each is a oneway `scheduleTransaction`, so the app runs them in the order decided.
//! Nothing waits on an ack. `resumed` / `paused` / `stopped` move one record to its next phase and advance its task again, so the order of acks between records does not change what is sent.
//! Generic over the token `K` (the services use SpIBinder), the owner `O` (the app's thread and identity, cloned into each command) and the launch payload `L` (taken by the one Launch step).

use std::collections::BTreeMap;

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
pub enum Step<L, I> {
    /// LaunchActivityItem: the record's launch payload, taken once.
    Launch(L),
    /// NewIntentItem: `intents` delivered to onNewIntent. `resume`: the item itself ends resumed (ActivityRecord.java:4997 `mState == RESUMED`); false when a Resume step follows or the record stays paused.
    NewIntent { intents: Vec<I>, resume: bool },
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
pub struct Command<K, O, L, I> {
    pub token: K,
    pub owner: O,
    pub steps: Vec<Step<L, I>>,
}

struct Record<K, O, L, I> {
    token: K,
    owner: O,
    phase: Phase,
    /// `finish` was called: the record leaves the stack once stopped.
    finishing: bool,
    /// Taken by the Launch step.
    launch: Option<L>,
    /// ActivityRecord.newIntents: intents not sent at once (record not resumed or paused), delivered before its next Resume (TaskFragment.java:1691-1693).
    pending: Vec<I>,
}

impl<K: Clone, O: Clone, L, I> Record<K, O, L, I> {
    fn command(&self, steps: Vec<Step<L, I>>) -> Command<K, O, L, I> {
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
pub struct Machine<K, O, L, I> {
    tasks: BTreeMap<TaskId, Vec<Record<K, O, L, I>>>,
}

impl<K, O, L, I> Default for Machine<K, O, L, I> {
    fn default() -> Self {
        Machine {
            tasks: BTreeMap::new(),
        }
    }
}

impl<K: Clone + PartialEq, O: Clone, L, I> Machine<K, O, L, I> {
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
    ) -> Vec<Command<K, O, L, I>> {
        self.tasks.entry(task).or_default().push(Record {
            token,
            owner,
            phase: Phase::Created,
            finishing: false,
            launch: Some(launch),
            pending: Vec::new(),
        });
        self.advance(task)
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
    ) -> Option<Vec<Command<K, O, L, I>>> {
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
    pub fn resumed(&mut self, token: &K) -> Vec<Command<K, O, L, I>> {
        let Some((task, record)) = self.find_mut(token) else {
            return Vec::new();
        };
        if record.phase == Phase::Resuming {
            record.phase = Phase::Resumed;
        }
        self.advance(task)
    }

    /// The app acknowledged a pause (`activityPaused`).
    pub fn paused(&mut self, token: &K) -> Vec<Command<K, O, L, I>> {
        let Some((task, record)) = self.find_mut(token) else {
            return Vec::new();
        };
        if record.phase == Phase::Pausing {
            record.phase = Phase::Paused;
        }
        self.advance(task)
    }

    /// The app acknowledged a stop (`activityStopped`).
    pub fn stopped(&mut self, token: &K) -> Vec<Command<K, O, L, I>> {
        let Some((task, record)) = self.find_mut(token) else {
            return Vec::new();
        };
        if record.phase == Phase::Stopping {
            record.phase = Phase::Stopped;
        }
        self.advance(task)
    }

    /// `finishActivity`: the record is finishing. None when the token names no record.
    pub fn finish(&mut self, token: &K) -> Option<Vec<Command<K, O, L, I>>> {
        let (task, record) = self.find_mut(token)?;
        record.finishing = true;
        Some(self.advance(task))
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
    pub fn relaunch(&mut self, token: &K) -> Vec<Step<L, I>> {
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

    fn find(&self, token: &K) -> Found<'_, Record<K, O, L, I>> {
        self.tasks.iter().find_map(|(task, records)| {
            records
                .iter()
                .find(|r| &r.token == token)
                .map(|r| (*task, r))
        })
    }

    fn find_mut(&mut self, token: &K) -> FoundMut<'_, Record<K, O, L, I>> {
        self.tasks.iter_mut().find_map(|(task, records)| {
            records
                .iter_mut()
                .find(|r| &r.token == token)
                .map(|r| (*task, r))
        })
    }

    /// The rules, applied to one task in order. D = the top record that is not finishing: the one to resume.
    fn advance(&mut self, task: TaskId) -> Vec<Command<K, O, L, I>> {
        let mut out = Vec::new();
        let Some(records) = self.tasks.get_mut(&task) else {
            return out;
        };
        let top = records.iter().rposition(|r| !r.finishing);
        // (1) every record other than D that is Resumed or Resuming is paused first
        for (i, r) in records.iter_mut().enumerate() {
            if Some(i) != top && matches!(r.phase, Phase::Resumed | Phase::Resuming) {
                r.phase = Phase::Pausing;
                let steps = vec![Step::Pause {
                    finished: r.finishing,
                    user_leaving: !r.finishing,
                }];
                out.push(r.command(steps));
            }
        }
        // (2) D comes back: launched, restarted from Stopped, or resumed from Paused. Pausing/Resuming/Stopping wait for their ack.
        if let Some(i) = top {
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
                // pending intents go first, before the Resume that follows (TaskFragment.java:1691-1693 sends them in the resume, ahead of its ResumeActivityItem)
                if !r.pending.is_empty() {
                    let intents = std::mem::take(&mut r.pending);
                    let at = steps.len() - 1;
                    steps.insert(
                        at,
                        Step::NewIntent {
                            intents,
                            resume: false,
                        },
                    );
                }
                r.phase = Phase::Resuming;
                out.push(r.command(steps));
            }
        }
        // (3) paused records other than D are stopped once D is Resumed, or once no D is left
        let ready = match top {
            None => true,
            Some(i) => records[i].phase == Phase::Resumed,
        };
        if ready {
            for (i, r) in records.iter_mut().enumerate() {
                if Some(i) != top && r.phase == Phase::Paused {
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

    type Machine = super::Machine<u32, &'static str, &'static str, &'static str>;
    type Command = super::Command<u32, &'static str, &'static str, &'static str>;
    type Step = super::Step<&'static str, &'static str>;

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
        assert!(sent(&m.finish(&SPLASH).expect("known token")).is_empty());
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

        let cmds = m.finish(&MAIN).expect("known token");
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
            sent(&m.finish(&MAIN).expect("known token")),
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
        assert!(m.finish(&UNKNOWN).is_none());
        assert_eq!(m.phase(&UNKNOWN), None);
        assert_eq!(m.is_finishing(&SPLASH), Some(false));
    }
}
