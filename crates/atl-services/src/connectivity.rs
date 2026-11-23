// SPDX-License-Identifier: MIT
//! The `connectivity` service (Context.CONNECTIVITY_SERVICE): ConnectivityFrameworkInitializer.registerServiceWrappers builds ConnectivityManager only from this binder; without it
//! getSystemService returns null and apps that never null-check it throw (Cut the Rope's ZSystemInfo.isNetworkAvailable from a native call).
//! The host's default network is the one network agent while it exists. Its netId: NetIdManager.reserveNetId hands out 100, 101, ... (NetIdManager.java:82-89), so a regained network is a new one to the app.
//! No default route -> no agent: the answers of a device with none (null network, metered, no callback).
//! Changes: the host's watch (rtnetlink, atl-platform-host) says something changed; the services ask atld for the host's network over the control connection ([`HostNetwork`]). They never read GIO
//! themselves: inside the sandbox GIO reaches no NetworkManager (DefaultNetwork doc).
//! Transitions (ConnectivityService, pinned Connectivity module): TRACK_DEFAULT callbacks AVAILABLE / LOST / CAP_CHANGED / IP_CHANGED, and the sticky CONNECTIVITY_ACTION (sendConnectedBroadcast,
//! sendLegacyNetworkBroadcast DISCONNECTED) with maxManifestReceiverApiLevel M.
//! Network callbacks: requestNetwork of type TRACK_DEFAULT (ConnectivityManager.registerDefaultNetworkCallback) answers its NetworkRequest and, with an agent, sends CALLBACK_AVAILABLE to the caller's Messenger
//! as ConnectivityService.callCallbackForRequest builds it. Not modelled (counted or silent): other request types, the client's death, onLosing (linger) on a default-network switch,
//! FLAG_RECEIVER_REGISTERED_ONLY_BEFORE_BOOT (S9), EXTRA_INET_CONDITION, delivery groups.
//! Known divergence: getNetworkInfo(type) for a type without an agent answers null where AOSP makes a fake DISCONNECTED NetworkInfo (getFilteredNetworkInfoForType -> makeFakeNetworkInfo).

use std::io::{self, Write};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use atl_aidl::aidl::android::net::IConnectivityManager::{
    BnConnectivityManager, IConnectivityManager,
};
use atl_aidl::aidl::android::os::IMessenger::IMessenger;
use atl_parcelables::android::net::{
    LinkProperties, Network, NetworkCapabilities, NetworkInfo, NetworkRequest, ProxyInfo,
};
use atl_parcelables::android::os::{Bundle, Message, Messenger};
use atl_parcelables::codecs::android::content::Intent::Intent;
use atl_parcelables::codecs::android::net::LinkProperties::{RTN_UNICAST, RouteInfo};
use atl_parcelables::codecs::android::net::NetworkCapabilities::*;
use atl_parcelables::codecs::android::os::Bundle::Value;
use atl_platform_host::{Connectivity, DefaultNetwork, LinkKind, NetworkSource, PlatformNetwork};
use binder::{
    BinderFeatures, ExceptionCode, FromIBinder, Interface, SpIBinder, Status, ThreadState,
};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use crate::packages::Permissions;

