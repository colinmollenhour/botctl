use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
    MouseButton, MouseEvent, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols::border;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};
use unicode_width::UnicodeWidthStr;

use crate::app::{
    AppResult, ensure_pane_owned_by_claude, execute_classified_workflow, inspect_pane,
    is_yolo_safe_to_approve, try_unstick_pane,
};
use crate::automation::GuardedWorkflow;
use crate::classifier::SessionState;
use crate::cli::DashboardArgs;
use crate::opencode::resolve_opencode_session_for_pane;
use crate::prompt::resolve_state_dir;
use crate::storage::{
    WorkspaceRecord, bootstrap_state_db, resolve_workspace_for_path, sync_tmux_claude_session_id,
    sync_tmux_wait_state,
};
use crate::tmux::{TmuxClient, TmuxPane};
use crate::yolo::{disable_yolo_record, read_yolo_record, write_yolo_record};

const FOOTER_LINE1: &[(&str, &str)] = &[
    ("Arrows/jk", "move"),
    ("y", "toggle pane"),
    ("Enter", "open"),
    ("A", "all on"),
    ("q", "quit"),
];
const FOOTER_LINE2: &[(&str, &str)] = &[
    ("Click", "select/open"),
    ("Y", "toggle workspace"),
    ("u", "unstick"),
    ("N", "all off"),
];
const FOOTER_LINE2_PERSISTENT_END: &[(&str, &str)] = &[("q", "detach"), ("X", "kill server")];
const FOOTER_INDENT: &str = "  ";
const FOOTER_COLUMN_GAP: &str = "    ";
const PERSISTENT_DASHBOARD_SOCKET: &str = "botctl-dashboard";
const PERSISTENT_DASHBOARD_SESSION: &str = "botctl-dashboard";
const TABLE_GAP: &str = "  ";
const DASHBOARD_LEFT_PANE_PERCENT: u16 = 58;
const DASHBOARD_LEFT_PANE_MAX_WIDTH: u16 = 80;
const DASHBOARD_STATE_EMOJIS: &[&str] =
    &["⚙️", "🤔", "💤", "🔐", "❓", "📁", "📝", "✏️", "🧾", "❔"];
const DASHBOARD_YOLO_MARKER: &str = "ʸ";
const LEGACY_YOLO_PREFIX_CHARS: &[&str] = &["🤠", "\u{200D}"];

pub fn run(args: DashboardArgs) -> AppResult<String> {
    let state_dir = resolve_state_dir(args.state_dir.as_deref())?;
    bootstrap_state_db(&state_dir)?;

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = DashboardApp::new(
        dashboard_tmux_client(),
        host_tmux_client(),
        state_dir,
        args.poll_ms,
        args.history_lines,
    )?;
    let loop_result = run_dashboard_loop(
        &mut terminal,
        &mut app,
        args.exit_on_navigate,
        args.persistent,
    );

    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;
    app.restore_window_names()?;

    let action = loop_result?;
    if let Some(pane) = action {
        navigate_to_pane(&app.client, &pane)?;
    }

    Ok(String::new())
}

struct DashboardApp {
    client: TmuxClient,
    host_client: TmuxClient,
    state_dir: std::path::PathBuf,
    poll_ms: u64,
    history_lines: usize,
    panes: Vec<PaneEntry>,
    rows: Vec<RowItem>,
    selected_pane_id: Option<String>,
    selection: usize,
    first_seen: HashMap<String, Instant>,
    previous_frames: HashMap<String, String>,
    yes_counts: HashMap<String, u32>,
    window_names: HashMap<String, ManagedWindowName>,
    message: String,
    last_refresh: Instant,
    restored_selection: SavedSelection,
}

#[derive(Clone)]
struct PaneEntry {
    pane: TmuxPane,
    source: PaneSource,
    workspace: WorkspaceRecord,
    workspace_group_key: String,
    workspace_group_label: String,
    branch: String,
    state: SessionState,
    has_questions: bool,
    recap_present: bool,
    recap_excerpt: Option<String>,
    yolo_enabled: bool,
    age: Duration,
    wait_duration: Option<Duration>,
    yes_count: u32,
    claude_session_id: Option<String>,
    focused_source: String,
}

