//! In-app update check.
//!
//! The release job publishes `latest.json` next to every build. At start the app downloads it
//! (plain HTTPS GET, a few bytes, no GitHub API rate limit), compares the version with its own and,
//! if newer, offers the download link for the current platform. It does not install anything by
//! itself: Android needs the user's tap anyway, and silent installers need a signing key that only
//! the repository owner can provide.

use std::collections::HashMap;
use std::time::Duration;

use serde::{Deserialize, Serialize};

const LATEST_URL: &str = "https://github.com/NoertNet/MumbleFunkClient/releases/download/latest-dev/latest.json";
/// Only links below this prefix are ever opened (see `open_update_url`).
pub const ALLOWED_PREFIX: &str = "https://github.com/NoertNet/MumbleFunkClient/releases/";
/// Local `cargo` builds carry this version; they are not on the release train.
const DEV_VERSION: &str = "0.1.0";

#[derive(Deserialize)]
struct Latest {
    version: String,
    #[serde(default)]
    page: String,
    #[serde(default)]
    assets: HashMap<String, String>,
}

#[derive(Serialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct UpdateInfo {
    pub version: String,
    /// Direct download for this platform.
    pub url: String,
    /// Release page, as fallback.
    pub page: String,
}

#[derive(Serialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CheckResult {
    pub current: String,
    /// True for local builds, which are never offered updates.
    pub dev_build: bool,
    pub update: Option<UpdateInfo>,
}

/// Parse `major.minor.patch` (extra suffixes after `-`/`+` are ignored).
pub fn parse_version(s: &str) -> Option<(u64, u64, u64)> {
    let core = s.trim().trim_start_matches('v').split(['-', '+']).next()?;
    let mut it = core.split('.');
    let v = (it.next()?.parse().ok()?, it.next()?.parse().ok()?, it.next()?.parse().ok()?);
    it.next().is_none().then_some(v)
}

pub fn is_newer(candidate: &str, current: &str) -> bool {
    match (parse_version(candidate), parse_version(current)) {
        (Some(c), Some(cur)) => c > cur,
        _ => false,
    }
}

/// Which entry of `latest.json` fits the running app.
fn asset_key() -> &'static str {
    if cfg!(target_os = "windows") {
        "windows"
    } else if cfg!(target_os = "android") {
        "android"
    } else if std::env::var_os("APPIMAGE").is_some() {
        "linux-appimage"
    } else {
        "linux-deb"
    }
}

fn evaluate(current: &str, latest: &Latest, key: &str) -> Option<UpdateInfo> {
    if !is_newer(&latest.version, current) {
        return None;
    }
    let url = latest.assets.get(key)?.clone();
    // Never hand the UI a link outside our own releases, whatever the file says.
    if !url.starts_with(ALLOWED_PREFIX) {
        return None;
    }
    let page = if latest.page.starts_with(ALLOWED_PREFIX) { latest.page.clone() } else { url.clone() };
    Some(UpdateInfo { version: latest.version.clone(), url, page })
}

