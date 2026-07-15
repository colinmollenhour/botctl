use std::io::{self, IsTerminal};
use std::path::PathBuf;

use crate::app::{AppError, AppResult};

#[derive(Debug, Clone)]
pub enum Command {
    Start(StartArgs),
    Attach(AttachArgs),
    ListPanes(ListPanesArgs),
    Capture(CaptureArgs),
    LastMessage(LastMessageArgs),
    Status(StatusArgs),
    Doctor(DoctorArgs),
    Observe(ObserveArgs),
    Runtime(RuntimeArgs),
    Serve(ServeArgs),
    Dashboard(DashboardArgs),
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
    Prompt(PromptRunArgs),
    Mcp(McpArgs),
    PreparePrompt(PreparePromptArgs),
    EditorHelper(EditorHelperArgs),
    SubmitPrompt(SubmitPromptArgs),
    YoloStart(YoloStartArgs),
    YoloStop(YoloStopArgs),
    Version,
    Help(HelpArgs),
}

#[derive(Debug, Clone)]
pub struct HelpArgs {
    pub topic: Option<String>,
    pub color: bool,
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
    pub plain: bool,
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
pub struct PromptRunArgs {
    pub session_name: Option<String>,
    pub window_name: String,
    pub cwd: Option<PathBuf>,
    pub command: String,
    pub sources: Vec<PathBuf>,
    pub text: Option<String>,
    pub stdin: bool,
    pub append_system_prompts: Vec<PathBuf>,
    pub poll_ms: u64,
    pub submit_delay_ms: u64,
    pub ready_timeout_ms: u64,
    pub idle_timeout_ms: u64,
    pub state_dir: Option<PathBuf>,
    pub workspace: Option<String>,
    pub large_prompt_threshold: usize,
    pub keep_temp: bool,
    pub no_yolo: bool,
    pub verbose: bool,
    pub claude_args: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpTransportArgs {
    Stdio,
    Http {
        bind: String,
        allow_non_loopback: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpArgs {
    pub transport: McpTransportArgs,
    pub state_dir: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct CaptureArgs {
    pub pane_id: String,
    pub history_lines: usize,
}

#[derive(Debug, Clone)]
pub struct LastMessageArgs {
    pub pane_id: String,
    pub out: Option<PathBuf>,
    pub history_lines: usize,
}

#[derive(Debug, Clone)]
pub struct ListPanesArgs {
    pub all: bool,
    pub json: bool,
    pub plain: bool,
}

#[derive(Debug, Clone)]
pub struct StatusArgs {
    pub pane_id: String,
    pub history_lines: usize,
    pub json: bool,
    pub plain: bool,
}

#[derive(Debug, Clone)]
pub struct DoctorArgs {
    pub session_name: Option<String>,
    pub pane_id: Option<String>,
    pub history_lines: usize,
    pub bindings_path: Option<PathBuf>,
    pub json: bool,
    pub plain: bool,
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
pub struct RuntimeArgs {
    pub foreground: bool,
    pub stop: bool,
    pub reconcile_ms: u64,
    pub history_lines: usize,
    pub state_dir: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct ServeArgs {
    pub session_name: String,
    pub pane_id: Option<String>,
    pub reconcile_ms: u64,
    pub history_lines: usize,
    pub http_addr: Option<String>,
    pub allowed_origins: Vec<String>,
    pub format: BabysitFormat,
    pub state_dir: Option<PathBuf>,
    pub unmanaged: bool,
}

#[derive(Debug, Clone)]
pub struct DashboardArgs {
    pub poll_ms: u64,
    pub history_lines: usize,
    pub state_dir: Option<PathBuf>,
    pub exit_on_navigate: bool,
    pub persistent: bool,
    pub unmanaged: bool,
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
pub struct YoloStartArgs {
    pub pane_id: Option<String>,
    pub all: bool,
    pub poll_ms: u64,
    pub live_preview: bool,
    pub follow: bool,
    pub format: BabysitFormat,
    pub state_dir: Option<PathBuf>,
    pub workspace: Option<String>,
    pub unmanaged: bool,
}

#[derive(Debug, Clone)]
pub struct YoloStopArgs {
    pub pane_id: Option<String>,
    pub all: bool,
    pub state_dir: Option<PathBuf>,
    pub workspace: Option<String>,
    pub unmanaged: bool,
}

pub fn parse_args<I>(args: I) -> AppResult<Command>
where
    I: IntoIterator<Item = String>,
{
    let mut iter = args.into_iter();
    let _program = iter.next();
    let Some(subcommand) = iter.next() else {
        return Ok(Command::Help(HelpArgs {
            topic: None,
            color: supports_color(io::stdout().is_terminal()),
        }));
    };

    let mut rest: Vec<String> = iter.collect();
    let no_color = subcommand == "--no-color" || rest.iter().any(|arg| arg == "--no-color");
    rest.retain(|arg| arg != "--no-color");
    let color = !no_color && supports_color(io::stdout().is_terminal());
    let subcommand = if subcommand == "--no-color" {
        let Some(next) = rest.first().cloned() else {
            return Ok(Command::Help(HelpArgs { topic: None, color }));
        };
        rest.remove(0);
        next
    } else {
        subcommand
    };

    if subcommand == "--version" || subcommand == "-V" {
        return Ok(Command::Version);
    }
    if subcommand == "--help" || subcommand == "-h" {
        return Ok(Command::Help(HelpArgs { topic: None, color }));
    }
    if let Some(index) = rest.iter().position(|arg| arg == "--help" || arg == "-h") {
        let topic = if subcommand == "help" {
            rest.iter()
                .enumerate()
                .find(|(i, _)| *i != index)
                .map(|(_, arg)| arg.clone())
        } else if rest.first().map(String::as_str) == Some("stop") && subcommand == "yolo" {
            Some(String::from("yolo stop"))
        } else {
            Some(subcommand.clone())
        };
        return Ok(Command::Help(HelpArgs { topic, color }));
    }

    match subcommand.as_str() {
        "start" => with_command_context(parse_start(rest), "start"),
        "attach" => with_command_context(parse_attach(rest), "attach"),
        "list" => with_command_context(parse_list_panes(rest), "list"),
        "capture" => with_command_context(parse_capture(rest), "capture"),
        "last-message" => with_command_context(parse_last_message(rest), "last-message"),
        "status" => with_command_context(parse_status(rest), "status"),
        "doctor" => with_command_context(parse_doctor(rest), "doctor"),
        "observe" => with_command_context(parse_observe(rest), "observe"),
        "runtime" => with_command_context(parse_runtime(rest), "runtime"),
        "serve" => with_command_context(parse_serve(rest), "serve"),
        "dashboard" => with_command_context(parse_dashboard(rest), "dashboard"),
        "record-fixture" => with_command_context(parse_record_fixture(rest), "record-fixture"),
        "classify" => with_command_context(parse_classify(rest), "classify"),
        "replay" => with_command_context(parse_replay(rest), "replay"),
        "bindings" => Ok(Command::Bindings),
        "install-bindings" => {
            with_command_context(parse_install_bindings(rest), "install-bindings")
        }
        "send-action" => with_command_context(parse_send_action(rest), "send-action"),
        "approve" | "approve-permission" => with_command_context(
            parse_approve_reject(rest, Command::ApprovePermission),
            "approve",
        ),
        "reject" | "reject-permission" => with_command_context(
            parse_approve_reject(rest, Command::RejectPermission),
            "reject",
        ),
        "dismiss-survey" => with_command_context(
            parse_pane_command(rest, "dismiss-survey", Command::DismissSurvey),
            "dismiss-survey",
        ),
        "continue-session" => {
            with_command_context(parse_continue_session(rest), "continue-session")
        }
        "auto-unstick" => with_command_context(parse_auto_unstick(rest), "auto-unstick"),
        "keep-going" => with_command_context(parse_keep_going(rest), "keep-going"),
        "prompt" => with_command_context(parse_prompt_run(rest), "prompt"),
        "mcp" => with_command_context(parse_mcp(rest), "mcp"),
        "prepare-prompt" => with_command_context(parse_prepare_prompt(rest), "prepare-prompt"),
        "editor-helper" => with_command_context(parse_editor_helper(rest), "editor-helper"),
        "submit-prompt" => with_command_context(parse_submit_prompt(rest), "submit-prompt"),
        "yolo" => with_command_context(parse_yolo(rest), "yolo"),
        "help" => Ok(Command::Help(HelpArgs {
            topic: rest.first().cloned(),
            color,
        })),
        other => Err(AppError::with_exit_code(
            render_unknown_command_error(other),
            2,
        )),
    }
}

fn with_command_context(result: AppResult<Command>, command: &str) -> AppResult<Command> {
    result.map_err(|error| {
        AppError::with_exit_code(
            format!("{error}\n\nTry `botctl {command} --help` for examples."),
            2,
        )
    })
}

pub fn usage() -> String {
    usage_with_color(supports_color(io::stdout().is_terminal()))
}

pub fn usage_for(args: &HelpArgs) -> String {
    match args.topic.as_deref() {
        None => usage_with_color(args.color),
        Some(topic) => command_usage(topic, args.color).unwrap_or_else(|| {
            format!("Unknown help topic: {topic}\n\nTry `botctl help` to list commands.")
        }),
    }
}

pub fn error_hint(message: &str) -> String {
    if message.contains("\nTry `botctl") {
        return String::new();
    }
    if let Some(command) = command_from_error(message) {
        format!("Try `botctl {command} --help` for examples.")
    } else {
        String::from("Try `botctl help` to list commands.")
    }
}

pub fn version() -> String {
    format!("botctl {}", env!("CARGO_PKG_VERSION"))
}

fn usage_with_color(color: bool) -> String {
    const RESET: &str = "\x1b[0m";
    const BOLD: &str = "\x1b[1m";
    const DIM: &str = "\x1b[2m";
    const BRIGHT_CYAN: &str = "\x1b[96m";
    const BRIGHT_YELLOW: &str = "\x1b[93m";
    const BRIGHT_WHITE: &str = "\x1b[97m";

    let style = |text: &str, before: &str| {
        if color {
            format!("{before}{text}{RESET}")
        } else {
            text.to_string()
        }
    };
    let title = style("botctl", &format!("{BOLD}{BRIGHT_WHITE}"));
    let subtitle = style("  Claude Code tmux automation for live sessions", DIM);
    let section = |name: &str| style(name, &format!("{BOLD}{BRIGHT_CYAN}"));
    let featured = |text: &str| format!("  {}", style(text, &format!("{BOLD}{BRIGHT_YELLOW}")));
    let command = |text: &str| format!("  {text}");

    let mut out = String::new();
    out.push_str(&title);
    out.push('\n');
    out.push_str(&subtitle);
    out.push_str("\n\n");

    out.push_str(&section("Usage:"));
    out.push_str("\n  botctl <command> [options]\n\n");

    out.push_str(&section("Canonical Names:"));
    out.push_str(
        "\n  Use `yolo`, `approve`, `reject`, and `dismiss-survey` in new docs and scripts.\n",
    );
    out.push_str("  Compatibility aliases: `approve-permission` -> `approve`, `reject-permission` -> `reject`.\n\n");

    out.push_str(&section("Main Commands:"));
    out.push('\n');
    out.push_str(&featured(
        "runtime [--foreground] [--reconcile-ms N] [--history-lines N] [--state-dir PATH] | runtime stop [--state-dir PATH]",
    ));
    out.push_str("\n    Start the central local runtime, by default in a hidden tmux session, or stop it.\n\n");
    out.push_str(&featured(
        "yolo [start] (--pane %ID|session:window.pane | --all) [--follow] [--poll-ms N] [--format human|jsonl] [--live-preview] [--state-dir PATH] [--workspace PATH|UUID] [--unmanaged]",
    ));
    out.push_str("\n    Set central yolo policy for one pane or all panes, optionally tailing runtime events.\n\n");
    out.push_str(&featured(
        "yolo stop (--pane %ID|session:window.pane | --all) [--state-dir PATH] [--workspace PATH|UUID] [--unmanaged]",
    ));
    out.push_str("\n    Stop autonomous babysitting.\n\n");
    out.push_str(&featured(
        "dashboard [--poll-ms N] [--history-lines N] [--state-dir PATH] [--exit-on-navigate] [--persistent] [--unmanaged]",
    ));
    out.push_str(
        "\n    Open the TUI for live Claude panes, state, wait times, and yolo toggles.\n",
    );
    out.push_str(
        "    With --persistent, keep the dashboard alive in a dedicated tmux session and reopen it in a popup.\n\n",
    );
    out.push_str(&featured(
        "serve --session NAME [--pane %ID|session:window.pane] [--reconcile-ms N] [--history-lines N] [--http ADDR] [--allowed-origin URL] [--format human|jsonl] [--state-dir PATH] [--unmanaged]",
    ));
    out.push_str("\n    Continuously inspect a Claude session and emit babysit output.\n\n");

    out.push_str(&section("Common Commands:"));
    out.push('\n');
    for line in [
        "status --pane %ID|session:window.pane [--history-lines N] [--json | --plain]",
        "last-message --pane %ID|session:window.pane [--out PATH] [--history-lines N]",
        "doctor [--session NAME] [--pane %ID|session:window.pane] [--history-lines N] [--bindings-path PATH] [--json | --plain]",
        "continue-session (--pane %ID|session:window.pane | --session NAME --window NAME)",
        "auto-unstick (--pane %ID|session:window.pane | --session NAME --window NAME) [--max-steps N]",
        "keep-going (--pane %ID|session:window.pane | --session NAME --window NAME) [--poll-ms N] [--submit-delay-ms N] [--state-dir PATH] [--source PATH | --text TEXT] [--no-yolo]",
        "approve --pane %ID|session:window.pane [--format human|jsonl]  (alias: approve-permission)",
        "reject --pane %ID|session:window.pane [--format human|jsonl]  (alias: reject-permission)",
        "dismiss-survey --pane %ID|session:window.pane",
    ] {
        out.push_str(&command(line));
        out.push('\n');
    }
    out.push('\n');

    out.push_str(&section("Session Setup:"));
    out.push('\n');
    for line in [
        "start --session NAME [--window NAME] [--cwd PATH] [--command CMD] [--dry-run]",
        "attach (--pane %ID|session:window.pane | --session NAME [--window NAME]) [--plain]",
        "list [--all] [--json | --plain]",
        "capture --pane %ID|session:window.pane [--history-lines N]",
        "observe --session NAME [--pane %ID|session:window.pane] [--events N] [--idle-timeout-ms N] [--history-lines N] [--state-dir PATH]",
    ] {
        out.push_str(&command(line));
        out.push('\n');
    }
    out.push('\n');

    out.push_str(&section("Prompt Workflow:"));
    out.push('\n');
    for line in [
        "prompt [--text TEXT] [--source PATH ...] [--stdin] [--append-system-prompt PATH ...] [--cwd PATH] [--no-yolo] [--verbose] [-- CLAUDE_ARG ...]",
        "prepare-prompt --session NAME [--state-dir PATH] [--workspace PATH|UUID] [--source PATH | --text TEXT]",
        "editor-helper --session NAME [--state-dir PATH] [--workspace PATH|UUID] [--source PATH] [--keep-pending] TARGET  [advanced]",
        "submit-prompt --session NAME --pane %ID|session:window.pane [--state-dir PATH] [--workspace PATH|UUID] [--source PATH | --text TEXT] [--submit-delay-ms N]",
    ] {
        out.push_str(&command(line));
        out.push('\n');
    }
    out.push('\n');

    out.push_str(&section("MCP:"));
    out.push('\n');
    for line in [
        "mcp stdio [--state-dir PATH]",
        "mcp http --bind 127.0.0.1:8787 [--allow-non-loopback] [--state-dir PATH]",
    ] {
        out.push_str(&command(line));
        out.push('\n');
    }
    out.push('\n');

    out.push_str(&section("Advanced Diagnostics And Plumbing:"));
    out.push('\n');
    for line in [
        "record-fixture --session NAME --case NAME [--pane %ID|session:window.pane] [--output-dir PATH] [--expected-state STATE] [--events N] [--idle-timeout-ms N] [--history-lines N]  [advanced]",
        "classify --path PATH  [advanced]",
        "replay --path PATH  [advanced]",
        "bindings",
        "install-bindings [--path PATH]",
        "send-action --pane %ID|session:window.pane --action NAME  [advanced]",
    ] {
        out.push_str(&command(line));
        out.push('\n');
    }
    out.push('\n');

    out.push_str(&section("Quick Start:"));
    out.push('\n');
    for line in [
        "botctl runtime",
        "botctl dashboard",
        "botctl prompt --text \"Summarize this repo\"",
        "botctl yolo --pane 0:6.0",
        "botctl serve --session my-session --format jsonl",
    ] {
        out.push_str(&featured(line));
        out.push('\n');
    }
    out.push('\n');

    out.push_str(&section("Help Topics:"));
    out.push_str(
        "\n  botctl help targeting\n  botctl help safety\n  botctl help json\n  botctl help state-dir\n  botctl help dashboard-keys\n  botctl help opencode\n  botctl help exit-codes\n",
    );

    out
}

fn supports_color(stream_is_tty: bool) -> bool {
    stream_is_tty
        && std::env::var_os("NO_COLOR").is_none()
        && std::env::var("TERM")
            .map(|term| term != "dumb")
            .unwrap_or(true)
}

fn render_unknown_command_error(command: &str) -> String {
    let suggestion = closest_command(command)
        .map(|candidate| format!("\n\nDid you mean `{candidate}`?"))
        .unwrap_or_default();
    format!("Unknown command: {command}{suggestion}\nTry `botctl help` to list commands.")
}

fn closest_command(command: &str) -> Option<&'static str> {
    let commands = [
        "start",
        "attach",
        "list",
        "capture",
        "last-message",
        "status",
        "doctor",
        "observe",
        "runtime",
        "serve",
        "dashboard",
        "record-fixture",
        "classify",
        "replay",
        "bindings",
        "install-bindings",
        "send-action",
        "approve",
        "reject",
        "dismiss-survey",
        "continue-session",
        "auto-unstick",
        "keep-going",
        "prompt",
        "prepare-prompt",
        "editor-helper",
        "submit-prompt",
        "mcp",
        "yolo",
        "help",
    ];
    commands
        .iter()
        .copied()
        .filter(|candidate| command_distance(command, candidate) <= 3)
        .min_by_key(|candidate| command_distance(command, candidate))
}

fn command_distance(left: &str, right: &str) -> usize {
    let mut previous = (0..=right.len()).collect::<Vec<_>>();
    for (i, left_char) in left.chars().enumerate() {
        let mut current = vec![i + 1];
        for (j, right_char) in right.chars().enumerate() {
            let cost = usize::from(left_char != right_char);
            current.push(
                (previous[j + 1] + 1)
                    .min(current[j] + 1)
                    .min(previous[j] + cost),
            );
        }
        previous = current;
    }
    previous[right.len()]
}

fn command_from_error(message: &str) -> Option<&'static str> {
    [
        "start",
        "attach",
        "list",
        "capture",
        "last-message",
        "status",
        "doctor",
        "observe",
        "runtime",
        "serve",
        "dashboard",
        "record-fixture",
        "classify",
        "replay",
        "install-bindings",
        "send-action",
        "dismiss-survey",
        "continue-session",
        "auto-unstick",
        "keep-going",
        "prompt",
        "prepare-prompt",
        "editor-helper",
        "submit-prompt",
        "yolo",
    ]
    .into_iter()
    .find(|command| message.contains(command))
}

fn command_usage(topic: &str, color: bool) -> Option<String> {
    let (name, purpose, usage, examples, options, safety) = match topic {
        "dashboard" => (
            "dashboard",
            "Open live TUI across Claude, Codex, OpenCode, Pi, Grok, and Antigravity panes.",
            "botctl dashboard [--poll-ms N] [--history-lines N] [--state-dir PATH] [--exit-on-navigate] [--persistent] [--unmanaged]",
            &[
                "botctl dashboard",
                "botctl dashboard --persistent",
                "botctl dashboard --unmanaged",
            ][..],
            &[
                "--poll-ms N (default: 1000)",
                "--history-lines N (default: 120)",
                "--state-dir PATH",
                "--exit-on-navigate",
                "--persistent",
                "--unmanaged (require an already-running runtime)",
                "--no-color",
            ][..],
            "TUI owns terminal directly. No pane automation runs until operator chooses action.",
        ),
        "runtime" => (
            "runtime",
            "Start, stop, or foreground the central local runtime.",
            "botctl runtime [--foreground] [--reconcile-ms N] [--history-lines N] [--state-dir PATH] | botctl runtime stop [--state-dir PATH]",
            &[
                "botctl runtime",
                "botctl runtime --foreground",
                "botctl runtime stop",
            ][..],
            &[
                "--foreground (run in this process instead of a managed tmux session)",
                "--reconcile-ms N (default: 1000; maximum dirty-pane delay and, above 10000, full-state safety interval)",
                "--history-lines N (default: 200)",
                "--state-dir PATH",
                "stop (subcommand: request the runtime to stop)",
                "--no-color",
            ][..],
            "The runtime owns the Unix socket at <state-dir>/runtime.sock. Managed clients auto-start it unless they pass --unmanaged.",
        ),
        "yolo" => (
            "yolo",
            "Start autonomous babysitting for one pane or all registered panes. Canonical name: yolo.",
            "botctl yolo [start] (--pane %ID|session:window.pane | --all) [--follow] [--poll-ms N] [--format human|jsonl] [--live-preview] [--state-dir PATH] [--workspace PATH|UUID] [--unmanaged]",
            &[
                "botctl yolo --pane %19",
                "botctl yolo --all --workspace .",
                "botctl yolo --pane %19 --format jsonl",
            ][..],
            &[
                "--pane TARGET",
                "--all",
                "--follow (tail matching runtime events after setting policy)",
                "--poll-ms N (default: 1000)",
                "--format human|jsonl (default: human)",
                "--live-preview",
                "--state-dir PATH",
                "--workspace PATH|UUID",
                "--unmanaged (require an already-running runtime)",
                "--no-color",
            ][..],
            "Uses guarded classifier states before sending keys. Unknown states wait.",
        ),
        "yolo stop" => (
            "yolo stop",
            "Stop autonomous babysitting records.",
            "botctl yolo stop (--pane %ID|session:window.pane | --all) [--state-dir PATH] [--workspace PATH|UUID] [--unmanaged]",
            &[
                "botctl yolo stop --pane %19",
                "botctl yolo stop --all --workspace .",
            ][..],
            &[
                "--pane TARGET",
                "--all",
                "--state-dir PATH",
                "--workspace PATH|UUID",
                "--unmanaged (require an already-running runtime)",
                "--no-color",
            ][..],
            "Does not send keys to panes; removes yolo state records.",
        ),
        "serve" => (
            "serve",
            "Continuously inspect tmux session and emit events.",
            "botctl serve --session NAME [--pane TARGET] [--reconcile-ms N] [--history-lines N] [--http ADDR] [--allowed-origin URL] [--format human|jsonl] [--state-dir PATH] [--unmanaged]",
            &[
                "botctl serve --session demo",
                "botctl serve --session demo --format jsonl",
                "botctl serve --session demo --pane %19",
            ][..],
            &[
                "--session NAME",
                "--pane TARGET",
                "--reconcile-ms N (default: 1500)",
                "--history-lines N (default: 120)",
                "--http ADDR",
                "--allowed-origin URL",
                "--format human|jsonl (default: human)",
                "--state-dir PATH",
                "--unmanaged (require an already-running runtime)",
                "--no-color",
            ][..],
            "JSONL is stable machine stream on stdout. Warnings go to stderr.",
        ),
        "attach" => (
            "attach",
            "Validate and report an existing Claude pane target.",
            "botctl attach (--pane TARGET | --session NAME [--window NAME]) [--plain]",
            &[
                "botctl attach --pane %19",
                "botctl attach --session demo --window claude --plain",
            ][..],
            &[
                "--pane TARGET",
                "--session NAME",
                "--window NAME",
                "--plain",
                "--no-color",
            ][..],
            TARGET_HELP,
        ),
        "list" => (
            "list",
            "List Claude panes, or all tmux panes with --all.",
            "botctl list [--all] [--json | --plain]",
            &[
                "botctl list",
                "botctl list --all --plain",
                "botctl list --json",
            ][..],
            &["--all", "--json", "--plain", "--no-color"][..],
            "Default and --plain output are line-oriented. --json emits a one-shot object on stdout.",
        ),
        "last-message" => (
            "last-message",
            "Dump the latest assistant message from a pane transcript (Claude/Codex/OpenCode/Pi/Grok) or pane scrollback (Antigravity).",
            "botctl last-message --pane %ID|session:window.pane [--out PATH] [--history-lines N]",
            &[
                "botctl last-message --pane %19",
                "botctl last-message --pane 0:2.3 --out last-agent-message.md",
                "botctl last-message --pane %19 --out -",
                "botctl last-message --pane %19 --history-lines 5000",
            ][..],
            &[
                "--pane TARGET",
                "--out PATH",
                "--history-lines N",
                "--no-color",
            ][..],
            "Reads provider transcript storage for Claude, Codex, OpenCode, Pi, Grok (updates.jsonl), and pane-scrape for Antigravity. Without --out, writes MESSAGE_<provider-session-id>.md in the current directory and prints only the line count plus file path. Use --out - to write the markdown body to stdout. --history-lines (default: 2000) widens the captured scrollback for Antigravity pane-scrape extraction; non-agy providers ignore it.",
        ),
        "status" => (
            "status",
            "Classify one pane and show next safe action.",
            "botctl status --pane %ID|session:window.pane [--history-lines N] [--json | --plain]",
            &[
                "botctl status --pane %19",
                "botctl status --pane 0:2.3 --json",
            ][..],
            &[
                "--pane TARGET",
                "--history-lines N (default: 120)",
                "--json",
                "--plain",
                "--no-color",
            ][..],
            TARGET_HELP,
        ),
        "doctor" => (
            "doctor",
            "Inspect one pane plus keybinding readiness.",
            "botctl doctor (--pane TARGET | --session NAME) [--history-lines N] [--bindings-path PATH] [--json | --plain]",
            &[
                "botctl doctor --pane %19",
                "botctl doctor --session demo --json",
            ][..],
            &[
                "--pane TARGET",
                "--session NAME",
                "--history-lines N (default: 120)",
                "--bindings-path PATH",
                "--json",
                "--plain",
                "--no-color",
            ][..],
            TARGET_HELP,
        ),
        "approve" | "approve-permission" => (
            "approve",
            "Approve current Claude permission or folder-trust prompt when safe. Canonical name: approve. Alias: approve-permission.",
            "botctl approve --pane %ID|session:window.pane [--format human|jsonl]",
            &[
                "botctl approve --pane %19",
                "botctl approve --pane %19 --format jsonl",
            ][..],
            &[
                "--pane TARGET",
                "--format human|jsonl (default: human)",
                "--no-color",
            ][..],
            "Sends keys only from PermissionDialog or FolderTrustPrompt. Folder trust uses raw Enter.",
        ),
        "reject" | "reject-permission" => (
            "reject",
            "Reject current Claude permission prompt when safe. Canonical name: reject. Alias: reject-permission.",
            "botctl reject --pane %ID|session:window.pane [--format human|jsonl]",
            &["botctl reject --pane %19"][..],
            &[
                "--pane TARGET",
                "--format human|jsonl (default: human)",
                "--no-color",
            ][..],
            "Sends keys only from PermissionDialog.",
        ),
        "keep-going" => (
            "keep-going",
            "Audit current task progress and submit follow-up work when Claude is ready.",
            "botctl keep-going (--pane TARGET | --session NAME --window NAME) [--poll-ms N] [--submit-delay-ms N] [--state-dir PATH] [--source PATH | --text TEXT] [--no-yolo]",
            &[
                "botctl keep-going --pane %19",
                "botctl keep-going --session demo --window claude --no-yolo",
                "botctl keep-going --pane %19 --text \"Finish only the requested change, then stop.\"",
            ][..],
            &[
                "--pane TARGET",
                "--session NAME --window NAME",
                "--poll-ms N (default: 1000)",
                "--submit-delay-ms N (default: 250)",
                "--state-dir PATH",
                "--source PATH",
                "--text TEXT",
                "--no-yolo",
                "--no-color",
            ][..],
            "Targets must be explicit. Default mode may use yolo state while waiting; --no-yolo refuses blockers instead of auto-recovering. Prompt input must use either --source or --text, not both.",
        ),
        "prompt" => (
            "prompt",
            "Run a one-shot prompt through an observable Claude TUI in tmux.",
            "botctl prompt [--session NAME] [--window NAME] [--cwd PATH] [--command CMD] [--source PATH ...] [--text TEXT] [--stdin] [--append-system-prompt PATH ...] [--poll-ms N] [--submit-delay-ms N] [--ready-timeout-ms N] [--idle-timeout-ms N] [--state-dir PATH] [--workspace PATH|UUID] [--large-prompt-threshold BYTES] [--keep-temp] [--no-yolo] [--verbose] [-- CLAUDE_ARG ...]",
            &[
                "botctl prompt --text \"Summarize this repo\"",
                "botctl prompt --source task.md --append-system-prompt rules.md",
                "cat prompt.md | botctl prompt --stdin --cwd /path/to/project",
                "botctl prompt --source big-plan.md --large-prompt-threshold 10 --keep-temp",
                "botctl prompt --text hi -- --model sonnet --name \"Just testing\"",
            ][..],
            &[
                "--session NAME (default: botctl)",
                "--window NAME (default: claude; each run creates a new window)",
                "--cwd PATH (default: current directory)",
                "--command CMD (default: claude; not claude -p/--prompt)",
                "--source PATH (repeatable)",
                "--text TEXT",
                "--stdin (explicit stdin; implicit when no input flags and stdin is piped)",
                "--append-system-prompt PATH (repeatable)",
                "--poll-ms N (default: 1000)",
                "--submit-delay-ms N (default: 250)",
                "--ready-timeout-ms N (default: 30000)",
                "--idle-timeout-ms N (default: 600000)",
                "--state-dir PATH",
                "--workspace PATH|UUID",
                "--large-prompt-threshold BYTES (default: 8192)",
                "--keep-temp",
                "--no-yolo",
                "--verbose",
                "-- CLAUDE_ARG ... (passed to the interactive Claude command)",
                "--no-color",
            ][..],
            "Launches Claude in a new detached tmux window in the owning session, creating that session first when needed, pastes the resolved prompt into the interactive TUI through tmux, and prints only the latest assistant text to stdout. The default owning session is botctl; --session overrides it. On success, botctl loads a fresh assistant message before killing only the captured prompt window. Failed prompt windows are left alive for inspection. Use --verbose for launch/wait progress on stderr. Arguments after -- are passed to Claude, except prompt/headless mode is refused. Safe blockers may be handled unless --no-yolo is set.",
        ),
        "mcp" => (
            "mcp",
            "Run the botctl MCP JSON-RPC server for persistent agent sessions (stdio, or stateless Streamable-HTTP-compatible POST /mcp). Claude default; Codex and Agy also supported.",
            "botctl mcp stdio [--state-dir PATH] | botctl mcp http --bind 127.0.0.1:8787 [--allow-non-loopback] [--state-dir PATH]",
            &["botctl mcp stdio", "botctl mcp http --bind 127.0.0.1:8787"][..],
            &[
                "stdio (newline-delimited JSON-RPC on stdin/stdout)",
                "http --bind ADDR [--allow-non-loopback] (stateless Streamable-HTTP-compatible JSON-RPC POST /mcp; GET->405, DELETE->204, Origin/Host checks, no SSE)",
                "--state-dir PATH",
                "--no-color",
            ][..],
            "Tools use generated managed IDs and exact tmux pane IDs. Provider-specific spawn tools are advertised only when their binaries are on PATH and validate provider-specific options. one_shot spawns a temporary session, runs one prompt to a terminal outcome, then always attempts to kill the window (best-effort). send_keys is unsafe/operator-only and does not imply progress.",
        ),
        "targeting" => return Some(topic_page("targeting", TARGET_HELP)),
        "safety" => {
            return Some(topic_page(
                "safety",
                "Unknown refuses automation. Explicit pane IDs are safest. Guarded workflows classify before sending any key. Folder trust approval is special and sends raw Enter.",
            ));
        }
        "json" => {
            return Some(topic_page(
                "json",
                "Use --json for one-shot objects on list, status, and doctor. Use --plain to require current line-oriented output on attach, list, status, and doctor. Use --format jsonl for long event streams such as serve and yolo.",
            ));
        }
        "state-dir" => {
            return Some(topic_page(
                "state-dir",
                "botctl uses XDG state paths by default. Pass --state-dir PATH on commands that store yolo records, prompt handoff, or observation artifacts.",
            ));
        }
        "dashboard-keys" => {
            return Some(topic_page(
                "dashboard-keys",
                "Dashboard keys are shown in the TUI footer. Typical flow: select pane, Enter to navigate, y to toggle yolo where supported, q to quit.",
            ));
        }
        "opencode" => {
            return Some(topic_page(
                "opencode",
                "OpenCode panes are dashboard-visible only when tmux command, title, cwd, and SQLite session metadata match uniquely. botctl never sends automation keys to OpenCode panes.",
            ));
        }
        "exit-codes" => {
            return Some(topic_page(
                "exit-codes",
                "0 success, including help and --version.\n1 runtime failure, such as tmux, filesystem, state database, or keybinding errors.\n2 usage error or guarded refusal where botctl intentionally refuses unsafe/non-actionable automation.\n130 interrupted when the process is terminated by the default Ctrl-C/SIGINT path; long-running cleanup handlers may stop gracefully with 0.",
            ));
        }
        _ => return generic_command_usage(topic, color),
    };
    Some(render_command_usage(
        name, purpose, usage, examples, options, safety, color,
    ))
}

const TARGET_HELP: &str = "Targets:\n  --pane %19              tmux pane id, safest\n  --pane 0:2.3            explicit tmux pane target\n  --session demo --window claude\n                           named tmux session/window where accepted and unambiguous";

fn topic_page(name: &str, body: &str) -> String {
    format!("botctl help {name}\n\n{body}")
}

fn generic_command_usage(topic: &str, color: bool) -> Option<String> {
    let usage = match topic {
        "start" => {
            "botctl start --session NAME [--window NAME] [--cwd PATH] [--command CMD] [--dry-run]"
        }
        "attach" => "botctl attach (--pane TARGET | --session NAME [--window NAME]) [--plain]",
        "list" => "botctl list [--all] [--json | --plain]",
        "capture" => "botctl capture --pane TARGET [--history-lines N]",
        "last-message" => "botctl last-message --pane TARGET [--out PATH] [--history-lines N]",
        "observe" => {
            "botctl observe --session NAME [--pane TARGET] [--events N] [--idle-timeout-ms N] [--history-lines N] [--state-dir PATH]"
        }
        "runtime" => {
            "botctl runtime [--foreground] [--reconcile-ms N] [--history-lines N] [--state-dir PATH] | botctl runtime stop [--state-dir PATH]"
        }
        "record-fixture" => {
            "botctl record-fixture --session NAME --case NAME [--pane TARGET] [--output-dir PATH] [--expected-state STATE] [--events N] [--idle-timeout-ms N] [--history-lines N]"
        }
        "classify" => "botctl classify --path PATH",
        "replay" => "botctl replay --path PATH",
        "bindings" => "botctl bindings",
        "install-bindings" => "botctl install-bindings [--path PATH]",
        "send-action" => "botctl send-action --pane TARGET --action NAME",
        "dismiss-survey" => "botctl dismiss-survey --pane TARGET",
        "continue-session" => {
            "botctl continue-session (--pane TARGET | --session NAME --window NAME)"
        }
        "auto-unstick" => {
            "botctl auto-unstick (--pane TARGET | --session NAME --window NAME) [--max-steps N]"
        }
        "keep-going" => {
            "botctl keep-going (--pane TARGET | --session NAME --window NAME) [--poll-ms N] [--submit-delay-ms N] [--state-dir PATH] [--source PATH | --text TEXT] [--no-yolo]"
        }
        "prompt" => {
            "botctl prompt [--text TEXT] [--source PATH ...] [--stdin] [--append-system-prompt PATH ...] [--cwd PATH] [--no-yolo] [--verbose] [-- CLAUDE_ARG ...]"
        }
        "mcp" => {
            "botctl mcp stdio [--state-dir PATH] | botctl mcp http --bind ADDR [--allow-non-loopback] [--state-dir PATH]"
        }
        "prepare-prompt" => {
            "botctl prepare-prompt --session NAME [--state-dir PATH] [--workspace PATH|UUID] [--source PATH | --text TEXT]"
        }
        "editor-helper" => {
            "botctl editor-helper --session NAME [--state-dir PATH] [--workspace PATH|UUID] [--source PATH] [--keep-pending] TARGET"
        }
        "submit-prompt" => {
            "botctl submit-prompt --session NAME --pane TARGET [--state-dir PATH] [--workspace PATH|UUID] [--source PATH | --text TEXT] [--submit-delay-ms N]"
        }
        _ => return None,
    };
    let purpose = match topic {
        "record-fixture" => "Advanced: capture a fixture case from a live session.",
        "classify" => "Advanced: classify a saved frame file.",
        "replay" => "Advanced: replay and compare a saved fixture case.",
        "send-action" => "Advanced plumbing: send one named automation action to a pane.",
        "dismiss-survey" => "Dismiss a survey prompt. Canonical name: dismiss-survey.",
        "last-message" => {
            "Dump the latest assistant message for a Claude, Codex, OpenCode, Pi, Grok, or Antigravity pane (Antigravity messages are scraped from pane scrollback, not persisted transcripts)."
        }
        "editor-helper" => {
            "Advanced prompt plumbing: generate an editor target file for prompt handoff."
        }
        _ => "Run command-specific workflow.",
    };
    Some(render_command_usage(
        topic,
        purpose,
        usage,
        &[usage],
        &["--help", "--no-color"],
        TARGET_HELP,
        color,
    ))
}

fn render_command_usage(
    name: &str,
    purpose: &str,
    usage: &str,
    examples: &[&str],
    options: &[&str],
    safety: &str,
    color: bool,
) -> String {
    let heading = |text: &str| {
        if color {
            format!("\x1b[1;96m{text}\x1b[0m")
        } else {
            text.to_string()
        }
    };
    let mut out = format!(
        "botctl {name}\n{purpose}\n\n{}\n  {usage}\n\n{}\n",
        heading("Usage:"),
        heading("Examples:")
    );
    for example in examples {
        out.push_str("  ");
        out.push_str(example);
        out.push('\n');
    }
    out.push('\n');
    out.push_str(&heading("Options:"));
    out.push('\n');
    for option in options {
        out.push_str("  ");
        out.push_str(option);
        out.push('\n');
    }
    out.push('\n');
    out.push_str(&heading("Safety / Output:"));
    out.push('\n');
    out.push_str(safety);
    out
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
    let mut target_args = Vec::new();
    let mut plain = false;
    for arg in args {
        if arg == "--plain" {
            plain = true;
        } else {
            target_args.push(arg);
        }
    }
    let target = parse_pane_target_args(target_args, "attach")?;
    Ok(Command::Attach(AttachArgs { target, plain }))
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
    let mut json = false;
    let mut plain = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--all" => all = true,
            "--json" => json = true,
            "--plain" => plain = true,
            flag => return Err(AppError::new(format!("unknown list flag: {flag}"))),
        }
        i += 1;
    }

    reject_json_plain_conflict("list", json, plain)?;
    Ok(Command::ListPanes(ListPanesArgs { all, json, plain }))
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

fn parse_last_message(args: Vec<String>) -> AppResult<Command> {
    let mut pane_id = None;
    let mut out = None;
    // Default to 2000 history lines for agy pane-scrape extraction; matches
    // the dashboard polling default. Non-agy providers ignore this value.
    let mut history_lines = 2000usize;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--pane" => {
                pane_id = Some(read_value(&args, &mut i, "--pane")?);
            }
            "--out" => {
                out = Some(PathBuf::from(read_value(&args, &mut i, "--out")?));
            }
            "--history-lines" => {
                let raw = read_value(&args, &mut i, "--history-lines")?;
                history_lines = raw.parse::<usize>().map_err(|_| {
                    AppError::new(format!("invalid value for --history-lines: {raw}"))
                })?;
            }
            flag => {
                return Err(AppError::new(format!("unknown last-message flag: {flag}")));
            }
        }
        i += 1;
    }

    let pane_id = pane_id.ok_or_else(|| AppError::new("missing required flag: --pane"))?;

    Ok(Command::LastMessage(LastMessageArgs {
        pane_id,
        out,
        history_lines,
    }))
}

fn parse_status(args: Vec<String>) -> AppResult<Command> {
    let mut pane_id = None;
    let mut history_lines = 120usize;
    let mut json = false;
    let mut plain = false;

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
            "--json" => {
                json = true;
            }
            "--plain" => {
                plain = true;
            }
            flag => {
                return Err(AppError::new(format!("unknown status flag: {flag}")));
            }
        }
        i += 1;
    }

