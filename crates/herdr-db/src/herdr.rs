//! Talking to Herdr. A pane never opens a view itself: it asks Herdr to open
//! a plugin pane, which runs the matching subcommand. The socket API is used
//! directly (one JSON request per connection); the `herdr` CLI is the
//! fallback when no socket path is known.

use crate::paths::{HerdrEnv, PLUGIN_ID};
use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placement {
    Split,
    Tab,
    Popup,
}

impl Placement {
    fn as_str(self) -> &'static str {
        match self {
            Placement::Split => "split",
            Placement::Tab => "tab",
            Placement::Popup => "popup",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Right,
    Down,
}

#[derive(Debug, Clone)]
pub struct OpenPane {
    pub entrypoint: &'static str,
    pub placement: Placement,
    pub target_pane: Option<String>,
    pub direction: Option<Direction>,
    pub width: Option<String>,
    pub height: Option<String>,
    pub cwd: Option<PathBuf>,
    pub env: BTreeMap<String, String>,
    pub focus: bool,
}

impl OpenPane {
    pub fn new(entrypoint: &'static str, placement: Placement) -> OpenPane {
        OpenPane {
            entrypoint,
            placement,
            target_pane: None,
            direction: None,
            width: None,
            height: None,
            cwd: None,
            env: BTreeMap::new(),
            focus: true,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Herdr {
    socket: Option<PathBuf>,
    bin: PathBuf,
}

static REQUEST_ID: AtomicU64 = AtomicU64::new(1);

impl Herdr {
    pub fn new(env: &HerdrEnv) -> Herdr {
        let socket = env.socket.clone().or_else(default_socket);
        Herdr { socket, bin: env.bin.clone().unwrap_or_else(|| PathBuf::from("herdr")) }
    }

    pub fn bin(&self) -> &PathBuf {
        &self.bin
    }

    /// One request per connection: `{"id","method","params"}\n`, one line back.
    pub async fn call(&self, method: &str, params: Value) -> Result<Value> {
        let socket = self.socket.as_ref().ok_or_else(|| anyhow!("socket Herdr introuvable"))?;
        let id = format!("herdr-db-{}-{}", std::process::id(), REQUEST_ID.fetch_add(1, Ordering::Relaxed));
        let request = json!({ "id": id, "method": method, "params": params });
        let exchange = async {
            let stream = tokio::net::UnixStream::connect(socket)
                .await
                .with_context(|| format!("connexion au socket Herdr {}", socket.display()))?;
            let (read, mut write) = stream.into_split();
            let mut line = serde_json::to_vec(&request)?;
            line.push(b'\n');
            write.write_all(&line).await?;
            let mut response = String::new();
            BufReader::new(read).read_line(&mut response).await?;
            anyhow::Ok(response)
        };
        let response = tokio::time::timeout(Duration::from_secs(10), exchange)
            .await
            .map_err(|_| anyhow!("Herdr ne répond pas ({method})"))??;
        let value: Value = serde_json::from_str(&response).context("réponse Herdr illisible")?;
        if let Some(error) = value.get("error") {
            let code = error.get("code").and_then(Value::as_str).unwrap_or("error");
            let message = error.get("message").and_then(Value::as_str).unwrap_or("");
            bail!("Herdr {method} : {code} {message}");
        }
        Ok(value.get("result").cloned().unwrap_or(Value::Null))
    }

    /// Opens a plugin pane and returns its pane id when Herdr reports it.
    pub async fn open_pane(&self, open: &OpenPane) -> Result<Option<String>> {
        let mut params = json!({
            "plugin_id": PLUGIN_ID,
            "entrypoint": open.entrypoint,
            "placement": open.placement.as_str(),
            "focus": open.focus,
            "env": open.env,
        });
        let object = params.as_object_mut().expect("object");
        if let Some(target) = &open.target_pane {
            object.insert("target_pane_id".into(), json!(target));
        }
        if let Some(direction) = open.direction {
            object.insert(
                "direction".into(),
                json!(match direction {
                    Direction::Right => "right",
                    Direction::Down => "down",
                }),
            );
        }
        if let Some(width) = &open.width {
            object.insert("width".into(), json!(width));
        }
        if let Some(height) = &open.height {
            object.insert("height".into(), json!(height));
        }
        if let Some(cwd) = &open.cwd {
            object.insert("cwd".into(), json!(cwd));
        }
        if self.socket.is_some() {
            let result = self.call("plugin.pane.open", params).await?;
            return Ok(result.pointer("/plugin_pane/pane/pane_id").and_then(Value::as_str).map(str::to_string));
        }
        self.open_pane_cli(open).await.map(|()| None)
    }

    async fn open_pane_cli(&self, open: &OpenPane) -> Result<()> {
        let mut command = tokio::process::Command::new(&self.bin);
        command.args([
            "plugin",
            "pane",
            "open",
            "--plugin",
            PLUGIN_ID,
            "--entrypoint",
            open.entrypoint,
            "--placement",
            open.placement.as_str(),
        ]);
        if let Some(target) = &open.target_pane {
            command.args(["--target-pane", target]);
        }
        if let Some(direction) = open.direction {
            command.args([
                "--direction",
                match direction {
                    Direction::Right => "right",
                    Direction::Down => "down",
                },
            ]);
        }
        if let Some(cwd) = &open.cwd {
            command.arg("--cwd").arg(cwd);
        }
        for (key, value) in &open.env {
            command.arg("--env").arg(format!("{key}={value}"));
        }
        command.arg(if open.focus { "--focus" } else { "--no-focus" });
        let output = command
            .stdin(std::process::Stdio::null())
            .output()
            .await
            .with_context(|| format!("lancement de {}", self.bin.display()))?;
        if !output.status.success() {
            bail!("herdr plugin pane open : {}", String::from_utf8_lossy(&output.stderr).trim());
        }
        Ok(())
    }

    pub async fn close_plugin_pane(&self, pane_id: &str) -> Result<()> {
        self.call("plugin.pane.close", json!({ "pane_id": pane_id })).await.map(|_| ())
    }

    /// Pane ids of the given tab (all tabs when `None`).
    pub async fn panes(&self, tab_id: Option<&str>) -> Result<Vec<PaneSummary>> {
        let result = self.call("pane.list", json!({})).await?;
        let panes = result.get("panes").and_then(Value::as_array).cloned().unwrap_or_default();
        Ok(panes
            .iter()
            .filter_map(|p| {
                Some(PaneSummary {
                    pane_id: p.get("pane_id")?.as_str()?.to_string(),
                    tab_id: p.get("tab_id").and_then(Value::as_str).map(str::to_string),
                    focused: p.get("focused").and_then(Value::as_bool).unwrap_or(false),
                })
            })
            .filter(|p| tab_id.is_none_or(|t| p.tab_id.as_deref() == Some(t)))
            .collect())
    }

    /// The pane next to `pane_id` in `direction`. The result's `pane_id` is
    /// the queried pane; the neighbor is `neighbor_pane_id`.
    pub async fn neighbor(&self, pane_id: &str, direction: &str) -> Option<String> {
        let result = self.call("pane.neighbor", json!({ "pane_id": pane_id, "direction": direction })).await.ok()?;
        result.pointer("/neighbor/neighbor_pane_id").and_then(Value::as_str).map(str::to_string)
    }

    /// Layout of the tab holding `pane_id`: pane rects and splits.
    pub async fn layout(&self, pane_id: &str) -> Result<Layout> {
        let result = self.call("pane.layout", json!({ "pane_id": pane_id })).await?;
        let layout = result.get("layout").unwrap_or(&result);
        Ok(Layout::parse(layout))
    }

    pub async fn set_split_ratio(&self, pane_id: &str, path: &[bool], ratio: f64) -> Result<()> {
        self.call("layout.set_split_ratio", json!({ "pane_id": pane_id, "path": path, "ratio": ratio }))
            .await
            .map(|_| ())
    }

    /// Herdr 0.9.3 gives a pane opened by `plugin.pane.open` in a split a
    /// stale terminal size (another pane's), and leaves its target's size
    /// unchanged, until the next layout change. Re-applying the root split
    /// ratio is invisible and makes Herdr resize every pane of the tab.
    pub async fn resync_sizes(&self, pane_id: &str) {
        let Ok(layout) = self.layout(pane_id).await else { return };
        if let Some(root) = layout.splits.iter().find(|s| s.path.is_empty())
            && let Err(e) = self.set_split_ratio(pane_id, &[], root.ratio).await
        {
            tracing::debug!(error = %e, "size resync failed");
        }
    }

    pub async fn swap(&self, source: &str, target: &str) -> Result<()> {
        self.call("pane.swap", json!({ "source_pane_id": source, "target_pane_id": target })).await.map(|_| ())
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: i64,
    pub y: i64,
    pub width: i64,
    pub height: i64,
}

impl Rect {
    fn parse(value: &Value) -> Option<Rect> {
        let field = |k: &str| value.get(k).and_then(Value::as_i64);
        Some(Rect { x: field("x")?, y: field("y")?, width: field("width")?, height: field("height")? })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Split {
    /// From the root: `false` first child, `true` second child.
    pub path: Vec<bool>,
    pub horizontal: bool,
    pub ratio: f64,
    pub rect: Rect,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Layout {
    pub area: Option<Rect>,
    pub panes: Vec<(String, Rect)>,
    pub splits: Vec<Split>,
}

impl Layout {
    /// Split ids encode their path: `split_0_root`, `split_5_101`.
    fn parse(value: &Value) -> Layout {
        let panes = value
            .get("panes")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|p| Some((p.get("pane_id")?.as_str()?.to_string(), Rect::parse(p.get("rect")?)?)))
            .collect();
        let splits = value
            .get("splits")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|s| {
                let id = s.get("id")?.as_str()?;
                let bits = id.rsplit('_').next()?;
                let path = if bits == "root" { Vec::new() } else { bits.chars().map(|c| c == '1').collect() };
                Some(Split {
                    path,
                    horizontal: s.get("direction").and_then(Value::as_str) == Some("right"),
                    ratio: s.get("ratio").and_then(Value::as_f64)?,
                    rect: Rect::parse(s.get("rect")?)?,
                })
            })
            .collect();
        Layout { area: value.get("area").and_then(Rect::parse), panes, splits }
    }

    pub fn rect(&self, pane_id: &str) -> Option<Rect> {
        self.panes.iter().find(|(id, _)| id == pane_id).map(|(_, r)| *r)
    }

    /// The left/right split whose first child starts with `pane_id`'s column.
    pub fn column_split(&self, pane_id: &str) -> Option<&Split> {
        let rect = self.rect(pane_id)?;
        self.splits
            .iter()
            .filter(|s| {
                s.horizontal
                    && s.rect.x == rect.x
                    && s.rect.y <= rect.y
                    && s.rect.y + s.rect.height >= rect.y + rect.height
                    && s.rect.width > rect.width
            })
            .min_by_key(|s| s.rect.width)
    }
}

#[derive(Debug, Clone)]
pub struct PaneSummary {
    pub pane_id: String,
    pub tab_id: Option<String>,
    pub focused: bool,
}

/// Default socket of the default session, for runs outside a Herdr pane.
fn default_socket() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    let path = base.join("herdr/herdr.sock");
    path.exists().then_some(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_layout_and_finds_the_tree_column() {
        let value = json!({
            "area": {"x": 0, "y": 0, "width": 134, "height": 44},
            "panes": [
                {"pane_id": "w1:p2", "rect": {"x": 0, "y": 0, "width": 34, "height": 44}},
                {"pane_id": "w1:p3", "rect": {"x": 34, "y": 0, "width": 50, "height": 44}},
                {"pane_id": "w1:p1", "rect": {"x": 84, "y": 0, "width": 50, "height": 44}}
            ],
            "splits": [
                {"id": "split_0_root", "direction": "right", "ratio": 0.25, "rect": {"x": 0, "y": 0, "width": 134, "height": 44}},
                {"id": "split_1_1", "direction": "right", "ratio": 0.5, "rect": {"x": 34, "y": 0, "width": 100, "height": 44}}
            ]
        });
        let layout = Layout::parse(&value);
        assert_eq!(layout.splits[0].path, Vec::<bool>::new());
        assert_eq!(layout.splits[1].path, vec![true]);
        let split = layout.column_split("w1:p3").unwrap();
        assert_eq!(split.path, vec![true]);
        assert!(layout.column_split("w1:p1").is_none(), "last column starts no split");
    }
}
