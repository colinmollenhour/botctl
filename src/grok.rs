//! Grok Build TUI integration.
//!
//! Passive read-only discovery: pane process name `grok`, session identity
//! via `~/.grok/active_sessions.json` (PID match in the pane process tree),
//! open `events.jsonl` FD walk, or latest session for the pane cwd under
//! `~/.grok/sessions/<urlencode(cwd)>/`. State classification is screen-first
//! (BusyResponding / ChatReady). `last-message` rebuilds the latest assistant
//! turn from `updates.jsonl` (`agent_message_chunk` stream).
//!
//! No YOLO, prompt submission, keybinding automation, or managed MCP spawn.

use std::collections::HashSet;
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::Deserialize;
use serde_json::Value;

use crate::app::AppResult;
use crate::classifier::SessionState;
use crate::proc_fd::{ChildResolver, transcript_from_process_tree_fds_with_resolver};
use crate::tmux::TmuxPane;

const GROK_HOME_DIR: &str = ".grok";
const GROK_SESSIONS_DIR: &str = "sessions";
const GROK_ACTIVE_SESSIONS: &str = "active_sessions.json";
const GROK_CONTEXT_LINE_LIMIT: usize = 80;
const GROK_CONTEXT_TEXT_LIMIT: usize = 4000;
/// Cap how far we scan from the end of updates.jsonl when extracting the last
/// assistant message (bytes). Keeps last-message cheap on huge sessions.
const UPDATES_TAIL_WINDOW_BYTES: u64 = 2 * 1024 * 1024;

