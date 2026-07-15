//! Antigravity (`agy`) CLI integration.
//!
//! Passive read-only discovery: pane process-name + secondary
//! signal (state-dir-exists or frame fingerprint), conversation
//! id resolution via /proc fd walk first and ~/.gemini/antigravity-cli/history.jsonl
//! workspace match second, pane-scrape last-message extraction
//! with a strict rule-boundary contract.
//!
//! ## WL-001 update: protobuf encryption blocker
//!
//! The original wishlist plan (AGY-001) proposed reading conversation
//! transcripts directly from `~/.gemini/antigravity-cli/conversations/<uuid>.pb`
//! by adding a `.proto` definition. Investigation revealed that those `.pb`
//! files are uniformly high-entropy from byte 0 with no inline ASCII — they
//! appear to be encrypted rather than raw protobuf. Parsing the file content
//! is therefore blocked until the encryption layer is understood. See
//! `WISHLIST.md` under "AGY-001" for the full finding and three follow-up
//! paths. The FD-walk half of AGY-001 (resolving *which* `.pb` a running `agy`
//! process has open) already works via `conversation_id_from_process_tree_fds`.

use std::borrow::Cow;
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use serde_json::Value;
use unicode_width::UnicodeWidthStr;

use crate::app::AppResult;
use crate::classifier::SessionState;
use crate::proc_fd::{ChildResolver, transcript_from_process_tree_fds_with_resolver};
use crate::tmux::TmuxPane;

/// Read up to this many bytes from the tail of `history.jsonl` to recover the
/// latest entries without scanning unbounded history.
const HISTORY_TAIL_WINDOW_BYTES: u64 = 128 * 1024;
/// Cap on the number of trailing lines considered from `history.jsonl`.
const HISTORY_MAX_TAIL_LINES: usize = 1000;
/// Disambiguation window — if two distinct conversationIds for the same
/// workspace appear within this many ms of each other, refuse to guess.
const HISTORY_AMBIGUITY_WINDOW_MS: u64 = 60_000;

const SPINNER_GLYPHS: &[char] = &[
    '⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏', '⣾', '⣷', '⣯', '⣟', '⡿', '⢿', '⣻', '⣽',
];

/// Maximum display width (via `UnicodeWidthStr::width`) of an accepted Gemini
/// model label. Generous cap; observed labels are ~24 chars
/// (`Gemini 3.5 Flash (High)`). Anything longer is rejected as a misaligned
/// or attacker-influenced capture.
pub const MODEL_LABEL_MAX_WIDTH: usize = 64;

/// Bottom-anchor scan window: number of non-empty lines from the end of the
/// (ANSI-stripped) frame to consider as candidate footer lines.
pub const MODEL_LABEL_TAIL_WINDOW: usize = 6;

/// The two-space + "Gemini " gutter delimiter that separates the left-aligned
/// footer prefix (`esc to cancel` / `? for shortcuts`) from the right-aligned
/// model label. Using a named constant avoids repeated magic string literals
/// and makes the offset arithmetic below self-documenting.
pub const MODEL_LABEL_GUTTER: &str = "  Gemini ";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgySession {
    /// `None` means "agy pane but no conversation yet resolvable".
    pub id: Option<String>,
    pub workspace: String,
    pub state: SessionState,
    pub has_questions: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgyLastMessage {
    pub conversation_id: String,
    pub text: String,
}

pub fn is_agy_pane(pane: &TmuxPane) -> bool {
    pane.current_command.eq_ignore_ascii_case("agy")
}

pub fn resolve_agy_session_for_pane(
    pane: &TmuxPane,
    frame: &str,
    resolver: &dyn ChildResolver,
) -> AppResult<Option<AgySession>> {
    if !is_agy_pane(pane) {
        return Ok(None);
    }
    if !agy_secondary_signal(frame) {
        return Ok(None);
    }

    let id = if let Some(pid) = pane.pane_pid.filter(|&p| p != 0) {
        match conversation_id_from_process_tree_fds(pid, resolver)? {
            Some(uuid) => Some(uuid),
            None => conversation_id_from_history_jsonl(&pane.current_path)?,
        }
    } else {
        conversation_id_from_history_jsonl(&pane.current_path)?
    };

    let state = classify_agy_state(frame).unwrap_or(SessionState::Unknown);
    Ok(Some(AgySession {
        id,
        workspace: pane.current_path.clone(),
        state,
        has_questions: false,
    }))
}

pub fn latest_assistant_message_for_pane(
    pane: &TmuxPane,
    frame: &str,
    resolver: &dyn ChildResolver,
) -> AppResult<Option<AgyLastMessage>> {
    let Some(session) = resolve_agy_session_for_pane(pane, frame, resolver)? else {
        return Ok(None);
    };
    let Some(conversation_id) = session.id else {
        return Ok(None);
    };

    let stripped = strip_ansi(frame);
    let Some(text) = extract_last_assistant_text(&stripped) else {
        return Ok(None);
    };

    Ok(Some(AgyLastMessage {
        conversation_id,
        text,
    }))
}

pub fn frame_has_agy_fingerprint(frame: &str) -> bool {
    // Strong fingerprint short-circuit — these are unique to agy.
    if frame.contains("Antigravity CLI")
        || frame.contains("1 artifact · /artifact to review")
        || frame.contains("▄▀▀▄")
        || frame.contains("▀▄▀")
    {
        return true;
    }

    // The agy bottom footer always renders one of the footer markers
    // (`esc to cancel` or `? for shortcuts`) left-aligned, with the
    // current Gemini model right-aligned on the same line:
    //
    //   esc to cancel                                          Gemini 3.5 Flash (High)
    //   ? for shortcuts                                        Gemini 3.5 Flash (High)
    //
    // That combination is unique to agy — Claude/Codex/OpenCode/Pi never
    // render `Gemini ` in their footer — and is bottom-anchored so it
    // survives long after the banner scrolls off.
    let mut has_agy_footer = false;
    let mut has_shortcuts_hint = false;
    let mut has_strong_corroborator = false;
    for line in frame.lines() {
        let trimmed = line.trim();

        if (trimmed.starts_with("esc to cancel") || trimmed.starts_with("? for shortcuts"))
            && trimmed.contains("Gemini ")
        {
            has_agy_footer = true;
        }
        if trimmed == "? for shortcuts" {
            has_shortcuts_hint = true;
        }
        if !has_strong_corroborator
            && (trimmed.contains("Gemini 3.")
                || trimmed.contains("Gemini 4.")
                || trimmed.contains("Antigravity")
                || line_has_spinner_with_busy_verb(trimmed))
        {
            has_strong_corroborator = true;
        }
    }
    has_agy_footer || (has_shortcuts_hint && has_strong_corroborator)
}

pub fn classify_agy_state(frame: &str) -> Option<SessionState> {
    if !frame_has_agy_fingerprint(frame) {
        return None;
    }

    let lines: Vec<&str> = frame.lines().map(str::trim).collect();

    // Dispatch in declared order: command-permission (the only shape we
    // characterized well enough to act on) → folder-trust → settings-persist.
    // Each detector keys on its own load-bearing tokens; falling through to
    // `Unknown` keeps us conservative for any future permission-shaped
    // overlay we have not seen yet.
    if is_agy_command_permission_prompt(&lines) {
        return Some(SessionState::AgyCommandPermissionPrompt);
    }
    if is_agy_folder_trust_prompt(&lines) {
        return Some(SessionState::AgyFolderTrustPrompt);
    }
    if is_agy_settings_persist_prompt(&lines) {
        return Some(SessionState::AgySettingsPersistPrompt);
    }

    let tail_window = lines
        .iter()
        .rev()
        .filter(|line| !line.is_empty())
        .take(6)
        .copied()
        .collect::<Vec<_>>();

    let is_busy = tail_window.iter().any(|line| {
        line.contains("esc to cancel")
            || line_has_spinner_with_busy_verb(line)
            || line_has_thought_for_tokens(line)
    });
    if is_busy {
        return Some(SessionState::BusyResponding);
    }

    let is_chat_ready = tail_window
        .iter()
        .any(|line| *line == "? for shortcuts" || line.starts_with("? for shortcuts"));
    if is_chat_ready {
        return Some(SessionState::ChatReady);
    }

    Some(SessionState::Unknown)
}

fn agy_secondary_signal(frame: &str) -> bool {
    state_dir_exists() || frame_has_agy_fingerprint(frame)
}

fn state_dir_exists() -> bool {
    default_state_dir().is_dir()
}

/// Resolve the agy state directory honoring the `ANTIGRAVITY_STATE_DIR`
/// environment variable. Defaults to `$HOME/.gemini/antigravity-cli`.
pub fn default_state_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("ANTIGRAVITY_STATE_DIR")
        && !dir.is_empty()
    {
        return PathBuf::from(dir);
    }

    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".gemini").join("antigravity-cli")
}

