//! Opening a view from an action. An action never draws the view itself: it
//! asks Herdr to open a plugin pane running the matching subcommand, with
//! the targeted object in the environment and in the request queue.

use crate::herdr::{Direction, Herdr, OpenPane, Placement};
use crate::paths::{ENV_CONFIG_DIR, ENV_STATE_DIR, ENV_TEAM_CONFIG, HerdrEnv};
use crate::requests;
use anyhow::{Result, bail};
use herdr_db_core::config::GridPlacement;
use herdr_db_core::request::{Action, ENV_VAR, PaneRequest};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;

/// Which pane triggers the action: placement depends on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    Tree,
    Grid,
    Other,
}

#[derive(Clone)]
pub struct Opener {
    pub env: Arc<HerdrEnv>,
    pub herdr: Herdr,
    pub grid_placement: GridPlacement,
    pub team_file: Option<PathBuf>,
}

impl Opener {
    pub async fn open(&self, request: PaneRequest, origin: Origin) -> Result<()> {
        if !self.env.inside_herdr() {
            bail!(
                "hors de Herdr : lancez `herdr-db {}` avec {ENV_VAR}='{}'",
                request.action.entrypoint(),
                request.to_json()
            );
        }
        let queued = requests::enqueue(&self.env.state_dir, &request, SystemTime::now())?;
        let mut open = OpenPane::new(request.action.entrypoint(), Placement::Split);
        open.env.insert(ENV_VAR.to_string(), queued.to_json());
        open.env.insert(ENV_STATE_DIR.to_string(), self.env.state_dir.display().to_string());
        open.env.insert(ENV_CONFIG_DIR.to_string(), self.env.config_dir.display().to_string());
        if let Some(team) = &self.team_file {
            open.env.insert(ENV_TEAM_CONFIG.to_string(), team.display().to_string());
        }
        open.cwd = Some(self.env.cwd.clone());
        let own = self.env.pane_id.clone();

        match request.action {
            Action::QuickDoc => {
                open.placement = Placement::Popup;
                open.width = Some("70%".into());
                open.height = Some("60%".into());
            }
            Action::GoToDdl => open.placement = Placement::Tab,
            Action::EditData if self.grid_placement == GridPlacement::Tab => open.placement = Placement::Tab,
            Action::Console if origin == Origin::Grid => {
                // The console opens in a split under the grid.
                open.target_pane = own;
                open.direction = Some(Direction::Down);
            }
            Action::EditData | Action::Console => match (origin, &own) {
                (Origin::Tree, Some(tree)) => match self.herdr.neighbor(tree, "right").await {
                    Some(main) => {
                        open.target_pane = Some(main);
                        open.direction = Some(Direction::Down);
                    }
                    None => {
                        open.target_pane = Some(tree.clone());
                        open.direction = Some(Direction::Right);
                    }
                },
                (_, Some(pane)) => {
                    open.target_pane = Some(pane.clone());
                    open.direction = Some(Direction::Down);
                }
                (_, None) => open.direction = Some(Direction::Right),
            },
        }
        self.herdr.open_pane(&open).await?;
        Ok(())
    }
}
