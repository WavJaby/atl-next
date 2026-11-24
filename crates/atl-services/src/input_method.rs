// SPDX-License-Identifier: MIT
//! The `input_method` service (Context.INPUT_METHOD_SERVICE, Context.java:5471): the converter's IInputMethodManager stub. InputMethodManager.createRealInstance throws without it (InputMethodManager.java:1638-1641; the lookup is IInputMethodManagerGlobalInvoker.java:96).
//! Each app process serves its own input method (atl-ime, IME plan route R1), found through the app's UI agent (INPUT_METHOD); IMMS-lite binds it as IMMS binds an IME, by the state machine in ime_bind.rs:
//! - addClient (InputMethodManager.java:1652, the startup call; InputMethodManagerService.java:1791-1805): the client is kept per app process (ui_agents.rs AppProcess: uid + binder calling pid), as
//!   AOSP keeps one ClientState per client binder with its uid and pid; its input method is that process's UI agent's.
//! - startInputOrWindowGainedFocusAsync (every start of input, a window focus gain's included, ViewRootImpl.handleWindowFocusChanged -> InputMethodManager.startInputInner:3560): ime_bind.rs; every call gets its onStartInputResult (ZeroJankProxy.java:466-476).
//! - startInputOrWindowGainedFocus (a window focus gain that starts no input, WINDOW_FOCUS_GAIN_REPORT_ONLY): null, as ZeroJankProxy; only our focus owner moves (focus.rs).
//! - showSoftInput / hideSoftInput (ImeTracker.Token decoded): false (the desktop input method shows its own UI; no soft keyboard window). getImeTrackerService: null (the client's no-tracker path).
//!   getInputMethodList / getEnabledInputMethodList: the empty InputMethodInfoSafeList (the in-app method is no installed InputMethodInfo).
//!
//! Every other method stays counted and answered UNKNOWN_TRANSACTION.

use std::collections::HashMap;
use std::io::Write;
use std::sync::{Mutex, MutexGuard};

use atl_aidl::aidl::com::android::internal::inputmethod::IInputMethod::IInputMethod;
use atl_aidl::aidl::com::android::internal::inputmethod::IInputMethodSessionCallback::{
    BnInputMethodSessionCallback, IInputMethodSessionCallback,
};
use atl_aidl::aidl::com::android::internal::view::IInputMethodManager::{
    BnInputMethodManager, EnforcePermission, IInputMethodManager,
};
use atl_aidl::mangled::{
    _3_com_7_android_8_internal_11_inputmethod_12_IInputMethod_10_InitParams as InitParams,
    _3_com_7_android_8_internal_11_inputmethod_12_IInputMethod_16_StartInputParams as StartInputParams,
    _3_com_7_android_8_internal_11_inputmethod_18_IInputMethodClient as IInputMethodClient,
    _3_com_7_android_8_internal_11_inputmethod_19_IInputMethodSession as IInputMethodSession,
    _3_com_7_android_8_internal_11_inputmethod_22_IRemoteInputConnection as IRemoteInputConnection,
};
use atl_parcelables::android::view::InputChannel;
use atl_parcelables::android::view::inputmethod::{EditorInfo, ImeTracker_Token, InputBinding};
use atl_parcelables::android::window::ImeOnBackInvokedDispatcher;
use atl_parcelables::com::android::internal::inputmethod::{
    InputBindResult, InputMethodInfoSafeList,
};
use binder::{
    BinderFeatures, DeathRecipient, ExceptionCode, IBinder, Interface, SpIBinder, Status, Strong,
    ThreadState,
};

use crate::focus::FocusedProcess;
use crate::ime_bind::{self, Bind, ClientBind, Peer};
use crate::packages::Permissions;
use crate::ui_agents::{AppProcess, UiAgents};
use atl_services_wire::ui_agent::IUiAgent;
use std::sync::Arc;

/// The interface name counters use.
const INTERFACE: &str = "IInputMethodManager";
/// StartInputFlags.INITIAL_CONNECTION (core/java/com/android/internal/inputmethod/StartInputFlags.java:51).
const INITIAL_CONNECTION: i32 = 1 << 2;

type Session = Strong<dyn IInputMethodSession>;

/// One startInput's arguments, kept for the session's attach (AOSP's mCurInputConnection / mCurEditorInfo / mCurImeDispatcher).
struct Start {
    connection: Option<Strong<dyn IRemoteInputConnection>>,
    editor: EditorInfo,
    ime_dispatcher: Option<ImeOnBackInvokedDispatcher>,
}

