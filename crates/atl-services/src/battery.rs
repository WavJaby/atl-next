// SPDX-License-Identifier: MIT
//! The `batteryproperties` and `batterystats` services, and the sticky ACTION_BATTERY_CHANGED: SystemServiceRegistry builds BatteryManager only when both lookups succeed (SystemServiceRegistry.java:507),
//! else getSystemService(BATTERY_SERVICE) is null (WeChat died on the cast). The one power-source fact of the services: power.rs's plugged state, were PowerManager ever to answer one, reads [`Battery`].
//! Source = AOSP's default health HAL (`android.hardware.health-service.example`, atld `--health`): its BatteryMonitor is the only reader of the host's /sys/class/power_supply; this service is its IHealth
//! client, as BatteryService is through HealthServiceWrapperAidl (frameworks/base/services/core/java/com/android/server/BatteryService.java, health/HealthServiceWrapperAidl.java).
//! - BatteryService: last HealthInfo from the HAL's IHealthInfoCallback, plug type (plugType :699-711), low-battery state (processValuesLocked :848-867, shouldSendBatteryLowLocked :590-605; warning level
//!   config_lowBatteryWarningLevel 20, close bump 5, config.xml :1448/:1459, LOW_POWER_MODE_TRIGGER_LEVEL unset), the change test (:757-771) and its broadcasts: POWER_CONNECTED / DISCONNECTED, BATTERY_LOW /
//!   OKAY (:874-959), sticky ACTION_BATTERY_CHANGED to USER_ALL (sendBatteryChangedIntentLocked :1005-1035, broadcastBatteryChangedIntent :1070) through the activity service as the system.
//! - BatteryPropertiesRegistrar.getProperty (:1845-1864 -> HealthServiceWrapperAidl.getPropertyInternal :116-180); scheduleUpdate -> IHealth.update.
//! - IBatteryStats.isCharging = BatteryStatsImpl.mCharging ([`Charging`]); computeChargeTimeRemaining (:15390-15406, BatteryStatsService ms). Every other IBatteryStats method: UNKNOWN_TRANSACTION, counted.
//!
//! Not sent / not modelled, each counted when reached: `icon-small` (framework-res's stat_sys_battery ids are not read by the services), ACTION_BATTERY_LEVEL_CHANGED (BATTERY_STATS holders only), the charge
//! step tracker's estimate, the aconfig flags (stateOfHealthPublic, batteryPartStatusApi, rateLimitBatteryChangedBroadcast: off), invalid-charger uevents (0), shutdown on empty / hot battery (desktop: the host's own).

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use atl_aidl::aidl::android::os::IBatteryPropertiesRegistrar::{
    BnBatteryPropertiesRegistrar, IBatteryPropertiesRegistrar,
};
use atl_aidl::aidl::com::android::internal::app::IBatteryStats::{
    BnBatteryStats, EnforcePermission, IBatteryStats,
};
use atl_aidl::health::aidl::android::hardware::health::HealthInfo::HealthInfo;
use atl_aidl::health::aidl::android::hardware::health::IHealth::IHealth;
use atl_aidl::health::aidl::android::hardware::health::IHealthInfoCallback::{
    BnHealthInfoCallback, IHealthInfoCallback,
};
use atl_parcelables::android::content::Intent;
use atl_parcelables::android::os::{BatteryProperty, Bundle};
use atl_parcelables::codecs::android::os::Bundle::Value;
use binder::{
    BinderFeatures, ExceptionCode, Interface, SpIBinder, Status, StatusCode, Strong, ThreadState,
};

use crate::broadcast::{FLAG_RECEIVER_REGISTERED_ONLY, FLAG_RECEIVER_REGISTERED_ONLY_BEFORE_BOOT};
use crate::packages::Permissions;

