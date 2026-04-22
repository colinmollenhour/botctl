use std::path::PathBuf;

use crate::app::{AppError, AppResult};

#[derive(Debug, Clone)]
pub enum Command {
    Start(StartArgs),
    Attach(AttachArgs),
    ListPanes(ListPanesArgs),
    Capture(CaptureArgs),
    Status(StatusArgs),
    Doctor(DoctorArgs),
    Observe(ObserveArgs),
    Serve(ServeArgs),
    RecordFixture(RecordFixtureArgs),
    Classify(ClassifyArgs),
    Replay(ReplayArgs),
    Bindings,
    InstallBindings(InstallBindingsArgs),
    SendAction(SendActionArgs),
    ApprovePermission(PaneCommandArgs),
    RejectPermission(PaneCommandArgs),
    DismissSurvey(PaneCommandArgs),
    ContinueSession(ContinueSessionArgs),
    AutoUnstick(AutoUnstickArgs),
    KeepGoing(KeepGoingArgs),
    PreparePrompt(PreparePromptArgs),
    EditorHelper(EditorHelperArgs),
    SubmitPrompt(SubmitPromptArgs),
    PermissionBabysitStart(PermissionBabysitStartArgs),
    PermissionBabysitStop(PermissionBabysitStopArgs),
    Help,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BabysitFormat {
    Human,
    Jsonl,
}

#[derive(Debug, Clone)]
pub struct StartArgs {
    pub session_name: String,
    pub window_name: String,
    pub cwd: Option<PathBuf>,
    pub command: String,
    pub dry_run: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneTargetArgs {
    pub pane_id: Option<String>,
    pub session_name: Option<String>,
    pub window_name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachArgs {
    pub target: PaneTargetArgs,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContinueSessionArgs {
    pub target: PaneTargetArgs,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutoUnstickArgs {
    pub target: PaneTargetArgs,
    pub max_steps: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeepGoingArgs {
    pub target: PaneTargetArgs,
    pub no_yolo: bool,
    pub poll_ms: u64,
    pub submit_delay_ms: u64,
    pub state_dir: Option<PathBuf>,
    pub prompt_source: Option<PathBuf>,
    pub prompt_text: Option<String>,
}

#[derive(Debug, Clone)]
pub struct CaptureArgs {
    pub pane_id: String,
    pub history_lines: usize,
}

#[derive(Debug, Clone)]
pub struct ListPanesArgs {
    pub all: bool,
}

#[derive(Debug, Clone)]
pub struct StatusArgs {
    pub pane_id: String,
    pub history_lines: usize,
}

#[derive(Debug, Clone)]
pub struct DoctorArgs {
    pub session_name: Option<String>,
    pub pane_id: Option<String>,
    pub history_lines: usize,
    pub bindings_path: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct ObserveArgs {
    pub session_name: String,
    pub pane_id: Option<String>,
    pub events: usize,
    pub idle_timeout_ms: u64,
    pub history_lines: usize,
    pub state_dir: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct ServeArgs {
    pub session_name: String,
    pub pane_id: Option<String>,
    pub reconcile_ms: u64,
    pub history_lines: usize,
    pub format: BabysitFormat,
    pub state_dir: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct RecordFixtureArgs {
    pub session_name: String,
    pub pane_id: Option<String>,
    pub case_name: String,
    pub output_dir: PathBuf,
    pub expected_state: Option<String>,
    pub events: usize,
    pub idle_timeout_ms: u64,
    pub history_lines: usize,
}

#[derive(Debug, Clone)]
pub struct ClassifyArgs {
    pub path: PathBuf,
}

#[derive(Debug, Clone)]
pub struct ReplayArgs {
    pub path: PathBuf,
}

#[derive(Debug, Clone)]
pub struct SendActionArgs {
    pub pane_id: String,
    pub action: String,
}

#[derive(Debug, Clone)]
pub struct PaneCommandArgs {
    pub pane_id: String,
    pub format: BabysitFormat,
}

#[derive(Debug, Clone)]
pub struct InstallBindingsArgs {
    pub path: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct PreparePromptArgs {
    pub session_name: String,
    pub state_dir: Option<PathBuf>,
    pub workspace: Option<String>,
    pub source: Option<PathBuf>,
    pub text: Option<String>,
}

#[derive(Debug, Clone)]
pub struct EditorHelperArgs {
    pub session_name: String,
    pub state_dir: Option<PathBuf>,
    pub workspace: Option<String>,
    pub source: Option<PathBuf>,
    pub target: PathBuf,
    pub keep_pending: bool,
}

#[derive(Debug, Clone)]
pub struct SubmitPromptArgs {
    pub session_name: String,
    pub pane_id: String,
    pub state_dir: Option<PathBuf>,
    pub workspace: Option<String>,
    pub source: Option<PathBuf>,
    pub text: Option<String>,
    pub submit_delay_ms: u64,
}

#[derive(Debug, Clone)]
pub struct PermissionBabysitStartArgs {
    pub pane_id: Option<String>,
    pub all: bool,
    pub poll_ms: u64,
    pub live_preview: bool,
    pub format: BabysitFormat,
    pub state_dir: Option<PathBuf>,
    pub workspace: Option<String>,
}

#[derive(Debug, Clone)]
pub struct PermissionBabysitStopArgs {
    pub pane_id: Option<String>,
    pub all: bool,
    pub state_dir: Option<PathBuf>,
    pub workspace: Option<String>,
}

pub fn parse_args<I>(args: I) -> AppResult<Command>
where
    I: IntoIterator<Item = String>,
{
    let mut iter = args.into_iter();
    let _program = iter.next();
    let Some(subcommand) = iter.next() else {
        return Ok(Command::Help);
    };

    let rest: Vec<String> = iter.collect();

    match subcommand.as_str() {
        "start" => parse_start(rest),
        "attach" => parse_attach(rest),
        "list-panes" | "list" => parse_list_panes(rest),
        "capture" => parse_capture(rest),
        "status" => parse_status(rest),
        "doctor" => parse_doctor(rest),
        "observe" => parse_observe(rest),
        "serve" => parse_serve(rest),
        "record-fixture" => parse_record_fixture(rest),
        "classify" => parse_classify(rest),
        "replay" => parse_replay(rest),
        "bindings" => Ok(Command::Bindings),
        "install-bindings" => parse_install_bindings(rest),
        "send-action" => parse_send_action(rest),
        "approve" | "approve-permission" => parse_approve_reject(rest, Command::ApprovePermission),
        "reject" | "reject-permission" => parse_approve_reject(rest, Command::RejectPermission),
        "dismiss-survey" => parse_pane_command(rest, "dismiss-survey", Command::DismissSurvey),
        "continue-session" => parse_continue_session(rest),
        "auto-unstick" => parse_auto_unstick(rest),
        "keep-going" => parse_keep_going(rest),
        "prepare-prompt" => parse_prepare_prompt(rest),
        "editor-helper" => parse_editor_helper(rest),
        "submit-prompt" => parse_submit_prompt(rest),
        "yolo" | "permission-babysit" | "babysit" => parse_permission_babysit(rest),
        "help" | "--help" | "-h" => Ok(Command::Help),
        other => Err(AppError::new(format!("unknown subcommand: {other}"))),
    }
}

pub fn usage() -> String {
    String::from(
        "botctl\n\
          \n\
          Commands:\n\
            start --session NAME [--window NAME] [--cwd PATH] [--command CMD] [--dry-run]\n\
            attach (--pane %ID|session:window.pane | --session NAME [--window NAME])\n\
            list-panes|list [--all]\n\
            capture --pane %ID|session:window.pane [--history-lines N]\n\
            status --pane %ID|session:window.pane [--history-lines N]\n\
            doctor [--session NAME] [--pane %ID|session:window.pane] [--history-lines N] [--bindings-path PATH]\n\
            observe --session NAME [--pane %ID|session:window.pane] [--events N] [--idle-timeout-ms N] [--history-lines N] [--state-dir PATH]\n\
            serve --session NAME [--pane %ID|session:window.pane] [--reconcile-ms N] [--history-lines N] [--format human|jsonl] [--state-dir PATH]\n\
            record-fixture --session NAME --case NAME [--pane %ID|session:window.pane] [--output-dir PATH] [--expected-state STATE] [--events N] [--idle-timeout-ms N] [--history-lines N]\n\
            classify --path PATH\n\
            replay --path PATH\n\
            bindings\n\
            install-bindings [--path PATH]\n\
            send-action --pane %ID|session:window.pane --action NAME\n\
            approve --pane %ID|session:window.pane [--format human|jsonl]\n\
            reject --pane %ID|session:window.pane [--format human|jsonl]\n\
            dismiss-survey --pane %ID|session:window.pane\n\
            continue-session (--pane %ID|session:window.pane | --session NAME --window NAME)\n\
            auto-unstick (--pane %ID|session:window.pane | --session NAME --window NAME) [--max-steps N]\n\
            keep-going (--pane %ID|session:window.pane | --session NAME --window NAME) [--poll-ms N] [--submit-delay-ms N] [--state-dir PATH] [--source PATH | --text TEXT] [--no-yolo]\n\
            prepare-prompt --session NAME [--state-dir PATH] [--workspace PATH|UUID] [--source PATH | --text TEXT]\n\
            editor-helper --session NAME [--state-dir PATH] [--workspace PATH|UUID] [--source PATH] [--keep-pending] TARGET\n\
            submit-prompt --session NAME --pane %ID|session:window.pane [--state-dir PATH] [--workspace PATH|UUID] [--source PATH | --text TEXT] [--submit-delay-ms N]\n\
            yolo [start] (--pane %ID|session:window.pane | --all) [--poll-ms N] [--format human|jsonl] [--live-preview] [--state-dir PATH] [--workspace PATH|UUID]\n\
            yolo stop (--pane %ID|session:window.pane | --all) [--state-dir PATH] [--workspace PATH|UUID]\n",
    )
}

fn parse_start(args: Vec<String>) -> AppResult<Command> {
    let mut session_name = None;
    let mut window_name = String::from("claude");
    let mut cwd = None;
    let mut command = String::from("claude");
    let mut dry_run = false;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--session" => {
                session_name = Some(read_value(&args, &mut i, "--session")?);
            }
            "--window" => {
                window_name = read_value(&args, &mut i, "--window")?;
            }
            "--cwd" => {
                cwd = Some(PathBuf::from(read_value(&args, &mut i, "--cwd")?));
            }
            "--command" => {
                command = read_value(&args, &mut i, "--command")?;
            }
            "--dry-run" => {
                dry_run = true;
            }
            flag => {
                return Err(AppError::new(format!("unknown start flag: {flag}")));
            }
        }
        i += 1;
    }

    let session_name =
        session_name.ok_or_else(|| AppError::new("missing required flag: --session"))?;

    Ok(Command::Start(StartArgs {
        session_name,
        window_name,
        cwd,
        command,
        dry_run,
    }))
}

fn parse_attach(args: Vec<String>) -> AppResult<Command> {
    let target = parse_pane_target_args(args, "attach")?;
    Ok(Command::Attach(AttachArgs { target }))
}

fn parse_approve_reject(
    args: Vec<String>,
    command: fn(PaneCommandArgs) -> Command,
) -> AppResult<Command> {
    let mut pane_id = None;
    let mut format = BabysitFormat::Human;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--pane" => pane_id = Some(read_value(&args, &mut i, "--pane")?),
            "--format" => {
                let raw = read_value(&args, &mut i, "--format")?;
                format = parse_human_jsonl_format(&raw)?;
            }
            flag => {
                return Err(AppError::new(format!(
                    "unknown approve/reject flag: {flag}"
                )));
            }
        }
        i += 1;
    }

    let pane_id = pane_id.ok_or_else(|| AppError::new("missing required flag: --pane"))?;
    Ok(command(PaneCommandArgs { pane_id, format }))
}

fn parse_list_panes(args: Vec<String>) -> AppResult<Command> {
    let mut all = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--all" => all = true,
            flag => return Err(AppError::new(format!("unknown list-panes flag: {flag}"))),
        }
        i += 1;
    }

    Ok(Command::ListPanes(ListPanesArgs { all }))
}

fn parse_capture(args: Vec<String>) -> AppResult<Command> {
    let mut pane_id = None;
    let mut history_lines = 200usize;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--pane" => {
                pane_id = Some(read_value(&args, &mut i, "--pane")?);
            }
            "--history-lines" => {
                let raw = read_value(&args, &mut i, "--history-lines")?;
                history_lines = raw.parse::<usize>().map_err(|_| {
                    AppError::new(format!("invalid value for --history-lines: {raw}"))
                })?;
            }
            flag => {
                return Err(AppError::new(format!("unknown capture flag: {flag}")));
            }
        }
        i += 1;
    }

