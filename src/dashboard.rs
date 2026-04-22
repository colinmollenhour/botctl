use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::io;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
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
use crate::prompt::resolve_state_dir;
use crate::storage::{
    WorkspaceRecord, bootstrap_state_db, resolve_workspace_for_path, sync_tmux_wait_state,
};
use crate::tmux::{TmuxClient, TmuxPane};
use crate::yolo::{disable_yolo_record, read_yolo_record, write_yolo_record};

const FOOTER_TEXT: &str = "Arrows/jk move  Enter open  u unstick  y toggle pane  Y toggle workspace  A all on  N all off  r refresh  q quit";
const TABLE_GAP: &str = "  ";

pub fn run(args: DashboardArgs) -> AppResult<String> {
    let state_dir = resolve_state_dir(args.state_dir.as_deref())?;
    bootstrap_state_db(&state_dir)?;

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = DashboardApp::new(state_dir, args.poll_ms, args.history_lines);
    let loop_result = run_dashboard_loop(&mut terminal, &mut app, args.exit_on_navigate);

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
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
    state_dir: std::path::PathBuf,
    poll_ms: u64,
    history_lines: usize,
    panes: Vec<PaneEntry>,
    rows: Vec<RowItem>,
    selected_pane_id: Option<String>,
    selection: usize,
    first_seen: HashMap<String, Instant>,
    window_names: HashMap<String, ManagedWindowName>,
    message: String,
    last_refresh: Instant,
}

#[derive(Clone)]
struct PaneEntry {
    pane: TmuxPane,
    workspace: WorkspaceRecord,
    workspace_group_key: String,
    workspace_group_label: String,
    branch: String,
    state: SessionState,
    recap_present: bool,
    recap_excerpt: Option<String>,
    yolo_enabled: bool,
    age: Duration,
    wait_duration: Option<Duration>,
    focused_source: String,
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

impl DashboardApp {
    fn new(state_dir: std::path::PathBuf, poll_ms: u64, history_lines: usize) -> Self {
        Self {
            client: TmuxClient::default(),
            state_dir,
            poll_ms,
            history_lines,
            panes: Vec::new(),
            rows: Vec::new(),
            selected_pane_id: None,
            selection: 0,
            first_seen: HashMap::new(),
            window_names: HashMap::new(),
            message: String::new(),
            last_refresh: Instant::now() - Duration::from_millis(poll_ms),
        }
    }

    fn refresh(&mut self) -> AppResult<()> {
        let now = Instant::now();
        let mut panes = Vec::new();
        let mut seen_pane_ids = HashSet::new();
        let mut branch_by_path = HashMap::new();

        for pane in self.client.list_panes()?.into_iter().filter(is_claude_pane) {
            seen_pane_ids.insert(pane.pane_id.clone());
            self.first_seen.entry(pane.pane_id.clone()).or_insert(now);

            let inspected = inspect_pane(&self.client, &pane.pane_id, self.history_lines)?;
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
                inspected.classification.state.as_str(),
                is_waiting_state(inspected.classification.state),
            )?;

            panes.push(PaneEntry {
                pane,
                workspace,
                workspace_group_key,
                workspace_group_label,
                branch,
                state: inspected.classification.state,
                recap_present: inspected.classification.recap_present,
                recap_excerpt: inspected.classification.recap_excerpt.clone(),
                yolo_enabled,
                age,
                wait_duration,
                focused_source: inspected.focused_source,
            });
        }

        self.first_seen
            .retain(|pane_id, _| seen_pane_ids.contains(pane_id));

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

    fn is_blocked_pane_id(&self, pane_id: &str) -> bool {
        self.panes
            .iter()
            .find(|pane| pane.pane.pane_id == pane_id)
            .map(|pane| is_attention_state(pane.state))
            .unwrap_or(false)
    }