/// The HAL instance BatteryService waits for (HealthServiceWrapperAidl.SERVICE_NAME).
pub const IHEALTH: &str = "android.hardware.health.IHealth/default";
/// Counter names (atl_diag::answered).
const PROPERTIES: &str = "IBatteryPropertiesRegistrar";
const STATS: &str = "IBatteryStats";
/// Manifest.permission.BATTERY_STATS: the guarded property ids (BatteryPropertiesRegistrar.getProperty :1849-1861).
const BATTERY_STATS: &str = "android.permission.BATTERY_STATS";
/// BatteryManager.BATTERY_PROPERTY_* (BatteryManager.java).
const PROPERTY_CHARGE_COUNTER: i32 = 1;
const PROPERTY_CURRENT_NOW: i32 = 2;
const PROPERTY_CURRENT_AVERAGE: i32 = 3;
const PROPERTY_CAPACITY: i32 = 4;
const PROPERTY_ENERGY_COUNTER: i32 = 5;
const PROPERTY_STATUS: i32 = 6;
const PROPERTY_MANUFACTURING_DATE: i32 = 7;
const PROPERTY_FIRST_USAGE_DATE: i32 = 8;
const PROPERTY_CHARGING_POLICY: i32 = 9;
const PROPERTY_STATE_OF_HEALTH: i32 = 10;
const PROPERTY_SERIAL_NUMBER: i32 = 11;
const PROPERTY_PART_STATUS: i32 = 12;
/// BatteryManager.BATTERY_STATUS_UNKNOWN, BATTERY_PLUGGED_* (plugType's answers).
const STATUS_UNKNOWN: i32 = 1;
const PLUGGED_NONE: i32 = 0;
const PLUGGED_AC: i32 = 1;
const PLUGGED_USB: i32 = 2;
const PLUGGED_WIRELESS: i32 = 4;
const PLUGGED_DOCK: i32 = 8;
/// BatteryService.BATTERY_SCALE.
const SCALE: i32 = 100;
/// config.xml :1448 config_lowBatteryWarningLevel (above config_criticalBatteryWarningLevel 10, so kept: updateBatteryWarningLevelLocked :557), :1459 config_lowBatteryCloseWarningBump.
const LOW_WARNING_LEVEL: i32 = 20;
const LOW_CLOSE_LEVEL: i32 = LOW_WARNING_LEVEL + 5;
/// BatteryStatsImpl.Constants DEFAULT_BATTERY_CHARGING_ENFORCE_LEVEL (:16042), DEFAULT_BATTERY_CHARGED_DELAY_MS (:16041).
const CHARGING_ENFORCE_LEVEL: i32 = 90;
const CHARGED_DELAY: Duration = Duration::from_millis(900_000);
/// Intent actions and flags (Intent.java).
const ACTION_BATTERY_CHANGED: &str = "android.intent.action.BATTERY_CHANGED";
const ACTION_BATTERY_LOW: &str = "android.intent.action.BATTERY_LOW";
const ACTION_BATTERY_OKAY: &str = "android.intent.action.BATTERY_OKAY";
const ACTION_POWER_CONNECTED: &str = "android.intent.action.ACTION_POWER_CONNECTED";
const ACTION_POWER_DISCONNECTED: &str = "android.intent.action.ACTION_POWER_DISCONNECTED";
const FLAG_RECEIVER_REPLACE_PENDING: i32 = 0x2000_0000;

/// Sends a broadcast as the system to every user, sticky or not (wired to the activity service's `broadcast_as_system` by atl-system-services).
pub type SystemBroadcast = Arc<dyn Fn(&Intent, bool) + Send + Sync>;

/// The HealthInfo fields BatteryService reads (processValuesLocked, sendBatteryChangedIntentLocked).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Values {
    pub status: i32,
    pub health: i32,
    pub present: bool,
    pub level: i32,
    pub voltage_mv: i32,
    pub temperature: i32,
    pub technology: String,
    pub max_charging_current: i32,
    pub max_charging_voltage: i32,
    pub charge_counter: i32,
    pub cycle_count: i32,
    pub charging_state: i32,
    pub capacity_level: i32,
    pub time_to_full_s: i64,
    pub ac: bool,
    pub usb: bool,
    pub wireless: bool,
    pub dock: bool,
}

impl From<&HealthInfo> for Values {
    fn from(h: &HealthInfo) -> Self {
        Values {
            status: h.batteryStatus.0,
            health: h.batteryHealth.0,
            present: h.batteryPresent,
            level: h.batteryLevel,
            voltage_mv: h.batteryVoltageMillivolts,
            temperature: h.batteryTemperatureTenthsCelsius,
            technology: h.batteryTechnology.clone(),
            max_charging_current: h.maxChargingCurrentMicroamps,
            max_charging_voltage: h.maxChargingVoltageMicrovolts,
            charge_counter: h.batteryChargeCounterUah,
            cycle_count: h.batteryCycleCount,
            charging_state: h.chargingState.0,
            capacity_level: h.batteryCapacityLevel.0,
            time_to_full_s: h.batteryChargeTimeToFullNowSeconds,
            ac: h.chargerAcOnline,
            usb: h.chargerUsbOnline,
            wireless: h.chargerWirelessOnline,
            dock: h.chargerDockOnline,
        }
    }
}

