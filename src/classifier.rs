#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionState {
    ChatReady,
    BusyResponding,
    PermissionDialog,
    PlanApprovalPrompt,
    FolderTrustPrompt,
    SurveyPrompt,
    ExternalEditorActive,
    DiffDialog,
    Unknown,
}

impl SessionState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ChatReady => "ChatReady",
            Self::BusyResponding => "BusyResponding",
            Self::PermissionDialog => "PermissionDialog",
            Self::PlanApprovalPrompt => "PlanApprovalPrompt",
            Self::FolderTrustPrompt => "FolderTrustPrompt",
            Self::SurveyPrompt => "SurveyPrompt",
            Self::ExternalEditorActive => "ExternalEditorActive",
            Self::DiffDialog => "DiffDialog",
            Self::Unknown => "Unknown",
        }
    }

    pub fn from_str(value: &str) -> Option<Self> {
        match value.trim() {
            "ChatReady" => Some(Self::ChatReady),
            "BusyResponding" => Some(Self::BusyResponding),
            "PermissionDialog" => Some(Self::PermissionDialog),
            "PlanApprovalPrompt" => Some(Self::PlanApprovalPrompt),
            "FolderTrustPrompt" => Some(Self::FolderTrustPrompt),
            "SurveyPrompt" => Some(Self::SurveyPrompt),
            "ExternalEditorActive" => Some(Self::ExternalEditorActive),
            "DiffDialog" => Some(Self::DiffDialog),
            "Unknown" => Some(Self::Unknown),
            _ => None,
        }
    }
}

pub const SIGNAL_PERMISSION_KEYWORDS: &str = "permission-keywords";
pub const SIGNAL_PLAN_APPROVAL_KEYWORDS: &str = "plan-approval-keywords";
pub const SIGNAL_CHAT_KEYWORDS: &str = "chat-keywords";
pub const SIGNAL_AMBIGUOUS_PERMISSION_CHAT: &str = "ambiguous-permission-chat";
pub const SIGNAL_FOLDER_TRUST_KEYWORDS: &str = "folder-trust-keywords";
pub const SIGNAL_SURVEY_KEYWORDS: &str = "survey-keywords";
pub const SIGNAL_EXTERNAL_EDITOR_KEYWORDS: &str = "external-editor-keywords";
pub const SIGNAL_DIFF_KEYWORDS: &str = "diff-keywords";
pub const SIGNAL_BUSY_KEYWORDS: &str = "busy-keywords";
pub const SIGNAL_CHAT_QUESTIONS: &str = "chat-questions";
pub const SIGNAL_SELF_SETTINGS_LANGUAGE: &str = "self-settings-language";
pub const SIGNAL_SENSITIVE_CLAUDE_PATH: &str = "sensitive-claude-path";

const PERMISSION_KEYWORDS: &[&str] = &[
    "allow once",
    "allow for session",
    "permission",
    "do you want to proceed",
    "do you want to allow claude to",
    "claude wants to",
    "don't ask again",
    "unsandboxed",
    "tab to amend",
    "ctrl+e to explain",
    "confirm action",
    "approve",
];

