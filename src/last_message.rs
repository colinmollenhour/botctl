use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde_json::Value;

use crate::agy;
use crate::app::{AppError, AppResult};
use crate::grok;
use crate::opencode;
use crate::pi;
use crate::proc_fd::{
    ChildResolver, LiveProc, transcript_from_process_fds, transcript_from_process_tree_fds,
};
use crate::tmux::{TmuxClient, TmuxPane};

const CLAUDE_PROJECTS_DIR: &str = ".claude/projects";
const CLAUDE_SESSIONS_DIR: &str = ".claude/sessions";
const CLAUDE_CURSOR_WITNESS_BYTES: usize = 256;
const CODEX_SESSIONS_DIR: &str = ".codex/sessions";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LastAgentMessage {
    pub provider: &'static str,
    pub session_id: String,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ClaudeFileIdentity {
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ClaudeTranscriptRecordIdentity {
    pub(crate) session_id: String,
    pub(crate) transcript_path: PathBuf,
    file_identity: ClaudeFileIdentity,
    pub(crate) complete_record_end_offset: u64,
    pub(crate) outer_uuid: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ClaudeTerminalRecord {
    pub(crate) identity: ClaudeTranscriptRecordIdentity,
    pub(crate) text: String,
}

#[derive(Debug, Clone)]
pub(crate) struct ClaudeTranscriptCursor {
    session_id: String,
    transcript_path: PathBuf,
    file_identity: ClaudeFileIdentity,
    complete_record_end_offset: u64,
    read_offset: u64,
    read_witness_start: u64,
    read_witness: Vec<u8>,
    pending_start_offset: u64,
    pending: Vec<u8>,
    fresh_record_start_floor: u64,
    seen_main_chain_uuids: HashSet<String>,
}

impl ClaudeTranscriptCursor {
    pub(crate) fn at_complete_record_boundary(&self) -> bool {
        self.pending.is_empty()
    }
}

pub(crate) fn capture_claude_transcript_cursor(
    pane: &TmuxPane,
) -> AppResult<ClaudeTranscriptCursor> {
    if !pane.current_command.eq_ignore_ascii_case("claude") {
        return Err(AppError::new(format!(
            "cannot bind Claude transcript cursor for pane {} running {}",
            pane.pane_id, pane.current_command
        )));
    }
    let (session_id, transcript_path) = resolved_claude_transcript_for_cursor(pane)?;
    capture_claude_transcript_cursor_from_resolved(session_id, transcript_path)
}

pub(crate) fn read_fresh_claude_terminal_record(
    pane: &TmuxPane,
    cursor: &mut ClaudeTranscriptCursor,
) -> AppResult<Option<ClaudeTerminalRecord>> {
    if !pane.current_command.eq_ignore_ascii_case("claude") {
        return Err(AppError::new(format!(
            "Claude transcript cursor provider changed for pane {}: now running {}",
            pane.pane_id, pane.current_command
        )));
    }
    let (session_id, transcript_path) = resolved_claude_transcript_for_cursor(pane)?;
    read_fresh_claude_terminal_record_from_resolved(&session_id, &transcript_path, cursor)
}

fn resolved_claude_transcript_for_cursor(pane: &TmuxPane) -> AppResult<(String, PathBuf)> {
    match resolve_claude_transcript_for_pane(pane)? {
        ClaudeTranscriptResolve::Found { session_id, path } => Ok((session_id, path)),
        ClaudeTranscriptResolve::None => Err(AppError::new(format!(
            "no main-chain Claude transcript can be bound to pane {} session {}",
            pane.pane_id, pane.session_name
        ))),
        ClaudeTranscriptResolve::Ambiguous { candidates } => Err(AppError::new(format!(
            "ambiguous Claude transcript continuity for pane {}: candidates {}",
            pane.pane_id,
            format_candidate_preview(&candidates, 10)
        ))),
    }
}

fn capture_claude_transcript_cursor_from_resolved(
    session_id: String,
    transcript_path: PathBuf,
) -> AppResult<ClaudeTranscriptCursor> {
    let transcript_path = fs::canonicalize(&transcript_path).map_err(|error| {
        AppError::new(format!(
            "failed to canonicalize Claude transcript {}: {error}",
            transcript_path.display()
        ))
    })?;
    let mut file = File::open(&transcript_path)?;
    let metadata = file.metadata()?;
    let file_identity = claude_file_identity(&metadata)?;
    let mut content = Vec::new();
    file.read_to_end(&mut content)?;
    let capture_offset = content.len() as u64;

    let witness_start = content.len().saturating_sub(CLAUDE_CURSOR_WITNESS_BYTES);
    let read_witness = content[witness_start..].to_vec();
    let mut cursor = ClaudeTranscriptCursor {
        session_id,
        transcript_path,
        file_identity,
        complete_record_end_offset: 0,
        read_offset: capture_offset,
        read_witness_start: witness_start as u64,
        read_witness,
        pending_start_offset: 0,
        pending: content,
        fresh_record_start_floor: capture_offset,
        seen_main_chain_uuids: HashSet::new(),
    };
    consume_complete_claude_records(&mut cursor, false)?;
    Ok(cursor)
}

fn read_fresh_claude_terminal_record_from_resolved(
    session_id: &str,
    transcript_path: &Path,
    cursor: &mut ClaudeTranscriptCursor,
) -> AppResult<Option<ClaudeTerminalRecord>> {
    let transcript_path = fs::canonicalize(transcript_path).map_err(|error| {
        AppError::new(format!(
            "failed to revalidate Claude transcript {}: {error}",
            transcript_path.display()
        ))
    })?;
    if session_id != cursor.session_id || transcript_path != cursor.transcript_path {
        return Err(AppError::new(format!(
            "Claude transcript continuity changed: bound session {} path {}, now session {} path {}",
            cursor.session_id,
            cursor.transcript_path.display(),
            session_id,
            transcript_path.display()
        )));
    }

    let mut file = File::open(&transcript_path)?;
    let metadata = file.metadata()?;
    let file_identity = claude_file_identity(&metadata)?;
    if file_identity != cursor.file_identity {
        return Err(AppError::new(format!(
            "Claude transcript {} was replaced while waiting for completion",
            transcript_path.display()
        )));
    }
    if metadata.len() < cursor.read_offset
        || metadata.len() < cursor.complete_record_end_offset
        || cursor.pending_start_offset > cursor.read_offset
    {
        return Err(AppError::new(format!(
            "Claude transcript {} offset regressed while waiting for completion",
            transcript_path.display()
        )));
    }

    verify_claude_cursor_witness(&mut file, cursor)?;
    file.seek(SeekFrom::Start(cursor.read_offset))?;
    let appended_start = cursor.pending.len();
    let bytes_read = file.read_to_end(&mut cursor.pending)?;
    cursor.read_offset += bytes_read as u64;
    cursor
        .read_witness
        .extend_from_slice(&cursor.pending[appended_start..]);
    if cursor.read_witness.len() > CLAUDE_CURSOR_WITNESS_BYTES {
        let excess = cursor.read_witness.len() - CLAUDE_CURSOR_WITNESS_BYTES;
        cursor.read_witness.drain(..excess);
    }
    cursor.read_witness_start = cursor.read_offset - cursor.read_witness.len() as u64;
    consume_complete_claude_records(cursor, true)
}

fn verify_claude_cursor_witness(file: &mut File, cursor: &ClaudeTranscriptCursor) -> AppResult<()> {
    if cursor.read_witness.is_empty() {
        return Ok(());
    }
    file.seek(SeekFrom::Start(cursor.read_witness_start))?;
    let mut observed = vec![0; cursor.read_witness.len()];
    file.read_exact(&mut observed)?;
    if observed != cursor.read_witness {
        return Err(AppError::new(format!(
            "Claude transcript {} content before offset {} changed while waiting for completion",
            cursor.transcript_path.display(),
            cursor.read_offset
        )));
    }
    Ok(())
}

fn consume_complete_claude_records(
    cursor: &mut ClaudeTranscriptCursor,
    collect_terminal: bool,
) -> AppResult<Option<ClaudeTerminalRecord>> {
    let mut consumed = 0usize;
    let mut terminal = None;

    while let Some(relative_newline) = cursor.pending[consumed..]
        .iter()
        .position(|byte| *byte == b'\n')
    {
        let end = consumed + relative_newline;
        let record_end_offset = cursor.pending_start_offset + end as u64 + 1;
        let record_start_offset = cursor.pending_start_offset + consumed as u64;
        if cursor.pending[consumed..end]
            .iter()
            .any(|byte| !byte.is_ascii_whitespace())
        {
            let value = serde_json::from_slice::<Value>(&cursor.pending[consumed..end])
                .map_err(|error| {
                    AppError::new(format!(
                        "malformed complete Claude transcript record ending at offset {record_end_offset}: {error}"
                    ))
                })?;
            if let Some(candidate) =
                inspect_claude_main_chain_record(cursor, value, record_end_offset)?
                && collect_terminal
                && record_start_offset >= cursor.fresh_record_start_floor
            {
                terminal = Some(candidate);
            }
        }
        consumed = end + 1;
        cursor.complete_record_end_offset = record_end_offset;
    }

    if consumed > 0 {
        cursor.pending.drain(..consumed);
        cursor.pending_start_offset += consumed as u64;
    }
    Ok(terminal)
}

fn inspect_claude_main_chain_record(
    cursor: &mut ClaudeTranscriptCursor,
    value: Value,
    record_end_offset: u64,
) -> AppResult<Option<ClaudeTerminalRecord>> {
    let Some(record_type) = value.get("type").and_then(Value::as_str) else {
        return Err(AppError::new(format!(
            "Claude transcript record ending at offset {record_end_offset} has malformed type metadata"
        )));
    };
    let has_chain_metadata = value.get("parentUuid").is_some()
        || value.get("isSidechain").is_some()
        || value.get("uuid").is_some();
    if !has_chain_metadata && !matches!(record_type, "user" | "assistant") {
        return Ok(None);
    }

    let record_session = required_nonempty_string(&value, "sessionId", record_end_offset)?;
    if record_session != cursor.session_id {
        return Err(AppError::new(format!(
            "Claude transcript record ending at offset {record_end_offset} changed session from {} to {record_session}",
            cursor.session_id
        )));
    }
    let is_sidechain = value
        .get("isSidechain")
        .and_then(Value::as_bool)
        .ok_or_else(|| {
            AppError::new(format!(
                "Claude transcript record ending at offset {record_end_offset} has malformed sidechain metadata"
            ))
        })?;
    if is_sidechain {
        return Ok(None);
    }

    let outer_uuid = required_nonempty_string(&value, "uuid", record_end_offset)?.to_string();
    if cursor.seen_main_chain_uuids.contains(&outer_uuid) {
        return Err(AppError::new(format!(
            "Claude transcript record ending at offset {record_end_offset} repeats outer uuid {outer_uuid}"
        )));
    }
    let parent_uuid = match value.get("parentUuid") {
        Some(Value::Null) => None,
        Some(Value::String(parent)) if !parent.is_empty() => Some(parent.as_str()),
        _ => {
            return Err(AppError::new(format!(
                "Claude transcript record ending at offset {record_end_offset} has malformed parent continuity metadata"
            )));
        }
    };
    let parent_is_valid = match parent_uuid {
        Some(parent) => cursor.seen_main_chain_uuids.contains(parent),
        None => cursor.seen_main_chain_uuids.is_empty(),
    };
    if !parent_is_valid {
        return Err(AppError::new(format!(
            "Claude transcript main-chain continuity is ambiguous at offset {record_end_offset}"
        )));
    }
    cursor.seen_main_chain_uuids.insert(outer_uuid.clone());

    if record_type != "assistant" {
        return Ok(None);
    }
    if value.pointer("/message/role").and_then(Value::as_str) != Some("assistant") {
        return Err(AppError::new(format!(
            "Claude assistant record ending at offset {record_end_offset} has malformed role metadata"
        )));
    }
    if value
        .pointer("/message/stop_reason")
        .and_then(Value::as_str)
        != Some("end_turn")
    {
        return Ok(None);
    }
    let Some(text) = text_from_claude_content(value.pointer("/message/content"))
        .filter(|text| !text.trim().is_empty())
    else {
        return Ok(None);
    };

    Ok(Some(ClaudeTerminalRecord {
        identity: ClaudeTranscriptRecordIdentity {
            session_id: cursor.session_id.clone(),
            transcript_path: cursor.transcript_path.clone(),
            file_identity: cursor.file_identity.clone(),
            complete_record_end_offset: record_end_offset,
            outer_uuid,
        },
        text,
    }))
}

fn required_nonempty_string<'a>(
    value: &'a Value,
    field: &str,
    record_end_offset: u64,
) -> AppResult<&'a str> {
    value
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            AppError::new(format!(
                "Claude transcript record ending at offset {record_end_offset} has malformed {field} metadata"
            ))
        })
}