impl Values {
    /// BatteryService.plugType :699-711: the first online charger in AC, USB, wireless, dock order.
    fn plug_type(&self) -> i32 {
        [
            (self.ac, PLUGGED_AC),
            (self.usb, PLUGGED_USB),
            (self.wireless, PLUGGED_WIRELESS),
            (self.dock, PLUGGED_DOCK),
        ]
        .into_iter()
        .find_map(|(on, p)| on.then_some(p))
        .unwrap_or(PLUGGED_NONE)
    }

    /// BatteryStatsImpl.isOnBattery :15294-15296.
    fn on_battery(&self) -> bool {
        self.plug_type() == PLUGGED_NONE && self.status != STATUS_UNKNOWN
    }
}

/// BatteryStatsImpl.mCharging as setBatteryStateLocked moves it (:15099-15268, mDeferSetCharging :814-831): true at start (:1764); on battery false; plugged at or above the enforce level true; plugged and
/// not charging, a level rise schedules true after the charged delay (a fall cancels it, being on battery when it fires voids it); plugged and charging, a level fall is false.
#[derive(Debug)]
pub struct Charging {
    charging: bool,
    on_battery: Option<bool>,
    last_charge_step: i32,
    deferred: Option<Instant>,
    /// A charge step was recorded (:15258): the step tracker's estimate, not modelled, would answer computeChargeTimeRemaining.
    stepped: bool,
}

impl Default for Charging {
    fn default() -> Self {
        Charging {
            charging: true,
            on_battery: None,
            last_charge_step: 0,
            deferred: None,
            stepped: false,
        }
    }
}

impl Charging {
    /// The deferred set runs when due, with the state it finds (the state between updates is the last one).
    fn run_due(&mut self, now: Instant) {
        if self.deferred.is_some_and(|t| t <= now) {
            self.deferred = None;
            if self.on_battery == Some(false) {
                self.charging = true;
            }
        }
    }

    fn set_battery_state(&mut self, on_battery: bool, level: i32, now: Instant) {
        self.run_due(now);
        // the first state and every plug-in start the charge steps at this level (:15117, :15068)
        if self.on_battery != Some(false) && !on_battery {
            self.last_charge_step = level;
        }
        self.on_battery = Some(on_battery);
        if on_battery {
            self.charging = false;
            return;
        }
        if level >= CHARGING_ENFORCE_LEVEL {
            self.charging = true;
        } else if !self.charging {
            if self.last_charge_step < level {
                self.deferred.get_or_insert(now + CHARGED_DELAY);
            } else if self.last_charge_step > level {
                self.deferred = None;
            }
        } else if self.last_charge_step > level {
            self.charging = false;
        }
        if self.last_charge_step < level {
            self.stepped = true;
        }
        self.last_charge_step = level;
    }

    pub fn is_charging(&mut self, now: Instant) -> bool {
        self.run_due(now);
        self.charging
    }
}

/// BatteryService's state between HAL callbacks.
#[derive(Debug, Default)]
struct State {
    values: Option<Values>,
    plug: i32,
    /// The values of the last ACTION_BATTERY_CHANGED (mLastBroadcast*); None before the first.
    last: Option<(Values, i32)>,
    low: bool,
    sent_low: bool,
    sequence: i32,
    charging: Charging,
}

