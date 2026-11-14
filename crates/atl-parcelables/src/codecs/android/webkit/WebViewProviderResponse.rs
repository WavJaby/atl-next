// SPDX-License-Identifier: MIT
//! android.webkit.WebViewProviderResponse (frameworks/base/core/java/android/webkit/WebViewProviderResponse.java writeToParcel / WebViewProviderResponse(Parcel)): the provider package as
//! writeTypedObject (null = no valid provider), then the status, one of WebViewFactory.LIBLOAD_*. IWebViewUpdateService.waitForAndGetProvider returns it.

use crate::codecs::android::content::pm::PackageInfo::PackageInfo;
use crate::parcel::*;
use crate::parts::*;

#[derive(Debug, Clone, Default)]
pub struct WebViewProviderResponse {
    pub package_info: Option<PackageInfo>,
    /// WebViewFactory.LIBLOAD_* (LIBLOAD_SUCCESS 0, LIBLOAD_FAILED_WAITING_FOR_RELRO 3, LIBLOAD_FAILED_LISTING_WEBVIEW_PACKAGES 4, ...)
    pub status: i32,
}

impl Part for WebViewProviderResponse {
    fn write(&self, p: &mut In<'_>) -> Result<()> {
        w_typed(p, &self.package_info)?;
        w_i32(p, self.status)
    }

    fn read(p: &In<'_>) -> Result<Self> {
        Ok(WebViewProviderResponse {
            package_info: r_typed(p)?,
            status: r_i32(p)?,
        })
    }
}

crate::codec!(WebViewProviderResponse);
