use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use serde::Serialize;

use crate::rules;
use crate::wayland::{LaunchConfig, MoveRequest, Query, Session, WindowRecord, WorkspaceRecord};

/// JSON representation of a window (for the `--json` output of `windows`).
#[derive(Debug, Clone, Serialize)]
pub struct WindowJson {
    pub title: String,
    pub app_id: String,
    pub identifier: String,
    pub workspaces: Vec<String>,
    pub outputs: Vec<String>,
}

impl WindowJson {
    fn from_record(window: &WindowRecord) -> Self {
        Self {
            title: window.title.clone(),
            app_id: window.app_id.clone(),
            identifier: window.identifier.clone(),
            workspaces: window.workspace_names.clone(),
            outputs: window.output_names.clone(),
        }
    }
}

/// JSON representation of a workspace (for the `--json` output of `workspaces`).
#[derive(Debug, Clone, Serialize)]
pub struct WorkspaceJson {
    pub name: String,
    pub id: Option<String>,
    pub coords: String,
    pub outputs: Vec<String>,
}

impl WorkspaceJson {
    fn from_record(workspace: &WorkspaceRecord) -> Self {
        Self {
            name: workspace.name.clone(),
            id: workspace.id.clone(),
            coords: workspace.coordinates_string(),
            outputs: workspace.output_names.clone(),
        }
    }
}

#[derive(Debug, Parser)]
#[command(author, version, about)]
pub struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// List all windows.
    Windows(WindowsArgs),
    /// List all workspaces.
    Workspaces(WorkspacesArgs),
    /// Print runtime Wayland globals and COSMIC toplevel capabilities.
    DebugCapabilities,
    /// Move a window to a workspace.
    ///
    /// Usage: cosmic-wmctl window-move <window-query> <workspace-query>
    ///
    /// Window query: field=value pairs combined with ' and '
    ///   Fields: app_id, title, identifier, active=true|false (default field: app_id)
    ///   Matching: case-insensitive substring by default
    ///   Use --exact for exact match instead of substring
    ///
    /// Workspace query: bare id, name, or coordinates (e.g. 1,0)
    ///
    /// Examples:
    ///   cosmic-wmctl window-move 'app_id=firefox' '6'
    ///   cosmic-wmctl window-move 'title=Main and active=true' '1,0'
    WindowMove(WindowMoveArgs),
    /// Activate (focus) a window.
    ///
    /// Usage: cosmic-wmctl window-activate <window-query>
    WindowActivate(WindowActivateArgs),
    /// Activate (focus) a workspace.
    ///
    /// Usage: cosmic-wmctl workspace-activate <workspace-query>
    WorkspaceActivate(WorkspaceActivateArgs),
    /// Launch a command and move its window to a workspace.
    ///
    /// Usage: cosmic-wmctl launch [--workspace <query>] [--app-id <glob>] [--title <glob>]
    ///                            [--timeout <secs>] [--return] [--dry-run] -- <command...>
    ///
    /// The command is started, then cosmic-wmctl waits for a matching window to
    /// appear (up to --timeout) and moves it. With no --app-id/--title filter,
    /// the first window that opens after the launch is moved.
    Launch(LaunchArgs),
    /// Watch for new windows and move them per rules in a config file.
    ///
    /// Rules live in $XDG_CONFIG_HOME/cosmic-wmctl/rules.toml (or
    /// ~/.config/cosmic-wmctl/rules.toml): a list of `app_id` globs, optional
    /// `title` globs, and `workspace` selectors. Newly opened windows are moved
    /// to the workspace of the first matching rule.
    Daemon(DaemonArgs),
    /// Run a command, optionally on a specific workspace.
    ///
    /// Usage: cosmic-wmctl run [--workspace <query>] [--return] -- <command...>
    ///
    /// If --workspace is given, the command is started and its window is moved
    /// to that workspace once it appears.
    /// If --return is given, the command is started and the process waits
    /// for it to exit before the tool returns.
    Run(RunArgs),
}

