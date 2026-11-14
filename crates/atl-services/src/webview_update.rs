// SPDX-License-Identifier: MIT
//! The `webviewupdate` service (IWebViewUpdateService): WebViewFactory asks it for the provider package (getProvider -> waitForAndGetProvider) once FEATURE_WEBVIEW is declared.
//! Behaviour = WebViewUpdateServiceImpl2 over config_webview_packages' one candidate, `com.android.webview` (availableByDefault, no signatures), on a user build (ro.debuggable=0).
//! The current package is decided once at start (packages change only at atld start): the candidate if installed, enabled and valid (validityResult), else none.
//! With one: waitForAndGetProvider = (LIBLOAD_SUCCESS, its PackageInfo); without: (null, LIBLOAD_FAILED_LISTING_WEBVIEW_PACKAGES), as !mAnyWebViewInstalled.
//! AOSP first waits WAIT_TIMEOUT_MS (1 s) for an install broadcast or a relro; nothing can arrive here and no relro is made (WebViewFactory ignores the relro load), so the answer is immediate.
//! changeProviderAndSetting, getAllWebViewPackages, getDefaultWebViewPackage (WebViewProviderInfo has no codec): UNKNOWN_TRANSACTION (counted).

use anyhow::Result;
use atl_aidl::aidl::android::webkit::IWebViewUpdateService::{
    BnWebViewUpdateService, IWebViewUpdateService,
};
use atl_parcelables::android::content::pm::PackageInfo;
use atl_parcelables::android::webkit::{WebViewProviderInfo, WebViewProviderResponse};
use atl_parcelables::codecs::android::os::Bundle::Value;
use binder::{BinderFeatures, Interface, SpIBinder};

// STOCK_PM_FLAGS = GET_SHARED_LIBRARY_FILES
use crate::package::{FLAG_INSTALLED, GET_SIGNATURES, PackageManager, STOCK_PM_FLAGS};
use crate::packages::{FLAG_SYSTEM, GET_META_DATA, Packages};

/// config_webview_packages.xml's only provider (frameworks/base/core/res/res/xml).
pub const CANDIDATE: &str = "com.android.webview";
/// WebViewFactory.LIBLOAD_SUCCESS, LIBLOAD_FAILED_LISTING_WEBVIEW_PACKAGES (WebViewFactory.java:71, :78).
const LIBLOAD_SUCCESS: i32 = 0;
const LIBLOAD_FAILED_LISTING_WEBVIEW_PACKAGES: i32 = 4;
/// SystemImpl.PACKAGE_FLAGS (SystemImpl.java:298-300) less MATCH_ANY_USER (one user): GET_META_DATA | GET_SIGNATURES | GET_SHARED_LIBRARY_FILES.
const PACKAGE_FLAGS: i64 = GET_META_DATA | GET_SIGNATURES | STOCK_PM_FLAGS;
/// WebViewFactory.getWebViewLibrary's meta-data key (WebViewFactory.java:257).
const WEBVIEW_LIBRARY: &str = "com.android.webview.WebViewLibrary";
/// WebViewFactoryProvider.MINIMUM_SUPPORTED_TARGET_SDK = TIRAMISU (WebViewFactoryProvider.java:43): isCompatibleImplementationPackage without use_b_entry_point (no release value: aconfig default false).
const MINIMUM_SUPPORTED_TARGET_SDK: i32 = 33;

struct WebViewUpdateService {
    /// mCurrentWebViewPackage: the valid candidate, as SystemImpl reads it (PACKAGE_FLAGS)
    current: Option<PackageInfo>,
}

impl Interface for WebViewUpdateService {}

impl IWebViewUpdateService for WebViewUpdateService {
    /// notifyRelroCreationCompleted: counts a finished relro; none is ever started (WebViewFactory ignores the relro load), so nothing waits on it.
    fn r#notifyRelroCreationCompleted(&self) -> binder::Result<()> {
        Ok(())
    }
    /// waitForAndGetProvider: (LIBLOAD_SUCCESS, mCurrentWebViewPackage), or LIBLOAD_FAILED_LISTING_WEBVIEW_PACKAGES with no valid package (!mAnyWebViewInstalled).
    fn r#waitForAndGetProvider(&self) -> binder::Result<Option<WebViewProviderResponse>> {
        Ok(Some(WebViewProviderResponse {
            package_info: self.current.clone(),
            status: if self.current.is_some() {
                LIBLOAD_SUCCESS
            } else {
                LIBLOAD_FAILED_LISTING_WEBVIEW_PACKAGES
            },
        }))
    }
    /// getValidWebViewPackages: the candidates passing validityResult. WebViewProviderInfo has no codec, so a valid one cannot be answered: refused (counted) rather than a false empty list.
    fn r#getValidWebViewPackages(
        &self,
    ) -> binder::Result<Option<Vec<Option<WebViewProviderInfo>>>> {
        if self.current.is_some() {
            return Err(binder::StatusCode::UNKNOWN_TRANSACTION.into());
        }
        Ok(Some(Vec::new()))
    }
    /// getCurrentWebViewPackageName: the current package's name, null without one (WebViewUpdateService.BinderService).
    fn r#getCurrentWebViewPackageName(&self) -> binder::Result<Option<String>> {
        Ok(self.current.as_ref().map(|_| CANDIDATE.to_owned()))
    }
    /// getCurrentWebViewPackage: mCurrentWebViewPackage.
    fn r#getCurrentWebViewPackage(&self) -> binder::Result<Option<PackageInfo>> {
        Ok(self.current.clone())
    }
}

