// SPDX-License-Identifier: MIT
//! The ordinary audio focus stack: AOSP MediaFocusControl's one stack with multi-audio-focus off (its default), FocusRequester's loss table and its callbacks.
//! Pure state. audio.rs runs the eligibility checks and links the death recipient, then calls [`Stack::request`]; each owner's dispatch is a oneway IAudioFocusDispatcher call, made under the caller's stack lock in mutation order.
//! Top removal (abandon, death) restores the next owner with GAIN (AOSP notifyTopOfAudioFocusStack); buried removal is silent.
//! Not modelled, counted where AOSP would act: player attenuation (duckPlayers) and fade-out (fadeOutPlayers) have no tracked player, so CAN_DUCK and LOSS go out as plain callbacks.

/// AudioManager.AUDIOFOCUS_NONE (AudioManager.java:4289).
pub const NONE: i32 = 0;
/// AudioManager.AUDIOFOCUS_GAIN (:4296).
pub const GAIN: i32 = 1;
/// AudioManager.AUDIOFOCUS_GAIN_TRANSIENT (:4304).
pub const GAIN_TRANSIENT: i32 = 2;
/// AudioManager.AUDIOFOCUS_GAIN_TRANSIENT_MAY_DUCK (:4314).
pub const GAIN_TRANSIENT_MAY_DUCK: i32 = 3;
/// AudioManager.AUDIOFOCUS_GAIN_TRANSIENT_EXCLUSIVE (:4323).
pub const GAIN_TRANSIENT_EXCLUSIVE: i32 = 4;
/// AudioManager.AUDIOFOCUS_LOSS (:4328): -1 * GAIN.
pub const LOSS: i32 = -1;
/// AudioManager.AUDIOFOCUS_LOSS_TRANSIENT (:4333): -1 * GAIN_TRANSIENT.
pub const LOSS_TRANSIENT: i32 = -2;
/// AudioManager.AUDIOFOCUS_LOSS_TRANSIENT_CAN_DUCK (:4340): -1 * GAIN_TRANSIENT_MAY_DUCK.
pub const LOSS_TRANSIENT_CAN_DUCK: i32 = -3;
/// AudioManager.AUDIOFOCUS_REQUEST_FAILED (:4536).
pub const REQUEST_FAILED: i32 = 0;
/// AudioManager.AUDIOFOCUS_REQUEST_GRANTED (:4540).
pub const REQUEST_GRANTED: i32 = 1;
/// AudioManager.AUDIOFOCUS_REQUEST_DELAYED (:4548).
pub const REQUEST_DELAYED: i32 = 2;
/// AudioManager.AUDIOFOCUS_FLAG_DELAY_OK (:4633).
pub const FLAG_DELAY_OK: i32 = 0x1 << 0;
/// AudioManager.AUDIOFOCUS_FLAG_PAUSES_ON_DUCKABLE_LOSS (:4645).
pub const FLAG_PAUSES_ON_DUCKABLE_LOSS: i32 = 0x1 << 1;
/// AudioManager.AUDIOFOCUS_FLAG_LOCK (:4654).
pub const FLAG_LOCK: i32 = 0x1 << 2;
/// AudioManager.AUDIOFOCUS_FLAG_TEST (:4661).
pub const FLAG_TEST: i32 = 0x1 << 3;

/// Owner count above which a request fails (MediaFocusControl MAX_STACK_SIZE:218, checked as `size > MAX`).
pub const MAX_STACK_SIZE: usize = 100;

/// MediaFocusControl.DUCKING_IN_APP_SDK_LEVEL: Build.VERSION_CODES.N_MR1 (MediaFocusControl.java:84). An owner at or below it is never ducked by the framework.
pub const DUCKING_IN_APP_SDK_LEVEL: i32 = 25;

/// The counted-refusal family of this module (`atl-diag`, interface name as IAudioService's counters).
const INTERFACE: &str = "IAudioService";

/// Delivers one owner's focus change: (focusChange, clientId).
pub type Dispatch = Box<dyn Fn(i32, &str) + Send>;

