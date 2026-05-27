use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::app::{AppError, AppResult};
use crate::automation::{AutomationAction, load_resolved_keybindings};
use crate::classifier::{Classification, Classifier, SessionState};
use crate::last_message::{LastAgentMessage, load_last_agent_message};
use crate::mcp_registry::{
    LifecycleState, McpRegistry, McpSessionRecord, NewSessionRecord, SessionLock,
};
use crate::tmux::{StartWindowRequest, TmuxClient, TmuxPane};

const DEFAULT_SESSION_NAME: &str = "botctl-mcp";
const DEFAULT_POLL_MS: u64 = 500;
const DEFAULT_SPAWN_TIMEOUT_MS: u64 = 60_000;
const DEFAULT_TURN_TIMEOUT_MS: u64 = 600_000;
const DEFAULT_KILL_TIMEOUT_MS: u64 = 30_000;
const DEFAULT_CAPTURE_LINES: usize = 200;
const MAX_TIMEOUT_MS: u64 = 60 * 60 * 1000;
const LOCK_REFRESH_INTERVAL_MS: u64 = 30_000;

#[derive(Debug, Clone)]
pub struct McpSessionService {
    registry: McpRegistry,
    server_id: String,
}

impl McpSessionService {
    pub fn new(registry: McpRegistry, server_id: String) -> Self {
        Self {
            registry,
            server_id,
        }
    }

    pub fn spawn(&self, args: &Value) -> AppResult<Value> {
        let cwd = required_str(args, "cwd")?;
        let cwd = canonical_dir(cwd)?;
        let timeout_ms = optional_u64(args, "timeout_ms", DEFAULT_SPAWN_TIMEOUT_MS)?;
        let window_name = format!(
            "botctl-mcp-{}",
            &self.server_id[..8.min(self.server_id.len())]
        );
        let client = TmuxClient::default();
        let started = client.start_window_in_session(&StartWindowRequest {
            session_name: DEFAULT_SESSION_NAME.to_string(),
            window_name,
            cwd: cwd.clone(),
            command: "claude".to_string(),
        })?;
        let record = self.registry.insert_session(NewSessionRecord {
            owner_server_id: self.server_id.clone(),
            tmux_session_name: started.session_name,
            tmux_window_id: started.window_id,
            tmux_window_name: started.window_name,
            tmux_pane_id: started.pane_id,
            cwd: cwd.display().to_string(),
        })?;

        let outcome = self.wait_inner(&record, timeout_ms, false, false, None)?;
        if outcome["outcome"] == "ready" {
            self.registry
                .update_state(&record.id, LifecycleState::Ready, Some("ChatReady"))?;
        }
        let record = self.registry.get(&record.id)?.unwrap_or(record);
        let mut result = json!({ "agent": agent_ref(&record), "outcome": outcome });
        if let Some(initial_prompt) = args.get("initial_prompt").and_then(Value::as_str) {
            let prompt_result = self.prompt(&json!({
                "id": record.id,
                "prompt": initial_prompt,
                "timeout_ms": timeout_ms,
                "policy": args.get("policy").cloned().unwrap_or(Value::Null),
            }))?;
            result["initial_prompt"] = prompt_result;
        }
        Ok(result)
    }

