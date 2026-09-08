use anyhow::{Context, Result, bail};
use cosmic_client_toolkit::{
    delegate_toplevel_info, delegate_toplevel_manager, delegate_workspace,
    toplevel_info::{ToplevelInfoHandler, ToplevelInfoState},
    toplevel_management::{ToplevelManagerHandler, ToplevelManagerState},
    workspace::{WorkspaceHandler, WorkspaceState},
};
use sctk::{
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    seat::{SeatHandler, SeatState},
};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::{Duration, Instant, SystemTime};
use wayland_client::backend::WaylandError;
use wayland_client::protocol::wl_seat;
use wayland_client::{
    Connection, Proxy, QueueHandle, WEnum, globals::registry_queue_init, protocol::wl_output,
};
use wayland_protocols::ext::{
    foreign_toplevel_list::v1::client::ext_foreign_toplevel_handle_v1,
    workspace::v1::client::ext_workspace_handle_v1,
};

use crate::model::{
    Coordinates, match_window, match_workspace, resolve_workspace_output, workspace_names,
};
use crate::rules::Rule;

type ToplevelCapability = cosmic_client_toolkit::cosmic_protocols::toplevel_management::v1::client::zcosmic_toplevel_manager_v1::ZcosmicToplelevelManagementCapabilitiesV1;
type CosmicToplevel = cosmic_client_toolkit::cosmic_protocols::toplevel_info::v1::client::zcosmic_toplevel_handle_v1::ZcosmicToplevelHandleV1;

/// How long a newly opened window remains eligible for rule matching while its
/// title/app_id are still settling.
const SETTLE_GRACE: Duration = Duration::from_secs(5);

#[derive(Debug, Clone)]
pub struct WorkspaceRecord {
    pub handle: ext_workspace_handle_v1::ExtWorkspaceHandleV1,
    pub name: String,
    pub id: Option<String>,
    pub coordinates: Vec<u32>,
    pub output_names: Vec<String>,
    pub output_handles: Vec<wl_output::WlOutput>,
}

impl WorkspaceRecord {
    pub fn matches_selector(&self, selector: &str) -> bool {
        self.id.as_deref() == Some(selector)
            || self.name == selector
            || Coordinates::parse(selector).is_some_and(|coords| coords.0 == self.coordinates)
    }

    pub fn summary(&self) -> String {
        format!(
            "{} (id={}, coords={}, outputs={})",
            self.name,
            self.id.as_deref().unwrap_or("-"),
            self.coordinates_string(),
            self.outputs_csv(),
        )
    }

    pub fn coordinates_string(&self) -> String {
        Coordinates(self.coordinates.clone()).to_string()
    }

    pub fn outputs_csv(&self) -> String {
        if self.output_names.is_empty() { "-".to_owned() } else { self.output_names.join(", ") }
    }

    pub fn output_by_name(&self, name: &str) -> Option<wl_output::WlOutput> {
        self.output_handles
            .iter()
            .zip(&self.output_names)
            .find(|(_, output_name)| output_name.eq_ignore_ascii_case(name))
            .map(|(output, _)| output.clone())
    }
}

#[derive(Debug, Clone)]
pub struct WindowRecord {
    pub title: String,
    pub app_id: String,
    pub identifier: String,
    pub output_names: Vec<String>,
    pub output_handles: Vec<wl_output::WlOutput>,
    pub workspace_names: Vec<String>,
    /// The cosmic toplevel handle, available once the compositor has sent
    /// cosmic toplevel info for this window (it does so lazily, per window).
    pub cosmic_toplevel: Option<CosmicToplevel>,
    /// Wayland object id of the underlying ext foreign toplevel handle, used to
    /// tell windows apart across snapshots.
    pub foreign_id: u32,
}

impl WindowRecord {
    pub fn summary(&self) -> String {
        format!(
            "{} (app_id={}, identifier={})",
            display_field(&self.title),
            display_field(&self.app_id),
            display_field(&self.identifier),
        )
    }
}