    let pane_id = pane_id.ok_or_else(|| AppError::new("missing required flag: --pane"))?;
    reject_json_plain_conflict("status", json, plain)?;
    Ok(Command::Status(StatusArgs {
        pane_id,
        history_lines,
        json,
        plain,
    }))
}

fn parse_doctor(args: Vec<String>) -> AppResult<Command> {
    let mut session_name = None;
    let mut pane_id = None;
    let mut history_lines = 120usize;
    let mut bindings_path = None;
    let mut json = false;
    let mut plain = false;

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
            "--json" => {
                json = true;
            }
            "--plain" => {
                plain = true;
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
    reject_json_plain_conflict("doctor", json, plain)?;

    Ok(Command::Doctor(DoctorArgs {
        session_name,
        pane_id,
        history_lines,
        bindings_path,
        json,
        plain,
    }))
}

fn reject_json_plain_conflict(command: &str, json: bool, plain: bool) -> AppResult<()> {
    if json && plain {
        Err(AppError::new(format!(
            "{command} cannot combine --json and --plain"
        )))
    } else {
        Ok(())
    }
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
    let mut http_addr: Option<String> = None;
    let mut allowed_origins = Vec::new();
    let mut state_dir = None;
    let mut unmanaged = false;

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
            "--http" => {
                http_addr = Some(read_value(&args, &mut i, "--http")?);
            }
            "--allowed-origin" => {
                allowed_origins.push(read_value(&args, &mut i, "--allowed-origin")?);
            }
            "--unmanaged" => {
                unmanaged = true;
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
        http_addr,
        allowed_origins,
        format,
        state_dir,
        unmanaged,
    }))
}