/// ConnectivityService.enforceAccessPermission: every query below needs it.
const ACCESS_NETWORK_STATE: &str = "android.permission.ACCESS_NETWORK_STATE";
/// NetIdManager.MIN_NET_ID (NetIdManager.java:43): the first netId ConnectivityService gives an agent.
const MIN_NET_ID: i32 = 100;
/// ConnectivityManager.TYPE_WIFI / TYPE_ETHERNET, the legacy types the Wi-Fi and Ethernet agents declare (ClientModeImpl, EthernetNetworkFactory).
const TYPE_WIFI: i32 = 1;
const TYPE_ETHERNET: i32 = 9;
/// NetworkRequest.Type ordinals (NetworkRequest.java:235-244) and names.
const TRACK_DEFAULT: i32 = 2;
const TRACK_DEFAULT_NAME: &str = "TRACK_DEFAULT";
/// ConnectivityManager.CALLBACK_* (ConnectivityManager.java:4504-4516).
const CALLBACK_AVAILABLE: i32 = 2;
const CALLBACK_LOST: i32 = 4;
const CALLBACK_CAP_CHANGED: i32 = 6;
const CALLBACK_IP_CHANGED: i32 = 7;
/// ConnectivityManager.CONNECTIVITY_ACTION (:172) and its extras: EXTRA_NETWORK_INFO (:213), EXTRA_NETWORK_TYPE (:224), EXTRA_NO_CONNECTIVITY (:250).
const CONNECTIVITY_ACTION: &str = "android.net.conn.CONNECTIVITY_CHANGE";
const EXTRA_NETWORK_INFO: &str = "networkInfo";
const EXTRA_NETWORK_TYPE: &str = "networkType";
const EXTRA_NO_CONNECTIVITY: &str = "noConnectivity";
/// ConnectivityService.sendStickyBroadcast: BroadcastOptions.setMaxManifestReceiverApiLevel(Build.VERSION_CODES.M) for CONNECTIVITY_ACTION (ConnectivityService.java:4071-4075).
pub const CONNECTIVITY_MAX_MANIFEST_API: i32 = 23;
/// The diagnostics interface name (`atl_diag::answered`).
const CM: &str = "IConnectivityManager";

/// Sends `intent` as a sticky broadcast to every user as the system, with maxManifestReceiverApiLevel `Some(level)` (None = no limit): the activity manager's dispatch.
pub type StickyBroadcast = Arc<dyn Fn(&Intent, Option<i32>) + Send + Sync>;
/// The host's default network as `DefaultNetwork::to_arg` writes it (atld's answer to the control request `Network`); None = no default route.
pub type HostNetwork = Arc<dyn Fn() -> io::Result<Option<String>> + Send + Sync>;

/// The one agent: the host's default network as the app sees it.
#[derive(Clone)]
struct Agent {
    net_id: i32,
    /// The host's network it was built from: the transitions compare against it.
    source: DefaultNetwork,
    info: NetworkInfo,
    capabilities: NetworkCapabilities,
    link: LinkProperties,
    metered: bool,
}