/// One app process's client (AOSP ClientState): its IInputMethodClient, its binding (the process's uid, pid), its input method once fetched and the bind state.
struct Client {
    client: Strong<dyn IInputMethodClient>,
    process: AppProcess,
    method: Option<Strong<dyn IInputMethod>>,
    bind: ClientBind<Session, Start>,
    /// The death link on `client`; held only so the link lives as long as the entry. None only under `--no-ime-death-link`.
    _death: Option<DeathRecipient>,
}

/// The service's state, shared with the session callbacks it hands out.
struct State {
    agents: Arc<UiAgents>,
    /// one client per process (InputMethodManager is a process singleton), keyed by its process
    clients: Mutex<HashMap<AppProcess, Client>>,
    /// Clients get a death link (false only for the ime gate's deliberate break, services `--no-ime-death-link`).
    death_link: bool,
    /// Every startInputOrWindowGainedFocus and ...Async names its caller the focus owner (focus.rs).
    focus: Arc<FocusedProcess>,
}

impl State {
    /// `client` of `process` died: its entry goes, unless the entry holds another client by now (the process added a new one, or a reused pid's process did; AOSP keys ClientState by the
    /// client binder). Counted and logged with what is left, for the ime gate's death sub-gate.
    fn client_died(&self, process: AppProcess, client: &SpIBinder) {
        let mut clients = self.clients();
        if !clients
            .get(&process)
            .is_some_and(|c| c.client.as_binder() == *client)
        {
            return;
        }
        clients.remove(&process);
        let left = clients.len();
        drop(clients);
        atl_diag::answered(INTERFACE, "client died");
        let AppProcess { uid, pid } = process;
        let _ = writeln!(
            std::io::stderr(),
            "services: input_method client uid={uid} pid={pid} died; entry removed, {left} clients left"
        );
    }
}

impl State {
    /// A focus gain of a window of `pid` that started no input: the pid owns the focus, the answer is null and no client's state changes.
    fn report_only_focus(&self, pid: i32) -> Option<InputBindResult> {
        self.focus.gained(pid);
        None
    }

    fn clients(&self) -> MutexGuard<'_, HashMap<AppProcess, Client>> {
        // a poisoned table still holds the clients
        self.clients.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// [`Peer`] over binder for one client: every call oneway (IInputMethod and IInputMethodClient are oneway interfaces); a refused one is counted.
struct Remote<'a> {
    state: &'a Arc<State>,
    client: &'a Strong<dyn IInputMethodClient>,
    process: AppProcess,
    method: &'a mut Option<Strong<dyn IInputMethod>>,
}

fn sent(call: &str, r: binder::Result<()>) {
    atl_diag::sent(INTERFACE, call);
    if let Err(e) = r {
        atl_diag::refused(INTERFACE, &format!("{call}: {e:?}"));
    }
}

fn bind_result(bind: Bind<Session>) -> InputBindResult {
    InputBindResult {
        result: bind.result,
        method: bind.session.map(|s| s.as_binder()),
        id: (bind.sequence >= 0).then(|| ime_bind::IME_ID.to_owned()),
        sequence: bind.sequence,
        is_input_method_suppressing_spell_checker: false,
    }
}

