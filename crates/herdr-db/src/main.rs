//! `herdr-db`: one binary serving every Herdr DB pane. Herdr runs one
//! subcommand per manifest entrypoint; panes share the metadata model
//! through the SQLite cache in the plugin state dir, without a daemon.

mod clipboard;
mod db;
mod herdr;
mod introspect;
mod logging;
mod paths;
mod requests;
mod secrets;
mod settings;
mod tunnel;
mod ui;
mod update;
mod views;

use anyhow::{Context, Result, anyhow, bail};
use clap::{Parser, Subcommand};
use herdr::{Direction, Herdr, OpenPane, Placement};
use herdr_db_core::config::{Config, SourceConfig};
use herdr_db_core::model::ObjectRef;
use herdr_db_core::readonly_role::{RoleScriptInput, readonly_role_script};
use herdr_db_core::request::{Action, PaneRequest};
use paths::HerdrEnv;
use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::SystemTime;
use ui::keys::Keymap;
use ui::picker::{Fatal, NoEffects, PickItem, Picker};
use ui::theme::Icons;
use views::Opener;

#[derive(Parser)]
#[command(name = "herdr-db", version, about = "Database explorer for Herdr")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Database tree (pane entrypoint).
    Tree,
    /// Read-only data grid (pane entrypoint).
    Grid,
    /// SQL console (pane entrypoint).
    Console,
    /// Go to DDL (pane entrypoint).
    Ddl,
    /// Quick Documentation (popup entrypoint).
    Quickdoc,
    /// Show or hide the database tree in the current tab (action).
    ToggleTree,
    /// Open the update popup (action).
    Update,
    /// Run `herdr plugin install` for this plugin (popup entrypoint).
    SelfUpdate,
    /// Cleanup when the Herdr server starts (startup hook).
    Startup,
    /// Administration helpers.
    Admin {
        #[command(subcommand)]
        command: AdminCommand,
    },
    /// Store or delete a password in the OS keyring.
    Secret {
        #[command(subcommand)]
        command: SecretCommand,
    },
    /// Delete the metadata cache of a source (rebuilt by the next introspection).
    ForgetCache {
        #[arg(long)]
        source: String,
    },
    /// Print paths, configuration files and data sources.
    Doctor,
}

#[derive(Subcommand)]
enum AdminCommand {
    /// Print the SQL creating per-person read-only roles. Never runs it.
    ReadonlyRole {
        #[arg(long)]
        source: String,
        /// One login role per person (repeat the flag).
        #[arg(long = "user")]
        users: Vec<String>,
        /// Role running migrations, for PostgreSQL default privileges.
        #[arg(long)]
        owner: Option<String>,
    },
}

#[derive(Subcommand)]
enum SecretCommand {
    /// Prompt for a password and store it in the keyring.
    Set {
        #[arg(long)]
        source: String,
        #[arg(long)]
        user: Option<String>,
    },
    /// Remove a stored password.
    Delete {
        #[arg(long)]
        source: String,
        #[arg(long)]
        user: Option<String>,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let runtime = match tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build() {
        Ok(runtime) => runtime,
        Err(e) => {
            eprintln!("herdr-db: {e}");
            return ExitCode::FAILURE;
        }
    };
    let result = runtime.block_on(run(cli));
    // Pending tasks hold connections and tunnel leases: drop them now.
    runtime.shutdown_timeout(std::time::Duration::from_secs(2));
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("herdr-db: {e:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> Result<()> {
    let env = Arc::new(HerdrEnv::detect());
    logging::init(&env.logs_dir());
    tracing::info!(version = update::CURRENT_VERSION, pane = ?env.pane_id, "start");
    match cli.command {
        Command::Tree => in_pane(run_tree(env)).await,
        Command::Grid => in_pane(run_view(env, Action::EditData)).await,
        Command::Console => in_pane(run_view(env, Action::Console)).await,
        Command::Ddl => in_pane(run_view(env, Action::GoToDdl)).await,
        Command::Quickdoc => in_pane(run_view(env, Action::QuickDoc)).await,
        Command::ToggleTree => toggle_tree(&env).await,
        Command::Update => open_update_popup(&env).await,
        Command::SelfUpdate => self_update(&env).await,
        Command::Startup => {
            startup(&env);
            Ok(())
        }
        Command::Admin { command: AdminCommand::ReadonlyRole { source, users, owner } } => {
            let config = settings::load(&env)?;
            let source = find_source(&config, &source)?;
            print!(
                "{}",
                readonly_role_script(&RoleScriptInput {
                    engine: source.engine,
                    source_id: &source.id,
                    database: &source.database,
                    schemas: &source.schemas,
                    users: &users,
                    owner: owner.as_deref(),
                })
            );
            Ok(())
        }
        Command::Secret { command } => secret(&env, command),
        Command::ForgetCache { source } => {
            herdr_db_store::forget(&env.state_dir, &source)?;
            println!("cache de {source} supprimé");
            Ok(())
        }
        Command::Doctor => doctor(&env),
    }
}

/// A pane that fails shows its error until dismissed instead of vanishing.
async fn in_pane(future: impl std::future::Future<Output = Result<()>>) -> Result<()> {
    if let Err(e) = future.await {
        tracing::error!(error = %format!("{e:#}"), "pane failed");
        ui::run(Fatal::new(&format!("{e:#}")), NoEffects).await?;
    }
    Ok(())
}

fn find_source<'a>(config: &'a Config, id: &str) -> Result<&'a SourceConfig> {
    config.source(id).ok_or_else(|| {
        let known: Vec<&str> = config.sources.iter().map(|s| s.id.as_str()).collect();
        anyhow!(
            "data source `{id}` inconnue (sources : {})",
            if known.is_empty() { "aucune".to_string() } else { known.join(", ") }
        )
    })
}