/// Resolve the agy history.jsonl path. `ANTIGRAVITY_HISTORY_FILE` wins over
/// `ANTIGRAVITY_STATE_DIR`.
pub fn default_history_file() -> PathBuf {
    if let Some(file) = std::env::var_os("ANTIGRAVITY_HISTORY_FILE")
        && !file.is_empty()
    {
        return PathBuf::from(file);
    }
    default_state_dir().join("history.jsonl")
}

pub(crate) fn conversation_id_from_process_tree_fds(
    pid: u32,
    resolver: &dyn ChildResolver,
) -> AppResult<Option<String>> {
    let conversations_root = default_state_dir().join("conversations");
    let target = transcript_from_process_tree_fds_with_resolver(pid, resolver, |path| {
        path.starts_with(&conversations_root)
            && path.extension().and_then(|value| value.to_str()) == Some("pb")
    })?;

    Ok(target.and_then(|path| {
        path.file_stem()
            .and_then(|stem| stem.to_str())
            .map(str::to_string)
    }))
}

fn conversation_id_from_history_jsonl(current_path: &str) -> AppResult<Option<String>> {
    let history_path = default_history_file();
    let lines = match read_history_tail(&history_path) {
        Ok(lines) => lines,
        Err(_) => return Ok(None),
    };
    Ok(conversation_id_from_history_lines(&lines, current_path))
}

fn conversation_id_from_history_lines(lines: &[String], current_path: &str) -> Option<String> {
    let mut matched: Vec<(u64, String)> = Vec::new();
    for line in lines {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let Some(workspace) = value.get("workspace").and_then(Value::as_str) else {
            continue;
        };
        if workspace != current_path {
            continue;
        }
        let Some(conversation_id) = value.get("conversationId").and_then(Value::as_str) else {
            continue;
        };
        let Some(timestamp) = value.get("timestamp").and_then(Value::as_u64) else {
            continue;
        };
        matched.push((timestamp, conversation_id.to_string()));
    }

    if matched.is_empty() {
        return None;
    }

    matched.sort_by_key(|entry| std::cmp::Reverse(entry.0));
    let (latest_ts, latest_id) = matched[0].clone();
    let mut distinct_recent_ids: Vec<&str> = Vec::new();
    for (ts, id) in &matched {
        if latest_ts.saturating_sub(*ts) > HISTORY_AMBIGUITY_WINDOW_MS {
            break;
        }
        if !distinct_recent_ids.contains(&id.as_str()) {
            distinct_recent_ids.push(id.as_str());
        }
    }

    if distinct_recent_ids.len() > 1 {
        return None;
    }

    Some(latest_id)
}

fn read_history_tail(path: &Path) -> AppResult<Vec<String>> {
    let mut file = fs::File::open(path)?;
    let metadata = file.metadata()?;
    let len = metadata.len();
    let offset = len.saturating_sub(HISTORY_TAIL_WINDOW_BYTES);
    if offset > 0 {
        file.seek(SeekFrom::Start(offset))?;
    }
    let mut buf = Vec::new();
    file.read_to_end(&mut buf)?;
    let text = String::from_utf8_lossy(&buf);
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    if offset > 0 && !lines.is_empty() {
        // first line is a partial mid-line fragment; discard it.
        lines.remove(0);
    }
    if lines.len() > HISTORY_MAX_TAIL_LINES {
        let start = lines.len() - HISTORY_MAX_TAIL_LINES;
        lines = lines[start..].to_vec();
    }
    Ok(lines)
}

/// Detect the agy *command-permission* prompt shape.
///
/// Real-world capture (verbatim from `tmux capture-pane -t 0:1.1 -p` during
/// megamind testing of this branch — see
/// `fixtures/cases/agy_command_permission/`):
///
/// ```text
///   Command
///   ─────────────────────────────────────────────────────────────────
///
///     Requesting permission for: git remote -v
///
///   Do you want to proceed?
///   > 1. Yes
///     2. Yes, and always allow in this conversation for commands that start with 'git remote'
///     3. Yes, and always allow for commands that start with 'git remote' (Persist to settings.json)
///     4. No
///
///     ↑/↓ Navigate · tab Amend · e edit command
///   esc to cancel                                         Gemini 3.5 Flash (High)
/// ```
///
/// Any of the load-bearing tokens below is sufficient to identify the
/// prompt; the structure is unique to agy command-permission prompts.
///
/// Allocation-free: each clause iterates the slice directly rather than
/// joining into an owned `String`.
fn is_agy_command_permission_prompt(lines: &[&str]) -> bool {
    let mut has_requesting = false;
    let mut has_do_you = false;
    let mut has_one_yes = false;
    let mut has_four_no = false;
    let mut has_navigate = false;

    for line in lines {
        if !has_requesting && line.contains("Requesting permission for:") {
            has_requesting = true;
        }
        if !has_do_you && line.contains("Do you want to proceed?") {
            has_do_you = true;
        }
        if !has_one_yes && line.contains("> 1. Yes") {
            has_one_yes = true;
        }
        if !has_four_no && line.contains("4. No") {
            has_four_no = true;
        }
        if !has_navigate && line.contains("↑/↓ Navigate · tab Amend") {
            has_navigate = true;
        }
        if has_navigate
            || (has_requesting && has_do_you)
            || (has_do_you && has_one_yes && has_four_no)
        {
            return true;
        }
    }
    false
}

/// Detect the agy *folder-trust* prompt shape (workspace trust gate shown the
/// first time the CLI sees a new workspace). Keys on canonical `Trust this …`
/// strings, the newer "Do you trust the contents of this project?" wording, and
/// a `[y/n]` / `(y/n)` tail fallback when the frame also mentions workspace,
/// folder, or project.
fn is_agy_folder_trust_prompt(lines: &[&str]) -> bool {
    let mut mentions_workspace_folder_or_project = false;
    for line in lines {
        if line.contains("Trust this workspace?") || line.contains("Trust this folder") {
            return true;
        }
        let lower = line.to_ascii_lowercase();
        if lower.contains("do you trust the contents of this project")
            || lower.contains("yes, i trust this folder")
            || (lower.contains("requires permission")
                && lower.contains("read")
                && lower.contains("edit")
                && lower.contains("execute"))
        {
            return true;
        }
        if !mentions_workspace_folder_or_project
            && (lower.contains("workspace")
                || lower.contains("folder")
                || lower.contains("project"))
        {
            mentions_workspace_folder_or_project = true;
        }
    }
    // Fallback: `[y/n]` / `(y/n)` tail plus a workspace/folder mention elsewhere.
    // Ordered BEFORE settings-persist's `[y/n]` fallback so tail-truncated
    // folder-trust frames are not misclassified as settings-persist.
    if mentions_workspace_folder_or_project
        && let Some(last_non_blank) = lines.iter().rev().find(|line| !line.is_empty()) {
            let lower = last_non_blank.to_ascii_lowercase();
            if lower.ends_with("[y/n]") || lower.ends_with("(y/n)") {
                return true;
            }
        }
    false
}

