use std::fmt;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use crate::automation::{AutomationAction, prompt_submission_sequence, render_keybindings_json};
use crate::classifier::{Classifier, SessionState};
use crate::cli::{
    CaptureArgs, ClassifyArgs, Command, EditorHelperArgs, ObserveArgs, PreparePromptArgs,
    RecordFixtureArgs, ReplayArgs, SendActionArgs, StartArgs, SubmitPromptArgs,
};
use crate::fixtures::{FixtureCase, FixtureRecordInput, record_case};
use crate::observe::{ObserveRequest, collect_observation, observe_session};
use crate::prompt::{
    PromptSource, default_state_dir, prepare_prompt, resolve_prompt_text, write_editor_target,
    write_editor_target_from_pending,
};
use crate::tmux::{StartSessionRequest, TmuxClient};

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
        Command::ListPanes => run_list_panes(),
        Command::Capture(args) => run_capture(args),
        Command::Observe(args) => run_observe(args),
        Command::RecordFixture(args) => run_record_fixture(args),
        Command::Classify(args) => run_classify(args),
        Command::Replay(args) => run_replay(args),
        Command::Bindings => Ok(render_keybindings_json()),
        Command::SendAction(args) => run_send_action(args),
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
    TmuxClient::default().send_keys(&args.pane_id, action.tmux_keys())?;
    Ok(format!(
        "sent action={} pane={} keys={}",
        action.as_str(),
        args.pane_id,
        action.tmux_keys().join("+")
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
    let state_dir = resolve_state_dir(args.state_dir.as_deref());
    let prompt_text = read_prompt_input(args.source.as_deref(), args.text.as_deref())?;
    let pending_path = prepare_prompt(&state_dir, &args.session_name, &prompt_text)?;

    let client = TmuxClient::default();
    for action in prompt_submission_sequence() {
        client.send_keys(&args.pane_id, action.tmux_keys())?;
        if matches!(action, AutomationAction::ExternalEditor) {
            thread::sleep(Duration::from_millis(args.submit_delay_ms));
        }
    }

    Ok(format!(
        "submitted prepared prompt session={} pane={} pending_path={} delay_ms={}",
        args.session_name,
        args.pane_id,
        pending_path.display(),
        args.submit_delay_ms
    ))
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