fn parse_dashboard(args: Vec<String>) -> AppResult<Command> {
    let mut poll_ms = 1000u64;
    let mut history_lines = 120usize;
    let mut state_dir = None;
    let mut exit_on_navigate = false;
    let mut persistent = false;
    let mut unmanaged = false;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--poll-ms" => {
                let raw = read_value(&args, &mut i, "--poll-ms")?;
                poll_ms = raw
                    .parse::<u64>()
                    .map_err(|_| AppError::new(format!("invalid value for --poll-ms: {raw}")))?;
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
            "--exit-on-navigate" => {
                exit_on_navigate = true;
            }
            "--persistent" => {
                persistent = true;
            }
            "--unmanaged" => {
                unmanaged = true;
            }
            flag => {
                return Err(AppError::new(format!("unknown dashboard flag: {flag}")));
            }
        }
        i += 1;
    }

    if poll_ms == 0 {
        return Err(AppError::new(
            "dashboard requires --poll-ms to be at least 1",
        ));
    }

    if persistent && exit_on_navigate {
        return Err(AppError::new(
            "dashboard --persistent cannot be combined with --exit-on-navigate",
        ));
    }

    Ok(Command::Dashboard(DashboardArgs {
        poll_ms,
        history_lines,
        state_dir,
        exit_on_navigate,
        persistent,
        unmanaged,
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

fn parse_prompt_run(args: Vec<String>) -> AppResult<Command> {
    let mut session_name = None;
    let mut window_name = String::from("claude");
    let mut cwd = None;
    let mut command = String::from("claude");
    let mut sources = Vec::new();
    let mut text = None;
    let mut stdin = false;
    let mut append_system_prompts = Vec::new();
    let mut poll_ms = 1000u64;
    let mut submit_delay_ms = 250u64;
    let mut ready_timeout_ms = 30_000u64;
    let mut idle_timeout_ms = 600_000u64;
    let mut state_dir = None;
    let mut workspace = None;
    let mut large_prompt_threshold = 8192usize;
    let mut keep_temp = false;
    let mut no_yolo = false;
    let mut verbose = false;
    let mut claude_args = Vec::new();

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--" => {
                claude_args.extend(args[i + 1..].iter().cloned());
                break;
            }
            "--session" => session_name = Some(read_value(&args, &mut i, "--session")?),
            "--window" => window_name = read_value(&args, &mut i, "--window")?,
            "--cwd" => cwd = Some(PathBuf::from(read_value(&args, &mut i, "--cwd")?)),
            "--command" => command = read_value(&args, &mut i, "--command")?,
            "--source" => sources.push(PathBuf::from(read_value(&args, &mut i, "--source")?)),
            "--text" => {
                if text.is_some() {
                    return Err(AppError::new("prompt accepts --text at most once"));
                }
                text = Some(read_value(&args, &mut i, "--text")?);
            }
            "--stdin" => stdin = true,
            "--append-system-prompt" => append_system_prompts.push(PathBuf::from(read_value(
                &args,
                &mut i,
                "--append-system-prompt",
            )?)),
            "--poll-ms" => poll_ms = parse_u64_flag(&args, &mut i, "--poll-ms")?,
            "--submit-delay-ms" => {
                submit_delay_ms = parse_u64_flag(&args, &mut i, "--submit-delay-ms")?
            }
            "--ready-timeout-ms" => {
                ready_timeout_ms = parse_u64_flag(&args, &mut i, "--ready-timeout-ms")?
            }
            "--idle-timeout-ms" => {
                idle_timeout_ms = parse_u64_flag(&args, &mut i, "--idle-timeout-ms")?
            }
            "--state-dir" => {
                state_dir = Some(PathBuf::from(read_value(&args, &mut i, "--state-dir")?));
            }
            "--workspace" => workspace = Some(read_value(&args, &mut i, "--workspace")?),
            "--large-prompt-threshold" => {
                let raw = read_value(&args, &mut i, "--large-prompt-threshold")?;
                large_prompt_threshold = raw.parse::<usize>().map_err(|_| {
                    AppError::new(format!("invalid value for --large-prompt-threshold: {raw}"))
                })?;
            }
            "--keep-temp" => keep_temp = true,
            "--no-yolo" => no_yolo = true,
            "--verbose" => verbose = true,
            flag => return Err(AppError::new(format!("unknown prompt flag: {flag}"))),
        }
        i += 1;
    }

    reject_zero_prompt_flag(poll_ms, "--poll-ms")?;
    reject_zero_prompt_flag(submit_delay_ms, "--submit-delay-ms")?;
    reject_zero_prompt_flag(ready_timeout_ms, "--ready-timeout-ms")?;
    reject_zero_prompt_flag(idle_timeout_ms, "--idle-timeout-ms")?;
    if large_prompt_threshold == 0 {
        return Err(AppError::new(
            "prompt requires --large-prompt-threshold to be at least 1",
        ));
    }
    reject_prompt_mode_claude_command(&command, &claude_args)?;

    Ok(Command::Prompt(PromptRunArgs {
        session_name,
        window_name,
        cwd,
        command,
        sources,
        text,
        stdin,
        append_system_prompts,
        poll_ms,
        submit_delay_ms,
        ready_timeout_ms,
        idle_timeout_ms,
        state_dir,
        workspace,
        large_prompt_threshold,
        keep_temp,
        no_yolo,
        verbose,
        claude_args,
    }))
}