#[derive(Debug, Args)]
pub struct WindowsArgs {
    /// Emit a JSON array instead of the human-readable table.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
pub struct WorkspacesArgs {
    /// Emit a JSON array instead of the human-readable table.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
pub struct WindowMoveArgs {
    /// Window selector query (field=value pairs joined with ' and ').
    #[arg(value_name = "WINDOW_QUERY")]
    window: String,
    /// Workspace query (id, name, or coordinates).
    #[arg(value_name = "WORKSPACE_QUERY")]
    workspace: String,
    /// Prefer a specific output when the target workspace spans multiple outputs.
    #[arg(long)]
    output: Option<String>,
    /// Require exact matching instead of case-insensitive substring matching.
    #[arg(long)]
    exact: bool,
    /// Resolve selectors and print the intended move without sending the request.
    #[arg(long)]
    dry_run: bool,
    /// Bypass capability checks and try the ext workspace move request anyway.
    #[arg(long)]
    force_ext_move: bool,
}

#[derive(Debug, Args)]
pub struct WindowActivateArgs {
    /// Window selector query (field=value pairs joined with ' and ').
    #[arg(value_name = "WINDOW_QUERY")]
    window: String,
}

#[derive(Debug, Args)]
pub struct WorkspaceActivateArgs {
    /// Workspace query (id, name, or coordinates).
    #[arg(value_name = "WORKSPACE_QUERY")]
    workspace: String,
}

#[derive(Debug, Args)]
pub struct LaunchArgs {
    /// Move the launched window to this workspace (name, id, or coordinates).
    #[arg(short, long, value_name = "WORKSPACE_QUERY")]
    workspace: Option<String>,
    /// Glob matched against the window's app_id.
    #[arg(long, value_name = "GLOB", requires = "workspace")]
    app_id: Option<String>,
    /// Glob matched against the window's title (requires --app-id).
    #[arg(long, value_name = "GLOB", requires = "app_id")]
    title: Option<String>,
    /// Seconds to wait for a matching window before giving up.
    #[arg(long, default_value_t = 30, value_name = "SECS")]
    timeout: u64,
    /// Resolve and print the intended move without sending the request.
    #[arg(long)]
    dry_run: bool,
    /// Wait for the launched process to exit before returning.
    #[arg(short, long)]
    r#return: bool,
    /// Command to run (passed through to shell).
    #[arg(trailing_var_arg = true, value_name = "COMMAND")]
    command: Vec<String>,
}

#[derive(Debug, Args)]
pub struct DaemonArgs {
    /// Rules file with [[rules]] entries.
    #[arg(long, value_name = "PATH")]
    config: Option<std::path::PathBuf>,
    /// Print what would be moved without moving anything.
    #[arg(long)]
    dry_run: bool,
    /// Also apply rules to windows already open when the daemon starts.
    #[arg(long)]
    move_existing: bool,
    /// Write an example rules file to the config path and exit.
    #[arg(long)]
    init: bool,
}

#[derive(Debug, Args)]
pub struct RunArgs {
    /// Start the command on this workspace (name, id, or coordinates).
    #[arg(long, value_name = "WORKSPACE_QUERY")]
    workspace: Option<String>,
    /// Wait for the command to exit before returning.
    #[arg(short, long)]
    r#return: bool,
    /// Command to run (passed through to shell).
    #[arg(trailing_var_arg = true, value_name = "COMMAND")]
    command: Vec<String>,
}

impl Cli {
    pub fn run(self) -> Result<()> {
        match self.command {
            Command::Windows(args) => {
                let mut session = Session::connect()?;
                let windows = session.snapshot()?.windows;
                if args.json {
                    let records = windows.iter().map(WindowJson::from_record).collect::<Vec<_>>();
                    println!("{}", serde_json::to_string_pretty(&records)?);
                } else {
                    for w in &windows {
                        println!(
                            "{} | app_id={} | identifier={} | workspaces={} | outputs={}",
                            display_field(&w.title),
                            display_field(&w.app_id),
                            display_field(&w.identifier),
                            joined_or_dash(&w.workspace_names),
                            joined_or_dash(&w.output_names),
                        );
                    }
                }
            }
            Command::Workspaces(args) => {
                let mut session = Session::connect()?;
                let workspaces = session.snapshot()?.workspaces;
                if args.json {
                    let records =
                        workspaces.iter().map(WorkspaceJson::from_record).collect::<Vec<_>>();
                    println!("{}", serde_json::to_string_pretty(&records)?);
                } else {
                    for w in &workspaces {
                        println!(
                            "{} | id={} | coords={} | outputs={}",
                            w.name,
                            w.id.as_deref().unwrap_or("-"),
                            w.coordinates_string(),
                            w.outputs_csv(),
                        );
                    }
                }
            }
            Command::DebugCapabilities => {
                let mut session = Session::connect()?;
                let debug = session.debug_info();
                println!("globals:");
                for g in &debug.globals {
                    println!("  {} v{} name={}", g.interface, g.version, g.name);
                }
                println!("toplevel_manager_capabilities:");
                for cap in &debug.toplevel_manager_capabilities {
                    println!("  {}", cap);
                }
            }
            Command::WindowMove(args) => {
                let window_query = parse_window_query(&args.window)?;
                let mut session = Session::connect()?;
                let result = session.move_window(MoveRequest {
                    window_query,
                    workspace_selector: args.workspace,
                    output_name: args.output,
                    dry_run: args.dry_run,
                    force_ext_move: args.force_ext_move,
                })?;

                println!(
                    "Moved '{}' ({}) to workspace '{}' on output '{}'",
                    display_field(&result.window_title),
                    display_field(&result.window_app_id),
                    result.workspace_name,
                    result.output_name,
                );
            }
            Command::WindowActivate(args) => {
                let window_query = parse_window_query(&args.window)?;
                let mut session = Session::connect()?;
                let result = session.activate_window(window_query)?;
                println!(
                    "Activated '{}' ({})",
                    display_field(&result.title),
                    display_field(&result.app_id),
                );
            }
            Command::WorkspaceActivate(args) => {
                let mut session = Session::connect()?;
                session.activate_workspace(&args.workspace)?;
            }
            Command::Launch(args) => {
                let mut session = Session::connect()?;
                let result = session.launch(
                    LaunchConfig {
                        workspace_selector: args.workspace,
                        app_id: args.app_id,
                        title: args.title,
                        timeout: Duration::from_secs(args.timeout),
                        dry_run: args.dry_run,
                        wait: args.r#return,
                    },
                    &args.command,
                )?;
                println!("Launched '{}'", result.command);
                if let Some(pid) = result.pid {
                    println!("pid={pid}");
                }
                if let Some(moved) = &result.move_result {
                    println!(
                        "Moved '{}' ({}) to workspace '{}' on output '{}'",
                        display_field(&moved.window_title),
                        display_field(&moved.window_app_id),
                        moved.workspace_name,
                        moved.output_name,
                    );
                }
                if result.exited {
                    println!("exited");
                }
            }
            Command::Daemon(args) => {
                let config_path = args.config.unwrap_or_else(rules::default_config_path);
                if args.init {
                    if let Some(parent) = config_path.parent() {
                        std::fs::create_dir_all(parent)
                            .with_context(|| format!("failed to create {}", parent.display()))?;
                    }
                    std::fs::write(&config_path, rules::EXAMPLE_CONFIG)
                        .with_context(|| format!("failed to write {}", config_path.display()))?;
                    println!(
                        "wrote example rules to {}; edit it and run `cosmic-wmctl daemon`",
                        config_path.display()
                    );
                    return Ok(());
                }
                let loaded = rules::load_rules(&config_path)?;
                if loaded.is_empty() {
                    eprintln!(
                        "cosmic-wmctl: no rules loaded from {}; add [[rules]] entries or pass --config",
                        config_path.display()
                    );
                }
                let mut session = Session::connect()?;
                eprintln!(
                    "cosmic-wmctl: watching for new windows ({} rule(s)); press Ctrl-C to stop",
                    loaded.len()
                );
                session.daemon(&config_path, loaded, args.dry_run, args.move_existing)?;
            }
            Command::Run(args) => {
                let mut session = Session::connect()?;
                let result =
                    session.run(args.workspace.as_deref(), &args.command, args.r#return)?;
                println!("Ran '{}'", result.command);
                if let Some(pid) = result.pid {
                    println!("pid={pid}");
                }
                if let Some(moved) = &result.moved {
                    println!(
                        "Moved '{}' ({}) to workspace '{}' on output '{}'",
                        display_field(&moved.window_title),
                        display_field(&moved.window_app_id),
                        moved.workspace_name,
                        moved.output_name,
                    );
                }
                if result.exited {
                    println!("exited");
                }
            }
        }

        Ok(())
    }
}

fn parse_window_query(raw: &str) -> Result<Query> {
    Query::parse(raw)
}

fn display_field(value: &str) -> &str {
    if value.is_empty() { "-" } else { value }
}

fn joined_or_dash(values: &[String]) -> String {
    if values.is_empty() { "-".to_owned() } else { values.join(", ") }
}

#[cfg(test)]
mod tests {
    use super::{WindowJson, WorkspaceJson};
    use crate::wayland::WindowRecord;