impl Agent {
    /// The agent of `n` under netId `net_id`. Capabilities: those an Ethernet / Wi-Fi agent declares (INTERNET, NOT_ROAMING, NOT_CONGESTED, NOT_SUSPENDED; NOT_METERED unless metered) on top of the constructor's defaults,
    /// plus what ConnectivityService mixes in for a default network (FOREGROUND, NOT_VCN_MANAGED) and its validation result: VALIDATED for full connectivity, CAPTIVE_PORTAL behind a portal, neither when limited.
    /// Owner, administrator and requestor fields stay unset: networkCapabilitiesRestrictedForCallerPermissions clears them for an unprivileged caller.
    fn of(n: &DefaultNetwork, net_id: i32) -> Agent {
        let mut nc = NetworkCapabilities::new();
        nc.capabilities |= bits(&[
            NET_CAPABILITY_INTERNET,
            NET_CAPABILITY_NOT_ROAMING,
            NET_CAPABILITY_FOREGROUND,
            NET_CAPABILITY_NOT_CONGESTED,
            NET_CAPABILITY_NOT_SUSPENDED,
            NET_CAPABILITY_NOT_VCN_MANAGED,
        ]);
        if !n.metered {
            nc.capabilities |= bits(&[NET_CAPABILITY_NOT_METERED]);
        }
        nc.capabilities |= match n.connectivity {
            Connectivity::Full => bits(&[NET_CAPABILITY_VALIDATED]),
            Connectivity::Portal => bits(&[NET_CAPABILITY_CAPTIVE_PORTAL]),
            Connectivity::Limited => 0,
        };
        let (transport, network_type, type_name, extra_info) = match n.kind {
            LinkKind::Wifi => (TRANSPORT_WIFI, TYPE_WIFI, "WIFI", None),
            // EthernetNetworkFactory: legacy extra info = the interface's hardware address
            LinkKind::Ethernet => (
                TRANSPORT_ETHERNET,
                TYPE_ETHERNET,
                "Ethernet",
                n.hw_address.clone(),
            ),
        };
        nc.transports = bits(&[transport]);
        let routes = n
            .gateways
            .iter()
            .map(|gw| RouteInfo {
                destination: match gw {
                    IpAddr::V4(_) => (IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0),
                    IpAddr::V6(_) => (IpAddr::V6(Ipv6Addr::UNSPECIFIED), 0),
                },
                gateway: Some(*gw),
                interface: Some(n.interface.clone()),
                route_type: RTN_UNICAST,
                mtu: 0,
            })
            .collect();
        Agent {
            net_id,
            source: n.clone(),
            // NetworkAgent.markConnected: CONNECTED with no reason; NetworkAgentConfig's legacy subtype name defaults to ""
            info: NetworkInfo {
                network_type,
                subtype: 0,
                type_name: Some(type_name.to_owned()),
                subtype_name: Some(String::new()),
                state: "CONNECTED".to_owned(),
                detailed_state: "CONNECTED".to_owned(),
                is_failover: false,
                is_available: true,
                is_roaming: false,
                reason: None,
                extra_info,
            },
            capabilities: nc,
            link: LinkProperties {
                interface_name: Some(n.interface.clone()),
                dnses: n.dns.clone(),
                domains: n.search.clone(),
                mtu: n.mtu.and_then(|m| i32::try_from(m).ok()).unwrap_or(0),
                routes,
                ..LinkProperties::default()
            },
            metered: n.metered,
        }
    }
}

/// What the transitions read and write under one lock.
struct State {
    agent: Option<Agent>,
    /// NetIdManager.mLastNetId: the last netId reserved (MIN_NET_ID - 1 before the first).
    last_net_id: i32,
}

/// The service's state, shared by the binder and the watch thread.
struct Shared {
    permissions: Arc<Permissions>,
    state: Mutex<State>,
    /// ConnectivityService.mNextNetworkRequestId.
    next_request_id: AtomicI32,
    /// The registered callbacks: each request with the Messenger its callbacks go to.
    requests: Mutex<Vec<(NetworkRequest, SpIBinder)>>,
    sticky: StickyBroadcast,
}

fn locked<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Logs to the services' stderr with the `connectivity:` prefix.
fn note(msg: std::fmt::Arguments<'_>) {
    let _ = writeln!(std::io::stderr(), "connectivity: {msg}");
}

/// The entries of CALLBACK_AVAILABLE (callCallbackForRequest): capabilities, link properties, and a null LocalNetworkInfo (no local network).
fn available_entries(a: &Agent) -> binder::Result<Vec<(String, Value)>> {
    let mut entries = cap_entries(&a.capabilities)?;
    entries.extend(link_entries(&a.link)?);
    entries.push(("LocalNetworkInfo".to_owned(), Value::Null));
    Ok(entries)
}

/// The NetworkCapabilities entry of CALLBACK_AVAILABLE and CALLBACK_CAP_CHANGED.
fn cap_entries(c: &NetworkCapabilities) -> binder::Result<Vec<(String, Value)>> {
    Ok(vec![(
        "NetworkCapabilities".to_owned(),
        Value::parcelable("android.net.NetworkCapabilities", c)?,
    )])
}

/// The LinkProperties entry of CALLBACK_AVAILABLE and CALLBACK_IP_CHANGED.
fn link_entries(l: &LinkProperties) -> binder::Result<Vec<(String, Value)>> {
    Ok(vec![(
        "LinkProperties".to_owned(),
        Value::parcelable("android.net.LinkProperties", l)?,
    )])
}