#[derive(Clone)]
enum PaneSource {
    Claude,
    OpenCode { session_id: String, title: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DetailBodyKind {
    BlockingDialog,
    Recap,
    Context,
}

enum RowItem {
    WorkspaceHeader { label: String },
    Pane { pane_id: String },
}

struct ManagedWindowName {
    target: String,
    base_name: String,
    applied_name: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct SavedSelection {
    pane_id: Option<String>,
    row_index: Option<usize>,
}

impl DashboardApp {
    fn new(
        client: TmuxClient,
        host_client: TmuxClient,
        state_dir: std::path::PathBuf,
        poll_ms: u64,
        history_lines: usize,
    ) -> AppResult<Self> {
        let restored_selection = load_saved_selection(&state_dir)?;
        Ok(Self {
            client,
            host_client,
            state_dir,
            poll_ms,
            history_lines,
            panes: Vec::new(),
            rows: Vec::new(),
            selected_pane_id: None,
            selection: 0,
            first_seen: HashMap::new(),
            previous_frames: HashMap::new(),
            yes_counts: HashMap::new(),
            window_names: HashMap::new(),
            message: String::new(),
            last_refresh: Instant::now() - Duration::from_millis(poll_ms),
            restored_selection,
        })
    }

    fn refresh(&mut self) -> AppResult<()> {
        let now = Instant::now();
        let mut panes = Vec::new();
        let mut seen_pane_ids = HashSet::new();
        let mut branch_by_path = HashMap::new();
        let mut next_frames = HashMap::new();

        for pane in self.client.list_panes()? {
            if is_claude_pane(&pane) {
                seen_pane_ids.insert(pane.pane_id.clone());
                self.first_seen.entry(pane.pane_id.clone()).or_insert(now);

                let inspected = inspect_pane(&self.client, &pane.pane_id, self.history_lines)?;
                let previous_frame = self.previous_frames.get(&pane.pane_id);
                let state = dashboard_display_state(
                    inspected.classification.state,
                    previous_frame,
                    &inspected.raw_source,
                );
                let workspace =
                    resolve_workspace_for_path(&self.state_dir, Path::new(&pane.current_path))?;
                let workspace_group_key = workspace_group_key(&workspace);
                let workspace_group_label = workspace_group_label(&workspace_group_key);
                let branch = branch_by_path
                    .entry(pane.current_path.clone())
                    .or_insert_with(|| git_branch_for_path(&pane.current_path))
                    .clone();
                let yolo_enabled = matches!(read_yolo_record(&self.state_dir, &pane.pane_id)?, Some(record) if record.enabled);
                let age = process_age(pane.pane_pid).unwrap_or_else(|| {
                    self.first_seen
                        .get(&pane.pane_id)
                        .map(Instant::elapsed)
                        .unwrap_or_default()
                });
                let wait_duration = sync_tmux_wait_state(
                    &self.state_dir,
                    &workspace.id,
                    &pane,
                    state.as_str(),
                    is_waiting_state(state),
                )?;
                let claude_session_id =
                    sync_tmux_claude_session_id(&self.state_dir, &workspace.id, &pane)?;
                let yes_count = self
                    .yes_counts
                    .get(&yes_count_key(claude_session_id.as_deref(), &pane.pane_id))
                    .copied()
                    .unwrap_or(0);
                next_frames.insert(pane.pane_id.clone(), inspected.raw_source.clone());

                panes.push(PaneEntry {
                    pane,
                    source: PaneSource::Claude,
                    workspace,
                    workspace_group_key,
                    workspace_group_label,
                    branch,
                    state,
                    has_questions: inspected.classification.has_questions,
                    recap_present: inspected.classification.recap_present,
                    recap_excerpt: inspected.classification.recap_excerpt.clone(),
                    yolo_enabled,
                    age,
                    wait_duration,
                    yes_count,
                    claude_session_id,
                    focused_source: inspected.focused_source,
                });
                continue;
            }

            let Some(opencode_session) = resolve_opencode_session_for_pane(&pane) else {
                continue;
            };
            seen_pane_ids.insert(pane.pane_id.clone());
            self.first_seen.entry(pane.pane_id.clone()).or_insert(now);

            let workspace =
                resolve_workspace_for_path(&self.state_dir, Path::new(&pane.current_path))?;
            let workspace_group_key = workspace_group_key(&workspace);
            let workspace_group_label = workspace_group_label(&workspace_group_key);
            let branch = branch_by_path
                .entry(pane.current_path.clone())
                .or_insert_with(|| git_branch_for_path(&pane.current_path))
                .clone();
            let age = process_age(pane.pane_pid).unwrap_or_else(|| {
                self.first_seen
                    .get(&pane.pane_id)
                    .map(Instant::elapsed)
                    .unwrap_or_default()
            });

            panes.push(PaneEntry {
                pane,
                source: PaneSource::OpenCode {
                    session_id: opencode_session.id,
                    title: opencode_session.title,
                },
                workspace,
                workspace_group_key,
                workspace_group_label,
                branch,
                state: opencode_session.state,
                has_questions: opencode_session.has_questions,
                recap_present: false,
                recap_excerpt: None,
                yolo_enabled: false,
                age,
                wait_duration: None,
                yes_count: 0,
                claude_session_id: None,
                focused_source: opencode_session.context,
            });
        }

        self.first_seen
            .retain(|pane_id, _| seen_pane_ids.contains(pane_id));
        self.previous_frames = next_frames;

        let first_window_by_workspace = panes.iter().fold(
            HashMap::<String, u32>::new(),
            |mut first_window_by_workspace, pane| {
                let window_order = tmux_object_id_order(&pane.pane.window_id);
                first_window_by_workspace
                    .entry(pane.workspace_group_key.clone())
                    .and_modify(|current| *current = (*current).min(window_order))
                    .or_insert(window_order);
                first_window_by_workspace
            },
        );

        panes.sort_by(|left, right| {
            first_window_by_workspace
                .get(&left.workspace_group_key)
                .copied()
                .unwrap_or(u32::MAX)
                .cmp(
                    &first_window_by_workspace
                        .get(&right.workspace_group_key)
                        .copied()
                        .unwrap_or(u32::MAX),
                )
                .then(left.workspace_group_key.cmp(&right.workspace_group_key))
                .then(left.pane.session_name.cmp(&right.pane.session_name))
                .then(left.pane.window_index.cmp(&right.pane.window_index))
                .then(left.pane.pane_index.cmp(&right.pane.pane_index))
        });

        self.panes = panes;
        self.rebuild_rows();
        self.run_yolo_supervisor()?;
        self.apply_window_name_prefixes()?;
        self.last_refresh = Instant::now();
        Ok(())
    }

    fn rebuild_rows(&mut self) {
        let previous = self.selected_pane_id.clone();
        self.rows.clear();

        let mut last_workspace = None::<String>;
        for pane in &self.panes {
            if last_workspace.as_deref() != Some(&pane.workspace_group_key) {
                self.rows.push(RowItem::WorkspaceHeader {
                    label: pane.workspace_group_label.clone(),
                });
                last_workspace = Some(pane.workspace_group_key.clone());
            }
            self.rows.push(RowItem::Pane {
                pane_id: pane.pane.pane_id.clone(),
            });
        }

        if self.rows.is_empty() {
            self.selection = 0;
            self.selected_pane_id = None;
            return;
        }

        if let Some(pane_id) = previous {
            if let Some(index) = self
                .rows
                .iter()
                .position(|row| matches!(row, RowItem::Pane { pane_id: row_pane_id } if row_pane_id == &pane_id))
            {
                self.selection = index;
                self.selected_pane_id = Some(pane_id);
                return;
            }
        }

        if let Some(pane_id) = self.restored_selection.pane_id.as_ref() {
            if let Some(index) = self
                .rows
                .iter()
                .position(|row| matches!(row, RowItem::Pane { pane_id: row_pane_id } if row_pane_id == pane_id))
            {
                self.set_selection(index);
                return;
            }
        }

        if let Some(index) = self.restored_selection.row_index {
            if let Some(index) = self.nearest_selectable_row(index) {
                self.set_selection(index);
                return;
            }
        }

        self.selection = self
            .rows
            .iter()
            .enumerate()
            .find_map(|(index, row)| match row {
                RowItem::Pane { pane_id } if self.is_blocked_pane_id(pane_id) => Some(index),
                _ => None,
            })
            .or_else(|| {
                self.rows
                    .iter()
                    .position(|row| matches!(row, RowItem::Pane { .. }))
            })
            .unwrap_or(0);
        self.selected_pane_id = match &self.rows[self.selection] {
            RowItem::Pane { pane_id } => Some(pane_id.clone()),
            RowItem::WorkspaceHeader { .. } => None,
        };
    }

    fn nearest_selectable_row(&self, index: usize) -> Option<usize> {
        if self.rows.is_empty() {
            return None;
        }

        let clamped = index.min(self.rows.len().saturating_sub(1));
        if matches!(self.rows.get(clamped), Some(RowItem::Pane { .. })) {
            return Some(clamped);
        }
        for forward in clamped + 1..self.rows.len() {
            if matches!(self.rows.get(forward), Some(RowItem::Pane { .. })) {
                return Some(forward);
            }
        }
        (0..clamped)
            .rev()
            .find(|candidate| matches!(self.rows.get(*candidate), Some(RowItem::Pane { .. })))
    }

    fn set_selection(&mut self, index: usize) {
        self.selection = index;
        self.selected_pane_id = match &self.rows[self.selection] {
            RowItem::Pane { pane_id } => Some(pane_id.clone()),
            RowItem::WorkspaceHeader { .. } => None,
        };
    }

    fn is_blocked_pane_id(&self, pane_id: &str) -> bool {
        self.panes
            .iter()
            .find(|pane| pane.pane.pane_id == pane_id)
            .map(|pane| is_attention_state(pane.state))
            .unwrap_or(false)
    }

    fn run_yolo_supervisor(&mut self) -> AppResult<()> {
        let mut message = None;
        for pane in self.panes.clone() {
            if !pane.is_claude() || !pane.yolo_enabled {
                continue;
            }

            let inspected = inspect_pane(&self.client, &pane.pane.pane_id, self.history_lines)?;
            match inspected.classification.state {
                SessionState::PermissionDialog if is_yolo_safe_to_approve(&inspected) => {
                    execute_classified_workflow(
                        &self.client,
                        &pane.pane.pane_id,
                        GuardedWorkflow::ApprovePermission,
                        &inspected.classification,
                    )?;
                    self.increment_yes_count(&pane);
                    message = Some(format!(
                        "approved permission for {}",
                        pane_descriptor(&pane.pane)
                    ));
                }
                SessionState::SurveyPrompt => {
                    execute_classified_workflow(
                        &self.client,
                        &pane.pane.pane_id,
                        GuardedWorkflow::DismissSurvey,
                        &inspected.classification,
                    )?;
                    message = Some(format!(
                        "dismissed survey for {}",
                        pane_descriptor(&pane.pane)
                    ));
                }
                _ => {}
            }
        }

        if let Some(message) = message {
            self.message = message;
        }
        Ok(())
    }

    fn apply_window_name_prefixes(&mut self) -> AppResult<()> {
        let mut windows = BTreeMap::<(String, String), Vec<&PaneEntry>>::new();
        for pane in &self.panes {
            windows
                .entry((pane.pane.session_name.clone(), pane.pane.window_id.clone()))
                .or_default()
                .push(pane);
        }

        let active_window_ids = windows
            .keys()
            .map(|(_, window_id)| window_id.clone())
            .collect::<HashSet<_>>();

        for ((session_name, window_id), mut panes) in windows {
            panes.sort_by_key(|pane| pane.pane.pane_index);
            let first = panes[0];
            let prefix = panes
                .iter()
                .map(|pane| pane_window_prefix(pane.state, pane.has_questions, pane.yolo_enabled))
                .collect::<String>();
            let managed = self
                .window_names
                .entry(window_id.clone())
                .or_insert_with(|| ManagedWindowName {
                    target: format!("{session_name}:{}", first.pane.window_index),
                    base_name: derive_base_window_name(&first.pane.window_name, &prefix),
                    applied_name: None,
                });
            managed.base_name = current_base_window_name(
                &first.pane.window_name,
                managed.applied_name.as_deref(),
                &managed.base_name,
                &prefix,
            );
            let target_name = format!("{prefix} {}", managed.base_name);
            if managed.applied_name.as_deref() != Some(target_name.as_str()) {
                self.client.rename_window(&managed.target, &target_name)?;
                managed.applied_name = Some(target_name);
            }
        }

        let stale = self
            .window_names
            .keys()
            .filter(|window_id| !active_window_ids.contains(*window_id))
            .cloned()
            .collect::<Vec<_>>();
        for window_id in stale {
            if let Some(managed) = self.window_names.remove(&window_id) {
                self.client
                    .rename_window(&managed.target, &managed.base_name)?;
            }
        }

        Ok(())
    }

    fn restore_window_names(&mut self) -> AppResult<()> {
        let mut windows = BTreeMap::new();
        for pane in &self.panes {
            windows.insert(
                pane.pane.window_id.clone(),
                (pane.pane.session_name.clone(), pane.pane.window_index),
            );
        }
        for (window_id, managed) in self.window_names.drain() {
            if let Some((session_name, window_index)) = windows.get(&window_id) {
                self.client.rename_window(
                    &format!("{session_name}:{window_index}"),
                    &managed.base_name,
                )?;
            } else {
                self.client
                    .rename_window(&managed.target, &managed.base_name)?;
            }
        }
        Ok(())
    }

    fn selected_pane(&self) -> Option<&PaneEntry> {
        let pane_id = self.selected_pane_id.as_ref()?;
        self.panes.iter().find(|pane| &pane.pane.pane_id == pane_id)
    }

    fn pane_for_row(&self, index: usize) -> Option<&PaneEntry> {
        let RowItem::Pane { pane_id } = self.rows.get(index)? else {
            return None;
        };
        self.panes.iter().find(|pane| &pane.pane.pane_id == pane_id)
    }

    fn select_next(&mut self) -> AppResult<()> {
        self.move_selection(1)
    }

    fn select_previous(&mut self) -> AppResult<()> {
        self.move_selection(-1)
    }

    fn move_selection(&mut self, delta: isize) -> AppResult<()> {
        if self.rows.is_empty() {
            return Ok(());
        }

        let mut index = self.selection as isize;
        for _ in 0..self.rows.len() {
            index = (index + delta).rem_euclid(self.rows.len() as isize);
            if let RowItem::Pane { pane_id } = &self.rows[index as usize] {
                self.selection = index as usize;
                self.selected_pane_id = Some(pane_id.clone());
                self.persist_selection()?;
                return Ok(());
            }
        }
        Ok(())
    }

    fn select_row(&mut self, index: usize) -> AppResult<bool> {
        if !matches!(self.rows.get(index), Some(RowItem::Pane { .. })) {
            return Ok(false);
        }
        self.set_selection(index);
        self.persist_selection()?;
        Ok(true)
    }

    fn persist_selection(&self) -> AppResult<()> {
        save_selection(
            &self.state_dir,
            &SavedSelection {
                pane_id: self.selected_pane_id.clone(),
                row_index: Some(self.selection),
            },
        )
    }

    fn toggle_selected_yolo(&mut self) -> AppResult<()> {
        let Some(pane) = self.selected_pane().cloned() else {
            return Ok(());
        };
        self.toggle_yolo_for_pane(&pane)
    }

    fn unstick_selected_pane(&mut self) -> AppResult<()> {
        let Some(pane) = self.selected_pane().cloned() else {
            return Ok(());
        };
        ensure_pane_owned_by_claude(&pane.pane)?;
        self.message = try_unstick_pane(&self.client, &pane.pane)?;
        if matches!(
            pane.state,
            SessionState::PermissionDialog | SessionState::FolderTrustPrompt
        ) {
            self.increment_yes_count(&pane);
        }
        Ok(())
    }

    fn increment_yes_count(&mut self, pane: &PaneEntry) {
        let key = yes_count_key(pane.claude_session_id.as_deref(), &pane.pane.pane_id);
        *self.yes_counts.entry(key).or_insert(0) += 1;
    }

    fn toggle_workspace_yolo(&mut self) -> AppResult<()> {
        let Some(selected) = self.selected_pane().cloned() else {
            return Ok(());
        };
        let workspace_group_key = selected.workspace_group_key.clone();
        let enable = self
            .panes
            .iter()
            .filter(|pane| pane.workspace_group_key == workspace_group_key)
            .any(|pane| !pane.yolo_enabled);

        for pane in self
            .panes
            .iter()
            .filter(|pane| pane.workspace_group_key == workspace_group_key)
            .cloned()
            .collect::<Vec<_>>()
        {
            self.set_yolo_for_pane(&pane, enable)?;
        }
        self.message = if enable {
            format!(
                "enabled yolo for workspace {}",
                selected.workspace_group_label
            )
        } else {
            format!(
                "disabled yolo for workspace {}",
                selected.workspace_group_label
            )
        };
        Ok(())
    }

    fn set_all_yolo(&mut self, enabled: bool) -> AppResult<()> {
        for pane in self.panes.clone() {
            self.set_yolo_for_pane(&pane, enabled)?;
        }
        self.message = if enabled {
            String::from("enabled yolo for all panes")
        } else {
            String::from("disabled yolo for all panes")
        };
        Ok(())
    }

    fn toggle_yolo_for_pane(&mut self, pane: &PaneEntry) -> AppResult<()> {
        if !pane.is_claude() {
            self.message = String::from("YOLO is only available for Claude panes");
            return Ok(());
        }
        self.set_yolo_for_pane(pane, !pane.yolo_enabled)?;
        self.message = if pane.yolo_enabled {
            format!("disabled yolo for {}", pane_descriptor(&pane.pane))
        } else {
            format!("enabled yolo for {}", pane_descriptor(&pane.pane))
        };
        Ok(())
    }

    fn set_yolo_for_pane(&mut self, pane: &PaneEntry, enabled: bool) -> AppResult<()> {
        if !pane.is_claude() {
            return Ok(());
        }
        ensure_pane_owned_by_claude(&pane.pane)?;
        if enabled {
            write_yolo_record(&self.state_dir, &pane.workspace.id, &pane.pane)?;
        } else {
            let _ = disable_yolo_record(&self.state_dir, &pane.pane.pane_id)?;
        }
        Ok(())
    }
}

impl PaneEntry {
    fn is_claude(&self) -> bool {
        matches!(self.source, PaneSource::Claude)
    }
}

fn dashboard_tmux_client() -> TmuxClient {
    match std::env::var_os("BOTCTL_DASHBOARD_TARGET_TMUX_SOCKET") {
        Some(socket_path) if !socket_path.is_empty() => TmuxClient::with_socket_path(socket_path),
        _ => TmuxClient::default(),
    }
}

fn host_tmux_client() -> TmuxClient {
    if std::env::var_os("BOTCTL_DASHBOARD_PERSISTENT_CHILD").is_some() {
        TmuxClient::with_socket(PERSISTENT_DASHBOARD_SOCKET)
    } else {
        TmuxClient::default()
    }
}

fn kill_persistent_dashboard_server() -> AppResult<()> {
    TmuxClient::with_socket(PERSISTENT_DASHBOARD_SOCKET).kill_session(PERSISTENT_DASHBOARD_SESSION)
}

fn should_return_navigation(exit_on_navigate: bool, inside_tmux: bool) -> bool {
    exit_on_navigate || !inside_tmux
}

fn open_dashboard_pane(
    app: &mut DashboardApp,
    pane: TmuxPane,
    exit_on_navigate: bool,
    persistent: bool,
) -> AppResult<Option<TmuxPane>> {
    app.persist_selection()?;
    if should_return_navigation(exit_on_navigate, std::env::var_os("TMUX").is_some()) {
        return Ok(Some(pane));
    }
    navigate_to_pane(&app.client, &pane)?;
    if persistent {
        app.host_client.detach_client()?;
    }
    app.message = format!("navigated to {}", pane_descriptor(&pane));
    Ok(None)
}

fn run_dashboard_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut DashboardApp,
    exit_on_navigate: bool,
    persistent: bool,
) -> AppResult<Option<TmuxPane>> {
    app.refresh()?;
    loop {
        terminal.draw(|frame| render_dashboard(frame, app))?;

        let timeout = Duration::from_millis(100);
        if event::poll(timeout)? {
            match event::read()? {
                Event::Key(key) => {
                    if key.kind != KeyEventKind::Press {
                        continue;
                    }
                    let code = match key.code {
                        KeyCode::Char(c)
                            if c.is_ascii_uppercase()
                                && !key.modifiers.contains(KeyModifiers::SHIFT) =>
                        {
                            KeyCode::Char(c.to_ascii_lowercase())
                        }
                        other => other,
                    };
                    match code {
                        KeyCode::Char('q') => {
                            app.persist_selection()?;
                            if persistent {
                                app.host_client.detach_client()?;
                                continue;
                            }
                            return Ok(None);
                        }
                        KeyCode::Char('X') => {
                            if persistent {
                                kill_persistent_dashboard_server()?;
                                return Ok(None);
                            }
                        }
                        KeyCode::Down | KeyCode::Char('j') => app.select_next()?,
                        KeyCode::Up | KeyCode::Char('k') => app.select_previous()?,
                        KeyCode::Enter => {
                            let Some(pane) = app.selected_pane().map(|pane| pane.pane.clone())
                            else {
                                continue;
                            };
                            if let Some(pane) =
                                open_dashboard_pane(app, pane, exit_on_navigate, persistent)?
                            {
                                return Ok(Some(pane));
                            }
                        }
                        KeyCode::Char('r') => app.refresh()?,
                        KeyCode::Char('u') => app.unstick_selected_pane()?,
                        KeyCode::Char('y') => app.toggle_selected_yolo()?,
                        KeyCode::Char('Y') => app.toggle_workspace_yolo()?,
                        KeyCode::Char('A') => app.set_all_yolo(true)?,
                        KeyCode::Char('N') => app.set_all_yolo(false)?,
                        _ => {}
                    }
                }
                Event::Mouse(mouse) => {
                    let size = terminal.size()?;
                    let area = Rect {
                        x: 0,
                        y: 0,
                        width: size.width,
                        height: size.height,
                    };
                    if let Some(pane) =
                        handle_mouse_event(app, mouse, area, exit_on_navigate, persistent)?
                    {
                        return Ok(Some(pane));
                    }
                }
                _ => {}
            }
        }

        if app.last_refresh.elapsed() >= Duration::from_millis(app.poll_ms) {
            app.refresh()?;
        }
    }
}

fn handle_mouse_event(
    app: &mut DashboardApp,
    mouse: MouseEvent,
    terminal_area: Rect,
    exit_on_navigate: bool,
    persistent: bool,
) -> AppResult<Option<TmuxPane>> {
    match mouse.kind {
        MouseEventKind::ScrollDown => {
            app.select_next()?;
            Ok(None)
        }
        MouseEventKind::ScrollUp => {
            app.select_previous()?;
            Ok(None)
        }
        MouseEventKind::Down(MouseButton::Left) => {
            let list_area = pane_list_content_area(terminal_area);
            if !rect_contains(list_area, mouse.column, mouse.row) {
                return Ok(None);
            }
            let index = (mouse.row - list_area.y) as usize;
            if yolo_column_contains(app, list_area, mouse.column)
                && let Some(pane) = app.pane_for_row(index).cloned()
            {
                app.select_row(index)?;
                app.toggle_yolo_for_pane(&pane)?;
                app.refresh()?;
                return Ok(None);
            }
            let was_selected = app.selection == index;
            if !app.select_row(index)? {
                return Ok(None);
            }
            if was_selected {
                let Some(pane) = app.selected_pane().map(|pane| pane.pane.clone()) else {
                    return Ok(None);
                };
                return open_dashboard_pane(app, pane, exit_on_navigate, persistent);
            }
            Ok(None)
        }
        _ => Ok(None),
    }
}

fn render_dashboard(frame: &mut ratatui::Frame<'_>, app: &DashboardApp) {
    let layout = dashboard_layout(frame.area());
    let body = dashboard_body_sections(layout.body);

    let panes_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(5)])
        .split(body[0]);