impl Peer for Remote<'_> {
    type Session = Session;
    type Start = Start;

    fn bind_method(&mut self) -> bool {
        let method = self
            .state
            .agents
            .agent(self.process)
            .and_then(|b| b.into_interface::<dyn IUiAgent>().ok())
            .and_then(|agent| agent.input_method().ok().flatten())
            .and_then(|b| b.into_interface::<dyn IInputMethod>().ok());
        let Some(method) = method else {
            atl_diag::answered(INTERFACE, "bind: the app serves no input method");
            return false;
        };
        sent(
            "IInputMethod.initializeInternal",
            method.r#initializeInternal(Some(&InitParams::default())),
        );
        *self.method = Some(method);
        true
    }

    fn create_session(&mut self) {
        let Some(method) = self.method.as_ref() else {
            return;
        };
        let callback = BnInputMethodSessionCallback::new_binder(
            SessionCallback {
                state: Arc::clone(self.state),
                process: self.process,
            },
            BinderFeatures::default(),
        );
        sent(
            "IInputMethod.createSession",
            method.r#createSession(Some(&InputChannel::default()), Some(&callback)),
        );
    }

    fn bind_input(&mut self) {
        if let Some(method) = self.method.as_ref() {
            let binding = InputBinding {
                connection_token: None,
                uid: i32::try_from(self.process.uid).unwrap_or(-1),
                pid: self.process.pid,
            };
            sent("IInputMethod.bindInput", method.r#bindInput(Some(&binding)));
        }
    }

    fn enable_session(&mut self, session: &Session) {
        if let Some(method) = self.method.as_ref() {
            sent(
                "IInputMethod.setSessionEnabled",
                method.r#setSessionEnabled(Some(session), true),
            );
        }
    }

    fn start_input(&mut self, start: &Start, restarting: bool) {
        if let Some(method) = self.method.as_ref() {
            let params = StartInputParams {
                startInputToken: None,
                remoteInputConnection: start.connection.clone(),
                editorInfo: Some(start.editor.clone()),
                restarting,
                navigationBarFlags: 0,
                imeDispatcher: start.ime_dispatcher.clone(),
            };
            sent(
                "IInputMethod.startInput",
                method.r#startInput(Some(&params)),
            );
        }
    }

    fn set_active(&mut self) {
        sent(
            "IInputMethodClient.setActive",
            self.client.r#setActive(true, false),
        );
    }

    fn start_input_result(&mut self, bind: Bind<Session>, start_input_seq: i32) {
        let res = bind_result(bind);
        sent(
            &format!(
                "IInputMethodClient.onStartInputResult result={}",
                res.result
            ),
            self.client
                .r#onStartInputResult(Some(&res), start_input_seq),
        );
    }

    fn bind_method_result(&mut self, bind: Bind<Session>) {
        let res = bind_result(bind);
        sent(
            "IInputMethodClient.onBindMethod",
            self.client.r#onBindMethod(Some(&res)),
        );
    }
}

/// Runs `f` on `process`'s client with its [`Remote`].
fn with_client(
    state: &Arc<State>,
    process: AppProcess,
    f: impl FnOnce(&mut ClientBind<Session, Start>, &mut Remote<'_>),
) {
    let mut clients = state.clients();
    let Some(c) = clients.get_mut(&process) else {
        atl_diag::answered(INTERFACE, "call for a client never added");
        return;
    };
    let Client {
        client,
        process,
        method,
        bind,
        ..
    } = c;
    let mut remote = Remote {
        state,
        client,
        process: *process,
        method,
    };
    f(bind, &mut remote);
}

/// IInputMethodSessionCallback for one client's createSession (InputMethodManagerService.requestClientSessionLocked's Stub -> onSessionCreated).
struct SessionCallback {
    state: Arc<State>,
    process: AppProcess,
}

impl Interface for SessionCallback {}

impl IInputMethodSessionCallback for SessionCallback {
    fn r#sessionCreated(&self, session: Option<&Session>) -> binder::Result<()> {
        let Some(session) = session.cloned() else {
            atl_diag::answered(INTERFACE, "sessionCreated with no session");
            return Ok(());
        };
        with_client(&self.state, self.process, |bind, remote| {
            bind.session_created(remote, session)
        });
        Ok(())
    }
}

struct InputMethodManager {
    permissions: Arc<Permissions>,
    state: Arc<State>,
}

impl Interface for InputMethodManager {}

