// SPDX-License-Identifier: MIT
//! android.app.servertransaction.ClientTransaction: app/servertransaction/ClientTransaction.java writeToParcel (writeParcelableList of mTransactionItems) / ClientTransaction(Parcel).
//! Each item is Parcel.writeParcelable: its class name (String16, null for a null item), then its writeToParcel. Modelled: LaunchActivityItem, ResumeActivityItem, PauseActivityItem, StopActivityItem, DestroyActivityItem, StartActivityItem, NewIntentItem, ActivityResultItem, ConfigurationChangeItem, ActivityConfigurationChangeItem, ActivityRelaunchItem; any other item class is refused.

use crate::parcel::*;
use crate::parts::*;

/// The item classes modelled, by `getClass().getName()`.
const LAUNCH_ACTIVITY_ITEM: &str = "android.app.servertransaction.LaunchActivityItem";
const RESUME_ACTIVITY_ITEM: &str = "android.app.servertransaction.ResumeActivityItem";
const ACTIVITY_CONFIGURATION_CHANGE_ITEM: &str =
    "android.app.servertransaction.ActivityConfigurationChangeItem";
const CONFIGURATION_CHANGE_ITEM: &str = "android.app.servertransaction.ConfigurationChangeItem";
const ACTIVITY_RELAUNCH_ITEM: &str = "android.app.servertransaction.ActivityRelaunchItem";
const PAUSE_ACTIVITY_ITEM: &str = "android.app.servertransaction.PauseActivityItem";
const STOP_ACTIVITY_ITEM: &str = "android.app.servertransaction.StopActivityItem";
const DESTROY_ACTIVITY_ITEM: &str = "android.app.servertransaction.DestroyActivityItem";
const START_ACTIVITY_ITEM: &str = "android.app.servertransaction.StartActivityItem";
const NEW_INTENT_ITEM: &str = "android.app.servertransaction.NewIntentItem";
const ACTIVITY_RESULT_ITEM: &str = "android.app.servertransaction.ActivityResultItem";

#[derive(Debug, Clone)]
pub enum ClientTransactionItem {
    /// Boxed: LaunchActivityItem is several hundred bytes, the other variant tens (clippy `large_enum_variant`, denied by the converter's clippy-driver build).
    LaunchActivity(Box<LaunchActivityItem>),
    ResumeActivity(ResumeActivityItem),
    PauseActivity(PauseActivityItem),
    StopActivity(StopActivityItem),
    DestroyActivity(DestroyActivityItem),
    StartActivity(StartActivityItem),
    NewIntent(NewIntentItem),
    /// The results of activities that finished for this one (ActivityResultItem): a token and a list, so not boxed.
    ActivityResult(ActivityResultItem),
    /// Boxed like LaunchActivity: the Configuration and ActivityWindowInfo make it large.
    ActivityConfigurationChange(Box<ActivityConfigurationChangeItem>),
    /// Boxed: a Configuration.
    ConfigurationChange(Box<ConfigurationChangeItem>),
    /// Boxed: three Configurations.
    ActivityRelaunch(Box<ActivityRelaunchItem>),
}

#[derive(Debug, Clone, Default)]
pub struct ClientTransaction {
    /// mTransactionItems; None = a null item
    pub items: Vec<Option<ClientTransactionItem>>,
}

