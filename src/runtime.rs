use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, Condvar, Mutex,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::app::{
    ACTION_GUARD_HISTORY_LINES, AppError, AppResult, ContinueOutcome, InspectedPane,
    continue_from_classification, ensure_pane_owned_by_automatable_provider,
    ensure_pane_owned_by_claude, ensure_pane_owned_by_yolo_provider, ensure_workflow_state,
    execute_automation_action, execute_classified_workflow, extract_permission_prompt_details,
    inspect_pane, is_yolo_safe_to_approve, render_next_safe_action, render_screen_excerpt,
    resolve_workspace_for_pane, submit_prompt_for_pane, yolo_action_for_state,
};
use crate::automation::{AutomationAction, GuardedWorkflow, KeybindingsInspection};
use crate::classifier::{Classification, SIGNAL_CODEX_KEYWORDS, SessionState};
use crate::last_message::resolve_codex_session_id_for_pane;
use crate::observe::{ControlEvent, decode_tmux_escaped, parse_control_line};
use crate::recovery::{
    RecoveryLifecycle, RecoveryMatchState, RecoveryTarget, build_recovery_command,
    match_recoveries, offer_from_record,
};
use crate::screen_model::ScreenModel;
use crate::storage::{
    VerifiedClaudeRecoveryEvidence, WorkspaceRecord, apply_recovery_inventory_evidence,
    begin_runtime_run, claim_recovery_for_staging, dismiss_recovery, finish_runtime_run_clean,
    list_babysit_registration_pane_ids, list_nonterminal_recoveries, load_recovery,
    mark_recovery_staged, mark_recovery_uncertain, mark_stale_staging_uncertain,
    release_known_failed_staging_claim, resolve_live_claude_session_id, resolve_workspace,
    resolve_workspace_for_path, sync_tmux_runtime_state,
};
use crate::tmux::{ControlModeReceive, TmuxClient, TmuxInventory, TmuxPane};

pub use crate::recovery::RuntimeRecoveryOffer;
use crate::yolo::{YoloRecord, disable_yolo_record, read_yolo_record, write_yolo_record};

const SOCKET_FILENAME: &str = "runtime.sock";
const TARGET_TMUX_SOCKET_ENV: &str = "BOTCTL_RUNTIME_TARGET_TMUX_SOCKET";
const CONTROL_POLL_MS: u64 = 250;
const SUBSCRIPTION_READ_TIMEOUT_MS: u64 = 200;
const RUNTIME_TIMEOUT_EXIT_CODE: i32 = 408;
const STREAM_DEBOUNCE_MS: u64 = 200;
const MAX_LIVE_EXCERPT_LINES: usize = 20;
pub const RUNTIME_PROTOCOL_VERSION: u32 = 2;
pub const RUNTIME_SCHEMA_VERSION: i64 = crate::storage::CURRENT_SCHEMA_VERSION;

struct ShutdownState {
    result: Option<Result<(), String>>,
    acknowledged: bool,
}

#[derive(Clone)]
struct MutationGate {
    state: Arc<(Mutex<MutationState>, Condvar)>,
}

struct MutationState {
    stopping: bool,
    active: usize,
}

struct MutationPermit {
    gate: MutationGate,
}

impl MutationGate {
    fn new() -> Self {
        Self {
            state: Arc::new((
                Mutex::new(MutationState {
                    stopping: false,
                    active: 0,
                }),
                Condvar::new(),
            )),
        }
    }

    fn acquire(&self) -> AppResult<MutationPermit> {
        let (state, _) = &*self.state;
        let mut state = state.lock().unwrap();
        if state.stopping {
            return Err(AppError::with_exit_code(
                "runtime shutdown has begun; refusing new mutating request",
                409,
            ));
        }
        state.active += 1;
        Ok(MutationPermit { gate: self.clone() })
    }

    fn begin_stop(&self) -> bool {
        let (state, _) = &*self.state;
        let mut state = state.lock().unwrap();
        if state.stopping {
            return false;
        }
        state.stopping = true;
        true
    }

    fn wait_for_drain(&self) {
        let (state, ready) = &*self.state;
        let mut state = state.lock().unwrap();
        while state.active != 0 {
            state = ready.wait(state).unwrap();
        }
    }
}

impl Drop for MutationPermit {
    fn drop(&mut self) {
        let (state, ready) = &*self.gate.state;
        let mut state = state.lock().unwrap();
        state.active -= 1;
        if state.active == 0 {
            ready.notify_all();
        }
    }
}

#[derive(Debug, Clone)]
pub struct RuntimeServerConfig {
    pub state_dir: PathBuf,
    pub history_lines: usize,
    pub reconcile_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeStatus {
    pub protocol_version: u32,
    pub schema_version: i64,
    pub socket_path: String,
    pub boot_time_unix_ms: i64,
    pub revision: u64,
    pub pane_count: usize,
    pub tmux_socket_path: Option<String>,
}

impl RuntimeStatus {
    pub fn is_compatible(&self) -> bool {
        self.protocol_version == RUNTIME_PROTOCOL_VERSION
            && self.schema_version == RUNTIME_SCHEMA_VERSION
    }
}

#[derive(Debug, Clone)]
pub enum RuntimeProbe {
    Compatible(RuntimeStatus),
    Incompatible(String),
    Unavailable,
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
    ListRecoveries,
    StageRecovery {
        recovery_id: String,
    },
    DismissRecovery {
        recovery_id: String,
    },
    Stop,
    Subscribe,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum RuntimeResponse {
    Ok,
    Status {
        status: RuntimeStatus,
    },
    Panes {
        panes: Vec<RuntimePaneSnapshot>,
    },
    Pane {
        pane: Option<RuntimePaneSnapshot>,
    },
    YoloUpdated {
        updated: Vec<String>,
    },
    Action {
        result: RuntimeActionResult,
    },
    Recoveries {
        recoveries: Vec<RuntimeRecoveryOffer>,
    },
    RecoveryStaged {
        recovery: RuntimeRecoveryOffer,
    },
    RecoveryDismissed {
        recovery_id: String,
    },
    Error {
        message: String,
        exit_code: i32,
    },
    Event {
        event: RuntimeEvent,
    },
}

#[derive(Debug, Clone)]
pub struct RuntimeClient {
    socket_path: PathBuf,
}

impl RuntimeClient {
    pub fn connect(state_dir: &Path) -> AppResult<Self> {
        Ok(Self {
            socket_path: runtime_socket_path(state_dir),
        })
    }

    #[cfg(test)]
    pub(crate) fn with_socket_path(socket_path: PathBuf) -> Self {
        Self { socket_path }
    }

    pub fn get_status(&self) -> AppResult<RuntimeStatus> {
        match self.request(RuntimeRequest::GetStatus)? {
            RuntimeResponse::Status { status } => Ok(status),
            other => Err(unexpected_response("status", &other)),
        }
    }

