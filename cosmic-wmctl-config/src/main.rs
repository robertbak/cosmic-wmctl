//! COSMIC GUI for configuring `cosmic-wmctl` window placement rules.
//!
//! Edits the same `rules.toml` the `cosmic-wmctl daemon` reads, and talks to
//! the live COSMIC session to list workspaces and running windows.

use cosmic::app::{Core, Settings, Task};
use cosmic::executor;
use cosmic::iced::{Alignment, Length, Size};
use cosmic::widget::popover::{self, Position};
use cosmic::widget::settings;
use cosmic::widget::{
    Column, Row, button, container, header_bar, icon, list, scrollable, text, text_input,
};
use cosmic::{Application, Element};

use cosmic_wmctl::rules::{Rule, default_config_path, load_rules};
use cosmic_wmctl::wayland::{Session, WorkspaceRecord};
use std::path::{Path, PathBuf};

const APP_ID: &str = "org.cosmic-wmctl.Config";

/// The rules reference shown in the help popup.
const HELP_TEXT: &str = r#"Each [[rules]] entry moves matching windows to a workspace:

    app_id      (required) glob against the window's app id,
                e.g. "firefox" or "com.mitchellh.ghostty"
    title       (optional) glob against the window's title
    workspace   (required) pick it in the UI; the saved TOML
                accepts a workspace name, id, or coordinates,
                e.g. "6" or "1,0"

Glob syntax:
    *              any sequence of characters
    ?              exactly one character
    [a-z0-9]       one character from a set or range
    [!abc]         any character NOT in the set
    everything else matches literally (dots, dashes, spaces)
    matching is case-sensitive; dots are not wildcards

Examples:
    app_id = "firefox"              every Firefox window
    app_id = "firefox", title = "*DevTools*"   only DevTools

Notes:
  - app_id and title must BOTH match for a rule to fire.
  - Rules created by clicking Move are app_id-only.
  - The daemon only matches a window within ~5s of it opening, so title
    rules need titles that are stable when the window appears.
  - Clicking Move on a window moves it now and creates + saves a rule.
  - Save persists manual edits. Apply now runs all rules against the
    windows currently open."#;

fn main() -> cosmic::iced::Result {
    let settings = Settings::default().size(Size::new(1024.0, 720.0));
    cosmic::app::run::<App>(settings, default_config_path())
}

/// One editable rule row.
#[derive(Clone, Debug, Default)]
struct RuleDraft {
    app_id: String,
    title: String,
    workspace: String,
}

impl RuleDraft {
    fn from_rule(rule: &Rule) -> Self {
        Self {
            app_id: rule.app_id.clone(),
            title: rule.title.clone().unwrap_or_default(),
            workspace: rule.workspace.clone(),
        }
    }

    fn to_rule(&self) -> Result<Rule, String> {
        let app_id = self.app_id.trim();
        if app_id.is_empty() {
            return Err("app_id is empty".into());
        }
        let title = if self.title.trim().is_empty() { None } else { Some(self.title.trim()) };
        Rule::new(app_id, title, self.workspace.trim()).map_err(|e| e.to_string())
    }
}

/// A running window, enough to identify and move it.
#[derive(Clone, Debug)]
struct WindowInfo {
    app_id: String,
    title: String,
}

/// A window the user clicked "Move" on, awaiting a workspace choice.
#[derive(Clone, Debug)]
/// What the modal dialog is currently showing, if any.
enum Dialog {
    MoveWindow {
        app_id: String,
        title: String,
        /// Index into [`LiveData::workspaces`].
        workspace: Option<usize>,
    },
    /// Pick a workspace for rule `index` (fills its text input).
    PickWorkspace { rule_index: usize },
    /// Confirm removal of rule `index`.
    ConfirmRemove { rule_index: usize },
}

#[derive(Clone, Debug, Default)]
struct LiveData {
    /// Live workspaces, for the pickers.
    workspaces: Vec<WorkspaceRecord>,
    windows: Vec<WindowInfo>,
}