    let columns = pane_list_columns(app);
    let mut header_text = String::new();
    header_text.push(' ');
    header_text.push_str(&pad_display("", columns.state_width));
    header_text.push_str(TABLE_GAP);
    header_text.push_str(&pad_display("", columns.yolo_width));
    header_text.push_str(TABLE_GAP);
    header_text.push_str(&pad_display("Pane", columns.pane_width));
    header_text.push_str(TABLE_GAP);
    header_text.push_str(&pad_display("Uptime", columns.uptime_width));
    header_text.push_str(TABLE_GAP);
    header_text.push_str(&pad_display("Wait", columns.wait_width));
    header_text.push_str(TABLE_GAP);
    header_text.push_str(&pad_display_right("Yes", columns.yes_width));
    header_text.push_str(TABLE_GAP);
    header_text.push_str(&pad_display("Branch", columns.branch_width));
    header_text.push_str(TABLE_GAP);
    let header_width = panes_layout[0].width as usize;
    let header_display_width = UnicodeWidthStr::width(header_text.as_str());
    if header_display_width < header_width {
        header_text.push_str(&" ".repeat(header_width - header_display_width));
    }
    let header =
        Paragraph::new(Line::from(Span::styled(header_text, header_style()))).style(header_style());
    frame.render_widget(header, panes_layout[0]);

