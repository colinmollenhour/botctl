use std::path::PathBuf;

use crate::app::{AppError, AppResult};

#[derive(Debug, Clone)]
pub enum Command {
    Start(StartArgs),
    ListPanes,
    Capture(CaptureArgs),
    Observe(ObserveArgs),
    Classify(ClassifyArgs),
    Replay(ReplayArgs),
    Bindings,
    SendAction(SendActionArgs),
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
pub struct ObserveArgs {
    pub session_name: String,
    pub pane_id: Option<String>,
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
        "observe" => parse_observe(rest),
        "classify" => parse_classify(rest),
        "replay" => parse_replay(rest),
        "bindings" => Ok(Command::Bindings),
        "send-action" => parse_send_action(rest),
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
           observe --session NAME [--pane %ID] [--events N] [--idle-timeout-ms N] [--history-lines N]\n\
           classify --path PATH\n\
           replay --path PATH\n\
           bindings\n\
           send-action --pane %ID --action NAME\n",
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

#[cfg(test)]
mod tests {
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
}