    let pane_id = pane_id.ok_or_else(|| AppError::new("missing required flag: --pane"))?;

    Ok(Command::Capture(CaptureArgs {
        pane_id,
        history_lines,
    }))
}

fn parse_status(args: Vec<String>) -> AppResult<Command> {
    let mut pane_id = None;
    let mut history_lines = 120usize;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--pane" => {
                pane_id = Some(read_value(&args, &mut i, "--pane")?);
            }
            "--history-lines" => {
                let raw = read_value(&args, &mut i, "--history-lines")?;
                history_lines = raw.parse::<usize>().map_err(|_| {
                    AppError::new(format!("invalid value for --history-lines: {raw}"))
                })?;
            }
            flag => {
                return Err(AppError::new(format!("unknown status flag: {flag}")));
            }
        }
        i += 1;
    }

    let pane_id = pane_id.ok_or_else(|| AppError::new("missing required flag: --pane"))?;
    Ok(Command::Status(StatusArgs {
        pane_id,
        history_lines,
    }))
}

fn parse_doctor(args: Vec<String>) -> AppResult<Command> {
    let mut session_name = None;
    let mut pane_id = None;
    let mut history_lines = 120usize;
    let mut bindings_path = None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--session" => {
                session_name = Some(read_value(&args, &mut i, "--session")?);
            }
            "--pane" => {
                pane_id = Some(read_value(&args, &mut i, "--pane")?);
            }
            "--history-lines" => {
                let raw = read_value(&args, &mut i, "--history-lines")?;
                history_lines = raw.parse::<usize>().map_err(|_| {
                    AppError::new(format!("invalid value for --history-lines: {raw}"))
                })?;
            }
            "--bindings-path" => {
                bindings_path = Some(PathBuf::from(read_value(&args, &mut i, "--bindings-path")?));
            }
            flag => {
                return Err(AppError::new(format!("unknown doctor flag: {flag}")));
            }
        }
        i += 1;
    }

    if pane_id.is_none() && session_name.is_none() {
        return Err(AppError::new(
            "doctor requires either --pane %ID|session:window.pane or --session NAME",
        ));
    }

    Ok(Command::Doctor(DoctorArgs {
        session_name,
        pane_id,
        history_lines,
        bindings_path,
    }))
}