    pub fn prompt(&self, args: &Value) -> AppResult<Value> {
        let id = required_str(args, "id")?.to_string();
        let prompt = required_str(args, "prompt")?;
        let timeout_ms = optional_u64(args, "timeout_ms", DEFAULT_TURN_TIMEOUT_MS)?;
        let no_yolo = args
            .pointer("/policy/no_yolo")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let Some(lock) = self.registry.acquire_lock(&id, &self.server_id, "prompt")? else {
            return self.busy_result(&id);
        };
        let record = self.get_record(&id)?;
        let ready = self.wait_inner(&record, timeout_ms, false, no_yolo, Some(&lock))?;
        if ready["outcome"] != "ready" {
            self.registry.update_state(
                &id,
                lifecycle_from_outcome(ready["outcome"].as_str().unwrap_or("unknown")),
                ready["classified_state"].as_str(),
            )?;
            let record = self.registry.get(&id)?.unwrap_or(record);
            return Ok(json!({ "agent": agent_ref(&record), "outcome": ready }));
        }
        let pane = self.verify_pane(&record)?;
        let baseline = load_last_agent_message(&pane)
            .ok()
            .or_else(|| cursor_as_message(&record));
        self.submit_direct(&pane, prompt)?;
        self.registry
            .update_state(&id, LifecycleState::Running, Some("PromptEditing"))?;
        let outcome = self.wait_inner(&record, timeout_ms, false, no_yolo, Some(&lock))?;
        let mut outcome = outcome;
        let outcome_kind = outcome["outcome"].as_str().unwrap_or("unknown");
        if outcome_kind == "ready" || outcome_kind == "needs_user_input" {
            match self
                .verify_pane(&record)
                .and_then(|pane| fresh_message(&pane, baseline.as_ref()))
            {
                Ok(message) => {
                    self.registry
                        .update_cursor(&id, Some(&message.session_id), &message.text)?;
                    outcome["message"] =
                        json!({ "role":"assistant", "text": message.text, "fresh": true });
                    outcome["fresh_message"] = json!(true);
                }
                Err(_) => {
                    outcome["message"] = Value::Null;
                    outcome["fresh_message"] = json!(false);
                    outcome["warnings"] = json!(["stale_transcript"]);
                }
            }
        }
        let state = lifecycle_from_outcome(outcome["outcome"].as_str().unwrap_or("unknown"));
        self.registry
            .update_state(&id, state, outcome["classified_state"].as_str())?;
        let record = self.registry.get(&id)?.unwrap_or(record);
        Ok(json!({ "agent": agent_ref(&record), "outcome": outcome }))
    }

    pub fn wait(&self, args: &Value) -> AppResult<Value> {
        let id = required_str(args, "id")?.to_string();
        let timeout_ms = optional_u64(args, "timeout_ms", DEFAULT_TURN_TIMEOUT_MS)?;
        let require_fresh = args
            .get("require_fresh_message")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let Some(lock) = self.registry.acquire_lock(&id, &self.server_id, "wait")? else {
            return self.busy_result(&id);
        };
        let record = self.get_record(&id)?;
        let outcome = self.wait_inner(&record, timeout_ms, require_fresh, false, Some(&lock))?;
        self.registry.update_state(
            &id,
            lifecycle_from_outcome(outcome["outcome"].as_str().unwrap_or("unknown")),
            outcome["classified_state"].as_str(),
        )?;
        let record = self.registry.get(&id)?.unwrap_or(record);
        Ok(json!({ "agent": agent_ref(&record), "outcome": outcome }))
    }

    pub fn kill(&self, args: &Value) -> AppResult<Value> {
        let id = required_str(args, "id")?.to_string();
        let _ = optional_u64(args, "timeout_ms", DEFAULT_KILL_TIMEOUT_MS)?;
        let Some(_lock) = self.registry.acquire_lock(&id, &self.server_id, "kill")? else {
            return self.busy_result(&id);
        };
        let record = self.get_record(&id)?;
        let client = TmuxClient::default();
        let already_gone = match client.pane_by_id(&record.tmux_pane_id)? {
            Some(pane) if pane.window_id == record.tmux_window_id => {
                client.kill_window(&record.tmux_window_id)?;
                false
            }
            Some(_) => {
                return Err(AppError::new(
                    "ambiguous_target: managed pane no longer belongs to recorded window",
                ));
            }
            None => true,
        };
        self.registry
            .update_state(&id, LifecycleState::Killed, None)?;
        let record = self.registry.get(&id)?.unwrap_or(record);
        Ok(json!({ "agent": agent_ref(&record), "killed": true, "already_gone": already_gone }))
    }

