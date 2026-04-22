use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;
use std::fs::{self, File};
use std::io::{self, BufWriter, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread;
use std::time::Duration;
use time::OffsetDateTime;
use time::macros::format_description;

use crate::automation::{
    AutomationAction, GuardedWorkflow, KeybindingsInspection, KeybindingsStatus,
    ResolvedKeybindings, inspect_keybindings, install_recommended_keybindings,
    load_resolved_keybindings, prompt_submission_sequence, render_keybindings_json,
    validate_workflow_state,
};
use crate::classifier::{
    Classification, Classifier, SIGNAL_SELF_SETTINGS_LANGUAGE, SIGNAL_SENSITIVE_CLAUDE_PATH,
    SessionState,
};
use crate::cli::{
    AttachArgs, AutoUnstickArgs, BabysitFormat, CaptureArgs, ClassifyArgs, Command,
    ContinueSessionArgs, DoctorArgs, EditorHelperArgs, InstallBindingsArgs, KeepGoingArgs,
    ListPanesArgs, ObserveArgs, PaneCommandArgs, PaneTargetArgs, PreparePromptArgs,
    RecordFixtureArgs, ReplayArgs, SendActionArgs, ServeArgs, StartArgs, StatusArgs,
    SubmitPromptArgs, YoloStartArgs, YoloStopArgs,
};
use crate::fixtures::{FixtureCase, FixtureRecordInput, record_case};
use crate::observe::{CollectedObservation, ObserveRequest, collect_observation};
use crate::yolo::{
    YoloRecord, disable_yolo_record, list_yolo_pane_ids, read_yolo_record, write_yolo_record,
};
use crate::prompt::{
    PromptSource, prepare_prompt, resolve_prompt_text, resolve_state_dir, write_editor_target,
    write_editor_target_from_pending,
};
use crate::serve::{ServeEvent, ServePaneSnapshot, ServeRequest, run_serve_loop};
use crate::storage::{
    WorkspaceRecord, bootstrap_state_db, capture_artifact_path, export_artifact_path,
    resolve_workspace, resolve_workspace_for_path, state_db_path,
    store_pending_prompt_for_tmux_instance, tape_artifact_path,
};
use crate::tmux::{StartSessionRequest, TmuxClient, TmuxPane};

const ACTION_GUARD_HISTORY_LINES: usize = 120;
const AUTO_UNSTICK_STEP_DELAY_MS: u64 = 150;
const KEEP_GOING_HISTORY_LINES: usize = 2000;
const KEEP_GOING_PROMPT_ANCHOR: &str = "Audit the task currently in scope";
const KEEP_GOING_CUSTOM_PROMPT_ANCHOR: &str = "[[BOTCTL_KEEP_GOING_END_PROMPT]]";
const PROMPT_SUBMISSION_POLL_MS: u64 = 100;
const PROMPT_SUBMISSION_TIMEOUT_MS: u64 = 5000;
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
        Command::Status(args) => run_status(args),
        Command::Doctor(args) => run_doctor(args),
        Command::Observe(args) => run_observe(args),
        Command::Serve(args) => run_serve(args),
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
        Command::PreparePrompt(args) => run_prepare_prompt(args),
        Command::EditorHelper(args) => run_editor_helper(args),
        Command::SubmitPrompt(args) => run_submit_prompt(args),
        Command::YoloStart(args) => run_yolo_start(args),
        Command::YoloStop(args) => run_yolo_stop(args),
        Command::Help => Ok(crate::cli::usage()),
    }
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

    let client = TmuxClient::default();

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
    Ok(render_list_panes(&panes, args.all))
}

fn render_list_panes(panes: &[TmuxPane], include_all: bool) -> String {
    let panes = if include_all {
        panes.iter().collect::<Vec<_>>()
    } else {
        panes
            .iter()
            .filter(|pane| is_pane_command_claude(pane))
            .collect::<Vec<_>>()
    };
    if panes.is_empty() {
        return if include_all {
            String::from("no panes found")
        } else {
            String::from("no claude panes found")
        };
    }

    let mut out = String::new();
    for pane in panes {
        let cursor = match (pane.cursor_x, pane.cursor_y) {
            (Some(x), Some(y)) => format!("{x},{y}"),
            _ => String::from("-"),
        };
        out.push_str(&format!(
            "{}\tsession={}\twindow={}\tactive={}\tcommand={}\tcwd={}\tcursor={}\n",
            pane.pane_id,
            pane.session_name,
            pane.window_name,
            pane.pane_active,
            pane.current_command,
            pane.current_path,
            cursor
        ));
    }
    out.trim_end().to_string()
}

fn run_capture(args: CaptureArgs) -> AppResult<String> {
    let client = TmuxClient::default();
    let pane = resolve_pane_by_id(&client, &args.pane_id)?;
    client.capture_pane(&pane.pane_id, args.history_lines)
}

fn run_status(args: StatusArgs) -> AppResult<String> {
    let client = TmuxClient::default();
    let pane = resolve_pane_by_id(&client, &args.pane_id)?;
    let frame = client.capture_pane(&pane.pane_id, args.history_lines)?;
    let focused = focused_frame_source(&frame);
    let classification = Classifier.classify(&pane.pane_id, &focused);
    let bindings = inspect_keybindings(None).map_err(AppError::new)?;

    Ok(render_status_report(
        &pane,
        &classification,
        &bindings,
        &frame,
    ))
}