    #[test]
    fn window_json_shape() {
        let record = WindowRecord {
            title: "Terminal".to_owned(),
            app_id: "org.gnome.Terminal".to_owned(),
            identifier: "term".to_owned(),
            output_names: vec!["eDP-1".to_owned()],
            output_handles: vec![],
            workspace_names: vec!["2".to_owned(), "1".to_owned()],
            cosmic_toplevel: None,
            foreign_id: 42,
        };
        let view = WindowJson::from_record(&record);
        let value = serde_json::to_value(&view).expect("serializable");
        assert_eq!(value["title"], "Terminal");
        assert_eq!(value["app_id"], "org.gnome.Terminal");
        assert_eq!(value["identifier"], "term");
        assert_eq!(value["workspaces"], serde_json::json!(["2", "1"]));
        assert_eq!(value["outputs"], serde_json::json!(["eDP-1"]));
    }

    #[test]
    fn workspace_json_shape() {
        // The view struct is plain data; the Wayland proxy handle in the record
        // cannot be constructed outside a session, so we test the view directly.
        let view = WorkspaceJson {
            name: "work".to_owned(),
            id: None,
            coords: "1,0".to_owned(),
            outputs: vec!["eDP-1".to_owned()],
        };
        let value = serde_json::to_value(&view).expect("serializable");
        assert_eq!(value["name"], "work");
        assert_eq!(value["id"], serde_json::Value::Null);
        assert_eq!(value["coords"], "1,0");
        assert_eq!(value["outputs"], serde_json::json!(["eDP-1"]));
    }
}
