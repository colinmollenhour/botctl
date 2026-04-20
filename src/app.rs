use std::collections::HashMap;
use std::fmt;
use std::io::{self, IsTerminal, Write};
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
use crate::classifier::{Classification, Classifier, SessionState};
use crate::cli::{
    AttachArgs, AutoUnstickArgs, BabysitFormat, CaptureArgs, ClassifyArgs, Command,
    ContinueSessionArgs, DoctorArgs, EditorHelperArgs, InstallBindingsArgs, ListPanesArgs,
    ObserveArgs, PaneCommandArgs, PaneTargetArgs, PermissionBabysitStartArgs,
    PermissionBabysitStopArgs, PreparePromptArgs, RecordFixtureArgs, ReplayArgs, SendActionArgs,
    ServeArgs, StartArgs, StatusArgs, SubmitPromptArgs,
};
use crate::fixtures::{FixtureCase, FixtureRecordInput, record_case};
use crate::observe::{ObserveRequest, collect_observation, observe_session};
use crate::permission_babysit::{
    BabysitRecord, disable_babysit_record, read_babysit_record,
    resolve_state_dir as resolve_babysit_state_dir, write_babysit_record,
};
use crate::prompt::{
    PromptSource, default_state_dir, prepare_prompt, resolve_prompt_text, write_editor_target,
    write_editor_target_from_pending,
};
use crate::serve::{ServeEvent, ServePaneSnapshot, ServeRequest, run_serve_loop};
use crate::tmux::{StartSessionRequest, TmuxClient, TmuxPane};

const ACTION_GUARD_HISTORY_LINES: usize = 120;
const AUTO_UNSTICK_STEP_DELAY_MS: u64 = 150;

pub type AppResult<T> = Result<T, AppError>;

#[derive(Debug)]
pub struct AppError {
    message: String,
}

impl AppError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
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
        Command::PreparePrompt(args) => run_prepare_prompt(args),
        Command::EditorHelper(args) => run_editor_helper(args),
        Command::SubmitPrompt(args) => run_submit_prompt(args),
        Command::PermissionBabysitStart(args) => run_permission_babysit_start(args),
        Command::PermissionBabysitStop(args) => run_permission_babysit_stop(args),
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
    TmuxClient::default().capture_pane(&args.pane_id, args.history_lines)
}

fn run_status(args: StatusArgs) -> AppResult<String> {
    let client = TmuxClient::default();
    let pane = resolve_pane_by_id(&client, &args.pane_id)?;
    let frame = client.capture_pane(&args.pane_id, args.history_lines)?;
    let focused = focused_frame_source(&frame);
    let classification = Classifier.classify(&args.pane_id, &focused);
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
    let report = observe_session(&client, &request)?;
    Ok(report.render())
}