fn parse_observe(args: Vec<String>) -> AppResult<Command> {
    let mut session_name = None;
    let mut pane_id = None;
    let mut events = 25usize;
    let mut idle_timeout_ms = 1500u64;
    let mut history_lines = 120usize;
    let mut state_dir = None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--session" => {
                session_name = Some(read_value(&args, &mut i, "--session")?);
            }
            "--pane" => {
                pane_id = Some(read_value(&args, &mut i, "--pane")?);
            }
            "--events" => {
                let raw = read_value(&args, &mut i, "--events")?;
                events = raw
                    .parse::<usize>()
                    .map_err(|_| AppError::new(format!("invalid value for --events: {raw}")))?;
            }
            "--idle-timeout-ms" => {
                let raw = read_value(&args, &mut i, "--idle-timeout-ms")?;
                idle_timeout_ms = raw.parse::<u64>().map_err(|_| {
                    AppError::new(format!("invalid value for --idle-timeout-ms: {raw}"))
                })?;
            }
            "--history-lines" => {
                let raw = read_value(&args, &mut i, "--history-lines")?;
                history_lines = raw.parse::<usize>().map_err(|_| {
                    AppError::new(format!("invalid value for --history-lines: {raw}"))
                })?;
            }
            "--state-dir" => {
                state_dir = Some(PathBuf::from(read_value(&args, &mut i, "--state-dir")?));
            }
            flag => {
                return Err(AppError::new(format!("unknown observe flag: {flag}")));
            }
        }
        i += 1;
    }

    let session_name =
        session_name.ok_or_else(|| AppError::new("missing required flag: --session"))?;

    Ok(Command::Observe(ObserveArgs {
        session_name,
        pane_id,
        events,
        idle_timeout_ms,
        history_lines,
        state_dir,
    }))
}

fn parse_serve(args: Vec<String>) -> AppResult<Command> {
    let mut session_name = None;
    let mut pane_id = None;
    let mut reconcile_ms = 1500u64;
    let mut history_lines = 120usize;
    let mut format = BabysitFormat::Human;
    let mut state_dir = None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--session" => {
                session_name = Some(read_value(&args, &mut i, "--session")?);
            }
            "--pane" => {
                pane_id = Some(read_value(&args, &mut i, "--pane")?);
            }
            "--reconcile-ms" => {
                let raw = read_value(&args, &mut i, "--reconcile-ms")?;
                reconcile_ms = raw.parse::<u64>().map_err(|_| {
                    AppError::new(format!("invalid value for --reconcile-ms: {raw}"))
                })?;
            }
            "--history-lines" => {
                let raw = read_value(&args, &mut i, "--history-lines")?;
                history_lines = raw.parse::<usize>().map_err(|_| {
                    AppError::new(format!("invalid value for --history-lines: {raw}"))
                })?;
            }
            "--state-dir" => {
                state_dir = Some(PathBuf::from(read_value(&args, &mut i, "--state-dir")?));
            }
            "--format" => {
                let raw = read_value(&args, &mut i, "--format")?;
                format = parse_human_jsonl_format(&raw)?;
            }
            flag => {
                return Err(AppError::new(format!("unknown serve flag: {flag}")));
            }
        }
        i += 1;
    }

    let session_name =
        session_name.ok_or_else(|| AppError::new("missing required flag: --session"))?;
    if reconcile_ms == 0 {
        return Err(AppError::new(
            "serve requires --reconcile-ms to be at least 1",
        ));
    }

    Ok(Command::Serve(ServeArgs {
        session_name,
        pane_id,
        reconcile_ms,
        history_lines,
        format,
        state_dir,
    }))
}

fn parse_record_fixture(args: Vec<String>) -> AppResult<Command> {
    let mut session_name = None;
    let mut pane_id = None;
    let mut case_name = None;
    let mut output_dir = PathBuf::from("fixtures/cases");
    let mut expected_state = None;
    let mut events = 25usize;
    let mut idle_timeout_ms = 1500u64;
    let mut history_lines = 120usize;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--session" => {
                session_name = Some(read_value(&args, &mut i, "--session")?);
            }
            "--pane" => {
                pane_id = Some(read_value(&args, &mut i, "--pane")?);
            }
            "--case" => {
                case_name = Some(read_value(&args, &mut i, "--case")?);
            }
            "--output-dir" => {
                output_dir = PathBuf::from(read_value(&args, &mut i, "--output-dir")?);
            }
            "--expected-state" => {
                expected_state = Some(read_value(&args, &mut i, "--expected-state")?);
            }
            "--events" => {
                let raw = read_value(&args, &mut i, "--events")?;
                events = raw
                    .parse::<usize>()
                    .map_err(|_| AppError::new(format!("invalid value for --events: {raw}")))?;
            }
            "--idle-timeout-ms" => {
                let raw = read_value(&args, &mut i, "--idle-timeout-ms")?;
                idle_timeout_ms = raw.parse::<u64>().map_err(|_| {
                    AppError::new(format!("invalid value for --idle-timeout-ms: {raw}"))
                })?;
            }
            "--history-lines" => {
                let raw = read_value(&args, &mut i, "--history-lines")?;
                history_lines = raw.parse::<usize>().map_err(|_| {
                    AppError::new(format!("invalid value for --history-lines: {raw}"))
                })?;
            }
            flag => {
                return Err(AppError::new(format!(
                    "unknown record-fixture flag: {flag}"
                )));
            }
        }
        i += 1;
    }

    let session_name =
        session_name.ok_or_else(|| AppError::new("missing required flag: --session"))?;
    let case_name = case_name.ok_or_else(|| AppError::new("missing required flag: --case"))?;

    Ok(Command::RecordFixture(RecordFixtureArgs {
        session_name,
        pane_id,
        case_name,
        output_dir,
        expected_state,
        events,
        idle_timeout_ms,
        history_lines,
    }))
}

fn parse_classify(args: Vec<String>) -> AppResult<Command> {
    let path = parse_single_path_flag(args, "--path", "classify")?;
    Ok(Command::Classify(ClassifyArgs { path }))
}

