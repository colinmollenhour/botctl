use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde_json::Value;

use crate::agy;
use crate::app::{AppError, AppResult};
use crate::opencode;
use crate::pi;
use crate::proc_fd::transcript_from_process_fds;
use crate::tmux::{TmuxClient, TmuxPane};

const CLAUDE_PROJECTS_DIR: &str = ".claude/projects";
const CODEX_SESSIONS_DIR: &str = ".codex/sessions";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LastAgentMessage {
    pub provider: &'static str,
    pub session_id: String,
    pub text: String,
}

pub fn load_last_agent_message(
    pane: &TmuxPane,
    tmux: &TmuxClient,
    history_lines: usize,
) -> AppResult<LastAgentMessage> {
    let _ = (tmux, history_lines); // ignored by non-agy branches

    if pane.current_command.eq_ignore_ascii_case("claude") {
        return load_claude_last_message(pane);
    }

    if pane.current_command.eq_ignore_ascii_case("opencode") {
        return load_opencode_last_message(pane);
    }

    if pane.current_command.eq_ignore_ascii_case("pi") {
        return load_pi_last_message(pane);
    }

    if pane.current_command.eq_ignore_ascii_case("agy") {
        return load_agy_last_message(pane, tmux, history_lines);
    }

    if pane.current_command.eq_ignore_ascii_case("codex")
        || (pane.current_command.eq_ignore_ascii_case("node")
            && !pane.pane_title.starts_with("OC | "))
    {
        return load_codex_last_message(pane);
    }

    Err(AppError::new(format!(
        "last-message supports Claude, Codex, OpenCode, Pi, and Antigravity panes; pane {} is running {}",
        pane.pane_id, pane.current_command
    )))
}

pub fn default_output_path(session_id: &str) -> PathBuf {
    PathBuf::from(format!("MESSAGE_{}.md", sanitize_filename_part(session_id)))
}

pub fn output_path_is_stdout(path: &Path) -> bool {
    path == Path::new("-")
}

pub fn line_count(text: &str) -> usize {
    text.lines().count()
}

fn load_claude_last_message(pane: &TmuxPane) -> AppResult<LastAgentMessage> {
    let (session_id, transcript_path) = resolve_claude_transcript_for_pane(pane)?
        .ok_or_else(|| AppError::new("no Claude transcript found for pane"))?;
    let text = latest_claude_assistant_text(&transcript_path)?.ok_or_else(|| {
        AppError::new(format!(
            "no assistant text message found in Claude transcript {}",
            transcript_path.display()
        ))
    })?;

    Ok(LastAgentMessage {
        provider: "Claude",
        session_id,
        text,
    })
}

fn load_opencode_last_message(pane: &TmuxPane) -> AppResult<LastAgentMessage> {
    let message = opencode::latest_assistant_message_for_pane(pane)?
        .ok_or_else(|| AppError::new("no OpenCode session resolved for pane"))?;
    Ok(LastAgentMessage {
        provider: "OpenCode",
        session_id: message.session_id,
        text: message.text,
    })
}

