use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde_json::Value;

use crate::app::AppResult;
use crate::classifier::SessionState;
use crate::proc_fd::transcript_from_process_tree_fds;
use crate::tmux::TmuxPane;

const PI_AGENT_DIR: &str = ".pi/agent";
const PI_SESSION_DIR_NAME: &str = "sessions";
const PI_CONTEXT_LINE_LIMIT: usize = 80;
const PI_CONTEXT_TEXT_LIMIT: usize = 4000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PiSession {
    pub id: String,
    pub path: PathBuf,
    pub cwd: String,
    pub state: SessionState,
    pub has_questions: bool,
    pub context: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PiLastMessage {
    pub session_id: String,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PiSessionMeta {
    id: String,
    cwd: String,
}

pub fn is_pi_pane(pane: &TmuxPane) -> bool {
    pane.current_command.eq_ignore_ascii_case("pi")
}

pub fn resolve_pi_session_for_pane(pane: &TmuxPane) -> AppResult<Option<PiSession>> {
    if !is_pi_pane(pane) {
        return Ok(None);
    }
    let Some((session_id, path, cwd)) = resolve_pi_transcript_for_pane(pane)? else {
        return Ok(None);
    };
    let state = pi_session_state(&path)?.unwrap_or(SessionState::ChatReady);
    let last_text = latest_pi_assistant_text(&path)?.unwrap_or_default();
    let has_questions = latest_assistant_text_has_question(&last_text);
    let context =
        pi_session_context(&path)?.unwrap_or_else(|| String::from("No Pi message context found."));
    Ok(Some(PiSession {
        id: session_id,
        path,
        cwd,
        state: if state == SessionState::ChatReady && has_questions {
            SessionState::UserQuestionPrompt
        } else {
            state
        },
        has_questions,
        context,
    }))
}

pub fn latest_assistant_message_for_pane(pane: &TmuxPane) -> AppResult<Option<PiLastMessage>> {
    if !is_pi_pane(pane) {
        return Ok(None);
    }
    let Some((session_id, path, _)) = resolve_pi_transcript_for_pane(pane)? else {
        return Ok(None);
    };
    let Some(text) = latest_pi_assistant_text(&path)?.filter(|text| !text.trim().is_empty()) else {
        return Ok(None);
    };
    Ok(Some(PiLastMessage { session_id, text }))
}

fn resolve_pi_transcript_for_pane(pane: &TmuxPane) -> AppResult<Option<(String, PathBuf, String)>> {
    let sessions_root = default_pi_sessions_root();

    if let Some(pid) = pane.pane_pid
        && let Some(transcript) = transcript_from_process_tree_fds(pid, &sessions_root, "jsonl")?
        && let Some(meta) = pi_session_meta(&transcript)?
    {
        return Ok(Some((meta.id, transcript, meta.cwd)));
    }

    latest_pi_transcript_for_cwd(&sessions_root, &pane.current_path)
}

fn default_pi_sessions_root() -> PathBuf {
    if let Some(dir) = std::env::var_os("PI_CODING_AGENT_SESSION_DIR")
        && !dir.is_empty()
    {
        return PathBuf::from(dir);
    }

    if let Some(dir) = std::env::var_os("PI_CODING_AGENT_DIR")
        && !dir.is_empty()
    {
        return PathBuf::from(dir).join(PI_SESSION_DIR_NAME);
    }

    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(PI_AGENT_DIR)
        .join(PI_SESSION_DIR_NAME)
}

fn latest_pi_transcript_for_cwd(
    sessions_root: &Path,
    current_path: &str,
) -> AppResult<Option<(String, PathBuf, String)>> {
    let mut latest = None::<(SystemTime, String, PathBuf, String)>;
    for path in candidate_pi_session_files(sessions_root, current_path)? {
        let Some(meta) = pi_session_meta(&path)? else {
            continue;
        };
        if meta.cwd != current_path {
            continue;
        }
        let modified = fs::metadata(&path)?
            .modified()
            .unwrap_or(SystemTime::UNIX_EPOCH);
        if latest
            .as_ref()
            .map(|(latest_modified, _, _, _)| modified > *latest_modified)
            .unwrap_or(true)
        {
            latest = Some((modified, meta.id, path, meta.cwd));
        }
    }
    Ok(latest.map(|(_, session_id, path, cwd)| (session_id, path, cwd)))
}

fn candidate_pi_session_files(sessions_root: &Path, current_path: &str) -> AppResult<Vec<PathBuf>> {
    let mut files = Vec::new();
    for session_dir in candidate_pi_session_dirs(sessions_root, current_path) {
        collect_jsonl_files_in_dir(&session_dir, &mut files)?;
    }
    if files.is_empty() {
        collect_jsonl_files_in_dir(sessions_root, &mut files)?;
    }
    Ok(files)
}

fn candidate_pi_session_dirs(sessions_root: &Path, current_path: &str) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    for ancestor in Path::new(current_path).ancestors() {
        if ancestor.as_os_str().is_empty() {
            continue;
        }
        let candidate = sessions_root.join(encode_pi_session_path(ancestor));
        if candidate.is_dir() {
            candidates.push(candidate);
        }
    }
    candidates
}

fn encode_pi_session_path(path: &Path) -> String {
    let trimmed = path
        .display()
        .to_string()
        .trim_matches('/')
        .replace('/', "-");
    format!("--{trimmed}--")
}

fn collect_jsonl_files_in_dir(path: &Path, files: &mut Vec<PathBuf>) -> AppResult<()> {
    let entries = match fs::read_dir(path) {
        Ok(entries) => entries,
        Err(_) => return Ok(()),
    };
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            collect_jsonl_files_in_dir(&path, files)?;
        } else if path.extension().and_then(|value| value.to_str()) == Some("jsonl") {
            files.push(path);
        }
    }
    Ok(())
}

