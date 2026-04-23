use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::app::{
    ACTION_GUARD_HISTORY_LINES, AppError, AppResult, ContinueOutcome, InspectedPane,
    continue_from_classification, ensure_pane_owned_by_claude, execute_automation_action,
    execute_classified_workflow, extract_permission_prompt_details, inspect_pane,
    is_yolo_safe_to_approve, render_next_safe_action, render_screen_excerpt,
    resolve_workspace_for_pane, submit_prompt_for_pane,
};
use crate::automation::{AutomationAction, GuardedWorkflow, inspect_keybindings};
use crate::classifier::{Classification, SessionState};
use crate::observe::{ControlEvent, decode_tmux_escaped, parse_control_line};
use crate::screen_model::ScreenModel;
use crate::storage::{
    WorkspaceRecord, list_babysit_registration_pane_ids, resolve_workspace,
    resolve_workspace_for_path, sync_tmux_claude_session_id, sync_tmux_wait_state,
};
use crate::tmux::{ControlModeReceive, TmuxClient, TmuxPane};
use crate::yolo::{YoloRecord, disable_yolo_record, read_yolo_record, write_yolo_record};

const SOCKET_FILENAME: &str = "runtime.sock";
const TARGET_TMUX_SOCKET_ENV: &str = "BOTCTL_RUNTIME_TARGET_TMUX_SOCKET";
const CONTROL_POLL_MS: u64 = 250;
const STREAM_DEBOUNCE_MS: u64 = 200;
const MAX_LIVE_EXCERPT_LINES: usize = 20;

