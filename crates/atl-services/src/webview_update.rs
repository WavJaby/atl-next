// SPDX-License-Identifier: MIT
//! The `webviewupdate` service (IWebViewUpdateService): WebViewFactory asks it for the provider package (getProvider -> waitForAndGetProvider) once FEATURE_WEBVIEW is declared.
//! Behaviour = WebViewUpdateServiceImpl2 over config_webview_packages' one candidate, `com.android.webview` (availableByDefault, no signatures), on a user build (ro.debuggable=0).
//! Validity (validityResult): providerHasValidSignature passes a non-debuggable build only for a system app, as the config lists no signatures; installs here are never FLAG_SYSTEM,
//! so no installed package is a valid provider: mAnyWebViewInstalled false, mCurrentWebViewPackage null, and waitForAndGetProvider answers LIBLOAD_FAILED_LISTING_WEBVIEW_PACKAGES.
//! AOSP first waits WAIT_TIMEOUT_MS (1 s) for an install broadcast; packages here change only at atld start, so nothing can arrive and the answer is immediate.
//! A system candidate (preinstalled provider) needs the remaining checks (OS compatibility, version code, library meta-data) and the success reply: start fails until that path exists.
//! changeProviderAndSetting, getAllWebViewPackages, getDefaultWebViewPackage (WebViewProviderInfo has no codec): UNKNOWN_TRANSACTION (counted).

use anyhow::{Result, bail};
use atl_aidl::aidl::android::webkit::IWebViewUpdateService::{
    BnWebViewUpdateService, IWebViewUpdateService,
};
use atl_parcelables::android::content::pm::PackageInfo;
use atl_parcelables::android::webkit::{WebViewProviderInfo, WebViewProviderResponse};
use binder::{BinderFeatures, Interface, SpIBinder};

use crate::packages::{FLAG_SYSTEM, Packages};

/// config_webview_packages.xml's only provider (frameworks/base/core/res/res/xml).
pub const CANDIDATE: &str = "com.android.webview";
/// WebViewFactory.LIBLOAD_FAILED_LISTING_WEBVIEW_PACKAGES (WebViewFactory.java:78).
const LIBLOAD_FAILED_LISTING_WEBVIEW_PACKAGES: i32 = 4;

struct WebViewUpdateService;

impl Interface for WebViewUpdateService {}

impl IWebViewUpdateService for WebViewUpdateService {
    /// notifyRelroCreationCompleted: counts a finished relro; none is ever started (no provider), so nothing waits on it.
    fn r#notifyRelroCreationCompleted(&self) -> binder::Result<()> {
        Ok(())
    }
    /// waitForAndGetProvider with no valid provider: mCurrentWebViewPackage (null) and LIBLOAD_FAILED_LISTING_WEBVIEW_PACKAGES (!mAnyWebViewInstalled).
    fn r#waitForAndGetProvider(&self) -> binder::Result<Option<WebViewProviderResponse>> {
        Ok(Some(WebViewProviderResponse {
            package_info: None,
            status: LIBLOAD_FAILED_LISTING_WEBVIEW_PACKAGES,
        }))
    }
    /// getValidWebViewPackages: the candidates passing validityResult, none.
    fn r#getValidWebViewPackages(
        &self,
    ) -> binder::Result<Option<Vec<Option<WebViewProviderInfo>>>> {
        Ok(Some(Vec::new()))
    }
    /// getCurrentWebViewPackageName: the current package's name, null without one (WebViewUpdateService.BinderService).
    fn r#getCurrentWebViewPackageName(&self) -> binder::Result<Option<String>> {
        Ok(None)
    }
    /// getCurrentWebViewPackage: mCurrentWebViewPackage, null.
    fn r#getCurrentWebViewPackage(&self) -> binder::Result<Option<PackageInfo>> {
        Ok(None)
    }
}

/// True for a package findPreferredWebViewPackage could pick: the candidate, installed, a system app (the signature rule above).
fn is_provider_candidate(packages: &Packages) -> bool {
    packages
        .by_name(CANDIDATE)
        .is_some_and(|p| p.app().flags & FLAG_SYSTEM != 0)
}

/// The `webviewupdate` binder, for `addService`; fails while a system `com.android.webview` is installed (its provider path is not built).
pub fn new_binder(packages: &Packages) -> Result<SpIBinder> {
    if is_provider_candidate(packages) {
        bail!("webviewupdate: {CANDIDATE} is a system package; the provider path is not built");
    }
    Ok(
        BnWebViewUpdateService::new_binder(WebViewUpdateService, BinderFeatures::default())
            .as_binder(),
    )
}