fn opener(env: &Arc<HerdrEnv>, config: &Config) -> Opener {
    Opener {
        env: env.clone(),
        herdr: Herdr::new(env),
        grid_placement: config.settings.grid_placement,
        team_file: config.team_file.clone(),
    }
}

async fn run_tree(env: Arc<HerdrEnv>) -> Result<()> {
    let config = settings::load(&env)?;
    let registration = register_tree(&env);
    let icons = Icons::new(config.settings.icons);
    let keys = Keymap::new(&config.settings.keys);
    let exec = ui::tree::TreeExec::new(env.clone(), opener(&env, &config));
    let result = ui::run(ui::tree::Tree::new(config, icons, keys), exec).await.map(|_| ());
    if let Some(path) = registration {
        let _ = std::fs::remove_file(path);
    }
    result
}

/// Records this tree pane for `toggle-tree`: file named after the pane,
/// holding its id and our PID.
fn register_tree(env: &HerdrEnv) -> Option<std::path::PathBuf> {
    let pane = env.pane_id.as_ref()?;
    let dir = ui::tree::registry_dir(&env.state_dir);
    herdr_db_store::create_private_dir(&dir).ok()?;
    let path = dir.join(paths::sanitize(pane));
    std::fs::write(&path, format!("{pane}\n{}", std::process::id())).ok()?;
    Some(path)
}

/// Live tree panes: (pane id, registry file).
fn registered_trees(state_dir: &Path) -> Vec<(String, std::path::PathBuf)> {
    let dir = ui::tree::registry_dir(state_dir);
    let mut trees = Vec::new();
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let text = std::fs::read_to_string(entry.path()).unwrap_or_default();
        let mut lines = text.lines();
        let (Some(pane), Some(pid)) = (lines.next(), lines.next().and_then(|p| p.parse::<u32>().ok())) else {
            let _ = std::fs::remove_file(entry.path());
            continue;
        };
        if tunnel::pid_alive(pid) {
            trees.push((pane.to_string(), entry.path()));
        } else {
            let _ = std::fs::remove_file(entry.path());
        }
    }
    trees
}

/// Grid, console, DDL and docs: request from the environment or the queue,
/// else a selector.
async fn run_view(env: Arc<HerdrEnv>, action: Action) -> Result<()> {
    let config = settings::load(&env)?;
    let entrypoint = action.entrypoint();
    let request = match requests::receive(&env.state_dir, entrypoint, &env.holder_id(), SystemTime::now()) {
        Some(request) => request,
        None => match pick(&env, &config, action).await? {
            Some(request) => request,
            None => return Ok(()),
        },
    };
    let source_id = request.source_id().context("requête sans data source")?;
    let source = find_source(&config, source_id)?.clone();
    let keys = Keymap::new(&config.settings.keys);
    let icons = Icons::new(config.settings.icons);
    let opener = opener(&env, &config);
    let object = || request.object.clone().ok_or_else(|| anyhow!("requête sans objet pour {entrypoint}"));
    match action {
        Action::EditData => {
            let object = object()?;
            let estimate = estimate(&env, &source, &object).await;
            let grid = ui::grid::Grid::new(
                source.clone(),
                object.clone(),
                request.filter.clone(),
                keys,
                config.settings.page_size,
                estimate,
            );
            ui::run(grid, ui::grid::GridExec::new(env.clone(), source, object, opener)).await?;
        }
        Action::Console => {
            let max_rows = config.settings.console_max_rows;
            let console = ui::console::Console::new(source.clone(), request.schema.clone(), keys, max_rows);
            ui::run(console, ui::console::ConsoleExec::new(env.clone(), source, max_rows)).await?;
        }
        Action::GoToDdl => {
            let object = object()?;
            let view = ui::ddl::DdlView::new(source.clone(), object.clone(), keys);
            ui::run(view, ui::ddl::DdlExec::new(env.clone(), source, object)).await?;
        }
        Action::QuickDoc => {
            let object = object()?;
            let estimate = estimate(&env, &source, &object).await;
            let doc = ui::quickdoc::QuickDoc::new(
                source.clone(),
                object.clone(),
                request.column.clone(),
                keys,
                icons,
                estimate,
            );
            ui::run(doc, ui::quickdoc::QuickDocExec::new(env.clone(), source, object, opener)).await?;
        }
    }
    Ok(())
}