fn pi_session_meta(path: &Path) -> AppResult<Option<PiSessionMeta>> {
    let content = fs::read_to_string(path)?;
    for line in content.lines().take(8) {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if value.get("type").and_then(Value::as_str) != Some("session") {
            continue;
        }
        let Some(id) = value.get("id").and_then(Value::as_str) else {
            continue;
        };
        let Some(cwd) = value.get("cwd").and_then(Value::as_str) else {
            continue;
        };
        return Ok(Some(PiSessionMeta {
            id: id.to_string(),
            cwd: cwd.to_string(),
        }));
    }
    Ok(None)
}

fn pi_session_state(path: &Path) -> AppResult<Option<SessionState>> {
    let content = fs::read_to_string(path)?;
    let mut latest_role = None::<String>;
    let mut latest_stop_reason = None::<String>;
    for line in content.lines() {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        match value.get("type").and_then(Value::as_str) {
            Some("message") => {
                latest_role = value
                    .pointer("/message/role")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                latest_stop_reason = value
                    .pointer("/message/stopReason")
                    .and_then(Value::as_str)
                    .map(str::to_string);
            }
            Some("custom_message") => latest_role = Some(String::from("custom")),
            _ => {}
        }
    }

    Ok(match latest_role.as_deref() {
        Some("assistant") if latest_stop_reason.as_deref() == Some("stop") => {
            Some(SessionState::ChatReady)
        }
        Some("assistant") if latest_stop_reason.as_deref() == Some("aborted") => {
            Some(SessionState::ChatReady)
        }
        Some("assistant") if latest_stop_reason.as_deref() == Some("length") => {
            Some(SessionState::ChatReady)
        }
        Some("assistant") => Some(SessionState::BusyResponding),
        Some("user") | Some("toolResult") | Some("bashExecution") | Some("custom") => {
            Some(SessionState::BusyResponding)
        }
        Some("branchSummary") | Some("compactionSummary") => Some(SessionState::ChatReady),
        _ => None,
    })
}

fn latest_pi_assistant_text(path: &Path) -> AppResult<Option<String>> {
    let content = fs::read_to_string(path)?;
    let mut latest = None;
    for line in content.lines() {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if value.get("type").and_then(Value::as_str) != Some("message") {
            continue;
        }
        if value.pointer("/message/role").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        if let Some(text) = text_from_pi_content(value.pointer("/message/content"))
            .filter(|text| !text.trim().is_empty())
        {
            latest = Some(text);
        }
    }
    Ok(latest)
}

