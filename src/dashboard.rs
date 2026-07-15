use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
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
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use unicode_width::UnicodeWidthStr;

use crate::agy::{is_agy_pane, resolve_agy_session_for_pane};
use crate::app::{AppError, AppResult};
use crate::classifier::{
    Classifier, SIGNAL_CODEX_KEYWORDS, SessionState, prepare_frame_for_classification,
};
use crate::cli::DashboardArgs;
use crate::grok::{is_grok_pane, resolve_grok_session_for_pane};
use crate::last_message::resolve_codex_session_id_for_pane;
use crate::opencode::{pane_opencode_title, resolve_opencode_session_for_pane};
use crate::pi::resolve_pi_session_for_pane;
use crate::proc_fd::ChildResolver;
use crate::prompt::resolve_state_dir;
use crate::runtime::{RuntimeClient, RuntimeEvent, RuntimePaneSnapshot, RuntimeSubscription};
use crate::storage::{WorkspaceRecord, bootstrap_state_db, sync_tmux_runtime_state};
use crate::tmux::{TmuxClient, TmuxPane, TmuxWindow};

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
const VISIBILITY_PROBE_INTERVAL: Duration = Duration::from_secs(1);
const DETACHED_FALLBACK_INTERVAL: Duration = Duration::from_secs(3);
const DETACHED_RUNTIME_POLL_INTERVAL: Duration = Duration::from_secs(1);
const RUNTIME_RECONNECT_MIN: Duration = Duration::from_millis(250);
const RUNTIME_RECONNECT_MAX: Duration = Duration::from_secs(5);
const RUNTIME_STABLE_CONNECTION: Duration = Duration::from_secs(5);
const TABLE_GAP: &str = "  ";
const COMPACT_TABLE_GAP: &str = " ";
const ICON_GAP: &str = " ";
const DASHBOARD_LEFT_PANE_PERCENT: u16 = 58;
const DASHBOARD_LEFT_PANE_MAX_WIDTH: u16 = 80;
const RESOURCE_GAUGE_WIDTH: usize = 1;
const RESOURCE_GAUGE_LEVELS: &[char] = &['▏', '▎', '▍', '▌', '▋', '▊', '▉', '█'];
const GIB_BYTES: u64 = 1024 * 1024 * 1024;
const DASHBOARD_STATE_EMOJIS: &[&str] = &[
    "⚙️", "💬", "🤔", "💤", "🔐", "❓", "📁", "📝", "✏️", "🧾", "❔",
];
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

    let client = dashboard_tmux_client();
    let preferred_pane_id = current_dashboard_pane_id(&client);
    let mut app = DashboardApp::new(
        client,
        host_tmux_client(),
        RuntimeClient::connect(&state_dir)?,
        state_dir,
        args.poll_ms,
        args.history_lines,
        preferred_pane_id,
    )?;
    app.runtime_cache.start(app.runtime.clone());
    app.runtime_cache.wait_for_initial(Duration::from_secs(2));
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
    runtime: RuntimeClient,
    state_dir: std::path::PathBuf,
    poll_ms: u64,
    history_lines: usize,
    panes: Vec<PaneEntry>,
    rows: Vec<RowItem>,
    selected_pane_id: Option<String>,
    selection: usize,
    first_seen: HashMap<String, Instant>,
    previous_frames: HashMap<String, String>,
    /// Sticky cache for agy model labels. Preserves the last successfully
    /// parsed model label so it remains visible when the footer is temporarily
    /// unparseable (e.g. busy spinner obscuring the footer line).
    previous_agy_model: HashMap<String, String>,
    cpu_samples: HashMap<String, CpuSample>,
    yes_counts: HashMap<String, u32>,
    title_synchronizer: WindowTitleWorker,
    fallback_title_states: HashMap<String, TitlePaneState>,
    runtime_cache: RuntimeSnapshotCache,
    detached_fallback: DetachedFallbackWorker,
    message: String,
    last_refresh: Instant,
    restored_selection: SavedSelection,
    preferred_pane_id: Option<String>,
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
    yolo_effective: bool,
    yolo_stop_reason: Option<String>,
    age: Duration,
    cook_duration: Duration,
    wait_duration: Option<Duration>,
    runtime_duration_sample_at_unix_ms: Option<i64>,
    runtime_cook_active: bool,
    runtime_wait_active: bool,
    yes_count: u32,
    claude_session_id: Option<String>,
    focused_source: String,
    resource_usage: ResourceUsage,
    /// Parsed Gemini model label from the agy footer. `None` for non-agy
    /// panes and for agy panes where the label has never been seen yet.
    /// Populated with the sticky last-seen value on subsequent refreshes.
    agy_model: Option<String>,
}

#[derive(Debug, Clone, Copy, Default)]
struct ResourceUsage {
    cpu_percent: Option<f64>,
    memory_bytes: Option<u64>,
}

#[derive(Debug, Clone, Copy)]
struct CpuSample {
    at: Instant,
    total_ticks: u64,
    process_start_ticks: u64,
}

#[derive(Debug, Clone)]
struct ProcessSnapshot {
    pid: u32,
    ppid: u32,
    pgrp: i32,
    tpgid: i32,
    command: String,
    cmdline_basenames: Vec<String>,
    start_ticks: u64,
    cpu_ticks: u64,
    rss_pages: u64,
}

struct ProcessTreeSnapshot {
    snapshots: HashMap<u32, ProcessSnapshot>,
    children: HashMap<u32, Vec<u32>>,
    uptime_secs: Option<f64>,
    ticks_per_second: Option<u64>,
    page_size: Option<u64>,
}

impl ProcessTreeSnapshot {
    fn from_process_snapshots(snapshots: &[ProcessSnapshot]) -> Self {
        Self::from_process_snapshots_with_system(
            snapshots,
            process_uptime_secs(),
            ticks_per_second(),
            page_size(),
        )
    }

    fn from_process_snapshots_with_system(
        snapshots: &[ProcessSnapshot],
        uptime_secs: Option<f64>,
        ticks_per_second: Option<u64>,
        page_size: Option<u64>,
    ) -> Self {
        let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
        for snap in snapshots {
            children.entry(snap.ppid).or_default().push(snap.pid);
        }
        Self {
            snapshots: snapshots
                .iter()
                .cloned()
                .map(|snapshot| (snapshot.pid, snapshot))
                .collect(),
            children,
            uptime_secs,
            ticks_per_second,
            page_size,
        }
    }

    fn scan() -> Self {
        Self::scan_with(process_snapshots_with_cmdlines)
    }

    fn scan_with(scanner: impl FnOnce() -> Vec<ProcessSnapshot>) -> Self {
        Self::from_process_snapshots(&scanner())
    }

    fn descendants(&self, root_pid: u32) -> Vec<&ProcessSnapshot> {
        let mut descendants = Vec::new();
        let mut stack = vec![root_pid];
        let mut visited = HashSet::new();
        while let Some(pid) = stack.pop() {
            if !visited.insert(pid) {
                continue;
            }
            if let Some(snapshot) = self.snapshots.get(&pid) {
                descendants.push(snapshot);
            }
            if let Some(children) = self.children.get(&pid) {
                stack.extend(children.iter().copied());
            }
        }
        descendants
    }

    fn select_agent_process(
        &self,
        root_pid: u32,
        current_command: &str,
    ) -> Option<&ProcessSnapshot> {
        let root = self.snapshots.get(&root_pid)?;
        let descendants = self.descendants(root_pid);
        let foreground_pgrp = (root.tpgid > 0).then_some(root.tpgid);

        foreground_pgrp
            .and_then(|pgrp| {
                oldest_matching_process(descendants.iter().copied().filter(|snapshot| {
                    snapshot.pgrp == pgrp && process_matches_command(snapshot, current_command)
                }))
            })
            .or_else(|| {
                oldest_matching_process(
                    descendants
                        .iter()
                        .copied()
                        .filter(|snapshot| process_matches_command(snapshot, current_command)),
                )
            })
            .or_else(|| {
                foreground_pgrp.and_then(|pgrp| {
                    oldest_matching_process(
                        descendants
                            .iter()
                            .copied()
                            .filter(|snapshot| snapshot.pgrp == pgrp),
                    )
                })
            })
            .or(Some(root))
    }

    fn pane_age(&self, pane: &TmuxPane) -> Option<Duration> {
        let root_pid = pane.pane_pid?;
        let snapshot = self.select_agent_process(root_pid, &pane.current_command)?;
        let uptime_secs = self.uptime_secs?;
        let ticks_per_second = self.ticks_per_second? as f64;
        let start_secs = snapshot.start_ticks as f64 / ticks_per_second;
        (uptime_secs >= start_secs).then(|| Duration::from_secs_f64(uptime_secs - start_secs))
    }

    fn usage(&self, root_pid: u32) -> Option<ProcessTreeUsage> {
        let root = self.snapshots.get(&root_pid)?;
        let page_size = self.page_size?;
        let mut cpu_ticks = 0u64;
        let mut rss_pages = 0u64;
        for snapshot in self.descendants(root_pid) {
            cpu_ticks = cpu_ticks.saturating_add(snapshot.cpu_ticks);
            rss_pages = rss_pages.saturating_add(snapshot.rss_pages);
        }
        Some(ProcessTreeUsage {
            cpu_ticks,
            memory_bytes: rss_pages.saturating_mul(page_size),
            process_start_ticks: root.start_ticks,
        })
    }
}

impl ChildResolver for ProcessTreeSnapshot {
    fn children_of(&self, pid: u32) -> Vec<u32> {
        self.children.get(&pid).cloned().unwrap_or_default()
    }
}

#[derive(Debug, Clone, Copy)]
struct ProcessTreeUsage {
    cpu_ticks: u64,
    memory_bytes: u64,
    process_start_ticks: u64,
}

#[derive(Clone)]
enum PaneSource {
    Claude,
    Codex,
    OpenCode {
        session_id: Option<String>,
        title: String,
    },
    Pi {
        session_id: String,
    },
    Grok {
        session_id: Option<String>,
        title: Option<String>,
    },
    Agy {
        session_id: Option<String>,
    },
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

#[derive(Debug, Clone)]
struct TitlePaneState {
    pane: TmuxPane,
    state: SessionState,
    has_questions: bool,
    yolo_enabled: bool,
}

struct WindowTitleSynchronizer {
    client: TmuxClient,
    managed: HashMap<String, ManagedWindowName>,
    desired: HashMap<String, TitlePaneState>,
    retry_at: Option<Instant>,
}

#[derive(Clone, Default)]
struct TitleSyncRequest {
    desired: HashMap<String, TitlePaneState>,
    topology_complete: bool,
}

struct WindowTitleWorker {
    request: Arc<Mutex<TitleSyncRequest>>,
    wake_tx: mpsc::SyncSender<()>,
    error_rx: mpsc::Receiver<AppError>,
    restore_tx: mpsc::Sender<mpsc::SyncSender<AppResult<()>>>,
    stop: Arc<AtomicBool>,
}

trait WindowTitleIo: Send + 'static {
    fn apply(
        &mut self,
        desired: HashMap<String, TitlePaneState>,
        topology_complete: bool,
    ) -> AppResult<()>;
    fn retry_pending(&self) -> bool;
    fn restore(&mut self) -> AppResult<()>;
}

struct DetachedFallbackWorker {
    request_tx: mpsc::SyncSender<()>,
    result_rx: mpsc::Receiver<AppResult<HashMap<String, TitlePaneState>>>,
    stop: Arc<AtomicBool>,
}

struct EmptyChildResolver;