    fn run_yolo_supervisor(&mut self) -> AppResult<()> {
        let mut message = None;
        for pane in &self.panes {
            if !pane.yolo_enabled {
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
            let managed = self
                .window_names
                .entry(window_id.clone())
                .or_insert_with(|| ManagedWindowName {
                    target: format!("{session_name}:{}", first.pane.window_index),
                    base_name: first.pane.window_name.clone(),
                    applied_name: None,
                });
            let prefix = panes
                .iter()
                .map(|pane| state_emoji(pane.state))
                .collect::<String>();
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

    fn select_next(&mut self) {
        self.move_selection(1);
    }

    fn select_previous(&mut self) {
        self.move_selection(-1);
    }

    fn move_selection(&mut self, delta: isize) {
        if self.rows.is_empty() {
            return;
        }

        let mut index = self.selection as isize;
        for _ in 0..self.rows.len() {
            index = (index + delta).rem_euclid(self.rows.len() as isize);
            if let RowItem::Pane { pane_id } = &self.rows[index as usize] {
                self.selection = index as usize;
                self.selected_pane_id = Some(pane_id.clone());
                return;
            }
        }
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
        Ok(())
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
        self.set_yolo_for_pane(pane, !pane.yolo_enabled)?;
        self.message = if pane.yolo_enabled {
            format!("disabled yolo for {}", pane_descriptor(&pane.pane))
        } else {
            format!("enabled yolo for {}", pane_descriptor(&pane.pane))
        };
        Ok(())
    }

    fn set_yolo_for_pane(&mut self, pane: &PaneEntry, enabled: bool) -> AppResult<()> {
        ensure_pane_owned_by_claude(&pane.pane)?;
        if enabled {
            write_yolo_record(&self.state_dir, &pane.workspace.id, &pane.pane)?;
        } else {
            let _ = disable_yolo_record(&self.state_dir, &pane.pane.pane_id)?;
        }
        Ok(())
    }
}

fn run_dashboard_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut DashboardApp,
    exit_on_navigate: bool,
) -> AppResult<Option<TmuxPane>> {
    app.refresh()?;
    loop {
        terminal.draw(|frame| render_dashboard(frame, app))?;

        let timeout = Duration::from_millis(100);
        if event::poll(timeout)? {
            if let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                match key.code {
                    KeyCode::Char('q') => return Ok(None),
                    KeyCode::Down | KeyCode::Char('j') => app.select_next(),
                    KeyCode::Up | KeyCode::Char('k') => app.select_previous(),
                    KeyCode::Enter => {
                        let Some(pane) = app.selected_pane().map(|pane| pane.pane.clone()) else {
                            continue;
                        };
                        if exit_on_navigate || std::env::var_os("TMUX").is_none() {
                            return Ok(Some(pane));
                        }
                        navigate_to_pane(&app.client, &pane)?;
                        app.message = format!("navigated to {}", pane_descriptor(&pane));
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
        }

        if app.last_refresh.elapsed() >= Duration::from_millis(app.poll_ms) {
            app.refresh()?;
        }
    }
}

fn render_dashboard(frame: &mut ratatui::Frame<'_>, app: &DashboardApp) {
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(10),
            Constraint::Length(3),
            Constraint::Length(2),
        ])
        .split(frame.area());
    let body = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(58), Constraint::Percentage(42)])
        .split(layout[0]);

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