#[derive(Clone, Debug)]
enum Message {
    /// Live session data arrived.
    LiveLoaded(Result<LiveData, String>),
    Refresh,
    AppIdChanged(usize, String),
    TitleChanged(usize, String),
    /// Set a rule's workspace from the workspace-picker dialog.
    WorkspacePicked(usize, usize),
    AddRule,
    /// Open a confirmation dialog before removing a rule.
    RemoveRule(usize),
    /// The removal was confirmed in the dialog.
    RemoveRuleConfirmed(usize),
    Save,
    ApplyNow,
    Applied(Result<Vec<String>, String>),
    /// Open the "move window to workspace" dialog for a window.
    MoveWindow { app_id: String, title: String },
    /// Open the workspace-picker dialog for a rule.
    PickWorkspace(usize),
    /// A workspace was picked inside the move dialog.
    PendingMoveWorkspacePicked(usize),
    CloseDialog,
    /// Move the window and add + save a rule for its app_id.
    ConfirmMove,
    WindowMoved(Result<String, String>),
    ToggleHelp,
    CloseHelp,
}

struct App {
    core: Core,
    config_path: PathBuf,
    rules: Vec<RuleDraft>,
    live: Option<LiveData>,
    dialog: Option<Dialog>,
    show_help: bool,
    status: Option<String>,
}

impl Application for App {
    type Executor = executor::Default;
    type Flags = PathBuf;
    type Message = Message;
    const APP_ID: &'static str = APP_ID;

    fn core(&self) -> &Core {
        &self.core
    }

    fn core_mut(&mut self) -> &mut Core {
        &mut self.core
    }

