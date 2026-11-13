// SPDX-License-Identifier: MIT
//! The `connectivity` service (Context.CONNECTIVITY_SERVICE): ConnectivityFrameworkInitializer.registerServiceWrappers builds ConnectivityManager only from this binder; without it
//! getSystemService returns null and apps that never null-check it throw (Cut the Rope's ZSystemInfo.isNetworkAvailable from a native call).
//! The host's default network (atl_platform_host::NetworkSource, read once at start) is the one network agent: netId 100 (NetIdManager.MIN_NET_ID, the first id ConnectivityService hands out), what the app sees of it
//! as ConnectivityService answers an unprivileged app for an Ethernet or Wi-Fi agent (plan app-network.md). No default route on the host -> no agent: the answers of a device with none (null network, metered, no callback).
//! Network callbacks: requestNetwork of type TRACK_DEFAULT (ConnectivityManager.registerDefaultNetworkCallback) answers its NetworkRequest and, with an agent, sends CALLBACK_AVAILABLE to the caller's Messenger
//! as ConnectivityService.callCallbackForRequest builds it. Not modelled (counted as unimplemented): other request types, host network changes after start, the client's death.
//! Known divergence: getNetworkInfo(type) for a type without an agent answers null where AOSP makes a fake DISCONNECTED NetworkInfo (getFilteredNetworkInfoForType -> makeFakeNetworkInfo).

use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Mutex};

use atl_aidl::aidl::android::net::IConnectivityManager::{
    BnConnectivityManager, IConnectivityManager,
};
use atl_aidl::aidl::android::os::IMessenger::IMessenger;
use atl_parcelables::android::net::{
    LinkProperties, Network, NetworkCapabilities, NetworkInfo, NetworkRequest, ProxyInfo,
};
use atl_parcelables::android::os::{Bundle, Message, Messenger};
use atl_parcelables::codecs::android::net::LinkProperties::{RTN_UNICAST, RouteInfo};
use atl_parcelables::codecs::android::net::NetworkCapabilities::*;
use atl_parcelables::codecs::android::os::Bundle::Value;
use atl_platform_host::{Connectivity, DefaultNetwork, LinkKind};
use binder::{
    BinderFeatures, ExceptionCode, FromIBinder, Interface, SpIBinder, Status, ThreadState,
};
use std::io::Write;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use crate::packages::Permissions;

/// The interface name counters use.
const INTERFACE: &str = "IConnectivityManager";
/// ConnectivityService.enforceAccessPermission: every query below needs it.
const ACCESS_NETWORK_STATE: &str = "android.permission.ACCESS_NETWORK_STATE";
/// NetIdManager.MIN_NET_ID: the first netId ConnectivityService gives an agent.
const NET_ID: i32 = 100;
/// ConnectivityManager.TYPE_WIFI / TYPE_ETHERNET, the legacy types the Wi-Fi and Ethernet agents declare (ClientModeImpl, EthernetNetworkFactory).
const TYPE_WIFI: i32 = 1;
const TYPE_ETHERNET: i32 = 9;
/// NetworkRequest.Type ordinals (NetworkRequest.java:235-244) and names.
const TRACK_DEFAULT: i32 = 2;
const TRACK_DEFAULT_NAME: &str = "TRACK_DEFAULT";
/// ConnectivityManager.CALLBACK_AVAILABLE (ConnectivityManager.java:4506).
const CALLBACK_AVAILABLE: i32 = 2;

/// The one agent: the host's default network as the app sees it.
struct Agent {
    info: NetworkInfo,
    capabilities: NetworkCapabilities,
    link: LinkProperties,
    metered: bool,
}