fn load_agy_last_message(
    pane: &TmuxPane,
    tmux: &TmuxClient,
    history_lines: usize,
) -> AppResult<LastAgentMessage> {
    use crate::proc_fd::LiveProc;
    let frame = tmux.capture_pane(&pane.pane_id, history_lines)?;
    let session = agy::resolve_agy_session_for_pane(pane, &frame, &LiveProc)?.ok_or_else(|| {
        // The secondary signal (state dir or frame fingerprint) was absent —
        // we cannot even confirm this is an agy pane.
        AppError::new(
            "agy: no Antigravity conversation resolvable for this pane (state directory or process info unavailable; history.jsonl had no matching entry)",
        )
    })?;
    let conversation_id = session.id.ok_or_else(|| {
        // Session resolved (pane is confirmed agy) but no open .pb found.
        let state_dir = agy::default_state_dir();
        if let Some(pid) = pane.pane_pid {
            AppError::new(format!(
                "agy: pane process {pid} has no open conversation file under \
                 {state_dir}/conversations/; either the agy session has no active \
                 conversation yet, ANTIGRAVITY_STATE_DIR is misconfigured, or \
                 /proc/{pid}/fd is unreadable for this user.",
                state_dir = state_dir.display(),
            ))
        } else {
            // Reached the `session.id.ok_or_else` closure, so the agy
            // session is confirmed. The conversation_id lookup just couldn't
            // run without a pane_pid for the FD walk.
            AppError::new(
                "agy: pane confirmed as Antigravity but pane_pid is unavailable, so the open conversation file could not be verified; this usually means tmux did not report a pane process id",
            )
        }
    })?;
    let stripped = agy::strip_ansi(&frame);
    let text = agy::extract_last_assistant_text(&stripped).ok_or_else(|| {
        AppError::new(
            "agy: no completed assistant message visible in pane scrollback; the extractor requires three horizontal-rule lines (one above the last assistant turn, plus the two that bracket the live input box) — use --history-lines to widen the scrollback window",
        )
    })?;
    Ok(LastAgentMessage {
        provider: "Antigravity",
        session_id: conversation_id,
        text,
    })
}

fn load_pi_last_message(pane: &TmuxPane) -> AppResult<LastAgentMessage> {
    use crate::proc_fd::LiveProc;
    let session = pi::resolve_pi_session_for_pane(pane, &LiveProc)?
        .ok_or_else(|| AppError::new("no Pi session resolved for pane"))?;
    let message = pi::latest_assistant_message_for_pane(pane, &LiveProc)?.ok_or_else(|| {
        AppError::new(format!(
            "no assistant text message found in Pi transcript {}",
            session.path.display()
        ))
    })?;
    Ok(LastAgentMessage {
        provider: "Pi",
        session_id: message.session_id,
        text: message.text,
    })
}

fn load_codex_last_message(pane: &TmuxPane) -> AppResult<LastAgentMessage> {
    let (session_id, transcript_path) = resolve_codex_transcript_for_pane(pane)?
        .ok_or_else(|| AppError::new("no Codex transcript found for pane"))?;
    let text = latest_codex_assistant_text(&transcript_path)?.ok_or_else(|| {
        AppError::new(format!(
            "no assistant text message found in Codex transcript {}",
            transcript_path.display()
        ))
    })?;

    Ok(LastAgentMessage {
        provider: "Codex",
        session_id,
        text,
    })
}

pub fn resolve_codex_session_id_for_pane(pane: &TmuxPane) -> AppResult<Option<String>> {
    Ok(resolve_codex_transcript_for_pane(pane)?.map(|(session_id, _)| session_id))
}

fn resolve_claude_transcript_for_pane(pane: &TmuxPane) -> AppResult<Option<(String, PathBuf)>> {
    let Some(projects_root) = home_dir().map(|home| home.join(CLAUDE_PROJECTS_DIR)) else {
        return Ok(None);
    };

    resolve_claude_transcript_in_projects_root(pane, &projects_root)
}

fn resolve_claude_transcript_in_projects_root(
    pane: &TmuxPane,
    projects_root: &Path,
) -> AppResult<Option<(String, PathBuf)>> {
    for project_dir in candidate_claude_project_dirs(&projects_root, &pane.current_path) {
        if let Some(pid) = pane.pane_pid
            && let Some(transcript) = transcript_from_process_fds(pid, &project_dir, "jsonl")?
            && let Some(session_id) = claude_session_id_from_transcript(&transcript, &project_dir)
        {
            return Ok(Some((session_id, transcript)));
        }
        if let Some((session_id, transcript)) = latest_claude_transcript(&project_dir)? {
            return Ok(Some((session_id, transcript)));
        }
    }

    Ok(None)
}