impl IInputMethodManager for InputMethodManager {
    /// @EnforcePermission: root / system, or a caller whose install-time grants hold the permissions (Permissions::holds, rule A).
    fn r#enforce_permission(
        &self,
        method: &str,
        permission: EnforcePermission<'_>,
    ) -> binder::Result<()> {
        let (perms, any): (&[&str], bool) = match &permission {
            EnforcePermission::Single(p) => (std::slice::from_ref(p), false),
            EnforcePermission::AllOf(p) => (p, false),
            EnforcePermission::AnyOf(p) => (p, true),
        };
        if self
            .permissions
            .holds(ThreadState::get_calling_uid(), perms, any)
        {
            return Ok(());
        }
        Err(Status::new_exception_str(
            ExceptionCode::SECURITY,
            Some(format!("{method}: caller lacks {permission:?}")),
        ))
    }

    /// InputMethodManagerService.addClient (:1791-1805), as createRealInstance calls it (InputMethodManager.java:1652): the client is kept for its process (uid + calling pid) with a fresh bind
    /// state; a uid's other processes keep theirs. The client's binder death removes it (ClientController.addClient's death recipient -> removeClientAsBinder, ClientController.java:63-81, :108-113);
    /// a death of a replaced client leaves its successor alone (binder check). Counted.
    fn r#addClient(
        &self,
        client: Option<&Strong<dyn IInputMethodClient>>,
        _input_connection: Option<&Strong<dyn IRemoteInputConnection>>,
        _untrusted_display_id: i32,
    ) -> binder::Result<()> {
        atl_diag::answered(INTERFACE, "addClient");
        let Some(client) = client.cloned() else {
            return Ok(());
        };
        let process = AppProcess::calling();
        let death = if self.state.death_link {
            let weak = Arc::downgrade(&self.state);
            let dead = client.as_binder();
            let mut death = DeathRecipient::new(move || {
                if let Some(state) = weak.upgrade() {
                    state.client_died(process, &dead);
                }
            });
            client.as_binder().link_to_death(&mut death).map_err(|e| {
                Status::new_exception_str(
                    ExceptionCode::ILLEGAL_STATE,
                    Some(format!("linkToDeath: {e:?}")),
                )
            })?;
            Some(death)
        } else {
            None
        };
        let entry = Client {
            client,
            process,
            method: None,
            bind: ClientBind::default(),
            _death: death,
        };
        self.state.clients().insert(process, entry);
        Ok(())
    }

    /// startInputOrWindowGainedFocusAsync (ZeroJankProxy.java:228-250 + 466-476, use_zero_jank_proxy on in the pinned release configs): the bind state machine (ime_bind.rs) answers through
    /// client.onStartInputResult, every call. Counted per result.
    #[allow(clippy::too_many_arguments)]
    fn r#startInputOrWindowGainedFocusAsync(
        &self,
        _start_input_reason: i32,
        client: Option<&Strong<dyn IInputMethodClient>>,
        _window_token: Option<&SpIBinder>,
        start_input_flags: i32,
        _soft_input_mode: i32,
        _window_flags: i32,
        editor_info: Option<&EditorInfo>,
        input_connection: Option<&Strong<dyn IRemoteInputConnection>>,
        _remote_accessibility_input_connection: Option<
            &Strong<dyn atl_aidl::mangled::_3_com_7_android_8_internal_11_inputmethod_35_IRemoteAccessibilityInputConnection>,
        >,
        _unverified_target_sdk_version: i32,
        _user_id: i32,
        ime_dispatcher: Option<&ImeOnBackInvokedDispatcher>,
        _ime_requested_visible: bool,
        start_input_seq: i32,
        _use_async_show_hide_method: bool,
    ) -> binder::Result<()> {
        // a window of the caller has focus, editor or not, client or not
        self.state.focus.gained(ThreadState::get_calling_pid());
        let Some(client) = client else {
            atl_diag::answered(
                INTERFACE,
                "startInputOrWindowGainedFocusAsync with no client",
            );
            return Ok(());
        };
        let process = AppProcess::calling();
        let known = self
            .state
            .clients()
            .get(&process)
            .is_some_and(|c| c.client.as_binder() == client.as_binder());
        if !known {
            // a start before addClient (AOSP: the client is unknown and the call is refused); kept as added so the reply still goes out
            self.r#addClient(Some(client), None, 0)?;
        }
        let start = editor_info.map(|editor| Start {
            connection: input_connection.cloned(),
            editor: editor.clone(),
            ime_dispatcher: ime_dispatcher.cloned(),
        });
        let initial = start_input_flags & INITIAL_CONNECTION != 0;
        with_client(&self.state, process, |bind, remote| {
            bind.start_input(remote, start, initial, start_input_seq)
        });
        Ok(())
    }

    /// startInputOrWindowGainedFocus: AOSP answers null and changes no state (ZeroJankProxy.java:263-275, use_zero_jank_proxy on in the pinned release configs); the client still calls it for a report-only focus gain (InputMethodManager.java:958-977, result ignored).
    /// Ours, not AOSP: the caller becomes the focus owner (focus.rs). Counted.
    #[allow(clippy::too_many_arguments)]
    fn r#startInputOrWindowGainedFocus(
        &self,
        _start_input_reason: i32,
        _client: Option<&Strong<dyn IInputMethodClient>>,
        _window_token: Option<&SpIBinder>,
        _start_input_flags: i32,
        _soft_input_mode: i32,
        _window_flags: i32,
        _editor_info: Option<&EditorInfo>,
        _input_connection: Option<&Strong<dyn IRemoteInputConnection>>,
        _remote_accessibility_input_connection: Option<
            &Strong<dyn atl_aidl::mangled::_3_com_7_android_8_internal_11_inputmethod_35_IRemoteAccessibilityInputConnection>,
        >,
        _unverified_target_sdk_version: i32,
        _user_id: i32,
        _ime_dispatcher: Option<&ImeOnBackInvokedDispatcher>,
        _ime_requested_visible: bool,
    ) -> binder::Result<Option<InputBindResult>> {
        atl_diag::answered(
            INTERFACE,
            "startInputOrWindowGainedFocus (null: ZeroJankProxy)",
        );
        Ok(self.state.report_only_focus(ThreadState::get_calling_pid()))
    }

    /// showSoftInput: no IME window to show -> false (IMMS answers false when there is no current method).
    #[allow(clippy::too_many_arguments)]
    fn r#showSoftInput(
        &self,
        _client: Option<&Strong<dyn atl_aidl::mangled::_3_com_7_android_8_internal_11_inputmethod_18_IInputMethodClient>>,
        _window_token: Option<&SpIBinder>,
        _stats_token: Option<&ImeTracker_Token>,
        _flags: i32,
        _last_click_tool_type: i32,
        _result_receiver: Option<&atl_parcelables::android::os::ResultReceiver>,
        _reason: i32,
        _async: bool,
    ) -> binder::Result<bool> {
        atl_diag::answered(INTERFACE, "showSoftInput (false: no IME)");
        Ok(false)
    }

    /// hideSoftInput: nothing shown -> false.
    #[allow(clippy::too_many_arguments)]
    fn r#hideSoftInput(
        &self,
        _client: Option<&Strong<dyn atl_aidl::mangled::_3_com_7_android_8_internal_11_inputmethod_18_IInputMethodClient>>,
        _window_token: Option<&SpIBinder>,
        _stats_token: Option<&ImeTracker_Token>,
        _flags: i32,
        _result_receiver: Option<&atl_parcelables::android::os::ResultReceiver>,
        _reason: i32,
        _async: bool,
    ) -> binder::Result<bool> {
        atl_diag::answered(INTERFACE, "hideSoftInput (false: no IME)");
        Ok(false)
    }

    /// getImeTrackerService: null. AOSP's ImeTrackerService only logs show/hide requests for IMEs; the client's null path is the designed one (IInputMethodManagerGlobalInvoker.onStart -> ImeTracker.Token.empty, every other tracker call returns).
    /// The client caches only a non-null answer, so it asks on every request: counted.
    fn r#getImeTrackerService(
        &self,
    ) -> binder::Result<
        Option<
            Strong<
                dyn atl_aidl::mangled::_3_com_7_android_8_internal_11_inputmethod_11_IImeTracker,
            >,
        >,
    > {
        atl_diag::answered(INTERFACE, "getImeTrackerService (null: no IME tracker)");
        Ok(None)
    }

    /// getInputMethodList / getEnabledInputMethodList: no input methods, as InputMethodInfoSafeList.empty() (callers dereference it, so never null).
    fn r#getInputMethodList(
        &self,
        _user_id: i32,
        _direct_boot_awareness: i32,
    ) -> binder::Result<Option<InputMethodInfoSafeList>> {
        Ok(Some(InputMethodInfoSafeList))
    }

    fn r#getEnabledInputMethodList(
        &self,
        _user_id: i32,
    ) -> binder::Result<Option<InputMethodInfoSafeList>> {
        Ok(Some(InputMethodInfoSafeList))
    }
}