const PERMISSION_CONFIRM_KEYWORDS: &[&str] = &["yes", "no", "enter", "escape", "esc"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Classification {
    pub source: String,
    pub state: SessionState,
    pub has_questions: bool,
    pub recap_present: bool,
    pub recap_excerpt: Option<String>,
    pub signals: Vec<String>,
}

impl Classification {
    pub fn render(&self) -> String {
        let mut out = format!("source={}\nstate={}", self.source, self.state.as_str());
        out.push_str(&format!("\nhas_questions={}", self.has_questions));
        out.push_str(&format!("\nrecap_present={}", self.recap_present));
        out.push_str("\nrecap_excerpt=");
        out.push_str(self.recap_excerpt.as_deref().unwrap_or("none"));
        if self.signals.is_empty() {
            out.push_str("\nsignals=none");
        } else {
            out.push_str("\nsignals=");
            out.push_str(&self.signals.join(", "));
        }
        out
    }
}

#[derive(Debug, Default)]
pub struct Classifier;

impl Classifier {
    pub fn classify(&self, source: &str, frame_text: &str) -> Classification {
        let normalized = normalize(frame_text);
        let lines = frame_text.lines().map(str::trim).collect::<Vec<_>>();
        let recap = detect_recap(frame_text, &normalized);
        let mut signals = Vec::new();
        let has_chat_input = lines.iter().copied().any(is_chat_keyword_line)
            || lines.iter().copied().any(is_chat_input_line);
        let has_permission_keywords = contains_any(&normalized, PERMISSION_KEYWORDS)
            && contains_any(&normalized, PERMISSION_CONFIRM_KEYWORDS);
        let has_plain_chat_prompt_after_permission =
            has_permission_keywords && has_plain_chat_prompt_after_permission(&lines);
        let has_conflicting_chat_after_permission =
            has_permission_keywords && has_chat_indicators_after_permission(&lines);
        let has_plan_approval_keywords = contains_any(
            &normalized,
            &[
                "claude has written up a plan and is ready to execute",
                "would you like to proceed?",
            ],
        ) && contains_any(
            &normalized,
            &[
                "yes, and use auto mode",
                "yes, manually approve edits",
                "refine with ultraplan",
                "shift+tab to approve with this feedback",
            ],
        );
        let mentions_self_settings_language = contains_any(
            &normalized,
            &[
                "edit its own settings",
                "allow claude to edit its own settings",
            ],
        );
        let mentions_sensitive_claude_path = contains_sensitive_claude_path(&normalized);

        let has_busy_interrupt_hint = contains_any(
            &normalized,
            &[
                "press esc to interrupt",
                "esc to interrupt",
                "ctrl+c to interrupt",
            ],
        );
        let has_busy_keywords = contains_any(
            &normalized,
            &[
                "thinking",
                "running",
                "background task",
                "still thinking",
                "working",
            ],
        );
        let has_busy_status_banner = lines.iter().copied().any(is_busy_status_line);

        let state = if contains_any(
            &normalized,
            &[
                "quick safety check",
                "yes, i trust this folder",
                "i trust this folder",
                "trust this folder",
                "accessing workspace:",
                "security guide",
                "this folder",
            ],
        ) && contains_any(&normalized, &["enter to confirm", "esc to cancel"])
        {
            signals.push(String::from(SIGNAL_FOLDER_TRUST_KEYWORDS));
            SessionState::FolderTrustPrompt
        } else if has_plan_approval_keywords {
            signals.push(String::from(SIGNAL_PLAN_APPROVAL_KEYWORDS));
            SessionState::PlanApprovalPrompt
        } else if has_permission_keywords && has_plain_chat_prompt_after_permission {
            signals.push(String::from(SIGNAL_CHAT_KEYWORDS));
            SessionState::ChatReady
        } else if has_permission_keywords && has_conflicting_chat_after_permission {
            signals.push(String::from(SIGNAL_PERMISSION_KEYWORDS));
            if mentions_self_settings_language {
                signals.push(String::from(SIGNAL_SELF_SETTINGS_LANGUAGE));
            }
            if mentions_sensitive_claude_path {
                signals.push(String::from(SIGNAL_SENSITIVE_CLAUDE_PATH));
            }
            signals.push(String::from(SIGNAL_CHAT_KEYWORDS));
            signals.push(String::from(SIGNAL_AMBIGUOUS_PERMISSION_CHAT));
            SessionState::Unknown
        } else if has_permission_keywords {
            signals.push(String::from(SIGNAL_PERMISSION_KEYWORDS));
            if mentions_self_settings_language {
                signals.push(String::from(SIGNAL_SELF_SETTINGS_LANGUAGE));
            }
            if mentions_sensitive_claude_path {
                signals.push(String::from(SIGNAL_SENSITIVE_CLAUDE_PATH));
            }
            SessionState::PermissionDialog
        } else if contains_any(
            &normalized,
            &[
                "how likely are you to recommend claude code",
                "how is claude doing this session",
                "rate your experience",
                "take our survey",
                "survey",
                "rate this conversation",
            ],
        ) {
            signals.push(String::from(SIGNAL_SURVEY_KEYWORDS));
            SessionState::SurveyPrompt
        } else if contains_any(
            &normalized,
            &[
                "external editor",
                "open in your editor",
                "waiting for editor",
                "close the editor to continue",
                "editor to continue",
            ],
        ) {
            signals.push(String::from(SIGNAL_EXTERNAL_EDITOR_KEYWORDS));
            SessionState::ExternalEditorActive
        } else if has_diff_dialog_keywords(&normalized) {
            signals.push(String::from(SIGNAL_DIFF_KEYWORDS));
            SessionState::DiffDialog
        } else if has_busy_status_banner || (has_busy_interrupt_hint && has_busy_keywords) {
            signals.push(String::from(SIGNAL_BUSY_KEYWORDS));
            SessionState::BusyResponding
        } else if has_chat_input || contains_any(&normalized, &["claude"]) {
            signals.push(String::from(SIGNAL_CHAT_KEYWORDS));
            SessionState::ChatReady
        } else {
            SessionState::Unknown
        };

        let has_questions = state == SessionState::ChatReady && chat_ready_has_questions(frame_text);
        if has_questions {
            signals.push(String::from(SIGNAL_CHAT_QUESTIONS));
        }

        Classification {
            source: source.to_string(),
            state,
            has_questions,
            recap_present: recap.recap_present,
            recap_excerpt: recap.recap_excerpt,
            signals,
        }
    }
}

#[derive(Debug, Clone)]
struct RecapDetection {
    recap_present: bool,
    recap_excerpt: Option<String>,
}

fn detect_recap(frame_text: &str, normalized: &str) -> RecapDetection {
    let strong_anchor = contains_any(normalized, &["while you were away", "away summary"])
        || frame_text.lines().map(str::trim).any(is_recap_anchor);
    if !strong_anchor {
        return RecapDetection {
            recap_present: false,
            recap_excerpt: None,
        };
    }

    let excerpt = extract_recap_excerpt(frame_text);

    RecapDetection {
        recap_present: true,
        recap_excerpt: excerpt,
    }
}

fn extract_recap_excerpt(frame_text: &str) -> Option<String> {
    let lines = frame_text.lines().map(str::trim).collect::<Vec<_>>();
    let anchor_index = lines.iter().position(|line| is_recap_anchor(line))?;

    if let Some(inline) = inline_recap_excerpt(lines[anchor_index]) {
        return Some(inline);
    }

    let mut summary_lines = Vec::new();
    let mut i = anchor_index;
    while i < lines.len() && is_recap_anchor(lines[i]) {
        i += 1;
    }

    while i < lines.len() && summary_lines.len() < 3 {
        let line = lines[i];
        if line.is_empty() {
            i += 1;
            continue;
        }
        if is_recap_stop(line) {
            break;
        }
        summary_lines.push(line.chars().take(120).collect::<String>());
        i += 1;
    }

    if summary_lines.is_empty() {
        lines.get(anchor_index).map(|line| (*line).to_string())
    } else {
        Some(summary_lines.join(" | "))
    }
}

fn is_recap_anchor(line: &str) -> bool {
    let lower = line.to_lowercase();
    lower.contains("while you were away")
        || lower.contains("away summary")
        || lower.starts_with("※ recap:")
        || lower.starts_with("recap:")
}

fn inline_recap_excerpt(line: &str) -> Option<String> {
    let trimmed = line.trim();
    for prefix in ["※ recap:", "recap:"] {
        if trimmed.len() >= prefix.len() && trimmed[..prefix.len()].eq_ignore_ascii_case(prefix) {
            let excerpt = trimmed[prefix.len()..].trim();
            if excerpt.is_empty() {
                return None;
            }
            return Some(excerpt.chars().take(240).collect::<String>());
        }
    }
    None
}

fn is_recap_stop(line: &str) -> bool {
    let lower = line.to_lowercase();
    contains_any(
        &lower,
        &[
            "main chat input area",
            "enter submit message",
            "chat:",
            "press esc to interrupt",
            "esc to interrupt",
        ],
    ) || line == "Claude"
        || line == ">"
}

fn normalize(input: &str) -> String {
    input
        .lines()
        .map(str::trim)
        .collect::<Vec<_>>()
        .join("\n")
        .to_lowercase()
}

fn contains_any(haystack: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| haystack.contains(needle))
}