    fn init(core: Core, config_path: Self::Flags) -> (Self, Task<Self::Message>) {
        let rules = match load_rules(&config_path) {
            Ok(rules) => rules.iter().map(RuleDraft::from_rule).collect(),
            Err(error) => {
                eprintln!("cosmic-wmctl-config: {error:#}");
                Vec::new()
            }
        };

        let app = Self {
            core,
            config_path,
            rules,
            live: None,
            dialog: None,
            show_help: false,
            status: None,
        };

        let task = cosmic::task::future(async { Message::LiveLoaded(fetch_live()) });
        (app, task)
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::LiveLoaded(result) => {
                self.status = None;
                match result {
                    Ok(live) => self.live = Some(live),
                    Err(error) => {
                        self.live = Some(LiveData::default());
                        self.status = Some(format!("could not read the COSMIC session: {error}"));
                    }
                }
                cosmic::task::none()
            }
            Message::Refresh => {
                cosmic::task::future(async { Message::LiveLoaded(fetch_live()) })
            }
            Message::AppIdChanged(index, value) => {
                if let Some(rule) = self.rules.get_mut(index) {
                    rule.app_id = value;
                }
                cosmic::task::none()
            }
            Message::TitleChanged(index, value) => {
                if let Some(rule) = self.rules.get_mut(index) {
                    rule.title = value;
                }
                cosmic::task::none()
            }
            Message::WorkspacePicked(rule_index, workspace_index) => {
                let pick = self
                    .live
                    .as_ref()
                    .and_then(|live| live.workspaces.get(workspace_index))
                    .and_then(workspace_selector);
                if let (Some(rule), Some(pick)) = (self.rules.get_mut(rule_index), pick) {
                    rule.workspace = pick;
                }
                self.dialog = None;
                cosmic::task::none()
            }
            Message::AddRule => {
                self.rules.push(RuleDraft::default());
                cosmic::task::none()
            }
            Message::RemoveRule(index) => {
                self.dialog = Some(Dialog::ConfirmRemove { rule_index: index });
                cosmic::task::none()
            }
            Message::RemoveRuleConfirmed(index) => {
                self.rules.remove(index);
                self.dialog = None;
                self.status = Some(match save_rules(&self.config_path, &self.rules) {
                    Ok(written) => format!("removed rule, saved {written} rule(s) total"),
                    Err(error) => format!("rule removed but save failed: {error}"),
                });
                cosmic::task::none()
            }
            Message::Save => {
                self.status = Some(match save_rules(&self.config_path, &self.rules) {
                    Ok(written) => format!(
                        "wrote {written} rule(s) to {}",
                        self.config_path.display()
                    ),
                    Err(error) => error,
                });
                cosmic::task::none()
            }
            Message::ApplyNow => {
                let rules: Vec<Rule> =
                    self.rules.iter().filter_map(|draft| draft.to_rule().ok()).collect();
                if rules.is_empty() {
                    self.status =
                        Some("nothing to apply: add at least one rule with an app_id".into());
                    return cosmic::task::none();
                }
                cosmic::task::future(async move { Message::Applied(apply_rules_now(rules)) })
            }
            Message::Applied(result) => {
                self.status = Some(match result {
                    Ok(moves) if moves.is_empty() => {
                        "no currently open windows match any rule".into()
                    }
                    Ok(moves) => {
                        format!("moved {} window(s): {}", moves.len(), moves.join("; "))
                    }
                    Err(error) => format!("failed to apply rules: {error}"),
                });
                cosmic::task::none()
            }
            Message::MoveWindow { app_id, title } => {
                if self.live.is_none() {
                    self.status =
                        Some("session data not loaded — press Refresh first".into());
                    return cosmic::task::none();
                }
                self.dialog = Some(Dialog::MoveWindow { app_id, title, workspace: None });
                cosmic::task::none()
            }
            Message::PickWorkspace(rule_index) => {
                if self.live.is_none() {
                    self.status =
                        Some("session data not loaded — press Refresh first".into());
                    return cosmic::task::none();
                }
                self.dialog = Some(Dialog::PickWorkspace { rule_index });
                cosmic::task::none()
            }
            Message::PendingMoveWorkspacePicked(index) => {
                if let Some(Dialog::MoveWindow { workspace, .. }) = self.dialog.as_mut() {
                    *workspace = Some(index);
                }
                cosmic::task::none()
            }
            Message::CloseDialog => {
                self.dialog = None;
                cosmic::task::none()
            }
            Message::ConfirmMove => {
                let Some(Dialog::MoveWindow { app_id, workspace, .. }) = self.dialog.take() else {
                    return cosmic::task::none();
                };
                let Some(record) = workspace.and_then(|index| {
                    self.live.as_ref().and_then(|live| live.workspaces.get(index))
                }) else {
                    self.status = Some("no workspace selected".into());
                    return cosmic::task::none();
                };
                let Some(workspace) = workspace_selector(record) else {
                    self.status = Some("selected workspace has no selector".into());
                    return cosmic::task::none();
                };
                let app_id = app_id.clone();

                // Create a rule from the real window and persist it.
                self.rules.push(RuleDraft {
                    app_id: app_id.clone(),
                    title: String::new(),
                    workspace: workspace.clone(),
                });
                self.status = Some(match save_rules(&self.config_path, &self.rules) {
                    Ok(written) => {
                        format!("created rule and saved {written} rule(s) total")
                    }
                    Err(error) => format!("rule added but save failed: {error}"),
                });

                // Move the window now, in the background.
                cosmic::task::future(async move {
                    Message::WindowMoved(move_window_to_workspace(&app_id, &workspace))
                })
            }
            Message::WindowMoved(result) => {
                self.status = Some(match result {
                    Ok(what) => format!("moved: {what}"),
                    Err(error) => format!("failed to move window: {error}"),
                });
                cosmic::task::none()
            }
            Message::ToggleHelp => {
                self.show_help = !self.show_help;
                cosmic::task::none()
            }
            Message::CloseHelp => {
                self.show_help = false;
                cosmic::task::none()
            }
        }
    }

    fn view(&self) -> Element<'_, Message> {
        let help_button = button::icon(icon::from_name("help-about-symbolic"))
            .on_press(Message::ToggleHelp);
        let help = if self.show_help {
            popover::popover(help_button)
                .position(Position::Bottom)
                .on_close(Message::CloseHelp)
                .popup(self.help_popup())
        } else {
            popover::popover(help_button)
        };

        let header = header_bar()
            .title("Window Placement Rules")
            .end(help);

        let spacing = cosmic::theme::spacing();
        let actions = Row::with_children([
            button::standard("Add rule").on_press(Message::AddRule).into(),
            cosmic::widget::space::horizontal().into(),
            button::standard("Refresh").on_press(Message::Refresh).into(),
            button::standard("Apply now").on_press(Message::ApplyNow).into(),
            button::suggested("Save").on_press(Message::Save).into(),
        ])
        .spacing(8)
        .width(Length::Fill)
        .align_y(Alignment::Center)
        .padding([spacing.space_xxs, 0]);

        let content = settings::view_column(vec![
            self.rules_section(),
            actions.into(),
            self.windows_section(),
            self.status_row(),
        ])
        .width(Length::FillPortion(2));

        let body = scrollable(
            container(content)
                .align_x(Alignment::Center)
                .width(Length::Fill)
                .padding([spacing.space_xs, spacing.space_s, spacing.space_s, spacing.space_s]),
        )
        .width(Length::Fill)
        .height(Length::Fill);

        Column::with_children([header.into(), body.into()])
            .width(Length::Fill)
            .height(Length::Fill)
            .into()
    }

    fn dialog(&self) -> Option<Element<'_, Message>> {
        let workspaces = || {
            self.live
                .as_ref()
                .map(|live| live.workspaces.clone())
                .unwrap_or_default()
        };
        match self.dialog.as_ref()? {
            Dialog::MoveWindow { app_id, title, workspace } => {
                let picked = workspace
                    .and_then(|index| self.live.as_ref()?.workspaces.get(index))
                    .and_then(workspace_selector);
                let label = if title.is_empty() {
                    app_id.clone()
                } else {
                    format!("{title} ({app_id})")
                };
                let body = match &picked {
                    Some(workspace) => format!("{label}\nWill move to workspace {workspace}"),
                    None => format!("{label}\nChoose a workspace:"),
                };

                // The primary action stays disabled until a workspace is picked.
                let confirm = workspace.map(|_| Message::ConfirmMove);

                let records = workspaces();
                let mut picker = list::list_column();
                for (ws_index, ws) in records.iter().enumerate() {
                    picker = picker.add(
                        list::button(workspace_row_content(ws))
                            .selected(*workspace == Some(ws_index))
                            .on_press(Message::PendingMoveWorkspacePicked(ws_index)),
                    );
                }

                Some(
                    cosmic::widget::dialog()
                        .title("Move window to workspace")
                        .body(body)
                        .control(container(picker).width(Length::Fill))
                        .primary_action(
                            button::standard("Move & add rule").on_press_maybe(confirm),
                        )
                        .secondary_action(
                            button::standard("Cancel").on_press(Message::CloseDialog),
                        )
                        .into(),
                )
            }
            Dialog::PickWorkspace { rule_index } => {
                let rule_index = *rule_index;
                let records = workspaces();
                let mut picker = list::list_column();
                for (ws_index, ws) in records.iter().enumerate() {
                    picker = picker.add(
                        list::button(workspace_row_content(ws))
                            .on_press(Message::WorkspacePicked(rule_index, ws_index)),
                    );
                }
                Some(
                    cosmic::widget::dialog()
                        .title("Pick a workspace")
                        .body("Choose the workspace new windows matching this rule open on:")
                        .control(container(picker).width(Length::Fill))
                        .secondary_action(button::standard("Cancel").on_press(Message::CloseDialog))
                        .into(),
                )
            }
            Dialog::ConfirmRemove { rule_index } => {
                let rule = self.rules.get(*rule_index)?;
                let mut label = rule.app_id.clone();
                if !rule.title.is_empty() {
                    label.push_str(&format!(" \u{2014} title {}", rule.title));
                }
                Some(
                    cosmic::widget::dialog()
                        .title("Remove rule?")
                        .body(format!("Windows of \"{label}\" will no longer be moved."))
                        .icon(icon::from_name("user-trash-symbolic"))
                        .tertiary_action(
                            button::destructive("Remove")
                                .on_press(Message::RemoveRuleConfirmed(*rule_index)),
                        )
                        .secondary_action(button::standard("Cancel").on_press(Message::CloseDialog))
                        .into(),
                )
            }
        }
    }
}