const SPINNER_GLYPHS: &[char] = &[
    '⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏', '⣾', '⣷', '⣯', '⣟', '⡿', '⢿', '⣻', '⣽',
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrokSession {
    pub id: String,
    pub path: PathBuf,
    pub cwd: String,
    pub title: Option<String>,
    pub model: Option<String>,
    pub agent_name: Option<String>,
    pub state: SessionState,
    pub has_questions: bool,
    pub context: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrokLastMessage {
    pub session_id: String,
    pub text: String,
}

#[derive(Debug, Clone, Deserialize)]
struct ActiveSessionEntry {
    session_id: String,
    pid: u32,
    cwd: String,
}

#[derive(Debug, Clone)]
struct ResolvedSession {
    id: String,
    path: PathBuf,
    cwd: String,
}

pub fn is_grok_pane(pane: &TmuxPane) -> bool {
    pane.current_command.eq_ignore_ascii_case("grok")
}

pub fn resolve_grok_session_for_pane(
    pane: &TmuxPane,
    frame: &str,
    resolver: &dyn ChildResolver,
) -> AppResult<Option<GrokSession>> {
    if !is_grok_pane(pane) {
        return Ok(None);
    }

    let Some(resolved) = resolve_grok_session_identity(pane, resolver)? else {
        // Pane is grok but no session on disk yet — still a visible provider;
        // callers that need id/title get None from higher-level helpers.
        let state = classify_grok_state(frame).unwrap_or(SessionState::Unknown);
        return Ok(Some(GrokSession {
            id: String::new(),
            path: PathBuf::new(),
            cwd: pane.current_path.clone(),
            title: None,
            model: None,
            agent_name: None,
            state,
            has_questions: false,
            context: String::from("No Grok session resolved for pane."),
        }));
    };

    let summary = read_summary(&resolved.path.join("summary.json"))?.unwrap_or_default();
    let state = classify_grok_state(frame).unwrap_or(SessionState::Unknown);
    let last_text = latest_assistant_text_from_updates(&resolved.path.join("updates.jsonl"))?
        .unwrap_or_default();
    let has_questions = latest_assistant_text_has_question(&last_text);
    let context = if last_text.trim().is_empty() {
        String::from("No Grok message context found.")
    } else {
        compact_context(&last_text)
    };

    let mut effective_state = state;
    if effective_state == SessionState::ChatReady && has_questions {
        effective_state = SessionState::UserQuestionPrompt;
    }

    Ok(Some(GrokSession {
        id: resolved.id,
        path: resolved.path,
        cwd: resolved.cwd,
        title: summary.title,
        model: summary.model,
        agent_name: summary.agent_name,
        state: effective_state,
        has_questions,
        context,
    }))
}

pub fn latest_assistant_message_for_pane(
    pane: &TmuxPane,
    resolver: &dyn ChildResolver,
) -> AppResult<Option<GrokLastMessage>> {
    if !is_grok_pane(pane) {
        return Ok(None);
    }
    let Some(resolved) = resolve_grok_session_identity(pane, resolver)? else {
        return Ok(None);
    };
    if resolved.id.is_empty() {
        return Ok(None);
    }
    let Some(text) =
        latest_assistant_text_from_updates(&resolved.path.join("updates.jsonl"))?
            .filter(|text| !text.trim().is_empty())
    else {
        return Ok(None);
    };
    Ok(Some(GrokLastMessage {
        session_id: resolved.id,
        text,
    }))
}

/// Screen fingerprint unique enough to claim a frame as Grok Build TUI.
pub fn frame_has_grok_fingerprint(frame: &str) -> bool {
    let mut has_model_footer = false;
    let mut has_send_to_bg = false;
    let mut has_shortcuts = false;
    let mut has_always_approve = false;

    for line in frame.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        // Prompt footer: `Grok 4.5 (high) · always-approve`
        if trimmed.contains("Grok ")
            && (trimmed.contains("always-approve")
                || trimmed.contains('·')
                || trimmed.contains("bypass")
                || trimmed.contains("acceptEdits")
                || trimmed.contains("dontAsk")
                || trimmed.contains("default"))
        {
            has_model_footer = true;
        }
        if trimmed.contains("Ctrl+g:send to bg") {
            has_send_to_bg = true;
        }
        if trimmed.contains("Ctrl+.:shortcuts") {
            has_shortcuts = true;
        }
        if trimmed.contains("always-approve") {
            has_always_approve = true;
        }
    }

    has_model_footer
        || has_send_to_bg
        || (has_shortcuts && has_always_approve)
        || (has_shortcuts && frame.contains("Grok "))
}

/// Classify a Grok frame. Returns `None` when the frame does not fingerprint
/// as Grok (so the shared classifier can fall through to other providers).
pub fn classify_grok_state(frame: &str) -> Option<SessionState> {
    if !frame_has_grok_fingerprint(frame) {
        return None;
    }

    let lines: Vec<&str> = frame.lines().map(str::trim).collect();
    let tail_window = lines
        .iter()
        .rev()
        .filter(|line| !line.is_empty())
        .take(10)
        .copied()
        .collect::<Vec<_>>();

    if tail_window.iter().any(|line| is_grok_busy_status_line(line)) {
        return Some(SessionState::BusyResponding);
    }

    // Idle chrome: model footer and/or shortcut bar without a busy status line.
    if tail_window.iter().any(|line| {
        line.contains("Grok ")
            || line.contains("Ctrl+.:shortcuts")
            || line.contains("Ctrl+e:expand thinking")
            || line.contains("←:collapse")
            || line.contains("Shift+Tab:mode")
    }) {
        return Some(SessionState::ChatReady);
    }

    Some(SessionState::Unknown)
}

fn is_grok_busy_status_line(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    // Background tool still active while the prompt chrome is otherwise idle.
    // Do not key on historical "Task started:" scrollback alone — that can
    // remain visible after the task finishes and would false-busy the pane.
    if lower.contains("command still running") || lower.contains("commands still running") {
        return true;
    }

    let has_spinner = line.chars().any(|ch| SPINNER_GLYPHS.contains(&ch));
    if !has_spinner {
        // "Waiting for response…" without spinner still counts when paired with [stop]
        return (lower.contains("waiting for response") || lower.contains("thinking"))
            && (line.contains("[stop]") || line.contains("⇣"));
    }

    line.contains("[stop]")
        || lower.contains("waiting for response")
        || line.contains("⇣")
        || line_has_elapsed_token(line)
}

fn line_has_elapsed_token(line: &str) -> bool {
    // Matches `1m53s`, `42s`, `0.2s` style timers commonly shown next to the spinner.
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i].is_ascii_digit() {
            let start = i;
            while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b'.') {
                i += 1;
            }
            if i > start {
                let rest = &line[i..];
                if rest.starts_with('s')
                    || rest.starts_with("ms")
                    || rest.starts_with('m')
                    || rest.starts_with('h')
                {
                    return true;
                }
            }
        } else {
            i += 1;
        }
    }
    false
}