/// Detect the agy *settings-persist / allow-once* overlay shape. Synthetic —
/// no verbatim live capture yet. Keys on the canonical `Allow once`,
/// `Allow for session`, `Awaiting confirmation` strings, plus a
/// last-non-blank `[y/n]` / `(y/n)` fallback for legacy yes/no shapes. The
/// fallback runs AFTER folder-trust's fallback at the dispatcher level so a
/// `[y/n]` tail with a workspace/folder mention classifies as folder-trust.
/// Allocation-free.
fn is_agy_settings_persist_prompt(lines: &[&str]) -> bool {
    for line in lines {
        if line.contains("Allow once")
            || line.contains("Allow for session")
            || line.contains("Awaiting confirmation")
        {
            return true;
        }
    }
    if let Some(last_non_blank) = lines.iter().rev().find(|line| !line.is_empty()) {
        let lower = last_non_blank.to_ascii_lowercase();
        if lower.ends_with("[y/n]") || lower.ends_with("(y/n)") {
            return true;
        }
    }
    false
}

/// Returns `true` only when the *live* command-permission prompt still shows
/// `> 1. Yes` as the selected option. This is the "cursor on the captured
/// default" guard that gates YOLO auto-approve: if the user arrowed away from
/// option 1 (e.g. onto `> 3. Persist to settings.json`), the predicate fails
/// and the YOLO loop refuses to send `Enter`.
///
/// To stay immune to historical `> 1. Yes` strings sitting in scrollback (a
/// previous approved prompt still visible in the captured frame), the search
/// is restricted to a small window AFTER the live `Do you want to proceed?`
/// line. The first `> [0-9]\.` line in that window must trim-equal
/// `"> 1. Yes"`; otherwise the cursor has been moved off option 1.
///
/// Uses `trim()` equality (not `starts_with`) so a line like `> 1. Yesterday`
/// does NOT match — the comparison must be exact after trimming whitespace.
pub(crate) fn agy_command_permission_default_option_is_yes(frame: &str) -> bool {
    /// How many lines after `Do you want to proceed?` to consider part of the
    /// live prompt. The captured shape always lists options 1–4 immediately
    /// after the prompt question, so a window of 8 covers blank separators
    /// plus all four options with margin.
    const LIVE_WINDOW: usize = 8;

    let lines: Vec<&str> = frame.lines().collect();
    let Some(prompt_idx) = lines
        .iter()
        .rposition(|line| line.contains("Do you want to proceed?"))
    else {
        return false;
    };

    let start = prompt_idx + 1;
    let end = start.saturating_add(LIVE_WINDOW).min(lines.len());
    for line in &lines[start..end] {
        let trimmed = line.trim();
        if !is_cursor_option_line(trimmed) {
            continue;
        }
        return trimmed == "> 1. Yes";
    }
    false
}

/// Returns `true` if `trimmed` matches the cursor-option shape `> N. ...`
/// where `N` is a single ASCII digit (1–9). Allocation-free.
fn is_cursor_option_line(trimmed: &str) -> bool {
    let mut chars = trimmed.chars();
    if chars.next() != Some('>') {
        return false;
    }
    if chars.next() != Some(' ') {
        return false;
    }
    let Some(digit) = chars.next() else {
        return false;
    };
    if !digit.is_ascii_digit() {
        return false;
    }
    chars.next() == Some('.')
}

/// Detect a busy-spinner line: any of the Braille spinner glyphs from
/// `SPINNER_GLYPHS`, optionally surrounded by whitespace, followed by a
/// single ASCII space and an uppercase ASCII verb whose token ends with
/// `...`. This covers `Working`, `Generating`, `Thinking`, `Reading`,
/// `Searching`, `Calling`, `Reasoning`, and any future agy verb without
/// hard-coding the verb set. Allocation-free: scans chars in-place.
fn line_has_spinner_with_busy_verb(line: &str) -> bool {
    for (idx, ch) in line.char_indices() {
        if !SPINNER_GLYPHS.contains(&ch) {
            continue;
        }
        // Expect: glyph, space, uppercase ASCII letter, then more letters,
        // then `...` somewhere on the rest of the line (allowing trailing
        // text such as `esc to cancel`).
        let rest = &line[idx + ch.len_utf8()..];
        let mut rest_chars = rest.chars();
        let Some(next_char) = rest_chars.next() else {
            continue;
        };
        if next_char != ' ' {
            continue;
        }
        let Some(verb_first) = rest_chars.next() else {
            continue;
        };
        if !verb_first.is_ascii_uppercase() {
            continue;
        }
        // After the first uppercase letter, require at least one more ASCII
        // letter, then look for `...` anywhere afterward.
        let after_first = &rest[1 + verb_first.len_utf8()..];
        let letter_run = after_first
            .chars()
            .take_while(|c| c.is_ascii_alphabetic())
            .count();
        if letter_run == 0 {
            continue;
        }
        if after_first.contains("...") {
            return true;
        }
    }
    false
}

fn line_has_thought_for_tokens(line: &str) -> bool {
    let trimmed = line.trim_start_matches('▸').trim();
    trimmed.starts_with("Thought for") && trimmed.contains("tokens")
}

fn line_is_rule(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.chars().count() >= 20 && trimmed.chars().all(|c| c == '─')
}

