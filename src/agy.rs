//! Antigravity (`agy`) CLI integration.
//!
//! Passive read-only discovery: pane process-name + secondary
//! signal (state-dir-exists or frame fingerprint), conversation
//! id resolution via /proc fd walk first and ~/.gemini/antigravity-cli/history.jsonl
//! workspace match second, pane-scrape last-message extraction
//! with a strict rule-boundary contract.
//!
//! TODO(v2): surface the agy model line ("Gemini 3.5 Flash (High)") in the
//! dashboard detail panel by parsing it from the captured banner once per
//! session and caching. Deferred from v1 because the banner-parser is
//! unverified against the live capture set and the failure mode (showing
//! nothing) is not load-bearing for v1.

use std::borrow::Cow;
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::app::AppResult;
use crate::classifier::SessionState;
use crate::proc_fd::transcript_from_process_tree_fds_with;
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

pub fn resolve_agy_session_for_pane(pane: &TmuxPane, frame: &str) -> AppResult<Option<AgySession>> {
    if !is_agy_pane(pane) {
        return Ok(None);
    }
    if !agy_secondary_signal(frame) {
        return Ok(None);
    }

    let id = if let Some(pid) = pane.pane_pid {
        match conversation_id_from_process_tree_fds(pid)? {
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
) -> AppResult<Option<AgyLastMessage>> {
    let Some(session) = resolve_agy_session_for_pane(pane, frame)? else {
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

    if is_agy_permission_prompt(&lines) {
        return Some(SessionState::Unknown);
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

fn conversation_id_from_process_tree_fds(pid: u32) -> AppResult<Option<String>> {
    let conversations_root = default_state_dir().join("conversations");
    let target = transcript_from_process_tree_fds_with(pid, |path| {
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

fn is_agy_permission_prompt(lines: &[&str]) -> bool {
    let joined = lines.join("\n");

    // Real-world agy command-permission prompt observed in the wild
    // (captured from `tmux capture-pane -t 0:1.1 -p` during megamind testing
    // of this branch):
    //
    //   Command
    //   ─────────────────────────────────────────────────────────────────
    //
    //     Requesting permission for: git remote -v
    //
    //   Do you want to proceed?
    //   > 1. Yes
    //     2. Yes, and always allow in this conversation for commands that start with 'git remote'
    //     3. Yes, and always allow for commands that start with 'git remote' (Persist to settings.json)
    //     4. No
    //
    //     ↑/↓ Navigate · tab Amend · e edit command
    //   esc to cancel                                         Gemini 3.5 Flash (High)
    //
    // Any of the load-bearing tokens below is sufficient to identify the
    // prompt; we require only one because the structure is unique to agy
    // command-permission prompts.
    if joined.contains("Requesting permission for:") && joined.contains("Do you want to proceed?") {
        return true;
    }
    if joined.contains("Do you want to proceed?")
        && joined.contains("> 1. Yes")
        && joined.contains("4. No")
    {
        return true;
    }
    if joined.contains("↑/↓ Navigate · tab Amend") {
        return true;
    }

    // Future / hypothetical signals (folder-trust prompts, allow-once
    // overlays). Kept conservative: returning `Unknown` is always safer
    // than letting the rest of the classifier guess.
    if joined.contains("Trust this workspace?")
        || joined.contains("Trust this folder")
        || joined.contains("Allow once")
        || joined.contains("Allow for session")
        || joined.contains("Awaiting confirmation")
    {
        return true;
    }
    if let Some(last_non_blank) = lines.iter().rev().find(|line| !line.is_empty()) {
        let lower = last_non_blank.to_ascii_lowercase();
        if lower.ends_with("[y/n]") || lower.ends_with("(y/n)") {
            return true;
        }
    }
    false
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
    let (start, end) = match prior_rule {
        Some(prior) => (prior + 1, input_box_top),
        None => return None,
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
    fn classify_agy_permission_prompt_returns_unknown() {
        let frame = concat!(
            "Antigravity CLI 1.0.2\n",
            "Trust this workspace?\n",
            "[y/N]\n",
            "? for shortcuts\n",
        );
        assert_eq!(classify_agy_state(frame), Some(SessionState::Unknown));
    }

    #[test]
    fn classify_real_world_agy_command_permission_prompt_returns_unknown() {
        // Verbatim shape of the command-permission prompt captured live
        // from `tmux capture-pane -t 0:1.1 -p` during megamind testing
        // of this branch (see fixtures/cases/agy_permission_prompt/).
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
        // Must classify as `Unknown` (never auto-act on a permission UI).
        assert_eq!(classify_agy_state(frame), Some(SessionState::Unknown));
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
        let session = resolve_agy_session_for_pane(&pane, frame)
            .expect("resolution should not error")
            .expect("agy pane with fingerprint should yield a session");
        assert!(
            session.id.is_none(),
            "no conversation should resolve when fds and history are absent"
        );

        // And latest_assistant_message_for_pane should return Ok(None) for this case
        // (callers in last_message.rs translate this into the no-session error).
        assert!(
            latest_assistant_message_for_pane(&pane, frame)
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
}
