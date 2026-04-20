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
    pub signals: Vec<String>,
}

impl Classification {
    pub fn render(&self) -> String {
        let mut out = format!("source={}\nstate={}", self.source, self.state.as_str());
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
            signals,
        }
    }
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
}
