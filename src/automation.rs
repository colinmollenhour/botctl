use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

use crate::classifier::{Classification, SessionState};

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuardedWorkflow {
    SubmitPrompt,
    ApprovePermission,
    RejectPermission,
    DismissSurvey,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeybindingsStatus {
    Valid,
    Missing,
    Invalid,
    Incomplete,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeybindingsInspection {
    pub path: PathBuf,
    pub status: KeybindingsStatus,
    pub missing_bindings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeybindingsInstallReport {
    pub path: PathBuf,
    pub backup_path: Option<PathBuf>,
    pub wrote_file: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedKeybindings {
    pub path: PathBuf,
    bindings: Vec<ResolvedBinding>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ResolvedBinding {
    action: AutomationAction,
    keys: Vec<String>,
}

const SUBMIT_PROMPT_ACTIONS: [AutomationAction; 3] = [
    AutomationAction::ClearInput,
    AutomationAction::ExternalEditor,
    AutomationAction::Submit,
];

const APPROVE_PERMISSION_ACTIONS: [AutomationAction; 1] = [AutomationAction::ConfirmYes];
const REJECT_PERMISSION_ACTIONS: [AutomationAction; 1] = [AutomationAction::ConfirmNo];
const DISMISS_SURVEY_ACTIONS: [AutomationAction; 0] = [];

impl KeybindingsStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Valid => "valid",
            Self::Missing => "missing",
            Self::Invalid => "invalid",
            Self::Incomplete => "incomplete",
        }
    }
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

    fn binding_descriptor(self) -> (&'static str, &'static str) {
        match self {
            Self::ClearInput => ("Chat", "chat:clearInput"),
            Self::ExternalEditor => ("Chat", "chat:externalEditor"),
            Self::Submit => ("Chat", "chat:submit"),
            Self::Interrupt => ("Global", "app:interrupt"),
            Self::ConfirmPrevious => ("Confirmation", "confirm:previous"),
            Self::ConfirmNext => ("Confirmation", "confirm:next"),
            Self::ConfirmYes => ("Confirmation", "confirm:yes"),
            Self::ConfirmNo => ("Confirmation", "confirm:no"),
        }
    }

    fn all() -> [Self; 8] {
        [
            Self::ClearInput,
            Self::ExternalEditor,
            Self::Submit,
            Self::Interrupt,
            Self::ConfirmPrevious,
            Self::ConfirmNext,
            Self::ConfirmYes,
            Self::ConfirmNo,
        ]
    }
}

impl GuardedWorkflow {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SubmitPrompt => "submit-prompt",
            Self::ApprovePermission => "approve",
            Self::RejectPermission => "reject",
            Self::DismissSurvey => "dismiss-survey",
        }
    }

    pub fn required_state(self) -> SessionState {
        match self {
            Self::SubmitPrompt => SessionState::ChatReady,
            Self::ApprovePermission | Self::RejectPermission => SessionState::PermissionDialog,
            Self::DismissSurvey => SessionState::SurveyPrompt,
        }
    }

    pub fn supports_state(self, state: SessionState) -> bool {
        match self {
            Self::ApprovePermission => {
                matches!(
                    state,
                    SessionState::PermissionDialog | SessionState::FolderTrustPrompt
                )
            }
            _ => state == self.required_state(),
        }
    }

    pub fn required_states_description(self) -> &'static str {
        match self {
            Self::ApprovePermission => "PermissionDialog|FolderTrustPrompt",
            _ => self.required_state().as_str(),
        }
    }

    pub fn actions(self) -> &'static [AutomationAction] {
        match self {
            Self::SubmitPrompt => &SUBMIT_PROMPT_ACTIONS,
            Self::ApprovePermission => &APPROVE_PERMISSION_ACTIONS,
            Self::RejectPermission => &REJECT_PERMISSION_ACTIONS,
            Self::DismissSurvey => &DISMISS_SURVEY_ACTIONS,
        }
    }
}

impl ResolvedKeybindings {
    pub fn keys_for(&self, action: AutomationAction) -> Option<&[String]> {
        self.bindings
            .iter()
            .find(|binding| binding.action == action)
            .map(|binding| binding.keys.as_slice())
    }
}

pub fn prompt_submission_sequence() -> [AutomationAction; 3] {
    SUBMIT_PROMPT_ACTIONS
}

