//! Updates. Herdr has no native update command: the "Mettre à jour Herdr DB"
//! action opens a popup running `herdr plugin install <repo>`, whose build
//! step fetches the new binary. The tree checks the latest stable release at
//! most once a day; every pane notices when its binary was replaced on disk.

use crate::paths::REPOSITORY;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");
const CHECK_INTERVAL: Duration = Duration::from_secs(24 * 3600);

#[derive(Debug, Default, Serialize, Deserialize)]
struct CheckRecord {
    checked_at: u64,
    latest: Option<String>,
}

fn record_path(state_dir: &Path) -> PathBuf {
    state_dir.join("update-check.json")
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default().as_secs()
}

/// `Some(version)` when a newer stable release exists. Network at most once
/// a day; failures are silent (offline, rate limit).
pub async fn newer_release(state_dir: &Path) -> Option<String> {
    let path = record_path(state_dir);
    let record: CheckRecord =
        std::fs::read_to_string(&path).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
    let latest = if now_secs().saturating_sub(record.checked_at) < CHECK_INTERVAL.as_secs() {
        record.latest
    } else {
        let latest = fetch_latest().await;
        let record = CheckRecord { checked_at: now_secs(), latest: latest.clone() };
        if let Ok(text) = serde_json::to_string(&record) {
            let _ = herdr_db_store::create_private_dir(state_dir);
            let _ = std::fs::write(&path, text);
            let _ = herdr_db_store::set_private(&path);
        }
        latest
    };
    latest.filter(|v| is_newer(v, CURRENT_VERSION))
}

/// `releases/latest` excludes drafts and pre-releases.
async fn fetch_latest() -> Option<String> {
    let url = format!("https://api.github.com/repos/{REPOSITORY}/releases/latest");
    let output = tokio::time::timeout(
        Duration::from_secs(8),
        tokio::process::Command::new("curl")
            .args(["-fsSL", "--max-time", "6", "-H", "Accept: application/vnd.github+json", &url])
            .stdin(std::process::Stdio::null())
            .output(),
    )
    .await
    .ok()?
    .ok()?;
    if !output.status.success() {
        return None;
    }
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;
    if value.get("prerelease").and_then(|v| v.as_bool()) == Some(true) {
        return None;
    }
    value.get("tag_name")?.as_str().map(|t| t.trim_start_matches('v').to_string())
}

fn parse(version: &str) -> Option<(u64, u64, u64)> {
    let core = version.split(['-', '+']).next()?;
    let mut parts = core.split('.').map(|p| p.parse::<u64>().ok());
    Some((parts.next()??, parts.next().flatten().unwrap_or(0), parts.next().flatten().unwrap_or(0)))
}

pub fn is_newer(candidate: &str, current: &str) -> bool {
    match (parse(candidate), parse(current)) {
        (Some(a), Some(b)) => a > b,
        _ => false,
    }
}

/// Identity of the running binary on disk, to notice a reinstall.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BinaryStamp(Option<(u64, SystemTime)>);

impl BinaryStamp {
    pub fn current() -> BinaryStamp {
        let stamp = std::env::current_exe()
            .ok()
            .and_then(|p| std::fs::metadata(p).ok())
            .and_then(|m| Some((m.len(), m.modified().ok()?)));
        BinaryStamp(stamp)
    }

    pub fn replaced(&self) -> bool {
        self.0.is_some() && BinaryStamp::current() != *self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_ordering() {
        assert!(is_newer("0.1.1", "0.1.0"));
        assert!(is_newer("0.2.0", "0.1.9"));
        assert!(is_newer("1.0", "0.9.9"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.1.0-rc.1", "0.1.0"));
        assert!(!is_newer("garbage", "0.1.0"));
    }
}