impl State {
    /// processValuesLocked(force) for new values: the broadcasts it sends, in its order, each with whether it is sticky.
    fn update(&mut self, v: Values, now: Instant) -> Vec<(Intent, bool)> {
        let force = self.last.is_none();
        self.plug = v.plug_type();
        self.charging
            .set_battery_state(v.on_battery(), v.level, now);
        let mut out = Vec::new();
        // :757-771 (charge counter included: rateLimitBatteryChangedBroadcast off; invalid charger always 0)
        let changed = match &self.last {
            None => true,
            Some((l, plug)) => *plug != self.plug || *l != v,
        };
        if !changed {
            self.values = Some(v);
            return out;
        }
        let last_plug = self.last.as_ref().map_or(PLUGGED_NONE, |(_, p)| *p);
        let last_level = self.last.as_ref().map(|(l, _)| l.level);
        // :848-867
        if !self.low {
            if self.plug == PLUGGED_NONE
                && v.status != STATUS_UNKNOWN
                && v.level <= LOW_WARNING_LEVEL
            {
                self.low = true;
            }
        } else if self.plug != PLUGGED_NONE
            || v.level >= LOW_CLOSE_LEVEL
            || (force && v.level >= LOW_WARNING_LEVEL)
        {
            self.low = false;
        }
        self.sequence += 1;
        let seq = |action: &str| {
            intent(
                action,
                FLAG_RECEIVER_REGISTERED_ONLY_BEFORE_BOOT,
                vec![("seq", Value::Int(self.sequence))],
            )
        };
        // :874-914
        if self.plug != PLUGGED_NONE && last_plug == PLUGGED_NONE {
            out.push((seq(ACTION_POWER_CONNECTED), false));
        } else if self.plug == PLUGGED_NONE && last_plug != PLUGGED_NONE {
            out.push((seq(ACTION_POWER_DISCONNECTED), false));
        }
        // :916-959, shouldSendBatteryLowLocked :590-605 (mLastLowBatteryWarningLevel = the warning level: the setting never changes here)
        let should_low = self.plug == PLUGGED_NONE
            && v.status != STATUS_UNKNOWN
            && v.level <= LOW_WARNING_LEVEL
            && (last_plug != PLUGGED_NONE
                || last_level.is_none_or(|l| l > LOW_WARNING_LEVEL)
                || v.level > LOW_WARNING_LEVEL);
        if should_low {
            self.sent_low = true;
            out.push((seq(ACTION_BATTERY_LOW), false));
        } else if self.sent_low && v.level >= LOW_CLOSE_LEVEL {
            self.sent_low = false;
            out.push((seq(ACTION_BATTERY_OKAY), false));
        }
        out.push((self.battery_changed(&v), true));
        self.last = Some((v.clone(), self.plug));
        self.values = Some(v);
        out
    }

    /// sendBatteryChangedIntentLocked :1005-1035 without `icon-small` (counted by the caller).
    fn battery_changed(&self, v: &Values) -> Intent {
        intent(
            ACTION_BATTERY_CHANGED,
            FLAG_RECEIVER_REGISTERED_ONLY | FLAG_RECEIVER_REPLACE_PENDING,
            vec![
                ("seq", Value::Int(self.sequence)),
                ("status", Value::Int(v.status)),
                ("health", Value::Int(v.health)),
                ("present", Value::Bool(v.present)),
                ("level", Value::Int(v.level)),
                ("battery_low", Value::Bool(self.sent_low)),
                ("scale", Value::Int(SCALE)),
                ("plugged", Value::Int(self.plug)),
                ("voltage", Value::Int(v.voltage_mv)),
                ("temperature", Value::Int(v.temperature)),
                ("technology", Value::String(Some(v.technology.clone()))),
                ("invalid_charger", Value::Int(0)),
                ("max_charging_current", Value::Int(v.max_charging_current)),
                ("max_charging_voltage", Value::Int(v.max_charging_voltage)),
                ("charge_counter", Value::Int(v.charge_counter)),
                ("android.os.extra.CYCLE_COUNT", Value::Int(v.cycle_count)),
                (
                    "android.os.extra.CHARGING_STATUS",
                    Value::Int(v.charging_state),
                ),
                (
                    "android.os.extra.CAPACITY_LEVEL",
                    Value::Int(v.capacity_level),
                ),
            ],
        )
    }
}

fn intent(action: &str, flags: i32, extras: Vec<(&str, Value)>) -> Intent {
    Intent {
        action: Some(action.to_owned()),
        flags,
        extras: Some(Bundle::from_entries(
            extras.into_iter().map(|(k, v)| (k.to_owned(), v)),
        )),
        ..Intent::default()
    }
}

/// BatteryService over the health HAL: the state, the HAL and where broadcasts go.
pub struct Battery {
    state: Mutex<State>,
    health: Strong<dyn IHealth>,
    broadcast: SystemBroadcast,
}