pub fn validate_workflow_state(
    workflow: GuardedWorkflow,
    classification: &Classification,
) -> Result<(), String> {
    if workflow.supports_state(classification.state) {
        return Ok(());
    }

    let signals = if classification.signals.is_empty() {
        String::from("none")
    } else {
        classification.signals.join(", ")
    };

    Err(format!(
        "workflow {} requires state={} but pane is {} (signals={signals})",
        workflow.as_str(),
        workflow.required_states_description(),
        classification.state.as_str(),
    ))
}

pub fn inspect_keybindings(path: Option<&Path>) -> Result<KeybindingsInspection, String> {
    let path = resolve_keybindings_path(path)?;
    let content = match fs::read_to_string(&path) {
        Ok(content) => content,
        Err(error) if error.kind() == ErrorKind::NotFound => {
            return Ok(KeybindingsInspection {
                path,
                status: KeybindingsStatus::Missing,
                missing_bindings: AutomationAction::all()
                    .into_iter()
                    .map(|action| action.as_str().to_string())
                    .collect(),
            });
        }
        Err(error) => {
            return Err(format!(
                "failed to read Claude keybindings at {}: {}",
                path.display(),
                error
            ));
        }
    };

    match resolve_bindings_from_content(&path, &content) {
        Ok(resolved) => {
            let mut missing_bindings = Vec::new();
            for action in AutomationAction::all() {
                if resolved.keys_for(action).is_none() {
                    missing_bindings.push(action.as_str().to_string());
                }
            }

            let status = if missing_bindings.is_empty() {
                KeybindingsStatus::Valid
            } else {
                KeybindingsStatus::Incomplete
            };

            Ok(KeybindingsInspection {
                path,
                status,
                missing_bindings,
            })
        }
        Err(error) => Ok(KeybindingsInspection {
            path,
            status: KeybindingsStatus::Invalid,
            missing_bindings: vec![error],
        }),
    }
}

pub fn load_resolved_keybindings(path: Option<&Path>) -> Result<ResolvedKeybindings, String> {
    let path = resolve_keybindings_path(path)?;
    let content = fs::read_to_string(&path).map_err(|error| {
        format!(
            "failed to read Claude keybindings at {}: {}",
            path.display(),
            error
        )
    })?;
    resolve_bindings_from_content(&path, &content)
}

pub fn install_recommended_keybindings(
    path: Option<&Path>,
) -> Result<KeybindingsInstallReport, String> {
    let path = resolve_keybindings_path(path)?;

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| {
            format!(
                "failed to create Claude keybindings directory {}: {}",
                parent.display(),
                error
            )
        })?;
    }

    match fs::read_to_string(&path) {
        Ok(existing) => {
            let merged = merge_required_bindings(&path, &existing)?;
            if merged == existing {
                return Ok(KeybindingsInstallReport {
                    path,
                    backup_path: None,
                    wrote_file: false,
                });
            }

            fs::write(&path, merged).map_err(|error| {
                format!(
                    "failed to update Claude keybindings at {}: {}",
                    path.display(),
                    error
                )
            })?;

            return Ok(KeybindingsInstallReport {
                path,
                backup_path: None,
                wrote_file: true,
            });
        }
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => {
            return Err(format!(
                "failed to read Claude keybindings at {}: {}",
                path.display(),
                error
            ));
        }
    }

    fs::write(&path, render_keybindings_json()).map_err(|error| {
        format!(
            "failed to write Claude keybindings at {}: {}",
            path.display(),
            error
        )
    })?;

    Ok(KeybindingsInstallReport {
        path,
        backup_path: None,
        wrote_file: true,
    })
}