/// Two-line content for a workspace picker row: selector + details caption.
fn workspace_row_content(ws: &WorkspaceRecord) -> Element<'static, Message> {
    let selector = workspace_selector(ws).unwrap_or_else(|| "?".into());
    Column::with_children([
        cosmic::widget::text::body(selector).into(),
        cosmic::widget::text::caption(format!(
            "coordinates {} \u{b7} {}",
            ws.coordinates_string(),
            ws.outputs_csv()
        ))
        .into(),
    ])
    .spacing(2)
    .width(Length::Fill)
    .into()
}

impl App {
    /// The rules reference shown in the help popover.
    /// Surface styling mirrors cosmic-settings' popovers: content padded
    /// inside a `Container::Dropdown` card (solid background + 1px border).
    fn help_popup(&self) -> Element<'_, Message> {
        container(text(HELP_TEXT).size(13).width(Length::Fill))
            .padding(16)
            .width(Length::Fixed(460.0))
            .class(cosmic::theme::Container::Dropdown)
            .into()
    }

    /// Rules editor: one card per rule, under a titled section.
    fn rules_section(&self) -> Element<'_, Message> {
        let mut section: settings::Section<'_, Message> = settings::section().title("Rules");
        if self.rules.is_empty() {
            section = section.add(
                cosmic::widget::text::body("No rules yet — add one above, or click Move on a window."),
            );
        } else {
            for (index, rule) in self.rules.iter().enumerate() {
                section = section.add(self.rule_row(index, rule));
            }
        }
        section.into()
    }

    /// One rule = one compact row: app_id · title · workspace button · remove.
    fn rule_row<'a>(&'a self, index: usize, rule: &'a RuleDraft) -> Element<'a, Message> {
        let app_id = text_input("app_id glob", &rule.app_id)
            .on_input(move |value| Message::AppIdChanged(index, value))
            .width(Length::FillPortion(3));
        let title = text_input("title glob (optional)", &rule.title)
            .on_input(move |value| Message::TitleChanged(index, value))
            .width(Length::FillPortion(3));

        // The single workspace control: shows the current selection and
        // opens the picker dialog. The raw TOML still accepts names, ids
        // and coordinates ("1,0") for anything not in the live list.
        let workspace = if rule.workspace.is_empty() {
            "pick workspace".to_owned()
        } else {
            rule.workspace.clone()
        };
        let pick = button::standard(workspace).on_press(Message::PickWorkspace(index));
        let remove = button::standard("Remove").on_press(Message::RemoveRule(index));

        Row::with_children([
            app_id.into(),
            title.into(),
            pick.into(),
            remove.into(),
        ])
        .spacing(8)
        .align_y(Alignment::Center)
        .width(Length::Fill)
        .into()
    }

    /// Open windows: one label/control row per window, Move on the right.
    fn windows_section(&self) -> Element<'_, Message> {
        let mut section: settings::Section<'_, Message> = settings::section().title("Open windows");
        match &self.live {
            Some(live) if live.windows.is_empty() => {
                section = section.add(cosmic::widget::text::body("No windows open."));
            }
            Some(live) => {
                for window in &live.windows {
                    let label = if window.title.is_empty() {
                        window.app_id.clone()
                    } else {
                        format!("{} · {}", window.app_id, window.title)
                    };
                    section = section.add(settings::flex_item(
                        label,
                        button::standard("Move").on_press(Message::MoveWindow {
                            app_id: window.app_id.clone(),
                            title: window.title.clone(),
                        }),
                    ));
                }
            }
            None => {
                section = section.add(cosmic::widget::text::body("Loading session…"));
            }
        }
        section.into()
    }

    /// Bottom status line; keeps its slot so the layout does not jump.
    fn status_row(&self) -> Element<'_, Message> {
        match &self.status {
            Some(status) => container(cosmic::widget::text::caption(status))
                .padding([0, 4])
                .width(Length::Fill)
                .into(),
            None => container(cosmic::widget::text::caption(" "))
                .padding([0, 4])
                .width(Length::Fill)
                .into(),
        }
    }
}