fn run_doctor(args: DoctorArgs) -> AppResult<String> {
    let client = TmuxClient::default();
    let pane = resolve_doctor_pane(
        &client,
        args.session_name.as_deref(),
        args.pane_id.as_deref(),
    )?;
    let frame = client.capture_pane(&pane.pane_id, args.history_lines)?;
    let focused = focused_frame_source(&frame);
    let classification = Classifier.classify(&pane.pane_id, &focused);
    let bindings = inspect_keybindings(args.bindings_path.as_deref()).map_err(AppError::new)?;
    let automation_ready =
        is_pane_command_claude(&pane) && bindings.status == KeybindingsStatus::Valid;

    Ok(format!(
        "automation_ready={}\npane={}\nsession={}\nwindow={}\nactive={}\ncommand={}\ncommand_matches_claude={}\ncwd={}\ncursor={}\nstate={}\nrecap_present={}\nrecap_excerpt={}\nsignals={}\nscreen_excerpt={}\nnext_safe_action={}\nbindings_path={}\nbindings_status={}\nbindings_missing={}{}",
        automation_ready,
        pane.pane_id,
        pane.session_name,
        pane.window_name,
        pane.pane_active,
        pane.current_command,
        is_pane_command_claude(&pane),
        pane.current_path,
        render_cursor(&pane),
        classification.state.as_str(),
        classification.recap_present,
        classification.recap_excerpt.as_deref().unwrap_or("none"),
        render_signals(&classification),
        render_screen_excerpt(&frame),
        render_next_safe_action(&classification, &pane, &bindings),
        bindings.path.display(),
        bindings.status.as_str(),
        render_missing_bindings(&bindings),
        render_doctor_recommendations(&pane, &bindings)
    ))
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

#[derive(Debug)]
struct ServeExportSummary {
    session_name: String,
    target_pane: Option<String>,
    reconcile_ms: u64,
    history_lines: usize,
    tape_path: PathBuf,
    started_at: String,
    ended_at: Option<String>,
    stopped_reason: Option<String>,
    total_events: usize,
    event_counts: BTreeMap<String, usize>,
    tracked_panes: BTreeSet<String>,
}

struct ServeArtifacts {
    tape_writer: BufWriter<File>,
    export_path: PathBuf,
    summary: ServeExportSummary,
}

impl ServeExportSummary {
    fn new(
        session_name: &str,
        target_pane: Option<&str>,
        reconcile_ms: u64,
        history_lines: usize,
        tape_path: PathBuf,
    ) -> Self {
        Self {
            session_name: session_name.to_string(),
            target_pane: target_pane.map(ToString::to_string),
            reconcile_ms,
            history_lines,
            tape_path,
            started_at: current_babysit_timestamp(),
            ended_at: None,
            stopped_reason: None,
            total_events: 0,
            event_counts: BTreeMap::new(),
            tracked_panes: BTreeSet::new(),
        }
    }

    fn record_event(&mut self, event: &ServeEvent) {
        self.total_events += 1;

        match event {
            ServeEvent::Started { tracked_panes, .. } => {
                *self
                    .event_counts
                    .entry(String::from("serve-start"))
                    .or_insert(0) += 1;
                self.tracked_panes.extend(tracked_panes.iter().cloned());
            }
            ServeEvent::Snapshot(snapshot) => {
                *self
                    .event_counts
                    .entry(String::from("snapshot"))
                    .or_insert(0) += 1;
                self.tracked_panes.insert(snapshot.pane.pane_id.clone());
            }
            ServeEvent::Notification(_) => {
                *self.event_counts.entry(String::from("notify")).or_insert(0) += 1;
            }
            ServeEvent::PaneRemoved { pane_id } => {
                *self
                    .event_counts
                    .entry(String::from("pane-removed"))
                    .or_insert(0) += 1;
                self.tracked_panes.remove(pane_id);
            }
            ServeEvent::Stopped { reason } => {
                *self
                    .event_counts
                    .entry(String::from("serve-stop"))
                    .or_insert(0) += 1;
                self.stopped_reason = Some(reason.clone());
            }
        }
    }

    fn finalize(&mut self) {
        self.ended_at = Some(current_babysit_timestamp());
    }

    fn as_json(&self) -> serde_json::Value {
        let event_counts = self.event_counts.clone();
        let tracked_panes = self.tracked_panes.iter().cloned().collect::<Vec<_>>();
        serde_json::json!({
            "command": "serve",
            "generated_at": self.started_at,
            "session_name": self.session_name,
            "target_pane": self.target_pane,
            "reconcile_ms": self.reconcile_ms,
            "history_lines": self.history_lines,
            "tape_path": self.tape_path.display().to_string(),
            "started_at": self.started_at,
            "ended_at": self.ended_at,
            "stopped_reason": self.stopped_reason,
            "total_events": self.total_events,
            "event_counts": event_counts,
            "tracked_panes": tracked_panes,
        })
    }
}

fn run_serve(args: ServeArgs) -> AppResult<String> {
    let client = TmuxClient::default();
    let artifacts_required = args.state_dir.is_some();
    let interrupted = Arc::new(AtomicBool::new(false));
    install_babysit_sigint_handler(Arc::clone(&interrupted))?;
    let (state_dir, artifact_warning) = resolve_artifact_state_dir(args.state_dir.as_deref())?;
    if let Some(warning) = artifact_warning.as_deref() {
        eprintln!("warning: serve artifacts unavailable: {warning}");
    }
    let mut artifact_root = state_dir;
    let mut artifacts: Option<ServeArtifacts> = None;
    let artifact_session_name = args.session_name.clone();
    let artifact_pane_id = args.pane_id.clone();

    let request = ServeRequest {
        session_name: args.session_name,
        target_pane: args.pane_id,
        history_lines: args.history_lines,
        reconcile_ms: args.reconcile_ms,
    };
    let format = args.format;
    let use_color = supports_babysit_color();

    let serve_result = run_serve_loop(&client, &request, interrupted, |event| {
        let payload = render_serve_event_payload(&request, &event);
        if artifacts.is_none() {
            if let Some(state_dir) = artifact_root.as_deref() {
                match open_serve_artifacts(
                    state_dir,
                    &artifact_session_name,
                    artifact_pane_id.as_deref(),
                    args.reconcile_ms,
                    args.history_lines,
                ) {
                    Ok(opened_artifacts) => {
                        artifacts = Some(opened_artifacts);
                    }
                    Err(error) if artifacts_required => return Err(error),
                    Err(error) => {
                        eprintln!("warning: serve artifacts unavailable: {error}");
                        artifact_root = None;
                    }
                }
            }
        }
        if let Some(active_artifacts) = artifacts.as_mut() {
            active_artifacts.summary.record_event(&event);
            if let Err(error) = append_jsonl_event(&mut active_artifacts.tape_writer, &payload) {
                if artifacts_required {
                    return Err(error);
                }
                eprintln!("warning: serve artifacts disabled after write failure: {error}");
                artifacts = None;
                artifact_root = None;
            }
        }
        emit_babysit_output(render_babysit_output(format.into(), payload, use_color))
    });

    if let Some(mut artifacts) = artifacts {
        if let Err(error) = &serve_result {
            if artifacts.summary.stopped_reason.is_none() {
                artifacts.summary.stopped_reason = Some(error.to_string());
            }
        }
        artifacts.summary.finalize();
        if let Err(error) = write_serve_summary_export(&artifacts.export_path, &artifacts.summary) {
            if artifacts_required && serve_result.is_ok() {
                return Err(error);
            }
            eprintln!("warning: serve summary export failed: {error}");
        }
    }

    serve_result?;

    Ok(String::new())
}

fn open_serve_artifacts(
    state_dir: &Path,
    session_name: &str,
    pane_id: Option<&str>,
    reconcile_ms: u64,
    history_lines: usize,
) -> AppResult<ServeArtifacts> {
    let artifact_id = new_artifact_id("serve", session_name, pane_id)?;
    let tape_path = tape_artifact_path(state_dir, &artifact_id, "events.jsonl")?;
    let export_path = export_artifact_path(state_dir, &artifact_id, "summary.json")?;
    let tape_file = File::create(&tape_path)?;

    Ok(ServeArtifacts {
        tape_writer: BufWriter::new(tape_file),
        export_path,
        summary: ServeExportSummary::new(
            session_name,
            pane_id,
            reconcile_ms,
            history_lines,
            tape_path,
        ),
    })
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

fn append_jsonl_event(writer: &mut BufWriter<File>, payload: &serde_json::Value) -> AppResult<()> {
    writeln!(writer, "{}", payload)?;
    writer.flush()?;
    Ok(())
}

fn write_serve_summary_export(path: &Path, summary: &ServeExportSummary) -> AppResult<()> {
    let content = serde_json::to_string_pretty(&summary.as_json())
        .map_err(|error| AppError::new(format!("failed to encode serve summary JSON: {error}")))?;
    fs::write(path, content)?;
    Ok(())
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

fn render_serve_event_payload(request: &ServeRequest, event: &ServeEvent) -> serde_json::Value {
    match event {
        ServeEvent::Started {
            session_name,
            target_pane,
            tracked_panes,
        } => serde_json::json!({
            "timestamp": current_babysit_timestamp(),
            "kind": "serve-start",
            "session": session_name,
            "target_pane": target_pane,
            "tracked_panes": tracked_panes,
            "summary": format!("Serve mode attached to session {session_name}"),
            "details": [
                format!("Target: {}", target_pane.as_deref().unwrap_or("all panes")),
                format!("Reconcile interval: {}ms", request.reconcile_ms),
                format!("History lines: {}", request.history_lines),
                format!(
                    "Tracked panes: {}",
                    if tracked_panes.is_empty() {
                        String::from("none yet")
                    } else {
                        tracked_panes.join(", ")
                    }
                )
            ]
        }),
        ServeEvent::Snapshot(snapshot) => render_serve_snapshot_event_payload(snapshot),
        ServeEvent::Notification(message) => serde_json::json!({
            "timestamp": current_babysit_timestamp(),
            "kind": "notify",
            "summary": format!("tmux notification: {message}"),
            "details": [format!("Session: {}", request.session_name)],
            "body_title": "Notification",
            "body_lines": [message]
        }),
        ServeEvent::PaneRemoved { pane_id } => serde_json::json!({
            "timestamp": current_babysit_timestamp(),
            "kind": "pane-removed",
            "pane_id": pane_id,
            "summary": format!("Observed pane {pane_id} disappeared"),
            "details": [format!("Session: {}", request.session_name)]
        }),
        ServeEvent::Stopped { reason } => serde_json::json!({
            "timestamp": current_babysit_timestamp(),
            "kind": "serve-stop",
            "summary": format!("Serve mode stopped for session {}", request.session_name),
            "details": [format!("Reason: {reason}")]
        }),
    }
}

fn render_serve_snapshot_event_payload(snapshot: &ServePaneSnapshot) -> serde_json::Value {
    serde_json::json!({
        "timestamp": current_babysit_timestamp(),
        "kind": "snapshot",
        "pane_id": snapshot.pane.pane_id,
        "session": snapshot.pane.session_name,
        "window": snapshot.pane.window_name,
        "command": snapshot.pane.current_command,
        "cwd": snapshot.pane.current_path,
        "state": snapshot.classification.state.as_str(),
        "signals": snapshot.classification.signals.clone(),
        "changed": snapshot.changed,
        "reason": snapshot.reason.as_str(),
        "summary": if snapshot.changed {
            serde_json::json!(format!(
                "Pane {} changed to {} ({})",
                snapshot.pane.pane_id,
                snapshot.classification.state.as_str(),
                snapshot.reason.as_str()
            ))
        } else {
            serde_json::json!(format!(
                "Pane {} reconciled at {} ({})",
                snapshot.pane.pane_id,
                snapshot.classification.state.as_str(),
                snapshot.reason.as_str()
            ))
        },
        "details": [
            format!("Window: {}", snapshot.pane.window_name),
            format!("Command: {}", snapshot.pane.current_command),
            format!("Cwd: {}", snapshot.pane.current_path),
            format!("Signals: {}", render_signals(&snapshot.classification)),
            format!("Recap: {}", snapshot.classification.recap_excerpt.as_deref().unwrap_or("none")),
            format!("Changed: {}", snapshot.changed),
        ],
        "body_title": if snapshot.live_excerpt.is_empty() { serde_json::Value::Null } else { serde_json::json!("Recent output") },
        "body_lines": if snapshot.live_excerpt.is_empty() {
            serde_json::json!([])
        } else {
            serde_json::json!(serve_body_lines(&snapshot.live_excerpt))
        }
    })
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
    ensure_pane_owned_by_claude(&pane)?;
    let classification = classify_pane(&client, &pane.pane_id, ACTION_GUARD_HISTORY_LINES)?;
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
                    if is_yolo_safe_to_approve(&inspected) {
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
                YoloAction::Wait => {}
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
        SessionState::PermissionDialog | SessionState::FolderTrustPrompt
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
        | SessionState::PermissionDialog
        | SessionState::FolderTrustPrompt
        | SessionState::SurveyPrompt
        | SessionState::DiffDialog => return true,
        SessionState::ExternalEditorActive | SessionState::Unknown => return false,
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
    if let Some(anchor) = prompt_anchor.filter(|anchor| !anchor.trim().is_empty()) {
        if let Some(prompt_idx) = lines[..token_idx]
            .iter()
            .rposition(|line| line.contains(anchor))
        {
            return prompt_idx + 1;
        }
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
    let mut classification = classify_pane(&client, &pane.pane_id, ACTION_GUARD_HISTORY_LINES)?;
    if let Some(workflow) = submit_prompt_preflight_workflow(&classification) {
        execute_classified_workflow(&client, &pane.pane_id, workflow, &classification)?;
        thread::sleep(Duration::from_millis(AUTO_UNSTICK_STEP_DELAY_MS));
        let after = classify_pane(&client, &pane.pane_id, ACTION_GUARD_HISTORY_LINES)?;
        ensure_state_transition(&classification, &after, workflow.as_str())?;
        classification = after;
    }
    ensure_workflow_state(GuardedWorkflow::SubmitPrompt, &classification)?;

    let state_dir = resolve_bootstrapped_state_dir(args.state_dir.as_deref())?;
    let workspace = resolve_workspace_for_pane(&state_dir, &pane, args.workspace.as_deref())?;
    let prompt_text = read_prompt_input(args.source.as_deref(), args.text.as_deref())?;
    store_pending_prompt_for_tmux_instance(
        &state_dir,
        &workspace.id,
        &args.session_name,
        &pane,
        &prompt_text,
    )?;

    let bindings = load_automation_keybindings(None)?;
    let before_submit = client.capture_pane(&pane.pane_id, KEEP_GOING_HISTORY_LINES)?;

    send_actions(
        &client,
        &pane.pane_id,
        &prompt_submission_sequence(),
        &bindings,
        args.submit_delay_ms,
    )?;
    wait_for_prompt_submission_start(&client, &pane.pane_id, &before_submit, None).map_err(|_| {
        AppError::new(format!(
            "submit-prompt sent the prepared prompt but pane {} did not show a prompt-submission transition",
            pane.pane_id
        ))
    })?;

    Ok(format!(
        "submitted prepared prompt session={} pane={} workspace={} state={} state_db={} delay_ms={}",
        args.session_name,
        pane.pane_id,
        workspace.id,
        classification.state.as_str(),
        state_db_path(&state_dir).display(),
        args.submit_delay_ms
    ))
}

fn run_yolo_start(args: YoloStartArgs) -> AppResult<String> {
    let client = TmuxClient::default();
    let state_dir = resolve_bootstrapped_state_dir(args.state_dir.as_deref())?;
    let workspace = match args.workspace.as_deref() {
        Some(selector) => Some(resolve_selected_workspace(&state_dir, Some(selector))?),
        None => None,
    };
    let interrupted = Arc::new(AtomicBool::new(false));
    install_babysit_sigint_handler(Arc::clone(&interrupted))?;
    if args.all {
        run_yolo_all(
            &client,
            &state_dir,
            workspace.as_ref(),
            args.poll_ms,
            args.live_preview,
            args.format,
            interrupted,
        )
    } else {
        let pane_id = args.pane_id.as_deref().unwrap();
        run_yolo_single(
            &client,
            &state_dir,
            args.workspace.as_deref(),
            pane_id,
            args.poll_ms,
            args.live_preview,
            args.format,
            interrupted,
        )
    }
}

fn run_yolo_stop(args: YoloStopArgs) -> AppResult<String> {
    let state_dir = resolve_bootstrapped_state_dir(args.state_dir.as_deref())?;
    let workspace = match args.workspace.as_deref() {
        Some(selector) => Some(resolve_selected_workspace(&state_dir, Some(selector))?),
        None => None,
    };
    if args.all {
        let mut out = Vec::new();
        for pane_id in
            tracked_pane_ids(&state_dir, workspace.as_ref().map(|item| item.id.as_str()))?
        {
            let pane_label = babysit_pane_label(&state_dir, &pane_id);
            let disabled = disable_yolo_record(&state_dir, &pane_id)?;
            out.push(render_babysit_stop_event(
                &pane_label,
                &pane_id,
                if disabled {
                    "disabled"
                } else {
                    "already-disabled"
                },
                BabysitFormat::Human,
                supports_babysit_color(),
            ));
        }
        Ok(out.join("\n"))
    } else {
        let raw_target = args.pane_id.as_deref().unwrap();
        let pane_id = match TmuxClient::default().pane_by_target(raw_target)? {
            Some(pane) => pane.pane_id,
            None => raw_target.to_string(),
        };
        let pane_label = babysit_pane_label(&state_dir, &pane_id);
        let disabled = disable_yolo_record(&state_dir, &pane_id)?;
        Ok(render_babysit_stop_event(
            &pane_label,
            &pane_id,
            if disabled {
                "disabled"
            } else {
                "already-disabled"
            },
            BabysitFormat::Human,
            supports_babysit_color(),
        ))
    }
}

fn resolve_bootstrapped_state_dir(path: Option<&Path>) -> AppResult<PathBuf> {
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

fn resolve_workspace_for_pane(
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

fn run_yolo_single(
    client: &TmuxClient,
    state_dir: &Path,
    workspace_selector: Option<&str>,
    pane_id: &str,
    poll_ms: u64,
    live_preview: bool,
    format: BabysitFormat,
    interrupted: Arc<AtomicBool>,
) -> AppResult<String> {
    let pane = resolve_pane_by_id(client, pane_id)?;
    let workspace = resolve_workspace_for_pane(state_dir, &pane, workspace_selector)?;
    if matches!(read_yolo_record(state_dir, &pane.pane_id)?, Some(record) if record.enabled) {
        return Err(AppError::new(format!(
            "yolo is already active for pane {}",
            pane.pane_id
        )));
    }
    ensure_pane_owned_by_claude(&pane)?;
    let bindings = load_automation_keybindings(None)?;
    let _ = keys_for_action(&bindings, AutomationAction::ConfirmYes)?;
    let record_path = write_yolo_record(state_dir, &workspace.id, &pane)?;
    let record = read_yolo_record(state_dir, &pane.pane_id)?.ok_or_else(|| {
        AppError::new(format!(
            "failed to reload yolo record for pane {}",
            pane.pane_id
        ))
    })?;
    let pane_label = pane_label_from_path(&record.current_path, &pane.pane_id);
    emit_babysit_output(render_babysit_start_event(
        &pane_label,
        &pane.pane_id,
        poll_ms,
        live_preview,
        &record,
        &record_path,
        format,
        supports_babysit_color(),
    ))?;
    loop_yolo_pane(
        client,
        state_dir,
        pane,
        record,
        pane_label,
        poll_ms,
        live_preview,
        format,
        interrupted,
    )
}

fn loop_yolo_pane(
    client: &TmuxClient,
    state_dir: &Path,
    pane: TmuxPane,
    record: YoloRecord,
    pane_label: String,
    poll_ms: u64,
    live_preview: bool,
    format: BabysitFormat,
    interrupted: Arc<AtomicBool>,
) -> AppResult<String> {
    let mut last_state: Option<SessionState> = None;
    let mut poll_count: usize = 0;
    loop {
        if interrupted.load(Ordering::SeqCst) {
            cleanup_babysit_record(state_dir, &pane.pane_id)?;
            emit_babysit_output(render_babysit_stop_event(
                &pane_label,
                &pane.pane_id,
                "sigint",
                format,
                supports_babysit_color(),
            ))?;
            return Ok(String::new());
        }
        poll_count += 1;
        if !yolo_record_enabled(state_dir, &pane.pane_id)? {
            emit_babysit_output(render_babysit_stop_event(
                &pane_label,
                &pane.pane_id,
                "disabled",
                format,
                supports_babysit_color(),
            ))?;
            return Ok(String::new());
        }
        let Some(current) = client.pane_by_id(&pane.pane_id)? else {
            cleanup_babysit_record(state_dir, &pane.pane_id)?;
            emit_babysit_output(render_babysit_stop_event(
                &pane_label,
                &pane.pane_id,
                "missing-pane",
                format,
                supports_babysit_color(),
            ))?;
            return Ok(String::new());
        };
        if !record.matches_pane(&current) {
            cleanup_babysit_record(state_dir, &pane.pane_id)?;
            emit_babysit_output(render_babysit_stop_event(
                &pane_label,
                &pane.pane_id,
                "identity-changed",
                format,
                supports_babysit_color(),
            ))?;
            return Ok(String::new());
        }
        let current_view = inspect_pane(client, &pane.pane_id, ACTION_GUARD_HISTORY_LINES)?;
        let classification = &current_view.classification;
        if last_state != Some(classification.state) {
            emit_babysit_output(render_babysit_wait_event(
                &pane_label,
                &pane.pane_id,
                poll_count,
                &current_view,
                live_preview,
                format,
                supports_babysit_color(),
            ))?;
            last_state = Some(classification.state);
        }
        match yolo_action_for_state(classification.state) {
            YoloAction::ApprovePermission => {
                if !is_yolo_safe_to_approve(&current_view) {
                    thread::sleep(Duration::from_millis(poll_ms));
                    continue;
                }
                emit_babysit_output(render_babysit_action_event(
                    &pane_label,
                    &pane.pane_id,
                    poll_count,
                    &current_view,
                    GuardedWorkflow::ApprovePermission,
                    live_preview,
                    format,
                    supports_babysit_color(),
                ))?;
                execute_classified_workflow(
                    client,
                    &pane.pane_id,
                    GuardedWorkflow::ApprovePermission,
                    classification,
                )?;
                thread::sleep(Duration::from_millis(poll_ms));
                let after = inspect_pane(client, &pane.pane_id, ACTION_GUARD_HISTORY_LINES)?;
                emit_babysit_output(render_babysit_action_completed_event(
                    &pane_label,
                    &pane.pane_id,
                    poll_count,
                    &after,
                    GuardedWorkflow::ApprovePermission,
                    live_preview,
                    format,
                    supports_babysit_color(),
                ))?;
                last_state = Some(after.classification.state);
            }
            YoloAction::DismissSurvey => {
                emit_babysit_output(render_babysit_action_event(
                    &pane_label,
                    &pane.pane_id,
                    poll_count,
                    &current_view,
                    GuardedWorkflow::DismissSurvey,
                    live_preview,
                    format,
                    supports_babysit_color(),
                ))?;
                execute_classified_workflow(
                    client,
                    &pane.pane_id,
                    GuardedWorkflow::DismissSurvey,
                    classification,
                )?;
                thread::sleep(Duration::from_millis(poll_ms));
                let after = inspect_pane(client, &pane.pane_id, ACTION_GUARD_HISTORY_LINES)?;
                emit_babysit_output(render_babysit_action_completed_event(
                    &pane_label,
                    &pane.pane_id,
                    poll_count,
                    &after,
                    GuardedWorkflow::DismissSurvey,
                    live_preview,
                    format,
                    supports_babysit_color(),
                ))?;
                last_state = Some(after.classification.state);
            }
            YoloAction::Wait => thread::sleep(Duration::from_millis(poll_ms)),
        }
    }
}

fn run_yolo_all(
    client: &TmuxClient,
    state_dir: &Path,
    workspace: Option<&WorkspaceRecord>,
    poll_ms: u64,
    live_preview: bool,
    format: BabysitFormat,
    interrupted: Arc<AtomicBool>,
) -> AppResult<String> {
    let mut tracked: HashMap<String, TrackedYoloPane> = HashMap::new();
    loop {
        if interrupted.load(Ordering::SeqCst) {
            cleanup_all_babysit_records(state_dir, tracked.keys())?;
            return Ok(String::new());
        }
        for pane in client
            .list_panes()?
            .into_iter()
            .filter(is_pane_command_claude)
        {
            let pane_workspace =
                resolve_workspace_for_path(state_dir, Path::new(&pane.current_path))?;
            if workspace.is_some_and(|workspace| workspace.id != pane_workspace.id) {
                continue;
            }
            if tracked.contains_key(&pane.pane_id) {
                continue;
            }
            let record_path = write_yolo_record(state_dir, &pane_workspace.id, &pane)?;
            let record = read_yolo_record(state_dir, &pane.pane_id)?.ok_or_else(|| {
                AppError::new(format!(
                    "failed to reload yolo record for pane {}",
                    pane.pane_id
                ))
            })?;
            let pane_label = pane_label_from_path(&record.current_path, &pane.pane_id);
            emit_babysit_output(render_babysit_start_event(
                &pane_label,
                &pane.pane_id,
                poll_ms,
                live_preview,
                &record,
                &record_path,
                format,
                supports_babysit_color(),
            ))?;
            tracked.insert(
                pane.pane_id.clone(),
                TrackedYoloPane {
                    record,
                    pane_label,
                    last_state: None,
                    poll_count: 0,
                },
            );
        }
        let ids = tracked.keys().cloned().collect::<Vec<_>>();
        for pane_id in ids {
            if interrupted.load(Ordering::SeqCst) {
                break;
            }
            let Some(tracked_pane) = tracked.get_mut(&pane_id) else {
                continue;
            };
            if !yolo_record_enabled(state_dir, &pane_id)? {
                tracked.remove(&pane_id);
                continue;
            }
            let Some(current) = client.pane_by_id(&pane_id)? else {
                cleanup_babysit_record(state_dir, &pane_id)?;
                emit_babysit_output(render_babysit_stop_event(
                    &tracked_pane.pane_label,
                    &pane_id,
                    "missing-pane",
                    format,
                    supports_babysit_color(),
                ))?;
                tracked.remove(&pane_id);
                continue;
            };
            if !tracked_pane.record.matches_pane(&current) {
                cleanup_babysit_record(state_dir, &pane_id)?;
                emit_babysit_output(render_babysit_stop_event(
                    &tracked_pane.pane_label,
                    &pane_id,
                    "identity-changed",
                    format,
                    supports_babysit_color(),
                ))?;
                tracked.remove(&pane_id);
                continue;
            }
            let current_view = inspect_pane(client, &pane_id, ACTION_GUARD_HISTORY_LINES)?;
            let classification = current_view.classification.state;
            tracked_pane.poll_count += 1;
            if tracked_pane.last_state != Some(classification) {
                emit_babysit_output(render_babysit_wait_event(
                    &tracked_pane.pane_label,
                    &pane_id,
                    tracked_pane.poll_count,
                    &current_view,
                    live_preview,
                    format,
                    supports_babysit_color(),
                ))?;
                tracked_pane.last_state = Some(classification);
            }
            match yolo_action_for_state(classification) {
                YoloAction::ApprovePermission => {
                    if !is_yolo_safe_to_approve(&current_view) {
                        continue;
                    }
                    emit_babysit_output(render_babysit_action_event(
                        &tracked_pane.pane_label,
                        &pane_id,
                        tracked_pane.poll_count,
                        &current_view,
                        GuardedWorkflow::ApprovePermission,
                        live_preview,
                        format,
                        supports_babysit_color(),
                    ))?;
                    execute_classified_workflow(
                        client,
                        &pane_id,
                        GuardedWorkflow::ApprovePermission,
                        &current_view.classification,
                    )?;
                    thread::sleep(Duration::from_millis(poll_ms));
                    let after = inspect_pane(client, &pane_id, ACTION_GUARD_HISTORY_LINES)?;
                    emit_babysit_output(render_babysit_action_completed_event(
                        &tracked_pane.pane_label,
                        &pane_id,
                        tracked_pane.poll_count,
                        &after,
                        GuardedWorkflow::ApprovePermission,
                        live_preview,
                        format,
                        supports_babysit_color(),
                    ))?;
                    tracked_pane.last_state = Some(after.classification.state);
                }
                YoloAction::DismissSurvey => {
                    emit_babysit_output(render_babysit_action_event(
                        &tracked_pane.pane_label,
                        &pane_id,
                        tracked_pane.poll_count,
                        &current_view,
                        GuardedWorkflow::DismissSurvey,
                        live_preview,
                        format,
                        supports_babysit_color(),
                    ))?;
                    execute_classified_workflow(
                        client,
                        &pane_id,
                        GuardedWorkflow::DismissSurvey,
                        &current_view.classification,
                    )?;
                    thread::sleep(Duration::from_millis(poll_ms));
                    let after = inspect_pane(client, &pane_id, ACTION_GUARD_HISTORY_LINES)?;
                    emit_babysit_output(render_babysit_action_completed_event(
                        &tracked_pane.pane_label,
                        &pane_id,
                        tracked_pane.poll_count,
                        &after,
                        GuardedWorkflow::DismissSurvey,
                        live_preview,
                        format,
                        supports_babysit_color(),
                    ))?;
                    tracked_pane.last_state = Some(after.classification.state);
                }
                YoloAction::Wait => thread::sleep(Duration::from_millis(poll_ms)),
            }
        }
        thread::sleep(Duration::from_millis(poll_ms));
    }
}

struct TrackedYoloPane {
    record: YoloRecord,
    pane_label: String,
    last_state: Option<SessionState>,
    poll_count: usize,
}

fn yolo_record_enabled(state_dir: &Path, pane_id: &str) -> AppResult<bool> {
    Ok(matches!(read_yolo_record(state_dir, pane_id)?, Some(record) if record.enabled))
}

fn tracked_pane_ids(state_dir: &Path, workspace_id: Option<&str>) -> AppResult<Vec<String>> {
    list_yolo_pane_ids(state_dir, workspace_id)
}

fn cleanup_all_babysit_records<'a, I: IntoIterator<Item = &'a String>>(
    state_dir: &Path,
    pane_ids: I,
) -> AppResult<()> {
    for pane_id in pane_ids {
        cleanup_babysit_record(state_dir, pane_id)?;
    }
    Ok(())
}

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
enum YoloAction {
    Wait,
    ApprovePermission,
    DismissSurvey,
}

fn yolo_action_for_state(state: SessionState) -> YoloAction {
    match state {
        SessionState::ChatReady
        | SessionState::BusyResponding
        | SessionState::FolderTrustPrompt
        | SessionState::ExternalEditorActive
        | SessionState::DiffDialog
        | SessionState::Unknown => YoloAction::Wait,
        SessionState::PermissionDialog => YoloAction::ApprovePermission,
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
    io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none()
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

fn babysit_pane_label(state_dir: &std::path::Path, pane_id: &str) -> String {
    read_yolo_record(state_dir, pane_id)
        .ok()
        .flatten()
        .map(|record| pane_label_from_path(&record.current_path, pane_id))
        .unwrap_or_else(|| pane_id.to_string())
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

fn render_babysit_stop_event(
    pane_label: &str,
    pane_id: &str,
    reason: &str,
    format: BabysitFormat,
    use_color: bool,
) -> String {
    render_babysit_output(
        format.into(),
        serde_json::json!({
            "timestamp": current_babysit_timestamp(),
            "kind": "stop",
            "pane_label": pane_label,
            "pane_id": pane_id,
            "reason": reason,
            "summary": format!("YOLO stopped for {pane_label} (id:{pane_id})"),
            "details": [format!("Reason: {reason}")]
        }),
        use_color,
    )
}

fn serve_body_lines(input: &str) -> Vec<String> {
    input
        .lines()
        .rev()
        .take(8)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .map(|line| line.to_string())
        .collect()
}

#[derive(Debug, Clone)]
struct InspectedPane {
    classification: Classification,
    focused_source: String,
    raw_source: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PermissionPromptDetails {
    prompt_type: String,
    sandbox_mode: Option<String>,
    command: Option<String>,
    reason: Option<String>,
    question: Option<String>,
}

fn inspect_pane(
    client: &TmuxClient,
    pane_id: &str,
    history_lines: usize,
) -> AppResult<InspectedPane> {
    let frame = client.capture_pane(pane_id, history_lines)?;
    let focused_source = focused_frame_source(&frame);
    Ok(InspectedPane {
        classification: Classifier.classify(pane_id, &focused_source),
        focused_source,
        raw_source: frame,
    })
}

fn extract_permission_prompt_details(inspected: &InspectedPane) -> Option<PermissionPromptDetails> {
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
        content_lines.push(line.to_string());
    }

    if question.is_none() {
        return None;
    }

    let mut content_lines = content_lines;
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
    if let Some((label, suffix)) = title.rsplit_once(" (") {
        if let Some(mode) = suffix.strip_suffix(')') {
            if matches!(mode, "sandboxed" | "unsandboxed") {
                return (label.to_string(), Some(mode.to_string()));
            }
        }
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
    lower.starts_with("❯")
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

fn is_permission_annotation_line(line: &str) -> bool {
    matches!(
        line.trim(),
        "Contains simple_expansion"
            | "Contains command substitution"
            | "Contains variable expansion"
            | "Contains globbing"
    )
}

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

fn is_yolo_safe_to_approve(inspected: &InspectedPane) -> bool {
    if inspected.classification.state != SessionState::PermissionDialog {
        return false;
    }

    if permission_manual_review_reason(&inspected.classification).is_some() {
        return false;
    }

    let Some(details) = extract_permission_prompt_details(inspected) else {
        return false;
    };

    details.question.is_some()
        && details.command.is_some()
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
            .any(is_chat_input_line_for_yolo)
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
    if trimmed == ">" || trimmed == "❯" {
        return true;
    }

    let Some(rest) = trimmed.strip_prefix('❯') else {
        return false;
    };
    let rest = rest.trim();
    !rest.is_empty() && !starts_with_numbered_option_for_yolo(rest)
}

fn starts_with_numbered_option_for_yolo(line: &str) -> bool {
    let digits = line.chars().take_while(|ch| ch.is_ascii_digit()).count();
    digits > 0 && line[digits..].starts_with('.')
}

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

fn render_babysit_action_completed_event(
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
    let _live_preview = live_preview;
    let body_title: Option<&str> = None;
    let body_lines: Vec<String> = Vec::new();
    let json_details = serde_json::json!([format!("Signals: {}", render_signals(classification))]);
    render_babysit_output(
        format.into(),
        serde_json::json!({
            "timestamp": current_babysit_timestamp(),
            "kind": format!("{}d", workflow.as_str()),
            "pane_label": pane_label,
            "pane_id": pane_id,
            "poll": poll_count,
            "state": classification.state.as_str(),
            "signals": classification.signals.clone(),
            "summary": format!(
                "{} for {pane_label} (id:{pane_id}) (poll #{poll_count})",
                match workflow {
                    GuardedWorkflow::ApprovePermission => "Approval sent",
                    GuardedWorkflow::DismissSurvey => "Survey dismissed",
                    GuardedWorkflow::RejectPermission => "Rejection sent",
                    GuardedWorkflow::SubmitPrompt => "Prompt submitted",
                }
            ),
            "details": if format == BabysitFormat::Human { serde_json::json!([]) } else { json_details },
            "body_title": body_title,
            "body_lines": body_lines
        }),
        use_color,
    )
}

fn classify_pane(
    client: &TmuxClient,
    pane_id: &str,
    history_lines: usize,
) -> AppResult<Classification> {
    let frame = client.capture_pane(pane_id, history_lines)?;
    let focused = focused_frame_source(&frame);
    Ok(Classifier.classify(pane_id, &focused))
}

pub(crate) fn focused_frame_source(frame: &str) -> String {
    let focused = focus_live_frame(frame, 15);
    if focused.is_empty() {
        frame.to_string()
    } else {
        focused
    }
}

fn ensure_workflow_state(
    workflow: GuardedWorkflow,
    classification: &Classification,
) -> AppResult<()> {
    validate_workflow_state(workflow, classification).map_err(AppError::new)
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
struct ContinueOutcome {
    action: String,
    outcome: String,
    after: Classification,
    used_permission_approval: bool,
}

fn continue_from_classification(
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
        SessionState::PermissionDialog => Ok(Some(RecoveryAction::ApprovePermission)),
        SessionState::SurveyPrompt => Ok(Some(RecoveryAction::DismissSurvey)),
        SessionState::FolderTrustPrompt => Ok(Some(RecoveryAction::ConfirmFolderTrust)),
        SessionState::DiffDialog => Err(AppError::new(
            "no safe automatic recovery is defined for DiffDialog; review the pane manually",
        )),
        SessionState::ExternalEditorActive => Err(AppError::new(
            "no safe automatic recovery is defined while the external editor is active",
        )),
        SessionState::Unknown => Err(AppError::new(
            "no safe automatic recovery is defined for Unknown state; refusing to guess",
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

    let start = lines.len().saturating_sub(max_non_empty_lines);
    lines[start..].join("\n")
}

fn raw_key_for_workflow(
    workflow: GuardedWorkflow,
    classification: &Classification,
) -> Option<(&'static str, &'static str)> {
    match (workflow, classification.state) {
        (GuardedWorkflow::ApprovePermission, SessionState::FolderTrustPrompt) => {
            Some(("Enter", "enter"))
        }
        (GuardedWorkflow::DismissSurvey, SessionState::SurveyPrompt) => Some(("0", "0")),
        _ => None,
    }
}

fn execute_classified_workflow(
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

fn load_automation_keybindings(path: Option<&Path>) -> AppResult<ResolvedKeybindings> {
    load_resolved_keybindings(path).map_err(AppError::new)
}

fn keys_for_action<'a>(
    bindings: &'a ResolvedKeybindings,
    action: AutomationAction,
) -> AppResult<&'a [String]> {
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
        "pane={}\nsession={}\nwindow={}\nactive={}\ncommand={}\ncwd={}\ncursor={}\nstate={}\nrecap_present={}\nrecap_excerpt={}\nsignals={}\nscreen_excerpt={}\nnext_safe_action={}\nbindings_path={}\nbindings_status={}\nbindings_missing={}",
        pane.pane_id,
        pane.session_name,
        pane.window_name,
        pane.pane_active,
        pane.current_command,
        pane.current_path,
        render_cursor(pane),
        classification.state.as_str(),
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

fn render_screen_excerpt(frame: &str) -> String {
    let excerpt = focus_live_frame(frame, 8);
    if excerpt.is_empty() {
        String::from("none")
    } else {
        excerpt.replace('\n', " | ")
    }
}

fn render_next_safe_action(
    classification: &Classification,
    pane: &TmuxPane,
    bindings: &KeybindingsInspection,
) -> String {
    if !is_pane_command_claude(pane) {
        return format!("manual-review: pane is running {}", pane.current_command);
    }
    if bindings.status != KeybindingsStatus::Valid {
        return String::from("manual-review: fix Claude keybindings before automation");
    }
    match classification.state {
        SessionState::ChatReady => String::from("safe-action: submit-prompt"),
        SessionState::BusyResponding => String::from("safe-action: wait"),
        SessionState::PermissionDialog => {
            if let Some(reason) = permission_manual_review_reason(classification) {
                format!("manual-review: {reason}")
            } else {
                String::from("safe-action: approve")
            }
        }
        SessionState::SurveyPrompt => String::from("safe-action: dismiss-survey (0)"),
        SessionState::FolderTrustPrompt => String::from("safe-action: approve (Enter)"),
        SessionState::DiffDialog => {
            String::from("manual-review: diff dialog needs operator confirmation")
        }
        SessionState::ExternalEditorActive => {
            String::from("manual-review: external editor is active")
        }
        SessionState::Unknown => String::from("manual-review: classify the pane before acting"),
    }
}

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

fn ensure_pane_owned_by_claude(pane: &TmuxPane) -> AppResult<()> {
    if is_pane_command_claude(pane) {
        Ok(())
    } else {
        Err(AppError::new(format!(
            "refusing to drive pane {} because current command is {} instead of claude",
            pane.pane_id, pane.current_command
        )))
    }
}

fn render_doctor_recommendations(pane: &TmuxPane, bindings: &KeybindingsInspection) -> String {
    let mut lines = Vec::new();

    if !is_pane_command_claude(pane) {
        lines.push(format!(
            "recommendation=target pane is running {} instead of claude",
            pane.current_command
        ));
    }

    if bindings.status != KeybindingsStatus::Valid {
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
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::{
        AppError, BabysitFormat, BabysitOutputFormat, InspectedPane,
        KEEP_GOING_CUSTOM_PROMPT_ANCHOR, KEEP_GOING_PROMPT_ANCHOR, KeepGoingDirective,
        ObserveArtifactPaths, RecoveryAction, YoloAction, artifact_state_dir_result,
        cleanup_babysit_record, ensure_pane_owned_by_claude, ensure_state_transition,
        extract_keep_going_response, extract_permission_prompt_details, is_usable_state,
        is_yolo_safe_to_approve, keep_going_no_yolo_blocker, yolo_action_for_state,
        permission_manual_review_reason, prompt_submission_started, raw_key_for_workflow,
        recovery_action_for_state, render_babysit_action_event, render_babysit_output,
        render_babysit_start_event, render_babysit_wait_event, render_guarded_workflow_output,
        render_keep_going_wait_message, render_list_panes, render_next_safe_action,
        render_observe_command_output, render_screen_excerpt, render_serve_event_payload,
        render_status_report, resolve_keep_going_prompt, run_prepare_prompt,
        submit_prompt_preflight_workflow,
    };
    use crate::automation::{GuardedWorkflow, KeybindingsInspection, KeybindingsStatus};
    use crate::classifier::{
        Classification, SIGNAL_SELF_SETTINGS_LANGUAGE, SIGNAL_SENSITIVE_CLAUDE_PATH, SessionState,
    };
    use crate::cli::PreparePromptArgs;
    use crate::yolo::{YoloRecord, read_yolo_record, write_yolo_record};
    use crate::prompt::pending_prompt_text;
    use crate::serve::{ServeEvent, ServeRequest};
    use crate::storage::{CURRENT_SCHEMA_VERSION, resolve_workspace_for_path, state_db_path};
    use crate::tmux::TmuxPane;

    fn sample_pane(current_command: &str) -> TmuxPane {
        TmuxPane {
            pane_id: String::from("%1"),
            pane_tty: String::from("/dev/pts/1"),
            pane_pid: Some(123),
            session_id: String::from("$1"),
            session_name: String::from("demo"),
            window_id: String::from("@2"),
            window_name: String::from("claude"),
            current_command: current_command.to_string(),
            current_path: String::from("/tmp/demo"),
            pane_active: true,
            cursor_x: Some(0),
            cursor_y: Some(0),
        }
    }

    fn sample_bindings(status: KeybindingsStatus) -> KeybindingsInspection {
        KeybindingsInspection {
            path: std::path::PathBuf::from("/tmp/keybindings.json"),
            status,
            missing_bindings: vec![],
        }
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
    fn serve_pane_removed_events_keep_their_own_kind() {
        let payload = render_serve_event_payload(
            &ServeRequest {
                session_name: String::from("demo"),
                target_pane: None,
                history_lines: 120,
                reconcile_ms: 1500,
            },
            &ServeEvent::PaneRemoved {
                pane_id: String::from("%7"),
            },
        );

        assert_eq!(payload["kind"], "pane-removed");
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
                    recap_present: false,
                    recap_excerpt: None,
                    ..classification.clone()
                },
                &sample_pane("claude"),
                &sample_bindings(KeybindingsStatus::Valid)
            ),
            "safe-action: dismiss-survey (0)"
        );
    }

    #[test]
    fn uses_raw_keys_for_special_case_workflows() {
        let folder_trust = Classification {
            source: String::from("pane"),
            state: SessionState::FolderTrustPrompt,
            recap_present: false,
            recap_excerpt: None,
            signals: vec![String::from("folder-trust-keywords")],
        };
        let survey = Classification {
            source: String::from("pane"),
            state: SessionState::SurveyPrompt,
            recap_present: false,
            recap_excerpt: None,
            signals: vec![String::from("survey-keywords")],
        };

        assert_eq!(
            raw_key_for_workflow(GuardedWorkflow::ApprovePermission, &folder_trust),
            Some(("Enter", "enter"))
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
    fn list_panes_defaults_to_claude_only_output() {
        let output = render_list_panes(&[sample_pane("claude"), sample_pane("bash")], false);
        assert!(output.contains("command=claude"));
        assert!(!output.contains("command=bash"));
    }

    #[test]
    fn list_panes_all_flag_includes_non_claude_output() {
        let output = render_list_panes(&[sample_pane("claude"), sample_pane("bash")], true);
        assert!(output.contains("command=claude"));
        assert!(output.contains("command=bash"));
    }

    #[test]
    fn list_panes_claude_only_reports_empty_state() {
        let output = render_list_panes(&[sample_pane("bash")], false);
        assert_eq!(output, "no claude panes found");
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
            SessionState::FolderTrustPrompt,
            SessionState::ExternalEditorActive,
            SessionState::DiffDialog,
            SessionState::Unknown,
        ] {
            assert_eq!(
                yolo_action_for_state(state),
                YoloAction::Wait
            );
        }
    }

    #[test]
    fn submit_prompt_preflight_dismisses_surveys_only() {
        let survey = Classification {
            source: String::from("pane"),
            state: SessionState::SurveyPrompt,
            recap_present: false,
            recap_excerpt: None,
            signals: vec![String::from("survey-keywords")],
        };
        let ready = Classification {
            source: String::from("pane"),
            state: SessionState::ChatReady,
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
        assert!(is_yolo_safe_to_approve(&inspected));
    }

    #[test]
    fn extracts_permission_prompt_details_skips_preceding_chat_text() {
        let inspected = InspectedPane {
            classification: Classification {
                source: String::from("pane"),
                state: SessionState::PermissionDialog,
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
        assert!(is_yolo_safe_to_approve(&inspected));
    }

    #[test]
    fn extract_permission_prompt_details_refuses_bogus_titles() {
        let inspected = InspectedPane {
            classification: Classification {
                source: String::from("pane"),
                state: SessionState::PermissionDialog,
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

        assert!(!is_yolo_safe_to_approve(&inspected));
    }

    #[test]
    fn yolo_refuses_to_approve_self_settings_prompt_even_when_parseable() {
        let inspected = InspectedPane {
            classification: Classification {
                source: String::from("pane"),
                state: SessionState::PermissionDialog,
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

        assert!(!is_yolo_safe_to_approve(&inspected));
    }

    #[test]
    fn yolo_refuses_to_approve_sensitive_claude_path_prompt_even_when_parseable() {
        let inspected = InspectedPane {
            classification: Classification {
                source: String::from("pane"),
                state: SessionState::PermissionDialog,
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

        assert!(!is_yolo_safe_to_approve(&inspected));
    }

    #[test]
    fn yolo_allows_repo_claude_command_prompt_when_parseable() {
        let inspected = InspectedPane {
            classification: Classification {
                source: String::from("pane"),
                state: SessionState::PermissionDialog,
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

        assert!(is_yolo_safe_to_approve(&inspected));
    }

    #[test]
    fn yolo_wait_log_omits_live_preview_even_when_enabled() {
        let inspected = InspectedPane {
            classification: Classification {
                source: String::from("pane"),
                state: SessionState::PermissionDialog,
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
    fn yolo_approve_log_shows_only_permission_preview_in_human_mode() {
        let inspected = InspectedPane {
            classification: Classification {
                source: String::from("pane"),
                state: SessionState::PermissionDialog,
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
            window_name: String::from("claude"),
            current_command: String::from("claude"),
            current_path: workspace_root.display().to_string(),
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
}