/// callCallbackForRequest: the common Bundle (NetworkRequest, Network) plus `extra`, sent to the request's Messenger as `code` (arg1 = blocked reasons: none).
fn send_callback(
    request: &NetworkRequest,
    messenger: &SpIBinder,
    code: i32,
    net_id: i32,
    extra: binder::Result<Vec<(String, Value)>>,
) {
    let bundle = (|| -> binder::Result<Bundle> {
        let mut entries = vec![
            (
                "NetworkRequest".to_owned(),
                Value::parcelable("android.net.NetworkRequest", request)?,
            ),
            (
                "Network".to_owned(),
                Value::parcelable("android.net.Network", &Network { net_id })?,
            ),
        ];
        entries.extend(extra?);
        Ok(Bundle::from_entries(entries))
    })();
    let sent = bundle.and_then(|b| {
        let client = <dyn IMessenger as FromIBinder>::try_from(messenger.clone())?;
        client.send(Some(&Message::new(code, 0, b)))
    });
    // a dead client is ConnectivityService's "may occur naturally in the race of binder death": logged, not thrown
    if let Err(e) = sent {
        note(format_args!(
            "callback {code} for request {} not sent: {e}",
            request.request_id
        ));
    }
}

/// The CONNECTIVITY_ACTION intent (ConnectivityService.makeGeneralIntent, sendLegacyNetworkBroadcast): EXTRA_NETWORK_INFO, EXTRA_NETWORK_TYPE, and EXTRA_NO_CONNECTIVITY when the default network went away.
fn connectivity_action(info: &NetworkInfo, no_connectivity: bool) -> binder::Result<Intent> {
    let mut entries = vec![
        (
            EXTRA_NETWORK_INFO.to_owned(),
            Value::parcelable("android.net.NetworkInfo", info)?,
        ),
        (EXTRA_NETWORK_TYPE.to_owned(), Value::Int(info.network_type)),
    ];
    if no_connectivity {
        entries.push((EXTRA_NO_CONNECTIVITY.to_owned(), Value::Bool(true)));
    }
    Ok(Intent {
        action: Some(CONNECTIVITY_ACTION.to_owned()),
        extras: Some(Bundle::from_entries(entries)),
        ..Intent::default()
    })
}

impl Shared {
    fn agent(&self) -> Option<Agent> {
        locked(&self.state).agent.clone()
    }

    fn sticky_send(&self, intent: binder::Result<Intent>) {
        // AOSP's U+ delivery group policy (applyMostRecentPolicyForConnectivityAction, ConnectivityService.java:4095-4103) and EXTRA_INET_CONDITION are not sent: counted
        atl_diag::answered(CM, "delivery group policy not modelled");
        atl_diag::answered(CM, "EXTRA_INET_CONDITION not sent");
        match intent {
            Ok(intent) => (self.sticky)(&intent, Some(CONNECTIVITY_MAX_MANIFEST_API)),
            Err(e) => note(format_args!("CONNECTIVITY_ACTION not built: {e}")),
        }
    }

    /// Asks the host for its network now and applies it. A refused or unreadable answer keeps the current agent.
    fn refresh(&self, host: &HostNetwork) {
        match host() {
            Ok(arg) => match arg.as_deref().map(DefaultNetwork::parse_arg).transpose() {
                Ok(next) => self.apply(next),
                Err(e) => note(format_args!("host network answer unreadable: {e}")),
            },
            Err(e) => note(format_args!("host network request failed: {e}")),
        }
    }