fn merge_required_bindings(path: &Path, existing: &str) -> Result<String, String> {
    let mut parsed: Value = serde_json::from_str(existing).map_err(|error| {
        format!(
            "invalid Claude keybindings JSON at {}: {}",
            path.display(),
            error
        )
    })?;

    let binding_entries = parsed
        .get_mut("bindings")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| {
            format!(
                "Claude keybindings at {} are missing a top-level bindings array",
                path.display()
            )
        })?;

    let desired: Value = serde_json::from_str(&render_keybindings_json()).map_err(|error| {
        format!(
            "internal keybindings template is invalid at {}: {}",
            path.display(),
            error
        )
    })?;
    let desired_entries = desired
        .get("bindings")
        .and_then(Value::as_array)
        .ok_or_else(|| String::from("internal keybindings template is missing bindings"))?;

    for desired_entry in desired_entries {
        let Some(context) = desired_entry.get("context").and_then(Value::as_str) else {
            continue;
        };
        let Some(desired_map) = desired_entry.get("bindings").and_then(Value::as_object) else {
            continue;
        };

        let existing_entry = binding_entries
            .iter_mut()
            .find(|entry| entry.get("context").and_then(Value::as_str) == Some(context));

        let entry = if let Some(entry) = existing_entry {
            entry
        } else {
            binding_entries.push(desired_entry.clone());
            continue;
        };

        let Some(existing_map) = entry.get_mut("bindings").and_then(Value::as_object_mut) else {
            continue;
        };

        for (key, command) in desired_map {
            if existing_map
                .values()
                .any(|existing_command| existing_command == command)
            {
                continue;
            }

            if existing_map.contains_key(key) {
                return Err(format!(
                    "cannot add missing Claude binding {}={} at {} because that key is already used in {} context",
                    key,
                    command.as_str().unwrap_or("<non-string>"),
                    path.display(),
                    context
                ));
            }

            existing_map.insert(key.clone(), command.clone());
        }
    }

    serde_json::to_string_pretty(&parsed).map_err(|error| {
        format!(
            "failed to render merged Claude keybindings at {}: {}",
            path.display(),
            error
        )
    })
}

fn resolve_bindings_from_content(
    path: &Path,
    content: &str,
) -> Result<ResolvedKeybindings, String> {
    let parsed: Value = serde_json::from_str(content).map_err(|error| {
        format!(
            "invalid Claude keybindings JSON at {}: {}",
            path.display(),
            error
        )
    })?;

    let binding_entries = parsed
        .get("bindings")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            format!(
                "Claude keybindings at {} are missing a top-level bindings array",
                path.display()
            )
        })?;

    let mut bindings = Vec::new();
    for action in AutomationAction::all() {
        let (required_context, required_command) = action.binding_descriptor();
        let mut found = None;

        for entry in binding_entries {
            let Some(context) = entry.get("context").and_then(Value::as_str) else {
                continue;
            };
            if context != required_context {
                continue;
            }

            let Some(map) = entry.get("bindings").and_then(Value::as_object) else {
                continue;
            };

            for (key_spec, command) in map {
                if command.as_str() != Some(required_command) {
                    continue;
                }

                found = Some(parse_key_spec(key_spec).map_err(|error| {
                    format!(
                        "unsupported keybinding for {} in {}: {}",
                        action.as_str(),
                        path.display(),
                        error
                    )
                })?);
                break;
            }

            if found.is_some() {
                break;
            }
        }

        if let Some(keys) = found {
            bindings.push(ResolvedBinding { action, keys });
        }
    }

    Ok(ResolvedKeybindings {
        path: path.to_path_buf(),
        bindings,
    })
}

fn parse_key_spec(spec: &str) -> Result<Vec<String>, String> {
    spec.split_whitespace().map(parse_chord).collect()
}

fn parse_chord(chord: &str) -> Result<String, String> {
    let normalized = chord.trim().to_lowercase();
    match normalized.as_str() {
        "enter" => return Ok(String::from("Enter")),
        "escape" => return Ok(String::from("Escape")),
        "up" => return Ok(String::from("Up")),
        "down" => return Ok(String::from("Down")),
        "left" => return Ok(String::from("Left")),
        "right" => return Ok(String::from("Right")),
        "space" => return Ok(String::from("Space")),
        "tab" => return Ok(String::from("Tab")),
        "shift+tab" => return Ok(String::from("BTab")),
        _ => {}
    }

    if let Some(rest) = normalized.strip_prefix('f') {
        if !rest.is_empty() && rest.chars().all(|ch| ch.is_ascii_digit()) {
            return Ok(format!("F{rest}"));
        }
    }

    if normalized.len() == 1 {
        return Ok(normalized);
    }

    if let Some(rest) = normalized.strip_prefix("ctrl+") {
        return parse_modifier_chord("C-", rest);
    }

    if let Some(rest) = normalized.strip_prefix("meta+") {
        return parse_modifier_chord("M-", rest);
    }

    Err(format!("unsupported chord {chord}"))
}

fn parse_modifier_chord(prefix: &str, value: &str) -> Result<String, String> {
    if value.len() == 1 {
        return Ok(format!("{prefix}{value}"));
    }

    match value {
        "enter" => Ok(format!("{prefix}Enter")),
        "space" => Ok(format!("{prefix}Space")),
        _ => Err(format!("unsupported modified chord {prefix}{value}")),
    }
}