#[derive(Debug)]
pub struct Snapshot {
    pub workspaces: Vec<WorkspaceRecord>,
    pub windows: Vec<WindowRecord>,
}

#[derive(Debug, Clone)]
pub struct GlobalRecord {
    pub interface: String,
    pub version: u32,
    pub name: u32,
}

#[derive(Debug, Clone)]
pub struct DebugInfo {
    pub globals: Vec<GlobalRecord>,
    pub toplevel_manager_capabilities: Vec<String>,
}

/// A window selector: which field to match and how.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowSelectorField {
    AppId,
    Title,
    Identifier,
    #[allow(dead_code)]
    Any,
}

/// A single predicate like `app_id=firefox`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowSelector {
    pub field: WindowSelectorField,
    pub value: String,
    pub exact: bool,
}

/// Combined window query: one or more predicates joined by ' and '.
#[derive(Debug, Clone)]
pub struct Query {
    pub selectors: Vec<WindowSelector>,
}

impl Query {
    /// Parse a query string like `app_id=firefox and title=Main`.
    ///
    /// Grammar:
    ///   query = predicate (" and " predicate)*
    ///   predicate = field "=" value
    ///   field = "app_id" | "title" | "identifier" | "active"
    ///   value = any non-whitespace token
    ///
    /// Default field (single token without '=') is `app_id`.
    pub fn parse(raw: &str) -> Result<Self> {
        let raw = raw.trim();
        if raw.is_empty() {
            bail!("empty window query");
        }

        // Split on ' and ' to get individual predicates
        let predicates: Vec<&str> = raw
            .split(" and ")
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .collect();

        let mut selectors = Vec::new();
        for pred in predicates {
            selectors.push(Self::parse_predicate(pred)?);
        }

        if selectors.is_empty() {
            bail!("empty window query");
        }

        Ok(Self { selectors })
    }

    fn parse_predicate(raw: &str) -> Result<WindowSelector> {
        // Try field=value form
        if let Some(eq_pos) = raw.find('=') {
            let field_name = raw[..eq_pos].trim();
            let value = raw[eq_pos + 1..].trim().to_string();
            if value.is_empty() {
                bail!("empty value in predicate '{}'", raw);
            }
            let field = match field_name {
                "app_id" => WindowSelectorField::AppId,
                "title" => WindowSelectorField::Title,
                "identifier" => WindowSelectorField::Identifier,
                "active" => {
                    if value == "true" {
                        return Ok(WindowSelector {
                            field: WindowSelectorField::AppId,
                            value: String::new(),
                            exact: true,
                        });
                    } else if value == "false" {
                        return Ok(WindowSelector {
                            field: WindowSelectorField::AppId,
                            value: String::new(),
                            exact: false,
                        });
                    }
                    bail!("'active' value must be 'true' or 'false', got '{}'", value);
                }
                _ => bail!("unknown field '{}': use app_id, title, identifier, or active", field_name),
            };
            Ok(WindowSelector { field, value, exact: false })
        } else {
            // Bare token: defaults to app_id (case-insensitive substring match)
            Ok(WindowSelector {
                field: WindowSelectorField::AppId,
                value: raw.to_string(),
                exact: false,
            })
        }
    }

    /// Check if a query matches a window. All selectors must match (AND logic).
    pub fn matches(&self, window: &WindowRecord) -> bool {
        self.selectors.iter().all(|s| s.matches(window))
    }
}

impl std::fmt::Display for Query {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let parts: Vec<String> = self
            .selectors
            .iter()
            .map(|s| format!("{}", s))
            .collect();
        write!(f, "{}", parts.join(" and "))
    }
}

impl WindowSelector {
    pub fn matches(&self, window: &WindowRecord) -> bool {
        match self.field {
            WindowSelectorField::AppId => self.matches_field(&window.app_id),
            WindowSelectorField::Title => self.matches_field(&window.title),
            WindowSelectorField::Identifier => self.matches_field(&window.identifier),
            WindowSelectorField::Any => {
                self.matches_field(&window.app_id)
                    || self.matches_field(&window.title)
                    || self.matches_field(&window.identifier)
            }
        }
    }