    pub fn snapshot(&self, args: &Value) -> AppResult<Value> {
        let id = required_str(args, "id")?.to_string();
        let capture_lines = optional_usize(args, "capture_lines", DEFAULT_CAPTURE_LINES)?;
        let Some(_lock) = self
            .registry
            .acquire_lock(&id, &self.server_id, "snapshot")?
        else {
            return self.busy_result(&id);
        };
        let record = self.get_record(&id)?;
        let client = TmuxClient::default();
        let Some(pane) = client.pane_by_id(&record.tmux_pane_id)? else {
            self.registry
                .update_state(&id, LifecycleState::Dead, None)?;
            return Ok(
                json!({ "agent": agent_ref(&record), "outcome": outcome("dead", None, None, None), "pane_text":"", "recent_lines": [] }),
            );
        };
        if pane.window_id != record.tmux_window_id {
            return Err(AppError::new(
                "ambiguous_target: managed pane no longer belongs to recorded window",
            ));
        }
        let pane_text = client.capture_pane(&pane.pane_id, capture_lines)?;
        let classification = Classifier.classify(&pane.pane_id, &pane_text);
        let recent_lines = pane_text
            .lines()
            .rev()
            .take(20)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>();
        Ok(json!({
            "agent": agent_ref(&record),
            "outcome": outcome(outcome_for_state(classification.state), Some(&classification), Some(&pane_text), None),
            "pane_text": pane_text,
            "recent_lines": recent_lines,
        }))
    }

    pub fn send_keys(&self, args: &Value) -> AppResult<Value> {
        let id = required_str(args, "id")?.to_string();
        let Some(_lock) = self
            .registry
            .acquire_lock(&id, &self.server_id, "send_keys")?
        else {
            return self.busy_result(&id);
        };
        let record = self.get_record(&id)?;
        let pane = self.verify_pane(&record)?;
        let client = TmuxClient::default();
        let has_keys = args.get("keys").is_some();
        let has_text = args.get("text").is_some();
        if has_keys == has_text {
            return Err(AppError::new(
                "invalid_params: botctl_send_keys requires exactly one of keys or text",
            ));
        }
        if let Some(keys) = args.get("keys") {
            let keys = keys
                .as_array()
                .ok_or_else(|| AppError::new("invalid_params: keys must be an array"))?
                .iter()
                .map(|v| {
                    v.as_str()
                        .map(str::to_string)
                        .ok_or_else(|| AppError::new("invalid_params: key must be a string"))
                })
                .collect::<AppResult<Vec<_>>>()?;
            client.send_keys(&pane.pane_id, &keys)?;
        } else {
            let text = required_str(args, "text")?;
            if args.get("paste").and_then(Value::as_bool).unwrap_or(true) {
                client.paste_text(&pane.pane_id, text)?;
            } else {
                client.send_keys(&pane.pane_id, &[text])?;
            }
        }
        Ok(
            json!({ "agent": agent_ref(&record), "sent": true, "warning": "unsafe_operator_escape_hatch_no_progress_implied" }),
        )
    }

    fn get_record(&self, id: &str) -> AppResult<McpSessionRecord> {
        self.registry
            .get(id)?
            .ok_or_else(|| AppError::new(format!("not_found: unknown MCP session id {id}")))
    }

    fn verify_pane(&self, record: &McpSessionRecord) -> AppResult<TmuxPane> {
        let client = TmuxClient::default();
        let pane = client
            .pane_by_id(&record.tmux_pane_id)?
            .ok_or_else(|| AppError::new("dead_pane: managed pane is gone"))?;
        if pane.window_id != record.tmux_window_id {
            return Err(AppError::new(
                "ambiguous_target: managed pane no longer belongs to recorded window",
            ));
        }
        Ok(pane)
    }

    fn submit_direct(&self, pane: &TmuxPane, prompt: &str) -> AppResult<()> {
        let client = TmuxClient::default();
        let bindings = load_resolved_keybindings(None).map_err(AppError::new)?;
        let submit = bindings
            .keys_for(AutomationAction::Submit)
            .ok_or_else(|| AppError::new("missing_keybinding: submit"))?;
        client.paste_text(&pane.pane_id, prompt)?;
        thread::sleep(Duration::from_millis(250));
        client.send_keys(&pane.pane_id, submit)
    }

