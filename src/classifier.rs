#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionState {
    ChatReady,
    BusyResponding,
    PermissionDialog,
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
                "allow once",
                "allow for session",
                "permission",
                "confirm action",
                "confirm",
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
            ],
        ) {
            signals.push(String::from("survey-keywords"));
            SessionState::SurveyPrompt
        } else if contains_any(
            &normalized,
            &[
                "external editor",
                "waiting for editor",
                "close the editor to continue",
            ],
        ) {
            signals.push(String::from("external-editor-keywords"));
            SessionState::ExternalEditorActive
        } else if contains_any(&normalized, &["diff", "accept", "reject", "view details"]) {
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
    fn classifies_survey_prompt() {
        let frame = "How likely are you to recommend Claude Code to a friend?";
        let result = Classifier.classify("test", frame);
        assert_eq!(result.state, SessionState::SurveyPrompt);
    }
}