    pub fn matches_field(&self, haystack: &str) -> bool {
        if self.exact {
            haystack == self.value
        } else {
            haystack.to_ascii_lowercase().contains(&self.value.to_ascii_lowercase())
        }
    }
}

impl std::fmt::Display for WindowSelector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let field = match self.field {
            WindowSelectorField::AppId => "app_id",
            WindowSelectorField::Title => "title",
            WindowSelectorField::Identifier => "identifier",
            WindowSelectorField::Any => "window",
        };
        write!(f, "{}={}", field, self.value)
    }
}

/// Request to move a window to a workspace.
#[derive(Debug, Clone)]
pub struct MoveRequest {
    pub window_query: Query,
    pub workspace_selector: String,
    pub output_name: Option<String>,
    pub dry_run: bool,
    pub force_ext_move: bool,
}

/// Result of window activation.
#[derive(Debug)]
pub struct ActivateWindowResult {
    pub title: String,
    pub app_id: String,
}

/// Result of running a command.
#[derive(Debug)]
pub struct RunResult {
    pub command: String,
    pub pid: Option<u32>,
    pub exited: bool,
    /// Set when the command was placed on a workspace.
    pub moved: Option<MoveResult>,
}

/// Result of a window move operation.
#[derive(Debug, Clone)]
pub struct MoveResult {
    pub window_title: String,
    pub window_app_id: String,
    pub workspace_name: String,
    pub output_name: String,
}

/// Settings for [`Session::launch`].
#[derive(Debug, Clone)]
pub struct LaunchConfig {
    /// Where to move the window; `None` launches without placing.
    pub workspace_selector: Option<String>,
    /// Optional glob to match against the window's app_id.
    pub app_id: Option<String>,
    /// Optional glob to match against the window's title (requires `app_id`).
    pub title: Option<String>,
    /// How long to wait for a matching window to appear.
    pub timeout: Duration,
    /// Resolve and print the intended move without sending the request.
    pub dry_run: bool,
    /// Wait for the launched process to exit before returning.
    pub wait: bool,
}

/// Result of [`Session::launch`].
#[derive(Debug)]
pub struct LaunchResult {
    pub command: String,
    pub pid: Option<u32>,
    pub exited: bool,
    pub move_result: Option<MoveResult>,
}

fn spawn_shell(command: &str) -> Result<std::process::Child> {
    std::process::Command::new("sh").arg("-c").arg(command).spawn().context("failed to spawn command")
}

pub struct Session {
    conn: Connection,
    event_queue: wayland_client::EventQueue<AppState>,
    app: AppState,
    /// Every toplevel we have already anchored/seen for rule matching.
    seen: HashSet<u32>,
    /// Newly opened toplevels still within the settle grace, waiting for a rule match.
    pending: HashMap<u32, Instant>,
    /// Toplevel/rule pairs already acted on, so we never move the same window twice.
    moved: HashSet<(u32, String)>,
}

impl Session {
    pub fn connect() -> Result<Self> {
        let conn =
            Connection::connect_to_env().context("failed to connect to the Wayland compositor")?;
        let (globals, event_queue) =
            registry_queue_init(&conn).context("failed to initialize Wayland globals")?;
        let qh = event_queue.handle();

        let registry_state = RegistryState::new(&globals);
        let workspace_state = WorkspaceState::new(&registry_state, &qh);
        let toplevel_info_state = ToplevelInfoState::new(&registry_state, &qh);
        let toplevel_manager_state = ToplevelManagerState::try_new(&registry_state, &qh)
            .context("the compositor does not expose zcosmic_toplevel_manager_v1")?;

        let app = AppState {
            output_state: OutputState::new(&globals, &qh),
            registry_state,
            seats: SeatState::new(&globals, &qh),
            workspace_state,
            toplevel_info_state,
            toplevel_manager_state,
            globals: globals.contents().with_list(|list| {
                list.iter()
                    .map(|global| GlobalRecord {
                        interface: global.interface.to_owned(),
                        version: global.version,
                        name: global.name,
                    })
                    .collect::<Vec<_>>()
            }),
            manager_capabilities: Vec::new(),
            workspace_done: false,
            manager_capabilities_ready: false,
        };

        let mut session = Self {
            conn,
            event_queue,
            app,
            seen: HashSet::new(),
            pending: HashMap::new(),
            moved: HashSet::new(),
        };
        session.wait_for_initial_state()?;
        Ok(session)
    }