    pub fn probe(&self) -> AppResult<RuntimeProbe> {
        let mut stream = match UnixStream::connect(&self.socket_path) {
            Ok(stream) => stream,
            Err(_) => return Ok(RuntimeProbe::Unavailable),
        };
        write_message(&mut stream, &RuntimeRequest::GetStatus)?;
        let mut line = String::new();
        if BufReader::new(stream).read_line(&mut line)? == 0 {
            return Ok(RuntimeProbe::Incompatible(
                "runtime closed the capability handshake".to_string(),
            ));
        }
        let value: serde_json::Value = serde_json::from_str(line.trim_end()).map_err(|error| {
            AppError::new(format!(
                "failed to decode runtime capability response: {error}"
            ))
        })?;
        let Some(status) = value.get("status") else {
            return Ok(RuntimeProbe::Incompatible(
                "runtime status omitted capability metadata".to_string(),
            ));
        };
        let Some(protocol_version) = status
            .get("protocol_version")
            .and_then(|value| value.as_u64())
        else {
            return Ok(RuntimeProbe::Incompatible(
                "runtime status has no protocol_version; restart the runtime with this botctl version"
                    .to_string(),
            ));
        };
        let Some(schema_version) = status
            .get("schema_version")
            .and_then(|value| value.as_i64())
        else {
            return Ok(RuntimeProbe::Incompatible(
                "runtime status has no schema_version; restart the runtime with this botctl version"
                    .to_string(),
            ));
        };
        let response: RuntimeResponse = serde_json::from_value(value).map_err(|error| {
            AppError::new(format!(
                "failed to decode runtime capability response: {error}"
            ))
        })?;
        let RuntimeResponse::Status { status } = response else {
            return Ok(RuntimeProbe::Incompatible(
                "runtime returned an unexpected capability response".to_string(),
            ));
        };
        if status.is_compatible() {
            Ok(RuntimeProbe::Compatible(status))
        } else {
            Ok(RuntimeProbe::Incompatible(format!(
                "runtime protocol/schema is {protocol_version}/{schema_version}, but this botctl requires {RUNTIME_PROTOCOL_VERSION}/{RUNTIME_SCHEMA_VERSION}"
            )))
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
        stream.set_read_timeout(Some(Duration::from_millis(SUBSCRIPTION_READ_TIMEOUT_MS)))?;
        write_message(&mut stream, &RuntimeRequest::Subscribe)?;
        Ok(RuntimeSubscription {
            reader: BufReader::new(stream),
        })
    }

    pub fn list_recoveries(&self) -> AppResult<Vec<RuntimeRecoveryOffer>> {
        match self.request(RuntimeRequest::ListRecoveries)? {
            RuntimeResponse::Recoveries { recoveries } => Ok(recoveries),
            other => Err(unexpected_response("recoveries", &other)),
        }
    }

    pub fn stage_recovery(&self, recovery_id: &str) -> AppResult<RuntimeRecoveryOffer> {
        match self.request(RuntimeRequest::StageRecovery {
            recovery_id: recovery_id.to_string(),
        })? {
            RuntimeResponse::RecoveryStaged { recovery } => Ok(recovery),
            other => Err(unexpected_response("stage_recovery", &other)),
        }
    }

    pub fn dismiss_recovery(&self, recovery_id: &str) -> AppResult<()> {
        match self.request(RuntimeRequest::DismissRecovery {
            recovery_id: recovery_id.to_string(),
        })? {
            RuntimeResponse::RecoveryDismissed { .. } => Ok(()),
            other => Err(unexpected_response("dismiss_recovery", &other)),
        }
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
        let response = match read_response(&mut self.reader) {
            Ok(response) => response,
            Err(error) if error.exit_code() == RUNTIME_TIMEOUT_EXIT_CODE => return Ok(None),
            Err(error) => return Err(error),
        };
        match response {
            RuntimeResponse::Event { event } => Ok(Some(event)),
            RuntimeResponse::Ok => Ok(None),
            other => Err(unexpected_response("event", &other)),
        }
    }
}

pub fn runtime_socket_path(state_dir: &Path) -> PathBuf {
    state_dir.join(SOCKET_FILENAME)
}

pub fn run_runtime_server(config: RuntimeServerConfig) -> AppResult<String> {
    fs::create_dir_all(&config.state_dir)?;
    fs::set_permissions(&config.state_dir, fs::Permissions::from_mode(0o700))?;
    let socket_path = runtime_socket_path(&config.state_dir);
    match fs::symlink_metadata(&socket_path) {
        Ok(metadata) if metadata.file_type().is_socket() => {
            if let Ok(client) = RuntimeClient::connect(&config.state_dir)
                && client.get_status().is_ok()
            {
                return Err(AppError::with_exit_code(
                    format!(
                        "runtime socket is already served by a live runtime: {}",
                        socket_path.display()
                    ),
                    409,
                ));
            }
            fs::remove_file(&socket_path)?;
        }
        Ok(_) => {
            return Err(AppError::with_exit_code(
                format!(
                    "refusing to remove non-socket runtime path: {}",
                    socket_path.display()
                ),
                409,
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }

    let listener = UnixListener::bind(&socket_path).map_err(|error| {
        AppError::new(format!(
            "failed to bind runtime socket {}: {error}",
            socket_path.display()
        ))
    })?;
    fs::set_permissions(&socket_path, fs::Permissions::from_mode(0o600))?;
    listener.set_nonblocking(true)?;

    let run_id = begin_runtime_run(&config.state_dir)?;
    mark_stale_staging_uncertain(&config.state_dir, &run_id)?;
    let shared = Arc::new(Mutex::new(RuntimeShared::new(&socket_path, run_id.clone())));
    let stop = Arc::new(AtomicBool::new(false));
    let graceful_stop = Arc::new(AtomicBool::new(false));
    let mutation_gate = MutationGate::new();
    let shutdown_result = Arc::new((
        Mutex::new(ShutdownState {
            result: None,
            acknowledged: false,
        }),
        Condvar::new(),
    ));
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
                let graceful_stop = Arc::clone(&graceful_stop);
                let shutdown_result = Arc::clone(&shutdown_result);
                let mutation_gate = mutation_gate.clone();
                thread::spawn(move || {
                    if let Err(error) = handle_client(
                        stream,
                        shared,
                        config,
                        Arc::clone(&stop),
                        graceful_stop,
                        shutdown_result,
                        mutation_gate,
                    ) {
                        eprintln!("warning: runtime client failed: {error}");
                    }
                });
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(100));
            }
            Err(error) => {
                stop.store(true, Ordering::SeqCst);
                mutation_gate.begin_stop();
                let _ = observer.join();
                mutation_gate.wait_for_drain();
                let message = format!("runtime socket accept failed: {error}");
                if graceful_stop.load(Ordering::SeqCst) {
                    let (shutdown, ready) = &*shutdown_result;
                    let mut shutdown = shutdown.lock().unwrap();
                    shutdown.result = Some(Err(message.clone()));
                    ready.notify_all();
                    while !shutdown.acknowledged {
                        shutdown = ready.wait(shutdown).unwrap();
                    }
                }
                let _ = fs::remove_file(&socket_path);
                return Err(AppError::new(message));
            }
        }
    }

    let observer_result = observer
        .join()
        .map_err(|_| AppError::new("runtime observer thread panicked"));
    mutation_gate.wait_for_drain();
    let finalization = if graceful_stop.load(Ordering::SeqCst) {
        observer_result.and_then(|_| finish_runtime_run_clean(&config.state_dir, &run_id))
    } else {
        observer_result
    };
    if graceful_stop.load(Ordering::SeqCst) {
        let (shutdown, ready) = &*shutdown_result;
        let mut shutdown = shutdown.lock().unwrap();
        shutdown.result = Some(
            finalization
                .as_ref()
                .map(|_| ())
                .map_err(ToString::to_string),
        );
        ready.notify_all();
        while !shutdown.acknowledged {
            shutdown = ready.wait(shutdown).unwrap();
        }
    }
    let _ = fs::remove_file(&socket_path);
    finalization.map(|_| String::new())
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
    in_flight_recoveries: HashSet<String>,
    run_id: String,
}

impl RuntimeShared {
    fn new(socket_path: &Path, run_id: String) -> Self {
        Self {
            socket_path: socket_path.display().to_string(),
            boot_time_unix_ms: now_unix_ms(),
            tmux_socket_path: current_tmux_socket_path(),
            next_revision: 0,
            panes: BTreeMap::new(),
            subscribers: Vec::new(),
            in_flight_actions: HashSet::new(),
            in_flight_recoveries: HashSet::new(),
            run_id,
        }
    }