fn parse_replay(args: Vec<String>) -> AppResult<Command> {
    let path = parse_single_path_flag(args, "--path", "replay")?;
    Ok(Command::Replay(ReplayArgs { path }))
}

fn parse_send_action(args: Vec<String>) -> AppResult<Command> {
    let mut pane_id = None;
    let mut action = None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--pane" => {
                pane_id = Some(read_value(&args, &mut i, "--pane")?);
            }
            "--action" => {
                action = Some(read_value(&args, &mut i, "--action")?);
            }
            flag => {
                return Err(AppError::new(format!("unknown send-action flag: {flag}")));
            }
        }
        i += 1;
    }

    let pane_id = pane_id.ok_or_else(|| AppError::new("missing required flag: --pane"))?;
    let action = action.ok_or_else(|| AppError::new("missing required flag: --action"))?;

    Ok(Command::SendAction(SendActionArgs { pane_id, action }))
}

fn parse_install_bindings(args: Vec<String>) -> AppResult<Command> {
    let mut path = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--path" => {
                path = Some(PathBuf::from(read_value(&args, &mut i, "--path")?));
            }
            flag => {
                return Err(AppError::new(format!(
                    "unknown install-bindings flag: {flag}"
                )));
            }
        }
        i += 1;
    }

    Ok(Command::InstallBindings(InstallBindingsArgs { path }))
}

fn parse_pane_command(
    args: Vec<String>,
    command_name: &str,
    command: fn(PaneCommandArgs) -> Command,
) -> AppResult<Command> {
    let mut pane_id = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--pane" => {
                pane_id = Some(read_value(&args, &mut i, "--pane")?);
            }
            flag => {
                return Err(AppError::new(format!(
                    "unknown {command_name} flag: {flag}"
                )));
            }
        }
        i += 1;
    }

    let pane_id = pane_id.ok_or_else(|| AppError::new("missing required flag: --pane"))?;
    Ok(command(PaneCommandArgs {
        pane_id,
        format: BabysitFormat::Human,
    }))
}

fn parse_continue_session(args: Vec<String>) -> AppResult<Command> {
    let target = parse_pane_target_args(args, "continue-session")?;
    validate_send_target(&target, "continue-session")?;
    Ok(Command::ContinueSession(ContinueSessionArgs { target }))
}

fn parse_auto_unstick(args: Vec<String>) -> AppResult<Command> {
    let mut max_steps = 6usize;
    let mut target_args = Vec::new();
    let mut i = 0;

    while i < args.len() {
        if args[i] == "--max-steps" {
            let raw = read_value(&args, &mut i, "--max-steps")?;
            max_steps = raw
                .parse::<usize>()
                .map_err(|_| AppError::new(format!("invalid value for --max-steps: {raw}")))?;
        } else {
            target_args.push(args[i].clone());
        }
        i += 1;
    }

    let target = parse_pane_target_args(target_args, "auto-unstick")?;
    validate_send_target(&target, "auto-unstick")?;
    if max_steps == 0 {
        return Err(AppError::new(
            "auto-unstick requires --max-steps to be at least 1",
        ));
    }
    Ok(Command::AutoUnstick(AutoUnstickArgs { target, max_steps }))
}

fn parse_keep_going(args: Vec<String>) -> AppResult<Command> {
    let mut no_yolo = false;
    let mut poll_ms = 1000u64;
    let mut submit_delay_ms = 250u64;
    let mut state_dir = None;
    let mut prompt_source = None;
    let mut prompt_text = None;
    let mut target_args = Vec::new();
    let mut i = 0;

    while i < args.len() {
        match args[i].as_str() {
            "--no-yolo" => no_yolo = true,
            "--poll-ms" => {
                let raw = read_value(&args, &mut i, "--poll-ms")?;
                poll_ms = raw
                    .parse::<u64>()
                    .map_err(|_| AppError::new(format!("invalid value for --poll-ms: {raw}")))?;
            }
            "--submit-delay-ms" => {
                let raw = read_value(&args, &mut i, "--submit-delay-ms")?;
                submit_delay_ms = raw.parse::<u64>().map_err(|_| {
                    AppError::new(format!("invalid value for --submit-delay-ms: {raw}"))
                })?;
            }
            "--state-dir" => {
                state_dir = Some(PathBuf::from(read_value(&args, &mut i, "--state-dir")?));
            }
            "--source" => {
                prompt_source = Some(PathBuf::from(read_value(&args, &mut i, "--source")?));
            }
            "--text" => {
                prompt_text = Some(read_value(&args, &mut i, "--text")?);
            }
            _ => target_args.push(args[i].clone()),
        }
        i += 1;
    }

    let target = parse_pane_target_args(target_args, "keep-going")?;
    validate_send_target(&target, "keep-going")?;
    if poll_ms == 0 {
        return Err(AppError::new(
            "keep-going requires --poll-ms to be at least 1",
        ));
    }
    if submit_delay_ms == 0 {
        return Err(AppError::new(
            "keep-going requires --submit-delay-ms to be at least 1",
        ));
    }
    validate_optional_prompt_input(&prompt_source, &prompt_text)?;

    Ok(Command::KeepGoing(KeepGoingArgs {
        target,
        no_yolo,
        poll_ms,
        submit_delay_ms,
        state_dir,
        prompt_source,
        prompt_text,
    }))
}

fn parse_pane_target_args(args: Vec<String>, command_name: &str) -> AppResult<PaneTargetArgs> {
    let mut pane_id = None;
    let mut session_name = None;
    let mut window_name = None;
    let mut i = 0;

    while i < args.len() {
        match args[i].as_str() {
            "--pane" => {
                pane_id = Some(read_value(&args, &mut i, "--pane")?);
            }
            "--session" => {
                session_name = Some(read_value(&args, &mut i, "--session")?);
            }
            "--window" => {
                window_name = Some(read_value(&args, &mut i, "--window")?);
            }
            flag => {
                return Err(AppError::new(format!(
                    "unknown {command_name} flag: {flag}"
                )));
            }
        }
        i += 1;
    }

    match (&pane_id, &session_name, &window_name) {
        (Some(_), None, None) => Ok(PaneTargetArgs {
            pane_id,
            session_name,
            window_name,
        }),
        (None, Some(_), _) => Ok(PaneTargetArgs {
            pane_id,
            session_name,
            window_name,
        }),
        (Some(_), Some(_), _) | (Some(_), None, Some(_)) => Err(AppError::new(format!(
            "{command_name} target must use either --pane %ID|session:window.pane or --session NAME [--window NAME]"
        ))),
        (None, None, Some(_)) => Err(AppError::new(format!(
            "{command_name} requires --session NAME when --window NAME is provided"
        ))),
        (None, None, None) => Err(AppError::new(format!(
            "{command_name} requires either --pane %ID|session:window.pane or --session NAME"
        ))),
    }
}

fn validate_send_target(target: &PaneTargetArgs, command_name: &str) -> AppResult<()> {
    match (
        target.pane_id.as_deref(),
        target.session_name.as_deref(),
        target.window_name.as_deref(),
    ) {
        (Some(_), None, None) | (None, Some(_), Some(_)) => Ok(()),
        _ => Err(AppError::new(format!(
            "{command_name} requires --pane %ID|session:window.pane or --session NAME --window NAME"
        ))),
    }
}

