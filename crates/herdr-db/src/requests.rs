//! Handing the targeted object to a new pane. Primary channel: the
//! `HERDR_DB_REQUEST` variable given to `plugin.pane.open`. Fallback, for a
//! host that would not forward the environment: a queue in the state dir.
//!
//! 1. The sender writes `requests/<entrypoint>/<timestamp>-<random>.json`
//!    atomically (temporary file, then rename) and passes its name as token.
//! 2. The new pane reads the variable and deletes the queue file named by the
//!    token; without the variable it claims the oldest file of its
//!    entrypoint by renaming it to `claimed/<holder>.json`. The rename is
//!    atomic: two panes cannot claim the same request.
//! 3. Requests older than [`MAX_AGE`] are ignored; without a request the pane
//!    shows a selector.

use herdr_db_core::request::{ENV_VAR, PaneRequest};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const MAX_AGE: Duration = Duration::from_secs(10);

fn queue_dir(state_dir: &Path, entrypoint: &str) -> PathBuf {
    state_dir.join("requests").join(entrypoint)
}

fn claimed_dir(state_dir: &Path) -> PathBuf {
    state_dir.join("requests").join("claimed")
}

fn now_millis(now: SystemTime) -> u128 {
    now.duration_since(UNIX_EPOCH).unwrap_or_default().as_millis()
}

fn random_suffix() -> String {
    // No RNG dependency: the clock, the pid and a counter are unique enough
    // for files that live ten seconds.
    use std::sync::atomic::{AtomicU32, Ordering};
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().subsec_nanos();
    format!("{:x}{:x}{:x}", std::process::id(), nanos, COUNTER.fetch_add(1, Ordering::Relaxed))
}

/// Writes the queue file and returns the request carrying its token, ready
/// to be serialized into the environment variable.
pub fn enqueue(state_dir: &Path, request: &PaneRequest, now: SystemTime) -> std::io::Result<PaneRequest> {
    let dir = queue_dir(state_dir, request.action.entrypoint());
    herdr_db_store::create_private_dir(&dir)?;
    let name = format!("{:013}-{}.json", now_millis(now), random_suffix());
    let mut request = request.clone();
    request.token = Some(name.clone());
    let tmp = dir.join(format!(".{name}.tmp"));
    {
        let mut file = fs::File::create(&tmp)?;
        herdr_db_store::set_private(&tmp)?;
        file.write_all(request.to_json().as_bytes())?;
        file.sync_all()?;
    }
    fs::rename(&tmp, dir.join(&name))?;
    Ok(request)
}

/// The request for this pane: environment first, then the queue.
pub fn receive(state_dir: &Path, entrypoint: &str, holder: &str, now: SystemTime) -> Option<PaneRequest> {
    if let Some(request) = std::env::var(ENV_VAR).ok().and_then(|json| PaneRequest::from_json(&json)) {
        if let Some(token) = &request.token
            && !token.contains('/')
        {
            let _ = fs::remove_file(queue_dir(state_dir, entrypoint).join(token));
        }
        return Some(request);
    }
    claim(state_dir, entrypoint, holder, now)
}

pub fn claim(state_dir: &Path, entrypoint: &str, holder: &str, now: SystemTime) -> Option<PaneRequest> {
    let dir = queue_dir(state_dir, entrypoint);
    let mut names: Vec<String> = fs::read_dir(&dir)
        .ok()?
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .filter(|n| n.ends_with(".json") && !n.starts_with('.'))
        .collect();
    names.sort();
    let claimed = claimed_dir(state_dir);
    herdr_db_store::create_private_dir(&claimed).ok()?;
    let cutoff = now_millis(now).saturating_sub(MAX_AGE.as_millis());
    for name in names {
        let path = dir.join(&name);
        let stamp: u128 = name.split('-').next().and_then(|s| s.parse().ok()).unwrap_or(0);
        if stamp < cutoff {
            let _ = fs::remove_file(&path);
            continue;
        }
        let target = claimed.join(format!("{holder}.json"));
        if fs::rename(&path, &target).is_ok() {
            let text = fs::read_to_string(&target).ok();
            let _ = fs::remove_file(&target);
            if let Some(request) = text.as_deref().and_then(PaneRequest::from_json) {
                return Some(request);
            }
        }
    }
    None
}

/// Startup cleanup: removes stale queued and claimed requests.
pub fn purge(state_dir: &Path, now: SystemTime) -> usize {
    let root = state_dir.join("requests");
    let cutoff = now_millis(now).saturating_sub(MAX_AGE.as_millis());
    let mut removed = 0;
    let Ok(entries) = fs::read_dir(&root) else {
        return 0;
    };
    for dir in entries.filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.is_dir()) {
        let is_claimed = dir.file_name().is_some_and(|n| n == "claimed");
        for file in fs::read_dir(&dir).into_iter().flatten().filter_map(|e| e.ok()) {
            let name = file.file_name().to_string_lossy().into_owned();
            let stamp: u128 = name.trim_start_matches('.').split('-').next().and_then(|s| s.parse().ok()).unwrap_or(0);
            if (is_claimed || stamp < cutoff) && fs::remove_file(file.path()).is_ok() {
                removed += 1;
            }
        }
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;
    use herdr_db_core::model::ObjectRef;
    use herdr_db_core::request::Action;

    fn request() -> PaneRequest {
        PaneRequest::for_object(Action::EditData, ObjectRef::new("db", "public", "users"))
    }

    #[test]
    fn oldest_request_is_claimed_once() {
        let dir = tempfile::tempdir().unwrap();
        let now = SystemTime::now();
        let first = enqueue(dir.path(), &request(), now - Duration::from_secs(2)).unwrap();
        let mut other = request();
        other.object.as_mut().unwrap().name = "orders".into();
        enqueue(dir.path(), &other, now).unwrap();

        let claimed = claim(dir.path(), "grid", "w1_p2-1", now).unwrap();
        assert_eq!(claimed.token, first.token);
        assert_eq!(claimed.object.unwrap().name, "users");
        assert_eq!(claim(dir.path(), "grid", "w1_p3-1", now).unwrap().object.unwrap().name, "orders");
        assert!(claim(dir.path(), "grid", "w1_p4-1", now).is_none());
        assert!(claim(dir.path(), "ddl", "w1_p4-1", now).is_none());
    }

    #[test]
    fn stale_requests_are_ignored_and_purged() {
        let dir = tempfile::tempdir().unwrap();
        let now = SystemTime::now();
        enqueue(dir.path(), &request(), now - Duration::from_secs(30)).unwrap();
        assert!(claim(dir.path(), "grid", "h", now).is_none());
        enqueue(dir.path(), &request(), now - Duration::from_secs(30)).unwrap();
        enqueue(dir.path(), &request(), now).unwrap();
        assert_eq!(purge(dir.path(), now), 1);
        assert!(claim(dir.path(), "grid", "h", now).is_some());
    }

    #[test]
    fn queue_files_are_private() {
        let dir = tempfile::tempdir().unwrap();
        let sent = enqueue(dir.path(), &request(), SystemTime::now()).unwrap();
        let path = dir.path().join("requests/grid").join(sent.token.unwrap());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }
    }
}