    let pane_list_width = rounded_block(None).inner(panes_layout[1]).width as usize;
    let mut items = Vec::new();
    for row in &app.rows {
        match row {
            RowItem::WorkspaceHeader { label } => items.push(ListItem::new(
                render_workspace_header_line(label, pane_list_width),
            )),
            RowItem::Pane { pane_id } => {
                let pane = app.panes.iter().find(|pane| &pane.pane.pane_id == pane_id);
                if let Some(pane) = pane {
                    let pane_target = pane_target_label(&pane.pane);
                    items.push(ListItem::new(Line::from(vec![
                        Span::raw(pad_display(
                            state_emoji(pane.state, pane.has_questions),
                            columns.state_width,
                        )),
                        Span::raw(TABLE_GAP),
                        Span::styled(
                            pad_display(yolo_marker(pane.yolo_enabled), columns.yolo_width),
                            Style::default().fg(if pane.yolo_enabled {
                                Color::Yellow
                            } else {
                                Color::DarkGray
                            }),
                        ),
                        Span::raw(TABLE_GAP),
                        Span::raw(pad_display(&pane_target, columns.pane_width)),
                        Span::raw(TABLE_GAP),
                        Span::raw(pad_display(&format_age(pane.age), columns.uptime_width)),
                        Span::raw(TABLE_GAP),
                        Span::raw(pad_display(
                            &pane.wait_duration.map(format_age).unwrap_or_default(),
                            columns.wait_width,
                        )),
                        Span::raw(TABLE_GAP),
                        Span::raw(pad_display_right(
                            &pane.yes_count.to_string(),
                            columns.yes_width,
                        )),
                        Span::raw(TABLE_GAP),
                        Span::raw(pad_display(&pane.branch, columns.branch_width)),
                        Span::raw(TABLE_GAP),
                    ])));
                }
            }
        }
    }

    let mut list_state = ListState::default();
    if !app.rows.is_empty() {
        list_state.select(Some(app.selection));
    }
    let list = List::new(items).block(rounded_block(None)).highlight_style(
        Style::default()
            .bg(Color::Blue)
            .add_modifier(Modifier::BOLD),
    );
    frame.render_stateful_widget(list, panes_layout[1], &mut list_state);

    let details_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(11), Constraint::Min(3)])
        .split(body[1]);

    if let Some(pane) = app.selected_pane() {
        let mut details = vec![
            Line::from(format!("Workspace: {}", detail_workspace_label(pane))),
            Line::from(format!("Provider: {}", pane_provider_label(pane))),
            Line::from(format!("Pane: {}", pane.pane.pane_id)),
            Line::from(format!(
                "Target: {}:{}.{}",
                pane.pane.session_name, pane.pane.window_index, pane.pane.pane_index
            )),
            Line::from(format!(
                "State: {} {}",
                state_emoji(pane.state, pane.has_questions),
                pane.state.as_str()
            )),
            Line::from(format!("Has questions: {}", pane.has_questions)),
            Line::from(format!(
                "YOLO: {}",
                if pane.yolo_enabled {
                    "enabled"
                } else {
                    "disabled"
                }
            )),
            Line::from(format!("Age: {}", format_age(pane.age))),
            Line::from(format!("Path: {}", detail_current_path(pane))),
        ];
        if pane.workspace.workspace_root != pane.workspace_group_key {
            details.push(Line::from(format!(
                "Worktree: {}",
                abbreviate_home_path(&pane.workspace.workspace_root)
            )));
        }
        if let PaneSource::OpenCode { title, .. } = &pane.source {
            details.push(Line::from(format!("Title: {title}")));
        }
        details.push(Line::from(format!(
            "Session ID: {}",
            pane_session_id(pane).unwrap_or("-")
        )));

        let details = Paragraph::new(details).block(rounded_block(Some("Details")));
        frame.render_widget(details, details_layout[0]);

        let body_kind = detail_body_kind(pane);
        let body_lines = detail_body_lines(pane, available_body_lines(details_layout[1]));
        let body = Paragraph::new(body_lines.into_iter().map(Line::from).collect::<Vec<_>>())
            .block(rounded_block(Some(detail_body_title(body_kind))))
            .wrap(Wrap {
                trim: matches!(body_kind, DetailBodyKind::Recap),
            });
        frame.render_widget(body, details_layout[1]);
    } else {
        let details = Paragraph::new(vec![Line::from("No supported panes found")])
            .block(rounded_block(Some("Details")));
        frame.render_widget(details, details_layout[0]);
        let body =
            Paragraph::new(vec![Line::from(String::new())]).block(rounded_block(Some("Context")));
        frame.render_widget(body, details_layout[1]);
    }

    let message = Paragraph::new(app.message.as_str())
        .block(rounded_block(Some("Status")))
        .wrap(Wrap { trim: true });
    frame.render_widget(message, layout.status);

    frame.render_widget(render_footer(), layout.footer);
}

struct DashboardLayout {
    body: Rect,
    status: Rect,
    footer: Rect,
}

fn dashboard_layout(area: Rect) -> DashboardLayout {
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(10),
            Constraint::Length(3),
            Constraint::Length(2),
        ])
        .split(area);
    DashboardLayout {
        body: layout[0],
        status: layout[1],
        footer: layout[2],
    }
}

fn dashboard_body_sections(area: Rect) -> Vec<Rect> {
    let preferred_left = area.width.saturating_mul(DASHBOARD_LEFT_PANE_PERCENT) / 100;
    let left_width = preferred_left.min(DASHBOARD_LEFT_PANE_MAX_WIDTH);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(left_width), Constraint::Min(0)])
        .split(area)
        .to_vec()
}

fn pane_list_content_area(area: Rect) -> Rect {
    let body = dashboard_body_sections(dashboard_layout(area).body);
    let panes_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(5)])
        .split(body[0]);
    rounded_block(None).inner(panes_layout[1])
}

fn yolo_column_contains(app: &DashboardApp, list_area: Rect, column: u16) -> bool {
    let columns = pane_list_columns(app);
    let x = column.saturating_sub(list_area.x) as usize;
    let yolo_start = columns.state_width + UnicodeWidthStr::width(TABLE_GAP);
    let yolo_end = yolo_start + columns.yolo_width;
    x >= yolo_start && x < yolo_end
}

fn render_footer() -> Paragraph<'static> {
    Paragraph::new(footer_lines(
        std::env::var_os("BOTCTL_DASHBOARD_PERSISTENT_CHILD").is_some(),
    ))
    .wrap(Wrap { trim: true })
}

