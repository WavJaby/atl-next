// SPDX-License-Identifier: MIT
//! Where ATL keeps a user's persistent state, as a native Linux program does: the XDG Base Directory spec's data home (`$XDG_DATA_HOME`, else `$HOME/.local/share`), under `atl/`.
//! The spec ignores a relative `XDG_DATA_HOME` ("all paths ... must be absolute"), so a relative or empty value falls back to `HOME`; a relative or empty `HOME` gives no dir.

use std::ffi::OsString;
use std::path::PathBuf;

/// The default data root (atld `--data-root`): `<data home>/atl`, one per user, kept across runs. `None` when neither variable names an absolute dir.
pub fn user_data_root() -> Option<PathBuf> {
    data_root_in(std::env::var_os("XDG_DATA_HOME"), std::env::var_os("HOME"))
}

/// [`user_data_root`] for the values `xdg_data_home` and `home`.
fn data_root_in(xdg_data_home: Option<OsString>, home: Option<OsString>) -> Option<PathBuf> {
    let absolute = |v: Option<OsString>| v.map(PathBuf::from).filter(|p| p.is_absolute());
    let data_home =
        absolute(xdg_data_home).or_else(|| absolute(home).map(|h| h.join(".local/share")))?;
    Some(data_home.join("atl"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root(xdg: Option<&str>, home: Option<&str>) -> Option<PathBuf> {
        data_root_in(xdg.map(OsString::from), home.map(OsString::from))
    }

    /// XDG spec: an absolute XDG_DATA_HOME wins; else $HOME/.local/share.
    #[test]
    fn data_home_then_home() {
        assert_eq!(
            root(Some("/x/data"), Some("/home/u")),
            Some(PathBuf::from("/x/data/atl"))
        );
        assert_eq!(
            root(None, Some("/home/u")),
            Some(PathBuf::from("/home/u/.local/share/atl"))
        );
    }

    /// Relative or empty values are not dirs per the spec: fall back, or none.
    #[test]
    fn relative_or_empty_is_ignored() {
        assert_eq!(
            root(Some("rel"), Some("/home/u")),
            Some(PathBuf::from("/home/u/.local/share/atl"))
        );
        assert_eq!(
            root(Some(""), Some("/home/u")),
            Some(PathBuf::from("/home/u/.local/share/atl"))
        );
        assert_eq!(root(None, Some("")), None);
        assert_eq!(root(None, None), None);
    }
}
