// SPDX-License-Identifier: MIT
//! android.view.inputmethod.InputBinding (InputBinding.java writeToParcel:131-135): the connection token (strong binder), the client's uid and pid. IMMS-lite sends it in IInputMethod.bindInput, as
//! InputMethodManagerService.attachNewInputLocked does with the client's binding.

use binder::SpIBinder;

use crate::parcel::*;
use crate::parts::*;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct InputBinding {
    pub connection_token: Option<SpIBinder>,
    pub uid: i32,
    pub pid: i32,
}

impl Part for InputBinding {
    fn write(&self, p: &mut In<'_>) -> Result<()> {
        w_binder(p, &self.connection_token)?;
        w_i32(p, self.uid)?;
        w_i32(p, self.pid)
    }

    fn read(p: &In<'_>) -> Result<Self> {
        Ok(InputBinding {
            connection_token: r_binder(p)?,
            uid: r_i32(p)?,
            pid: r_i32(p)?,
        })
    }
}

crate::codec!(InputBinding);