pub(crate) fn extract_last_assistant_text(frame: &str) -> Option<String> {
    let lines: Vec<&str> = frame.lines().collect();

    let last_non_blank = lines.iter().rev().find(|line| !line.trim().is_empty())?;
    let last_trimmed = last_non_blank.trim();
    if line_has_spinner_with_busy_verb(last_trimmed) || last_trimmed.contains("esc to cancel") {
        return None;
    }

    let rule_indices: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter_map(|(i, line)| line_is_rule(line).then_some(i))
        .collect();
    if rule_indices.len() < 2 {
        return None;
    }

    // The most-recent rule pair brackets the live input box (rules around `>`).
    // Above that, the previous rule marks the boundary above the latest
    // assistant turn. We want the slice between the rule just above the
    // input box and the prior rule above that.
    let len = rule_indices.len();
    // Identify the contiguous input-box rules. Treat the last rule as the
    // bottom of the input box; the one immediately above it as the top of
    // the input box; the next rule above is the end of the assistant turn.
    let input_box_bottom = rule_indices[len - 1];
    let input_box_top = rule_indices[len - 2];
    // The previous boundary above the input box (if any) is the previous rule.
    let prior_rule = if len >= 3 {
        Some(rule_indices[len - 3])
    } else {
        None
    };

    // The assistant turn lives between `prior_rule` (exclusive) and
    // `input_box_top` (exclusive). If there is no prior rule we still need
    // two visible rules to satisfy the contract.
    let (start, end) = {
        let prior = prior_rule?;
        (prior + 1, input_box_top)
    };
    if end <= start {
        return None;
    }
    let _ = input_box_bottom; // already validated as the last rule

    let mut collected = Vec::new();
    // V3: track filter context across iterations. When we filter a
    // `▸ Thought for ...` header or a `● <tool>` line, the agy CLI may
    // render continuation/subtitle lines indented by 2+ spaces (e.g.
    // `  Considering Scheduling Needs`, `  Drafting an example response`,
    // wrapped tool-call bodies) and outcome blocks led by `⎿ ` / `└─`.
    // Those continuation lines must also be dropped until we encounter a
    // non-indented, non-outcome-block, non-blank line — only then are we
    // back to assistant text.
    let mut skip_until_dedent = false;
    for raw in &lines[start..end] {
        let line = raw.trim_end();
        let trimmed = line.trim_start();
        if trimmed.starts_with("> ") || trimmed == ">" {
            // User-prompt echo — does not enter the skip state because the
            // agy CLI does not render indented continuations after a prompt
            // echo, and treating it as such would swallow legitimate
            // assistant prose that immediately follows.
            continue;
        }
        if trimmed.starts_with("● ") {
            skip_until_dedent = true;
            continue;
        }
        if trimmed.starts_with("▸ Thought for ") {
            skip_until_dedent = true;
            continue;
        }
        if trimmed.starts_with('⎿') || trimmed.starts_with("└─") {
            // Outcome block markers always belong to the previous tool/thought
            // block, never to assistant text.
            skip_until_dedent = true;
            continue;
        }
        if skip_until_dedent {
            // Indented continuation (2+ spaces) belongs to the prior block.
            // Blank lines neither end the skip nor land in `collected` — they
            // would visually re-open the block on the next non-blank line.
            if line.is_empty() {
                continue;
            }
            let leading_spaces = line.len() - line.trim_start_matches(' ').len();
            if leading_spaces >= 2 {
                continue;
            }
            // First non-indented, non-outcome-block, non-blank line — fall
            // through and treat as regular assistant text.
            skip_until_dedent = false;
        }
        if line.is_empty() {
            collected.push(String::new());
            continue;
        }
        collected.push(line.to_string());
    }

    // Trim blank lines from both ends.
    while collected.first().is_some_and(|line| line.trim().is_empty()) {
        collected.remove(0);
    }
    while collected.last().is_some_and(|line| line.trim().is_empty()) {
        collected.pop();
    }

    if collected.is_empty() {
        return None;
    }

    let joined = collected.join("\n");
    if joined.trim().is_empty() {
        return None;
    }
    Some(joined)
}

/// Extract the right-aligned Gemini model label from the agy bottom footer.
///
/// Bottom-anchored, ANSI-stripped, control-character-rejecting (whole line,
/// not just the label slice), width-capped.
///
/// Looks at the last [`MODEL_LABEL_TAIL_WINDOW`] non-empty lines of the frame.
/// For each line (scanning bottom-up), embedded `\r` bytes are stripped first;
/// then the whole line is checked for ASCII control characters — a control char
/// anywhere (prefix or label) disqualifies the line. The line must start with
/// `"esc to cancel"` or `"? for shortcuts"` (after `trim_start`), and must
/// contain [`MODEL_LABEL_GUTTER`] as the right-most delimiter. The label is
/// extracted starting at "Gemini" and trimmed of trailing whitespace.
///
/// Returns `None` when no qualifying footer line is found.
pub fn extract_model_label(frame: &str) -> Option<String> {
    let stripped = strip_ansi(frame);
    // Collect the last MODEL_LABEL_TAIL_WINDOW non-empty lines bottom-up
    // (index 0 = bottom-most line) in a single pass — no double-reverse needed.
    let tail: Vec<&str> = stripped
        .lines()
        .filter(|line| !line.trim().is_empty())
        .rev()
        .take(MODEL_LABEL_TAIL_WINDOW)
        .collect();

    // Walk bottom-up — most-recent footer line wins (tail[0] is the bottom-most).
    for raw_line in tail.iter() {
        // Strip embedded CR bytes before any delimiter search (handles TUI
        // redraws that emit \r before the newline).
        let line = raw_line.replace('\r', "");
        // Reject the whole line if it contains any ASCII control character
        // (covers both the prefix and the label slice — consistent with the
        // doc claim "control-character-rejecting").
        if line.chars().any(|c| c.is_control()) {
            continue;
        }
        let trimmed_start = line.trim_start().to_string();
        if !trimmed_start.starts_with("esc to cancel")
            && !trimmed_start.starts_with("? for shortcuts")
        {
            continue;
        }
        // Use rfind so that the rightmost gutter occurrence is used, which
        // is the correct right-aligned label even on a line that contains
        // the gutter string more than once.
        let Some(idx) = line.rfind(MODEL_LABEL_GUTTER) else {
            continue;
        };
        // Extract from "Gemini" (skip the two leading spaces of the gutter).
        // The offset is derived from the constant length, not a magic number.
        let label_start = idx + MODEL_LABEL_GUTTER.len() - "Gemini ".len();
        let label = line[label_start..].trim_end();
        // Reject incomplete captures: the gutter without an actual label
        // (e.g. a footer that ends mid-render at `...  Gemini `) would
        // otherwise leave just `Gemini` and get cached as the sticky model.
        let Some(suffix) = label.strip_prefix("Gemini ") else {
            continue;
        };
        if suffix.trim().is_empty() {
            continue;
        }
        // Reject labels exceeding the display-width cap.
        if UnicodeWidthStr::width(label) > MODEL_LABEL_MAX_WIDTH {
            continue;
        }
        return Some(label.to_string());
    }
    None
}

pub(crate) fn strip_ansi(input: &str) -> Cow<'_, str> {
    if !input.contains('\x1b') {
        return Cow::Borrowed(input);
    }
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            match chars.peek() {
                Some(&'[') => {
                    chars.next();
                    while let Some(&n) = chars.peek() {
                        chars.next();
                        if matches!(n, '@'..='~') {
                            break;
                        }
                    }
                    continue;
                }
                Some(&']') => {
                    chars.next();
                    // OSC sequences end with BEL (\x07) or ST (\x1b\\).
                    while let Some(&n) = chars.peek() {
                        chars.next();
                        if n == '\x07' {
                            break;
                        }
                        if n == '\x1b' {
                            if chars.peek() == Some(&'\\') {
                                chars.next();
                            }
                            break;
                        }
                    }
                    continue;
                }
                _ => {
                    // Unknown / single-char escape — drop the ESC.
                    continue;
                }
            }
        }
        out.push(c);
    }
    Cow::Owned(out)
}

#[cfg(any(test, rust_analyzer))]
mod tests {
    use super::*;
    use std::sync::{Mutex, OnceLock};