fn contains_sensitive_claude_path(normalized: &str) -> bool {
    contains_any(
        normalized,
        &[
            "/.claude/settings",
            "~/.claude/settings",
            ".claude/settings",
        ],
    )
}

fn has_diff_dialog_keywords(normalized: &str) -> bool {
    let has_diff_context = contains_any(normalized, &["review changes", "view details", "diff"]);
    let has_diff_choice = contains_any(
        normalized,
        &["keep changes", "discard changes", "accept", "reject"],
    );

    // Require both the review context and a concrete choice so stale words like
    // "reject" in scrollback do not masquerade as an active diff dialog.
    has_diff_context && has_diff_choice
}

fn has_chat_indicators_after_permission(lines: &[&str]) -> bool {
    let Some(last_permission_anchor) = lines
        .iter()
        .rposition(|line| is_permission_anchor_line(line.trim()))
    else {
        return lines.iter().copied().any(is_chat_keyword_line)
            || lines.iter().copied().any(is_chat_input_line);
    };

    lines
        .iter()
        .skip(last_permission_anchor + 1)
        .copied()
        .any(|line| is_chat_keyword_line(line) || is_chat_input_line(line))
}

fn has_plain_chat_prompt_after_permission(lines: &[&str]) -> bool {
    let Some(last_permission_anchor) = lines
        .iter()
        .rposition(|line| is_permission_anchor_line(line.trim()))
    else {
        return false;
    };

    lines
        .iter()
        .skip(last_permission_anchor + 1)
        .copied()
        .any(is_plain_chat_input_line)
}

