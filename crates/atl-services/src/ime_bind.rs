// SPDX-License-Identifier: MIT
//! IMMS-lite's bind state machine (IME plan route R1, step 4): per app client, the in-app input method (the app's IUiAgent INPUT_METHOD) bound the way InputMethodManagerService binds an IME,
//! with no binder types, so the order of calls is unit-tested here; input_method.rs carries it over binder ([`Peer`]).
//! AOSP flow mirrored (services/core/java/com/android/server/inputmethod/InputMethodManagerService.java):
//! - startInput, client not yet current: prepareClientSwitchLocked -> client.setActive(true, false).
//! - no method bound: InputMethodBindingController.bindCurrentInputMethodService, replaced by the app's UI agent (the IME lives in the app process); initializeInternal; bind sequence + 1.
//! - no session: requestClientSessionLocked -> createSession once; reply SUCCESS_WAITING_IME_SESSION with id + sequence (startInputUncheckedLocked / tryReuseConnectionLocked).
//! - session created (onSessionCreated): attachNewInputLocked for the stored start, then client.onBindMethod(SUCCESS_WITH_IME_SESSION). The client's MSG_BIND starts input again (BOUND_TO_IMMS).
//! - startInput with a session: attachNewInputLocked -> reply SUCCESS_WITH_IME_SESSION.
//! - attachNewInputLocked: bindInput once per bound method (mBoundToMethod), setSessionEnabled(session, true) (setEnabledSessionLocked), startInput(start, restarting = !initial).
//!
//! Not modelled: several clients sharing one IME (each app process has its own), setInteractive (AOSP sends it only on a screen interactive change: nothing reports one yet), accessibility sessions, show/hide visibility state.

/// InputBindResult.ResultCode values sent (core/java/com/android/internal/inputmethod/InputBindResult.java:62-113).
pub mod result {
    pub const SUCCESS_WITH_IME_SESSION: i32 = 0;
    pub const SUCCESS_WAITING_IME_SESSION: i32 = 1;
    pub const ERROR_NO_IME: i32 = 6;
    pub const ERROR_NULL_EDITOR_INFO: i32 = 11;
}

/// The input method's id IMMS-lite reports (InputBindResult.id; the client needs it non-null to keep the bind: InputMethodManager MSG_START_INPUT_RESULT). A component-name form, as AOSP's ids are.
pub const IME_ID: &str = "atl/.InAppInputMethod";

/// A bind result for the client, before its binder form: `session` is the IInputMethodSession when one is attached.
#[derive(Debug, Clone, PartialEq)]
pub struct Bind<S> {
    pub result: i32,
    pub session: Option<S>,
    /// The bind sequence (InputMethodBindingController.getSequenceNumber); -1 with no IME, as InputBindResult.error.
    pub sequence: i32,
}

/// The calls the state machine makes: the client's IInputMethodClient and the app's IInputMethod. Every one is oneway in AOSP; a failed call is the implementation's to count.
pub trait Peer {
    type Session: Clone;
    type Start;
    /// Finds the client's input method (the app's UI agent) and sends initializeInternal; false when the app has none.
    fn bind_method(&mut self) -> bool;
    /// IInputMethod.createSession; the answer comes back as [`ClientBind::session_created`].
    fn create_session(&mut self);
    /// IInputMethod.bindInput with the client's binding.
    fn bind_input(&mut self);
    /// IInputMethod.setSessionEnabled(session, true).
    fn enable_session(&mut self, session: &Self::Session);
    /// IInputMethod.startInput for `start`.
    fn start_input(&mut self, start: &Self::Start, restarting: bool);
    /// IInputMethodClient.setActive(true, false).
    fn set_active(&mut self);
    /// IInputMethodClient.onStartInputResult.
    fn start_input_result(&mut self, bind: Bind<Self::Session>, start_input_seq: i32);
    /// IInputMethodClient.onBindMethod.
    fn bind_method_result(&mut self, bind: Bind<Self::Session>);
}