fn footer_lines(persistent: bool) -> Vec<Line<'static>> {
    let line1 = if persistent {
        &FOOTER_LINE1[..FOOTER_LINE1.len() - 1]
    } else {
        FOOTER_LINE1
    };
    let mut line2 = FOOTER_LINE2.to_vec();
    if persistent {
        line2.extend_from_slice(FOOTER_LINE2_PERSISTENT_END);
    }
    let column_widths = footer_column_widths(line1, &line2);
    vec![
        footer_help_line(line1, &column_widths),
        footer_help_line(&line2, &column_widths),
    ]
}

fn footer_column_widths(line1: &[(&str, &str)], line2: &[(&str, &str)]) -> Vec<usize> {
    let columns = line1.len().max(line2.len());
    (0..columns)
        .map(|idx| {
            [line1.get(idx), line2.get(idx)]
                .into_iter()
                .flatten()
                .map(|(key, description)| {
                    UnicodeWidthStr::width(format!("{key} {description}").as_str())
                })
                .max()
                .unwrap_or(0)
        })
        .collect()
}

fn footer_help_line(entries: &[(&str, &str)], column_widths: &[usize]) -> Line<'static> {
    let mut spans = Vec::new();
    spans.push(Span::raw(FOOTER_INDENT));
    for (index, (key, description)) in entries.iter().enumerate() {
        if index > 0 {
            spans.push(Span::raw(FOOTER_COLUMN_GAP));
        }
        spans.push(Span::styled(
            (*key).to_string(),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            (*description).to_string(),
            Style::default().fg(Color::Gray),
        ));
        let rendered = format!("{key} {description}");
        let padding =
            column_widths[index].saturating_sub(UnicodeWidthStr::width(rendered.as_str()));
        if padding > 0 {
            spans.push(Span::raw(" ".repeat(padding)));
        }
    }
    Line::from(spans)
}

fn rect_contains(area: Rect, column: u16, row: u16) -> bool {
    column >= area.x
        && column < area.x.saturating_add(area.width)
        && row >= area.y
        && row < area.y.saturating_add(area.height)
}

struct PaneListColumns {
    state_width: usize,
    yolo_width: usize,
    pane_width: usize,
    uptime_width: usize,
    wait_width: usize,
    yes_width: usize,
    branch_width: usize,
}

fn pane_list_columns(app: &DashboardApp) -> PaneListColumns {
    PaneListColumns {
        state_width: 2,
        yolo_width: 2,
        pane_width: app
            .panes
            .iter()
            .map(|pane| UnicodeWidthStr::width(pane_target_label(&pane.pane).as_str()))
            .max()
            .unwrap_or(4)
            .max(4),
        uptime_width: app
            .panes
            .iter()
            .map(|pane| format_age(pane.age).len())
            .max()
            .unwrap_or(6)
            .max(6),
        wait_width: app
            .panes
            .iter()
            .map(|pane| pane.wait_duration.map(format_age).unwrap_or_default().len())
            .max()
            .unwrap_or(4)
            .max(4),
        yes_width: 3,
        branch_width: app
            .panes
            .iter()
            .map(|pane| UnicodeWidthStr::width(pane.branch.as_str()))
            .max()
            .unwrap_or(6)
            .max(6),
    }
}

fn header_style() -> Style {
    Style::default()
        .bg(Color::Magenta)
        .fg(Color::Rgb(0, 0, 0))
        .add_modifier(Modifier::BOLD)
}

fn rounded_block(title: Option<&str>) -> Block<'static> {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_set(border::ROUNDED);
    match title {
        Some(title) => block.title(title.to_string()),
        None => block,
    }
}

fn pad_display(value: &str, width: usize) -> String {
    let display_width = UnicodeWidthStr::width(value);
    if display_width >= width {
        value.to_string()
    } else {
        format!("{}{}", value, " ".repeat(width - display_width))
    }
}

fn pad_display_right(value: &str, width: usize) -> String {
    let display_width = UnicodeWidthStr::width(value);
    if display_width >= width {
        value.to_string()
    } else {
        format!("{}{}", " ".repeat(width - display_width), value)
    }
}

fn yes_count_key(claude_session_id: Option<&str>, pane_id: &str) -> String {
    claude_session_id.unwrap_or(pane_id).to_string()
}

fn render_workspace_header_line(label: &str, width: usize) -> Line<'static> {
    let style = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD);
    let Some((name, path)) = split_workspace_header(label) else {
        return Line::from(vec![Span::styled(pad_display_right(label, width), style)]);
    };

    let path = format!("({path})");

    let name_width = UnicodeWidthStr::width(name);
    let path_width = UnicodeWidthStr::width(path.as_str());
    let spacer_width = width.saturating_sub(name_width + path_width);

    Line::from(vec![
        Span::styled(name.to_string(), style),
        Span::styled(" ".repeat(spacer_width), style),
        Span::styled(path, style),
    ])
}

fn split_workspace_header(label: &str) -> Option<(&str, &str)> {
    let (name, path) = label.rsplit_once("  (")?;
    Some((name, path.strip_suffix(')')?))
}

fn detail_body_kind(pane: &PaneEntry) -> DetailBodyKind {
    match pane.state {
        SessionState::PermissionDialog
        | SessionState::UserQuestionPrompt
        | SessionState::PlanApprovalPrompt
        | SessionState::FolderTrustPrompt
        | SessionState::SurveyPrompt
        | SessionState::ExternalEditorActive
        | SessionState::DiffDialog
        | SessionState::Unknown => DetailBodyKind::BlockingDialog,
        SessionState::ChatReady if pane.recap_present => DetailBodyKind::Recap,
        SessionState::BusyResponding | SessionState::ChatReady => DetailBodyKind::Context,
    }
}

fn detail_body_title(kind: DetailBodyKind) -> &'static str {
    match kind {
        DetailBodyKind::BlockingDialog => "Dialog",
        DetailBodyKind::Recap => "Recap",
        DetailBodyKind::Context => "Context",
    }
}

fn available_body_lines(area: ratatui::layout::Rect) -> usize {
    area.height.saturating_sub(2) as usize
}

fn detail_body_lines(pane: &PaneEntry, max_lines: usize) -> Vec<String> {
    if max_lines == 0 {
        return Vec::new();
    }

    let lines = match detail_body_kind(pane) {
        DetailBodyKind::BlockingDialog => dialog_lines(&pane.focused_source),
        DetailBodyKind::Recap => recap_lines(pane),
        DetailBodyKind::Context => context_lines_above_input(&pane.focused_source),
    };

    if lines.is_empty() {
        return vec![String::from("<empty>")];
    }

    let start = lines.len().saturating_sub(max_lines);
    lines[start..].to_vec()
}

fn dialog_lines(source: &str) -> Vec<String> {
    source
        .lines()
        .map(str::trim_end)
        .filter(|line| !line.trim().is_empty())
        .map(str::to_string)
        .collect()
}

fn recap_lines(pane: &PaneEntry) -> Vec<String> {
    let recap_region = pane
        .focused_source
        .lines()
        .map(str::trim_end)
        .skip_while(|line| !is_recap_line(line))
        .take_while(|line| !is_chat_input_prompt(line))
        .filter(|line| !line.trim().is_empty())
        .map(clean_recap_line)
        .collect::<Vec<_>>();
    if recap_region.is_empty() {
        pane.recap_excerpt
            .as_ref()
            .map(|excerpt| vec![clean_recap_line(excerpt)])
            .unwrap_or_default()
    } else {
        collapse_wrapped_recap_lines(&recap_region)
    }
}

fn collapse_wrapped_recap_lines(lines: &[String]) -> Vec<String> {
    let mut filtered = lines
        .iter()
        .map(|line| line.trim())
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();

    while filtered
        .last()
        .is_some_and(|line| is_recap_separator_line(line))
    {
        filtered.pop();
    }

    let collapsed = filtered.join(" ");

    if collapsed.is_empty() {
        Vec::new()
    } else {
        vec![collapsed]
    }
}

fn is_recap_separator_line(line: &str) -> bool {
    let trimmed = line.trim();
    !trimmed.is_empty() && trimmed.chars().all(|ch| ch == '-' || ch == '─')
}

fn clean_recap_line(line: &str) -> String {
    let trimmed = line.trim();
    let lower = trimmed.to_ascii_lowercase();
    for prefix in ["※ recap:", "recap:"] {
        if lower.starts_with(prefix) {
            let cleaned = trimmed[prefix.len()..].trim();
            return if cleaned.is_empty() {
                String::new()
            } else {
                cleaned.to_string()
            };
        }
    }
    trimmed.to_string()
}

fn context_lines_above_input(source: &str) -> Vec<String> {
    let lines = source.lines().map(str::trim_end).collect::<Vec<_>>();
    let Some(prompt_index) = lines.iter().rposition(|line| is_chat_input_prompt(line)) else {
        return lines
            .into_iter()
            .filter(|line| !line.trim().is_empty())
            .map(str::to_string)
            .collect();
    };
    lines[..prompt_index]
        .iter()
        .rev()
        .skip_while(|line| line.trim().is_empty())
        .copied()
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .filter(|line| !line.trim().is_empty())
        .map(str::to_string)
        .collect()
}