/// A focus request after the binder layer's eligibility checks (the caller's uid, its AudioAttributes-derived gain, flags and target sdk).
pub struct Request<B> {
    pub client: String,
    pub binder: B,
    pub uid: u32,
    pub gain: i32,
    pub flags: i32,
    pub sdk: i32,
    pub dispatch: Option<Dispatch>,
}

/// One focus owner (FocusRequester): `binder` identifies the requesting binder (death removes by it), `link` is its death link (released with the owner, unlinking it), `loss` the strongest loss received so far.
pub struct Owner<B, L> {
    client: String,
    binder: B,
    uid: u32,
    gain: i32,
    flags: i32,
    sdk: i32,
    loss: i32,
    dispatch: Option<Dispatch>,
    link: L,
}

/// The focus stack, bottom to top; the last owner holds focus. Removed links are returned so the caller releases them outside its lock.
pub struct Stack<B, L> {
    owners: Vec<Owner<B, L>>,
}

impl<B: PartialEq, L> Default for Stack<B, L> {
    fn default() -> Self {
        Stack { owners: Vec::new() }
    }
}

impl<B: PartialEq, L> Stack<B, L> {
    /// Number of owners.
    pub fn len(&self) -> usize {
        self.owners.len()
    }

    /// Whether no owner holds any focus.
    pub fn is_empty(&self) -> bool {
        self.owners.is_empty()
    }

    /// The clientId of the owner holding focus.
    pub fn top(&self) -> Option<&str> {
        self.owners.last().map(|o| o.client.as_str())
    }

    /// The strongest loss `client` has received (NONE when it holds no loss).
    pub fn loss_of(&self, client: &str) -> Option<i32> {
        self.owners
            .iter()
            .find(|o| o.client == client)
            .map(|o| o.loss)
    }

    /// MediaFocusControl.requestAudioFocus after the binder checks. `alive` is pingBinder (dead client refused first); `link` runs only once the request is admissible, so no owner exists without its live link.
    /// Returns the result and the links this request released (the same client's replaced owner, a silent removal, the losers that left, its own unused link): the caller drops them after releasing its lock.
    pub fn request(
        &mut self,
        r: Request<B>,
        alive: bool,
        link: impl FnOnce() -> Option<L>,
    ) -> (i32, Vec<L>) {
        if !alive {
            atl_diag::refused(INTERFACE, "requestAudioFocus (client binder dead)");
            return (REQUEST_FAILED, Vec::new());
        }
        if !is_gain(r.gain) {
            atl_diag::refused(INTERFACE, "requestAudioFocus (invalid focus gain)");
            return (REQUEST_FAILED, Vec::new());
        }
        if self.owners.len() > MAX_STACK_SIZE {
            atl_diag::refused(INTERFACE, "requestAudioFocus (stack full)");
            return (REQUEST_FAILED, Vec::new());
        }
        let Some(link) = link() else {
            atl_diag::refused(INTERFACE, "requestAudioFocus (linkToDeath failed)");
            return (REQUEST_FAILED, Vec::new());
        };
        let mut gone = Vec::new();
        // the same client on top with the same reason: unchanged, its new link is released (:1231-1237)
        match self
            .owners
            .last()
            .filter(|t| t.client == r.client)
            .map(|t| (t.gain, t.flags))
        {
            Some((gain, flags)) if gain == r.gain && flags == r.flags => {
                gone.push(link);
                return (REQUEST_GRANTED, gone);
            }
            // a changed reason replaces the top (:1241-1245)
            Some(_) => gone.extend(self.owners.pop().map(|o| o.link)),
            None => {}
        }
        // a buried entry of the same client leaves silently (:1249)
        gone.extend(self.remove_client(&r.client));
        gone.extend(self.propagate(r.gain, r.uid));
        self.owners.push(Owner {
            client: r.client,
            binder: r.binder,
            uid: r.uid,
            gain: r.gain,
            flags: r.flags,
            sdk: r.sdk,
            loss: NONE,
            dispatch: r.dispatch,
            link,
        });
        (REQUEST_GRANTED, gone)
    }