impl Battery {
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn update(&self, info: &HealthInfo) {
        let sent = self.state().update(Values::from(info), Instant::now());
        for (intent, sticky) in sent {
            if intent.action.as_deref() == Some(ACTION_BATTERY_CHANGED) {
                atl_diag::answered(STATS, "ACTION_BATTERY_CHANGED (icon-small not sent)");
            }
            (self.broadcast)(&intent, sticky);
        }
    }
}

/// The HAL's IHealthInfoCallback (BatteryService.HealthHalCallback): each HealthInfo goes through processValuesLocked.
struct Callback(Arc<Battery>);

impl Interface for Callback {}

impl IHealthInfoCallback for Callback {
    fn r#healthInfoChanged(&self, info: &HealthInfo) -> binder::Result<()> {
        self.0.update(info);
        Ok(())
    }
}

/// HealthServiceWrapperAidl.getPropertyInternal's exception mapping (:168-174): unsupported -1, service-specific -2, others thrown on.
fn property<T>(r: binder::Result<T>) -> binder::Result<Result<T, i32>> {
    match r {
        Ok(v) => Ok(Ok(v)),
        Err(e) if e.exception_code() == ExceptionCode::UNSUPPORTED_OPERATION => Ok(Err(-1)),
        Err(e) if e.exception_code() == ExceptionCode::SERVICE_SPECIFIC => Ok(Err(-2)),
        Err(e) => Err(e),
    }
}

struct PropertiesRegistrar {
    battery: Arc<Battery>,
    permissions: Arc<Permissions>,
}

impl Interface for PropertiesRegistrar {}

impl IBatteryPropertiesRegistrar for PropertiesRegistrar {
    /// :1845-1864 -> :116-180: the property's IHealth getter fills `prop`; a failed getter leaves BatteryProperty() (Long.MIN_VALUE); an unknown id sets nothing and answers 0.
    fn r#getProperty(&self, id: i32, prop: &mut Option<BatteryProperty>) -> binder::Result<i32> {
        let guarded = matches!(
            id,
            PROPERTY_STATE_OF_HEALTH
                | PROPERTY_MANUFACTURING_DATE
                | PROPERTY_FIRST_USAGE_DATE
                | PROPERTY_CHARGING_POLICY
                | PROPERTY_SERIAL_NUMBER
                | PROPERTY_PART_STATUS
        );
        if guarded
            && !self
                .permissions
                .holds(ThreadState::get_calling_uid(), &[BATTERY_STATS], false)
        {
            return Err(Status::new_exception_str(
                ExceptionCode::SECURITY,
                Some(format!("getProperty({id}): caller lacks {BATTERY_STATS}")),
            ));
        }
        let h = &self.battery.health;
        let mut out = BatteryProperty::default();
        let long = |r: Result<i64, i32>, out: &mut BatteryProperty| match r {
            Ok(v) => {
                out.value_long = v;
                0
            }
            Err(code) => code,
        };
        let ret = match id {
            PROPERTY_CHARGE_COUNTER => {
                long(property(h.getChargeCounterUah())?.map(i64::from), &mut out)
            }
            PROPERTY_CURRENT_NOW => long(
                property(h.getCurrentNowMicroamps())?.map(i64::from),
                &mut out,
            ),
            PROPERTY_CURRENT_AVERAGE => long(
                property(h.getCurrentAverageMicroamps())?.map(i64::from),
                &mut out,
            ),
            PROPERTY_CAPACITY => long(property(h.getCapacity())?.map(i64::from), &mut out),
            PROPERTY_STATUS => long(
                property(h.getChargeStatus())?.map(|s| i64::from(s.0)),
                &mut out,
            ),
            PROPERTY_ENERGY_COUNTER => long(property(h.getEnergyCounterNwh())?, &mut out),
            PROPERTY_CHARGING_POLICY => long(
                property(h.getChargingPolicy())?.map(|p| i64::from(p.0)),
                &mut out,
            ),
            PROPERTY_MANUFACTURING_DATE
            | PROPERTY_FIRST_USAGE_DATE
            | PROPERTY_STATE_OF_HEALTH
            | PROPERTY_PART_STATUS
            | PROPERTY_SERIAL_NUMBER => {
                let data = property(h.getBatteryHealthData())?;
                match (id, data) {
                    (_, Err(code)) => code,
                    (PROPERTY_MANUFACTURING_DATE, Ok(d)) => {
                        long(Ok(d.batteryManufacturingDateSeconds), &mut out)
                    }
                    (PROPERTY_FIRST_USAGE_DATE, Ok(d)) => {
                        long(Ok(d.batteryFirstUsageSeconds), &mut out)
                    }
                    (PROPERTY_STATE_OF_HEALTH, Ok(d)) => long(Ok(d.batteryStateOfHealth), &mut out),
                    (PROPERTY_PART_STATUS, Ok(d)) => {
                        long(Ok(i64::from(d.batteryPartStatus.0)), &mut out)
                    }
                    (_, Ok(d)) => {
                        out.value_string = d.batterySerialNumber.clone();
                        0
                    }
                }
            }
            _ => 0,
        };
        atl_diag::answered(PROPERTIES, &format!("getProperty({id}) -> {ret}"));
        *prop = Some(out);
        Ok(ret)
    }

    /// :1866-1868 -> HealthServiceWrapperAidl.scheduleUpdate: IHealth.update, whose callback brings the new values.
    fn r#scheduleUpdate(&self) -> binder::Result<()> {
        self.battery.health.update()
    }
}