fn resolve_grok_session_identity(
    pane: &TmuxPane,
    resolver: &dyn ChildResolver,
) -> AppResult<Option<ResolvedSession>> {
    let home = default_grok_home();
    let sessions_root = home.join(GROK_SESSIONS_DIR);

    if let Some(pid) = pane.pane_pid.filter(|&p| p != 0) {
        if let Some(resolved) =
            resolve_from_active_sessions(&home, &sessions_root, pid, resolver)?
        {
            return Ok(Some(resolved));
        }

        if let Some(path) = transcript_from_process_tree_fds_with_resolver(pid, resolver, |target| {
            target.starts_with(&sessions_root)
                && target
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name == "events.jsonl")
        })? && let Some(resolved) = resolved_from_events_path(&path)
        {
            return Ok(Some(resolved));
        }
    }

    latest_session_for_cwd(&sessions_root, &pane.current_path)
}

fn resolve_from_active_sessions(
    home: &Path,
    sessions_root: &Path,
    pane_pid: u32,
    resolver: &dyn ChildResolver,
) -> AppResult<Option<ResolvedSession>> {
    let active_path = home.join(GROK_ACTIVE_SESSIONS);
    let content = match fs::read_to_string(&active_path) {
        Ok(content) => content,
        Err(_) => return Ok(None),
    };
    let entries: Vec<ActiveSessionEntry> = match serde_json::from_str(&content) {
        Ok(entries) => entries,
        Err(_) => return Ok(None),
    };
    if entries.is_empty() {
        return Ok(None);
    }

    let tree_pids = collect_process_tree_pids(pane_pid, resolver);
    let mut matches = Vec::new();
    for entry in entries {
        if tree_pids.contains(&entry.pid) {
            matches.push(entry);
        }
    }
    if matches.len() != 1 {
        return Ok(None);
    }
    let entry = matches.remove(0);
    let path = sessions_root
        .join(encode_grok_cwd(&entry.cwd))
        .join(&entry.session_id);
    if !path.is_dir() {
        // Still return identity; updates/summary may appear later.
        return Ok(Some(ResolvedSession {
            id: entry.session_id,
            path,
            cwd: entry.cwd,
        }));
    }
    Ok(Some(ResolvedSession {
        id: entry.session_id,
        path,
        cwd: entry.cwd,
    }))
}

fn collect_process_tree_pids(root: u32, resolver: &dyn ChildResolver) -> HashSet<u32> {
    let mut seen = HashSet::new();
    let mut stack = vec![root];
    while let Some(pid) = stack.pop() {
        if !seen.insert(pid) {
            continue;
        }
        stack.extend(resolver.children_of(pid));
    }
    seen
}

fn resolved_from_events_path(events_path: &Path) -> Option<ResolvedSession> {
    // .../<encoded-cwd>/<session-id>/events.jsonl
    let session_dir = events_path.parent()?;
    let id = session_dir.file_name()?.to_str()?.to_string();
    if id.is_empty() {
        return None;
    }
    let cwd = match read_summary(&session_dir.join("summary.json")) {
        Ok(Some(summary)) => summary.cwd.unwrap_or_default(),
        _ => String::new(),
    };
    Some(ResolvedSession {
        id,
        path: session_dir.to_path_buf(),
        cwd,
    })
}