    /// MediaFocusControl.abandonAudioFocus -> removeFocusStackEntry(clientId, signal=true): every owner of `client` leaves, matched by clientId alone (AOSP hasSameClient, no uid check).
    /// Leaving the top restores the next owner; a buried owner leaves silently. Returns the removed links.
    pub fn abandon(&mut self, client: &str) -> Vec<L> {
        let top_left = self.owners.last().is_some_and(|o| o.client == client);
        let links = self.take_where(|o| o.client == client);
        if top_left {
            self.restore_top();
        }
        links
    }

    /// MediaFocusControl.removeFocusStackEntryOnDeath: every owner of `binder` leaves at once, and the top is restored once if the top was among them: a dead client gets no callback. Returns the removed links.
    /// Removal is by binder identity, the object each owner's death link was made on. No check for a link whose owner already left: its death comes only after the binder died (BpBinder::sendObituary), so every owner still found here is dead.
    pub fn die(&mut self, binder: &B) -> Vec<L> {
        let top_died = self.owners.last().is_some_and(|o| o.binder == *binder);
        let links = self.take_where(|o| o.binder == *binder);
        if top_died {
            self.restore_top();
        }
        links
    }

    /// removeFocusStackEntry(client, signal=false): every owner of `client`, top or buried, silently (a request replacing its own entry).
    fn remove_client(&mut self, client: &str) -> Vec<L> {
        self.take_where(|o| o.client == client)
    }

    /// notifyTopOfAudioFocusStack -> FocusRequester.handleFocusGain: the owner now on top regains focus, GAIN answering the loss it holds.
    /// A buried owner always holds a loss (propagate gave it one when the owner above was pushed), so the GAIN answers a loss that was sent.
    fn restore_top(&mut self) {
        let Some(top) = self.owners.last_mut() else {
            return;
        };
        debug_assert!(top.loss != NONE, "a buried owner holds a loss");
        top.loss = NONE;
        if let Some(dispatch) = &top.dispatch {
            dispatch(GAIN, &top.client);
        }
    }

    /// Removes the owners `gone` selects, returning their links in stack order.
    fn take_where(&mut self, gone: impl Fn(&Owner<B, L>) -> bool) -> Vec<L> {
        let mut links = Vec::new();
        let mut kept = Vec::with_capacity(self.owners.len());
        for owner in std::mem::take(&mut self.owners) {
            if gone(&owner) {
                links.push(owner.link);
            } else {
                kept.push(owner);
            }
        }
        self.owners = kept;
        links
    }

    /// MediaFocusControl.propagateFocusLossFromGain_syncAf: each owner, bottom up, receives the loss `gain` causes; a loss is notified once per change (FocusRequester.handleFocusLoss); owners that lost definitively (LOSS) leave the stack.
    fn propagate(&mut self, gain: i32, winner_uid: u32) -> Vec<L> {
        let mut lost: Vec<String> = Vec::new();
        for owner in &mut self.owners {
            let loss = focus_loss_for_gain(owner.loss, gain);
            if loss != owner.loss {
                owner.loss = loss;
                let cross_uid = owner.uid != winner_uid;
                // frameworkHandleFocusLoss: AOSP would attenuate (CAN_DUCK) or fade (LOSS) a cross-uid owner first; neither exists here
                if loss == LOSS_TRANSIENT_CAN_DUCK
                    && cross_uid
                    && (owner.flags & FLAG_PAUSES_ON_DUCKABLE_LOSS) == 0
                    && owner.sdk > DUCKING_IN_APP_SDK_LEVEL
                {
                    atl_diag::refused(
                        INTERFACE,
                        "requestAudioFocus (CAN_DUCK: no player attenuation, sent as callback)",
                    );
                }
                if loss == LOSS && cross_uid {
                    atl_diag::refused(
                        INTERFACE,
                        "requestAudioFocus (LOSS: no fade-out of active players, sent at once)",
                    );
                }
                if let Some(dispatch) = &owner.dispatch {
                    dispatch(loss, &owner.client);
                }
            }
            if loss == LOSS {
                lost.push(owner.client.clone());
            }
        }
        self.take_where(|o| lost.contains(&o.client))
    }
}

/// Whether `gain` is one of the four focus gains a request may ask for.
fn is_gain(gain: i32) -> bool {
    matches!(
        gain,
        GAIN | GAIN_TRANSIENT | GAIN_TRANSIENT_MAY_DUCK | GAIN_TRANSIENT_EXCLUSIVE
    )
}