fn is_chat_keyword_line(line: &str) -> bool {
    contains_any(
        &line.to_ascii_lowercase(),
        &["enter submit message", "main chat input area", "chat:"],
    )
}

fn is_permission_anchor_line(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    contains_any(
        &lower,
        PERMISSION_KEYWORDS,
    ) || is_permission_choice_line(line)
        || contains_any(
            &lower,
            &[
                "enter confirms",
                "enter to confirm",
                "esc to cancel",
                "escape to cancel",
            ],
        )
}

fn is_permission_choice_line(line: &str) -> bool {
    let trimmed = line.trim().trim_start_matches('❯').trim();
    (trimmed.starts_with("1.") || trimmed.starts_with("2.") || trimmed.starts_with("3."))
        && contains_any(
            &trimmed.to_ascii_lowercase(),
            &["yes", "no", "allow once", "allow for session"],
        )
}

fn is_chat_input_line(line: &str) -> bool {
    let trimmed = line.trim();
    if is_plain_chat_input_line(trimmed) {
        return true;
    }

    let Some(rest) = trimmed.strip_prefix('❯') else {
        return false;
    };
    let rest = rest.trim();
    !rest.is_empty() && !starts_with_numbered_option(rest)
}

fn is_plain_chat_input_line(line: &str) -> bool {
    matches!(line.trim(), ">" | "❯")
}