    fn wait_inner(
        &self,
        record: &McpSessionRecord,
        timeout_ms: u64,
        require_fresh: bool,
        no_yolo: bool,
        lock: Option<&SessionLock>,
    ) -> AppResult<Value> {
        let client = TmuxClient::default();
        let deadline = safe_deadline(timeout_ms);
        let mut last_lock_refresh = Instant::now();
        let baseline = if require_fresh {
            cursor_as_message(record)
        } else {
            None
        };
        loop {
            if let Some(lock) = lock
                && last_lock_refresh.elapsed() >= Duration::from_millis(LOCK_REFRESH_INTERVAL_MS)
            {
                lock.refresh()?;
                last_lock_refresh = Instant::now();
            }
            let Some(pane) = client.pane_by_id(&record.tmux_pane_id)? else {
                self.registry
                    .update_state(&record.id, LifecycleState::Dead, None)?;
                return Ok(outcome("dead", None, None, None));
            };
            if pane.window_id != record.tmux_window_id {
                return Ok(outcome("blocked", None, None, Some("ambiguous_target")));
            }
            let frame = client.capture_pane(&pane.pane_id, 2000)?;
            let classification = Classifier.classify(&pane.pane_id, &frame);
            match classification.state {
                SessionState::ChatReady => {
                    let mut out = outcome("ready", Some(&classification), Some(&frame), None);
                    if require_fresh && let Ok(message) = fresh_message(&pane, baseline.as_ref()) {
                        self.registry.update_cursor(
                            &record.id,
                            Some(&message.session_id),
                            &message.text,
                        )?;
                        out["message"] =
                            json!({ "role":"assistant", "text": message.text, "fresh": true });
                    }
                    return Ok(out);
                }
                SessionState::UserQuestionPrompt => {
                    return Ok(outcome(
                        "needs_user_input",
                        Some(&classification),
                        Some(&frame),
                        None,
                    ));
                }
                SessionState::BusyResponding
                | SessionState::PromptEditing
                | SessionState::ExternalEditorActive => {}
                SessionState::FolderTrustPrompt if !no_yolo => {
                    client.send_keys(&pane.pane_id, &["Enter"])?
                }
                SessionState::PermissionDialog
                | SessionState::FolderTrustPrompt
                | SessionState::SurveyPrompt
                | SessionState::PlanApprovalPrompt
                | SessionState::DiffDialog => {
                    return Ok(outcome(
                        "blocked",
                        Some(&classification),
                        Some(&frame),
                        Some("blocked_state"),
                    ));
                }
                SessionState::Unknown => {
                    return Ok(outcome(
                        "unknown",
                        Some(&classification),
                        Some(&frame),
                        Some("unknown_state"),
                    ));
                }
            }
            if Instant::now() >= deadline {
                return Ok(outcome(
                    "timeout",
                    Some(&classification),
                    Some(&frame),
                    Some("timeout"),
                ));
            }
            thread::sleep(Duration::from_millis(DEFAULT_POLL_MS));
        }
    }

    fn busy_result(&self, id: &str) -> AppResult<Value> {
        let record = self.get_record(id)?;
        Ok(
            json!({ "agent": agent_ref(&record), "outcome": outcome("busy", None, None, Some("busy")) }),
        )
    }
}

fn required_str<'a>(args: &'a Value, name: &str) -> AppResult<&'a str> {
    args.get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| AppError::new(format!("invalid_params: missing or invalid {name}")))
}

fn optional_u64(args: &Value, name: &str, default: u64) -> AppResult<u64> {
    match args.get(name) {
        Some(value) => value
            .as_u64()
            .filter(|v| *v >= 1000 && *v <= MAX_TIMEOUT_MS)
            .ok_or_else(|| {
                AppError::new(format!(
                    "invalid_params: {name} must be an integer between 1000 and {MAX_TIMEOUT_MS}"
                ))
            }),
        None => Ok(default),
    }
}

fn safe_deadline(timeout_ms: u64) -> Instant {
    Instant::now()
        .checked_add(Duration::from_millis(timeout_ms.min(MAX_TIMEOUT_MS)))
        .unwrap_or_else(Instant::now)
}