/// FocusRequester.focusLossForGainRequest: the loss `gain` causes an owner that holds `current`, the stronger of the held loss and the new one (a loss never weakens).
pub fn focus_loss_for_gain(current: i32, gain: i32) -> i32 {
    stronger(current, loss_for_gain(gain))
}

/// The loss a gain causes an owner holding none (focusLossForGainRequest before the held loss is considered).
fn loss_for_gain(gain: i32) -> i32 {
    match gain {
        GAIN => LOSS,
        GAIN_TRANSIENT | GAIN_TRANSIENT_EXCLUSIVE => LOSS_TRANSIENT,
        GAIN_TRANSIENT_MAY_DUCK => LOSS_TRANSIENT_CAN_DUCK,
        _ => NONE,
    }
}

/// The stronger of two losses: larger severity wins, a tie keeps `held`.
fn stronger(held: i32, new: i32) -> i32 {
    if severity(new) > severity(held) {
        new
    } else {
        held
    }
}

/// LOSS > LOSS_TRANSIENT > LOSS_TRANSIENT_CAN_DUCK > NONE.
fn severity(loss: i32) -> i32 {
    match loss {
        LOSS => 3,
        LOSS_TRANSIENT => 2,
        LOSS_TRANSIENT_CAN_DUCK => 1,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    type Log = Arc<Mutex<Vec<(i32, String)>>>;

    fn log() -> Log {
        Arc::new(Mutex::new(Vec::new()))
    }

    fn to(log: &Log) -> Option<Dispatch> {
        let log = Arc::clone(log);
        Some(Box::new(move |change, client: &str| {
            log.lock().expect("log").push((change, client.to_owned()));
        }))
    }

    fn req(client: &str, binder: u32, uid: u32, gain: i32, flags: i32, log: &Log) -> Request<u32> {
        Request {
            client: client.to_owned(),
            binder,
            uid,
            gain,
            flags,
            sdk: 35,
            dispatch: to(log),
        }
    }

    /// The refusal counters are process-wide: the tests that bump or read them run one at a time.
    static COUNTERS: Mutex<()> = Mutex::new(());

    fn serial() -> std::sync::MutexGuard<'static, ()> {
        COUNTERS.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn calls(log: &Log) -> Vec<(i32, String)> {
        log.lock().expect("log").clone()
    }

    /// The count of a named refusal: atl-diag keys it "<interface>.<what>", 0 when none.
    fn counted(what: &str) -> u64 {
        let key = format!("IAudioService.{what}");
        atl_diag::counts()
            .into_iter()
            .find(|(o, a, _)| o == "refused" && *a == key)
            .map_or(0, |(_, _, n)| n)
    }

    /// AudioManager's constants, typed in from AudioManager.java (not read from the code under test).
    #[test]
    fn constants_match_audiomanager() {
        assert_eq!(
            [
                NONE,
                GAIN,
                GAIN_TRANSIENT,
                GAIN_TRANSIENT_MAY_DUCK,
                GAIN_TRANSIENT_EXCLUSIVE
            ],
            [0, 1, 2, 3, 4]
        );
        assert_eq!(
            [LOSS, LOSS_TRANSIENT, LOSS_TRANSIENT_CAN_DUCK],
            [-1, -2, -3]
        );
        assert_eq!(
            [REQUEST_FAILED, REQUEST_GRANTED, REQUEST_DELAYED],
            [0, 1, 2]
        );
        assert_eq!(
            [
                FLAG_DELAY_OK,
                FLAG_PAUSES_ON_DUCKABLE_LOSS,
                FLAG_LOCK,
                FLAG_TEST
            ],
            [1, 2, 4, 8]
        );
    }

    /// FocusRequester.focusLossForGainRequest, row by row: the loss the owner already received (columns) and the gain (rows).
    #[test]
    fn loss_table_matches_focusrequester() {
        let cases = [
            (NONE, GAIN, LOSS),
            (LOSS_TRANSIENT_CAN_DUCK, GAIN, LOSS),
            (LOSS_TRANSIENT, GAIN, LOSS),
            (LOSS, GAIN, LOSS),
            (NONE, GAIN_TRANSIENT, LOSS_TRANSIENT),
            (LOSS_TRANSIENT_CAN_DUCK, GAIN_TRANSIENT, LOSS_TRANSIENT),
            (LOSS_TRANSIENT, GAIN_TRANSIENT, LOSS_TRANSIENT),
            (LOSS, GAIN_TRANSIENT, LOSS),
            (NONE, GAIN_TRANSIENT_EXCLUSIVE, LOSS_TRANSIENT),
            (
                LOSS_TRANSIENT_CAN_DUCK,
                GAIN_TRANSIENT_EXCLUSIVE,
                LOSS_TRANSIENT,
            ),
            (LOSS_TRANSIENT, GAIN_TRANSIENT_EXCLUSIVE, LOSS_TRANSIENT),
            (LOSS, GAIN_TRANSIENT_EXCLUSIVE, LOSS),
            (NONE, GAIN_TRANSIENT_MAY_DUCK, LOSS_TRANSIENT_CAN_DUCK),
            (
                LOSS_TRANSIENT_CAN_DUCK,
                GAIN_TRANSIENT_MAY_DUCK,
                LOSS_TRANSIENT_CAN_DUCK,
            ),
            (LOSS_TRANSIENT, GAIN_TRANSIENT_MAY_DUCK, LOSS_TRANSIENT),
            (LOSS, GAIN_TRANSIENT_MAY_DUCK, LOSS),
        ];
        for (current, gain, want) in cases {
            assert_eq!(
                focus_loss_for_gain(current, gain),
                want,
                "loss for gain {gain} after {current}"
            );
        }
    }

    /// Eligible foreground media gain: GRANTED, the requester gets no initial callback, and an identical repeat keeps one owner.
    #[test]
    fn grant_is_granted_without_initial_callback() {
        let log = log();
        let mut s: Stack<u32, ()> = Stack::default();
        let (code, gone) = s.request(req("a", 1, 10_001, GAIN, 0, &log), true, || Some(()));
        assert_eq!(code, REQUEST_GRANTED);
        assert!(gone.is_empty());
        assert_eq!(s.len(), 1);
        assert_eq!(s.top(), Some("a"));
        assert!(
            calls(&log).is_empty(),
            "no initial callback: {:?}",
            calls(&log)
        );
    }

    #[test]
    fn identical_repeat_has_one_owner() {
        let log = log();
        // link ids: the owner keeps link 100, the repeat's own link 200 is the one released
        let mut s: Stack<u32, u32> = Stack::default();
        s.request(req("a", 1, 10_001, GAIN, 0, &log), true, || Some(100));
        let (code, gone) = s.request(req("a", 1, 10_001, GAIN, 0, &log), true, || Some(200));
        assert_eq!(code, REQUEST_GRANTED);
        assert_eq!(s.len(), 1, "one owner after an identical repeat");
        assert_eq!(gone, vec![200], "the repeat's own new link is released");
        assert!(calls(&log).is_empty());
    }

    #[test]
    fn repeated_abandon_succeeds() {
        let log = log();
        let mut s: Stack<u32, ()> = Stack::default();
        s.request(req("a", 1, 10_001, GAIN, 0, &log), true, || Some(()));
        assert_eq!(s.abandon("a").len(), 1);
        assert!(s.is_empty());
        assert!(
            s.abandon("a").is_empty(),
            "a second abandon removes nothing"
        );
        assert!(calls(&log).is_empty(), "the abandoner gets no callback");
    }

    /// DELAY_OK with no lock held is GRANTED, never DELAYED (AOSP canReassignAudioFocus is true with no locked owner).
    #[test]
    fn delay_ok_unlocked_is_granted() {
        let log = log();
        let mut s: Stack<u32, ()> = Stack::default();
        s.request(req("a", 1, 10_001, GAIN, 0, &log), true, || Some(()));
        let (code, _) = s.request(
            req("b", 2, 10_002, GAIN_TRANSIENT, FLAG_DELAY_OK, &log),
            true,
            || Some(()),
        );
        assert_eq!(code, REQUEST_GRANTED);
        assert_eq!(s.top(), Some("b"));
    }

    /// pingBinder false (DOA client): FAILED, no owner, and the death link is never made.
    #[test]
    fn dead_binder_is_refused_without_state_change() {
        let log = log();
        let mut s: Stack<u32, ()> = Stack::default();
        s.request(req("a", 1, 10_001, GAIN, 0, &log), true, || Some(()));
        let mut linked = 0;
        let (code, _) = s.request(req("b", 2, 10_002, GAIN, 0, &log), false, || {
            linked += 1;
            Some(())
        });
        assert_eq!(code, REQUEST_FAILED);
        assert_eq!(linked, 0);
        assert_eq!(s.len(), 1);
        assert_eq!(s.top(), Some("a"));
        assert!(calls(&log).is_empty());
    }

    /// linkToDeath failure: FAILED, no owner, and the existing owner keeps focus without any callback.
    #[test]
    fn failed_death_link_allocates_no_owner() {
        let log = log();
        let mut s: Stack<u32, ()> = Stack::default();
        s.request(req("a", 1, 10_001, GAIN, 0, &log), true, || Some(()));
        let (code, _) = s.request(req("b", 2, 10_002, GAIN, 0, &log), true, || None);
        assert_eq!(code, REQUEST_FAILED);
        assert_eq!(s.len(), 1);
        assert_eq!(s.top(), Some("a"));
        assert!(calls(&log).is_empty());
    }

    /// Permanent gain evicts the owner: it gets LOSS and leaves the stack.
    #[test]
    fn permanent_gain_evicts_with_loss() {
        let _serial = serial();
        let log = log();
        let mut s: Stack<u32, ()> = Stack::default();
        s.request(req("a", 1, 10_001, GAIN, 0, &log), true, || Some(()));
        let (code, _) = s.request(req("b", 2, 10_002, GAIN, 0, &log), true, || Some(()));
        assert_eq!(code, REQUEST_GRANTED);
        assert_eq!(calls(&log), vec![(LOSS, "a".to_owned())]);
        assert_eq!(s.len(), 1);
        assert_eq!(s.top(), Some("b"));
    }

    /// Transient gain keeps the owner under the new one with LOSS_TRANSIENT, for restoration later.
    #[test]
    fn transient_gain_keeps_owner_with_loss_transient() {
        let log = log();
        let mut s: Stack<u32, ()> = Stack::default();
        s.request(req("a", 1, 10_001, GAIN, 0, &log), true, || Some(()));
        let (code, _) = s.request(req("b", 2, 10_002, GAIN_TRANSIENT, 0, &log), true, || {
            Some(())
        });
        assert_eq!(code, REQUEST_GRANTED);
        assert_eq!(calls(&log), vec![(LOSS_TRANSIENT, "a".to_owned())]);
        assert_eq!(s.len(), 2);
        assert_eq!(s.loss_of("a"), Some(LOSS_TRANSIENT));
    }

    #[test]
    fn exclusive_gain_sends_loss_transient() {
        let log = log();
        let mut s: Stack<u32, ()> = Stack::default();
        s.request(req("a", 1, 10_001, GAIN, 0, &log), true, || Some(()));
        s.request(
            req("b", 2, 10_002, GAIN_TRANSIENT_EXCLUSIVE, 0, &log),
            true,
            || Some(()),
        );
        assert_eq!(calls(&log), vec![(LOSS_TRANSIENT, "a".to_owned())]);
        assert_eq!(s.len(), 2);
    }

    /// MAY_DUCK to the same uid: CAN_DUCK is a callback and nothing is counted (AOSP does not duck within one app).
    #[test]
    fn may_duck_within_one_uid_is_a_callback() {
        let _serial = serial();
        let log = log();
        let mut s: Stack<u32, ()> = Stack::default();
        s.request(req("a", 1, 10_001, GAIN, 0, &log), true, || Some(()));
        let key = "requestAudioFocus (CAN_DUCK: no player attenuation, sent as callback)";
        let before = counted(key);
        s.request(
            req("b", 2, 10_001, GAIN_TRANSIENT_MAY_DUCK, 0, &log),
            true,
            || Some(()),
        );
        assert_eq!(calls(&log), vec![(LOSS_TRANSIENT_CAN_DUCK, "a".to_owned())]);
        assert_eq!(counted(key), before);
    }

    /// MAY_DUCK to another uid: AOSP attenuates the owner's players; here the owner gets CAN_DUCK and the refusal is counted.
    #[test]
    fn may_duck_across_uids_counts_the_missing_attenuation() {
        let _serial = serial();
        let log = log();
        let mut s: Stack<u32, ()> = Stack::default();
        s.request(req("a", 1, 10_001, GAIN, 0, &log), true, || Some(()));
        let key = "requestAudioFocus (CAN_DUCK: no player attenuation, sent as callback)";
        let before = counted(key);
        s.request(
            req("b", 2, 10_002, GAIN_TRANSIENT_MAY_DUCK, 0, &log),
            true,
            || Some(()),
        );
        assert_eq!(calls(&log), vec![(LOSS_TRANSIENT_CAN_DUCK, "a".to_owned())]);
        assert_eq!(counted(key), before + 1);
    }

    /// Permanent loss to another uid: AOSP would fade active players first; none are tracked, so LOSS goes out at once and the refusal is counted.
    #[test]
    fn cross_uid_loss_counts_the_missing_fade() {
        let _serial = serial();
        let log = log();
        let mut s: Stack<u32, ()> = Stack::default();
        s.request(req("a", 1, 10_001, GAIN, 0, &log), true, || Some(()));
        let key = "requestAudioFocus (LOSS: no fade-out of active players, sent at once)";
        let before = counted(key);
        s.request(req("b", 2, 10_002, GAIN, 0, &log), true, || Some(()));
        assert_eq!(calls(&log), vec![(LOSS, "a".to_owned())]);
        assert_eq!(counted(key), before + 1);
    }

    /// Death: every owner of the binder leaves, buried ones too, silently; another binder's owner stays.
    #[test]
    fn death_removes_every_entry_of_the_binder_silently() {
        let log = log();
        let mut s: Stack<u32, ()> = Stack::default();
        s.request(req("a", 1, 10_001, GAIN_TRANSIENT, 0, &log), true, || {
            Some(())
        });
        s.request(req("b", 1, 10_001, GAIN_TRANSIENT, 0, &log), true, || {
            Some(())
        });
        s.request(req("c", 2, 10_002, GAIN_TRANSIENT, 0, &log), true, || {
            Some(())
        });
        let before = calls(&log);
        assert_eq!(s.die(&1).len(), 2);
        assert_eq!(s.len(), 1);
        assert_eq!(s.top(), Some("c"));
        assert_eq!(calls(&log), before, "a dead owner gets no callback");
    }

    /// The brief's sequence (MediaFocusControl.notifyTopOfAudioFocusStack, FocusRequester.handleFocusGain): A GAIN, B TRANSIENT, C MAY_DUCK; abandon C restores B; death of B restores A.
    /// A keeps -2 while B is on top; MAY_DUCK does not weaken it. B's -3 goes out as a callback (no attenuation exists).
    #[test]
    fn full_callback_sequence_restores_the_next_owner() {
        let _serial = serial();
        let log = log();
        let mut s: Stack<u32, ()> = Stack::default();
        s.request(req("a", 1, 10_001, GAIN, 0, &log), true, || Some(()));
        s.request(req("b", 2, 10_002, GAIN_TRANSIENT, 0, &log), true, || {
            Some(())
        });
        s.request(
            req("c", 3, 10_003, GAIN_TRANSIENT_MAY_DUCK, 0, &log),
            true,
            || Some(()),
        );
        assert_eq!(s.abandon("c").len(), 1);
        assert_eq!(s.top(), Some("b"));
        assert_eq!(s.die(&2).len(), 1);
        assert_eq!(s.top(), Some("a"));
        assert_eq!(
            calls(&log),
            vec![
                (LOSS_TRANSIENT, "a".to_owned()),
                (LOSS_TRANSIENT_CAN_DUCK, "b".to_owned()),
                (GAIN, "b".to_owned()),
                (GAIN, "a".to_owned()),
            ]
        );
    }

    /// Removing a buried owner (abandon or death) emits nothing: the top keeps focus and nobody is restored.
    #[test]
    fn buried_removal_emits_nothing() {
        let log = log();
        let mut s: Stack<u32, ()> = Stack::default();
        s.request(req("a", 1, 10_001, GAIN, 0, &log), true, || Some(()));
        s.request(req("b", 2, 10_002, GAIN_TRANSIENT, 0, &log), true, || {
            Some(())
        });
        s.request(req("c", 3, 10_003, GAIN_TRANSIENT, 0, &log), true, || {
            Some(())
        });
        let before = calls(&log);
        assert_eq!(s.abandon("b").len(), 1);
        assert_eq!(s.die(&1).len(), 1);
        assert_eq!(s.top(), Some("c"));
        assert_eq!(calls(&log), before, "buried removals send nothing");
    }

    /// A permanent gain evicts every loser (LOSS, then out of the stack); abandoning the GAIN restores nobody.
    #[test]
    fn permanent_gain_eviction_leaves_nothing_to_restore() {
        let _serial = serial();
        let log = log();
        let mut s: Stack<u32, ()> = Stack::default();
        s.request(req("a", 1, 10_001, GAIN, 0, &log), true, || Some(()));
        s.request(req("b", 2, 10_002, GAIN_TRANSIENT, 0, &log), true, || {
            Some(())
        });
        s.request(req("c", 3, 10_003, GAIN, 0, &log), true, || Some(()));
        assert_eq!(s.len(), 1);
        assert_eq!(s.abandon("c").len(), 1);
        assert!(s.is_empty(), "no evicted owner comes back");
        assert_eq!(
            calls(&log),
            vec![
                (LOSS_TRANSIENT, "a".to_owned()),
                (LOSS, "a".to_owned()),
                (LOSS, "b".to_owned()),
            ]
        );
    }

    /// Same UID, distinct client IDs compete on one stack: abandoning the second restores the first.
    #[test]
    fn same_uid_distinct_ids_restore_on_abandon() {
        let log = log();
        let mut s: Stack<u32, ()> = Stack::default();
        s.request(req("a", 1, 10_001, GAIN, 0, &log), true, || Some(()));
        s.request(req("b", 2, 10_001, GAIN_TRANSIENT, 0, &log), true, || {
            Some(())
        });
        assert_eq!(s.abandon("b").len(), 1);
        assert_eq!(s.top(), Some("a"));
        assert_eq!(
            calls(&log),
            vec![(LOSS_TRANSIENT, "a".to_owned()), (GAIN, "a".to_owned())]
        );
    }

    /// A restored owner holds no loss, so its next loser is notified again.
    #[test]
    fn restored_owner_is_notified_of_its_next_loss() {
        let log = log();
        let mut s: Stack<u32, ()> = Stack::default();
        s.request(req("a", 1, 10_001, GAIN, 0, &log), true, || Some(()));
        s.request(req("b", 2, 10_002, GAIN_TRANSIENT, 0, &log), true, || {
            Some(())
        });
        s.abandon("b");
        s.request(req("c", 3, 10_003, GAIN_TRANSIENT, 0, &log), true, || {
            Some(())
        });
        assert_eq!(
            calls(&log),
            vec![
                (LOSS_TRANSIENT, "a".to_owned()),
                (GAIN, "a".to_owned()),
                (LOSS_TRANSIENT, "a".to_owned()),
            ]
        );
    }

    /// One binder's death removes all of its owners and restores the next owner once, not once per owner.
    #[test]
    fn death_restores_the_next_owner_once() {
        let log = log();
        let mut s: Stack<u32, ()> = Stack::default();
        s.request(req("x", 2, 10_003, GAIN, 0, &log), true, || Some(()));
        s.request(req("a", 1, 10_001, GAIN_TRANSIENT, 0, &log), true, || {
            Some(())
        });
        s.request(req("b", 1, 10_001, GAIN_TRANSIENT, 0, &log), true, || {
            Some(())
        });
        assert_eq!(s.die(&1).len(), 2);
        assert_eq!(s.top(), Some("x"));
        assert_eq!(
            calls(&log),
            vec![
                (LOSS_TRANSIENT, "x".to_owned()),
                (LOSS_TRANSIENT, "a".to_owned()),
                (GAIN, "x".to_owned()),
            ]
        );
    }
}