fn run_serve(args: ServeArgs) -> AppResult<String> {
    let client = TmuxClient::default();
    let interrupted = Arc::new(AtomicBool::new(false));
    install_babysit_sigint_handler(Arc::clone(&interrupted))?;
    let request = ServeRequest {
        session_name: args.session_name,
        target_pane: args.pane_id,
        history_lines: args.history_lines,
        reconcile_ms: args.reconcile_ms,
    };
    let format = args.format;
    let use_color = supports_babysit_color();

    run_serve_loop(&client, &request, interrupted, |event| {
        emit_babysit_output(render_serve_event(&request, &event, format, use_color))
    })?;

    Ok(String::new())
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
    let bindings = load_automation_keybindings(None)?;
    let classification = classify_pane(&client, &pane.pane_id, ACTION_GUARD_HISTORY_LINES)?;
    ensure_workflow_state(workflow, &classification)?;
    let executed_actions = if should_confirm_folder_trust_with_enter(workflow, &classification) {
        client.send_keys(&pane.pane_id, &["Enter"])?;
        String::from("enter")
    } else {
        send_actions(&client, &pane.pane_id, workflow.actions(), &bindings, 0)?;
        render_action_names(workflow.actions())
    };

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

fn run_prepare_prompt(args: PreparePromptArgs) -> AppResult<String> {
    let state_dir = resolve_state_dir(args.state_dir.as_deref());
    let prompt_text = read_prompt_input(args.source.as_deref(), args.text.as_deref())?;
    let pending_path = prepare_prompt(&state_dir, &args.session_name, &prompt_text)?;
    Ok(format!(
        "prepared prompt session={} path={}",
        args.session_name,
        pending_path.display()
    ))
}

fn run_editor_helper(args: EditorHelperArgs) -> AppResult<String> {
    let state_dir = resolve_state_dir(args.state_dir.as_deref());
    let content = match args.source.as_deref() {
        Some(source_path) => {
            let prompt_text = resolve_prompt_text(PromptSource::File(source_path))?;
            write_editor_target(&args.target, &prompt_text)?;
            prompt_text
        }
        None => write_editor_target_from_pending(
            &state_dir,
            &args.session_name,
            &args.target,
            !args.keep_pending,
        )?,
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
    let bindings = load_automation_keybindings(None)?;
    let classification = classify_pane(&client, &pane.pane_id, ACTION_GUARD_HISTORY_LINES)?;
    ensure_workflow_state(GuardedWorkflow::SubmitPrompt, &classification)?;

    let state_dir = resolve_state_dir(args.state_dir.as_deref());
    let prompt_text = read_prompt_input(args.source.as_deref(), args.text.as_deref())?;
    let pending_path = prepare_prompt(&state_dir, &args.session_name, &prompt_text)?;

    send_actions(
        &client,
        &pane.pane_id,
        &prompt_submission_sequence(),
        &bindings,
        args.submit_delay_ms,
    )?;

    Ok(format!(
        "submitted prepared prompt session={} pane={} state={} pending_path={} delay_ms={}",
        args.session_name,
        pane.pane_id,
        classification.state.as_str(),
        pending_path.display(),
        args.submit_delay_ms
    ))
}

fn run_permission_babysit_start(args: PermissionBabysitStartArgs) -> AppResult<String> {
    let client = TmuxClient::default();
    let state_dir = resolve_babysit_state_dir(args.state_dir.as_deref());
    let interrupted = Arc::new(AtomicBool::new(false));
    install_babysit_sigint_handler(Arc::clone(&interrupted))?;
    if args.all {
        run_yolo_all(
            &client,
            &state_dir,
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
            pane_id,
            args.poll_ms,
            args.live_preview,
            args.format,
            interrupted,
        )
    }
}

fn run_permission_babysit_stop(args: PermissionBabysitStopArgs) -> AppResult<String> {
    let state_dir = resolve_babysit_state_dir(args.state_dir.as_deref());
    if args.all {
        let mut out = Vec::new();
        for pane_id in tracked_pane_ids(&state_dir)? {
            let pane_label = babysit_pane_label(&state_dir, &pane_id);
            let disabled = disable_babysit_record(&state_dir, &pane_id)?;
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
        let pane_id = args.pane_id.as_deref().unwrap();
        let pane_label = babysit_pane_label(&state_dir, pane_id);
        let disabled = disable_babysit_record(&state_dir, pane_id)?;
        Ok(render_babysit_stop_event(
            &pane_label,
            pane_id,
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

fn run_yolo_single(
    client: &TmuxClient,
    state_dir: &Path,
    pane_id: &str,
    poll_ms: u64,
    live_preview: bool,
    format: BabysitFormat,
    interrupted: Arc<AtomicBool>,
) -> AppResult<String> {
    if matches!(read_babysit_record(state_dir, pane_id)?, Some(record) if record.enabled) {
        return Err(AppError::new(format!(
            "yolo is already active for pane {pane_id}"
        )));
    }
    let pane = resolve_pane_by_id(client, pane_id)?;
    ensure_pane_owned_by_claude(&pane)?;
    let bindings = load_automation_keybindings(None)?;
    let _ = keys_for_action(&bindings, AutomationAction::ConfirmYes)?;
    let record = BabysitRecord::from_pane(&pane);
    let pane_label = pane_label_from_path(&record.current_path, &pane.pane_id);
    let record_path = write_babysit_record(state_dir, &record)?;
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
        bindings,
    )
}

fn loop_yolo_pane(
    client: &TmuxClient,
    state_dir: &Path,
    pane: TmuxPane,
    record: BabysitRecord,
    pane_label: String,
    poll_ms: u64,
    live_preview: bool,
    format: BabysitFormat,
    interrupted: Arc<AtomicBool>,
    bindings: crate::automation::ResolvedKeybindings,
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
        match permission_babysit_action_for_state(classification.state) {
            PermissionBabysitAction::Approve => {
                if !is_yolo_safe_to_approve(&current_view) {
                    thread::sleep(Duration::from_millis(poll_ms));
                    continue;
                }
                emit_babysit_output(render_babysit_approval_event(
                    &pane_label,
                    &pane.pane_id,
                    poll_count,
                    &current_view,
                    live_preview,
                    format,
                    supports_babysit_color(),
                ))?;
                send_actions(
                    client,
                    &pane.pane_id,
                    GuardedWorkflow::ApprovePermission.actions(),
                    &bindings,
                    0,
                )?;
                thread::sleep(Duration::from_millis(poll_ms));
                let after = inspect_pane(client, &pane.pane_id, ACTION_GUARD_HISTORY_LINES)?;
                emit_babysit_output(render_babysit_approved_event(
                    &pane_label,
                    &pane.pane_id,
                    poll_count,
                    &after,
                    live_preview,
                    format,
                    supports_babysit_color(),
                ))?;
                last_state = Some(after.classification.state);
            }
            PermissionBabysitAction::Wait => thread::sleep(Duration::from_millis(poll_ms)),
        }
    }
}

fn run_yolo_all(
    client: &TmuxClient,
    state_dir: &Path,
    poll_ms: u64,
    live_preview: bool,
    format: BabysitFormat,
    interrupted: Arc<AtomicBool>,
) -> AppResult<String> {
    let bindings = load_automation_keybindings(None)?;
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
            if tracked.contains_key(&pane.pane_id) {
                continue;
            }
            let record = BabysitRecord::from_pane(&pane);
            let pane_label = pane_label_from_path(&record.current_path, &pane.pane_id);
            let record_path = write_babysit_record(state_dir, &record)?;
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
            match permission_babysit_action_for_state(classification) {
                PermissionBabysitAction::Approve => {
                    if !is_yolo_safe_to_approve(&current_view) {
                        continue;
                    }
                    emit_babysit_output(render_babysit_approval_event(
                        &tracked_pane.pane_label,
                        &pane_id,
                        tracked_pane.poll_count,
                        &current_view,
                        live_preview,
                        format,
                        supports_babysit_color(),
                    ))?;
                    send_actions(
                        client,
                        &pane_id,
                        GuardedWorkflow::ApprovePermission.actions(),
                        &bindings,
                        0,
                    )?;
                    thread::sleep(Duration::from_millis(poll_ms));
                    let after = inspect_pane(client, &pane_id, ACTION_GUARD_HISTORY_LINES)?;
                    emit_babysit_output(render_babysit_approved_event(
                        &tracked_pane.pane_label,
                        &pane_id,
                        tracked_pane.poll_count,
                        &after,
                        live_preview,
                        format,
                        supports_babysit_color(),
                    ))?;
                    tracked_pane.last_state = Some(after.classification.state);
                }
                PermissionBabysitAction::Wait => thread::sleep(Duration::from_millis(poll_ms)),
            }
        }
        thread::sleep(Duration::from_millis(poll_ms));
    }
}

struct TrackedYoloPane {
    record: BabysitRecord,
    pane_label: String,
    last_state: Option<SessionState>,
    poll_count: usize,
}

fn yolo_record_enabled(state_dir: &Path, pane_id: &str) -> AppResult<bool> {
    Ok(matches!(read_babysit_record(state_dir, pane_id)?, Some(record) if record.enabled))
}

fn tracked_pane_ids(state_dir: &Path) -> AppResult<Vec<String>> {
    let dir = state_dir.join("permission-babysit");
    let mut ids = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            if let Some(name) = entry.file_name().to_str() {
                ids.push(name.to_string());
            }
        }
    }
    Ok(ids)
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
    let _ = disable_babysit_record(state_dir, pane_id)?;
    Ok(())
}

fn install_babysit_sigint_handler(flag: Arc<AtomicBool>) -> AppResult<()> {
    ctrlc::set_handler(move || {
        flag.store(true, Ordering::SeqCst);
    })
    .map_err(|error| AppError::new(error.to_string()))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PermissionBabysitAction {
    Wait,
    Approve,
}

fn permission_babysit_action_for_state(state: SessionState) -> PermissionBabysitAction {
    match state {
        SessionState::ChatReady
        | SessionState::BusyResponding
        | SessionState::FolderTrustPrompt
        | SessionState::SurveyPrompt
        | SessionState::ExternalEditorActive
        | SessionState::DiffDialog
        | SessionState::Unknown => PermissionBabysitAction::Wait,
        SessionState::PermissionDialog => PermissionBabysitAction::Approve,
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
    read_babysit_record(state_dir, pane_id)
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
    record: &BabysitRecord,
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

fn render_serve_event(
    request: &ServeRequest,
    event: &ServeEvent,
    format: BabysitFormat,
    use_color: bool,
) -> String {
    match event {
        ServeEvent::Started {
            session_name,
            target_pane,
            tracked_panes,
        } => render_babysit_output(
            format.into(),
            serde_json::json!({
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
            use_color,
        ),
        ServeEvent::Snapshot(snapshot) => render_serve_snapshot_event(snapshot, format, use_color),
        ServeEvent::Notification(message) => render_babysit_output(
            format.into(),
            serde_json::json!({
                "timestamp": current_babysit_timestamp(),
                "kind": "notify",
                "summary": format!("tmux notification: {message}"),
                "details": [format!("Session: {}", request.session_name)],
                "body_title": "Notification",
                "body_lines": [message]
            }),
            use_color,
        ),
        ServeEvent::PaneRemoved { pane_id } => render_babysit_output(
            format.into(),
            serde_json::json!({
                "timestamp": current_babysit_timestamp(),
                "kind": "notify",
                "pane_id": pane_id,
                "summary": format!("Observed pane {pane_id} disappeared"),
                "details": [format!("Session: {}", request.session_name)]
            }),
            use_color,
        ),
        ServeEvent::Stopped { reason } => render_babysit_output(
            format.into(),
            serde_json::json!({
                "timestamp": current_babysit_timestamp(),
                "kind": "serve-stop",
                "summary": format!("Serve mode stopped for session {}", request.session_name),
                "details": [format!("Reason: {reason}")]
            }),
            use_color,
        ),
    }
}

fn render_serve_snapshot_event(
    snapshot: &ServePaneSnapshot,
    format: BabysitFormat,
    use_color: bool,
) -> String {
    render_babysit_output(
        format.into(),
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
    })
}

fn extract_permission_prompt_details(inspected: &InspectedPane) -> Option<PermissionPromptDetails> {
    if inspected.classification.state != SessionState::PermissionDialog {
        return None;
    }

    let lines = inspected.focused_source.lines().collect::<Vec<_>>();
    let question_idx = lines.iter().rposition(|line| is_permission_question_line(line.trim()))?;
    let title_idx = find_permission_prompt_title_idx(&lines[..question_idx])?;
    let title = lines[title_idx].trim();
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
    if let Some(aside) = content_lines.last().filter(|line| is_permission_annotation_line(line)) {
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

fn find_permission_prompt_title_idx(lines: &[&str]) -> Option<usize> {
    lines.iter().enumerate().rfind(|(_, line)| {
        let trimmed = line.trim();
        !trimmed.is_empty()
            && !is_permission_decoration_line(trimmed)
            && is_plausible_permission_prompt_title(trimmed)
    }).map(|(idx, _)| idx)
}

fn is_plausible_permission_prompt_title(title: &str) -> bool {
    let Some((base, suffix)) = title.rsplit_once(" (") else {
        return matches!(title, "Bash command" | "JavaScript code" | "TypeScript code" | "Python code" | "Rust code" | "SQL query" | "Shell command" | "Command" );
    };
    let Some(suffix) = suffix.strip_suffix(')') else { return false; };
    if matches!(suffix, "sandboxed" | "unsandboxed") {
        return is_plausible_permission_prompt_title(base);
    }
    matches!(suffix, "Contains simple_expansion" | "Contains command substitution" | "Contains variable expansion" | "Contains globbing") && is_plausible_permission_prompt_title(base)
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
        || lower == "no"
        || lower.starts_with("enter ")
        || lower.starts_with("escape ")
        || lower.starts_with("esc ")
        || lower.starts_with("tab ")
        || lower.starts_with("ctrl+")
}

fn is_permission_annotation_line(line: &str) -> bool {
    matches!(line.trim(),
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

    let Some(details) = extract_permission_prompt_details(inspected) else {
        return false;
    };

    details.question.is_some()
        && details.command.is_some()
        && !inspected
            .focused_source
            .lines()
            .map(str::trim)
            .any(is_chat_input_line_for_yolo)
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

fn render_babysit_approval_event(
    pane_label: &str,
    pane_id: &str,
    poll_count: usize,
    inspected: &InspectedPane,
    live_preview: bool,
    format: BabysitFormat,
    use_color: bool,
) -> String {
    let classification = &inspected.classification;
    let prompt_details = extract_permission_prompt_details(inspected);
    let json_details = serde_json::json!([format!("Signals: {}", render_signals(classification))]);
    let human_details: Vec<String> = Vec::new();
    let body_title = if live_preview {
        Some("Permission prompt")
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
            "kind": "approve",
            "pane_label": pane_label,
            "pane_id": pane_id,
            "poll": poll_count,
            "state": classification.state.as_str(),
            "signals": classification.signals.clone(),
            "prompt": babysit_prompt_json(prompt_details.as_ref()),
            "summary": format!("Auto-approving {pane_label} (id:{pane_id}) (poll #{poll_count})"),
            "details": if format == BabysitFormat::Human { serde_json::json!(human_details) } else { json_details },
            "body_title": body_title,
            "body_lines": body_lines
        }),
        use_color,
    )
}

fn render_babysit_approved_event(
    pane_label: &str,
    pane_id: &str,
    poll_count: usize,
    inspected: &InspectedPane,
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
            "kind": "approved",
            "pane_label": pane_label,
            "pane_id": pane_id,
            "poll": poll_count,
            "state": classification.state.as_str(),
            "signals": classification.signals.clone(),
            "summary": format!("Approval sent for {pane_label} (id:{pane_id}) (poll #{poll_count})"),
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
    match recovery_action_for_state(classification.state)? {
        Some(RecoveryAction::ApprovePermission) => {
            let bindings = load_automation_keybindings(None)?;
            send_actions(
                client,
                &pane.pane_id,
                GuardedWorkflow::ApprovePermission.actions(),
                &bindings,
                0,
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
            let bindings = load_automation_keybindings(None)?;
            send_actions(
                client,
                &pane.pane_id,
                GuardedWorkflow::DismissSurvey.actions(),
                &bindings,
                0,
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

fn should_confirm_folder_trust_with_enter(
    workflow: GuardedWorkflow,
    classification: &Classification,
) -> bool {
    workflow == GuardedWorkflow::ApprovePermission
        && classification.state == SessionState::FolderTrustPrompt
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
        .pane_by_id(pane_id)?
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
        SessionState::PermissionDialog => String::from("safe-action: approve"),
        SessionState::SurveyPrompt => String::from("safe-action: dismiss-survey"),
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
            "recommendation=add the missing automation actions to your Claude keybindings; use the bindings command to inspect the recommended mapping without overwriting your existing file",
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

fn resolve_state_dir(path: Option<&Path>) -> PathBuf {
    path.map(Path::to_path_buf)
        .unwrap_or_else(default_state_dir)
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

#[cfg(test)]
mod tests {
    use super::{
        BabysitFormat, InspectedPane, PermissionBabysitAction, RecoveryAction,
        cleanup_babysit_record, ensure_pane_owned_by_claude, ensure_state_transition,
        extract_permission_prompt_details, is_usable_state, is_yolo_safe_to_approve,
        permission_babysit_action_for_state, recovery_action_for_state,
        render_babysit_approval_event, render_babysit_start_event, render_babysit_wait_event,
        render_guarded_workflow_output, render_list_panes, render_next_safe_action,
        render_screen_excerpt, render_status_report,
    };
    use crate::automation::{GuardedWorkflow, KeybindingsInspection, KeybindingsStatus};
    use crate::classifier::Classification;
    use crate::classifier::SessionState;
    use crate::permission_babysit::{BabysitRecord, read_babysit_record, write_babysit_record};
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
            permission_babysit_action_for_state(SessionState::PermissionDialog),
            PermissionBabysitAction::Approve
        );
        assert_eq!(
            permission_babysit_action_for_state(SessionState::ChatReady),
            PermissionBabysitAction::Wait
        );
        assert_eq!(
            permission_babysit_action_for_state(SessionState::BusyResponding),
            PermissionBabysitAction::Wait
        );
        assert_eq!(
            permission_babysit_action_for_state(SessionState::FolderTrustPrompt),
            PermissionBabysitAction::Wait
        );
        assert_eq!(
            permission_babysit_action_for_state(SessionState::Unknown),
            PermissionBabysitAction::Wait
        );
    }

    #[test]
    fn permission_babysit_keeps_waiting_on_non_permission_blockers() {
        for state in [
            SessionState::FolderTrustPrompt,
            SessionState::SurveyPrompt,
            SessionState::ExternalEditorActive,
            SessionState::DiffDialog,
            SessionState::Unknown,
        ] {
            assert_eq!(
                permission_babysit_action_for_state(state),
                PermissionBabysitAction::Wait
            );
        }
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
        };

        let details = extract_permission_prompt_details(&inspected).expect("details should parse");
        assert_eq!(details.prompt_type, "Bash command (Contains simple_expansion)");
        assert_eq!(
            details.command.as_deref(),
            Some("GOCACHE=$TMPDIR/go-cache make manifests 2>&1 | tail-40")
        );
        assert_eq!(
            details.reason.as_deref(),
            Some("Regenerate manifests")
        );
        assert_eq!(details.question.as_deref(), Some("Do you want to proceed?"));
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
        };

        let details = extract_permission_prompt_details(&inspected).expect("details should parse");
        assert_eq!(details.prompt_type, "Bash command");
        assert_eq!(details.command.as_deref(), Some("mysql --execute=\"SELECT 1\""));
        assert_eq!(details.reason.as_deref(), Some("Run permission prompt extraction test"));
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
                "✔ Add PITR verification spec + binlog replay\n✔ Add SanityCheck spec + Checking phase\n✔ Write tests for Phase 2 changes\n✔ Regenerate manifests + RBAC mirror + docs\n◼ Run pre-PR gate\n────────────────────────────────────────────────────────────────────────\nBash command\n  export PATH=\"$(go env GOPATH)/bin:$PATH\" && GOCACHE=$TMPDIR/go-cache GOLANGCI_LINT_CACHE=$TMPDIR/golangci-cache make lint 2>&1 |\n  tail -40\n  Run golangci-lint\nContains simple_expansion\nDo you want to proceed?\n❯ 1. Yes\n  2. No\nEsc to cancel · Tab to amend · ctrl+e to explain",
            ),
        };

        let details = extract_permission_prompt_details(&inspected).expect("details should parse");
        assert_eq!(details.prompt_type, "Bash command (Contains simple_expansion)");
        assert_eq!(
            details.command.as_deref(),
            Some("export PATH=\"$(go env GOPATH)/bin:$PATH\" && GOCACHE=$TMPDIR/go-cache GOLANGCI_LINT_CACHE=$TMPDIR/golangci-cache make lint 2>&1 | tail -40")
        );
        assert_eq!(details.reason.as_deref(), Some("Run golangci-lint"));
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
        };

        assert!(!is_yolo_safe_to_approve(&inspected));
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
        };

        let log = render_babysit_approval_event(
            "bloodraven",
            "%20",
            3,
            &inspected,
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
    fn yolo_jsonl_renders_one_object_per_line() {
        let output = render_babysit_start_event(
            "bloodraven",
            "%20",
            500,
            false,
            &BabysitRecord {
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
        let _ = std::fs::remove_dir_all(&dir);
        let pane_id = "%99";
        let record = BabysitRecord {
            enabled: true,
            pane_id: pane_id.to_string(),
            pane_tty: String::from("/dev/pts/9"),
            pane_pid: Some(9),
            session_id: String::from("$9"),
            session_name: String::from("demo"),
            window_id: String::from("@9"),
            window_name: String::from("claude"),
            current_command: String::from("claude"),
            current_path: String::from("/tmp"),
        };
        write_babysit_record(&dir, &record).expect("write record");
        cleanup_babysit_record(&dir, pane_id).expect("cleanup record");
        let reread = read_babysit_record(&dir, pane_id)
            .expect("read record")
            .expect("record exists");
        assert!(!reread.enabled);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