fn is_busy_status_line(line: &str) -> bool {
    let trimmed = line.trim();
    let lower = trimmed.to_ascii_lowercase();
    let stripped = lower.trim_start_matches(|ch: char| !ch.is_ascii_alphanumeric());
    stripped.starts_with("thinking...") || stripped.starts_with("thinking…")
}

fn chat_ready_has_questions(frame_text: &str) -> bool {
    let tail = recent_chat_ready_tail(frame_text);
    if tail.is_empty() {
        return false;
    }

    let joined = tail.join("\n").to_ascii_lowercase();
    let has_question_line = tail.iter().any(|line| line.ends_with('?'));
    let has_question_phrase = contains_any(
        &joined,
        &[
            "should i",
            "would you like",
            "do you want",
            "want me to",
            "how would you like",
            "which option",
            "which approach",
            "do you prefer",
            "let me know which",
        ],
    );
    let option_like_lines = tail.iter().filter(|line| is_lettered_option_line(line)).count();

    has_question_line || has_question_phrase || option_like_lines >= 2
}

fn recent_chat_ready_tail(frame_text: &str) -> Vec<String> {
    let lines = frame_text.lines().map(str::trim).collect::<Vec<_>>();
    if lines.is_empty() {
        return Vec::new();
    }

    let tail_end = lines
        .iter()
        .rposition(|line| !line.is_empty() && !is_terminal_status_line(line))
        .map(|idx| idx + 1)
        .unwrap_or(0);
    let lines = &lines[..tail_end];

    let start = lines
        .iter()
        .rposition(|line| is_plain_chat_input_line(line))
        .map(|idx| idx.saturating_sub(8))
        .unwrap_or_else(|| lines.len().saturating_sub(8));

    let mut tail = lines[start..]
        .iter()
        .copied()
        .filter(|line| !line.is_empty())
        .filter(|line| !is_plain_chat_input_line(line))
        .filter(|line| !is_chat_keyword_line(line))
        .filter(|line| !is_terminal_status_line(line))
        .map(str::to_string)
        .collect::<Vec<_>>();
    if tail.len() > 4 {
        tail = tail.split_off(tail.len() - 4);
    }
    tail
}

fn is_terminal_status_line(line: &str) -> bool {
    line.starts_with('~')
        || line.starts_with('/')
        || line.starts_with('(')
        || line.ends_with(") ✅")
        || line.ends_with(")")
}

fn is_lettered_option_line(line: &str) -> bool {
    let trimmed = line.trim_start_matches('❯').trim();
    let bytes = trimmed.as_bytes();
    if bytes.len() < 3 || !bytes[0].is_ascii_alphabetic() {
        return false;
    }
    matches!(bytes[1], b')' | b'.' | b':') && bytes[2] == b' '
}

fn starts_with_numbered_option(line: &str) -> bool {
    let digits = line.chars().take_while(|ch| ch.is_ascii_digit()).count();
    digits > 0 && line[digits..].starts_with('.')
}

#[cfg(any(test, rust_analyzer))]
mod tests {
    use super::{
        Classifier, SIGNAL_BUSY_KEYWORDS, SIGNAL_CHAT_KEYWORDS, SIGNAL_CHAT_QUESTIONS,
        SIGNAL_DIFF_KEYWORDS, SIGNAL_PERMISSION_KEYWORDS, SIGNAL_PLAN_APPROVAL_KEYWORDS,
        SIGNAL_SELF_SETTINGS_LANGUAGE, SIGNAL_SENSITIVE_CLAUDE_PATH, SessionState,
    };

    #[test]
    fn classifies_permission_dialog() {
        let frame = "Claude Code needs permission\nAllow once\nAllow for session\nEnter confirms";
        let result = Classifier.classify("test", frame);
        assert_eq!(result.state, SessionState::PermissionDialog);
    }

    #[test]
    fn classifies_new_permission_dialog_wording() {
        let frame = "Bash command (unsandboxed)\nDo you want to proceed?\n❯ 1. Yes\n2. No\nEsc to cancel · Tab to amend · ctrl+e to explain";
        let result = Classifier.classify("test", frame);
        assert_eq!(result.state, SessionState::PermissionDialog);
    }