fn reject_prompt_mode_claude_command(command: &str, claude_args: &[String]) -> AppResult<()> {
    if claude_args
        .iter()
        .any(|arg| arg == "-p" || arg == "--prompt" || arg.starts_with("--prompt="))
    {
        return Err(AppError::new(
            "prompt must launch the interactive Claude TUI; do not pass claude -p/--prompt",
        ));
    }
    let mut saw_claude = false;
    let tokens = shell_like_command_tokens(command)?;
    for token in tokens {
        if saw_claude && (token == "-p" || token == "--prompt" || token.starts_with("--prompt=")) {
            return Err(AppError::new(
                "prompt --command must launch the interactive Claude TUI; do not use claude -p/--prompt",
            ));
        }
        if token == "claude" || token.ends_with("/claude") {
            saw_claude = true;
        }
    }
    Ok(())
}

fn shell_like_command_tokens(command: &str) -> AppResult<Vec<String>> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut chars = command.chars().peekable();
    let mut quote = None;
    while let Some(ch) = chars.next() {
        match quote {
            Some(q) if ch == q => quote = None,
            Some(_) if ch == '\\' => {
                if let Some(next) = chars.next() {
                    current.push(next);
                } else {
                    current.push(ch);
                }
            }
            Some(_) => current.push(ch),
            None if ch == '\'' || ch == '"' => quote = Some(ch),
            None if ch == '\\' => {
                if let Some(next) = chars.next() {
                    current.push(next);
                } else {
                    current.push(ch);
                }
            }
            None if ch.is_whitespace() => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            None => current.push(ch),
        }
    }
    if let Some(q) = quote {
        return Err(AppError::new(format!(
            "prompt --command contains an unterminated {q} quote"
        )));
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    Ok(tokens)
}