    pub fn snapshot(&mut self) -> Result<Snapshot> {
        let workspaces = self
            .app
            .workspace_state
            .workspaces()
            .filter_map(|workspace| {
                let group = self
                    .app
                    .workspace_state
                    .workspace_groups()
                    .find(|group| group.workspaces.contains(&workspace.handle))?;
                let output_names = group
                    .outputs
                    .iter()
                    .filter_map(|output| self.output_name(output))
                    .collect::<Vec<_>>();

                Some(WorkspaceRecord {
                    handle: workspace.handle.clone(),
                    name: workspace.name.clone(),
                    id: workspace.id.clone(),
                    coordinates: workspace.coordinates.clone(),
                    output_names,
                    output_handles: group.outputs.clone(),
                })
            })
            .collect::<Vec<_>>();

        let windows = self
            .app
            .toplevel_info_state
            .toplevels()
            .map(|window| {
                let cosmic_toplevel = window.cosmic_toplevel.clone();
                let mut output_names = window
                    .output
                    .iter()
                    .filter_map(|output| self.output_name(output))
                    .collect::<Vec<_>>();
                output_names.sort();
                output_names.dedup();

                WindowRecord {
                    title: window.title.clone(),
                    app_id: window.app_id.clone(),
                    identifier: window.identifier.clone(),
                    output_names,
                    output_handles: window.output.iter().cloned().collect(),
                    workspace_names: workspace_names(&window.workspace, &workspaces),
                    cosmic_toplevel,
                    foreign_id: window.foreign_toplevel.id().protocol_id(),
                }
            })
            .collect::<Vec<_>>();

        Ok(Snapshot { workspaces, windows })
    }

    pub fn move_window(&mut self, request: MoveRequest) -> Result<MoveResult> {
        self.ensure_move_capability(request.force_ext_move)?;
        let snapshot = self.snapshot()?;
        let window = match_window(&snapshot.windows, &request.window_query)?;
        let workspace = match_workspace(&snapshot.workspaces, &request.workspace_selector)?;
        let output = resolve_workspace_output(workspace, window, request.output_name.as_deref())?;
        self.move_impl(window, workspace, &output, request.dry_run)
    }

    /// Move a specific window record to a workspace selected by name, id, or coordinates.
    pub fn move_window_record(
        &mut self,
        window: &WindowRecord,
        workspace_selector: &str,
        dry_run: bool,
    ) -> Result<MoveResult> {
        let snapshot = self.snapshot()?;
        let workspace = match_workspace(&snapshot.workspaces, workspace_selector)?;
        let output = resolve_workspace_output(workspace, window, None)?;
        self.move_impl(window, workspace, &output, dry_run)
    }

    /// Send the `move_to_ext_workspace` request (unless `dry_run`) and describe the result.
    fn move_impl(
        &mut self,
        window: &WindowRecord,
        workspace: &WorkspaceRecord,
        output: &wl_output::WlOutput,
        dry_run: bool,
    ) -> Result<MoveResult> {
        let Some(cosmic_toplevel) = &window.cosmic_toplevel else {
            bail!(
                "window '{}' does not support moving (no cosmic toplevel info yet)",
                window.title
            );
        };
        let output_name = self.output_name(output).unwrap_or_else(|| "<unknown>".to_owned());

        if !dry_run {
            self.app.toplevel_manager_state.manager.move_to_ext_workspace(
                cosmic_toplevel,
                &workspace.handle,
                output,
            );
            self.conn.flush().context("failed to flush move_to_ext_workspace request")?;
        }

        Ok(MoveResult {
            window_title: window.title.clone(),
            window_app_id: window.app_id.clone(),
            workspace_name: workspace.name.clone(),
            output_name,
        })
    }