#[derive(Debug, Clone)]
pub struct RuntimeServerConfig {
    pub state_dir: PathBuf,
    pub history_lines: usize,
    pub reconcile_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeStatus {
    pub socket_path: String,
    pub boot_time_unix_ms: i64,
    pub revision: u64,
    pub pane_count: usize,
    pub tmux_socket_path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimePaneSnapshot {
    pub pane: TmuxPane,
    pub classification: Classification,
    pub focused_source: String,
    pub raw_source: String,
    pub live_excerpt: String,
    pub wait_duration_ms: Option<u64>,
    pub claude_session_id: Option<String>,
    pub workspace_id: String,
    pub workspace_root: String,
    pub desired_yolo_enabled: bool,
    pub actual_yolo_enabled: bool,
    pub last_stop_reason: Option<String>,
    pub last_action: Option<String>,
    pub revision: u64,
    pub updated_at_unix_ms: i64,
}

impl RuntimePaneSnapshot {
    pub(crate) fn to_inspected_pane(&self) -> InspectedPane {
        InspectedPane {
            classification: self.classification.clone(),
            focused_source: self.focused_source.clone(),
            raw_source: self.raw_source.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeActionResult {
    pub pane_id: String,
    pub action: String,
    pub executed: String,
    pub after_state: Option<String>,
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeEvent {
    pub revision: u64,
    pub timestamp_unix_ms: i64,
    pub kind: String,
    pub pane_id: Option<String>,
    pub workspace_id: Option<String>,
    pub summary: String,
    pub snapshot: Option<RuntimePaneSnapshot>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum RuntimeRequest {
    GetStatus,
    ListPanes {
        session_name: Option<String>,
        target_pane: Option<String>,
    },
    GetPane {
        pane_id: String,
        session_name: Option<String>,
    },
    SetYolo {
        pane_id: Option<String>,
        workspace: Option<String>,
        all: bool,
        enabled: bool,
    },
    RunAction {
        pane_id: String,
        session_name: Option<String>,
        action: String,
    },
    SubmitPrompt {
        session_name: String,
        pane_id: String,
        workspace: Option<String>,
        prompt_text: String,
        submit_delay_ms: u64,
    },
    Stop,
    Subscribe,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum RuntimeResponse {
    Ok,
    Status { status: RuntimeStatus },
    Panes { panes: Vec<RuntimePaneSnapshot> },
    Pane { pane: Option<RuntimePaneSnapshot> },
    YoloUpdated { updated: Vec<String> },
    Action { result: RuntimeActionResult },
    Error { message: String, exit_code: i32 },
    Event { event: RuntimeEvent },
}

#[derive(Clone)]
pub struct RuntimeClient {
    socket_path: PathBuf,
}

impl RuntimeClient {
    pub fn connect(state_dir: &Path) -> AppResult<Self> {
        Ok(Self {
            socket_path: runtime_socket_path(state_dir),
        })
    }

    pub(crate) fn with_socket_path(socket_path: PathBuf) -> Self {
        Self { socket_path }
    }

    pub fn get_status(&self) -> AppResult<RuntimeStatus> {
        match self.request(RuntimeRequest::GetStatus)? {
            RuntimeResponse::Status { status } => Ok(status),
            other => Err(unexpected_response("status", &other)),
        }
    }

    pub fn list_panes(
        &self,
        session_name: Option<&str>,
        target_pane: Option<&str>,
    ) -> AppResult<Vec<RuntimePaneSnapshot>> {
        match self.request(RuntimeRequest::ListPanes {
            session_name: session_name.map(str::to_string),
            target_pane: target_pane.map(str::to_string),
        })? {
            RuntimeResponse::Panes { panes } => Ok(panes),
            other => Err(unexpected_response("panes", &other)),
        }
    }

    pub fn get_pane(
        &self,
        pane_id: &str,
        session_name: Option<&str>,
    ) -> AppResult<Option<RuntimePaneSnapshot>> {
        match self.request(RuntimeRequest::GetPane {
            pane_id: pane_id.to_string(),
            session_name: session_name.map(str::to_string),
        })? {
            RuntimeResponse::Pane { pane } => Ok(pane),
            other => Err(unexpected_response("pane", &other)),
        }
    }

    pub fn set_yolo(
        &self,
        pane_id: Option<&str>,
        workspace: Option<&str>,
        all: bool,
        enabled: bool,
    ) -> AppResult<Vec<String>> {
        match self.request(RuntimeRequest::SetYolo {
            pane_id: pane_id.map(str::to_string),
            workspace: workspace.map(str::to_string),
            all,
            enabled,
        })? {
            RuntimeResponse::YoloUpdated { updated } => Ok(updated),
            other => Err(unexpected_response("yolo", &other)),
        }
    }

    pub fn run_action(
        &self,
        pane_id: &str,
        session_name: Option<&str>,
        action: &str,
    ) -> AppResult<RuntimeActionResult> {
        match self.request(RuntimeRequest::RunAction {
            pane_id: pane_id.to_string(),
            session_name: session_name.map(str::to_string),
            action: action.to_string(),
        })? {
            RuntimeResponse::Action { result } => Ok(result),
            other => Err(unexpected_response("action", &other)),
        }
    }

    pub fn submit_prompt(
        &self,
        session_name: &str,
        pane_id: &str,
        workspace: Option<&str>,
        prompt_text: &str,
        submit_delay_ms: u64,
    ) -> AppResult<RuntimeActionResult> {
        match self.request(RuntimeRequest::SubmitPrompt {
            session_name: session_name.to_string(),
            pane_id: pane_id.to_string(),
            workspace: workspace.map(str::to_string),
            prompt_text: prompt_text.to_string(),
            submit_delay_ms,
        })? {
            RuntimeResponse::Action { result } => Ok(result),
            other => Err(unexpected_response("submit_prompt", &other)),
        }
    }

    pub fn subscribe(&self) -> AppResult<RuntimeSubscription> {
        let mut stream = UnixStream::connect(&self.socket_path).map_err(|error| {
            AppError::new(format!(
                "failed to connect runtime socket {}: {error}",
                self.socket_path.display()
            ))
        })?;
        write_message(&mut stream, &RuntimeRequest::Subscribe)?;
        Ok(RuntimeSubscription {
            reader: BufReader::new(stream),
        })
    }

    pub fn stop(&self) -> AppResult<()> {
        match self.request(RuntimeRequest::Stop)? {
            RuntimeResponse::Ok => Ok(()),
            other => Err(unexpected_response("stop", &other)),
        }
    }

    fn request(&self, request: RuntimeRequest) -> AppResult<RuntimeResponse> {
        let mut stream = UnixStream::connect(&self.socket_path).map_err(|error| {
            AppError::new(format!(
                "failed to connect runtime socket {}: {error}",
                self.socket_path.display()
            ))
        })?;
        write_message(&mut stream, &request)?;
        let mut reader = BufReader::new(stream);
        read_response(&mut reader)
    }
}

pub struct RuntimeSubscription {
    reader: BufReader<UnixStream>,
}

impl RuntimeSubscription {
    pub fn recv(&mut self) -> AppResult<Option<RuntimeEvent>> {
        match read_response(&mut self.reader)? {
            RuntimeResponse::Event { event } => Ok(Some(event)),
            RuntimeResponse::Ok => Ok(None),
            RuntimeResponse::Error { message, exit_code } => {
                Err(AppError::with_exit_code(message, exit_code))
            }
            other => Err(unexpected_response("event", &other)),
        }
    }
}

pub fn runtime_socket_path(state_dir: &Path) -> PathBuf {
    state_dir.join(SOCKET_FILENAME)
}

pub fn run_runtime_server(config: RuntimeServerConfig) -> AppResult<String> {
    fs::create_dir_all(&config.state_dir)?;
    let socket_path = runtime_socket_path(&config.state_dir);
    if socket_path.exists() {
        let _ = fs::remove_file(&socket_path);
    }

    let listener = UnixListener::bind(&socket_path).map_err(|error| {
        AppError::new(format!(
            "failed to bind runtime socket {}: {error}",
            socket_path.display()
        ))
    })?;
    listener.set_nonblocking(true)?;

    let shared = Arc::new(Mutex::new(RuntimeShared::new(&socket_path)));
    let stop = Arc::new(AtomicBool::new(false));
    let observer = spawn_observer(config.clone(), Arc::clone(&shared), Arc::clone(&stop));
    emit_shared_event(
        &shared,
        None,
        None,
        "runtime-started",
        format!("runtime listening on {}", socket_path.display()),
        None,
    );

    while !stop.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, _)) => {
                let shared = Arc::clone(&shared);
                let config = config.clone();
                let stop = Arc::clone(&stop);
                thread::spawn(move || {
                    if let Err(error) = handle_client(stream, shared, config, Arc::clone(&stop)) {
                        eprintln!("warning: runtime client failed: {error}");
                    }
                });
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(100));
            }
            Err(error) => {
                stop.store(true, Ordering::SeqCst);
                let _ = observer.join();
                let _ = fs::remove_file(&socket_path);
                return Err(AppError::new(format!("runtime socket accept failed: {error}")));
            }
        }
    }

    let _ = observer.join();
    let _ = fs::remove_file(&socket_path);
    Ok(String::new())
}

#[derive(Debug)]
struct RuntimeShared {
    socket_path: String,
    boot_time_unix_ms: i64,
    tmux_socket_path: Option<String>,
    next_revision: u64,
    panes: BTreeMap<String, TrackedPane>,
    subscribers: Vec<mpsc::Sender<RuntimeEvent>>,
    in_flight_actions: HashSet<String>,
}

impl RuntimeShared {
    fn new(socket_path: &Path) -> Self {
        Self {
            socket_path: socket_path.display().to_string(),
            boot_time_unix_ms: now_unix_ms(),
            tmux_socket_path: current_tmux_socket_path(),
            next_revision: 0,
            panes: BTreeMap::new(),
            subscribers: Vec::new(),
            in_flight_actions: HashSet::new(),
        }
    }

    fn status(&self) -> RuntimeStatus {
        RuntimeStatus {
            socket_path: self.socket_path.clone(),
            boot_time_unix_ms: self.boot_time_unix_ms,
            revision: self.next_revision,
            pane_count: self.panes.len(),
            tmux_socket_path: self.tmux_socket_path.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SnapshotSignature {
    pane: TmuxPane,
    classification: Classification,
}

#[derive(Debug)]
struct TrackedPane {
    pane: TmuxPane,
    workspace: WorkspaceRecord,
    snapshot: RuntimePaneSnapshot,
    signature: SnapshotSignature,
    screen_model: Option<ScreenModel>,
    screen_model_seeded: bool,
    dirty: bool,
    last_stream_activity: Option<Instant>,
}

#[derive(Debug)]
enum ObserverMessage {
    ControlLine(String),
    ControlClosed(String),
}

fn handle_client(
    stream: UnixStream,
    shared: Arc<Mutex<RuntimeShared>>,
    config: RuntimeServerConfig,
    stop: Arc<AtomicBool>,
) -> AppResult<()> {
    let mut reader = BufReader::new(stream);
    let request = read_request(&mut reader)?;
    let mut stream = reader.into_inner();

    match request {
        RuntimeRequest::GetStatus => {
            let status = shared.lock().unwrap().status();
            write_message(&mut stream, &RuntimeResponse::Status { status })
        }
        RuntimeRequest::ListPanes {
            session_name,
            target_pane,
        } => {
            let panes = {
                let shared = shared.lock().unwrap();
                filtered_snapshots(&shared, session_name.as_deref(), target_pane.as_deref())
            };
            write_message(&mut stream, &RuntimeResponse::Panes { panes })
        }
        RuntimeRequest::GetPane {
            pane_id,
            session_name,
        } => {
            let pane = {
                let shared = shared.lock().unwrap();
                shared
                    .panes
                    .get(&pane_id)
                    .map(|tracked| tracked.snapshot.clone())
                    .filter(|snapshot| session_matches(snapshot, session_name.as_deref()))
            };
            write_message(&mut stream, &RuntimeResponse::Pane { pane })
        }
        RuntimeRequest::SetYolo {
            pane_id,
            workspace,
            all,
            enabled,
        } => {
            let updated = handle_set_yolo(&config.state_dir, &shared, pane_id, workspace, all, enabled)?;
            write_message(&mut stream, &RuntimeResponse::YoloUpdated { updated })
        }
        RuntimeRequest::RunAction {
            pane_id,
            session_name,
            action,
        } => {
            let result = handle_run_action(&config, &shared, &pane_id, session_name.as_deref(), &action)?;
            write_message(&mut stream, &RuntimeResponse::Action { result })
        }
        RuntimeRequest::SubmitPrompt {
            session_name,
            pane_id,
            workspace,
            prompt_text,
            submit_delay_ms,
        } => {
            let result = handle_submit_prompt(
                &config,
                &shared,
                &session_name,
                &pane_id,
                workspace.as_deref(),
                &prompt_text,
                submit_delay_ms,
            )?;
            write_message(&mut stream, &RuntimeResponse::Action { result })
        }
        RuntimeRequest::Stop => {
            stop.store(true, Ordering::SeqCst);
            write_message(&mut stream, &RuntimeResponse::Ok)
        }
        RuntimeRequest::Subscribe => handle_subscribe(stream, &shared),
    }
}

fn handle_subscribe(mut stream: UnixStream, shared: &Arc<Mutex<RuntimeShared>>) -> AppResult<()> {
    let (tx, rx) = mpsc::channel();
    let backlog = {
        let mut shared = shared.lock().unwrap();
        shared.subscribers.push(tx);
        shared
            .panes
            .values()
            .map(|tracked| RuntimeEvent {
                revision: tracked.snapshot.revision,
                timestamp_unix_ms: tracked.snapshot.updated_at_unix_ms,
                kind: String::from("pane-snapshot-updated"),
                pane_id: Some(tracked.snapshot.pane.pane_id.clone()),
                workspace_id: Some(tracked.snapshot.workspace_id.clone()),
                summary: format!(
                    "{} {}",
                    tracked.snapshot.pane.pane_id,
                    tracked.snapshot.classification.state.as_str()
                ),
                snapshot: Some(tracked.snapshot.clone()),
            })
            .collect::<Vec<_>>()
    };

    for event in backlog {
        write_message(&mut stream, &RuntimeResponse::Event { event })?;
    }

    loop {
        match rx.recv() {
            Ok(event) => {
                if write_message(&mut stream, &RuntimeResponse::Event { event }).is_err() {
                    return Ok(());
                }
            }
            Err(_) => return Ok(()),
        }
    }
}

fn handle_set_yolo(
    state_dir: &Path,
    shared: &Arc<Mutex<RuntimeShared>>,
    pane_id: Option<String>,
    workspace: Option<String>,
    all: bool,
    enabled: bool,
) -> AppResult<Vec<String>> {
    let client = runtime_tmux_client();
    let mut updated = Vec::new();
    if let Some(pane_id) = pane_id {
        let pane = client
            .pane_by_target(&pane_id)?
            .ok_or_else(|| AppError::new(format!("pane not found: {pane_id}")))?;
        ensure_pane_owned_by_claude(&pane)?;
        let workspace = resolve_workspace_for_pane(state_dir, &pane, workspace.as_deref())?;
        if enabled {
            write_yolo_record(state_dir, &workspace.id, &pane)?;
        } else {
            let _ = disable_yolo_record(state_dir, &pane.pane_id)?;
        }
        updated.push(pane.pane_id.clone());
        let summary = if enabled { "yolo-enabled" } else { "yolo-disabled" };
        emit_shared_event(
            shared,
            Some(pane.pane_id),
            Some(workspace.id),
            summary,
            summary.to_string(),
            None,
        );
        return Ok(updated);
    }

    if all {
        let pane_ids = if let Some(selector) = workspace.as_deref() {
            let selected = resolve_workspace(state_dir, Some(selector), &std::env::current_dir()?)?;
            if enabled {
                client
                    .list_panes()?
                    .into_iter()
                    .filter(|pane| pane.current_command.eq_ignore_ascii_case("claude"))
                    .filter_map(|pane| {
                        resolve_workspace_for_path(state_dir, Path::new(&pane.current_path))
                            .ok()
                            .filter(|workspace| workspace.id == selected.id)
                            .map(|_| pane)
                    })
                    .map(|pane| {
                        write_yolo_record(state_dir, &selected.id, &pane)?;
                        Ok(pane.pane_id)
                    })
                    .collect::<AppResult<Vec<_>>>()?
            } else {
                list_babysit_registration_pane_ids(state_dir, Some(&selected.id))?
                    .into_iter()
                    .map(|pane_id| {
                        let _ = disable_yolo_record(state_dir, &pane_id)?;
                        Ok(pane_id)
                    })
                    .collect::<AppResult<Vec<_>>>()?
            }
        } else if enabled {
            client
                .list_panes()?
                .into_iter()
                .filter(|pane| pane.current_command.eq_ignore_ascii_case("claude"))
                .map(|pane| {
                    let workspace = resolve_workspace_for_path(state_dir, Path::new(&pane.current_path))?;
                    write_yolo_record(state_dir, &workspace.id, &pane)?;
                    Ok(pane.pane_id)
                })
                .collect::<AppResult<Vec<_>>>()?
        } else {
            list_babysit_registration_pane_ids(state_dir, None)?
                .into_iter()
                .map(|pane_id| {
                    let _ = disable_yolo_record(state_dir, &pane_id)?;
                    Ok(pane_id)
                })
                .collect::<AppResult<Vec<_>>>()?
        };
        updated.extend(pane_ids);
    }

    Ok(updated)
}

fn handle_run_action(
    config: &RuntimeServerConfig,
    shared: &Arc<Mutex<RuntimeShared>>,
    pane_id: &str,
    session_name: Option<&str>,
    action: &str,
) -> AppResult<RuntimeActionResult> {
    let _guard = ActionGuard::acquire(shared, pane_id)?;
    let client = runtime_tmux_client();
    let snapshot = get_snapshot(shared, pane_id, session_name)?;
    let current = validate_action_target(&client, &snapshot)?;
    ensure_pane_owned_by_claude(&current)?;
    emit_shared_event(
        shared,
        Some(pane_id.to_string()),
        Some(snapshot.workspace_id.clone()),
        "action-requested",
        format!("{} {}", pane_id, action),
        None,
    );

    let result = execute_runtime_action(&client, config, &current, action, session_name)?;
    emit_shared_event(
        shared,
        Some(pane_id.to_string()),
        Some(snapshot.workspace_id.clone()),
        "action-executed",
        format!("{} {}", pane_id, action),
        get_snapshot(shared, pane_id, None).ok(),
    );
    Ok(result)
}

fn handle_submit_prompt(
    config: &RuntimeServerConfig,
    shared: &Arc<Mutex<RuntimeShared>>,
    session_name: &str,
    pane_id: &str,
    workspace: Option<&str>,
    prompt_text: &str,
    submit_delay_ms: u64,
) -> AppResult<RuntimeActionResult> {
    let _guard = ActionGuard::acquire(shared, pane_id)?;
    let client = runtime_tmux_client();
    let snapshot = get_snapshot(shared, pane_id, None)?;
    let current = validate_action_target(&client, &snapshot)?;
    ensure_pane_owned_by_claude(&current)?;
    let submitted = submit_prompt_for_pane(
        &client,
        &current,
        session_name,
        Some(&config.state_dir),
        workspace,
        prompt_text,
        submit_delay_ms,
    )?;
    let result = RuntimeActionResult {
        pane_id: pane_id.to_string(),
        action: String::from("submit-prompt"),
        executed: String::from("submit-prompt"),
        after_state: Some(submitted.state),
        detail: Some(format!("workspace={} delay_ms={}", submitted.workspace_id, submitted.delay_ms)),
    };
    emit_shared_event(
        shared,
        Some(pane_id.to_string()),
        Some(snapshot.workspace_id.clone()),
        "action-executed",
        format!("{} submit-prompt", pane_id),
        None,
    );
    Ok(result)
}

fn execute_runtime_action(
    client: &TmuxClient,
    _config: &RuntimeServerConfig,
    pane: &TmuxPane,
    action: &str,
    _session_name: Option<&str>,
) -> AppResult<RuntimeActionResult> {
    let classification = inspect_pane(client, &pane.pane_id, ACTION_GUARD_HISTORY_LINES)?;
    let after_state = match action {
        "approve-permission" => {
            execute_classified_workflow(
                client,
                &pane.pane_id,
                GuardedWorkflow::ApprovePermission,
                &classification.classification,
            )?;
            Some(inspect_pane(client, &pane.pane_id, ACTION_GUARD_HISTORY_LINES)?.classification.state)
        }
        "reject-permission" => {
            execute_classified_workflow(
                client,
                &pane.pane_id,
                GuardedWorkflow::RejectPermission,
                &classification.classification,
            )?;
            Some(inspect_pane(client, &pane.pane_id, ACTION_GUARD_HISTORY_LINES)?.classification.state)
        }
        "dismiss-survey" => {
            execute_classified_workflow(
                client,
                &pane.pane_id,
                GuardedWorkflow::DismissSurvey,
                &classification.classification,
            )?;
            Some(inspect_pane(client, &pane.pane_id, ACTION_GUARD_HISTORY_LINES)?.classification.state)
        }
        "confirm-previous" => {
            execute_automation_action(client, &pane.pane_id, AutomationAction::ConfirmPrevious)?;
            None
        }
        "confirm-next" => {
            execute_automation_action(client, &pane.pane_id, AutomationAction::ConfirmNext)?;
            None
        }
        "confirm-yes" => {
            execute_automation_action(client, &pane.pane_id, AutomationAction::ConfirmYes)?;
            None
        }
        "confirm-no" => {
            execute_automation_action(client, &pane.pane_id, AutomationAction::ConfirmNo)?;
            None
        }
        "interrupt" => {
            execute_automation_action(client, &pane.pane_id, AutomationAction::Interrupt)?;
            None
        }
        "continue-session" => Some(continue_from_classification(client, pane, &classification.classification)?.after.state),
        "auto-unstick" => Some(auto_unstick(client, pane)?.after.state),
        "enter" => {
            client.send_keys(&pane.pane_id, &["Enter"])?;
            None
        }
        action if action.starts_with("send-text:") => {
            let text = action.trim_start_matches("send-text:");
            client.send_keys(&pane.pane_id, &[text])?;
            None
        }
        _ => {
            return Err(AppError::with_exit_code(
                format!("unsupported action: {action}"),
                404,
            ));
        }
    };

    Ok(RuntimeActionResult {
        pane_id: pane.pane_id.clone(),
        action: action.to_string(),
        executed: action.to_string(),
        after_state: after_state.map(SessionState::as_str).map(str::to_string),
        detail: None,
    })
}

fn auto_unstick(client: &TmuxClient, pane: &TmuxPane) -> AppResult<ContinueOutcome> {
    let current = inspect_pane(client, &pane.pane_id, ACTION_GUARD_HISTORY_LINES)?;
    continue_from_classification(client, pane, &current.classification)
}

fn spawn_observer(
    config: RuntimeServerConfig,
    shared: Arc<Mutex<RuntimeShared>>,
    stop: Arc<AtomicBool>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let client = runtime_tmux_client();
        let (tx, rx) = mpsc::channel();
        let mut observed_sessions = BTreeSet::new();
        let mut last_reconcile = Instant::now() - Duration::from_millis(config.reconcile_ms);

        while !stop.load(Ordering::SeqCst) {
            if let Ok(panes) = client.list_panes() {
                for session_name in panes
                    .into_iter()
                    .filter(|pane| pane.current_command.eq_ignore_ascii_case("claude"))
                    .map(|pane| pane.session_name)
                    .collect::<BTreeSet<_>>()
                {
                    if observed_sessions.insert(session_name.clone()) {
                        let tx = tx.clone();
                        let client = client.clone();
                        let stop = Arc::clone(&stop);
                        thread::spawn(move || observe_session_lines(client, &session_name, tx, stop));
                    }
                }
            }

            while let Ok(message) = rx.try_recv() {
                match message {
                    ObserverMessage::ControlLine(line) => handle_control_line_shared(&shared, &line),
                    ObserverMessage::ControlClosed(session_name) => {
                        observed_sessions.remove(&session_name);
                    }
                }
            }

            if last_reconcile.elapsed() >= Duration::from_millis(config.reconcile_ms) {
                if let Err(error) = reconcile_all(&client, &config, &shared) {
                    emit_shared_event(
                        &shared,
                        None,
                        None,
                        "runtime-error",
                        error.to_string(),
                        None,
                    );
                }
                last_reconcile = Instant::now();
            }

            thread::sleep(Duration::from_millis(50));
        }
    })
}

fn observe_session_lines(
    client: TmuxClient,
    session_name: &str,
    tx: mpsc::Sender<ObserverMessage>,
    stop: Arc<AtomicBool>,
) {
    let Ok(mut control) = client.control_mode_session(session_name) else {
        let _ = tx.send(ObserverMessage::ControlClosed(session_name.to_string()));
        return;
    };

    while !stop.load(Ordering::SeqCst) {
        match control.recv_timeout(Duration::from_millis(CONTROL_POLL_MS)) {
            Ok(ControlModeReceive::Line(line)) => {
                if tx.send(ObserverMessage::ControlLine(line)).is_err() {
                    break;
                }
            }
            Ok(ControlModeReceive::Idle) => {}
            Ok(ControlModeReceive::Closed) | Err(_) => break,
        }
    }

    let _ = tx.send(ObserverMessage::ControlClosed(session_name.to_string()));
}

fn handle_control_line_shared(shared: &Arc<Mutex<RuntimeShared>>, line: &str) {
    let mut shared = shared.lock().unwrap();
    match parse_control_line(line) {
        Some(ControlEvent::Output { pane_id, payload })
        | Some(ControlEvent::ExtendedOutput { pane_id, payload, .. }) => {
            if let Some(tracked) = shared.panes.get_mut(&pane_id) {
                let decoded = decode_tmux_escaped(&payload);
                let model = tracked
                    .screen_model
                    .get_or_insert_with(|| ScreenModel::new(tracked.snapshot.raw_source.lines().count().max(MAX_LIVE_EXCERPT_LINES)));
                model.ingest(&decoded);
                tracked.snapshot.live_excerpt = trim_visible_block(&model.render(), MAX_LIVE_EXCERPT_LINES);
                tracked.dirty = true;
                tracked.last_stream_activity = Some(Instant::now());
            }
        }
        Some(ControlEvent::Notification(message)) => {
            if notification_requires_reconcile(&message) {
                drop(shared);
            }
        }
        None => {}
    }
}

fn reconcile_all(
    client: &TmuxClient,
    config: &RuntimeServerConfig,
    shared: &Arc<Mutex<RuntimeShared>>,
) -> AppResult<()> {
    let current_panes = client
        .list_panes()?
        .into_iter()
        .filter(|pane| pane.current_command.eq_ignore_ascii_case("claude"))
        .collect::<Vec<_>>();
    let current_ids = current_panes
        .iter()
        .map(|pane| pane.pane_id.clone())
        .collect::<BTreeSet<_>>();

    let removed = {
        let shared = shared.lock().unwrap();
        shared
            .panes
            .keys()
            .filter(|pane_id| !current_ids.contains(*pane_id))
            .cloned()
            .collect::<Vec<_>>()
    };

    for pane_id in removed {
        let snapshot = {
            let mut shared = shared.lock().unwrap();
            shared.panes.remove(&pane_id).map(|tracked| tracked.snapshot)
        };
        if let Some(snapshot) = snapshot {
            if snapshot.desired_yolo_enabled {
                let _ = disable_yolo_record(&config.state_dir, &pane_id);
                emit_shared_event(
                    shared,
                    Some(pane_id.clone()),
                    Some(snapshot.workspace_id.clone()),
                    "yolo-monitoring-stopped",
                    String::from("missing-pane"),
                    None,
                );
            }
            emit_shared_event(
                shared,
                Some(pane_id),
                Some(snapshot.workspace_id),
                "pane-removed",
                String::from("pane removed"),
                None,
            );
        }
    }

    for pane in current_panes {
        let workspace = resolve_workspace_for_path(&config.state_dir, Path::new(&pane.current_path))?;
        let inspected = inspect_pane(client, &pane.pane_id, config.history_lines)?;
        let wait_duration = sync_tmux_wait_state(
            &config.state_dir,
            &workspace.id,
            &pane,
            inspected.classification.state.as_str(),
            is_waiting_state(inspected.classification.state),
        )?;
        let claude_session_id = sync_tmux_claude_session_id(&config.state_dir, &workspace.id, &pane)?;
        let desired_yolo_enabled = matches!(read_yolo_record(&config.state_dir, &pane.pane_id)?, Some(record) if record.enabled);

        let live_excerpt = {
            let shared = shared.lock().unwrap();
            shared
                .panes
                .get(&pane.pane_id)
                .map(|tracked| tracked.snapshot.live_excerpt.clone())
                .filter(|excerpt| !excerpt.is_empty())
                .unwrap_or_else(|| trim_visible_block(&inspected.focused_source, MAX_LIVE_EXCERPT_LINES))
        };

        let updated_at_unix_ms = now_unix_ms();
        let event_snapshot;
        let mut discovered = false;
        let mut state_changed = None;
        {
            let mut shared = shared.lock().unwrap();
            let revision = next_revision(&mut shared);
            let snapshot = RuntimePaneSnapshot {
                pane: pane.clone(),
                classification: inspected.classification.clone(),
                focused_source: inspected.focused_source.clone(),
                raw_source: inspected.raw_source.clone(),
                live_excerpt,
                wait_duration_ms: wait_duration.map(|duration| duration.as_millis() as u64),
                claude_session_id,
                workspace_id: workspace.id.clone(),
                workspace_root: workspace.workspace_root.clone(),
                desired_yolo_enabled,
                actual_yolo_enabled: desired_yolo_enabled,
                last_stop_reason: None,
                last_action: shared
                    .panes
                    .get(&pane.pane_id)
                    .and_then(|tracked| tracked.snapshot.last_action.clone()),
                revision,
                updated_at_unix_ms,
            };
            let signature = SnapshotSignature {
                pane: pane.clone(),
                classification: inspected.classification.clone(),
            };
            let entry = shared.panes.entry(pane.pane_id.clone());
            match entry {
                std::collections::btree_map::Entry::Vacant(entry) => {
                    discovered = true;
                    event_snapshot = Some(snapshot.clone());
                    entry.insert(TrackedPane {
                        pane: pane.clone(),
                        workspace,
                        snapshot,
                        signature,
                        screen_model: None,
                        screen_model_seeded: false,
                        dirty: false,
                        last_stream_activity: None,
                    });
                }
                std::collections::btree_map::Entry::Occupied(mut entry) => {
                    let tracked = entry.get_mut();
                    if tracked.snapshot.classification.state != inspected.classification.state {
                        state_changed = Some(inspected.classification.state);
                    }
                    tracked.pane = pane.clone();
                    tracked.workspace = workspace;
                    tracked.signature = signature;
                    tracked.snapshot = snapshot.clone();
                    if !tracked.screen_model_seeded && !inspected.focused_source.is_empty() {
                        let mut model = ScreenModel::new(config.history_lines.max(MAX_LIVE_EXCERPT_LINES));
                        model.seed(&inspected.focused_source);
                        tracked.screen_model = Some(model);
                        tracked.screen_model_seeded = true;
                    }
                    event_snapshot = Some(snapshot);
                }
            }
        }

        if let Some(snapshot) = event_snapshot.clone() {
            if discovered {
                emit_shared_event(
                    shared,
                    Some(snapshot.pane.pane_id.clone()),
                    Some(snapshot.workspace_id.clone()),
                    "pane-discovered",
                    format!("pane discovered {}", snapshot.pane.pane_id),
                    Some(snapshot.clone()),
                );
            }
            emit_shared_event(
                shared,
                Some(snapshot.pane.pane_id.clone()),
                Some(snapshot.workspace_id.clone()),
                "pane-snapshot-updated",
                format!("{} {}", snapshot.pane.pane_id, snapshot.classification.state.as_str()),
                Some(snapshot.clone()),
            );
            if let Some(state) = state_changed {
                emit_shared_event(
                    shared,
                    Some(snapshot.pane.pane_id.clone()),
                    Some(snapshot.workspace_id.clone()),
                    "pane-state-changed",
                    format!("{} {}", snapshot.pane.pane_id, state.as_str()),
                    Some(snapshot.clone()),
                );
            }
            maybe_run_yolo_action(client, config, shared, &snapshot)?;
        }
    }

    Ok(())
}

fn maybe_run_yolo_action(
    client: &TmuxClient,
    config: &RuntimeServerConfig,
    shared: &Arc<Mutex<RuntimeShared>>,
    snapshot: &RuntimePaneSnapshot,
) -> AppResult<()> {
    if !snapshot.desired_yolo_enabled {
        return Ok(());
    }

    let Some(record) = read_yolo_record(&config.state_dir, &snapshot.pane.pane_id)? else {
        return Ok(());
    };
    if !record.matches_pane(&snapshot.pane) {
        let _ = disable_yolo_record(&config.state_dir, &snapshot.pane.pane_id)?;
        emit_shared_event(
            shared,
            Some(snapshot.pane.pane_id.clone()),
            Some(snapshot.workspace_id.clone()),
            "yolo-monitoring-stopped",
            String::from("identity-changed"),
            Some(snapshot.clone()),
        );
        return Ok(());
    }

    let workflow = match yolo_workflow(snapshot) {
        Some(workflow) => workflow,
        None => return Ok(()),
    };

    let _guard = match ActionGuard::acquire(shared, &snapshot.pane.pane_id) {
        Ok(guard) => guard,
        Err(_) => return Ok(()),
    };

    let inspected = snapshot.to_inspected_pane();
    if workflow == GuardedWorkflow::ApprovePermission && !is_yolo_safe_to_approve(&inspected) {
        return Ok(());
    }

    emit_shared_event(
        shared,
        Some(snapshot.pane.pane_id.clone()),
        Some(snapshot.workspace_id.clone()),
        "action-requested",
        format!("yolo {} {}", snapshot.pane.pane_id, workflow.as_str()),
        Some(snapshot.clone()),
    );
    match execute_classified_workflow(client, &snapshot.pane.pane_id, workflow, &snapshot.classification) {
        Ok(executed) => {
            let after = inspect_pane(client, &snapshot.pane.pane_id, ACTION_GUARD_HISTORY_LINES)?;
            let mut next_snapshot = snapshot.clone();
            next_snapshot.classification = after.classification;
            next_snapshot.focused_source = after.focused_source;
            next_snapshot.raw_source = after.raw_source;
            next_snapshot.last_action = Some(executed);
            emit_shared_event(
                shared,
                Some(snapshot.pane.pane_id.clone()),
                Some(snapshot.workspace_id.clone()),
                "action-executed",
                format!("yolo {} {}", snapshot.pane.pane_id, workflow.as_str()),
                Some(next_snapshot),
            );
        }
        Err(error) => {
            emit_shared_event(
                shared,
                Some(snapshot.pane.pane_id.clone()),
                Some(snapshot.workspace_id.clone()),
                "action-failed",
                error.to_string(),
                Some(snapshot.clone()),
            );
        }
    }
    Ok(())
}

fn yolo_workflow(snapshot: &RuntimePaneSnapshot) -> Option<GuardedWorkflow> {
    match snapshot.classification.state {
        SessionState::PermissionDialog => Some(GuardedWorkflow::ApprovePermission),
        SessionState::SurveyPrompt => Some(GuardedWorkflow::DismissSurvey),
        _ => None,
    }
}

fn filtered_snapshots(
    shared: &RuntimeShared,
    session_name: Option<&str>,
    target_pane: Option<&str>,
) -> Vec<RuntimePaneSnapshot> {
    shared
        .panes
        .values()
        .map(|tracked| tracked.snapshot.clone())
        .filter(|snapshot| session_matches(snapshot, session_name))
        .filter(|snapshot| target_pane.map(|target| snapshot.pane.pane_id == target).unwrap_or(true))
        .collect()
}

fn session_matches(snapshot: &RuntimePaneSnapshot, session_name: Option<&str>) -> bool {
    session_name
        .map(|session_name| snapshot.pane.session_name == session_name)
        .unwrap_or(true)
}

fn get_snapshot(
    shared: &Arc<Mutex<RuntimeShared>>,
    pane_id: &str,
    session_name: Option<&str>,
) -> AppResult<RuntimePaneSnapshot> {
    let shared = shared.lock().unwrap();
    let snapshot = shared
        .panes
        .get(pane_id)
        .map(|tracked| tracked.snapshot.clone())
        .ok_or_else(|| AppError::with_exit_code(format!("pane not found: {pane_id}"), 404))?;
    if !session_matches(&snapshot, session_name) {
        return Err(AppError::with_exit_code(
            format!("pane {} is not tracked for requested session", pane_id),
            404,
        ));
    }
    Ok(snapshot)
}

fn validate_action_target(client: &TmuxClient, snapshot: &RuntimePaneSnapshot) -> AppResult<TmuxPane> {
    let current = client
        .pane_by_id(&snapshot.pane.pane_id)?
        .ok_or_else(|| AppError::with_exit_code(format!("pane not found: {}", snapshot.pane.pane_id), 404))?;
    let current_record = YoloRecord {
        instance_id: String::new(),
        workspace_id: snapshot.workspace_id.clone(),
        enabled: snapshot.desired_yolo_enabled,
        pane_id: snapshot.pane.pane_id.clone(),
        pane_tty: snapshot.pane.pane_tty.clone(),
        pane_pid: snapshot.pane.pane_pid,
        session_id: snapshot.pane.session_id.clone(),
        session_name: snapshot.pane.session_name.clone(),
        window_id: snapshot.pane.window_id.clone(),
        window_name: snapshot.pane.window_name.clone(),
        current_command: snapshot.pane.current_command.clone(),
        current_path: snapshot.pane.current_path.clone(),
    };
    if !current_record.matches_pane(&current) {
        return Err(AppError::with_exit_code(
            format!("pane identity changed for {}", snapshot.pane.pane_id),
            409,
        ));
    }
    Ok(current)
}

struct ActionGuard {
    shared: Arc<Mutex<RuntimeShared>>,
    pane_id: String,
}

impl ActionGuard {
    fn acquire(shared: &Arc<Mutex<RuntimeShared>>, pane_id: &str) -> AppResult<Self> {
        let mut shared_lock = shared.lock().unwrap();
        if !shared_lock.in_flight_actions.insert(pane_id.to_string()) {
            return Err(AppError::with_exit_code(
                format!("pane {} already has an in-flight action", pane_id),
                409,
            ));
        }
        Ok(Self {
            shared: Arc::clone(shared),
            pane_id: pane_id.to_string(),
        })
    }
}

impl Drop for ActionGuard {
    fn drop(&mut self) {
        let mut shared = self.shared.lock().unwrap();
        shared.in_flight_actions.remove(&self.pane_id);
    }
}

fn emit_shared_event(
    shared: &Arc<Mutex<RuntimeShared>>,
    pane_id: Option<String>,
    workspace_id: Option<String>,
    kind: &str,
    summary: String,
    snapshot: Option<RuntimePaneSnapshot>,
) {
    let event = {
        let mut shared = shared.lock().unwrap();
        let event = RuntimeEvent {
            revision: next_revision(&mut shared),
            timestamp_unix_ms: now_unix_ms(),
            kind: kind.to_string(),
            pane_id,
            workspace_id,
            summary,
            snapshot,
        };
        shared
            .subscribers
            .retain(|sender| sender.send(event.clone()).is_ok());
        event
    };
    let _ = event;
}

fn next_revision(shared: &mut RuntimeShared) -> u64 {
    shared.next_revision += 1;
    shared.next_revision
}

fn read_request(reader: &mut BufReader<UnixStream>) -> AppResult<RuntimeRequest> {
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        return Err(AppError::new("runtime client closed before sending a request"));
    }
    serde_json::from_str(line.trim_end()).map_err(|error| {
        AppError::new(format!("failed to decode runtime request JSON: {error}"))
    })
}

fn read_response(reader: &mut BufReader<UnixStream>) -> AppResult<RuntimeResponse> {
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        return Err(AppError::new("runtime socket closed unexpectedly"));
    }
    let response = serde_json::from_str::<RuntimeResponse>(line.trim_end())
        .map_err(|error| AppError::new(format!("failed to decode runtime response JSON: {error}")))?;
    match response {
        RuntimeResponse::Error { message, exit_code } => Err(AppError::with_exit_code(message, exit_code)),
        response => Ok(response),
    }
}

fn write_message<T: Serialize>(stream: &mut UnixStream, value: &T) -> AppResult<()> {
    let encoded = serde_json::to_string(value)
        .map_err(|error| AppError::new(format!("failed to encode runtime JSON: {error}")))?;
    stream.write_all(encoded.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()?;
    Ok(())
}

fn unexpected_response(expected: &str, response: &RuntimeResponse) -> AppError {
    AppError::new(format!("unexpected runtime response for {expected}: {response:?}"))
}

fn trim_visible_block(input: &str, max_lines: usize) -> String {
    if input.is_empty() {
        return String::new();
    }
    let lines = input.lines().collect::<Vec<_>>();
    if lines.len() <= max_lines {
        input.trim_start_matches('\n').to_string()
    } else {
        lines[lines.len() - max_lines..].join("\n")
    }
}

fn notification_requires_reconcile(message: &str) -> bool {
    ["%window-add", "%window-close", "%session-changed", "%pane-add", "%pane-close"]
        .iter()
        .any(|prefix| message.starts_with(prefix))
}

fn is_waiting_state(state: SessionState) -> bool {
    matches!(state, SessionState::BusyResponding)
}

fn now_unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

fn current_tmux_socket_path() -> Option<String> {
    let tmux = std::env::var_os("TMUX")?;
    tmux.to_string_lossy().split(',').next().map(str::to_string)
}

fn runtime_tmux_client() -> TmuxClient {
    match std::env::var_os(TARGET_TMUX_SOCKET_ENV) {
        Some(socket_path) if !socket_path.is_empty() => TmuxClient::with_socket_path(socket_path),
        _ => TmuxClient::default(),
    }
}

pub fn build_instance_summary_json(snapshot: &RuntimePaneSnapshot) -> AppResult<serde_json::Value> {
    let inspected = snapshot.to_inspected_pane();
    let bindings = inspect_keybindings(None).map_err(AppError::new)?;
    Ok(serde_json::json!({
        "id": snapshot.pane.pane_id,
        "pane": snapshot.pane,
        "owned_by_claude": snapshot.pane.current_command.eq_ignore_ascii_case("claude"),
        "classification": {
            "source": inspected.classification.source,
            "state": inspected.classification.state.as_str(),
            "has_questions": inspected.classification.has_questions,
            "recap_present": inspected.classification.recap_present,
            "recap_excerpt": inspected.classification.recap_excerpt,
            "signals": inspected.classification.signals,
        },
        "screen_excerpt": render_screen_excerpt(&inspected.raw_source),
        "next_safe_action": render_next_safe_action(&inspected.classification, &snapshot.pane, &bindings),
        "runtime": {
            "desired_yolo_enabled": snapshot.desired_yolo_enabled,
            "actual_yolo_enabled": snapshot.actual_yolo_enabled,
            "last_stop_reason": snapshot.last_stop_reason,
            "revision": snapshot.revision,
            "updated_at_unix_ms": snapshot.updated_at_unix_ms,
        },
    }))
}

pub fn build_instance_detail_json(snapshot: &RuntimePaneSnapshot) -> AppResult<serde_json::Value> {
    let inspected = snapshot.to_inspected_pane();
    let bindings = inspect_keybindings(None).map_err(AppError::new)?;
    Ok(serde_json::json!({
        "id": snapshot.pane.pane_id,
        "pane": snapshot.pane,
        "owned_by_claude": snapshot.pane.current_command.eq_ignore_ascii_case("claude"),
        "classification": {
            "source": inspected.classification.source,
            "state": inspected.classification.state.as_str(),
            "has_questions": inspected.classification.has_questions,
            "recap_present": inspected.classification.recap_present,
            "recap_excerpt": inspected.classification.recap_excerpt,
            "signals": inspected.classification.signals,
        },
        "screen": {
            "excerpt": render_screen_excerpt(&inspected.raw_source),
            "focused_source": inspected.focused_source,
        },
        "prompt": extract_permission_prompt_details(&inspected).map(|details| serde_json::json!({
            "prompt_type": details.prompt_type,
            "sandbox_mode": details.sandbox_mode,
            "command": details.command,
            "reason": details.reason,
            "question": details.question,
        })),
        "next_safe_action": render_next_safe_action(&inspected.classification, &snapshot.pane, &bindings),
        "runtime": {
            "workspace_id": snapshot.workspace_id,
            "workspace_root": snapshot.workspace_root,
            "desired_yolo_enabled": snapshot.desired_yolo_enabled,
            "actual_yolo_enabled": snapshot.actual_yolo_enabled,
            "last_stop_reason": snapshot.last_stop_reason,
            "wait_duration_ms": snapshot.wait_duration_ms,
            "claude_session_id": snapshot.claude_session_id,
            "revision": snapshot.revision,
            "updated_at_unix_ms": snapshot.updated_at_unix_ms,
        },
    }))
}

#[cfg(test)]
mod tests {
    use super::{RuntimeEvent, RuntimeResponse, runtime_socket_path};
    use std::path::Path;

    #[test]
    fn runtime_socket_path_uses_state_dir() {
        let path = runtime_socket_path(Path::new("/tmp/botctl"));
        assert_eq!(path.display().to_string(), "/tmp/botctl/runtime.sock");
    }

    #[test]
    fn runtime_event_round_trips_json() {
        let event = RuntimeEvent {
            revision: 7,
            timestamp_unix_ms: 9,
            kind: String::from("pane-state-changed"),
            pane_id: Some(String::from("%1")),
            workspace_id: Some(String::from("ws")),
            summary: String::from("updated"),
            snapshot: None,
        };
        let encoded = serde_json::to_string(&RuntimeResponse::Event { event: event.clone() })
            .expect("event should encode");
        let decoded = serde_json::from_str::<RuntimeResponse>(&encoded).expect("event should decode");
        match decoded {
            RuntimeResponse::Event { event: decoded } => assert_eq!(decoded.revision, event.revision),
            other => panic!("unexpected response: {other:?}"),
        }
    }
}