    /// Serialize tests that mutate process-global env vars
    /// (`ANTIGRAVITY_STATE_DIR`, `ANTIGRAVITY_HISTORY_FILE`) so they don't
    /// race with each other or with other tests that read agy env defaults
    /// when `cargo test` runs in parallel.
    fn env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    #[test]
    fn frame_fingerprint_detects_banner_footer_spinner_artifact() {
        // Strong fingerprints (banner / banner glyphs / artifact tray).
        assert!(frame_has_agy_fingerprint("...\nAntigravity CLI 1.0.2\n"));
        assert!(frame_has_agy_fingerprint(
            "...\n1 artifact · /artifact to review\n"
        ));
        assert!(frame_has_agy_fingerprint("...\n      ▄▀▀▄\n     ▀▀▀▀▀▀\n"));

        // Bottom-anchored agy footer: `(esc to cancel|? for shortcuts)`
        // co-occurring with the right-aligned Gemini model line.
        assert!(frame_has_agy_fingerprint(
            "...\n? for shortcuts                              Gemini 3.5 Flash (High)\n",
        ));
        assert!(frame_has_agy_fingerprint(
            "...\nesc to cancel                                Gemini 3.5 Flash (High)\n",
        ));

        // `? for shortcuts` co-occurring with another corroborator (banner /
        // Antigravity / Gemini X.X) elsewhere in the frame.
        assert!(frame_has_agy_fingerprint(
            "Some output\nGemini 3.5 Flash\n...\n? for shortcuts\n"
        ));

        // Negative cases:
        // - Bare `? for shortcuts` with no corroborator.
        assert!(!frame_has_agy_fingerprint("...\n? for shortcuts\n"));
        // - Bare `esc to cancel` (Claude's permission/busy footer also uses
        //   it, so it cannot be the only signal).
        assert!(!frame_has_agy_fingerprint("...\nesc to cancel\n"));
        // - Spinner alone without footer / banner / Gemini context.
        assert!(!frame_has_agy_fingerprint("⣾ Working..."));
        assert!(!frame_has_agy_fingerprint("⣾ Thinking..."));
        // - Random text.
        assert!(!frame_has_agy_fingerprint("just some random text"));
        // - Lowercase verbs after a glyph must NOT match
        //   (assistant prose can start with a spinner-shaped glyph).
        assert!(!frame_has_agy_fingerprint("⣾ working hard on this..."));
    }

    #[test]
    fn line_has_spinner_with_busy_verb_covers_observed_verbs() {
        // V7: cover the verbs agy emits in practice beyond Working/Generating.
        assert!(line_has_spinner_with_busy_verb("⣾ Working..."));
        assert!(line_has_spinner_with_busy_verb("⣾ Generating..."));
        assert!(line_has_spinner_with_busy_verb("⣾ Thinking..."));
        assert!(line_has_spinner_with_busy_verb("⣷ Reading..."));
        assert!(line_has_spinner_with_busy_verb("⠋ Searching for fish..."));
        assert!(line_has_spinner_with_busy_verb(
            "⣾ Working...                                  esc to cancel"
        ));
        // Negative cases.
        assert!(!line_has_spinner_with_busy_verb("just text"));
        assert!(!line_has_spinner_with_busy_verb("⣾"));
        assert!(!line_has_spinner_with_busy_verb("⣾ working..."));
        assert!(!line_has_spinner_with_busy_verb("⣾ W"));
    }

    #[test]
    fn classify_agy_chat_ready() {
        let frame = concat!(
            "────────────────────────────\n",
            "> hello\n",
            "────────────────────────────\n",
            "Some assistant reply text\n",
            "───────────────────────────────────────\n",
            ">\n",
            "───────────────────────────────────────\n",
            "? for shortcuts                              Gemini 3.5 Flash (High)\n",
        );
        assert_eq!(classify_agy_state(frame), Some(SessionState::ChatReady));
    }

    #[test]
    fn classify_agy_busy_responding_via_esc_to_cancel() {
        let frame = concat!(
            "Antigravity CLI 1.0.2\n",
            "> please do something\n",
            "▸ Thought for 1s, 200 tokens\n",
            "⣾ Working...                                  esc to cancel\n",
        );
        assert_eq!(
            classify_agy_state(frame),
            Some(SessionState::BusyResponding)
        );
    }

    #[test]
    fn classify_agy_busy_responding_via_spinner() {
        let frame = "Antigravity CLI 1.0.2\n⣾ Working... Gemini 3.5 Flash (High)\n";
        assert_eq!(
            classify_agy_state(frame),
            Some(SessionState::BusyResponding)
        );
    }

    #[test]
    fn classify_agy_folder_trust_prompt_returns_agy_folder_trust() {
        // Folder-trust shape: `Trust this workspace?` + `[y/N]` token. The
        // frame must also satisfy the agy fingerprint — the bare
        // `? for shortcuts` line needs a strong corroborator, so the banner
        // line carries the load.
        let frame = concat!(
            "Antigravity CLI 1.0.2\n",
            "Trust this workspace?\n",
            "[y/N]\n",
            "? for shortcuts\n",
        );
        assert_eq!(
            classify_agy_state(frame),
            Some(SessionState::AgyFolderTrustPrompt)
        );
    }

    #[test]
    fn classify_agy_project_trust_prompt_returns_agy_folder_trust() {
        let frame = concat!(
            "Accessing workspace:\n",
            "/home/colin/Projects/shipstream/thespider\n",
            "Do you trust the contents of this project?\n",
            "Antigravity CLI requires permission to read, edit, and execute files here.\n",
            "> Yes, I trust this folder\n",
            "  No, exit\n",
            "  ↑/↓ Navigate · enter Confirm\n",
            "                                                         Gemini 3.5 Flash (High)\n",
        );
        assert_eq!(
            classify_agy_state(frame),
            Some(SessionState::AgyFolderTrustPrompt)
        );
    }

    #[test]
    fn classify_agy_settings_persist_returns_settings_persist() {
        // Synthetic allow-once / allow-for-session overlay. Frame must
        // fingerprint as agy via the banner.
        let frame = concat!(
            "Antigravity CLI 1.0.2\n",
            "Allow once\n",
            "Allow for session\n",
            "? for shortcuts\n",
        );
        assert_eq!(
            classify_agy_state(frame),
            Some(SessionState::AgySettingsPersistPrompt)
        );
    }

    #[test]
    fn classify_real_world_agy_command_permission_prompt_returns_agy_command_permission() {
        // Verbatim shape of the command-permission prompt captured live
        // from `tmux capture-pane -t 0:1.1 -p` during megamind testing
        // of this branch (see fixtures/cases/agy_command_permission/).
        let frame = concat!(
            "● Bash(git remote -v) (ctrl+o to expand)\n",
            "\n",
            "Command\n",
            "─────────────────────────────────────────────\n",
            "\n",
            "  Requesting permission for: git remote -v\n",
            "\n",
            "Do you want to proceed?\n",
            "> 1. Yes\n",
            "  2. Yes, and always allow in this conversation for commands that start with 'git remote'\n",
            "  3. Yes, and always allow for commands that start with 'git remote' (Persist to settings.json)\n",
            "  4. No\n",
            "\n",
            "  ↑/↓ Navigate · tab Amend · e edit command\n",
            "esc to cancel                                       Gemini 3.5 Flash (High)\n",
        );
        // Must fingerprint as agy via the bottom-anchored footer.
        assert!(
            frame_has_agy_fingerprint(frame),
            "real-world command prompt should fingerprint as agy"
        );
        // Must classify as the new shape-specific variant so the YOLO loop
        // can act on it (and only on it).
        assert_eq!(
            classify_agy_state(frame),
            Some(SessionState::AgyCommandPermissionPrompt)
        );
    }

    #[test]
    fn is_agy_command_permission_prompt_accepts_captured_frame() {
        let lines: Vec<&str> = vec![
            "Command",
            "─────────────────────────────────────────────",
            "",
            "  Requesting permission for: git remote -v",
            "",
            "Do you want to proceed?",
            "> 1. Yes",
            "  4. No",
        ];
        assert!(is_agy_command_permission_prompt(&lines));
    }

    #[test]
    fn is_agy_command_permission_prompt_rejects_folder_trust_frame() {
        let lines: Vec<&str> = vec![
            "Antigravity CLI 1.0.2",
            "Trust this workspace?",
            "[y/N]",
            "? for shortcuts",
        ];
        assert!(!is_agy_command_permission_prompt(&lines));
    }

    #[test]
    fn is_agy_folder_trust_prompt_rejects_command_permission_frame() {
        let lines: Vec<&str> = vec![
            "Command",
            "─────────────────────────────────────────────",
            "",
            "  Requesting permission for: git remote -v",
            "",
            "Do you want to proceed?",
            "> 1. Yes",
            "  4. No",
        ];
        assert!(!is_agy_folder_trust_prompt(&lines));
    }