fn is_chat_input_prompt(line: &str) -> bool {
    matches!(line.trim(), "❯" | ">")
}

fn is_recap_line(line: &str) -> bool {
    let lower = line.trim().to_ascii_lowercase();
    lower.starts_with("※ recap:")
        || lower.starts_with("recap:")
        || lower.contains("while you were away")
        || lower.contains("away summary")
}

fn workspace_group_key(workspace: &WorkspaceRecord) -> String {
    workspace
        .repo_key
        .as_deref()
        .and_then(repo_root_from_repo_key)
        .unwrap_or_else(|| workspace.workspace_root.clone())
}

fn repo_root_from_repo_key(repo_key: &str) -> Option<String> {
    let marker = "/.git";
    let index = repo_key.find(marker)?;
    let prefix = &repo_key[..index];
    if prefix.is_empty() {
        None
    } else {
        Some(prefix.to_string())
    }
}

fn workspace_group_label(workspace_root: &str) -> String {
    let workspace_root = abbreviate_home_path(workspace_root);
    let path = Path::new(&workspace_root);
    path.file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .map(|name| format!("{name}  ({workspace_root})"))
        .unwrap_or_else(|| workspace_root.to_string())
}

fn tmux_object_id_order(id: &str) -> u32 {
    id.trim_start_matches(|ch: char| !ch.is_ascii_digit())
        .parse::<u32>()
        .unwrap_or(u32::MAX)
}

fn current_base_window_name(
    current_name: &str,
    _applied_name: Option<&str>,
    _previous_base_name: &str,
    prefix: &str,
) -> String {
    derive_base_window_name(current_name, prefix)
}

fn derive_base_window_name(current_name: &str, prefix: &str) -> String {
    strip_dashboard_emoji_prefixes(current_name)
        .strip_prefix(&format!("{prefix} "))
        .unwrap_or_else(|| strip_dashboard_emoji_prefixes(current_name))
        .to_string()
}

fn strip_dashboard_emoji_prefixes(value: &str) -> &str {
    let mut rest = value.trim_start();
    loop {
        if let Some(stripped) = rest.strip_prefix(DASHBOARD_YOLO_MARKER) {
            rest = stripped.trim_start();
            continue;
        }
        if let Some(stripped) = LEGACY_YOLO_PREFIX_CHARS
            .iter()
            .find_map(|legacy| rest.strip_prefix(*legacy))
        {
            rest = stripped.trim_start();
            continue;
        }
        let Some(emoji) = DASHBOARD_STATE_EMOJIS
            .iter()
            .find(|emoji| rest.starts_with(**emoji))
        else {
            break;
        };
        rest = rest[emoji.len()..].trim_start();
    }
    rest
}

fn dashboard_display_state(
    classified_state: SessionState,
    previous_frame: Option<&String>,
    current_frame: &str,
) -> SessionState {
    if classified_state == SessionState::ChatReady
        && previous_frame.is_some_and(|previous_frame| previous_frame != current_frame)
    {
        SessionState::BusyResponding
    } else {
        classified_state
    }
}

fn dashboard_selection_path(state_dir: &Path) -> PathBuf {
    state_dir.join("dashboard-selection")
}

fn load_saved_selection(state_dir: &Path) -> AppResult<SavedSelection> {
    let path = dashboard_selection_path(state_dir);
    let Ok(content) = fs::read_to_string(path) else {
        return Ok(SavedSelection::default());
    };
    let mut lines = content.lines();
    let pane_id = lines
        .next()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let row_index = lines
        .next()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .and_then(|value| value.parse::<usize>().ok());
    Ok(SavedSelection { pane_id, row_index })
}

fn save_selection(state_dir: &Path, selection: &SavedSelection) -> AppResult<()> {
    let pane_id = selection.pane_id.as_deref().unwrap_or_default();
    let row_index = selection
        .row_index
        .map(|value| value.to_string())
        .unwrap_or_default();
    fs::write(
        dashboard_selection_path(state_dir),
        format!("{pane_id}\n{row_index}\n"),
    )?;
    Ok(())
}

fn workspace_name(workspace_root: &str) -> String {
    let workspace_root = abbreviate_home_path(workspace_root);
    let path = Path::new(&workspace_root);
    path.file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .unwrap_or(workspace_root)
}

fn repo_root_for_workspace(workspace: &WorkspaceRecord) -> Option<String> {
    workspace
        .repo_key
        .as_deref()
        .and_then(repo_root_from_repo_key)
}

fn uses_worktree(workspace: &WorkspaceRecord) -> bool {
    repo_root_for_workspace(workspace)
        .map(|repo_root| repo_root != workspace.workspace_root)
        .unwrap_or(false)
}

fn detail_workspace_label(pane: &PaneEntry) -> String {
    if uses_worktree(&pane.workspace) {
        pane.workspace_group_label.clone()
    } else {
        workspace_name(&pane.workspace_group_key)
    }
}

fn detail_current_path(pane: &PaneEntry) -> String {
    let abbreviated = abbreviate_home_path(&pane.pane.current_path);
    if !uses_worktree(&pane.workspace) {
        return abbreviated;
    }

    let Some(repo_root) = repo_root_for_workspace(&pane.workspace) else {
        return abbreviated;
    };
    let Ok(relative) = Path::new(&pane.pane.current_path).strip_prefix(&repo_root) else {
        return abbreviated;
    };
    if relative.as_os_str().is_empty() {
        abbreviated
    } else {
        relative.display().to_string()
    }
}

fn pane_provider_label(pane: &PaneEntry) -> &str {
    match pane.source {
        PaneSource::Claude => "Claude",
        PaneSource::OpenCode { .. } => "OpenCode",
    }
}

fn pane_session_id(pane: &PaneEntry) -> Option<&str> {
    match &pane.source {
        PaneSource::Claude => pane.claude_session_id.as_deref(),
        PaneSource::OpenCode { session_id, .. } => Some(session_id.as_str()),
    }
}

fn abbreviate_home_path(path: &str) -> String {
    let Some(home) = std::env::var_os("HOME") else {
        return path.to_string();
    };
    let home = home.to_string_lossy();
    if path == home {
        return String::from("~");
    }
    if let Some(suffix) = path.strip_prefix(&format!("{home}/")) {
        return format!("~/{suffix}");
    }
    path.to_string()
}

fn is_waiting_state(state: SessionState) -> bool {
    matches!(
        state,
        SessionState::ChatReady | SessionState::PermissionDialog | SessionState::FolderTrustPrompt
    )
}

fn is_attention_state(state: SessionState) -> bool {
    matches!(
        state,
        SessionState::PermissionDialog
            | SessionState::UserQuestionPrompt
            | SessionState::PlanApprovalPrompt
            | SessionState::FolderTrustPrompt
            | SessionState::SurveyPrompt
            | SessionState::ExternalEditorActive
            | SessionState::DiffDialog
            | SessionState::Unknown
    )
}

fn pane_descriptor(pane: &TmuxPane) -> String {
    format!(
        "{}:{}.{} ({})",
        pane.session_name, pane.window_index, pane.pane_index, pane.pane_id
    )
}

fn pane_target_label(pane: &TmuxPane) -> String {
    format!(
        "{}:{}.{}",
        pane.session_name, pane.window_index, pane.pane_index
    )
}

fn git_branch_for_path(path: &str) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .arg("rev-parse")
        .arg("--abbrev-ref")
        .arg("HEAD")
        .output();

    match output {
        Ok(output) if output.status.success() => String::from_utf8(output.stdout)
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| String::from("-")),
        _ => String::from("-"),
    }
}

fn yolo_marker(enabled: bool) -> &'static str {
    if enabled { "🤠" } else { "  " }
}

fn state_emoji(state: SessionState, has_questions: bool) -> &'static str {
    match state {
        SessionState::BusyResponding => "⚙️",
        SessionState::UserQuestionPrompt => "🤔",
        SessionState::ChatReady if has_questions => "🤔",
        SessionState::ChatReady => "💤",
        SessionState::PermissionDialog => "🔐",
        SessionState::PlanApprovalPrompt => "❓",
        SessionState::FolderTrustPrompt => "📁",
        SessionState::SurveyPrompt => "📝",
        SessionState::ExternalEditorActive => "✏️",
        SessionState::DiffDialog => "🧾",
        SessionState::Unknown => "❔",
    }
}

fn pane_window_prefix(state: SessionState, has_questions: bool, yolo_enabled: bool) -> String {
    let state = state_emoji(state, has_questions);
    if yolo_enabled {
        format!("{state}{DASHBOARD_YOLO_MARKER}")
    } else {
        state.to_string()
    }
}

fn format_age(age: Duration) -> String {
    let total = age.as_secs();
    let hours = total / 3600;
    let minutes = (total % 3600) / 60;
    let seconds = total % 60;
    if hours > 0 {
        format!("{hours:02}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes:02}:{seconds:02}")
    }
}