    fn status(&self) -> RuntimeStatus {
        RuntimeStatus {
            protocol_version: RUNTIME_PROTOCOL_VERSION,
            schema_version: RUNTIME_SCHEMA_VERSION,
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
    /// `Some(raw_source)` immediately after a successful YOLO approve.
    /// `maybe_run_yolo_action` skips the next call if the current snapshot's
    /// `raw_source` still matches this — that means the agy UI has not yet
    /// redrawn so re-firing `Enter` would land on whatever the post-approve
    /// frame is showing (potentially a downstream prompt). Cleared on the
    /// first reconcile pass whose `raw_source` differs.
    post_approve_guard_raw_source: Option<String>,
}

#[derive(Debug)]
enum ObserverMessage {
    ControlLine(String),
    ControlClosed(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ControlLineEffect {
    None,
    Reconcile,
}

fn handle_client(
    stream: UnixStream,
    shared: Arc<Mutex<RuntimeShared>>,
    config: RuntimeServerConfig,
    stop: Arc<AtomicBool>,
    graceful_stop: Arc<AtomicBool>,
    shutdown_result: Arc<(Mutex<ShutdownState>, Condvar)>,
    mutation_gate: MutationGate,
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
            let _mutation = mutation_gate.acquire()?;
            let updated =
                handle_set_yolo(&config.state_dir, &shared, pane_id, workspace, all, enabled)?;
            write_message(&mut stream, &RuntimeResponse::YoloUpdated { updated })
        }
        RuntimeRequest::RunAction {
            pane_id,
            session_name,
            action,
        } => {
            let _mutation = mutation_gate.acquire()?;
            let result =
                handle_run_action(&config, &shared, &pane_id, session_name.as_deref(), &action)?;
            write_message(&mut stream, &RuntimeResponse::Action { result })
        }
        RuntimeRequest::SubmitPrompt {
            session_name,
            pane_id,
            workspace,
            prompt_text,
            submit_delay_ms,
        } => {
            let _mutation = mutation_gate.acquire()?;
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
        RuntimeRequest::ListRecoveries => {
            match recovery_offers(&config.state_dir, &runtime_tmux_client()) {
                Ok(recoveries) => {
                    write_message(&mut stream, &RuntimeResponse::Recoveries { recoveries })
                }
                Err(error) => write_runtime_error(&mut stream, &error),
            }
        }
        RuntimeRequest::StageRecovery { recovery_id } => {
            let _mutation = match mutation_gate.acquire() {
                Ok(permit) => permit,
                Err(error) => return write_runtime_error(&mut stream, &error),
            };
            match handle_stage_recovery(&config, &shared, &recovery_id) {
                Ok(recovery) => {
                    write_message(&mut stream, &RuntimeResponse::RecoveryStaged { recovery })
                }
                Err(error) => write_runtime_error(&mut stream, &error),
            }
        }
        RuntimeRequest::DismissRecovery { recovery_id } => {
            let _mutation = match mutation_gate.acquire() {
                Ok(permit) => permit,
                Err(error) => return write_runtime_error(&mut stream, &error),
            };
            let result = RecoveryGuard::acquire(&shared, &recovery_id)
                .and_then(|_guard| dismiss_recovery(&config.state_dir, &recovery_id));
            match result {
                Ok(_) => write_message(
                    &mut stream,
                    &RuntimeResponse::RecoveryDismissed { recovery_id },
                ),
                Err(error) => write_runtime_error(&mut stream, &error),
            }
        }
        RuntimeRequest::Stop => {
            if !mutation_gate.begin_stop() || graceful_stop.swap(true, Ordering::SeqCst) {
                return write_message(
                    &mut stream,
                    &RuntimeResponse::Error {
                        message: "runtime stop is already in progress".to_string(),
                        exit_code: 409,
                    },
                );
            }
            stop.store(true, Ordering::SeqCst);
            let (shutdown, ready) = &*shutdown_result;
            let result = {
                let mut shutdown = shutdown.lock().unwrap();
                while shutdown.result.is_none() {
                    shutdown = ready.wait(shutdown).unwrap();
                }
                shutdown
                    .result
                    .as_ref()
                    .expect("shutdown result should be set")
                    .clone()
            };
            let response = match result {
                Ok(()) => write_message(&mut stream, &RuntimeResponse::Ok),
                Err(message) => write_message(
                    &mut stream,
                    &RuntimeResponse::Error {
                        message,
                        exit_code: 1,
                    },
                ),
            };
            {
                let mut shutdown = shutdown.lock().unwrap();
                shutdown.acknowledged = true;
                ready.notify_all();
            }
            response
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
    match (pane_id.is_some(), all) {
        (true, true) | (false, false) => {
            return Err(AppError::with_exit_code(
                "set-yolo requires exactly one target: pane_id or all",
                400,
            ));
        }
        _ => {}
    }

    let client = runtime_tmux_client();
    let mut updated = Vec::new();
    if let Some(pane_id) = pane_id {
        let pane = client
            .pane_by_target(&pane_id)?
            .ok_or_else(|| AppError::new(format!("pane not found: {pane_id}")))?;
        let inspected = inspect_pane(&client, &pane.pane_id, ACTION_GUARD_HISTORY_LINES)?;
        ensure_pane_owned_by_yolo_provider(&pane, &inspected.classification)?;
        let workspace = resolve_workspace_for_pane(state_dir, &pane, workspace.as_deref())?;
        if enabled {
            write_yolo_record(state_dir, &workspace.id, &pane)?;
        } else {
            let _ = disable_yolo_record(state_dir, &pane.pane_id)?;
        }
        update_yolo_runtime_state(shared, &pane.pane_id, enabled, None);
        updated.push(pane.pane_id.clone());
        let summary = if enabled {
            "yolo-enabled"
        } else {
            "yolo-disabled"
        };
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
                    .filter(is_yolo_candidate_pane)
                    .filter_map(|pane| {
                        inspect_pane(&client, &pane.pane_id, ACTION_GUARD_HISTORY_LINES)
                            .ok()
                            .and_then(|inspected| {
                                ensure_pane_owned_by_yolo_provider(&pane, &inspected.classification)
                                    .ok()
                                    .map(|_| pane)
                            })
                    })
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
                .filter(is_yolo_candidate_pane)
                .filter_map(|pane| {
                    inspect_pane(&client, &pane.pane_id, ACTION_GUARD_HISTORY_LINES)
                        .ok()
                        .and_then(|inspected| {
                            ensure_pane_owned_by_yolo_provider(&pane, &inspected.classification)
                                .ok()
                                .map(|_| pane)
                        })
                })
                .map(|pane| {
                    let workspace =
                        resolve_workspace_for_path(state_dir, Path::new(&pane.current_path))?;
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
        for pane_id in &pane_ids {
            update_yolo_runtime_state(shared, pane_id, enabled, None);
        }
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
    ensure_pane_owned_by_automatable_provider(&current, &snapshot.classification)?;
    emit_shared_event(
        shared,
        Some(pane_id.to_string()),
        Some(snapshot.workspace_id.clone()),
        "action-requested",
        format!("{} {}", pane_id, action),
        None,
    );

    let result = execute_runtime_action(&client, config, &current, action, session_name)?;
    let refreshed = refresh_action_snapshot(
        &client,
        shared,
        pane_id,
        session_name,
        Some(result.executed.clone()),
    )
    .ok();
    emit_shared_event(
        shared,
        Some(pane_id.to_string()),
        Some(
            refreshed
                .as_ref()
                .map(|snapshot| snapshot.workspace_id.clone())
                .unwrap_or(snapshot.workspace_id.clone()),
        ),
        "action-executed",
        format!("{} {}", pane_id, action),
        refreshed,
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
    let snapshot = get_snapshot(shared, pane_id, Some(session_name))?;
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
        detail: Some(format!(
            "workspace={} delay_ms={}",
            submitted.workspace_id, submitted.delay_ms
        )),
    };
    let refreshed = refresh_action_snapshot(
        &client,
        shared,
        pane_id,
        Some(session_name),
        Some(result.executed.clone()),
    )
    .ok();
    emit_shared_event(
        shared,
        Some(pane_id.to_string()),
        Some(
            refreshed
                .as_ref()
                .map(|snapshot| snapshot.workspace_id.clone())
                .unwrap_or(snapshot.workspace_id.clone()),
        ),
        "action-executed",
        format!("{} submit-prompt", pane_id),
        refreshed,
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
            ensure_workflow_state(
                GuardedWorkflow::ApprovePermission,
                &classification.classification,
            )?;
            execute_classified_workflow(
                client,
                &pane.pane_id,
                GuardedWorkflow::ApprovePermission,
                &classification.classification,
            )?;
            Some(
                inspect_pane(client, &pane.pane_id, ACTION_GUARD_HISTORY_LINES)?
                    .classification
                    .state,
            )
        }
        "reject-permission" => {
            ensure_workflow_state(
                GuardedWorkflow::RejectPermission,
                &classification.classification,
            )?;
            execute_classified_workflow(
                client,
                &pane.pane_id,
                GuardedWorkflow::RejectPermission,
                &classification.classification,
            )?;
            Some(
                inspect_pane(client, &pane.pane_id, ACTION_GUARD_HISTORY_LINES)?
                    .classification
                    .state,
            )
        }
        "dismiss-survey" => {
            ensure_workflow_state(
                GuardedWorkflow::DismissSurvey,
                &classification.classification,
            )?;
            execute_classified_workflow(
                client,
                &pane.pane_id,
                GuardedWorkflow::DismissSurvey,
                &classification.classification,
            )?;
            Some(
                inspect_pane(client, &pane.pane_id, ACTION_GUARD_HISTORY_LINES)?
                    .classification
                    .state,
            )
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
        "continue-session" => Some(
            continue_from_classification(client, pane, &classification.classification)?
                .after
                .state,
        ),
        "auto-unstick" => Some(auto_unstick(client, pane)?.after.state),
        "enter" => {
            client.send_keys(&pane.pane_id, &["Enter"])?;
            None
        }
        action if action.starts_with("select-numbered-option:") => {
            let (current, target) = parse_numbered_option_action(action)?;
            let before_navigation =
                inspect_pane(client, &pane.pane_id, ACTION_GUARD_HISTORY_LINES)?;
            ensure_same_numbered_option_dialog(
                &classification.classification,
                &before_navigation.classification,
            )?;
            if target > current {
                for _ in 0..(target - current) {
                    execute_automation_action(
                        client,
                        &pane.pane_id,
                        AutomationAction::ConfirmNext,
                    )?;
                }
            } else if current > target {
                for _ in 0..(current - target) {
                    execute_automation_action(
                        client,
                        &pane.pane_id,
                        AutomationAction::ConfirmPrevious,
                    )?;
                }
            }
            let before_enter = inspect_pane(client, &pane.pane_id, ACTION_GUARD_HISTORY_LINES)?;
            ensure_same_numbered_option_dialog(
                &classification.classification,
                &before_enter.classification,
            )?;
            client.send_keys(&pane.pane_id, &["Enter"])?;
            Some(
                inspect_pane(client, &pane.pane_id, ACTION_GUARD_HISTORY_LINES)?
                    .classification
                    .state,
            )
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

fn parse_numbered_option_action(action: &str) -> AppResult<(usize, usize)> {
    let value = action
        .strip_prefix("select-numbered-option:")
        .ok_or_else(|| AppError::new(format!("invalid numbered option action: {action}")))?;
    let (current, target) = value
        .split_once(':')
        .ok_or_else(|| AppError::new(format!("invalid numbered option action: {action}")))?;
    let current = current
        .parse::<usize>()
        .map_err(|_| AppError::new(format!("invalid numbered option action: {action}")))?;
    let target = target
        .parse::<usize>()
        .map_err(|_| AppError::new(format!("invalid numbered option action: {action}")))?;
    Ok((current, target))
}

fn ensure_same_numbered_option_dialog(
    expected: &Classification,
    current: &Classification,
) -> AppResult<()> {
    if expected.state != current.state {
        return Err(AppError::with_exit_code(
            format!(
                "refusing numbered option selection after state changed from {} to {}",
                expected.state.as_str(),
                current.state.as_str()
            ),
            409,
        ));
    }
    Ok(())
}

fn recovery_offers(state_dir: &Path, client: &TmuxClient) -> AppResult<Vec<RuntimeRecoveryOffer>> {
    let inventory = client.inventory()?;
    recovery_offers_for_inventory(state_dir, &inventory)
}

fn recovery_offers_for_inventory(
    state_dir: &Path,
    inventory: &TmuxInventory,
) -> AppResult<Vec<RuntimeRecoveryOffer>> {
    let mut recoveries = list_nonterminal_recoveries(state_dir)?;
    let mut stale_targets = BTreeSet::new();
    for recovery in &mut recoveries {
        let Some(target) = recovery.target.as_mut() else {
            continue;
        };
        if let Some(current) = verified_persisted_target(inventory, target) {
            target.pane = current.clone();
        } else {
            stale_targets.insert(recovery.id.clone());
            recovery.target = None;
        }
    }
    let mut matched = match_recoveries(&recoveries, inventory);
    Ok(recoveries
        .iter()
        .filter_map(|record| {
            matched.remove(&record.id).map(|mut result| {
                if stale_targets.contains(&record.id) {
                    result.target = None;
                    result.disabled_reason = Some(
                        "persisted recovery target is not verified in the current stable tmux inventory; navigation is disabled"
                            .to_string(),
                    );
                }
                offer_from_record(record, result)
            })
        })
        .collect())
}

fn verified_persisted_target<'a>(
    inventory: &'a TmuxInventory,
    target: &RecoveryTarget,
) -> Option<&'a TmuxPane> {
    if target.server != inventory.server {
        return None;
    }
    inventory.panes.iter().find(|pane| {
        pane.pane_id == target.pane.pane_id
            && pane.session_id == target.pane.session_id
            && pane.window_id == target.pane.window_id
            && pane.session_name == target.pane.session_name
            && pane.window_index == target.pane.window_index
            && pane.window_name == target.pane.window_name
            && pane.pane_index == target.pane.pane_index
            && pane.current_path == target.pane.current_path
    })
}

fn handle_stage_recovery(
    config: &RuntimeServerConfig,
    shared: &Arc<Mutex<RuntimeShared>>,
    recovery_id: &str,
) -> AppResult<RuntimeRecoveryOffer> {
    let _recovery_guard = RecoveryGuard::acquire(shared, recovery_id)?;
    let client = runtime_tmux_client();
    let run_id = shared.lock().unwrap().run_id.clone();
    let store = SqliteRecoveryStageStore {
        state_dir: &config.state_dir,
    };
    let hooks = RuntimeRecoveryStageHooks { shared };
    let staged = stage_recovery_with(&client, &store, &hooks, &run_id, recovery_id)?;
    Ok(offer_from_record(
        &staged,
        crate::recovery::RecoveryMatch {
            state: RecoveryMatchState::NotStageable,
            target: staged.target.clone(),
            disabled_reason: Some("command was staged without Enter".to_string()),
        },
    ))
}

trait RecoveryStageTransport {
    fn inventory(&self) -> AppResult<TmuxInventory>;
    fn paste_text(&self, pane_id: &str, text: &str) -> AppResult<()>;
}

impl RecoveryStageTransport for TmuxClient {
    fn inventory(&self) -> AppResult<TmuxInventory> {
        TmuxClient::inventory(self)
    }

    fn paste_text(&self, pane_id: &str, text: &str) -> AppResult<()> {
        TmuxClient::paste_text(self, pane_id, text)
    }
}

trait RecoveryStageStore {
    fn list(&self) -> AppResult<Vec<crate::recovery::RecoveryRecord>>;
    fn claim(
        &self,
        recovery_id: &str,
        run_id: &str,
        token: &str,
        target: &RecoveryTarget,
        command: &str,
    ) -> AppResult<()>;
    fn release(&self, recovery_id: &str, token: &str) -> AppResult<bool>;
    fn mark_staged(&self, recovery_id: &str, token: &str) -> AppResult<bool>;
    fn mark_uncertain(&self, recovery_id: &str, token: &str) -> AppResult<bool>;
    fn load(&self, recovery_id: &str) -> AppResult<Option<crate::recovery::RecoveryRecord>>;
}

struct SqliteRecoveryStageStore<'a> {
    state_dir: &'a Path,
}

impl RecoveryStageStore for SqliteRecoveryStageStore<'_> {
    fn list(&self) -> AppResult<Vec<crate::recovery::RecoveryRecord>> {
        list_nonterminal_recoveries(self.state_dir)
    }

    fn claim(
        &self,
        recovery_id: &str,
        run_id: &str,
        token: &str,
        target: &RecoveryTarget,
        command: &str,
    ) -> AppResult<()> {
        claim_recovery_for_staging(self.state_dir, recovery_id, run_id, token, target, command)
    }

    fn release(&self, recovery_id: &str, token: &str) -> AppResult<bool> {
        release_known_failed_staging_claim(self.state_dir, recovery_id, token)
    }

    fn mark_staged(&self, recovery_id: &str, token: &str) -> AppResult<bool> {
        mark_recovery_staged(self.state_dir, recovery_id, token)
    }

    fn mark_uncertain(&self, recovery_id: &str, token: &str) -> AppResult<bool> {
        mark_recovery_uncertain(self.state_dir, recovery_id, token)
    }

    fn load(&self, recovery_id: &str) -> AppResult<Option<crate::recovery::RecoveryRecord>> {
        load_recovery(self.state_dir, recovery_id)
    }
}

trait RecoveryStageHooks {
    type TargetGuard;

    fn target_selected(&self, target: &RecoveryTarget) -> AppResult<Self::TargetGuard>;
    fn before_claim(&self) {}
    fn before_paste(&self) {}
}

struct RuntimeRecoveryStageHooks<'a> {
    shared: &'a Arc<Mutex<RuntimeShared>>,
}

impl RecoveryStageHooks for RuntimeRecoveryStageHooks<'_> {
    type TargetGuard = ActionGuard;

    fn target_selected(&self, target: &RecoveryTarget) -> AppResult<Self::TargetGuard> {
        ActionGuard::acquire(self.shared, &target.pane.pane_id)
    }
}

fn stage_recovery_with<T, S, H>(
    transport: &T,
    store: &S,
    hooks: &H,
    run_id: &str,
    recovery_id: &str,
) -> AppResult<crate::recovery::RecoveryRecord>
where
    T: RecoveryStageTransport,
    S: RecoveryStageStore,
    H: RecoveryStageHooks,
{
    let first_inventory = transport.inventory()?;
    let first_records = store.list()?;
    let first_matches = match_recoveries(&first_records, &first_inventory);
    let record = first_records
        .iter()
        .find(|record| record.id == recovery_id)
        .ok_or_else(|| {
            AppError::with_exit_code(format!("recovery not found: {recovery_id}"), 404)
        })?;
    if record.lifecycle != RecoveryLifecycle::Crashed {
        return Err(AppError::with_exit_code(
            "recovery is not available for staging",
            409,
        ));
    }
    let first_match = first_matches
        .get(recovery_id)
        .ok_or_else(|| AppError::with_exit_code("recovery has no current matching result", 409))?;
    if first_match.state != RecoveryMatchState::Ready {
        return Err(AppError::with_exit_code(
            first_match
                .disabled_reason
                .clone()
                .unwrap_or_else(|| "recovery target is not ready".to_string()),
            409,
        ));
    }
    let target = first_match
        .target
        .clone()
        .ok_or_else(|| AppError::with_exit_code("recovery has no unique target", 409))?;
    let _target_guard = hooks.target_selected(&target)?;

    let second_inventory = transport.inventory()?;
    let second_records = store.list()?;
    let second_matches = match_recoveries(&second_records, &second_inventory);
    let second_target = second_matches
        .get(recovery_id)
        .filter(|matched| matched.state == RecoveryMatchState::Ready)
        .and_then(|matched| matched.target.as_ref())
        .ok_or_else(|| AppError::with_exit_code("recovery target changed before claim", 409))?;
    if second_target != &target {
        return Err(AppError::with_exit_code(
            "recovery target identity changed before claim",
            409,
        ));
    }

    let command = build_recovery_command(
        &record.provider,
        &record.original.cwd,
        &record.provider_session_id,
    )?;
    let token = Uuid::now_v7().to_string();
    hooks.before_claim();
    store.claim(recovery_id, run_id, &token, &target, &command)?;

    let current_inventory = match transport.inventory() {
        Ok(inventory) => inventory,
        Err(error) => {
            let _ = store.release(recovery_id, &token);
            return Err(error);
        }
    };
    if !inventory_contains_exact_target(&current_inventory, &target) {
        let _ = store.release(recovery_id, &token);
        return Err(AppError::with_exit_code(
            "recovery target changed immediately before paste",
            409,
        ));
    }
    hooks.before_paste();
    if let Err(error) = transport.paste_text(&target.pane.pane_id, &command) {
        let _ = store.release(recovery_id, &token);
        return Err(error);
    }
    match store.mark_staged(recovery_id, &token) {
        Ok(true) => {}
        Ok(false) => {
            let _ = store.mark_uncertain(recovery_id, &token);
            return Err(AppError::new(
                "command was pasted but recovery staging could not be confirmed",
            ));
        }
        Err(error) => {
            let _ = store.mark_uncertain(recovery_id, &token);
            return Err(AppError::new(format!(
                "command was pasted but recovery staging could not be confirmed: {error}"
            )));
        }
    }
    store
        .load(recovery_id)?
        .ok_or_else(|| AppError::new("staged recovery disappeared"))
}

fn inventory_contains_exact_target(inventory: &TmuxInventory, target: &RecoveryTarget) -> bool {
    inventory.server == target.server && inventory.panes.iter().any(|pane| pane == &target.pane)
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
            let mut force_reconcile = false;
            if let Ok(panes) = client.list_panes() {
                for session_name in panes
                    .into_iter()
                    .filter(is_runtime_discovery_candidate)
                    .map(|pane| pane.session_name)
                    .collect::<BTreeSet<_>>()
                {
                    if observed_sessions.insert(session_name.clone()) {
                        let tx = tx.clone();
                        let client = client.clone();
                        let stop = Arc::clone(&stop);
                        thread::spawn(move || {
                            observe_session_lines(client, &session_name, tx, stop)
                        });
                    }
                }
            }

            while let Ok(message) = rx.try_recv() {
                match message {
                    ObserverMessage::ControlLine(line) => {
                        if handle_control_line_shared(&shared, &line)
                            == ControlLineEffect::Reconcile
                        {
                            force_reconcile = true;
                        }
                    }
                    ObserverMessage::ControlClosed(session_name) => {
                        observed_sessions.remove(&session_name);
                        force_reconcile = true;
                    }
                }
            }

            if emit_dirty_stream_events(&shared, Duration::from_millis(STREAM_DEBOUNCE_MS)) {
                last_reconcile = Instant::now();
            }

            if force_reconcile
                || last_reconcile.elapsed() >= Duration::from_millis(config.reconcile_ms)
            {
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

fn handle_control_line_shared(shared: &Arc<Mutex<RuntimeShared>>, line: &str) -> ControlLineEffect {
    let mut shared = shared.lock().unwrap();
    match parse_control_line(line) {
        Some(ControlEvent::Output { pane_id, payload })
        | Some(ControlEvent::ExtendedOutput {
            pane_id, payload, ..
        }) => {
            if let Some(tracked) = shared.panes.get_mut(&pane_id) {
                let decoded = decode_tmux_escaped(&payload);
                let model = tracked
                    .screen_model
                    .get_or_insert_with(|| ScreenModel::new(MAX_LIVE_EXCERPT_LINES));
                model.ingest(&decoded);
                tracked.snapshot.live_excerpt =
                    trim_visible_block(&model.render(), MAX_LIVE_EXCERPT_LINES);
                tracked.dirty = true;
                tracked.last_stream_activity = Some(Instant::now());
            }
            ControlLineEffect::None
        }
        Some(ControlEvent::Notification(message)) => {
            if notification_requires_reconcile(&message) {
                ControlLineEffect::Reconcile
            } else {
                ControlLineEffect::None
            }
        }
        None => ControlLineEffect::None,
    }
}

fn emit_dirty_stream_events(shared: &Arc<Mutex<RuntimeShared>>, debounce: Duration) -> bool {
    let now = Instant::now();
    let events = {
        let mut shared = shared.lock().unwrap();
        let pane_ids = shared
            .panes
            .iter()
            .filter(|(_, tracked)| {
                tracked.dirty
                    && tracked
                        .last_stream_activity
                        .is_some_and(|last_activity| now.duration_since(last_activity) >= debounce)
            })
            .map(|(pane_id, _)| pane_id.clone())
            .collect::<Vec<_>>();

        pane_ids
            .into_iter()
            .filter_map(|pane_id| {
                let revision = next_revision(&mut shared);
                let tracked = shared.panes.get_mut(&pane_id)?;
                tracked.dirty = false;
                tracked.snapshot.revision = revision;
                tracked.snapshot.updated_at_unix_ms = now_unix_ms();
                Some(RuntimeEvent {
                    revision,
                    timestamp_unix_ms: tracked.snapshot.updated_at_unix_ms,
                    kind: String::from("pane-snapshot-updated"),
                    pane_id: Some(pane_id),
                    workspace_id: Some(tracked.snapshot.workspace_id.clone()),
                    summary: String::from("stream-output"),
                    snapshot: Some(tracked.snapshot.clone()),
                })
            })
            .collect::<Vec<_>>()
    };

    if events.is_empty() {
        return false;
    }

    let mut shared = shared.lock().unwrap();
    for event in events {
        shared
            .subscribers
            .retain(|sender| sender.send(event.clone()).is_ok());
    }
    true
}

fn reconcile_all(
    client: &TmuxClient,
    config: &RuntimeServerConfig,
    shared: &Arc<Mutex<RuntimeShared>>,
) -> AppResult<()> {
    let inventory = client.inventory()?;
    let mut current_panes = Vec::new();
    for pane in inventory
        .panes
        .iter()
        .cloned()
        .filter(is_runtime_discovery_candidate)
    {
        let inspected = if pane.current_command.eq_ignore_ascii_case("node") {
            let inspected = inspect_pane(client, &pane.pane_id, config.history_lines)?;
            if !is_verified_runtime_discovery_candidate(&pane, &inspected.classification) {
                continue;
            }
            Some(inspected)
        } else {
            None
        };
        let workspace =
            resolve_workspace_for_path(&config.state_dir, Path::new(&pane.current_path))?;
        let inspected = match inspected {
            Some(inspected) => inspected,
            None => inspect_pane(client, &pane.pane_id, config.history_lines)?,
        };
        let claude_session_id = if pane.current_command.eq_ignore_ascii_case("claude") {
            resolve_live_claude_session_id(&pane)?
        } else {
            None
        };
        let codex_session_id = if is_yolo_candidate_pane(&pane) {
            resolve_codex_session_id_for_pane(&pane)?
        } else {
            None
        };
        current_panes.push((
            pane,
            inspected,
            workspace,
            claude_session_id,
            codex_session_id,
        ));
    }

    let run_id = shared.lock().unwrap().run_id.clone();
    // Evidence changes begin only after the complete raw inventory and all
    // provider/classifier lookups for this pass have succeeded.
    let verified_claude = current_panes
        .iter()
        .filter_map(|(pane, _, workspace, claude_session_id, _)| {
            let claude_session_id = claude_session_id
                .as_ref()
                .filter(|session_id| Uuid::parse_str(session_id).is_ok())?;
            Some(VerifiedClaudeRecoveryEvidence {
                workspace_id: workspace.id.clone(),
                pane: pane.clone(),
                claude_session_id: claude_session_id.clone(),
            })
        })
        .collect::<Vec<_>>();
    apply_recovery_inventory_evidence(&config.state_dir, &run_id, &inventory, &verified_claude)?;

    let current_ids = current_panes
        .iter()
        .map(|(pane, _, _, _, _)| pane.pane_id.clone())
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
            shared
                .panes
                .remove(&pane_id)
                .map(|tracked| tracked.snapshot)
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

    for (pane, inspected, workspace, claude_session_id, codex_session_id) in current_panes {
        let runtime_durations = sync_tmux_runtime_state(
            &config.state_dir,
            &workspace.id,
            &pane,
            inspected.classification.state.as_str(),
            is_waiting_state(inspected.classification.state),
            inspected.classification.state == SessionState::BusyResponding,
            claude_session_id.as_deref().or(codex_session_id.as_deref()),
        )?;
        let desired_yolo_enabled = matches!(read_yolo_record(&config.state_dir, &pane.pane_id)?, Some(record) if record.enabled);

        let live_excerpt = {
            let shared = shared.lock().unwrap();
            shared
                .panes
                .get(&pane.pane_id)
                .map(|tracked| tracked.snapshot.live_excerpt.clone())
                .filter(|excerpt| !excerpt.is_empty())
                .unwrap_or_else(|| {
                    trim_visible_block(&inspected.focused_source, MAX_LIVE_EXCERPT_LINES)
                })
        };

        let updated_at_unix_ms = now_unix_ms();
        let event_snapshot;
        let mut discovered = false;
        let mut state_changed = None;
        {
            let mut shared = shared.lock().unwrap();
            let previous_stop_reason = shared
                .panes
                .get(&pane.pane_id)
                .and_then(|tracked| tracked.snapshot.last_stop_reason.clone());
            let actual_yolo_enabled = desired_yolo_enabled && previous_stop_reason.is_none();
            let revision = next_revision(&mut shared);
            let snapshot = RuntimePaneSnapshot {
                pane: pane.clone(),
                classification: inspected.classification.clone(),
                focused_source: inspected.focused_source.clone(),
                raw_source: inspected.raw_source.clone(),
                live_excerpt,
                wait_duration_ms: runtime_durations
                    .wait_duration
                    .map(|duration| duration.as_millis() as u64),
                claude_session_id,
                workspace_id: workspace.id.clone(),
                workspace_root: workspace.workspace_root.clone(),
                desired_yolo_enabled,
                actual_yolo_enabled,
                last_stop_reason: previous_stop_reason,
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
                        post_approve_guard_raw_source: None,
                    });
                }
                std::collections::btree_map::Entry::Occupied(mut entry) => {
                    let tracked = entry.get_mut();
                    if tracked.snapshot.classification.state != inspected.classification.state {
                        state_changed = Some(inspected.classification.state);
                    }
                    // Clear the post-approve guard once the frame has changed
                    // — the agy UI has redrawn, so it is safe to evaluate the
                    // next prompt on its own merits.
                    if let Some(prev_raw) = tracked.post_approve_guard_raw_source.as_deref() {
                        if prev_raw != inspected.raw_source.as_str() {
                            tracked.post_approve_guard_raw_source = None;
                        }
                    }
                    tracked.pane = pane.clone();
                    tracked.workspace = workspace;
                    tracked.signature = signature;
                    tracked.snapshot = snapshot.clone();
                    if !tracked.screen_model_seeded && !inspected.focused_source.is_empty() {
                        let mut model =
                            ScreenModel::new(config.history_lines.max(MAX_LIVE_EXCERPT_LINES));
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
                format!(
                    "{} {}",
                    snapshot.pane.pane_id,
                    snapshot.classification.state.as_str()
                ),
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
    if snapshot.last_stop_reason.is_some() {
        return Ok(());
    }

    // Post-approve guard: if the previous reconcile pass already approved this
    // exact frame (same `raw_source`), skip — the agy UI has not yet redrawn
    // and re-firing `Enter` would land on whatever the post-approve frame is
    // showing. The guard is cleared by `reconcile_all` once `raw_source`
    // changes.
    {
        let locked = shared.lock().unwrap();
        if let Some(tracked) = locked.panes.get(&snapshot.pane.pane_id) {
            if tracked
                .post_approve_guard_raw_source
                .as_deref()
                .is_some_and(|prev| prev == snapshot.raw_source.as_str())
            {
                return Ok(());
            }
        }
    }

    let Some(record) = read_yolo_record(&config.state_dir, &snapshot.pane.pane_id)? else {
        return Ok(());
    };
    if !record.matches_pane(&snapshot.pane) {
        let _ = disable_yolo_record(&config.state_dir, &snapshot.pane.pane_id)?;
        update_yolo_runtime_state(
            shared,
            &snapshot.pane.pane_id,
            false,
            Some("identity-changed"),
        );
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
    if workflow == GuardedWorkflow::ApprovePermission
        && !is_yolo_safe_to_approve(&inspected, &snapshot.pane)
    {
        return Ok(());
    }

    // Resolve the `YoloAction` once so both `action-requested` and
    // `action-executed` payloads carry it in their summary. This lets log
    // consumers distinguish a Claude approve from an agy command-permission
    // approve even though both share `GuardedWorkflow::ApprovePermission`.
    let yolo_action = yolo_action_for_state(snapshot.classification.state);
    let action_summary = format!(
        "yolo {} {} {}",
        snapshot.pane.pane_id,
        workflow.as_str(),
        yolo_action.as_str()
    );

    emit_shared_event(
        shared,
        Some(snapshot.pane.pane_id.clone()),
        Some(snapshot.workspace_id.clone()),
        "action-requested",
        action_summary.clone(),
        Some(snapshot.clone()),
    );
    match execute_classified_workflow(
        client,
        &snapshot.pane.pane_id,
        workflow,
        &snapshot.classification,
    ) {
        Ok(executed) => {
            let after = inspect_pane(client, &snapshot.pane.pane_id, ACTION_GUARD_HISTORY_LINES)?;
            let mut next_snapshot = snapshot.clone();
            next_snapshot.classification = after.classification.clone();
            next_snapshot.focused_source = after.focused_source.clone();
            next_snapshot.raw_source = after.raw_source.clone();
            next_snapshot.last_action = Some(executed);
            // Write the refreshed snapshot back into `shared.panes` and arm the
            // post-approve guard with the pre-approve `raw_source`. The next
            // reconcile pass will skip re-firing if `raw_source` is still
            // unchanged (UI has not redrawn yet) and clear the guard once it
            // diverges. The `ActionGuard` released on `Drop` here only covers
            // concurrent calls within the same reconcile.
            {
                let mut locked = shared.lock().unwrap();
                if let Some(tracked) = locked.panes.get_mut(&snapshot.pane.pane_id) {
                    tracked.snapshot.classification = after.classification.clone();
                    tracked.snapshot.focused_source = after.focused_source.clone();
                    tracked.snapshot.raw_source = after.raw_source.clone();
                    tracked.snapshot.last_action = next_snapshot.last_action.clone();
                    tracked.signature = SnapshotSignature {
                        pane: tracked.pane.clone(),
                        classification: after.classification,
                    };
                    tracked.post_approve_guard_raw_source = Some(snapshot.raw_source.clone());
                }
            }
            emit_shared_event(
                shared,
                Some(snapshot.pane.pane_id.clone()),
                Some(snapshot.workspace_id.clone()),
                "action-executed",
                action_summary.clone(),
                Some(next_snapshot),
            );
        }
        Err(error) => {
            update_yolo_runtime_state(shared, &snapshot.pane.pane_id, false, Some("action-failed"));
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
        // Agy command-permission auto-approve. Mapped to `ApprovePermission`
        // because `raw_key_for_workflow` short-circuits this state in
        // `execute_classified_workflow` and emits raw `Enter`, exactly
        // mirroring the FolderTrustPrompt special case. The per-shape safety
        // gate runs in `is_yolo_safe_to_approve` above.
        SessionState::AgyCommandPermissionPrompt => Some(GuardedWorkflow::ApprovePermission),
        SessionState::SurveyPrompt => Some(GuardedWorkflow::DismissSurvey),
        _ => None,
    }
}

fn is_runtime_discovery_candidate(pane: &TmuxPane) -> bool {
    pane.current_command.eq_ignore_ascii_case("claude")
        || pane.current_command.eq_ignore_ascii_case("agy")
        || is_yolo_candidate_pane(pane)
}

fn is_verified_runtime_discovery_candidate(
    pane: &TmuxPane,
    classification: &Classification,
) -> bool {
    !pane.current_command.eq_ignore_ascii_case("node")
        || classification
            .signals
            .iter()
            .any(|signal| signal == SIGNAL_CODEX_KEYWORDS)
}

fn is_yolo_candidate_pane(pane: &TmuxPane) -> bool {
    pane.current_command.eq_ignore_ascii_case("codex")
        || pane.current_command.eq_ignore_ascii_case("agy")
        || (pane.current_command.eq_ignore_ascii_case("node")
            && !pane.pane_title.starts_with("OC | "))
}

fn update_yolo_runtime_state(
    shared: &Arc<Mutex<RuntimeShared>>,
    pane_id: &str,
    actual_yolo_enabled: bool,
    stop_reason: Option<&str>,
) {
    let mut shared = shared.lock().unwrap();
    if let Some(tracked) = shared.panes.get_mut(pane_id) {
        tracked.snapshot.actual_yolo_enabled = actual_yolo_enabled;
        tracked.snapshot.last_stop_reason = stop_reason.map(str::to_string);
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
        .filter(|snapshot| {
            target_pane
                .map(|target| snapshot.pane.pane_id == target)
                .unwrap_or(true)
        })
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

fn refresh_action_snapshot(
    client: &TmuxClient,
    shared: &Arc<Mutex<RuntimeShared>>,
    pane_id: &str,
    session_name: Option<&str>,
    last_action: Option<String>,
) -> AppResult<RuntimePaneSnapshot> {
    let inspected = inspect_pane(client, pane_id, ACTION_GUARD_HISTORY_LINES)?;
    let mut shared = shared.lock().unwrap();
    let revision = next_revision(&mut shared);
    let tracked = shared
        .panes
        .get_mut(pane_id)
        .ok_or_else(|| AppError::with_exit_code(format!("pane not found: {pane_id}"), 404))?;
    if !session_matches(&tracked.snapshot, session_name) {
        return Err(AppError::with_exit_code(
            format!("pane {} is not tracked for requested session", pane_id),
            404,
        ));
    }
    tracked.snapshot.classification = inspected.classification.clone();
    tracked.snapshot.focused_source = inspected.focused_source.clone();
    tracked.snapshot.raw_source = inspected.raw_source.clone();
    tracked.snapshot.live_excerpt =
        trim_visible_block(&inspected.focused_source, MAX_LIVE_EXCERPT_LINES);
    tracked.snapshot.last_action = last_action;
    tracked.snapshot.revision = revision;
    tracked.snapshot.updated_at_unix_ms = now_unix_ms();
    tracked.signature = SnapshotSignature {
        pane: tracked.pane.clone(),
        classification: inspected.classification,
    };
    Ok(tracked.snapshot.clone())
}

fn validate_action_target(
    client: &TmuxClient,
    snapshot: &RuntimePaneSnapshot,
) -> AppResult<TmuxPane> {
    let current = client.pane_by_id(&snapshot.pane.pane_id)?.ok_or_else(|| {
        AppError::with_exit_code(format!("pane not found: {}", snapshot.pane.pane_id), 404)
    })?;
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

struct RecoveryGuard {
    shared: Arc<Mutex<RuntimeShared>>,
    recovery_id: String,
}

impl RecoveryGuard {
    fn acquire(shared: &Arc<Mutex<RuntimeShared>>, recovery_id: &str) -> AppResult<Self> {
        let mut locked = shared.lock().unwrap();
        if !locked.in_flight_recoveries.insert(recovery_id.to_string()) {
            return Err(AppError::with_exit_code(
                format!("recovery {recovery_id} already has an in-flight action"),
                409,
            ));
        }
        Ok(Self {
            shared: Arc::clone(shared),
            recovery_id: recovery_id.to_string(),
        })
    }
}

impl Drop for RecoveryGuard {
    fn drop(&mut self) {
        self.shared
            .lock()
            .unwrap()
            .in_flight_recoveries
            .remove(&self.recovery_id);
    }
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
        return Err(AppError::new(
            "runtime client closed before sending a request",
        ));
    }
    serde_json::from_str(line.trim_end())
        .map_err(|error| AppError::new(format!("failed to decode runtime request JSON: {error}")))
}

fn read_response(reader: &mut BufReader<UnixStream>) -> AppResult<RuntimeResponse> {
    let mut line = String::new();
    let read = match reader.read_line(&mut line) {
        Ok(read) => read,
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
            ) =>
        {
            return Err(AppError::with_exit_code(
                "runtime socket read timed out",
                RUNTIME_TIMEOUT_EXIT_CODE,
            ));
        }
        Err(error) => return Err(error.into()),
    };
    if read == 0 {
        return Err(AppError::new("runtime socket closed unexpectedly"));
    }
    let response = serde_json::from_str::<RuntimeResponse>(line.trim_end()).map_err(|error| {
        AppError::new(format!("failed to decode runtime response JSON: {error}"))
    })?;
    match response {
        RuntimeResponse::Error { message, exit_code } => {
            Err(AppError::with_exit_code(message, exit_code))
        }
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

fn write_runtime_error(stream: &mut UnixStream, error: &AppError) -> AppResult<()> {
    write_message(
        stream,
        &RuntimeResponse::Error {
            message: error.to_string(),
            exit_code: error.exit_code(),
        },
    )
}

fn unexpected_response(expected: &str, response: &RuntimeResponse) -> AppError {
    AppError::new(format!(
        "unexpected runtime response for {expected}: {response:?}"
    ))
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
    [
        "%window-add",
        "%window-close",
        "%session-changed",
        "%pane-add",
        "%pane-close",
    ]
    .iter()
    .any(|prefix| message.starts_with(prefix))
}

fn is_waiting_state(state: SessionState) -> bool {
    matches!(
        state,
        SessionState::ChatReady
            | SessionState::PromptEditing
            | SessionState::PermissionDialog
            | SessionState::FolderTrustPrompt
    )
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

pub fn build_instance_summary_json(
    snapshot: &RuntimePaneSnapshot,
    bindings: &KeybindingsInspection,
) -> AppResult<serde_json::Value> {
    let inspected = snapshot.to_inspected_pane();
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
        "next_safe_action": render_next_safe_action(&inspected.classification, &snapshot.pane, bindings),
        "runtime": {
            "desired_yolo_enabled": snapshot.desired_yolo_enabled,
            "actual_yolo_enabled": snapshot.actual_yolo_enabled,
            "last_stop_reason": snapshot.last_stop_reason,
            "revision": snapshot.revision,
            "updated_at_unix_ms": snapshot.updated_at_unix_ms,
        },
    }))
}

pub fn build_instance_detail_json(
    snapshot: &RuntimePaneSnapshot,
    bindings: &KeybindingsInspection,
) -> AppResult<serde_json::Value> {
    let inspected = snapshot.to_inspected_pane();
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
        "next_safe_action": render_next_safe_action(&inspected.classification, &snapshot.pane, bindings),
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
    use std::collections::VecDeque;
    use std::sync::mpsc;
    use std::sync::{Arc, Mutex};
    use std::thread;

    use super::{
        MutationGate, RecoveryStageHooks, RecoveryStageStore, RecoveryStageTransport, RuntimeEvent,
        RuntimeRequest, RuntimeResponse, is_runtime_discovery_candidate,
        is_verified_runtime_discovery_candidate, is_yolo_candidate_pane,
        recovery_offers_for_inventory, runtime_socket_path, stage_recovery_with,
    };
    use crate::classifier::{Classification, SIGNAL_CODEX_KEYWORDS, SessionState};
    use crate::recovery::{
        RecoveryLifecycle, RecoveryOriginalIdentity, RecoveryRecord, RecoveryTarget,
    };
    use crate::tmux::{TmuxInventory, TmuxPane, TmuxServerIdentity};
    use std::path::Path;

    const CLAUDE_ID: &str = "4d8dc7f8-a842-438a-b2c2-4d39ad509a53";

    fn sample_pane(current_command: &str) -> TmuxPane {
        TmuxPane {
            pane_id: String::from("%1"),
            pane_tty: String::from("/dev/pts/1"),
            pane_pid: Some(1234),
            session_id: String::from("$1"),
            session_name: String::from("demo"),
            window_id: String::from("@2"),
            window_index: 1,
            window_name: String::from("demo"),
            pane_index: 0,
            current_command: current_command.to_string(),
            current_path: String::from("/tmp/demo"),
            pane_title: String::new(),
            pane_active: true,
            cursor_x: Some(0),
            cursor_y: Some(0),
        }
    }

    fn sample_server(pid: u32) -> TmuxServerIdentity {
        TmuxServerIdentity {
            socket_path: "/tmp/tmux/default".to_string(),
            pid,
            start_time: 100,
        }
    }

    fn sample_recovery() -> RecoveryRecord {
        let pane = sample_pane("claude");
        RecoveryRecord {
            id: "recovery-1".to_string(),
            source_observation_id: "observation-1".to_string(),
            workspace_id: "workspace-1".to_string(),
            workspace_root: "/tmp/demo".to_string(),
            lifecycle: RecoveryLifecycle::Crashed,
            provider: "claude".to_string(),
            provider_session_id: CLAUDE_ID.to_string(),
            original: RecoveryOriginalIdentity::from_inventory(&sample_server(1), &pane),
            crashed_at_unix_ms: 1,
            staging_run_id: None,
            staging_token: None,
            staging_started_at_unix_ms: None,
            target: None,
            staged_command: None,
            staged_at_unix_ms: None,
            resolved_at_unix_ms: None,
            dismissed_at_unix_ms: None,
        }
    }

    fn matching_inventory() -> TmuxInventory {
        let mut pane = sample_pane("bash");
        pane.pane_id = "%9".to_string();
        pane.session_id = "$9".to_string();
        pane.window_id = "@9".to_string();
        TmuxInventory {
            server: sample_server(2),
            panes: vec![pane],
        }
    }

    #[derive(Clone)]
    struct FakeTransport {
        inventories: Arc<Mutex<VecDeque<Result<TmuxInventory, String>>>>,
        paste_error: Arc<Mutex<Option<String>>>,
        pastes: Arc<Mutex<Vec<(String, String)>>>,
        events: Arc<Mutex<Vec<String>>>,
    }

    impl FakeTransport {
        fn new(
            inventories: Vec<Result<TmuxInventory, String>>,
            events: Arc<Mutex<Vec<String>>>,
        ) -> Self {
            Self {
                inventories: Arc::new(Mutex::new(inventories.into())),
                paste_error: Arc::new(Mutex::new(None)),
                pastes: Arc::new(Mutex::new(Vec::new())),
                events,
            }
        }
    }

    impl RecoveryStageTransport for FakeTransport {
        fn inventory(&self) -> crate::app::AppResult<TmuxInventory> {
            self.events.lock().unwrap().push("inventory".to_string());
            self.inventories
                .lock()
                .unwrap()
                .pop_front()
                .expect("test inventory should exist")
                .map_err(crate::app::AppError::new)
        }

        fn paste_text(&self, pane_id: &str, text: &str) -> crate::app::AppResult<()> {
            self.events.lock().unwrap().push("paste".to_string());
            self.pastes
                .lock()
                .unwrap()
                .push((pane_id.to_string(), text.to_string()));
            match self.paste_error.lock().unwrap().clone() {
                Some(error) => Err(crate::app::AppError::new(error)),
                None => Ok(()),
            }
        }
    }

    #[derive(Clone)]
    struct FakeStore {
        record: Arc<Mutex<RecoveryRecord>>,
        claim_error: Arc<Mutex<Option<String>>>,
        mark_staged_error: Arc<Mutex<Option<String>>>,
        events: Arc<Mutex<Vec<String>>>,
    }

    impl FakeStore {
        fn new(record: RecoveryRecord, events: Arc<Mutex<Vec<String>>>) -> Self {
            Self {
                record: Arc::new(Mutex::new(record)),
                claim_error: Arc::new(Mutex::new(None)),
                mark_staged_error: Arc::new(Mutex::new(None)),
                events,
            }
        }
    }

    impl RecoveryStageStore for FakeStore {
        fn list(&self) -> crate::app::AppResult<Vec<RecoveryRecord>> {
            Ok(vec![self.record.lock().unwrap().clone()])
        }

        fn claim(
            &self,
            _recovery_id: &str,
            run_id: &str,
            token: &str,
            target: &RecoveryTarget,
            command: &str,
        ) -> crate::app::AppResult<()> {
            self.events.lock().unwrap().push("claim".to_string());
            if let Some(error) = self.claim_error.lock().unwrap().clone() {
                return Err(crate::app::AppError::with_exit_code(error, 409));
            }
            let mut record = self.record.lock().unwrap();
            record.lifecycle = RecoveryLifecycle::Staging;
            record.staging_run_id = Some(run_id.to_string());
            record.staging_token = Some(token.to_string());
            record.target = Some(target.clone());
            record.staged_command = Some(command.to_string());
            Ok(())
        }

        fn release(&self, _recovery_id: &str, _token: &str) -> crate::app::AppResult<bool> {
            self.events.lock().unwrap().push("release".to_string());
            let mut record = self.record.lock().unwrap();
            if record.lifecycle != RecoveryLifecycle::Staging {
                return Ok(false);
            }
            record.lifecycle = RecoveryLifecycle::Crashed;
            record.target = None;
            record.staging_token = None;
            Ok(true)
        }

        fn mark_staged(&self, _recovery_id: &str, _token: &str) -> crate::app::AppResult<bool> {
            self.events.lock().unwrap().push("mark-staged".to_string());
            if let Some(error) = self.mark_staged_error.lock().unwrap().clone() {
                return Err(crate::app::AppError::new(error));
            }
            self.record.lock().unwrap().lifecycle = RecoveryLifecycle::Staged;
            Ok(true)
        }

        fn mark_uncertain(&self, _recovery_id: &str, _token: &str) -> crate::app::AppResult<bool> {
            self.events
                .lock()
                .unwrap()
                .push("mark-uncertain".to_string());
            self.record.lock().unwrap().lifecycle = RecoveryLifecycle::Uncertain;
            Ok(true)
        }

        fn load(&self, _recovery_id: &str) -> crate::app::AppResult<Option<RecoveryRecord>> {
            Ok(Some(self.record.lock().unwrap().clone()))
        }
    }

    struct NoopHooks;

    impl RecoveryStageHooks for NoopHooks {
        type TargetGuard = ();

        fn target_selected(&self, _target: &RecoveryTarget) -> crate::app::AppResult<()> {
            Ok(())
        }
    }

    struct BlockingHooks {
        before_claim_reached: mpsc::Sender<()>,
        before_claim_release: Mutex<mpsc::Receiver<()>>,
        before_paste_reached: mpsc::Sender<()>,
        before_paste_release: Mutex<mpsc::Receiver<()>>,
    }

    impl RecoveryStageHooks for BlockingHooks {
        type TargetGuard = ();

        fn target_selected(&self, _target: &RecoveryTarget) -> crate::app::AppResult<()> {
            Ok(())
        }

        fn before_claim(&self) {
            self.before_claim_reached.send(()).unwrap();
            self.before_claim_release.lock().unwrap().recv().unwrap();
        }

        fn before_paste(&self) {
            self.before_paste_reached.send(()).unwrap();
            self.before_paste_release.lock().unwrap().recv().unwrap();
        }
    }

    #[test]
    fn is_runtime_discovery_candidate_admits_agy_pane() {
        assert!(is_runtime_discovery_candidate(&sample_pane("agy")));
        // Casing must not matter — `pane.current_command` may arrive
        // capitalised on some installs.
        assert!(is_runtime_discovery_candidate(&sample_pane("Agy")));
    }

    #[test]
    fn is_runtime_discovery_candidate_admits_claude_and_codex_panes() {
        assert!(is_runtime_discovery_candidate(&sample_pane("claude")));
        assert!(is_runtime_discovery_candidate(&sample_pane("codex")));
    }

    #[test]
    fn is_runtime_discovery_candidate_rejects_bash_pane() {
        assert!(!is_runtime_discovery_candidate(&sample_pane("bash")));
        assert!(!is_runtime_discovery_candidate(&sample_pane("zsh")));
    }

    #[test]
    fn node_runtime_candidate_requires_codex_screen_evidence() {
        let node = sample_pane("node");
        let mut classification = Classification {
            source: String::from("test"),
            state: SessionState::Unknown,
            has_questions: false,
            recap_present: false,
            recap_excerpt: None,
            signals: Vec::new(),
        };

        assert!(!is_verified_runtime_discovery_candidate(
            &node,
            &classification
        ));
        classification
            .signals
            .push(String::from(SIGNAL_CODEX_KEYWORDS));
        assert!(is_verified_runtime_discovery_candidate(
            &node,
            &classification
        ));
        assert!(is_verified_runtime_discovery_candidate(
            &sample_pane("claude"),
            &classification
        ));
    }

    #[test]
    fn is_yolo_candidate_pane_admits_agy_pane() {
        assert!(is_yolo_candidate_pane(&sample_pane("agy")));
        assert!(is_yolo_candidate_pane(&sample_pane("AGY")));
    }

    #[test]
    fn is_yolo_candidate_pane_rejects_bash_pane() {
        assert!(!is_yolo_candidate_pane(&sample_pane("bash")));
    }

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
        let encoded = serde_json::to_string(&RuntimeResponse::Event {
            event: event.clone(),
        })
        .expect("event should encode");
        let decoded =
            serde_json::from_str::<RuntimeResponse>(&encoded).expect("event should decode");
        match decoded {
            RuntimeResponse::Event { event: decoded } => {
                assert_eq!(decoded.revision, event.revision)
            }
            other => panic!("unexpected response: {other:?}"),
        }
    }

    #[test]
    fn recovery_requests_round_trip_as_dedicated_protocol_variants() {
        for request in [
            RuntimeRequest::ListRecoveries,
            RuntimeRequest::StageRecovery {
                recovery_id: "recovery-1".to_string(),
            },
            RuntimeRequest::DismissRecovery {
                recovery_id: "recovery-1".to_string(),
            },
        ] {
            let encoded = serde_json::to_string(&request).unwrap();
            let decoded: RuntimeRequest = serde_json::from_str(&encoded).unwrap();
            assert_eq!(
                serde_json::to_value(decoded).unwrap(),
                serde_json::to_value(request).unwrap()
            );
        }
    }

    #[test]
    fn stage_recovery_repeats_inventory_and_pastes_exactly_once() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let inventory = matching_inventory();
        let transport = FakeTransport::new(
            vec![Ok(inventory.clone()), Ok(inventory.clone()), Ok(inventory)],
            Arc::clone(&events),
        );
        let store = FakeStore::new(sample_recovery(), Arc::clone(&events));
        let staged =
            stage_recovery_with(&transport, &store, &NoopHooks, "run-1", "recovery-1").unwrap();
        assert_eq!(staged.lifecycle, RecoveryLifecycle::Staged);
        let pastes = transport.pastes.lock().unwrap();
        assert_eq!(pastes.len(), 1);
        assert_eq!(pastes[0].0, "%9");
        assert_eq!(
            pastes[0].1,
            "cd '/tmp/demo' && claude --resume '4d8dc7f8-a842-438a-b2c2-4d39ad509a53'"
        );
        assert_eq!(
            *events.lock().unwrap(),
            vec![
                "inventory",
                "inventory",
                "claim",
                "inventory",
                "paste",
                "mark-staged"
            ]
        );
    }

    #[test]
    fn stage_recovery_refuses_changed_target_without_claim_or_paste() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let first = matching_inventory();
        let mut changed = first.clone();
        changed.panes[0].pane_title = "changed".to_string();
        let transport = FakeTransport::new(vec![Ok(first), Ok(changed)], Arc::clone(&events));
        let store = FakeStore::new(sample_recovery(), Arc::clone(&events));
        assert!(
            stage_recovery_with(&transport, &store, &NoopHooks, "run-1", "recovery-1").is_err()
        );
        assert!(transport.pastes.lock().unwrap().is_empty());
        assert_eq!(
            store.record.lock().unwrap().lifecycle,
            RecoveryLifecycle::Crashed
        );
        assert_eq!(*events.lock().unwrap(), vec!["inventory", "inventory"]);
    }

    #[test]
    fn stage_recovery_known_failures_release_to_crashed() {
        for failure in ["inventory", "paste"] {
            let events = Arc::new(Mutex::new(Vec::new()));
            let inventory = matching_inventory();
            let transport = FakeTransport::new(
                vec![
                    Ok(inventory.clone()),
                    Ok(inventory.clone()),
                    if failure == "inventory" {
                        Err("inventory failed".to_string())
                    } else {
                        Ok(inventory)
                    },
                ],
                Arc::clone(&events),
            );
            if failure == "paste" {
                *transport.paste_error.lock().unwrap() = Some("paste failed".to_string());
            }
            let store = FakeStore::new(sample_recovery(), Arc::clone(&events));
            assert!(
                stage_recovery_with(&transport, &store, &NoopHooks, "run-1", "recovery-1").is_err()
            );
            assert_eq!(
                store.record.lock().unwrap().lifecycle,
                RecoveryLifecycle::Crashed
            );
            assert!(
                events
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|event| event == "release")
            );
        }
    }

    #[test]
    fn stage_recovery_immediate_target_change_releases_without_paste() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let inventory = matching_inventory();
        let mut changed = inventory.clone();
        changed.panes[0].cursor_x = Some(99);
        let transport = FakeTransport::new(
            vec![Ok(inventory.clone()), Ok(inventory), Ok(changed)],
            Arc::clone(&events),
        );
        let store = FakeStore::new(sample_recovery(), Arc::clone(&events));
        assert!(
            stage_recovery_with(&transport, &store, &NoopHooks, "run-1", "recovery-1").is_err()
        );
        assert!(transport.pastes.lock().unwrap().is_empty());
        assert_eq!(
            store.record.lock().unwrap().lifecycle,
            RecoveryLifecycle::Crashed
        );
        assert!(
            events
                .lock()
                .unwrap()
                .iter()
                .any(|event| event == "release")
        );
    }

    #[test]
    fn stage_recovery_post_paste_commit_failure_is_not_retryable() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let inventory = matching_inventory();
        let transport = FakeTransport::new(
            vec![Ok(inventory.clone()), Ok(inventory.clone()), Ok(inventory)],
            Arc::clone(&events),
        );
        let store = FakeStore::new(sample_recovery(), Arc::clone(&events));
        *store.mark_staged_error.lock().unwrap() = Some("commit failed".to_string());
        assert!(
            stage_recovery_with(&transport, &store, &NoopHooks, "run-1", "recovery-1").is_err()
        );
        assert_eq!(transport.pastes.lock().unwrap().len(), 1);
        assert_eq!(
            store.record.lock().unwrap().lifecycle,
            RecoveryLifecycle::Uncertain
        );
        assert!(
            !events
                .lock()
                .unwrap()
                .iter()
                .any(|event| event == "release")
        );
    }

    #[test]
    fn stage_recovery_claim_conflict_never_pastes() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let inventory = matching_inventory();
        let transport = FakeTransport::new(
            vec![Ok(inventory.clone()), Ok(inventory)],
            Arc::clone(&events),
        );
        let store = FakeStore::new(sample_recovery(), Arc::clone(&events));
        *store.claim_error.lock().unwrap() = Some("target conflict".to_string());
        let error =
            stage_recovery_with(&transport, &store, &NoopHooks, "run-1", "recovery-1").unwrap_err();
        assert_eq!(error.exit_code(), 409);
        assert!(transport.pastes.lock().unwrap().is_empty());
        assert_eq!(
            store.record.lock().unwrap().lifecycle,
            RecoveryLifecycle::Crashed
        );
    }

    #[test]
    fn stage_recovery_protocol_returns_guarded_conflict_without_paste() {
        let state_dir = std::env::temp_dir().join(format!(
            "botctl-stage-protocol-conflict-{}-{}",
            std::process::id(),
            super::now_unix_ms()
        ));
        std::fs::create_dir_all(&state_dir).unwrap();
        let socket_path = state_dir.join("runtime.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket_path).unwrap();
        let events = Arc::new(Mutex::new(Vec::new()));
        let inventory = matching_inventory();
        let transport = FakeTransport::new(
            vec![Ok(inventory.clone()), Ok(inventory)],
            Arc::clone(&events),
        );
        let store = FakeStore::new(sample_recovery(), Arc::clone(&events));
        *store.claim_error.lock().unwrap() = Some("target conflict".to_string());
        let server_transport = transport.clone();
        let server_store = store.clone();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = std::io::BufReader::new(stream);
            let request = super::read_request(&mut reader).unwrap();
            assert!(matches!(
                request,
                RuntimeRequest::StageRecovery { ref recovery_id } if recovery_id == "recovery-1"
            ));
            let mut stream = reader.into_inner();
            let error = stage_recovery_with(
                &server_transport,
                &server_store,
                &NoopHooks,
                "run-1",
                "recovery-1",
            )
            .unwrap_err();
            super::write_runtime_error(&mut stream, &error).unwrap();
        });

        let client = super::RuntimeClient::with_socket_path(socket_path);
        let error = client.stage_recovery("recovery-1").unwrap_err();
        assert_eq!(error.exit_code(), 409);
        assert!(error.to_string().contains("target conflict"));
        server.join().unwrap();
        assert!(transport.pastes.lock().unwrap().is_empty());
        assert_eq!(
            store.record.lock().unwrap().lifecycle,
            RecoveryLifecycle::Crashed
        );
        let _ = std::fs::remove_dir_all(state_dir);
    }

    #[test]
    fn stop_waits_for_accepted_stage_before_claim_and_paste() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let inventory = matching_inventory();
        let transport = FakeTransport::new(
            vec![Ok(inventory.clone()), Ok(inventory.clone()), Ok(inventory)],
            Arc::clone(&events),
        );
        let store = FakeStore::new(sample_recovery(), Arc::clone(&events));
        let gate = MutationGate::new();
        let permit = gate.acquire().unwrap();
        let (claim_reached_tx, claim_reached_rx) = mpsc::channel();
        let (claim_release_tx, claim_release_rx) = mpsc::channel();
        let (paste_reached_tx, paste_reached_rx) = mpsc::channel();
        let (paste_release_tx, paste_release_rx) = mpsc::channel();
        let hooks = BlockingHooks {
            before_claim_reached: claim_reached_tx,
            before_claim_release: Mutex::new(claim_release_rx),
            before_paste_reached: paste_reached_tx,
            before_paste_release: Mutex::new(paste_release_rx),
        };
        let stage = thread::spawn(move || {
            let result = stage_recovery_with(&transport, &store, &hooks, "run-1", "recovery-1");
            drop(permit);
            result
        });
        claim_reached_rx.recv().unwrap();
        assert!(gate.begin_stop());
        assert!(gate.acquire().is_err());
        let (stopped_tx, stopped_rx) = mpsc::channel();
        let stop_gate = gate.clone();
        let stop = thread::spawn(move || {
            stop_gate.wait_for_drain();
            stopped_tx.send(()).unwrap();
        });
        assert!(stopped_rx.try_recv().is_err());
        claim_release_tx.send(()).unwrap();
        paste_reached_rx.recv().unwrap();
        assert!(stopped_rx.try_recv().is_err());
        paste_release_tx.send(()).unwrap();
        stage.join().unwrap().unwrap();
        stopped_rx.recv().unwrap();
        stop.join().unwrap();
        assert!(gate.acquire().is_err());
    }

    #[test]
    fn replacement_server_removes_persisted_navigation_target() {
        let state_dir = std::env::temp_dir().join(format!(
            "botctl-runtime-stale-target-{}-{}",
            std::process::id(),
            crate::runtime::now_unix_ms()
        ));
        let _ = std::fs::remove_dir_all(&state_dir);
        let workspace_root = state_dir.join("workspace");
        std::fs::create_dir_all(&workspace_root).unwrap();
        let workspace =
            crate::storage::resolve_workspace_for_path(&state_dir, &workspace_root).unwrap();
        let old_server = sample_server(1);
        let mut original = sample_pane("claude");
        original.current_path = workspace_root.display().to_string();
        let abandoned = crate::storage::begin_runtime_run(&state_dir).unwrap();
        crate::storage::checkpoint_claude_observation(
            &state_dir,
            &abandoned,
            &workspace.id,
            &old_server,
            &original,
            CLAUDE_ID,
        )
        .unwrap();
        let run = crate::storage::begin_runtime_run(&state_dir).unwrap();
        crate::storage::reconcile_abandoned_observations(
            &state_dir,
            &run,
            &TmuxInventory {
                server: old_server.clone(),
                panes: Vec::new(),
            },
        )
        .unwrap();
        let recovery = crate::storage::list_nonterminal_recoveries(&state_dir)
            .unwrap()
            .remove(0);
        let mut target_pane = original;
        target_pane.current_command = "bash".to_string();
        let old_target = RecoveryTarget {
            server: old_server,
            pane: target_pane,
        };
        let command =
            crate::recovery::build_recovery_command("claude", &recovery.original.cwd, CLAUDE_ID)
                .unwrap();
        crate::storage::claim_recovery_for_staging(
            &state_dir,
            &recovery.id,
            &run,
            "token",
            &old_target,
            &command,
        )
        .unwrap();
        crate::storage::mark_recovery_staged(&state_dir, &recovery.id, "token").unwrap();
        let replacement = TmuxInventory {
            server: sample_server(2),
            panes: vec![old_target.pane.clone()],
        };
        let offers = recovery_offers_for_inventory(&state_dir, &replacement).unwrap();
        assert_eq!(offers.len(), 1);
        assert!(offers[0].target.is_none());
        assert!(
            offers[0]
                .disabled_reason
                .as_deref()
                .unwrap()
                .contains("navigation is disabled")
        );
        let _ = std::fs::remove_dir_all(state_dir);
    }
}
