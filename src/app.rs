use serde::{Deserialize, Serialize};
use std::collections::hash_map::DefaultHasher;
use std::fmt;
use std::fs::{self, File};
use std::hash::{Hash, Hasher};
use std::io::{self, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread;
use std::time::{Duration, Instant};
use time::OffsetDateTime;
use time::macros::format_description;

use crate::automation::{
    AutomationAction, GuardedWorkflow, KeybindingsInspection, KeybindingsStatus,
    ResolvedKeybindings, inspect_keybindings, install_recommended_keybindings,
    load_resolved_keybindings, prompt_submission_sequence, render_keybindings_json,
    validate_workflow_state,
};
use crate::classifier::{
    Classification, Classifier, SIGNAL_CODEX_KEYWORDS, SIGNAL_SELF_SETTINGS_LANGUAGE,
    SIGNAL_SENSITIVE_CLAUDE_PATH, SessionState, has_startup_choice_prompt_text,
    prepare_frame_for_classification,
};
use crate::cli::{
    AttachArgs, AutoUnstickArgs, BabysitFormat, CaptureArgs, ClassifyArgs, Command,
    ContinueSessionArgs, DashboardArgs, DoctorArgs, EditorHelperArgs, InstallBindingsArgs,
    KeepGoingArgs, LastMessageArgs, ListPanesArgs, McpArgs, McpTransportArgs, ObserveArgs,
    PaneCommandArgs, PaneTargetArgs, PreparePromptArgs, PromptRunArgs, RecordFixtureArgs,
    ReplayArgs, RuntimeArgs, SendActionArgs, ServeArgs, StartArgs, StatusArgs, SubmitPromptArgs,
    YoloStartArgs, YoloStopArgs,
};
use crate::dashboard;
use crate::fixtures::{FixtureCase, FixtureRecordInput, record_case};
use crate::http_api::spawn_http_server;
use crate::last_message::{
    default_output_path, line_count, load_last_agent_message, output_path_is_stdout,
};
use crate::observe::{CollectedObservation, ObserveRequest, collect_observation};
use crate::prompt::{
    PromptSource, prepare_prompt, resolve_prompt_text, resolve_state_dir, write_editor_target,
    write_editor_target_from_pending,
};
use crate::runtime::{RuntimeClient, RuntimeServerConfig, run_runtime_server};
use crate::serve::ServeRequest;
use crate::storage::{
    WorkspaceRecord, bootstrap_state_db, capture_artifact_path, export_artifact_path,
    resolve_workspace, resolve_workspace_for_path, state_db_path,
    store_pending_prompt_for_tmux_instance, tape_artifact_path,
};
use crate::tmux::{StartSessionRequest, StartWindowRequest, StartedWindow, TmuxClient, TmuxPane};
#[cfg(test)]
use crate::yolo::{YoloRecord, disable_yolo_record};

pub(crate) const ACTION_GUARD_HISTORY_LINES: usize = 120;
const AUTO_UNSTICK_STEP_DELAY_MS: u64 = 150;
const KEEP_GOING_HISTORY_LINES: usize = 2000;
const KEEP_GOING_PROMPT_ANCHOR: &str = "Audit the task currently in scope";
const KEEP_GOING_CUSTOM_PROMPT_ANCHOR: &str = "[[BOTCTL_KEEP_GOING_END_PROMPT]]";
const CODEX_PERMISSION_CONFIRM_FOOTER: &str = "press enter to confirm or esc to cancel";
const PROMPT_SUBMISSION_POLL_MS: u64 = 100;
const PROMPT_SUBMISSION_TIMEOUT_MS: u64 = 5000;
const DASHBOARD_PERSISTENT_SOCKET: &str = "botctl-dashboard";
const DASHBOARD_PERSISTENT_SESSION: &str = "botctl-dashboard";
const DASHBOARD_PERSISTENT_WINDOW: &str = "dashboard";
const DASHBOARD_PERSISTENT_CHILD_ENV: &str = "BOTCTL_DASHBOARD_PERSISTENT_CHILD";
const DASHBOARD_TARGET_TMUX_SOCKET_ENV: &str = "BOTCTL_DASHBOARD_TARGET_TMUX_SOCKET";
const DEFAULT_PROMPT_SESSION: &str = "botctl";
const RUNTIME_TARGET_TMUX_SOCKET_ENV: &str = "BOTCTL_RUNTIME_TARGET_TMUX_SOCKET";
const RUNTIME_BACKGROUND_WINDOW: &str = "runtime";
const RUNTIME_MANAGER_METADATA_FILE: &str = "runtime-manager.json";
const RUNTIME_MANAGER_LEASES_DIR: &str = "runtime-leases";
const RUNTIME_AUTOSTART_TIMEOUT_MS: u64 = 5_000;
const RUNTIME_AUTOSTART_POLL_MS: u64 = 100;
const KEEP_GOING_PROMPT: &str = r#"Audit the task currently in scope — only what the user explicitly asked for, not nice-to-haves or tangents you noticed.

Check: every deliverable produced (not just described), every file actually written, every stated acceptance criterion met, every test/build you committed to actually run and passing, no unresolved TODOs or stubs in code you just wrote.

End your reply with EXACTLY one token as the literal last non-empty line:

- `OKIE_DOKIE` — before that final token, include a brief note of what's left, then immediately continue working on it. No asking permission.
- `ALL_DONE` — before that final token, include a one- or two-sentence summary. Stop. No new work, no unsolicited next steps.
- `PANIC` — before that final token, include: what's done, what's blocking, what you tried, and the specific input you need. Do not use this just because something is hard — only when continuing would be guessing. Do NOT commit or push when emitting PANIC.

Before emitting `ALL_DONE`, perform these git steps in order:

1. **Commit.** Run `git status` to confirm there are changes to commit. Stage the files you modified (`git add <paths>` — do not `git add .` blindly; skip anything unrelated to the task). Write a concise, conventional commit message describing the change. If the working tree is already clean, skip to step 2.

2. **Push — only if on a feature branch.** Run `git rev-parse --abbrev-ref HEAD`. If the branch is `main`, `master`, `develop`, `dev`, `trunk`, or `release/*`, do NOT push — just note in your summary that the commit is local and the user should push manually. Otherwise, `git push` (use `-u origin <branch>` if upstream isn't set).

3. **PR/MR — only if the user explicitly requested one in this conversation.** Re-read the user's messages. If they asked for a PR, MR, pull request, or merge request as part of the completion, open it now (`gh pr create` or equivalent). If they did not, do not open one and do not suggest it.

If any git step fails (push rejected, auth failure, merge conflict, etc.), switch to `PANIC` with what you tried.

Be honest: unsure if something works → `OKIE_DOKIE`, go verify. Don't use `ALL_DONE` just because the turn got long. Don't use `OKIE_DOKIE` to invent new work. Don't use `PANIC` to avoid effort."#;

pub type AppResult<T> = Result<T, AppError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RuntimeManagementMode {
    Managed,
    Unmanaged,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
enum RuntimeLaunchMode {
    Manual,
    AutoManaged,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RuntimeManagerMetadata {
    mode: RuntimeLaunchMode,
    session_name: Option<String>,
    tmux_socket_path: Option<String>,
}

#[derive(Debug, Clone, Copy)]
struct RuntimeBootstrapConfig {
    reconcile_ms: u64,
    history_lines: usize,
}

struct ManagedRuntimeGuard {
    state_dir: PathBuf,
    lease_path: Option<PathBuf>,
    should_consider_stop: bool,
}

impl ManagedRuntimeGuard {
    fn noop(state_dir: PathBuf) -> Self {
        Self {
            state_dir,
            lease_path: None,
            should_consider_stop: false,
        }
    }
}

impl Drop for ManagedRuntimeGuard {
    fn drop(&mut self) {
        if let Some(lease_path) = self.lease_path.take() {
            let _ = fs::remove_file(&lease_path);
        }
        if !self.should_consider_stop {
            return;
        }
        match should_stop_auto_runtime(&self.state_dir) {
            Ok(true) => {
                if let Err(error) = stop_runtime_process(&self.state_dir) {
                    eprintln!("warning: failed to stop managed runtime: {error}");
                }
            }
            Ok(false) => {}
            Err(error) => {
                eprintln!("warning: failed to evaluate managed runtime shutdown: {error}");
            }
        }
    }
}

#[derive(Debug)]
pub struct AppError {
    message: String,
    exit_code: i32,
}

impl AppError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            exit_code: 1,
        }
    }

    pub fn with_exit_code(message: impl Into<String>, exit_code: i32) -> Self {
        Self {
            message: message.into(),
            exit_code,
        }
    }

    pub fn exit_code(&self) -> i32 {
        self.exit_code
    }
}

impl fmt::Display for AppError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for AppError {}

impl From<std::io::Error> for AppError {
    fn from(value: std::io::Error) -> Self {
        Self::new(value.to_string())
    }
}

impl From<rusqlite::Error> for AppError {
    fn from(value: rusqlite::Error) -> Self {
        Self::new(value.to_string())
    }
}

pub fn run(command: Command) -> AppResult<String> {
    match command {
        Command::Start(args) => run_start(args),
        Command::Attach(args) => run_attach(args),
        Command::ListPanes(args) => run_list_panes(args),
        Command::Capture(args) => run_capture(args),
        Command::LastMessage(args) => run_last_message(args),
        Command::Status(args) => run_status(args),
        Command::Doctor(args) => run_doctor(args),
        Command::Observe(args) => run_observe(args),
        Command::Runtime(args) => run_runtime(args),
        Command::Serve(args) => run_serve(args),
        Command::Dashboard(args) => run_dashboard(args),
        Command::RecordFixture(args) => run_record_fixture(args),
        Command::Classify(args) => run_classify(args),
        Command::Replay(args) => run_replay(args),
        Command::Bindings => Ok(render_keybindings_json()),
        Command::InstallBindings(args) => run_install_bindings(args),
        Command::SendAction(args) => run_send_action(args),
        Command::ApprovePermission(args) => {
            run_guarded_pane_workflow(args, GuardedWorkflow::ApprovePermission)
        }
        Command::RejectPermission(args) => {
            run_guarded_pane_workflow(args, GuardedWorkflow::RejectPermission)
        }
        Command::DismissSurvey(args) => {
            run_guarded_pane_workflow(args, GuardedWorkflow::DismissSurvey)
        }
        Command::ContinueSession(args) => run_continue_session(args),
        Command::AutoUnstick(args) => run_auto_unstick(args),
        Command::KeepGoing(args) => run_keep_going(args),
        Command::Prompt(args) => run_prompt(args),
        Command::Mcp(args) => run_mcp(args),
        Command::PreparePrompt(args) => run_prepare_prompt(args),
        Command::EditorHelper(args) => run_editor_helper(args),
        Command::SubmitPrompt(args) => run_submit_prompt(args),
        Command::YoloStart(args) => run_yolo_start(args),
        Command::YoloStop(args) => run_yolo_stop(args),
        Command::Version => Ok(crate::cli::version()),
        Command::Help(args) => Ok(crate::cli::usage_for(&args)),
    }
}

fn run_mcp(args: McpArgs) -> AppResult<String> {
    match args.transport {
        McpTransportArgs::Stdio => crate::mcp_stdio::run_stdio(args.state_dir.as_deref())?,
        McpTransportArgs::Http {
            bind,
            allow_non_loopback,
        } => crate::mcp_http::run_http(&bind, args.state_dir.as_deref(), allow_non_loopback)?,
    }
    Ok(String::new())
}

fn run_dashboard(args: DashboardArgs) -> AppResult<String> {
    if args.persistent && std::env::var_os(DASHBOARD_PERSISTENT_CHILD_ENV).is_none() {
        return run_persistent_dashboard(args);
    }
    let state_dir = resolve_bootstrapped_state_dir(args.state_dir.as_deref())?;
    let (_runtime_client, _runtime_guard) = ensure_runtime_client(
        &state_dir,
        if args.unmanaged {
            RuntimeManagementMode::Unmanaged
        } else {
            RuntimeManagementMode::Managed
        },
        RuntimeBootstrapConfig {
            reconcile_ms: 1000,
            history_lines: args.history_lines.max(200),
        },
    )?;
    dashboard::run(args)
}

fn run_runtime(args: RuntimeArgs) -> AppResult<String> {
    let state_dir = resolve_bootstrapped_state_dir(args.state_dir.as_deref())?;
    if args.stop {
        stop_runtime_process(&state_dir)?;
        return Ok(String::from("stopped runtime"));
    }
    if !args.foreground {
        let client = start_background_runtime(
            &state_dir,
            RuntimeBootstrapConfig {
                reconcile_ms: args.reconcile_ms,
                history_lines: args.history_lines,
            },
            RuntimeLaunchMode::Manual,
        )?;
        let status = client.get_status()?;
        return Ok(format!("runtime listening on {}", status.socket_path));
    }

    let metadata = load_runtime_manager_metadata(&state_dir)?;
    if metadata.is_none() {
        write_runtime_manager_metadata(
            &state_dir,
            &RuntimeManagerMetadata {
                mode: RuntimeLaunchMode::Manual,
                session_name: None,
                tmux_socket_path: managed_runtime_target_socket_path()
                    .as_ref()
                    .map(|path| path.display().to_string()),
            },
        )?;
    }
    let result = run_runtime_server(RuntimeServerConfig {
        state_dir: state_dir.clone(),
        history_lines: args.history_lines,
        reconcile_ms: args.reconcile_ms,
    });
    let _ = remove_runtime_manager_metadata(&state_dir);
    result
}

fn run_persistent_dashboard(args: DashboardArgs) -> AppResult<String> {
    let persistent_client = TmuxClient::with_socket(DASHBOARD_PERSISTENT_SOCKET);
    let state_dir = resolve_state_dir(args.state_dir.as_deref())?;
    let current_pane_id = dashboard::current_dashboard_pane_id(&TmuxClient::default());
    let desired_target_socket = current_tmux_socket_path();
    if persistent_client.has_session(DASHBOARD_PERSISTENT_SESSION)? {
        let current_target_socket = persistent_dashboard_target_socket(&persistent_client)?;
        if current_target_socket != desired_target_socket {
            persistent_client.kill_session(DASHBOARD_PERSISTENT_SESSION)?;
        }
    }

    if !persistent_client.has_session(DASHBOARD_PERSISTENT_SESSION)? {
        start_persistent_dashboard_session(
            &persistent_client,
            &args,
            desired_target_socket.as_deref(),
        )?;
    }

    persistent_client.set_global_option("status", "off")?;
    dashboard::request_dashboard_pane_selection(&state_dir, current_pane_id.as_deref())?;

    persistent_client.attach_session(DASHBOARD_PERSISTENT_SESSION)?;
    Ok(String::new())
}

fn start_persistent_dashboard_session(
    persistent_client: &TmuxClient,
    args: &DashboardArgs,
    target_socket_path: Option<&Path>,
) -> AppResult<()> {
    let cwd = std::env::current_dir()?;
    let request = StartSessionRequest {
        session_name: String::from(DASHBOARD_PERSISTENT_SESSION),
        window_name: String::from(DASHBOARD_PERSISTENT_WINDOW),
        cwd,
        command: persistent_dashboard_child_command_with_target_socket(args, target_socket_path)?,
    };
    persistent_client.start_session(&request)?;
    Ok(())
}

fn persistent_dashboard_child_command_with_target_socket(
    args: &DashboardArgs,
    target_socket_path: Option<&Path>,
) -> AppResult<String> {
    let exe = std::env::current_exe()?;
    let mut parts = vec![format!("{}=1", DASHBOARD_PERSISTENT_CHILD_ENV)];
    if let Some(socket_path) = target_socket_path {
        parts.push(format!(
            "{}={}",
            DASHBOARD_TARGET_TMUX_SOCKET_ENV,
            shell_escape(socket_path.to_string_lossy().as_ref())
        ));
    }
    parts.push(shell_escape(exe.to_string_lossy().as_ref()));
    parts.push(String::from("dashboard"));
    parts.push(String::from("--persistent"));
    parts.push(String::from("--poll-ms"));
    parts.push(args.poll_ms.to_string());
    parts.push(String::from("--history-lines"));
    parts.push(args.history_lines.to_string());
    if let Some(state_dir) = &args.state_dir {
        parts.push(String::from("--state-dir"));
        parts.push(shell_escape(state_dir.to_string_lossy().as_ref()));
    }
    if args.unmanaged {
        parts.push(String::from("--unmanaged"));
    }
    Ok(parts.join(" "))
}

fn shell_escape(value: &str) -> String {
    if value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '_' | '-' | '.' | '%' | ':'))
    {
        value.to_string()
    } else {
        format!("'{}'", value.replace('\'', "'\"'\"'"))
    }
}

fn current_tmux_socket_path() -> Option<PathBuf> {
    let tmux = std::env::var_os("TMUX")?;
    tmux_socket_path_from_value(tmux.to_string_lossy().as_ref())
}

fn managed_runtime_target_socket_path() -> Option<PathBuf> {
    match std::env::var_os(DASHBOARD_TARGET_TMUX_SOCKET_ENV) {
        Some(socket_path) if !socket_path.is_empty() => Some(PathBuf::from(socket_path)),
        _ => current_tmux_socket_path(),
    }
}

fn managed_runtime_tmux_client() -> TmuxClient {
    match managed_runtime_target_socket_path() {
        Some(socket_path) => TmuxClient::with_socket_path(socket_path),
        None => TmuxClient::default(),
    }
}

fn runtime_manager_metadata_path(state_dir: &Path) -> PathBuf {
    state_dir.join(RUNTIME_MANAGER_METADATA_FILE)
}

fn runtime_manager_leases_dir(state_dir: &Path) -> PathBuf {
    state_dir.join(RUNTIME_MANAGER_LEASES_DIR)
}

fn runtime_session_name(state_dir: &Path) -> String {
    let mut hasher = DefaultHasher::new();
    state_dir.hash(&mut hasher);
    format!("botctl-runtime-{:x}", hasher.finish())
}

fn load_runtime_manager_metadata(state_dir: &Path) -> AppResult<Option<RuntimeManagerMetadata>> {
    let path = runtime_manager_metadata_path(state_dir);
    if !path.exists() {
        return Ok(None);
    }
    let content = fs::read_to_string(&path)?;
    let metadata = serde_json::from_str(&content)
        .map_err(|error| AppError::new(format!("failed to parse {}: {error}", path.display())))?;
    Ok(Some(metadata))
}

fn write_runtime_manager_metadata(
    state_dir: &Path,
    metadata: &RuntimeManagerMetadata,
) -> AppResult<()> {
    fs::create_dir_all(state_dir)?;
    let content = serde_json::to_string_pretty(metadata)
        .map_err(|error| AppError::new(format!("failed to encode runtime metadata: {error}")))?;
    fs::write(runtime_manager_metadata_path(state_dir), content)?;
    Ok(())
}

fn remove_runtime_manager_metadata(state_dir: &Path) -> AppResult<()> {
    let path = runtime_manager_metadata_path(state_dir);
    if path.exists() {
        fs::remove_file(path)?;
    }
    Ok(())
}

fn create_runtime_lease(state_dir: &Path) -> AppResult<PathBuf> {
    let lease_dir = runtime_manager_leases_dir(state_dir);
    fs::create_dir_all(&lease_dir)?;
    let lease_path = lease_dir.join(format!("{}-{}", process::id(), current_unix_millis()?));
    fs::write(&lease_path, process::id().to_string())?;
    Ok(lease_path)
}

fn live_runtime_leases(state_dir: &Path) -> AppResult<Vec<PathBuf>> {
    let lease_dir = runtime_manager_leases_dir(state_dir);
    if !lease_dir.exists() {
        return Ok(Vec::new());
    }
    let mut live = Vec::new();
    for entry in fs::read_dir(&lease_dir)? {
        let entry = entry?;
        let path = entry.path();
        let Ok(content) = fs::read_to_string(&path) else {
            let _ = fs::remove_file(&path);
            continue;
        };
        let Ok(pid) = content.trim().parse::<u32>() else {
            let _ = fs::remove_file(&path);
            continue;
        };
        if process_exists(pid) {
            live.push(path);
        } else {
            let _ = fs::remove_file(path);
        }
    }
    Ok(live)
}

fn process_exists(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }

    let result = unsafe { libc::kill(pid as libc::pid_t, 0) };
    if result == 0 {
        return true;
    }

    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

fn current_unix_millis() -> AppResult<u128> {
    Ok(std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|error| AppError::new(format!("system clock before unix epoch: {error}")))?
        .as_millis())
}

fn background_runtime_command(
    state_dir: &Path,
    config: RuntimeBootstrapConfig,
    target_socket_path: Option<&Path>,
) -> AppResult<String> {
    let exe = std::env::current_exe()?;
    let mut parts = Vec::new();
    if let Some(socket_path) = target_socket_path {
        parts.push(format!(
            "{}={}",
            RUNTIME_TARGET_TMUX_SOCKET_ENV,
            shell_escape(socket_path.to_string_lossy().as_ref())
        ));
    }
    parts.push(shell_escape(exe.to_string_lossy().as_ref()));
    parts.push(String::from("runtime"));
    parts.push(String::from("--foreground"));
    parts.push(String::from("--reconcile-ms"));
    parts.push(config.reconcile_ms.to_string());
    parts.push(String::from("--history-lines"));
    parts.push(config.history_lines.to_string());
    parts.push(String::from("--state-dir"));
    parts.push(shell_escape(state_dir.to_string_lossy().as_ref()));
    Ok(parts.join(" "))
}

fn wait_for_runtime(state_dir: &Path) -> AppResult<RuntimeClient> {
    let client = RuntimeClient::connect(state_dir)?;
    let deadline = std::time::Instant::now() + Duration::from_millis(RUNTIME_AUTOSTART_TIMEOUT_MS);
    loop {
        match client.get_status() {
            Ok(_) => return Ok(client),
            Err(error) if std::time::Instant::now() < deadline => {
                if !matches!(error.exit_code(), 1) {
                    return Err(error);
                }
                thread::sleep(Duration::from_millis(RUNTIME_AUTOSTART_POLL_MS));
            }
            Err(error) => return Err(error),
        }
    }
}

fn start_background_runtime(
    state_dir: &Path,
    config: RuntimeBootstrapConfig,
    mode: RuntimeLaunchMode,
) -> AppResult<RuntimeClient> {
    let tmux_client = managed_runtime_tmux_client();
    let session_name = runtime_session_name(state_dir);
    let target_socket_path = managed_runtime_target_socket_path();
    write_runtime_manager_metadata(
        state_dir,
        &RuntimeManagerMetadata {
            mode,
            session_name: Some(session_name.clone()),
            tmux_socket_path: target_socket_path
                .as_ref()
                .map(|path| path.display().to_string()),
        },
    )?;

    if tmux_client.has_session(&session_name)? {
        if let Ok(client) = wait_for_runtime(state_dir) {
            return Ok(client);
        }
        tmux_client.kill_session(&session_name)?;
    }

    let request = StartSessionRequest {
        session_name,
        window_name: String::from(RUNTIME_BACKGROUND_WINDOW),
        cwd: std::env::current_dir()?,
        command: background_runtime_command(state_dir, config, target_socket_path.as_deref())?,
    };
    tmux_client.start_session(&request)?;
    wait_for_runtime(state_dir)
}

fn ensure_runtime_client(
    state_dir: &Path,
    management: RuntimeManagementMode,
    config: RuntimeBootstrapConfig,
) -> AppResult<(RuntimeClient, ManagedRuntimeGuard)> {
    if management == RuntimeManagementMode::Unmanaged {
        let client = RuntimeClient::connect(state_dir)?;
        client.get_status()?;
        return Ok((client, ManagedRuntimeGuard::noop(state_dir.to_path_buf())));
    }

    let client = RuntimeClient::connect(state_dir)?;
    let client = match client.get_status() {
        Ok(_) => client,
        Err(_) => start_background_runtime(state_dir, config, RuntimeLaunchMode::AutoManaged)?,
    };
    let lease_path = create_runtime_lease(state_dir)?;
    let should_consider_stop = matches!(
        load_runtime_manager_metadata(state_dir)?.map(|metadata| metadata.mode),
        Some(RuntimeLaunchMode::AutoManaged)
    );
    Ok((
        client,
        ManagedRuntimeGuard {
            state_dir: state_dir.to_path_buf(),
            lease_path: Some(lease_path),
            should_consider_stop,
        },
    ))
}

fn runtime_has_active_yolo(state_dir: &Path) -> AppResult<bool> {
    let client = RuntimeClient::connect(state_dir)?;
    Ok(client
        .list_panes(None, None)?
        .into_iter()
        .any(|snapshot| snapshot.desired_yolo_enabled))
}

fn should_stop_auto_runtime(state_dir: &Path) -> AppResult<bool> {
    let metadata = load_runtime_manager_metadata(state_dir)?;
    if !matches!(
        metadata.map(|item| item.mode),
        Some(RuntimeLaunchMode::AutoManaged)
    ) {
        return Ok(false);
    }
    if !live_runtime_leases(state_dir)?.is_empty() {
        return Ok(false);
    }
    if runtime_has_active_yolo(state_dir).unwrap_or(false) {
        return Ok(false);
    }
    Ok(true)
}

fn stop_runtime_process(state_dir: &Path) -> AppResult<()> {
    let client = RuntimeClient::connect(state_dir)?;
    if client.stop().is_ok() {
        let deadline =
            std::time::Instant::now() + Duration::from_millis(RUNTIME_AUTOSTART_TIMEOUT_MS);
        while std::time::Instant::now() < deadline {
            if client.get_status().is_err() {
                let _ = remove_runtime_manager_metadata(state_dir);
                return Ok(());
            }
            thread::sleep(Duration::from_millis(RUNTIME_AUTOSTART_POLL_MS));
        }
    }

    if let Some(metadata) = load_runtime_manager_metadata(state_dir)?
        && let Some(session_name) = metadata.session_name
    {
        let tmux_client = match metadata.tmux_socket_path {
            Some(socket_path) => TmuxClient::with_socket_path(socket_path),
            None => TmuxClient::default(),
        };
        if tmux_client.has_session(&session_name)? {
            tmux_client.kill_session(&session_name)?;
        }
    }
    remove_runtime_manager_metadata(state_dir)?;
    Ok(())
}

fn tmux_socket_path_from_value(value: &str) -> Option<PathBuf> {
    let socket_path = value.split(',').next()?.trim();
    if socket_path.is_empty() {
        None
    } else {
        Some(PathBuf::from(socket_path))
    }
}

fn persistent_dashboard_target_socket(
    persistent_client: &TmuxClient,
) -> AppResult<Option<PathBuf>> {
    let pane = persistent_client
        .list_panes_for_target(Some(DASHBOARD_PERSISTENT_SESSION))?
        .into_iter()
        .next();
    let Some(pane) = pane else {
        return Ok(None);
    };
    let Some(pid) = pane.pane_pid else {
        return Ok(None);
    };
    dashboard_target_socket_from_pid(pid)
}

fn dashboard_target_socket_from_pid(pid: u32) -> AppResult<Option<PathBuf>> {
    let path = PathBuf::from(format!("/proc/{pid}/environ"));
    let bytes = fs::read(&path).map_err(|error| {
        AppError::new(format!(
            "failed to read dashboard process environment {}: {error}",
            path.display()
        ))
    })?;
    Ok(
        parse_env_value_from_environ_bytes(&bytes, DASHBOARD_TARGET_TMUX_SOCKET_ENV)
            .map(PathBuf::from),
    )
}

fn parse_env_value_from_environ_bytes(bytes: &[u8], key: &str) -> Option<String> {
    let prefix = format!("{key}=");
    bytes
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
        .find_map(|entry| {
            let entry = std::str::from_utf8(entry).ok()?;
            entry.strip_prefix(&prefix).map(str::to_string)
        })
}

fn run_start(args: StartArgs) -> AppResult<String> {
    let cwd = match args.cwd {
        Some(cwd) => cwd,
        None => std::env::current_dir()?,
    };

    let request = StartSessionRequest {
        session_name: args.session_name,
        window_name: args.window_name,
        cwd,
        command: args.command,
    };

    let client = managed_runtime_tmux_client();

    if args.dry_run {
        Ok(client.plan_start_session(&request).render())
    } else {
        let started = client.start_session(&request)?;
        Ok(format!(
            "started tmux session={} window={} cwd={}",
            started.session_name,
            started.window_name,
            started.cwd.display()
        ))
    }
}

fn run_attach(args: AttachArgs) -> AppResult<String> {
    let client = TmuxClient::default();
    let pane = resolve_target_pane(&client, &args.target)?;
    ensure_pane_owned_by_claude(&pane)?;

    Ok(format!(
        "attached pane={} session={} window={} command={} cwd={}",
        pane.pane_id, pane.session_name, pane.window_name, pane.current_command, pane.current_path
    ))
}

fn run_list_panes(args: ListPanesArgs) -> AppResult<String> {
    let panes = TmuxClient::default().list_panes()?;
    if args.json {
        render_list_panes_json(&panes, args.all)
    } else {
        Ok(render_list_panes(&panes, args.all))
    }
}

fn render_list_panes(panes: &[TmuxPane], include_all: bool) -> String {
    let panes = if include_all {
        panes.iter().collect::<Vec<_>>()
    } else {
        panes
            .iter()
            .filter(|pane| is_default_list_pane(pane))
            .collect::<Vec<_>>()
    };
    if panes.is_empty() {
        return if include_all {
            String::from("no panes found")
        } else {
            String::from("no managed agent panes found")
        };
    }

    let mut out = String::new();
    for pane in panes {
        let cursor = match (pane.cursor_x, pane.cursor_y) {
            (Some(x), Some(y)) => format!("{x},{y}"),
            _ => String::from("-"),
        };
        out.push_str(&format!(
            "{}\tsession={}\twindow={}\tpid={}\tactive={}\tprovider={}\tcommand={}\tcwd={}\tcursor={}\n",
            pane.pane_id,
            pane.session_name,
            pane.window_name,
            pane.pane_pid
                .map(|pid| pid.to_string())
                .unwrap_or_else(|| String::from("-")),
            pane.pane_active,
            pane_provider_label(pane),
            pane.current_command,
            pane.current_path,
            cursor
        ));
    }
    out.trim_end().to_string()
}