/// One client's bind state (AOSP's ClientState + the user's binding controller, for an app with its own IME).
#[derive(Debug)]
pub struct ClientBind<S, T> {
    active: bool,
    method_bound: bool,
    bound_to_method: bool,
    session_requested: bool,
    session: Option<S>,
    sequence: i32,
    /// The last start (window token, EditorInfo, connection, ...): attached when the session arrives.
    start: Option<(T, bool)>,
}

impl<S, T> Default for ClientBind<S, T> {
    fn default() -> Self {
        ClientBind {
            active: false,
            method_bound: false,
            bound_to_method: false,
            session_requested: false,
            session: None,
            sequence: 0,
            start: None,
        }
    }
}

impl<S: Clone, T> ClientBind<S, T> {
    /// startInputOrWindowGainedFocus(Async): `start` None = no EditorInfo (a focus gain without an editor); `initial` = StartInputFlags.INITIAL_CONNECTION; the reply goes out with `start_input_seq`.
    pub fn start_input<P: Peer<Session = S, Start = T>>(
        &mut self,
        peer: &mut P,
        start: Option<T>,
        initial: bool,
        start_input_seq: i32,
    ) {
        if !self.active {
            peer.set_active();
            self.active = true;
        }
        let Some(start) = start else {
            // startInputOrWindowGainedFocusInternalLocked: InputBindResult.NULL_EDITOR_INFO
            peer.start_input_result(
                Bind {
                    result: result::ERROR_NULL_EDITOR_INFO,
                    session: None,
                    sequence: -1,
                },
                start_input_seq,
            );
            return;
        };
        if !self.method_bound {
            if !peer.bind_method() {
                peer.start_input_result(
                    Bind {
                        result: result::ERROR_NO_IME,
                        session: None,
                        sequence: -1,
                    },
                    start_input_seq,
                );
                return;
            }
            self.method_bound = true;
            // InputMethodBindingController.advanceSequenceNumber: never 0 or negative
            self.sequence = self.sequence.checked_add(1).filter(|s| *s > 0).unwrap_or(1);
        }
        self.start = Some((start, initial));
        let bind = match self.session.clone() {
            Some(session) => self.attach(peer, session),
            None => {
                if !self.session_requested {
                    self.session_requested = true;
                    peer.create_session();
                }
                Bind {
                    result: result::SUCCESS_WAITING_IME_SESSION,
                    session: None,
                    sequence: self.sequence,
                }
            }
        };
        peer.start_input_result(bind, start_input_seq);
    }

    /// IInputMethodSessionCallback.sessionCreated (onSessionCreated): attach the pending start and tell the client (onBindMethod).
    pub fn session_created<P: Peer<Session = S, Start = T>>(&mut self, peer: &mut P, session: S) {
        self.session = Some(session.clone());
        if self.start.is_none() {
            return;
        }
        if let Some((_, initial)) = &mut self.start {
            // AOSP attaches with SESSION_CREATED_BY_IME, initial = true
            *initial = true;
        }
        let bind = self.attach(peer, session);
        peer.bind_method_result(bind);
    }