    /// Read and dispatch any pending Wayland events without blocking.
    ///
    /// Returns as soon as the socket has no more data, so callers can poll.
    fn pump(&mut self) -> Result<()> {
        self.conn.flush().context("failed to flush Wayland connection")?;
        if let Some(guard) = self.conn.prepare_read() {
            match guard.read() {
                Ok(_) => {}
                Err(WaylandError::Io(error)) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(error) => {
                    return Err(anyhow::Error::new(error))
                        .context("failed to read Wayland events");
                }
            }
        }
        self.event_queue
            .dispatch_pending(&mut self.app)
            .context("failed to dispatch Wayland events")?;
        Ok(())
    }


    pub fn debug_info(&mut self) -> DebugInfo {
        DebugInfo {
            globals: self.app.globals.clone(),
            toplevel_manager_capabilities: self
                .manager_capabilities()
                .iter()
                .map(display_capability)
                .collect(),
        }
    }

    fn ensure_move_capability(&self, force_ext_move: bool) -> Result<()> {
        let supports_ext = self.manager_capabilities().iter().any(|capability| {
            matches!(capability, WEnum::Value(ToplevelCapability::MoveToExtWorkspace))
        });

        if supports_ext {
            return Ok(());
        }

        if force_ext_move {
            return Ok(());
        }

        if self.should_assume_ext_move_support() {
            return Ok(());
        }

        let advertises_legacy_only = self.manager_capabilities().iter().any(|capability| {
            matches!(capability, WEnum::Value(ToplevelCapability::MoveToWorkspace))
        });

        if advertises_legacy_only {
            bail!(
                "this COSMIC session exposes ext workspaces (ext_workspace_manager_v1 + zcosmic_workspace_manager_v2) but does not advertise move_to_ext_workspace; move-window is unavailable on this compositor build"
            );
        }

        bail!("the compositor does not advertise move_to_ext_workspace support")
    }

    fn manager_capabilities(&self) -> &[WEnum<ToplevelCapability>] {
        &self.app.manager_capabilities
    }

    fn should_assume_ext_move_support(&self) -> bool {
        let has_ext_workspace_manager =
            self.app.globals.iter().any(|global| global.interface == "ext_workspace_manager_v1");
        let has_cosmic_workspace_manager_v2 = self
            .app
            .globals
            .iter()
            .any(|global| global.interface == "zcosmic_workspace_manager_v2");
        let has_legacy_workspace_manager = self
            .app
            .globals
            .iter()
            .any(|global| global.interface == "zcosmic_workspace_manager_v1");
        let advertises_legacy_move_only = self.manager_capabilities().iter().any(|capability| {
            matches!(capability, WEnum::Value(ToplevelCapability::MoveToWorkspace))
        });

        has_ext_workspace_manager
            && has_cosmic_workspace_manager_v2
            && !has_legacy_workspace_manager
            && advertises_legacy_move_only
    }

    fn output_name(&self, output: &wl_output::WlOutput) -> Option<String> {
        self.app.output_state.info(output).and_then(|info| info.name.clone())
    }

    fn first_seat(&self) -> Option<wl_seat::WlSeat> {
        self.app.seats.seats().next()
    }

    /// Activate (focus) a window by query.
    pub fn activate_window(&mut self, query: Query) -> Result<ActivateWindowResult> {
        self.ensure_capabilities()?;
        let snapshot = self.snapshot()?;
        let window = match_window(&snapshot.windows, &query)?;

        if let (Some(seat), Some(cosmic_toplevel)) =
            (self.first_seat(), &window.cosmic_toplevel)
        {
            self.app
                .toplevel_manager_state
                .manager
                .activate(cosmic_toplevel, &seat);
            self.conn.flush()?;
        }

        Ok(ActivateWindowResult {
            title: window.title.clone(),
            app_id: window.app_id.clone(),
        })
    }