fn render_list_panes_json(panes: &[TmuxPane], include_all: bool) -> AppResult<String> {
    let panes = panes
        .iter()
        .filter(|pane| include_all || is_default_list_pane(pane))
        .map(|pane| {
            serde_json::json!({
                "pane_id": pane.pane_id,
                "session": pane.session_name,
                "window": pane.window_name,
                "pid": pane.pane_pid,
                "active": pane.pane_active,
                "provider": pane_provider_label(pane),
                "command": pane.current_command,
                "cwd": pane.current_path,
                "cursor": {
                    "x": pane.cursor_x,
                    "y": pane.cursor_y,
                },
            })
        })
        .collect::<Vec<_>>();
    serde_json::to_string_pretty(&serde_json::json!({ "panes": panes }))
        .map_err(|error| AppError::new(format!("failed to encode list JSON: {error}")))
}

fn run_capture(args: CaptureArgs) -> AppResult<String> {
    let client = TmuxClient::default();
    let pane = resolve_pane_by_id(&client, &args.pane_id)?;
    client.capture_pane(&pane.pane_id, args.history_lines)
}

fn run_last_message(args: LastMessageArgs) -> AppResult<String> {
    let client = TmuxClient::default();
    let pane = resolve_pane_by_id(&client, &args.pane_id)?;
    // `args.history_lines` defaults to 2000 for agy pane-scrape extraction;
    // matches the dashboard polling default. Non-agy providers ignore it.
    let message = load_last_agent_message(&pane, &client, args.history_lines)?;
    if args.out.as_deref().is_some_and(output_path_is_stdout) {
        let mut stdout = io::stdout().lock();
        stdout.write_all(message.text.as_bytes())?;
        stdout.flush()?;
        return Ok(String::new());
    }

    let output_path = args
        .out
        .unwrap_or_else(|| default_output_path(&message.session_id));
    if let Some(parent) = output_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)?;
    }
    fs::write(&output_path, &message.text)?;
    Ok(format!(
        "Dumped last message of {} lines out to file:\n{}",
        line_count(&message.text),
        output_path.display()
    ))
}

fn run_status(args: StatusArgs) -> AppResult<String> {
    let client = TmuxClient::default();
    let pane =
        normalize_dashboard_window_name(&client, &resolve_pane_by_id(&client, &args.pane_id)?)?;
    let frame = client.capture_pane_ansi(&pane.pane_id, args.history_lines)?;
    let frame = prepare_frame_for_classification(&frame);
    let focused = focused_frame_source(&frame);
    let classification = Classifier.classify(&pane.pane_id, &focused);
    let bindings = inspect_keybindings(None).map_err(AppError::new)?;

    if args.json {
        render_status_json(&pane, &classification, &bindings, &frame, false)
    } else {
        Ok(render_status_report(
            &pane,
            &classification,
            &bindings,
            &frame,
        ))
    }
}

fn run_doctor(args: DoctorArgs) -> AppResult<String> {
    let client = TmuxClient::default();
    let pane = normalize_dashboard_window_name(
        &client,
        &resolve_doctor_pane(
            &client,
            args.session_name.as_deref(),
            args.pane_id.as_deref(),
        )?,
    )?;
    let frame = client.capture_pane_ansi(&pane.pane_id, args.history_lines)?;
    let frame = prepare_frame_for_classification(&frame);
    let focused = focused_frame_source(&frame);
    let classification = Classifier.classify(&pane.pane_id, &focused);
    let bindings = inspect_keybindings(args.bindings_path.as_deref()).map_err(AppError::new)?;
    let automation_ready =
        is_pane_command_claude(&pane) && bindings.status == KeybindingsStatus::Valid;

    if args.json {
        render_status_json(&pane, &classification, &bindings, &frame, automation_ready)
    } else {
        Ok(format!(
            "automation_ready={}\npane={}\nsession={}\nwindow={}\nactive={}\nprovider={}\ncommand={}\ncommand_matches_claude={}\ncwd={}\ncursor={}\nstate={}\nhas_questions={}\nrecap_present={}\nrecap_excerpt={}\nsignals={}\nscreen_excerpt={}\nnext_safe_action={}\nbindings_path={}\nbindings_status={}\nbindings_missing={}{}",
            automation_ready,
            pane.pane_id,
            pane.session_name,
            pane.window_name,
            pane.pane_active,
            classification_provider_label(&classification, &pane),
            pane.current_command,
            is_pane_command_claude(&pane),
            pane.current_path,
            render_cursor(&pane),
            classification.state.as_str(),
            classification.has_questions,
            classification.recap_present,
            classification.recap_excerpt.as_deref().unwrap_or("none"),
            render_signals(&classification),
            render_screen_excerpt(&frame),
            render_next_safe_action(&classification, &pane, &bindings),
            bindings.path.display(),
            bindings.status.as_str(),
            render_missing_bindings(&bindings),
            render_doctor_recommendations(&classification, &pane, &bindings)
        ))
    }
}

fn run_install_bindings(args: InstallBindingsArgs) -> AppResult<String> {
    let report = install_recommended_keybindings(args.path.as_deref()).map_err(AppError::new)?;
    let inspection = inspect_keybindings(Some(&report.path)).map_err(AppError::new)?;

    Ok(format!(
        "installed_bindings={}\npath={}\nbackup={}\nstatus={}\nmissing={}",
        report.wrote_file,
        report.path.display(),
        report
            .backup_path
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| String::from("none")),
        inspection.status.as_str(),
        render_missing_bindings(&inspection)
    )
    .trim_end()
    .to_string())
}

fn run_observe(args: ObserveArgs) -> AppResult<String> {
    let client = TmuxClient::default();
    let request = ObserveRequest {
        session_name: args.session_name,
        target_pane: args.pane_id,
        max_events: args.events,
        idle_timeout_ms: args.idle_timeout_ms,
        history_lines: args.history_lines,
    };
    let collected = collect_observation(&client, &request)?;
    let report = collected.report.render();
    let artifacts_required = args.state_dir.is_some();
    let (state_dir, mut artifact_warning) = resolve_artifact_state_dir(args.state_dir.as_deref())?;
    let artifact_paths = match state_dir {
        Some(state_dir) => match write_observe_artifacts(&state_dir, &collected) {
            Ok(paths) => Some(paths),
            Err(error) => {
                if artifacts_required {
                    return Err(error);
                }
                artifact_warning = Some(error.to_string());
                None
            }
        },
        None => None,
    };

    Ok(render_observe_command_output(
        &report,
        artifact_paths.as_ref(),
        artifact_warning.as_deref(),
    ))
}

#[derive(Debug)]
struct ObserveArtifactPaths {
    capture_file: PathBuf,
    tape_file: PathBuf,
    export_file: PathBuf,
}

fn run_serve(args: ServeArgs) -> AppResult<String> {
    let state_dir = resolve_bootstrapped_state_dir(args.state_dir.as_deref())?;
    let (runtime, _runtime_guard) = ensure_runtime_client(
        &state_dir,
        if args.unmanaged {
            RuntimeManagementMode::Unmanaged
        } else {
            RuntimeManagementMode::Managed
        },
        RuntimeBootstrapConfig {
            reconcile_ms: args.reconcile_ms,
            history_lines: args.history_lines.max(200),
        },
    )?;

    let interrupted = Arc::new(AtomicBool::new(false));
    install_babysit_sigint_handler(Arc::clone(&interrupted))?;
    let request = ServeRequest {
        session_name: args.session_name,
        target_pane: args.pane_id,
        history_lines: args.history_lines,
        reconcile_ms: args.reconcile_ms,
        state_dir: Some(state_dir.clone()),
        allowed_origins: args.allowed_origins,
    };

    let http_thread = if let Some(http_addr) = args.http_addr.clone() {
        eprintln!("http api listening on http://{http_addr}");
        Some(spawn_http_server(
            runtime.clone(),
            request.clone(),
            http_addr,
            Arc::clone(&interrupted),
        ))
    } else {
        None
    };

    let mut subscription = runtime.subscribe()?;
    while !interrupted.load(Ordering::SeqCst) {
        if let Some(event) = subscription.recv()? {
            if let Some(snapshot) = event.snapshot.as_ref() {
                if snapshot.pane.session_name != request.session_name {
                    continue;
                }
                if let Some(target_pane) = request.target_pane.as_deref()
                    && snapshot.pane.pane_id != target_pane
                {
                    continue;
                }
            }
            let payload = serde_json::json!({
                "kind": event.kind,
                "pane_id": event.pane_id,
                "workspace_id": event.workspace_id,
                "summary": event.summary,
                "revision": event.revision,
                "timestamp_unix_ms": event.timestamp_unix_ms,
            });
            emit_babysit_output(render_babysit_output(
                args.format.into(),
                payload,
                supports_babysit_color(),
            ))?;
        }
    }

    if let Some(http_thread) = http_thread {
        http_thread
            .join()
            .map_err(|_| AppError::new("http api thread panicked"))??;
    }

    Ok(String::new())
}

fn write_observe_artifacts(
    state_dir: &Path,
    collected: &CollectedObservation,
) -> AppResult<ObserveArtifactPaths> {
    let artifact_id = new_artifact_id(
        "observe",
        &collected.report.session_name,
        Some(&collected.report.target_pane),
    )?;

    let capture_path = capture_artifact_path(state_dir, &artifact_id, "capture.txt")?;
    let tape_path = tape_artifact_path(state_dir, &artifact_id, "control-mode.log")?;
    let export_path = export_artifact_path(state_dir, &artifact_id, "report.json")?;

    fs::write(&capture_path, &collected.report.captured_frame)?;
    fs::write(
        &tape_path,
        render_control_log_lines(&collected.raw_control_lines),
    )?;
    let export = serde_json::json!({
        "command": "observe",
        "generated_at": current_babysit_timestamp(),
        "state_dir": state_dir,
        "session_name": collected.report.session_name,
        "target_pane": collected.report.target_pane,
        "output_events": collected.report.output_events,
        "notifications": collected.report.notifications,
        "seen_panes": collected.report.seen_panes,
        "live_excerpt": collected.report.live_excerpt,
        "capture_file": capture_path.display().to_string(),
        "tape_file": tape_path.display().to_string(),
        "classification": {
            "source": collected.report.classification.source,
            "state": collected.report.classification.state.as_str(),
            "recap_present": collected.report.classification.recap_present,
            "recap_excerpt": collected.report.classification.recap_excerpt,
            "signals": collected.report.classification.signals,
        },
    });
    let export_content = serde_json::to_string_pretty(&export)
        .map_err(|error| AppError::new(format!("failed to encode observe export JSON: {error}")))?;
    fs::write(&export_path, export_content)?;

    Ok(ObserveArtifactPaths {
        capture_file: capture_path,
        tape_file: tape_path,
        export_file: export_path,
    })
}

fn new_artifact_id(command: &str, session_name: &str, pane_id: Option<&str>) -> AppResult<String> {
    let timestamp = OffsetDateTime::now_utc()
        .format(&format_description!(
            "[year][month][day]-[hour][minute][second]-[subsecond digits:3]"
        ))
        .map_err(|error| AppError::new(format!("failed to format artifact timestamp: {error}")))?;
    let sanitized_command = sanitize_artifact_token(command);
    let sanitized_session = sanitize_artifact_token(session_name);
    let sanitized_pane = pane_id
        .map(sanitize_artifact_token)
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| String::from("all"));
    let pid = std::process::id();

    Ok(format!(
        "{sanitized_command}-{sanitized_session}-{sanitized_pane}-{timestamp}-{pid}"
    ))
}

fn sanitize_artifact_token(input: &str) -> String {
    let mut sanitized = String::with_capacity(input.len());
    for ch in input.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
            sanitized.push(ch);
        } else {
            sanitized.push('_');
        }
    }

    let sanitized = sanitized.trim_matches('_');
    if sanitized.is_empty() {
        String::from("item")
    } else {
        sanitized.to_string()
    }
}

fn render_observe_command_output(
    report: &str,
    artifact_paths: Option<&ObserveArtifactPaths>,
    artifact_warning: Option<&str>,
) -> String {
    let mut out = report.to_string();
    if let Some(artifact_paths) = artifact_paths {
        out.push_str(&format!(
            "\nartifacts:\n  capture_file={}\n  control_log={}\n  export={}\n",
            artifact_paths.capture_file.display(),
            artifact_paths.tape_file.display(),
            artifact_paths.export_file.display()
        ));
    } else if let Some(artifact_warning) = artifact_warning {
        out.push_str(&format!(
            "\nartifacts:\n  unavailable={}\n",
            artifact_warning
        ));
    }
    out
}

fn resolve_artifact_state_dir(path: Option<&Path>) -> AppResult<(Option<PathBuf>, Option<String>)> {
    artifact_state_dir_result(resolve_state_dir(path), path.is_some())
}

fn artifact_state_dir_result(
    result: AppResult<PathBuf>,
    required: bool,
) -> AppResult<(Option<PathBuf>, Option<String>)> {
    match result {
        Ok(state_dir) => Ok((Some(state_dir), None)),
        Err(error) if required => Err(error),
        Err(error) => Ok((None, Some(error.to_string()))),
    }
}

fn render_control_log_lines(lines: &[String]) -> String {
    if lines.is_empty() {
        return String::from("# no control-mode lines captured\n");
    }

    let mut out = lines.join("\n");
    out.push('\n');
    out
}

fn run_record_fixture(args: RecordFixtureArgs) -> AppResult<String> {
    let client = TmuxClient::default();
    let request = ObserveRequest {
        session_name: args.session_name.clone(),
        target_pane: args.pane_id,
        max_events: args.events,
        idle_timeout_ms: args.idle_timeout_ms,
        history_lines: args.history_lines,
    };
    let collected = collect_observation(&client, &request)?;
    let expected_state = parse_expected_state_arg(
        args.expected_state.as_deref(),
        collected.report.classification.state,
    )?;

    let recorded = record_case(FixtureRecordInput {
        case_name: &args.case_name,
        output_dir: &args.output_dir,
        expected_state,
        classified_state: collected.report.classification.state,
        session_name: &collected.report.session_name,
        target_pane: &collected.report.target_pane,
        output_events: collected.report.output_events,
        notifications: collected.report.notifications,
        recap_present: collected.report.classification.recap_present,
        recap_excerpt: collected.report.classification.recap_excerpt.as_deref(),
        signals: &collected.report.classification.signals,
        frame_text: &collected.report.captured_frame,
        raw_control_lines: &collected.raw_control_lines,
    })?;

    Ok(format!(
        "recorded fixture case={} dir={} pane={} expected={} classified={}",
        args.case_name,
        recorded.case_dir.display(),
        recorded.target_pane,
        recorded.expected_state.as_str(),
        recorded.classified_state.as_str()
    ))
}

fn run_classify(args: ClassifyArgs) -> AppResult<String> {
    let frame = load_frame_text(&args.path)?;
    let classification = Classifier.classify("fixture", &frame);
    Ok(classification.render())
}

fn run_replay(args: ReplayArgs) -> AppResult<String> {
    let case = FixtureCase::load(&args.path)?;
    let classification = Classifier.classify(&case.name, &case.frame_text);
    let matches = classification.state == case.expected_state;
    Ok(format!(
        "case={}\nexpected={}\nactual={}\nmatch={}\n{}",
        case.name,
        case.expected_state.as_str(),
        classification.state.as_str(),
        matches,
        classification.render()
    ))
}

fn run_send_action(args: SendActionArgs) -> AppResult<String> {
    let action = AutomationAction::from_str(&args.action)
        .ok_or_else(|| AppError::new(format!("unknown action: {}", args.action)))?;
    let client = TmuxClient::default();
    let pane = resolve_pane_by_id(&client, &args.pane_id)?;
    ensure_pane_owned_by_claude(&pane)?;
    let bindings = load_automation_keybindings(None)?;
    let keys = keys_for_action(&bindings, action)?;
    client.send_keys(&pane.pane_id, keys)?;
    Ok(format!(
        "sent action={} pane={} keys={}",
        action.as_str(),
        pane.pane_id,
        keys.join("+")
    ))
}

fn run_guarded_pane_workflow(
    args: PaneCommandArgs,
    workflow: GuardedWorkflow,
) -> AppResult<String> {
    let client = TmuxClient::default();
    let pane = resolve_pane_by_id(&client, &args.pane_id)?;
    let classification = classify_pane(&client, &pane.pane_id, ACTION_GUARD_HISTORY_LINES)?;
    ensure_pane_owned_by_automatable_provider(&pane, &classification)?;
    ensure_workflow_state(workflow, &classification)?;
    let executed_actions =
        execute_classified_workflow(&client, &pane.pane_id, workflow, &classification)?;

    let pane_label = pane_label_from_path(&pane.current_path, &pane.pane_id);
    Ok(render_guarded_workflow_output(
        workflow,
        &pane_label,
        &pane.pane_id,
        &classification,
        &executed_actions,
        matches!(workflow, GuardedWorkflow::ApprovePermission)
            && classification.state == SessionState::FolderTrustPrompt,
        args.format,
        supports_babysit_color(),
    ))
}

fn run_continue_session(args: ContinueSessionArgs) -> AppResult<String> {
    let client = TmuxClient::default();
    let pane = resolve_target_pane(&client, &args.target)?;
    ensure_pane_owned_by_claude(&pane)?;
    let before = classify_pane(&client, &pane.pane_id, ACTION_GUARD_HISTORY_LINES)?;
    let outcome = continue_from_classification(&client, &pane, &before)?;

    if !is_usable_state(outcome.after.state) && outcome.action != "none" {
        return Err(AppError::new(format!(
            "continue-session sent {} but pane {} is still in state {}",
            outcome.action,
            pane.pane_id,
            outcome.after.state.as_str()
        )));
    }

    Ok(format!(
        "continue-session pane={} before={} after={} action={} outcome={}",
        pane.pane_id,
        before.state.as_str(),
        outcome.after.state.as_str(),
        outcome.action,
        outcome.outcome
    ))
}

fn run_auto_unstick(args: AutoUnstickArgs) -> AppResult<String> {
    let client = TmuxClient::default();
    let pane = resolve_target_pane(&client, &args.target)?;
    ensure_pane_owned_by_claude(&pane)?;
    let mut actions = Vec::new();
    let mut approved_permission = false;
    let mut current = classify_pane(&client, &pane.pane_id, ACTION_GUARD_HISTORY_LINES)?;

    for _ in 0..args.max_steps {
        if is_usable_state(current.state) {
            let action_summary = if actions.is_empty() {
                String::from("none")
            } else {
                actions.join(",")
            };
            return Ok(format!(
                "auto-unstick pane={} final_state={} actions={} steps={}",
                pane.pane_id,
                current.state.as_str(),
                action_summary,
                actions.len()
            ));
        }

        if approved_permission && current.state == SessionState::PermissionDialog {
            return Err(AppError::new(format!(
                "auto-unstick refuses to approve more than one permission dialog for pane {}",
                pane.pane_id
            )));
        }

        let outcome = continue_from_classification(&client, &pane, &current)?;
        if outcome.used_permission_approval {
            approved_permission = true;
        }
        actions.push(outcome.action);
        current = outcome.after;
    }

    if is_usable_state(current.state) {
        let action_summary = if actions.is_empty() {
            String::from("none")
        } else {
            actions.join(",")
        };
        Ok(format!(
            "auto-unstick pane={} final_state={} actions={} steps={}",
            pane.pane_id,
            current.state.as_str(),
            action_summary,
            actions.len()
        ))
    } else {
        Err(AppError::new(format!(
            "auto-unstick stopped after {} steps with pane {} still in state {}",
            args.max_steps,
            pane.pane_id,
            current.state.as_str()
        )))
    }
}

fn run_keep_going(args: KeepGoingArgs) -> AppResult<String> {
    let client = TmuxClient::default();
    let pane = resolve_target_pane(&client, &args.target)?;
    ensure_pane_owned_by_claude(&pane)?;
    let state_dir = resolve_bootstrapped_state_dir(args.state_dir.as_deref())?;
    let prompt =
        resolve_keep_going_prompt(args.prompt_source.as_deref(), args.prompt_text.as_deref())?;
    let bindings = load_automation_keybindings(None)?;
    let mut awaiting_reply = false;
    let mut last_state: Option<SessionState> = None;

    emit_babysit_output(format!(
        "keep-going watching pane={} yolo={}",
        pane.pane_id,
        if args.no_yolo { "off" } else { "on" }
    ))?;

    loop {
        let current_pane = resolve_pane_by_id(&client, &pane.pane_id)?;
        ensure_pane_owned_by_claude(&current_pane)?;
        let inspected = inspect_pane(&client, &pane.pane_id, KEEP_GOING_HISTORY_LINES)?;
        let classification = &inspected.classification;

        if last_state != Some(classification.state) {
            emit_babysit_output(render_keep_going_wait_message(
                &pane.pane_id,
                classification,
                awaiting_reply,
                args.no_yolo,
            ))?;
            last_state = Some(classification.state);
        }

        if !args.no_yolo {
            if classification.state == SessionState::FolderTrustPrompt {
                emit_babysit_output(format!(
                    "keep-going pane={} action=confirm-folder-trust",
                    pane.pane_id
                ))?;
                execute_keep_going_yolo_action(
                    &client,
                    &pane.pane_id,
                    GuardedWorkflow::ApprovePermission,
                    classification,
                )?;
                thread::sleep(Duration::from_millis(args.poll_ms));
                continue;
            }

            match yolo_action_for_state(classification.state) {
                YoloAction::ApprovePermission => {
                    if is_yolo_safe_to_approve(&inspected, &current_pane) {
                        emit_babysit_output(format!(
                            "keep-going pane={} action=approve",
                            pane.pane_id
                        ))?;
                        execute_keep_going_yolo_action(
                            &client,
                            &pane.pane_id,
                            GuardedWorkflow::ApprovePermission,
                            classification,
                        )?;
                        thread::sleep(Duration::from_millis(args.poll_ms));
                        continue;
                    }
                }
                YoloAction::DismissSurvey => {
                    emit_babysit_output(format!(
                        "keep-going pane={} action=dismiss-survey",
                        pane.pane_id
                    ))?;
                    execute_keep_going_yolo_action(
                        &client,
                        &pane.pane_id,
                        GuardedWorkflow::DismissSurvey,
                        classification,
                    )?;
                    thread::sleep(Duration::from_millis(args.poll_ms));
                    continue;
                }
                // Defensive: keep-going gates on `ensure_pane_owned_by_claude`
                // above, so an agy command-permission state is unreachable here.
                // Treat it as Wait so the match stays exhaustive without
                // accidentally driving agy panes through the Claude keystroke
                // path.
                YoloAction::ApproveAgyCommandPermission | YoloAction::Wait => {}
            }
        } else if let Some(error) = keep_going_no_yolo_blocker(classification, &pane.pane_id) {
            return Err(error);
        }

        if awaiting_reply {
            let frame = client.capture_pane(&pane.pane_id, KEEP_GOING_HISTORY_LINES)?;
            let response = extract_keep_going_response(&frame, prompt.anchor.as_deref());
            let terminal_reply_ready = classification.state == SessionState::ChatReady
                || frame_has_keep_going_chat_input(&frame);

            if let Some(response) = response {
                match response.directive {
                    KeepGoingDirective::OkieDokie => {
                        if terminal_reply_ready {
                            emit_babysit_output(format!(
                                "keep-going pane={} token={}",
                                pane.pane_id,
                                response.directive.as_str()
                            ))?;
                            awaiting_reply = false;
                        }
                    }
                    KeepGoingDirective::AllDone | KeepGoingDirective::Panic => {
                        if terminal_reply_ready {
                            emit_babysit_output(format!(
                                "keep-going pane={} token={}",
                                pane.pane_id,
                                response.directive.as_str()
                            ))?;
                            return Ok(response.render());
                        }
                    }
                }
            }
        } else if classification.state == SessionState::ChatReady {
            let before_submit = client.capture_pane(&pane.pane_id, KEEP_GOING_HISTORY_LINES)?;
            submit_keep_going_prompt(
                &client,
                &current_pane,
                &state_dir,
                &prompt.text,
                &bindings,
                args.submit_delay_ms,
            )?;
            emit_babysit_output(format!(
                "keep-going pane={} action=submit-prompt",
                pane.pane_id
            ))?;
            wait_for_prompt_submission_start(
                &client,
                &pane.pane_id,
                &before_submit,
                prompt.anchor.as_deref(),
            )
            .map_err(
                |_| {
                    AppError::new(format!(
                        "keep-going submitted the loop prompt but pane {} did not show a prompt-submission transition",
                        pane.pane_id
                    ))
                },
            )?;
            awaiting_reply = true;
        }

        thread::sleep(Duration::from_millis(args.poll_ms));
    }
}

fn submit_keep_going_prompt(
    client: &TmuxClient,
    pane: &TmuxPane,
    state_dir: &Path,
    prompt_text: &str,
    bindings: &ResolvedKeybindings,
    submit_delay_ms: u64,
) -> AppResult<()> {
    let workspace = resolve_workspace_for_path(state_dir, Path::new(&pane.current_path))?;
    store_pending_prompt_for_tmux_instance(
        state_dir,
        &workspace.id,
        &pane.session_name,
        pane,
        prompt_text,
    )?;
    send_actions(
        client,
        &pane.pane_id,
        &prompt_submission_sequence(),
        bindings,
        submit_delay_ms,
    )
}

fn execute_keep_going_yolo_action(
    client: &TmuxClient,
    pane_id: &str,
    workflow: GuardedWorkflow,
    classification: &Classification,
) -> AppResult<()> {
    execute_classified_workflow(client, pane_id, workflow, classification)?;
    thread::sleep(Duration::from_millis(AUTO_UNSTICK_STEP_DELAY_MS));
    let after = classify_pane(client, pane_id, ACTION_GUARD_HISTORY_LINES)?;
    ensure_state_transition(classification, &after, workflow.as_str())
}

fn keep_going_no_yolo_blocker(classification: &Classification, pane_id: &str) -> Option<AppError> {
    if !matches!(
        classification.state,
        SessionState::PermissionDialog
            | SessionState::UserQuestionPrompt
            | SessionState::PlanApprovalPrompt
            | SessionState::FolderTrustPrompt
    ) {
        return None;
    }

    let detail = permission_manual_review_reason(classification)
        .map(|reason| format!(": {reason}"))
        .unwrap_or_default();

    Some(AppError::with_exit_code(
        format!(
            "keep-going stopped for pane {} in state {}{}",
            pane_id,
            classification.state.as_str(),
            detail,
        ),
        2,
    ))
}

fn prompt_submission_started(
    before_frame: &str,
    after_frame: &str,
    after_classification: &Classification,
    prompt_anchor: Option<&str>,
) -> bool {
    match after_classification.state {
        SessionState::BusyResponding
        | SessionState::UserQuestionPrompt
        | SessionState::PermissionDialog
        | SessionState::PlanApprovalPrompt
        | SessionState::FolderTrustPrompt
        | SessionState::StartupChoicePrompt
        | SessionState::SurveyPrompt
        | SessionState::DiffDialog => return true,
        SessionState::PromptEditing
        | SessionState::ExternalEditorActive
        | SessionState::Unknown => return false,
        // Agy panes do not use the prompt-submission path (no Claude-style
        // keybinding contract), so a frame that landed in any agy state can
        // never represent "submission started". Treat as not-started so the
        // caller does not advance to the wait-for-response phase.
        SessionState::AgyCommandPermissionPrompt
        | SessionState::AgyFolderTrustPrompt
        | SessionState::AgySettingsPersistPrompt => return false,
        SessionState::ChatReady => {}
    }

    match prompt_anchor {
        Some(anchor) => {
            if count_prompt_anchors(after_frame, anchor)
                > count_prompt_anchors(before_frame, anchor)
            {
                return true;
            }

            last_prompt_anchor_idx(after_frame, anchor)
                != last_prompt_anchor_idx(before_frame, anchor)
                || before_frame != after_frame
        }
        None => before_frame != after_frame,
    }
}

fn wait_for_prompt_submission_start(
    client: &TmuxClient,
    pane_id: &str,
    before_frame: &str,
    prompt_anchor: Option<&str>,
) -> AppResult<()> {
    let attempts = std::cmp::max(1, PROMPT_SUBMISSION_TIMEOUT_MS / PROMPT_SUBMISSION_POLL_MS);
    for _ in 0..attempts {
        thread::sleep(Duration::from_millis(PROMPT_SUBMISSION_POLL_MS));
        let after_frame = client.capture_pane(pane_id, KEEP_GOING_HISTORY_LINES)?;
        let after_classification =
            Classifier.classify(pane_id, &focused_frame_source(&after_frame));
        if prompt_submission_started(
            before_frame,
            &after_frame,
            &after_classification,
            prompt_anchor,
        ) {
            return Ok(());
        }
    }

    Err(AppError::new(format!(
        "pane {pane_id} did not show a prompt-submission transition"
    )))
}

fn count_prompt_anchors(frame: &str, anchor: &str) -> usize {
    frame.lines().filter(|line| line.contains(anchor)).count()
}

fn last_prompt_anchor_idx(frame: &str, anchor: &str) -> Option<usize> {
    frame
        .lines()
        .collect::<Vec<_>>()
        .iter()
        .rposition(|line| line.contains(anchor))
}