/// Blocking; call from a worker thread. Errors are plain strings for the manual "check now" button.
pub fn check_blocking(current: &str) -> Result<CheckResult, String> {
    let dev_build = current == DEV_VERSION;
    if dev_build {
        return Ok(CheckResult { current: current.into(), dev_build, update: None });
    }
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(5)))
        .build()
        .into();
    let body = agent
        .get(LATEST_URL)
        .call()
        .map_err(|e| e.to_string())?
        .into_body()
        .with_config()
        .limit(64 * 1024)
        .read_to_string()
        .map_err(|e| e.to_string())?;
    let latest: Latest = serde_json::from_str(&body).map_err(|e| format!("latest.json: {e}"))?;
    Ok(CheckResult { current: current.into(), dev_build, update: evaluate(current, &latest, asset_key()) })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn latest(version: &str, url: &str) -> Latest {
        Latest {
            version: version.into(),
            page: format!("{ALLOWED_PREFIX}tag/latest-dev"),
            assets: HashMap::from([("android".to_string(), url.to_string())]),
        }
    }

    #[test]
    fn parses_versions() {
        assert_eq!(parse_version("0.1.15"), Some((0, 1, 15)));
        assert_eq!(parse_version("v1.2.3"), Some((1, 2, 3)));
        assert_eq!(parse_version("1.2.3-beta.1"), Some((1, 2, 3)));
        assert_eq!(parse_version("1.2"), None);
        assert_eq!(parse_version("1.2.3.4"), None);
        assert_eq!(parse_version("x.y.z"), None);
    }

    #[test]
    fn compares_numerically_not_textually() {
        assert!(is_newer("0.1.15", "0.1.9"));
        assert!(!is_newer("0.1.9", "0.1.15"));
        assert!(!is_newer("0.1.15", "0.1.15"));
        assert!(is_newer("0.2.0", "0.1.99"));
        assert!(!is_newer("garbage", "0.1.1"));
    }

    #[test]
    fn offers_update_only_when_newer_and_link_is_ours() {
        let good = format!("{ALLOWED_PREFIX}download/latest-dev/MumbleFunk-Android.apk");
        let l = latest("0.1.20", &good);
        let u = evaluate("0.1.19", &l, "android").unwrap();
        assert_eq!((u.version.as_str(), u.url.as_str()), ("0.1.20", good.as_str()));
        assert!(evaluate("0.1.20", &l, "android").is_none(), "same version");
        assert!(evaluate("0.1.19", &l, "windows").is_none(), "no asset for platform");
        let evil = latest("0.1.20", "https://evil.example/app.apk");
        assert!(evaluate("0.1.19", &evil, "android").is_none(), "foreign link is rejected");
    }

    /// The exact text the release job publishes (jq output), so a key or shape mismatch between the
    /// workflow and this parser fails here instead of silently meaning "no update" forever.
    const PUBLISHED: &str = r#"{
      "version": "0.1.15",
      "commit": "8d78210f17d8da8156138b1d6aca561ab3c32013",
      "page": "https://github.com/NoertNet/MumbleFunkClient/releases/tag/latest-dev",
      "assets": {
        "windows": "https://github.com/NoertNet/MumbleFunkClient/releases/download/latest-dev/MumbleFunk-Windows-setup.exe",
        "android": "https://github.com/NoertNet/MumbleFunkClient/releases/download/latest-dev/MumbleFunk-Android.apk",
        "linux-appimage": "https://github.com/NoertNet/MumbleFunkClient/releases/download/latest-dev/MumbleFunk-Linux.AppImage",
        "linux-deb": "https://github.com/NoertNet/MumbleFunkClient/releases/download/latest-dev/MumbleFunk-Linux.deb"
      }
    }"#;

    #[test]
    fn parses_the_published_latest_json() {
        let l: Latest = serde_json::from_str(PUBLISHED).unwrap();
        for (key, file) in [
            ("windows", "MumbleFunk-Windows-setup.exe"),
            ("android", "MumbleFunk-Android.apk"),
            ("linux-appimage", "MumbleFunk-Linux.AppImage"),
            ("linux-deb", "MumbleFunk-Linux.deb"),
        ] {
            let u = evaluate("0.1.14", &l, key).unwrap_or_else(|| panic!("no update offered for {key}"));
            assert!(u.url.ends_with(file), "{key}: {}", u.url);
            assert_eq!(u.version, "0.1.15");
            assert!(u.page.ends_with("tag/latest-dev"));
        }
        assert!(evaluate("0.1.15", &l, "android").is_none());
    }

    #[test]
    fn dev_builds_are_not_checked() {
        let r = check_blocking(DEV_VERSION).unwrap();
        assert!(r.dev_build && r.update.is_none());
    }

    /// Needs network; proves the TLS stack initialises (certificate errors are fine in sandboxes).
    #[test]
    #[ignore]
    fn live_fetch_reaches_tls() {
        match check_blocking("0.1.1") {
            Ok(r) => println!("ok: {r:?}"),
            Err(e) => println!("err: {e}"),
        }
    }
}