impl Agent {
    /// The agent of `n`. Capabilities: those an Ethernet / Wi-Fi agent declares (INTERNET, NOT_ROAMING, NOT_CONGESTED, NOT_SUSPENDED; NOT_METERED unless metered) on top of the constructor's defaults,
    /// plus what ConnectivityService mixes in for a default network (FOREGROUND, NOT_VCN_MANAGED) and its validation result: VALIDATED for full connectivity, CAPTIVE_PORTAL behind a portal, neither when limited.
    /// Owner, administrator and requestor fields stay unset: networkCapabilitiesRestrictedForCallerPermissions clears them for an unprivileged caller.
    fn of(n: &DefaultNetwork) -> Agent {
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

struct ConnectivityService {
    permissions: Arc<Permissions>,
    agent: Option<Agent>,
    /// ConnectivityService.mNextNetworkRequestId.
    next_request_id: AtomicI32,
    /// The registered callbacks: each request with the Messenger its callbacks go to.
    requests: Mutex<Vec<(NetworkRequest, SpIBinder)>>,
}

impl ConnectivityService {
    /// ConnectivityService.enforceAccessPermission (enforceCallingOrSelfPermission -> SecurityException).
    fn enforce_access(&self, method: &str) -> binder::Result<()> {
        if self.permissions.holds(
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
        self.agent.as_ref().map(|_| Network { net_id: NET_ID })
    }

    /// The agent `n` names, if it is ours.
    fn agent_of(&self, n: Option<&Network>) -> Option<&Agent> {
        n.filter(|n| n.net_id == NET_ID).and(self.agent.as_ref())
    }

    /// callCallbackForRequest(CALLBACK_AVAILABLE): the common Bundle (NetworkRequest, Network) plus the capabilities, link properties and a null LocalNetworkInfo, keyed by class simple name; arg1 = blocked reasons (none).
    fn send_available(&self, request: &NetworkRequest, messenger: &SpIBinder, agent: &Agent) {
        let bundle = (|| -> binder::Result<Bundle> {
            Ok(Bundle::from_entries([
                (
                    "NetworkRequest".to_owned(),
                    Value::parcelable("android.net.NetworkRequest", request)?,
                ),
                (
                    "Network".to_owned(),
                    Value::parcelable("android.net.Network", &Network { net_id: NET_ID })?,
                ),
                (
                    "NetworkCapabilities".to_owned(),
                    Value::parcelable("android.net.NetworkCapabilities", &agent.capabilities)?,
                ),
                (
                    "LinkProperties".to_owned(),
                    Value::parcelable("android.net.LinkProperties", &agent.link)?,
                ),
                ("LocalNetworkInfo".to_owned(), Value::Null),
            ]))
        })();
        let sent = bundle.and_then(|b| {
            let client = <dyn IMessenger as FromIBinder>::try_from(messenger.clone())?;
            client.send(Some(&Message::new(CALLBACK_AVAILABLE, 0, b)))
        });
        // a dead client is ConnectivityService's "may occur naturally in the race of binder death": logged, not thrown
        if let Err(e) = sent {
            let _ = writeln!(
                std::io::stderr(),
                "connectivity: CALLBACK_AVAILABLE for request {} not sent: {e}",
                request.request_id
            );
        }
    }
}

impl Interface for ConnectivityService {}

impl IConnectivityManager for ConnectivityService {
    /// getActiveNetworkInfo -> getFilteredNetworkInfo of the default network: its NetworkInfo (nothing blocks it), null without one.
    fn r#getActiveNetworkInfo(&self) -> binder::Result<Option<NetworkInfo>> {
        self.enforce_access("getActiveNetworkInfo")?;
        Ok(self.agent.as_ref().map(|a| a.info.clone()))
    }

    fn r#getActiveNetworkInfoForUid(
        &self,
        _uid: i32,
        _ignore_blocked: bool,
    ) -> binder::Result<Option<NetworkInfo>> {
        self.enforce_access("getActiveNetworkInfoForUid")?;
        Ok(self.agent.as_ref().map(|a| a.info.clone()))
    }

    /// getNetworkInfo(type): the agent's NetworkInfo when its legacy type is `network_type` (see the module doc's divergence otherwise).
    fn r#getNetworkInfo(&self, network_type: i32) -> binder::Result<Option<NetworkInfo>> {
        self.enforce_access("getNetworkInfo")?;
        Ok(self
            .agent
            .as_ref()
            .filter(|a| a.info.network_type == network_type)
            .map(|a| a.info.clone()))
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
            .agent
            .as_ref()
            .filter(|a| a.info.network_type == network_type)
            .map(|_| Network { net_id: NET_ID }))
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
        Ok(self.agent_of(network).map(|a| a.capabilities.clone()))
    }

    fn r#getLinkProperties(
        &self,
        network: Option<&Network>,
    ) -> binder::Result<Option<LinkProperties>> {
        self.enforce_access("getLinkProperties")?;
        Ok(self.agent_of(network).map(|a| a.link.clone()))
    }

    fn r#getActiveLinkProperties(&self) -> binder::Result<Option<LinkProperties>> {
        self.enforce_access("getActiveLinkProperties")?;
        Ok(self.agent.as_ref().map(|a| a.link.clone()))
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
        Ok(self.agent.as_ref().is_none_or(|a| a.metered))
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
            return Err(atl_services_wire::unimplemented(
                INTERFACE,
                "requestNetwork(type other than TRACK_DEFAULT)",
            ));
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
            request_id: self.next_request_id.fetch_add(1, Ordering::Relaxed),
            request_type: TRACK_DEFAULT_NAME.to_owned(),
        };
        self.requests
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((request.clone(), messenger.target.clone()));
        // oneway: ConnectivityManager holds sCallbacks while this call runs, so its handler finds the request registered when the message arrives
        if let Some(agent) = &self.agent {
            self.send_available(&request, &messenger.target, agent);
        }
        Ok(Some(request))
    }

    /// releaseNetworkRequest: forgets the request (ConnectivityService.handleReleaseNetworkRequest). No UNAVAIL callback: AOSP sends none on release.
    fn r#releaseNetworkRequest(&self, request: Option<&NetworkRequest>) -> binder::Result<()> {
        if let Some(r) = request {
            self.requests
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .retain(|(have, _)| have.request_id != r.request_id);
        }
        Ok(())
    }
}

/// The service's binder, for `addService`: `network` is the host's default network at start, None offline.
pub fn new_binder(permissions: Arc<Permissions>, network: Option<&DefaultNetwork>) -> SpIBinder {
    BnConnectivityManager::new_binder(
        ConnectivityService {
            permissions,
            agent: network.map(Agent::of),
            next_request_id: AtomicI32::new(1),
            requests: Mutex::new(Vec::new()),
        },
        BinderFeatures::default(),
    )
    .as_binder()
}