    #[test]
    fn ignores_stale_chat_prompt_before_monitor_permission_dialog() {
        let frame = "❯\n● Monitor(FG ready state)\nMonitor\n  until out=$(kubectl -n bloodraven-playground get mysqlfailovergroup playground -o jsonpath='{.status.activeSite}={.status.ready}'\n  2>/dev/null); [[ \"$out\" =~ ^[a-z]+=true$ ]]; do sleep 5; done; echo \"FG ready: $out\"\n  FG ready state\nUnhandled node type: string\nDo you want to proceed?\n❯ 1. Yes\n2. No\nEsc to cancel · Tab to amend";
        let result = Classifier.classify("test", frame);
        assert_eq!(result.state, SessionState::PermissionDialog);
        assert!(
            !result
                .signals
                .contains(&String::from("ambiguous-permission-chat"))
        );
    }

    #[test]
    fn classifies_fetch_allow_dialog_as_permission() {
        let frame = "Fetch\n\nhttps://docus.dev/raw/en/getting-started/studio.md\nClaude wants to fetch content from docus.dev\n\nDo you want to allow Claude to fetch this content?\n❯ 1. Yes\n2. Yes, and don't ask again for docus.dev\n3. No, and tell Claude what to do differently (esc)";
        let result = Classifier.classify("test", frame);

        assert_eq!(result.state, SessionState::PermissionDialog);
        assert!(
            result
                .signals
                .contains(&String::from(SIGNAL_PERMISSION_KEYWORDS))
        );
    }

    #[test]
    fn classifies_plan_approval_prompt() {
        let frame = "Claude has written up a plan and is ready to execute. Would you like to proceed?\n❯ 1. Yes, and use auto mode\n2. Yes, manually approve edits\n3. No, refine with Ultraplan on Claude Code on the web\n4. Tell Claude what to change\nshift+tab to approve with this feedback\nctrl-g to edit in Vim";
        let result = Classifier.classify("test", frame);

        assert_eq!(result.state, SessionState::PlanApprovalPrompt);
        assert!(
            result
                .signals
                .contains(&String::from(SIGNAL_PLAN_APPROVAL_KEYWORDS))
        );
    }

    #[test]
    fn refuses_permission_dialog_when_chat_input_is_active() {
        let frame = "Bash command (unsandboxed)\nDo you want to proceed?\n❯ 1. Yes\n2. No\nEsc to cancel · Tab to amend · ctrl+e to explain\n❯ Here's some of the output:";
        let result = Classifier.classify("test", frame);
        assert_eq!(result.state, SessionState::Unknown);
        assert!(
            result
                .signals
                .contains(&String::from("ambiguous-permission-chat"))
        );
    }

    #[test]
    fn treats_stale_permission_prompt_above_plain_chat_prompt_as_chat_ready() {
        let frame = "Bash command (unsandboxed)\nDo you want to proceed?\n❯ 1. Yes\n2. No\nEsc to cancel · Tab to amend · ctrl+e to explain\n● Build confirmed the pre-existing broken links in multi-site.mdx\n※ recap: Shipping WISHLIST #24\n❯\n~/Projects/shipstream/bloodraven/docs (wishlist/upgrade-policy)";
        let result = Classifier.classify("test", frame);

        assert_eq!(result.state, SessionState::ChatReady);
        assert!(result.signals.contains(&String::from(SIGNAL_CHAT_KEYWORDS)));
        assert!(!result.signals.contains(&String::from(SIGNAL_PERMISSION_KEYWORDS)));
        assert!(result.recap_present);
    }