struct BatteryStats(Arc<Battery>, Arc<Permissions>);

impl Interface for BatteryStats {}

impl IBatteryStats for BatteryStats {
    /// @EnforcePermission (the stub methods' BATTERY_STATS / UPDATE_DEVICE_STATS ...): root / system, or a caller whose grants hold them (Permissions::holds).
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
        if self.1.holds(ThreadState::get_calling_uid(), perms, any) {
            return Ok(());
        }
        Err(Status::new_exception_str(
            ExceptionCode::SECURITY,
            Some(format!("{method}: caller lacks {permission:?}")),
        ))
    }

    fn r#isCharging(&self) -> binder::Result<bool> {
        Ok(self.0.state().charging.is_charging(Instant::now()))
    }

    /// BatteryStatsImpl.computeChargeTimeRemaining :15390-15406 in ms (BatteryStatsService): on battery -1; the HAL's time to full when it has one; else the charge step estimate, -1 with no step recorded.
    fn r#computeChargeTimeRemaining(&self) -> binder::Result<i64> {
        let st = self.0.state();
        let Some(v) = &st.values else {
            return Ok(-1);
        };
        if v.on_battery() {
            return Ok(-1);
        }
        if v.time_to_full_s >= 0 {
            return Ok(v.time_to_full_s * 1000);
        }
        if !st.charging.stepped {
            return Ok(-1);
        }
        atl_diag::answered(
            STATS,
            "computeChargeTimeRemaining (step estimate not modelled)",
        );
        Err(StatusCode::UNKNOWN_TRANSACTION.into())
    }
}