async fn estimate(env: &HerdrEnv, source: &SourceConfig, object: &ObjectRef) -> Option<i64> {
    let (dir, source, object) = (env.state_dir.clone(), source.clone(), object.clone());
    tokio::task::spawn_blocking(move || introspect::cached_estimate(&dir, &source, &object)).await.ok().flatten()
}

async fn pick(env: &HerdrEnv, config: &Config, action: Action) -> Result<Option<PaneRequest>> {
    let mut items = Vec::new();
    if action == Action::Console {
        for source in &config.sources {
            let mut request = PaneRequest::new(Action::Console);
            request.source = Some(source.id.clone());
            items.push(PickItem {
                label: source.label().to_string(),
                detail: format!("{} · {}", source.engine.label(), source.environment),
                request,
            });
        }
    } else {
        for source in &config.sources {
            let Ok(cache) = herdr_db_store::Cache::open(&env.state_dir, &source.id, source.engine) else {
                continue;
            };
            for schema in cache.schemas().unwrap_or_default() {
                for object in cache.objects(&schema.name).unwrap_or_default() {
                    let target = ObjectRef::new(source.id.clone(), schema.name.clone(), object.summary.name.clone());
                    items.push(PickItem {
                        label: format!("{}.{}", schema.name, object.summary.name),
                        detail: format!("{} · {}", source.label(), object.summary.kind.label()),
                        request: PaneRequest::for_object(action, target),
                    });
                }
            }
        }
    }
    if items.is_empty() {
        bail!(
            "rien à ouvrir : {}",
            if action == Action::Console {
                "aucune data source configurée"
            } else {
                "aucun objet introspecté (ouvrez l'arbre d'abord)"
            }
        );
    }
    let title = match action {
        Action::Console => "Console SQL : choisir une data source",
        Action::EditData => "Edit Data : choisir une table",
        Action::GoToDdl => "Go to DDL : choisir un objet",
        Action::QuickDoc => "Quick Documentation : choisir un objet",
    };
    let picker = ui::run(Picker::new(title, items), NoEffects).await?;
    Ok(picker.chosen)
}

/// Toggle: close the tree of the current tab, or open one on the left.
async fn toggle_tree(env: &Arc<HerdrEnv>) -> Result<()> {
    let herdr = Herdr::new(env);
    let tab = env.context.tab_id.clone();
    let panes = herdr.panes(tab.as_deref()).await?;
    let trees = registered_trees(&env.state_dir);
    if let Some((pane, _)) = trees.iter().find(|(id, _)| panes.iter().any(|p| &p.pane_id == id)) {
        return herdr.close_plugin_pane(pane).await;
    }
    let target =
        env.context.focused_pane_id.clone().or_else(|| panes.iter().find(|p| p.focused).map(|p| p.pane_id.clone()));
    let single = panes.len() == 1;
    let mut open = OpenPane::new("tree", Placement::Split);
    open.target_pane = target.clone();
    open.direction = Some(Direction::Right);
    open.cwd = env.context.project_dir().or_else(|| Some(env.cwd.clone()));
    open.env.insert(paths::ENV_STATE_DIR.into(), env.state_dir.display().to_string());
    open.env.insert(paths::ENV_CONFIG_DIR.into(), env.config_dir.display().to_string());
    if let Some(team) = env.team_config() {
        open.env.insert(paths::ENV_TEAM_CONFIG.into(), team.display().to_string());
    }
    let tree = herdr.open_pane(&open).await?;
    // The tree lives on the left: swap it with the pane it split.
    if let (Some(tree), Some(target)) = (tree, target) {
        if let Err(e) = herdr.swap(&tree, &target).await {
            tracing::warn!(error = %e, "swap failed");
        }
        if single && let Some(tab) = &tab {
            let _ = herdr
                .call("layout.set_split_ratio", serde_json::json!({ "tab_id": tab, "path": [], "ratio": 0.25 }))
                .await;
        }
    }
    Ok(())
}

async fn open_update_popup(env: &Arc<HerdrEnv>) -> Result<()> {
    let herdr = Herdr::new(env);
    let mut open = OpenPane::new("update", Placement::Popup);
    open.width = Some("80%".into());
    open.height = Some("70%".into());
    if let Some(bin) = &env.bin {
        open.env.insert("HERDR_BIN_PATH".into(), bin.display().to_string());
    }
    herdr.open_pane(&open).await.map(|_| ())
}

