// SPDX-License-Identifier: MIT
//! WebChromeClient.onCreateWindow's transport state on the WebView's thread (a34): pure, so every terminal path is a unit case; jni.rs does the JNI and the engine calls the effects name.
//! AOSP WebChromeClient.java:132-196: true = the app will send resultMsg with a WebView set on its WebViewTransport (WebView.java:130-150), possibly turns later (no timer); false = no window
//! (a send after false is undefined: stale here). The target may be attached before the send; it must be a fresh WebView of this provider and thread that has loaded nothing.

use atl_platform_ui::webview::PopupToken;

/// Where the opener's one pending popup stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    /// PopupRequested delivered; onCreateWindow not yet returned.
    Offered,
    /// onCreateWindow returned true; the message has not come.
    Accepted,
    /// The message came with an eligible target; adoption queued, its answer not back.
    Completing,
}

/// A destination WebView's content, for eligibility.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContentState {
    /// Nothing loaded or run: may take a popup.
    Pristine,
    /// The app started content (load, evaluate, reload, history) on it.
    Started,
    /// Reserved by a queued adoption.
    AdoptionQueued,
    /// It shows an adopted popup.
    Adopted,
}

impl ContentState {
    /// The app starts content: anything but an adopted view becomes Started (a queued adoption's later failure then leaves it Started).
    pub fn start(self) -> Self {
        match self {
            Self::Adopted => Self::Adopted,
            _ => Self::Started,
        }
    }

    /// The display's adoption answer for a view this state reserved.
    pub fn adopted(self, ok: bool) -> Self {
        match (self, ok) {
            (Self::AdoptionQueued, true) => Self::Adopted,
            (Self::AdoptionQueued, false) => Self::Pristine,
            (s, _) => s,
        }
    }
}

/// What `transport.getWebView()` named at the send.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    /// No WebView set.
    Null,
    /// The opener itself.
    SelfView,
    /// A WebView whose provider is not this one's kind.
    OtherProvider,
    /// A WebView owned by another thread.
    WrongLooper,
    /// A destroyed WebView.
    Destroyed,
    /// A live WebView of this provider and thread, in this content state.
    Live(ContentState),
}

impl Target {
    /// Ok for a target that may take the popup, else the named refusal.
    pub fn eligible(self) -> Result<(), &'static str> {
        match self {
            Self::Null => Err("popup_target_null"),
            Self::SelfView => Err("popup_target_self"),
            Self::OtherProvider => Err("popup_target_other_provider"),
            Self::WrongLooper => Err("popup_target_wrong_looper"),
            Self::Destroyed => Err("popup_target_destroyed"),
            Self::Live(ContentState::Pristine) => Ok(()),
            Self::Live(ContentState::Started) => Err("popup_target_started"),
            Self::Live(ContentState::AdoptionQueued) => Err("popup_target_reserved"),
            Self::Live(ContentState::Adopted) => Err("popup_target_adopted"),
        }
    }
}

/// An engine call the state asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Effect {
    /// Nothing.
    None,
    /// `cancel_popup(token)`, for the named reason.
    Cancel(PopupToken, &'static str),
    /// `adopt_popup(token, target)`.
    Adopt(PopupToken),
}

/// The opener's popup slot (one at a time: the engine refuses an overlapping open).
#[derive(Debug, Default)]
pub struct PopupSlot {
    slot: Option<(PopupToken, Phase)>,
}

impl PopupSlot {
    /// PopupRequested: the slot takes `token` (a stale entry the engine already dropped is replaced).
    pub fn offer(&mut self, token: PopupToken) {
        self.slot = Some((token, Phase::Offered));
    }

    /// onCreateWindow ended: `accepted` = returned true without throwing (and the message could be made). False cancels at once.
    pub fn answered(&mut self, token: PopupToken, accepted: bool) -> Effect {
        match self.slot {
            Some((t, Phase::Offered)) if t == token => {
                if accepted {
                    self.slot = Some((t, Phase::Accepted));
                    Effect::None
                } else {
                    self.slot = None;
                    Effect::Cancel(t, "popup_declined")
                }
            }
            _ => Effect::None,
        }
    }

    /// The app's message for `token` arrived naming `target`. Only an accepted slot of that token acts; a stale or duplicate message does nothing (a newer token survives).
    pub fn message(&mut self, token: PopupToken, target: Target) -> Effect {
        match self.slot {
            Some((t, Phase::Accepted)) if t == token => match target.eligible() {
                Ok(()) => {
                    self.slot = Some((t, Phase::Completing));
                    Effect::Adopt(t)
                }
                Err(why) => {
                    self.slot = None;
                    Effect::Cancel(t, why)
                }
            },
            _ => Effect::None,
        }
    }

    /// The display answered the adoption of `token`.
    pub fn completed(&mut self, token: PopupToken) {
        if matches!(self.slot, Some((t, Phase::Completing)) if t == token) {
            self.slot = None;
        }
    }

    /// The engine dropped `token` before adoption (PopupCancelled): nothing more for it.
    pub fn engine_cancelled(&mut self, token: PopupToken) {
        if self.slot.is_some_and(|(t, _)| t == token) {
            self.slot = None;
        }
    }

