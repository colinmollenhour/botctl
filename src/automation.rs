#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutomationAction {
    ClearInput,
    ExternalEditor,
    Submit,
    Interrupt,
    ConfirmPrevious,
    ConfirmNext,
    ConfirmYes,
    ConfirmNo,
}

impl AutomationAction {
    pub fn from_str(value: &str) -> Option<Self> {
        match value.trim() {
            "clear-input" => Some(Self::ClearInput),
            "external-editor" => Some(Self::ExternalEditor),
            "submit" => Some(Self::Submit),
            "interrupt" => Some(Self::Interrupt),
            "confirm-previous" => Some(Self::ConfirmPrevious),
            "confirm-next" => Some(Self::ConfirmNext),
            "confirm-yes" => Some(Self::ConfirmYes),
            "confirm-no" => Some(Self::ConfirmNo),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::ClearInput => "clear-input",
            Self::ExternalEditor => "external-editor",
            Self::Submit => "submit",
            Self::Interrupt => "interrupt",
            Self::ConfirmPrevious => "confirm-previous",
            Self::ConfirmNext => "confirm-next",
            Self::ConfirmYes => "confirm-yes",
            Self::ConfirmNo => "confirm-no",
        }
    }

    pub fn tmux_keys(self) -> &'static [&'static str] {
        match self {
            Self::ClearInput => &["F6"],
            Self::ExternalEditor => &["F7"],
            Self::Submit => &["F8"],
            Self::Interrupt => &["F10"],
            Self::ConfirmPrevious => &["F6"],
            Self::ConfirmNext => &["F7"],
            Self::ConfirmYes => &["F8"],
            Self::ConfirmNo => &["F9"],
        }
    }
}

pub fn prompt_submission_sequence() -> [AutomationAction; 3] {
    [
        AutomationAction::ClearInput,
        AutomationAction::ExternalEditor,
        AutomationAction::Submit,
    ]
}

pub fn render_keybindings_json() -> String {
    String::from(
        "{\n\
         \t\"$schema\": \"https://www.schemastore.org/claude-code-keybindings.json\",\n\
         \t\"$docs\": \"https://code.claude.com/docs/en/keybindings\",\n\
         \t\"bindings\": [\n\
         \t\t{\n\
         \t\t\t\"context\": \"Global\",\n\
         \t\t\t\"bindings\": {\n\
         \t\t\t\t\"f10\": \"app:interrupt\"\n\
         \t\t\t}\n\
         \t\t},\n\
         \t\t{\n\
         \t\t\t\"context\": \"Chat\",\n\
         \t\t\t\"bindings\": {\n\
         \t\t\t\t\"f6\": \"chat:clearInput\",\n\
         \t\t\t\t\"f7\": \"chat:externalEditor\",\n\
         \t\t\t\t\"f8\": \"chat:submit\"\n\
         \t\t\t}\n\
         \t\t},\n\
         \t\t{\n\
         \t\t\t\"context\": \"Confirmation\",\n\
         \t\t\t\"bindings\": {\n\
         \t\t\t\t\"f6\": \"confirm:previous\",\n\
         \t\t\t\t\"f7\": \"confirm:next\",\n\
         \t\t\t\t\"f8\": \"confirm:yes\",\n\
         \t\t\t\t\"f9\": \"confirm:no\"\n\
         \t\t\t}\n\
         \t\t}\n\
         \t]\n\
         }\n",
    )
}

#[cfg(test)]
mod tests {
    use super::{AutomationAction, prompt_submission_sequence, render_keybindings_json};

    #[test]
    fn action_lookup_round_trips() {
        let action = AutomationAction::from_str("confirm-yes").expect("action should parse");
        assert_eq!(action.as_str(), "confirm-yes");
        assert_eq!(action.tmux_keys(), &["F8"]);
    }

    #[test]
    fn keybindings_json_contains_expected_actions() {
        let json = render_keybindings_json();
        assert!(json.contains("\"f7\": \"chat:externalEditor\""));
        assert!(json.contains("\"f9\": \"confirm:no\""));
        assert!(json.contains("\"f10\": \"app:interrupt\""));
    }

    #[test]
    fn prompt_submission_sequence_is_stable() {
        assert_eq!(
            prompt_submission_sequence(),
            [
                AutomationAction::ClearInput,
                AutomationAction::ExternalEditor,
                AutomationAction::Submit,
            ]
        );
    }
}