    let mut items = Vec::new();
    for row in &app.rows {
        match row {
            RowItem::WorkspaceHeader { label } => {
                items.push(ListItem::new(Line::from(vec![Span::styled(
                    label.clone(),
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                )])))
            }
            RowItem::Pane { pane_id } => {
                let pane = app.panes.iter().find(|pane| &pane.pane.pane_id == pane_id);
                if let Some(pane) = pane {
                    let pane_target = pane_target_label(&pane.pane);
                    items.push(ListItem::new(Line::from(vec![
                        Span::raw(pad_display(state_emoji(pane.state), columns.state_width)),
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
        .constraints([Constraint::Length(9), Constraint::Min(3)])
        .split(body[1]);

    if let Some(pane) = app.selected_pane() {
        let mut details = vec![
            Line::from(format!("Workspace: {}", detail_workspace_label(pane))),
            Line::from(format!("Pane: {}", pane.pane.pane_id)),
            Line::from(format!(
                "Target: {}:{}.{}",
                pane.pane.session_name, pane.pane.window_index, pane.pane.pane_index
            )),
            Line::from(format!(
                "State: {} {}",
                state_emoji(pane.state),
                pane.state.as_str()
            )),
            Line::from(format!(
                "YOLO: {}",
                if pane.yolo_enabled {
                    "enabled"
                } else {
                    "disabled"
                }
            )),
            Line::from(format!("Age: {}", format_age(pane.age))),
            Line::from(format!(
                "Path: {}",
                detail_current_path(pane)
            )),
        ];
        if pane.workspace.workspace_root != pane.workspace_group_key {
            details.push(Line::from(format!(
                "Worktree: {}",
                abbreviate_home_path(&pane.workspace.workspace_root)
            )));
        }

        let details = Paragraph::new(details).block(rounded_block(Some("Details")));
        frame.render_widget(details, details_layout[0]);

        let body_kind = detail_body_kind(pane);
        let body_lines = detail_body_lines(pane, available_body_lines(details_layout[1]));
        let body = Paragraph::new(body_lines.into_iter().map(Line::from).collect::<Vec<_>>())
            .block(rounded_block(Some(detail_body_title(body_kind))));
        frame.render_widget(body, details_layout[1]);
    } else {
        let details = Paragraph::new(vec![Line::from("No Claude panes found")])
            .block(rounded_block(Some("Details")));
        frame.render_widget(details, details_layout[0]);
        let body =
            Paragraph::new(vec![Line::from(String::new())]).block(rounded_block(Some("Context")));
        frame.render_widget(body, details_layout[1]);
    }

    let message = Paragraph::new(app.message.as_str())
        .block(rounded_block(Some("Status")))
        .wrap(Wrap { trim: true });
    frame.render_widget(message, layout[1]);

    frame.render_widget(Paragraph::new(FOOTER_TEXT), layout[2]);
}

struct PaneListColumns {
    state_width: usize,
    yolo_width: usize,
    pane_width: usize,
    uptime_width: usize,
    wait_width: usize,
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

fn detail_body_kind(pane: &PaneEntry) -> DetailBodyKind {
    match pane.state {
        SessionState::PermissionDialog
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
        .map(str::to_string)
        .collect::<Vec<_>>();
    if recap_region.is_empty() {
        pane.recap_excerpt
            .as_ref()
            .map(|excerpt| vec![excerpt.clone()])
            .unwrap_or_default()
    } else {
        recap_region
    }
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

fn state_emoji(state: SessionState) -> &'static str {
    match state {
        SessionState::BusyResponding => "⚙️",
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
    use std::time::Duration;

    use super::{
        DetailBodyKind, PaneEntry, abbreviate_home_path, context_lines_above_input,
        detail_body_kind, detail_current_path, detail_workspace_label, format_age, recap_lines,
        repo_root_from_repo_key, state_emoji, tmux_object_id_order, workspace_group_key,
        workspace_group_label,
    };
    use crate::classifier::SessionState;
    use crate::storage::WorkspaceRecord;
    use crate::tmux::TmuxPane;

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
        assert_eq!(state_emoji(SessionState::PermissionDialog), "🔐");
        assert_eq!(state_emoji(SessionState::PlanApprovalPrompt), "❓");
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
        let pane = sample_pane_entry(SessionState::ChatReady, true, "※ recap: fixed issue\n❯");
        assert_eq!(detail_body_kind(&pane), DetailBodyKind::Recap);
        assert_eq!(
            recap_lines(&pane),
            vec![String::from("※ recap: fixed issue")]
        );
    }

    #[test]
    fn detail_workspace_label_omits_root_for_non_worktree() {
        let pane = sample_pane_entry(SessionState::ChatReady, false, "");
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

    fn sample_pane_entry(
        state: SessionState,
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
                pane_active: true,
                cursor_x: Some(0),
                cursor_y: Some(0),
            },
            workspace: WorkspaceRecord {
                id: String::from("workspace-1"),
                workspace_root: String::from("/tmp/demo"),
                repo_key: None,
            },
            workspace_group_key: String::from("/tmp/demo"),
            workspace_group_label: String::from("demo  (/tmp/demo)"),
            branch: String::from("main"),
            state,
            recap_present,
            recap_excerpt: if recap_present {
                Some(String::from("fixed issue"))
            } else {
                None
            },
            yolo_enabled: false,
            age: Duration::from_secs(1),
            wait_duration: None,
            focused_source: focused_source.to_string(),
        }
    }

    fn sample_worktree_pane_entry(current_path: &str) -> PaneEntry {
        PaneEntry {
            pane: TmuxPane {
                current_path: current_path.to_string(),
                ..sample_pane_entry(SessionState::ChatReady, false, "").pane
            },
            workspace: WorkspaceRecord {
                id: String::from("workspace-2"),
                workspace_root: String::from("/tmp/demo/project/worktrees/feature"),
                repo_key: Some(String::from("/tmp/demo/project/.git/worktrees/feature")),
            },
            workspace_group_key: String::from("/tmp/demo/project"),
            workspace_group_label: String::from("project  (/tmp/demo/project)"),
            ..sample_pane_entry(SessionState::ChatReady, false, "")
        }
    }
}
