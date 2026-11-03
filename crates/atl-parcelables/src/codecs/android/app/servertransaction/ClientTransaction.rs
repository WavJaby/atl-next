// SPDX-License-Identifier: MIT
//! android.app.servertransaction.ClientTransaction: app/servertransaction/ClientTransaction.java writeToParcel (writeParcelableList of mTransactionItems) / ClientTransaction(Parcel).
//! Each item is Parcel.writeParcelable: its class name (String16, null for a null item), then its writeToParcel. Modelled: LaunchActivityItem, ResumeActivityItem; any other item class is refused.

use crate::parcel::*;
use crate::parts::*;

/// The item classes modelled, by `getClass().getName()`.
const LAUNCH_ACTIVITY_ITEM: &str = "android.app.servertransaction.LaunchActivityItem";
const RESUME_ACTIVITY_ITEM: &str = "android.app.servertransaction.ResumeActivityItem";

#[derive(Debug, Clone)]
pub enum ClientTransactionItem {
    /// Boxed: LaunchActivityItem is several hundred bytes, the other variant tens (clippy `large_enum_variant`, denied by the converter's clippy-driver build).
    LaunchActivity(Box<LaunchActivityItem>),
    ResumeActivity(ResumeActivityItem),
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
