// SPDX-License-Identifier: MIT
//! The `netstats` service (INetworkStatsService): every app process's ActivityThread.handleBindApplication calls TrafficStats.init (ActivityThread.java:7749), which takes this binder through
//! NetworkStatsManager; without it every later TrafficStats getter throws IllegalStateException "TrafficStats not initialized" (TrafficStats.java:233-239; WeChat died on it).
//! Behaviour = NetworkStatsService (packages/modules/Connectivity service-t) on a device whose per-uid and per-interface counters cannot be read: its native reads return null and the
//! result is null (NetworkStatsService.java:2258-2261), which TrafficStats turns into UNSUPPORTED (-1, TrafficStats.java:1343-1345) -- the API's documented "not available on this device".
//! Why null and not host counters: app sandboxes share the host's network namespace, so no counter is the app's own (getUidStats), and interface / total counters are the whole host's traffic,
//! a datum Android derives only from the device's own traffic (plan empty-device-services P). Another uid's stats are null for any caller but SYSTEM (:2215-2219), as here.
//! Sessions (openSession*, NetworkStatsManager.querySummary ...), incrementOperationCount, forceUpdate, providers: not implemented, answered UNKNOWN_TRANSACTION and counted by atl-diag's reply hook.
//! Rung 6 of "AOSP's own code first": NetworkStatsService runs in system_server over eBPF maps the host does not have.

use atl_aidl::aidl::android::net::INetworkStatsService::{
    BnNetworkStatsService, INetworkStatsService,
};
use atl_aidl::aidl::android::net::netstats::StatsResult::StatsResult;
use atl_aidl::aidl::android::net::netstats::TrafficStatsRateLimitCacheConfig::TrafficStatsRateLimitCacheConfig;
use binder::{BinderFeatures, Interface, SpIBinder};

/// The name the answers are counted under (`atl_diag::answered`, the app gate's SERVICE_CALLS).
const NETSTATS: &str = "INetworkStatsService";

struct NetworkStatsService;

impl Interface for NetworkStatsService {}

impl INetworkStatsService for NetworkStatsService {
    /// :2215-2227: another uid -> null; the caller's own -> the native read, null here (no per-app accounting in a shared netns).
    fn r#getUidStats(&self, _uid: i32) -> binder::Result<Option<StatsResult>> {
        atl_diag::answered(NETSTATS, "getUidStats (null: no per-app accounting)");
        Ok(None)
    }

    /// :2245-2256: the interface's counters would be host-wide traffic.
    fn r#getIfaceStats(&self, _iface: Option<&str>) -> binder::Result<Option<StatsResult>> {
        atl_diag::answered(NETSTATS, "getIfaceStats (null: host-wide counters withheld)");
        Ok(None)
    }

    /// :2275-2283: the device total would be host-wide traffic.
    fn r#getTotalStats(&self) -> binder::Result<Option<StatsResult>> {
        atl_diag::answered(NETSTATS, "getTotalStats (null: host-wide counters withheld)");
        Ok(None)
    }

    /// :2296-2306, the client-side cache off (the feature's off state, Builder defaults): with every result null the client caches nothing anyway (TrafficStats fetchStatsWithCache keeps only non-null, non-empty results).
    fn r#getRateLimitCacheConfig(
        &self,
    ) -> binder::Result<Option<TrafficStatsRateLimitCacheConfig>> {
        atl_diag::answered(NETSTATS, "getRateLimitCacheConfig (cache off)");
        Ok(Some(TrafficStatsRateLimitCacheConfig {
            isCacheEnabled: false,
            expiryDurationMs: 0,
            maxEntries: 0,
        }))
    }
}

/// The `netstats` binder, for `addService`.
pub fn new_binder() -> SpIBinder {
    BnNetworkStatsService::new_binder(NetworkStatsService, BinderFeatures::default()).as_binder()
}