/// Runs `herdr plugin install`: Herdr previews the commands, the user
/// confirms, the build step fetches the new binary.
async fn self_update(env: &Arc<HerdrEnv>) -> Result<()> {
    let herdr = Herdr::new(env);
    println!("Herdr DB {} — mise à jour depuis {}\n", update::CURRENT_VERSION, paths::REPOSITORY);
    let status = tokio::process::Command::new(herdr.bin())
        .args(["plugin", "install", paths::REPOSITORY])
        .status()
        .await
        .with_context(|| format!("lancement de {}", herdr.bin().display()))?;
    println!();
    if status.success() {
        println!("Mise à jour terminée. Relancez les panes Herdr DB encore ouverts.");
    } else {
        println!("La mise à jour a échoué ({status}).");
    }
    println!("Entrée pour fermer.");
    let mut line = String::new();
    let _ = std::io::stdin().read_line(&mut line);
    Ok(())
}

/// `[[startup]]`: purge orphan requests, dead tunnel leases and stale tree
/// registrations left by a previous server.
fn startup(env: &HerdrEnv) {
    let removed = requests::purge(&env.state_dir, SystemTime::now());
    let stopped = tunnel::Tunnels::new(&env.state_dir).collect_garbage(&tunnel::SystemProbe);
    let trees = registered_trees(&env.state_dir).len();
    tracing::info!(removed, ?stopped, trees, "startup cleanup");
}

fn read_password() -> Result<String> {
    use std::io::Write;
    print!("Mot de passe : ");
    std::io::stdout().flush()?;
    #[cfg(unix)]
    {
        // SAFETY: plain termios calls on stdin, restored before returning.
        let fd = libc::STDIN_FILENO;
        let mut original: libc::termios = unsafe { std::mem::zeroed() };
        let is_tty = unsafe { libc::tcgetattr(fd, &mut original) } == 0;
        if is_tty {
            let mut silent = original;
            silent.c_lflag &= !libc::ECHO;
            unsafe { libc::tcsetattr(fd, libc::TCSANOW, &silent) };
        }
        let mut line = String::new();
        let read = std::io::stdin().read_line(&mut line);
        if is_tty {
            unsafe { libc::tcsetattr(fd, libc::TCSANOW, &original) };
        }
        println!();
        read?;
        Ok(line.trim_end_matches(['\n', '\r']).to_string())
    }
}

fn secret(env: &HerdrEnv, command: SecretCommand) -> Result<()> {
    let config = settings::load(env)?;
    match command {
        SecretCommand::Set { source, user } => {
            let source = find_source(&config, &source)?;
            let user = user.unwrap_or_else(|| secrets::user_of(source));
            let password = read_password()?;
            if password.is_empty() {
                bail!("mot de passe vide : rien enregistré");
            }
            secrets::save_keyring(&source.id, &user, &secrecy::SecretString::from(password))?;
            println!("mot de passe de {user} enregistré pour {}", source.id);
        }
        SecretCommand::Delete { source, user } => {
            let source = find_source(&config, &source)?;
            let user = user.unwrap_or_else(|| secrets::user_of(source));
            if secrets::delete_keyring(&source.id, &user)? {
                println!("mot de passe de {user} supprimé pour {}", source.id);
            } else {
                println!("aucun mot de passe enregistré pour {user} sur {}", source.id);
            }
        }
    }
    Ok(())
}

fn doctor(env: &HerdrEnv) -> Result<()> {
    println!("herdr-db {}", update::CURRENT_VERSION);
    println!("state dir      {}", env.state_dir.display());
    println!("config dir     {}", env.config_dir.display());
    println!("dans Herdr     {}", if env.inside_herdr() { "oui" } else { "non" });
    match env.team_config() {
        Some(path) => println!("fichier équipe {}", path.display()),
        None => println!("fichier équipe aucun herdr-db.toml trouvé depuis {}", env.cwd.display()),
    }
    let personal = env.personal_config();
    println!("fichier perso  {}{}", personal.display(), if personal.exists() { "" } else { " (absent)" });
    let config = settings::load(env)?;
    println!();
    for source in &config.sources {
        let secret = if source.password_command.is_some() { "password_command" } else { "trousseau ou saisie" };
        println!(
            "{:<20} {:<10} {:<12} {}:{}/{}  user={}  {}{}{}",
            source.id,
            source.engine.label(),
            source.environment,
            source.host,
            source.port,
            source.database,
            secrets::user_of(source),
            secret,
            if source.read_only { "  lecture seule" } else { "" },
            if source.pre_connect.is_some() { "  pré-connexion" } else { "" },
        );
    }
    Ok(())
}