    /// ConnectivityService's transitions for the host's network becoming `next` (module doc). The same network changing its link or capabilities keeps its netId.
    fn apply(&self, next: Option<DefaultNetwork>) {
        let (old, new, requests) = {
            let mut st = locked(&self.state);
            let old = st.agent.clone();
            let new = match (&old, next) {
                (Some(o), Some(n)) if o.source == n => return,
                (Some(o), Some(n))
                    if o.source.interface == n.interface && o.source.kind == n.kind =>
                {
                    Some(Agent::of(&n, o.net_id))
                }
                (_, Some(n)) => {
                    st.last_net_id += 1;
                    Some(Agent::of(&n, st.last_net_id))
                }
                (_, None) => None,
            };
            st.agent = new.clone();
            let requests = locked(&self.requests).clone();
            (old, new, requests)
        };
        match (old, new) {
            (None, None) => {}
            (None, Some(n)) => {
                for (r, m) in &requests {
                    send_callback(r, m, CALLBACK_AVAILABLE, n.net_id, available_entries(&n));
                }
                self.sticky_send(connectivity_action(&n.info, false));
            }
            (Some(o), None) => {
                for (r, m) in &requests {
                    send_callback(r, m, CALLBACK_LOST, o.net_id, Ok(Vec::new()));
                }
                let mut info = o.info.clone();
                info.state = "DISCONNECTED".to_owned();
                info.detailed_state = "DISCONNECTED".to_owned();
                self.sticky_send(connectivity_action(&info, true));
            }
            (Some(o), Some(n)) if o.net_id != n.net_id => {
                for (r, m) in &requests {
                    send_callback(r, m, CALLBACK_AVAILABLE, n.net_id, available_entries(&n));
                }
                for (r, m) in &requests {
                    send_callback(r, m, CALLBACK_LOST, o.net_id, Ok(Vec::new()));
                }
                atl_diag::answered(CM, "default network switch: onLosing not modelled");
                self.sticky_send(connectivity_action(&n.info, false));
            }
            (Some(o), Some(n)) => {
                if o.source.connectivity != n.source.connectivity
                    || o.source.metered != n.source.metered
                {
                    for (r, m) in &requests {
                        send_callback(
                            r,
                            m,
                            CALLBACK_CAP_CHANGED,
                            n.net_id,
                            cap_entries(&n.capabilities),
                        );
                    }
                }
                if o.source.gateways != n.source.gateways
                    || o.source.dns != n.source.dns
                    || o.source.search != n.source.search
                    || o.source.mtu != n.source.mtu
                {
                    for (r, m) in &requests {
                        send_callback(r, m, CALLBACK_IP_CHANGED, n.net_id, link_entries(&n.link));
                    }
                }
            }
        }
    }
}

struct ConnectivityService {
    shared: Arc<Shared>,
}

impl ConnectivityService {
    /// ConnectivityService.enforceAccessPermission (enforceCallingOrSelfPermission -> SecurityException).
    fn enforce_access(&self, method: &str) -> binder::Result<()> {
        if self.shared.permissions.holds(
            ThreadState::get_calling_uid(),
            &[ACCESS_NETWORK_STATE],
            false,
        ) {
            return Ok(());
        }
        Err(Status::new_exception_str(
            ExceptionCode::SECURITY,
            Some(format!("{method}: caller lacks {ACCESS_NETWORK_STATE}")),
        ))
    }

    fn network(&self) -> Option<Network> {
        self.shared.agent().map(|a| Network { net_id: a.net_id })
    }

    /// The agent `n` names, if it is the current one.
    fn agent_of(&self, n: Option<&Network>) -> Option<Agent> {
        let n = n?;
        self.shared.agent().filter(|a| a.net_id == n.net_id)
    }
}

impl Interface for ConnectivityService {}