fn pi_session_context(path: &Path) -> AppResult<Option<String>> {
    let Some(text) = latest_pi_assistant_text(path)? else {
        return Ok(None);
    };
    Ok(Some(compact_context(&text)))
}

fn text_from_pi_content(content: Option<&Value>) -> Option<String> {
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
        .take(PI_CONTEXT_LINE_LIMIT)
        .collect::<Vec<_>>()
        .join("\n");

    if compact.chars().count() <= PI_CONTEXT_TEXT_LIMIT {
        return compact;
    }

    let mut truncated = compact
        .chars()
        .take(PI_CONTEXT_TEXT_LIMIT)
        .collect::<String>();
    truncated.push('…');
    truncated
}

#[cfg(any(test, rust_analyzer))]
mod tests {
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::{
        encode_pi_session_path, latest_pi_assistant_text, latest_pi_transcript_for_cwd,
        pi_session_state,
    };

    #[test]
    fn encodes_pi_session_directory_names() {
        assert_eq!(
            encode_pi_session_path(std::path::Path::new("/home/colin/Projects/botctl")),
            "--home-colin-Projects-botctl--"
        );
    }

    #[test]
    fn reads_latest_pi_assistant_text_parts() {
        let path = unique_temp_path("pi-last-message");
        fs::write(
            &path,
            concat!(
                r#"{"type":"session","version":3,"id":"session-one","cwd":"/tmp/project"}"#,
                "\n",
                r#"{"type":"message","message":{"role":"assistant","content":[{"type":"thinking","thinking":"hidden"},{"type":"text","text":"first"}],"stopReason":"stop"}}"#,
                "\n",
                r#"{"type":"message","message":{"role":"assistant","content":[{"type":"text","text":"second"},{"type":"toolCall","name":"bash"}]}}"#,
                "\n",
            ),
        )
        .expect("transcript should write");

        let text = latest_pi_assistant_text(&path)
            .expect("reader should succeed")
            .expect("message should exist");

        assert_eq!(text, "second");
    }

    #[test]
    fn resolves_latest_pi_session_for_cwd() {
        let root = unique_temp_dir("pi-session-root");
        let session_dir = root.join(encode_pi_session_path(std::path::Path::new("/tmp/project")));
        fs::create_dir_all(&session_dir).expect("session dir should create");
        let transcript = session_dir.join("session.jsonl");
        fs::write(
            &transcript,
            r#"{"type":"session","version":3,"id":"session-one","cwd":"/tmp/project"}"#,
        )
        .expect("transcript should write");

        let (session_id, path, cwd) = latest_pi_transcript_for_cwd(&root, "/tmp/project")
            .expect("resolver should succeed")
            .expect("session should resolve");

        assert_eq!(session_id, "session-one");
        assert_eq!(path, transcript);
        assert_eq!(cwd, "/tmp/project");
    }

    #[test]
    fn maps_pi_state_from_latest_message() {
        let path = unique_temp_path("pi-state");
        fs::write(
            &path,
            concat!(
                r#"{"type":"session","version":3,"id":"session-one","cwd":"/tmp/project"}"#,
                "\n",
                r#"{"type":"message","message":{"role":"user","content":"hello"}}"#,
                "\n",
            ),
        )
        .expect("transcript should write");
        assert_eq!(
            pi_session_state(&path).expect("state should read"),
            Some(crate::classifier::SessionState::BusyResponding)
        );

        fs::write(
            &path,
            concat!(
                r#"{"type":"session","version":3,"id":"session-one","cwd":"/tmp/project"}"#,
                "\n",
                r#"{"type":"message","message":{"role":"assistant","content":[{"type":"text","text":"done"}],"stopReason":"stop"}}"#,
                "\n",
            ),
        )
        .expect("transcript should write");
        assert_eq!(
            pi_session_state(&path).expect("state should read"),
            Some(crate::classifier::SessionState::ChatReady)
        );
    }

    fn unique_temp_dir(prefix: &str) -> std::path::PathBuf {
        let path = unique_temp_path(prefix);
        fs::create_dir_all(&path).expect("temp dir should create");
        path
    }

    fn unique_temp_path(prefix: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "botctl-{prefix}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("time should be monotonic")
                .as_nanos()
        ))
    }
}