fn parse_u64_flag(args: &[String], index: &mut usize, flag: &str) -> AppResult<u64> {
    let raw = read_value(args, index, flag)?;
    raw.parse::<u64>()
        .map_err(|_| AppError::new(format!("invalid value for {flag}: {raw}")))
}

fn reject_zero_prompt_flag(value: u64, flag: &str) -> AppResult<()> {
    if value == 0 {
        Err(AppError::new(format!(
            "prompt requires {flag} to be at least 1"
        )))
    } else {
        Ok(())
    }
}

fn parse_mcp(args: Vec<String>) -> AppResult<Command> {
    let mut iter = args.into_iter();
    let transport_name = iter
        .next()
        .ok_or_else(|| AppError::new("mcp requires transport: stdio or http"))?;
    let rest = iter.collect::<Vec<_>>();
    let mut state_dir = None;
    let mut bind = None;
    let mut allow_non_loopback = false;
    let mut i = 0;
    while i < rest.len() {
        match rest[i].as_str() {
            "--state-dir" => {
                state_dir = Some(PathBuf::from(read_value(&rest, &mut i, "--state-dir")?));
            }
            "--bind" if transport_name == "http" => {
                bind = Some(read_value(&rest, &mut i, "--bind")?);
            }
            "--allow-non-loopback" if transport_name == "http" => {
                allow_non_loopback = true;
            }
            flag => return Err(AppError::new(format!("unknown mcp flag: {flag}"))),
        }
        i += 1;
    }

    let transport = match transport_name.as_str() {
        "stdio" => McpTransportArgs::Stdio,
        "http" => McpTransportArgs::Http {
            bind: bind.ok_or_else(|| AppError::new("mcp http requires --bind ADDR"))?,
            allow_non_loopback,
        },
        other => return Err(AppError::new(format!("unknown mcp transport: {other}"))),
    };
    Ok(Command::Mcp(McpArgs {
        transport,
        state_dir,
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

fn parse_runtime(args: Vec<String>) -> AppResult<Command> {
    let mut foreground = false;
    let mut stop = false;
    let mut reconcile_ms = 1000u64;
    let mut history_lines = 200usize;
    let mut state_dir = None;

    let start_index = match args.first().map(String::as_str) {
        Some("stop") => {
            stop = true;
            1
        }
        _ => 0,
    };

    let mut i = start_index;
    while i < args.len() {
        match args[i].as_str() {
            "--foreground" => {
                if stop {
                    return Err(AppError::new(
                        "runtime stop cannot be combined with --foreground",
                    ));
                }
                foreground = true;
            }
            "--reconcile-ms" => {
                if stop {
                    return Err(AppError::new(
                        "--reconcile-ms cannot be used with runtime stop",
                    ));
                }
                let raw = read_value(&args, &mut i, "--reconcile-ms")?;
                reconcile_ms = raw.parse::<u64>().map_err(|_| {
                    AppError::new(format!("invalid value for --reconcile-ms: {raw}"))
                })?;
            }
            "--history-lines" => {
                if stop {
                    return Err(AppError::new(
                        "--history-lines cannot be used with runtime stop",
                    ));
                }
                let raw = read_value(&args, &mut i, "--history-lines")?;
                history_lines = raw.parse::<usize>().map_err(|_| {
                    AppError::new(format!("invalid value for --history-lines: {raw}"))
                })?;
            }
            "--state-dir" => {
                state_dir = Some(PathBuf::from(read_value(&args, &mut i, "--state-dir")?));
            }
            flag => return Err(AppError::new(format!("unknown runtime flag: {flag}"))),
        }
        i += 1;
    }

    if stop && foreground {
        return Err(AppError::new(
            "runtime stop cannot be combined with --foreground",
        ));
    }

    if reconcile_ms == 0 {
        return Err(AppError::new(
            "runtime requires --reconcile-ms to be at least 1",
        ));
    }
    if history_lines == 0 {
        return Err(AppError::new(
            "runtime requires --history-lines to be at least 1",
        ));
    }

    Ok(Command::Runtime(RuntimeArgs {
        foreground,
        stop,
        reconcile_ms,
        history_lines,
        state_dir,
    }))
}

fn parse_yolo(args: Vec<String>) -> AppResult<Command> {
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
            let mut follow = false;
            let mut format = BabysitFormat::Human;
            let mut state_dir = None;
            let mut workspace = None;
            let mut unmanaged = false;
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
                    "--follow" => {
                        follow = true;
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
                    "--unmanaged" => {
                        unmanaged = true;
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
            Ok(Command::YoloStart(YoloStartArgs {
                pane_id,
                all,
                poll_ms,
                live_preview,
                follow,
                format,
                state_dir,
                workspace,
                unmanaged,
            }))
        }
        "stop" => {
            let mut pane_id = None;
            let mut all = false;
            let mut state_dir = None;
            let mut workspace = None;
            let mut unmanaged = false;
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
                    "--unmanaged" => {
                        unmanaged = true;
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
            Ok(Command::YoloStop(YoloStopArgs {
                pane_id,
                all,
                state_dir,
                workspace,
                unmanaged,
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
    fn parses_list_alias() {
        let command = parse_args(vec![String::from("botctl"), String::from("list")])
            .expect("list command should parse");

        match command {
            Command::ListPanes(args) => {
                assert!(!args.all);
                assert!(!args.json);
                assert!(!args.plain);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_list_all_flag() {
        let command = parse_args(vec![
            String::from("botctl"),
            String::from("list"),
            String::from("--all"),
        ])
        .expect("list --all command should parse");

        match command {
            Command::ListPanes(args) => {
                assert!(args.all);
                assert!(!args.json);
                assert!(!args.plain);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_list_plain_flag() {
        let command = parse_args(vec![
            String::from("botctl"),
            String::from("list"),
            String::from("--plain"),
        ])
        .expect("list --plain command should parse");

        match command {
            Command::ListPanes(args) => {
                assert!(args.plain);
                assert!(!args.json);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn rejects_list_json_plain_combination() {
        let error = parse_args(vec![
            String::from("botctl"),
            String::from("list"),
            String::from("--json"),
            String::from("--plain"),
        ])
        .expect_err("list should reject conflicting output formats");

        assert!(
            error
                .to_string()
                .contains("list cannot combine --json and --plain")
        );
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
                assert!(!args.plain);
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
                assert!(!args.plain);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_attach_plain_flag() {
        let command = parse_args(vec![
            String::from("botctl"),
            String::from("attach"),
            String::from("--pane"),
            String::from("%7"),
            String::from("--plain"),
        ])
        .expect("attach --plain command should parse");

        match command {
            Command::Attach(args) => {
                assert_eq!(args.target.pane_id.as_deref(), Some("%7"));
                assert!(args.plain);
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
    fn parses_last_message_command() {
        let command = parse_args(vec![
            String::from("botctl"),
            String::from("last-message"),
            String::from("--pane"),
            String::from("0:4.1"),
            String::from("--out"),
            String::from("agent.md"),
        ])
        .expect("last-message command should parse");

        match command {
            Command::LastMessage(args) => {
                assert_eq!(args.pane_id, "0:4.1");
                assert_eq!(args.out, Some(PathBuf::from("agent.md")));
                assert_eq!(args.history_lines, 2000);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_last_message_stdout_output() {
        let command = parse_args(vec![
            String::from("botctl"),
            String::from("last-message"),
            String::from("--pane"),
            String::from("%7"),
            String::from("--out"),
            String::from("-"),
        ])
        .expect("last-message stdout command should parse");

        match command {
            Command::LastMessage(args) => {
                assert_eq!(args.pane_id, "%7");
                assert_eq!(args.out, Some(PathBuf::from("-")));
                assert_eq!(args.history_lines, 2000);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_last_message_history_lines_override() {
        let command = parse_args(vec![
            String::from("botctl"),
            String::from("last-message"),
            String::from("--pane"),
            String::from("%19"),
            String::from("--history-lines"),
            String::from("5000"),
        ])
        .expect("last-message with --history-lines should parse");

        match command {
            Command::LastMessage(args) => {
                assert_eq!(args.pane_id, "%19");
                assert_eq!(args.history_lines, 5000);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn last_message_rejects_invalid_history_lines() {
        let error = parse_args(vec![
            String::from("botctl"),
            String::from("last-message"),
            String::from("--pane"),
            String::from("%19"),
            String::from("--history-lines"),
            String::from("not-a-number"),
        ])
        .expect_err("last-message should reject non-numeric --history-lines");
        assert!(error.to_string().contains("--history-lines"));
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
            String::from("--http"),
            String::from("127.0.0.1:8787"),
            String::from("--allowed-origin"),
            String::from("http://localhost:3000"),
            String::from("--allowed-origin"),
            String::from("http://127.0.0.1:3000"),
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
                assert_eq!(args.http_addr.as_deref(), Some("127.0.0.1:8787"));
                assert_eq!(
                    args.allowed_origins,
                    vec![
                        String::from("http://localhost:3000"),
                        String::from("http://127.0.0.1:3000"),
                    ]
                );
                assert_eq!(args.format, super::BabysitFormat::Jsonl);
                assert_eq!(args.state_dir, Some(PathBuf::from("/tmp/botctl-serve")));
                assert!(!args.unmanaged);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_serve_unmanaged_flag() {
        let command = parse_args(vec![
            String::from("botctl"),
            String::from("serve"),
            String::from("--session"),
            String::from("demo"),
            String::from("--unmanaged"),
        ])
        .expect("serve command should parse unmanaged flag");

        match command {
            Command::Serve(args) => assert!(args.unmanaged),
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
    fn parses_dashboard_command() {
        let command = parse_args(vec![
            String::from("botctl"),
            String::from("dashboard"),
            String::from("--poll-ms"),
            String::from("750"),
            String::from("--history-lines"),
            String::from("200"),
            String::from("--state-dir"),
            String::from("/tmp/botctl-dashboard"),
        ])
        .expect("dashboard command should parse");

        match command {
            Command::Dashboard(args) => {
                assert_eq!(args.poll_ms, 750);
                assert_eq!(args.history_lines, 200);
                assert_eq!(args.state_dir, Some(PathBuf::from("/tmp/botctl-dashboard")));
                assert!(!args.exit_on_navigate);
                assert!(!args.persistent);
                assert!(!args.unmanaged);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_dashboard_unmanaged_flag() {
        let command = parse_args(vec![
            String::from("botctl"),
            String::from("dashboard"),
            String::from("--unmanaged"),
        ])
        .expect("dashboard command should parse unmanaged flag");

        match command {
            Command::Dashboard(args) => assert!(args.unmanaged),
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_dashboard_exit_on_navigate_flag() {
        let command = parse_args(vec![
            String::from("botctl"),
            String::from("dashboard"),
            String::from("--exit-on-navigate"),
        ])
        .expect("dashboard command should parse exit flag");

        match command {
            Command::Dashboard(args) => {
                assert!(args.exit_on_navigate);
                assert!(!args.persistent);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_dashboard_persistent_flag() {
        let command = parse_args(vec![
            String::from("botctl"),
            String::from("dashboard"),
            String::from("--persistent"),
        ])
        .expect("dashboard command should parse persistent flag");

        match command {
            Command::Dashboard(args) => {
                assert!(args.persistent);
                assert!(!args.exit_on_navigate);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn rejects_dashboard_persistent_exit_on_navigate_combination() {
        let error = parse_args(vec![
            String::from("botctl"),
            String::from("dashboard"),
            String::from("--persistent"),
            String::from("--exit-on-navigate"),
        ])
        .expect_err("dashboard should reject persistent exit-on-navigate");

        assert!(
            error
                .to_string()
                .contains("dashboard --persistent cannot be combined with --exit-on-navigate")
        );
    }

    #[test]
    fn rejects_dashboard_zero_poll_interval() {
        let error = parse_args(vec![
            String::from("botctl"),
            String::from("dashboard"),
            String::from("--poll-ms"),
            String::from("0"),
        ])
        .expect_err("dashboard should reject zero poll interval");

        assert!(
            error
                .to_string()
                .contains("dashboard requires --poll-ms to be at least 1")
        );
    }

    #[test]
    fn usage_renders_real_newlines_for_observe_and_serve() {
        let usage = super::usage();

        assert!(!usage.contains("\\n"));
        assert!(usage.contains("Claude Code tmux automation for live sessions"));
        assert!(usage.contains("Main Commands:"));
        assert!(usage.contains("yolo [start]"));
        assert!(usage.contains("serve --session NAME"));
        assert!(usage.contains("dashboard [--poll-ms N]"));
        assert!(usage.contains("Common Commands:"));
        assert!(usage.contains("[--json | --plain]"));
        assert!(usage.contains("Canonical Names:"));
        assert!(usage.contains("approve-permission` -> `approve"));
        assert!(usage.contains("Advanced Diagnostics And Plumbing:"));
        assert!(usage.contains("Quick Start:"));
        assert!(usage.contains("Help Topics:"));
        assert!(usage.contains("dashboard [--poll-ms N]"));
        assert!(usage.contains("[--exit-on-navigate]"));
        assert!(usage.contains("botctl help exit-codes"));
    }

    #[test]
    fn exit_codes_help_topic_documents_stable_codes() {
        let usage = super::usage_for(&super::HelpArgs {
            topic: Some(String::from("exit-codes")),
            color: false,
        });

        assert!(usage.contains("botctl help exit-codes"));
        assert!(usage.contains("0 success"));
        assert!(usage.contains("1 runtime failure"));
        assert!(usage.contains("2 usage error or guarded refusal"));
        assert!(usage.contains("130 interrupted"));
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
                assert!(!args.json);
                assert!(!args.plain);
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
                assert!(!args.json);
                assert!(!args.plain);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_status_plain_flag() {
        let command = parse_args(vec![
            String::from("botctl"),
            String::from("status"),
            String::from("--pane"),
            String::from("%7"),
            String::from("--plain"),
        ])
        .expect("status --plain command should parse");

        match command {
            Command::Status(args) => {
                assert!(args.plain);
                assert!(!args.json);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn rejects_status_json_plain_combination() {
        let error = parse_args(vec![
            String::from("botctl"),
            String::from("status"),
            String::from("--pane"),
            String::from("%7"),
            String::from("--json"),
            String::from("--plain"),
        ])
        .expect_err("status should reject conflicting output formats");

        assert!(
            error
                .to_string()
                .contains("status cannot combine --json and --plain")
        );
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
                assert!(!args.json);
                assert!(!args.plain);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_doctor_plain_flag() {
        let command = parse_args(vec![
            String::from("botctl"),
            String::from("doctor"),
            String::from("--session"),
            String::from("demo"),
            String::from("--plain"),
        ])
        .expect("doctor --plain command should parse");

        match command {
            Command::Doctor(args) => {
                assert_eq!(args.session_name.as_deref(), Some("demo"));
                assert!(args.plain);
                assert!(!args.json);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn rejects_doctor_json_plain_combination() {
        let error = parse_args(vec![
            String::from("botctl"),
            String::from("doctor"),
            String::from("--session"),
            String::from("demo"),
            String::from("--json"),
            String::from("--plain"),
        ])
        .expect_err("doctor should reject conflicting output formats");

        assert!(
            error
                .to_string()
                .contains("doctor cannot combine --json and --plain")
        );
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
    fn parses_prompt_command_with_defaults() {
        let command = parse_args(vec![
            String::from("botctl"),
            String::from("prompt"),
            String::from("--text"),
            String::from("hi"),
        ])
        .expect("prompt command should parse");

        match command {
            Command::Prompt(args) => {
                assert!(args.session_name.is_none());
                assert_eq!(args.window_name, "claude");
                assert_eq!(args.command, "claude");
                assert_eq!(args.text.as_deref(), Some("hi"));
                assert_eq!(args.poll_ms, 1000);
                assert_eq!(args.submit_delay_ms, 250);
                assert_eq!(args.ready_timeout_ms, 30_000);
                assert_eq!(args.idle_timeout_ms, 600_000);
                assert_eq!(args.large_prompt_threshold, 8192);
                assert!(!args.no_yolo);
                assert!(!args.verbose);
                assert!(args.claude_args.is_empty());
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_mcp_stdio_command() {
        let command = parse_args(vec![
            String::from("botctl"),
            String::from("mcp"),
            String::from("stdio"),
            String::from("--state-dir"),
            String::from("/tmp/botctl-mcp"),
        ])
        .expect("mcp stdio command should parse");

        match command {
            Command::Mcp(args) => {
                assert_eq!(args.transport, super::McpTransportArgs::Stdio);
                assert_eq!(args.state_dir, Some(PathBuf::from("/tmp/botctl-mcp")));
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_mcp_http_command() {
        let command = parse_args(vec![
            String::from("botctl"),
            String::from("mcp"),
            String::from("http"),
            String::from("--bind"),
            String::from("127.0.0.1:8787"),
        ])
        .expect("mcp http command should parse");

        match command {
            Command::Mcp(args) => assert_eq!(
                args.transport,
                super::McpTransportArgs::Http {
                    bind: String::from("127.0.0.1:8787"),
                    allow_non_loopback: false,
                }
            ),
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_mcp_http_with_allow_non_loopback() {
        let command = parse_args(vec![
            String::from("botctl"),
            String::from("mcp"),
            String::from("http"),
            String::from("--bind"),
            String::from("0.0.0.0:8787"),
            String::from("--allow-non-loopback"),
        ])
        .expect("mcp http command with allow-non-loopback should parse");

        match command {
            Command::Mcp(args) => assert_eq!(
                args.transport,
                super::McpTransportArgs::Http {
                    bind: String::from("0.0.0.0:8787"),
                    allow_non_loopback: true,
                }
            ),
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn rejects_allow_non_loopback_for_stdio() {
        let result = parse_args(vec![
            String::from("botctl"),
            String::from("mcp"),
            String::from("stdio"),
            String::from("--allow-non-loopback"),
        ]);
        assert!(result.is_err());
    }

    #[test]
    fn parses_prompt_repeated_sources_and_system_prompts() {
        let command = parse_args(vec![
            String::from("botctl"),
            String::from("prompt"),
            String::from("--source"),
            String::from("a.md"),
            String::from("--source"),
            String::from("b.md"),
            String::from("--append-system-prompt"),
            String::from("rules.md"),
            String::from("--stdin"),
            String::from("--no-yolo"),
            String::from("--verbose"),
        ])
        .expect("prompt command should parse repeated inputs");

        match command {
            Command::Prompt(args) => {
                assert_eq!(
                    args.sources,
                    vec![PathBuf::from("a.md"), PathBuf::from("b.md")]
                );
                assert_eq!(args.append_system_prompts, vec![PathBuf::from("rules.md")]);
                assert!(args.stdin);
                assert!(args.no_yolo);
                assert!(args.verbose);
                assert!(args.claude_args.is_empty());
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_prompt_claude_args_after_separator() {
        let command = parse_args(vec![
            String::from("botctl"),
            String::from("prompt"),
            String::from("--text"),
            String::from("hi"),
            String::from("--"),
            String::from("--model"),
            String::from("sonnet"),
            String::from("--name"),
            String::from("Just testing"),
        ])
        .expect("prompt should parse Claude passthrough args");

        match command {
            Command::Prompt(args) => assert_eq!(
                args.claude_args,
                vec!["--model", "sonnet", "--name", "Just testing"]
            ),
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn rejects_prompt_mode_in_claude_passthrough_args() {
        let error = parse_args(vec![
            String::from("botctl"),
            String::from("prompt"),
            String::from("--text"),
            String::from("hi"),
            String::from("--"),
            String::from("--prompt"),
            String::from("hi"),
        ])
        .expect_err("prompt should reject prompt mode passthrough args");

        assert!(error.to_string().contains("do not pass claude -p/--prompt"));
    }

    #[test]
    fn rejects_prompt_zero_values() {
        for flag in [
            "--poll-ms",
            "--submit-delay-ms",
            "--ready-timeout-ms",
            "--idle-timeout-ms",
            "--large-prompt-threshold",
        ] {
            let error = parse_args(vec![
                String::from("botctl"),
                String::from("prompt"),
                String::from("--text"),
                String::from("hi"),
                String::from(flag),
                String::from("0"),
            ])
            .expect_err("prompt should reject zero numeric flags");

            assert!(error.to_string().contains(flag));
        }
    }

    #[test]
    fn rejects_prompt_command_prompt_mode_with_shell_quotes() {
        let error = parse_args(vec![
            String::from("botctl"),
            String::from("prompt"),
            String::from("--text"),
            String::from("hi"),
            String::from("--command"),
            String::from("/opt/Claude Code/bin/claude --prompt 'hi there'"),
        ])
        .expect_err("prompt should reject quoted claude prompt-mode commands");

        assert!(error.to_string().contains("do not use claude -p/--prompt"));
    }

    #[test]
    fn accepts_prompt_command_paths_with_spaces() {
        let command = parse_args(vec![
            String::from("botctl"),
            String::from("prompt"),
            String::from("--text"),
            String::from("hi"),
            String::from("--command"),
            String::from("'/opt/Claude Code/bin/claude' --dangerously-skip-permissions"),
        ])
        .expect("quoted interactive Claude command should parse");

        match command {
            Command::Prompt(args) => assert_eq!(
                args.command,
                "'/opt/Claude Code/bin/claude' --dangerously-skip-permissions"
            ),
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn prompt_help_and_usage_include_command() {
        let usage = super::usage();
        assert!(usage.contains("prompt [--text TEXT"));
        let help = super::usage_for(&super::HelpArgs {
            topic: Some(String::from("prompt")),
            color: false,
        });
        assert!(help.contains("botctl prompt"));
        assert!(help.contains("--append-system-prompt"));
        assert!(help.contains("prints only the latest assistant text to stdout"));
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
            Command::YoloStart(args) => {
                assert_eq!(args.pane_id.as_deref(), Some("%9"));
                assert!(!args.all);
                assert_eq!(args.poll_ms, 250);
                assert!(!args.live_preview);
                assert_eq!(args.format, super::BabysitFormat::Human);
                assert!(!args.unmanaged);
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
            Command::YoloStart(args) => {
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
            Command::YoloStart(args) => {
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
            Command::YoloStart(args) => {
                assert_eq!(args.pane_id.as_deref(), Some("%9"));
                assert!(args.live_preview);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_yolo_follow_flag() {
        let command = parse_args(vec![
            String::from("botctl"),
            String::from("yolo"),
            String::from("--pane"),
            String::from("%9"),
            String::from("--follow"),
        ])
        .expect("yolo follow should parse");

        match command {
            Command::YoloStart(args) => {
                assert_eq!(args.pane_id.as_deref(), Some("%9"));
                assert!(args.follow);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_yolo_unmanaged_flag() {
        let command = parse_args(vec![
            String::from("botctl"),
            String::from("yolo"),
            String::from("--pane"),
            String::from("%9"),
            String::from("--unmanaged"),
        ])
        .expect("yolo should parse unmanaged flag");

        match command {
            Command::YoloStart(args) => assert!(args.unmanaged),
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_runtime_command() {
        let command = parse_args(vec![
            String::from("botctl"),
            String::from("runtime"),
            String::from("--reconcile-ms"),
            String::from("250"),
            String::from("--history-lines"),
            String::from("400"),
        ])
        .expect("runtime command should parse");

        match command {
            Command::Runtime(args) => {
                assert!(!args.foreground);
                assert!(!args.stop);
                assert_eq!(args.reconcile_ms, 250);
                assert_eq!(args.history_lines, 400);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_runtime_stop_command() {
        let command = parse_args(vec![
            String::from("botctl"),
            String::from("runtime"),
            String::from("stop"),
        ])
        .expect("runtime stop should parse");

        match command {
            Command::Runtime(args) => {
                assert!(args.stop);
                assert!(!args.foreground);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_runtime_foreground_flag() {
        let command = parse_args(vec![
            String::from("botctl"),
            String::from("runtime"),
            String::from("--foreground"),
        ])
        .expect("runtime foreground should parse");

        match command {
            Command::Runtime(args) => {
                assert!(args.foreground);
                assert!(!args.stop);
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
            Command::YoloStop(args) => assert!(args.all),
            other => panic!("unexpected command: {other:?}"),
        }
    }
}
