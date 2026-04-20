use std::path::PathBuf;

use crate::app::{AppError, AppResult};

#[derive(Debug, Clone)]
pub enum Command {
    Start(StartArgs),
    ListPanes,
    Capture(CaptureArgs),
    Status(StatusArgs),
    Doctor(DoctorArgs),
    Observe(ObserveArgs),
    RecordFixture(RecordFixtureArgs),
    Classify(ClassifyArgs),
    Replay(ReplayArgs),
    Bindings,
    InstallBindings(InstallBindingsArgs),
    SendAction(SendActionArgs),
    ApprovePermission(PaneCommandArgs),
    RejectPermission(PaneCommandArgs),
    DismissSurvey(PaneCommandArgs),
    PreparePrompt(PreparePromptArgs),
    EditorHelper(EditorHelperArgs),
    SubmitPrompt(SubmitPromptArgs),
    Help,
}

#[derive(Debug, Clone)]
pub struct StartArgs {
    pub session_name: String,
    pub window_name: String,
    pub cwd: Option<PathBuf>,
    pub command: String,
    pub dry_run: bool,
}

#[derive(Debug, Clone)]
pub struct CaptureArgs {
    pub pane_id: String,
    pub history_lines: usize,
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
}

#[derive(Debug, Clone)]
pub struct InstallBindingsArgs {
    pub path: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct PreparePromptArgs {
    pub session_name: String,
    pub state_dir: Option<PathBuf>,
    pub source: Option<PathBuf>,
    pub text: Option<String>,
}

#[derive(Debug, Clone)]
pub struct EditorHelperArgs {
    pub session_name: String,
    pub state_dir: Option<PathBuf>,
    pub source: Option<PathBuf>,
    pub target: PathBuf,
    pub keep_pending: bool,
}

#[derive(Debug, Clone)]
pub struct SubmitPromptArgs {
    pub session_name: String,
    pub pane_id: String,
    pub state_dir: Option<PathBuf>,
    pub source: Option<PathBuf>,
    pub text: Option<String>,
    pub submit_delay_ms: u64,
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
        "list-panes" => Ok(Command::ListPanes),
        "capture" => parse_capture(rest),
        "status" => parse_status(rest),
        "doctor" => parse_doctor(rest),
        "observe" => parse_observe(rest),
        "record-fixture" => parse_record_fixture(rest),
        "classify" => parse_classify(rest),
        "replay" => parse_replay(rest),
        "bindings" => Ok(Command::Bindings),
        "install-bindings" => parse_install_bindings(rest),
        "send-action" => parse_send_action(rest),
        "approve-permission" => {
            parse_pane_command(rest, "approve-permission", Command::ApprovePermission)
        }
        "reject-permission" => {
            parse_pane_command(rest, "reject-permission", Command::RejectPermission)
        }
        "dismiss-survey" => parse_pane_command(rest, "dismiss-survey", Command::DismissSurvey),
        "prepare-prompt" => parse_prepare_prompt(rest),
        "editor-helper" => parse_editor_helper(rest),
        "submit-prompt" => parse_submit_prompt(rest),
        "help" | "--help" | "-h" => Ok(Command::Help),
        other => Err(AppError::new(format!("unknown subcommand: {other}"))),
    }
}

pub fn usage() -> String {
    String::from(
        "sdmux\n\
          \n\
          Commands:\n\
            start --session NAME [--window NAME] [--cwd PATH] [--command CMD] [--dry-run]\n\
            list-panes\n\
            capture --pane %ID [--history-lines N]\n\
            status --pane %ID [--history-lines N]\n\
            doctor [--session NAME] [--pane %ID] [--history-lines N] [--bindings-path PATH]\n\
            observe --session NAME [--pane %ID] [--events N] [--idle-timeout-ms N] [--history-lines N]\n\
            record-fixture --session NAME --case NAME [--pane %ID] [--output-dir PATH] [--expected-state STATE] [--events N] [--idle-timeout-ms N] [--history-lines N]\n\
            classify --path PATH\n\
            replay --path PATH\n\
            bindings\n\
            install-bindings [--path PATH]\n\
            send-action --pane %ID --action NAME\n\
            approve-permission --pane %ID\n\
            reject-permission --pane %ID\n\
            dismiss-survey --pane %ID\n\
            prepare-prompt --session NAME [--state-dir PATH] [--source PATH | --text TEXT]\n\
            editor-helper --session NAME [--state-dir PATH] [--source PATH] [--keep-pending] TARGET\n\
            submit-prompt --session NAME --pane %ID [--state-dir PATH] [--source PATH | --text TEXT] [--submit-delay-ms N]\n",
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
            "doctor requires either --pane %ID or --session NAME",
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
    Ok(command(PaneCommandArgs { pane_id }))
}

fn parse_prepare_prompt(args: Vec<String>) -> AppResult<Command> {
    let mut session_name = None;
    let mut state_dir = None;
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
        source,
        text,
    }))
}

fn parse_editor_helper(args: Vec<String>) -> AppResult<Command> {
    let mut session_name = None;
    let mut state_dir = None;
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
        source,
        target,
        keep_pending,
    }))
}

fn parse_submit_prompt(args: Vec<String>) -> AppResult<Command> {
    let mut session_name = None;
    let mut pane_id = None;
    let mut state_dir = None;
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

    Ok(Command::SubmitPrompt(SubmitPromptArgs {
        session_name,
        pane_id,
        state_dir,
        source,
        text,
        submit_delay_ms,
    }))
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

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{Command, parse_args};

    #[test]
    fn parses_start_command() {
        let command = parse_args(vec![
            String::from("sdmux"),
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
    fn parses_observe_command() {
        let command = parse_args(vec![
            String::from("sdmux"),
            String::from("observe"),
            String::from("--session"),
            String::from("demo"),
            String::from("--events"),
            String::from("10"),
        ])
        .expect("observe command should parse");

        match command {
            Command::Observe(args) => {
                assert_eq!(args.session_name, "demo");
                assert_eq!(args.events, 10);
                assert_eq!(args.idle_timeout_ms, 1500);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_status_command() {
        let command = parse_args(vec![
            String::from("sdmux"),
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
    fn parses_doctor_command_with_session() {
        let command = parse_args(vec![
            String::from("sdmux"),
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
            String::from("sdmux"),
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
            String::from("sdmux"),
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
    fn parses_approve_permission_command() {
        let command = parse_args(vec![
            String::from("sdmux"),
            String::from("approve-permission"),
            String::from("--pane"),
            String::from("%9"),
        ])
        .expect("approve-permission command should parse");

        match command {
            Command::ApprovePermission(args) => {
                assert_eq!(args.pane_id, "%9");
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }
}