/// Why the candidate is not the current package; AOSP's VALIDITY_* names where one applies.
#[derive(Debug, PartialEq, Eq)]
enum Invalid {
    NotInstalled,
    Disabled,
    OsIncompatible,
    IncorrectSignature,
    NoLibraryFlag,
}

/// WebViewUpdateServiceImpl2.validityResult (:590-608) plus findPreferredWebViewPackage's installed-and-enabled filter, over `info` read with PACKAGE_FLAGS.
/// The version-code check is met by construction: its minimum is the lowest versionCode of the available-by-default system providers, and the candidate is the only one (getMinimumVersionCode).
/// providerHasValidSignature: a user build with no config signatures passes only a system app (:676).
fn validity(info: &PackageInfo) -> Result<(), Invalid> {
    let Some(app) = &info.application_info else {
        return Err(Invalid::NotInstalled);
    };
    if app.flags & FLAG_INSTALLED == 0 {
        return Err(Invalid::NotInstalled);
    }
    if !app.enabled {
        return Err(Invalid::Disabled);
    }
    if app.target_sdk_version < MINIMUM_SUPPORTED_TARGET_SDK {
        return Err(Invalid::OsIncompatible);
    }
    if app.flags & FLAG_SYSTEM == 0 {
        return Err(Invalid::IncorrectSignature);
    }
    let library = app
        .item
        .meta_data
        .as_ref()
        .and_then(|m| m.entries().ok())
        .is_some_and(|e| {
            e.iter()
                .any(|(k, v)| k == WEBVIEW_LIBRARY && matches!(v, Value::String(Some(_))))
        });
    if !library {
        return Err(Invalid::NoLibraryFlag);
    }
    Ok(())
}

/// mCurrentWebViewPackage at start: Ok(None) without the candidate, its PackageInfo when valid, Err(why) for an installed candidate that fails a check (the caller logs it; no current package).
pub fn current_package(
    packages: &Packages,
    pm: &PackageManager,
) -> Result<Option<PackageInfo>, String> {
    let Some(p) = packages.by_name(CANDIDATE) else {
        return Ok(None);
    };
    let info = pm.package_info(p, PACKAGE_FLAGS);
    validity(&info)
        .map(|()| Some(info))
        .map_err(|why| format!("{CANDIDATE} is not a valid provider: {why:?}"))
}

/// The `webviewupdate` binder, for `addService`, over [`current_package`]'s answer.
pub fn new_binder(current: Option<PackageInfo>) -> Result<SpIBinder> {
    Ok(BnWebViewUpdateService::new_binder(
        WebViewUpdateService { current },
        BinderFeatures::default(),
    )
    .as_binder())
}

#[cfg(test)]
mod tests {
    use super::*;
    use atl_parcelables::android::content::pm::ApplicationInfo;
    use atl_parcelables::android::os::Bundle;

    /// A provider as the product builds it: installed, enabled, system, the platform's target SDK, the library meta-data.
    fn provider() -> PackageInfo {
        let mut app = ApplicationInfo {
            flags: FLAG_INSTALLED | FLAG_SYSTEM,
            enabled: true,
            target_sdk_version: MINIMUM_SUPPORTED_TARGET_SDK,
            ..Default::default()
        };
        app.item.meta_data = Some(Bundle::from_strings(&[(
            WEBVIEW_LIBRARY,
            "libatl_webview.so",
        )]));
        PackageInfo {
            application_info: Some(app),
            ..Default::default()
        }
    }

    fn with(edit: impl FnOnce(&mut ApplicationInfo)) -> PackageInfo {
        let mut info = provider();
        edit(info.application_info.as_mut().expect("app"));
        info
    }

    #[test]
    fn a_system_provider_is_valid() {
        assert_eq!(validity(&provider()), Ok(()));
    }

    /// Each validityResult rule rejects on its own (the order follows AOSP's).
    #[test]
    fn each_rule_rejects() {
        let installed = with(|a| a.flags &= !FLAG_INSTALLED);
        assert_eq!(validity(&installed), Err(Invalid::NotInstalled));
        let disabled = with(|a| a.enabled = false);
        assert_eq!(validity(&disabled), Err(Invalid::Disabled));
        let old = with(|a| a.target_sdk_version = MINIMUM_SUPPORTED_TARGET_SDK - 1);
        assert_eq!(validity(&old), Err(Invalid::OsIncompatible));
        let user_app = with(|a| a.flags &= !FLAG_SYSTEM);
        assert_eq!(validity(&user_app), Err(Invalid::IncorrectSignature));
        let no_library = with(|a| a.item.meta_data = Some(Bundle::from_strings(&[("other", "x")])));
        assert_eq!(validity(&no_library), Err(Invalid::NoLibraryFlag));
    }
}