fn render_keep_going_wait_message(
    pane_id: &str,
    classification: &Classification,
    awaiting_reply: bool,
    no_yolo: bool,
) -> String {
    let waiting_for = if awaiting_reply {
        "audit-response"
    } else {
        "chat-input"
    };
    let yolo = if no_yolo { "off" } else { "on" };
    let detail = permission_manual_review_reason(classification)
        .map(|reason| format!(" detail={reason}"))
        .unwrap_or_default();

    format!(
        "keep-going pane={} state={} waiting-for={} yolo={}{}",
        pane_id,
        classification.state.as_str(),
        waiting_for,
        yolo,
        detail,
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeepGoingDirective {
    OkieDokie,
    AllDone,
    Panic,
}

impl KeepGoingDirective {
    fn as_str(self) -> &'static str {
        match self {
            Self::OkieDokie => "OKIE_DOKIE",
            Self::AllDone => "ALL_DONE",
            Self::Panic => "PANIC",
        }
    }

    fn from_line(line: &str) -> Option<Self> {
        match line.trim() {
            "OKIE_DOKIE" => Some(Self::OkieDokie),
            "ALL_DONE" => Some(Self::AllDone),
            "PANIC" => Some(Self::Panic),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct KeepGoingResponse {
    directive: KeepGoingDirective,
    body: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct KeepGoingPromptConfig {
    text: String,
    anchor: Option<String>,
}

impl KeepGoingResponse {
    fn render(&self) -> String {
        if self.body.is_empty() {
            self.directive.as_str().to_string()
        } else {
            format!("{}\n{}", self.body, self.directive.as_str())
        }
    }
}

fn extract_keep_going_response(
    frame: &str,
    prompt_anchor: Option<&str>,
) -> Option<KeepGoingResponse> {
    let lines = frame.lines().collect::<Vec<_>>();
    let mut search_end = lines.len();
    while search_end > 0
        && (is_keep_going_chat_input_line(lines[search_end - 1])
            || lines[search_end - 1].trim().is_empty())
    {
        search_end -= 1;
    }
    if search_end == 0 {
        return None;
    }

    let token_idx = search_end - 1;
    let directive = KeepGoingDirective::from_line(lines[token_idx])?;
    let body_start = keep_going_body_start(&lines, token_idx, prompt_anchor);

    let mut body_lines = lines[body_start..token_idx]
        .iter()
        .map(|line| line.trim_end())
        .collect::<Vec<_>>();
    while matches!(body_lines.first(), Some(line) if line.trim().is_empty()) {
        body_lines.remove(0);
    }
    while matches!(body_lines.last(), Some(line) if line.trim().is_empty()) {
        body_lines.pop();
    }

    Some(KeepGoingResponse {
        directive,
        body: body_lines.join("\n"),
    })
}

fn keep_going_body_start(lines: &[&str], token_idx: usize, prompt_anchor: Option<&str>) -> usize {
    if let Some(anchor) = prompt_anchor.filter(|anchor| !anchor.trim().is_empty())
        && let Some(prompt_idx) = lines[..token_idx]
            .iter()
            .rposition(|line| line.contains(anchor))
    {
        return prompt_idx + 1;
    }

    lines[..token_idx]
        .iter()
        .rposition(|line| line.trim().is_empty())
        .map(|idx| idx + 1)
        .unwrap_or(0)
}

fn frame_has_keep_going_chat_input(frame: &str) -> bool {
    frame
        .lines()
        .rev()
        .take(4)
        .any(is_keep_going_chat_input_line)
}

fn resolve_keep_going_prompt(
    source: Option<&Path>,
    text: Option<&str>,
) -> AppResult<KeepGoingPromptConfig> {
    match (source, text) {
        (None, None) => Ok(KeepGoingPromptConfig {
            text: KEEP_GOING_PROMPT.to_string(),
            anchor: Some(KEEP_GOING_PROMPT_ANCHOR.to_string()),
        }),
        _ => {
            let text = read_prompt_input(source, text)?;
            if text.trim().is_empty() {
                return Err(AppError::new("keep-going custom prompt must not be empty"));
            }
            Ok(KeepGoingPromptConfig {
                anchor: Some(KEEP_GOING_CUSTOM_PROMPT_ANCHOR.to_string()),
                text: format!(
                    "{text}\n\n(Do not repeat the marker line below in your reply.)\n{KEEP_GOING_CUSTOM_PROMPT_ANCHOR}"
                ),
            })
        }
    }
}

fn is_keep_going_chat_input_line(line: &str) -> bool {
    let trimmed = line.trim();
    if matches!(trimmed, ">" | "❯" | "Enter to submit message") {
        return true;
    }

    let Some(rest) = trimmed.strip_prefix('❯') else {
        return false;
    };
    let rest = rest.trim();
    !rest.is_empty() && !keep_going_starts_with_numbered_option(rest)
}

fn keep_going_starts_with_numbered_option(line: &str) -> bool {
    let digits = line.chars().take_while(|ch| ch.is_ascii_digit()).count();
    digits > 0 && line[digits..].starts_with('.')
}

#[derive(Debug, Clone)]
struct ResolvedPromptRunInput {
    temp_file: Option<PathBuf>,
    submitted_text: String,
}

fn run_prompt(args: PromptRunArgs) -> AppResult<String> {
    let state_dir = resolve_bootstrapped_state_dir(args.state_dir.as_deref())?;
    let session_name = args
        .session_name
        .clone()
        .unwrap_or_else(|| String::from(DEFAULT_PROMPT_SESSION));
    let prompt_run_id = default_prompt_run_id();
    let resolved_input = resolve_prompt_run_input(&args, &state_dir, &prompt_run_id)?;
    let result = run_prompt_with_resolved_input(&args, &state_dir, &session_name, &resolved_input);
    cleanup_prompt_temp(&resolved_input, args.keep_temp || result.is_err());
    result
}

fn run_prompt_with_resolved_input(
    args: &PromptRunArgs,
    state_dir: &Path,
    session_name: &str,
    resolved_input: &ResolvedPromptRunInput,
) -> AppResult<String> {
    let cwd = match args.cwd.clone() {
        Some(cwd) => cwd,
        None => std::env::current_dir()?,
    };
    let launch_command = prompt_launch_command(args, state_dir, session_name)?;
    let client = TmuxClient::default();
    prompt_verbose(
        args,
        format_args!("starting prompt window in tmux session {session_name}"),
    );
    let started = start_prompt_window(
        &client,
        session_name,
        &args.window_name,
        cwd,
        launch_command,
    )?;
    let mut pane = client.pane_by_id(&started.pane_id)?.ok_or_else(|| {
        AppError::with_exit_code(
            format!(
                "no pane found for prompt window {} pane {}",
                started.window_id, started.pane_id
            ),
            2,
        )
    })?;

    prompt_verbose(
        args,
        format_args!("waiting for Claude pane {} to become ready", pane.pane_id),
    );
    wait_for_prompt_pane_ready(
        &client,
        &pane,
        args,
        Duration::from_millis(args.ready_timeout_ms),
    )?;
    pane = resolve_pane_by_id(&client, &pane.pane_id)?;
    ensure_pane_owned_by_claude(&pane)
        .map_err(|error| AppError::with_exit_code(error.to_string(), 2))?;
    // `history_lines = 2000` matches the dashboard polling default and is only
    // used by the agy pane-scrape branch; other providers ignore it.
    let pre_submit_message = load_last_agent_message(&pane, &client, 2000).ok();
    submit_prompt_text_direct(
        &client,
        &pane,
        &resolved_input.submitted_text,
        &load_automation_keybindings(None)?,
        args.submit_delay_ms,
    )?;
    prompt_verbose(
        args,
        format_args!("submitted prompt to pane {}", pane.pane_id),
    );
    prompt_verbose(args, format_args!("waiting for assistant response"));
    wait_for_prompt_completion(
        &client,
        &pane,
        args,
        Duration::from_millis(args.idle_timeout_ms),
    )?;
    let message = wait_for_fresh_last_message(
        &pane,
        &client,
        pre_submit_message.as_ref(),
        Duration::from_millis(args.idle_timeout_ms),
    )?;
    cleanup_prompt_window(&client, &started);

    Ok(message.text)
}

fn start_prompt_window(
    client: &TmuxClient,
    session_name: &str,
    window_name: &str,
    cwd: PathBuf,
    command: String,
) -> AppResult<StartedWindow> {
    client.start_window_in_session(&StartWindowRequest {
        session_name: session_name.to_string(),
        window_name: window_name.to_string(),
        cwd,
        command,
    })
}

fn cleanup_prompt_window(client: &TmuxClient, started: &StartedWindow) {
    match client.pane_by_id(&started.pane_id) {
        Ok(Some(pane)) if pane.window_id == started.window_id => {
            if let Err(error) = client.kill_window(&started.window_id) {
                prompt_warning(format_args!(
                    "failed to kill prompt window {}: {}",
                    started.window_id, error
                ));
            }
        }
        Ok(Some(pane)) => {
            prompt_warning(format_args!(
                "refused to kill prompt window {}; pane {} now belongs to window {}",
                started.window_id, started.pane_id, pane.window_id
            ));
        }
        Ok(None) => {
            if let Err(error) = client.kill_window(&started.window_id) {
                prompt_warning(format_args!(
                    "prompt pane {} disappeared and killing window {} failed: {}",
                    started.pane_id, started.window_id, error
                ));
            }
        }
        Err(error) => {
            prompt_warning(format_args!(
                "failed to verify prompt pane {} before cleanup: {}",
                started.pane_id, error
            ));
        }
    }
}

fn prompt_verbose(args: &PromptRunArgs, message: std::fmt::Arguments<'_>) {
    if args.verbose {
        eprintln!("prompt: {message}");
    }
}

fn prompt_warning(message: std::fmt::Arguments<'_>) {
    eprintln!("prompt: warning: {message}");
}

fn submit_prompt_text_direct(
    client: &TmuxClient,
    pane: &TmuxPane,
    prompt_text: &str,
    bindings: &ResolvedKeybindings,
    submit_delay_ms: u64,
) -> AppResult<()> {
    client.paste_text(&pane.pane_id, prompt_text)?;
    thread::sleep(Duration::from_millis(submit_delay_ms));
    send_actions(
        client,
        &pane.pane_id,
        &[AutomationAction::Submit],
        bindings,
        submit_delay_ms,
    )
}

fn cleanup_prompt_temp(resolved_input: &ResolvedPromptRunInput, keep_temp: bool) {
    if let Some(temp_file) = &resolved_input.temp_file {
        if keep_temp {
            eprintln!(
                "prompt: kept large prompt instructions at {}",
                temp_file.display()
            );
            return;
        }
        if let Err(error) = fs::remove_file(temp_file)
            && error.kind() != io::ErrorKind::NotFound
        {
            eprintln!(
                "prompt: warning: failed to remove temp file {}: {}",
                temp_file.display(),
                error
            );
        }
        if let Some(parent) = temp_file.parent()
            && let Err(error) = fs::remove_dir(parent)
            && error.kind() != io::ErrorKind::NotFound
        {
            eprintln!(
                "prompt: warning: failed to remove temp directory {}: {}",
                parent.display(),
                error
            );
        }
    }
}

fn prompt_launch_command(
    args: &PromptRunArgs,
    state_dir: &Path,
    session_name: &str,
) -> AppResult<String> {
    let exe = std::env::current_exe()?;
    let state_dir = absolute_prompt_path(state_dir)?;
    let mut editor_parts = vec![
        shell_escape(exe.to_string_lossy().as_ref()),
        String::from("editor-helper"),
        String::from("--session"),
        shell_escape(session_name),
        String::from("--state-dir"),
        shell_escape(state_dir.to_string_lossy().as_ref()),
        String::from("--keep-pending"),
    ];
    if let Some(workspace) = &args.workspace {
        editor_parts.push(String::from("--workspace"));
        editor_parts.push(shell_escape(workspace));
    }
    let editor = editor_parts.join(" ");
    let claude_args = args
        .claude_args
        .iter()
        .map(|arg| shell_escape(arg))
        .collect::<Vec<_>>()
        .join(" ");
    let command = if claude_args.is_empty() {
        args.command.clone()
    } else {
        format!("{} {}", args.command, claude_args)
    };
    Ok(format!(
        "EDITOR={} VISUAL={} {}",
        shell_escape(&editor),
        shell_escape(&editor),
        command
    ))
}

fn absolute_prompt_path(path: &Path) -> AppResult<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

fn default_prompt_run_id() -> String {
    let timestamp = OffsetDateTime::now_utc()
        .format(&format_description!(
            "[hour][minute][second]-[subsecond digits:3]"
        ))
        .unwrap_or_else(|_| String::from("now"));
    format!("botctl-prompt-{}-{timestamp}", std::process::id())
}

fn resolve_prompt_run_input(
    args: &PromptRunArgs,
    state_dir: &Path,
    prompt_run_id: &str,
) -> AppResult<ResolvedPromptRunInput> {
    let combined = compose_prompt_run_content(args)?;
    if combined.len() <= args.large_prompt_threshold {
        return Ok(ResolvedPromptRunInput {
            temp_file: None,
            submitted_text: combined,
        });
    }

    let temp_file = write_prompt_instruction_temp_file(state_dir, prompt_run_id, &combined)?;
    prompt_verbose(
        args,
        format_args!("wrote large prompt instructions to {}", temp_file.display()),
    );
    let submitted_text = format!(
        "Follow the instructions in this UTF-8 text file, then respond normally in this chat:\n\n{}",
        temp_file.display()
    );
    Ok(ResolvedPromptRunInput {
        temp_file: Some(temp_file),
        submitted_text,
    })
}

fn compose_prompt_run_content(args: &PromptRunArgs) -> AppResult<String> {
    let mut sections = Vec::new();
    for path in &args.append_system_prompts {
        sections.push(format!(
            "--- system prompt: {} ---\n{}",
            path.display(),
            fs::read_to_string(path)?
        ));
    }

    let mut user_parts = Vec::new();
    for path in &args.sources {
        user_parts.push((
            format!("source: {}", path.display()),
            fs::read_to_string(path)?,
        ));
    }
    if let Some(text) = &args.text {
        user_parts.push((String::from("text"), text.clone()));
    }
    let should_read_stdin = args.stdin || (user_parts.is_empty() && !io::stdin().is_terminal());
    if should_read_stdin {
        if io::stdin().is_terminal() {
            return Err(AppError::with_exit_code(
                "prompt --stdin cannot read from an interactive terminal; pipe data or use --text/--source",
                2,
            ));
        }
        let mut stdin_text = String::new();
        io::stdin().read_to_string(&mut stdin_text)?;
        user_parts.push((String::from("stdin"), stdin_text));
    }
    if user_parts.is_empty() {
        return Err(AppError::with_exit_code(
            "prompt input requires --source, --text, --stdin, or piped stdin",
            2,
        ));
    }
    if user_parts
        .iter()
        .all(|(_, content)| content.trim().is_empty())
    {
        return Err(AppError::with_exit_code(
            "prompt input must not be empty",
            2,
        ));
    }

    if user_parts.len() == 1 {
        sections.push(format!("--- user prompt ---\n{}", user_parts.remove(0).1));
    } else {
        for (label, content) in user_parts {
            sections.push(format!("--- user prompt: {label} ---\n{content}"));
        }
    }
    Ok(sections.join("\n\n"))
}

fn write_prompt_instruction_temp_file(
    state_dir: &Path,
    prompt_run_id: &str,
    content: &str,
) -> AppResult<PathBuf> {
    let run_id = new_artifact_id("prompt", prompt_run_id, None)?;
    let dir = state_dir.join("prompt-runs").join(run_id);
    fs::create_dir_all(&dir)?;
    let path = dir.join("instructions.md");
    let mut file = private_create_file(&path)?;
    writeln!(file, "# botctl prompt instruction file\n")?;
    writeln!(file, "Run ID: {prompt_run_id}")?;
    writeln!(file, "Generated: {}\n", current_babysit_timestamp())?;
    file.write_all(content.as_bytes())?;
    Ok(path)
}

#[cfg(unix)]
fn private_create_file(path: &Path) -> AppResult<File> {
    use std::os::unix::fs::OpenOptionsExt;
    Ok(fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?)
}

#[cfg(not(unix))]
fn private_create_file(path: &Path) -> AppResult<File> {
    Ok(fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?)
}

fn wait_for_prompt_pane_ready(
    client: &TmuxClient,
    pane: &TmuxPane,
    args: &PromptRunArgs,
    timeout: Duration,
) -> AppResult<Classification> {
    let deadline = Instant::now() + timeout;
    loop {
        let current_pane = resolve_pane_by_id(client, &pane.pane_id)?;
        if !is_pane_command_claude(&current_pane) {
            if Instant::now() >= deadline {
                return Err(AppError::with_exit_code(
                    format!(
                        "timed out waiting for prompt pane {} to run claude; last command {}",
                        pane.pane_id, current_pane.current_command
                    ),
                    2,
                ));
            }
            thread::sleep(Duration::from_millis(args.poll_ms));
            continue;
        }
        let inspected = inspect_pane(client, &pane.pane_id, KEEP_GOING_HISTORY_LINES)?;
        if inspected.classification.state == SessionState::ChatReady {
            return Ok(inspected.classification);
        }
        if !args.no_yolo && is_startup_enter_prompt(&inspected.focused_source) {
            client.send_keys(&pane.pane_id, &["Enter"])?;
            thread::sleep(Duration::from_millis(args.poll_ms));
            continue;
        }
        if inspected.classification.state == SessionState::Unknown {
            if Instant::now() >= deadline {
                return Err(AppError::new(format!(
                    "timed out waiting for pane {} to become ChatReady; last state {}",
                    pane.pane_id,
                    inspected.classification.state.as_str()
                )));
            }
            thread::sleep(Duration::from_millis(args.poll_ms));
            continue;
        }
        prompt_wait_step(client, pane, &inspected, args.no_yolo)?;
        if Instant::now() >= deadline {
            return Err(AppError::new(format!(
                "timed out waiting for pane {} to become ChatReady; last state {}",
                pane.pane_id,
                inspected.classification.state.as_str()
            )));
        }
        thread::sleep(Duration::from_millis(args.poll_ms));
    }
}

fn wait_for_prompt_completion(
    client: &TmuxClient,
    pane: &TmuxPane,
    args: &PromptRunArgs,
    timeout: Duration,
) -> AppResult<()> {
    let deadline = Instant::now() + timeout;
    loop {
        let inspected = inspect_pane(client, &pane.pane_id, KEEP_GOING_HISTORY_LINES)?;
        if is_prompt_completion_state(inspected.classification.state) {
            return Ok(());
        }
        if matches!(
            inspected.classification.state,
            SessionState::PromptEditing | SessionState::Unknown
        ) {
            if Instant::now() >= deadline {
                return Err(AppError::new(format!(
                    "timed out waiting for assistant response in pane {}; last state {}",
                    pane.pane_id,
                    inspected.classification.state.as_str()
                )));
            }
            thread::sleep(Duration::from_millis(args.poll_ms));
            continue;
        }
        prompt_wait_step(client, pane, &inspected, args.no_yolo)?;
        if Instant::now() >= deadline {
            return Err(AppError::new(format!(
                "timed out waiting for assistant response in pane {}; last state {}",
                pane.pane_id,
                inspected.classification.state.as_str()
            )));
        }
        thread::sleep(Duration::from_millis(args.poll_ms));
    }
}

fn is_prompt_completion_state(state: SessionState) -> bool {
    matches!(
        state,
        SessionState::ChatReady | SessionState::UserQuestionPrompt
    )
}

fn prompt_wait_step(
    client: &TmuxClient,
    pane: &TmuxPane,
    inspected: &InspectedPane,
    no_yolo: bool,
) -> AppResult<()> {
    match inspected.classification.state {
        SessionState::BusyResponding => Ok(()),
        SessionState::SurveyPrompt if !no_yolo => {
            execute_keep_going_yolo_action(
                client,
                &pane.pane_id,
                GuardedWorkflow::DismissSurvey,
                &inspected.classification,
            )?;
            Ok(())
        }
        SessionState::FolderTrustPrompt if !no_yolo => {
            execute_keep_going_yolo_action(
                client,
                &pane.pane_id,
                GuardedWorkflow::ApprovePermission,
                &inspected.classification,
            )?;
            Ok(())
        }
        SessionState::PermissionDialog if !no_yolo && is_yolo_safe_to_approve(inspected, pane) => {
            execute_keep_going_yolo_action(
                client,
                &pane.pane_id,
                GuardedWorkflow::ApprovePermission,
                &inspected.classification,
            )?;
            Ok(())
        }
        state => Err(AppError::with_exit_code(
            format!(
                "prompt refused to continue from pane {} state {}; review manually{}",
                pane.pane_id,
                state.as_str(),
                if no_yolo { " (--no-yolo)" } else { "" }
            ),
            2,
        )),
    }
}

fn wait_for_fresh_last_message(
    pane: &TmuxPane,
    tmux: &TmuxClient,
    prior: Option<&crate::last_message::LastAgentMessage>,
    timeout: Duration,
) -> AppResult<crate::last_message::LastAgentMessage> {
    let deadline = Instant::now() + timeout;
    let mut last_error = None;
    loop {
        match load_last_agent_message(pane, tmux, 2000) {
            Ok(message)
                if prior
                    .map(|old| old.session_id != message.session_id || old.text != message.text)
                    .unwrap_or(true) =>
            {
                return Ok(message);
            }
            Ok(_) => {}
            Err(error) => last_error = Some(error.to_string()),
        }
        if Instant::now() >= deadline {
            return Err(AppError::new(match last_error {
                Some(error) if prior.is_none() => {
                    format!(
                        "Claude became ready but no assistant transcript message was found: {error}"
                    )
                }
                _ => String::from(
                    "Claude became ready but no new assistant transcript message was found; refusing to print stale output",
                ),
            }));
        }
        thread::sleep(Duration::from_millis(250));
    }
}

fn run_prepare_prompt(args: PreparePromptArgs) -> AppResult<String> {
    let state_dir = resolve_bootstrapped_state_dir(args.state_dir.as_deref())?;
    let workspace = resolve_selected_workspace(&state_dir, args.workspace.as_deref())?;
    let prompt_text = read_prompt_input(args.source.as_deref(), args.text.as_deref())?;
    prepare_prompt(&state_dir, &workspace.id, &args.session_name, &prompt_text)?;
    Ok(format!(
        "prepared prompt session={} workspace={} state_db={}",
        args.session_name,
        workspace.id,
        state_db_path(&state_dir).display()
    ))
}

fn run_editor_helper(args: EditorHelperArgs) -> AppResult<String> {
    let content = match args.source.as_deref() {
        Some(source_path) => {
            let prompt_text = resolve_prompt_text(PromptSource::File(source_path))?;
            write_editor_target(&args.target, &prompt_text)?;
            prompt_text
        }
        None => {
            let state_dir = resolve_bootstrapped_state_dir(args.state_dir.as_deref())?;
            let workspace = resolve_selected_workspace(&state_dir, args.workspace.as_deref())?;
            write_editor_target_from_pending(
                &state_dir,
                &workspace.id,
                &args.session_name,
                &args.target,
                !args.keep_pending,
            )?
        }
    };

    Ok(format!(
        "editor helper wrote {} bytes to {}",
        content.len(),
        args.target.display()
    ))
}

fn run_submit_prompt(args: SubmitPromptArgs) -> AppResult<String> {
    let client = TmuxClient::default();
    let pane = resolve_pane_by_id(&client, &args.pane_id)?;
    ensure_pane_owned_by_claude(&pane)?;
    let prompt_text = read_prompt_input(args.source.as_deref(), args.text.as_deref())?;
    let submitted = submit_prompt_for_pane(
        &client,
        &pane,
        &args.session_name,
        args.state_dir.as_deref(),
        args.workspace.as_deref(),
        &prompt_text,
        args.submit_delay_ms,
    )?;

    Ok(format!(
        "submitted prepared prompt session={} pane={} workspace={} state={} state_db={} delay_ms={}",
        args.session_name,
        submitted.pane_id,
        submitted.workspace_id,
        submitted.state,
        submitted.state_db.display(),
        submitted.delay_ms
    ))
}

pub(crate) struct PromptSubmissionOutcome {
    pub(crate) pane_id: String,
    pub(crate) workspace_id: String,
    pub(crate) state: String,
    pub(crate) state_db: PathBuf,
    pub(crate) delay_ms: u64,
}

pub(crate) fn submit_prompt_for_pane(
    client: &TmuxClient,
    pane: &TmuxPane,
    session_name: &str,
    state_dir_override: Option<&Path>,
    workspace_selector: Option<&str>,
    prompt_text: &str,
    submit_delay_ms: u64,
) -> AppResult<PromptSubmissionOutcome> {
    let mut classification = classify_pane(client, &pane.pane_id, ACTION_GUARD_HISTORY_LINES)?;
    if let Some(workflow) = submit_prompt_preflight_workflow(&classification) {
        execute_classified_workflow(client, &pane.pane_id, workflow, &classification)?;
        thread::sleep(Duration::from_millis(AUTO_UNSTICK_STEP_DELAY_MS));
        let after = classify_pane(client, &pane.pane_id, ACTION_GUARD_HISTORY_LINES)?;
        ensure_state_transition(&classification, &after, workflow.as_str())?;
        classification = after;
    }
    ensure_workflow_state(GuardedWorkflow::SubmitPrompt, &classification)?;

    let state_dir = resolve_bootstrapped_state_dir(state_dir_override)?;
    let workspace = resolve_workspace_for_pane(&state_dir, pane, workspace_selector)?;
    store_pending_prompt_for_tmux_instance(
        &state_dir,
        &workspace.id,
        session_name,
        pane,
        prompt_text,
    )?;

    let bindings = load_automation_keybindings(None)?;
    let before_submit = client.capture_pane(&pane.pane_id, KEEP_GOING_HISTORY_LINES)?;

    send_actions(
        client,
        &pane.pane_id,
        &prompt_submission_sequence(),
        &bindings,
        submit_delay_ms,
    )?;
    wait_for_prompt_submission_start(client, &pane.pane_id, &before_submit, None).map_err(|_| {
        AppError::new(format!(
            "submit-prompt sent the prepared prompt but pane {} did not show a prompt-submission transition",
            pane.pane_id
        ))
    })?;

    Ok(PromptSubmissionOutcome {
        pane_id: pane.pane_id.clone(),
        workspace_id: workspace.id,
        state: classification.state.as_str().to_string(),
        state_db: state_db_path(&state_dir),
        delay_ms: submit_delay_ms,
    })
}

fn run_yolo_start(args: YoloStartArgs) -> AppResult<String> {
    let state_dir = resolve_bootstrapped_state_dir(args.state_dir.as_deref())?;
    let (runtime, _runtime_guard) = ensure_runtime_client(
        &state_dir,
        if args.unmanaged {
            RuntimeManagementMode::Unmanaged
        } else {
            RuntimeManagementMode::Managed
        },
        RuntimeBootstrapConfig {
            reconcile_ms: args.poll_ms.max(1000),
            history_lines: 200,
        },
    )?;
    let client = managed_runtime_tmux_client();
    let updated = if args.all {
        runtime.set_yolo(None, args.workspace.as_deref(), true, true)?
    } else {
        let pane = client
            .pane_by_target(args.pane_id.as_deref().unwrap())?
            .ok_or_else(|| AppError::new("could not resolve yolo pane target"))?;
        runtime.set_yolo(Some(&pane.pane_id), args.workspace.as_deref(), false, true)?
    };
    if updated.is_empty() {
        Ok(String::from("no panes matched yolo policy update"))
    } else if args.follow || args.live_preview {
        let interrupted = Arc::new(AtomicBool::new(false));
        install_babysit_sigint_handler(Arc::clone(&interrupted))?;
        let mut subscription = runtime.subscribe()?;
        while !interrupted.load(Ordering::SeqCst) {
            if let Some(event) = subscription.recv()? {
                let matches_scope = if args.all {
                    event
                        .pane_id
                        .as_ref()
                        .map(|pane_id| updated.iter().any(|updated_id| updated_id == pane_id))
                        .unwrap_or(false)
                } else {
                    event
                        .pane_id
                        .as_ref()
                        .map(|pane_id| {
                            updated
                                .first()
                                .map(|updated_id| updated_id == pane_id)
                                .unwrap_or(false)
                        })
                        .unwrap_or(false)
                };
                if !matches_scope {
                    continue;
                }
                let payload = serde_json::json!({
                    "kind": event.kind,
                    "pane_id": event.pane_id,
                    "summary": event.summary,
                    "revision": event.revision,
                    "timestamp_unix_ms": event.timestamp_unix_ms,
                });
                emit_babysit_output(render_babysit_output(
                    args.format.into(),
                    payload,
                    supports_babysit_color(),
                ))?;
            }
        }
        Ok(String::new())
    } else {
        Ok(format!("updated yolo policy for {} pane(s)", updated.len()))
    }
}

fn run_yolo_stop(args: YoloStopArgs) -> AppResult<String> {
    let state_dir = resolve_bootstrapped_state_dir(args.state_dir.as_deref())?;
    let (runtime, _runtime_guard) = ensure_runtime_client(
        &state_dir,
        if args.unmanaged {
            RuntimeManagementMode::Unmanaged
        } else {
            RuntimeManagementMode::Managed
        },
        RuntimeBootstrapConfig {
            reconcile_ms: 1000,
            history_lines: 200,
        },
    )?;
    let updated = if args.all {
        runtime.set_yolo(None, args.workspace.as_deref(), true, false)?
    } else {
        let raw_target = args.pane_id.as_deref().unwrap();
        let pane_id = match managed_runtime_tmux_client().pane_by_target(raw_target)? {
            Some(pane) => pane.pane_id,
            None => raw_target.to_string(),
        };
        runtime.set_yolo(Some(&pane_id), args.workspace.as_deref(), false, false)?
    };
    if updated.is_empty() {
        Ok(String::from("no panes matched yolo policy update"))
    } else {
        Ok(format!("updated yolo policy for {} pane(s)", updated.len()))
    }
}

pub(crate) fn resolve_bootstrapped_state_dir(path: Option<&Path>) -> AppResult<PathBuf> {
    let state_dir = resolve_state_dir(path)?;
    bootstrap_state_db(&state_dir)?;
    Ok(state_dir)
}

fn resolve_selected_workspace(
    state_dir: &Path,
    selector: Option<&str>,
) -> AppResult<WorkspaceRecord> {
    let cwd = std::env::current_dir()?;
    resolve_workspace(state_dir, selector, &cwd)
}

pub(crate) fn resolve_workspace_for_pane(
    state_dir: &Path,
    pane: &TmuxPane,
    selector: Option<&str>,
) -> AppResult<WorkspaceRecord> {
    let pane_workspace = resolve_workspace_for_path(state_dir, Path::new(&pane.current_path))?;
    if let Some(selector) = selector {
        let selected = resolve_selected_workspace(state_dir, Some(selector))?;
        if selected.id != pane_workspace.id {
            return Err(AppError::new(format!(
                "workspace {} does not match pane {} workspace {}",
                selector, pane.pane_id, pane_workspace.workspace_root
            )));
        }
    }
    Ok(pane_workspace)
}

#[cfg(test)]
fn cleanup_babysit_record(state_dir: &Path, pane_id: &str) -> AppResult<()> {
    let _ = disable_yolo_record(state_dir, pane_id)?;
    Ok(())
}

fn install_babysit_sigint_handler(flag: Arc<AtomicBool>) -> AppResult<()> {
    ctrlc::set_handler(move || {
        flag.store(true, Ordering::SeqCst);
    })
    .map_err(|error| AppError::new(error.to_string()))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum YoloAction {
    Wait,
    ApprovePermission,
    /// Agy command-permission auto-approve (raw `Enter` to confirm the
    /// captured default option `> 1. Yes`). Kept separate from
    /// `ApprovePermission` for telemetry/readability — both paths converge
    /// inside `execute_classified_workflow` via `raw_key_for_workflow`. The
    /// interactive `keep-going` loop continues to reject agy panes at the
    /// pane-ownership gate, so this variant is observed in practice only by
    /// the runtime YOLO loop (`maybe_run_yolo_action`).
    ApproveAgyCommandPermission,
    DismissSurvey,
}

impl YoloAction {
    /// Stable string rendering for telemetry/event payloads. Kept in sync
    /// with the runtime event consumers (babysit log, `runtime` event
    /// stream): existing strings (`wait`, `approve-permission`,
    /// `dismiss-survey`) preserved verbatim so historical log parsers do
    /// not break. The agy variant gets its own distinct string so an
    /// operator scanning events can tell agy approves apart from Claude
    /// approves.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            YoloAction::Wait => "wait",
            YoloAction::ApprovePermission => "approve-permission",
            YoloAction::ApproveAgyCommandPermission => "approve-agy-command-permission",
            YoloAction::DismissSurvey => "dismiss-survey",
        }
    }
}

pub(crate) fn yolo_action_for_state(state: SessionState) -> YoloAction {
    match state {
        SessionState::ChatReady
        | SessionState::PromptEditing
        | SessionState::UserQuestionPrompt
        | SessionState::BusyResponding
        | SessionState::PlanApprovalPrompt
        | SessionState::FolderTrustPrompt
        | SessionState::StartupChoicePrompt
        | SessionState::ExternalEditorActive
        | SessionState::DiffDialog
        | SessionState::AgyFolderTrustPrompt
        | SessionState::AgySettingsPersistPrompt
        | SessionState::Unknown => YoloAction::Wait,
        SessionState::PermissionDialog => YoloAction::ApprovePermission,
        SessionState::AgyCommandPermissionPrompt => YoloAction::ApproveAgyCommandPermission,
        SessionState::SurveyPrompt => YoloAction::DismissSurvey,
    }
}

fn emit_babysit_output(line: String) -> AppResult<()> {
    let mut stdout = io::stdout().lock();
    writeln!(stdout, "{line}")?;
    stdout.flush()?;
    Ok(())
}

fn supports_babysit_color() -> bool {
    io::stdout().is_terminal()
        && std::env::var_os("NO_COLOR").is_none()
        && std::env::var("TERM")
            .map(|term| term != "dumb")
            .unwrap_or(true)
}

fn current_babysit_timestamp() -> String {
    let format = format_description!("[hour]:[minute]:[second].[subsecond digits:3]");
    OffsetDateTime::now_local()
        .unwrap_or_else(|_| OffsetDateTime::now_utc())
        .format(format)
        .unwrap_or_else(|_| String::from("00:00:00.000"))
}

fn pane_label_from_path(current_path: &str, fallback: &str) -> String {
    std::path::Path::new(current_path)
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .map(|name| name.to_string())
        .unwrap_or_else(|| fallback.to_string())
}

fn style_babysit(text: impl AsRef<str>, code: &str, use_color: bool) -> String {
    let text = text.as_ref();
    if use_color {
        format!("\x1b[{code}m{text}\x1b[0m")
    } else {
        text.to_string()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BabysitOutputFormat {
    Human,
    Jsonl,
}

impl From<BabysitFormat> for BabysitOutputFormat {
    fn from(value: BabysitFormat) -> Self {
        match value {
            BabysitFormat::Human => Self::Human,
            BabysitFormat::Jsonl => Self::Jsonl,
        }
    }
}

fn render_babysit_output(
    format: BabysitOutputFormat,
    event: serde_json::Value,
    use_color: bool,
) -> String {
    match format {
        BabysitOutputFormat::Jsonl => event.to_string(),
        BabysitOutputFormat::Human => render_babysit_human(event, use_color),
    }
}

fn render_babysit_human(event: serde_json::Value, use_color: bool) -> String {
    let kind = event
        .get("kind")
        .and_then(|v| v.as_str())
        .unwrap_or("event");
    let timestamp = style_babysit(
        format!("[{}]", current_babysit_timestamp()),
        "2;37",
        use_color,
    );
    let (icon, label, label_color) = match kind {
        "start" => ("🚀", "START", "1;32"),
        "serve-start" => ("📡", "SERVE", "1;34"),
        "wait" => ("👀", "WAIT", "1;33"),
        "snapshot" => ("👀", "STATE", "1;33"),
        "notify" => ("🔔", "NOTE", "1;36"),
        "pane-removed" => ("🔔", "NOTE", "1;36"),
        "approve" => ("🔐", "APPROVE", "1;36"),
        "approved" => ("✅", "APPROVED", "1;32"),
        "reject" => ("⛔", "REJECT", "1;31"),
        "dismiss-survey" => ("🙈", "DISMISS", "1;35"),
        "serve-stop" => ("🛑", "STOP", "1;31"),
        _ => ("⛔", "STOP", "1;31"),
    };
    let label = style_babysit(format!("{icon} {label:<8}"), label_color, use_color);
    let mut lines = vec![format!(
        "{timestamp} {label} {}",
        event.get("summary").and_then(|v| v.as_str()).unwrap_or("")
    )];
    for detail in event
        .get("details")
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
    {
        if let Some(text) = detail.as_str() {
            lines.push(format!("  │ {text}"));
        }
    }
    if let Some(body_title) = event.get("body_title").and_then(|v| v.as_str()) {
        lines.push(format!(
            "  │ {}",
            style_babysit(body_title, "1;37", use_color)
        ));
        if let Some(body_lines) = event.get("body_lines").and_then(|v| v.as_array()) {
            if body_lines.is_empty() {
                lines.push(String::from("  │   <empty>"));
            } else {
                for line in body_lines {
                    if let Some(text) = line.as_str() {
                        lines.push(format!("  │   {text}"));
                    }
                }
            }
        }
    }
    lines.join("\n")
}

#[cfg(test)]
fn babysit_prompt_json(details: Option<&PermissionPromptDetails>) -> serde_json::Value {
    match details {
        Some(details) => serde_json::json!({
            "type": details.prompt_type,
            "sandbox": details.sandbox_mode,
            "command": details.command,
            "reason": details.reason,
            "question": details.question,
        }),
        None => serde_json::Value::Null,
    }
}

#[cfg(test)]
#[allow(clippy::too_many_arguments)]
fn render_babysit_start_event(
    pane_label: &str,
    pane_id: &str,
    poll_ms: u64,
    live_preview: bool,
    record: &YoloRecord,
    record_path: &Path,
    format: BabysitFormat,
    use_color: bool,
) -> String {
    render_babysit_output(
        format.into(),
        serde_json::json!({
            "timestamp": current_babysit_timestamp(),
            "kind": "start",
            "pane_label": pane_label,
            "pane_id": pane_id,
            "poll_ms": poll_ms,
            "live_preview": live_preview,
            "session": record.session_name,
            "window": record.window_name,
            "command": record.current_command,
            "cwd": record.current_path,
            "record_path": record_path.display().to_string(),
            "summary": format!("YOLO started for {pane_label} (id:{pane_id})"),
            "details": [
                format!("Poll interval: {poll_ms}ms"),
                format!("Session: {}", record.session_name),
                format!("Window: {}", record.window_name),
                format!("Command: {}", record.current_command),
                format!("Cwd: {}", record.current_path),
                format!("Record: {}", record_path.display()),
                format!("Live preview: {live_preview}"),
            ],
        }),
        use_color,
    )
}

#[derive(Debug, Clone)]
pub(crate) struct InspectedPane {
    pub(crate) classification: Classification,
    pub(crate) focused_source: String,
    pub(crate) raw_source: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PermissionPromptDetails {
    pub(crate) prompt_type: String,
    pub(crate) sandbox_mode: Option<String>,
    pub(crate) command: Option<String>,
    pub(crate) reason: Option<String>,
    pub(crate) question: Option<String>,
}

pub(crate) fn inspect_pane(
    client: &TmuxClient,
    pane_id: &str,
    history_lines: usize,
) -> AppResult<InspectedPane> {
    let frame = client.capture_pane_ansi(pane_id, history_lines)?;
    let frame = prepare_frame_for_classification(&frame);
    let focused_source = focused_frame_source(&frame);
    Ok(InspectedPane {
        classification: Classifier.classify(pane_id, &focused_source),
        focused_source,
        raw_source: frame,
    })
}

pub(crate) fn extract_permission_prompt_details(
    inspected: &InspectedPane,
) -> Option<PermissionPromptDetails> {
    if inspected.classification.state != SessionState::PermissionDialog {
        return None;
    }

    let lines = permission_prompt_lines(inspected)?;
    let line_refs = lines.iter().map(String::as_str).collect::<Vec<_>>();
    let (title_idx, question_idx) = extract_permission_prompt_region(&line_refs)?;
    let title = line_refs[title_idx].trim();
    let (mut prompt_type, sandbox_mode) = parse_permission_prompt_title(title);

    let mut content_lines = Vec::new();
    let mut question = None;
    for line in lines.iter().take(question_idx + 1).skip(title_idx + 1) {
        let line = line.trim();
        if line.is_empty() || is_permission_decoration_line(line) {
            continue;
        }
        if is_permission_question_line(line) {
            question = Some(line.to_string());
            continue;
        }
        if is_permission_ignored_line(line) {
            continue;
        }
        content_lines.push(normalize_permission_content_line(line));
    }

    question.as_ref()?;

    if let Some(aside) = content_lines
        .last()
        .filter(|line| is_permission_annotation_line(line))
    {
        prompt_type = format!("{prompt_type} ({aside})");
        content_lines.pop();
    }

    let (command, reason) = match content_lines.as_slice() {
        [] => (None, None),
        [command] => (Some(command.clone()), None),
        [command, reason] => (Some(command.clone()), Some(reason.clone())),
        _ => {
            let reason = content_lines.last().cloned();
            let command = Some(content_lines[..content_lines.len() - 1].join(" "));
            (command, reason)
        }
    };

    Some(PermissionPromptDetails {
        prompt_type,
        sandbox_mode,
        command,
        reason,
        question,
    })
}

fn permission_prompt_lines(inspected: &InspectedPane) -> Option<Vec<String>> {
    permission_prompt_region_lines(&inspected.focused_source).or_else(|| {
        if inspected.raw_source == inspected.focused_source {
            None
        } else {
            permission_prompt_region_lines(&inspected.raw_source)
        }
    })
}

fn permission_prompt_region_lines(source: &str) -> Option<Vec<String>> {
    let lines = source
        .lines()
        .map(|line| line.to_string())
        .collect::<Vec<_>>();
    let line_refs = lines.iter().map(String::as_str).collect::<Vec<_>>();
    let (title_idx, _) = extract_permission_prompt_region(&line_refs)?;
    Some(lines.into_iter().skip(title_idx).collect())
}

fn extract_permission_prompt_region(lines: &[&str]) -> Option<(usize, usize)> {
    let question_idx = lines
        .iter()
        .rposition(|line| is_permission_question_line(line.trim()))?;
    let title_idx = find_permission_prompt_title_idx(&lines[..question_idx])?;
    Some((title_idx, question_idx))
}

fn find_permission_prompt_title_idx(lines: &[&str]) -> Option<usize> {
    lines
        .iter()
        .enumerate()
        .rfind(|(_, line)| {
            let trimmed = line.trim();
            !trimmed.is_empty()
                && !is_permission_decoration_line(trimmed)
                && is_plausible_permission_prompt_title(trimmed)
        })
        .map(|(idx, _)| idx)
}

fn is_plausible_permission_prompt_title(title: &str) -> bool {
    let Some((base, suffix)) = title.rsplit_once(" (") else {
        return matches!(
            title,
            "Bash command"
                | "Fetch"
                | "Monitor"
                | "JavaScript code"
                | "TypeScript code"
                | "Python code"
                | "Rust code"
                | "SQL query"
                | "Shell command"
                | "Command"
        );
    };
    let Some(suffix) = suffix.strip_suffix(')') else {
        return false;
    };
    if matches!(suffix, "sandboxed" | "unsandboxed") {
        return is_plausible_permission_prompt_title(base);
    }
    matches!(
        suffix,
        "Contains simple_expansion"
            | "Contains command substitution"
            | "Contains variable expansion"
            | "Contains globbing"
    ) && is_plausible_permission_prompt_title(base)
}

fn parse_permission_prompt_title(title: &str) -> (String, Option<String>) {
    if let Some((label, suffix)) = title.rsplit_once(" (")
        && let Some(mode) = suffix.strip_suffix(')')
        && matches!(mode, "sandboxed" | "unsandboxed")
    {
        return (label.to_string(), Some(mode.to_string()));
    }
    (title.to_string(), None)
}

fn is_permission_decoration_line(line: &str) -> bool {
    let trimmed = line.trim();
    !trimmed.is_empty()
        && trimmed
            .chars()
            .all(|ch| matches!(ch, '─' | '━' | '═' | '[' | ']' | '|' | ' '))
}

fn is_permission_question_line(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    lower.contains("do you want to proceed") || lower.ends_with('?')
}

fn is_permission_ignored_line(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    split_numbered_option_line(line).is_some()
        || lower.starts_with("1.")
        || lower.starts_with("2.")
        || lower.starts_with("allow once")
        || lower.starts_with("allow for session")
        || lower.starts_with("unhandled node type:")
        || lower == "no"
        || lower.starts_with("enter ")
        || lower.starts_with("escape ")
        || lower.starts_with("esc ")
        || lower.starts_with("tab ")
        || lower.starts_with("ctrl+")
}

fn normalize_permission_content_line(line: &str) -> String {
    line.trim_start_matches(['❯', '›']).trim_start().to_string()
}

fn permission_prompt_has_active_chat_input(inspected: &InspectedPane) -> bool {
    permission_prompt_lines(inspected)
        .unwrap_or_else(|| {
            inspected
                .focused_source
                .lines()
                .map(|line| line.to_string())
                .collect()
        })
        .iter()
        .skip_while(|line| !is_permission_question_line(line.trim()))
        .skip(1)
        .map(|line| line.trim())
        .any(is_chat_input_line_for_yolo)
}

fn is_permission_annotation_line(line: &str) -> bool {
    matches!(
        line.trim(),
        "Contains simple_expansion"
            | "Contains command substitution"
            | "Contains variable expansion"
            | "Contains globbing"
    )
}

#[cfg(test)]
fn render_permission_prompt_details(details: &PermissionPromptDetails) -> Vec<String> {
    let mut lines = vec![format!("Type: {}", details.prompt_type)];
    if let Some(mode) = &details.sandbox_mode {
        lines.push(format!("Sandbox: {mode}"));
    }
    if let Some(command) = &details.command {
        lines.push(format!("Command: {command}"));
    }
    if let Some(reason) = &details.reason {
        lines.push(format!("Reason: {reason}"));
    }
    if let Some(question) = &details.question {
        lines.push(format!("Question: {question}"));
    }
    lines
}

pub(crate) fn is_yolo_safe_to_approve(inspected: &InspectedPane, pane: &TmuxPane) -> bool {
    // Agy panes have their own per-shape safety contract: classify as the
    // command-permission variant, pane process must be `agy`, and the cursor
    // must still be on the captured default `> 1. Yes`. Folder-trust and
    // settings-persist agy shapes refuse here (returning false → YOLO loop
    // waits for manual review). Keep this branch first so a frame that
    // accidentally contains Claude-style permission strings on an agy pane
    // is not routed through the Claude/Codex dispatch.
    //
    // Gate the branch on the pane process (`current_command == "agy"`) rather
    // than on the classification signal: a Claude pane whose scrollback
    // contains "Antigravity"/"Gemini" can carry `SIGNAL_AGY_KEYWORDS` without
    // actually being an agy pane, and we must NOT refuse to approve a genuine
    // Claude PermissionDialog in that case. The downstream
    // `is_agy_command_permission_safe_to_approve` re-checks `is_agy_pane`
    // for defense-in-depth.
    if is_pane_command_agy(pane) {
        return is_agy_command_permission_safe_to_approve(inspected, pane);
    }

    if inspected.classification.state != SessionState::PermissionDialog {
        return false;
    }

    if permission_manual_review_reason(&inspected.classification).is_some() {
        return false;
    }

    if is_codex_permission_prompt_safe_to_approve(inspected) {
        return true;
    }

    let Some(details) = extract_permission_prompt_details(inspected) else {
        return has_project_scoped_permission_option(inspected)
            && !permission_prompt_lines(inspected)
                .unwrap_or_else(|| {
                    inspected
                        .focused_source
                        .lines()
                        .map(|line| line.to_string())
                        .collect()
                })
                .iter()
                .map(|line| line.trim())
                .any(is_chat_input_line_for_yolo);
    };

    details.question.is_some()
        && details.command.is_some()
        && !permission_prompt_has_active_chat_input(inspected)
}

/// Per-shape YOLO safety gate for agy panes. Approve is allowed iff all three
/// preconditions hold:
///
/// 1. The frame classified as `AgyCommandPermissionPrompt` (so neither
///    folder-trust nor settings-persist shapes reach this code path).
/// 2. The pane process is actually `agy` — the frame fingerprint alone is
///    insufficient because a Claude/Codex pane could contain the literal
///    string "Antigravity" in its scrollback (see consolidated critique #5).
/// 3. The cursor is still on the captured default option (`> 1. Yes`). If
///    the user arrowed the selector to option 3 (the global-settings persist
///    option), `Enter` would mutate settings.json. This guard refuses to
///    fire in that case; the YOLO loop will wait for the next tick.
pub(crate) fn is_agy_command_permission_safe_to_approve(
    inspected: &InspectedPane,
    pane: &TmuxPane,
) -> bool {
    inspected.classification.state == SessionState::AgyCommandPermissionPrompt
        && crate::agy::is_agy_pane(pane)
        && crate::agy::agy_command_permission_default_option_is_yes(&inspected.raw_source)
}

fn is_codex_permission_prompt_safe_to_approve(inspected: &InspectedPane) -> bool {
    if !inspected
        .classification
        .signals
        .iter()
        .any(|signal| signal == SIGNAL_CODEX_KEYWORDS)
    {
        return false;
    }

    codex_permission_prompt_source_is_safe(&inspected.focused_source)
        || codex_permission_prompt_source_is_safe(&inspected.raw_source)
}

fn codex_permission_prompt_source_is_safe(source: &str) -> bool {
    let lines = source.lines().map(str::trim).collect::<Vec<_>>();
    let Some(prompt_start) = latest_codex_permission_prompt_start(&lines) else {
        return false;
    };
    let prompt_lines = &lines[prompt_start..];
    let normalized = prompt_lines.join("\n").to_ascii_lowercase();

    normalized.contains("yes, proceed")
        && (prompt_lines
            .iter()
            .copied()
            .any(|line| line.eq_ignore_ascii_case("would you like to run the following command?"))
            || codex_permission_confirm_footer_is_live_tail(prompt_lines)
            || codex_permission_options_are_live_tail(prompt_lines))
        && !prompt_lines
            .iter()
            .copied()
            .any(is_chat_input_line_for_yolo)
}

fn latest_codex_permission_prompt_start(lines: &[&str]) -> Option<usize> {
    let prompt_header = lines.iter().rposition(|line| {
        line.eq_ignore_ascii_case("would you like to run the following command?")
    });
    if codex_permission_confirm_footer_is_live_tail(lines)
        || codex_permission_options_are_live_tail(lines)
    {
        return prompt_header.or_else(|| {
            lines
                .iter()
                .rposition(|line| line.to_ascii_lowercase().contains("yes, proceed"))
        });
    }
    prompt_header
}

fn codex_permission_options_are_live_tail(lines: &[&str]) -> bool {
    let tail = lines
        .iter()
        .rev()
        .filter_map(|line| {
            let trimmed = line.trim();
            (!trimmed.is_empty()).then_some(trimmed.to_ascii_lowercase())
        })
        .take(6)
        .collect::<Vec<_>>();

    !tail.is_empty()
        && tail.iter().any(|line| line.contains("yes, proceed"))
        && tail
            .iter()
            .any(|line| line.contains("no, and tell codex what to do differently"))
}

fn codex_permission_confirm_footer_is_live_tail(lines: &[&str]) -> bool {
    lines
        .iter()
        .rev()
        .find(|line| !line.trim().is_empty())
        .is_some_and(|line| {
            line.trim()
                .eq_ignore_ascii_case(CODEX_PERMISSION_CONFIRM_FOOTER)
        })
}

fn has_project_scoped_permission_option(inspected: &InspectedPane) -> bool {
    permission_prompt_lines(inspected)
        .unwrap_or_else(|| {
            inspected
                .focused_source
                .lines()
                .map(|line| line.to_string())
                .collect()
        })
        .iter()
        .map(|line| line.trim())
        .any(is_project_scoped_permission_option_line)
}

fn is_project_scoped_permission_option_line(line: &str) -> bool {
    let Some((_, rest)) = split_numbered_option_line(line) else {
        return false;
    };
    let lower = rest.to_ascii_lowercase();
    lower.starts_with("yes, and always allow access to ") && lower.ends_with(" from this project")
}

fn split_numbered_option_line(line: &str) -> Option<(&str, &str)> {
    let trimmed = line.trim_start_matches('❯').trim();
    let digits = trimmed.chars().take_while(|ch| ch.is_ascii_digit()).count();
    if digits == 0 || !trimmed[digits..].starts_with('.') {
        return None;
    }

    let rest = trimmed[digits + 1..].trim();
    if rest.is_empty() {
        None
    } else {
        Some((&trimmed[..digits], rest))
    }
}

fn permission_manual_review_reason(classification: &Classification) -> Option<&'static str> {
    if classification.state != SessionState::PermissionDialog {
        return None;
    }

    if classification
        .signals
        .iter()
        .any(|signal| signal == SIGNAL_SELF_SETTINGS_LANGUAGE)
    {
        Some("Claude wants to edit its own settings")
    } else if classification
        .signals
        .iter()
        .any(|signal| signal == SIGNAL_SENSITIVE_CLAUDE_PATH)
    {
        Some("prompt targets Claude settings or slash-command paths")
    } else {
        None
    }
}

fn is_chat_input_line_for_yolo(line: &str) -> bool {
    let trimmed = line.trim();
    if trimmed == ">" || trimmed == "❯" || trimmed == "›" {
        return true;
    }

    let Some(rest) = trimmed
        .strip_prefix('❯')
        .or_else(|| trimmed.strip_prefix('›'))
    else {
        return false;
    };
    let rest = rest.trim();
    !rest.is_empty() && !starts_with_numbered_option_for_yolo(rest)
}

fn starts_with_numbered_option_for_yolo(line: &str) -> bool {
    let digits = line.chars().take_while(|ch| ch.is_ascii_digit()).count();
    digits > 0 && line[digits..].starts_with('.')
}

#[cfg(test)]
fn render_babysit_wait_event(
    pane_label: &str,
    pane_id: &str,
    poll_count: usize,
    inspected: &InspectedPane,
    _live_preview: bool,
    format: BabysitFormat,
    use_color: bool,
) -> String {
    let classification = &inspected.classification;
    let mut details = Vec::new();
    if classification.recap_present {
        details.push(String::from("Recap: present"));
    }
    if let Some(permission_details) = extract_permission_prompt_details(inspected) {
        details.extend(render_permission_prompt_details(&permission_details));
    }
    let body_title: Option<&str> = None;
    let body_lines: Vec<String> = Vec::new();
    let prompt_details = extract_permission_prompt_details(inspected);
    let json_details = serde_json::json!([format!("Signals: {}", render_signals(classification))]);
    render_babysit_output(
        format.into(),
        serde_json::json!({
            "timestamp": current_babysit_timestamp(),
            "kind": "wait",
            "pane_label": pane_label,
            "pane_id": pane_id,
            "poll": poll_count,
            "state": classification.state.as_str(),
            "signals": classification.signals.clone(),
            "recap_present": classification.recap_present,
            "recap_excerpt": classification.recap_excerpt,
            "prompt": babysit_prompt_json(prompt_details.as_ref()),
            "summary": format!("Watching {pane_label} (id:{pane_id}) (poll #{poll_count})"),
            "details": if format == BabysitFormat::Human { serde_json::json!(details) } else { json_details },
            "body_title": body_title,
            "body_lines": body_lines,
        }),
        use_color,
    )
}

#[cfg(test)]
#[allow(clippy::too_many_arguments)]
fn render_babysit_action_event(
    pane_label: &str,
    pane_id: &str,
    poll_count: usize,
    inspected: &InspectedPane,
    workflow: GuardedWorkflow,
    live_preview: bool,
    format: BabysitFormat,
    use_color: bool,
) -> String {
    let classification = &inspected.classification;
    let prompt_details = extract_permission_prompt_details(inspected);
    let json_details = serde_json::json!([format!("Signals: {}", render_signals(classification))]);
    let human_details: Vec<String> = Vec::new();
    let body_title = if live_preview {
        Some(match workflow {
            GuardedWorkflow::ApprovePermission => "Permission prompt",
            GuardedWorkflow::DismissSurvey => "Survey prompt",
            GuardedWorkflow::RejectPermission | GuardedWorkflow::SubmitPrompt => "Prompt",
        })
    } else {
        None
    };
    let body_lines = if live_preview {
        inspected
            .focused_source
            .lines()
            .map(str::to_string)
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    render_babysit_output(
        format.into(),
        serde_json::json!({
            "timestamp": current_babysit_timestamp(),
            "kind": workflow.as_str(),
            "pane_label": pane_label,
            "pane_id": pane_id,
            "poll": poll_count,
            "state": classification.state.as_str(),
            "signals": classification.signals.clone(),
            "prompt": babysit_prompt_json(prompt_details.as_ref()),
            "summary": format!(
                "{} {pane_label} (id:{pane_id}) (poll #{poll_count})",
                match workflow {
                    GuardedWorkflow::ApprovePermission => "Auto-approving",
                    GuardedWorkflow::DismissSurvey => "Auto-dismissing",
                    GuardedWorkflow::RejectPermission => "Auto-rejecting",
                    GuardedWorkflow::SubmitPrompt => "Auto-submitting",
                }
            ),
            "details": if format == BabysitFormat::Human { serde_json::json!(human_details) } else { json_details },
            "body_title": body_title,
            "body_lines": body_lines
        }),
        use_color,
    )
}

pub(crate) fn classify_pane(
    client: &TmuxClient,
    pane_id: &str,
    history_lines: usize,
) -> AppResult<Classification> {
    let frame = client.capture_pane_ansi(pane_id, history_lines)?;
    let frame = prepare_frame_for_classification(&frame);
    let focused = focused_frame_source(&frame);
    Ok(Classifier.classify(pane_id, &focused))
}

pub(crate) fn is_startup_enter_prompt(frame: &str) -> bool {
    let lower = frame.to_ascii_lowercase();
    lower.contains("press enter to confirm")
        && !has_startup_choice_prompt_text(frame)
        && (lower.contains("choose how you'd like codex to proceed")
            || lower.contains("introducing gpt-")
            || lower.contains("try new model"))
}

pub(crate) fn focused_frame_source(frame: &str) -> String {
    let focused = focus_live_frame(frame, 30);
    if focused.is_empty() {
        frame.to_string()
    } else {
        focused
    }
}

pub(crate) fn ensure_workflow_state(
    workflow: GuardedWorkflow,
    classification: &Classification,
) -> AppResult<()> {
    validate_workflow_state(workflow, classification)
        .map_err(|error| AppError::with_exit_code(error, 2))
}

fn send_actions(
    client: &TmuxClient,
    pane_id: &str,
    actions: &[AutomationAction],
    bindings: &ResolvedKeybindings,
    external_editor_delay_ms: u64,
) -> AppResult<()> {
    for action in actions {
        let keys = keys_for_action(bindings, *action)?;
        client.send_keys(pane_id, keys)?;
        if matches!(action, AutomationAction::ExternalEditor) {
            thread::sleep(Duration::from_millis(external_editor_delay_ms));
        }
    }

    Ok(())
}

fn render_action_names(actions: &[AutomationAction]) -> String {
    actions
        .iter()
        .map(|action| action.as_str())
        .collect::<Vec<_>>()
        .join(",")
}

fn is_usable_state(state: SessionState) -> bool {
    matches!(
        state,
        SessionState::ChatReady | SessionState::BusyResponding
    )
}

#[derive(Debug, Clone)]
pub(crate) struct ContinueOutcome {
    pub(crate) action: String,
    pub(crate) outcome: String,
    pub(crate) after: Classification,
    pub(crate) used_permission_approval: bool,
}

pub(crate) fn continue_from_classification(
    client: &TmuxClient,
    pane: &TmuxPane,
    classification: &Classification,
) -> AppResult<ContinueOutcome> {
    if let Some(reason) = permission_manual_review_reason(classification) {
        return Err(AppError::new(format!(
            "no safe automatic recovery is defined for PermissionDialog; {reason}"
        )));
    }

    match recovery_action_for_state(classification.state)? {
        Some(RecoveryAction::ApprovePermission) => {
            execute_classified_workflow(
                client,
                &pane.pane_id,
                GuardedWorkflow::ApprovePermission,
                classification,
            )?;
            thread::sleep(Duration::from_millis(AUTO_UNSTICK_STEP_DELAY_MS));
            let after = classify_pane(client, &pane.pane_id, ACTION_GUARD_HISTORY_LINES)?;
            ensure_state_transition(classification, &after, "approve")?;
            Ok(ContinueOutcome {
                action: String::from("approve"),
                outcome: render_continue_outcome(after.state),
                after,
                used_permission_approval: true,
            })
        }
        Some(RecoveryAction::DismissSurvey) => {
            execute_classified_workflow(
                client,
                &pane.pane_id,
                GuardedWorkflow::DismissSurvey,
                classification,
            )?;
            thread::sleep(Duration::from_millis(AUTO_UNSTICK_STEP_DELAY_MS));
            let after = classify_pane(client, &pane.pane_id, ACTION_GUARD_HISTORY_LINES)?;
            ensure_state_transition(classification, &after, "dismiss-survey")?;
            Ok(ContinueOutcome {
                action: String::from("dismiss-survey"),
                outcome: render_continue_outcome(after.state),
                after,
                used_permission_approval: false,
            })
        }
        Some(RecoveryAction::ConfirmFolderTrust) => {
            client.send_keys(&pane.pane_id, &["Enter"])?;
            thread::sleep(Duration::from_millis(AUTO_UNSTICK_STEP_DELAY_MS));
            let after = classify_pane(client, &pane.pane_id, ACTION_GUARD_HISTORY_LINES)?;
            ensure_state_transition(classification, &after, "confirm-folder-trust")?;
            Ok(ContinueOutcome {
                action: String::from("confirm-folder-trust"),
                outcome: render_continue_outcome(after.state),
                after,
                used_permission_approval: false,
            })
        }
        None => Ok(ContinueOutcome {
            action: String::from("none"),
            outcome: String::from("already-usable"),
            after: classification.clone(),
            used_permission_approval: false,
        }),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecoveryAction {
    ApprovePermission,
    DismissSurvey,
    ConfirmFolderTrust,
}

fn recovery_action_for_state(state: SessionState) -> AppResult<Option<RecoveryAction>> {
    match state {
        SessionState::ChatReady | SessionState::BusyResponding => Ok(None),
        SessionState::PromptEditing => Err(AppError::new(
            "no safe automatic recovery is defined for PromptEditing; submit or clear the prompt manually",
        )),
        SessionState::UserQuestionPrompt => Err(AppError::new(
            "no safe automatic recovery is defined for UserQuestionPrompt; review the pane manually",
        )),
        SessionState::PermissionDialog => Ok(Some(RecoveryAction::ApprovePermission)),
        SessionState::PlanApprovalPrompt => Err(AppError::new(
            "no safe automatic recovery is defined for PlanApprovalPrompt; review the pane manually",
        )),
        SessionState::SurveyPrompt => Ok(Some(RecoveryAction::DismissSurvey)),
        SessionState::FolderTrustPrompt => Ok(Some(RecoveryAction::ConfirmFolderTrust)),
        SessionState::StartupChoicePrompt => Err(AppError::new(
            "no safe automatic recovery is defined for StartupChoicePrompt; choose the startup option manually",
        )),
        SessionState::DiffDialog => Err(AppError::new(
            "no safe automatic recovery is defined for DiffDialog; review the pane manually",
        )),
        SessionState::ExternalEditorActive => Err(AppError::new(
            "no safe automatic recovery is defined while the external editor is active",
        )),
        SessionState::Unknown => Err(AppError::new(
            "no safe automatic recovery is defined for Unknown state; refusing to guess",
        )),
        SessionState::AgyCommandPermissionPrompt => Err(AppError::new(
            "auto-unstick is not supported for agy command-permission prompts; opt in to YOLO with `botctl yolo start --pane <agy-pane>` instead",
        )),
        SessionState::AgyFolderTrustPrompt => Err(AppError::new(
            "auto-unstick is not supported for agy folder-trust prompts; confirm the workspace manually",
        )),
        SessionState::AgySettingsPersistPrompt => Err(AppError::new(
            "auto-unstick is not supported for agy settings-persist prompts; review and confirm manually",
        )),
    }
}

fn render_continue_outcome(state: SessionState) -> String {
    if is_usable_state(state) {
        String::from("usable")
    } else {
        format!("still-blocked:{}", state.as_str())
    }
}

fn submit_prompt_preflight_workflow(classification: &Classification) -> Option<GuardedWorkflow> {
    match classification.state {
        SessionState::SurveyPrompt => Some(GuardedWorkflow::DismissSurvey),
        _ => None,
    }
}

fn ensure_state_transition(
    before: &Classification,
    after: &Classification,
    action: &str,
) -> AppResult<()> {
    if before.state == after.state {
        Err(AppError::new(format!(
            "{action} did not change the classified state from {}; refusing to retry on potentially stale screen data",
            before.state.as_str()
        )))
    } else {
        Ok(())
    }
}

fn focus_live_frame(frame: &str, max_non_empty_lines: usize) -> String {
    let lines = frame
        .lines()
        .map(str::trim_end)
        .filter(|line| !line.trim().is_empty())
        .collect::<Vec<_>>();

    if let Some(prompt_idx) = lines.iter().rposition(is_focus_prompt_line) {
        let trailing = (max_non_empty_lines / 5).max(1);
        let start = prompt_idx.saturating_sub(max_non_empty_lines.saturating_sub(trailing));
        let end = (prompt_idx + trailing + 1).min(lines.len());
        return lines[start..end].join("\n");
    }

    let start = lines.len().saturating_sub(max_non_empty_lines);
    lines[start..].join("\n")
}

fn is_focus_prompt_line(line: &&str) -> bool {
    let trimmed = line.trim();
    matches!(trimmed, ">" | "❯")
        || trimmed == "›"
        || trimmed.starts_with("❯ 1.")
        || trimmed.starts_with("❯ 2.")
        || trimmed.starts_with("❯ 3.")
        || trimmed.starts_with("› 1.")
        || trimmed.starts_with("› 2.")
        || trimmed.starts_with("› 3.")
        || (trimmed.starts_with("› ")
            && !starts_with_numbered_focus_option(trimmed.trim_start_matches('›').trim()))
        // Grok Build TUI prompt rows: `│ ❯` or `│ ❯ Build anything`
        || (trimmed.contains('│') && trimmed.contains('❯') && !trimmed.contains("❯ 1."))
}

fn starts_with_numbered_focus_option(line: &str) -> bool {
    let trimmed = line.trim_start();
    let mut chars = trimmed.chars();
    matches!(chars.next(), Some('0'..='9')) && matches!(chars.next(), Some('.' | ')'))
}

fn raw_key_for_workflow(
    workflow: GuardedWorkflow,
    classification: &Classification,
) -> Option<(&'static str, &'static str)> {
    match (workflow, classification.state) {
        (GuardedWorkflow::ApprovePermission, SessionState::PermissionDialog)
            if classification
                .signals
                .iter()
                .any(|signal| signal == SIGNAL_CODEX_KEYWORDS) =>
        {
            Some(("y", "y"))
        }
        (GuardedWorkflow::ApprovePermission, SessionState::FolderTrustPrompt) => {
            Some(("Enter", "enter"))
        }
        // Agy command-permission prompt: the captured shape pre-positions the
        // cursor on `> 1. Yes`, so approve is literally raw `Enter`. Routing
        // through `~/.claude/keybindings.json` would be wrong (the binding
        // exists in the user's Claude keymap, not in agy's), so we mirror the
        // FolderTrustPrompt special case. The safety predicate
        // `is_agy_command_permission_safe_to_approve` (in this module) is
        // responsible for ensuring the cursor is actually still on option 1
        // before this arm is reached.
        (GuardedWorkflow::ApprovePermission, SessionState::AgyCommandPermissionPrompt) => {
            Some(("Enter", "enter"))
        }
        (GuardedWorkflow::DismissSurvey, SessionState::SurveyPrompt) => Some(("0", "0")),
        _ => None,
    }
}

pub(crate) fn execute_classified_workflow(
    client: &TmuxClient,
    pane_id: &str,
    workflow: GuardedWorkflow,
    classification: &Classification,
) -> AppResult<String> {
    if let Some((raw_key, rendered)) = raw_key_for_workflow(workflow, classification) {
        client.send_keys(pane_id, &[raw_key])?;
        Ok(String::from(rendered))
    } else {
        let bindings = load_automation_keybindings(None)?;
        send_actions(client, pane_id, workflow.actions(), &bindings, 0)?;
        Ok(render_action_names(workflow.actions()))
    }
}

pub(crate) fn load_automation_keybindings(path: Option<&Path>) -> AppResult<ResolvedKeybindings> {
    load_resolved_keybindings(path).map_err(AppError::new)
}

pub(crate) fn keys_for_action(
    bindings: &ResolvedKeybindings,
    action: AutomationAction,
) -> AppResult<&[String]> {
    bindings.keys_for(action).ok_or_else(|| {
        AppError::new(format!(
            "Claude keybindings at {} do not define action {}; run doctor to inspect the missing automation bindings",
            bindings.path.display(),
            action.as_str()
        ))
    })
}

fn resolve_pane_by_id(client: &TmuxClient, pane_id: &str) -> AppResult<TmuxPane> {
    client
        .pane_by_target(pane_id)?
        .ok_or_else(|| AppError::new(format!("pane not found: {pane_id}")))
}

fn resolve_target_pane(client: &TmuxClient, target: &PaneTargetArgs) -> AppResult<TmuxPane> {
    match (
        target.pane_id.as_deref(),
        target.session_name.as_deref(),
        target.window_name.as_deref(),
    ) {
        (Some(pane_id), None, None) => resolve_pane_by_id(client, pane_id),
        (None, Some(session_name), None) => client
            .active_pane_for_session(session_name)?
            .ok_or_else(|| {
                AppError::new(format!("no active pane found for session {session_name}"))
            }),
        (None, Some(session_name), Some(window_name)) => client
            .active_pane_for_window(session_name, window_name)?
            .ok_or_else(|| {
                AppError::new(format!(
                    "no active pane found for session {} window {}",
                    session_name, window_name
                ))
            }),
        _ => Err(AppError::new(
            "target must use either --pane %ID or --session NAME [--window NAME]",
        )),
    }
}

fn normalize_dashboard_window_name(client: &TmuxClient, pane: &TmuxPane) -> AppResult<TmuxPane> {
    let base_name = strip_dashboard_window_prefixes(&pane.window_name);
    if base_name == pane.window_name {
        return Ok(pane.clone());
    }

    client.rename_window(
        &format!("{}:{}", pane.session_name, pane.window_index),
        &base_name,
    )?;
    resolve_pane_by_id(client, &pane.pane_id)
}

fn strip_dashboard_window_prefixes(value: &str) -> String {
    const DASHBOARD_STATE_EMOJIS: &[&str] = &[
        "⚙️", "💬", "🤔", "💤", "🔐", "❓", "📁", "📝", "✏️", "🧾", "❔",
    ];
    const DASHBOARD_YOLO_MARKER: &str = "ʸ";
    const LEGACY_YOLO_PREFIX_CHARS: &[&str] = &["🤠", "\u{200D}"];

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
    rest.to_string()
}

fn resolve_doctor_pane(
    client: &TmuxClient,
    session_name: Option<&str>,
    pane_id: Option<&str>,
) -> AppResult<TmuxPane> {
    match (pane_id, session_name) {
        (Some(pane_id), _) => resolve_pane_by_id(client, pane_id),
        (None, Some(session_name)) => {
            client
                .active_pane_for_session(session_name)?
                .ok_or_else(|| {
                    AppError::new(format!("no active pane found for session {session_name}"))
                })
        }
        (None, None) => Err(AppError::new(
            "doctor requires either an explicit pane or a session name",
        )),
    }
}

fn render_status_report(
    pane: &TmuxPane,
    classification: &Classification,
    bindings: &KeybindingsInspection,
    frame: &str,
) -> String {
    format!(
        "pane={}\nsession={}\nwindow={}\nactive={}\nprovider={}\ncommand={}\ncwd={}\ncursor={}\nstate={}\nhas_questions={}\nrecap_present={}\nrecap_excerpt={}\nsignals={}\nscreen_excerpt={}\nnext_safe_action={}\nbindings_path={}\nbindings_status={}\nbindings_missing={}",
        pane.pane_id,
        pane.session_name,
        pane.window_name,
        pane.pane_active,
        classification_provider_label(classification, pane),
        pane.current_command,
        pane.current_path,
        render_cursor(pane),
        classification.state.as_str(),
        classification.has_questions,
        classification.recap_present,
        classification.recap_excerpt.as_deref().unwrap_or("none"),
        render_signals(classification),
        render_screen_excerpt(frame),
        render_next_safe_action(classification, pane, bindings),
        bindings.path.display(),
        bindings.status.as_str(),
        render_missing_bindings(bindings)
    )
}

fn render_status_json(
    pane: &TmuxPane,
    classification: &Classification,
    bindings: &KeybindingsInspection,
    frame: &str,
    automation_ready: bool,
) -> AppResult<String> {
    let mut json = serde_json::json!({
        "pane_id": pane.pane_id,
        "session": pane.session_name,
        "window": pane.window_name,
        "active": pane.pane_active,
        "provider": classification_provider_label(classification, pane),
        "command": pane.current_command,
        "command_matches_claude": is_pane_command_claude(pane),
        "cwd": pane.current_path,
        "cursor": {
            "x": pane.cursor_x,
            "y": pane.cursor_y,
        },
        "state": classification.state.as_str(),
        "has_questions": classification.has_questions,
        "recap_present": classification.recap_present,
        "recap_excerpt": classification.recap_excerpt.as_deref(),
        "signals": classification.signals.clone(),
        "screen_excerpt": render_screen_excerpt(frame),
        "automation_ready": automation_ready,
        "next_safe_action": render_next_safe_action(classification, pane, bindings),
        "bindings": {
            "path": bindings.path.display().to_string(),
            "status": bindings.status.as_str(),
            "missing": bindings.missing_bindings.clone(),
        },
    });
    // "agy_model" is present (possibly null) for agy panes and absent for all
    // others (D4=A decision from the MBOD). Use is_classified_agy (not
    // is_pane_command_agy) so signal-fingerprinted panes — where the process
    // name may not be "agy" yet but "provider" is already "Antigravity" — also
    // get the key (V-3 contract fix).
    if is_classified_agy(classification, pane) {
        let model_value = crate::agy::extract_model_label(frame)
            .map_or(serde_json::Value::Null, serde_json::Value::String);
        json["agy_model"] = model_value;
    }
    serde_json::to_string_pretty(&json)
        .map_err(|error| AppError::new(format!("failed to encode status JSON: {error}")))
}

pub(crate) fn render_screen_excerpt(frame: &str) -> String {
    let excerpt = focus_live_frame(frame, 8);
    if excerpt.is_empty() {
        String::from("none")
    } else {
        excerpt.replace('\n', " | ")
    }
}

pub(crate) fn render_next_safe_action(
    classification: &Classification,
    pane: &TmuxPane,
    bindings: &KeybindingsInspection,
) -> String {
    if is_classified_agy(classification, pane) {
        // Per-shape dispatch: only the command-permission shape is
        // auto-approvable; the other two require manual review. The default
        // arm covers any future agy state that has not been wired up yet.
        return match classification.state {
            SessionState::AgyCommandPermissionPrompt => {
                String::from("safe-action: approve (Enter)")
            }
            SessionState::AgyFolderTrustPrompt => {
                String::from("manual-review: agy folder-trust prompt")
            }
            SessionState::AgySettingsPersistPrompt => {
                String::from("manual-review: agy settings-persist prompt")
            }
            _ => String::from("manual-review: agy automation is not implemented"),
        };
    }
    if is_classified_codex(classification, pane) {
        return match classification.state {
            SessionState::BusyResponding => String::from("safe-action: wait"),
            SessionState::PermissionDialog => String::from("safe-action: approve (y)"),
            _ => String::from("manual-review: Codex automation is not implemented"),
        };
    }
    if !is_pane_command_claude(pane) {
        return format!("manual-review: pane is running {}", pane.current_command);
    }
    if bindings.status != KeybindingsStatus::Valid {
        return String::from("manual-review: fix Claude keybindings before automation");
    }
    match classification.state {
        SessionState::ChatReady => String::from("safe-action: submit-prompt"),
        SessionState::PromptEditing => String::from("manual-review: prompt has unsubmitted input"),
        SessionState::UserQuestionPrompt => {
            String::from("manual-review: Claude is asking an operator question")
        }
        SessionState::BusyResponding => String::from("safe-action: wait"),
        SessionState::PermissionDialog => {
            if let Some(reason) = permission_manual_review_reason(classification) {
                format!("manual-review: {reason}")
            } else {
                String::from("safe-action: approve")
            }
        }
        SessionState::PlanApprovalPrompt => {
            String::from("manual-review: plan approval prompt needs operator confirmation")
        }
        SessionState::SurveyPrompt => String::from("safe-action: dismiss-survey (0)"),
        SessionState::FolderTrustPrompt => String::from("safe-action: approve (Enter)"),
        SessionState::StartupChoicePrompt => {
            String::from("manual-review: startup choice prompt needs operator selection")
        }
        SessionState::DiffDialog => {
            String::from("manual-review: diff dialog needs operator confirmation")
        }
        SessionState::ExternalEditorActive => {
            String::from("manual-review: external editor is active")
        }
        SessionState::Unknown => String::from("manual-review: classify the pane before acting"),
        // Defensive: agy panes short-circuit at the top of this function via
        // `is_classified_agy`, so these arms are dead-code. Spell them out
        // explicitly so a future change that drops the agy short-circuit
        // cannot silently route agy frames through the Claude action map.
        SessionState::AgyCommandPermissionPrompt
        | SessionState::AgyFolderTrustPrompt
        | SessionState::AgySettingsPersistPrompt => {
            String::from("manual-review: agy panes use the per-shape doctor recommendation")
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn render_guarded_workflow_output(
    workflow: GuardedWorkflow,
    pane_label: &str,
    pane_id: &str,
    classification: &Classification,
    actions: &str,
    used_raw_enter: bool,
    format: BabysitFormat,
    use_color: bool,
) -> String {
    let summary = format!(
        "{} for {pane_label} (id:{pane_id}) in state {}",
        match workflow {
            GuardedWorkflow::ApprovePermission => "Approval sent",
            GuardedWorkflow::RejectPermission => "Rejection sent",
            GuardedWorkflow::DismissSurvey => "Survey dismissed",
            GuardedWorkflow::SubmitPrompt => "Prompt submitted",
        },
        classification.state.as_str()
    );
    render_babysit_output(
        format.into(),
        serde_json::json!({
            "timestamp": current_babysit_timestamp(),
            "kind": workflow.as_str(),
            "workflow": workflow.as_str(),
            "action": workflow.as_str(),
            "pane_label": pane_label,
            "pane_id": pane_id,
            "state": classification.state.as_str(),
            "signals": classification.signals.clone(),
            "actions": actions,
            "used_raw_enter": used_raw_enter,
            "summary": summary,
            "details": [
                format!("Workflow: {}", workflow.as_str()),
                format!("State: {}", classification.state.as_str()),
                format!("Signals: {}", render_signals(classification)),
                format!("Actions: {actions}"),
                format!("Folder trust raw Enter: {used_raw_enter}"),
            ],
        }),
        use_color,
    )
}

fn render_cursor(pane: &TmuxPane) -> String {
    match (pane.cursor_x, pane.cursor_y) {
        (Some(x), Some(y)) => format!("{x},{y}"),
        _ => String::from("-"),
    }
}

fn render_signals(classification: &Classification) -> String {
    if classification.signals.is_empty() {
        String::from("none")
    } else {
        classification.signals.join(",")
    }
}

fn render_missing_bindings(bindings: &KeybindingsInspection) -> String {
    if bindings.missing_bindings.is_empty() {
        String::from("none")
    } else {
        bindings.missing_bindings.join(" | ")
    }
}

fn is_pane_command_claude(pane: &TmuxPane) -> bool {
    pane.current_command.eq_ignore_ascii_case("claude")
}

fn is_default_list_pane(pane: &TmuxPane) -> bool {
    is_pane_command_claude(pane) || is_botctl_mcp_managed_pane(pane)
}

fn is_botctl_mcp_managed_pane(pane: &TmuxPane) -> bool {
    pane.session_name == "botctl-mcp" && pane.window_name.starts_with("botctl-mcp-")
}

fn is_pane_likely_codex(pane: &TmuxPane) -> bool {
    pane.current_command.eq_ignore_ascii_case("codex")
}

fn is_pane_command_opencode(pane: &TmuxPane) -> bool {
    pane.current_command.eq_ignore_ascii_case("opencode")
}

fn pane_provider_label(pane: &TmuxPane) -> &'static str {
    if is_pane_command_claude(pane) {
        "Claude"
    } else if is_pane_command_opencode(pane) {
        "OpenCode"
    } else if is_pane_command_pi(pane) {
        "Pi"
    } else if is_pane_command_grok(pane) {
        "Grok"
    } else if pane.window_name.starts_with("botctl-mcp-codex-") {
        "Codex"
    } else if pane.window_name.starts_with("botctl-mcp-agy-") {
        "Antigravity"
    } else if is_pane_likely_codex(pane) {
        "Codex"
    } else if is_pane_command_agy(pane) {
        "Antigravity"
    } else {
        "Unknown"
    }
}

fn is_pane_command_agy(pane: &TmuxPane) -> bool {
    pane.current_command.eq_ignore_ascii_case("agy")
}

fn is_pane_command_pi(pane: &TmuxPane) -> bool {
    pane.current_command.eq_ignore_ascii_case("pi")
}

fn is_pane_command_grok(pane: &TmuxPane) -> bool {
    pane.current_command.eq_ignore_ascii_case("grok")
}

fn is_classified_codex(classification: &Classification, pane: &TmuxPane) -> bool {
    if is_pane_command_opencode(pane) {
        return false;
    }
    is_pane_likely_codex(pane)
        || classification
            .signals
            .iter()
            .any(|signal| signal == SIGNAL_CODEX_KEYWORDS)
}

/// True when this pane should be treated as an Antigravity (agy) pane for
/// diagnostics and labeling. Covers both the process-name short-circuit
/// (`current_command == "agy"`) and the signal-based classification path
/// (the classifier set `SIGNAL_AGY_KEYWORDS` because the captured frame
/// fingerprinted as agy). Keeping these in one predicate makes `status` and
/// `doctor` stay internally consistent for signal-discovered panes.
fn is_classified_agy(classification: &Classification, pane: &TmuxPane) -> bool {
    is_pane_command_agy(pane)
        || classification
            .signals
            .iter()
            .any(|signal| signal == crate::classifier::SIGNAL_AGY_KEYWORDS)
}

fn is_classified_grok(classification: &Classification, pane: &TmuxPane) -> bool {
    is_pane_command_grok(pane)
        || classification
            .signals
            .iter()
            .any(|signal| signal == crate::classifier::SIGNAL_GROK_KEYWORDS)
}

fn classification_provider_label(classification: &Classification, pane: &TmuxPane) -> &'static str {
    if is_pane_command_claude(pane) {
        "Claude"
    } else if is_pane_command_opencode(pane) {
        "OpenCode"
    } else if is_pane_command_pi(pane) {
        "Pi"
    } else if is_classified_grok(classification, pane) {
        "Grok"
    } else if is_classified_codex(classification, pane) {
        "Codex"
    } else if is_classified_agy(classification, pane) {
        "Antigravity"
    } else {
        "Unknown"
    }
}

pub(crate) fn ensure_pane_owned_by_automatable_provider(
    pane: &TmuxPane,
    classification: &Classification,
) -> AppResult<()> {
    if is_pane_command_claude(pane) || is_classified_codex(classification, pane) {
        Ok(())
    } else {
        Err(AppError::new(format!(
            "refusing to drive pane {} because current command is {} instead of claude or codex; agy panes only support opt-in YOLO command-permission auto-approve, not direct keybinding automation",
            pane.pane_id, pane.current_command
        )))
    }
}

pub(crate) fn ensure_pane_owned_by_yolo_provider(
    pane: &TmuxPane,
    classification: &Classification,
) -> AppResult<()> {
    if is_pane_command_claude(pane)
        || is_classified_codex(classification, pane)
        || is_classified_agy(classification, pane)
    {
        Ok(())
    } else {
        Err(AppError::new(format!(
            "refusing to start yolo for pane {} because current command is {} and the screen is not classified as claude, codex, or agy",
            pane.pane_id, pane.current_command
        )))
    }
}

pub(crate) fn ensure_pane_owned_by_claude(pane: &TmuxPane) -> AppResult<()> {
    if is_pane_command_claude(pane) {
        Ok(())
    } else {
        Err(AppError::new(format!(
            "refusing to drive pane {} because current command is {} instead of claude",
            pane.pane_id, pane.current_command
        )))
    }
}

pub(crate) fn execute_automation_action(
    client: &TmuxClient,
    pane_id: &str,
    action: AutomationAction,
) -> AppResult<String> {
    let bindings = load_automation_keybindings(None)?;
    let keys = keys_for_action(&bindings, action)?;
    client.send_keys(pane_id, keys)?;
    Ok(action.as_str().to_string())
}

/// Per-shape operator-facing recommendation copy emitted by `doctor` for
/// agy panes. Mirrors (but does not enforce) the runtime YOLO policy:
/// only the command-permission shape is auto-approvable; the other two
/// require manual review.
fn agy_doctor_recommendation_line(state: SessionState) -> &'static str {
    match state {
        SessionState::AgyCommandPermissionPrompt => {
            "recommendation=agy command-permission prompt; opt in with `botctl yolo start --pane <agy-pane>` to auto-approve the default `1. Yes` option"
        }
        SessionState::AgyFolderTrustPrompt => {
            "recommendation=agy folder-trust prompt; confirm the workspace manually (folder-trust auto-approve is not enabled in v1)"
        }
        SessionState::AgySettingsPersistPrompt => {
            "recommendation=agy settings-persist prompt; review and confirm manually (settings-persist auto-approve is not enabled in v1)"
        }
        _ => {
            "recommendation=agy pane detected; dashboard and last-message extraction are supported; opt in to YOLO command-permission auto-approve with `botctl yolo start --pane <agy-pane>`"
        }
    }
}

fn render_doctor_recommendations(
    classification: &Classification,
    pane: &TmuxPane,
    bindings: &KeybindingsInspection,
) -> String {
    let mut lines = Vec::new();
    let agy = is_classified_agy(classification, pane);

    if !is_pane_command_claude(pane) {
        if agy {
            lines.push(String::from(agy_doctor_recommendation_line(
                classification.state,
            )));
        } else {
            lines.push(format!(
                "recommendation=target pane is running {} instead of claude",
                pane.current_command
            ));
        }
    }

    if !agy && bindings.status != KeybindingsStatus::Valid {
        lines.push(String::from(
            "recommendation=add the missing automation actions to your Claude keybindings; use install-bindings to merge missing botctl bindings into your existing file or bindings to inspect the recommended mapping",
        ));
    }

    if lines.is_empty() {
        String::from("\nrecommendation=none")
    } else {
        format!("\n{}", lines.join("\n"))
    }
}

fn load_frame_text(path: &PathBuf) -> AppResult<String> {
    std::fs::read_to_string(path).map_err(AppError::from)
}

fn read_prompt_input(source: Option<&Path>, text: Option<&str>) -> AppResult<String> {
    match (source, text) {
        (Some(path), None) => resolve_prompt_text(PromptSource::File(path)),
        (None, Some(text)) => resolve_prompt_text(PromptSource::Text(text)),
        (Some(_), Some(_)) => Err(AppError::new(
            "prompt input must use either --source PATH or --text TEXT, not both",
        )),
        (None, None) => Err(AppError::new(
            "prompt input requires either --source PATH or --text TEXT",
        )),
    }
}

fn parse_expected_state_arg(
    raw_state: Option<&str>,
    fallback: SessionState,
) -> AppResult<SessionState> {
    match raw_state {
        Some(value) => SessionState::from_str(value)
            .ok_or_else(|| AppError::new(format!("unknown session state: {value}"))),
        None => Ok(fallback),
    }
}

#[cfg(any(test, rust_analyzer))]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::{
        AppError, BabysitFormat, BabysitOutputFormat, DASHBOARD_PERSISTENT_CHILD_ENV,
        DASHBOARD_TARGET_TMUX_SOCKET_ENV, DashboardArgs, InspectedPane,
        KEEP_GOING_CUSTOM_PROMPT_ANCHOR, KEEP_GOING_PROMPT_ANCHOR, KeepGoingDirective,
        ObserveArtifactPaths, RecoveryAction, YoloAction, agy_doctor_recommendation_line,
        artifact_state_dir_result, classification_provider_label, cleanup_babysit_record,
        compose_prompt_run_content, ensure_pane_owned_by_automatable_provider,
        ensure_pane_owned_by_claude, ensure_pane_owned_by_yolo_provider, ensure_state_transition,
        extract_keep_going_response, extract_permission_prompt_details, focused_frame_source,
        is_agy_command_permission_safe_to_approve, is_prompt_completion_state,
        is_startup_enter_prompt, is_usable_state, is_yolo_safe_to_approve,
        keep_going_no_yolo_blocker, pane_provider_label, parse_env_value_from_environ_bytes,
        permission_manual_review_reason, persistent_dashboard_child_command_with_target_socket,
        prompt_launch_command, prompt_submission_started, raw_key_for_workflow,
        recovery_action_for_state, render_babysit_action_event, render_babysit_output,
        render_babysit_start_event, render_babysit_wait_event, render_doctor_recommendations,
        render_guarded_workflow_output, render_keep_going_wait_message, render_list_panes,
        render_next_safe_action, render_observe_command_output, render_screen_excerpt,
        render_status_json, render_status_report, resolve_keep_going_prompt,
        resolve_prompt_run_input, run_prepare_prompt, shell_escape,
        strip_dashboard_window_prefixes, submit_prompt_preflight_workflow,
        tmux_socket_path_from_value, write_prompt_instruction_temp_file, yolo_action_for_state,
    };
    use crate::automation::{GuardedWorkflow, KeybindingsInspection, KeybindingsStatus};
    use crate::classifier::{
        Classification, SIGNAL_CODEX_KEYWORDS, SIGNAL_SELF_SETTINGS_LANGUAGE,
        SIGNAL_SENSITIVE_CLAUDE_PATH, SessionState,
    };
    use crate::cli::{PreparePromptArgs, PromptRunArgs};
    use crate::prompt::pending_prompt_text;
    use crate::storage::{CURRENT_SCHEMA_VERSION, resolve_workspace_for_path, state_db_path};
    use crate::tmux::TmuxPane;
    use crate::yolo::{YoloRecord, read_yolo_record, write_yolo_record};

    fn sample_pane(current_command: &str) -> TmuxPane {
        TmuxPane {
            pane_id: String::from("%1"),
            pane_tty: String::from("/dev/pts/1"),
            pane_pid: Some(123),
            session_id: String::from("$1"),
            session_name: String::from("demo"),
            window_id: String::from("@2"),
            window_index: 1,
            window_name: String::from("claude"),
            pane_index: 0,
            current_command: current_command.to_string(),
            current_path: String::from("/tmp/demo"),
            pane_title: String::new(),
            pane_active: true,
            cursor_x: Some(0),
            cursor_y: Some(0),
        }
    }

    #[test]
    fn codex_new_model_chooser_is_not_auto_entered() {
        let frame = "Introducing GPT-5.5\nChoose how you'd like Codex to proceed.\n› 1. Try new model\n  2. Use existing model\nUse ↑/↓ to move, press enter to confirm";
        assert!(!is_startup_enter_prompt(frame));
    }

    fn sample_bindings(status: KeybindingsStatus) -> KeybindingsInspection {
        KeybindingsInspection {
            path: std::path::PathBuf::from("/tmp/keybindings.json"),
            status,
            missing_bindings: vec![],
        }
    }

    fn prompt_args_for_test() -> PromptRunArgs {
        PromptRunArgs {
            session_name: None,
            window_name: String::from("claude"),
            cwd: None,
            command: String::from("claude"),
            sources: Vec::new(),
            text: Some(String::from("hello")),
            stdin: false,
            append_system_prompts: Vec::new(),
            poll_ms: 1000,
            submit_delay_ms: 250,
            ready_timeout_ms: 30_000,
            idle_timeout_ms: 600_000,
            state_dir: None,
            workspace: None,
            large_prompt_threshold: 8192,
            keep_temp: false,
            no_yolo: false,
            verbose: false,
            claude_args: Vec::new(),
        }
    }

    #[test]
    fn prompt_run_combines_system_and_user_sources() {
        let root = unique_temp_dir("prompt-run-combine");
        fs::create_dir_all(&root).expect("temp dir should exist");
        let rules = root.join("rules.md");
        let source = root.join("source.md");
        fs::write(&rules, "be brief\n").expect("rules should write");
        fs::write(&source, "from file\n").expect("source should write");
        let mut args = prompt_args_for_test();
        args.append_system_prompts = vec![rules.clone()];
        args.sources = vec![source.clone()];
        args.text = Some(String::from("inline"));

        let content = compose_prompt_run_content(&args).expect("content should compose");

        assert!(content.contains(&format!("--- system prompt: {} ---", rules.display())));
        assert!(content.contains("be brief"));
        assert!(content.contains(&format!(
            "--- user prompt: source: {} ---",
            source.display()
        )));
        assert!(content.contains("from file"));
        assert!(content.contains("--- user prompt: text ---\ninline"));
    }

    #[test]
    fn prompt_run_rejects_empty_user_prompt() {
        let mut args = prompt_args_for_test();
        args.text = Some(String::from("  \n\t"));

        let error = compose_prompt_run_content(&args).expect_err("empty input should fail");

        assert_eq!(error.exit_code(), 2);
        assert!(error.to_string().contains("prompt input must not be empty"));
    }

    #[test]
    fn prompt_completion_treats_assistant_question_as_final_response() {
        assert!(is_prompt_completion_state(SessionState::ChatReady));
        assert!(is_prompt_completion_state(SessionState::UserQuestionPrompt));
        assert!(!is_prompt_completion_state(SessionState::PermissionDialog));
        assert!(!is_prompt_completion_state(SessionState::BusyResponding));
    }

    #[test]
    fn prompt_launch_command_appends_escaped_claude_args() {
        let mut args = prompt_args_for_test();
        args.claude_args = vec![
            String::from("--model"),
            String::from("sonnet"),
            String::from("--name"),
            String::from("Just testing"),
        ];

        let command = prompt_launch_command(&args, Path::new("/tmp/botctl-state"), "demo")
            .expect("launch command should render");

        assert!(command.ends_with("claude --model sonnet --name 'Just testing'"));
    }

    #[test]
    fn prompt_run_uses_temp_file_above_threshold() {
        let root = unique_temp_dir("prompt-run-temp");
        let state_dir = root.join("state");
        fs::create_dir_all(&state_dir).expect("state dir should exist");
        let mut args = prompt_args_for_test();
        args.text = Some(String::from("this prompt is intentionally long"));
        args.large_prompt_threshold = 4;

        let resolved = resolve_prompt_run_input(&args, &state_dir, "demo/session")
            .expect("large prompt should resolve through temp file");
        let temp_file = resolved.temp_file.expect("temp file should exist");

        assert!(temp_file.starts_with(state_dir.join("prompt-runs")));
        assert!(temp_file.ends_with("instructions.md"));
        assert!(
            resolved
                .submitted_text
                .contains(&temp_file.display().to_string())
        );
        assert!(!resolved.submitted_text.contains("intentionally long"));
        assert!(
            fs::read_to_string(temp_file)
                .expect("temp should read")
                .contains("intentionally long")
        );
    }

    #[test]
    fn prompt_instruction_temp_file_sanitizes_session_path() {
        let root = unique_temp_dir("prompt-run-temp-path");
        fs::create_dir_all(&root).expect("temp dir should exist");
        let path = write_prompt_instruction_temp_file(&root, "demo/session:%1", "hello")
            .expect("temp instruction should write");

        assert!(path.starts_with(root.join("prompt-runs")));
        assert!(!path.display().to_string().contains("demo/session"));
        assert!(
            fs::read_to_string(path)
                .expect("temp should read")
                .contains("hello")
        );
    }

    #[test]
    fn accepts_claude_owned_pane() {
        ensure_pane_owned_by_claude(&sample_pane("claude"))
            .expect("claude pane should pass ownership check");
    }

    #[test]
    fn rejects_non_claude_owned_pane() {
        let error = ensure_pane_owned_by_claude(&sample_pane("bash"))
            .expect_err("non-claude pane should fail ownership check");

        assert!(error.to_string().contains("instead of claude"));
    }

    #[test]
    fn auto_unstick_treats_busy_and_ready_as_usable() {
        assert!(is_usable_state(SessionState::ChatReady));
        assert!(is_usable_state(SessionState::BusyResponding));
        assert!(!is_usable_state(SessionState::PermissionDialog));
    }

    #[test]
    fn chooses_recovery_actions_for_known_blockers() {
        assert_eq!(
            recovery_action_for_state(SessionState::PermissionDialog)
                .expect("permission dialog should map")
                .expect("permission dialog should need recovery"),
            RecoveryAction::ApprovePermission
        );
        assert_eq!(
            recovery_action_for_state(SessionState::SurveyPrompt)
                .expect("survey should map")
                .expect("survey should need recovery"),
            RecoveryAction::DismissSurvey
        );
        assert_eq!(
            recovery_action_for_state(SessionState::FolderTrustPrompt)
                .expect("folder trust should map")
                .expect("folder trust should need recovery"),
            RecoveryAction::ConfirmFolderTrust
        );

        let error = recovery_action_for_state(SessionState::PlanApprovalPrompt)
            .expect_err("plan approval prompt should require manual review");
        assert!(error.to_string().contains("PlanApprovalPrompt"));
    }

    #[test]
    fn refuses_recovery_for_ambiguous_states() {
        let error = recovery_action_for_state(SessionState::Unknown)
            .expect_err("unknown state should fail recovery planning");
        assert!(error.to_string().contains("refusing to guess"));
    }

    #[test]
    fn refuses_retry_when_state_does_not_change() {
        let before = Classification {
            source: String::from("pane"),
            state: SessionState::PermissionDialog,
            has_questions: false,
            recap_present: false,
            recap_excerpt: None,
            signals: vec![String::from("permission-keywords")],
        };
        let after = before.clone();

        let error = ensure_state_transition(&before, &after, "approve")
            .expect_err("unchanged state should fail");
        assert!(error.to_string().contains("potentially stale screen data"));
    }

    #[test]
    fn renders_a_focused_screen_excerpt() {
        let excerpt = render_screen_excerpt("\n\nfirst\n\nsecond\nthird\n");
        assert_eq!(excerpt, "first | second | third");
    }

    #[test]
    fn focused_frame_source_keeps_question_block_above_long_footer() {
        let frame = [
            "I'd suggest Option A unless you already have a NuxtHub project.",
            "---",
            "Want me to:",
            "1. Update .env.example with the new STUDIO_GITLAB_* vars (and a comment about the moderator allow-list)?",
            "2. Flip media.external to false (Option A)?",
            "3. Add a short \"Studio editing\" section to README.md?",
            "Or all three? Once you create the OAuth app and paste me the Application ID, I can't do that part for you — but I can stage everything else.",
            "✻ Cogitated for 2m 12s",
            "────────────────────────────────────────────────────────────────────────────────",
            "❯",
            "────────────────────────────────────────────────────────────────────────────────",
            "~/Projects/shipstream/knowledge-base/node_modules/.pnpm/nuxt-studio@1.4.0",
            "🧠 Opus 4.7 (1M context) │ Ctx: 6% │ Commits:6",
            "MCP:0/0 │ 🔥$13.85/hr │ Cache: 98% hit │ Est: $69.02 (209.6K)",
        ]
        .join("\n");

        let focused = focused_frame_source(&frame);

        assert!(focused.contains("Want me to:"));
        assert!(focused.contains("1. Update .env.example"));
    }

    #[test]
    fn focused_frame_source_prefers_active_permission_prompt_over_stale_chat_prompt() {
        let frame = [
            "Older chat context",
            "❯",
            "More stale chat output",
            "Bash command (unsandboxed)",
            "Probe opencode to diagnose empty output",
            "Do you want to proceed?",
            "❯ 1. Yes",
            "2. Yes, and don't ask again for: opencode run *",
            "3. No",
            "Esc to cancel · Tab to amend · ctrl+e to explain",
        ]
        .join("\n");

        let focused = focused_frame_source(&frame);

        assert!(focused.contains("Bash command (unsandboxed)"));
        assert!(focused.contains("Do you want to proceed?"));
        assert!(focused.contains("❯ 1. Yes"));
    }

    #[test]
    fn optional_artifact_state_dir_turns_default_errors_into_warnings() {
        let (state_dir, warning) =
            artifact_state_dir_result(Err(AppError::new("disk full")), false)
                .expect("default artifact state dir should degrade to warning");

        assert_eq!(state_dir, None);
        assert_eq!(warning.as_deref(), Some("disk full"));
    }

    #[test]
    fn optional_artifact_state_dir_preserves_explicit_errors() {
        let error = artifact_state_dir_result(Err(AppError::new("disk full")), true)
            .expect_err("explicit artifact state dir should stay fatal");

        assert_eq!(error.to_string(), "disk full");
    }

    #[test]
    fn observe_output_reports_artifact_warning_without_paths() {
        let rendered = render_observe_command_output("report body", None, Some("disk full"));
        assert_eq!(
            rendered,
            "report body\nartifacts:\n  unavailable=disk full\n"
        );
    }

    #[test]
    fn observe_output_reports_artifact_paths_when_available() {
        let rendered = render_observe_command_output(
            "report body",
            Some(&ObserveArtifactPaths {
                capture_file: std::path::PathBuf::from("/tmp/capture.txt"),
                tape_file: std::path::PathBuf::from("/tmp/control-mode.log"),
                export_file: std::path::PathBuf::from("/tmp/report.json"),
            }),
            Some("ignored warning"),
        );

        assert!(rendered.contains("capture_file=/tmp/capture.txt"));
        assert!(rendered.contains("control_log=/tmp/control-mode.log"));
        assert!(rendered.contains("export=/tmp/report.json"));
        assert!(!rendered.contains("unavailable="));
    }

    #[test]
    fn pane_removed_human_output_uses_note_label() {
        let rendered = render_babysit_output(
            BabysitOutputFormat::Human,
            serde_json::json!({
                "kind": "pane-removed",
                "summary": "Observed pane %7 disappeared",
                "details": ["Session: demo"]
            }),
            false,
        );

        assert!(rendered.contains("NOTE"));
        assert!(!rendered.contains("STOP"));
    }

    #[test]
    fn maps_state_to_safe_next_action_or_review() {
        let classification = Classification {
            source: String::from("pane"),
            state: SessionState::PermissionDialog,
            has_questions: false,
            recap_present: false,
            recap_excerpt: None,
            signals: vec![],
        };
        assert_eq!(
            render_next_safe_action(
                &classification,
                &sample_pane("claude"),
                &sample_bindings(KeybindingsStatus::Valid)
            ),
            "safe-action: approve"
        );
        assert!(
            render_next_safe_action(
                &Classification {
                    state: SessionState::Unknown,
                    has_questions: false,
                    recap_present: false,
                    recap_excerpt: None,
                    ..classification.clone()
                },
                &sample_pane("claude"),
                &sample_bindings(KeybindingsStatus::Valid)
            )
            .starts_with("manual-review:")
        );
        assert_eq!(
            render_next_safe_action(
                &Classification {
                    state: SessionState::SurveyPrompt,
                    has_questions: false,
                    recap_present: false,
                    recap_excerpt: None,
                    ..classification.clone()
                },
                &sample_pane("claude"),
                &sample_bindings(KeybindingsStatus::Valid)
            ),
            "safe-action: dismiss-survey (0)"
        );
        assert_eq!(
            render_next_safe_action(
                &Classification {
                    state: SessionState::PlanApprovalPrompt,
                    has_questions: false,
                    recap_present: false,
                    recap_excerpt: None,
                    ..classification.clone()
                },
                &sample_pane("claude"),
                &sample_bindings(KeybindingsStatus::Valid)
            ),
            "manual-review: plan approval prompt needs operator confirmation"
        );
        assert_eq!(
            render_next_safe_action(
                &Classification {
                    state: SessionState::PermissionDialog,
                    has_questions: false,
                    recap_present: false,
                    recap_excerpt: None,
                    signals: vec![String::from(SIGNAL_CODEX_KEYWORDS)],
                    ..classification
                },
                &sample_pane("node"),
                &sample_bindings(KeybindingsStatus::Valid)
            ),
            "safe-action: approve (y)"
        );
    }

    #[test]
    fn render_next_safe_action_branches_per_agy_shape() {
        let bindings = sample_bindings(KeybindingsStatus::Valid);
        let pane = sample_pane("agy");

        // Command-permission: the only auto-approvable agy shape.
        let mut classification =
            sample_agy_classification(SessionState::AgyCommandPermissionPrompt);
        assert_eq!(
            render_next_safe_action(&classification, &pane, &bindings),
            "safe-action: approve (Enter)"
        );

        // Folder-trust: manual review.
        classification.state = SessionState::AgyFolderTrustPrompt;
        assert_eq!(
            render_next_safe_action(&classification, &pane, &bindings),
            "manual-review: agy folder-trust prompt"
        );

        // Settings-persist: manual review.
        classification.state = SessionState::AgySettingsPersistPrompt;
        assert_eq!(
            render_next_safe_action(&classification, &pane, &bindings),
            "manual-review: agy settings-persist prompt"
        );

        // Generic agy state (ChatReady/BusyResponding/Unknown) falls through
        // to the generic copy.
        classification.state = SessionState::ChatReady;
        assert_eq!(
            render_next_safe_action(&classification, &pane, &bindings),
            "manual-review: agy automation is not implemented"
        );
    }

    #[test]
    fn opencode_panes_do_not_become_codex_from_screen_signals() {
        let classification = Classification {
            source: String::from("pane"),
            state: SessionState::PermissionDialog,
            has_questions: false,
            recap_present: false,
            recap_excerpt: None,
            signals: vec![String::from(SIGNAL_CODEX_KEYWORDS)],
        };
        let mut pane = sample_pane("opencode");
        pane.pane_title = String::from("OC | Remove glob dependency from Cypress tests");

        assert_eq!(
            classification_provider_label(&classification, &pane),
            "OpenCode"
        );
        assert_eq!(pane_provider_label(&pane), "OpenCode");
        assert_eq!(
            render_next_safe_action(
                &classification,
                &pane,
                &sample_bindings(KeybindingsStatus::Valid)
            ),
            "manual-review: pane is running opencode"
        );
        assert!(ensure_pane_owned_by_automatable_provider(&pane, &classification).is_err());
        assert!(ensure_pane_owned_by_yolo_provider(&pane, &classification).is_err());
    }

    #[test]
    fn uses_raw_keys_for_special_case_workflows() {
        let folder_trust = Classification {
            source: String::from("pane"),
            state: SessionState::FolderTrustPrompt,
            has_questions: false,
            recap_present: false,
            recap_excerpt: None,
            signals: vec![String::from("folder-trust-keywords")],
        };
        let survey = Classification {
            source: String::from("pane"),
            state: SessionState::SurveyPrompt,
            has_questions: false,
            recap_present: false,
            recap_excerpt: None,
            signals: vec![String::from("survey-keywords")],
        };
        let codex_permission = Classification {
            source: String::from("pane"),
            state: SessionState::PermissionDialog,
            has_questions: false,
            recap_present: false,
            recap_excerpt: None,
            signals: vec![String::from(SIGNAL_CODEX_KEYWORDS)],
        };

        assert_eq!(
            raw_key_for_workflow(GuardedWorkflow::ApprovePermission, &folder_trust),
            Some(("Enter", "enter"))
        );
        assert_eq!(
            raw_key_for_workflow(GuardedWorkflow::ApprovePermission, &codex_permission),
            Some(("y", "y"))
        );
        assert_eq!(
            raw_key_for_workflow(GuardedWorkflow::DismissSurvey, &survey),
            Some(("0", "0"))
        );
    }

    #[test]
    fn marks_self_settings_permission_prompt_for_manual_review() {
        let classification = Classification {
            source: String::from("pane"),
            state: SessionState::PermissionDialog,
            has_questions: false,
            recap_present: false,
            recap_excerpt: None,
            signals: vec![String::from(SIGNAL_SELF_SETTINGS_LANGUAGE)],
        };

        assert_eq!(
            render_next_safe_action(
                &classification,
                &sample_pane("claude"),
                &sample_bindings(KeybindingsStatus::Valid)
            ),
            "manual-review: Claude wants to edit its own settings"
        );
    }

    #[test]
    fn marks_sensitive_claude_path_permission_prompt_for_manual_review() {
        let classification = Classification {
            source: String::from("pane"),
            state: SessionState::PermissionDialog,
            has_questions: false,
            recap_present: false,
            recap_excerpt: None,
            signals: vec![String::from(SIGNAL_SENSITIVE_CLAUDE_PATH)],
        };

        assert_eq!(
            render_next_safe_action(
                &classification,
                &sample_pane("claude"),
                &sample_bindings(KeybindingsStatus::Valid)
            ),
            "manual-review: prompt targets Claude settings or slash-command paths"
        );
    }

    #[test]
    fn reports_sensitive_permission_reason_for_auto_recovery_guard() {
        let classification = Classification {
            source: String::from("pane"),
            state: SessionState::PermissionDialog,
            has_questions: false,
            recap_present: false,
            recap_excerpt: None,
            signals: vec![String::from(SIGNAL_SELF_SETTINGS_LANGUAGE)],
        };

        assert_eq!(
            permission_manual_review_reason(&classification),
            Some("Claude wants to edit its own settings")
        );
    }

    #[test]
    fn guarded_workflow_human_output_mentions_pane_state_and_raw_enter() {
        let output = render_guarded_workflow_output(
            GuardedWorkflow::ApprovePermission,
            "bloodraven",
            "%20",
            &Classification {
                source: String::from("pane"),
                state: SessionState::FolderTrustPrompt,
                has_questions: false,
                recap_present: false,
                recap_excerpt: None,
                signals: vec![String::from("folder-trust")],
            },
            "enter",
            true,
            BabysitFormat::Human,
            false,
        );

        assert!(output.contains("bloodraven"));
        assert!(output.contains("%20"));
        assert!(output.contains("Approval sent"));
        assert!(output.contains("State: FolderTrustPrompt"));
        assert!(output.contains("Folder trust raw Enter: true"));
        assert!(!output.contains("key=value"));
    }

    #[test]
    fn guarded_workflow_jsonl_is_structured() {
        let output = render_guarded_workflow_output(
            GuardedWorkflow::RejectPermission,
            "bloodraven",
            "%20",
            &Classification {
                source: String::from("pane"),
                state: SessionState::PermissionDialog,
                has_questions: false,
                recap_present: false,
                recap_excerpt: None,
                signals: vec![String::from("permission-keywords")],
            },
            "escape",
            false,
            BabysitFormat::Jsonl,
            false,
        );

        let parsed: serde_json::Value = serde_json::from_str(&output).expect("jsonl should parse");
        assert_eq!(parsed["workflow"], "reject");
        assert_eq!(parsed["action"], "reject");
        assert_eq!(parsed["pane_label"], "bloodraven");
        assert_eq!(parsed["pane_id"], "%20");
        assert_eq!(parsed["state"], "PermissionDialog");
        assert_eq!(parsed["used_raw_enter"], false);
    }

    #[test]
    fn status_report_includes_recap_metadata() {
        let report = render_status_report(
            &sample_pane("claude"),
            &Classification {
                source: String::from("pane"),
                state: SessionState::ChatReady,
                has_questions: false,
                recap_present: true,
                recap_excerpt: Some(String::from("Fixed parser edge cases")),
                signals: vec![String::from("chat-keywords")],
            },
            &sample_bindings(KeybindingsStatus::Valid),
            "While you were away\nFixed parser edge cases\nEnter submit message",
        );

        assert!(report.contains("recap_present=true"));
        assert!(report.contains("recap_excerpt=Fixed parser edge cases"));
    }

    #[test]
    fn strip_dashboard_window_prefixes_removes_stale_state_emoji_prefixes() {
        assert_eq!(strip_dashboard_window_prefixes("🤔 wms"), "wms");
        assert_eq!(strip_dashboard_window_prefixes("🤔 💤 wms"), "wms");
        assert_eq!(strip_dashboard_window_prefixes("wms"), "wms");
    }

    #[test]
    fn list_panes_defaults_to_claude_and_managed_mcp_output() {
        let output = render_list_panes(&[sample_pane("claude"), sample_pane("bash")], false);
        assert!(output.contains("command=claude"));
        assert!(output.contains("pid=123"));
        assert!(!output.contains("command=bash"));
    }

    #[test]
    fn list_panes_default_includes_managed_mcp_node_panes() {
        let mut pane = sample_pane("node");
        pane.session_name = String::from("botctl-mcp");
        pane.window_name = String::from("botctl-mcp-codex-12345678");

        let output = render_list_panes(&[pane], false);

        assert!(output.contains("command=node"));
        assert!(output.contains("provider=Codex"));
    }

    #[test]
    fn list_panes_all_flag_includes_non_claude_output() {
        let output = render_list_panes(&[sample_pane("claude"), sample_pane("bash")], true);
        assert!(output.contains("command=claude"));
        assert!(output.contains("command=bash"));
    }

    #[test]
    fn list_panes_default_reports_empty_state() {
        let output = render_list_panes(&[sample_pane("bash")], false);
        assert_eq!(output, "no managed agent panes found");
    }

    #[test]
    fn permission_babysit_only_approves_permission_dialogs() {
        assert_eq!(
            yolo_action_for_state(SessionState::PermissionDialog),
            YoloAction::ApprovePermission
        );
        assert_eq!(
            yolo_action_for_state(SessionState::SurveyPrompt),
            YoloAction::DismissSurvey
        );
        assert_eq!(
            yolo_action_for_state(SessionState::ChatReady),
            YoloAction::Wait
        );
        assert_eq!(
            yolo_action_for_state(SessionState::PlanApprovalPrompt),
            YoloAction::Wait
        );
        assert_eq!(
            yolo_action_for_state(SessionState::BusyResponding),
            YoloAction::Wait
        );
        assert_eq!(
            yolo_action_for_state(SessionState::FolderTrustPrompt),
            YoloAction::Wait
        );
        assert_eq!(
            yolo_action_for_state(SessionState::Unknown),
            YoloAction::Wait
        );
    }

    #[test]
    fn permission_babysit_keeps_waiting_on_non_permission_blockers() {
        for state in [
            SessionState::PlanApprovalPrompt,
            SessionState::FolderTrustPrompt,
            SessionState::ExternalEditorActive,
            SessionState::DiffDialog,
            SessionState::AgyFolderTrustPrompt,
            SessionState::AgySettingsPersistPrompt,
            SessionState::Unknown,
        ] {
            assert_eq!(yolo_action_for_state(state), YoloAction::Wait);
        }
    }

    #[test]
    fn yolo_action_for_state_maps_agy_command_permission_to_approve() {
        assert_eq!(
            yolo_action_for_state(SessionState::AgyCommandPermissionPrompt),
            YoloAction::ApproveAgyCommandPermission
        );
    }

    #[test]
    fn submit_prompt_preflight_dismisses_surveys_only() {
        let survey = Classification {
            source: String::from("pane"),
            state: SessionState::SurveyPrompt,
            has_questions: false,
            recap_present: false,
            recap_excerpt: None,
            signals: vec![String::from("survey-keywords")],
        };
        let ready = Classification {
            source: String::from("pane"),
            state: SessionState::ChatReady,
            has_questions: false,
            recap_present: false,
            recap_excerpt: None,
            signals: vec![String::from("chat-keywords")],
        };

        assert_eq!(
            submit_prompt_preflight_workflow(&survey),
            Some(GuardedWorkflow::DismissSurvey)
        );
        assert_eq!(submit_prompt_preflight_workflow(&ready), None);
    }

    #[test]
    fn extracts_keep_going_all_done_response() {
        let response = extract_keep_going_response(
            "Audit the task currently in scope — only what the user explicitly asked for.\nCompleted the requested parser change and the tests are passing.\nALL_DONE\nEnter to submit message\n❯",
            Some(KEEP_GOING_PROMPT_ANCHOR),
        )
        .expect("keep-going response should parse");

        assert_eq!(response.directive, KeepGoingDirective::AllDone);
        assert_eq!(
            response.body,
            "Completed the requested parser change and the tests are passing."
        );
    }

    #[test]
    fn extracts_latest_keep_going_response_before_chat_input() {
        let response = extract_keep_going_response(
            "Audit the task currently in scope — only what the user explicitly asked for.\nBlocked on repo credentials\nWhat I tried: git push\nPANIC\nEnter to submit message\n❯",
            Some(KEEP_GOING_PROMPT_ANCHOR),
        )
        .expect("latest keep-going response should parse");

        assert_eq!(response.directive, KeepGoingDirective::Panic);
        assert_eq!(
            response.body,
            "Blocked on repo credentials\nWhat I tried: git push"
        );
    }

    #[test]
    fn ignores_stale_keep_going_token_before_latest_prompt() {
        let response = extract_keep_going_response(
            "Audit the task currently in scope — old audit\nOld summary\nALL_DONE\nAudit the task currently in scope — new audit\nStill working\nEnter to submit message\n❯",
            Some(KEEP_GOING_PROMPT_ANCHOR),
        );

        assert!(response.is_none());
    }

    #[test]
    fn extracts_keep_going_terminal_token_without_visible_prompt_anchor() {
        let response = extract_keep_going_response(
            "long build output\n\nRan targeted checks\nPushed feature branch successfully\nALL_DONE\nEnter to submit message\n❯",
            Some(KEEP_GOING_PROMPT_ANCHOR),
        )
        .expect("terminal keep-going response should parse without anchor");

        assert_eq!(response.directive, KeepGoingDirective::AllDone);
        assert_eq!(
            response.body,
            "Ran targeted checks\nPushed feature branch successfully"
        );
    }

    #[test]
    fn ignores_non_literal_keep_going_tokens() {
        let response = extract_keep_going_response(
            "Audit the task currently in scope — only what the user explicitly asked for.\nRespond with EXACTLY one token\n- `ALL_DONE` — everything is complete\nEnter to submit message\n❯",
            Some(KEEP_GOING_PROMPT_ANCHOR),
        );

        assert!(response.is_none());
    }

    #[test]
    fn extracts_keep_going_response_for_custom_prompt_anchor() {
        let response = extract_keep_going_response(
            "Custom loop header\nCheck the branch\n\n(Do not repeat the marker line below in your reply.)\n[[BOTCTL_KEEP_GOING_END_PROMPT]]\nWork completed successfully\nALL_DONE\nEnter to submit message\n❯",
            Some(KEEP_GOING_CUSTOM_PROMPT_ANCHOR),
        )
        .expect("custom keep-going response should parse");

        assert_eq!(response.directive, KeepGoingDirective::AllDone);
        assert_eq!(response.body, "Work completed successfully");
    }

    #[test]
    fn ignores_stale_keep_going_token_before_latest_custom_prompt() {
        let response = extract_keep_going_response(
            "Custom loop header\n\n(Do not repeat the marker line below in your reply.)\n[[BOTCTL_KEEP_GOING_END_PROMPT]]\nOld result\nALL_DONE\nCustom loop header\n\n(Do not repeat the marker line below in your reply.)\n[[BOTCTL_KEEP_GOING_END_PROMPT]]\nStill working\nEnter to submit message\n❯",
            Some(KEEP_GOING_CUSTOM_PROMPT_ANCHOR),
        );

        assert!(response.is_none());
    }

    #[test]
    fn resolve_keep_going_prompt_defaults_to_builtin_audit_prompt() {
        let prompt = resolve_keep_going_prompt(None, None)
            .expect("default keep-going prompt should resolve");

        assert_eq!(prompt.anchor.as_deref(), Some(KEEP_GOING_PROMPT_ANCHOR));
        assert!(prompt.text.contains(KEEP_GOING_PROMPT_ANCHOR));
    }

    #[test]
    fn resolve_keep_going_prompt_accepts_custom_text() {
        let prompt = resolve_keep_going_prompt(None, Some("Custom loop header\nContinue"))
            .expect("custom keep-going prompt should resolve");

        assert_eq!(
            prompt.anchor.as_deref(),
            Some(KEEP_GOING_CUSTOM_PROMPT_ANCHOR)
        );
        assert!(prompt.text.starts_with("Custom loop header\nContinue"));
        assert!(
            prompt
                .text
                .contains("Do not repeat the marker line below in your reply.")
        );
        assert!(prompt.text.contains("Custom loop header\nContinue"));
        assert!(prompt.text.ends_with(KEEP_GOING_CUSTOM_PROMPT_ANCHOR));
    }

    #[test]
    fn resolve_keep_going_prompt_rejects_blank_custom_text() {
        let error = resolve_keep_going_prompt(None, Some("   \n\t  "))
            .expect_err("blank keep-going prompt should fail");

        assert_eq!(
            error.to_string(),
            "keep-going custom prompt must not be empty"
        );
    }

    #[test]
    fn prompt_submission_started_accepts_non_ready_transition() {
        let after = Classification {
            source: String::from("pane"),
            state: SessionState::BusyResponding,
            has_questions: false,
            recap_present: false,
            recap_excerpt: None,
            signals: vec![String::from("busy-keywords")],
        };

        assert!(prompt_submission_started("before", "after", &after, None));
    }

    #[test]
    fn prompt_submission_started_requires_prompt_anchor_when_still_ready() {
        let ready = Classification {
            source: String::from("pane"),
            state: SessionState::ChatReady,
            has_questions: false,
            recap_present: false,
            recap_excerpt: None,
            signals: vec![String::from("chat-keywords")],
        };

        assert!(prompt_submission_started(
            "old output",
            "Audit the task currently in scope — only what the user explicitly asked for.",
            &ready,
            Some(KEEP_GOING_PROMPT_ANCHOR),
        ));
        assert!(prompt_submission_started(
            "old output",
            "different output",
            &ready,
            Some(KEEP_GOING_PROMPT_ANCHOR),
        ));
        assert!(!prompt_submission_started(
            "same output",
            "same output",
            &ready,
            Some(KEEP_GOING_PROMPT_ANCHOR),
        ));
    }

    #[test]
    fn prompt_submission_started_accepts_generic_frame_change_without_anchor() {
        let ready = Classification {
            source: String::from("pane"),
            state: SessionState::ChatReady,
            has_questions: false,
            recap_present: false,
            recap_excerpt: None,
            signals: vec![String::from("chat-keywords")],
        };

        assert!(prompt_submission_started("before", "after", &ready, None));
    }

    #[test]
    fn keep_going_wait_message_shows_manual_reason_in_no_yolo_mode() {
        let log = render_keep_going_wait_message(
            "%9",
            &Classification {
                source: String::from("pane"),
                state: SessionState::PermissionDialog,
                has_questions: false,
                recap_present: false,
                recap_excerpt: None,
                signals: vec![String::from(SIGNAL_SELF_SETTINGS_LANGUAGE)],
            },
            false,
            true,
        );

        assert!(log.contains("yolo=off"));
        assert!(log.contains("Claude wants to edit its own settings"));
    }

    #[test]
    fn app_error_can_override_exit_code() {
        let error = AppError::with_exit_code("manual review required", 2);
        assert_eq!(error.exit_code(), 2);
        assert_eq!(error.to_string(), "manual review required");
    }

    #[test]
    fn prepare_prompt_bootstraps_state_db() {
        let state_dir = unique_temp_dir("app-prepare-prompt-state-db");
        let workspace_root = unique_temp_dir("app-prepare-prompt-workspace");
        let _ = fs::remove_dir_all(&state_dir);
        fs::create_dir_all(&workspace_root).expect("workspace should exist");
        let workspace = resolve_workspace_for_path(&state_dir, &workspace_root)
            .expect("workspace should resolve");

        run_prepare_prompt(PreparePromptArgs {
            session_name: String::from("demo/session"),
            state_dir: Some(state_dir.clone()),
            workspace: Some(workspace_root.display().to_string()),
            source: None,
            text: Some(String::from("hello world")),
        })
        .expect("prepare-prompt should succeed");

        assert_eq!(
            pending_prompt_text(&state_dir, &workspace.id, "demo/session")
                .expect("pending prompt should load"),
            Some(String::from("hello world"))
        );

        let connection = rusqlite::Connection::open(state_db_path(&state_dir))
            .expect("state db should exist after prepare-prompt");
        let version = connection
            .query_row(
                "SELECT version FROM schema_version WHERE id = 1",
                [],
                |row| row.get::<_, i64>(0),
            )
            .expect("schema version row should exist");
        let stored_prompt = connection
            .query_row(
                "SELECT pending_prompts.content \
                 FROM pending_prompts \
                 JOIN instances ON instances.id = pending_prompts.instance_id \
                 WHERE instances.workspace_id = ?1 AND instances.session_name = ?2",
                rusqlite::params![workspace.id, "demo/session"],
                |row| row.get::<_, String>(0),
            )
            .expect("pending prompt row should exist");
        assert_eq!(version, CURRENT_SCHEMA_VERSION);
        assert_eq!(stored_prompt, "hello world");

        let _ = fs::remove_dir_all(&state_dir);
    }

    #[test]
    fn keep_going_no_yolo_blocker_returns_exit_code_two() {
        let error = keep_going_no_yolo_blocker(
            &Classification {
                source: String::from("pane"),
                state: SessionState::PermissionDialog,
                has_questions: false,
                recap_present: false,
                recap_excerpt: None,
                signals: vec![],
            },
            "%9",
        )
        .expect("permission dialog should block no-yolo mode");

        assert_eq!(error.exit_code(), 2);
        assert!(error.to_string().contains("state PermissionDialog"));
    }

    #[test]
    fn yolo_wait_log_includes_state_signals_and_excerpt() {
        let inspected = InspectedPane {
            classification: Classification {
                source: String::from("pane"),
                state: SessionState::PermissionDialog,
                has_questions: false,
                recap_present: true,
                recap_excerpt: Some(String::from("allow access")),
                signals: vec![String::from("permission-keywords")],
            },
            focused_source: String::from(
                "Bash command (unsandboxed)\nDo you want to proceed?\n❯ 1. Yes",
            ),
            raw_source: String::from(
                "Bash command (unsandboxed)\nDo you want to proceed?\n❯ 1. Yes",
            ),
        };

        let log = render_babysit_wait_event(
            "bloodraven",
            "%20",
            3,
            &inspected,
            false,
            BabysitFormat::Human,
            false,
        );
        assert!(log.contains("Watching bloodraven (id:%20)"));
        assert!(!log.to_lowercase().contains("babysit"));
        assert!(log.contains("poll #3"));
        assert!(log.contains("Recap: present"));
        assert!(log.contains("Type: Bash command"));
        assert!(log.contains("Sandbox: unsandboxed"));
        assert!(!log.contains("Extracted prompt"));
    }

    #[test]
    fn extracts_permission_prompt_details_from_bash_prompt() {
        let inspected = InspectedPane {
            classification: Classification {
                source: String::from("pane"),
                state: SessionState::PermissionDialog,
                has_questions: false,
                recap_present: false,
                recap_excerpt: None,
                signals: vec![String::from("permission-keywords")],
            },
            focused_source: String::from(
                "Bash command (unsandboxed)\n  bun run scripts/test-prompt-safety.ts 2>&1\n  Run classifier test cases\nDo you want to proceed?\n❯ 1. Yes\n  2. No\nEsc to cancel · Tab to amend · ctrl+e to explain",
            ),
            raw_source: String::from(
                "Bash command (unsandboxed)\n  bun run scripts/test-prompt-safety.ts 2>&1\n  Run classifier test cases\nDo you want to proceed?\n❯ 1. Yes\n  2. No\nEsc to cancel · Tab to amend · ctrl+e to explain",
            ),
        };

        let details = extract_permission_prompt_details(&inspected).expect("details should parse");
        assert_eq!(details.prompt_type, "Bash command");
        assert_eq!(details.sandbox_mode.as_deref(), Some("unsandboxed"));
        assert_eq!(
            details.command.as_deref(),
            Some("bun run scripts/test-prompt-safety.ts 2>&1")
        );
        assert_eq!(details.reason.as_deref(), Some("Run classifier test cases"));
        assert_eq!(details.question.as_deref(), Some("Do you want to proceed?"));
    }

    #[test]
    fn extracts_permission_prompt_details_from_exact_unsandboxed_prompt() {
        let inspected = InspectedPane {
            classification: Classification {
                source: String::from("pane"),
                state: SessionState::PermissionDialog,
                has_questions: false,
                recap_present: false,
                recap_excerpt: None,
                signals: vec![String::from("permission-keywords")],
            },
            focused_source: String::from(
                "  Bash command (unsandboxed)\n    make generate 2>&1 | tail -40\n    Run make generate to regenerate deepcopy (no sandbox)\n  Do you want to proceed?\n",
            ),
            raw_source: String::from(
                "  Bash command (unsandboxed)\n    make generate 2>&1 | tail -40\n    Run make generate to regenerate deepcopy (no sandbox)\n  Do you want to proceed?\n",
            ),
        };

        let details = extract_permission_prompt_details(&inspected).expect("details should parse");
        assert_eq!(
            details.command.as_deref(),
            Some("make generate 2>&1 | tail -40")
        );
        assert_eq!(
            details.reason.as_deref(),
            Some("Run make generate to regenerate deepcopy (no sandbox)")
        );
        assert_eq!(details.question.as_deref(), Some("Do you want to proceed?"));
    }

    #[test]
    fn extracts_permission_prompt_details_when_wrapped_by_border_lines() {
        let inspected = InspectedPane {
            classification: Classification {
                source: String::from("pane"),
                state: SessionState::PermissionDialog,
                has_questions: false,
                recap_present: false,
                recap_excerpt: None,
                signals: vec![String::from("permission-keywords")],
            },
            focused_source: String::from(
                "[────────────────────────────────────────────────────────]\nJavaScript code (unsandboxed)\n\n  let pass = 0, fail = 0\n  for (const [pat, path, expected] of cases) {\nDo you want to proceed?\n❯ 1. Yes\n  2. No\nEsc to cancel · Tab to amend · ctrl+e to explain",
            ),
            raw_source: String::from(
                "[────────────────────────────────────────────────────────]\nJavaScript code (unsandboxed)\n\n  let pass = 0, fail = 0\n  for (const [pat, path, expected] of cases) {\nDo you want to proceed?\n❯ 1. Yes\n  2. No\nEsc to cancel · Tab to amend · ctrl+e to explain",
            ),
        };

        let details = extract_permission_prompt_details(&inspected).expect("details should parse");
        assert_eq!(details.prompt_type, "JavaScript code");
        assert_eq!(details.sandbox_mode.as_deref(), Some("unsandboxed"));
        assert_eq!(details.command.as_deref(), Some("let pass = 0, fail = 0"));
        assert_eq!(
            details.reason.as_deref(),
            Some("for (const [pat, path, expected] of cases) {")
        );
    }

    #[test]
    fn extracts_permission_prompt_details_with_aside_annotation() {
        let inspected = InspectedPane {
            classification: Classification {
                source: String::from("pane"),
                state: SessionState::PermissionDialog,
                has_questions: false,
                recap_present: false,
                recap_excerpt: None,
                signals: vec![String::from("permission-keywords")],
            },
            focused_source: String::from(
                "Bash command\n\n  GOCACHE=$TMPDIR/go-cache make manifests 2>&1 | tail-40\n  Regenerate manifests\nContains simple_expansion\n\nDo you want to proceed?\n❯ 1. Yes\n  2. No\nEsc to cancel · Tab to amend · ctrl+e to explain",
            ),
            raw_source: String::from(
                "Bash command\n\n  GOCACHE=$TMPDIR/go-cache make manifests 2>&1 | tail-40\n  Regenerate manifests\nContains simple_expansion\n\nDo you want to proceed?\n❯ 1. Yes\n  2. No\nEsc to cancel · Tab to amend · ctrl+e to explain",
            ),
        };

        let details = extract_permission_prompt_details(&inspected).expect("details should parse");
        assert_eq!(
            details.prompt_type,
            "Bash command (Contains simple_expansion)"
        );
        assert_eq!(
            details.command.as_deref(),
            Some("GOCACHE=$TMPDIR/go-cache make manifests 2>&1 | tail-40")
        );
        assert_eq!(details.reason.as_deref(), Some("Regenerate manifests"));
        assert_eq!(details.question.as_deref(), Some("Do you want to proceed?"));
    }

    #[test]
    fn extracts_permission_prompt_details_from_monitor_prompt() {
        let inspected = InspectedPane {
            classification: Classification {
                source: String::from("pane"),
                state: SessionState::PermissionDialog,
                has_questions: false,
                recap_present: false,
                recap_excerpt: None,
                signals: vec![String::from("permission-keywords")],
            },
            focused_source: String::from(
                "Monitor\n  until out=$(kubectl -n bloodraven-playground get mysqlfailovergroup playground -o jsonpath='{.status.activeSite}={.status.ready}'\n  2>/dev/null); [[ \"$out\" =~ ^[a-z]+=true$ ]]; do sleep 5; done; echo \"FG ready: $out\"\n  FG ready state\nUnhandled node type: string\nDo you want to proceed?\n❯ 1. Yes\n  2. No\nEsc to cancel · Tab to amend",
            ),
            raw_source: String::from(
                "Monitor\n  until out=$(kubectl -n bloodraven-playground get mysqlfailovergroup playground -o jsonpath='{.status.activeSite}={.status.ready}'\n  2>/dev/null); [[ \"$out\" =~ ^[a-z]+=true$ ]]; do sleep 5; done; echo \"FG ready: $out\"\n  FG ready state\nUnhandled node type: string\nDo you want to proceed?\n❯ 1. Yes\n  2. No\nEsc to cancel · Tab to amend",
            ),
        };

        let details = extract_permission_prompt_details(&inspected).expect("details should parse");
        assert_eq!(details.prompt_type, "Monitor");
        assert_eq!(details.sandbox_mode, None);
        assert_eq!(
            details.command.as_deref(),
            Some(
                "until out=$(kubectl -n bloodraven-playground get mysqlfailovergroup playground -o jsonpath='{.status.activeSite}={.status.ready}' 2>/dev/null); [[ \"$out\" =~ ^[a-z]+=true$ ]]; do sleep 5; done; echo \"FG ready: $out\""
            )
        );
        assert_eq!(details.reason.as_deref(), Some("FG ready state"));
        assert_eq!(details.question.as_deref(), Some("Do you want to proceed?"));
        assert!(is_yolo_safe_to_approve(&inspected, &sample_pane("claude")));
    }

    #[test]
    fn extracts_permission_prompt_details_from_fetch_prompt() {
        let inspected = InspectedPane {
            classification: Classification {
                source: String::from("pane"),
                state: SessionState::PermissionDialog,
                has_questions: false,
                recap_present: false,
                recap_excerpt: None,
                signals: vec![String::from("permission-keywords")],
            },
            focused_source: String::from(
                "Fetch\n  https://docus.dev/raw/en/getting-started/studio.md\n  Claude wants to fetch content from docus.dev\nDo you want to allow Claude to fetch this content?\n❯ 1. Yes\n  2. Yes, and don't ask again for docus.dev\n  3. No, and tell Claude what to do differently (esc)",
            ),
            raw_source: String::from(
                "Fetch\n  https://docus.dev/raw/en/getting-started/studio.md\n  Claude wants to fetch content from docus.dev\nDo you want to allow Claude to fetch this content?\n❯ 1. Yes\n  2. Yes, and don't ask again for docus.dev\n  3. No, and tell Claude what to do differently (esc)",
            ),
        };

        let details = extract_permission_prompt_details(&inspected).expect("details should parse");
        assert_eq!(details.prompt_type, "Fetch");
        assert_eq!(details.sandbox_mode, None);
        assert_eq!(
            details.command.as_deref(),
            Some("https://docus.dev/raw/en/getting-started/studio.md")
        );
        assert_eq!(
            details.reason.as_deref(),
            Some("Claude wants to fetch content from docus.dev")
        );
        assert_eq!(
            details.question.as_deref(),
            Some("Do you want to allow Claude to fetch this content?")
        );
        assert!(is_yolo_safe_to_approve(&inspected, &sample_pane("claude")));
    }

    #[test]
    fn extracts_permission_prompt_details_skips_preceding_chat_text() {
        let inspected = InspectedPane {
            classification: Classification {
                source: String::from("pane"),
                state: SessionState::PermissionDialog,
                has_questions: false,
                recap_present: false,
                recap_excerpt: None,
                signals: vec![String::from("permission-keywords")],
            },
            focused_source: String::from(
                "✔ Add MysqlBackup.status.mysqlImage field (◼ Write tests for Phase 2 changes)\n✔ Add PITR verification spec + binlog replay\n✔ Add SanityCheck spec + Checking phase\nBash command (unsandboxed)\n  mysql --execute=\"SELECT 1\"\n  Run permission prompt extraction test\nDo you want to proceed?\n❯ 1. Yes\n  2. No\nEsc to cancel · Tab to amend · ctrl+e to explain",
            ),
            raw_source: String::from(
                "✔ Add MysqlBackup.status.mysqlImage field (◼ Write tests for Phase 2 changes)\n✔ Add PITR verification spec + binlog replay\n✔ Add SanityCheck spec + Checking phase\nBash command (unsandboxed)\n  mysql --execute=\"SELECT 1\"\n  Run permission prompt extraction test\nDo you want to proceed?\n❯ 1. Yes\n  2. No\nEsc to cancel · Tab to amend · ctrl+e to explain",
            ),
        };

        let details = extract_permission_prompt_details(&inspected).expect("details should parse");
        assert_eq!(details.prompt_type, "Bash command");
        assert_eq!(
            details.command.as_deref(),
            Some("mysql --execute=\"SELECT 1\"")
        );
        assert_eq!(
            details.reason.as_deref(),
            Some("Run permission prompt extraction test")
        );
        assert_eq!(details.question.as_deref(), Some("Do you want to proceed?"));
    }

    #[test]
    fn extracts_permission_prompt_details_with_wrapped_command_after_todo_list() {
        let inspected = InspectedPane {
            classification: Classification {
                source: String::from("pane"),
                state: SessionState::PermissionDialog,
                has_questions: false,
                recap_present: false,
                recap_excerpt: None,
                signals: vec![String::from("permission-keywords")],
            },
            focused_source: String::from(
                "✔ Add PITR verification spec + binlog replay\n✔ Add SanityCheck spec + Checking phase\n✔ Write tests for Phase 2 changes\n✔ Regenerate manifests + RBAC mirror + docs\n◼ Run pre-PR gate\n────────────────────────────────────────────────────────────────────────\nBash command\n  export PATH=\"$(go env GOPATH)/bin:$PATH\" && GOCACHE=$TMPDIR/go-cache GOLANGCI_LINT_CACHE=$TMPDIR/golangci-cache make lint 2>&1 |\n  tail-40\n  Run golangci-lint\nContains simple_expansion\nDo you want to proceed?\n❯ 1. Yes\n  2. No\nEsc to cancel · Tab to amend · ctrl+e to explain",
            ),
            raw_source: String::from(
                r#"✔ Add PITR verification spec + binlog replay
✔ Add SanityCheck spec + Checking phase
✔ Write tests for Phase 2 changes
✔ Regenerate manifests + RBAC mirror + docs
◼ Run pre-PR gate
────────────────────────────────────────────────────────────────────────
Bash command
  export PATH="$(go env GOPATH)/bin:$PATH" && GOCACHE=$TMPDIR/go-cache GOLANGCI_LINT_CACHE=$TMPDIR/golangci-cache make lint 2>&1 |
  tail-40
  Run golangci-lint
Contains simple_expansion
Do you want to proceed?
❯ 1. Yes
  2. No
Esc to cancel · Tab to amend · ctrl+e to explain"#,
            ),
        };

        let details = extract_permission_prompt_details(&inspected).expect("details should parse");
        assert_eq!(
            details.prompt_type,
            "Bash command (Contains simple_expansion)"
        );
        assert_eq!(
            details.command.as_deref(),
            Some(
                r#"export PATH="$(go env GOPATH)/bin:$PATH" && GOCACHE=$TMPDIR/go-cache GOLANGCI_LINT_CACHE=$TMPDIR/golangci-cache make lint 2>&1 | tail-40"#
            )
        );
        assert_eq!(details.reason.as_deref(), Some("Run golangci-lint"));
    }

    #[test]
    fn extracts_permission_prompt_details_from_full_frame_when_focused_view_is_truncated() {
        let inspected = InspectedPane {
            classification: Classification {
                source: String::from("pane"),
                state: SessionState::PermissionDialog,
                has_questions: false,
                recap_present: false,
                recap_excerpt: None,
                signals: vec![String::from("permission-keywords")],
            },
            focused_source: String::from(
                "✔ Add PITR verification spec + binlog replay\n✔ Add SanityCheck spec + Checking phase\n✔ Write tests for Phase 2 changes\n✔ Regenerate manifests + RBAC mirror + docs\n◼ Run pre-PR gate\nDo you want to proceed?\n❯ 1. Yes\n  2. No",
            ),
            raw_source: String::from(
                r#"❯ commit and push
✔ Add PITR verification spec + binlog replay
✔ Add SanityCheck spec + Checking phase
✔ Write tests for Phase 2 changes
✔ Regenerate manifests + RBAC mirror + docs
◼ Run pre-PR gate
────────────────────────────────────────────────────────────────────────
Bash command (unsandboxed)
  export PATH="$(go env GOPATH)/bin:$PATH" && GOCACHE=$TMPDIR/go-cache GOLANGCI_LINT_CACHE=$TMPDIR/golangci-cache make lint 2>&1 |
  tail-40
  Run golangci-lint
Contains simple_expansion
Do you want to proceed?
❯ 1. Yes
  2. No
Esc to cancel · Tab to amend · ctrl+e to explain"#,
            ),
        };

        let details = extract_permission_prompt_details(&inspected).expect("details should parse");
        assert_eq!(
            details.prompt_type,
            "Bash command (Contains simple_expansion)"
        );
        assert_eq!(
            details.command.as_deref(),
            Some(
                r#"export PATH="$(go env GOPATH)/bin:$PATH" && GOCACHE=$TMPDIR/go-cache GOLANGCI_LINT_CACHE=$TMPDIR/golangci-cache make lint 2>&1 | tail-40"#
            )
        );
        assert_eq!(details.reason.as_deref(), Some("Run golangci-lint"));
        assert!(is_yolo_safe_to_approve(&inspected, &sample_pane("claude")));
    }

    #[test]
    fn extract_permission_prompt_details_refuses_bogus_titles() {
        let inspected = InspectedPane {
            classification: Classification {
                source: String::from("pane"),
                state: SessionState::PermissionDialog,
                has_questions: false,
                recap_present: false,
                recap_excerpt: None,
                signals: vec![String::from("permission-keywords")],
            },
            focused_source: String::from(
                "✔ Add MysqlBackup.status.mysqlImage field\n✔ Add PITR verification spec + binlog replay\nDo you want to proceed?\n❯ 1. Yes",
            ),
            raw_source: String::from(
                "✔ Add MysqlBackup.status.mysqlImage field\n✔ Add PITR verification spec + binlog replay\nDo you want to proceed?\n❯ 1. Yes",
            ),
        };

        assert!(extract_permission_prompt_details(&inspected).is_none());
    }

    #[test]
    fn yolo_refuses_to_approve_when_chat_input_is_active() {
        let inspected = InspectedPane {
            classification: Classification {
                source: String::from("pane"),
                state: SessionState::PermissionDialog,
                has_questions: false,
                recap_present: false,
                recap_excerpt: None,
                signals: vec![String::from("permission-keywords")],
            },
            focused_source: String::from(
                "Bash command (unsandboxed)\nmake generate 2>&1 | tail -40\nRun tests\nDo you want to proceed?\n❯ 1. Yes\n2. No\n❯ Here's some of the output:",
            ),
            raw_source: String::from(
                "Bash command (unsandboxed)\nmake generate 2>&1 | tail -40\nRun tests\nDo you want to proceed?\n❯ 1. Yes\n2. No\n❯ Here's some of the output:",
            ),
        };

        assert!(!is_yolo_safe_to_approve(&inspected, &sample_pane("claude")));
    }

    #[test]
    fn yolo_allows_permission_prompt_with_cursor_marked_command_line() {
        let inspected = InspectedPane {
            classification: Classification {
                source: String::from("pane"),
                state: SessionState::PermissionDialog,
                has_questions: false,
                recap_present: false,
                recap_excerpt: None,
                signals: vec![String::from("permission-keywords")],
            },
            focused_source: String::from(
                r#"Bash command (unsandboxed)
❯ OPENCODE_SERVER_HOST=seamus OPENCODE_SERVER_PORT=4095 occtl run --model openai/gpt-5.4 --variant xhigh --title "ultra-review 250d41d runtime/GPT-5.4" --file
   .tmp/ultra-review-250d41d/diff.patch --out .tmp/ultra-review-250d41d/results/runtime-gpt54.out
   GPT-5.4 runtime review via OpenCode
Do you want to proceed?
❯ 1. Yes
  2. No
Esc to cancel · Tab to amend · ctrl+e to explain"#,
            ),
            raw_source: String::from(
                r#"Bash command (unsandboxed)
❯ OPENCODE_SERVER_HOST=seamus OPENCODE_SERVER_PORT=4095 occtl run --model openai/gpt-5.4 --variant xhigh --title "ultra-review 250d41d runtime/GPT-5.4" --file
   .tmp/ultra-review-250d41d/diff.patch --out .tmp/ultra-review-250d41d/results/runtime-gpt54.out
   GPT-5.4 runtime review via OpenCode
Do you want to proceed?
❯ 1. Yes
  2. No
Esc to cancel · Tab to amend · ctrl+e to explain"#,
            ),
        };

        let details = extract_permission_prompt_details(&inspected).expect("details should parse");
        assert_eq!(details.prompt_type, "Bash command");
        assert_eq!(details.sandbox_mode.as_deref(), Some("unsandboxed"));
        assert!(
            details
                .command
                .as_deref()
                .is_some_and(|command| command.starts_with("OPENCODE_SERVER_HOST=seamus"))
        );
        assert!(is_yolo_safe_to_approve(&inspected, &sample_pane("claude")));
    }

    #[test]
    fn yolo_still_refuses_chat_input_after_permission_question() {
        let inspected = InspectedPane {
            classification: Classification {
                source: String::from("pane"),
                state: SessionState::PermissionDialog,
                has_questions: false,
                recap_present: false,
                recap_excerpt: None,
                signals: vec![String::from("permission-keywords")],
            },
            focused_source: String::from(
                "Bash command (unsandboxed)\nmake generate\nRegenerate manifests\nDo you want to proceed?\n❯ 1. Yes\n2. No\n❯ explain why first",
            ),
            raw_source: String::from(
                "Bash command (unsandboxed)\nmake generate\nRegenerate manifests\nDo you want to proceed?\n❯ 1. Yes\n2. No\n❯ explain why first",
            ),
        };

        assert!(!is_yolo_safe_to_approve(&inspected, &sample_pane("claude")));
    }

    #[test]
    fn yolo_allows_codex_command_permission_prompt() {
        let inspected = InspectedPane {
            classification: Classification {
                source: String::from("pane"),
                state: SessionState::PermissionDialog,
                has_questions: false,
                recap_present: false,
                recap_excerpt: None,
                signals: vec![
                    String::from("permission-keywords"),
                    String::from(SIGNAL_CODEX_KEYWORDS),
                ],
            },
            focused_source: String::from(
                "Would you like to run the following command?\n\n$ tmux list-windows -a -F '#{session_name}:#{window_index} #{window_name}'\n\n› 1. Yes, proceed (y)\n  2. Yes, and don't ask again for tmux list-windows commands in /home/colin/Projects/botctl (a)\n  3. No, and tell Codex what to do differently (esc)",
            ),
            raw_source: String::from(
                "Would you like to run the following command?\n\n$ tmux list-windows -a -F '#{session_name}:#{window_index} #{window_name}'\n\n› 1. Yes, proceed (y)\n  2. Yes, and don't ask again for tmux list-windows commands in /home/colin/Projects/botctl (a)\n  3. No, and tell Codex what to do differently (esc)",
            ),
        };

        assert!(is_yolo_safe_to_approve(&inspected, &sample_pane("claude")));
    }

    #[test]
    fn yolo_allows_codex_permission_prompt_after_stale_chat_input() {
        let inspected = InspectedPane {
            classification: Classification {
                source: String::from("pane"),
                state: SessionState::PermissionDialog,
                has_questions: false,
                recap_present: false,
                recap_excerpt: None,
                signals: vec![
                    String::from("permission-keywords"),
                    String::from(SIGNAL_CODEX_KEYWORDS),
                ],
            },
            focused_source: String::from(
                "› Look at 0:7.0 it is being incorrectly identified as a permission prompt\n\n• Running tmux list-panes -a\n\nWould you like to run the following command?\n\n$ tmux list-panes -a\n\n› 1. Yes, proceed (y)\n  2. Yes, and don't ask again for commands that start with `tmux list-panes` (p)\n  3. No, and tell Codex what to do differently (esc)",
            ),
            raw_source: String::from(
                "› Look at 0:7.0 it is being incorrectly identified as a permission prompt\n\n• Running tmux list-panes -a\n\nWould you like to run the following command?\n\n$ tmux list-panes -a\n\n› 1. Yes, proceed (y)\n  2. Yes, and don't ask again for commands that start with `tmux list-panes` (p)\n  3. No, and tell Codex what to do differently (esc)",
            ),
        };

        assert!(is_yolo_safe_to_approve(&inspected, &sample_pane("claude")));
    }

    #[test]
    fn yolo_allows_truncated_codex_prompt_with_live_confirm_footer() {
        let inspected = InspectedPane {
            classification: Classification {
                source: String::from("pane"),
                state: SessionState::PermissionDialog,
                has_questions: false,
                recap_present: false,
                recap_excerpt: None,
                signals: vec![
                    String::from("permission-keywords"),
                    String::from(SIGNAL_CODEX_KEYWORDS),
                ],
            },
            focused_source: String::from(
                "› 1. Yes, proceed (y)\n  2. Yes, and don't ask again for commands that start with `tmux list-panes` (p)\n  3. No, and tell Codex what to do differently (esc)\n\nPress enter to confirm or esc to cancel",
            ),
            raw_source: String::from(
                "› Look at 0:7.0 it is being incorrectly identified as a permission prompt\n\n› 1. Yes, proceed (y)\n  2. Yes, and don't ask again for commands that start with `tmux list-panes` (p)\n  3. No, and tell Codex what to do differently (esc)\n\nPress enter to confirm or esc to cancel",
            ),
        };

        assert!(is_yolo_safe_to_approve(&inspected, &sample_pane("claude")));
    }

    #[test]
    fn yolo_refuses_codex_permission_prompt_when_chat_input_is_active() {
        let inspected = InspectedPane {
            classification: Classification {
                source: String::from("pane"),
                state: SessionState::PermissionDialog,
                has_questions: false,
                recap_present: false,
                recap_excerpt: None,
                signals: vec![
                    String::from("permission-keywords"),
                    String::from(SIGNAL_CODEX_KEYWORDS),
                ],
            },
            focused_source: String::from(
                "Would you like to run the following command?\n\n$ tmux list-windows -a\n\n› 1. Yes, proceed (y)\n  2. No\n› explain why this is needed first",
            ),
            raw_source: String::from(
                "Would you like to run the following command?\n\n$ tmux list-windows -a\n\n› 1. Yes, proceed (y)\n  2. No\n› explain why this is needed first",
            ),
        };

        assert!(!is_yolo_safe_to_approve(&inspected, &sample_pane("claude")));
    }

    #[test]
    fn yolo_refuses_to_approve_self_settings_prompt_even_when_parseable() {
        let inspected = InspectedPane {
            classification: Classification {
                source: String::from("pane"),
                state: SessionState::PermissionDialog,
                has_questions: false,
                recap_present: false,
                recap_excerpt: None,
                signals: vec![
                    String::from("permission-keywords"),
                    String::from(SIGNAL_SELF_SETTINGS_LANGUAGE),
                ],
            },
            focused_source: String::from(
                "Bash command (unsandboxed)\npython tools/update.py .claude/commands/commit-and-push.md\nUpdate project slash command\nDo you want to proceed?\n❯ 1. Yes\n2. Yes, and allow Claude to edit its own settings for this session\n3. No\nEsc to cancel · Tab to amend · ctrl+e to explain",
            ),
            raw_source: String::from(
                "Bash command (unsandboxed)\npython tools/update.py .claude/commands/commit-and-push.md\nUpdate project slash command\nDo you want to proceed?\n❯ 1. Yes\n2. Yes, and allow Claude to edit its own settings for this session\n3. No\nEsc to cancel · Tab to amend · ctrl+e to explain",
            ),
        };

        assert!(!is_yolo_safe_to_approve(&inspected, &sample_pane("claude")));
    }

    #[test]
    fn yolo_refuses_to_approve_sensitive_claude_path_prompt_even_when_parseable() {
        let inspected = InspectedPane {
            classification: Classification {
                source: String::from("pane"),
                state: SessionState::PermissionDialog,
                has_questions: false,
                recap_present: false,
                recap_excerpt: None,
                signals: vec![
                    String::from("permission-keywords"),
                    String::from(SIGNAL_SENSITIVE_CLAUDE_PATH),
                ],
            },
            focused_source: String::from(
                "Bash command (unsandboxed)\npython tools/update.py /repo/.claude/commands/commit-and-push.md\nUpdate project slash command\nDo you want to proceed?\n❯ 1. Yes\n2. No\nEsc to cancel · Tab to amend · ctrl+e to explain",
            ),
            raw_source: String::from(
                "Bash command (unsandboxed)\npython tools/update.py /repo/.claude/commands/commit-and-push.md\nUpdate project slash command\nDo you want to proceed?\n❯ 1. Yes\n2. No\nEsc to cancel · Tab to amend · ctrl+e to explain",
            ),
        };

        assert!(!is_yolo_safe_to_approve(&inspected, &sample_pane("claude")));
    }

    #[test]
    fn yolo_allows_repo_claude_command_prompt_when_parseable() {
        let inspected = InspectedPane {
            classification: Classification {
                source: String::from("pane"),
                state: SessionState::PermissionDialog,
                has_questions: false,
                recap_present: false,
                recap_excerpt: None,
                signals: vec![String::from("permission-keywords")],
            },
            focused_source: String::from(
                "Bash command (unsandboxed)\nfor f in /repo/.claude/commands/colin/*.md /repo/.claude/commands/coolify.md; do head -20 \"$f\"; done\nRead command headers\nUnhandled node type: string\nDo you want to proceed?\n❯ 1. Yes\n2. No\nEsc to cancel · Tab to amend · ctrl+e to explain",
            ),
            raw_source: String::from(
                "Bash command (unsandboxed)\nfor f in /repo/.claude/commands/colin/*.md /repo/.claude/commands/coolify.md; do head -20 \"$f\"; done\nRead command headers\nUnhandled node type: string\nDo you want to proceed?\n❯ 1. Yes\n2. No\nEsc to cancel · Tab to amend · ctrl+e to explain",
            ),
        };

        assert!(is_yolo_safe_to_approve(&inspected, &sample_pane("claude")));
    }

    #[test]
    fn yolo_allows_codex_live_option_only_prompt() {
        let source = "  Verification:\n  - `pnpm test:unit tests/unit/server/qbo-sync.spec.ts` passed.\n› 1. Yes, proceed (y)\n  2. No, and tell Codex what to do differently (esc)";
        let inspected = InspectedPane {
            classification: Classification {
                source: String::from("pane"),
                state: SessionState::PermissionDialog,
                has_questions: false,
                recap_present: false,
                recap_excerpt: None,
                signals: vec![
                    String::from("permission-keywords"),
                    String::from(SIGNAL_CODEX_KEYWORDS),
                ],
            },
            focused_source: String::from(source),
            raw_source: String::from(source),
        };

        assert!(is_yolo_safe_to_approve(&inspected, &sample_pane("claude")));
    }

    #[test]
    fn yolo_allows_truncated_project_scoped_permission_prompt() {
        let inspected = InspectedPane {
            classification: Classification {
                source: String::from("pane"),
                state: SessionState::PermissionDialog,
                has_questions: false,
                recap_present: false,
                recap_excerpt: None,
                signals: vec![String::from("permission-keywords")],
            },
            focused_source: String::from(
                "Save Opus craft findings\n\nDo you want to proceed?\n❯ 1. Yes\n  2. Yes, and always allow access to results/ from this project\n  3. No\n\nEsc to cancel · Tab to amend · ctrl+e to explain",
            ),
            raw_source: String::from(
                "Save Opus craft findings\n\nDo you want to proceed?\n❯ 1. Yes\n  2. Yes, and always allow access to results/ from this project\n  3. No\n\nEsc to cancel · Tab to amend · ctrl+e to explain",
            ),
        };

        assert!(is_yolo_safe_to_approve(&inspected, &sample_pane("claude")));
    }

    #[test]
    fn persistent_dashboard_child_command_preserves_dashboard_flags() {
        let command = persistent_dashboard_child_command_with_target_socket(
            &DashboardArgs {
                poll_ms: 750,
                history_lines: 200,
                state_dir: Some(PathBuf::from("/tmp/botctl state")),
                exit_on_navigate: false,
                persistent: true,
                unmanaged: false,
            },
            Some(Path::new("/tmp/tmux-1000/default")),
        )
        .expect("persistent child command should build");

        assert!(command.contains(&format!("{}=1", DASHBOARD_PERSISTENT_CHILD_ENV)));
        assert!(command.contains(&format!(
            "{}=/tmp/tmux-1000/default",
            DASHBOARD_TARGET_TMUX_SOCKET_ENV
        )));
        assert!(command.contains(" dashboard --persistent --poll-ms 750 --history-lines 200 "));
        assert!(command.contains("--state-dir '/tmp/botctl state'"));
    }

    #[test]
    fn parses_tmux_socket_path_from_env_value() {
        assert_eq!(
            tmux_socket_path_from_value("/tmp/tmux-1000/default,1234,0"),
            Some(PathBuf::from("/tmp/tmux-1000/default"))
        );
        assert_eq!(tmux_socket_path_from_value(""), None);
    }

    #[test]
    fn persistent_dashboard_child_command_captures_outer_tmux_socket() {
        let command = persistent_dashboard_child_command_with_target_socket(
            &DashboardArgs {
                poll_ms: 750,
                history_lines: 200,
                state_dir: None,
                exit_on_navigate: false,
                persistent: true,
                unmanaged: false,
            },
            Some(Path::new("/tmp/tmux-1000/default")),
        )
        .expect("persistent child command should build");

        assert!(command.contains(&format!(
            "{}=/tmp/tmux-1000/default",
            DASHBOARD_TARGET_TMUX_SOCKET_ENV
        )));
    }

    #[test]
    fn persistent_dashboard_child_command_omits_outer_tmux_socket_when_absent() {
        let command = persistent_dashboard_child_command_with_target_socket(
            &DashboardArgs {
                poll_ms: 750,
                history_lines: 200,
                state_dir: None,
                exit_on_navigate: false,
                persistent: true,
                unmanaged: false,
            },
            None,
        )
        .expect("persistent child command should build");

        assert!(!command.contains(DASHBOARD_TARGET_TMUX_SOCKET_ENV));
    }

    #[test]
    fn parse_env_value_from_environ_bytes_extracts_target_socket() {
        let bytes = b"BOTCTL_DASHBOARD_PERSISTENT_CHILD=1\0BOTCTL_DASHBOARD_TARGET_TMUX_SOCKET=/tmp/tmux-1000/default\0TMUX=/tmp/tmux-1000/botctl-dashboard,1,0\0";
        assert_eq!(
            parse_env_value_from_environ_bytes(bytes, DASHBOARD_TARGET_TMUX_SOCKET_ENV),
            Some(String::from("/tmp/tmux-1000/default"))
        );
        assert_eq!(parse_env_value_from_environ_bytes(bytes, "MISSING"), None);
    }

    #[test]
    fn shell_escape_quotes_whitespace() {
        assert_eq!(
            shell_escape("/tmp/botctl state"),
            String::from("'/tmp/botctl state'")
        );
    }

    #[test]
    fn yolo_wait_log_omits_live_preview_even_when_enabled() {
        let inspected = InspectedPane {
            classification: Classification {
                source: String::from("pane"),
                state: SessionState::PermissionDialog,
                has_questions: false,
                recap_present: false,
                recap_excerpt: None,
                signals: vec![String::from("permission-keywords")],
            },
            focused_source: String::from("Bash command (unsandboxed)\nDo you want to proceed?"),
            raw_source: String::from("Bash command (unsandboxed)\nDo you want to proceed?"),
        };

        let log = render_babysit_wait_event(
            "bloodraven",
            "%20",
            4,
            &inspected,
            true,
            BabysitFormat::Human,
            false,
        );
        assert!(!log.contains("Extracted prompt"));
        assert!(!log.to_lowercase().contains("babysit"));
    }

    #[test]
    fn keep_going_no_yolo_blocks_plan_approval_prompt() {
        let classification = Classification {
            source: String::from("pane"),
            state: SessionState::PlanApprovalPrompt,
            has_questions: false,
            recap_present: false,
            recap_excerpt: None,
            signals: vec![String::from("plan-approval-keywords")],
        };

        let error = keep_going_no_yolo_blocker(&classification, "%42")
            .expect("plan approval prompt should block no-yolo keep-going");
        assert!(error.to_string().contains("PlanApprovalPrompt"));
    }

    #[test]
    fn yolo_approve_log_shows_only_permission_preview_in_human_mode() {
        let inspected = InspectedPane {
            classification: Classification {
                source: String::from("pane"),
                state: SessionState::PermissionDialog,
                has_questions: false,
                recap_present: false,
                recap_excerpt: None,
                signals: vec![String::from("permission-keywords")],
            },
            focused_source: String::from(
                "Bash command (unsandboxed)\nmake generate 2>&1 | tail -40\nRegenerate manifests\nDo you want to proceed?\n❯ 1. Yes",
            ),
            raw_source: String::from(
                "Bash command (unsandboxed)\nmake generate 2>&1 | tail -40\nRegenerate manifests\nDo you want to proceed?\n❯ 1. Yes",
            ),
        };

        let log = render_babysit_action_event(
            "bloodraven",
            "%20",
            3,
            &inspected,
            GuardedWorkflow::ApprovePermission,
            true,
            BabysitFormat::Human,
            false,
        );

        assert!(log.contains("Auto-approving bloodraven (id:%20) (poll #3)"));
        assert!(!log.contains("Signals:"));
        assert!(!log.contains("Type:"));
        assert!(log.contains("Permission prompt"));
        assert!(log.contains("Do you want to proceed?"));
    }

    #[test]
    fn yolo_survey_log_uses_dismiss_language() {
        let inspected = InspectedPane {
            classification: Classification {
                source: String::from("pane"),
                state: SessionState::SurveyPrompt,
                has_questions: false,
                recap_present: false,
                recap_excerpt: None,
                signals: vec![String::from("survey-keywords")],
            },
            focused_source: String::from(
                "How is Claude doing this session? (optional)\n1: Bad    2: Fine   3: Good   0: Dismiss",
            ),
            raw_source: String::from(
                "How is Claude doing this session? (optional)\n1: Bad    2: Fine   3: Good   0: Dismiss",
            ),
        };

        let log = render_babysit_action_event(
            "seamus",
            "%20",
            7,
            &inspected,
            GuardedWorkflow::DismissSurvey,
            true,
            BabysitFormat::Human,
            false,
        );

        assert!(log.contains("Auto-dismissing seamus (id:%20) (poll #7)"));
        assert!(log.contains("Survey prompt"));
    }

    #[test]
    fn yolo_jsonl_renders_one_object_per_line() {
        let output = render_babysit_start_event(
            "bloodraven",
            "%20",
            500,
            false,
            &YoloRecord {
                instance_id: String::from("instance-20"),
                workspace_id: String::from("workspace-20"),
                enabled: true,
                pane_id: String::from("%20"),
                pane_tty: String::from("/dev/pts/1"),
                pane_pid: Some(123),
                session_id: String::from("$1"),
                session_name: String::from("demo"),
                window_id: String::from("@1"),
                window_name: String::from("claude"),
                current_command: String::from("bash"),
                current_path: String::from("/tmp/demo"),
            },
            std::path::Path::new("/tmp/record.txt"),
            BabysitFormat::Jsonl,
            false,
        );
        let parsed: serde_json::Value = serde_json::from_str(&output).expect("jsonl should parse");
        assert_eq!(parsed["kind"], "start");
        assert_eq!(parsed["pane_label"], "bloodraven");
        assert_eq!(parsed["pane_id"], "%20");
        assert_eq!(parsed["poll_ms"], 500);
        assert_eq!(parsed["command"], "bash");
        assert_eq!(parsed["record_path"], "/tmp/record.txt");
        assert!(parsed.get("timestamp").is_some());
        assert!(!output.to_lowercase().contains("babysit"));
    }

    #[test]
    fn pane_label_from_path_uses_cwd_basename() {
        assert_eq!(
            super::pane_label_from_path("/home/colin/Projects/bloodraven", "%10"),
            "bloodraven"
        );
        assert_eq!(super::pane_label_from_path("/", "%10"), "%10");
    }

    #[test]
    fn cleanup_babysit_record_disables_record() {
        let unique = format!("botctl-test-{}", std::process::id());
        let dir = std::env::temp_dir().join(unique);
        let workspace_root = dir.join("workspace");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&workspace_root).expect("workspace should exist");
        let workspace =
            resolve_workspace_for_path(&dir, &workspace_root).expect("workspace should resolve");
        let pane_id = "%99";
        let pane = TmuxPane {
            pane_id: pane_id.to_string(),
            pane_tty: String::from("/dev/pts/9"),
            pane_pid: Some(9),
            session_id: String::from("$9"),
            session_name: String::from("demo"),
            window_id: String::from("@9"),
            window_index: 9,
            window_name: String::from("claude"),
            pane_index: 0,
            current_command: String::from("claude"),
            current_path: workspace_root.display().to_string(),
            pane_title: String::new(),
            pane_active: true,
            cursor_x: Some(0),
            cursor_y: Some(0),
        };
        write_yolo_record(&dir, &workspace.id, &pane).expect("write record");
        cleanup_babysit_record(&dir, pane_id).expect("cleanup record");
        let reread = read_yolo_record(&dir, pane_id)
            .expect("read record")
            .expect("record exists");
        assert!(!reread.enabled);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn unique_temp_dir(label: &str) -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be valid")
            .as_nanos();
        std::env::temp_dir().join(format!("botctl-{label}-{}-{nanos}", std::process::id()))
    }

    // ── render_status_json agy_model tests ───────────────────────────────────

    fn agy_footer_frame() -> &'static str {
        // A minimal agy frame with a parseable model label in the footer.
        "Antigravity CLI\n? for shortcuts                              Gemini 3.5 Flash (High)\n"
    }

    fn sample_classification(state: SessionState) -> Classification {
        Classification {
            source: String::new(),
            state,
            has_questions: false,
            recap_present: false,
            recap_excerpt: None,
            signals: vec![],
        }
    }

    #[test]
    fn render_status_json_agy_pane_with_parseable_footer_has_string_agy_model() {
        let pane = sample_pane("agy");
        let classification = sample_classification(SessionState::ChatReady);
        let bindings = sample_bindings(crate::automation::KeybindingsStatus::Valid);

        let json_str =
            render_status_json(&pane, &classification, &bindings, agy_footer_frame(), false)
                .expect("render should succeed");
        let parsed: serde_json::Value = serde_json::from_str(&json_str).expect("json should parse");

        assert!(
            parsed.get("agy_model").is_some(),
            "agy_model key must be present for agy pane"
        );
        assert_eq!(
            parsed["agy_model"],
            serde_json::Value::String(String::from("Gemini 3.5 Flash (High)"))
        );
    }

    #[test]
    fn render_status_json_agy_pane_without_parseable_footer_has_null_agy_model() {
        let pane = sample_pane("agy");
        let classification = sample_classification(SessionState::BusyResponding);
        let bindings = sample_bindings(crate::automation::KeybindingsStatus::Valid);
        // Frame has no valid footer (spinner line only, no model delimiter).
        let frame = "Antigravity CLI\n⣾ Working...\n";

        let json_str = render_status_json(&pane, &classification, &bindings, frame, false)
            .expect("render should succeed");
        let parsed: serde_json::Value = serde_json::from_str(&json_str).expect("json should parse");

        assert!(
            parsed.get("agy_model").is_some(),
            "agy_model key must be present even when null"
        );
        assert_eq!(parsed["agy_model"], serde_json::Value::Null);
    }

    #[test]
    fn render_status_json_non_agy_pane_omits_agy_model_key() {
        let pane = sample_pane("claude");
        let classification = sample_classification(SessionState::ChatReady);
        let bindings = sample_bindings(crate::automation::KeybindingsStatus::Valid);
        let frame = "some claude output\n";

        let json_str = render_status_json(&pane, &classification, &bindings, frame, false)
            .expect("render should succeed");
        let parsed: serde_json::Value = serde_json::from_str(&json_str).expect("json should parse");

        assert!(
            parsed.get("agy_model").is_none(),
            "agy_model key must be absent for non-agy panes"
        );
    }

    /// V-3: A signal-fingerprinted agy pane (where `current_command != "agy"` but
    /// the classifier set SIGNAL_AGY_KEYWORDS) must still emit "agy_model" because
    /// "provider" is already "Antigravity" for such panes.
    #[test]
    fn render_status_json_signal_fingerprinted_agy_pane_emits_agy_model_key() {
        // Pane whose process name is not "agy" but the classification has the
        // agy signal (simulating a signal-only detection path).
        let pane = sample_pane("node"); // not literally "agy"
        let mut classification = sample_classification(SessionState::ChatReady);
        classification
            .signals
            .push(String::from(crate::classifier::SIGNAL_AGY_KEYWORDS));
        let bindings = sample_bindings(crate::automation::KeybindingsStatus::Valid);
        // Frame with a parseable agy footer.
        let frame = "? for shortcuts                              Gemini 3.5 Flash (High)\n";

        let json_str = render_status_json(&pane, &classification, &bindings, frame, false)
            .expect("render should succeed");
        let parsed: serde_json::Value = serde_json::from_str(&json_str).expect("json should parse");

        assert_eq!(
            parsed["provider"],
            serde_json::Value::String(String::from("Antigravity"))
        );
        assert!(
            parsed.get("agy_model").is_some(),
            "agy_model key must be present for signal-fingerprinted agy pane, got: {parsed}"
        );
    }

    // ── AGY-004: per-shape YOLO safety + ownership + raw-key + doctor ──────

    /// Captured agy command-permission frame with cursor on default `> 1. Yes`.
    /// Includes the bottom-anchored agy footer so the classifier short-circuits
    /// into `agy::classify_agy_state` rather than the Claude permission chain.
    const AGY_COMMAND_PERMISSION_FRAME_DEFAULT_CURSOR: &str = concat!(
        "● Bash(git remote -v) (ctrl+o to expand)\n",
        "\n",
        "Command\n",
        "─────────────────────────────────────────────\n",
        "\n",
        "  Requesting permission for: git remote -v\n",
        "\n",
        "Do you want to proceed?\n",
        "> 1. Yes\n",
        "  2. Yes, and always allow in this conversation for commands that start with 'git remote'\n",
        "  3. Yes, and always allow for commands that start with 'git remote' (Persist to settings.json)\n",
        "  4. No\n",
        "\n",
        "  ↑/↓ Navigate · tab Amend · e edit command\n",
        "esc to cancel                                       Gemini 3.5 Flash (High)\n",
    );

    /// Same shape but the operator has arrow-keyed onto option 3 (the persist
    /// option). The safety predicate must refuse.
    const AGY_COMMAND_PERMISSION_FRAME_CURSOR_MOVED: &str = concat!(
        "Command\n",
        "─────────────────────────────────────────────\n",
        "\n",
        "  Requesting permission for: git remote -v\n",
        "\n",
        "Do you want to proceed?\n",
        "  1. Yes\n",
        "  2. Yes, and always allow in this conversation for commands that start with 'git remote'\n",
        "> 3. Yes, and always allow for commands that start with 'git remote' (Persist to settings.json)\n",
        "  4. No\n",
        "\n",
        "esc to cancel                                       Gemini 3.5 Flash (High)\n",
    );

    fn sample_agy_inspected(raw_source: &str) -> InspectedPane {
        InspectedPane {
            classification: sample_agy_classification(SessionState::AgyCommandPermissionPrompt),
            focused_source: raw_source.to_string(),
            raw_source: raw_source.to_string(),
        }
    }

    /// Shared test fixture: a `Classification` carrying `SIGNAL_AGY_KEYWORDS`
    /// at the given state, with the recap/questions/source fields set to the
    /// canonical agy-classifier defaults. Extracted from five repeated inline
    /// literals across the tests below.
    fn sample_agy_classification(state: SessionState) -> Classification {
        Classification {
            source: String::from("pane"),
            state,
            has_questions: false,
            recap_present: false,
            recap_excerpt: None,
            signals: vec![String::from(crate::classifier::SIGNAL_AGY_KEYWORDS)],
        }
    }

    #[test]
    fn is_agy_command_permission_safe_to_approve_accepts_default_cursor() {
        let inspected = sample_agy_inspected(AGY_COMMAND_PERMISSION_FRAME_DEFAULT_CURSOR);
        let pane = sample_pane("agy");
        assert!(is_agy_command_permission_safe_to_approve(&inspected, &pane));
    }

    #[test]
    fn is_agy_command_permission_safe_to_approve_rejects_moved_cursor() {
        // Cursor on `> 3. ...` — must refuse so the YOLO loop does not
        // confirm the global-settings persist option.
        let inspected = sample_agy_inspected(AGY_COMMAND_PERMISSION_FRAME_CURSOR_MOVED);
        let pane = sample_pane("agy");
        assert!(!is_agy_command_permission_safe_to_approve(
            &inspected, &pane
        ));
    }

    #[test]
    fn is_agy_command_permission_safe_to_approve_rejects_non_agy_pane() {
        // Frame fingerprints as agy and is classified as the agy
        // command-permission shape — but the pane process is `claude`. The
        // pane process gate (`is_agy_pane`) must reject this so a stray
        // scrollback that contains the literal "Antigravity" string cannot
        // drive a Claude pane through the agy approve path.
        let inspected = sample_agy_inspected(AGY_COMMAND_PERMISSION_FRAME_DEFAULT_CURSOR);
        let pane = sample_pane("claude");
        assert!(!is_agy_command_permission_safe_to_approve(
            &inspected, &pane
        ));
    }

    #[test]
    fn is_agy_command_permission_safe_to_approve_rejects_wrong_state() {
        let mut inspected = sample_agy_inspected(AGY_COMMAND_PERMISSION_FRAME_DEFAULT_CURSOR);
        // Folder-trust agy shape: classifier-only, no auto-approve.
        inspected.classification.state = SessionState::AgyFolderTrustPrompt;
        let pane = sample_pane("agy");
        assert!(!is_agy_command_permission_safe_to_approve(
            &inspected, &pane
        ));
    }

    #[test]
    fn is_yolo_safe_to_approve_routes_agy_through_per_shape_branch() {
        // Top-level safety predicate must dispatch into the agy branch when
        // the pane process is `agy`. The branch is now gated on
        // `is_pane_command_agy` (not on the signal), so it fires only for a
        // real agy pane and the downstream `is_agy_pane` check is
        // defense-in-depth.
        let inspected = sample_agy_inspected(AGY_COMMAND_PERMISSION_FRAME_DEFAULT_CURSOR);
        assert!(is_yolo_safe_to_approve(&inspected, &sample_pane("agy")));
        // Same classification, wrong pane process — falls through to the
        // Claude/Codex dispatch. The agy state is not a `PermissionDialog`,
        // so the Claude branch refuses (no auto-approve).
        assert!(!is_yolo_safe_to_approve(&inspected, &sample_pane("claude")));
    }

    #[test]
    fn is_yolo_safe_to_approve_claude_pane_with_agy_signal_does_not_short_circuit() {
        // Regression for the over-broad early branch: a Claude pane whose
        // scrollback contains "Antigravity"/"Gemini" can carry
        // `SIGNAL_AGY_KEYWORDS` on its classification without actually being
        // an agy pane. The top-level safety predicate must NOT route through
        // the agy refusal in that case — it must fall through to the
        // Claude/Codex dispatch and approve a genuine PermissionDialog.
        let raw = "Bash command (unsandboxed)\n  echo hi\n  Run a harmless echo\nDo you want to proceed?\n❯ 1. Yes\n  2. No\nEsc to cancel";
        let inspected = InspectedPane {
            classification: Classification {
                source: String::from("pane"),
                state: SessionState::PermissionDialog,
                has_questions: false,
                recap_present: false,
                recap_excerpt: None,
                // Stale agy signal from scrollback — the pane process is the
                // source of truth.
                signals: vec![String::from(crate::classifier::SIGNAL_AGY_KEYWORDS)],
            },
            focused_source: raw.to_string(),
            raw_source: raw.to_string(),
        };
        // Claude pane → must approve a genuine PermissionDialog despite the
        // stray agy signal.
        assert!(is_yolo_safe_to_approve(&inspected, &sample_pane("claude")));
    }

    #[test]
    fn ensure_pane_owned_by_yolo_provider_accepts_agy_pane() {
        let classification = sample_agy_classification(SessionState::AgyCommandPermissionPrompt);
        let pane = sample_pane("agy");
        ensure_pane_owned_by_yolo_provider(&pane, &classification)
            .expect("yolo start must admit agy panes");
    }

    #[test]
    fn ensure_pane_owned_by_yolo_provider_rejects_unknown_provider() {
        let classification = Classification {
            source: String::from("pane"),
            state: SessionState::Unknown,
            has_questions: false,
            recap_present: false,
            recap_excerpt: None,
            signals: vec![],
        };
        let pane = sample_pane("bash");
        assert!(ensure_pane_owned_by_yolo_provider(&pane, &classification).is_err());
    }

    #[test]
    fn ensure_pane_owned_by_automatable_provider_still_rejects_agy_pane() {
        // `automatable_provider` gates approve/reject/submit/auto-unstick;
        // agy has no Claude-style keybinding contract so it must keep
        // returning Err here even though the YOLO guard now admits it.
        let classification = sample_agy_classification(SessionState::AgyCommandPermissionPrompt);
        let pane = sample_pane("agy");
        let error = ensure_pane_owned_by_automatable_provider(&pane, &classification)
            .expect_err("automatable guard must still reject agy");
        assert!(error.to_string().contains("agy"));
    }

    #[test]
    fn raw_key_for_workflow_returns_enter_for_agy_command_permission() {
        let classification = sample_agy_classification(SessionState::AgyCommandPermissionPrompt);
        assert_eq!(
            raw_key_for_workflow(GuardedWorkflow::ApprovePermission, &classification),
            Some(("Enter", "enter"))
        );
    }

    #[test]
    fn render_doctor_recommendations_per_agy_shape() {
        let pane = sample_pane("agy");
        let bindings = sample_bindings(KeybindingsStatus::Valid);

        // Each of the three agy shapes must round-trip *exactly* to its
        // per-shape recommendation line. Substring matching is too weak — a
        // regression that truncated the helper to just "recommendation=agy"
        // would silently pass. Assert byte-for-byte equality (post-trim).
        for state in [
            SessionState::AgyCommandPermissionPrompt,
            SessionState::AgyFolderTrustPrompt,
            SessionState::AgySettingsPersistPrompt,
        ] {
            let classification = sample_agy_classification(state);
            let report = render_doctor_recommendations(&classification, &pane, &bindings);
            assert_eq!(
                report.trim(),
                agy_doctor_recommendation_line(state).trim(),
                "doctor report for {} must equal the per-shape helper line verbatim",
                state.as_str(),
            );
            assert!(
                !report.contains("install-bindings"),
                "agy doctor output must not suggest install-bindings; got {report}",
            );
        }

        // Fallback arm: any non-agy-prompt state on an agy pane funnels into
        // the generic copy. Exercise multiple states so a regression that
        // collapses one fallback case is caught.
        for state in [
            SessionState::ChatReady,
            SessionState::BusyResponding,
            SessionState::Unknown,
        ] {
            let classification = sample_agy_classification(state);
            let report = render_doctor_recommendations(&classification, &pane, &bindings);
            assert_eq!(
                report.trim(),
                agy_doctor_recommendation_line(state).trim(),
                "doctor report for {} must equal the fallback helper line verbatim",
                state.as_str(),
            );
            assert!(
                !report.contains("install-bindings"),
                "agy doctor output must not suggest install-bindings; got {report}",
            );
        }
    }
}