/// The `batteryproperties` and `batterystats` binders over the HAL `health` (IHealth/default), its callback registered: the HAL calls it at once with the current values (Health::registerCallback),
/// which sends the first sticky ACTION_BATTERY_CHANGED.
pub fn new_binders(
    health: Strong<dyn IHealth>,
    permissions: Arc<Permissions>,
    broadcast: SystemBroadcast,
) -> binder::Result<(SpIBinder, SpIBinder)> {
    let battery = Arc::new(Battery {
        state: Mutex::new(State::default()),
        health,
        broadcast,
    });
    let callback =
        BnHealthInfoCallback::new_binder(Callback(Arc::clone(&battery)), BinderFeatures::default());
    battery.health.registerCallback(&callback)?;
    let properties = BnBatteryPropertiesRegistrar::new_binder(
        PropertiesRegistrar {
            battery: Arc::clone(&battery),
            permissions: Arc::clone(&permissions),
        },
        BinderFeatures::default(),
    );
    let stats = BnBatteryStats::new_binder(
        BatteryStats(battery, permissions),
        BinderFeatures::default(),
    );
    Ok((properties.as_binder(), stats.as_binder()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// BatteryMonitor's answer on a host without power supplies (initHealthInfo + updateValues: status and health UNKNOWN 1, nothing present, level 0, no charger).
    fn no_battery() -> Values {
        Values {
            status: STATUS_UNKNOWN,
            health: 1,
            ..Values::default()
        }
    }

    fn on_battery(level: i32) -> Values {
        // BatteryManager.BATTERY_STATUS_DISCHARGING 3
        Values {
            status: 3,
            present: true,
            level,
            ..no_battery()
        }
    }

    fn on_ac(level: i32) -> Values {
        // BATTERY_STATUS_CHARGING 2
        Values {
            status: 2,
            ac: true,
            ..on_battery(level)
        }
    }

    fn actions(sent: &[(Intent, bool)]) -> Vec<(&str, bool)> {
        sent.iter()
            .map(|(i, s)| (i.action.as_deref().unwrap_or(""), *s))
            .collect()
    }

    /// No battery: the first values force one sticky BATTERY_CHANGED; status UNKNOWN is not on battery, so mCharging keeps its initial true and no low state; equal values send nothing.
    #[test]
    fn desktop_without_battery() {
        let mut st = State::default();
        let t = Instant::now();
        assert_eq!(
            actions(&st.update(no_battery(), t)),
            [(ACTION_BATTERY_CHANGED, true)]
        );
        assert!(st.charging.is_charging(t));
        assert!(!st.low && !st.sent_low);
        assert_eq!(st.plug, PLUGGED_NONE);
        assert!(st.update(no_battery(), t).is_empty());
    }

    /// Unplugged at or below 20 % sends BATTERY_LOW (first values: no previous level above the warning), plugging in sends POWER_CONNECTED, BATTERY_OKAY only at 25 %.
    #[test]
    fn low_battery_and_plug_broadcasts() {
        let mut st = State::default();
        let t = Instant::now();
        assert_eq!(
            actions(&st.update(on_battery(15), t)),
            [(ACTION_BATTERY_LOW, false), (ACTION_BATTERY_CHANGED, true)]
        );
        assert!(st.low && st.sent_low);
        assert!(!st.charging.is_charging(t));
        assert_eq!(
            actions(&st.update(on_ac(16), t)),
            [
                (ACTION_POWER_CONNECTED, false),
                (ACTION_BATTERY_CHANGED, true)
            ]
        );
        assert!(!st.low, "plugged leaves low mode");
        assert_eq!(
            actions(&st.update(on_ac(25), t)),
            [(ACTION_BATTERY_OKAY, false), (ACTION_BATTERY_CHANGED, true)]
        );
    }

    /// Plugged below 90 % after being on battery: the rise is reported as charging only after the 15 min delay; at 90 % at once; a fall while charging is not charging.
    #[test]
    fn charging_machine() {
        let mut c = Charging::default();
        let t = Instant::now();
        c.set_battery_state(true, 50, t);
        assert!(!c.is_charging(t));
        c.set_battery_state(false, 50, t);
        c.set_battery_state(false, 51, t);
        assert!(!c.is_charging(t + CHARGED_DELAY - Duration::from_secs(1)));
        assert!(c.is_charging(t + CHARGED_DELAY));
        c.set_battery_state(false, 49, t + CHARGED_DELAY);
        assert!(!c.is_charging(t + CHARGED_DELAY));
        c.set_battery_state(false, 90, t + CHARGED_DELAY);
        assert!(c.is_charging(t + CHARGED_DELAY));
    }

    /// The deferred set is void when the device is on battery when it fires.
    #[test]
    fn deferred_charging_voided_on_battery() {
        let mut c = Charging::default();
        let t = Instant::now();
        c.set_battery_state(true, 50, t);
        c.set_battery_state(false, 50, t);
        c.set_battery_state(false, 51, t);
        c.set_battery_state(true, 51, t + Duration::from_secs(1));
        assert!(!c.is_charging(t + CHARGED_DELAY));
    }

    /// BATTERY_CHANGED carries BatteryService's extras: scale 100, plugged AC 1, the HAL's level and status.
    // Break: change either Intent.java bit (0x40000000 | 0x20000000).
    #[test]
    fn battery_changed_extras() {
        let mut st = State::default();
        let sent = st.update(on_ac(60), Instant::now());
        let (changed, sticky) = sent.last().expect("sent");
        assert!(sticky);
        assert_eq!(changed.flags, 0x6000_0000);
        let extras = changed
            .extras
            .as_ref()
            .expect("extras")
            .entries()
            .expect("entries");
        for want in [
            ("scale", Value::Int(100)),
            ("plugged", Value::Int(1)),
            ("level", Value::Int(60)),
            ("status", Value::Int(2)),
            ("battery_low", Value::Bool(false)),
        ] {
            assert!(
                extras.contains(&(want.0.to_owned(), want.1.clone())),
                "{want:?}"
            );
        }
    }
}