fn optional_usize(args: &Value, name: &str, default: usize) -> AppResult<usize> {
    match args.get(name) {
        Some(value) => value
            .as_u64()
            .filter(|v| *v >= 1 && *v <= 5000)
            .map(|v| v as usize)
            .ok_or_else(|| AppError::new(format!("invalid_params: {name} out of range"))),
        None => Ok(default),
    }
}

fn canonical_dir(path: &str) -> AppResult<PathBuf> {
    let path = Path::new(path);
    if !path.is_dir() {
        return Err(AppError::new("bad_cwd: cwd is not an existing directory"));
    }
    Ok(path.canonicalize().unwrap_or_else(|_| path.to_path_buf()))
}

fn agent_ref(record: &McpSessionRecord) -> Value {
    json!({
        "id": record.id,
        "state": record.lifecycle_state.as_str(),
        "cwd": record.cwd,
        "tmux": {
            "session_name": record.tmux_session_name,
            "window_id": record.tmux_window_id,
            "window_name": record.tmux_window_name,
            "pane_id": record.tmux_pane_id,
        },
        "created_at_ms": record.created_at_ms,
        "updated_at_ms": record.updated_at_ms,
    })
}

fn outcome(
    kind: &str,
    classification: Option<&Classification>,
    snapshot: Option<&str>,
    reason: Option<&str>,
) -> Value {
    json!({
        "outcome": kind,
        "classified_state": classification.map(|c| c.state.as_str()),
        "message": Value::Null,
        "warnings": reason.map(|r| vec![r]).unwrap_or_default(),
        "snapshot": snapshot.map(|s| s.lines().rev().take(20).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n")),
    })
}

fn outcome_for_state(state: SessionState) -> &'static str {
    match state {
        SessionState::ChatReady => "ready",
        SessionState::UserQuestionPrompt => "needs_user_input",
        SessionState::BusyResponding
        | SessionState::PromptEditing
        | SessionState::ExternalEditorActive => "timeout",
        SessionState::Unknown => "unknown",
        _ => "blocked",
    }
}

fn lifecycle_from_outcome(outcome: &str) -> LifecycleState {
    match outcome {
        "ready" | "needs_user_input" | "timeout" => LifecycleState::Ready,
        "blocked" | "unknown" => LifecycleState::Blocked,
        "dead" => LifecycleState::Dead,
        _ => LifecycleState::Ready,
    }
}

fn cursor_as_message(record: &McpSessionRecord) -> Option<LastAgentMessage> {
    Some(LastAgentMessage {
        provider: "Claude",
        session_id: record.last_message_id.clone()?,
        text: record.last_message_text.clone()?,
    })
}

fn fresh_message(pane: &TmuxPane, prior: Option<&LastAgentMessage>) -> AppResult<LastAgentMessage> {
    let message = load_last_agent_message(pane)?;
    if prior
        .map(|old| old.session_id != message.session_id || old.text != message.text)
        .unwrap_or(true)
    {
        Ok(message)
    } else {
        Err(AppError::new("stale_transcript"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn send_keys_requires_exactly_one_payload() {
        assert!(required_str(&json!({}), "id").is_err());
        assert_eq!(
            optional_u64(&json!({"timeout_ms":1000}), "timeout_ms", 5).unwrap(),
            1000
        );
        assert!(optional_u64(&json!({"timeout_ms":999}), "timeout_ms", 5).is_err());
        assert!(optional_u64(&json!({"timeout_ms":MAX_TIMEOUT_MS + 1}), "timeout_ms", 5).is_err());
    }

    #[test]
    fn maps_lifecycle_outcomes() {
        assert_eq!(lifecycle_from_outcome("dead"), LifecycleState::Dead);
        assert_eq!(lifecycle_from_outcome("blocked"), LifecycleState::Blocked);
        assert_eq!(lifecycle_from_outcome("ready"), LifecycleState::Ready);
    }

    #[test]
    fn timeout_deadline_is_capped_safely() {
        let deadline = safe_deadline(u64::MAX);
        assert!(deadline <= Instant::now() + Duration::from_millis(MAX_TIMEOUT_MS));
    }
}