fn process_age(pid: Option<u32>) -> Option<Duration> {
    let pid = pid?;
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let (_, fields) = stat.rsplit_once(") ")?;
    let start_ticks = fields.split_whitespace().nth(19)?.parse::<u64>().ok()?;
    let ticks_per_second = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if ticks_per_second <= 0 {
        return None;
    }
    let uptime = fs::read_to_string("/proc/uptime").ok()?;
    let uptime_secs = uptime.split_whitespace().next()?.parse::<f64>().ok()?;
    let start_secs = start_ticks as f64 / ticks_per_second as f64;
    if uptime_secs < start_secs {
        return None;
    }
    Some(Duration::from_secs_f64(uptime_secs - start_secs))
}

fn is_claude_pane(pane: &TmuxPane) -> bool {
    pane.current_command.eq_ignore_ascii_case("claude")
}

fn navigate_to_pane(client: &TmuxClient, pane: &TmuxPane) -> AppResult<()> {
    client.select_window(&format!("{}:{}", pane.session_name, pane.window_index))?;
    client.select_pane(&pane.pane_id)?;
    if std::env::var_os("TMUX").is_some() {
        client.switch_client(&pane.session_name)?;
    } else {
        client.attach_session(&pane.session_name)?;
    }
    Ok(())
}

