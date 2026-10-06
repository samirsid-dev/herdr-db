//! Where Herdr DB lives: state dir (cache, requests, tunnels, logs), config
//! dir (personal file), and the Herdr context of the current process.

use serde::Deserialize;
use std::path::{Path, PathBuf};

pub const PLUGIN_ID: &str = "herdr-db";
pub const REPOSITORY: &str = "samirsid-dev/herdr-db";

/// Explicit overrides, also forwarded to the panes this process opens.
pub const ENV_STATE_DIR: &str = "HERDR_DB_STATE_DIR";
pub const ENV_CONFIG_DIR: &str = "HERDR_DB_CONFIG_DIR";
pub const ENV_TEAM_CONFIG: &str = "HERDR_DB_TEAM_CONFIG";

/// `HERDR_PLUGIN_CONTEXT_JSON`, given to actions and hooks.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct PluginContext {
    pub focused_pane_id: Option<String>,
    pub focused_pane_cwd: Option<String>,
    pub tab_id: Option<String>,
    pub workspace_cwd: Option<String>,
    pub worktree: Option<serde_json::Value>,
}

impl PluginContext {
    fn worktree_path(&self) -> Option<PathBuf> {
        let worktree = self.worktree.as_ref()?;
        ["path", "root", "worktree_path"]
            .iter()
            .find_map(|k| worktree.get(k).and_then(|v| v.as_str()))
            .map(PathBuf::from)
    }

    /// Directory the team file is searched from.
    pub fn project_dir(&self) -> Option<PathBuf> {
        self.worktree_path()
            .or_else(|| self.focused_pane_cwd.as_ref().map(PathBuf::from))
            .or_else(|| self.workspace_cwd.as_ref().map(PathBuf::from))
    }
}

#[derive(Debug, Clone)]
pub struct HerdrEnv {
    pub state_dir: PathBuf,
    pub config_dir: PathBuf,
    pub pane_id: Option<String>,
    pub socket: Option<PathBuf>,
    pub bin: Option<PathBuf>,
    pub context: PluginContext,
    pub cwd: PathBuf,
}

fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name).filter(|v| !v.is_empty()).map(PathBuf::from)
}

fn home() -> PathBuf {
    env_path("HOME").unwrap_or_else(|| PathBuf::from("/tmp"))
}

impl HerdrEnv {
    pub fn detect() -> HerdrEnv {
        let state_dir = env_path("HERDR_PLUGIN_STATE_DIR").or_else(|| env_path(ENV_STATE_DIR)).unwrap_or_else(|| {
            env_path("XDG_STATE_HOME").unwrap_or_else(|| home().join(".local/state")).join(PLUGIN_ID)
        });
        let config_dir =
            env_path("HERDR_PLUGIN_CONFIG_DIR").or_else(|| env_path(ENV_CONFIG_DIR)).unwrap_or_else(|| {
                env_path("XDG_CONFIG_HOME")
                    .unwrap_or_else(|| home().join(".config"))
                    .join("herdr/plugins/config")
                    .join(PLUGIN_ID)
            });
        let context = std::env::var("HERDR_PLUGIN_CONTEXT_JSON")
            .ok()
            .and_then(|json| serde_json::from_str(&json).ok())
            .unwrap_or_default();
        HerdrEnv {
            state_dir,
            config_dir,
            pane_id: std::env::var("HERDR_PANE_ID").ok().filter(|v| !v.is_empty()),
            socket: env_path("HERDR_SOCKET_PATH"),
            bin: env_path("HERDR_BIN_PATH"),
            context,
            cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        }
    }

    pub fn inside_herdr(&self) -> bool {
        self.pane_id.is_some() || self.socket.is_some() || std::env::var("HERDR_ENV").is_ok_and(|v| v == "1")
    }

    pub fn personal_config(&self) -> PathBuf {
        self.config_dir.join(herdr_db_core::config::PERSONAL_FILE_NAME)
    }

    pub fn logs_dir(&self) -> PathBuf {
        self.state_dir.join("logs")
    }

    /// Team file: explicit path, else searched upward from the project dir.
    pub fn team_config(&self) -> Option<PathBuf> {
        if let Some(path) = env_path(ENV_TEAM_CONFIG) {
            return path.is_file().then_some(path);
        }
        let start = self.context.project_dir().unwrap_or_else(|| self.cwd.clone());
        find_upward(&start, herdr_db_core::config::TEAM_FILE_NAME)
    }

    /// Identifies this process for leases and claimed requests.
    pub fn holder_id(&self) -> String {
        let pid = std::process::id();
        match &self.pane_id {
            Some(pane) => format!("{}-{pid}", sanitize(pane)),
            None => format!("pid-{pid}"),
        }
    }
}

pub fn find_upward(start: &Path, name: &str) -> Option<PathBuf> {
    let mut dir = Some(start);
    while let Some(d) = dir {
        let candidate = d.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
        dir = d.parent();
    }
    None
}

/// Pane ids look like `w1:p3`: keep them file-name safe.
pub fn sanitize(value: &str) -> String {
    value.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn team_file_is_found_upward() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("a/b");
        std::fs::create_dir_all(&nested).unwrap();
        assert_eq!(find_upward(&nested, "herdr-db.toml"), None);
        std::fs::write(dir.path().join("herdr-db.toml"), "").unwrap();
        assert_eq!(find_upward(&nested, "herdr-db.toml"), Some(dir.path().join("herdr-db.toml")));
    }

    #[test]
    fn context_prefers_worktree() {
        let context: PluginContext =
            serde_json::from_str(r#"{"focused_pane_cwd": "/a", "workspace_cwd": "/b", "worktree": {"path": "/c"}}"#)
                .unwrap();
        assert_eq!(context.project_dir(), Some(PathBuf::from("/c")));
        assert_eq!(sanitize("w1:p3"), "w1_p3");
    }
}