impl IConnectivityManager for ConnectivityService {
    /// getActiveNetworkInfo -> getFilteredNetworkInfo of the default network: its NetworkInfo (nothing blocks it), null without one.
    fn r#getActiveNetworkInfo(&self) -> binder::Result<Option<NetworkInfo>> {
        self.enforce_access("getActiveNetworkInfo")?;
        Ok(self.shared.agent().map(|a| a.info))
    }

    fn r#getActiveNetworkInfoForUid(
        &self,
        _uid: i32,
        _ignore_blocked: bool,
    ) -> binder::Result<Option<NetworkInfo>> {
        self.enforce_access("getActiveNetworkInfoForUid")?;
        Ok(self.shared.agent().map(|a| a.info))
    }

    /// getNetworkInfo(type): the agent's NetworkInfo when its legacy type is `network_type` (see the module doc's divergence otherwise).
    fn r#getNetworkInfo(&self, network_type: i32) -> binder::Result<Option<NetworkInfo>> {
        self.enforce_access("getNetworkInfo")?;
        Ok(self
            .shared
            .agent()
            .filter(|a| a.info.network_type == network_type)
            .map(|a| a.info))
    }

    /// getActiveNetwork -> getActiveNetworkForUidInternal: the default network, null without one.
    fn r#getActiveNetwork(&self) -> binder::Result<Option<Network>> {
        self.enforce_access("getActiveNetwork")?;
        Ok(self.network())
    }

    fn r#getActiveNetworkForUid(
        &self,
        _uid: i32,
        _ignore_blocked: bool,
    ) -> binder::Result<Option<Network>> {
        self.enforce_access("getActiveNetworkForUid")?;
        Ok(self.network())
    }

    fn r#getNetworkForType(&self, network_type: i32) -> binder::Result<Option<Network>> {
        self.enforce_access("getNetworkForType")?;
        Ok(self
            .shared
            .agent()
            .filter(|a| a.info.network_type == network_type)
            .map(|a| Network { net_id: a.net_id }))
    }

    /// getAllNetworks: one entry per agent.
    fn r#getAllNetworks(&self) -> binder::Result<Option<Vec<Option<Network>>>> {
        self.enforce_access("getAllNetworks")?;
        Ok(Some(self.network().into_iter().map(Some).collect()))
    }

    /// getNetworkCapabilities: the agent's, restricted for the caller (see Agent::of); null for a network that is not ours.
    fn r#getNetworkCapabilities(
        &self,
        network: Option<&Network>,
        _package: Option<&str>,
        _tag: Option<&str>,
    ) -> binder::Result<Option<NetworkCapabilities>> {
        self.enforce_access("getNetworkCapabilities")?;
        Ok(self.agent_of(network).map(|a| a.capabilities))
    }

    fn r#getLinkProperties(
        &self,
        network: Option<&Network>,
    ) -> binder::Result<Option<LinkProperties>> {
        self.enforce_access("getLinkProperties")?;
        Ok(self.agent_of(network).map(|a| a.link))
    }

    fn r#getActiveLinkProperties(&self) -> binder::Result<Option<LinkProperties>> {
        self.enforce_access("getActiveLinkProperties")?;
        Ok(self.shared.agent().map(|a| a.link))
    }

    /// getProxyForNetwork: no global proxy, no PAC, no per-network proxy -> null (ProxyTracker.getGlobalProxy null, LinkProperties.getHttpProxy null).
    fn r#getProxyForNetwork(
        &self,
        _network: Option<&Network>,
    ) -> binder::Result<Option<ProxyInfo>> {
        Ok(None)
    }

    /// isActiveNetworkMetered: the agent's metered state; no active network -> true ("the most conservative value").
    fn r#isActiveNetworkMetered(&self) -> binder::Result<bool> {
        self.enforce_access("isActiveNetworkMetered")?;
        Ok(self.shared.agent().is_none_or(|a| a.metered))
    }

    /// requestNetwork, TRACK_DEFAULT only (registerDefaultNetworkCallback): copyDefaultNetworkCapabilitiesForUid's capabilities (the default request's: INTERNET, NOT_VCN_MANAGED, constructor defaults without NOT_VPN)
    /// for the caller's uid, with the requestor set (restrictRequestUidsForCallerAndSetRequestorInfo); then CALLBACK_AVAILABLE when there is a default network.
    fn r#requestNetwork(
        &self,
        _as_uid: i32,
        _capabilities: Option<&NetworkCapabilities>,
        req_type: i32,
        messenger: Option<&Messenger>,
        _timeout: i32,
        _binder: Option<&SpIBinder>,
        legacy: i32,
        _callback_flags: i32,
        package: Option<&str>,
        _tag: Option<&str>,
        _declared_methods: i32,
    ) -> binder::Result<Option<NetworkRequest>> {
        if req_type != TRACK_DEFAULT {
            // an unimplemented sub-case: the reply hook counts requestNetwork `unimplemented` with this cause
            atl_diag::set_cause("type other than TRACK_DEFAULT");
            return Err(binder::StatusCode::UNKNOWN_TRANSACTION.into());
        }
        self.enforce_access("requestNetwork")?;
        let Some(messenger) = messenger else {
            return Err(Status::new_exception_str(
                ExceptionCode::ILLEGAL_ARGUMENT,
                Some("requestNetwork: null Messenger"),
            ));
        };
        let uid = ThreadState::get_calling_uid() as i32;
        let mut nc = NetworkCapabilities::new();
        nc.capabilities |= bits(&[NET_CAPABILITY_INTERNET, NET_CAPABILITY_NOT_VCN_MANAGED]);
        nc.capabilities &= !bits(&[NET_CAPABILITY_NOT_VPN]);
        nc.uids = Some(vec![UidRange {
            start: uid,
            stop: uid,
        }]);
        nc.requestor_uid = uid;
        nc.requestor_package_name = package.map(str::to_owned);
        let request = NetworkRequest {
            capabilities: nc,
            legacy_type: legacy,
            request_id: self.shared.next_request_id.fetch_add(1, Ordering::Relaxed),
            request_type: TRACK_DEFAULT_NAME.to_owned(),
        };
        locked(&self.shared.requests).push((request.clone(), messenger.target.clone()));
        // oneway: ConnectivityManager holds sCallbacks while this call runs, so its handler finds the request registered when the message arrives
        if let Some(agent) = self.shared.agent() {
            send_callback(
                &request,
                &messenger.target,
                CALLBACK_AVAILABLE,
                agent.net_id,
                available_entries(&agent),
            );
        }
        Ok(Some(request))
    }

    /// releaseNetworkRequest: forgets the request (ConnectivityService.handleReleaseNetworkRequest). No UNAVAIL callback: AOSP sends none on release.
    fn r#releaseNetworkRequest(&self, request: Option<&NetworkRequest>) -> binder::Result<()> {
        if let Some(r) = request {
            locked(&self.shared.requests).retain(|(have, _)| have.request_id != r.request_id);
        }
        Ok(())
    }
}