    #[test]
    fn is_agy_settings_persist_prompt_rejects_command_permission_frame() {
        let lines: Vec<&str> = vec![
            "Command",
            "─────────────────────────────────────────────",
            "",
            "  Requesting permission for: git remote -v",
            "",
            "Do you want to proceed?",
            "> 1. Yes",
            "  4. No",
        ];
        assert!(!is_agy_settings_persist_prompt(&lines));
    }

    #[test]
    fn agy_command_permission_default_option_is_yes_positive() {
        let frame = concat!(
            "Do you want to proceed?\n",
            "> 1. Yes\n",
            "  2. No\n",
            "esc to cancel                                       Gemini 3.5 Flash (High)\n",
        );
        assert!(agy_command_permission_default_option_is_yes(frame));
    }

    #[test]
    fn agy_command_permission_default_option_is_yes_rejects_stale_scrollback() {
        // Regression: scrollback contains a `> 1. Yes` from a previously
        // approved prompt, but the LIVE prompt's cursor is on option 3
        // ("Persist to settings.json"). The guard must refuse so YOLO does
        // not write `settings.json`.
        let frame = concat!(
            "Do you want to proceed?\n",
            "> 1. Yes\n",
            "  2. No\n",
            "(scrollback above; live prompt below)\n",
            "─────────────────────────────────────────────\n",
            "Do you want to proceed?\n",
            "  1. Yes\n",
            "  2. Yes, and always allow in this conversation\n",
            "> 3. Yes, and always allow (Persist to settings.json)\n",
            "  4. No\n",
        );
        assert!(!agy_command_permission_default_option_is_yes(frame));
    }

    #[test]
    fn agy_command_permission_default_option_is_yes_negative() {
        // Cursor moved to option 3 — the default-on-1 guard must refuse so the
        // YOLO loop does not approve the destructive "Persist to settings.json"
        // option.
        let frame = concat!(
            "Do you want to proceed?\n",
            "  1. Yes\n",
            "  2. Yes, and always allow in this conversation\n",
            "> 3. Yes, and always allow (Persist to settings.json)\n",
            "  4. No\n",
        );
        assert!(!agy_command_permission_default_option_is_yes(frame));

        // Also reject prefix matches like `> 1. Yesterday` — guards against
        // any future option-1 string that happens to begin with the captured
        // token.
        let frame_prefix_only = "> 1. Yesterday\n";
        assert!(!agy_command_permission_default_option_is_yes(
            frame_prefix_only
        ));
    }

    #[test]
    fn classify_agy_folder_trust_yn_fallback_classifies_as_folder_trust() {
        // Regression: tail-truncated folder-trust frame whose verbatim
        // `Trust this workspace?` has scrolled out of the captured window.
        // The `[y/n]` tail plus a `workspace` mention elsewhere must
        // classify as folder-trust (not settings-persist). The agy
        // fingerprint here comes from the strong `Antigravity CLI`
        // short-circuit so we do NOT rely on a footer (which would otherwise
        // make the `[y/n]` not be the last non-blank line).
        let frame = concat!(
            "Antigravity CLI 1.0.2\n",
            "Reviewing workspace /tmp/demo\n",
            "Confirm to continue: [y/N]\n",
        );
        assert_eq!(
            classify_agy_state(frame),
            Some(SessionState::AgyFolderTrustPrompt)
        );
    }

    #[test]
    fn classify_returns_none_when_not_agy() {
        assert!(classify_agy_state("just claude output").is_none());
    }

    #[test]
    fn history_jsonl_exact_workspace_match_picks_latest() {
        // "older" is well outside the 60s ambiguity window, so "newer" wins
        // unambiguously.
        let lines = vec![
            r#"{"display":"old","timestamp":1000,"workspace":"/tmp/a","conversationId":"older"}"#
                .to_string(),
            r#"{"display":"new","timestamp":5000000,"workspace":"/tmp/a","conversationId":"newer"}"#
                .to_string(),
            r#"{"display":"other","timestamp":4000000,"workspace":"/tmp/b","conversationId":"unrelated"}"#
                .to_string(),
        ];
        assert_eq!(
            conversation_id_from_history_lines(&lines, "/tmp/a"),
            Some(String::from("newer"))
        );
    }

    #[test]
    fn history_jsonl_two_recent_conversations_same_cwd_returns_none() {
        // Two distinct conversationIds within HISTORY_AMBIGUITY_WINDOW_MS (60s)
        // → ambiguous, return None.
        let lines = vec![
            r#"{"display":"a","timestamp":100000,"workspace":"/tmp/a","conversationId":"alpha"}"#
                .to_string(),
            r#"{"display":"b","timestamp":150000,"workspace":"/tmp/a","conversationId":"beta"}"#
                .to_string(),
        ];
        assert_eq!(conversation_id_from_history_lines(&lines, "/tmp/a"), None);
    }

    #[test]
    fn history_jsonl_drops_partial_write_line() {
        // The malformed JSON line is silently dropped; the valid latest entry
        // wins.
        let lines = vec![
            r#"{"display":"good","timestamp":3000,"workspace":"/tmp/a","conversationId":"keep"}"#
                .to_string(),
            r#"{"display":"truncate"#.to_string(), // partial write
        ];
        assert_eq!(
            conversation_id_from_history_lines(&lines, "/tmp/a"),
            Some(String::from("keep"))
        );
    }

    #[test]
    fn history_jsonl_no_workspace_match_returns_none() {
        let lines = vec![
            r#"{"display":"x","timestamp":1,"workspace":"/tmp/other","conversationId":"x"}"#
                .to_string(),
        ];
        assert_eq!(conversation_id_from_history_lines(&lines, "/tmp/a"), None);
    }

    #[test]
    fn history_jsonl_ignores_entries_missing_conversation_id() {
        let lines = vec![
            r#"{"display":"first","timestamp":100,"workspace":"/tmp/a"}"#.to_string(),
            r#"{"display":"second","timestamp":200,"workspace":"/tmp/a","conversationId":"ok"}"#
                .to_string(),
        ];
        assert_eq!(
            conversation_id_from_history_lines(&lines, "/tmp/a"),
            Some(String::from("ok"))
        );
    }

    #[test]
    fn last_message_returns_none_when_one_rule_missing() {
        let frame = concat!(
            "Some assistant reply\n",
            "───────────────────────────────────────\n",
            ">\n",
            "───────────────────────────────────────\n",
            "? for shortcuts\n",
        );
        assert!(extract_last_assistant_text(frame).is_none());
    }

    #[test]
    fn last_message_returns_some_with_clean_text_between_rules() {
        let frame = concat!(
            "────────────────────────────\n",
            "> question\n",
            "────────────────────────────\n",
            "assistant line one\n",
            "assistant line two\n",
            "────────────────────────────\n",
            ">\n",
            "────────────────────────────\n",
            "? for shortcuts\n",
        );
        let text = extract_last_assistant_text(frame).expect("should extract");
        assert!(text.contains("assistant line one"));
        assert!(text.contains("assistant line two"));
        assert!(!text.contains('>'));
    }

    #[test]
    fn last_message_returns_none_when_frame_ends_mid_spinner() {
        let frame = concat!(
            "────────────────────────────\n",
            "answer in progress\n",
            "────────────────────────────\n",
            "⣾ Working...\n",
        );
        assert!(extract_last_assistant_text(frame).is_none());
    }

