// SPDX-License-Identifier: MIT
//! `check` step `provider-refusals`: the WebView provider (java/webview-provider) throws UnsupportedOperationException only for the class-C methods in [`CLASS_C`].
//! AOSP's `android.webkit` contract never throws it, and SDKs call WebView methods unconditionally on the main thread, so every other unimplemented method answers the contract's
//! default / failure value / no-op, counted (`Unimplemented.accept` / `answer`). A refusal not in the table fails, and so does a table entry no source refuses (the table cannot rot).

use std::collections::BTreeSet;
use std::path::Path;

use anyhow::{Context, Result, bail};

/// The provider's Java sources.
const DIR: &str = "java/webview-provider/java";

/// (api as passed to `Unimplemented.refuse`, why no contract-legal answer exists).
const CLASS_C: &[(&str, &str)] = &[
    (
        "WebViewProvider.postUrl",
        "a no-op drops the POST navigation (login forms); WebKitGTK's URIRequest carries no body",
    ),
    (
        "WebViewProvider.insertVisualStateCallback",
        "a never-called callback stalls an app that reveals the view on it",
    ),
    (
        "WebViewProvider.createPrintDocumentAdapter",
        "@NonNull adapter; null fails inside the print framework",
    ),
    (
        "WebViewProvider.copyBackForwardList",
        "@NonNull abstract WebBackForwardList; needs an implementation over the engine's list",
    ),
    (
        "WebViewProvider.createWebMessageChannel",
        "@NonNull entangled ports; null or dead ports lose the app's messages",
    ),
    (
        "WebViewProvider.postMessageToMainFrame",
        "a no-op silently drops the app's message to the page",
    ),
    (
        "WebViewFactoryProvider.Statics.clearClientCertPreferences",
        "a no-op never runs onCleared",
    ),
    (
        "WebViewFactoryProvider.Statics.getSafeBrowsingPrivacyPolicyUrl",
        "@NonNull; there is no Safe Browsing policy to name",
    ),
    (
        "WebViewFactoryProvider.getTokenBindingService",
        "abstract result, @hide API (not reachable from the public WebView API)",
    ),
    (
        "WebViewFactoryProvider.getTracingController",
        "@NonNull abstract TracingController; needs an implementation class",
    ),
    (
        "WebViewFactoryProvider.getServiceWorkerController",
        "@NonNull abstract ServiceWorkerController; needs an implementation class",
    ),
    (
        "WebViewFactoryProvider.getWebIconDatabase",
        "abstract WebIconDatabase; needs an implementation class",
    ),
    (
        "WebViewFactoryProvider.getWebViewDatabase",
        "@NonNull abstract WebViewDatabase; needs an implementation class",
    ),
];

pub fn check(root: &Path) -> Result<()> {
    let mut found = BTreeSet::new();
    let mut raw = Vec::new();
    let dir = root.join(DIR);
    for f in java_files(&dir)? {
        let text = std::fs::read_to_string(&f).with_context(|| format!("read {}", f.display()))?;
        let rel = f.strip_prefix(root).unwrap_or(&f).display().to_string();
        let (apis, direct) = scan(&text);
        found.extend(apis);
        raw.extend(direct.into_iter().map(|l| format!("{rel}:{l}")));
    }
    let table: BTreeSet<&str> = CLASS_C.iter().map(|(a, _)| *a).collect();
    let unlisted: Vec<&String> = found
        .iter()
        .filter(|a| !table.contains(a.as_str()))
        .collect();
    let stale: Vec<&&str> = table.iter().filter(|a| !found.contains(**a)).collect();
    if !unlisted.is_empty() || !stale.is_empty() || !raw.is_empty() {
        bail!(
            "provider-refusals: refused but not class C (answer the contract's default, counted: Unimplemented.accept / answer; or list it in provider_refusals.rs CLASS_C with its reason): {unlisted:?}; \
             listed but not refused: {stale:?}; UnsupportedOperationException thrown outside Unimplemented.refuse: {raw:?}"
        );
    }
    eprintln!("provider-refusals: ok ({} class-C methods)", table.len());
    Ok(())
}

/// The `Unimplemented.refuse("...")` apis of one source, and the lines that throw UnsupportedOperationException directly (1-based).
fn scan(text: &str) -> (Vec<String>, Vec<usize>) {
    const CALL: &str = "Unimplemented.refuse(\"";
    let mut apis = Vec::new();
    let mut direct = Vec::new();
    for (i, line) in text.lines().enumerate() {
        // doc and line comments name the call shape, they do not make it
        let t = line.trim_start();
        if t.starts_with('*') || t.starts_with("//") || t.starts_with("/*") {
            continue;
        }
        let mut rest = line;
        while let Some(at) = rest.find(CALL) {
            rest = &rest[at + CALL.len()..];
            if let Some(end) = rest.find('"') {
                apis.push(rest[..end].to_string());
                rest = &rest[end..];
            }
        }
        if line.contains("new UnsupportedOperationException") {
            direct.push(i + 1);
        }
    }
    (apis, direct)
}

fn java_files(dir: &Path) -> Result<Vec<std::path::PathBuf>> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir).with_context(|| format!("read {}", dir.display()))? {
        let p = e?.path();
        if p.is_dir() {
            out.extend(java_files(&p)?);
        } else if p.extension().is_some_and(|x| x == "java") {
            out.push(p);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_finds_refusals_and_direct_throws() {
        let src = "a() { throw Unimplemented.refuse(\"X.a\"); }\nb() { Unimplemented.accept(\"X.b\"); }\nc() { throw new UnsupportedOperationException(); }\n";
        let (apis, direct) = scan(src);
        assert_eq!(apis, ["X.a"]);
        assert_eq!(direct, [3]);
    }

    #[test]
    fn resume_timers_is_not_class_c() {
        // the maintainer's crash (Temple Run, Pangle ad): WebView.resumeTimers must answer, never refuse
        assert!(!CLASS_C.iter().any(|(a, _)| a.ends_with("Timers")
            || a.ends_with("onPause")
            || a.ends_with("onResume")));
    }
}