#[cfg(unix)]
fn claude_file_identity(metadata: &fs::Metadata) -> AppResult<ClaudeFileIdentity> {
    use std::os::unix::fs::MetadataExt;
    Ok(ClaudeFileIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

#[cfg(not(unix))]
fn claude_file_identity(_metadata: &fs::Metadata) -> AppResult<ClaudeFileIdentity> {
    Err(AppError::new(
        "durable Claude transcript file identity is unavailable on this platform",
    ))
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

    if pane.current_command.eq_ignore_ascii_case("grok") {
        return load_grok_last_message(pane);
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
        "last-message supports Claude, Codex, OpenCode, Pi, Grok, and Antigravity panes; pane {} is running {}",
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
    let (session_id, transcript_path) = match resolve_claude_transcript_for_pane(pane)? {
        ClaudeTranscriptResolve::Found { session_id, path } => (session_id, path),
        ClaudeTranscriptResolve::None => {
            return Err(AppError::new(format!(
                "no Claude transcript found for pane {} (cwd {}); expected an open project \
                 transcript FD, ~/.claude/sessions/<pid>.json for the pane process tree, \
                 --session-id on the Claude command line, or a single project transcript",
                pane.pane_id, pane.current_path
            )));
        }
        ClaudeTranscriptResolve::Ambiguous { candidates } => {
            return Err(AppError::new(format!(
                "ambiguous Claude transcript for pane {} cwd {}: {} candidate sessions and no \
                 unique binding via ~/.claude/sessions/<pid>.json, open transcript FD, or \
                 --session-id; candidates: {}",
                pane.pane_id,
                pane.current_path,
                candidates.len(),
                format_candidate_preview(&candidates, 10)
            )));
        }
    };
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

/// Build the error returned by [`load_agy_last_message`] when an agy pane is
/// confirmed but no `.pb` conversation file could be resolved. Extracted as a
/// shared helper so tests can assert against the production wording without
/// needing a real `TmuxClient` to drive `load_agy_last_message` end-to-end.
fn agy_no_conversation_error(pane_pid: Option<u32>) -> AppError {
    let state_dir = agy::default_state_dir();
    if let Some(pid) = pane_pid {
        AppError::new(format!(
            "agy: pane process {pid} has no open conversation file under \
             {state_dir}/conversations/; either the agy session has no active \
             conversation yet, ANTIGRAVITY_STATE_DIR is misconfigured, or \
             /proc/{pid}/fd is unreadable for this user.",
            state_dir = state_dir.display(),
        ))
    } else {
        // Reached only inside the `session.id.ok_or_else` closure, so the agy
        // session is already confirmed. The conversation_id lookup just
        // couldn't run without a pane_pid for the FD walk.
        AppError::new(
            "agy: pane confirmed as Antigravity but pane_pid is unavailable, so the open conversation file could not be verified; this usually means tmux did not report a pane process id",
        )
    }
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
    let conversation_id = session
        .id
        .ok_or_else(|| agy_no_conversation_error(pane.pane_pid))?;
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

fn load_grok_last_message(pane: &TmuxPane) -> AppResult<LastAgentMessage> {
    use crate::proc_fd::LiveProc;
    let message = grok::latest_assistant_message_for_pane(pane, &LiveProc)?.ok_or_else(|| {
        AppError::new(
            "no Grok session/message resolved for pane; expected an entry in \
             ~/.grok/active_sessions.json (or GROK_HOME) matching the pane process tree, \
             an open events.jsonl under ~/.grok/sessions, or a cwd session directory with \
             non-empty agent_message_chunk text in updates.jsonl",
        )
    })?;
    Ok(LastAgentMessage {
        provider: "Grok",
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

/// Resolve the live Claude session id for a pane without loading message text.
///
/// Prefers process-tree transcript FDs, then Claude's `~/.claude/sessions/<pid>.json`
/// registry, then `--session-id` on the process command line. Falls back to a
/// project-dir transcript only when it is unique for the pane cwd. Returns
/// `None` when no binding can be made safely (including multi-session ambiguity).
pub fn resolve_claude_session_id_for_pane(pane: &TmuxPane) -> AppResult<Option<String>> {
    match resolve_claude_transcript_for_pane(pane)? {
        ClaudeTranscriptResolve::Found { session_id, .. } => Ok(Some(session_id)),
        ClaudeTranscriptResolve::None | ClaudeTranscriptResolve::Ambiguous { .. } => Ok(None),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ClaudeTranscriptResolve {
    Found { session_id: String, path: PathBuf },
    None,
    Ambiguous { candidates: Vec<String> },
}

fn resolve_claude_transcript_for_pane(pane: &TmuxPane) -> AppResult<ClaudeTranscriptResolve> {
    let Some(home) = home_dir() else {
        return Ok(ClaudeTranscriptResolve::None);
    };
    let projects_root = home.join(CLAUDE_PROJECTS_DIR);
    let sessions_root = home.join(CLAUDE_SESSIONS_DIR);
    resolve_claude_transcript_with_roots(pane, &projects_root, &sessions_root, &LiveProc)
}

#[cfg(any(test, rust_analyzer))]
fn resolve_claude_transcript_in_projects_root(
    pane: &TmuxPane,
    projects_root: &Path,
) -> AppResult<ClaudeTranscriptResolve> {
    // Tests that only inject a projects root still get unique-transcript fallback;
    // production also supplies ~/.claude/sessions via resolve_claude_transcript_for_pane.
    let sessions_root = home_dir()
        .map(|home| home.join(CLAUDE_SESSIONS_DIR))
        .unwrap_or_else(|| PathBuf::from("/dev/null/botctl-no-sessions"));
    resolve_claude_transcript_with_roots(pane, projects_root, &sessions_root, &LiveProc)
}

fn resolve_claude_transcript_with_roots(
    pane: &TmuxPane,
    projects_root: &Path,
    sessions_root: &Path,
    resolver: &dyn ChildResolver,
) -> AppResult<ClaudeTranscriptResolve> {
    let project_dirs = candidate_claude_project_dirs(projects_root, &pane.current_path);

    // 1) Prefer an open transcript FD anywhere in the pane process tree.
    if let Some(pid) = pane.pane_pid {
        for project_dir in &project_dirs {
            if let Some(transcript) = transcript_from_process_tree_fds(pid, project_dir, "jsonl")?
                && let Some(session_id) =
                    claude_session_id_from_transcript(&transcript, project_dir)
            {
                return Ok(ClaudeTranscriptResolve::Found {
                    session_id,
                    path: transcript,
                });
            }
        }
    }

    // 2) Claude 2.x writes ~/.claude/sessions/<pid>.json with the live sessionId.
    // Once bound, never fall through to another transcript for the same cwd —
    // the file may not exist yet during session startup (prompt wait loops).
    if let Some(session_id) =
        claude_session_id_from_sessions_registry(pane.pane_pid, sessions_root, resolver)?
    {
        return Ok(
            match transcript_path_for_session_id(
                &project_dirs,
                projects_root,
                &pane.current_path,
                &session_id,
            ) {
                Some(path) => ClaudeTranscriptResolve::Found { session_id, path },
                None => ClaudeTranscriptResolve::None,
            },
        );
    }

    // 3) Explicit --session-id on the Claude command line (or a descendant).
    if let Some(session_id) = claude_session_id_from_process_tree_cmdline(pane.pane_pid, resolver) {
        return Ok(
            match transcript_path_for_session_id(
                &project_dirs,
                projects_root,
                &pane.current_path,
                &session_id,
            ) {
                Some(path) => ClaudeTranscriptResolve::Found { session_id, path },
                None => ClaudeTranscriptResolve::None,
            },
        );
    }

    // 4) Safe cwd fallback: only when exactly one top-level project transcript exists.
    let mut unique: Option<(String, PathBuf)> = None;
    let mut ambiguous_ids = Vec::new();
    for project_dir in &project_dirs {
        for (session_id, path) in list_claude_transcripts(project_dir)? {
            if ambiguous_ids.iter().any(|id| id == &session_id) {
                continue;
            }
            ambiguous_ids.push(session_id.clone());
            match &unique {
                None => unique = Some((session_id, path)),
                Some((existing_id, _)) if existing_id == &session_id => {}
                Some(_) => {
                    ambiguous_ids.sort();
                    ambiguous_ids.dedup();
                    return Ok(ClaudeTranscriptResolve::Ambiguous {
                        candidates: ambiguous_ids,
                    });
                }
            }
        }
        // Prefer the most specific (first) project dir that has transcripts.
        if unique.is_some() {
            break;
        }
    }

    Ok(match unique {
        Some((session_id, path)) if ambiguous_ids.len() <= 1 => {
            ClaudeTranscriptResolve::Found { session_id, path }
        }
        Some(_) => {
            ambiguous_ids.sort();
            ambiguous_ids.dedup();
            ClaudeTranscriptResolve::Ambiguous {
                candidates: ambiguous_ids,
            }
        }
        None => ClaudeTranscriptResolve::None,
    })
}

fn claude_session_id_from_sessions_registry(
    pane_pid: Option<u32>,
    sessions_root: &Path,
    resolver: &dyn ChildResolver,
) -> AppResult<Option<String>> {
    let Some(root_pid) = pane_pid else {
        return Ok(None);
    };
    for pid in collect_process_tree_pids(root_pid, resolver) {
        if let Some(session_id) =
            read_claude_sessions_file(&sessions_root.join(format!("{pid}.json")))?
        {
            return Ok(Some(session_id));
        }
    }
    Ok(None)
}

fn read_claude_sessions_file(path: &Path) -> AppResult<Option<String>> {
    let content = match fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let value: Value = match serde_json::from_str(&content) {
        Ok(value) => value,
        Err(_) => return Ok(None),
    };
    Ok(value
        .get("sessionId")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .map(str::to_string))
}

fn claude_session_id_from_process_tree_cmdline(
    pane_pid: Option<u32>,
    resolver: &dyn ChildResolver,
) -> Option<String> {
    let root_pid = pane_pid?;
    for pid in collect_process_tree_pids(root_pid, resolver) {
        if let Some(session_id) = session_id_from_process_cmdline(pid) {
            return Some(session_id);
        }
    }
    None
}

fn session_id_from_process_cmdline(pid: u32) -> Option<String> {
    let cmdline = fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    let args: Vec<&str> = cmdline
        .split(|&byte| byte == 0)
        .filter_map(|part| std::str::from_utf8(part).ok())
        .filter(|part| !part.is_empty())
        .collect();
    let mut index = 0;
    while index < args.len() {
        let arg = args[index];
        if let Some(value) = arg.strip_prefix("--session-id=") {
            if !value.is_empty() {
                return Some(value.to_string());
            }
        } else if (arg == "--session-id" || arg == "-s")
            && let Some(value) = args.get(index + 1)
            && !value.is_empty()
            && !value.starts_with('-')
        {
            return Some((*value).to_string());
        }
        index += 1;
    }
    None
}

fn collect_process_tree_pids(root: u32, resolver: &dyn ChildResolver) -> Vec<u32> {
    let mut stack = vec![root];
    let mut seen = HashSet::new();
    let mut ordered = Vec::new();
    while let Some(pid) = stack.pop() {
        if !seen.insert(pid) {
            continue;
        }
        ordered.push(pid);
        stack.extend(resolver.children_of(pid));
    }
    ordered
}

fn transcript_path_for_session_id(
    project_dirs: &[PathBuf],
    projects_root: &Path,
    current_path: &str,
    session_id: &str,
) -> Option<PathBuf> {
    let file_name = format!("{session_id}.jsonl");
    for project_dir in project_dirs {
        let path = project_dir.join(&file_name);
        if path.is_file() {
            return Some(path);
        }
    }
    // Session registry may point at a cwd whose project dir was not yet scanned
    // (e.g. path normalization). Try encoding the pane cwd directly.
    let direct = projects_root
        .join(encode_claude_project_path(Path::new(current_path)))
        .join(&file_name);
    if direct.is_file() {
        return Some(direct);
    }
    None
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

fn list_claude_transcripts(project_dir: &Path) -> AppResult<Vec<(String, PathBuf)>> {
    let mut transcripts = Vec::new();
    let entries = match fs::read_dir(project_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(transcripts),
        Err(error) => return Err(error.into()),
    };
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        let Some(session_id) = claude_session_id_from_transcript(&path, project_dir) else {
            continue;
        };
        transcripts.push((session_id, path));
    }
    Ok(transcripts)
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

/// Format an ambiguity candidate list for errors without dumping hundreds of ids.
fn format_candidate_preview(candidates: &[String], limit: usize) -> String {
    if candidates.is_empty() {
        return String::from("(none)");
    }
    if candidates.len() <= limit {
        return candidates.join(", ");
    }
    let preview = candidates[..limit].join(", ");
    format!(
        "{preview}, … (+{} more)",
        candidates.len().saturating_sub(limit)
    )
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
    use std::io::Write;
    use std::sync::{Mutex, OnceLock};
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::{
        ClaudeTranscriptResolve, capture_claude_transcript_cursor_from_resolved,
        default_output_path, encode_claude_project_path, format_candidate_preview,
        latest_claude_assistant_text, latest_codex_assistant_text, line_count,
        read_fresh_claude_terminal_record_from_resolved,
        resolve_claude_transcript_in_projects_root, resolve_claude_transcript_with_roots,
    };
    use crate::proc_fd::ChildResolver;
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
    fn format_candidate_preview_limits_long_lists() {
        let many: Vec<String> = (0..15).map(|i| format!("id-{i}")).collect();
        let preview = format_candidate_preview(&many, 10);
        assert!(preview.starts_with("id-0, id-1"));
        assert!(preview.contains("(+5 more)"));
        assert!(!preview.contains("id-14"));
        assert_eq!(format_candidate_preview(&many[..3], 10), "id-0, id-1, id-2");
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

    fn claude_conversation_record(
        record_type: &str,
        uuid: &str,
        parent_uuid: Option<&str>,
        message_id: &str,
        stop_reason: Option<&str>,
        text: &str,
        is_sidechain: bool,
    ) -> String {
        serde_json::json!({
            "type": record_type,
            "uuid": uuid,
            "parentUuid": parent_uuid,
            "sessionId": "session-main",
            "isSidechain": is_sidechain,
            "message": {
                "id": message_id,
                "role": record_type,
                "stop_reason": stop_reason,
                "content": [{"type": "text", "text": text}],
            },
        })
        .to_string()
    }

    fn append_jsonl(path: &std::path::Path, records: &[String]) {
        let mut file = fs::OpenOptions::new()
            .append(true)
            .open(path)
            .expect("transcript should open for append");
        for record in records {
            writeln!(file, "{record}").expect("record should append");
        }
    }

    fn cursor_fixture(name: &str) -> (std::path::PathBuf, super::ClaudeTranscriptCursor) {
        let path = unique_temp_path(name);
        fs::write(
            &path,
            format!(
                "{}\n",
                claude_conversation_record(
                    "assistant",
                    "old-assistant",
                    None,
                    "repeated-message-id",
                    Some("end_turn"),
                    "same reply",
                    false,
                )
            ),
        )
        .expect("initial transcript should write");
        let cursor = capture_claude_transcript_cursor_from_resolved(
            String::from("session-main"),
            path.clone(),
        )
        .expect("cursor should bind");
        (path, cursor)
    }

    #[test]
    fn claude_cursor_uses_outer_uuid_and_offset_not_text_or_inner_message_id() {
        let (path, mut cursor) = cursor_fixture("claude-cursor-fresh-identity");
        append_jsonl(
            &path,
            &[
                serde_json::json!({
                    "type": "system",
                    "subtype": "turn_duration",
                    "uuid": "turn-duration",
                    "parentUuid": "old-assistant",
                    "sessionId": "session-main",
                    "isSidechain": false,
                })
                .to_string(),
                claude_conversation_record(
                    "user",
                    "new-user",
                    Some("turn-duration"),
                    "user-message",
                    None,
                    "prompt",
                    false,
                ),
                claude_conversation_record(
                    "assistant",
                    "new-assistant",
                    Some("new-user"),
                    "repeated-message-id",
                    Some("end_turn"),
                    "same reply",
                    false,
                ),
            ],
        );

        let record =
            read_fresh_claude_terminal_record_from_resolved("session-main", &path, &mut cursor)
                .expect("incremental read should succeed")
                .expect("fresh terminal record should complete");

        assert_eq!(record.text, "same reply");
        assert_eq!(record.identity.outer_uuid, "new-assistant");
        assert_eq!(record.identity.session_id, "session-main");
        assert_eq!(
            record.identity.transcript_path,
            fs::canonicalize(&path).expect("path should canonicalize")
        );
        assert!(record.identity.complete_record_end_offset > 0);
        assert!(
            read_fresh_claude_terminal_record_from_resolved("session-main", &path, &mut cursor)
                .expect("second poll should succeed")
                .is_none(),
            "an already-consumed durable record must not be returned twice"
        );
    }

    #[test]
    fn claude_cursor_binds_before_any_assistant_text_exists() {
        let path = unique_temp_path("claude-cursor-no-prior-text");
        fs::write(
            &path,
            "{\"type\":\"permission-mode\",\"sessionId\":\"session-main\"}\n",
        )
        .expect("metadata-only transcript should write");
        let mut cursor = capture_claude_transcript_cursor_from_resolved(
            String::from("session-main"),
            path.clone(),
        )
        .expect("metadata-only cursor should bind");
        append_jsonl(
            &path,
            &[
                claude_conversation_record(
                    "user",
                    "first-user",
                    None,
                    "user-message",
                    None,
                    "first prompt",
                    false,
                ),
                claude_conversation_record(
                    "assistant",
                    "first-assistant",
                    Some("first-user"),
                    "assistant-message",
                    Some("end_turn"),
                    "first reply",
                    false,
                ),
            ],
        );

        let record =
            read_fresh_claude_terminal_record_from_resolved("session-main", &path, &mut cursor)
                .expect("first incremental response should parse")
                .expect("first response should complete");
        assert_eq!(record.text, "first reply");
    }

    #[test]
    fn claude_cursor_waits_for_partial_trailing_record() {
        let (path, mut cursor) = cursor_fixture("claude-cursor-partial");
        append_jsonl(
            &path,
            &[claude_conversation_record(
                "user",
                "partial-user",
                Some("old-assistant"),
                "user-message",
                None,
                "prompt",
                false,
            )],
        );
        let terminal = claude_conversation_record(
            "assistant",
            "partial-assistant",
            Some("partial-user"),
            "assistant-message",
            Some("end_turn"),
            "complete later",
            false,
        );
        let split = terminal.len() - 2;
        fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("transcript should open")
            .write_all(&terminal.as_bytes()[..split])
            .expect("partial record should append");

        assert!(
            read_fresh_claude_terminal_record_from_resolved("session-main", &path, &mut cursor)
                .expect("partial record should be nonterminal")
                .is_none()
        );

        let mut file = fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("transcript should reopen");
        file.write_all(&terminal.as_bytes()[split..])
            .expect("record tail should append");
        file.write_all(b"\n").expect("newline should append");
        let record =
            read_fresh_claude_terminal_record_from_resolved("session-main", &path, &mut cursor)
                .expect("completed record should parse")
                .expect("completed end_turn should be terminal");
        assert_eq!(record.text, "complete later");
    }

    #[test]
    fn claude_cursor_excludes_record_that_started_before_capture() {
        let path = unique_temp_path("claude-cursor-initial-partial");
        let old_user = claude_conversation_record(
            "user",
            "old-user",
            None,
            "old-user-message",
            None,
            "old prompt",
            false,
        );
        let stale_terminal = claude_conversation_record(
            "assistant",
            "old-assistant",
            Some("old-user"),
            "old-assistant-message",
            Some("end_turn"),
            "stale response",
            false,
        );
        fs::write(&path, format!("{old_user}\n{stale_terminal}"))
            .expect("initial partial transcript should write");
        let mut cursor = capture_claude_transcript_cursor_from_resolved(
            String::from("session-main"),
            path.clone(),
        )
        .expect("cursor should bind");

        fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("transcript should reopen")
            .write_all(b"\n")
            .expect("record delimiter should append");
        assert!(
            read_fresh_claude_terminal_record_from_resolved("session-main", &path, &mut cursor)
                .expect("pre-cursor partial record should parse")
                .is_none(),
            "a record that began before capture is not a fresh response"
        );
        assert!(cursor.at_complete_record_boundary());
    }

    #[test]
    fn claude_cursor_requires_newline_after_valid_json_record() {
        let (path, mut cursor) = cursor_fixture("claude-cursor-newline");
        append_jsonl(
            &path,
            &[claude_conversation_record(
                "user",
                "newline-user",
                Some("old-assistant"),
                "user-message",
                None,
                "prompt",
                false,
            )],
        );
        let terminal = claude_conversation_record(
            "assistant",
            "newline-assistant",
            Some("newline-user"),
            "assistant-message",
            Some("end_turn"),
            "durable after newline",
            false,
        );
        fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("transcript should open")
            .write_all(terminal.as_bytes())
            .expect("valid unterminated record should append");

        assert!(
            read_fresh_claude_terminal_record_from_resolved("session-main", &path, &mut cursor)
                .expect("unterminated JSON should remain pending")
                .is_none()
        );
        assert!(!cursor.at_complete_record_boundary());
        fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("transcript should reopen")
            .write_all(b"\n")
            .expect("record delimiter should append");
        let record =
            read_fresh_claude_terminal_record_from_resolved("session-main", &path, &mut cursor)
                .expect("delimited record should parse")
                .expect("delimited end_turn should complete");
        assert!(cursor.at_complete_record_boundary());
        assert_eq!(record.text, "durable after newline");
    }

    #[test]
    fn claude_cursor_returns_newest_terminal_record_in_append_batch() {
        let (path, mut cursor) = cursor_fixture("claude-cursor-newest-terminal");
        append_jsonl(
            &path,
            &[
                claude_conversation_record(
                    "user",
                    "batch-user",
                    Some("old-assistant"),
                    "user-message",
                    None,
                    "prompt",
                    false,
                ),
                claude_conversation_record(
                    "assistant",
                    "batch-status",
                    Some("batch-user"),
                    "status-message",
                    Some("end_turn"),
                    "intermediate status",
                    false,
                ),
                claude_conversation_record(
                    "user",
                    "batch-followup",
                    Some("batch-status"),
                    "user-message-2",
                    None,
                    "continue",
                    false,
                ),
                claude_conversation_record(
                    "assistant",
                    "batch-final",
                    Some("batch-followup"),
                    "final-message",
                    Some("end_turn"),
                    "final delivery",
                    false,
                ),
            ],
        );

        let record =
            read_fresh_claude_terminal_record_from_resolved("session-main", &path, &mut cursor)
                .expect("append batch should parse")
                .expect("latest terminal record should complete");
        assert_eq!(record.identity.outer_uuid, "batch-final");
        assert_eq!(record.text, "final delivery");
    }

    #[test]
    fn claude_cursor_accepts_nonadjacent_branched_tool_result_ancestry() {
        let (path, mut cursor) = cursor_fixture("claude-cursor-branched");
        append_jsonl(
            &path,
            &[
                claude_conversation_record(
                    "user",
                    "branch-user",
                    Some("old-assistant"),
                    "user-message",
                    None,
                    "prompt",
                    false,
                ),
                claude_conversation_record(
                    "assistant",
                    "tool-a1",
                    Some("branch-user"),
                    "tool-message-1",
                    None,
                    "",
                    false,
                ),
                claude_conversation_record(
                    "assistant",
                    "tool-a2",
                    Some("tool-a1"),
                    "tool-message-2",
                    None,
                    "",
                    false,
                ),
                claude_conversation_record(
                    "user",
                    "tool-r2",
                    Some("tool-a2"),
                    "result-message-2",
                    None,
                    "",
                    false,
                ),
                claude_conversation_record(
                    "user",
                    "tool-r1",
                    Some("tool-a1"),
                    "result-message-1",
                    None,
                    "",
                    false,
                ),
                claude_conversation_record(
                    "assistant",
                    "branch-final",
                    Some("tool-r1"),
                    "final-message",
                    Some("end_turn"),
                    "branched delivery",
                    false,
                ),
            ],
        );

        let record =
            read_fresh_claude_terminal_record_from_resolved("session-main", &path, &mut cursor)
                .expect("branched ancestry should parse")
                .expect("final branched record should complete");
        assert_eq!(record.text, "branched delivery");
    }

    #[test]
    fn claude_cursor_rejects_nonterminal_empty_and_sidechain_records() {
        let (path, mut cursor) = cursor_fixture("claude-cursor-nonterminal");
        append_jsonl(
            &path,
            &[
                claude_conversation_record(
                    "assistant",
                    "sidechain",
                    Some("old-assistant"),
                    "side-message",
                    Some("end_turn"),
                    "must not print",
                    true,
                ),
                claude_conversation_record(
                    "user",
                    "tool-user",
                    Some("old-assistant"),
                    "user-message",
                    None,
                    "prompt",
                    false,
                ),
                claude_conversation_record(
                    "assistant",
                    "tool-assistant",
                    Some("tool-user"),
                    "assistant-message",
                    Some("tool_use"),
                    "tool preamble",
                    false,
                ),
                claude_conversation_record(
                    "user",
                    "missing-stop-user",
                    Some("tool-assistant"),
                    "user-message",
                    None,
                    "tool result",
                    false,
                ),
                claude_conversation_record(
                    "assistant",
                    "missing-stop-assistant",
                    Some("missing-stop-user"),
                    "assistant-message",
                    None,
                    "not terminal",
                    false,
                ),
                claude_conversation_record(
                    "user",
                    "unknown-stop-user",
                    Some("missing-stop-assistant"),
                    "user-message",
                    None,
                    "continue",
                    false,
                ),
                claude_conversation_record(
                    "assistant",
                    "unknown-stop-assistant",
                    Some("unknown-stop-user"),
                    "assistant-message",
                    Some("pause_turn"),
                    "not terminal either",
                    false,
                ),
                claude_conversation_record(
                    "user",
                    "empty-user",
                    Some("unknown-stop-assistant"),
                    "user-message",
                    None,
                    "continue",
                    false,
                ),
                claude_conversation_record(
                    "assistant",
                    "empty-assistant",
                    Some("empty-user"),
                    "assistant-message",
                    Some("end_turn"),
                    "  ",
                    false,
                ),
            ],
        );

        assert!(
            read_fresh_claude_terminal_record_from_resolved("session-main", &path, &mut cursor)
                .expect("nonterminal records should parse")
                .is_none()
        );
    }

    #[test]
    fn claude_cursor_fails_closed_on_malformed_or_ambiguous_continuity() {
        let (malformed_path, mut malformed_cursor) = cursor_fixture("claude-cursor-malformed");
        append_jsonl(&malformed_path, &[String::from("{not-json}")]);
        let malformed = read_fresh_claude_terminal_record_from_resolved(
            "session-main",
            &malformed_path,
            &mut malformed_cursor,
        )
        .expect_err("terminated malformed JSON must fail closed");
        assert!(malformed.to_string().contains("malformed complete"));

        let (ambiguous_path, mut ambiguous_cursor) = cursor_fixture("claude-cursor-ambiguous");
        append_jsonl(
            &ambiguous_path,
            &[claude_conversation_record(
                "assistant",
                "unlinked-assistant",
                Some("foreign-parent"),
                "assistant-message",
                Some("end_turn"),
                "must not print",
                false,
            )],
        );
        let ambiguous = read_fresh_claude_terminal_record_from_resolved(
            "session-main",
            &ambiguous_path,
            &mut ambiguous_cursor,
        )
        .expect_err("unlinked main-chain record must fail closed");
        assert!(ambiguous.to_string().contains("continuity is ambiguous"));
    }

    #[test]
    fn claude_cursor_fails_closed_on_duplicate_uuid_session_switch_and_truncation() {
        let (duplicate_path, mut duplicate_cursor) = cursor_fixture("claude-cursor-duplicate");
        append_jsonl(
            &duplicate_path,
            &[claude_conversation_record(
                "user",
                "old-assistant",
                Some("old-assistant"),
                "user-message",
                None,
                "prompt",
                false,
            )],
        );
        let duplicate = read_fresh_claude_terminal_record_from_resolved(
            "session-main",
            &duplicate_path,
            &mut duplicate_cursor,
        )
        .expect_err("duplicate outer uuid must fail closed");
        assert!(duplicate.to_string().contains("repeats outer uuid"));

        let (session_path, mut session_cursor) = cursor_fixture("claude-cursor-session");
        let switched = read_fresh_claude_terminal_record_from_resolved(
            "other-session",
            &session_path,
            &mut session_cursor,
        )
        .expect_err("session switch must fail closed");
        assert!(switched.to_string().contains("continuity changed"));

        let (truncated_path, mut truncated_cursor) = cursor_fixture("claude-cursor-truncated");
        fs::write(&truncated_path, "").expect("same file should truncate");
        let truncated = read_fresh_claude_terminal_record_from_resolved(
            "session-main",
            &truncated_path,
            &mut truncated_cursor,
        )
        .expect_err("offset regression must fail closed");
        assert!(truncated.to_string().contains("offset regressed"));

        let (rewritten_path, mut rewritten_cursor) = cursor_fixture("claude-cursor-rewritten");
        let original_len = fs::metadata(&rewritten_path)
            .expect("transcript metadata should read")
            .len() as usize;
        fs::write(&rewritten_path, vec![b'x'; original_len])
            .expect("same-length in-place rewrite should succeed");
        let rewritten = read_fresh_claude_terminal_record_from_resolved(
            "session-main",
            &rewritten_path,
            &mut rewritten_cursor,
        )
        .expect_err("same-length rewrite before the cursor must fail closed");
        assert!(rewritten.to_string().contains("content before offset"));
    }

    #[cfg(unix)]
    #[test]
    fn claude_cursor_fails_closed_when_transcript_file_is_replaced() {
        let (path, mut cursor) = cursor_fixture("claude-cursor-replaced");
        let old_path = path.with_extension("old");
        fs::rename(&path, &old_path).expect("bound transcript should move");
        fs::write(
            &path,
            format!(
                "{}\n",
                claude_conversation_record(
                    "assistant",
                    "replacement",
                    None,
                    "message",
                    Some("end_turn"),
                    "replacement text",
                    false,
                )
            ),
        )
        .expect("replacement transcript should write");

        let replaced =
            read_fresh_claude_terminal_record_from_resolved("session-main", &path, &mut cursor)
                .expect_err("replacement identity must fail closed");
        assert!(replaced.to_string().contains("was replaced"));
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

        let resolved =
            resolve_claude_transcript_in_projects_root(&sample_pane("/tmp/project/subdir"), &root)
                .expect("resolver should succeed");

        match resolved {
            ClaudeTranscriptResolve::Found { session_id, path } => {
                assert_eq!(session_id, "session-live");
                assert_eq!(path, transcript);
            }
            other => panic!("expected Found, got {other:?}"),
        }
    }

    #[test]
    fn claude_resolver_binds_same_cwd_sessions_via_pid_registry() {
        let root = unique_temp_dir("claude-last-message-multi");
        let projects_root = root.join("projects");
        let sessions_root = root.join("sessions");
        fs::create_dir_all(&sessions_root).expect("sessions root should create");
        let cwd = "/tmp/shared-project";
        let project_dir = projects_root.join(encode_claude_project_path(std::path::Path::new(cwd)));
        fs::create_dir_all(&project_dir).expect("project dir should create");

        const A: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
        const B: &str = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
        for (session_id, text) in [(A, "sentinel-a"), (B, "sentinel-b")] {
            fs::write(
                project_dir.join(format!("{session_id}.jsonl")),
                format!(
                    "{{\"type\":\"permission-mode\",\"sessionId\":\"{session_id}\"}}\n\
                     {{\"type\":\"assistant\",\"message\":{{\"role\":\"assistant\",\"content\":[{{\"type\":\"text\",\"text\":\"{text}\"}}]}}}}\n"
                ),
            )
            .expect("transcript should write");
        }

        // No pane pid / sessions file: multi-session cwd must be ambiguous, not newest-wins.
        let ambiguous = resolve_claude_transcript_with_roots(
            &sample_pane(cwd),
            &projects_root,
            &sessions_root,
            &EmptyResolver,
        )
        .expect("resolver should succeed");
        match ambiguous {
            ClaudeTranscriptResolve::Ambiguous { candidates } => {
                assert_eq!(candidates.len(), 2);
                assert!(candidates.iter().any(|id| id == A));
                assert!(candidates.iter().any(|id| id == B));
            }
            other => panic!("expected Ambiguous without pid binding, got {other:?}"),
        }

        // Sessions registry pins pane pid 101 -> A and 102 -> B.
        fs::write(
            sessions_root.join("101.json"),
            format!(r#"{{"pid":101,"sessionId":"{A}","cwd":"{cwd}"}}"#),
        )
        .expect("session A registry should write");
        fs::write(
            sessions_root.join("102.json"),
            format!(r#"{{"pid":102,"sessionId":"{B}","cwd":"{cwd}"}}"#),
        )
        .expect("session B registry should write");

        let mut pane_a = sample_pane(cwd);
        pane_a.pane_pid = Some(101);
        let mut pane_b = sample_pane(cwd);
        pane_b.pane_id = String::from("%2");
        pane_b.pane_pid = Some(102);

        let resolved_a = resolve_claude_transcript_with_roots(
            &pane_a,
            &projects_root,
            &sessions_root,
            &EmptyResolver,
        )
        .expect("pane A should resolve");
        let resolved_b = resolve_claude_transcript_with_roots(
            &pane_b,
            &projects_root,
            &sessions_root,
            &EmptyResolver,
        )
        .expect("pane B should resolve");

        match resolved_a {
            ClaudeTranscriptResolve::Found { session_id, path } => {
                assert_eq!(session_id, A);
                assert_eq!(path, project_dir.join(format!("{A}.jsonl")));
                let text = latest_claude_assistant_text(&path)
                    .expect("read A")
                    .expect("text A");
                assert_eq!(text, "sentinel-a");
            }
            other => panic!("expected Found for pane A, got {other:?}"),
        }
        match resolved_b {
            ClaudeTranscriptResolve::Found { session_id, path } => {
                assert_eq!(session_id, B);
                assert_eq!(path, project_dir.join(format!("{B}.jsonl")));
                let text = latest_claude_assistant_text(&path)
                    .expect("read B")
                    .expect("text B");
                assert_eq!(text, "sentinel-b");
            }
            other => panic!("expected Found for pane B, got {other:?}"),
        }

        // Registry binding is exclusive: a known session id with no transcript yet
        // must not fall through to another same-cwd session's file.
        const C: &str = "cccccccc-cccc-4ccc-8ccc-cccccccccccc";
        fs::write(
            sessions_root.join("103.json"),
            format!(r#"{{"pid":103,"sessionId":"{C}","cwd":"{cwd}"}}"#),
        )
        .expect("session C registry should write");
        let mut pane_c = sample_pane(cwd);
        pane_c.pane_id = String::from("%3");
        pane_c.pane_pid = Some(103);
        let resolved_c = resolve_claude_transcript_with_roots(
            &pane_c,
            &projects_root,
            &sessions_root,
            &EmptyResolver,
        )
        .expect("pane C should resolve");
        assert!(
            matches!(resolved_c, ClaudeTranscriptResolve::None),
            "expected None for bound-but-missing transcript, got {resolved_c:?}"
        );

        let _ = fs::remove_dir_all(root);
    }

    /// Resolver with no children — unit tests supply sessions files for the
    /// pane pid itself and must not consult live /proc.
    struct EmptyResolver;

    impl ChildResolver for EmptyResolver {
        fn children_of(&self, _pid: u32) -> Vec<u32> {
            Vec::new()
        }
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
    /// RAII guard that captures the prior values of the agy env vars and
    /// restores them on drop, so a panicking test does not leak global state
    /// into sibling tests run in parallel.
    struct AgyEnvGuard {
        prior_state_dir: Option<std::ffi::OsString>,
        prior_history_file: Option<std::ffi::OsString>,
    }

    impl AgyEnvGuard {
        fn install(state_dir: &str, history_file: &str) -> Self {
            let prior_state_dir = std::env::var_os("ANTIGRAVITY_STATE_DIR");
            let prior_history_file = std::env::var_os("ANTIGRAVITY_HISTORY_FILE");
            // SAFETY: callers hold the module-level env_lock for the lifetime
            // of this guard.
            unsafe {
                std::env::set_var("ANTIGRAVITY_STATE_DIR", state_dir);
                std::env::set_var("ANTIGRAVITY_HISTORY_FILE", history_file);
            }
            Self {
                prior_state_dir,
                prior_history_file,
            }
        }
    }

    impl Drop for AgyEnvGuard {
        fn drop(&mut self) {
            // SAFETY: callers hold the module-level env_lock for the lifetime
            // of this guard; restoring is exactly the inverse of `install`.
            unsafe {
                match self.prior_state_dir.take() {
                    Some(value) => std::env::set_var("ANTIGRAVITY_STATE_DIR", value),
                    None => std::env::remove_var("ANTIGRAVITY_STATE_DIR"),
                }
                match self.prior_history_file.take() {
                    Some(value) => std::env::set_var("ANTIGRAVITY_HISTORY_FILE", value),
                    None => std::env::remove_var("ANTIGRAVITY_HISTORY_FILE"),
                }
            }
        }
    }

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
        let _env_guard = AgyEnvGuard::install(&unique, &format!("{unique}/history.jsonl"));

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

        // Exercise the real production path up to the point where the
        // conversation id resolution fails. We cannot drive
        // `load_agy_last_message` end-to-end here because it needs a real
        // `TmuxClient`; instead we exercise `resolve_agy_session_for_pane`
        // (the production input to the error path) and then assert the
        // exact error that `load_agy_last_message` would construct via the
        // shared `agy_no_conversation_error` helper.
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

        // Use the SAME helper the production code uses to build the error,
        // so any drift in the wording is caught here without re-stating the
        // message inline.
        let err = super::agy_no_conversation_error(pane.pane_pid);
        let err_msg = format!("{err}");

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

        fs::remove_dir_all(&unique).ok();
    }

    #[test]
    fn agy_no_conversation_error_no_pid_mentions_session_resolved() {
        // The `pane.pane_pid == None` branch is only reached inside the
        // `session.id.ok_or_else` closure, so the agy pane is already
        // confirmed. The error wording must reflect that.
        let err = super::agy_no_conversation_error(None);
        let err_msg = format!("{err}");
        assert!(
            err_msg.contains("pane confirmed as Antigravity"),
            "no-pid branch should state the pane is confirmed agy: {err_msg}"
        );
        assert!(
            err_msg.contains("pane_pid is unavailable"),
            "no-pid branch should name the missing pane_pid: {err_msg}"
        );
        assert!(
            !err_msg.contains("no matching history.jsonl entry"),
            "no-pid branch must not reuse the generic fallback wording: {err_msg}"
        );
    }
}
