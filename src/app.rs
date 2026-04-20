use std::fmt;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use crate::automation::{
    AutomationAction, GuardedWorkflow, KeybindingsInspection, KeybindingsStatus,
    ResolvedKeybindings, inspect_keybindings, install_recommended_keybindings,
    load_resolved_keybindings, prompt_submission_sequence, render_keybindings_json,
    validate_workflow_state,
};
use crate::classifier::{Classification, Classifier, SessionState};
use crate::cli::{
    AttachArgs, CaptureArgs, ClassifyArgs, Command, DoctorArgs, EditorHelperArgs,
    InstallBindingsArgs, ObserveArgs, PaneCommandArgs, PaneTargetArgs, PreparePromptArgs,
    RecordFixtureArgs, ReplayArgs, SendActionArgs, StartArgs, StatusArgs, SubmitPromptArgs,
};
use crate::fixtures::{FixtureCase, FixtureRecordInput, record_case};
use crate::observe::{ObserveRequest, collect_observation, observe_session};
use crate::prompt::{
    PromptSource, default_state_dir, prepare_prompt, resolve_prompt_text, write_editor_target,
    write_editor_target_from_pending,
};
use crate::tmux::{StartSessionRequest, TmuxClient, TmuxPane};

const ACTION_GUARD_HISTORY_LINES: usize = 120;

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
        Command::ListPanes => run_list_panes(),
        Command::Capture(args) => run_capture(args),
        Command::Status(args) => run_status(args),
        Command::Doctor(args) => run_doctor(args),
        Command::Observe(args) => run_observe(args),
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
        Command::PreparePrompt(args) => run_prepare_prompt(args),
        Command::EditorHelper(args) => run_editor_helper(args),
        Command::SubmitPrompt(args) => run_submit_prompt(args),
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

fn run_list_panes() -> AppResult<String> {
    let panes = TmuxClient::default().list_panes()?;
    if panes.is_empty() {
        return Ok(String::from("no panes found"));
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
    Ok(out.trim_end().to_string())
}

fn run_capture(args: CaptureArgs) -> AppResult<String> {
    TmuxClient::default().capture_pane(&args.pane_id, args.history_lines)
}

fn run_status(args: StatusArgs) -> AppResult<String> {
    let client = TmuxClient::default();
    let pane = resolve_pane_by_id(&client, &args.pane_id)?;
    let classification = classify_pane(&client, &args.pane_id, args.history_lines)?;
    let bindings = inspect_keybindings(None).map_err(AppError::new)?;

    Ok(render_status_report(&pane, &classification, &bindings))
}

fn run_doctor(args: DoctorArgs) -> AppResult<String> {
    let client = TmuxClient::default();
    let pane = resolve_doctor_pane(
        &client,
        args.session_name.as_deref(),
        args.pane_id.as_deref(),
    )?;
    let classification = classify_pane(&client, &pane.pane_id, args.history_lines)?;
    let bindings = inspect_keybindings(args.bindings_path.as_deref()).map_err(AppError::new)?;
    let automation_ready =
        is_pane_command_claude(&pane) && bindings.status == KeybindingsStatus::Valid;

    Ok(format!(
        "automation_ready={}\npane={}\nsession={}\nwindow={}\nactive={}\ncommand={}\ncommand_matches_claude={}\ncwd={}\ncursor={}\nstate={}\nsignals={}\nbindings_path={}\nbindings_status={}\nbindings_missing={}{}",
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
        render_signals(&classification),
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
    let bindings = load_automation_keybindings(None)?;
    let keys = keys_for_action(&bindings, action)?;
    TmuxClient::default().send_keys(&args.pane_id, keys)?;
    Ok(format!(
        "sent action={} pane={} keys={}",
        action.as_str(),
        args.pane_id,
        keys.join("+")
    ))
}

fn run_guarded_pane_workflow(
    args: PaneCommandArgs,
    workflow: GuardedWorkflow,
) -> AppResult<String> {
    let client = TmuxClient::default();
    let bindings = load_automation_keybindings(None)?;
    let classification = classify_pane(&client, &args.pane_id, ACTION_GUARD_HISTORY_LINES)?;
    ensure_workflow_state(workflow, &classification)?;
    let executed_actions = if should_confirm_folder_trust_with_enter(workflow, &classification) {
        client.send_keys(&args.pane_id, &["Enter"])?;
        String::from("enter")
    } else {
        send_actions(&client, &args.pane_id, workflow.actions(), &bindings, 0)?;
        render_action_names(workflow.actions())
    };

    Ok(format!(
        "executed workflow={} pane={} state={} actions={}",
        workflow.as_str(),
        args.pane_id,
        classification.state.as_str(),
        executed_actions
    ))
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
    let bindings = load_automation_keybindings(None)?;
    let classification = classify_pane(&client, &args.pane_id, ACTION_GUARD_HISTORY_LINES)?;
    ensure_workflow_state(GuardedWorkflow::SubmitPrompt, &classification)?;

    let state_dir = resolve_state_dir(args.state_dir.as_deref());
    let prompt_text = read_prompt_input(args.source.as_deref(), args.text.as_deref())?;
    let pending_path = prepare_prompt(&state_dir, &args.session_name, &prompt_text)?;

    send_actions(
        &client,
        &args.pane_id,
        &prompt_submission_sequence(),
        &bindings,
        args.submit_delay_ms,
    )?;

    Ok(format!(
        "submitted prepared prompt session={} pane={} state={} pending_path={} delay_ms={}",
        args.session_name,
        args.pane_id,
        classification.state.as_str(),
        pending_path.display(),
        args.submit_delay_ms
    ))
}

fn classify_pane(
    client: &TmuxClient,
    pane_id: &str,
    history_lines: usize,
) -> AppResult<Classification> {
    let frame = client.capture_pane(pane_id, history_lines)?;
    let focused = focus_live_frame(&frame, 15);
    let frame_for_classification = if focused.is_empty() { &frame } else { &focused };
    Ok(Classifier.classify(pane_id, frame_for_classification))
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
            .ok_or_else(|| AppError::new(format!("no active pane found for session {session_name}"))),
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
) -> String {
    format!(
        "pane={}\nsession={}\nwindow={}\nactive={}\ncommand={}\ncwd={}\ncursor={}\nstate={}\nsignals={}\nbindings_path={}\nbindings_status={}\nbindings_missing={}",
        pane.pane_id,
        pane.session_name,
        pane.window_name,
        pane.pane_active,
        pane.current_command,
        pane.current_path,
        render_cursor(pane),
        classification.state.as_str(),
        render_signals(classification),
        bindings.path.display(),
        bindings.status.as_str(),
        render_missing_bindings(bindings)
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
