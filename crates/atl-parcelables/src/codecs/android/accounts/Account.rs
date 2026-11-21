// SPDX-License-Identifier: MIT
//! android.accounts.Account (accounts/Account.java writeToParcel :138-142 / Account(Parcel) :95-104): three String16s, name, type, accessId (null unless the account reached the caller through a visibility grant).
//! Account(Parcel) throws BadParcelableException for an empty name or type -> refused here. The account service sends only empty Account[] today (no store, plan S17); the codec lets an app's own
//! Account arguments (addAccountExplicitly, getPassword, ...) decode.

use crate::parcel::*;
use crate::parts::*;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Account {
    pub name: String,
    pub r#type: String,
    /// Account.accessId (@hide)
    pub access_id: Option<String>,
}

impl Part for Account {
    fn write(&self, p: &mut In<'_>) -> Result<()> {
        w_str16(p, &Some(self.name.clone()))?;
        w_str16(p, &Some(self.r#type.clone()))?;
        w_str16(p, &self.access_id)
    }

    fn read(p: &In<'_>) -> Result<Self> {
        let name = r_str16(p)?.unwrap_or_default();
        let r#type = r_str16(p)?.unwrap_or_default();
        if name.is_empty() || r#type.is_empty() {
            return Err(crate::refuse(
                "android.accounts.Account (empty name or type)",
            ));
        }
        Ok(Account {
            name,
            r#type,
            access_id: r_str16(p)?,
        })
    }
}

crate::codec!(Account);