fn latest_session_for_cwd(
    sessions_root: &Path,
    current_path: &str,
) -> AppResult<Option<ResolvedSession>> {
    let group = sessions_root.join(encode_grok_cwd(current_path));
    if !group.is_dir() {
        return Ok(None);
    }

    let mut latest = None::<(SystemTime, ResolvedSession)>;
    let entries = match fs::read_dir(&group) {
        Ok(entries) => entries,
        Err(_) => return Ok(None),
    };
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(id) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if id.starts_with('.') {
            continue;
        }
        let summary_path = path.join("summary.json");
        let summary = match read_summary(&summary_path)? {
            Some(summary) => summary,
            None => continue,
        };
        if let Some(cwd) = summary.cwd.as_deref()
            && cwd != current_path
        {
            continue;
        }
        // Prefer updates.jsonl mtime: Grok keeps appending conversation
        // updates while summary.json can lag and leave cwd-fallback stuck on
        // a stale session.
        let modified = fs::metadata(path.join("updates.jsonl"))
            .and_then(|meta| meta.modified())
            .or_else(|_| fs::metadata(&summary_path).and_then(|meta| meta.modified()))
            .or_else(|_| fs::metadata(&path).and_then(|meta| meta.modified()))
            .unwrap_or(SystemTime::UNIX_EPOCH);
        let candidate = ResolvedSession {
            id: id.to_string(),
            path: path.clone(),
            cwd: summary.cwd.unwrap_or_else(|| current_path.to_string()),
        };
        if latest
            .as_ref()
            .map(|(latest_modified, _)| modified > *latest_modified)
            .unwrap_or(true)
        {
            latest = Some((modified, candidate));
        }
    }
    Ok(latest.map(|(_, session)| session))
}

#[derive(Debug, Clone, Default)]
struct SummaryInfo {
    title: Option<String>,
    model: Option<String>,
    agent_name: Option<String>,
    cwd: Option<String>,
}

fn read_summary(path: &Path) -> AppResult<Option<SummaryInfo>> {
    let content = match fs::read_to_string(path) {
        Ok(content) => content,
        Err(_) => return Ok(None),
    };
    let value: Value = match serde_json::from_str(&content) {
        Ok(value) => value,
        Err(_) => return Ok(None),
    };
    let title = value
        .get("generated_title")
        .and_then(Value::as_str)
        .or_else(|| value.get("session_summary").and_then(Value::as_str))
        .filter(|text| !text.trim().is_empty())
        .map(str::to_string);
    let model = value
        .get("current_model_id")
        .and_then(Value::as_str)
        .map(str::to_string);
    let agent_name = value
        .get("agent_name")
        .and_then(Value::as_str)
        .map(str::to_string);
    let cwd = value
        .pointer("/info/cwd")
        .and_then(Value::as_str)
        .map(str::to_string);
    Ok(Some(SummaryInfo {
        title,
        model,
        agent_name,
        cwd,
    }))
}

/// Reconstruct the latest non-empty assistant turn from the ACP-style
/// `updates.jsonl` stream by concatenating consecutive `agent_message_chunk`
/// text pieces until a user message or end-of-file.
pub fn latest_assistant_text_from_updates(path: &Path) -> AppResult<Option<String>> {
    let content = match read_file_tail(path, UPDATES_TAIL_WINDOW_BYTES) {
        Ok(content) => content,
        Err(_) => return Ok(None),
    };

    let mut turns: Vec<(String, String)> = Vec::new(); // (role, text)
    let mut current: Option<(String, String)> = None;

    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let Some(update) = value
            .pointer("/params/update")
            .or_else(|| value.get("update"))
        else {
            continue;
        };
        let Some(kind) = update.get("sessionUpdate").and_then(Value::as_str) else {
            continue;
        };
        match kind {
            "user_message_chunk" => {
                let text = text_from_update_content(update.get("content")).unwrap_or_default();
                match current.as_mut() {
                    Some((role, buf)) if role == "user" => buf.push_str(&text),
                    _ => {
                        if let Some(finished) = current.take() {
                            turns.push(finished);
                        }
                        current = Some((String::from("user"), text));
                    }
                }
            }
            "agent_message_chunk" => {
                let text = text_from_update_content(update.get("content")).unwrap_or_default();
                match current.as_mut() {
                    Some((role, buf)) if role == "assistant" => buf.push_str(&text),
                    _ => {
                        if let Some(finished) = current.take() {
                            turns.push(finished);
                        }
                        current = Some((String::from("assistant"), text));
                    }
                }
            }
            // Tool activity does not end an assistant turn; more chunks may follow.
            "tool_call" | "tool_call_update" | "agent_thought_chunk" => {}
            _ => {}
        }
    }
    if let Some(finished) = current.take() {
        turns.push(finished);
    }

    Ok(turns
        .into_iter()
        .rev()
        .find(|(role, text)| role == "assistant" && !text.trim().is_empty())
        .map(|(_, text)| text))
}