/// The service's binder, registered with `addService`, and its watch. The host's network is read at start, after the watch starts (so a change between the two is seen), and again on each watch burst.
/// `host` asks atld; `sticky` sends the CONNECTIVITY_ACTION broadcasts through the activity manager.
pub fn start(
    permissions: Arc<Permissions>,
    host: HostNetwork,
    sticky: StickyBroadcast,
) -> io::Result<SpIBinder> {
    let shared = Arc::new(Shared {
        permissions,
        state: Mutex::new(State {
            agent: None,
            last_net_id: MIN_NET_ID - 1,
        }),
        next_request_id: AtomicI32::new(1),
        requests: Mutex::new(Vec::new()),
        sticky,
    });
    let (tx, rx) = std::sync::mpsc::channel();
    match PlatformNetwork.watch(tx) {
        Ok(()) => {
            let (shared, host) = (Arc::clone(&shared), Arc::clone(&host));
            std::thread::Builder::new()
                .name("connectivity-watch".into())
                .spawn(move || {
                    for () in rx {
                        shared.refresh(&host);
                    }
                })?;
        }
        Err(e) => {
            note(format_args!("host network watch not started: {e}"));
            atl_diag::answered(CM, "network watch not started");
        }
    }
    shared.refresh(&host);
    Ok(
        BnConnectivityManager::new_binder(
            ConnectivityService { shared },
            BinderFeatures::default(),
        )
        .as_binder(),
    )
}