    #[test]
    fn last_message_strips_ansi_then_finds_rules() {
        let frame = concat!(
            "────────────────────────────\n",
            "\x1b[31m> question\x1b[0m\n",
            "────────────────────────────\n",
            "\x1b[1massistant\x1b[0m reply text\n",
            "────────────────────────────\n",
            ">\n",
            "────────────────────────────\n",
            "? for shortcuts\n",
        );
        let stripped = strip_ansi(frame);
        let text = extract_last_assistant_text(&stripped).expect("should extract");
        assert!(text.contains("assistant reply text"));
        assert!(!text.contains('\x1b'));
    }

    #[test]
    fn last_message_skips_user_prompt_tool_calls_thoughts_and_slash_echoes() {
        let frame = concat!(
            "────────────────────────────\n",
            "> earlier question\n",
            "────────────────────────────\n",
            "> stray user line\n",
            "● Bash(ls)\n",
            "▸ Thought for 1s, 5 tokens\n",
            "⎿  Exited /skills command\n",
            "real assistant content\n",
            "────────────────────────────\n",
            ">\n",
            "────────────────────────────\n",
            "? for shortcuts\n",
        );
        let text = extract_last_assistant_text(frame).expect("should extract");
        assert!(text.contains("real assistant content"));
        assert!(!text.contains("stray user line"));
        assert!(!text.contains("Bash(ls)"));
        assert!(!text.contains("Thought for"));
        assert!(!text.contains("Exited /skills"));
    }