fn text_from_update_content(content: Option<&Value>) -> Option<String> {
    let content = content?;
    if let Some(text) = content.get("text").and_then(Value::as_str) {
        return Some(text.to_string());
    }
    if content.get("type").and_then(Value::as_str) == Some("text")
        && let Some(text) = content.get("text").and_then(Value::as_str)
    {
        return Some(text.to_string());
    }
    None
}

fn read_file_tail(path: &Path, max_bytes: u64) -> std::io::Result<String> {
    let mut file = fs::File::open(path)?;
    let len = file.metadata()?.len();
    if len > max_bytes {
        file.seek(SeekFrom::Start(len - max_bytes))?;
    }
    // Read as bytes first so a seek that lands mid-UTF-8 sequence does not
    // fail the whole last-message extract via read_to_string.
    let mut buf = Vec::new();
    file.read_to_end(&mut buf)?;
    // If we started mid-line, drop the partial first line.
    if len > max_bytes {
        let Some(idx) = buf.iter().position(|byte| *byte == b'\n') else {
            return Ok(String::new());
        };
        buf.drain(..=idx);
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

fn latest_assistant_text_has_question(text: &str) -> bool {
    text.lines()
        .rev()
        .take(12)
        .any(|line| line.trim_end().ends_with('?'))
}

fn compact_context(text: &str) -> String {
    let compact = text
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed)
            }
        })
        .take(GROK_CONTEXT_LINE_LIMIT)
        .collect::<Vec<_>>()
        .join("\n");

    if compact.chars().count() <= GROK_CONTEXT_TEXT_LIMIT {
        return compact;
    }

    let mut truncated = compact
        .chars()
        .take(GROK_CONTEXT_TEXT_LIMIT)
        .collect::<String>();
    truncated.push('…');
    truncated
}

pub fn default_grok_home() -> PathBuf {
    if let Some(dir) = std::env::var_os("GROK_HOME")
        && !dir.is_empty()
    {
        return PathBuf::from(dir);
    }
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(GROK_HOME_DIR)
}

/// Percent-encode a cwd the way Grok groups sessions (`quote(path, safe='')`).
pub fn encode_grok_cwd(path: &str) -> String {
    let mut out = String::with_capacity(path.len() * 3);
    for byte in path.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            _ => {
                use std::fmt::Write as _;
                let _ = write!(out, "%{byte:02X}");
            }
        }
    }
    out
}

#[cfg(any(test, rust_analyzer))]
mod tests {
    use super::*;
    use std::sync::{Mutex, OnceLock};
    use std::time::{SystemTime, UNIX_EPOCH};

    use crate::proc_fd::ChildResolver;

    /// Serialize tests that mutate process-global `GROK_HOME`.
    fn env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    struct GrokHomeGuard {
        previous: Option<std::ffi::OsString>,
    }

    impl GrokHomeGuard {
        fn install(home: &Path) -> Self {
            let previous = std::env::var_os("GROK_HOME");
            // SAFETY: callers hold env_lock for the lifetime of this guard.
            unsafe {
                std::env::set_var("GROK_HOME", home);
            }
            Self { previous }
        }
    }