fn validate_optional_prompt_input(
    source: &Option<PathBuf>,
    text: &Option<String>,
) -> AppResult<()> {
    match (source, text) {
        (Some(_), Some(_)) => Err(AppError::new(
            "prompt input must use either --source PATH or --text TEXT, not both",
        )),
        _ => Ok(()),
    }
}

fn parse_prepare_prompt(args: Vec<String>) -> AppResult<Command> {
    let mut session_name = None;
    let mut state_dir = None;
    let mut workspace = None;
    let mut source = None;
    let mut text = None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--session" => {
                session_name = Some(read_value(&args, &mut i, "--session")?);
            }
            "--state-dir" => {
                state_dir = Some(PathBuf::from(read_value(&args, &mut i, "--state-dir")?));
            }
            "--workspace" => {
                workspace = Some(read_value(&args, &mut i, "--workspace")?);
            }
            "--source" => {
                source = Some(PathBuf::from(read_value(&args, &mut i, "--source")?));
            }
            "--text" => {
                text = Some(read_value(&args, &mut i, "--text")?);
            }
            flag => {
                return Err(AppError::new(format!(
                    "unknown prepare-prompt flag: {flag}"
                )));
            }
        }
        i += 1;
    }

    let session_name =
        session_name.ok_or_else(|| AppError::new("missing required flag: --session"))?;
    validate_prompt_input(&source, &text)?;

    Ok(Command::PreparePrompt(PreparePromptArgs {
        session_name,
        state_dir,
        workspace,
        source,
        text,
    }))
}

fn parse_editor_helper(args: Vec<String>) -> AppResult<Command> {
    let mut session_name = None;
    let mut state_dir = None;
    let mut workspace = None;
    let mut source = None;
    let mut keep_pending = false;
    let mut target = None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--session" => {
                session_name = Some(read_value(&args, &mut i, "--session")?);
            }
            "--state-dir" => {
                state_dir = Some(PathBuf::from(read_value(&args, &mut i, "--state-dir")?));
            }
            "--workspace" => {
                workspace = Some(read_value(&args, &mut i, "--workspace")?);
            }
            "--source" => {
                source = Some(PathBuf::from(read_value(&args, &mut i, "--source")?));
            }
            "--keep-pending" => {
                keep_pending = true;
            }
            value if value.starts_with("--") => {
                return Err(AppError::new(format!(
                    "unknown editor-helper flag: {value}"
                )));
            }
            value => {
                if target.is_some() {
                    return Err(AppError::new(format!(
                        "unexpected extra editor-helper target: {value}"
                    )));
                }
                target = Some(PathBuf::from(value));
            }
        }
        i += 1;
    }

    let session_name =
        session_name.ok_or_else(|| AppError::new("missing required flag: --session"))?;
    let target = target.ok_or_else(|| AppError::new("missing required editor-helper target"))?;

    Ok(Command::EditorHelper(EditorHelperArgs {
        session_name,
        state_dir,
        workspace,
        source,
        target,
        keep_pending,
    }))
}

fn parse_submit_prompt(args: Vec<String>) -> AppResult<Command> {
    let mut session_name = None;
    let mut pane_id = None;
    let mut state_dir = None;
    let mut workspace = None;
    let mut source = None;
    let mut text = None;
    let mut submit_delay_ms = 250u64;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--session" => {
                session_name = Some(read_value(&args, &mut i, "--session")?);
            }
            "--pane" => {
                pane_id = Some(read_value(&args, &mut i, "--pane")?);
            }
            "--state-dir" => {
                state_dir = Some(PathBuf::from(read_value(&args, &mut i, "--state-dir")?));
            }
            "--workspace" => {
                workspace = Some(read_value(&args, &mut i, "--workspace")?);
            }
            "--source" => {
                source = Some(PathBuf::from(read_value(&args, &mut i, "--source")?));
            }
            "--text" => {
                text = Some(read_value(&args, &mut i, "--text")?);
            }
            "--submit-delay-ms" => {
                let raw = read_value(&args, &mut i, "--submit-delay-ms")?;
                submit_delay_ms = raw.parse::<u64>().map_err(|_| {
                    AppError::new(format!("invalid value for --submit-delay-ms: {raw}"))
                })?;
            }
            flag => {
                return Err(AppError::new(format!("unknown submit-prompt flag: {flag}")));
            }
        }
        i += 1;
    }

    let session_name =
        session_name.ok_or_else(|| AppError::new("missing required flag: --session"))?;
    let pane_id = pane_id.ok_or_else(|| AppError::new("missing required flag: --pane"))?;
    validate_prompt_input(&source, &text)?;
    if submit_delay_ms == 0 {
        return Err(AppError::new(
            "submit-prompt requires --submit-delay-ms to be at least 1",
        ));
    }

    Ok(Command::SubmitPrompt(SubmitPromptArgs {
        session_name,
        pane_id,
        state_dir,
        workspace,
        source,
        text,
        submit_delay_ms,
    }))
}

fn parse_permission_babysit(args: Vec<String>) -> AppResult<Command> {
    let (mode, start_index) = match args.first().map(String::as_str) {
        Some("start") => ("start", 1),
        Some("stop") => ("stop", 1),
        Some(_) | None => ("start", 0),
    };

    match mode {
        "start" => {
            let mut pane_id = None;
            let mut all = false;
            let mut poll_ms = 1000u64;
            let mut live_preview = false;
            let mut format = BabysitFormat::Human;
            let mut state_dir = None;
            let mut workspace = None;
            let mut i = start_index;
            while i < args.len() {
                match args[i].as_str() {
                    "--pane" => pane_id = Some(read_value(&args, &mut i, "--pane")?),
                    "--all" => all = true,
                    "--poll-ms" => {
                        let raw = read_value(&args, &mut i, "--poll-ms")?;
                        poll_ms = raw.parse::<u64>().map_err(|_| {
                            AppError::new(format!("invalid value for --poll-ms: {raw}"))
                        })?;
                    }
                    "--live-preview" => {
                        live_preview = true;
                    }
                    "--format" => {
                        let raw = read_value(&args, &mut i, "--format")?;
                        format = parse_human_jsonl_format(&raw)?;
                    }
                    "--state-dir" => {
                        state_dir = Some(PathBuf::from(read_value(&args, &mut i, "--state-dir")?));
                    }
                    "--workspace" => {
                        workspace = Some(read_value(&args, &mut i, "--workspace")?);
                    }
                    flag => {
                        return Err(AppError::new(format!("unknown yolo flag: {flag}")));
                    }
                }
                i += 1;
            }
            if poll_ms == 0 {
                return Err(AppError::new(
                    "yolo start requires --poll-ms to be at least 1",
                ));
            }
            if pane_id.is_some() == all {
                return Err(AppError::new(
                    "yolo start requires exactly one of --pane or --all",
                ));
            }
            Ok(Command::PermissionBabysitStart(
                PermissionBabysitStartArgs {
                    pane_id,
                    all,
                    poll_ms,
                    live_preview,
                    format,
                    state_dir,
                    workspace,
                },
            ))
        }
        "stop" => {
            let mut pane_id = None;
            let mut all = false;
            let mut state_dir = None;
            let mut workspace = None;
            let mut i = 1;
            while i < args.len() {
                match args[i].as_str() {
                    "--pane" => pane_id = Some(read_value(&args, &mut i, "--pane")?),
                    "--all" => all = true,
                    "--state-dir" => {
                        state_dir = Some(PathBuf::from(read_value(&args, &mut i, "--state-dir")?));
                    }
                    "--workspace" => {
                        workspace = Some(read_value(&args, &mut i, "--workspace")?);
                    }
                    flag => {
                        return Err(AppError::new(format!("unknown yolo flag: {flag}")));
                    }
                }
                i += 1;
            }
            if pane_id.is_some() == all {
                return Err(AppError::new(
                    "yolo stop requires exactly one of --pane or --all",
                ));
            }
            Ok(Command::PermissionBabysitStop(PermissionBabysitStopArgs {
                pane_id,
                all,
                state_dir,
                workspace,
            }))
        }
        _ => Err(AppError::new("yolo requires start or stop subcommand")),
    }
}

