// SPDX-License-Identifier: MIT
//! The desktop's UI language from the process's locale variables; the pure rule is `atl_platform_host_api::ui_locale_from`.

/// The UI language as one BCP-47 tag from `LC_ALL`, `LC_MESSAGES`, `LANG`; `None` for C/POSIX, none set, or a malformed name.
pub fn ui_locale() -> Option<String> {
    atl_platform_host_api::ui_locale_from(|name| std::env::var(name).ok())
}