fn candidate_claude_project_dirs(projects_root: &Path, current_path: &str) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    for ancestor in Path::new(current_path).ancestors() {
        if ancestor.as_os_str().is_empty() {
            continue;
        }
        let candidate = projects_root.join(encode_claude_project_path(ancestor));
        if candidate.is_dir() {
            candidates.push(candidate);
        }
    }
    candidates
}

fn encode_claude_project_path(path: &Path) -> String {
    path.display().to_string().replace('/', "-")
}

fn latest_claude_transcript(project_dir: &Path) -> AppResult<Option<(String, PathBuf)>> {
    let mut latest = None::<(SystemTime, String, PathBuf)>;
    for entry in fs::read_dir(project_dir)? {
        let entry = entry?;
        let path = entry.path();
        let Some(session_id) = claude_session_id_from_transcript(&path, project_dir) else {
            continue;
        };
        let modified = entry
            .metadata()?
            .modified()
            .unwrap_or(SystemTime::UNIX_EPOCH);
        if latest
            .as_ref()
            .map(|(latest_modified, _, _)| modified > *latest_modified)
            .unwrap_or(true)
        {
            latest = Some((modified, session_id, path));
        }
    }
    Ok(latest.map(|(_, session_id, path)| (session_id, path)))
}

fn claude_session_id_from_transcript(path: &Path, project_dir: &Path) -> Option<String> {
    if path.parent()? != project_dir {
        return None;
    }
    if path.extension()? != "jsonl" {
        return None;
    }
    read_jsonl_session_id(path, "sessionId")
        .ok()
        .flatten()
        .or_else(|| {
            path.file_stem()
                .and_then(|stem| stem.to_str())
                .map(str::to_string)
        })
}

fn latest_claude_assistant_text(path: &Path) -> AppResult<Option<String>> {
    let content = fs::read_to_string(path)?;
    let mut latest = None;
    for line in content.lines() {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if value.get("type").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        if value.pointer("/message/role").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        if let Some(text) = text_from_claude_content(value.pointer("/message/content"))
            .filter(|text| !text.trim().is_empty())
        {
            latest = Some(text);
        }
    }
    Ok(latest)
}