fn parse_single_path_flag(
    args: Vec<String>,
    flag_name: &str,
    command_name: &str,
) -> AppResult<PathBuf> {
    let mut path = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            name if name == flag_name => {
                path = Some(PathBuf::from(read_value(&args, &mut i, flag_name)?));
            }
            flag => {
                return Err(AppError::new(format!(
                    "unknown {command_name} flag: {flag}"
                )));
            }
        }
        i += 1;
    }

    path.ok_or_else(|| AppError::new(format!("missing required flag: {flag_name}")))
}

fn read_value(args: &[String], index: &mut usize, flag: &str) -> AppResult<String> {
    *index += 1;
    args.get(*index)
        .cloned()
        .ok_or_else(|| AppError::new(format!("missing value for {flag}")))
}

fn parse_human_jsonl_format(raw: &str) -> AppResult<BabysitFormat> {
    match raw {
        "human" => Ok(BabysitFormat::Human),
        "jsonl" => Ok(BabysitFormat::Jsonl),
        _ => Err(AppError::new(format!("invalid value for --format: {raw}"))),
    }
}

fn validate_prompt_input(source: &Option<PathBuf>, text: &Option<String>) -> AppResult<()> {
    match (source, text) {
        (Some(_), Some(_)) => Err(AppError::new(
            "prompt input must use either --source PATH or --text TEXT, not both",
        )),
        (None, None) => Err(AppError::new(
            "prompt input requires either --source PATH or --text TEXT",
        )),
        _ => Ok(()),
    }
}

#[cfg(any(test, rust_analyzer))]
mod tests {
    use std::path::PathBuf;

    use super::{Command, parse_args};