/// A new input method service binder, finding each app's input method through its UI agent in `agents`. `death_link` false leaves clients without a death link (test only: the gate's break).
pub fn new_binder(
    permissions: Arc<Permissions>,
    agents: Arc<UiAgents>,
    death_link: bool,
    focus: Arc<FocusedProcess>,
) -> SpIBinder {
    let state = Arc::new(State {
        agents,
        clients: Mutex::new(HashMap::new()),
        death_link,
        focus,
    });
    BnInputMethodManager::new_binder(
        InputMethodManager { permissions, state },
        BinderFeatures::default(),
    )
    .as_binder()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A report-only focus gain: null (ZeroJankProxy.java:263-275), the caller is the focus owner, no client is added. The calling pid is a parameter: reading it opens the binder driver, which a unit test has not.
    #[test]
    fn a_report_only_focus_gain_is_null_and_names_the_owner() {
        let focus = Arc::new(FocusedProcess::default());
        let state = State {
            agents: Arc::new(UiAgents::default()),
            clients: Mutex::new(HashMap::new()),
            death_link: true,
            focus: Arc::clone(&focus),
        };
        assert!(!focus.is(7));
        assert!(state.report_only_focus(7).is_none());
        assert!(focus.is(7));
        assert!(state.clients().is_empty());
    }
}