impl ChildResolver for EmptyChildResolver {
    fn children_of(&self, _pid: u32) -> Vec<u32> {
        Vec::new()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DashboardVisibility {
    Unknown,
    Visible,
    Detached,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DashboardLoopEffects {
    draw: bool,
    full_refresh: bool,
    detached_fallback: bool,
}

fn dashboard_loop_effects(
    visibility: DashboardVisibility,
    draw_requested: bool,
    full_refresh_due: bool,
    detached_fallback_due: bool,
) -> DashboardLoopEffects {
    if visibility == DashboardVisibility::Detached {
        DashboardLoopEffects {
            draw: false,
            full_refresh: false,
            detached_fallback: detached_fallback_due,
        }
    } else {
        DashboardLoopEffects {
            draw: draw_requested,
            full_refresh: full_refresh_due,
            detached_fallback: false,
        }
    }
}

#[derive(Default)]
struct RuntimeCacheState {
    snapshots: HashMap<String, RuntimePaneSnapshot>,
    generation: u64,
    subscription_boundary_supported: bool,
}

struct RuntimeSnapshotCache {
    shared: Arc<Mutex<RuntimeCacheState>>,
    wake_rx: mpsc::Receiver<()>,
    wake_tx: mpsc::SyncSender<()>,
    stop: Arc<AtomicBool>,
    started: bool,
}

#[derive(Debug, Clone, Copy)]
struct ReconnectBackoff {
    next: Duration,
}

impl Default for ReconnectBackoff {
    fn default() -> Self {
        Self {
            next: RUNTIME_RECONNECT_MIN,
        }
    }
}

impl ReconnectBackoff {
    fn failure_delay(&mut self, stable_connection: bool) -> Duration {
        if stable_connection {
            self.next = RUNTIME_RECONNECT_MIN;
        }
        let delay = self.next;
        self.next = if self.next >= Duration::from_secs(2) {
            RUNTIME_RECONNECT_MAX
        } else {
            self.next * 2
        };
        delay
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct SavedSelection {
    pane_id: Option<String>,
    row_index: Option<usize>,
}

impl WindowTitleSynchronizer {
    fn new(client: TmuxClient) -> Self {
        Self {
            client,
            managed: HashMap::new(),
            desired: HashMap::new(),
            retry_at: None,
        }
    }

    fn publish(&mut self, panes: impl IntoIterator<Item = TitlePaneState>) {
        self.desired = panes
            .into_iter()
            .map(|pane| (pane.pane.pane_id.clone(), pane))
            .collect();
    }

    fn synchronize(&mut self, topology_complete: bool) -> AppResult<()> {
        if self
            .retry_at
            .is_some_and(|retry_at| Instant::now() < retry_at)
        {
            return Ok(());
        }

        let windows = grouped_title_states(&self.desired);
        let active_window_ids = windows
            .keys()
            .map(|(_, window_id)| window_id.clone())
            .collect::<HashSet<_>>();

        for ((_session_name, window_id), panes) in windows {
            let first = panes[0];
            let prefix = panes
                .iter()
                .map(|pane| pane_window_prefix(pane.state, pane.has_questions, pane.yolo_enabled))
                .collect::<String>();
            let managed =
                self.managed
                    .entry(window_id.clone())
                    .or_insert_with(|| ManagedWindowName {
                        target: window_id.clone(),
                        base_name: derive_base_window_name(&first.pane.window_name, &prefix),
                        applied_name: None,
                    });
            managed.target = window_id;
            managed.base_name = current_base_window_name(
                &first.pane.window_name,
                managed.applied_name.as_deref(),
                &managed.base_name,
                &prefix,
            );
            let target_name = format!("{prefix} {}", managed.base_name);
            if managed.applied_name.as_deref() != Some(target_name.as_str()) {
                if let Err(error) = self.client.rename_window(&managed.target, &target_name) {
                    self.retry_at = Some(Instant::now() + Duration::from_secs(1));
                    return Err(error);
                }
                managed.applied_name = Some(target_name);
            }
        }

        if topology_complete {
            let windows = match self.client.list_windows() {
                Ok(windows) => windows,
                Err(error) => {
                    self.retry_at = Some(Instant::now() + Duration::from_secs(1));
                    return Err(error);
                }
            };
            let existing_window_ids = windows
                .iter()
                .map(|window| window.window_id.clone())
                .collect::<HashSet<_>>();
            let stale_to_restore = prune_missing_stale_managed_windows(
                &mut self.managed,
                &active_window_ids,
                &existing_window_ids,
            );
            for window_id in stale_to_restore {
                if let Some(managed) = self.managed.get(&window_id)
                    && let Err(error) = self
                        .client
                        .rename_window(&managed.target, &managed.base_name)
                {
                    self.retry_at = Some(Instant::now() + Duration::from_secs(1));
                    return Err(error);
                }
                self.managed.remove(&window_id);
            }
            for window in windows {
                if active_window_ids.contains(&window.window_id) {
                    continue;
                }
                let cleaned_name = cleanup_dashboard_window_name(&window);
                if cleaned_name != window.window_name
                    && let Err(error) = self.client.rename_window(&window.window_id, &cleaned_name)
                {
                    self.retry_at = Some(Instant::now() + Duration::from_secs(1));
                    return Err(error);
                }
            }
        }
        self.retry_at = None;
        Ok(())
    }

    fn restore(&mut self) -> AppResult<()> {
        let mut first_error = None;
        for managed in self.managed.drain().map(|(_, managed)| managed) {
            if let Err(error) = self
                .client
                .rename_window(&managed.target, &managed.base_name)
                && first_error.is_none()
            {
                first_error = Some(error);
            }
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    fn retry_pending(&self) -> bool {
        self.retry_at.is_some()
    }
}

impl WindowTitleIo for WindowTitleSynchronizer {
    fn apply(
        &mut self,
        desired: HashMap<String, TitlePaneState>,
        topology_complete: bool,
    ) -> AppResult<()> {
        self.publish(desired.into_values());
        self.synchronize(topology_complete)
    }

    fn retry_pending(&self) -> bool {
        self.retry_pending()
    }

    fn restore(&mut self) -> AppResult<()> {
        self.restore()
    }
}

impl WindowTitleWorker {
    fn new(client: TmuxClient) -> Self {
        Self::start_with(WindowTitleSynchronizer::new(client))
    }

    fn start_with(mut io: impl WindowTitleIo) -> Self {
        let request = Arc::new(Mutex::new(TitleSyncRequest::default()));
        let worker_request = Arc::clone(&request);
        let (wake_tx, wake_rx) = mpsc::sync_channel(1);
        let (error_tx, error_rx) = mpsc::sync_channel(1);
        let (restore_tx, restore_rx) = mpsc::channel::<mpsc::SyncSender<AppResult<()>>>();
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        thread::spawn(move || {
            while !worker_stop.load(Ordering::Relaxed) {
                if let Ok(reply) = restore_rx.try_recv() {
                    let _ = reply.send(io.restore());
                    worker_stop.store(true, Ordering::Relaxed);
                    break;
                }
                let woke = match wake_rx.recv_timeout(Duration::from_millis(100)) {
                    Ok(()) => true,
                    Err(mpsc::RecvTimeoutError::Timeout) => false,
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                };
                if !woke && !io.retry_pending() {
                    continue;
                }
                let request = {
                    let mut request = worker_request
                        .lock()
                        .expect("window title request lock poisoned");
                    let snapshot = request.clone();
                    request.topology_complete = false;
                    snapshot
                };
                if let Err(error) = io.apply(request.desired, request.topology_complete) {
                    let _ = error_tx.try_send(error);
                }
            }
        });
        Self {
            request,
            wake_tx,
            error_rx,
            restore_tx,
            stop,
        }
    }

    fn publish(&self, panes: impl IntoIterator<Item = TitlePaneState>, topology_complete: bool) {
        let mut request = self
            .request
            .lock()
            .expect("window title request lock poisoned");
        request.desired = panes
            .into_iter()
            .map(|pane| (pane.pane.pane_id.clone(), pane))
            .collect();
        request.topology_complete |= topology_complete;
        drop(request);
        let _ = self.wake_tx.try_send(());
    }

    fn drain_error(&self) -> Option<AppError> {
        let mut latest = None;
        while let Ok(error) = self.error_rx.try_recv() {
            latest = Some(error);
        }
        latest
    }

    fn restore(&self) -> AppResult<()> {
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        self.restore_tx
            .send(reply_tx)
            .map_err(|_| AppError::new("window title synchronizer stopped before restoration"))?;
        reply_rx
            .recv()
            .map_err(|_| AppError::new("window title synchronizer stopped during restoration"))?
    }
}

impl Drop for WindowTitleWorker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

fn prune_missing_stale_managed_windows(
    managed: &mut HashMap<String, ManagedWindowName>,
    active_window_ids: &HashSet<String>,
    existing_window_ids: &HashSet<String>,
) -> Vec<String> {
    let stale = managed
        .keys()
        .filter(|window_id| !active_window_ids.contains(*window_id))
        .cloned()
        .collect::<Vec<_>>();
    let mut existing_stale = Vec::new();
    for window_id in stale {
        if existing_window_ids.contains(&window_id) {
            existing_stale.push(window_id);
        } else {
            managed.remove(&window_id);
        }
    }
    existing_stale
}

fn grouped_title_states(
    states: &HashMap<String, TitlePaneState>,
) -> BTreeMap<(String, String), Vec<&TitlePaneState>> {
    let mut windows = BTreeMap::<(String, String), Vec<&TitlePaneState>>::new();
    for pane in states.values() {
        windows
            .entry((pane.pane.session_name.clone(), pane.pane.window_id.clone()))
            .or_default()
            .push(pane);
    }
    for panes in windows.values_mut() {
        panes.sort_by_key(|pane| pane.pane.pane_index);
    }
    windows
}

impl RuntimeSnapshotCache {
    fn new() -> Self {
        let (wake_tx, wake_rx) = mpsc::sync_channel(1);
        Self {
            shared: Arc::new(Mutex::new(RuntimeCacheState::default())),
            wake_rx,
            wake_tx,
            stop: Arc::new(AtomicBool::new(false)),
            started: false,
        }
    }

    fn start(&mut self, client: RuntimeClient) {
        if self.started {
            return;
        }
        self.started = true;
        let shared = Arc::clone(&self.shared);
        let wake = self.wake_tx.clone();
        let stop = Arc::clone(&self.stop);
        let poll_client = client.clone();
        let poll_shared = Arc::clone(&shared);
        let poll_wake = wake.clone();
        let poll_stop = Arc::clone(&stop);
        thread::spawn(move || runtime_cache_worker(client, shared, wake, stop));
        thread::spawn(move || {
            runtime_poll_fallback_worker(poll_client, poll_shared, poll_wake, poll_stop)
        });
    }

    fn snapshots(&self) -> Vec<RuntimePaneSnapshot> {
        self.shared
            .lock()
            .expect("runtime dashboard cache lock poisoned")
            .snapshots
            .values()
            .cloned()
            .collect()
    }

    fn drain_wake(&self) -> bool {
        let mut changed = false;
        while self.wake_rx.try_recv().is_ok() {
            changed = true;
        }
        changed
    }

    fn wait_for_initial(&self, timeout: Duration) {
        let _ = self.wake_rx.recv_timeout(timeout);
    }
}

fn runtime_poll_fallback_worker(
    client: RuntimeClient,
    shared: Arc<Mutex<RuntimeCacheState>>,
    wake: mpsc::SyncSender<()>,
    stop: Arc<AtomicBool>,
) {
    while !stop.load(Ordering::Relaxed) {
        if poll_runtime_cache_once(&client, &shared).unwrap_or(false) {
            let _ = wake.try_send(());
        }
        let sleep_until = Instant::now() + DETACHED_RUNTIME_POLL_INTERVAL;
        while !stop.load(Ordering::Relaxed) && Instant::now() < sleep_until {
            thread::sleep(Duration::from_millis(100));
        }
    }
}

fn poll_runtime_cache_once(
    client: &RuntimeClient,
    shared: &Mutex<RuntimeCacheState>,
) -> AppResult<bool> {
    if shared
        .lock()
        .expect("runtime dashboard cache lock poisoned")
        .subscription_boundary_supported
    {
        return Ok(false);
    }
    let snapshots = client.list_panes(None, None)?;
    Ok(replace_runtime_poll_cache(shared, snapshots))
}

impl Drop for RuntimeSnapshotCache {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

fn runtime_cache_worker(
    client: RuntimeClient,
    shared: Arc<Mutex<RuntimeCacheState>>,
    wake: mpsc::SyncSender<()>,
    stop: Arc<AtomicBool>,
) {
    let mut backoff = ReconnectBackoff::default();
    while !stop.load(Ordering::Relaxed) {
        set_runtime_subscription_capability(&shared, false);
        let (stable_connection, should_retry) = match bootstrap_runtime_subscription(&client) {
            Ok((mut subscription, snapshots, boundary_revision)) => {
                replace_runtime_cache(&shared, snapshots);
                set_runtime_subscription_capability(&shared, true);
                let _ = wake.try_send(());
                let stable = consume_runtime_subscription(
                    &mut subscription,
                    boundary_revision,
                    &shared,
                    &wake,
                    &stop,
                );
                set_runtime_subscription_capability(&shared, false);
                (stable, !stop.load(Ordering::Relaxed))
            }
            Err(_) => (false, true),
        };
        if !should_retry || stop.load(Ordering::Relaxed) {
            break;
        }
        thread::sleep(backoff.failure_delay(stable_connection));
    }
}

fn replace_runtime_cache(shared: &Mutex<RuntimeCacheState>, snapshots: Vec<RuntimePaneSnapshot>) {
    let mut cache = shared
        .lock()
        .expect("runtime dashboard cache lock poisoned");
    cache.snapshots = snapshots
        .into_iter()
        .map(|snapshot| (snapshot.pane.pane_id.clone(), snapshot))
        .collect();
    cache.generation = cache.generation.saturating_add(1);
}

fn replace_runtime_poll_cache(
    shared: &Mutex<RuntimeCacheState>,
    snapshots: Vec<RuntimePaneSnapshot>,
) -> bool {
    let mut cache = shared
        .lock()
        .expect("runtime dashboard cache lock poisoned");
    if cache.subscription_boundary_supported {
        return false;
    }
    cache.snapshots = snapshots
        .into_iter()
        .map(|snapshot| (snapshot.pane.pane_id.clone(), snapshot))
        .collect();
    cache.generation = cache.generation.saturating_add(1);
    true
}

fn set_runtime_subscription_capability(
    shared: &Mutex<RuntimeCacheState>,
    boundary_supported: bool,
) {
    shared
        .lock()
        .expect("runtime dashboard cache lock poisoned")
        .subscription_boundary_supported = boundary_supported;
}

fn bootstrap_runtime_subscription(
    client: &RuntimeClient,
) -> AppResult<(RuntimeSubscription, Vec<RuntimePaneSnapshot>, u64)> {
    let mut subscription = client.subscribe()?;
    let snapshots = subscription.bootstrap()?.into_values().collect::<Vec<_>>();
    let boundary_revision = snapshots
        .iter()
        .map(|snapshot| snapshot.revision)
        .max()
        .unwrap_or_default();
    Ok((subscription, snapshots, boundary_revision))
}

fn consume_runtime_subscription(
    subscription: &mut RuntimeSubscription,
    mut last_revision: u64,
    shared: &Arc<Mutex<RuntimeCacheState>>,
    wake: &mpsc::SyncSender<()>,
    stop: &AtomicBool,
) -> bool {
    let connected_at = Instant::now();
    let mut live_event_seen = false;
    while !stop.load(Ordering::Relaxed) {
        let event = match subscription.recv() {
            Ok(Some(event)) => event,
            Ok(None) => continue,
            Err(_) => {
                return subscription_connection_is_stable(live_event_seen, connected_at.elapsed());
            }
        };
        live_event_seen = true;
        if !advance_runtime_revision(&mut last_revision, event.revision) {
            continue;
        }
        if apply_runtime_event(shared, event) {
            let _ = wake.try_send(());
        }
    }
    subscription_connection_is_stable(live_event_seen, connected_at.elapsed())
}

fn subscription_connection_is_stable(live_event_seen: bool, connected_for: Duration) -> bool {
    live_event_seen || connected_for >= RUNTIME_STABLE_CONNECTION
}

fn advance_runtime_revision(last_revision: &mut u64, event_revision: u64) -> bool {
    if event_revision <= *last_revision {
        return false;
    }
    *last_revision = event_revision;
    true
}

fn apply_runtime_event(shared: &Mutex<RuntimeCacheState>, event: RuntimeEvent) -> bool {
    let mut cache = shared
        .lock()
        .expect("runtime dashboard cache lock poisoned");
    let changed = if event.kind == "pane-removed" {
        event
            .pane_id
            .as_deref()
            .and_then(|pane_id| cache.snapshots.remove(pane_id))
            .is_some()
    } else if let Some(snapshot) = event.snapshot {
        cache
            .snapshots
            .insert(snapshot.pane.pane_id.clone(), snapshot);
        true
    } else {
        false
    };
    if changed {
        cache.generation = cache.generation.saturating_add(1);
    }
    changed
}

impl DetachedFallbackWorker {
    fn start(
        client: TmuxClient,
        runtime_cache: Arc<Mutex<RuntimeCacheState>>,
        state_dir: PathBuf,
        history_lines: usize,
    ) -> Self {
        let (request_tx, request_rx) = mpsc::sync_channel(1);
        let (result_tx, result_rx) = mpsc::sync_channel(1);
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        thread::spawn(move || {
            while !worker_stop.load(Ordering::Relaxed) {
                match request_rx.recv_timeout(Duration::from_millis(100)) {
                    Ok(()) => {
                        let result = detached_fallback_title_pass(
                            &client,
                            &runtime_cache,
                            &state_dir,
                            history_lines,
                        );
                        if result_tx.send(result).is_err() {
                            break;
                        }
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
        });
        Self {
            request_tx,
            result_rx,
            stop,
        }
    }

    fn request(&self) -> bool {
        enqueue_detached_fallback(&self.request_tx)
    }

    fn try_result(&self) -> Option<AppResult<HashMap<String, TitlePaneState>>> {
        self.result_rx.try_recv().ok()
    }
}

fn enqueue_detached_fallback(request_tx: &mpsc::SyncSender<()>) -> bool {
    request_tx.try_send(()).is_ok()
}

impl Drop for DetachedFallbackWorker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

fn detached_fallback_title_pass(
    client: &TmuxClient,
    runtime_cache: &Mutex<RuntimeCacheState>,
    state_dir: &Path,
    history_lines: usize,
) -> AppResult<HashMap<String, TitlePaneState>> {
    let runtime_ids = runtime_cache
        .lock()
        .expect("runtime dashboard cache lock poisoned")
        .snapshots
        .keys()
        .cloned()
        .collect::<HashSet<_>>();
    let panes = client
        .list_panes_strict()?
        .into_iter()
        .filter(|pane| {
            !runtime_ids.contains(&pane.pane_id) && is_dashboard_fallback_candidate(pane)
        })
        .collect::<Vec<_>>();
    let process_tree = fallback_process_tree_with(&panes, ProcessTreeSnapshot::scan);
    let empty_resolver = EmptyChildResolver;
    let resolver: &dyn ChildResolver = process_tree
        .as_ref()
        .map(|tree| tree as &dyn ChildResolver)
        .unwrap_or(&empty_resolver);
    let mut next = HashMap::new();

    for pane in panes {
        let frame = client.capture_pane_ansi(&pane.pane_id, history_lines)?;
        let frame = prepare_frame_for_classification(&frame);
        let classification = Classifier.classify(&pane.pane_id, &focused_dashboard_frame(&frame));
        let opencode_state = resolve_opencode_session_for_pane(&pane).map(|session| session.state);
        let agy_session = if is_agy_pane(&pane) {
            resolve_agy_session_for_pane(&pane, &frame, resolver)?
        } else {
            None
        };
        if !is_dashboard_visible_non_runtime_pane_with(
            &pane,
            &frame,
            &classification.signals,
            agy_session.as_ref(),
            resolver,
        )? {
            continue;
        }
        let state = opencode_state.unwrap_or(classification.state);
        let source = pane_source_for_non_runtime_pane_with(
            &pane,
            &frame,
            &classification.signals,
            agy_session,
            resolver,
        )?;
        let workspace =
            crate::storage::resolve_workspace_for_path(state_dir, Path::new(&pane.current_path))?;
        let codex_session_id = if matches!(source, PaneSource::Codex) {
            resolve_codex_session_id_for_pane(&pane)?
        } else {
            None
        };
        let session_key = match &source {
            PaneSource::Claude => None,
            PaneSource::Codex => codex_session_id.as_deref(),
            PaneSource::OpenCode { session_id, .. } => session_id.as_deref(),
            PaneSource::Pi { session_id } => Some(session_id.as_str()),
            PaneSource::Grok { session_id, .. } => session_id.as_deref(),
            PaneSource::Agy { session_id } => session_id.as_deref(),
        };
        sync_tmux_runtime_state(
            state_dir,
            &workspace.id,
            &pane,
            state.as_str(),
            is_waiting_state(state),
            is_cooking_state(state),
            session_key,
        )?;
        next.insert(
            pane.pane_id.clone(),
            TitlePaneState {
                pane,
                state,
                has_questions: classification.has_questions,
                yolo_enabled: false,
            },
        );
    }

    Ok(next)
}

fn fallback_process_tree_with(
    panes: &[TmuxPane],
    scanner: impl FnOnce() -> ProcessTreeSnapshot,
) -> Option<ProcessTreeSnapshot> {
    panes
        .iter()
        .any(|pane| {
            pane.current_command.eq_ignore_ascii_case("pi")
                || pane.current_command.eq_ignore_ascii_case("agy")
                || pane.current_command.eq_ignore_ascii_case("grok")
        })
        .then(scanner)
}

impl DashboardApp {
    fn new(
        client: TmuxClient,
        host_client: TmuxClient,
        runtime: RuntimeClient,
        state_dir: std::path::PathBuf,
        poll_ms: u64,
        history_lines: usize,
        preferred_pane_id: Option<String>,
    ) -> AppResult<Self> {
        let restored_selection = load_saved_selection(&state_dir)?;
        let title_synchronizer = WindowTitleWorker::new(client.clone());
        let runtime_cache = RuntimeSnapshotCache::new();
        let detached_fallback = DetachedFallbackWorker::start(
            client.clone(),
            Arc::clone(&runtime_cache.shared),
            state_dir.clone(),
            history_lines,
        );
        Ok(Self {
            client,
            host_client,
            runtime,
            state_dir,
            poll_ms,
            history_lines,
            panes: Vec::new(),
            rows: Vec::new(),
            selected_pane_id: None,
            selection: 0,
            first_seen: HashMap::new(),
            previous_frames: HashMap::new(),
            previous_agy_model: HashMap::new(),
            cpu_samples: HashMap::new(),
            yes_counts: HashMap::new(),
            title_synchronizer,
            fallback_title_states: HashMap::new(),
            runtime_cache,
            detached_fallback,
            message: String::new(),
            last_refresh: Instant::now() - Duration::from_millis(poll_ms),
            restored_selection,
            preferred_pane_id,
        })
    }

    fn refresh(&mut self) -> AppResult<()> {
        let now = Instant::now();
        let mut panes = Vec::new();
        let mut seen_pane_ids = HashSet::new();
        let mut branch_by_path = HashMap::new();
        let mut next_frames = HashMap::new();
        let process_tree = ProcessTreeSnapshot::scan();
        let mut fallback_title_states = HashMap::new();

        for snapshot in self.runtime_cache.snapshots() {
            let pane = snapshot.pane.clone();
            seen_pane_ids.insert(pane.pane_id.clone());
            self.first_seen.entry(pane.pane_id.clone()).or_insert(now);

            let state = dashboard_display_state(
                snapshot.classification.state,
                self.previous_frames.get(&pane.pane_id),
                &snapshot.raw_source,
                None,
            );
            let source = if is_claude_pane(&pane) {
                PaneSource::Claude
            } else if is_agy_pane(&pane) {
                // Short-circuit on the process name. Even when the secondary
                // signal hasn't fired yet (state dir missing AND frame
                // fingerprint not yet captured) we must NOT fall through to
                // `PaneSource::Claude`, otherwise the dashboard would offer
                // YOLO / Claude keybindings against an agy TUI (AGENTS.md
                // guardrail). An unresolved session id is encoded as
                // `Agy { session_id: None }`.
                let session_id =
                    resolve_agy_session_for_pane(&pane, &snapshot.raw_source, &process_tree)?
                        .and_then(|session| session.id);
                PaneSource::Agy { session_id }
            } else if is_grok_pane(&pane) {
                let session =
                    resolve_grok_session_for_pane(&pane, &snapshot.raw_source, &process_tree)?;
                PaneSource::Grok {
                    session_id: session
                        .as_ref()
                        .map(|session| session.id.clone())
                        .filter(|id| !id.is_empty()),
                    title: session.and_then(|session| session.title),
                }
            } else if let Some(source) = opencode_source_for_pane(&pane) {
                source
            } else if let Some(pi_session) = resolve_pi_session_for_pane(&pane, &process_tree)? {
                PaneSource::Pi {
                    session_id: pi_session.id,
                }
            } else if is_codex_candidate_pane(&pane) {
                PaneSource::Codex
            } else {
                PaneSource::Claude
            };
            let workspace = WorkspaceRecord {
                id: snapshot.workspace_id.clone(),
                workspace_root: snapshot.workspace_root.clone(),
                repo_key: None,
            };
            let workspace_group_key = workspace_group_key(&workspace);
            let workspace_group_label = workspace_group_label(&workspace_group_key);
            let branch = branch_by_path
                .entry(pane.current_path.clone())
                .or_insert_with(|| git_branch_for_path(&pane.current_path))
                .clone();
            let age = process_tree.pane_age(&pane).unwrap_or_else(|| {
                self.first_seen
                    .get(&pane.pane_id)
                    .map(Instant::elapsed)
                    .unwrap_or_default()
            });
            next_frames.insert(pane.pane_id.clone(), snapshot.raw_source.clone());
            let resource_usage =
                self.resource_usage_for_pane(&process_tree, &pane.pane_id, pane.pane_pid, now);
            let cook_duration =
                Duration::from_millis(snapshot.cook_duration_ms.unwrap_or_default());
            let wait_duration = snapshot.wait_duration_ms.map(Duration::from_millis);
            let yes_count = self
                .yes_counts
                .get(&yes_count_key(
                    snapshot.claude_session_id.as_deref(),
                    &pane.pane_id,
                ))
                .copied()
                .unwrap_or(0);
            let agy_model = self.resolve_agy_model_sticky(&pane, &snapshot.raw_source);

            panes.push(PaneEntry {
                pane,
                source,
                workspace,
                workspace_group_key,
                workspace_group_label,
                branch,
                state,
                has_questions: snapshot.classification.has_questions,
                recap_present: snapshot.classification.recap_present,
                recap_excerpt: snapshot.classification.recap_excerpt.clone(),
                yolo_enabled: snapshot.desired_yolo_enabled,
                yolo_effective: snapshot.actual_yolo_enabled,
                yolo_stop_reason: snapshot.last_stop_reason.clone(),
                age,
                cook_duration,
                wait_duration,
                runtime_duration_sample_at_unix_ms: snapshot.duration_sampled_at_unix_ms,
                runtime_cook_active: snapshot.cook_duration_ms.is_some()
                    && is_cooking_state(snapshot.classification.state),
                runtime_wait_active: is_waiting_state(snapshot.classification.state),
                yes_count,
                claude_session_id: snapshot.claude_session_id.clone(),
                focused_source: snapshot.focused_source.clone(),
                resource_usage,
                agy_model,
            });
        }

        for pane in self.client.list_panes_strict()? {
            if seen_pane_ids.contains(&pane.pane_id) || !is_dashboard_fallback_candidate(&pane) {
                continue;
            }
            let frame = self
                .client
                .capture_pane_ansi(&pane.pane_id, self.history_lines)?;
            let frame = prepare_frame_for_classification(&frame);
            let classification =
                Classifier.classify(&pane.pane_id, &focused_dashboard_frame(&frame));
            let opencode_state =
                resolve_opencode_session_for_pane(&pane).map(|session| session.state);
            // Resolve the agy session at most once per pane per refresh and
            // thread the result into both visibility and source construction
            // to avoid duplicate fd-walk + history-tail parsing.
            let agy_session = if is_agy_pane(&pane) {
                resolve_agy_session_for_pane(&pane, &frame, &process_tree)?
            } else {
                None
            };
            if !is_dashboard_visible_non_runtime_pane_with(
                &pane,
                &frame,
                &classification.signals,
                agy_session.as_ref(),
                &process_tree,
            )? {
                continue;
            }
            seen_pane_ids.insert(pane.pane_id.clone());
            self.first_seen.entry(pane.pane_id.clone()).or_insert(now);
            let state = dashboard_display_state(
                classification.state,
                self.previous_frames.get(&pane.pane_id),
                &frame,
                opencode_state,
            );
            let source = pane_source_for_non_runtime_pane_with(
                &pane,
                &frame,
                &classification.signals,
                agy_session,
                &process_tree,
            )?;
            let workspace = crate::storage::resolve_workspace_for_path(
                &self.state_dir,
                Path::new(&pane.current_path),
            )?;
            let workspace_group_key = workspace_group_key(&workspace);
            let workspace_group_label = workspace_group_label(&workspace_group_key);
            let branch = branch_by_path
                .entry(pane.current_path.clone())
                .or_insert_with(|| git_branch_for_path(&pane.current_path))
                .clone();
            let age = process_tree.pane_age(&pane).unwrap_or_else(|| {
                self.first_seen
                    .get(&pane.pane_id)
                    .map(Instant::elapsed)
                    .unwrap_or_default()
            });
            next_frames.insert(pane.pane_id.clone(), frame.clone());
            let resource_usage =
                self.resource_usage_for_pane(&process_tree, &pane.pane_id, pane.pane_pid, now);
            let codex_session_id = if matches!(source, PaneSource::Codex) {
                resolve_codex_session_id_for_pane(&pane)?
            } else {
                None
            };
            let session_key = match &source {
                PaneSource::Claude => None,
                PaneSource::Codex => codex_session_id.as_deref(),
                PaneSource::OpenCode { session_id, .. } => session_id.as_deref(),
                PaneSource::Pi { session_id } => Some(session_id.as_str()),
                PaneSource::Grok { session_id, .. } => session_id.as_deref(),
                PaneSource::Agy { session_id } => session_id.as_deref(),
            };
            let duration_state = opencode_state.unwrap_or(classification.state);
            let runtime_durations = sync_tmux_runtime_state(
                &self.state_dir,
                &workspace.id,
                &pane,
                duration_state.as_str(),
                is_waiting_state(duration_state),
                is_cooking_state(duration_state),
                session_key,
            )?;
            let agy_model = self.resolve_agy_model_sticky(&pane, &frame);
            fallback_title_states.insert(
                pane.pane_id.clone(),
                TitlePaneState {
                    pane: pane.clone(),
                    state: duration_state,
                    has_questions: classification.has_questions,
                    yolo_enabled: false,
                },
            );

            panes.push(PaneEntry {
                pane,
                source,
                workspace,
                workspace_group_key,
                workspace_group_label,
                branch,
                state,
                has_questions: classification.has_questions,
                recap_present: classification.recap_present,
                recap_excerpt: classification.recap_excerpt.clone(),
                yolo_enabled: false,
                yolo_effective: false,
                yolo_stop_reason: None,
                age,
                cook_duration: runtime_durations.cook_duration.unwrap_or_default(),
                wait_duration: runtime_durations.wait_duration,
                runtime_duration_sample_at_unix_ms: None,
                runtime_cook_active: false,
                runtime_wait_active: false,
                yes_count: 0,
                claude_session_id: None,
                focused_source: focused_dashboard_frame(&frame),
                resource_usage,
                agy_model,
            });
        }

        self.first_seen
            .retain(|pane_id, _| seen_pane_ids.contains(pane_id));
        self.cpu_samples
            .retain(|pane_id, _| seen_pane_ids.contains(pane_id));
        self.previous_frames = next_frames;
        self.previous_agy_model
            .retain(|pane_id, _| seen_pane_ids.contains(pane_id));
        self.fallback_title_states = fallback_title_states;

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

        if let Some(pane_id) = take_requested_dashboard_pane_selection(&self.state_dir)? {
            self.preferred_pane_id = Some(pane_id);
        }

        self.panes = panes;
        self.rebuild_rows();
        self.synchronize_titles(true)?;
        self.last_refresh = Instant::now();
        Ok(())
    }

    fn apply_requested_pane_selection(&mut self) -> AppResult<()> {
        if let Some(pane_id) = take_requested_dashboard_pane_selection(&self.state_dir)? {
            self.preferred_pane_id = Some(pane_id);
            self.rebuild_rows();
        }
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

        if let Some(pane_id) = self.preferred_pane_id.take()
            && let Some(index) = self
                .rows
                .iter()
                .position(|row| matches!(row, RowItem::Pane { pane_id: row_pane_id } if row_pane_id == &pane_id))
        {
            self.selection = index;
            self.selected_pane_id = Some(pane_id);
            return;
        }

        if let Some(pane_id) = previous
            && let Some(index) = self
                .rows
                .iter()
                .position(|row| matches!(row, RowItem::Pane { pane_id: row_pane_id } if row_pane_id == &pane_id))
        {
            self.selection = index;
            self.selected_pane_id = Some(pane_id);
            return;
        }

        if let Some(pane_id) = self.restored_selection.pane_id.as_ref()
            && let Some(index) = self
                .rows
                .iter()
                .position(|row| matches!(row, RowItem::Pane { pane_id: row_pane_id } if row_pane_id == pane_id))
        {
            self.set_selection(index);
            return;
        }

        if let Some(index) = self.restored_selection.row_index
            && let Some(index) = self.nearest_selectable_row(index)
        {
            self.set_selection(index);
            return;
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

    fn restore_window_names(&mut self) -> AppResult<()> {
        self.title_synchronizer.restore()
    }

    fn drain_title_error(&mut self) {
        if let Some(error) = self.title_synchronizer.drain_error() {
            self.message = format!("window title update pending: {error}");
        }
    }

    fn synchronize_titles(&mut self, topology_complete: bool) -> AppResult<()> {
        let runtime_snapshots = self.runtime_cache.snapshots();
        let runtime_ids = runtime_snapshots
            .iter()
            .map(|snapshot| snapshot.pane.pane_id.clone())
            .collect::<HashSet<_>>();
        let mut states = runtime_snapshots
            .into_iter()
            .map(|snapshot| TitlePaneState {
                pane: snapshot.pane,
                state: snapshot.classification.state,
                has_questions: snapshot.classification.has_questions,
                yolo_enabled: snapshot.desired_yolo_enabled,
            })
            .collect::<Vec<_>>();
        states.extend(
            self.fallback_title_states
                .values()
                .filter(|state| !runtime_ids.contains(&state.pane.pane_id))
                .cloned(),
        );
        self.title_synchronizer.publish(states, topology_complete);
        Ok(())
    }

    fn clear_detached_resource_state(&mut self) {
        self.cpu_samples.clear();
    }

    fn finish_reattach_attempt(&mut self, succeeded: bool) -> bool {
        if !succeeded {
            self.clear_detached_resource_state();
        }
        succeeded
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
        let result = self
            .runtime
            .run_action(&pane.pane.pane_id, None, "auto-unstick")?;
        self.message = format!("{} {}", result.pane_id, result.executed);
        if !result.executed.is_empty()
            && matches!(
                pane.state,
                SessionState::PermissionDialog | SessionState::FolderTrustPrompt
            )
        {
            self.increment_yes_count(&pane);
        }
        Ok(())
    }

    fn increment_yes_count(&mut self, pane: &PaneEntry) {
        let key = yes_count_key(pane.claude_session_id.as_deref(), &pane.pane.pane_id);
        *self.yes_counts.entry(key).or_insert(0) += 1;
    }

    fn resource_usage_for_pane(
        &mut self,
        process_tree: &ProcessTreeSnapshot,
        pane_id: &str,
        pid: Option<u32>,
        now: Instant,
    ) -> ResourceUsage {
        let Some(pid) = pid else {
            self.cpu_samples.remove(pane_id);
            return ResourceUsage::default();
        };
        let Some(total) = process_tree.usage(pid) else {
            self.cpu_samples.remove(pane_id);
            return ResourceUsage::default();
        };

        let sample = CpuSample {
            at: now,
            total_ticks: total.cpu_ticks,
            process_start_ticks: total.process_start_ticks,
        };
        let cpu_percent = self
            .cpu_samples
            .insert(pane_id.to_string(), sample)
            .and_then(|previous| {
                cpu_percent_between(previous, sample, process_tree.ticks_per_second?)
            });

        ResourceUsage {
            cpu_percent,
            memory_bytes: Some(total.memory_bytes),
        }
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
        if !pane.supports_yolo() {
            self.message = String::from("YOLO is only available for Claude or Codex panes");
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
        if !pane.supports_yolo() {
            return Ok(());
        }
        let _ = self
            .runtime
            .set_yolo(Some(&pane.pane.pane_id), None, false, enabled)?;
        Ok(())
    }
}

impl PaneEntry {
    fn supports_yolo(&self) -> bool {
        matches!(self.source, PaneSource::Claude | PaneSource::Codex)
    }

    fn displayed_cook_duration(&self) -> Duration {
        self.runtime_duration_sample_at_unix_ms
            .and_then(|sampled_at| {
                runtime_snapshot_duration(
                    Some(self.cook_duration.as_millis().min(u64::MAX as u128) as u64),
                    sampled_at,
                    self.runtime_cook_active,
                )
            })
            .unwrap_or(self.cook_duration)
    }

    fn displayed_wait_duration(&self) -> Option<Duration> {
        match self.runtime_duration_sample_at_unix_ms {
            Some(sampled_at) => runtime_snapshot_duration(
                self.wait_duration
                    .map(|duration| duration.as_millis().min(u64::MAX as u128) as u64),
                sampled_at,
                self.runtime_wait_active,
            ),
            None => self.wait_duration,
        }
    }
}

impl DashboardApp {
    /// Resolve the agy model label for a pane, using the sticky cache for
    /// resilience when the footer is temporarily unparseable.
    ///
    /// When the pane is no longer agy, the cache entry is evicted immediately
    /// (V-4 fix: prevents stale labels leaking into a future agy session that
    /// reuses the same pane_id). Returns `None` for non-agy panes.
    fn resolve_agy_model_sticky(
        &mut self,
        pane: &crate::tmux::TmuxPane,
        frame: &str,
    ) -> Option<String> {
        if !is_agy_pane(pane) {
            // Pane is no longer agy — evict any stale cache entry.
            self.previous_agy_model.remove(&pane.pane_id);
            return None;
        }
        let parsed = crate::agy::extract_model_label(frame);
        match parsed {
            Some(label) => {
                self.previous_agy_model
                    .insert(pane.pane_id.clone(), label.clone());
                Some(label)
            }
            None => self.previous_agy_model.get(&pane.pane_id).cloned(),
        }
    }

    fn max_memory_bytes(&self) -> Option<u64> {
        self.panes
            .iter()
            .filter_map(|pane| pane.resource_usage.memory_bytes)
            .max()
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
    let persistent_child = std::env::var_os("BOTCTL_DASHBOARD_PERSISTENT_CHILD").is_some();
    let mut visibility = if persistent_child {
        DashboardVisibility::Unknown
    } else {
        DashboardVisibility::Visible
    };
    let mut last_visibility_probe = Instant::now() - VISIBILITY_PROBE_INTERVAL;
    let mut last_detached_fallback = Instant::now() - DETACHED_FALLBACK_INTERVAL;
    let mut draw_needed = true;
    let mut last_clock_second = unix_time_ms() / 1000;

    loop {
        app.drain_title_error();
        let runtime_changed = app.runtime_cache.drain_wake();
        if runtime_changed {
            app.synchronize_titles(false)?;
        }

        if persistent_child && last_visibility_probe.elapsed() >= VISIBILITY_PROBE_INTERVAL {
            last_visibility_probe = Instant::now();
            let probed = visibility_from_attached_count(
                app.host_client
                    .session_attached_count(PERSISTENT_DASHBOARD_SESSION)
                    .map_err(|_| ()),
            );
            match (visibility, probed) {
                (
                    DashboardVisibility::Visible | DashboardVisibility::Unknown,
                    DashboardVisibility::Detached,
                ) => {
                    app.persist_selection()?;
                    app.clear_detached_resource_state();
                    visibility = DashboardVisibility::Detached;
                    draw_needed = false;
                }
                (DashboardVisibility::Detached, DashboardVisibility::Visible) => {
                    let refreshed = app.refresh().is_ok();
                    if app.finish_reattach_attempt(refreshed) {
                        visibility = DashboardVisibility::Visible;
                        draw_needed = true;
                    }
                }
                (_, next) => visibility = next,
            }
        }

        while let Some(result) = app.detached_fallback.try_result() {
            if visibility == DashboardVisibility::Detached
                && let Ok(states) = result
            {
                app.fallback_title_states = states;
                app.synchronize_titles(true)?;
            }
        }

        let effects = dashboard_loop_effects(
            visibility,
            draw_needed,
            app.last_refresh.elapsed() >= Duration::from_millis(app.poll_ms),
            last_detached_fallback.elapsed() >= DETACHED_FALLBACK_INTERVAL,
        );
        if visibility == DashboardVisibility::Detached {
            if effects.detached_fallback && app.detached_fallback.request() {
                last_detached_fallback = Instant::now();
            }
            thread::sleep(Duration::from_millis(100));
            continue;
        }

        if visibility == DashboardVisibility::Unknown {
            visibility = DashboardVisibility::Visible;
        }
        if effects.draw {
            app.apply_requested_pane_selection()?;
            terminal.draw(|frame| render_dashboard(frame, app))?;
            draw_needed = false;
        }

        let timeout = Duration::from_millis(100);
        if event::poll(timeout)? {
            app.apply_requested_pane_selection()?;
            draw_needed = true;
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

        if effects.full_refresh {
            app.refresh()?;
            draw_needed = true;
        }
        let clock_second = unix_time_ms() / 1000;
        if clock_second != last_clock_second {
            last_clock_second = clock_second;
            draw_needed = true;
        }
    }
}

fn visibility_from_attached_count(attached_count: Result<u32, ()>) -> DashboardVisibility {
    match attached_count {
        Ok(0) => DashboardVisibility::Detached,
        Ok(_) | Err(()) => DashboardVisibility::Visible,
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
            let index = pane_list_scroll_start(app, list_area.height as usize)
                + (mouse.row - list_area.y) as usize;
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
    render_pane_list_header(frame, panes_layout[0], &columns);
    render_pane_list(frame, app, panes_layout[1], &columns);

    if let Some(pane) = app.selected_pane() {
        let mut details = vec![
            Line::from(format!("Workspace: {}", detail_workspace_label(pane))),
            Line::from(format!("Provider: {}", pane_provider_label(pane))),
        ];
        if let PaneSource::Agy { .. } = &pane.source
            && let Some(model) = pane.agy_model.as_deref()
        {
            details.push(Line::from(format!("Model: {model}")));
        }
        details.extend([
            Line::from(format!(
                "Pane: {} ({})",
                pane.pane.pane_id,
                pane_target_label(&pane.pane)
            )),
            Line::from(format!("PID: {}", format_pid(pane.pane.pane_pid))),
            Line::from(format!(
                "State: {} {}",
                state_emoji(pane.state, pane.has_questions),
                pane.state.as_str()
            )),
            Line::from(format!("Has questions: {}", pane.has_questions)),
            Line::from(format!(
                "YOLO desired: {}",
                if pane.yolo_enabled {
                    "enabled"
                } else {
                    "disabled"
                }
            )),
            Line::from(format!(
                "YOLO active: {}",
                if pane.yolo_effective {
                    "active"
                } else {
                    "inactive"
                }
            )),
            Line::from(format!("Age: {}", format_age(pane.age))),
            Line::from(format!(
                "Avg CPU: {}",
                format_cpu_percent(pane.resource_usage.cpu_percent)
            )),
            Line::from(format!(
                "Memory: {}",
                format_memory_bytes(pane.resource_usage.memory_bytes)
            )),
            Line::from(format!("Path: {}", detail_current_path(pane))),
        ]);
        if pane.workspace.workspace_root != pane.workspace_group_key {
            details.push(Line::from(format!(
                "Worktree: {}",
                abbreviate_home_path(&pane.workspace.workspace_root)
            )));
        }
        if let PaneSource::OpenCode { title, .. } = &pane.source {
            details.push(Line::from(format!("Title: {title}")));
        }
        if let PaneSource::Grok {
            title: Some(title), ..
        } = &pane.source
        {
            details.push(Line::from(format!("Title: {title}")));
        }
        details.push(Line::from(format!(
            "Session ID: {}",
            pane_session_id(pane).unwrap_or("-")
        )));
        details.push(Line::from(format!(
            "Stop reason: {}",
            pane.yolo_stop_reason.as_deref().unwrap_or("-")
        )));

        let details_layout = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(details_panel_height(details.len())),
                Constraint::Min(3),
            ])
            .split(body[1]);

        let details = Paragraph::new(details).block(rounded_block(Some("Details")));
        frame.render_widget(details, details_layout[0]);

        let body_kind = detail_body_kind(pane);
        let body_lines = detail_body_lines(
            pane,
            available_body_lines(details_layout[1]),
            available_body_columns(details_layout[1]),
        );
        let body = Paragraph::new(body_lines.into_iter().map(Line::from).collect::<Vec<_>>())
            .block(rounded_block(Some(detail_body_title(body_kind))))
            .wrap(Wrap {
                trim: matches!(body_kind, DetailBodyKind::Recap),
            });
        frame.render_widget(body, details_layout[1]);
    } else {
        let details_layout = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(3), Constraint::Min(3)])
            .split(body[1]);

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

fn details_panel_height(line_count: usize) -> u16 {
    line_count.saturating_add(2).min(u16::MAX as usize) as u16
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
    let rects = pane_list_column_rects(list_area, &columns);
    column >= rects.yolo.x && column < rects.yolo.x.saturating_add(rects.yolo.width)
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

struct PaneListColumnRects {
    state: Rect,
    agent: Rect,
    yolo: Rect,
    cpu: Rect,
    mem: Rect,
    cook: Rect,
    wait: Rect,
    yes: Rect,
    branch: Rect,
}

fn render_pane_list_header(frame: &mut ratatui::Frame<'_>, area: Rect, columns: &PaneListColumns) {
    frame.render_widget(Paragraph::new(" ").style(header_style()), area);
    let rects = pane_list_column_rects(area, columns);
    render_cell_right(frame, rects.cpu, "CPU", header_style());
    render_cell_right(frame, rects.mem, "MEM", header_style());
    render_cell(frame, rects.cook, "Cook", header_style());
    render_cell(frame, rects.wait, "Wait", header_style());
    render_cell_right(frame, rects.yes, "🤠", header_style());
    render_cell(frame, rects.branch, "Branch", header_style());
}

fn render_pane_list(
    frame: &mut ratatui::Frame<'_>,
    app: &DashboardApp,
    area: Rect,
    columns: &PaneListColumns,
) {
    let block = rounded_block(None);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let start = pane_list_scroll_start(app, inner.height as usize);
    let rects = pane_list_column_rects(inner, columns);

    for (visible_row, row) in app
        .rows
        .iter()
        .skip(start)
        .take(inner.height as usize)
        .enumerate()
    {
        let y = inner.y + visible_row as u16;
        let row_area = Rect {
            y,
            height: 1,
            ..inner
        };
        let selected = start + visible_row == app.selection;
        if selected {
            frame.render_widget(
                Paragraph::new(" ").style(
                    Style::default()
                        .bg(Color::Blue)
                        .add_modifier(Modifier::BOLD),
                ),
                row_area,
            );
        }
        let row_style = if selected {
            Style::default()
                .bg(Color::Blue)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };

        match row {
            RowItem::WorkspaceHeader { label } => {
                let line = render_workspace_header_line(label, inner.width as usize);
                frame.render_widget(Paragraph::new(line).style(row_style), row_area);
            }
            RowItem::Pane { pane_id } => {
                let Some(pane) = app.panes.iter().find(|pane| &pane.pane.pane_id == pane_id) else {
                    continue;
                };
                render_cell(
                    frame,
                    rect_at_y(rects.state, y),
                    state_emoji(pane.state, pane.has_questions),
                    row_style,
                );
                render_cell(
                    frame,
                    rect_at_y(rects.agent, y),
                    agent_emoji(pane),
                    row_style,
                );
                render_cell(
                    frame,
                    rect_at_y(rects.yolo, y),
                    yolo_marker(pane.yolo_enabled),
                    row_style.fg(if pane.yolo_enabled {
                        Color::Yellow
                    } else {
                        Color::DarkGray
                    }),
                );
                render_cell(
                    frame,
                    rect_at_y(rects.cpu, y),
                    &format_cpu_gauge(pane.resource_usage.cpu_percent),
                    row_style,
                );
                render_cell(
                    frame,
                    rect_at_y(rects.mem, y),
                    &format_memory_gauge(pane.resource_usage.memory_bytes, app.max_memory_bytes()),
                    row_style,
                );
                render_cell(
                    frame,
                    rect_at_y(rects.cook, y),
                    &format_duration_compact(pane.displayed_cook_duration()),
                    row_style,
                );
                render_cell(
                    frame,
                    rect_at_y(rects.wait, y),
                    &pane
                        .displayed_wait_duration()
                        .map(format_duration_compact)
                        .unwrap_or_default(),
                    row_style,
                );
                render_cell_right(
                    frame,
                    rect_at_y(rects.yes, y),
                    &pane.yes_count.to_string(),
                    row_style,
                );
                render_cell(frame, rect_at_y(rects.branch, y), &pane.branch, row_style);
            }
        }
    }
}

fn pane_list_scroll_start(app: &DashboardApp, visible_rows: usize) -> usize {
    if visible_rows == 0 || app.selection < visible_rows {
        0
    } else {
        app.selection + 1 - visible_rows
    }
}

fn pane_list_column_rects(area: Rect, columns: &PaneListColumns) -> PaneListColumnRects {
    let mut x = area.x.saturating_add(1);
    let gap = UnicodeWidthStr::width(TABLE_GAP) as u16;
    let compact_gap = UnicodeWidthStr::width(COMPACT_TABLE_GAP) as u16;
    let icon_gap = UnicodeWidthStr::width(ICON_GAP) as u16;

    let state = take_rect(area, &mut x, columns.state_width as u16);
    x = x.saturating_add(icon_gap);
    let agent = take_rect(area, &mut x, columns.agent_width as u16);
    x = x.saturating_add(icon_gap);
    let yolo = take_rect(area, &mut x, columns.yolo_width as u16);
    x = x.saturating_add(gap);
    let cpu = take_rect(area, &mut x, columns.cpu_width as u16);
    x = x.saturating_add(compact_gap);
    let mem = take_rect(area, &mut x, columns.mem_width as u16);
    x = x.saturating_add(gap);
    let cook = take_rect(area, &mut x, columns.cook_width as u16);
    x = x.saturating_add(gap);
    let wait = take_rect(area, &mut x, columns.wait_width as u16);
    x = x.saturating_add(gap);
    let yes = take_rect(area, &mut x, columns.yes_width as u16);
    x = x.saturating_add(gap);
    let branch_width = area.x.saturating_add(area.width).saturating_sub(x);
    let branch = Rect {
        x,
        y: area.y,
        width: branch_width,
        height: area.height,
    };

    PaneListColumnRects {
        state,
        agent,
        yolo,
        cpu,
        mem,
        cook,
        wait,
        yes,
        branch,
    }
}

fn take_rect(area: Rect, x: &mut u16, width: u16) -> Rect {
    let right = area.x.saturating_add(area.width);
    let width = width.min(right.saturating_sub(*x));
    let rect = Rect {
        x: *x,
        y: area.y,
        width,
        height: area.height,
    };
    *x = x.saturating_add(width);
    rect
}

fn rect_at_y(rect: Rect, y: u16) -> Rect {
    Rect {
        y,
        height: 1,
        ..rect
    }
}

fn render_cell(frame: &mut ratatui::Frame<'_>, area: Rect, value: &str, style: Style) {
    frame.render_widget(
        Paragraph::new(pad_display(value, area.width as usize)).style(style),
        area,
    );
}

fn render_cell_right(frame: &mut ratatui::Frame<'_>, area: Rect, value: &str, style: Style) {
    frame.render_widget(
        Paragraph::new(pad_display_right(value, area.width as usize)).style(style),
        area,
    );
}

struct PaneListColumns {
    state_width: usize,
    yolo_width: usize,
    agent_width: usize,
    cpu_width: usize,
    mem_width: usize,
    cook_width: usize,
    wait_width: usize,
    yes_width: usize,
}

fn pane_list_columns(app: &DashboardApp) -> PaneListColumns {
    PaneListColumns {
        state_width: 2,
        yolo_width: 2,
        agent_width: 2,
        cpu_width: RESOURCE_GAUGE_WIDTH + 5,
        mem_width: app
            .panes
            .iter()
            .map(|pane| {
                UnicodeWidthStr::width(
                    format_memory_gauge(pane.resource_usage.memory_bytes, app.max_memory_bytes())
                        .as_str(),
                )
            })
            .max()
            .unwrap_or(3)
            .max(3),
        cook_width: app
            .panes
            .iter()
            .map(|pane| {
                UnicodeWidthStr::width(
                    format_duration_compact(pane.displayed_cook_duration()).as_str(),
                )
            })
            .max()
            .unwrap_or(4)
            .max(4),
        wait_width: app
            .panes
            .iter()
            .map(|pane| {
                pane.displayed_wait_duration()
                    .map(format_duration_compact)
                    .unwrap_or_default()
                    .len()
            })
            .max()
            .unwrap_or(4)
            .max(4),
        yes_width: app
            .panes
            .iter()
            .map(|pane| pane.yes_count.to_string().len())
            .max()
            .unwrap_or(1)
            .max(1),
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
        | SessionState::PromptEditing
        | SessionState::UserQuestionPrompt
        | SessionState::PlanApprovalPrompt
        | SessionState::FolderTrustPrompt
        | SessionState::StartupChoicePrompt
        | SessionState::SurveyPrompt
        | SessionState::ExternalEditorActive
        | SessionState::DiffDialog
        | SessionState::AgyCommandPermissionPrompt
        | SessionState::AgyFolderTrustPrompt
        | SessionState::AgySettingsPersistPrompt
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

fn available_body_columns(area: ratatui::layout::Rect) -> usize {
    area.width.saturating_sub(2) as usize
}

fn detail_body_lines(pane: &PaneEntry, max_rows: usize, max_columns: usize) -> Vec<String> {
    if max_rows == 0 {
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

    select_tail_lines_for_rows(&lines, max_rows, max_columns)
}

fn select_tail_lines_for_rows(
    lines: &[String],
    max_rows: usize,
    max_columns: usize,
) -> Vec<String> {
    if max_rows == 0 {
        return Vec::new();
    }
    if max_columns == 0 {
        let start = lines.len().saturating_sub(max_rows);
        return lines[start..].to_vec();
    }

    let mut used_rows = 0usize;
    let mut selected = Vec::new();
    for line in lines.iter().rev() {
        let rows = wrapped_line_rows(line, max_columns);
        if !selected.is_empty() && used_rows.saturating_add(rows) > max_rows {
            break;
        }
        used_rows = used_rows.saturating_add(rows);
        selected.push(line.clone());
        if used_rows >= max_rows {
            break;
        }
    }
    selected.reverse();
    selected
}

fn wrapped_line_rows(line: &str, max_columns: usize) -> usize {
    if max_columns == 0 {
        return 1;
    }
    let width = UnicodeWidthStr::width(line);
    width.div_ceil(max_columns).max(1)
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
    let trimmed = line.trim();
    matches!(trimmed, "❯" | ">" | "›")
        || (trimmed.starts_with("› ")
            && !starts_with_numbered_context_option(trimmed.trim_start_matches('›').trim()))
}

fn starts_with_numbered_context_option(line: &str) -> bool {
    let trimmed = line.trim_start();
    let mut chars = trimmed.chars();
    matches!(chars.next(), Some('0'..='9')) && matches!(chars.next(), Some('.' | ')'))
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

fn cleanup_dashboard_window_name(window: &TmuxWindow) -> String {
    let cleaned = strip_dashboard_emoji_prefixes(&window.window_name);
    if cleaned.is_empty() {
        window.window_name.clone()
    } else {
        cleaned.to_string()
    }
}

fn dashboard_display_state(
    classified_state: SessionState,
    previous_frame: Option<&String>,
    current_frame: &str,
    authoritative_state: Option<SessionState>,
) -> SessionState {
    if let Some(authoritative_state) = authoritative_state {
        authoritative_state
    } else if classified_state == SessionState::ChatReady
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

fn requested_dashboard_pane_selection_path(state_dir: &Path) -> PathBuf {
    state_dir.join("dashboard-requested-pane")
}

pub(crate) fn request_dashboard_pane_selection(
    state_dir: &Path,
    pane_id: Option<&str>,
) -> AppResult<()> {
    let Some(pane_id) = pane_id.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(());
    };
    fs::create_dir_all(state_dir)?;
    fs::write(requested_dashboard_pane_selection_path(state_dir), pane_id)?;
    Ok(())
}

fn take_requested_dashboard_pane_selection(state_dir: &Path) -> AppResult<Option<String>> {
    let path = requested_dashboard_pane_selection_path(state_dir);
    let content = match fs::read_to_string(&path) {
        Ok(content) => content,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let _ = fs::remove_file(path);
    Ok(content
        .lines()
        .next()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string))
}

pub(crate) fn current_dashboard_pane_id(client: &TmuxClient) -> Option<String> {
    std::env::var("TMUX_PANE")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .or_else(|| client.display_message("#{pane_id}").ok())
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
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
        PaneSource::Codex => "Codex",
        PaneSource::OpenCode { .. } => "OpenCode",
        PaneSource::Pi { .. } => "Pi",
        PaneSource::Grok { .. } => "Grok",
        PaneSource::Agy { .. } => "Antigravity",
    }
}

fn agent_emoji(pane: &PaneEntry) -> &'static str {
    match pane.source {
        PaneSource::Claude => "❋",
        PaneSource::Codex => "🞇",
        PaneSource::OpenCode { .. } => "⧈",
        PaneSource::Pi { .. } => "π",
        // BLACK FOUR POINTED STAR (U+2726) — single-width in terminal fonts.
        PaneSource::Grok { .. } => "✦",
        PaneSource::Agy { .. } => "⚛",
    }
}

#[cfg(any(test, rust_analyzer))]
fn agent_marker(pane: &PaneEntry) -> &'static str {
    match pane.source {
        PaneSource::Claude => "C",
        PaneSource::Codex => "X",
        PaneSource::OpenCode { .. } => "O",
        PaneSource::Pi { .. } => "P",
        PaneSource::Grok { .. } => "G",
        PaneSource::Agy { .. } => "A",
    }
}

fn pane_session_id(pane: &PaneEntry) -> Option<&str> {
    match &pane.source {
        PaneSource::Claude => pane.claude_session_id.as_deref(),
        PaneSource::Codex => None,
        PaneSource::OpenCode { session_id, .. } => session_id.as_deref(),
        PaneSource::Pi { session_id } => Some(session_id.as_str()),
        PaneSource::Grok { session_id, .. } => session_id.as_deref(),
        PaneSource::Agy { session_id } => session_id.as_deref(),
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

fn is_cooking_state(state: SessionState) -> bool {
    matches!(state, SessionState::BusyResponding)
}

fn is_waiting_state(state: SessionState) -> bool {
    // `AgyCommandPermissionPrompt` is included because YOLO can auto-approve
    // it (so wait-duration accounting stays meaningful). The other two agy
    // shapes are operator-blocking — they are NOT "still working" states and
    // intentionally remain excluded.
    matches!(
        state,
        SessionState::ChatReady
            | SessionState::PromptEditing
            | SessionState::PermissionDialog
            | SessionState::FolderTrustPrompt
            | SessionState::AgyCommandPermissionPrompt
    )
}

fn runtime_snapshot_duration(
    base_ms: Option<u64>,
    updated_at_unix_ms: i64,
    active: bool,
) -> Option<Duration> {
    let base_ms = base_ms?;
    let elapsed_ms = if active {
        unix_time_ms().saturating_sub(updated_at_unix_ms).max(0) as u64
    } else {
        0
    };
    Some(Duration::from_millis(base_ms.saturating_add(elapsed_ms)))
}

fn unix_time_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}

fn is_attention_state(state: SessionState) -> bool {
    // All three agy permission shapes warrant operator attention: only the
    // command-permission shape may auto-approve, and even then the operator
    // benefits from the dashboard cursor landing on the pane while waiting.
    matches!(
        state,
        SessionState::PermissionDialog
            | SessionState::PromptEditing
            | SessionState::UserQuestionPrompt
            | SessionState::PlanApprovalPrompt
            | SessionState::FolderTrustPrompt
            | SessionState::StartupChoicePrompt
            | SessionState::SurveyPrompt
            | SessionState::ExternalEditorActive
            | SessionState::DiffDialog
            | SessionState::AgyCommandPermissionPrompt
            | SessionState::AgyFolderTrustPrompt
            | SessionState::AgySettingsPersistPrompt
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
    if enabled { "🤠" } else { " " }
}

#[cfg(any(test, rust_analyzer))]
fn state_marker(state: SessionState, has_questions: bool) -> &'static str {
    // Distinct markers for the three agy shapes so an operator scanning the
    // dashboard can distinguish "will auto-approve" (lowercase `p`) from
    // "requires manual review" (`f`, `s`). Lowercase `p` preserves the 1:1
    // mapping invariant (no marker shared between two states).
    match state {
        SessionState::BusyResponding => "B",
        SessionState::PromptEditing => "E",
        SessionState::UserQuestionPrompt => "Q",
        SessionState::ChatReady if has_questions => "Q",
        SessionState::ChatReady => "I",
        SessionState::PermissionDialog => "P",
        SessionState::PlanApprovalPrompt => "A",
        SessionState::FolderTrustPrompt => "F",
        SessionState::StartupChoicePrompt => "C",
        SessionState::SurveyPrompt => "S",
        SessionState::ExternalEditorActive => "V",
        SessionState::DiffDialog => "D",
        SessionState::AgyCommandPermissionPrompt => "p",
        SessionState::AgyFolderTrustPrompt => "f",
        SessionState::AgySettingsPersistPrompt => "s",
        SessionState::Unknown => "?",
    }
}

fn state_emoji(state: SessionState, has_questions: bool) -> &'static str {
    // Agy emojis: command-permission shares the lock (`🔐`) with Claude
    // PermissionDialog — the marker disambiguates. Folder-trust uses an open
    // folder (`📂`) to distinguish from FolderTrustPrompt's closed folder
    // (`📁`). Settings-persist uses the gear (`⚙️`) since it writes to
    // `settings.json`.
    match state {
        SessionState::BusyResponding => "⚙️",
        SessionState::PromptEditing => "💬",
        SessionState::UserQuestionPrompt => "🤔",
        SessionState::ChatReady if has_questions => "🤔",
        SessionState::ChatReady => "💤",
        SessionState::PermissionDialog => "🔐",
        SessionState::PlanApprovalPrompt => "❓",
        SessionState::FolderTrustPrompt => "📁",
        SessionState::StartupChoicePrompt => "🔀",
        SessionState::SurveyPrompt => "📝",
        SessionState::ExternalEditorActive => "✏️",
        SessionState::DiffDialog => "🧾",
        SessionState::AgyCommandPermissionPrompt => "🔐",
        SessionState::AgyFolderTrustPrompt => "📂",
        SessionState::AgySettingsPersistPrompt => "⚙️",
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

fn format_duration_compact(duration: Duration) -> String {
    let total_minutes = duration.as_secs() / 60;
    let days = total_minutes / (24 * 60);
    let hours = (total_minutes % (24 * 60)) / 60;
    let minutes = total_minutes % 60;

    if days > 0 {
        format!("{days}d{hours}h")
    } else if hours > 0 {
        format!("{hours}h{minutes:02}m")
    } else {
        format!("{minutes}m")
    }
}

fn format_pid(pid: Option<u32>) -> String {
    pid.map(|pid| pid.to_string())
        .unwrap_or_else(|| String::from("-"))
}

fn format_cpu_percent(cpu_percent: Option<f64>) -> String {
    cpu_percent
        .map(|value| format!("{value:.0}%"))
        .unwrap_or_else(|| String::from("-"))
}

fn format_memory_bytes(memory_bytes: Option<u64>) -> String {
    memory_bytes
        .map(format_bytes)
        .unwrap_or_else(|| String::from("-"))
}

fn format_cpu_gauge(cpu_percent: Option<f64>) -> String {
    let label = format_cpu_percent(cpu_percent);
    let ratio = cpu_percent.map(|value| (value / 100.0).clamp(0.0, 1.0));
    format_gauge(ratio, &label, RESOURCE_GAUGE_WIDTH)
}

fn format_memory_gauge(memory_bytes: Option<u64>, max_memory_bytes: Option<u64>) -> String {
    let label = format_memory_bytes(memory_bytes);
    let bar = memory_bytes
        .map(format_memory_bar)
        .unwrap_or_else(|| String::from(" "));
    let width = max_memory_bytes
        .map(format_memory_bar)
        .map(|bar| UnicodeWidthStr::width(bar.as_str()))
        .unwrap_or(1)
        .max(1);
    format!("{} {label}", pad_display(&bar, width))
}

fn format_gauge(ratio: Option<f64>, label: &str, width: usize) -> String {
    let Some(ratio) = ratio else {
        return format!("{} {label}", " ".repeat(width));
    };

    format!("{} {label}", resource_gauge_char(ratio))
}

fn format_memory_bar(memory_bytes: u64) -> String {
    let full = (memory_bytes / GIB_BYTES) as usize;
    let remainder = memory_bytes % GIB_BYTES;
    let mut bar = "█".repeat(full);
    if remainder > 0 || bar.is_empty() {
        bar.push(resource_gauge_char(remainder as f64 / GIB_BYTES as f64));
    }
    bar
}

fn resource_gauge_char(ratio: f64) -> char {
    let ratio = ratio.clamp(0.0, 1.0);
    if ratio >= 1.0 {
        return '█';
    }
    let units = (ratio * RESOURCE_GAUGE_LEVELS.len() as f64).round() as usize;
    RESOURCE_GAUGE_LEVELS[units.saturating_sub(1)]
}

fn format_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    const GIB: f64 = GIB_BYTES as f64;

    let bytes = bytes as f64;
    if bytes >= GIB {
        format!("{:.1}G", bytes / GIB)
    } else if bytes >= MIB {
        format!("{:.0}M", bytes / MIB)
    } else if bytes >= KIB {
        format!("{:.0}K", bytes / KIB)
    } else {
        format!("{bytes:.0}B")
    }
}

#[cfg(any(test, rust_analyzer))]
fn select_agent_process<'a>(
    root_pid: u32,
    current_command: &str,
    snapshots: &'a [ProcessSnapshot],
) -> Option<&'a ProcessSnapshot> {
    let tree = ProcessTreeSnapshot::from_process_snapshots(snapshots);
    let selected_pid = tree.select_agent_process(root_pid, current_command)?.pid;
    snapshots
        .iter()
        .find(|snapshot| snapshot.pid == selected_pid)
}

fn oldest_matching_process<'a, I>(snapshots: I) -> Option<&'a ProcessSnapshot>
where
    I: Iterator<Item = &'a ProcessSnapshot>,
{
    snapshots.min_by_key(|snapshot| snapshot.start_ticks)
}

fn process_matches_command(snapshot: &ProcessSnapshot, command: &str) -> bool {
    let command = command_basename(command);
    if command.is_empty() {
        return false;
    }
    snapshot.command == command
        || snapshot
            .cmdline_basenames
            .iter()
            .any(|name| name == command)
}

fn command_basename(command: &str) -> &str {
    command.rsplit('/').next().unwrap_or(command)
}

fn cpu_percent_between(
    previous: CpuSample,
    current: CpuSample,
    ticks_per_second: u64,
) -> Option<f64> {
    if ticks_per_second == 0 || previous.process_start_ticks != current.process_start_ticks {
        return None;
    }
    let elapsed = current
        .at
        .checked_duration_since(previous.at)?
        .as_secs_f64();
    if elapsed <= 0.0 || current.total_ticks < previous.total_ticks {
        return None;
    }

    let ticks_per_second = ticks_per_second as f64;
    let cpu_seconds = (current.total_ticks - previous.total_ticks) as f64 / ticks_per_second;
    Some((cpu_seconds / elapsed) * 100.0)
}

fn process_snapshots_with_cmdlines() -> Vec<ProcessSnapshot> {
    process_snapshots_with_options(true)
}

fn process_snapshots_with_options(include_cmdline: bool) -> Vec<ProcessSnapshot> {
    let Ok(entries) = fs::read_dir("/proc") else {
        return Vec::new();
    };

    entries
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().to_string_lossy().parse::<u32>().ok())
        .filter_map(|pid| process_snapshot(pid, include_cmdline))
        .collect()
}

fn process_snapshot(pid: u32, include_cmdline: bool) -> Option<ProcessSnapshot> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let mut snapshot = parse_process_stat(pid, &stat)?;
    if include_cmdline {
        snapshot.cmdline_basenames = process_cmdline_basenames(pid);
    }
    Some(snapshot)
}

fn parse_process_stat(pid: u32, stat: &str) -> Option<ProcessSnapshot> {
    let (prefix, fields) = stat.rsplit_once(") ")?;
    let command = prefix.split_once('(')?.1.to_string();
    let fields = fields.split_whitespace().collect::<Vec<_>>();
    Some(ProcessSnapshot {
        pid,
        ppid: fields.get(1)?.parse::<u32>().ok()?,
        pgrp: fields.get(2)?.parse::<i32>().ok()?,
        tpgid: fields.get(5)?.parse::<i32>().ok()?,
        command,
        cmdline_basenames: Vec::new(),
        start_ticks: fields.get(19)?.parse::<u64>().ok()?,
        cpu_ticks: fields
            .get(11)?
            .parse::<u64>()
            .ok()?
            .saturating_add(fields.get(12)?.parse::<u64>().ok()?),
        rss_pages: fields
            .get(21)?
            .parse::<i64>()
            .ok()
            .unwrap_or_default()
            .max(0) as u64,
    })
}

fn process_cmdline_basenames(pid: u32) -> Vec<String> {
    let Ok(cmdline) = fs::read(format!("/proc/{pid}/cmdline")) else {
        return Vec::new();
    };
    cmdline
        .split(|byte| *byte == 0)
        .filter_map(|part| std::str::from_utf8(part).ok())
        .filter(|part| !part.is_empty())
        .map(command_basename)
        .filter(|part| !part.is_empty())
        .map(str::to_string)
        .collect()
}

fn ticks_per_second() -> Option<u64> {
    let ticks_per_second = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    (ticks_per_second > 0).then_some(ticks_per_second as u64)
}

fn process_uptime_secs() -> Option<f64> {
    fs::read_to_string("/proc/uptime")
        .ok()?
        .split_whitespace()
        .next()?
        .parse::<f64>()
        .ok()
}

fn page_size() -> Option<u64> {
    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    (page_size > 0).then_some(page_size as u64)
}

fn is_claude_pane(pane: &TmuxPane) -> bool {
    pane.current_command.eq_ignore_ascii_case("claude")
}

fn is_codex_candidate_pane(pane: &TmuxPane) -> bool {
    pane.current_command.eq_ignore_ascii_case("codex")
        || (pane.current_command.eq_ignore_ascii_case("node")
            && !pane.pane_title.starts_with("OC | "))
}

fn is_opencode_candidate_pane(pane: &TmuxPane) -> bool {
    pane.current_command.eq_ignore_ascii_case("opencode") || pane.pane_title.starts_with("OC | ")
}

fn opencode_source_for_pane(pane: &TmuxPane) -> Option<PaneSource> {
    if let Some(opencode_session) = resolve_opencode_session_for_pane(pane) {
        return Some(PaneSource::OpenCode {
            session_id: Some(opencode_session.id),
            title: opencode_session.title,
        });
    }

    if is_opencode_candidate_pane(pane) {
        return Some(PaneSource::OpenCode {
            session_id: None,
            title: pane_opencode_title(pane)
                .unwrap_or("unresolved")
                .to_string(),
        });
    }

    None
}

fn is_codex_screen(frame: &str, signals: &[String]) -> bool {
    if signals.iter().any(|signal| signal == SIGNAL_CODEX_KEYWORDS) {
        return true;
    }

    let normalized = frame.to_ascii_lowercase();
    normalized.contains("openai codex")
        || normalized.contains(">_ openai codex")
        || normalized.contains("pursuing goal")
        || normalized.contains("tab to queue message")
        || (normalized.contains("yes, proceed")
            && normalized.contains("press enter to confirm or esc to cancel"))
        || normalized.contains("suppress_unstable_features_warning")
        || normalized.lines().any(is_codex_statusline_line)
        || normalized.lines().any(is_codex_footer_line)
}

fn is_dashboard_fallback_candidate(pane: &TmuxPane) -> bool {
    is_codex_candidate_pane(pane)
        || is_opencode_candidate_pane(pane)
        || pane.current_command.eq_ignore_ascii_case("pi")
        || pane.current_command.eq_ignore_ascii_case("agy")
        || pane.current_command.eq_ignore_ascii_case("grok")
}

fn is_dashboard_visible_non_runtime_pane_with(
    pane: &TmuxPane,
    frame: &str,
    signals: &[String],
    agy_session: Option<&crate::agy::AgySession>,
    resolver: &dyn ChildResolver,
) -> AppResult<bool> {
    // Mirror the short-circuit in `pane_source_for_non_runtime_pane_with`:
    // when the pane's current command is `agy`/`grok` we treat it as a visible
    // pane even if session resolution returned nothing. This keeps
    // `PaneSource::Agy { session_id: None }` / `Grok { session_id: None }`
    // reachable on the non-runtime path; otherwise unresolved panes would
    // silently disappear from the dashboard.
    Ok(is_agy_pane(pane)
        || is_grok_pane(pane)
        || is_opencode_candidate_pane(pane)
        || is_codex_screen(frame, signals)
        || resolve_pi_session_for_pane(pane, resolver)?.is_some()
        || resolve_opencode_session_for_pane(pane).is_some()
        || agy_session.is_some())
}

fn pane_source_for_non_runtime_pane_with(
    pane: &TmuxPane,
    frame: &str,
    signals: &[String],
    agy_session: Option<crate::agy::AgySession>,
    resolver: &dyn ChildResolver,
) -> AppResult<PaneSource> {
    if is_agy_pane(pane) {
        // Short-circuit on the process name. Even if `agy_session` is None
        // (secondary signal not yet captured), the pane must not fall through
        // to `PaneSource::Codex` — that would let YOLO toggles target an agy
        // pane (AGENTS.md guardrail).
        return Ok(PaneSource::Agy {
            session_id: agy_session.and_then(|session| session.id),
        });
    }
    if is_grok_pane(pane) {
        let session = resolve_grok_session_for_pane(pane, frame, resolver)?;
        return Ok(PaneSource::Grok {
            session_id: session
                .as_ref()
                .map(|session| session.id.clone())
                .filter(|id| !id.is_empty()),
            title: session.and_then(|session| session.title),
        });
    }
    if let Some(source) = opencode_source_for_pane(pane) {
        return Ok(source);
    }
    if let Some(pi_session) = resolve_pi_session_for_pane(pane, resolver)? {
        return Ok(PaneSource::Pi {
            session_id: pi_session.id,
        });
    }
    if is_codex_screen(frame, signals) {
        return Ok(PaneSource::Codex);
    }
    Ok(PaneSource::Codex)
}

fn focused_dashboard_frame(frame: &str) -> String {
    let lines = frame.lines().collect::<Vec<_>>();
    let start = lines
        .iter()
        .rposition(|line| {
            line.contains("╭") || line.contains("┌") || line.contains(">_") || line.contains("❯")
        })
        .unwrap_or(0);
    // Grok renders the busy status line (spinner / "command still running")
    // immediately above the prompt box. Pull a few preceding lines so that
    // chrome remains visible without re-including the whole scrollback.
    let start = if crate::grok::frame_has_grok_fingerprint(frame) {
        start.saturating_sub(8)
    } else {
        start
    };
    lines[start..].join("\n")
}

fn is_codex_statusline_line(line: &str) -> bool {
    line.contains("gpt-")
        && line.contains("·")
        && line.contains("context")
        && !line.contains("claude")
}

fn is_codex_footer_line(line: &str) -> bool {
    line.contains("gpt-") && line.contains("·") && !line.contains("claude")
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
    use std::collections::{HashMap, HashSet};
    use std::fs;
    use std::path::PathBuf;
    use std::sync::{Mutex, mpsc};
    use std::time::Duration;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::{
        DASHBOARD_LEFT_PANE_MAX_WIDTH, DashboardApp, DashboardVisibility, DetailBodyKind, ICON_GAP,
        ManagedWindowName, PERSISTENT_DASHBOARD_SESSION, PERSISTENT_DASHBOARD_SOCKET, PaneEntry,
        PaneSource, ProcessSnapshot, ProcessTreeSnapshot, ReconnectBackoff, ResourceUsage,
        RuntimeCacheState, RuntimeSnapshotCache, SavedSelection, TitlePaneState, WindowTitleIo,
        WindowTitleWorker, abbreviate_home_path, advance_runtime_revision, agent_emoji,
        agent_marker, apply_runtime_event, cleanup_dashboard_window_name,
        context_lines_above_input, current_base_window_name, dashboard_body_sections,
        dashboard_display_state, dashboard_loop_effects, dashboard_selection_path,
        derive_base_window_name, detail_body_kind, detail_current_path, detail_workspace_label,
        details_panel_height, enqueue_detached_fallback, fallback_process_tree_with,
        footer_column_widths, footer_help_line, footer_lines, format_age, format_cpu_gauge,
        format_duration_compact, format_memory_bar, format_memory_gauge, grouped_title_states,
        is_attention_state, is_codex_candidate_pane, is_codex_screen, is_cooking_state,
        is_opencode_candidate_pane, is_waiting_state, load_saved_selection, pad_display_right,
        pane_list_columns, pane_list_content_area, pane_source_for_non_runtime_pane_with,
        pane_window_prefix, parse_process_stat, poll_runtime_cache_once,
        prune_missing_stale_managed_windows, recap_lines, rect_contains,
        render_workspace_header_line, replace_runtime_cache, replace_runtime_poll_cache,
        repo_root_from_repo_key, runtime_snapshot_duration, save_selection, select_agent_process,
        select_tail_lines_for_rows, set_runtime_subscription_capability, should_return_navigation,
        split_workspace_header, state_emoji, state_marker, strip_dashboard_emoji_prefixes,
        subscription_connection_is_stable, tmux_object_id_order, visibility_from_attached_count,
        workspace_group_key, workspace_group_label, yes_count_key, yolo_column_contains,
    };
    use crate::classifier::{Classification, SIGNAL_CODEX_KEYWORDS, SessionState};
    use crate::runtime::{RuntimeClient, RuntimeEvent, RuntimePaneSnapshot};
    use crate::storage::WorkspaceRecord;
    use crate::tmux::{TmuxClient, TmuxPane, TmuxWindow};
    use ratatui::layout::Rect;
    use ratatui::style::Modifier;
    use unicode_width::UnicodeWidthStr;

    struct BlockingWindowTitleIo {
        entered: mpsc::SyncSender<()>,
        release: Option<mpsc::Receiver<()>>,
    }

    impl WindowTitleIo for BlockingWindowTitleIo {
        fn apply(
            &mut self,
            _desired: HashMap<String, TitlePaneState>,
            _topology_complete: bool,
        ) -> crate::app::AppResult<()> {
            let _ = self.entered.try_send(());
            if let Some(release) = self.release.take() {
                let _ = release.recv();
            }
            Ok(())
        }

        fn retry_pending(&self) -> bool {
            false
        }

        fn restore(&mut self) -> crate::app::AppResult<()> {
            Ok(())
        }
    }

    #[test]
    fn formats_short_age() {
        assert_eq!(format_age(Duration::from_secs(65)), "01:05");
    }

    #[test]
    fn detects_codex_candidate_node_panes_without_guessing_opencode_titles() {
        assert!(is_codex_candidate_pane(&sample_tmux_pane(
            "node",
            "Action Required | demo"
        )));
        assert!(!is_codex_candidate_pane(&sample_tmux_pane(
            "node",
            "OC | demo"
        )));
    }

    #[test]
    fn codex_screen_detection_uses_screen_anchors_or_signals() {
        assert!(is_codex_screen("│ >_ OpenAI Codex (v0.128.0) │", &[]));
        assert!(is_codex_screen("› hello\n  tab to queue message", &[]));
        assert!(is_codex_screen(
            "gpt-5.5 high · ~/Projects/botctl · gpt-5.5 · main · Ready · Context 100% left · Context 0% used",
            &[]
        ));
        assert!(is_codex_screen(
            "plain",
            &[String::from(SIGNAL_CODEX_KEYWORDS)]
        ));
        assert!(is_codex_screen(
            "› 1. Yes, proceed (y)\nPress enter to confirm or esc to cancel",
            &[]
        ));
        assert!(!is_codex_screen("plain node output", &[]));
    }

    #[test]
    fn dashboard_keeps_opencode_source_ahead_of_codex_screen_signals() {
        let pane = sample_tmux_pane("opencode", "OC | Remove glob dependency from Cypress tests");
        let source = pane_source_for_non_runtime_pane_with(
            &pane,
            "Plan · GPT-5.5 OpenAI",
            &[String::from(SIGNAL_CODEX_KEYWORDS)],
            None,
            &crate::proc_fd::LiveProc,
        )
        .expect("source should resolve");

        assert!(is_opencode_candidate_pane(&pane));
        match source {
            PaneSource::OpenCode { session_id, title } => {
                assert_eq!(session_id, None);
                assert_eq!(title, "Remove glob dependency from Cypress tests");
            }
            _ => panic!("expected OpenCode source"),
        }
    }

    #[test]
    fn formats_long_age() {
        assert_eq!(format_age(Duration::from_secs(3665)), "01:01:05");
    }

    #[test]
    fn formats_compact_dashboard_durations_without_seconds() {
        assert_eq!(format_duration_compact(Duration::from_secs(45)), "0m");
        assert_eq!(format_duration_compact(Duration::from_secs(65)), "1m");
        assert_eq!(
            format_duration_compact(Duration::from_secs(5 * 3600 + 33 * 60 + 12)),
            "5h33m"
        );
        assert_eq!(
            format_duration_compact(Duration::from_secs(2 * 86400 + 3 * 3600)),
            "2d3h"
        );
    }

    #[test]
    fn runtime_duration_advances_only_while_authoritative_state_is_active() {
        let now = super::unix_time_ms();
        let active = runtime_snapshot_duration(Some(2_000), now - 1_000, true)
            .expect("active duration should exist");
        assert!(active >= Duration::from_millis(3_000));
        assert!(active < Duration::from_millis(3_250));
        assert_eq!(
            runtime_snapshot_duration(Some(2_000), now - 10_000, false),
            Some(Duration::from_millis(2_000))
        );
        assert_eq!(runtime_snapshot_duration(None, now, true), None);
    }

    #[test]
    fn visibility_probe_fails_open_and_zero_detaches() {
        assert_eq!(
            visibility_from_attached_count(Err(())),
            DashboardVisibility::Visible
        );
        assert_eq!(
            visibility_from_attached_count(Ok(1)),
            DashboardVisibility::Visible
        );
        assert_eq!(
            visibility_from_attached_count(Ok(0)),
            DashboardVisibility::Detached
        );
    }

    #[test]
    fn detached_loop_suppresses_draw_and_full_enrichment() {
        let effects = dashboard_loop_effects(DashboardVisibility::Detached, true, true, true);
        assert!(!effects.draw);
        assert!(!effects.full_refresh);
        assert!(effects.detached_fallback);
    }

    #[test]
    fn runtime_cache_replacement_removes_stale_panes_and_events_upsert_or_remove() {
        let cache = Mutex::new(RuntimeCacheState::default());
        replace_runtime_cache(
            &cache,
            vec![
                sample_runtime_snapshot("%1", 1),
                sample_runtime_snapshot("%2", 2),
            ],
        );
        replace_runtime_cache(&cache, vec![sample_runtime_snapshot("%1", 3)]);
        {
            let cache = cache.lock().unwrap();
            assert_eq!(cache.snapshots.len(), 1);
            assert!(!cache.snapshots.contains_key("%2"));
        }

        assert!(apply_runtime_event(
            &cache,
            RuntimeEvent {
                revision: 4,
                timestamp_unix_ms: 0,
                kind: String::from("pane-snapshot-updated"),
                pane_id: Some(String::from("%2")),
                workspace_id: None,
                summary: String::new(),
                snapshot: Some(sample_runtime_snapshot("%2", 4)),
            },
        ));
        assert!(apply_runtime_event(
            &cache,
            RuntimeEvent {
                revision: 5,
                timestamp_unix_ms: 0,
                kind: String::from("pane-removed"),
                pane_id: Some(String::from("%1")),
                workspace_id: None,
                summary: String::new(),
                snapshot: None,
            },
        ));
        let cache = cache.lock().unwrap();
        assert!(!cache.snapshots.contains_key("%1"));
        assert!(cache.snapshots.contains_key("%2"));
    }

    #[test]
    fn runtime_cache_ignores_duplicate_and_older_revisions() {
        let mut revision = 10;
        assert!(!advance_runtime_revision(&mut revision, 9));
        assert!(!advance_runtime_revision(&mut revision, 10));
        assert!(advance_runtime_revision(&mut revision, 11));
        assert_eq!(revision, 11);
    }

    #[test]
    fn boundary_capable_runtime_cache_never_uses_polling_fallback() {
        let cache = RuntimeSnapshotCache::new();
        replace_runtime_cache(&cache.shared, vec![sample_runtime_snapshot("%1", 1)]);
        set_runtime_subscription_capability(&cache.shared, true);
        let missing_runtime = RuntimeClient::with_socket_path(
            unique_temp_dir("dashboard-missing-runtime").join("runtime.sock"),
        );

        assert!(!poll_runtime_cache_once(&missing_runtime, &cache.shared).unwrap());
        assert_eq!(cache.snapshots()[0].pane.pane_id, "%1");
    }

    #[test]
    fn failed_legacy_runtime_poll_retains_last_good_cache() {
        let cache = RuntimeSnapshotCache::new();
        replace_runtime_cache(&cache.shared, vec![sample_runtime_snapshot("%1", 1)]);
        let missing_runtime = RuntimeClient::with_socket_path(
            unique_temp_dir("dashboard-missing-legacy-runtime").join("runtime.sock"),
        );

        assert!(poll_runtime_cache_once(&missing_runtime, &cache.shared).is_err());
        assert_eq!(cache.snapshots()[0].pane.pane_id, "%1");
    }

    #[test]
    fn subscription_capability_tracks_each_connection_and_runtime_transition() {
        let cache = Mutex::new(RuntimeCacheState::default());
        replace_runtime_cache(&cache, vec![sample_runtime_snapshot("%1", 1)]);
        set_runtime_subscription_capability(&cache, true);

        set_runtime_subscription_capability(&cache, false);
        assert_eq!(cache.lock().unwrap().snapshots.len(), 1);
        assert!(replace_runtime_poll_cache(
            &cache,
            vec![sample_runtime_snapshot("%2", 2)]
        ));
        assert!(cache.lock().unwrap().snapshots.contains_key("%2"));

        replace_runtime_cache(&cache, vec![sample_runtime_snapshot("%3", 3)]);
        set_runtime_subscription_capability(&cache, true);
        assert!(!replace_runtime_poll_cache(
            &cache,
            vec![sample_runtime_snapshot("%4", 4)]
        ));
        let cache = cache.lock().unwrap();
        assert!(cache.snapshots.contains_key("%3"));
        assert!(!cache.snapshots.contains_key("%4"));
    }

    #[test]
    fn delayed_bootstrap_uses_normal_backoff_without_losing_last_good_cache() {
        let cache = Mutex::new(RuntimeCacheState::default());
        replace_runtime_cache(&cache, vec![sample_runtime_snapshot("%1", 1)]);
        set_runtime_subscription_capability(&cache, false);
        let mut backoff = ReconnectBackoff::default();

        assert_eq!(backoff.failure_delay(false), Duration::from_millis(250));
        assert!(cache.lock().unwrap().snapshots.contains_key("%1"));

        replace_runtime_cache(&cache, vec![sample_runtime_snapshot("%2", 2)]);
        set_runtime_subscription_capability(&cache, true);
        let cache = cache.lock().unwrap();
        assert!(cache.subscription_boundary_supported);
        assert!(cache.snapshots.contains_key("%2"));
    }

    #[test]
    fn immediate_post_bootstrap_disconnects_back_off_until_connection_is_stable() {
        let mut backoff = ReconnectBackoff::default();
        let delays = (0..6)
            .map(|_| backoff.failure_delay(false))
            .collect::<Vec<_>>();
        assert_eq!(
            delays,
            vec![
                Duration::from_millis(250),
                Duration::from_millis(500),
                Duration::from_secs(1),
                Duration::from_secs(2),
                Duration::from_secs(5),
                Duration::from_secs(5),
            ]
        );
        assert_eq!(backoff.failure_delay(true), Duration::from_millis(250));
        assert_eq!(backoff.failure_delay(false), Duration::from_millis(500));
        assert!(!subscription_connection_is_stable(
            false,
            Duration::from_secs(1)
        ));
        assert!(subscription_connection_is_stable(
            true,
            Duration::from_millis(1)
        ));
        assert!(subscription_connection_is_stable(
            false,
            Duration::from_secs(5)
        ));
    }

    #[test]
    fn detached_fallback_request_is_non_blocking_while_work_is_pending() {
        let (request_tx, _request_rx) = mpsc::sync_channel(1);
        assert!(enqueue_detached_fallback(&request_tx));
        assert!(!enqueue_detached_fallback(&request_tx));
        assert_eq!(
            visibility_from_attached_count(Ok(1)),
            DashboardVisibility::Visible
        );
    }

    #[test]
    fn blocked_detached_title_io_does_not_block_visibility_result_handling() {
        let (entered_tx, entered_rx) = mpsc::sync_channel(1);
        let (release_tx, release_rx) = mpsc::sync_channel(1);
        let worker = WindowTitleWorker::start_with(BlockingWindowTitleIo {
            entered: entered_tx,
            release: Some(release_rx),
        });
        worker.publish(Vec::new(), true);
        entered_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("title worker should enter blocking I/O");

        // This is the exact operation used to apply a detached fallback result.
        // It only replaces coalesced desired state and cannot wait for title I/O.
        worker.publish(Vec::new(), true);
        assert_eq!(
            visibility_from_attached_count(Ok(1)),
            DashboardVisibility::Visible
        );

        release_tx.send(()).unwrap();
    }

    #[test]
    fn maps_states_to_emojis() {
        assert_eq!(state_emoji(SessionState::PermissionDialog, false), "🔐");
        assert_eq!(state_emoji(SessionState::PlanApprovalPrompt, false), "❓");
        assert_eq!(state_emoji(SessionState::PromptEditing, false), "💬");
        assert_eq!(state_emoji(SessionState::ChatReady, true), "🤔");
        // Per-shape agy emojis (folder-trust uses the open folder so it does
        // not collide with FolderTrustPrompt; settings-persist uses the gear
        // to communicate "writes settings.json").
        assert_eq!(
            state_emoji(SessionState::AgyCommandPermissionPrompt, false),
            "🔐"
        );
        assert_eq!(state_emoji(SessionState::AgyFolderTrustPrompt, false), "📂");
        assert_eq!(state_emoji(SessionState::FolderTrustPrompt, false), "📁");
        assert_eq!(
            state_emoji(SessionState::AgySettingsPersistPrompt, false),
            "⚙️"
        );
    }

    #[test]
    fn cooking_state_is_only_busy_responding() {
        for state in [
            SessionState::ChatReady,
            SessionState::PromptEditing,
            SessionState::UserQuestionPrompt,
            SessionState::BusyResponding,
            SessionState::PermissionDialog,
            SessionState::PlanApprovalPrompt,
            SessionState::FolderTrustPrompt,
            SessionState::SurveyPrompt,
            SessionState::ExternalEditorActive,
            SessionState::DiffDialog,
            SessionState::Unknown,
        ] {
            assert_eq!(
                is_cooking_state(state),
                state == SessionState::BusyResponding
            );
        }
    }

    #[test]
    fn maps_sources_to_agent_emojis() {
        let claude = sample_pane_entry(SessionState::ChatReady, false, false, "");
        let codex = PaneEntry {
            source: PaneSource::Codex,
            ..sample_pane_entry(SessionState::ChatReady, false, false, "")
        };
        let opencode = PaneEntry {
            source: PaneSource::OpenCode {
                session_id: Some(String::from("ses_1")),
                title: String::from("demo"),
            },
            ..sample_pane_entry(SessionState::ChatReady, false, false, "")
        };
        let pi = PaneEntry {
            source: PaneSource::Pi {
                session_id: String::from("session-one"),
            },
            ..sample_pane_entry(SessionState::ChatReady, false, false, "")
        };
        let grok = PaneEntry {
            source: PaneSource::Grok {
                session_id: Some(String::from("019f67b2-f34f-7b61-9d9f-2eb5554ace8a")),
                title: Some(String::from("Add Grok Support")),
            },
            ..sample_pane_entry(SessionState::ChatReady, false, false, "")
        };
        let agy = PaneEntry {
            source: PaneSource::Agy {
                session_id: Some(String::from("agy-uuid")),
            },
            ..sample_pane_entry(SessionState::ChatReady, false, false, "")
        };

        assert_eq!(agent_emoji(&claude), "❋");
        assert_eq!(agent_emoji(&opencode), "⧈");
        assert_eq!(agent_emoji(&codex), "🞇");
        assert_eq!(agent_emoji(&pi), "π");
        assert_eq!(agent_emoji(&grok), "✦");
        assert_eq!(agent_emoji(&agy), "⚛");
        assert_eq!(agent_marker(&claude), "C");
        assert_eq!(agent_marker(&opencode), "O");
        assert_eq!(agent_marker(&codex), "X");
        assert_eq!(agent_marker(&pi), "P");
        assert_eq!(agent_marker(&grok), "G");
        assert_eq!(agent_marker(&agy), "A");
    }

    #[test]
    fn grok_glyph_is_single_width() {
        use unicode_width::UnicodeWidthChar;
        let glyph: char = "✦".chars().next().unwrap();
        assert_eq!(glyph.width(), Some(1), "grok glyph must be single-width");
    }

    #[test]
    fn agy_glyph_is_single_width() {
        use unicode_width::UnicodeWidthChar;
        let glyph: char = "⚛".chars().next().unwrap();
        assert_eq!(glyph.width(), Some(1), "agy glyph must be single-width");
    }

    #[test]
    fn agy_pane_without_fingerprint_does_not_support_yolo() {
        // Regression for V1: a pane whose process is `agy` must never be
        // classified as `PaneSource::Claude` (which would enable YOLO toggles
        // and Claude keybindings against an agy TUI). Even when the secondary
        // signal hasn't been observed yet (frame has no Antigravity
        // fingerprint and the state dir doesn't exist), the source must
        // remain `PaneSource::Agy { session_id: None }`.
        let pane = PaneEntry {
            pane: TmuxPane {
                current_command: String::from("agy"),
                pane_pid: None,
                ..sample_pane_entry(SessionState::ChatReady, false, false, "").pane
            },
            source: PaneSource::Agy { session_id: None },
            ..sample_pane_entry(SessionState::ChatReady, false, false, "")
        };
        assert!(
            !pane.supports_yolo(),
            "agy panes must never support YOLO (AGENTS.md guardrail)"
        );
        assert!(matches!(pane.source, PaneSource::Agy { session_id: None }));

        // Also verify the non-runtime source resolver short-circuits to
        // PaneSource::Agy even when agy_session resolution returns None
        // (no fingerprint, no conversation resolvable).
        let raw_pane = TmuxPane {
            current_command: String::from("agy"),
            pane_pid: None,
            ..sample_pane_entry(SessionState::ChatReady, false, false, "").pane
        };
        let source = super::pane_source_for_non_runtime_pane_with(
            &raw_pane,
            "frame with no fingerprint",
            &[],
            None,
            &crate::proc_fd::LiveProc,
        )
        .expect("non-runtime source should resolve without error");
        assert!(matches!(source, PaneSource::Agy { session_id: None }));
    }

    #[test]
    fn table_state_markers_are_single_width() {
        assert_eq!(state_marker(SessionState::BusyResponding, false), "B");
        assert_eq!(state_marker(SessionState::ChatReady, true), "Q");
        assert_eq!(state_marker(SessionState::ChatReady, false), "I");
        assert_eq!(state_marker(SessionState::PermissionDialog, false), "P");
        // Lowercase distinct markers for the three agy shapes — preserves the
        // 1:1 invariant so an operator can map glyph → state by eye.
        assert_eq!(
            state_marker(SessionState::AgyCommandPermissionPrompt, false),
            "p"
        );
        assert_eq!(state_marker(SessionState::AgyFolderTrustPrompt, false), "f");
        assert_eq!(
            state_marker(SessionState::AgySettingsPersistPrompt, false),
            "s"
        );
    }

    #[test]
    fn is_attention_state_includes_all_agy_shapes() {
        assert!(is_attention_state(SessionState::AgyCommandPermissionPrompt));
        assert!(is_attention_state(SessionState::AgyFolderTrustPrompt));
        assert!(is_attention_state(SessionState::AgySettingsPersistPrompt));
    }

    #[test]
    fn is_waiting_state_only_includes_agy_command_permission() {
        // Only the auto-approvable shape counts as "waiting" for telemetry
        // purposes; the other two are operator-blocking.
        assert!(is_waiting_state(SessionState::AgyCommandPermissionPrompt));
        assert!(!is_waiting_state(SessionState::AgyFolderTrustPrompt));
        assert!(!is_waiting_state(SessionState::AgySettingsPersistPrompt));
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
    fn context_tail_selection_accounts_for_wrapped_rows() {
        let lines = vec![
            String::from("old"),
            String::from("medium-width"),
            String::from("new"),
        ];
        assert_eq!(
            select_tail_lines_for_rows(&lines, 3, 6),
            vec![String::from("medium-width"), String::from("new")]
        );
    }

    #[test]
    fn context_tail_selection_keeps_one_oversized_recent_line() {
        let lines = vec![String::from("old"), String::from("very-long-recent-line")];
        assert_eq!(
            select_tail_lines_for_rows(&lines, 1, 4),
            vec![String::from("very-long-recent-line")]
        );
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
    fn preferred_pane_selection_wins_over_saved_selection() {
        let state_dir = unique_temp_dir("dashboard-preferred-selection");
        fs::create_dir_all(&state_dir).expect("state dir should exist");
        save_selection(
            &state_dir,
            &SavedSelection {
                pane_id: Some(String::from("%2")),
                row_index: None,
            },
        )
        .expect("selection should save");

        let mut app = DashboardApp::new(
            TmuxClient::default(),
            TmuxClient::default(),
            RuntimeClient::with_socket_path(
                unique_temp_dir("dashboard-runtime-sock").join("runtime.sock"),
            ),
            state_dir.clone(),
            1000,
            120,
            Some(String::from("%1")),
        )
        .expect("app should initialize");
        app.panes = vec![
            sample_pane_entry_with_id("%1"),
            sample_pane_entry_with_id("%2"),
        ];

        app.rebuild_rows();

        assert_eq!(app.selected_pane_id.as_deref(), Some("%1"));
        let _ = fs::remove_file(dashboard_selection_path(&state_dir));
        let _ = fs::remove_dir_all(&state_dir);
    }

    #[test]
    fn preferred_pane_selection_wins_over_previous_selection() {
        let mut app = DashboardApp::new(
            TmuxClient::default(),
            TmuxClient::default(),
            RuntimeClient::with_socket_path(
                unique_temp_dir("dashboard-runtime-sock").join("runtime.sock"),
            ),
            unique_temp_dir("dashboard-preferred-over-previous"),
            1000,
            120,
            Some(String::from("%1")),
        )
        .expect("app should initialize");
        app.selected_pane_id = Some(String::from("%2"));
        app.panes = vec![
            sample_pane_entry_with_id("%1"),
            sample_pane_entry_with_id("%2"),
        ];

        app.rebuild_rows();

        assert_eq!(app.selected_pane_id.as_deref(), Some("%1"));
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
    fn details_panel_height_includes_borders() {
        assert_eq!(details_panel_height(10), 12);
    }

    #[test]
    fn pane_list_columns_size_cook_from_cook_duration() {
        let mut app = DashboardApp::new(
            TmuxClient::default(),
            TmuxClient::default(),
            RuntimeClient::with_socket_path(
                unique_temp_dir("dashboard-runtime-sock").join("runtime.sock"),
            ),
            unique_temp_dir("dashboard-cook-columns"),
            1000,
            120,
            None,
        )
        .expect("app should initialize");
        let mut pane = sample_pane_entry(SessionState::BusyResponding, false, false, "");
        pane.age = Duration::from_secs(999 * 86400);
        pane.cook_duration = Duration::from_secs(30);
        app.panes = vec![pane];

        assert_eq!(pane_list_columns(&app).cook_width, 4);

        app.panes[0].cook_duration = Duration::from_secs(10 * 86400 + 12 * 3600);
        assert_eq!(pane_list_columns(&app).cook_width, 6);
    }

    #[test]
    fn pad_display_right_left_pads_to_width() {
        assert_eq!(pad_display_right("demo", 8), "    demo");
        assert_eq!(pad_display_right("demo", 4), "demo");
    }

    #[test]
    fn resource_gauges_render_fixed_width_bars() {
        assert_eq!(format_cpu_gauge(Some(0.0)), "▏ 0%");
        assert_eq!(format_cpu_gauge(Some(50.0)), "▌ 50%");
        assert_eq!(format_cpu_gauge(Some(100.0)), "█ 100%");
        assert_eq!(format_cpu_gauge(None), "  -");
        assert_eq!(format_memory_gauge(Some(512 * 1024 * 1024), None), "▌ 512M");
        assert_eq!(format_memory_bar(1536 * 1024 * 1024), "█▌");
    }

    #[test]
    fn parses_process_stat_for_usage_fields() {
        let snapshot = parse_process_stat(
            123,
            "123 (agent worker) S 7 1 1 0 -1 4194560 100 0 0 0 11 13 0 0 20 0 1 0 1000 4096 42 0 0",
        )
        .expect("stat should parse");

        assert_eq!(snapshot.pid, 123);
        assert_eq!(snapshot.ppid, 7);
        assert_eq!(snapshot.pgrp, 1);
        assert_eq!(snapshot.tpgid, -1);
        assert_eq!(snapshot.command, "agent worker");
        assert_eq!(snapshot.start_ticks, 1000);
        assert_eq!(snapshot.cpu_ticks, 24);
        assert_eq!(snapshot.rss_pages, 42);
    }

    #[test]
    fn process_tree_aggregates_descendant_cpu_and_memory_from_one_index() {
        let mut snapshots = vec![
            test_process_snapshot(10, 1, 10, 10, "bash", 100, &[]),
            test_process_snapshot(20, 10, 20, 20, "claude", 200, &[]),
            test_process_snapshot(30, 20, 30, 30, "worker", 300, &[]),
        ];
        snapshots[0].cpu_ticks = 3;
        snapshots[0].rss_pages = 5;
        snapshots[1].cpu_ticks = 7;
        snapshots[1].rss_pages = 11;
        snapshots[2].cpu_ticks = 13;
        snapshots[2].rss_pages = 17;
        let tree = ProcessTreeSnapshot::from_process_snapshots_with_system(
            &snapshots,
            Some(10.0),
            Some(100),
            Some(4096),
        );

        let usage = tree.usage(10).expect("root should resolve");
        assert_eq!(usage.cpu_ticks, 23);
        assert_eq!(usage.memory_bytes, 33 * 4096);
        assert!(tree.usage(999).is_none());
    }

    #[test]
    fn process_tree_counts_each_pid_once_when_parent_links_cycle() {
        let mut snapshots = vec![
            test_process_snapshot(10, 20, 10, 10, "bash", 100, &[]),
            test_process_snapshot(20, 10, 20, 20, "claude", 200, &[]),
        ];
        snapshots[0].cpu_ticks = 3;
        snapshots[0].rss_pages = 5;
        snapshots[1].cpu_ticks = 7;
        snapshots[1].rss_pages = 11;
        let tree = ProcessTreeSnapshot::from_process_snapshots_with_system(
            &snapshots,
            Some(10.0),
            Some(100),
            Some(4096),
        );

        let descendants = tree.descendants(10);
        assert_eq!(descendants.len(), 2);
        let usage = tree.usage(10).expect("cyclic root should resolve");
        assert_eq!(usage.cpu_ticks, 10);
        assert_eq!(usage.memory_bytes, 16 * 4096);
    }

    #[test]
    fn process_tree_supplies_age_and_multiple_panes_from_one_scan() {
        let calls = std::cell::Cell::new(0);
        let snapshots = vec![
            test_process_snapshot(10, 1, 10, 20, "bash", 100, &[]),
            test_process_snapshot(20, 10, 20, 20, "claude", 250, &[]),
        ];
        let tree = ProcessTreeSnapshot::scan_with(|| {
            calls.set(calls.get() + 1);
            snapshots.clone()
        });

        assert_eq!(calls.get(), 1);
        assert!(tree.usage(10).is_some());
        assert!(tree.usage(20).is_some());
        let mut pane = sample_tmux_pane("claude", "demo");
        pane.pane_pid = Some(10);
        assert!(tree.pane_age(&pane).is_some());
    }

    #[test]
    fn detached_fallback_scans_proc_only_for_pi_antigravity_or_grok_candidates() {
        let calls = std::cell::Cell::new(0);
        let no_process_candidates = vec![
            sample_tmux_pane("opencode", "OC | demo"),
            sample_tmux_pane("codex", "Codex"),
        ];
        assert!(
            fallback_process_tree_with(&no_process_candidates, || {
                calls.set(calls.get() + 1);
                ProcessTreeSnapshot::from_process_snapshots(&[])
            })
            .is_none()
        );
        assert_eq!(calls.get(), 0);

        let process_candidates = vec![sample_tmux_pane("pi", "Pi")];
        assert!(
            fallback_process_tree_with(&process_candidates, || {
                calls.set(calls.get() + 1);
                ProcessTreeSnapshot::from_process_snapshots(&[])
            })
            .is_some()
        );
        assert_eq!(calls.get(), 1);

        calls.set(0);
        let grok_candidates = vec![sample_tmux_pane("grok", "Demo - grok")];
        assert!(
            fallback_process_tree_with(&grok_candidates, || {
                calls.set(calls.get() + 1);
                ProcessTreeSnapshot::from_process_snapshots(&[])
            })
            .is_some()
        );
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn detach_clears_cpu_baselines_and_first_reattach_sample_is_unavailable() {
        let mut app = make_test_app("dashboard-detach-cpu");
        app.cpu_samples.insert(
            String::from("%1"),
            super::CpuSample {
                at: std::time::Instant::now(),
                total_ticks: 10,
                process_start_ticks: 100,
            },
        );
        app.clear_detached_resource_state();
        assert!(app.cpu_samples.is_empty());

        let mut snapshots = vec![test_process_snapshot(10, 1, 10, 10, "claude", 100, &[])];
        snapshots[0].cpu_ticks = 20;
        let tree = ProcessTreeSnapshot::from_process_snapshots_with_system(
            &snapshots,
            Some(10.0),
            Some(100),
            Some(4096),
        );
        let usage = app.resource_usage_for_pane(&tree, "%1", Some(10), std::time::Instant::now());
        assert_eq!(usage.cpu_percent, None);
        assert_eq!(usage.memory_bytes, Some(0));
    }

    #[test]
    fn failed_reattach_attempt_clears_partially_repopulated_cpu_baselines() {
        let mut app = make_test_app("dashboard-failed-reattach-cpu");
        let mut snapshots = vec![test_process_snapshot(10, 1, 10, 10, "claude", 100, &[])];
        snapshots[0].cpu_ticks = 20;
        let tree = ProcessTreeSnapshot::from_process_snapshots_with_system(
            &snapshots,
            Some(10.0),
            Some(100),
            Some(4096),
        );

        app.resource_usage_for_pane(&tree, "%1", Some(10), std::time::Instant::now());
        assert!(!app.cpu_samples.is_empty());
        assert!(!app.finish_reattach_attempt(false));
        assert!(app.cpu_samples.is_empty());

        let usage = app.resource_usage_for_pane(
            &tree,
            "%1",
            Some(10),
            std::time::Instant::now() + Duration::from_secs(1),
        );
        assert_eq!(usage.cpu_percent, None);
    }

    #[test]
    fn cpu_sample_restarts_when_the_root_process_identity_changes() {
        let previous = super::CpuSample {
            at: std::time::Instant::now(),
            total_ticks: 10,
            process_start_ticks: 100,
        };
        let current = super::CpuSample {
            at: previous.at + Duration::from_secs(1),
            total_ticks: 1_000,
            process_start_ticks: 200,
        };

        assert_eq!(super::cpu_percent_between(previous, current, 100), None);
    }

    #[test]
    fn agent_process_selection_prefers_foreground_matching_child() {
        let snapshots = vec![
            test_process_snapshot(10, 1, 10, 20, "bash", 100, &[]),
            test_process_snapshot(20, 10, 20, 20, "claude", 900, &["node", "claude"]),
            test_process_snapshot(21, 20, 20, 20, "node", 950, &[]),
        ];

        let selected = select_agent_process(10, "claude", &snapshots)
            .expect("foreground claude process should be selected");

        assert_eq!(selected.pid, 20);
    }

    #[test]
    fn agent_process_selection_falls_back_to_matching_descendant() {
        let snapshots = vec![
            test_process_snapshot(10, 1, 10, 30, "bash", 100, &[]),
            test_process_snapshot(20, 10, 20, 20, "claude", 900, &[]),
        ];

        let selected = select_agent_process(10, "claude", &snapshots)
            .expect("matching descendant should be selected");

        assert_eq!(selected.pid, 20);
    }

    #[test]
    fn agent_process_selection_falls_back_to_root_when_no_match_exists() {
        let snapshots = vec![
            test_process_snapshot(10, 1, 10, 10, "bash", 100, &[]),
            test_process_snapshot(20, 10, 20, 20, "worker", 900, &[]),
        ];

        let selected = select_agent_process(10, "claude", &snapshots)
            .expect("root process should remain a safe fallback");

        assert_eq!(selected.pid, 10);
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
                "after",
                None,
            ),
            SessionState::BusyResponding
        );
        assert_eq!(
            dashboard_display_state(
                SessionState::ChatReady,
                Some(&String::from("same")),
                "same",
                None,
            ),
            SessionState::ChatReady
        );
    }

    #[test]
    fn authoritative_provider_state_wins_over_frame_motion() {
        assert_eq!(
            dashboard_display_state(
                SessionState::ChatReady,
                Some(&String::from("before")),
                "after",
                Some(SessionState::ChatReady),
            ),
            SessionState::ChatReady
        );
        assert_eq!(
            dashboard_display_state(
                SessionState::ChatReady,
                Some(&String::from("same")),
                "same",
                Some(SessionState::BusyResponding),
            ),
            SessionState::BusyResponding
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
        assert_eq!(strip_dashboard_emoji_prefixes("💬 project"), "project");
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
    fn cleanup_dashboard_window_name_strips_botctl_prefixes() {
        let window = TmuxWindow {
            session_name: String::from("work"),
            window_id: String::from("@7"),
            window_index: 3,
            window_name: String::from("⚙️ʸ💬 bloodraven"),
        };

        assert_eq!(cleanup_dashboard_window_name(&window), "bloodraven");
    }

    #[test]
    fn cleanup_dashboard_window_name_ignores_empty_cleaned_name() {
        let window = TmuxWindow {
            session_name: String::from("work"),
            window_id: String::from("@7"),
            window_index: 3,
            window_name: String::from("⚙️ʸ"),
        };

        assert_eq!(cleanup_dashboard_window_name(&window), "⚙️ʸ");
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
    fn title_states_group_and_order_multi_pane_prefixes_by_pane_index() {
        let mut second = sample_tmux_pane("claude", "demo");
        second.pane_id = String::from("%2");
        second.pane_index = 1;
        let mut first = second.clone();
        first.pane_id = String::from("%1");
        first.pane_index = 0;
        let states = [
            TitlePaneState {
                pane: second,
                state: SessionState::BusyResponding,
                has_questions: false,
                yolo_enabled: false,
            },
            TitlePaneState {
                pane: first,
                state: SessionState::ChatReady,
                has_questions: false,
                yolo_enabled: true,
            },
        ]
        .into_iter()
        .map(|state| (state.pane.pane_id.clone(), state))
        .collect();

        let grouped = grouped_title_states(&states);
        let panes = grouped.values().next().expect("window should be grouped");
        assert_eq!(panes[0].pane.pane_id, "%1");
        assert_eq!(panes[1].pane.pane_id, "%2");
        let prefix = panes
            .iter()
            .map(|pane| pane_window_prefix(pane.state, pane.has_questions, pane.yolo_enabled))
            .collect::<String>();
        assert_eq!(prefix, "💤ʸ⚙️");
    }

    #[test]
    fn complete_topology_drops_missing_managed_windows_without_retry_work() {
        let mut managed = HashMap::from([
            (
                String::from("@missing"),
                ManagedWindowName {
                    target: String::from("@missing"),
                    base_name: String::from("missing"),
                    applied_name: Some(String::from("💤 missing")),
                },
            ),
            (
                String::from("@existing"),
                ManagedWindowName {
                    target: String::from("@existing"),
                    base_name: String::from("existing"),
                    applied_name: Some(String::from("💤 existing")),
                },
            ),
        ]);
        let active = HashSet::new();
        let existing = HashSet::from([String::from("@existing")]);

        let restore = prune_missing_stale_managed_windows(&mut managed, &active, &existing);
        assert_eq!(restore, vec![String::from("@existing")]);
        assert!(!managed.contains_key("@missing"));
        assert!(managed.contains_key("@existing"));

        managed.remove("@existing");
        assert!(prune_missing_stale_managed_windows(&mut managed, &active, &existing).is_empty());
        assert!(managed.is_empty());
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
            RuntimeClient::with_socket_path(
                unique_temp_dir("dashboard-runtime-sock").join("runtime.sock"),
            ),
            unique_temp_dir("dashboard-hitbox"),
            1000,
            120,
            None,
        )
        .expect("app should initialize");
        let list_area = Rect {
            x: 10,
            y: 5,
            width: 80,
            height: 10,
        };
        let columns = pane_list_columns(&app);
        let yolo_x = list_area.x
            + 1
            + columns.state_width as u16
            + UnicodeWidthStr::width(ICON_GAP) as u16
            + columns.agent_width as u16
            + UnicodeWidthStr::width(ICON_GAP) as u16;
        assert!(yolo_column_contains(&app, list_area, yolo_x));
        assert!(!yolo_column_contains(&app, list_area, list_area.x));
    }

    fn make_test_app(label: &str) -> DashboardApp {
        DashboardApp::new(
            TmuxClient::default(),
            TmuxClient::default(),
            RuntimeClient::with_socket_path(
                unique_temp_dir(&format!("{label}-sock")).join("runtime.sock"),
            ),
            unique_temp_dir(label),
            1000,
            120,
            None,
        )
        .expect("app should initialize")
    }

    fn agy_tmux_pane(pane_id: &str) -> TmuxPane {
        TmuxPane {
            pane_id: pane_id.to_string(),
            pane_tty: String::from("/dev/pts/1"),
            pane_pid: None,
            session_id: String::from("$1"),
            session_name: String::from("demo"),
            window_id: String::from("@1"),
            window_index: 0,
            window_name: String::from("agy"),
            pane_index: 0,
            current_command: String::from("agy"),
            current_path: String::from("/tmp/demo"),
            pane_title: String::new(),
            pane_active: true,
            cursor_x: None,
            cursor_y: None,
        }
    }

    #[test]
    fn previous_agy_model_evicted_when_pane_no_longer_agy() {
        // V-4/V-9: when resolve_agy_model_sticky is called for a pane that is
        // no longer agy (different command), any cached label should be evicted.
        let mut app = make_test_app("agy-model-evict");
        let pane_id = "%evict1";

        // Seed the cache as if the pane previously had a model.
        app.previous_agy_model
            .insert(pane_id.to_string(), String::from("Gemini 3.5 Flash (High)"));

        // Now the pane's command has changed to "bash" — no longer agy.
        let non_agy_pane = TmuxPane {
            current_command: String::from("bash"),
            ..agy_tmux_pane(pane_id)
        };
        let result = app.resolve_agy_model_sticky(&non_agy_pane, "some bash output\n");

        assert!(
            result.is_none(),
            "non-agy pane must return None from sticky resolver"
        );
        assert!(
            !app.previous_agy_model.contains_key(pane_id),
            "cache entry must be evicted when pane is no longer agy"
        );
    }

    #[test]
    fn previous_agy_model_sticky_cache_returns_last_parsed_value_when_footer_absent() {
        // V-9: when the footer is temporarily unparseable, the sticky cache
        // serves the previous value.
        let mut app = make_test_app("agy-model-sticky");
        let pane_id = "%sticky1";
        let agy_pane = agy_tmux_pane(pane_id);

        // First call with a parseable footer — populates the cache.
        let frame_with_footer = "Antigravity CLI\nesc to cancel                                Gemini 3.5 Flash (High)\n";
        let first = app.resolve_agy_model_sticky(&agy_pane, frame_with_footer);
        assert_eq!(
            first,
            Some(String::from("Gemini 3.5 Flash (High)")),
            "should parse label from footer"
        );

        // Second call with no parseable footer — should return cached value.
        let frame_without_footer = "Antigravity CLI\n⣾ Working...\n";
        let second = app.resolve_agy_model_sticky(&agy_pane, frame_without_footer);
        assert_eq!(
            second,
            Some(String::from("Gemini 3.5 Flash (High)")),
            "sticky cache must serve previous value when footer is absent"
        );
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
            yolo_effective: false,
            yolo_stop_reason: None,
            age: Duration::from_secs(1),
            cook_duration: Duration::from_secs(0),
            wait_duration: None,
            runtime_duration_sample_at_unix_ms: None,
            runtime_cook_active: false,
            runtime_wait_active: false,
            yes_count: 0,
            claude_session_id: None,
            focused_source: focused_source.to_string(),
            resource_usage: ResourceUsage::default(),
            agy_model: None,
        }
    }

    fn sample_pane_entry_with_id(pane_id: &str) -> PaneEntry {
        PaneEntry {
            pane: TmuxPane {
                pane_id: pane_id.to_string(),
                ..sample_pane_entry(SessionState::ChatReady, false, false, "").pane
            },
            ..sample_pane_entry(SessionState::ChatReady, false, false, "")
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

    fn sample_tmux_pane(command: &str, title: &str) -> TmuxPane {
        TmuxPane {
            current_command: command.to_string(),
            pane_title: title.to_string(),
            ..sample_pane_entry(SessionState::ChatReady, false, false, "").pane
        }
    }

    fn test_process_snapshot(
        pid: u32,
        ppid: u32,
        pgrp: i32,
        tpgid: i32,
        command: &str,
        start_ticks: u64,
        cmdline_basenames: &[&str],
    ) -> ProcessSnapshot {
        ProcessSnapshot {
            pid,
            ppid,
            pgrp,
            tpgid,
            command: command.to_string(),
            cmdline_basenames: cmdline_basenames
                .iter()
                .map(|value| value.to_string())
                .collect(),
            start_ticks,
            cpu_ticks: 0,
            rss_pages: 0,
        }
    }

    fn sample_runtime_snapshot(pane_id: &str, revision: u64) -> RuntimePaneSnapshot {
        RuntimePaneSnapshot {
            pane: TmuxPane {
                pane_id: pane_id.to_string(),
                ..sample_tmux_pane("claude", "demo")
            },
            classification: Classification {
                source: String::from("test"),
                state: SessionState::ChatReady,
                has_questions: false,
                recap_present: false,
                recap_excerpt: None,
                signals: Vec::new(),
            },
            focused_source: String::new(),
            raw_source: String::new(),
            live_excerpt: String::new(),
            wait_duration_ms: Some(0),
            cook_duration_ms: Some(0),
            duration_sampled_at_unix_ms: Some(0),
            claude_session_id: None,
            workspace_id: String::from("workspace-1"),
            workspace_root: String::from("/tmp/demo"),
            desired_yolo_enabled: false,
            actual_yolo_enabled: false,
            last_stop_reason: None,
            last_action: None,
            revision,
            updated_at_unix_ms: 0,
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
