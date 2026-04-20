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
            signals.push(String::from("folder-trust-keywords"));
            SessionState::FolderTrustPrompt
        } else if contains_any(
            &normalized,
            &[
                "allow once",
                "allow for session",
                "permission",
                "confirm action",
                "approve",
            ],
        ) && contains_any(&normalized, &["yes", "no", "enter", "escape"])
        {
            signals.push(String::from("permission-keywords"));
            SessionState::PermissionDialog
        } else if contains_any(
            &normalized,
            &[
                "how likely are you to recommend claude code",
                "rate your experience",
                "take our survey",
                "survey",
                "rate this conversation",
            ],
        ) {
            signals.push(String::from("survey-keywords"));
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
            signals.push(String::from("external-editor-keywords"));
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
            signals.push(String::from("diff-keywords"));
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
            signals.push(String::from("busy-keywords"));
            SessionState::BusyResponding
        } else if contains_any(
            &normalized,
            &[
                "enter submit message",
                "main chat input area",
                "claude",
                ">",
                "chat:",
            ],
        ) {
            signals.push(String::from("chat-keywords"));
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

#[cfg(test)]
mod tests {
    use super::{Classifier, SessionState};

    #[test]
    fn classifies_permission_dialog() {
        let frame = "Claude Code needs permission\nAllow once\nAllow for session\nEnter confirms";
        let result = Classifier.classify("test", frame);
        assert_eq!(result.state, SessionState::PermissionDialog);
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