fn fetch_live() -> Result<LiveData, String> {
    let mut session = Session::connect().map_err(|e| format!("{e:#}"))?;
    let snapshot = session.snapshot().map_err(|e| format!("{e:#}"))?;

    let workspaces = snapshot.workspaces.clone();

    let windows = snapshot
        .windows
        .iter()
        .map(|window| WindowInfo {
            app_id: window.app_id.clone(),
            title: window.title.clone(),
        })
        .collect::<Vec<_>>();

    Ok(LiveData { workspaces, windows })
}

/// The selector used to reference a workspace: name, id, or coordinates.
fn workspace_selector(workspace: &WorkspaceRecord) -> Option<String> {
    if !workspace.name.is_empty() {
        return Some(workspace.name.clone());
    }
    if let Some(id) = workspace.id.as_deref().filter(|id| !id.is_empty()) {
        return Some(id.to_owned());
    }
    if !workspace.coordinates.is_empty() {
        return Some(workspace.coordinates_string());
    }
    None
}

fn save_rules(path: &Path, drafts: &[RuleDraft]) -> Result<usize, String> {
    let mut saved = 0;
    let mut table = toml::Table::new();
    let mut rules = Vec::new();

    for draft in drafts {
        let rule = match draft.to_rule() {
            Ok(rule) => rule,
            Err(error) => return Err(format!("rule not saved: {error}")),
        };
        let mut entry = toml::Table::new();
        entry.insert("app_id".into(), toml::Value::String(rule.app_id));
        if let Some(title) = rule.title {
            entry.insert("title".into(), toml::Value::String(title));
        }
        entry.insert("workspace".into(), toml::Value::String(rule.workspace));
        rules.push(toml::Value::Table(entry));
        saved += 1;
    }

    table.insert("rules".into(), toml::Value::Array(rules));
    let body = toml::to_string_pretty(&table).map_err(|e| e.to_string())?;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    std::fs::write(path, body).map_err(|e| format!("failed to write {}: {e}", path.display()))?;
    Ok(saved)
}

fn apply_rules_now(rules: Vec<Rule>) -> Result<Vec<String>, String> {
    let mut session = Session::connect().map_err(|e| format!("{e:#}"))?;
    let moved = session.apply_rules_now(&rules).map_err(|e| format!("{e:#}"))?;
    Ok(moved
        .iter()
        .map(|m| format!("'{}' → workspace '{}'", m.window_title, m.workspace_name))
        .collect())
}

fn move_window_to_workspace(app_id: &str, workspace: &str) -> Result<String, String> {
    let mut session = Session::connect().map_err(|e| format!("{e:#}"))?;
    let snapshot = session.snapshot().map_err(|e| format!("{e:#}"))?;
    let window = snapshot
        .windows
        .iter()
        .find(|window| window.app_id == app_id)
        .ok_or_else(|| format!("'{app_id}' is no longer open"))?;
    let result =
        session.move_window_record(window, workspace, false).map_err(|e| format!("{e:#}"))?;
    Ok(format!(
        "moved '{}' to workspace '{}' on output '{}'",
        result.window_title, result.workspace_name, result.output_name
    ))
}