    #[test]
    fn latest_assistant_message_returns_none_when_session_id_unresolvable() {
        // Serialize against other tests that read/write the same env vars.
        let _guard = env_lock().lock().expect("env lock poisoned");
        // Point env at a definitely-nonexistent state dir so:
        // - state_dir_exists() == false
        // - frame fingerprint still triggers agy_secondary_signal
        // - history file resolution fails (file doesn't exist)
        // - pane.pane_pid is None so process-tree fd walk is skipped
        // Result: resolve_agy_session_for_pane returns Some(session) with id=None,
        // and latest_assistant_message_for_pane returns Ok(None) (no conversation id).
        let unique = format!(
            "/tmp/agy-test-no-such-dir-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        // SAFETY: tests in this module are not racy on these env vars.
        unsafe {
            std::env::set_var("ANTIGRAVITY_STATE_DIR", &unique);
            std::env::set_var(
                "ANTIGRAVITY_HISTORY_FILE",
                format!("{unique}/history.jsonl"),
            );
        }
        let pane = TmuxPane {
            pane_id: String::from("%1"),
            pane_tty: String::from("/dev/pts/1"),
            pane_pid: None,
            session_id: String::from("$1"),
            session_name: String::from("demo"),
            window_id: String::from("@1"),
            window_index: 0,
            window_name: String::from("agy"),
            pane_index: 0,
            current_command: String::from("agy"),
            current_path: String::from("/tmp/agy-test-workspace"),
            pane_title: String::new(),
            pane_active: true,
            cursor_x: None,
            cursor_y: None,
        };
        let frame = "Antigravity CLI 1.0.2\n? for shortcuts\n";

        // The resolved session should have id == None.
        let session = resolve_agy_session_for_pane(&pane, frame, &crate::proc_fd::LiveProc)
            .expect("resolution should not error")
            .expect("agy pane with fingerprint should yield a session");
        assert!(
            session.id.is_none(),
            "no conversation should resolve when fds and history are absent"
        );

        // And latest_assistant_message_for_pane should return Ok(None) for this case
        // (callers in last_message.rs translate this into the no-session error).
        assert!(
            latest_assistant_message_for_pane(&pane, frame, &crate::proc_fd::LiveProc)
                .expect("call should not error")
                .is_none()
        );

        unsafe {
            std::env::remove_var("ANTIGRAVITY_STATE_DIR");
            std::env::remove_var("ANTIGRAVITY_HISTORY_FILE");
        }
    }

    #[test]
    fn last_message_drops_multi_line_thought_subtitle() {
        // V3: the line after `▸ Thought for ...` is the subtitle (indented),
        // and it must not leak into the extracted assistant text.
        let frame = concat!(
            "────────────────────────────\n",
            "> ask a thing\n",
            "────────────────────────────\n",
            "▸ Thought for 2s, 1.2k tokens\n",
            "  Considering Scheduling Needs\n",
            "  Drafting an example response\n",
            "real assistant content lives here\n",
            "────────────────────────────\n",
            ">\n",
            "────────────────────────────\n",
            "? for shortcuts\n",
        );
        let text = extract_last_assistant_text(frame).expect("should extract");
        assert!(text.contains("real assistant content lives here"));
        assert!(
            !text.contains("Considering Scheduling Needs"),
            "thought subtitle must not leak: {text}"
        );
        assert!(
            !text.contains("Drafting an example response"),
            "thought subtitle must not leak: {text}"
        );
        assert!(!text.contains("Thought for"));
    }

    #[test]
    fn last_message_drops_wrapped_tool_call_body_and_outcome_block() {
        // V3: the lines after `● Bash(...)` are wrapped body / outcome
        // markers (`⎿`). They must not leak into assistant text.
        let frame = concat!(
            "────────────────────────────\n",
            "> ask another thing\n",
            "────────────────────────────\n",
            "● Bash(echo something really really long that wraps)\n",
            "  result line one of the wrapped tool output\n",
            "  result line two of the wrapped tool output\n",
            "⎿  Exited /skills command\n",
            "  trailing tool detail line\n",
            "final assistant answer paragraph\n",
            "────────────────────────────\n",
            ">\n",
            "────────────────────────────\n",
            "? for shortcuts\n",
        );
        let text = extract_last_assistant_text(frame).expect("should extract");
        assert!(text.contains("final assistant answer paragraph"));
        assert!(
            !text.contains("result line one of the wrapped tool output"),
            "tool-call body must not leak: {text}"
        );
        assert!(
            !text.contains("result line two of the wrapped tool output"),
            "tool-call body must not leak: {text}"
        );
        assert!(!text.contains("Exited /skills command"));
        assert!(!text.contains("trailing tool detail line"));
        assert!(!text.contains("Bash"));
    }

    #[test]
    fn strip_ansi_preserves_input_with_no_escapes() {
        let input = "plain ascii text";
        match strip_ansi(input) {
            Cow::Borrowed(s) => assert_eq!(s, input),
            Cow::Owned(_) => panic!("should not allocate when input has no escapes"),
        }
    }

    #[test]
    fn strip_ansi_removes_color_codes() {
        let stripped = strip_ansi("\x1b[31mred\x1b[0m text");
        assert_eq!(stripped.as_ref(), "red text");
    }

    // ── extract_model_label tests ─────────────────────────────────────────────

    #[test]
    fn resolve_agy_session_skips_fd_walk_when_pid_is_zero() {
        // V-5: pane_pid == Some(0) must be treated as "no usable pid" — the
        // FD walk is skipped and we fall through to the history.jsonl path.
        // With a nonexistent state dir and history file, the result is
        // Some(session) with id == None (not a panic, not a /proc/0 access).
        let _guard = env_lock().lock().expect("env lock poisoned");
        let unique = format!(
            "/tmp/agy-test-pid0-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        unsafe {
            std::env::set_var("ANTIGRAVITY_STATE_DIR", &unique);
            std::env::set_var(
                "ANTIGRAVITY_HISTORY_FILE",
                format!("{unique}/history.jsonl"),
            );
        }
        // RAII guard: if the test panics partway through, the env vars are
        // still cleared so we don't poison sibling tests.
        struct EnvReset;
        impl Drop for EnvReset {
            fn drop(&mut self) {
                unsafe {
                    std::env::remove_var("ANTIGRAVITY_STATE_DIR");
                    std::env::remove_var("ANTIGRAVITY_HISTORY_FILE");
                }
            }
        }
        let _env_reset = EnvReset;
        let pane = TmuxPane {
            pane_id: String::from("%5"),
            pane_tty: String::from("/dev/pts/5"),
            pane_pid: Some(0), // should be rejected
            session_id: String::from("$5"),
            session_name: String::from("demo"),
            window_id: String::from("@5"),
            window_index: 0,
            window_name: String::from("agy"),
            pane_index: 0,
            current_command: String::from("agy"),
            current_path: String::from("/tmp/agy-pid0-workspace"),
            pane_title: String::new(),
            pane_active: true,
            cursor_x: None,
            cursor_y: None,
        };
        let frame = "Antigravity CLI 1.0.2\n? for shortcuts\n";
        // Must not panic or access /proc/0.
        let session = resolve_agy_session_for_pane(&pane, frame, &crate::proc_fd::LiveProc)
            .expect("should not error")
            .expect("fingerprint present, session should resolve");
        assert!(
            session.id.is_none(),
            "pid=0 skips fd walk; with no history.jsonl, id should be None"
        );
    }

    #[test]
    fn extract_model_label_finds_label_from_esc_to_cancel_footer() {
        let frame = concat!(
            "Some content\n",
            "More content\n",
            "esc to cancel                                          Gemini 3.5 Flash (High)\n",
        );
        assert_eq!(
            extract_model_label(frame),
            Some(String::from("Gemini 3.5 Flash (High)"))
        );
    }

    #[test]
    fn extract_model_label_finds_label_from_shortcuts_footer() {
        let frame = concat!(
            "Some content\n",
            "? for shortcuts                                        Gemini 3.5 Flash (High)\n",
        );
        assert_eq!(
            extract_model_label(frame),
            Some(String::from("Gemini 3.5 Flash (High)"))
        );
    }

    #[test]
    fn extract_model_label_returns_none_when_no_footer_in_window() {
        // No esc-to-cancel or shortcuts footer at all.
        let frame = "Some content\nMore content\nUnrelated last line\n";
        assert_eq!(extract_model_label(frame), None);
    }

    #[test]
    fn extract_model_label_ignores_spinner_line_with_gemini() {
        // A spinner line that contains "Gemini" but not the proper footer prefix
        // and two-space gutter should not be captured.
        let frame = "⣾ Working... Gemini 3.5 Flash (High)\n";
        assert_eq!(extract_model_label(frame), None);
    }

    #[test]
    fn extract_model_label_rejects_control_characters() {
        // Label contains a control character — must be rejected.
        let frame = "esc to cancel                                          Gemini 3.5 \x07Flash\n";
        assert_eq!(extract_model_label(frame), None);
    }

    #[test]
    fn extract_model_label_rejects_overlong_label() {
        // Build a label that exceeds MODEL_LABEL_MAX_WIDTH (64 display columns).
        let long_label = "Gemini ".to_string() + &"X".repeat(60);
        let frame = format!("esc to cancel                            {long_label}\n");
        // Verify the frame satisfies the delimiter condition first.
        assert!(frame.contains("  Gemini "));
        assert_eq!(extract_model_label(&frame), None);
    }

    #[test]
    fn extract_model_label_strips_ansi_then_parses() {
        // ANSI color codes surround the footer; after stripping, label must be found.
        let frame = "\x1b[32mesc to cancel\x1b[0m                          \x1b[1mGemini 3.5 Flash (High)\x1b[0m\n";
        assert_eq!(
            extract_model_label(frame),
            Some(String::from("Gemini 3.5 Flash (High)"))
        );
    }

    #[test]
    fn extract_model_label_picks_bottom_most_when_multiple_footers() {
        // Two footer lines in the tail window with the *same* prefix but
        // different labels. The bottom-most wins — an implementation that
        // just preferred one prefix type over the other would also have to
        // produce the correct result here.
        let frame = concat!(
            "esc to cancel                                          Gemini 2.0 Flash\n",
            "Some intermediate non-empty line\n",
            "esc to cancel                                          Gemini 3.5 Flash (High)\n",
        );
        assert_eq!(
            extract_model_label(frame),
            Some(String::from("Gemini 3.5 Flash (High)"))
        );
    }

    #[test]
    fn extract_model_label_picks_bottom_most_when_both_shortcuts() {
        // Symmetric mirror of the above: both lines use `? for shortcuts`.
        let frame = concat!(
            "? for shortcuts                                        Gemini 2.0 Flash\n",
            "Some intermediate non-empty line\n",
            "? for shortcuts                                        Gemini 3.5 Flash (High)\n",
        );
        assert_eq!(
            extract_model_label(frame),
            Some(String::from("Gemini 3.5 Flash (High)"))
        );
    }

    #[test]
    fn extract_model_label_picks_rightmost_gemini_when_multiple_in_one_line() {
        // V-1: rfind semantics. The line contains MODEL_LABEL_GUTTER twice;
        // the rightmost occurrence is the actual model label.
        let frame = "esc to cancel  Gemini API key prefix text    Gemini 3.5 Flash (High)\n";
        assert_eq!(
            extract_model_label(frame),
            Some(String::from("Gemini 3.5 Flash (High)"))
        );
    }

    #[test]
    fn extract_model_label_rejects_incomplete_capture_with_empty_suffix() {
        // CodeRabbit follow-up: a footer that ends mid-render at `...  Gemini ` (or
        // `...  Gemini` with trailing whitespace) must NOT cache `Gemini` as the
        // sticky model label. The label must contain at least one non-whitespace
        // character after the `Gemini ` brand.
        let frame_just_gutter = "esc to cancel                                          Gemini \n";
        assert_eq!(extract_model_label(frame_just_gutter), None);
        let frame_trimmed_brand = "esc to cancel                                          Gemini\n";
        assert_eq!(extract_model_label(frame_trimmed_brand), None);
        let frame_only_whitespace_suffix =
            "esc to cancel                                          Gemini    \n";
        assert_eq!(extract_model_label(frame_only_whitespace_suffix), None);
    }

    #[test]
    fn extract_model_label_handles_cr_line_endings() {
        // V-2: a \r embedded before the \n is stripped before delimiter search.
        // Without stripping, the label would contain \r which is a control char
        // and the whole line would be rejected.
        let frame =
            "esc to cancel                                          Gemini 3.5 Flash (High)\r\n";
        assert_eq!(
            extract_model_label(frame),
            Some(String::from("Gemini 3.5 Flash (High)"))
        );
    }

    #[test]
    fn extract_model_label_rejects_control_char_in_prefix() {
        // V-13: a control character in the prefix (before the gutter) disqualifies
        // the whole line, not just the label slice.
        let frame = "esc to cancel\x07                                  Gemini 3.5 Flash (High)\n";
        assert_eq!(extract_model_label(frame), None);
    }

    #[test]
    fn extract_model_label_rejects_footer_beyond_tail_window() {
        // V-15: a valid footer placed more than MODEL_LABEL_TAIL_WINDOW (6)
        // non-empty lines from the bottom is outside the scan window → None.
        let valid_footer =
            "esc to cancel                                          Gemini 3.5 Flash (High)\n";
        // 7 non-empty trailing lines push the footer out of the 6-line window.
        let trailing: String = (0..MODEL_LABEL_TAIL_WINDOW + 1)
            .map(|i| format!("trailing non-empty line {i}\n"))
            .collect();
        let frame = format!("{valid_footer}{trailing}");
        assert_eq!(extract_model_label(&frame), None);
    }
}