    /// Activate (focus) a workspace by selector.
    pub fn activate_workspace(&mut self, selector: &str) -> Result<()> {
        self.ensure_capabilities()?;
        let snapshot = self.snapshot()?;
        let workspace = match_workspace(&snapshot.workspaces, selector)?;

        // Activate the workspace via ext workspace handle
        self.app
            .workspace_state
            .workspace_info(&workspace.handle)
            .ok_or_else(|| anyhow::anyhow!("workspace {} not found", selector))?;
        workspace
            .handle
            .activate();
        self.conn.flush()?;

        Ok(())
    }

    /// Run a command, optionally placing its window on a workspace.
    pub fn run(
        &mut self,
        workspace_selector: Option<&str>,
        command: &[String],
        wait: bool,
    ) -> Result<RunResult> {
        let result = self.launch(
            LaunchConfig {
                workspace_selector: workspace_selector.map(str::to_owned),
                app_id: None,
                title: None,
                timeout: Duration::from_secs(30),
                dry_run: false,
                wait,
            },
            command,
        )?;
        Ok(RunResult {
            command: result.command,
            pid: result.pid,
            exited: result.exited,
            moved: result.move_result,
        })
    }

    /// Launch a command and, when a workspace is requested, wait for its window
    /// to appear and move it there.
    ///
    /// Without an app_id/title filter, the first window that opens after the
    /// spawn (that was not already open) is moved. With filters, only windows
    /// whose app_id (and title, when given) match are considered.
    pub fn launch(&mut self, cfg: LaunchConfig, command: &[String]) -> Result<LaunchResult> {
        self.ensure_capabilities()?;
        let cmd_str = command.join(" ");
        let rule = match (&cfg.app_id, &cfg.title) {
            (Some(app_id), title) => Some(Rule::new(
                app_id,
                title.as_deref(),
                cfg.workspace_selector.as_deref().unwrap_or(""),
            )?),
            (None, Some(_)) => bail!("--title requires --app-id"),
            (None, None) => None,
        };
        let mut child = spawn_shell(&cmd_str)?;
        let pid = child.id();

        let move_result = if let Some(workspace_selector) = cfg.workspace_selector.as_deref() {
            // Anchor: windows already open before the spawn are not candidates.
            let pre = self
                .snapshot()?
                .windows
                .iter()
                .map(|window| window.foreign_id)
                .collect::<HashSet<u32>>();

            let deadline = Instant::now() + cfg.timeout;
            let matched = loop {
                self.pump()?;
                let snapshot = self.snapshot()?;
                if let Some(window) = snapshot.windows.iter().find(|window| {
                    !pre.contains(&window.foreign_id)
                        && !window.app_id.is_empty()
                        && rule.as_ref().is_none_or(|rule| {
                            rule.matches(&window.app_id, &window.title)
                        })
                }) {
                    break window.clone();
                }
                if Instant::now() >= deadline {
                    bail!(
                        "no matching window appeared within {}s for '{}'; the app may not have opened a window",
                        cfg.timeout.as_secs(),
                        cmd_str,
                    );
                }
                std::thread::sleep(Duration::from_millis(50));
            };

            Some(self.move_window_record(&matched, workspace_selector, cfg.dry_run)?)
        } else {
            None
        };

        let exited = if cfg.wait {
            child.wait().context("failed to wait for command")?;
            true
        } else {
            std::mem::forget(child);
            false
        };

        Ok(LaunchResult {
            command: cmd_str,
            pid: Some(pid),
            exited,
            move_result,
        })
    }