    impl Drop for GrokHomeGuard {
        fn drop(&mut self) {
            // SAFETY: callers hold env_lock for the lifetime of this guard.
            unsafe {
                match &self.previous {
                    Some(value) => std::env::set_var("GROK_HOME", value),
                    None => std::env::remove_var("GROK_HOME"),
                }
            }
        }
    }

    struct MapResolver {
        children: std::collections::HashMap<u32, Vec<u32>>,
    }

    impl ChildResolver for MapResolver {
        fn children_of(&self, pid: u32) -> Vec<u32> {
            self.children.get(&pid).cloned().unwrap_or_default()
        }
    }

    #[test]
    fn encodes_cwd_like_python_quote_safe_empty() {
        assert_eq!(
            encode_grok_cwd("/home/colin/Projects/botctl"),
            "%2Fhome%2Fcolin%2FProjects%2Fbotctl"
        );
    }

    #[test]
    fn fingerprints_grok_footer_and_shortcuts() {
        let ready = concat!(
            "  ╭──────────────────────────────────────────────╮\n",
            "  │ ❯ Build anything                             │\n",
            "  ╰──────────────────────────── Grok 4.5 (high) · always-approve ─╯\n",
            "  ←:collapse  │  Ctrl+e:expand thinking  │  Ctrl+.:shortcuts\n",
        );
        assert!(frame_has_grok_fingerprint(ready));
        assert_eq!(classify_grok_state(ready), Some(SessionState::ChatReady));

        let busy = concat!(
            "    ⠸ Install deps if missing for format/tests… 1.3s   5m10s ⇣116k [↓][stop]\n",
            "  ╭──────────────────────────────────────────────╮\n",
            "  │ ❯ Build anything                             │\n",
            "  ╰──────────────────────────── Grok 4.5 (high) · always-approve ─╯\n",
            "  Ctrl+e:expand thinking  │  Space:prompt  │  Ctrl+c:cancel  │  Ctrl+.:shortcuts\n",
        );
        assert!(frame_has_grok_fingerprint(busy));
        assert_eq!(
            classify_grok_state(busy),
            Some(SessionState::BusyResponding)
        );

        assert!(!frame_has_grok_fingerprint("just claude output\n? for shortcuts\n"));
    }

    #[test]
    fn busy_waiting_for_response_line() {
        let frame = concat!(
            "    ⠧ Waiting for response… 33s   4m54s ⇣115k [stop]\n",
            "  ╭──────────────────────────────────────────────╮\n",
            "  │ ❯                                            │\n",
            "  ╰──────────────────────────── Grok 4.5 (high) · always-approve ─╯\n",
            "  Ctrl+c:cancel  │  Ctrl+g:send to bg  │  Ctrl+.:shortcuts\n",
        );
        assert_eq!(
            classify_grok_state(frame),
            Some(SessionState::BusyResponding)
        );
    }

    #[test]
    fn busy_when_background_command_still_running() {
        let frame = concat!(
            "     ◆ Task started: Live verify status\n",
            "     Worked for 6m18s. 1 command still running.\n",
            "  ╭──────────────────────────────────────────────╮\n",
            "  │ ❯                                            │\n",
            "  ╰──────────────────────────── Grok 4.5 (high) · always-approve ─╯\n",
            "  Shift+Tab:mode  │  Ctrl+.:shortcuts\n",
        );
        assert_eq!(
            classify_grok_state(frame),
            Some(SessionState::BusyResponding)
        );
    }

    #[test]
    fn historical_task_started_alone_is_not_busy() {
        let frame = concat!(
            "     ◆ Task started: finished work\n",
            "     Worked for 42s.\n",
            "  ╭──────────────────────────────────────────────╮\n",
            "  │ ❯ Build anything                             │\n",
            "  ╰──────────────────────────── Grok 4.5 (high) · always-approve ─╯\n",
            "  ←:collapse  │  Ctrl+.:shortcuts\n",
        );
        assert_eq!(classify_grok_state(frame), Some(SessionState::ChatReady));
    }