    #[test]
    fn adds_self_settings_signal_for_permission_dialog() {
        let frame = "Do you want to make this edit to commit-and-push.md?\n❯ 1. Yes\n2. Yes, and allow Claude to edit its own settings for this session\n3. No\nEsc to cancel · Tab to amend";
        let result = Classifier.classify("test", frame);

        assert_eq!(result.state, SessionState::PermissionDialog);
        assert!(
            result
                .signals
                .contains(&String::from(SIGNAL_SELF_SETTINGS_LANGUAGE))
        );
    }

    #[test]
    fn adds_sensitive_claude_path_signal_for_claude_settings_paths() {
        let frame = "Do you want to make this edit to /repo/.claude/settings.json?\n❯ 1. Yes\n2. No\nEsc to cancel · Tab to amend";
        let result = Classifier.classify("test", frame);

        assert_eq!(result.state, SessionState::PermissionDialog);
        assert!(
            result
                .signals
                .contains(&String::from(SIGNAL_SENSITIVE_CLAUDE_PATH))
        );
    }

    #[test]
    fn does_not_flag_project_claude_command_paths_as_sensitive() {
        let frame = "Do you want to make this edit to /repo/.claude/commands/commit-and-push.md?\n❯ 1. Yes\n2. No\nEsc to cancel · Tab to amend";
        let result = Classifier.classify("test", frame);

        assert_eq!(result.state, SessionState::PermissionDialog);
        assert!(
            !result
                .signals
                .contains(&String::from(SIGNAL_SENSITIVE_CLAUDE_PATH))
        );
    }

    #[test]
    fn classifies_diff_dialog() {
        let frame = "Review changes\nKeep changes\nDiscard changes\nView details";
        let result = Classifier.classify("test", frame);
        assert_eq!(result.state, SessionState::DiffDialog);
    }

    #[test]
    fn ignores_stale_reject_word_in_chat_scrollback() {
        let frame = "Want me to commit it? Once committed, run ./shell/scripts/sync-docs.sh push and it should take a handful of seconds and succeed.\nfix: sync-docs.sh push rewrite to avoid non-fast-forward reject\n● Committed as 37800bb8bd.\n※ recap: Goal was to sync DEV-2958 docs to the knowledge-base repo.\n❯\n~/Projects/shipstream/wms (master)";
        let result = Classifier.classify("test", frame);

        assert_eq!(result.state, SessionState::ChatReady);
        assert!(result.signals.contains(&String::from(SIGNAL_CHAT_KEYWORDS)));
        assert!(!result.signals.contains(&String::from(SIGNAL_DIFF_KEYWORDS)));
        assert!(result.recap_present);
    }

    #[test]
    fn classifies_folder_trust_prompt() {
        let frame = "Accessing workspace:\n/home/colin/Projects/botctl\nQuick safety check: Is this a project you created or one you trust?\nSecurity guide\n1. Yes, I trust this folder\n2. No, exit\nEnter to confirm · Esc to cancel";
        let result = Classifier.classify("test", frame);
        assert_eq!(result.state, SessionState::FolderTrustPrompt);
    }

    #[test]
    fn classifies_survey_prompt() {
        let frame = "How likely are you to recommend Claude Code to a friend?";
        let result = Classifier.classify("test", frame);
        assert_eq!(result.state, SessionState::SurveyPrompt);
    }

    #[test]
    fn classifies_session_feedback_prompt_as_survey() {
        let frame = "How is Claude doing this session? (optional)\n1: Bad    2: Fine   3: Good   0: Dismiss";
        let result = Classifier.classify("test", frame);
        assert_eq!(result.state, SessionState::SurveyPrompt);
    }

    #[test]
    fn classifies_busy_responding() {
        let frame = "Still thinking\nPress Esc to interrupt";
        let result = Classifier.classify("test", frame);
        assert_eq!(result.state, SessionState::BusyResponding);
    }

