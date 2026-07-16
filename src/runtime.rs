use std::collections::hash_map::DefaultHasher;
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs;
use std::hash::{Hash, Hasher};
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
    TmuxRuntimeDurations, VerifiedProviderRecoveryEvidence, WorkspaceRecord,
    apply_recovery_inventory_evidence, begin_runtime_run, claim_recovery_for_staging,
    dismiss_recovery, finish_runtime_run_clean, list_babysit_registration_pane_ids,
    list_nonterminal_recoveries, load_recovery, mark_recovery_staged, mark_recovery_uncertain,
    mark_stale_staging_uncertain, release_known_failed_staging_claim,
    resolve_live_claude_session_id, resolve_workspace, resolve_workspace_for_path,
    sync_tmux_claude_session_id, sync_tmux_runtime_state,
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
const SCHEDULER_TICK_MS: u64 = 50;
const TOPOLOGY_SCAN_INTERVAL_MS: u64 = 15_000;
const MIN_FULL_SAFETY_INTERVAL_MS: u64 = 10_000;
const SUBSCRIBER_CAPACITY: usize = 128;
const OBSERVER_CHANNEL_CAPACITY: usize = 256;
const OBSERVER_BATCH_SIZE: usize = 256;
const SUBSCRIPTION_WRITE_TIMEOUT_MS: u64 = 1_000;
const DIAGNOSTIC_THROTTLE_MS: u64 = 30_000;
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
    #[serde(default)]
    pub cook_duration_ms: Option<u64>,
    #[serde(default)]
    pub duration_sampled_at_unix_ms: Option<i64>,
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
    Status { status: RuntimeStatus },
    Panes { panes: Vec<RuntimePaneSnapshot> },
    Pane { pane: Option<RuntimePaneSnapshot> },
    YoloUpdated { updated: Vec<String> },
    Action { result: RuntimeActionResult },
    Recoveries {
        recoveries: Vec<RuntimeRecoveryOffer>,
    },
    RecoveryStaged {
        recovery: RuntimeRecoveryOffer,
    },
    RecoveryDismissed {
        recovery_id: String,
    },
    Error { message: String, exit_code: i32 },
    Event { event: RuntimeEvent },
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
    /// Receive the complete subscription backlog through the explicit `Ok`
    /// boundary. A timeout is returned as an error so callers can detect an
    /// older runtime that does not provide atomic bootstrap completion.
    pub fn bootstrap(&mut self) -> AppResult<BTreeMap<String, RuntimePaneSnapshot>> {
        let mut panes = BTreeMap::new();
        loop {
            match read_response(&mut self.reader)? {
                RuntimeResponse::Event { event } => {
                    if let Some(snapshot) = event.snapshot {
                        panes.insert(snapshot.pane.pane_id.clone(), snapshot);
                    }
                }
                RuntimeResponse::Ok => return Ok(panes),
                other => return Err(unexpected_response("subscription bootstrap", &other)),
            }
        }
    }

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
        let mapped = match &finalization {
            Ok(()) => Ok(()),
            Err(error) => Err(error.to_string()),
        };
        let (shutdown, ready) = &*shutdown_result;
        let mut shutdown = shutdown.lock().unwrap();
        shutdown.result = Some(mapped);
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
    subscribers: Vec<mpsc::SyncSender<RuntimeEvent>>,
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
struct PaneIdentityProjection {
    pane_id: String,
    pane_tty: String,
    pane_pid: Option<u32>,
    session_id: String,
    window_id: String,
    current_command: String,
}

