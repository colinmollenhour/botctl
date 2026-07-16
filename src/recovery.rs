use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::app::{AppError, AppResult};
use crate::tmux::{TmuxInventory, TmuxPane, TmuxServerIdentity};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryLifecycle {
    Crashed,
    Staging,
    Staged,
    Uncertain,
    Resolved,
    Dismissed,
}

impl RecoveryLifecycle {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Crashed => "crashed",
            Self::Staging => "staging",
            Self::Staged => "staged",
            Self::Uncertain => "uncertain",
            Self::Resolved => "resolved",
            Self::Dismissed => "dismissed",
        }
    }

    pub fn parse(value: &str) -> AppResult<Self> {
        match value {
            "crashed" => Ok(Self::Crashed),
            "staging" => Ok(Self::Staging),
            "staged" => Ok(Self::Staged),
            "uncertain" => Ok(Self::Uncertain),
            "resolved" => Ok(Self::Resolved),
            "dismissed" => Ok(Self::Dismissed),
            _ => Err(AppError::new(format!(
                "invalid recovery lifecycle: {value}"
            ))),
        }
    }

    pub fn is_nonterminal(self) -> bool {
        matches!(
            self,
            Self::Crashed | Self::Staging | Self::Staged | Self::Uncertain
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryOriginalIdentity {
    pub server: TmuxServerIdentity,
    pub pane_id: String,
    pub pane_tty: String,
    pub pane_pid: Option<u32>,
    pub session_id: String,
    pub session_name: String,
    pub window_id: String,
    pub window_index: u16,
    pub window_name: String,
    pub pane_index: u16,
    pub cwd: String,
}

impl RecoveryOriginalIdentity {
    pub fn from_inventory(server: &TmuxServerIdentity, pane: &TmuxPane) -> Self {
        Self {
            server: server.clone(),
            pane_id: pane.pane_id.clone(),
            pane_tty: pane.pane_tty.clone(),
            pane_pid: pane.pane_pid,
            session_id: pane.session_id.clone(),
            session_name: pane.session_name.clone(),
            window_id: pane.window_id.clone(),
            window_index: pane.window_index,
            window_name: pane.window_name.clone(),
            pane_index: pane.pane_index,
            cwd: pane.current_path.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryTarget {
    pub server: TmuxServerIdentity,
    pub pane: TmuxPane,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryRecord {
    pub id: String,
    pub source_observation_id: String,
    pub workspace_id: String,
    pub workspace_root: String,
    pub lifecycle: RecoveryLifecycle,
    pub provider: String,
    pub provider_session_id: String,
    pub original: RecoveryOriginalIdentity,
    pub crashed_at_unix_ms: i64,
    pub staging_run_id: Option<String>,
    pub staging_token: Option<String>,
    pub staging_started_at_unix_ms: Option<i64>,
    pub target: Option<RecoveryTarget>,
    pub staged_command: Option<String>,
    pub staged_at_unix_ms: Option<i64>,
    pub resolved_at_unix_ms: Option<i64>,
    pub dismissed_at_unix_ms: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryMatchState {
    Ready,
    Unmatched,
    Ambiguous,
    Conflict,
    Incompatible,
    NotStageable,
    InvalidMetadata,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryMatch {
    pub state: RecoveryMatchState,
    pub target: Option<RecoveryTarget>,
    pub disabled_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeRecoveryOffer {
    pub recovery_id: String,
    pub workspace_id: String,
    pub workspace_root: String,
    pub lifecycle: RecoveryLifecycle,
    pub provider: String,
    pub provider_session_id: String,
    pub original: RecoveryOriginalIdentity,
    pub command: String,
    pub target: Option<RecoveryTarget>,
    pub match_state: RecoveryMatchState,
    pub disabled_reason: Option<String>,
    pub crashed_at_unix_ms: i64,
    pub staged_at_unix_ms: Option<i64>,
    pub resolved_at_unix_ms: Option<i64>,
    pub dismissed_at_unix_ms: Option<i64>,
}

/// How a provider's CLI addresses a saved session on resume.
struct RecoverableProvider {
    provider: &'static str,
    /// `<binary> <resume_args..> '<session-id>'` — quoted id appended last.
    binary: &'static str,
    resume_prefix: &'static str,
    id_kind: SessionIdKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionIdKind {
    Uuid,
    /// OpenCode `ses_<base62>` identifiers.
    OpenCodeSession,
}

pub const RECOVERABLE_PROVIDERS: &[&str] = &["claude", "grok", "opencode", "agy", "pi", "codex"];

const PROVIDER_TABLE: &[RecoverableProvider] = &[
    RecoverableProvider {
        provider: "claude",
        binary: "claude",
        resume_prefix: "--resume",
        id_kind: SessionIdKind::Uuid,
    },
    RecoverableProvider {
        provider: "grok",
        binary: "grok",
        resume_prefix: "--resume",
        id_kind: SessionIdKind::Uuid,
    },
    RecoverableProvider {
        provider: "opencode",
        binary: "opencode",
        resume_prefix: "--session",
        id_kind: SessionIdKind::OpenCodeSession,
    },
    RecoverableProvider {
        provider: "agy",
        binary: "agy",
        resume_prefix: "--conversation",
        id_kind: SessionIdKind::Uuid,
    },
    RecoverableProvider {
        provider: "pi",
        binary: "pi",
        resume_prefix: "--session",
        id_kind: SessionIdKind::Uuid,
    },
    RecoverableProvider {
        provider: "codex",
        binary: "codex",
        resume_prefix: "resume",
        id_kind: SessionIdKind::Uuid,
    },
];

fn provider_entry(provider: &str) -> Option<&'static RecoverableProvider> {
    PROVIDER_TABLE
        .iter()
        .find(|entry| entry.provider == provider)
}

/// Validate a session id against the provider's format. Every staged resume
/// command embeds this value in a shell line, so anything outside the known
/// alphabet is rejected outright.
pub fn is_valid_provider_session_id(provider: &str, session_id: &str) -> bool {
    let Some(entry) = provider_entry(provider) else {
        return false;
    };
    match entry.id_kind {
        SessionIdKind::Uuid => Uuid::parse_str(session_id).is_ok(),
        SessionIdKind::OpenCodeSession => {
            session_id.len() > 4
                && session_id.len() <= 128
                && session_id.starts_with("ses_")
                && session_id[4..].chars().all(|c| c.is_ascii_alphanumeric())
        }
    }
}

pub fn build_recovery_command(
    provider: &str,
    original_cwd: &str,
    provider_session_id: &str,
) -> AppResult<String> {
    if original_cwd.is_empty() || !Path::new(original_cwd).is_absolute() {
        return Err(AppError::new(
            "recovery cwd must be a non-empty absolute path",
        ));
    }
    validate_command_value("recovery cwd", original_cwd)?;
    validate_command_value("provider session id", provider_session_id)?;
    let Some(entry) = provider_entry(provider) else {
        return Err(AppError::new(format!(
            "recovery is supported only for providers {} (got {provider})",
            RECOVERABLE_PROVIDERS.join(", ")
        )));
    };
    if !is_valid_provider_session_id(provider, provider_session_id) {
        return Err(AppError::new(format!(
            "recovery session id is not a valid {provider} session identifier"
        )));
    }
    Ok(format!(
        "cd {} && {} {} {}",
        posix_single_quote(original_cwd),
        entry.binary,
        entry.resume_prefix,
        posix_single_quote(provider_session_id)
    ))
}

/// Providers that support crash recovery checkpointing and staged resume.
pub fn is_recoverable_provider(provider: &str) -> bool {
    provider_entry(provider).is_some()
}

pub fn provider_for_pane_command(current_command: &str) -> Option<&'static str> {
    PROVIDER_TABLE
        .iter()
        .map(|entry| entry.provider)
        .find(|provider| current_command.eq_ignore_ascii_case(provider))
}

fn validate_command_value(label: &str, value: &str) -> AppResult<()> {
    if value
        .chars()
        .any(|character| character == '\u{7f}' || character.is_control())
    {
        return Err(AppError::new(format!(
            "{label} contains a forbidden control character"
        )));
    }
    Ok(())
}

pub fn posix_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

pub fn is_compatible_recovery_shell(command: &str) -> bool {
    let basename = command.rsplit('/').next().unwrap_or(command);
    matches!(basename, "sh" | "bash" | "dash" | "zsh" | "ksh")
}

pub fn match_recoveries(
    recoveries: &[RecoveryRecord],
    inventory: &TmuxInventory,
) -> BTreeMap<String, RecoveryMatch> {
    let mut matches = BTreeMap::new();
    let mut candidates = HashMap::<String, Vec<RecoveryTarget>>::new();
    let mut target_users = HashMap::<String, Vec<String>>::new();

    for recovery in recoveries {
        if matches!(
            recovery.lifecycle,
            RecoveryLifecycle::Staging | RecoveryLifecycle::Staged | RecoveryLifecycle::Uncertain
        ) && let Some(target) = &recovery.target
        {
            target_users
                .entry(target_key(target))
                .or_default()
                .push(recovery.id.clone());
        }

        if recovery.lifecycle != RecoveryLifecycle::Crashed {
            matches.insert(
                recovery.id.clone(),
                RecoveryMatch {
                    state: RecoveryMatchState::NotStageable,
                    target: recovery.target.clone(),
                    disabled_reason: Some(match recovery.lifecycle {
                        RecoveryLifecycle::Staging => {
                            "recovery is currently being staged".to_string()
                        }
                        RecoveryLifecycle::Staged => {
                            "command was already staged without Enter".to_string()
                        }
                        RecoveryLifecycle::Uncertain => {
                            "a prior paste may have succeeded; botctl will not retry it".to_string()
                        }
                        RecoveryLifecycle::Resolved => "recovery is already resolved".to_string(),
                        RecoveryLifecycle::Dismissed => "recovery is dismissed".to_string(),
                        RecoveryLifecycle::Crashed => unreachable!(),
                    }),
                },
            );
            continue;
        }

        if let Err(error) = build_recovery_command(
            &recovery.provider,
            &recovery.original.cwd,
            &recovery.provider_session_id,
        ) {
            matches.insert(
                recovery.id.clone(),
                RecoveryMatch {
                    state: RecoveryMatchState::InvalidMetadata,
                    target: None,
                    disabled_reason: Some(error.to_string()),
                },
            );
            continue;
        }

        let exact = inventory
            .panes
            .iter()
            .filter(|pane| same_server_exact_object(recovery, inventory, pane))
            .collect::<Vec<_>>();
        let selected = if exact.is_empty() {
            inventory
                .panes
                .iter()
                .filter(|pane| recreated_logical_pane(recovery, inventory, pane))
                .collect::<Vec<_>>()
        } else {
            exact
        };
        let targets = selected
            .into_iter()
            .map(|pane| RecoveryTarget {
                server: inventory.server.clone(),
                pane: pane.clone(),
            })
            .collect::<Vec<_>>();
        for target in &targets {
            target_users
                .entry(target_key(target))
                .or_default()
                .push(recovery.id.clone());
        }
        candidates.insert(recovery.id.clone(), targets);
    }

    for recovery in recoveries
        .iter()
        .filter(|recovery| recovery.lifecycle == RecoveryLifecycle::Crashed)
    {
        if matches.contains_key(&recovery.id) {
            continue;
        }
        let candidate = candidates.remove(&recovery.id).unwrap_or_default();
        let result = match candidate.as_slice() {
            [] => RecoveryMatch {
                state: RecoveryMatchState::Unmatched,
                target: None,
                disabled_reason: Some(
                    "no pane matches the original tmux coordinates and cwd".to_string(),
                ),
            },
            [target] if !is_compatible_recovery_shell(&target.pane.current_command) => {
                RecoveryMatch {
                    state: RecoveryMatchState::Incompatible,
                    target: Some(target.clone()),
                    disabled_reason: Some(format!(
                        "matched pane is not a compatible shell: {}",
                        target.pane.current_command
                    )),
                }
            }
            [target]
                if target_users
                    .get(&target_key(target))
                    .is_some_and(|users| users.len() > 1) =>
            {
                RecoveryMatch {
                    state: RecoveryMatchState::Conflict,
                    target: None,
                    disabled_reason: Some(
                        "matched pane is also selected by another active recovery".to_string(),
                    ),
                }
            }
            [target] => RecoveryMatch {
                state: RecoveryMatchState::Ready,
                target: Some(target.clone()),
                disabled_reason: None,
            },
            _ => RecoveryMatch {
                state: RecoveryMatchState::Ambiguous,
                target: None,
                disabled_reason: Some(format!(
                    "{} panes match the original tmux coordinates and cwd",
                    candidate.len()
                )),
            },
        };
        matches.insert(recovery.id.clone(), result);
    }
    matches
}

pub fn offer_from_record(record: &RecoveryRecord, matched: RecoveryMatch) -> RuntimeRecoveryOffer {
    let command = build_recovery_command(
        &record.provider,
        &record.original.cwd,
        &record.provider_session_id,
    )
    .unwrap_or_default();
    RuntimeRecoveryOffer {
        recovery_id: record.id.clone(),
        workspace_id: record.workspace_id.clone(),
        workspace_root: record.workspace_root.clone(),
        lifecycle: record.lifecycle,
        provider: record.provider.clone(),
        provider_session_id: record.provider_session_id.clone(),
        original: record.original.clone(),
        command,
        target: matched.target,
        match_state: matched.state,
        disabled_reason: matched.disabled_reason,
        crashed_at_unix_ms: record.crashed_at_unix_ms,
        staged_at_unix_ms: record.staged_at_unix_ms,
        resolved_at_unix_ms: record.resolved_at_unix_ms,
        dismissed_at_unix_ms: record.dismissed_at_unix_ms,
    }
}

fn same_server_exact_object(
    recovery: &RecoveryRecord,
    inventory: &TmuxInventory,
    pane: &TmuxPane,
) -> bool {
    recovery.original.server == inventory.server
        && recovery.original.pane_id == pane.pane_id
        && recovery.original.session_id == pane.session_id
        && recovery.original.window_id == pane.window_id
}

fn recreated_logical_pane(
    recovery: &RecoveryRecord,
    inventory: &TmuxInventory,
    pane: &TmuxPane,
) -> bool {
    recovery.original.server.socket_path == inventory.server.socket_path
        && recovery.original.session_name == pane.session_name
        && recovery.original.window_index == pane.window_index
        && recovery.original.window_name == pane.window_name
        && recovery.original.pane_index == pane.pane_index
        && recovery.original.cwd == pane.current_path
}

fn target_key(target: &RecoveryTarget) -> String {
    format!(
        "{}\0{}\0{}\0{}",
        target.server.socket_path, target.server.pid, target.server.start_time, target.pane.pane_id
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const SESSION_ID: &str = "4d8dc7f8-a842-438a-b2c2-4d39ad509a53";

    fn server(pid: u32) -> TmuxServerIdentity {
        TmuxServerIdentity {
            socket_path: "/tmp/tmux/default".to_string(),
            pid,
            start_time: 100,
        }
    }

    fn pane(command: &str) -> TmuxPane {
        TmuxPane {
            pane_id: "%1".to_string(),
            pane_tty: "/dev/pts/1".to_string(),
            pane_pid: Some(10),
            session_id: "$1".to_string(),
            session_name: "work".to_string(),
            window_id: "@1".to_string(),
            window_index: 2,
            window_name: "code".to_string(),
            pane_index: 0,
            current_command: command.to_string(),
            current_path: "/tmp/project".to_string(),
            pane_title: String::new(),
            pane_active: true,
            cursor_x: None,
            cursor_y: None,
        }
    }

    fn recovery(id: &str) -> RecoveryRecord {
        let server = server(1);
        let pane = pane("claude");
        RecoveryRecord {
            id: id.to_string(),
            source_observation_id: format!("obs-{id}"),
            workspace_id: "ws".to_string(),
            workspace_root: "/tmp/project".to_string(),
            lifecycle: RecoveryLifecycle::Crashed,
            provider: "claude".to_string(),
            provider_session_id: SESSION_ID.to_string(),
            original: RecoveryOriginalIdentity::from_inventory(&server, &pane),
            crashed_at_unix_ms: 1,
            staging_run_id: None,
            staging_token: None,
            staging_started_at_unix_ms: None,
            target: None,
            staged_command: None,
            staged_at_unix_ms: None,
            resolved_at_unix_ms: None,
            dismissed_at_unix_ms: None,
        }
    }

    #[test]
    fn command_quotes_spaces_and_apostrophes_without_submission() {
        let command = build_recovery_command("claude", "/tmp/Colin's work", SESSION_ID).unwrap();
        assert_eq!(
            command,
            "cd '/tmp/Colin'\"'\"'s work' && claude --resume '4d8dc7f8-a842-438a-b2c2-4d39ad509a53'"
        );
        assert!(!command.ends_with(['\n', '\r']));
        let grok = build_recovery_command("grok", "/tmp/demo", SESSION_ID).unwrap();
        assert_eq!(
            grok,
            "cd '/tmp/demo' && grok --resume '4d8dc7f8-a842-438a-b2c2-4d39ad509a53'"
        );
    }

    #[test]
    fn command_covers_every_recoverable_provider() {
        let commands = [
            ("claude", SESSION_ID, "claude --resume"),
            ("grok", SESSION_ID, "grok --resume"),
            ("agy", SESSION_ID, "agy --conversation"),
            ("pi", SESSION_ID, "pi --session"),
            ("codex", SESSION_ID, "codex resume"),
            (
                "opencode",
                "ses_0965e3dcbffeKBENMf0BDST6Cj",
                "opencode --session",
            ),
        ];
        for (provider, session_id, expected_prefix) in commands {
            assert!(is_recoverable_provider(provider));
            assert_eq!(provider_for_pane_command(provider), Some(provider));
            let command = build_recovery_command(provider, "/tmp/demo", session_id).unwrap();
            assert_eq!(
                command,
                format!("cd '/tmp/demo' && {expected_prefix} '{session_id}'")
            );
        }
    }

    #[test]
    fn command_rejects_invalid_metadata() {
        assert!(build_recovery_command("gemini", "/tmp/x", SESSION_ID).is_err());
        assert!(build_recovery_command("claude", "relative", SESSION_ID).is_err());
        assert!(build_recovery_command("claude", "/tmp/x", "not-a-uuid").is_err());
        assert!(build_recovery_command("claude", "/tmp/x\nrm -rf /", SESSION_ID).is_err());
        // UUID providers reject OpenCode-style ids and vice versa.
        assert!(
            build_recovery_command("codex", "/tmp/x", "ses_0965e3dcbffeKBENMf0BDST6Cj").is_err()
        );
        assert!(build_recovery_command("opencode", "/tmp/x", SESSION_ID).is_err());
        // OpenCode ids allow only `ses_` + ASCII alphanumerics.
        assert!(build_recovery_command("opencode", "/tmp/x", "ses_").is_err());
        assert!(build_recovery_command("opencode", "/tmp/x", "ses_abc'; rm -rf /'").is_err());
    }

    #[test]
    fn exact_object_wins_over_logical_candidates() {
        let recovery = recovery("one");
        let mut exact = pane("bash");
        exact.current_path = "/other".to_string();
        let mut logical = pane("zsh");
        logical.pane_id = "%2".to_string();
        logical.session_id = "$2".to_string();
        logical.window_id = "@2".to_string();
        let inventory = TmuxInventory {
            server: server(1),
            panes: vec![logical, exact.clone()],
        };
        let matched = match_recoveries(&[recovery], &inventory);
        assert_eq!(matched["one"].target.as_ref().unwrap().pane, exact);
    }

    #[test]
    fn cross_server_id_reuse_requires_all_logical_coordinates() {
        let recovery = recovery("one");
        let mut candidate = pane("bash");
        candidate.window_name = "renamed".to_string();
        let inventory = TmuxInventory {
            server: server(2),
            panes: vec![candidate],
        };
        assert_eq!(
            match_recoveries(&[recovery], &inventory)["one"].state,
            RecoveryMatchState::Unmatched
        );
    }

    #[test]
    fn recreated_logical_match_requires_every_coordinate_and_socket_namespace() {
        let recovery = recovery("one");
        let mut valid = pane("bash");
        valid.pane_id = "%99".to_string();
        valid.session_id = "$99".to_string();
        valid.window_id = "@99".to_string();
        let replacement_server = server(2);
        assert_eq!(
            match_recoveries(
                std::slice::from_ref(&recovery),
                &TmuxInventory {
                    server: replacement_server.clone(),
                    panes: vec![valid.clone()],
                }
            )["one"]
                .state,
            RecoveryMatchState::Ready
        );

        let mut mismatches = Vec::new();
        let mut changed = valid.clone();
        changed.session_name = "other".to_string();
        mismatches.push((replacement_server.clone(), changed));
        let mut changed = valid.clone();
        changed.window_index += 1;
        mismatches.push((replacement_server.clone(), changed));
        let mut changed = valid.clone();
        changed.window_name = "renamed".to_string();
        mismatches.push((replacement_server.clone(), changed));
        let mut changed = valid.clone();
        changed.pane_index += 1;
        mismatches.push((replacement_server.clone(), changed));
        let mut changed = valid.clone();
        changed.current_path = "/tmp/other".to_string();
        mismatches.push((replacement_server.clone(), changed));
        let mut other_socket = replacement_server;
        other_socket.socket_path = "/tmp/tmux/other".to_string();
        mismatches.push((other_socket, valid));

        for (server, pane) in mismatches {
            assert_eq!(
                match_recoveries(
                    std::slice::from_ref(&recovery),
                    &TmuxInventory {
                        server,
                        panes: vec![pane],
                    }
                )["one"]
                    .state,
                RecoveryMatchState::Unmatched
            );
        }
    }

    #[test]
    fn duplicate_logical_candidates_are_ambiguous() {
        let recovery = recovery("one");
        let mut first = pane("bash");
        first.pane_id = "%2".to_string();
        first.session_id = "$2".to_string();
        first.window_id = "@2".to_string();
        let mut second = first.clone();
        second.pane_id = "%3".to_string();
        let inventory = TmuxInventory {
            server: server(2),
            panes: vec![first, second],
        };
        assert_eq!(
            match_recoveries(&[recovery], &inventory)["one"].state,
            RecoveryMatchState::Ambiguous
        );
    }

    #[test]
    fn duplicate_and_global_conflict_are_disabled() {
        let first = recovery("one");
        let second = recovery("two");
        let inventory = TmuxInventory {
            server: server(2),
            panes: vec![pane("bash")],
        };
        let matched = match_recoveries(&[first, second], &inventory);
        assert_eq!(matched["one"].state, RecoveryMatchState::Conflict);
        assert_eq!(matched["two"].state, RecoveryMatchState::Conflict);
    }

    #[test]
    fn shell_whitelist_is_exact() {
        for shell in ["sh", "bash", "dash", "zsh", "ksh", "/bin/bash"] {
            assert!(is_compatible_recovery_shell(shell));
        }
        for command in ["Claude", "claude", "fish", "nvim", "mybash"] {
            assert!(!is_compatible_recovery_shell(command));
        }
    }
}