    /// Watch for new windows forever, moving those that match any of `rules` to
    /// their configured workspace.
    ///
    /// This is the long-running `daemon` entry point: it only returns on error.
    pub fn daemon(
        &mut self,
        config_path: &Path,
        rules: Vec<Rule>,
        dry_run: bool,
        move_existing: bool,
    ) -> Result<()> {
        self.ensure_capabilities()?;

        // Anchor on the window set already open, so we don't move windows that
        // were placed before the daemon started, unless explicitly asked to.
        let snapshot = self.snapshot()?;
        let now = Instant::now();
        for window in &snapshot.windows {
            if move_existing {
                self.pending.insert(window.foreign_id, now);
            } else {
                self.seen.insert(window.foreign_id);
            }
        }

        let mut rules = rules;
        let mut config_stamp = config_file_stamp(config_path);

        loop {
            self.event_queue
                .blocking_dispatch(&mut self.app)
                .context("failed while dispatching Wayland events; daemon exiting")?;

            // Hot-reload: the config GUI (or a manual edit) may have changed
            // the rules file since the last event.
            if let Some(stamp) = config_file_stamp(config_path) {
                if Some(stamp) != config_stamp {
                    config_stamp = Some(stamp);
                    match crate::rules::load_rules(config_path) {
                        Ok(new_rules) => {
                            rules = new_rules;
                            eprintln!(
                                "cosmic-wmctl: reloaded {} rule(s) from {}",
                                rules.len(),
                                config_path.display()
                            );
                            if let Err(error) = self.apply_rules(&rules, dry_run) {
                                eprintln!("cosmic-wmctl: {error:#}");
                            }
                        }
                        Err(error) => {
                            eprintln!("cosmic-wmctl: failed to reload rules: {error:#}")
                        }
                    }
                }
            }

            if let Err(error) = self.apply_rules(&rules, dry_run) {
                // A transient failure (e.g. a workspace disappearing) must not
                // kill the daemon; log and keep watching.
                eprintln!("cosmic-wmctl: {error:#}");
            }
        }
    }

    /// Apply `rules` to windows currently open, moving each matching window to its
    /// configured workspace. Used by the config GUI's apply-now action.
    pub fn apply_rules_now(&mut self, rules: &[Rule]) -> Result<Vec<MoveResult>> {
        let snapshot = self.snapshot()?;
        let mut moved = Vec::new();
        let mut handled = HashSet::new();
        for window in &snapshot.windows {
            if let Some(rule) = rules
                .iter()
                .find(|rule| rule.matches(&window.app_id, &window.title))
            {
                let key = (window.foreign_id, rule.workspace.as_str());
                if !handled.insert(key) {
                    continue;
                }
                moved.push(self.move_window_record(window, &rule.workspace, false)?);
            }
        }
        Ok(moved)
    }

    /// Match newly opened windows against `rules` and move them.
    fn apply_rules(&mut self, rules: &[Rule], dry_run: bool) -> Result<()> {
        if rules.is_empty() {
            return Ok(());
        }
        let snapshot = self.snapshot()?;
        let now = Instant::now();
        for window in &snapshot.windows {
            let id = window.foreign_id;
            if self.seen.insert(id) {
                // New window: eligible for matching during the settle grace.
                self.pending.insert(id, now);
            }
            if !self.pending.contains_key(&id) {
                continue;
            }
            if let Some(rule) = rules
                .iter()
                .find(|rule| rule.matches(&window.app_id, &window.title))
            {
                let key = (id, rule.workspace.clone());
                if self.moved.contains(&key) {
                    continue;
                }
                let result = self.move_window_record(window, &rule.workspace, dry_run)?;
                self.moved.insert(key);
                self.pending.remove(&id);
                let action = if dry_run { "would move" } else { "moved" };
                println!(
                    "{action} '{}' ({}) to workspace '{}' on output '{}'",
                    display_field(&result.window_title),
                    display_field(&result.window_app_id),
                    result.workspace_name,
                    result.output_name,
                );
            }
        }
        // Windows that never matched within the grace period are dropped.
        self.pending.retain(|_, birth| now.duration_since(*birth) < SETTLE_GRACE);
        Ok(())
    }

    fn ensure_capabilities(&mut self) -> Result<()> {
        // Ensure we've received the initial state
        self.wait_for_initial_state()?;
        Ok(())
    }