#[cfg(any(test, rust_analyzer))]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::time::Duration;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::{
        DASHBOARD_LEFT_PANE_MAX_WIDTH, DashboardApp, DetailBodyKind, PERSISTENT_DASHBOARD_SESSION,
        PERSISTENT_DASHBOARD_SOCKET, PaneEntry, PaneSource, SavedSelection, TABLE_GAP,
        abbreviate_home_path, context_lines_above_input, current_base_window_name,
        dashboard_body_sections, dashboard_display_state, dashboard_selection_path,
        derive_base_window_name, detail_body_kind, detail_current_path, detail_workspace_label,
        footer_column_widths, footer_help_line, footer_lines, format_age, load_saved_selection,
        pad_display_right, pane_list_columns, pane_list_content_area, pane_window_prefix,
        recap_lines, rect_contains, render_workspace_header_line, repo_root_from_repo_key,
        save_selection, should_return_navigation, split_workspace_header, state_emoji,
        strip_dashboard_emoji_prefixes, tmux_object_id_order, workspace_group_key,
        workspace_group_label, yes_count_key, yolo_column_contains,
    };
    use crate::classifier::SessionState;
    use crate::storage::WorkspaceRecord;
    use crate::tmux::{TmuxClient, TmuxPane};
    use ratatui::layout::Rect;
    use ratatui::style::Modifier;
    use unicode_width::UnicodeWidthStr;

    #[test]
    fn formats_short_age() {
        assert_eq!(format_age(Duration::from_secs(65)), "01:05");
    }

    #[test]
    fn formats_long_age() {
        assert_eq!(format_age(Duration::from_secs(3665)), "01:01:05");
    }

    #[test]
    fn maps_states_to_emojis() {
        assert_eq!(state_emoji(SessionState::PermissionDialog, false), "🔐");
        assert_eq!(state_emoji(SessionState::PlanApprovalPrompt, false), "❓");
        assert_eq!(state_emoji(SessionState::ChatReady, true), "🤔");
    }

    #[test]
    fn workspace_group_label_uses_leaf_name() {
        let label = workspace_group_label("/tmp/demo/project");
        assert!(label.starts_with("project  ("));
    }

    #[test]
    fn abbreviates_home_path() {
        let home = std::env::var("HOME").expect("HOME should be set for tests");
        assert_eq!(abbreviate_home_path(&home), "~");
        assert_eq!(
            abbreviate_home_path(&format!("{home}/demo/project")),
            "~/demo/project"
        );
    }

    #[test]
    fn repo_root_is_derived_from_standard_git_dir() {
        assert_eq!(
            repo_root_from_repo_key("/tmp/demo/project/.git"),
            Some(String::from("/tmp/demo/project"))
        );
    }

    #[test]
    fn repo_root_is_derived_from_worktree_common_dir() {
        assert_eq!(
            repo_root_from_repo_key("/tmp/demo/project/.git/worktrees/feature"),
            Some(String::from("/tmp/demo/project"))
        );
    }

    #[test]
    fn workspace_group_key_prefers_repo_identity() {
        let workspace = WorkspaceRecord {
            id: String::from("workspace-1"),
            workspace_root: String::from("/tmp/demo/project-worktree"),
            repo_key: Some(String::from("/tmp/demo/project/.git/worktrees/feature")),
        };
        assert_eq!(workspace_group_key(&workspace), "/tmp/demo/project");
    }

    #[test]
    fn tmux_object_id_order_parses_numeric_suffix() {
        assert_eq!(tmux_object_id_order("@12"), 12);
        assert_eq!(tmux_object_id_order("%7"), 7);
    }

    #[test]
    fn context_lines_stop_at_chat_input() {
        let lines = context_lines_above_input("alpha\nbeta\n❯\ninput");
        assert_eq!(lines, vec![String::from("alpha"), String::from("beta")]);
    }

    #[test]
    fn idle_pane_prefers_recap_body() {
        let pane = sample_pane_entry(
            SessionState::ChatReady,
            false,
            true,
            "※ recap: fixed issue\n❯",
        );
        assert_eq!(detail_body_kind(&pane), DetailBodyKind::Recap);
        assert_eq!(recap_lines(&pane), vec![String::from("fixed issue")]);
    }

    #[test]
    fn recap_excerpt_fallback_drops_recap_prefix() {
        let mut pane = sample_pane_entry(SessionState::ChatReady, false, true, "");
        pane.recap_excerpt = Some(String::from("※ recap: fixed issue"));
        assert_eq!(recap_lines(&pane), vec![String::from("fixed issue")]);
    }

    #[test]
    fn recap_lines_collapse_hard_wrapped_lines_before_rendering() {
        let pane = sample_pane_entry(
            SessionState::ChatReady,
            false,
            true,
            "※ recap: fixed issue on the dashboard\nthat used to wrap awkwardly\n❯",
        );

        assert_eq!(
            recap_lines(&pane),
            vec![String::from(
                "fixed issue on the dashboard that used to wrap awkwardly"
            )]
        );
    }

    #[test]
    fn recap_lines_drop_trailing_separator_lines() {
        let pane = sample_pane_entry(
            SessionState::ChatReady,
            false,
            true,
            "※ recap: fixed issue on the dashboard\n────────────────────────\n❯",
        );

        assert_eq!(
            recap_lines(&pane),
            vec![String::from("fixed issue on the dashboard")]
        );
    }

    #[test]
    fn detail_workspace_label_omits_root_for_non_worktree() {
        let pane = sample_pane_entry(SessionState::ChatReady, false, false, "");
        assert_eq!(detail_workspace_label(&pane), "demo");
    }

    #[test]
    fn detail_workspace_label_keeps_repo_root_for_worktree() {
        let pane = sample_worktree_pane_entry("/tmp/demo/project/worktrees/feature/src");
        assert_eq!(
            detail_workspace_label(&pane),
            "project  (/tmp/demo/project)"
        );
    }

    #[test]
    fn detail_path_strips_repo_root_for_nested_worktree_paths() {
        let pane = sample_worktree_pane_entry("/tmp/demo/project/worktrees/feature/src");
        assert_eq!(detail_current_path(&pane), "worktrees/feature/src");
    }

    #[test]
    fn detail_path_keeps_absolute_path_when_worktree_is_elsewhere() {
        let pane = sample_worktree_pane_entry("/tmp/demo/project-feature/src");
        assert_eq!(detail_current_path(&pane), "/tmp/demo/project-feature/src");
    }

    #[test]
    fn saved_selection_round_trips() {
        let state_dir = unique_temp_dir("dashboard-selection");
        fs::create_dir_all(&state_dir).expect("state dir should exist");

        save_selection(
            &state_dir,
            &SavedSelection {
                pane_id: Some(String::from("%9")),
                row_index: Some(4),
            },
        )
        .expect("selection should save");

        assert_eq!(
            load_saved_selection(&state_dir).expect("selection should load"),
            SavedSelection {
                pane_id: Some(String::from("%9")),
                row_index: Some(4),
            }
        );

        let _ = fs::remove_file(dashboard_selection_path(&state_dir));
        let _ = fs::remove_dir_all(&state_dir);
    }

    #[test]
    fn pane_list_content_area_excludes_borders() {
        let area = pane_list_content_area(Rect {
            x: 0,
            y: 0,
            width: 120,
            height: 30,
        });
        assert!(area.width > 0);
        assert!(area.height > 0);
        assert!(rect_contains(area, area.x, area.y));
    }

    #[test]
    fn dashboard_body_sections_caps_left_pane_width() {
        let narrow = dashboard_body_sections(Rect {
            x: 0,
            y: 0,
            width: 60,
            height: 20,
        });
        assert_eq!(narrow[0].width, 34);
        assert_eq!(narrow[1].width, 26);

        let wide = dashboard_body_sections(Rect {
            x: 0,
            y: 0,
            width: 200,
            height: 20,
        });
        assert_eq!(wide[0].width, DASHBOARD_LEFT_PANE_MAX_WIDTH);
        assert_eq!(wide[1].width, 120);
    }

    #[test]
    fn pad_display_right_left_pads_to_width() {
        assert_eq!(pad_display_right("demo", 8), "    demo");
        assert_eq!(pad_display_right("demo", 4), "demo");
    }

    #[test]
    fn yes_count_key_prefers_claude_session_id() {
        assert_eq!(yes_count_key(Some("session-123"), "%9"), "session-123");
        assert_eq!(yes_count_key(None, "%9"), "%9");
    }

    #[test]
    fn split_workspace_header_returns_name_and_path() {
        assert_eq!(
            split_workspace_header("my-workspace  (~/Code/my-workspace)"),
            Some(("my-workspace", "~/Code/my-workspace"))
        );
    }

    #[test]
    fn render_workspace_header_line_left_aligns_name_and_right_aligns_path() {
        let rendered = render_workspace_header_line("my-workspace  (~/Code/my-workspace)", 60);
        let text = rendered
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();

        assert!(text.starts_with("my-workspace"));
        assert!(text.ends_with("(~/Code/my-workspace)"));
        assert_eq!(UnicodeWidthStr::width(text.as_str()), 60);
    }

    #[test]
    fn persistent_footer_mentions_kill_server_action() {
        let lines = footer_lines(true)
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>();
        assert!(lines.iter().any(|line| line.contains("X kill server")));
    }

    #[test]
    fn footer_help_line_emphasizes_keys_and_groups_commands() {
        let widths = footer_column_widths(&[("Enter", "open")], &[("u", "unstick")]);
        let line = footer_help_line(&[("Enter", "open"), ("u", "unstick")], &[10, widths[0]]);
        let text = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();

        assert_eq!(text, "  Enter open    u unstick ");
        assert_eq!(line.spans[1].style.add_modifier, Modifier::BOLD);
    }

    #[test]
    fn kill_persistent_dashboard_server_uses_dedicated_socket_and_session() {
        let client = TmuxClient::with_socket(PERSISTENT_DASHBOARD_SOCKET);
        let plan = client.plan_kill_session(PERSISTENT_DASHBOARD_SESSION);

        assert_eq!(
            plan.render(),
            "tmux -L botctl-dashboard kill-session -t botctl-dashboard"
        );
    }

    #[test]
    fn changing_frame_promotes_chat_ready_to_busy_in_dashboard() {
        assert_eq!(
            dashboard_display_state(
                SessionState::ChatReady,
                Some(&String::from("before")),
                "after"
            ),
            SessionState::BusyResponding
        );
        assert_eq!(
            dashboard_display_state(SessionState::ChatReady, Some(&String::from("same")), "same"),
            SessionState::ChatReady
        );
    }

    #[test]
    fn derive_base_window_name_strips_matching_prefix() {
        assert_eq!(derive_base_window_name("💤⚙️ project", "💤⚙️"), "project");
        assert_eq!(derive_base_window_name("project", "💤⚙️"), "project");
    }

    #[test]
    fn derive_base_window_name_strips_any_known_dashboard_emojis() {
        assert_eq!(
            derive_base_window_name("🤔💤🔐 bloodraven", "💤"),
            "bloodraven"
        );
        assert_eq!(
            derive_base_window_name("🤔 💤  bloodraven", "💤"),
            "bloodraven"
        );
    }

    #[test]
    fn strip_dashboard_emoji_prefixes_leaves_plain_names_untouched() {
        assert_eq!(strip_dashboard_emoji_prefixes("bloodraven"), "bloodraven");
    }

    #[test]
    fn strip_dashboard_emoji_prefixes_handles_yolo_marker() {
        assert_eq!(
            strip_dashboard_emoji_prefixes("\u{2699}\u{fe0f}\u{02b8} project"),
            "project"
        );
        assert_eq!(
            strip_dashboard_emoji_prefixes("\u{2699}\u{fe0f}\u{02b8}\u{1f4a4} project"),
            "project"
        );
    }

    #[test]
    fn strip_dashboard_emoji_prefixes_handles_legacy_cowboy_zwj() {
        assert_eq!(
            strip_dashboard_emoji_prefixes("\u{2699}\u{fe0f}\u{200d}\u{1f920} project"),
            "project"
        );
        assert_eq!(
            strip_dashboard_emoji_prefixes("\u{2699}\u{fe0f}\u{1f920} project"),
            "project"
        );
    }

    #[test]
    fn pane_window_prefix_omits_marker_when_yolo_off() {
        let prefix = pane_window_prefix(SessionState::BusyResponding, false, false);
        assert_eq!(prefix, "\u{2699}\u{fe0f}");
    }

    #[test]
    fn pane_window_prefix_appends_superscript_y_when_yolo_on() {
        let prefix = pane_window_prefix(SessionState::BusyResponding, false, true);
        assert_eq!(prefix, "\u{2699}\u{fe0f}\u{02b8}");
    }

    #[test]
    fn current_base_window_name_preserves_live_rename() {
        assert_eq!(
            current_base_window_name(
                "renamed-project",
                Some("💤 old-project"),
                "old-project",
                "💤"
            ),
            "renamed-project"
        );
        assert_eq!(
            current_base_window_name(
                "💤 old-project",
                Some("💤 old-project"),
                "old-project",
                "💤"
            ),
            "old-project"
        );
        assert_eq!(
            current_base_window_name(
                "⚙️💤💤 ⚙️💤💤 ⚙️⚙️💤 bloodraven",
                Some("⚙️💤💤 ⚙️💤💤 ⚙️⚙️💤 bloodraven"),
                "⚙️💤💤 ⚙️⚙️💤 bloodraven",
                "⚙️💤💤"
            ),
            "bloodraven"
        );
    }

    #[test]
    fn navigation_returns_to_caller_only_when_requested_or_outside_tmux() {
        assert!(should_return_navigation(true, true));
        assert!(should_return_navigation(false, false));
        assert!(!should_return_navigation(false, true));
    }

    #[test]
    fn yolo_column_hitbox_matches_rendered_columns() {
        let app = DashboardApp::new(
            TmuxClient::default(),
            TmuxClient::default(),
            unique_temp_dir("dashboard-hitbox"),
            1000,
            120,
        )
        .expect("app should initialize");
        let list_area = Rect {
            x: 10,
            y: 5,
            width: 80,
            height: 10,
        };
        let columns = pane_list_columns(&app);
        let yolo_x =
            list_area.x + columns.state_width as u16 + UnicodeWidthStr::width(TABLE_GAP) as u16;
        assert!(yolo_column_contains(&app, list_area, yolo_x));
        assert!(!yolo_column_contains(&app, list_area, list_area.x));
    }

    fn sample_pane_entry(
        state: SessionState,
        has_questions: bool,
        recap_present: bool,
        focused_source: &str,
    ) -> PaneEntry {
        PaneEntry {
            pane: TmuxPane {
                pane_id: String::from("%1"),
                pane_tty: String::from("/dev/pts/1"),
                pane_pid: Some(1),
                session_id: String::from("$1"),
                session_name: String::from("demo"),
                window_id: String::from("@1"),
                window_index: 0,
                window_name: String::from("demo"),
                pane_index: 0,
                current_command: String::from("claude"),
                current_path: String::from("/tmp/demo"),
                pane_title: String::new(),
                pane_active: true,
                cursor_x: Some(0),
                cursor_y: Some(0),
            },
            source: PaneSource::Claude,
            workspace: WorkspaceRecord {
                id: String::from("workspace-1"),
                workspace_root: String::from("/tmp/demo"),
                repo_key: None,
            },
            workspace_group_key: String::from("/tmp/demo"),
            workspace_group_label: String::from("demo  (/tmp/demo)"),
            branch: String::from("main"),
            state,
            has_questions,
            recap_present,
            recap_excerpt: if recap_present {
                Some(String::from("fixed issue"))
            } else {
                None
            },
            yolo_enabled: false,
            age: Duration::from_secs(1),
            wait_duration: None,
            yes_count: 0,
            claude_session_id: None,
            focused_source: focused_source.to_string(),
        }
    }

    fn sample_worktree_pane_entry(current_path: &str) -> PaneEntry {
        PaneEntry {
            pane: TmuxPane {
                current_path: current_path.to_string(),
                ..sample_pane_entry(SessionState::ChatReady, false, false, "").pane
            },
            workspace: WorkspaceRecord {
                id: String::from("workspace-2"),
                workspace_root: String::from("/tmp/demo/project/worktrees/feature"),
                repo_key: Some(String::from("/tmp/demo/project/.git/worktrees/feature")),
            },
            workspace_group_key: String::from("/tmp/demo/project"),
            workspace_group_label: String::from("project  (/tmp/demo/project)"),
            ..sample_pane_entry(SessionState::ChatReady, false, false, "")
        }
    }

    fn unique_temp_dir(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be valid")
            .as_nanos();
        std::env::temp_dir().join(format!("botctl-{label}-{}-{nanos}", std::process::id()))
    }
}