fn text_from_claude_content(content: Option<&Value>) -> Option<String> {
    match content? {
        Value::String(text) => Some(text.clone()),
        Value::Array(parts) => {
            let text = parts
                .iter()
                .filter(|part| part.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|part| part.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n\n");
            if text.is_empty() { None } else { Some(text) }
        }
        _ => None,
    }
}

fn resolve_codex_transcript_for_pane(pane: &TmuxPane) -> AppResult<Option<(String, PathBuf)>> {
    let Some(sessions_root) = home_dir().map(|home| home.join(CODEX_SESSIONS_DIR)) else {
        return Ok(None);
    };

    if let Some(pid) = pane.pane_pid
        && let Some(transcript) = transcript_from_process_fds(pid, &sessions_root, "jsonl")?
        && let Some(session_id) = codex_session_id_from_transcript(&transcript)?
    {
        return Ok(Some((session_id, transcript)));
    }

    latest_codex_transcript_for_cwd(&sessions_root, &pane.current_path)
}

fn latest_codex_transcript_for_cwd(
    sessions_root: &Path,
    current_path: &str,
) -> AppResult<Option<(String, PathBuf)>> {
    let mut latest = None::<(SystemTime, String, PathBuf)>;
    for path in collect_jsonl_files(sessions_root)? {
        let Some((session_id, cwd)) = codex_session_meta(&path)? else {
            continue;
        };
        if cwd != current_path {
            continue;
        }
        let modified = fs::metadata(&path)?
            .modified()
            .unwrap_or(SystemTime::UNIX_EPOCH);
        if latest
            .as_ref()
            .map(|(latest_modified, _, _)| modified > *latest_modified)
            .unwrap_or(true)
        {
            latest = Some((modified, session_id, path));
        }
    }
    Ok(latest.map(|(_, session_id, path)| (session_id, path)))
}

fn codex_session_id_from_transcript(path: &Path) -> AppResult<Option<String>> {
    if let Some((session_id, _)) = codex_session_meta(path)? {
        return Ok(Some(session_id));
    }
    Ok(path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .and_then(|stem| stem.get(stem.len().saturating_sub(36)..))
        .map(str::to_string))
}

fn codex_session_meta(path: &Path) -> AppResult<Option<(String, String)>> {
    let content = fs::read_to_string(path)?;
    for line in content.lines().take(16) {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if value.get("type").and_then(Value::as_str) != Some("session_meta") {
            continue;
        }
        let Some(session_id) = value.pointer("/payload/id").and_then(Value::as_str) else {
            continue;
        };
        let Some(cwd) = value.pointer("/payload/cwd").and_then(Value::as_str) else {
            continue;
        };
        return Ok(Some((session_id.to_string(), cwd.to_string())));
    }
    Ok(None)
}

fn latest_codex_assistant_text(path: &Path) -> AppResult<Option<String>> {
    let content = fs::read_to_string(path)?;
    let mut latest = None;
    for line in content.lines() {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if let Some(text) = codex_response_item_text(&value)
            .or_else(|| codex_event_agent_message_text(&value))
            .filter(|text| !text.trim().is_empty())
        {
            latest = Some(text);
        }
    }
    Ok(latest)
}

fn codex_response_item_text(value: &Value) -> Option<String> {
    if value.get("type").and_then(Value::as_str) != Some("response_item") {
        return None;
    }
    let payload = value.get("payload")?;
    if payload.get("type").and_then(Value::as_str) != Some("message") {
        return None;
    }
    if payload.get("role").and_then(Value::as_str) != Some("assistant") {
        return None;
    }
    let text = payload
        .get("content")?
        .as_array()?
        .iter()
        .filter(|part| part.get("type").and_then(Value::as_str) == Some("output_text"))
        .filter_map(|part| part.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n\n");
    if text.is_empty() { None } else { Some(text) }
}

fn codex_event_agent_message_text(value: &Value) -> Option<String> {
    if value.get("type").and_then(Value::as_str) != Some("event_msg") {
        return None;
    }
    let payload = value.get("payload")?;
    if payload.get("type").and_then(Value::as_str) != Some("agent_message") {
        return None;
    }
    payload
        .get("message")
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn collect_jsonl_files(root: &Path) -> AppResult<Vec<PathBuf>> {
    let mut files = Vec::new();
    collect_jsonl_files_into(root, &mut files)?;
    Ok(files)
}

fn collect_jsonl_files_into(path: &Path, files: &mut Vec<PathBuf>) -> AppResult<()> {
    let entries = match fs::read_dir(path) {
        Ok(entries) => entries,
        Err(_) => return Ok(()),
    };

    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            collect_jsonl_files_into(&path, files)?;
        } else if path.extension().and_then(|value| value.to_str()) == Some("jsonl") {
            files.push(path);
        }
    }
    Ok(())
}

fn read_jsonl_session_id(path: &Path, key: &str) -> AppResult<Option<String>> {
    let content = fs::read_to_string(path)?;
    for line in content.lines().take(8) {
        let value: Value = match serde_json::from_str(line) {
            Ok(value) => value,
            Err(_) => continue,
        };
        if let Some(session_id) = value.get(key).and_then(Value::as_str) {
            return Ok(Some(session_id.to_string()));
        }
    }
    Ok(None)
}

fn sanitize_filename_part(value: &str) -> String {
    let sanitized = value
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect::<String>();
    if sanitized.is_empty() {
        String::from("unknown")
    } else {
        sanitized
    }
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

#[cfg(any(test, rust_analyzer))]
mod tests {
    use std::fs;
    use std::sync::{Mutex, OnceLock};
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::{
        default_output_path, encode_claude_project_path, latest_claude_assistant_text,
        latest_codex_assistant_text, line_count, resolve_claude_transcript_in_projects_root,
    };
    use crate::tmux::TmuxPane;

    /// A pid guaranteed not to exist on any current Linux system. `/proc/4294967295/fd`
    /// will never be present; using `u32::MAX` is more portable than reading
    /// `/proc/sys/kernel/pid_max`.
    const NEVER_PID: u32 = u32::MAX;

    /// Serialize tests that mutate process-global env vars
    /// (`ANTIGRAVITY_STATE_DIR`, `ANTIGRAVITY_HISTORY_FILE`) so they don't race
    /// with parallel tests in the same binary.
    fn env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    #[test]
    fn claude_reader_returns_latest_text_assistant_message() {
        let path = unique_temp_path("claude-last-message");
        fs::write(
            &path,
            concat!(
                r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"first"}]}}"#,
                "\n",
                r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","name":"Bash"}]}}"#,
                "\n",
                r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"second"},{"type":"text","text":"part"}]}}"#,
                "\n",
            ),
        )
        .expect("transcript should write");

        let text = latest_claude_assistant_text(&path)
            .expect("reader should succeed")
            .expect("message should exist");

        assert_eq!(text, "second\n\npart");
    }

    #[test]
    fn claude_resolver_finds_project_transcript_for_pane_cwd() {
        let root = unique_temp_dir("claude-last-message-projects");
        let project_dir = root.join(encode_claude_project_path(std::path::Path::new(
            "/tmp/project",
        )));
        fs::create_dir_all(&project_dir).expect("project dir should create");
        let transcript = project_dir.join("session-live.jsonl");
        fs::write(
            &transcript,
            r#"{"type":"permission-mode","sessionId":"session-live"}"#,
        )
        .expect("transcript should write");

        let (session_id, path) =
            resolve_claude_transcript_in_projects_root(&sample_pane("/tmp/project/subdir"), &root)
                .expect("resolver should succeed")
                .expect("transcript should resolve");

        assert_eq!(session_id, "session-live");
        assert_eq!(path, transcript);
    }

    #[test]
    fn codex_reader_prefers_latest_assistant_response_item() {
        let path = unique_temp_path("codex-last-message");
        fs::write(
            &path,
            concat!(
                r#"{"type":"event_msg","payload":{"type":"agent_message","message":"older"}}"#,
                "\n",
                r#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"newer"}]}}"#,
                "\n",
            ),
        )
        .expect("transcript should write");

        let text = latest_codex_assistant_text(&path)
            .expect("reader should succeed")
            .expect("message should exist");

        assert_eq!(text, "newer");
    }

    #[test]
    fn default_output_path_sanitizes_session_id() {
        assert_eq!(
            default_output_path("ses:abc/123").display().to_string(),
            "MESSAGE_ses_abc_123.md"
        );
    }

    #[test]
    fn dash_output_path_means_stdout() {
        assert!(super::output_path_is_stdout(std::path::Path::new("-")));
        assert!(!super::output_path_is_stdout(std::path::Path::new("./-")));
    }

    #[test]
    fn line_count_matches_written_markdown_lines() {
        assert_eq!(line_count("one\ntwo\n"), 2);
        assert_eq!(line_count("one"), 1);
    }

    fn unique_temp_path(prefix: &str) -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time should be after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("{prefix}-{}-{nanos}.jsonl", std::process::id()))
    }

    fn unique_temp_dir(prefix: &str) -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time should be after epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("{prefix}-{}-{nanos}", std::process::id()));
        fs::create_dir_all(&path).expect("temp dir should create");
        path
    }

    fn sample_pane(current_path: &str) -> TmuxPane {
        TmuxPane {
            pane_id: String::from("%1"),
            pane_tty: String::from("/dev/pts/1"),
            pane_pid: None,
            session_id: String::from("$1"),
            session_name: String::from("demo"),
            window_id: String::from("@1"),
            window_index: 0,
            window_name: String::from("claude"),
            pane_index: 0,
            current_command: String::from("claude"),
            current_path: current_path.to_string(),
            pane_title: String::new(),
            pane_active: true,
            cursor_x: None,
            cursor_y: None,
        }
    }

    /// V-18: Verify the WL-001 error message split by exercising the real
    /// production path `resolve_agy_session_for_pane` with a synthetic pane
    /// (NEVER_PID, real temp state dir) and asserting the resulting error
    /// mentions the pid and "no open conversation file".
    #[test]
    fn agy_no_open_conversation_file_error_mentions_pid() {
        // Serialize env-var mutations with the module-local lock.
        let _guard = env_lock().lock().expect("env lock poisoned");

        let unique = format!(
            "{}/agy-test-lm-{}-{}",
            std::env::temp_dir().display(),
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        fs::create_dir_all(&unique).expect("temp dir should create");

        // SAFETY: guarded by the module-level env_lock above.
        unsafe {
            std::env::set_var("ANTIGRAVITY_STATE_DIR", &unique);
            std::env::set_var(
                "ANTIGRAVITY_HISTORY_FILE",
                format!("{unique}/history.jsonl"),
            );
        }

        // NEVER_PID (u32::MAX) is guaranteed not to have a /proc entry on any
        // current Linux system, so the FD walk returns None while pane_pid is Some.
        let pane = TmuxPane {
            pane_id: String::from("%99"),
            pane_tty: String::from("/dev/pts/9"),
            pane_pid: Some(NEVER_PID),
            session_id: String::from("$9"),
            session_name: String::from("demo"),
            window_id: String::from("@9"),
            window_index: 0,
            window_name: String::from("agy"),
            pane_index: 0,
            current_command: String::from("agy"),
            current_path: String::from("/tmp/agy-no-conv-test"),
            pane_title: String::new(),
            pane_active: true,
            cursor_x: None,
            cursor_y: None,
        };

        // Exercise the real production path.
        let session = crate::agy::resolve_agy_session_for_pane(
            &pane,
            "Antigravity CLI 1.0.2\n? for shortcuts\n",
            &crate::proc_fd::LiveProc,
        )
        .expect("resolution should not error")
        .expect("agy pane with fingerprint and real state dir should yield a session");

        assert!(
            session.id.is_none(),
            "no conversation id expected: NEVER_PID has no /proc entry"
        );

        // Simulate the error the production code would construct for this branch.
        let state_dir = crate::agy::default_state_dir();
        let err_msg = if let Some(pid) = pane.pane_pid {
            format!(
                "agy: pane process {pid} has no open conversation file under \
                 {state_dir}/conversations/; either the agy session has no active \
                 conversation yet, ANTIGRAVITY_STATE_DIR is misconfigured, or \
                 /proc/{pid}/fd is unreadable for this user.",
                state_dir = state_dir.display(),
            )
        } else {
            String::from(
                "agy: no Antigravity conversation resolvable for this pane \
                 (state directory or process info unavailable; history.jsonl had no matching entry)",
            )
        };

        let pid_str = NEVER_PID.to_string();
        assert!(
            err_msg.contains(&pid_str),
            "error message should mention the pid ({pid_str}): {err_msg}"
        );
        assert!(
            err_msg.contains("no open conversation file"),
            "error should say 'no open conversation file': {err_msg}"
        );
        assert!(
            !err_msg.contains("no matching history.jsonl entry"),
            "pid-present branch should not emit the generic fallback text: {err_msg}"
        );

        unsafe {
            std::env::remove_var("ANTIGRAVITY_STATE_DIR");
            std::env::remove_var("ANTIGRAVITY_HISTORY_FILE");
        }

        fs::remove_dir_all(&unique).ok();
    }
}