impl Part for ClientTransaction {
    fn write(&self, p: &mut In<'_>) -> Result<()> {
        w_i32(p, len_i32(self.items.len())?)?;
        for item in &self.items {
            match item {
                None => w_str16(p, &None)?,
                Some(ClientTransactionItem::LaunchActivity(i)) => {
                    w_str16(p, &Some(LAUNCH_ACTIVITY_ITEM.to_owned()))?;
                    i.write(p)?;
                }
                Some(ClientTransactionItem::ResumeActivity(i)) => {
                    w_str16(p, &Some(RESUME_ACTIVITY_ITEM.to_owned()))?;
                    i.write(p)?;
                }
                Some(ClientTransactionItem::PauseActivity(i)) => {
                    w_str16(p, &Some(PAUSE_ACTIVITY_ITEM.to_owned()))?;
                    i.write(p)?;
                }
                Some(ClientTransactionItem::StopActivity(i)) => {
                    w_str16(p, &Some(STOP_ACTIVITY_ITEM.to_owned()))?;
                    i.write(p)?;
                }
                Some(ClientTransactionItem::DestroyActivity(i)) => {
                    w_str16(p, &Some(DESTROY_ACTIVITY_ITEM.to_owned()))?;
                    i.write(p)?;
                }
                Some(ClientTransactionItem::StartActivity(i)) => {
                    w_str16(p, &Some(START_ACTIVITY_ITEM.to_owned()))?;
                    i.write(p)?;
                }
                Some(ClientTransactionItem::NewIntent(i)) => {
                    w_str16(p, &Some(NEW_INTENT_ITEM.to_owned()))?;
                    i.write(p)?;
                }
                Some(ClientTransactionItem::ActivityResult(i)) => {
                    w_str16(p, &Some(ACTIVITY_RESULT_ITEM.to_owned()))?;
                    i.write(p)?;
                }
                Some(ClientTransactionItem::ActivityConfigurationChange(i)) => {
                    w_str16(p, &Some(ACTIVITY_CONFIGURATION_CHANGE_ITEM.to_owned()))?;
                    i.write(p)?;
                }
                Some(ClientTransactionItem::ConfigurationChange(i)) => {
                    w_str16(p, &Some(CONFIGURATION_CHANGE_ITEM.to_owned()))?;
                    i.write(p)?;
                }
                Some(ClientTransactionItem::ActivityRelaunch(i)) => {
                    w_str16(p, &Some(ACTIVITY_RELAUNCH_ITEM.to_owned()))?;
                    i.write(p)?;
                }
            }
        }
        Ok(())
    }

    fn read(p: &In<'_>) -> Result<Self> {
        // readParcelableList: -1 leaves the list empty
        let n = r_i32(p)?;
        let mut items = Vec::new();
        for _ in 0..n.max(0) {
            items.push(match r_str16(p)?.as_deref() {
                None => None,
                Some(LAUNCH_ACTIVITY_ITEM) => Some(ClientTransactionItem::LaunchActivity(
                    Box::new(LaunchActivityItem::read(p)?),
                )),
                Some(RESUME_ACTIVITY_ITEM) => Some(ClientTransactionItem::ResumeActivity(
                    ResumeActivityItem::read(p)?,
                )),
                Some(PAUSE_ACTIVITY_ITEM) => Some(ClientTransactionItem::PauseActivity(
                    PauseActivityItem::read(p)?,
                )),
                Some(STOP_ACTIVITY_ITEM) => Some(ClientTransactionItem::StopActivity(
                    StopActivityItem::read(p)?,
                )),
                Some(DESTROY_ACTIVITY_ITEM) => Some(ClientTransactionItem::DestroyActivity(
                    DestroyActivityItem::read(p)?,
                )),
                Some(START_ACTIVITY_ITEM) => Some(ClientTransactionItem::StartActivity(
                    StartActivityItem::read(p)?,
                )),
                Some(NEW_INTENT_ITEM) => {
                    Some(ClientTransactionItem::NewIntent(NewIntentItem::read(p)?))
                }
                Some(ACTIVITY_RESULT_ITEM) => Some(ClientTransactionItem::ActivityResult(
                    ActivityResultItem::read(p)?,
                )),
                Some(ACTIVITY_CONFIGURATION_CHANGE_ITEM) => {
                    Some(ClientTransactionItem::ActivityConfigurationChange(
                        Box::new(ActivityConfigurationChangeItem::read(p)?),
                    ))
                }
                Some(CONFIGURATION_CHANGE_ITEM) => {
                    Some(ClientTransactionItem::ConfigurationChange(Box::new(
                        ConfigurationChangeItem::read(p)?,
                    )))
                }
                Some(ACTIVITY_RELAUNCH_ITEM) => Some(ClientTransactionItem::ActivityRelaunch(
                    Box::new(ActivityRelaunchItem::read(p)?),
                )),
                Some(_) => {
                    return Err(crate::refuse(
                        "android.app.servertransaction.ClientTransactionItem (class not modelled)",
                    ));
                }
            });
        }
        Ok(ClientTransaction { items })
    }
}

crate::codec!(ClientTransaction);