    /// The opener is destroyed: the engine drops its pending popup with the view, so only the slot clears.
    pub fn destroyed(&mut self) {
        self.slot = None;
    }
}

/// An adopted popup's close reports: WebChromeClient.onCloseWindow once, never after destroy.
#[derive(Debug, Default)]
pub struct CloseState {
    closed: bool,
    destroyed: bool,
}

impl CloseState {
    /// A close report for a view in `content`: true = call onCloseWindow now.
    pub fn close(&mut self, content: ContentState) -> bool {
        if self.destroyed || content != ContentState::Adopted || self.closed {
            return false;
        }
        self.closed = true;
        true
    }

    /// The view was destroyed: later reports are dropped.
    pub fn destroy(&mut self) {
        self.destroyed = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tok(n: u64) -> PopupToken {
        PopupToken::of_raw(n).expect("nonzero")
    }

    /// The effects that are not None.
    fn effects(steps: impl IntoIterator<Item = Effect>) -> Vec<Effect> {
        steps.into_iter().filter(|e| *e != Effect::None).collect()
    }

    const OK: Target = Target::Live(ContentState::Pristine);

    #[test]
    fn popup_slot_terminal_effects() {
        // false, then a queued send: one cancel, no adopt
        let mut s = PopupSlot::default();
        s.offer(tok(1));
        let e = vec![s.answered(tok(1), false), s.message(tok(1), OK)];
        assert_eq!(effects(e), [Effect::Cancel(tok(1), "popup_declined")]);
        // accepted, two turns pass (nothing happens), then the send: one adopt
        let mut s = PopupSlot::default();
        s.offer(tok(2));
        assert_eq!(s.answered(tok(2), true), Effect::None);
        assert_eq!(
            s.answered(tok(2), true),
            Effect::None,
            "a later turn: no terminal"
        );
        assert_eq!(s.message(tok(2), OK), Effect::Adopt(tok(2)));
        // duplicate send, stale token: nothing more; the next token survives a late message of the old one
        assert_eq!(s.message(tok(2), OK), Effect::None);
        s.completed(tok(2));
        s.offer(tok(3));
        assert_eq!(s.answered(tok(3), true), Effect::None);
        assert_eq!(s.message(tok(2), OK), Effect::None, "stale message");
        s.completed(tok(2));
        assert_eq!(
            s.message(tok(3), OK),
            Effect::Adopt(tok(3)),
            "newer token survived"
        );
    }

    #[test]
    fn popup_target_eligibility() {
        let rejected = [
            (Target::Null, "popup_target_null"),
            (Target::SelfView, "popup_target_self"),
            (Target::OtherProvider, "popup_target_other_provider"),
            (Target::WrongLooper, "popup_target_wrong_looper"),
            (Target::Destroyed, "popup_target_destroyed"),
            (Target::Live(ContentState::Started), "popup_target_started"),
            (Target::Live(ContentState::Adopted), "popup_target_adopted"),
            (
                Target::Live(ContentState::AdoptionQueued),
                "popup_target_reserved",
            ),
        ];
        for (target, why) in rejected {
            let mut s = PopupSlot::default();
            s.offer(tok(7));
            s.answered(tok(7), true);
            assert_eq!(
                s.message(tok(7), target),
                Effect::Cancel(tok(7), why),
                "{target:?}"
            );
            assert_eq!(
                s.message(tok(7), OK),
                Effect::None,
                "{target:?}: one terminal"
            );
        }
        let mut s = PopupSlot::default();
        s.offer(tok(8));
        s.answered(tok(8), true);
        assert_eq!(s.message(tok(8), OK), Effect::Adopt(tok(8)));
        // the destination's state: reserved, then the display's answer; a failure restores pristine only if nothing started meanwhile
        let q = ContentState::AdoptionQueued;
        assert_eq!(q.adopted(true), ContentState::Adopted);
        assert_eq!(q.adopted(false), ContentState::Pristine);
        assert_eq!(q.start().adopted(false), ContentState::Started);
        assert_eq!(ContentState::Adopted.start(), ContentState::Adopted);
        assert_eq!(ContentState::Pristine.start(), ContentState::Started);
    }

    #[test]
    fn popup_teardown_and_close() {
        // opener destroyed while accepted: the slot clears, a late message does nothing
        let mut s = PopupSlot::default();
        s.offer(tok(1));
        s.answered(tok(1), true);
        s.destroyed();
        assert_eq!(s.message(tok(1), OK), Effect::None);
        // the child closed before adoption: the engine cancelled it; no message acts, and no close is reported (the view never was adopted)
        let mut s = PopupSlot::default();
        s.offer(tok(2));
        s.answered(tok(2), true);
        s.engine_cancelled(tok(2));
        assert_eq!(s.message(tok(2), OK), Effect::None);
        let mut c = CloseState::default();
        assert!(
            !c.close(ContentState::Pristine),
            "never adopted: no onCloseWindow"
        );
        // adopted: two close reports -> one callback
        let mut c = CloseState::default();
        assert!(c.close(ContentState::Adopted));
        assert!(!c.close(ContentState::Adopted));
        // destroy, then the engine's own close: no callback
        let mut c = CloseState::default();
        c.destroy();
        assert!(!c.close(ContentState::Adopted));
    }
}
