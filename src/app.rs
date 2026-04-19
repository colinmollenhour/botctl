use std::fmt;
use std::path::PathBuf;

use crate::automation::{AutomationAction, render_keybindings_json};
use crate::classifier::Classifier;
use crate::cli::{
    CaptureArgs, ClassifyArgs, Command, ObserveArgs, ReplayArgs, SendActionArgs, StartArgs,
};
use crate::fixtures::FixtureCase;
use crate::observe::{ObserveRequest, observe_session};
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
        Command::Classify(args) => run_classify(args),
        Command::Replay(args) => run_replay(args),
        Command::Bindings => Ok(render_keybindings_json()),
        Command::SendAction(args) => run_send_action(args),
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

fn load_frame_text(path: &PathBuf) -> AppResult<String> {
    std::fs::read_to_string(path).map_err(AppError::from)
}