fn resolve_keybindings_path(path: Option<&Path>) -> Result<PathBuf, String> {
    match path {
        Some(path) => Ok(path.to_path_buf()),
        None => {
            let home = std::env::var("HOME").map_err(|_| {
                String::from("HOME is not set; pass --path to locate Claude keybindings")
            })?;
            Ok(PathBuf::from(home).join(".claude").join("keybindings.json"))
        }
    }
}

#[allow(dead_code)]
fn backup_path_for(path: &Path) -> PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    PathBuf::from(format!("{}.backup.{stamp}", path.display()))
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
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::{
        AutomationAction, GuardedWorkflow, KeybindingsStatus, inspect_keybindings,
        install_recommended_keybindings, load_resolved_keybindings, prompt_submission_sequence,
        render_keybindings_json, validate_workflow_state,
    };
    use crate::classifier::{Classification, SessionState};

    const USER_BINDINGS: &str = r#"{
  "bindings": [
    {
      "context": "Global",
      "bindings": {
        "ctrl+c": "app:interrupt"
      }
    },
    {
      "context": "Chat",
      "bindings": {
        "ctrl+l": "chat:clearInput",
        "ctrl+x ctrl+e": "chat:externalEditor",
        "enter": "chat:submit"
      }
    },
    {
      "context": "Confirmation",
      "bindings": {
        "up": "confirm:previous",
        "down": "confirm:next",
        "y": "confirm:yes",
        "n": "confirm:no"
      }
    }
  ]
}"#;

    #[test]
    fn action_lookup_round_trips() {
        let action = AutomationAction::from_str("confirm-yes").expect("action should parse");
        assert_eq!(action.as_str(), "confirm-yes");
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

    #[test]
    fn guarded_workflow_maps_to_expected_actions() {
        assert_eq!(
            GuardedWorkflow::ApprovePermission.actions(),
            &[AutomationAction::ConfirmYes]
        );
        assert_eq!(GuardedWorkflow::DismissSurvey.actions(), &[]);
    }

    #[test]
    fn guarded_workflow_rejects_incompatible_state() {
        let classification = Classification {
            source: String::from("pane"),
            state: SessionState::BusyResponding,
            recap_present: false,
            recap_excerpt: None,
            signals: vec![String::from("busy-keywords")],
        };

        let error = validate_workflow_state(GuardedWorkflow::SubmitPrompt, &classification)
            .expect_err("workflow should reject incompatible state");
        assert!(error.contains("requires state=ChatReady"));
        assert!(error.contains("pane is BusyResponding"));
    }

    #[test]
    fn approve_permission_accepts_folder_trust_prompt() {
        let classification = Classification {
            source: String::from("pane"),
            state: SessionState::FolderTrustPrompt,
            recap_present: false,
            recap_excerpt: None,
            signals: vec![String::from("folder-trust-keywords")],
        };

        validate_workflow_state(GuardedWorkflow::ApprovePermission, &classification)
            .expect("approve should accept folder trust prompt");
    }

    #[test]
    fn keybinding_inspection_accepts_custom_user_bindings() {
        let root = unique_temp_dir("bindings-valid");
        fs::create_dir_all(&root).expect("temp dir should exist");
        let path = root.join("keybindings.json");
        fs::write(&path, USER_BINDINGS).expect("keybindings should write");

        let inspection = inspect_keybindings(Some(&path)).expect("inspection should succeed");
        assert_eq!(inspection.status, KeybindingsStatus::Valid);
        assert!(inspection.missing_bindings.is_empty());
    }

    #[test]
    fn resolves_custom_binding_keys_for_actions() {
        let root = unique_temp_dir("bindings-resolve");
        fs::create_dir_all(&root).expect("temp dir should exist");
        let path = root.join("keybindings.json");
        fs::write(&path, USER_BINDINGS).expect("keybindings should write");

        let bindings = load_resolved_keybindings(Some(&path)).expect("bindings should load");
        assert_eq!(
            bindings.keys_for(AutomationAction::ClearInput),
            Some([String::from("C-l")].as_slice())
        );
        assert_eq!(
            bindings.keys_for(AutomationAction::ExternalEditor),
            Some([String::from("C-x"), String::from("C-e")].as_slice())
        );
        assert_eq!(
            bindings.keys_for(AutomationAction::ConfirmYes),
            Some([String::from("y")].as_slice())
        );
    }

    #[test]
    fn keybinding_inspection_detects_missing_file() {
        let root = unique_temp_dir("bindings-missing");
        fs::create_dir_all(&root).expect("temp dir should exist");
        let path = root.join("missing.json");

        let inspection = inspect_keybindings(Some(&path)).expect("inspection should succeed");
        assert_eq!(inspection.status, KeybindingsStatus::Missing);
        assert!(!inspection.missing_bindings.is_empty());
    }

    const PARTIAL_BINDINGS: &str = r#"{
  "bindings": [
    {
      "context": "Global",
      "bindings": {
        "ctrl+c": "app:interrupt"
      }
    },
    {
      "context": "Chat",
      "bindings": {
        "ctrl+l": "chat:clearInput"
      }
    }
  ]
}"#;

    const CONFLICTING_BINDINGS: &str = r#"{
  "bindings": [
    {
      "context": "Chat",
      "bindings": {
        "f7": "chat:openHistory"
      }
    }
  ]
}"#;

    #[test]
    fn install_keybindings_preserves_existing_bindings_and_adds_missing_ones() {
        let root = unique_temp_dir("bindings-install");
        fs::create_dir_all(&root).expect("temp dir should exist");
        let path = root.join("keybindings.json");
        fs::write(&path, PARTIAL_BINDINGS).expect("existing keybindings should write");

        let report = install_recommended_keybindings(Some(&path))
            .expect("install should preserve user bindings and add missing ones");
        assert!(report.wrote_file);
        let installed = fs::read_to_string(&path).expect("installed keybindings should read");
        assert!(installed.contains("\"ctrl+c\": \"app:interrupt\""));
        assert!(installed.contains("\"ctrl+l\": \"chat:clearInput\""));
        assert!(installed.contains("\"f7\": \"chat:externalEditor\""));
        assert!(installed.contains("\"f8\": \"chat:submit\""));
        assert!(installed.contains("\"f6\": \"confirm:previous\""));
        assert!(installed.contains("\"f7\": \"confirm:next\""));
        assert!(installed.contains("\"f8\": \"confirm:yes\""));
        assert!(installed.contains("\"f9\": \"confirm:no\""));
    }

    #[test]
    fn merged_install_keeps_user_keys_for_action_routing() {
        let root = unique_temp_dir("bindings-install-routing");
        fs::create_dir_all(&root).expect("temp dir should exist");
        let path = root.join("keybindings.json");
        fs::write(&path, PARTIAL_BINDINGS).expect("existing keybindings should write");

        install_recommended_keybindings(Some(&path)).expect("install should succeed");
        let bindings = load_resolved_keybindings(Some(&path)).expect("bindings should load");

        assert_eq!(
            bindings.keys_for(AutomationAction::Interrupt),
            Some([String::from("C-c")].as_slice())
        );
        assert_eq!(
            bindings.keys_for(AutomationAction::ClearInput),
            Some([String::from("C-l")].as_slice())
        );
        assert_eq!(
            bindings.keys_for(AutomationAction::ExternalEditor),
            Some([String::from("F7")].as_slice())
        );
        assert_eq!(
            bindings.keys_for(AutomationAction::ConfirmYes),
            Some([String::from("F8")].as_slice())
        );
    }

    #[test]
    fn install_keybindings_refuses_to_overwrite_conflicting_keys() {
        let root = unique_temp_dir("bindings-install-conflict");
        fs::create_dir_all(&root).expect("temp dir should exist");
        let path = root.join("keybindings.json");
        fs::write(&path, CONFLICTING_BINDINGS).expect("existing keybindings should write");

        let error = install_recommended_keybindings(Some(&path))
            .expect_err("conflicting keybinding should fail");
        assert!(error.contains("cannot add missing Claude binding f7=chat:externalEditor"));
    }

    #[test]
    fn install_keybindings_creates_missing_file() {
        let root = unique_temp_dir("bindings-install-missing");
        fs::create_dir_all(&root).expect("temp dir should exist");
        let path = root.join("keybindings.json");

        let report = install_recommended_keybindings(Some(&path)).expect("install should succeed");
        assert!(report.wrote_file);
        assert_eq!(
            fs::read_to_string(&path).expect("installed keybindings should read"),
            render_keybindings_json()
        );
    }

    fn unique_temp_dir(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be valid")
            .as_nanos();
        std::env::temp_dir().join(format!("botctl-{label}-{}-{nanos}", std::process::id()))
    }
}