impl From<&TmuxPane> for PaneIdentityProjection {
    fn from(pane: &TmuxPane) -> Self {
        Self {
            pane_id: pane.pane_id.clone(),
            pane_tty: pane.pane_tty.clone(),
            pane_pid: pane.pane_pid,
            session_id: pane.session_id.clone(),
            window_id: pane.window_id.clone(),
            current_command: pane.current_command.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TopologyPaneProjection {
    identity: PaneIdentityProjection,
    session_name: String,
    window_index: u16,
    pane_index: u16,
    current_path: String,
}

impl From<&TmuxPane> for TopologyPaneProjection {
    fn from(pane: &TmuxPane) -> Self {
        // Window/pane titles, focus, and cursor position are volatile and do
        // not require a fresh classification. Identity, placement, and cwd do.
        Self {
            identity: PaneIdentityProjection::from(pane),
            session_name: pane.session_name.clone(),
            window_index: pane.window_index,
            pane_index: pane.pane_index,
            current_path: pane.current_path.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SnapshotPaneProjection {
    topology: TopologyPaneProjection,
}

impl From<&TmuxPane> for SnapshotPaneProjection {
    fn from(pane: &TmuxPane) -> Self {
        // Publication follows the same stable topology contract. The full
        // pane still updates internally for safety checks on every reconcile.
        Self {
            topology: TopologyPaneProjection::from(pane),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SnapshotSignature {
    pane: SnapshotPaneProjection,
    classification: Classification,
    workspace_id: String,
    workspace_root: String,
    provider_session_id: Option<String>,
    desired_yolo_enabled: bool,
    actual_yolo_enabled: bool,
    last_stop_reason: Option<String>,
    last_action: Option<String>,
    focused_source: String,
    live_excerpt: String,
    raw_source_hash: u64,
}

impl SnapshotSignature {
    fn from_snapshot(snapshot: &RuntimePaneSnapshot, provider_session_id: Option<String>) -> Self {
        let mut hasher = DefaultHasher::new();
        snapshot.raw_source.hash(&mut hasher);
        Self {
            pane: SnapshotPaneProjection::from(&snapshot.pane),
            classification: snapshot.classification.clone(),
            workspace_id: snapshot.workspace_id.clone(),
            workspace_root: snapshot.workspace_root.clone(),
            provider_session_id,
            desired_yolo_enabled: snapshot.desired_yolo_enabled,
            actual_yolo_enabled: snapshot.actual_yolo_enabled,
            last_stop_reason: snapshot.last_stop_reason.clone(),
            last_action: snapshot.last_action.clone(),
            focused_source: snapshot.focused_source.clone(),
            live_excerpt: snapshot.live_excerpt.clone(),
            raw_source_hash: hasher.finish(),
        }
    }
}

#[derive(Debug)]
struct TrackedPane {
    pane: TmuxPane,
    workspace: WorkspaceRecord,
    snapshot: RuntimePaneSnapshot,
    signature: SnapshotSignature,
    screen_model: Option<ScreenModel>,
    screen_model_seeded: bool,
    stream_generation: u64,
    reconciled_generation: u64,
    dirty_since: Option<Instant>,
    last_stream_activity: Option<Instant>,
    retry_at: Option<Instant>,
    retry_attempt: u8,
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
    Opened {
        session_id: String,
        generation: u64,
    },
    Line {
        session_id: String,
        generation: u64,
        line: String,
    },
    Closed {
        session_id: String,
        generation: u64,
    },
}

#[derive(Debug)]
struct ActiveObserver {
    generation: u64,
    cancel: Arc<AtomicBool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ControlLineEffect {
    None,
    Reconcile,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReconcileReason {
    Topology,
    Dirty,
    Safety,
    Retry,
}

#[derive(Debug, Clone, Copy)]
struct PendingReconcile {
    reason: ReconcileReason,
    retry_attempt: u8,
    retry_at: Option<Instant>,
}

impl PendingReconcile {
    fn new(reason: ReconcileReason) -> Self {
        Self {
            reason,
            retry_attempt: 0,
            retry_at: None,
        }
    }

    fn is_due(self, now: Instant) -> bool {
        self.retry_at.is_none_or(|retry_at| retry_at <= now)
    }

    fn failed(&mut self, now: Instant) {
        self.retry_at = Some(now + retry_delay(self.retry_attempt));
        self.retry_attempt = self.retry_attempt.saturating_add(1);
        self.reason = ReconcileReason::Retry;
    }
}

#[derive(Debug)]
struct RuntimeScheduler {
    topology_requested: bool,
    last_topology_scan: Option<Instant>,
    topology_retry_at: Option<Instant>,
    topology_retry_attempt: u8,
    last_full_safety_pass: Instant,
    full_safety_interval: Duration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SchedulerDecision {
    topology_scan: bool,
    full_safety_pass: bool,
}

impl RuntimeScheduler {
    fn new(now: Instant, reconcile_ms: u64) -> Self {
        Self {
            topology_requested: true,
            last_topology_scan: None,
            topology_retry_at: None,
            topology_retry_attempt: 0,
            last_full_safety_pass: now,
            full_safety_interval: Duration::from_millis(
                reconcile_ms.max(MIN_FULL_SAFETY_INTERVAL_MS),
            ),
        }
    }

    fn request_topology_scan(&mut self) {
        self.topology_requested = true;
    }

    fn take_due(&mut self, now: Instant) -> SchedulerDecision {
        let retry_due = self
            .topology_retry_at
            .is_none_or(|retry_at| retry_at <= now);
        let topology_scan = retry_due
            && (self.topology_requested
                || self.last_topology_scan.is_none_or(|last| {
                    now.duration_since(last) >= Duration::from_millis(TOPOLOGY_SCAN_INTERVAL_MS)
                }));
        if topology_scan {
            self.topology_requested = false;
            self.last_topology_scan = Some(now);
        }
        let full_safety_pass =
            now.duration_since(self.last_full_safety_pass) >= self.full_safety_interval;
        if full_safety_pass {
            self.last_full_safety_pass = now;
        }
        SchedulerDecision {
            topology_scan,
            full_safety_pass,
        }
    }

    fn finish_topology_scan(&mut self, now: Instant, succeeded: bool) {
        if succeeded {
            self.topology_retry_at = None;
            self.topology_retry_attempt = 0;
        } else {
            self.topology_requested = true;
            self.topology_retry_at = Some(now + retry_delay(self.topology_retry_attempt));
            self.topology_retry_attempt = self.topology_retry_attempt.saturating_add(1);
        }
    }
}

#[derive(Debug, Default)]
struct DiagnosticThrottle {
    last_emitted: BTreeMap<String, Instant>,
}

impl DiagnosticThrottle {
    fn should_emit(&mut self, key: &str, now: Instant) -> bool {
        if self.last_emitted.get(key).is_some_and(|last| {
            now.duration_since(*last) < Duration::from_millis(DIAGNOSTIC_THROTTLE_MS)
        }) {
            return false;
        }
        self.last_emitted.insert(key.to_string(), now);
        true
    }
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
            let mut shutdown = shutdown.lock().unwrap();
            while shutdown.result.is_none() {
                shutdown = ready.wait(shutdown).unwrap();
            }
            let result = shutdown.result.take().unwrap_or(Ok(()));
            shutdown.acknowledged = true;
            ready.notify_all();
            match result {
                Ok(()) => write_message(&mut stream, &RuntimeResponse::Ok),
                Err(message) => write_message(
                    &mut stream,
                    &RuntimeResponse::Error {
                        message,
                        exit_code: 1,
                    },
                ),
            }
        }
        RuntimeRequest::Subscribe => handle_subscribe(stream, &shared),
    }
}

fn handle_subscribe(mut stream: UnixStream, shared: &Arc<Mutex<RuntimeShared>>) -> AppResult<()> {
    stream.set_write_timeout(Some(Duration::from_millis(SUBSCRIPTION_WRITE_TIMEOUT_MS)))?;
    let (tx, rx) = mpsc::sync_channel(SUBSCRIBER_CAPACITY);
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
    write_message(&mut stream, &RuntimeResponse::Ok)?;

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
        let snapshot =
            update_yolo_runtime_state(shared, &pane.pane_id, Some(enabled), enabled, None);
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
            snapshot,
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
            let snapshot = update_yolo_runtime_state(shared, pane_id, Some(enabled), enabled, None);
            if let Some(snapshot) = snapshot {
                let kind = if enabled {
                    "yolo-enabled"
                } else {
                    "yolo-disabled"
                };
                emit_shared_event(
                    shared,
                    Some(pane_id.clone()),
                    Some(snapshot.workspace_id.clone()),
                    kind,
                    kind.to_string(),
                    Some(snapshot),
                );
            }
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
        config,
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
        config,
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

fn spawn_observer(
    config: RuntimeServerConfig,
    shared: Arc<Mutex<RuntimeShared>>,
    stop: Arc<AtomicBool>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let client = runtime_tmux_client();
        let (tx, rx) = mpsc::sync_channel(OBSERVER_CHANNEL_CAPACITY);
        let mut scheduler = RuntimeScheduler::new(Instant::now(), config.reconcile_ms);
        let mut known_candidates = BTreeMap::<String, TmuxPane>::new();
        let mut pending = BTreeMap::<String, PendingReconcile>::new();
        let mut active_observers = BTreeMap::<String, ActiveObserver>::new();
        let mut observer_retries = BTreeMap::<String, (u8, Instant)>::new();
        let mut next_observer_generation = 0u64;
        let mut diagnostics = DiagnosticThrottle::default();

        while !stop.load(Ordering::SeqCst) {
            for _ in 0..OBSERVER_BATCH_SIZE {
                let Ok(message) = rx.try_recv() else {
                    break;
                };
                match message {
                    ObserverMessage::Opened {
                        session_id,
                        generation,
                    } if observer_is_current(&active_observers, &session_id, generation) => {
                        observer_retries.remove(&session_id);
                    }
                    ObserverMessage::Line {
                        session_id,
                        generation,
                        line,
                    } if observer_is_current(&active_observers, &session_id, generation) => {
                        observer_retries.remove(&session_id);
                        if handle_queued_control_line_shared(&shared, &line)
                            == ControlLineEffect::Reconcile
                        {
                            scheduler.request_topology_scan();
                        }
                    }
                    ObserverMessage::Closed {
                        session_id,
                        generation,
                    } if observer_is_current(&active_observers, &session_id, generation) => {
                        active_observers.remove(&session_id);
                        let attempt = observer_retries
                            .get(&session_id)
                            .map(|(attempt, _)| *attempt)
                            .unwrap_or(0);
                        observer_retries.insert(
                            session_id,
                            (
                                attempt.saturating_add(1),
                                Instant::now() + retry_delay(attempt),
                            ),
                        );
                        scheduler.request_topology_scan();
                    }
                    _ => {}
                }
            }

            let now = Instant::now();
            let decision = scheduler.take_due(now);
            if decision.topology_scan {
                match topology_scan(&client, &config, &shared, &mut known_candidates) {
                    Ok(changed_panes) => {
                        scheduler.finish_topology_scan(now, true);
                        let tracked = tracked_pane_ids(&shared)
                            .into_iter()
                            .collect::<BTreeSet<_>>();
                        pending.retain(|pane_id, _| {
                            known_candidates.contains_key(pane_id) || tracked.contains(pane_id)
                        });
                        for pane_id in changed_panes {
                            pending.entry(pane_id).or_insert_with(|| {
                                PendingReconcile::new(ReconcileReason::Topology)
                            });
                        }
                        let candidate_session_ids = known_candidates
                            .values()
                            .map(|pane| pane.session_id.clone())
                            .collect::<BTreeSet<_>>();
                        retain_candidate_observers(
                            &mut active_observers,
                            &mut observer_retries,
                            &candidate_session_ids,
                        );
                    }
                    Err(error) => {
                        scheduler.finish_topology_scan(now, false);
                        if diagnostics.should_emit("topology-scan", now) {
                            emit_shared_event(
                                &shared,
                                None,
                                None,
                                "runtime-error",
                                format!("topology scan failed: {error}"),
                                None,
                            );
                        }
                    }
                }
            }

            let candidate_sessions = known_candidates
                .values()
                .map(|pane| pane.session_id.clone())
                .collect::<BTreeSet<_>>();
            for session_id in candidate_sessions {
                let retry_due = observer_retries
                    .get(&session_id)
                    .is_none_or(|(_, retry_at)| *retry_at <= now);
                if !active_observers.contains_key(&session_id) && retry_due {
                    next_observer_generation = next_observer_generation.wrapping_add(1);
                    let generation = next_observer_generation;
                    let cancel = Arc::new(AtomicBool::new(false));
                    active_observers.insert(
                        session_id.clone(),
                        ActiveObserver {
                            generation,
                            cancel: Arc::clone(&cancel),
                        },
                    );
                    let tx = tx.clone();
                    let client = client.clone();
                    let stop = Arc::clone(&stop);
                    let shared = Arc::clone(&shared);
                    thread::spawn(move || {
                        observe_session_lines(
                            client, session_id, generation, tx, stop, cancel, shared,
                        )
                    });
                }
            }

            if decision.full_safety_pass {
                queue_full_state_safety_pass(&shared, &mut pending);
            }
            for pane_id in dirty_panes_due(&shared, now, config.reconcile_ms) {
                pending
                    .entry(pane_id)
                    .or_insert_with(|| PendingReconcile::new(ReconcileReason::Dirty));
            }

            let due = pending
                .iter()
                .filter(|(_, work)| work.is_due(now))
                .map(|(pane_id, work)| (pane_id.clone(), work.reason))
                .collect::<Vec<_>>();
            for (pane_id, reason) in due {
                let generation = pane_stream_generation(&shared, &pane_id);
                match reconcile_pane(&client, &config, &shared, &pane_id, reason, generation) {
                    Ok(ReconcileOutcome::Complete) => {
                        pending.remove(&pane_id);
                    }
                    Ok(ReconcileOutcome::TopologyRequired) => {
                        mark_reconcile_failure(&shared, &pane_id, now);
                        if let Some(work) = pending.get_mut(&pane_id) {
                            work.failed(now);
                        }
                        scheduler.request_topology_scan();
                    }
                    Err(error) => {
                        mark_reconcile_failure(&shared, &pane_id, now);
                        if let Some(work) = pending.get_mut(&pane_id) {
                            work.failed(now);
                        }
                        if diagnostics.should_emit(&format!("reconcile:{pane_id}"), now) {
                            emit_shared_event(
                                &shared,
                                Some(pane_id),
                                None,
                                "runtime-error",
                                format!("pane reconcile failed: {error}"),
                                None,
                            );
                        }
                    }
                }
            }

            thread::sleep(Duration::from_millis(SCHEDULER_TICK_MS));
        }
    })
}

fn observer_is_current(
    active_observers: &BTreeMap<String, ActiveObserver>,
    session_id: &str,
    generation: u64,
) -> bool {
    active_observers
        .get(session_id)
        .is_some_and(|observer| observer.generation == generation)
}

fn retain_candidate_observers(
    active_observers: &mut BTreeMap<String, ActiveObserver>,
    observer_retries: &mut BTreeMap<String, (u8, Instant)>,
    candidate_session_ids: &BTreeSet<String>,
) {
    active_observers.retain(|session_id, observer| {
        let retain = candidate_session_ids.contains(session_id);
        if !retain {
            observer.cancel.store(true, Ordering::SeqCst);
        }
        retain
    });
    observer_retries.retain(|session_id, _| candidate_session_ids.contains(session_id));
}

fn observe_session_lines(
    client: TmuxClient,
    session_id: String,
    generation: u64,
    tx: mpsc::SyncSender<ObserverMessage>,
    stop: Arc<AtomicBool>,
    cancel: Arc<AtomicBool>,
    shared: Arc<Mutex<RuntimeShared>>,
) {
    let Ok(mut control) = client.control_mode_session(&session_id) else {
        let _ = tx.send(ObserverMessage::Closed {
            session_id,
            generation,
        });
        return;
    };
    if tx
        .send(ObserverMessage::Opened {
            session_id: session_id.clone(),
            generation,
        })
        .is_err()
    {
        return;
    }

    while !stop.load(Ordering::SeqCst) && !cancel.load(Ordering::SeqCst) {
        match control.recv_timeout(Duration::from_millis(CONTROL_POLL_MS)) {
            Ok(ControlModeReceive::Line(line)) => {
                mark_control_output_activity_shared(&shared, &line);
                if tx
                    .send(ObserverMessage::Line {
                        session_id: session_id.clone(),
                        generation,
                        line,
                    })
                    .is_err()
                {
                    break;
                }
            }
            Ok(ControlModeReceive::Idle) => {}
            Ok(ControlModeReceive::Closed) | Err(_) => break,
        }
    }

    let _ = tx.send(ObserverMessage::Closed {
        session_id,
        generation,
    });
}

fn handle_queued_control_line_shared(
    shared: &Arc<Mutex<RuntimeShared>>,
    line: &str,
) -> ControlLineEffect {
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

fn mark_control_output_activity_shared(shared: &Arc<Mutex<RuntimeShared>>, line: &str) {
    let Some(ControlEvent::Output { pane_id, .. } | ControlEvent::ExtendedOutput { pane_id, .. }) =
        parse_control_line(line)
    else {
        return;
    };
    let mut shared = shared.lock().unwrap();
    if let Some(tracked) = shared.panes.get_mut(&pane_id) {
        mark_stream_activity(tracked);
    }
}

fn mark_stream_activity(tracked: &mut TrackedPane) {
    let now = Instant::now();
    tracked.stream_generation = tracked.stream_generation.wrapping_add(1);
    tracked.dirty_since.get_or_insert(now);
    tracked.last_stream_activity = Some(now);
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


fn topology_scan(
    client: &TmuxClient,
    config: &RuntimeServerConfig,
    shared: &Arc<Mutex<RuntimeShared>>,
    known_candidates: &mut BTreeMap<String, TmuxPane>,
) -> AppResult<Vec<String>> {
    // Prefer structured inventory for recovery evidence when available; fall
    // back to strict listing for topology deltas if inventory fails.
    let inventory = client.inventory().ok();
    let current = if let Some(ref inv) = inventory {
        inv.panes
            .iter()
            .filter(|pane| is_runtime_discovery_candidate(pane))
            .map(|pane| (pane.pane_id.clone(), pane.clone()))
            .collect::<BTreeMap<_, _>>()
    } else {
        client
            .list_panes_strict()?
            .into_iter()
            .filter(is_runtime_discovery_candidate)
            .map(|pane| (pane.pane_id.clone(), pane))
            .collect::<BTreeMap<_, _>>()
    };
    let tracked_ids = tracked_pane_ids(shared)
        .into_iter()
        .collect::<BTreeSet<_>>();
    let (changed, removed) = topology_delta(known_candidates, &current, &tracked_ids);

    for pane_id in removed {
        remove_tracked_pane(config, shared, &pane_id, "missing-pane");
    }

    if let Some(inventory) = inventory {
        let run_id = shared.lock().unwrap().run_id.clone();
        let mut verified = Vec::new();
        let resolver = crate::proc_fd::LiveProc;
        for pane in &inventory.panes {
            let Some(provider) = crate::recovery::provider_for_pane_command(&pane.current_command)
            else {
                continue;
            };
            let session_id = match provider {
                "claude" => resolve_live_claude_session_id(pane).ok().flatten(),
                "grok" => crate::grok::resolve_live_grok_session_id(pane, &resolver)
                    .ok()
                    .flatten(),
                _ => None,
            };
            let Some(session_id) = session_id.filter(|id| Uuid::parse_str(id).is_ok()) else {
                continue;
            };
            if let Ok(workspace) =
                resolve_workspace_for_path(&config.state_dir, Path::new(&pane.current_path))
            {
                verified.push(VerifiedProviderRecoveryEvidence {
                    workspace_id: workspace.id.clone(),
                    pane: pane.clone(),
                    provider: provider.to_string(),
                    provider_session_id: session_id,
                });
            }
        }
        // Best-effort: recovery evidence should not fail topology scans.
        let _ = apply_recovery_inventory_evidence(&config.state_dir, &run_id, &inventory, &verified);
    }

    *known_candidates = current;
    Ok(changed)
}

fn topology_delta(
    previous: &BTreeMap<String, TmuxPane>,
    current: &BTreeMap<String, TmuxPane>,
    tracked_ids: &BTreeSet<String>,
) -> (Vec<String>, Vec<String>) {
    let changed = current
        .iter()
        .filter(|(pane_id, pane)| {
            previous.get(*pane_id).is_none_or(|previous| {
                TopologyPaneProjection::from(previous) != TopologyPaneProjection::from(*pane)
            }) || !tracked_ids.contains(*pane_id)
        })
        .map(|(pane_id, _)| pane_id.clone())
        .collect();
    let removed = tracked_ids
        .iter()
        .filter(|pane_id| !current.contains_key(*pane_id))
        .cloned()
        .collect();
    (changed, removed)
}

fn tracked_pane_ids(shared: &Arc<Mutex<RuntimeShared>>) -> Vec<String> {
    shared.lock().unwrap().panes.keys().cloned().collect()
}

fn queue_full_state_safety_pass(
    shared: &Arc<Mutex<RuntimeShared>>,
    pending: &mut BTreeMap<String, PendingReconcile>,
) {
    for pane_id in tracked_pane_ids(shared) {
        pending
            .entry(pane_id)
            .or_insert_with(|| PendingReconcile::new(ReconcileReason::Safety));
    }
}

fn pane_stream_generation(shared: &Arc<Mutex<RuntimeShared>>, pane_id: &str) -> u64 {
    shared
        .lock()
        .unwrap()
        .panes
        .get(pane_id)
        .map(|tracked| tracked.stream_generation)
        .unwrap_or(0)
}

fn dirty_panes_due(
    shared: &Arc<Mutex<RuntimeShared>>,
    now: Instant,
    reconcile_ms: u64,
) -> Vec<String> {
    let debounce = Duration::from_millis(STREAM_DEBOUNCE_MS);
    let deadline = Duration::from_millis(reconcile_ms);
    shared
        .lock()
        .unwrap()
        .panes
        .iter()
        .filter(|(_, tracked)| dirty_reconcile_due(tracked, now, debounce, deadline))
        .map(|(pane_id, _)| pane_id.clone())
        .collect()
}

fn dirty_reconcile_due(
    tracked: &TrackedPane,
    now: Instant,
    debounce: Duration,
    deadline: Duration,
) -> bool {
    tracked.stream_generation > tracked.reconciled_generation
        && tracked.retry_at.is_none_or(|retry_at| retry_at <= now)
        && (tracked
            .last_stream_activity
            .is_some_and(|activity| now.duration_since(activity) >= debounce)
            || tracked
                .dirty_since
                .is_some_and(|dirty_since| now.duration_since(dirty_since) >= deadline))
}

fn mark_reconcile_failure(shared: &Arc<Mutex<RuntimeShared>>, pane_id: &str, now: Instant) {
    let mut shared = shared.lock().unwrap();
    if let Some(tracked) = shared.panes.get_mut(pane_id) {
        tracked.retry_at = Some(now + retry_delay(tracked.retry_attempt));
        tracked.retry_attempt = tracked.retry_attempt.saturating_add(1);
    }
}

fn retry_delay(attempt: u8) -> Duration {
    Duration::from_millis(match attempt {
        0 => 250,
        1 => 500,
        2 => 1_000,
        3 => 2_000,
        _ => 5_000,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReconcileOutcome {
    Complete,
    TopologyRequired,
}

fn reconcile_pane(
    client: &TmuxClient,
    config: &RuntimeServerConfig,
    shared: &Arc<Mutex<RuntimeShared>>,
    pane_id: &str,
    _reason: ReconcileReason,
    generation: u64,
) -> AppResult<ReconcileOutcome> {
    let Some(pane) = client.pane_by_id_exact(pane_id)? else {
        return Ok(ReconcileOutcome::TopologyRequired);
    };
    if !is_runtime_discovery_candidate(&pane) {
        return Ok(ReconcileOutcome::TopologyRequired);
    }

    let identity_changed = shared
        .lock()
        .unwrap()
        .panes
        .get(pane_id)
        .is_some_and(|tracked| !same_pane_identity(&tracked.pane, &pane));
    let inspected = inspect_pane(client, &pane.pane_id, config.history_lines)?;
    if !is_verified_runtime_discovery_candidate(&pane, &inspected.classification) {
        remove_tracked_pane(config, shared, pane_id, "candidate-not-verified");
        return Ok(ReconcileOutcome::Complete);
    }
    let workspace = resolve_workspace_for_path(&config.state_dir, Path::new(&pane.current_path))?;
    let claude_session_id = sync_tmux_claude_session_id(&config.state_dir, &workspace.id, &pane)?;
    let codex_session_id = if is_yolo_candidate_pane(&pane) {
        resolve_codex_session_id_for_pane(&pane)?
    } else {
        None
    };
    let provider_session_id = claude_session_id
        .as_deref()
        .or(codex_session_id.as_deref())
        .map(str::to_string);
    let runtime_durations = sync_tmux_runtime_state(
        &config.state_dir,
        &workspace.id,
        &pane,
        inspected.classification.state.as_str(),
        is_waiting_state(inspected.classification.state),
        inspected.classification.state == SessionState::BusyResponding,
        provider_session_id.as_deref(),
    )?;
    let desired_yolo_enabled = matches!(
        read_yolo_record(&config.state_dir, &pane.pane_id)?,
        Some(record) if record.enabled
    );
    let (previous_stop_reason, previous_last_action, previous_revision, live_excerpt) = {
        let shared = shared.lock().unwrap();
        let previous = shared.panes.get(&pane.pane_id);
        (
            previous
                .filter(|_| !identity_changed)
                .and_then(|tracked| tracked.snapshot.last_stop_reason.clone()),
            previous
                .filter(|_| !identity_changed)
                .and_then(|tracked| tracked.snapshot.last_action.clone()),
            previous
                .map(|tracked| tracked.snapshot.revision)
                .unwrap_or(0),
            previous
                .filter(|_| !identity_changed)
                .map(|tracked| tracked.snapshot.live_excerpt.clone())
                .filter(|excerpt| !excerpt.is_empty())
                .unwrap_or_else(|| {
                    trim_visible_block(&inspected.focused_source, MAX_LIVE_EXCERPT_LINES)
                }),
        )
    };
    let actual_yolo_enabled = desired_yolo_enabled && previous_stop_reason.is_none();
    let mut snapshot = RuntimePaneSnapshot {
        pane: pane.clone(),
        classification: inspected.classification.clone(),
        focused_source: inspected.focused_source.clone(),
        raw_source: inspected.raw_source.clone(),
        live_excerpt,
        wait_duration_ms: runtime_durations
            .wait_duration
            .map(|duration| duration.as_millis() as u64),
        cook_duration_ms: runtime_durations
            .cook_duration
            .map(|duration| duration.as_millis() as u64),
        duration_sampled_at_unix_ms: Some(runtime_durations.sampled_at_unix_ms),
        claude_session_id,
        workspace_id: workspace.id.clone(),
        workspace_root: workspace.workspace_root.clone(),
        desired_yolo_enabled,
        actual_yolo_enabled,
        last_stop_reason: previous_stop_reason,
        last_action: previous_last_action,
        revision: previous_revision,
        updated_at_unix_ms: now_unix_ms(),
    };
    let mut discovered = false;
    let mut state_changed = None;
    let publish_snapshot;
    let generation_stable;
    {
        let mut shared = shared.lock().unwrap();
        generation_stable = shared
            .panes
            .get(&pane.pane_id)
            .is_none_or(|tracked| tracked.stream_generation == generation);
        if !generation_stable {
            snapshot.live_excerpt = shared.panes[&pane.pane_id].snapshot.live_excerpt.clone();
        }
        let signature = SnapshotSignature::from_snapshot(&snapshot, provider_session_id);
        let previous_signature = shared
            .panes
            .get(&pane.pane_id)
            .map(|tracked| tracked.signature.clone());
        let changed = previous_signature.as_ref() != Some(&signature);
        if changed {
            snapshot.revision = next_revision(&mut shared);
        }
        match shared.panes.entry(pane.pane_id.clone()) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                discovered = true;
                let mut model = ScreenModel::new(config.history_lines.max(MAX_LIVE_EXCERPT_LINES));
                let screen_model_seeded = !inspected.focused_source.is_empty();
                if screen_model_seeded {
                    model.seed(&inspected.focused_source);
                }
                entry.insert(TrackedPane {
                    pane: pane.clone(),
                    workspace,
                    snapshot: snapshot.clone(),
                    signature,
                    screen_model: screen_model_seeded.then_some(model),
                    screen_model_seeded,
                    stream_generation: generation,
                    reconciled_generation: generation,
                    dirty_since: None,
                    last_stream_activity: None,
                    retry_at: None,
                    retry_attempt: 0,
                    post_approve_guard_raw_source: None,
                });
            }
            std::collections::btree_map::Entry::Occupied(mut entry) => {
                let tracked = entry.get_mut();
                if tracked.snapshot.classification.state != inspected.classification.state {
                    state_changed = Some(inspected.classification.state);
                }
                if identity_changed
                    || tracked
                        .post_approve_guard_raw_source
                        .as_deref()
                        .is_some_and(|raw| raw != inspected.raw_source.as_str())
                {
                    tracked.post_approve_guard_raw_source = None;
                }
                tracked.pane = pane.clone();
                tracked.workspace = workspace;
                tracked.signature = signature;
                tracked.snapshot = snapshot.clone();
                complete_reconcile_generation(tracked, generation);
                tracked.retry_at = None;
                tracked.retry_attempt = 0;
                if identity_changed {
                    tracked.screen_model = None;
                    tracked.screen_model_seeded = false;
                }
                if !tracked.screen_model_seeded && !inspected.focused_source.is_empty() {
                    let mut model =
                        ScreenModel::new(config.history_lines.max(MAX_LIVE_EXCERPT_LINES));
                    model.seed(&inspected.focused_source);
                    tracked.screen_model = Some(model);
                    tracked.screen_model_seeded = true;
                }
            }
        }
        publish_snapshot = changed.then_some(snapshot.clone());
    }

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
    if let Some(published) = publish_snapshot {
        emit_shared_event(
            shared,
            Some(published.pane.pane_id.clone()),
            Some(published.workspace_id.clone()),
            "pane-snapshot-updated",
            format!(
                "{} {}",
                published.pane.pane_id,
                published.classification.state.as_str()
            ),
            Some(published.clone()),
        );
        if let Some(state) = state_changed {
            emit_shared_event(
                shared,
                Some(published.pane.pane_id.clone()),
                Some(published.workspace_id.clone()),
                "pane-state-changed",
                format!("{} {}", published.pane.pane_id, state.as_str()),
                Some(published),
            );
        }
    }

    // Internal persistence and guard maintenance still run when an unchanged
    // consumer snapshot does not need publication. Automatic action requires
    // a generation-stable capture; raced output remains dirty for a retry.
    let generation_stable =
        generation_stable && pane_stream_generation(shared, &snapshot.pane.pane_id) == generation;
    run_automatic_action_if_generation_stable(generation_stable, || {
        maybe_run_yolo_action(client, config, shared, &snapshot)
    })?;
    Ok(ReconcileOutcome::Complete)
}

fn same_pane_identity(expected: &TmuxPane, current: &TmuxPane) -> bool {
    PaneIdentityProjection::from(expected) == PaneIdentityProjection::from(current)
}

fn run_automatic_action_if_generation_stable(
    generation_stable: bool,
    action: impl FnOnce() -> AppResult<()>,
) -> AppResult<()> {
    if generation_stable {
        action()?;
    }
    Ok(())
}

fn reconcile_clears_dirty(stream_generation: u64, captured_generation: u64) -> bool {
    stream_generation == captured_generation
}

fn complete_reconcile_generation(tracked: &mut TrackedPane, captured_generation: u64) {
    tracked.reconciled_generation = tracked.reconciled_generation.max(captured_generation);
    if reconcile_clears_dirty(tracked.stream_generation, captured_generation) {
        tracked.dirty_since = None;
        tracked.last_stream_activity = None;
    } else {
        // A newer output event raced with capture. Keep it dirty, but start
        // its hard deadline at that newer activity instead of immediately
        // recapturing on every scheduler tick.
        tracked.dirty_since = tracked.last_stream_activity;
    }
}

fn remove_tracked_pane(
    config: &RuntimeServerConfig,
    shared: &Arc<Mutex<RuntimeShared>>,
    pane_id: &str,
    reason: &str,
) {
    let snapshot = shared
        .lock()
        .unwrap()
        .panes
        .remove(pane_id)
        .map(|tracked| tracked.snapshot);
    let Some(snapshot) = snapshot else {
        return;
    };
    if snapshot.desired_yolo_enabled {
        let _ = disable_yolo_record(&config.state_dir, pane_id);
        emit_shared_event(
            shared,
            Some(pane_id.to_string()),
            Some(snapshot.workspace_id.clone()),
            "yolo-monitoring-stopped",
            reason.to_string(),
            None,
        );
    }
    emit_shared_event(
        shared,
        Some(pane_id.to_string()),
        Some(snapshot.workspace_id),
        "pane-removed",
        reason.to_string(),
        None,
    );
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
    // showing. The guard is cleared by `reconcile_pane` once `raw_source`
    // changes.
    {
        let locked = shared.lock().unwrap();
        if let Some(tracked) = locked.panes.get(&snapshot.pane.pane_id)
            && post_approve_guard_matches(tracked, &snapshot.raw_source)
        {
            return Ok(());
        }
    }

    let Some(record) = read_yolo_record(&config.state_dir, &snapshot.pane.pane_id)? else {
        return Ok(());
    };
    if !record.matches_pane(&snapshot.pane) {
        let _ = disable_yolo_record(&config.state_dir, &snapshot.pane.pane_id)?;
        let stopped_snapshot = update_yolo_runtime_state(
            shared,
            &snapshot.pane.pane_id,
            Some(false),
            false,
            Some("identity-changed"),
        );
        emit_shared_event(
            shared,
            Some(snapshot.pane.pane_id.clone()),
            Some(snapshot.workspace_id.clone()),
            "yolo-monitoring-stopped",
            String::from("identity-changed"),
            stopped_snapshot.or_else(|| Some(snapshot.clone())),
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
            // Arm the guard before the post-action capture so a capture
            // failure cannot make the same prompt eligible a second time.
            {
                let mut locked = shared.lock().unwrap();
                if let Some(tracked) = locked.panes.get_mut(&snapshot.pane.pane_id) {
                    tracked.post_approve_guard_raw_source = Some(snapshot.raw_source.clone());
                }
            }
            let after = inspect_pane(client, &snapshot.pane.pane_id, ACTION_GUARD_HISTORY_LINES)?;
            let provider_session_id = shared
                .lock()
                .unwrap()
                .panes
                .get(&snapshot.pane.pane_id)
                .and_then(|tracked| tracked.signature.provider_session_id.clone());
            let runtime_durations = sync_tmux_runtime_state(
                &config.state_dir,
                &snapshot.workspace_id,
                &snapshot.pane,
                after.classification.state.as_str(),
                is_waiting_state(after.classification.state),
                after.classification.state == SessionState::BusyResponding,
                provider_session_id.as_deref(),
            )?;
            let next_snapshot = commit_post_yolo_snapshot(
                shared,
                &snapshot.pane.pane_id,
                &after,
                executed,
                runtime_durations,
                now_unix_ms(),
            )?;
            emit_shared_event(
                shared,
                Some(next_snapshot.pane.pane_id.clone()),
                Some(next_snapshot.workspace_id.clone()),
                "action-executed",
                action_summary,
                Some(next_snapshot),
            );
        }
        Err(error) => {
            let failed_snapshot = update_yolo_runtime_state(
                shared,
                &snapshot.pane.pane_id,
                None,
                false,
                Some("action-failed"),
            );
            emit_shared_event(
                shared,
                Some(snapshot.pane.pane_id.clone()),
                Some(snapshot.workspace_id.clone()),
                "action-failed",
                error.to_string(),
                failed_snapshot.or_else(|| Some(snapshot.clone())),
            );
        }
    }
    Ok(())
}

fn commit_post_yolo_snapshot(
    shared: &Arc<Mutex<RuntimeShared>>,
    pane_id: &str,
    inspected: &InspectedPane,
    last_action: String,
    runtime_durations: TmuxRuntimeDurations,
    updated_at_unix_ms: i64,
) -> AppResult<RuntimePaneSnapshot> {
    let mut shared = shared.lock().unwrap();
    if !shared.panes.contains_key(pane_id) {
        return Err(AppError::with_exit_code(
            format!("pane not found: {pane_id}"),
            404,
        ));
    }
    let revision = next_revision(&mut shared);
    let tracked = shared.panes.get_mut(pane_id).expect("pane checked above");
    tracked.snapshot.classification = inspected.classification.clone();
    tracked.snapshot.focused_source = inspected.focused_source.clone();
    tracked.snapshot.raw_source = inspected.raw_source.clone();
    tracked.snapshot.live_excerpt =
        trim_visible_block(&inspected.focused_source, MAX_LIVE_EXCERPT_LINES);
    tracked.snapshot.wait_duration_ms = runtime_durations
        .wait_duration
        .map(|duration| duration.as_millis() as u64);
    tracked.snapshot.cook_duration_ms = runtime_durations
        .cook_duration
        .map(|duration| duration.as_millis() as u64);
    tracked.snapshot.duration_sampled_at_unix_ms = Some(runtime_durations.sampled_at_unix_ms);
    tracked.snapshot.last_action = Some(last_action);
    tracked.snapshot.revision = revision;
    tracked.snapshot.updated_at_unix_ms = updated_at_unix_ms;
    if !inspected.focused_source.is_empty() {
        let model = tracked
            .screen_model
            .get_or_insert_with(|| ScreenModel::new(MAX_LIVE_EXCERPT_LINES));
        model.seed(&inspected.focused_source);
        tracked.screen_model_seeded = true;
    }
    let provider_session_id = tracked.signature.provider_session_id.clone();
    tracked.signature = SnapshotSignature::from_snapshot(&tracked.snapshot, provider_session_id);
    Ok(tracked.snapshot.clone())
}

fn post_approve_guard_matches(tracked: &TrackedPane, raw_source: &str) -> bool {
    tracked
        .post_approve_guard_raw_source
        .as_deref()
        .is_some_and(|previous| previous == raw_source)
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
    desired_yolo_enabled: Option<bool>,
    actual_yolo_enabled: bool,
    stop_reason: Option<&str>,
) -> Option<RuntimePaneSnapshot> {
    let mut shared = shared.lock().unwrap();
    if !shared.panes.contains_key(pane_id) {
        return None;
    }
    let revision = next_revision(&mut shared);
    let tracked = shared.panes.get_mut(pane_id).expect("pane checked above");
    if let Some(desired) = desired_yolo_enabled {
        tracked.snapshot.desired_yolo_enabled = desired;
    }
    tracked.snapshot.actual_yolo_enabled = actual_yolo_enabled;
    tracked.snapshot.last_stop_reason = stop_reason.map(str::to_string);
    tracked.snapshot.revision = revision;
    tracked.snapshot.updated_at_unix_ms = now_unix_ms();
    let provider_session_id = tracked.signature.provider_session_id.clone();
    tracked.signature = SnapshotSignature::from_snapshot(&tracked.snapshot, provider_session_id);
    Some(tracked.snapshot.clone())
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
    config: &RuntimeServerConfig,
    shared: &Arc<Mutex<RuntimeShared>>,
    pane_id: &str,
    session_name: Option<&str>,
    last_action: Option<String>,
) -> AppResult<RuntimePaneSnapshot> {
    let inspected = inspect_pane(client, pane_id, ACTION_GUARD_HISTORY_LINES)?;
    let (pane, workspace_id, provider_session_id) = {
        let shared = shared.lock().unwrap();
        let tracked = shared
            .panes
            .get(pane_id)
            .ok_or_else(|| AppError::with_exit_code(format!("pane not found: {pane_id}"), 404))?;
        if !session_matches(&tracked.snapshot, session_name) {
            return Err(AppError::with_exit_code(
                format!("pane {} is not tracked for requested session", pane_id),
                404,
            ));
        }
        (
            tracked.pane.clone(),
            tracked.workspace.id.clone(),
            tracked.signature.provider_session_id.clone(),
        )
    };
    let runtime_durations = sync_tmux_runtime_state(
        &config.state_dir,
        &workspace_id,
        &pane,
        inspected.classification.state.as_str(),
        is_waiting_state(inspected.classification.state),
        inspected.classification.state == SessionState::BusyResponding,
        provider_session_id.as_deref(),
    )?;
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
    tracked.snapshot.wait_duration_ms = runtime_durations
        .wait_duration
        .map(|duration| duration.as_millis() as u64);
    tracked.snapshot.cook_duration_ms = runtime_durations
        .cook_duration
        .map(|duration| duration.as_millis() as u64);
    tracked.snapshot.duration_sampled_at_unix_ms = Some(runtime_durations.sampled_at_unix_ms);
    tracked.snapshot.last_action = last_action;
    tracked.snapshot.revision = revision;
    tracked.snapshot.updated_at_unix_ms = now_unix_ms();
    let provider_session_id = tracked.signature.provider_session_id.clone();
    tracked.signature = SnapshotSignature::from_snapshot(&tracked.snapshot, provider_session_id);
    Ok(tracked.snapshot.clone())
}

fn validate_action_target(
    client: &TmuxClient,
    snapshot: &RuntimePaneSnapshot,
) -> AppResult<TmuxPane> {
    let current = client
        .pane_by_id_exact(&snapshot.pane.pane_id)?
        .ok_or_else(|| {
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
            .retain(|sender| sender.try_send(event.clone()).is_ok());
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

fn write_runtime_error(stream: &mut UnixStream, error: &AppError) -> AppResult<()> {
    write_message(
        stream,
        &RuntimeResponse::Error {
            message: error.to_string(),
            exit_code: error.exit_code(),
        },
    )
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
        "%window-renamed",
        "%session-renamed",
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
            | SessionState::AgyCommandPermissionPrompt
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
            "cook_duration_ms": snapshot.cook_duration_ms,
            "duration_sampled_at_unix_ms": snapshot.duration_sampled_at_unix_ms,
            "claude_session_id": snapshot.claude_session_id,
            "revision": snapshot.revision,
            "updated_at_unix_ms": snapshot.updated_at_unix_ms,
        },
    }))
}

#[cfg(test)]
mod tests {
    use super::{
        ActionGuard, ActiveObserver, ControlLineEffect, PendingReconcile, ReconcileReason,
        RuntimeEvent, RuntimePaneSnapshot, RuntimeResponse, RuntimeScheduler, RuntimeShared,
        RuntimeSubscription, SnapshotSignature, TrackedPane, commit_post_yolo_snapshot,
        complete_reconcile_generation, dirty_reconcile_due, emit_shared_event,
        handle_queued_control_line_shared, handle_subscribe, is_runtime_discovery_candidate,
        is_verified_runtime_discovery_candidate, is_waiting_state, is_yolo_candidate_pane,
        mark_control_output_activity_shared, notification_requires_reconcile, observer_is_current,
        post_approve_guard_matches, queue_full_state_safety_pass, reconcile_clears_dirty,
        retain_candidate_observers, retry_delay, run_automatic_action_if_generation_stable,
        runtime_socket_path, same_pane_identity, topology_delta, update_yolo_runtime_state,
        write_message, yolo_workflow,
    };
    use crate::app::InspectedPane;
    use crate::automation::GuardedWorkflow;
    use crate::classifier::{Classification, SIGNAL_CODEX_KEYWORDS, SessionState};
    use crate::storage::{TmuxRuntimeDurations, WorkspaceRecord};
    use crate::tmux::TmuxPane;
    use std::cell::Cell;
    use std::collections::{BTreeMap, BTreeSet};
    use std::io::BufReader;
    use std::os::unix::net::UnixStream;
    use std::path::{Path, PathBuf};
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    };
    use std::thread;
    use std::time::{Duration, Instant};

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

    fn sample_classification(state: SessionState) -> Classification {
        Classification {
            source: String::from("test"),
            state,
            has_questions: false,
            recap_present: false,
            recap_excerpt: None,
            signals: Vec::new(),
        }
    }

    fn sample_snapshot() -> RuntimePaneSnapshot {
        RuntimePaneSnapshot {
            pane: sample_pane("claude"),
            classification: sample_classification(SessionState::ChatReady),
            focused_source: String::from("ready"),
            raw_source: String::from("raw ready"),
            live_excerpt: String::from("ready"),
            wait_duration_ms: Some(10),
            cook_duration_ms: Some(20),
            duration_sampled_at_unix_ms: Some(90),
            claude_session_id: Some(String::from("session")),
            workspace_id: String::from("workspace"),
            workspace_root: String::from("/tmp/demo"),
            desired_yolo_enabled: false,
            actual_yolo_enabled: false,
            last_stop_reason: None,
            last_action: None,
            revision: 1,
            updated_at_unix_ms: 100,
        }
    }

    fn tracked_pane(now: Instant) -> TrackedPane {
        let snapshot = sample_snapshot();
        TrackedPane {
            pane: snapshot.pane.clone(),
            workspace: WorkspaceRecord {
                id: snapshot.workspace_id.clone(),
                workspace_root: snapshot.workspace_root.clone(),
                repo_key: None,
            },
            signature: SnapshotSignature::from_snapshot(
                &snapshot,
                snapshot.claude_session_id.clone(),
            ),
            snapshot,
            screen_model: None,
            screen_model_seeded: false,
            stream_generation: 1,
            reconciled_generation: 0,
            dirty_since: Some(now),
            last_stream_activity: Some(now),
            retry_at: None,
            retry_attempt: 0,
            post_approve_guard_raw_source: None,
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
    fn scheduler_scans_immediately_but_not_on_ordinary_ticks() {
        let start = Instant::now();
        let mut scheduler = RuntimeScheduler::new(start, 1_000);

        assert!(scheduler.take_due(start).topology_scan);
        let ordinary = scheduler.take_due(start + Duration::from_millis(50));
        assert!(!ordinary.topology_scan);
        assert!(!ordinary.full_safety_pass);

        let safety = scheduler.take_due(start + Duration::from_secs(10));
        assert!(!safety.topology_scan);
        assert!(safety.full_safety_pass);
        let topology = scheduler.take_due(start + Duration::from_secs(15));
        assert!(topology.topology_scan);
        assert!(!topology.full_safety_pass);
    }

    #[test]
    fn scheduler_coalesces_notifications_and_honors_long_reconcile_interval() {
        let start = Instant::now();
        let mut scheduler = RuntimeScheduler::new(start, 20_000);
        scheduler.take_due(start);
        scheduler.request_topology_scan();
        scheduler.request_topology_scan();

        assert!(
            scheduler
                .take_due(start + Duration::from_millis(50))
                .topology_scan
        );
        assert!(
            !scheduler
                .take_due(start + Duration::from_millis(100))
                .topology_scan
        );
        assert!(
            !scheduler
                .take_due(start + Duration::from_secs(10))
                .full_safety_pass
        );
        assert!(
            scheduler
                .take_due(start + Duration::from_secs(20))
                .full_safety_pass
        );
    }

    #[test]
    fn scheduler_retries_failed_topology_with_bounded_backoff() {
        let start = Instant::now();
        let mut scheduler = RuntimeScheduler::new(start, 1_000);
        assert!(scheduler.take_due(start).topology_scan);
        scheduler.finish_topology_scan(start, false);

        assert!(
            !scheduler
                .take_due(start + Duration::from_millis(249))
                .topology_scan
        );
        assert!(
            scheduler
                .take_due(start + Duration::from_millis(250))
                .topology_scan
        );
        scheduler.finish_topology_scan(start + Duration::from_millis(250), false);
        assert!(
            !scheduler
                .take_due(start + Duration::from_millis(749))
                .topology_scan
        );
        assert!(
            scheduler
                .take_due(start + Duration::from_millis(750))
                .topology_scan
        );
    }

    #[test]
    fn topology_delta_detects_add_change_and_complete_removal() {
        let old = sample_pane("claude");
        let mut volatile_only = old.clone();
        volatile_only.window_name = String::from("renamed");
        volatile_only.pane_title = String::from("terminal title");
        volatile_only.pane_active = false;
        volatile_only.cursor_x = Some(12);
        volatile_only.cursor_y = Some(8);
        let mut added = sample_pane("codex");
        added.pane_id = String::from("%2");
        let previous = BTreeMap::from([(old.pane_id.clone(), old.clone())]);
        let current = BTreeMap::from([
            (volatile_only.pane_id.clone(), volatile_only.clone()),
            (added.pane_id.clone(), added),
        ]);
        let tracked = BTreeSet::from([String::from("%1"), String::from("%3")]);

        let (changed, removed) = topology_delta(&previous, &current, &tracked);
        assert_eq!(changed, [String::from("%2")]);
        assert_eq!(removed, [String::from("%3")]);

        let mut relevant = volatile_only;
        relevant.current_path = String::from("/tmp/replacement-worktree");
        let current = BTreeMap::from([(relevant.pane_id.clone(), relevant)]);
        let (changed, _) = topology_delta(&previous, &current, &tracked);
        assert_eq!(changed, [String::from("%1")]);

        let mut replacement = old;
        replacement.pane_pid = Some(9999);
        let current = BTreeMap::from([(replacement.pane_id.clone(), replacement)]);
        let (changed, _) = topology_delta(&previous, &current, &tracked);
        assert_eq!(changed, [String::from("%1")]);
    }

    #[test]
    fn full_state_safety_pass_only_queues_tracked_panes() {
        let shared = Arc::new(Mutex::new(RuntimeShared::new(Path::new("/tmp/runtime-test.sock"), String::from("test-run"))));
        shared
            .lock()
            .unwrap()
            .panes
            .insert(String::from("%1"), tracked_pane(Instant::now()));
        let mut pending = BTreeMap::new();

        queue_full_state_safety_pass(&shared, &mut pending);

        assert_eq!(pending.len(), 1);
        assert_eq!(pending["%1"].reason, ReconcileReason::Safety);
    }

    #[test]
    fn dirty_reconcile_uses_quiet_debounce_or_hard_deadline() {
        let start = Instant::now();
        let mut tracked = tracked_pane(start);
        tracked.last_stream_activity = Some(start + Duration::from_millis(900));

        assert!(!dirty_reconcile_due(
            &tracked,
            start + Duration::from_millis(999),
            Duration::from_millis(200),
            Duration::from_secs(1),
        ));
        assert!(dirty_reconcile_due(
            &tracked,
            start + Duration::from_secs(1),
            Duration::from_millis(200),
            Duration::from_secs(1),
        ));

        tracked.dirty_since = Some(start + Duration::from_millis(950));
        tracked.last_stream_activity = Some(start);
        assert!(dirty_reconcile_due(
            &tracked,
            start + Duration::from_millis(200),
            Duration::from_millis(200),
            Duration::from_secs(1),
        ));
    }

    #[test]
    fn dirty_reconcile_retains_generation_and_respects_retry() {
        let start = Instant::now();
        let mut tracked = tracked_pane(start);
        tracked.retry_at = Some(start + Duration::from_millis(250));
        assert!(!dirty_reconcile_due(
            &tracked,
            start + Duration::from_millis(200),
            Duration::from_millis(200),
            Duration::from_secs(1),
        ));
        assert!(dirty_reconcile_due(
            &tracked,
            start + Duration::from_millis(250),
            Duration::from_millis(200),
            Duration::from_secs(1),
        ));
        assert!(reconcile_clears_dirty(7, 7));
        assert!(!reconcile_clears_dirty(8, 7));

        tracked.stream_generation = 2;
        tracked.last_stream_activity = Some(start + Duration::from_millis(100));
        complete_reconcile_generation(&mut tracked, 1);
        assert_eq!(tracked.reconciled_generation, 1);
        assert_eq!(
            tracked.dirty_since,
            Some(start + Duration::from_millis(100))
        );
        assert!(!dirty_reconcile_due(
            &tracked,
            start + Duration::from_millis(150),
            Duration::from_millis(200),
            Duration::from_secs(1),
        ));
    }

    #[test]
    fn raced_generation_blocks_automatic_action_until_stable_capture() {
        let start = Instant::now();
        let mut tracked = tracked_pane(start);
        let key_sends = Cell::new(0);
        let mut permission_snapshot = sample_snapshot();
        permission_snapshot.classification = sample_classification(SessionState::PermissionDialog);
        permission_snapshot.desired_yolo_enabled = true;
        assert_eq!(
            yolo_workflow(&permission_snapshot),
            Some(GuardedWorkflow::ApprovePermission)
        );

        tracked.stream_generation = 2;
        tracked.last_stream_activity = Some(start + Duration::from_millis(100));
        complete_reconcile_generation(&mut tracked, 1);
        run_automatic_action_if_generation_stable(tracked.stream_generation == 1, || {
            key_sends.set(key_sends.get() + 1);
            Ok(())
        })
        .unwrap();
        assert_eq!(key_sends.get(), 0);
        assert!(tracked.stream_generation > tracked.reconciled_generation);
        assert!(tracked.dirty_since.is_some());

        complete_reconcile_generation(&mut tracked, 2);
        run_automatic_action_if_generation_stable(tracked.stream_generation == 2, || {
            key_sends.set(key_sends.get() + 1);
            Ok(())
        })
        .unwrap();
        assert_eq!(key_sends.get(), 1);
        assert_eq!(tracked.stream_generation, tracked.reconciled_generation);
        assert!(tracked.dirty_since.is_none());
    }

    #[test]
    fn retry_backoff_is_bounded() {
        assert_eq!(retry_delay(0), Duration::from_millis(250));
        assert_eq!(retry_delay(1), Duration::from_millis(500));
        assert_eq!(retry_delay(2), Duration::from_secs(1));
        assert_eq!(retry_delay(3), Duration::from_secs(2));
        assert_eq!(retry_delay(4), Duration::from_secs(5));
        assert_eq!(retry_delay(u8::MAX), Duration::from_secs(5));

        let start = Instant::now();
        let mut pending = PendingReconcile::new(ReconcileReason::Dirty);
        pending.failed(start);
        assert!(!pending.is_due(start + Duration::from_millis(249)));
        assert!(pending.is_due(start + Duration::from_millis(250)));
        assert_eq!(pending.reason, ReconcileReason::Retry);
    }

    #[test]
    fn stream_output_only_marks_dirty_and_never_publishes_stale_snapshot() {
        let shared = Arc::new(Mutex::new(RuntimeShared::new(Path::new("/tmp/runtime-test.sock"), String::from("test-run"))));
        let now = Instant::now();
        shared
            .lock()
            .unwrap()
            .panes
            .insert(String::from("%1"), tracked_pane(now));
        let (tx, rx) = mpsc::sync_channel(2);
        shared.lock().unwrap().subscribers.push(tx);

        mark_control_output_activity_shared(&shared, "%output %1 new-output");
        assert_eq!(shared.lock().unwrap().panes["%1"].stream_generation, 2);
        assert_eq!(
            handle_queued_control_line_shared(&shared, "%output %1 new-output"),
            ControlLineEffect::None
        );
        let shared = shared.lock().unwrap();
        assert_eq!(shared.panes["%1"].stream_generation, 2);
        assert!(
            shared.panes["%1"]
                .snapshot
                .live_excerpt
                .contains("new-output")
        );
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn snapshot_signature_covers_consumer_content_but_not_timing() {
        let snapshot = sample_snapshot();
        let signature = SnapshotSignature::from_snapshot(&snapshot, Some(String::from("provider")));
        let mut timing_only = snapshot.clone();
        timing_only.revision += 1;
        timing_only.updated_at_unix_ms += 1;
        timing_only.wait_duration_ms = Some(999);
        timing_only.cook_duration_ms = Some(999);
        timing_only.duration_sampled_at_unix_ms = Some(999);
        assert_eq!(
            signature,
            SnapshotSignature::from_snapshot(&timing_only, Some(String::from("provider")))
        );

        let mut changed = snapshot.clone();
        changed.classification.has_questions = true;
        assert_ne!(
            signature,
            SnapshotSignature::from_snapshot(&changed, Some(String::from("provider")))
        );
        let mut changed = snapshot.clone();
        changed.raw_source.push_str(" changed");
        assert_ne!(
            signature,
            SnapshotSignature::from_snapshot(&changed, Some(String::from("provider")))
        );
        let mut changed = snapshot;
        changed.last_action = Some(String::from("approve"));
        assert_ne!(
            signature,
            SnapshotSignature::from_snapshot(&changed, Some(String::from("provider")))
        );
    }

    #[test]
    fn snapshot_signature_ignores_volatile_tmux_metadata_but_tracks_path_and_identity() {
        let snapshot = sample_snapshot();
        let signature = SnapshotSignature::from_snapshot(&snapshot, None);
        let mut volatile_only = snapshot.clone();
        volatile_only.pane.window_name = String::from("botctl title prefix");
        volatile_only.pane.pane_title = String::from("shell title");
        volatile_only.pane.pane_active = false;
        volatile_only.pane.cursor_x = Some(20);
        volatile_only.pane.cursor_y = Some(10);
        assert_eq!(
            signature,
            SnapshotSignature::from_snapshot(&volatile_only, None)
        );

        let mut changed_path = snapshot.clone();
        changed_path.pane.current_path = String::from("/tmp/other");
        assert_ne!(
            signature,
            SnapshotSignature::from_snapshot(&changed_path, None)
        );
        let mut replacement = snapshot;
        replacement.pane.pane_pid = Some(9999);
        assert_ne!(
            signature,
            SnapshotSignature::from_snapshot(&replacement, None)
        );
    }

    #[test]
    fn snapshot_decodes_without_additive_cook_duration() {
        let mut encoded = serde_json::to_value(sample_snapshot()).unwrap();
        encoded.as_object_mut().unwrap().remove("cook_duration_ms");

        let decoded: RuntimePaneSnapshot = serde_json::from_value(encoded).unwrap();
        assert_eq!(decoded.cook_duration_ms, None);
    }

    #[test]
    fn snapshot_decodes_without_additive_duration_sample_time() {
        let mut encoded = serde_json::to_value(sample_snapshot()).unwrap();
        encoded
            .as_object_mut()
            .unwrap()
            .remove("duration_sampled_at_unix_ms");

        let decoded: RuntimePaneSnapshot = serde_json::from_value(encoded).unwrap();
        assert_eq!(decoded.duration_sampled_at_unix_ms, None);
    }

    #[test]
    fn observer_lifecycle_uses_stable_ids_and_ignores_stale_generations() {
        let removed_cancel = Arc::new(AtomicBool::new(false));
        let retained_cancel = Arc::new(AtomicBool::new(false));
        let mut active = BTreeMap::from([
            (
                String::from("$1"),
                ActiveObserver {
                    generation: 7,
                    cancel: Arc::clone(&removed_cancel),
                },
            ),
            (
                String::from("$2"),
                ActiveObserver {
                    generation: 9,
                    cancel: Arc::clone(&retained_cancel),
                },
            ),
        ]);
        let mut retries = BTreeMap::from([
            (String::from("$1"), (1, Instant::now())),
            (String::from("$2"), (1, Instant::now())),
        ]);

        retain_candidate_observers(
            &mut active,
            &mut retries,
            &BTreeSet::from([String::from("$2")]),
        );

        assert!(removed_cancel.load(Ordering::SeqCst));
        assert!(!retained_cancel.load(Ordering::SeqCst));
        assert!(!active.contains_key("$1"));
        assert!(!retries.contains_key("$1"));
        assert!(observer_is_current(&active, "$2", 9));
        assert!(!observer_is_current(&active, "$2", 8));
    }

    #[test]
    fn runtime_wait_policy_matches_shared_provider_policy() {
        assert!(is_waiting_state(SessionState::ChatReady));
        assert!(is_waiting_state(SessionState::PromptEditing));
        assert!(is_waiting_state(SessionState::PermissionDialog));
        assert!(is_waiting_state(SessionState::FolderTrustPrompt));
        assert!(is_waiting_state(SessionState::AgyCommandPermissionPrompt));
        assert!(!is_waiting_state(SessionState::BusyResponding));
        assert!(!is_waiting_state(SessionState::AgyFolderTrustPrompt));
        assert!(!is_waiting_state(SessionState::AgySettingsPersistPrompt));
    }

    #[test]
    fn topology_notifications_include_renames() {
        assert!(notification_requires_reconcile("%window-add @1"));
        assert!(notification_requires_reconcile("%window-renamed @1 name"));
        assert!(notification_requires_reconcile("%session-renamed $1 name"));
        assert!(!notification_requires_reconcile("%output %1 text"));
    }

    #[test]
    fn full_subscriber_is_dropped_without_blocking_runtime() {
        let shared = Arc::new(Mutex::new(RuntimeShared::new(Path::new("/tmp/runtime-test.sock"), String::from("test-run"))));
        let (tx, _rx) = mpsc::sync_channel(1);
        tx.try_send(RuntimeEvent {
            revision: 1,
            timestamp_unix_ms: 1,
            kind: String::from("filled"),
            pane_id: None,
            workspace_id: None,
            summary: String::new(),
            snapshot: None,
        })
        .unwrap();
        shared.lock().unwrap().subscribers.push(tx);

        emit_shared_event(&shared, None, None, "next", String::new(), None);
        assert!(shared.lock().unwrap().subscribers.is_empty());
    }

    #[test]
    fn subscription_bootstrap_ends_at_ok_boundary_with_complete_backlog() {
        let shared = Arc::new(Mutex::new(RuntimeShared::new(Path::new("/tmp/runtime-test.sock"), String::from("test-run"))));
        shared
            .lock()
            .unwrap()
            .panes
            .insert(String::from("%1"), tracked_pane(Instant::now()));
        let (server, client) = UnixStream::pair().unwrap();
        let server_shared = Arc::clone(&shared);
        let server_thread = thread::spawn(move || handle_subscribe(server, &server_shared));
        let mut subscription = RuntimeSubscription {
            reader: BufReader::new(client),
        };

        let backlog = subscription.bootstrap().unwrap();
        assert_eq!(backlog.len(), 1);
        assert_eq!(backlog["%1"].pane.pane_id, "%1");

        drop(subscription);
        emit_shared_event(
            &shared,
            None,
            None,
            "wake-closed-subscriber",
            String::new(),
            None,
        );
        server_thread.join().unwrap().unwrap();
    }

    #[test]
    fn existing_subscription_receive_treats_bootstrap_marker_as_no_event() {
        let (mut server, client) = UnixStream::pair().unwrap();
        write_message(&mut server, &RuntimeResponse::Ok).unwrap();
        let mut subscription = RuntimeSubscription {
            reader: BufReader::new(client),
        };

        assert!(subscription.recv().unwrap().is_none());
    }

    #[test]
    fn action_and_post_approve_guards_reject_duplicate_work() {
        let shared = Arc::new(Mutex::new(RuntimeShared::new(Path::new("/tmp/runtime-test.sock"), String::from("test-run"))));
        let first = ActionGuard::acquire(&shared, "%1").unwrap();
        assert!(ActionGuard::acquire(&shared, "%1").is_err());
        drop(first);
        assert!(ActionGuard::acquire(&shared, "%1").is_ok());

        let mut tracked = tracked_pane(Instant::now());
        tracked.post_approve_guard_raw_source = Some(String::from("same-frame"));
        assert!(post_approve_guard_matches(&tracked, "same-frame"));
        assert!(!post_approve_guard_matches(&tracked, "changed-frame"));
    }

    #[test]
    fn stable_identity_excludes_mutable_topology_but_detects_process_replacement() {
        let original = sample_pane("claude");
        let mut renamed = original.clone();
        renamed.window_name = String::from("manual rename");
        renamed.current_path = String::from("/tmp/other");
        assert!(same_pane_identity(&original, &renamed));

        let mut replacement = original.clone();
        replacement.pane_pid = Some(9999);
        assert!(!same_pane_identity(&original, &replacement));
    }

    #[test]
    fn yolo_policy_update_is_immediately_reflected_in_snapshot_signature() {
        let shared = Arc::new(Mutex::new(RuntimeShared::new(Path::new("/tmp/runtime-test.sock"), String::from("test-run"))));
        shared
            .lock()
            .unwrap()
            .panes
            .insert(String::from("%1"), tracked_pane(Instant::now()));

        let snapshot = update_yolo_runtime_state(&shared, "%1", Some(true), true, None).unwrap();
        assert!(snapshot.desired_yolo_enabled);
        assert!(snapshot.actual_yolo_enabled);
        assert_eq!(snapshot.duration_sampled_at_unix_ms, Some(90));
        let shared = shared.lock().unwrap();
        assert_eq!(
            shared.panes["%1"].signature,
            SnapshotSignature::from_snapshot(
                &snapshot,
                shared.panes["%1"].signature.provider_session_id.clone(),
            )
        );
    }

    #[test]
    fn post_yolo_commit_atomically_updates_busy_and_ready_durations() {
        let shared = Arc::new(Mutex::new(RuntimeShared::new(Path::new("/tmp/runtime-test.sock"), String::from("test-run"))));
        let mut tracked = tracked_pane(Instant::now());
        tracked.snapshot.classification = sample_classification(SessionState::PermissionDialog);
        tracked.signature = SnapshotSignature::from_snapshot(
            &tracked.snapshot,
            tracked.signature.provider_session_id.clone(),
        );
        {
            let mut locked = shared.lock().unwrap();
            locked.next_revision = 10;
            locked.panes.insert(String::from("%1"), tracked);
        }

        let busy = InspectedPane {
            classification: sample_classification(SessionState::BusyResponding),
            focused_source: String::from("Working..."),
            raw_source: String::from("Working... esc to cancel"),
        };
        let busy_snapshot = commit_post_yolo_snapshot(
            &shared,
            "%1",
            &busy,
            String::from("approve-permission"),
            TmuxRuntimeDurations {
                wait_duration: None,
                cook_duration: Some(Duration::from_millis(75)),
                sampled_at_unix_ms: 1_900,
            },
            2_000,
        )
        .unwrap();
        assert_eq!(
            busy_snapshot.classification.state,
            SessionState::BusyResponding
        );
        assert_eq!(busy_snapshot.wait_duration_ms, None);
        assert_eq!(busy_snapshot.cook_duration_ms, Some(75));
        assert_eq!(busy_snapshot.duration_sampled_at_unix_ms, Some(1_900));
        assert_eq!(busy_snapshot.updated_at_unix_ms, 2_000);
        assert_eq!(busy_snapshot.revision, 11);

        let ready = InspectedPane {
            classification: sample_classification(SessionState::ChatReady),
            focused_source: String::from("> "),
            raw_source: String::from("> "),
        };
        let ready_snapshot = commit_post_yolo_snapshot(
            &shared,
            "%1",
            &ready,
            String::from("approve-permission"),
            TmuxRuntimeDurations {
                wait_duration: Some(Duration::from_millis(5)),
                cook_duration: Some(Duration::from_millis(80)),
                sampled_at_unix_ms: 2_900,
            },
            3_000,
        )
        .unwrap();
        assert_eq!(ready_snapshot.classification.state, SessionState::ChatReady);
        assert_eq!(ready_snapshot.wait_duration_ms, Some(5));
        assert_eq!(ready_snapshot.cook_duration_ms, Some(80));
        assert_eq!(ready_snapshot.duration_sampled_at_unix_ms, Some(2_900));
        assert_eq!(ready_snapshot.updated_at_unix_ms, 3_000);
        assert_eq!(ready_snapshot.revision, 12);
        let locked = shared.lock().unwrap();
        assert_eq!(
            locked.panes["%1"].snapshot.classification,
            ready_snapshot.classification
        );
        assert_eq!(
            locked.panes["%1"].snapshot.wait_duration_ms,
            ready_snapshot.wait_duration_ms
        );
        assert_eq!(
            locked.panes["%1"].snapshot.cook_duration_ms,
            ready_snapshot.cook_duration_ms
        );
        assert_eq!(
            locked.panes["%1"].snapshot.updated_at_unix_ms,
            ready_snapshot.updated_at_unix_ms
        );
        assert_eq!(
            locked.panes["%1"].signature,
            SnapshotSignature::from_snapshot(
                &ready_snapshot,
                locked.panes["%1"].signature.provider_session_id.clone(),
            )
        );
    }
}
