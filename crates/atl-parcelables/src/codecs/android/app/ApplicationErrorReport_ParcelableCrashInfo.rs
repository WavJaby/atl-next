// SPDX-License-Identifier: MIT
//! android.app.ApplicationErrorReport.ParcelableCrashInfo (ApplicationErrorReport.java:491, which inherits CrashInfo.writeToParcel :447-457 and reads back through CrashInfo(Parcel) :432-442):
//! the crash report an app's process sends as it dies (RuntimeInit's KillApplicationHandler, through IActivityManager.handleApplicationCrash). Nine Parcel.writeString fields (String16, null as -1)
//! and one int32, in this order; no nested parcelable.

use crate::parcel::*;
use crate::parts::*;

/// Java's field order: exceptionHandlerClassName, exceptionClassName, exceptionMessage, throwFileName, throwClassName, throwMethodName, throwLineNumber, stackTrace, crashTag.
#[allow(non_camel_case_types)]
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ApplicationErrorReport_ParcelableCrashInfo {
    pub exception_handler_class_name: Option<String>,
    pub exception_class_name: Option<String>,
    pub exception_message: Option<String>,
    pub throw_file_name: Option<String>,
    pub throw_class_name: Option<String>,
    pub throw_method_name: Option<String>,
    pub throw_line_number: i32,
    pub stack_trace: Option<String>,
    pub crash_tag: Option<String>,
}

impl Part for ApplicationErrorReport_ParcelableCrashInfo {
    fn write(&self, p: &mut In<'_>) -> Result<()> {
        w_str16(p, &self.exception_handler_class_name)?;
        w_str16(p, &self.exception_class_name)?;
        w_str16(p, &self.exception_message)?;
        w_str16(p, &self.throw_file_name)?;
        w_str16(p, &self.throw_class_name)?;
        w_str16(p, &self.throw_method_name)?;
        w_i32(p, self.throw_line_number)?;
        w_str16(p, &self.stack_trace)?;
        w_str16(p, &self.crash_tag)
    }

    fn read(p: &In<'_>) -> Result<Self> {
        Ok(ApplicationErrorReport_ParcelableCrashInfo {
            exception_handler_class_name: r_str16(p)?,
            exception_class_name: r_str16(p)?,
            exception_message: r_str16(p)?,
            throw_file_name: r_str16(p)?,
            throw_class_name: r_str16(p)?,
            throw_method_name: r_str16(p)?,
            throw_line_number: r_i32(p)?,
            stack_trace: r_str16(p)?,
            crash_tag: r_str16(p)?,
        })
    }
}

crate::codec!(ApplicationErrorReport_ParcelableCrashInfo);
