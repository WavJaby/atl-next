// SPDX-License-Identifier: MIT
//! android.window.ImeOnBackInvokedDispatcher (window/ImeOnBackInvokedDispatcher.java writeToParcel:136-138): writeTypedObject of its ResultReceiver, which writes one strong binder
//! (os/ResultReceiver.java writeToParcel:114-121). Passed by IInputMethodManager.startInputOrWindowGainedFocusAsync; the IME would send back-callback results through the binder.

use binder::SpIBinder;

use crate::parcel::*;
use crate::parts::*;

/// The ResultReceiver's binder; None = a null ResultReceiver (presence 0).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ImeOnBackInvokedDispatcher {
    pub result_receiver: Option<Option<SpIBinder>>,
}

impl Part for ImeOnBackInvokedDispatcher {
    fn write(&self, p: &mut In<'_>) -> Result<()> {
        w_present(p, self.result_receiver.is_some())?;
        if let Some(b) = &self.result_receiver {
            w_binder(p, b)?;
        }
        Ok(())
    }

    fn read(p: &In<'_>) -> Result<Self> {
        let result_receiver = if r_present(p)? {
            Some(r_binder(p)?)
        } else {
            None
        };
        Ok(ImeOnBackInvokedDispatcher { result_receiver })
    }
}

crate::codec!(ImeOnBackInvokedDispatcher);