    fn attach<P: Peer<Session = S, Start = T>>(&mut self, peer: &mut P, session: S) -> Bind<S> {
        if !self.bound_to_method {
            peer.bind_input();
            self.bound_to_method = true;
        }
        peer.enable_session(&session);
        if let Some((start, initial)) = &self.start {
            peer.start_input(start, !initial);
        }
        Bind {
            result: result::SUCCESS_WITH_IME_SESSION,
            session: Some(session),
            sequence: self.sequence,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Records each call as a line, in order.
    #[derive(Default)]
    struct Fake {
        has_ime: bool,
        calls: Vec<String>,
    }

    impl Peer for Fake {
        type Session = &'static str;
        type Start = &'static str;
        fn bind_method(&mut self) -> bool {
            self.calls.push("initializeInternal".into());
            self.has_ime
        }
        fn create_session(&mut self) {
            self.calls.push("createSession".into());
        }
        fn bind_input(&mut self) {
            self.calls.push("bindInput".into());
        }
        fn enable_session(&mut self, session: &&'static str) {
            self.calls.push(format!("setSessionEnabled {session}"));
        }
        fn start_input(&mut self, start: &&'static str, restarting: bool) {
            self.calls
                .push(format!("startInput {start} restarting={restarting}"));
        }
        fn set_active(&mut self) {
            self.calls.push("setActive".into());
        }
        fn start_input_result(&mut self, bind: Bind<&'static str>, seq: i32) {
            self.calls.push(format!(
                "onStartInputResult result={} session={:?} sequence={} seq={seq}",
                bind.result, bind.session, bind.sequence
            ));
        }
        fn bind_method_result(&mut self, bind: Bind<&'static str>) {
            self.calls.push(format!(
                "onBindMethod result={} session={:?} sequence={}",
                bind.result, bind.session, bind.sequence
            ));
        }
    }

    /// The AOSP order for a first focus: waiting reply, then (session created) bind + attach + onBindMethod, then the client's BOUND_TO_IMMS restart attaches again.
    /// Expected lines follow InputMethodManagerService: result codes from InputBindResult.java (0 with session, 1 waiting), sequence 1 = the first advanceSequenceNumber.
    #[test]
    fn first_focus_binds_then_attaches() {
        let mut peer = Fake {
            has_ime: true,
            ..Fake::default()
        };
        let mut bind = ClientBind::default();
        bind.start_input(&mut peer, Some("edit"), true, 7);
        bind.session_created(&mut peer, "S");
        bind.start_input(&mut peer, Some("edit"), false, 8);
        assert_eq!(
            peer.calls,
            [
                "setActive",
                "initializeInternal",
                "createSession",
                "onStartInputResult result=1 session=None sequence=1 seq=7",
                "bindInput",
                "setSessionEnabled S",
                "startInput edit restarting=false",
                "onBindMethod result=0 session=Some(\"S\") sequence=1",
                "setSessionEnabled S",
                "startInput edit restarting=true",
                "onStartInputResult result=0 session=Some(\"S\") sequence=1 seq=8",
            ]
        );
    }

    /// Every startInput is answered, also when the app serves no input method (ERROR_NO_IME = 6, sequence -1 as InputBindResult.error).
    #[test]
    fn no_ime_still_replies() {
        let mut peer = Fake::default();
        let mut bind = ClientBind::<&str, &str>::default();
        bind.start_input(&mut peer, Some("edit"), true, 3);
        assert_eq!(
            peer.calls,
            [
                "setActive",
                "initializeInternal",
                "onStartInputResult result=6 session=None sequence=-1 seq=3",
            ]
        );
    }

    /// A focus gain without an editor binds nothing and is answered NULL_EDITOR_INFO (11, InputBindResult.java:147).
    #[test]
    fn no_editor_replies_null_editor_info() {
        let mut peer = Fake {
            has_ime: true,
            ..Fake::default()
        };
        let mut bind = ClientBind::<&str, &str>::default();
        bind.start_input(&mut peer, None, true, 4);
        assert_eq!(
            peer.calls,
            [
                "setActive",
                "onStartInputResult result=11 session=None sequence=-1 seq=4",
            ]
        );
    }

    /// A second focus before the session exists asks for no second session; a session with no start pending is only kept.
    #[test]
    fn one_session_request() {
        let mut peer = Fake {
            has_ime: true,
            ..Fake::default()
        };
        let mut bind = ClientBind::default();
        bind.start_input(&mut peer, Some("a"), true, 1);
        bind.start_input(&mut peer, Some("b"), true, 2);
        let creates = peer.calls.iter().filter(|c| *c == "createSession").count();
        assert_eq!(creates, 1);
        let mut idle = ClientBind::<&str, &str>::default();
        idle.session_created(&mut peer, "S");
        assert_eq!(
            peer.calls.last().map(String::as_str),
            Some("onStartInputResult result=1 session=None sequence=1 seq=2")
        );
    }
}