    #[test]
    fn parses_start_command() {
        let command = parse_args(vec![
            String::from("botctl"),
            String::from("start"),
            String::from("--session"),
            String::from("demo"),
            String::from("--dry-run"),
        ])
        .expect("start command should parse");

        match command {
            Command::Start(args) => {
                assert_eq!(args.session_name, "demo");
                assert!(args.dry_run);
                assert_eq!(args.window_name, "claude");
                assert_eq!(args.command, "claude");
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_list_panes_default_as_claude_only() {
        let command = parse_args(vec![String::from("botctl"), String::from("list-panes")])
            .expect("list-panes command should parse");

        match command {
            Command::ListPanes(args) => assert!(!args.all),
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_list_alias() {
        let command = parse_args(vec![String::from("botctl"), String::from("list")])
            .expect("list alias should parse");

        match command {
            Command::ListPanes(args) => assert!(!args.all),
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_list_panes_all_flag() {
        let command = parse_args(vec![
            String::from("botctl"),
            String::from("list-panes"),
            String::from("--all"),
        ])
        .expect("list-panes --all command should parse");

        match command {
            Command::ListPanes(args) => assert!(args.all),
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_attach_command_with_session_window_target() {
        let command = parse_args(vec![
            String::from("botctl"),
            String::from("attach"),
            String::from("--session"),
            String::from("demo"),
            String::from("--window"),
            String::from("claude"),
        ])
        .expect("attach command should parse");

        match command {
            Command::Attach(args) => {
                assert_eq!(args.target.session_name.as_deref(), Some("demo"));
                assert_eq!(args.target.window_name.as_deref(), Some("claude"));
                assert!(args.target.pane_id.is_none());
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_attach_command_with_tmux_pane_target() {
        let command = parse_args(vec![
            String::from("botctl"),
            String::from("attach"),
            String::from("--pane"),
            String::from("0:2.3"),
        ])
        .expect("attach pane target should parse");

        match command {
            Command::Attach(args) => {
                assert_eq!(args.target.pane_id.as_deref(), Some("0:2.3"));
                assert!(args.target.session_name.is_none());
                assert!(args.target.window_name.is_none());
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn rejects_attach_window_without_session() {
        let error = parse_args(vec![
            String::from("botctl"),
            String::from("attach"),
            String::from("--window"),
            String::from("claude"),
        ])
        .expect_err("attach should reject window-only target");

        assert!(
            error
                .to_string()
                .contains("attach requires --session NAME when --window NAME is provided")
        );
    }

    #[test]
    fn parses_observe_command() {
        let command = parse_args(vec![
            String::from("botctl"),
            String::from("observe"),
            String::from("--session"),
            String::from("demo"),
            String::from("--events"),
            String::from("10"),
            String::from("--state-dir"),
            String::from("/tmp/botctl-observe"),
        ])
        .expect("observe command should parse");

        match command {
            Command::Observe(args) => {
                assert_eq!(args.session_name, "demo");
                assert_eq!(args.events, 10);
                assert_eq!(args.idle_timeout_ms, 1500);
                assert_eq!(args.state_dir, Some(PathBuf::from("/tmp/botctl-observe")));
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_serve_command() {
        let command = parse_args(vec![
            String::from("botctl"),
            String::from("serve"),
            String::from("--session"),
            String::from("demo"),
            String::from("--pane"),
            String::from("%7"),
            String::from("--reconcile-ms"),
            String::from("750"),
            String::from("--format"),
            String::from("jsonl"),
            String::from("--state-dir"),
            String::from("/tmp/botctl-serve"),
        ])
        .expect("serve command should parse");

        match command {
            Command::Serve(args) => {
                assert_eq!(args.session_name, "demo");
                assert_eq!(args.pane_id.as_deref(), Some("%7"));
                assert_eq!(args.reconcile_ms, 750);
                assert_eq!(args.format, super::BabysitFormat::Jsonl);
                assert_eq!(args.state_dir, Some(PathBuf::from("/tmp/botctl-serve")));
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn rejects_serve_zero_reconcile_interval() {
        let error = parse_args(vec![
            String::from("botctl"),
            String::from("serve"),
            String::from("--session"),
            String::from("demo"),
            String::from("--reconcile-ms"),
            String::from("0"),
        ])
        .expect_err("serve should reject zero reconcile interval");

        assert!(
            error
                .to_string()
                .contains("serve requires --reconcile-ms to be at least 1")
        );
    }

    #[test]
    fn usage_renders_real_newlines_for_observe_and_serve() {
        let usage = super::usage();

        assert!(!usage.contains("\\n"));
        assert!(usage.contains("observe --session NAME"));
        assert!(usage.contains("serve --session NAME"));
    }

    #[test]
    fn parses_status_command() {
        let command = parse_args(vec![
            String::from("botctl"),
            String::from("status"),
            String::from("--pane"),
            String::from("%7"),
        ])
        .expect("status command should parse");

        match command {
            Command::Status(args) => {
                assert_eq!(args.pane_id, "%7");
                assert_eq!(args.history_lines, 120);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_status_command_with_tmux_pane_target() {
        let command = parse_args(vec![
            String::from("botctl"),
            String::from("status"),
            String::from("--pane"),
            String::from("0:2.3"),
        ])
        .expect("status command should parse tmux pane target");

        match command {
            Command::Status(args) => {
                assert_eq!(args.pane_id, "0:2.3");
                assert_eq!(args.history_lines, 120);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_doctor_command_with_session() {
        let command = parse_args(vec![
            String::from("botctl"),
            String::from("doctor"),
            String::from("--session"),
            String::from("demo"),
        ])
        .expect("doctor command should parse");

        match command {
            Command::Doctor(args) => {
                assert_eq!(args.session_name.as_deref(), Some("demo"));
                assert!(args.pane_id.is_none());
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_install_bindings_command() {
        let command = parse_args(vec![
            String::from("botctl"),
            String::from("install-bindings"),
        ])
        .expect("install-bindings command should parse");

        match command {
            Command::InstallBindings(args) => {
                assert!(args.path.is_none());
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_editor_helper_with_positional_target() {
        let command = parse_args(vec![
            String::from("botctl"),
            String::from("editor-helper"),
            String::from("--session"),
            String::from("demo"),
            String::from("/tmp/target.txt"),
        ])
        .expect("editor helper command should parse");

        match command {
            Command::EditorHelper(args) => {
                assert_eq!(args.session_name, "demo");
                assert_eq!(args.target, PathBuf::from("/tmp/target.txt"));
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_approve_command_with_format() {
        let command = parse_args(vec![
            String::from("botctl"),
            String::from("approve"),
            String::from("--pane"),
            String::from("%9"),
            String::from("--format"),
            String::from("jsonl"),
        ])
        .expect("approve command should parse");

        match command {
            Command::ApprovePermission(args) => {
                assert_eq!(args.pane_id, "%9");
                assert_eq!(args.format, super::BabysitFormat::Jsonl);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_approve_permission_alias() {
        let command = parse_args(vec![
            String::from("botctl"),
            String::from("approve-permission"),
            String::from("--pane"),
            String::from("%9"),
        ])
        .expect("alias should parse");

        match command {
            Command::ApprovePermission(args) => {
                assert_eq!(args.format, super::BabysitFormat::Human);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_reject_command_with_format() {
        let command = parse_args(vec![
            String::from("botctl"),
            String::from("reject"),
            String::from("--pane"),
            String::from("%4"),
            String::from("--format"),
            String::from("human"),
        ])
        .expect("reject command should parse");

        match command {
            Command::RejectPermission(args) => {
                assert_eq!(args.pane_id, "%4");
                assert_eq!(args.format, super::BabysitFormat::Human);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_continue_session_command() {
        let command = parse_args(vec![
            String::from("botctl"),
            String::from("continue-session"),
            String::from("--session"),
            String::from("demo"),
            String::from("--window"),
            String::from("claude"),
        ])
        .expect("continue-session command should parse");

        match command {
            Command::ContinueSession(args) => {
                assert_eq!(args.target.session_name.as_deref(), Some("demo"));
                assert_eq!(args.target.window_name.as_deref(), Some("claude"));
                assert!(args.target.pane_id.is_none());
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn rejects_continue_session_session_only_target() {
        let error = parse_args(vec![
            String::from("botctl"),
            String::from("continue-session"),
            String::from("--session"),
            String::from("demo"),
        ])
        .expect_err("continue-session should reject ambiguous session-only targets");

        assert!(
            error
                .to_string()
                .contains("continue-session requires --pane %ID|session:window.pane or --session NAME --window NAME")
        );
    }

    #[test]
    fn parses_auto_unstick_command() {
        let command = parse_args(vec![
            String::from("botctl"),
            String::from("auto-unstick"),
            String::from("--pane"),
            String::from("%9"),
            String::from("--max-steps"),
            String::from("4"),
        ])
        .expect("auto-unstick command should parse");

        match command {
            Command::AutoUnstick(args) => {
                assert_eq!(args.target.pane_id.as_deref(), Some("%9"));
                assert_eq!(args.max_steps, 4);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_keep_going_command() {
        let command = parse_args(vec![
            String::from("botctl"),
            String::from("keep-going"),
            String::from("--pane"),
            String::from("%9"),
            String::from("--poll-ms"),
            String::from("400"),
        ])
        .expect("keep-going command should parse");

        match command {
            Command::KeepGoing(args) => {
                assert_eq!(args.target.pane_id.as_deref(), Some("%9"));
                assert_eq!(args.poll_ms, 400);
                assert_eq!(args.submit_delay_ms, 250);
                assert!(!args.no_yolo);
                assert!(args.state_dir.is_none());
                assert!(args.prompt_source.is_none());
                assert!(args.prompt_text.is_none());
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_keep_going_no_yolo_command() {
        let command = parse_args(vec![
            String::from("botctl"),
            String::from("keep-going"),
            String::from("--session"),
            String::from("demo"),
            String::from("--window"),
            String::from("claude"),
            String::from("--no-yolo"),
            String::from("--submit-delay-ms"),
            String::from("600"),
            String::from("--state-dir"),
            String::from("/tmp/state"),
        ])
        .expect("keep-going no-yolo command should parse");

        match command {
            Command::KeepGoing(args) => {
                assert_eq!(args.target.session_name.as_deref(), Some("demo"));
                assert_eq!(args.target.window_name.as_deref(), Some("claude"));
                assert!(args.no_yolo);
                assert_eq!(args.submit_delay_ms, 600);
                assert_eq!(args.state_dir, Some(std::path::PathBuf::from("/tmp/state")));
                assert!(args.prompt_source.is_none());
                assert!(args.prompt_text.is_none());
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_keep_going_custom_prompt_source() {
        let command = parse_args(vec![
            String::from("botctl"),
            String::from("keep-going"),
            String::from("--pane"),
            String::from("%9"),
            String::from("--source"),
            String::from("/tmp/loop-prompt.txt"),
        ])
        .expect("keep-going custom prompt command should parse");

        match command {
            Command::KeepGoing(args) => {
                assert_eq!(args.target.pane_id.as_deref(), Some("%9"));
                assert_eq!(
                    args.prompt_source,
                    Some(std::path::PathBuf::from("/tmp/loop-prompt.txt"))
                );
                assert!(args.prompt_text.is_none());
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_keep_going_custom_prompt_text() {
        let command = parse_args(vec![
            String::from("botctl"),
            String::from("keep-going"),
            String::from("--pane"),
            String::from("%9"),
            String::from("--text"),
            String::from("Custom loop"),
        ])
        .expect("keep-going custom prompt text should parse");

        match command {
            Command::KeepGoing(args) => {
                assert_eq!(args.target.pane_id.as_deref(), Some("%9"));
                assert!(args.prompt_source.is_none());
                assert_eq!(args.prompt_text.as_deref(), Some("Custom loop"));
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn rejects_keep_going_multiple_prompt_inputs() {
        let error = parse_args(vec![
            String::from("botctl"),
            String::from("keep-going"),
            String::from("--pane"),
            String::from("%9"),
            String::from("--source"),
            String::from("/tmp/loop-prompt.txt"),
            String::from("--text"),
            String::from("continue"),
        ])
        .expect_err("keep-going should reject multiple prompt inputs");

        assert!(
            error
                .to_string()
                .contains("prompt input must use either --source PATH or --text TEXT, not both")
        );
    }

    #[test]
    fn rejects_keep_going_zero_poll_interval() {
        let error = parse_args(vec![
            String::from("botctl"),
            String::from("keep-going"),
            String::from("--pane"),
            String::from("%9"),
            String::from("--poll-ms"),
            String::from("0"),
        ])
        .expect_err("keep-going should reject zero poll interval");

        assert!(
            error
                .to_string()
                .contains("keep-going requires --poll-ms to be at least 1")
        );
    }

    #[test]
    fn rejects_keep_going_zero_submit_delay() {
        let error = parse_args(vec![
            String::from("botctl"),
            String::from("keep-going"),
            String::from("--pane"),
            String::from("%9"),
            String::from("--submit-delay-ms"),
            String::from("0"),
        ])
        .expect_err("keep-going should reject zero submit delay");

        assert!(
            error
                .to_string()
                .contains("keep-going requires --submit-delay-ms to be at least 1")
        );
    }

    #[test]
    fn rejects_keep_going_session_only_target() {
        let error = parse_args(vec![
            String::from("botctl"),
            String::from("keep-going"),
            String::from("--session"),
            String::from("demo"),
        ])
        .expect_err("keep-going should reject ambiguous session-only targets");

        assert!(error.to_string().contains(
            "keep-going requires --pane %ID|session:window.pane or --session NAME --window NAME"
        ));
    }

    #[test]
    fn rejects_submit_prompt_zero_submit_delay() {
        let error = parse_args(vec![
            String::from("botctl"),
            String::from("submit-prompt"),
            String::from("--session"),
            String::from("demo"),
            String::from("--pane"),
            String::from("%7"),
            String::from("--text"),
            String::from("hello"),
            String::from("--submit-delay-ms"),
            String::from("0"),
        ])
        .expect_err("submit-prompt should reject zero submit delay");

        assert!(
            error
                .to_string()
                .contains("submit-prompt requires --submit-delay-ms to be at least 1")
        );
    }

    #[test]
    fn rejects_auto_unstick_zero_steps() {
        let error = parse_args(vec![
            String::from("botctl"),
            String::from("auto-unstick"),
            String::from("--pane"),
            String::from("%9"),
            String::from("--max-steps"),
            String::from("0"),
        ])
        .expect_err("auto-unstick should reject zero max steps");

        assert!(
            error
                .to_string()
                .contains("auto-unstick requires --max-steps to be at least 1")
        );
    }

    #[test]
    fn parses_yolo_pane_start_command() {
        let command = parse_args(vec![
            String::from("botctl"),
            String::from("yolo"),
            String::from("start"),
            String::from("--pane"),
            String::from("%9"),
            String::from("--poll-ms"),
            String::from("250"),
        ])
        .expect("yolo start should parse");

        match command {
            Command::PermissionBabysitStart(args) => {
                assert_eq!(args.pane_id.as_deref(), Some("%9"));
                assert!(!args.all);
                assert_eq!(args.poll_ms, 250);
                assert!(!args.live_preview);
                assert_eq!(args.format, super::BabysitFormat::Human);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_yolo_all_and_jsonl_format() {
        let command = parse_args(vec![
            String::from("botctl"),
            String::from("yolo"),
            String::from("--all"),
            String::from("--format"),
            String::from("jsonl"),
        ])
        .expect("yolo all should parse");

        match command {
            Command::PermissionBabysitStart(args) => {
                assert!(args.all);
                assert!(args.pane_id.is_none());
                assert_eq!(args.format, super::BabysitFormat::Jsonl);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_yolo_foreground_command_without_subcommand() {
        let command = parse_args(vec![
            String::from("botctl"),
            String::from("yolo"),
            String::from("--pane"),
            String::from("%9"),
        ])
        .expect("yolo foreground mode should parse");

        match command {
            Command::PermissionBabysitStart(args) => {
                assert_eq!(args.pane_id.as_deref(), Some("%9"));
                assert!(!args.all);
                assert_eq!(args.poll_ms, 1000);
                assert!(!args.live_preview);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_yolo_live_preview_flag() {
        let command = parse_args(vec![
            String::from("botctl"),
            String::from("yolo"),
            String::from("--pane"),
            String::from("%9"),
            String::from("--live-preview"),
        ])
        .expect("yolo live preview should parse");

        match command {
            Command::PermissionBabysitStart(args) => {
                assert_eq!(args.pane_id.as_deref(), Some("%9"));
                assert!(args.live_preview);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn rejects_yolo_zero_poll_interval() {
        let error = parse_args(vec![
            String::from("botctl"),
            String::from("yolo"),
            String::from("start"),
            String::from("--pane"),
            String::from("%9"),
            String::from("--poll-ms"),
            String::from("0"),
        ])
        .expect_err("yolo should reject zero poll interval");

        assert!(
            error
                .to_string()
                .contains("yolo start requires --poll-ms to be at least 1")
        );
    }

    #[test]
    fn rejects_yolo_missing_target() {
        let error = parse_args(vec![
            String::from("botctl"),
            String::from("yolo"),
            String::from("start"),
        ])
        .expect_err("yolo should reject missing target");

        assert!(
            error
                .to_string()
                .contains("requires exactly one of --pane or --all")
        );
    }

    #[test]
    fn rejects_yolo_invalid_combination() {
        let error = parse_args(vec![
            String::from("botctl"),
            String::from("yolo"),
            String::from("start"),
            String::from("--pane"),
            String::from("%9"),
            String::from("--all"),
        ])
        .expect_err("yolo should reject mixed targets");
        assert!(
            error
                .to_string()
                .contains("requires exactly one of --pane or --all")
        );
    }

    #[test]
    fn parses_yolo_stop_command() {
        let command = parse_args(vec![
            String::from("botctl"),
            String::from("yolo"),
            String::from("stop"),
            String::from("--all"),
        ])
        .expect("yolo stop should parse");

        match command {
            Command::PermissionBabysitStop(args) => assert!(args.all),
            other => panic!("unexpected command: {other:?}"),
        }
    }
}