    #[test]
    fn reconstructs_assistant_text_from_updates() {
        let dir = unique_temp_dir("grok-updates");
        let path = dir.join("updates.jsonl");
        fs::write(
            &path,
            concat!(
                r#"{"timestamp":1,"method":"session/update","params":{"sessionId":"s1","update":{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"hello"}}}}"#,
                "\n",
                r#"{"timestamp":2,"method":"session/update","params":{"sessionId":"s1","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"part one "}}}}"#,
                "\n",
                r#"{"timestamp":3,"method":"session/update","params":{"sessionId":"s1","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"part two"}}}}"#,
                "\n",
                r#"{"timestamp":4,"method":"session/update","params":{"sessionId":"s1","update":{"sessionUpdate":"tool_call","toolCallId":"c1","title":"bash"}}}"#,
                "\n",
                r#"{"timestamp":5,"method":"session/update","params":{"sessionId":"s1","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":" after tool"}}}}"#,
                "\n",
            ),
        )
        .expect("write updates");

        let text = latest_assistant_text_from_updates(&path)
            .expect("read")
            .expect("text");
        assert_eq!(text, "part one part two after tool");
    }

    #[test]
    fn resolves_active_session_by_process_tree_pid() {
        let home = unique_temp_dir("grok-home");
        let cwd = "/tmp/grok-project";
        let session_id = "019f67b2-f34f-7b61-9d9f-2eb5554ace8a";
        let session_dir = home
            .join(GROK_SESSIONS_DIR)
            .join(encode_grok_cwd(cwd))
            .join(session_id);
        fs::create_dir_all(&session_dir).expect("session dir");
        fs::write(
            session_dir.join("summary.json"),
            format!(
                r#"{{"info":{{"id":"{session_id}","cwd":"{cwd}"}},"generated_title":"Demo","current_model_id":"grok-4.5"}}"#
            ),
        )
        .expect("summary");
        fs::write(
            home.join(GROK_ACTIVE_SESSIONS),
            format!(
                r#"[{{"session_id":"{session_id}","pid":200,"cwd":"{cwd}","opened_at":"2026-01-01T00:00:00Z"}}]"#
            ),
        )
        .expect("active");

        let mut children = std::collections::HashMap::new();
        children.insert(100, vec![200]);
        let resolver = MapResolver { children };

        let _lock = env_lock().lock().expect("env lock poisoned");
        let _home_guard = GrokHomeGuard::install(&home);

        let pane = TmuxPane {
            pane_id: String::from("%1"),
            pane_tty: String::from("/dev/pts/1"),
            pane_pid: Some(100),
            session_id: String::from("$1"),
            session_name: String::from("demo"),
            window_id: String::from("@1"),
            window_index: 0,
            window_name: String::from("demo"),
            pane_index: 0,
            current_command: String::from("grok"),
            current_path: cwd.to_string(),
            pane_title: String::from("Demo - grok"),
            pane_active: true,
            cursor_x: Some(0),
            cursor_y: Some(0),
        };

        let frame = concat!(
            "  ╭──────────────────────────────────────────────╮\n",
            "  │ ❯                                            │\n",
            "  ╰──────────────────────────── Grok 4.5 (high) · always-approve ─╯\n",
            "  Ctrl+.:shortcuts\n",
        );
        let session = resolve_grok_session_for_pane(&pane, frame, &resolver)
            .expect("resolve")
            .expect("session");
        assert_eq!(session.id, session_id);
        assert_eq!(session.title.as_deref(), Some("Demo"));
        assert_eq!(session.state, SessionState::ChatReady);
    }

    fn unique_temp_dir(prefix: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "botctl-{prefix}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("time")
                .as_nanos()
        ));
        fs::create_dir_all(&path).expect("temp dir");
        path
    }
}
