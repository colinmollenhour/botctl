#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionState {
    ChatReady,
    BusyResponding,
    PermissionDialog,
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
pub const SIGNAL_CHAT_KEYWORDS: &str = "chat-keywords";
pub const SIGNAL_AMBIGUOUS_PERMISSION_CHAT: &str = "ambiguous-permission-chat";
pub const SIGNAL_FOLDER_TRUST_KEYWORDS: &str = "folder-trust-keywords";
pub const SIGNAL_SURVEY_KEYWORDS: &str = "survey-keywords";
pub const SIGNAL_EXTERNAL_EDITOR_KEYWORDS: &str = "external-editor-keywords";
pub const SIGNAL_DIFF_KEYWORDS: &str = "diff-keywords";
pub const SIGNAL_BUSY_KEYWORDS: &str = "busy-keywords";
pub const SIGNAL_SELF_SETTINGS_LANGUAGE: &str = "self-settings-language";
pub const SIGNAL_SENSITIVE_CLAUDE_PATH: &str = "sensitive-claude-path";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Classification {
    pub source: String,
    pub state: SessionState,
    pub recap_present: bool,
    pub recap_excerpt: Option<String>,
    pub signals: Vec<String>,
}

impl Classification {
    pub fn render(&self) -> String {
        let mut out = format!("source={}\nstate={}", self.source, self.state.as_str());
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
        let recap = detect_recap(frame_text, &normalized);
        let mut signals = Vec::new();
        let has_chat_input = contains_any(
            &normalized,
            &["enter submit message", "main chat input area", "chat:"],
        ) || frame_text.lines().map(str::trim).any(is_chat_input_line);
        let has_permission_keywords =
            contains_any(
                &normalized,
                &[
                    "allow once",
                    "allow for session",
                    "permission",
                    "do you want to proceed",
                    "unsandboxed",
                    "tab to amend",
                    "ctrl+e to explain",
                    "confirm action",
                    "approve",
                ],
            ) && contains_any(&normalized, &["yes", "no", "enter", "escape"]);
        let mentions_self_settings_language = contains_any(
            &normalized,
            &[
                "edit its own settings",
                "allow claude to edit its own settings",
            ],
        );
        let mentions_sensitive_claude_path = contains_sensitive_claude_path(&normalized);

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
        } else if has_permission_keywords && has_chat_input {
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
        } else if contains_any(
            &normalized,
            &[
                "diff",
                "review changes",
                "accept",
                "reject",
                "view details",
                "keep changes",
                "discard changes",
            ],
        ) {
            signals.push(String::from(SIGNAL_DIFF_KEYWORDS));
            SessionState::DiffDialog
        } else if contains_any(
            &normalized,
            &[
                "esc to interrupt",
                "ctrl+c to interrupt",
                "thinking",
                "running",
                "background task",
                "still thinking",
                "working",
            ],
        ) {
            signals.push(String::from(SIGNAL_BUSY_KEYWORDS));
            SessionState::BusyResponding
        } else if has_chat_input || contains_any(&normalized, &["claude"]) {
            signals.push(String::from(SIGNAL_CHAT_KEYWORDS));
            SessionState::ChatReady
        } else {
            SessionState::Unknown
        };

        Classification {
            source: source.to_string(),
            state,
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
            "/.claude/commands/",
            "/.claude/settings",
            "~/.claude/commands/",
            "~/.claude/settings",
            ".claude/commands/",
            ".claude/settings",
        ],
    )
}

fn is_chat_input_line(line: &str) -> bool {
    let trimmed = line.trim();
    if trimmed == ">" || trimmed == "❯" {
        return true;
    }

    let Some(rest) = trimmed.strip_prefix('❯') else {
        return false;
    };
    let rest = rest.trim();
    !rest.is_empty() && !starts_with_numbered_option(rest)
}

fn starts_with_numbered_option(line: &str) -> bool {
    let digits = line.chars().take_while(|ch| ch.is_ascii_digit()).count();
    digits > 0 && line[digits..].starts_with('.')
}

#[cfg(any(test, rust_analyzer))]
mod tests {
    use super::{
        Classifier, SIGNAL_SELF_SETTINGS_LANGUAGE, SIGNAL_SENSITIVE_CLAUDE_PATH, SessionState,
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
    fn adds_sensitive_claude_path_signal_for_project_slash_command_paths() {
        let frame = "Do you want to make this edit to /repo/.claude/commands/commit-and-push.md?\n❯ 1. Yes\n2. No\nEsc to cancel · Tab to amend";
        let result = Classifier.classify("test", frame);

        assert_eq!(result.state, SessionState::PermissionDialog);
        assert!(
            result
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
}