    #[test]
    fn recap_working_line_does_not_trigger_busy_without_interrupt_hint() {
        let frame = "※ recap: Working DEV-2812 Cypress parallelization MR !2505; just pushed a fix and posted an MR note.\n❯";
        let result = Classifier.classify("test", frame);
        assert_eq!(result.state, SessionState::ChatReady);
        assert!(result.recap_present);
    }

    #[test]
    fn classifies_live_thinking_banner_as_busy_even_with_chat_prompt_visible() {
        let frame = "✻ Thinking… (57s · ↓ 3.3k tokens)\n❯\n~/Projects/shipstream/bloodraven (wishlist/upgrade-policy) ✅";
        let result = Classifier.classify("test", frame);
        assert_eq!(result.state, SessionState::BusyResponding);
        assert!(result.signals.contains(&String::from(SIGNAL_BUSY_KEYWORDS)));
    }

    #[test]
    fn classifies_external_editor_active() {
        let frame = "Open in your editor\nClose the editor to continue";
        let result = Classifier.classify("test", frame);
        assert_eq!(result.state, SessionState::ExternalEditorActive);
    }

    #[test]
    fn detects_recap_only_with_strong_anchors() {
        let frame = "While you were away\nSummarized changes";
        let result = Classifier.classify("test", frame);
        assert!(result.recap_present);
        assert_eq!(result.recap_excerpt.as_deref(), Some("Summarized changes"));
    }

    #[test]
    fn ignores_recap_command_mention_alone() {
        let frame = "/recap\nEnter submit message\nClaude";
        let result = Classifier.classify("test", frame);
        assert!(!result.recap_present);
    }

    #[test]
    fn detects_inline_recap_banner() {
        let frame = "※ recap: Goal: DEV-2812 Step 1 landed on MR !2505. Next: watch the next pipeline run.\n❯";
        let result = Classifier.classify("test", frame);
        assert!(result.recap_present);
        assert_eq!(
            result.recap_excerpt.as_deref(),
            Some("Goal: DEV-2812 Step 1 landed on MR !2505. Next: watch the next pipeline run.")
        );
    }

    #[test]
    fn renders_recap_metadata() {
        let result = Classifier.classify("test", "While you were away\nFixed parser edge cases");
        let rendered = result.render();
        assert!(rendered.contains("recap_present=true"));
        assert!(rendered.contains("recap_excerpt=Fixed parser edge cases"));
    }

    #[test]
    fn recap_can_coexist_with_chat_ready() {
        let frame = "While you were away\nFixed parser edge cases\nMain chat input area\nEnter submit message";
        let result = Classifier.classify("test", frame);
        assert_eq!(result.state, SessionState::ChatReady);
        assert!(result.recap_present);
        assert_eq!(
            result.recap_excerpt.as_deref(),
            Some("Fixed parser edge cases")
        );
    }

    #[test]
    fn flags_chat_ready_with_direct_question() {
        let frame = "I fixed the bug and added coverage. Should I also update the docs?\n❯\n~/Projects/botctl (main)";
        let result = Classifier.classify("test", frame);

        assert_eq!(result.state, SessionState::ChatReady);
        assert!(result.has_questions);
        assert!(result.signals.contains(&String::from(SIGNAL_CHAT_QUESTIONS)));
    }

    #[test]
    fn flags_chat_ready_with_lettered_options() {
        let frame = "I can take either path:\nA. Keep the current state model and add a modifier\nB. Split done into a separate top-level state\n❯\n~/Projects/botctl (main)";
        let result = Classifier.classify("test", frame);

        assert_eq!(result.state, SessionState::ChatReady);
        assert!(result.has_questions);
    }

    #[test]
    fn ignores_stale_scrollback_question_far_above_prompt() {
        let frame = "Should I update the docs too?\nMore old scrollback\nStill older output\nLatest completed work item\n※ recap: Fixed the parser edge case\n❯\n~/Projects/botctl (main)";
        let result = Classifier.classify("test", frame);

        assert_eq!(result.state, SessionState::ChatReady);
        assert!(!result.has_questions);
    }
}