    fn wait_for_initial_state(&mut self) -> Result<()> {
        // Do not wait on the toplevel-info manager `done` event: it is
        // deprecated since protocol v2 and current cosmic-comp never sends
        // it, so a blocking loop would spin forever (e.g. while a window
        // title animates, flooding done events). Instead, roundtrip until
        // the workspace list, manager capabilities and at least one
        // toplevel have arrived, bounded by a deadline.
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            self.event_queue
                .roundtrip(&mut self.app)
                .context("failed while waiting for initial COSMIC state")?;
            if self.app.workspace_done
                && self.app.manager_capabilities_ready
                && self.app.toplevel_info_state.toplevels().next().is_some()
            {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Ok(());
            }
        }
    }
}

struct AppState {
    output_state: OutputState,
    registry_state: RegistryState,
    seats: SeatState,
    workspace_state: WorkspaceState,
    toplevel_info_state: ToplevelInfoState,
    toplevel_manager_state: ToplevelManagerState,
    globals: Vec<GlobalRecord>,
    manager_capabilities: Vec<WEnum<ToplevelCapability>>,
    /// Set when the workspace manager sends its initial `done`.
    workspace_done: bool,
    /// Set when the toplevel manager advertises its capabilities.
    manager_capabilities_ready: bool,
}

impl ProvidesRegistryState for AppState {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }

    sctk::registry_handlers!(OutputState, SeatState);
}

impl OutputHandler for AppState {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }

    fn update_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }

    fn output_destroyed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }
}

impl WorkspaceHandler for AppState {
    fn workspace_state(&mut self) -> &mut WorkspaceState {
        &mut self.workspace_state
    }

    fn done(&mut self) {
        self.workspace_done = true;
    }
}

impl ToplevelInfoHandler for AppState {
    fn toplevel_info_state(&mut self) -> &mut ToplevelInfoState {
        &mut self.toplevel_info_state
    }

    fn new_toplevel(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _toplevel: &ext_foreign_toplevel_handle_v1::ExtForeignToplevelHandleV1,
    ) {
    }

    fn update_toplevel(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _toplevel: &ext_foreign_toplevel_handle_v1::ExtForeignToplevelHandleV1,
    ) {
    }

    fn toplevel_closed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _toplevel: &ext_foreign_toplevel_handle_v1::ExtForeignToplevelHandleV1,
    ) {
    }

    fn info_done(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>) {}
}

impl ToplevelManagerHandler for AppState {
    fn toplevel_manager_state(&mut self) -> &mut ToplevelManagerState {
        &mut self.toplevel_manager_state
    }

    fn capabilities(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        capabilities: Vec<WEnum<ToplevelCapability>>,
    ) {
        self.manager_capabilities = capabilities;
        self.manager_capabilities_ready = true;
    }
}

impl SeatHandler for AppState {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seats
    }

    fn new_seat(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _seat: wl_seat::WlSeat,
    ) {
    }

    fn new_capability(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _seat: wl_seat::WlSeat,
        _capability: sctk::seat::Capability,
    ) {
    }

    fn remove_capability(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _seat: wl_seat::WlSeat,
        _capability: sctk::seat::Capability,
    ) {
    }

    fn remove_seat(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _seat: wl_seat::WlSeat,
    ) {
    }
}

delegate_workspace!(AppState);
delegate_toplevel_info!(AppState);
delegate_toplevel_manager!(AppState);
sctk::delegate_registry!(AppState);
sctk::delegate_dispatch2!(AppState);

fn display_field(value: &str) -> &str {
    if value.is_empty() { "-" } else { value }
}

fn display_capability(capability: &WEnum<ToplevelCapability>) -> String {
    match capability {
        WEnum::Value(value) => format!("{value:?}"),
        WEnum::Unknown(value) => format!("Unknown({value})"),
    }
}

/// Identity of a config file's contents: mtime + size, cheap to stat.
fn config_file_stamp(path: &Path) -> Option<(SystemTime, u64)> {
    let metadata = std::fs::metadata(path).ok()?;
    Some((metadata.modified().ok()?, metadata.len()))
}
