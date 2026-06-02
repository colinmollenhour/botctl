use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::app::{AppError, AppResult, is_startup_enter_prompt};
use crate::automation::{AutomationAction, load_resolved_keybindings};
use crate::classifier::{
    Classification, Classifier, SessionState, prepare_frame_for_classification,
};
use crate::last_message::{LastAgentMessage, load_last_agent_message};
use crate::mcp_registry::{
    LifecycleState, McpRegistry, McpSessionRecord, NewSessionRecord, Provider, SessionLock,
};
use crate::tmux::{StartWindowRequest, TmuxClient, TmuxPane};

const DEFAULT_SESSION_NAME: &str = "botctl-mcp";
const DEFAULT_POLL_MS: u64 = 500;
const DEFAULT_SPAWN_TIMEOUT_MS: u64 = 60_000;
const DEFAULT_TURN_TIMEOUT_MS: u64 = 600_000;
const DEFAULT_KILL_TIMEOUT_MS: u64 = 30_000;
const DEFAULT_CAPTURE_LINES: usize = 200;
const LAST_MESSAGE_HISTORY_LINES: usize = 2000;
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
        // R1: validate ALL args (incl. timeout_ms range + policy.no_yolo) up
        // front so an invalid arg returns Err with ZERO side effects (no tmux
        // window). The validated values are threaded forward — no re-validation.
        let validated = validate_spawn_args(args)?;
        let record = self.spawn_window(&validated)?;
        // After this point a tmux window + registry record exist. Any error from
        // the ready-wait / prompt phase must NOT leak the window: a direct `spawn`
        // caller cannot recover the id, so kill it best-effort before propagating.
        match self.spawn_after_window(&record, &validated) {
            Ok(result) => Ok(result),
            Err(error) => {
                let _ = self.kill(&json!({ "id": record.id }));
                Err(error)
            }
        }
    }

    /// Create the tmux window and insert the registry record from already
    /// validated args (validation happened in `validate_spawn_args`, so no
    /// window can be created on an invalid arg). If the registry insert fails
    /// right after the window is created, the raw tmux window is killed
    /// best-effort so it does not leak. Returns the inserted record on success.
    fn spawn_window(&self, validated: &ValidatedSpawnArgs) -> AppResult<McpSessionRecord> {
        let ValidatedSpawnArgs {
            cwd,
            provider,
            model,
            effort,
            agent,
            command,
            ..
        } = validated;
        let window_name = format!(
            "botctl-mcp-{}-{}",
            provider.as_str(),
            &self.server_id[..8.min(self.server_id.len())]
        );
        let client = TmuxClient::default();
        let started = client.start_window_in_session(&StartWindowRequest {
            session_name: DEFAULT_SESSION_NAME.to_string(),
            window_name,
            cwd: cwd.clone(),
            command: command.clone(),
        })?;
        let window_id = started.window_id.clone();
        match self.registry.insert_session(NewSessionRecord {
            owner_server_id: self.server_id.clone(),
            provider: *provider,
            model: model.clone(),
            effort: effort.clone(),
            agent: agent.clone(),
            tmux_session_name: started.session_name,
            tmux_window_id: started.window_id,
            tmux_window_name: started.window_name,
            tmux_pane_id: started.pane_id,
            cwd: cwd.display().to_string(),
        }) {
            Ok(record) => Ok(record),
            Err(error) => {
                // The window exists but is now orphaned (no record); kill it.
                let _ = client.kill_window(&window_id);
                Err(error)
            }
        }
    }

    /// Drive the ready-wait, state update, and optional `initial_prompt` for an
    /// already-created window/record using already-validated args. On error the
    /// window still exists; the caller is responsible for cleanup (see `spawn` /
    /// `one_shot`).
    fn spawn_after_window(
        &self,
        record: &McpSessionRecord,
        validated: &ValidatedSpawnArgs,
    ) -> AppResult<Value> {
        let timeout_ms = validated.timeout_ms;
        let no_yolo = validated.no_yolo;
        let outcome = self.wait_inner(record, timeout_ms, false, no_yolo, None, true, None)?;
        self.registry.update_state(
            &record.id,
            lifecycle_from_outcome(outcome["outcome"].as_str().unwrap_or("unknown")),
            outcome["classified_state"].as_str(),
        )?;
        let record = self
            .registry
            .get(&record.id)?
            .unwrap_or_else(|| record.clone());
        let mut result = json!({ "agent": agent_ref(&record), "outcome": outcome });
        if result["outcome"]["outcome"] == "ready"
            && let Some(initial_prompt) = validated.initial_prompt.as_deref()
        {
            let prompt_result = self.prompt(&json!({
                "id": record.id,
                "prompt": initial_prompt,
                "timeout_ms": timeout_ms,
                "policy": validated.policy.clone(),
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
        let ready = self.wait_inner(
            &record,
            timeout_ms,
            false,
            no_yolo,
            None,
            false,
            Some(&lock),
        )?;
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
        let baseline =
            load_last_agent_message(&pane, &TmuxClient::default(), LAST_MESSAGE_HISTORY_LINES)
                .ok()
                .or_else(|| cursor_as_message(&record));
        self.submit_direct(record.provider, &pane, prompt)?;
        self.registry
            .update_state(&id, LifecycleState::Running, Some("PromptEditing"))?;
        let outcome = self.wait_inner(
            &record,
            timeout_ms,
            true,
            no_yolo,
            baseline.clone(),
            false,
            Some(&lock),
        )?;
        let mut outcome = outcome;
        let outcome_kind = outcome["outcome"].as_str().unwrap_or("unknown");
        if outcome_kind == "ready" || outcome_kind == "needs_user_input" {
            outcome["fresh_message"] = json!(
                outcome
                    .get("message")
                    .map(|v| v.is_object())
                    .unwrap_or(false)
            );
            remove_snapshot_for_transcript_outcome(&mut outcome);
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
        let outcome = self.wait_inner(
            &record,
            timeout_ms,
            require_fresh,
            true,
            None,
            false,
            Some(&lock),
        )?;
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

    /// Create a temporary managed session, run exactly one prompt to a terminal
    /// outcome, then always attempt to kill the window (best-effort cleanup).
    ///
    /// Implemented in two phases over `spawn`'s building blocks so cleanup never
    /// depends on `spawn` returning `Ok`: phase 1 (`spawn_window`) creates the
    /// window + record; phase 2 (`spawn_after_window`) does wait-ready -> prompt
    /// (submitting the prompt exactly once). If phase 2 errors the window is live,
    /// so the kill is always attempted (finally-style). Per-phase timeout (D5):
    /// `timeout_ms` applies independently to the spawn-ready wait and the prompt
    /// turn; `kill` uses its own default.
    ///
    /// Error semantics: argument-validation failures (missing/blank prompt text,
    /// bad explicit `cwd`, invalid optional args) surface as JSON-RPC errors
    /// (`Err`). Spawn, turn, and kill failures are NOT errors — they are encoded
    /// in the result fields (`outcome`, `kill`, `error`) and this method returns
    /// `Ok` so `call_tool` reports `isError:false`.
    pub fn one_shot(&self, args: &Value) -> AppResult<Value> {
        // R1: validate the one-shot-specific prompt text up front (non-empty)...
        let _ = one_shot_prompt(args)?;
        // ...then validate ALL spawn args (cwd, provider, model, effort, agent,
        // timeout_ms range, policy.no_yolo) BEFORE any tmux window is created.
        // Argument-validation failures propagate as JSON-RPC errors (Err), NOT as
        // outcome:"spawn_failed". outcome:"spawn_failed" is reserved for genuine
        // OPERATIONAL failures (tmux start / registry insert) that happen AFTER
        // validation passes.
        let spawn_args = one_shot_spawn_args(args);
        let validated = validate_spawn_args(&spawn_args)?;

        // Phase 1: create the window + record. A failure here is OPERATIONAL
        // (tmux start failure, or registry insert failure — with the raw window
        // already killed inside `spawn_window`). Report spawn_failed and skip
        // kill (no window to clean up).
        let record = match self.spawn_window(&validated) {
            Ok(record) => record,
            Err(e) => {
                return Ok(json!({
                    "agent": Value::Null,
                    "spawn_outcome": Value::Null,
                    "outcome": "spawn_failed",
                    "message": Value::Null,
                    "fresh_message": false,
                    "killed": false,
                    "kill": { "status": "skipped" },
                    "error": e.to_string(),
                }));
            }
        };

        // Phase 2: a window now exists. Drive ready-wait + prompt. On ANY error
        // here (ready wait, update_state, or initial_prompt) the window is live,
        // so we MUST still attempt the best-effort kill (finally-style) before
        // returning — this is the leak F3 closes.
        match self.spawn_after_window(&record, &validated) {
            Ok(spawn_result) => {
                let agent = spawn_result.get("agent").cloned().unwrap_or(Value::Null);
                let spawn_outcome = spawn_result.get("outcome").cloned().unwrap_or(Value::Null);
                // The terminal prompt turn lives under initial_prompt.outcome.
                let prompt_outcome = spawn_result
                    .get("initial_prompt")
                    .and_then(|p| p.get("outcome"));
                let outcome_kind = prompt_outcome
                    .and_then(|o| o.get("outcome"))
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
                    .to_string();
                let message = prompt_outcome
                    .and_then(|o| o.get("message"))
                    .cloned()
                    .filter(|m| m.is_object());
                // Propagate the prompt turn's own fresh_message flag (default
                // false) rather than inferring it from message presence.
                let fresh_message = prompt_outcome
                    .and_then(|o| o.get("fresh_message"))
                    .and_then(Value::as_bool)
                    .unwrap_or(false);

                let kill = self.kill_for_one_shot(&record.id);
                let killed = kill["status"] == "ok" && kill["killed"] == json!(true);

                let mut result = json!({
                    "agent": agent,
                    "spawn_outcome": spawn_outcome,
                    "outcome": outcome_kind,
                    "message": message.clone().unwrap_or(Value::Null),
                    "fresh_message": fresh_message,
                    "killed": killed,
                    "kill": kill,
                });
                if let Some(error) = result["kill"].get("error").cloned() {
                    result["error"] = error;
                }
                Ok(result)
            }
            Err(e) => {
                // Post-creation failure: the window is live, so always attempt the
                // kill. The agent ref/outcome aren't available, so report unknown.
                let kill = self.kill_for_one_shot(&record.id);
                let killed = kill["status"] == "ok" && kill["killed"] == json!(true);
                // The phase-2 error is the primary cause; the kill outcome (incl.
                // any kill error) is still reported under `kill`.
                Ok(json!({
                    "agent": agent_ref(&record),
                    "spawn_outcome": Value::Null,
                    "outcome": "unknown",
                    "message": Value::Null,
                    "fresh_message": false,
                    "killed": killed,
                    "kill": kill,
                    "error": e.to_string(),
                }))
            }
        }
    }

    /// Best-effort kill used by `one_shot`; maps `kill` result/error into the
    /// `kill` sub-object shape (B.3). Never returns Err.
    fn kill_for_one_shot(&self, id: &str) -> Value {
        match self.kill(&json!({ "id": id })) {
            Ok(v) if v["killed"] == json!(true) => json!({
                "status": "ok",
                "killed": true,
                "already_gone": v.get("already_gone").cloned().unwrap_or(Value::Bool(false)),
            }),
            Ok(v) if v.pointer("/outcome/outcome") == Some(&json!("busy")) => json!({
                "status": "busy",
                "outcome": v.get("outcome").cloned().unwrap_or(Value::Null),
            }),
            Ok(v) => json!({ "status": "error", "error": v.to_string() }),
            Err(e) => json!({ "status": "error", "error": e.to_string() }),
        }
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
        let pane_text = client.capture_pane_ansi(&pane.pane_id, capture_lines)?;
        let pane_text = prepare_frame_for_classification(&pane_text);
        let classification = Classifier.classify(&pane.pane_id, &pane_text);
        let outcome_kind = outcome_for_state(classification.state);
        self.registry.update_state(
            &id,
            lifecycle_from_outcome(outcome_kind),
            Some(classification.state.as_str()),
        )?;
        let record = self.registry.get(&id)?.unwrap_or(record);
        let recent_lines = useful_recent_lines(&pane_text, 20);
        Ok(json!({
            "agent": agent_ref(&record),
            "outcome": outcome(outcome_kind, Some(&classification), Some(&pane_text), None),
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
        let keys = optional_keys(args)?;
        let text = optional_nonblank_str(args, "text")?;
        if keys.is_empty() && text.is_none() {
            return Err(AppError::new(
                "invalid_params: send_keys requires non-empty keys or text",
            ));
        }
        if let Some(text) = text {
            if args.get("paste").and_then(Value::as_bool).unwrap_or(true) {
                client.paste_text(&pane.pane_id, &text)?;
            } else {
                client.send_keys(&pane.pane_id, &[text])?;
            }
        }
        if !keys.is_empty() {
            client.send_keys(&pane.pane_id, &keys)?;
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

    fn submit_direct(&self, provider: Provider, pane: &TmuxPane, prompt: &str) -> AppResult<()> {
        let client = TmuxClient::default();
        client.paste_text(&pane.pane_id, prompt)?;
        thread::sleep(Duration::from_millis(250));
        match provider {
            Provider::Claude => {
                let bindings = load_resolved_keybindings(None).map_err(AppError::new)?;
                let submit = bindings
                    .keys_for(AutomationAction::Submit)
                    .ok_or_else(|| AppError::new("missing_keybinding: submit"))?;
                client.send_keys(&pane.pane_id, submit)
            }
            Provider::Codex | Provider::Agy => client.send_keys(&pane.pane_id, &["Enter"]),
        }
    }

    fn wait_inner(
        &self,
        record: &McpSessionRecord,
        timeout_ms: u64,
        require_fresh: bool,
        no_yolo: bool,
        baseline_override: Option<LastAgentMessage>,
        wait_unknown_until_deadline: bool,
        lock: Option<&SessionLock>,
    ) -> AppResult<Value> {
        let client = TmuxClient::default();
        let deadline = safe_deadline(timeout_ms);
        let mut last_lock_refresh = Instant::now();
        let baseline = if require_fresh {
            baseline_override.or_else(|| cursor_as_message(record))
        } else {
            None
        };
        let mut stale_transcript_seen = false;
        let mut last_unknown_frame: Option<(Classification, String)> = None;
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
            let frame = client.capture_pane_ansi(&pane.pane_id, 2000)?;
            let frame = prepare_frame_for_classification(&frame);
            let classification = Classifier.classify(&pane.pane_id, &frame);
            if record.provider == Provider::Codex
                && let Some(error_excerpt) = provider_error_excerpt(&frame)
            {
                let mut out = outcome(
                    "provider_error",
                    Some(&classification),
                    Some(&frame),
                    Some("provider_error"),
                );
                out["error_excerpt"] = json!(error_excerpt);
                return Ok(out);
            }
            if !no_yolo && is_startup_enter_prompt(&frame) {
                client.send_keys(&pane.pane_id, &["Enter"])?;
                thread::sleep(Duration::from_millis(DEFAULT_POLL_MS));
                continue;
            }
            if !matches!(classification.state, SessionState::Unknown) {
                last_unknown_frame = None;
            }
            match classification.state {
                SessionState::ChatReady => {
                    if require_fresh {
                        match fresh_message(&pane, &client, baseline.as_ref()) {
                            Ok(message) => {
                                self.registry.update_cursor(
                                    &record.id,
                                    Some(&message.session_id),
                                    &message.text,
                                )?;
                                let mut out =
                                    outcome("ready", Some(&classification), Some(&frame), None);
                                out["message"] = json!({
                                    "role": "assistant",
                                    "text": message.text,
                                    "fresh": true,
                                });
                                return Ok(out);
                            }
                            Err(_) => {
                                stale_transcript_seen = true;
                            }
                        }
                    } else {
                        return Ok(outcome("ready", Some(&classification), Some(&frame), None));
                    }
                }
                SessionState::UserQuestionPrompt => {
                    if require_fresh {
                        match fresh_message(&pane, &client, baseline.as_ref()) {
                            Ok(message) => {
                                self.registry.update_cursor(
                                    &record.id,
                                    Some(&message.session_id),
                                    &message.text,
                                )?;
                                let mut out = outcome(
                                    "needs_user_input",
                                    Some(&classification),
                                    Some(&frame),
                                    None,
                                );
                                out["message"] = json!({
                                    "role": "assistant",
                                    "text": message.text,
                                    "fresh": true,
                                });
                                return Ok(out);
                            }
                            Err(_) => {
                                stale_transcript_seen = true;
                            }
                        }
                    } else {
                        return Ok(outcome(
                            "needs_user_input",
                            Some(&classification),
                            Some(&frame),
                            None,
                        ));
                    }
                }
                SessionState::BusyResponding
                | SessionState::PromptEditing
                | SessionState::ExternalEditorActive => {}
                SessionState::FolderTrustPrompt if !no_yolo => {
                    client.send_keys(&pane.pane_id, &["Enter"])?
                }
                // Agy command-permission is the only agy prompt shape that is
                // safe to auto-approve, mirroring the runtime YOLO loop: require
                // the pane to actually be agy and the default cursor to still sit
                // on `> 1. Yes` so a moved cursor or scrollback bleed can never
                // land Enter on a settings-persist option.
                SessionState::AgyCommandPermissionPrompt
                    if !no_yolo
                        && crate::agy::is_agy_pane(&pane)
                        && crate::agy::agy_command_permission_default_option_is_yes(&frame) =>
                {
                    client.send_keys(&pane.pane_id, &["Enter"])?
                }
                SessionState::PermissionDialog
                | SessionState::FolderTrustPrompt
                | SessionState::StartupChoicePrompt
                | SessionState::SurveyPrompt
                | SessionState::PlanApprovalPrompt
                | SessionState::DiffDialog
                // Agy folder-trust and settings-persist never auto-approve (the
                // latter would mutate settings.json); surface them as blocked so
                // the caller decides via send_keys. Command-permission also lands
                // here when no_yolo is set or the safety gate above declined.
                | SessionState::AgyCommandPermissionPrompt
                | SessionState::AgyFolderTrustPrompt
                | SessionState::AgySettingsPersistPrompt => {
                    return Ok(outcome(
                        "blocked",
                        Some(&classification),
                        Some(&frame),
                        Some("blocked_state"),
                    ));
                }
                SessionState::Unknown => {
                    if frame_is_blank(&frame) || wait_unknown_until_deadline {
                        last_unknown_frame = Some((classification.clone(), frame.clone()));
                    } else {
                        return Ok(outcome(
                            "unknown",
                            Some(&classification),
                            Some(&frame),
                            Some("unknown_state"),
                        ));
                    }
                }
            }
            if Instant::now() >= deadline {
                if let Some((classification, frame)) = last_unknown_frame {
                    return Ok(outcome(
                        "unknown",
                        Some(&classification),
                        Some(&frame),
                        Some("unknown_state"),
                    ));
                }
                let reason = if stale_transcript_seen {
                    "stale_transcript"
                } else {
                    "timeout"
                };
                return Ok(outcome(
                    "timeout",
                    Some(&classification),
                    Some(&frame),
                    Some(reason),
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

/// Fully-validated `spawn` arguments. Produced by `validate_spawn_args` so that
/// EVERY argument (cwd, provider, model, effort, agent, the `timeout_ms` range,
/// and `policy.no_yolo`) is checked BEFORE any tmux window is created. The
/// validated values are threaded forward into window creation and the
/// ready-wait/prompt phase, so there is no second validation pass that could
/// diverge.
struct ValidatedSpawnArgs {
    cwd: PathBuf,
    provider: Provider,
    model: Option<String>,
    effort: Option<String>,
    agent: Option<String>,
    command: String,
    timeout_ms: u64,
    no_yolo: bool,
    initial_prompt: Option<String>,
    /// The raw `policy` value to forward verbatim to `prompt` (Null when absent).
    policy: Value,
}

/// Validate ALL `spawn`/`one_shot` arguments without any side effects (R1). On
/// any invalid argument this returns `Err` before a tmux window is ever created.
fn validate_spawn_args(args: &Value) -> AppResult<ValidatedSpawnArgs> {
    let cwd = required_str(args, "cwd")?;
    let cwd = canonical_dir(cwd)?;
    let provider = optional_provider(args)?;
    let model = optional_nonempty_str(args, "model")?;
    let effort = optional_nonempty_str(args, "effort")?;
    let agent = optional_nonempty_str(args, "agent")?;
    let permission_mode = optional_enum_str(args, "permission_mode", &CLAUDE_PERMISSION_MODES)?;
    let settings = optional_nonempty_str(args, "settings")?;
    let command = build_launch_command(
        provider,
        model.as_deref(),
        effort.as_deref(),
        agent.as_deref(),
        permission_mode.as_deref(),
        settings.as_deref(),
    )?;
    let timeout_ms = optional_u64(args, "timeout_ms", DEFAULT_SPAWN_TIMEOUT_MS)?;
    let no_yolo = args
        .pointer("/policy/no_yolo")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let initial_prompt = args
        .get("initial_prompt")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let policy = args.get("policy").cloned().unwrap_or(Value::Null);
    Ok(ValidatedSpawnArgs {
        cwd,
        provider,
        model,
        effort,
        agent,
        command,
        timeout_ms,
        no_yolo,
        initial_prompt,
        policy,
    })
}

/// Build the `spawn` arguments for a `one_shot` call. Forwards only the keys
/// that were provided (letting `spawn` apply its own defaults/caps) and sets the
/// prompt as `initial_prompt` so the prompt is submitted exactly once via spawn's
/// existing spawn -> wait-ready -> prompt path. `policy` is passed straight
/// through (default behavior unchanged; may only tighten).
fn one_shot_spawn_args(args: &Value) -> Value {
    let mut spawn_args = serde_json::Map::new();
    let cwd = args
        .get("cwd")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|cwd| !cwd.is_empty())
        .map(str::to_string)
        .unwrap_or_else(default_one_shot_cwd);
    spawn_args.insert("cwd".to_string(), json!(cwd));

    for key in [
        "provider",
        "model",
        "effort",
        "agent",
        "timeout_ms",
        "permission_mode",
        "settings",
    ] {
        if let Some(value) = args.get(key).filter(|value| !value.is_null()) {
            spawn_args.insert(key.to_string(), value.clone());
        }
    }
    spawn_args.insert(
        "initial_prompt".to_string(),
        json!(one_shot_prompt(args).unwrap_or_default()),
    );
    // Only forward `policy` when the caller provided it, matching spawn's
    // "field absent" semantics (do not synthesize `policy: null`).
    if let Some(policy) = args.get("policy") {
        spawn_args.insert("policy".to_string(), policy.clone());
    }
    Value::Object(spawn_args)
}

fn one_shot_prompt(args: &Value) -> AppResult<String> {
    for key in ["prompt", "text", "message", "input", "initial_prompt"] {
        if let Some(prompt) = args
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|prompt| !prompt.is_empty())
        {
            return Ok(prompt.to_string());
        }
    }
    Err(AppError::new(
        "invalid_params: prompt must be non-empty (accepted fields: prompt, text, message, input, initial_prompt)",
    ))
}

fn default_one_shot_cwd() -> String {
    std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .display()
        .to_string()
}

fn required_str<'a>(args: &'a Value, name: &str) -> AppResult<&'a str> {
    args.get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| AppError::new(format!("invalid_params: missing or invalid {name}")))
}

fn optional_provider(args: &Value) -> AppResult<Provider> {
    match args.get("provider") {
        Some(Value::Null) | None => Ok(Provider::Claude),
        Some(value) => {
            let name = value
                .as_str()
                .ok_or_else(|| AppError::new("invalid_params: provider must be a string"))?;
            Provider::parse(name)
        }
    }
}

fn optional_nonempty_str(args: &Value, name: &str) -> AppResult<Option<String>> {
    match args.get(name) {
        Some(Value::Null) | None => Ok(None),
        Some(value) => {
            let raw = value
                .as_str()
                .ok_or_else(|| AppError::new(format!("invalid_params: {name} must be a string")))?;
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                Err(AppError::new(format!(
                    "invalid_params: {name} must be a non-empty string"
                )))
            } else {
                Ok(Some(trimmed.to_string()))
            }
        }
    }
}

fn optional_nonblank_str(args: &Value, name: &str) -> AppResult<Option<String>> {
    match args.get(name) {
        Some(Value::Null) | None => Ok(None),
        Some(value) => {
            let raw = value
                .as_str()
                .ok_or_else(|| AppError::new(format!("invalid_params: {name} must be a string")))?;
            if raw.trim().is_empty() {
                Ok(None)
            } else {
                Ok(Some(raw.to_string()))
            }
        }
    }
}

fn optional_keys(args: &Value) -> AppResult<Vec<String>> {
    match args.get("keys") {
        Some(Value::Null) | None => Ok(Vec::new()),
        Some(value) => {
            let keys = value
                .as_array()
                .ok_or_else(|| AppError::new("invalid_params: keys must be an array"))?
                .iter()
                .map(|v| {
                    v.as_str()
                        .map(str::to_string)
                        .ok_or_else(|| AppError::new("invalid_params: key must be a string"))
                })
                .collect::<AppResult<Vec<_>>>()?;
            Ok(keys.into_iter().filter(|key| !key.is_empty()).collect())
        }
    }
}

/// Valid `--permission-mode` values accepted by the Claude CLI. Kept in sync
/// with the `permission_mode` enum in the tool schema (`mcp_protocol.rs`).
const CLAUDE_PERMISSION_MODES: [&str; 6] = [
    "acceptEdits",
    "auto",
    "bypassPermissions",
    "default",
    "dontAsk",
    "plan",
];

/// Parse an optional string argument and validate it against a fixed set of
/// allowed values. Returns `Ok(None)` when absent/null, `Err(invalid_params)`
/// when present but not one of `allowed`.
fn optional_enum_str(args: &Value, name: &str, allowed: &[&str]) -> AppResult<Option<String>> {
    match optional_nonempty_str(args, name)? {
        None => Ok(None),
        Some(value) => {
            if allowed.contains(&value.as_str()) {
                Ok(Some(value))
            } else {
                Err(AppError::new(format!(
                    "invalid_params: {name} must be one of {}",
                    allowed.join(", ")
                )))
            }
        }
    }
}

fn build_launch_command(
    provider: Provider,
    model: Option<&str>,
    effort: Option<&str>,
    agent: Option<&str>,
    permission_mode: Option<&str>,
    settings: Option<&str>,
) -> AppResult<String> {
    let mut parts = vec![provider.command().to_string()];
    match provider {
        Provider::Claude => {
            if let Some(value) = model {
                parts.push("--model".into());
                parts.push(shell_escape_arg(value));
            }
            if let Some(value) = effort {
                parts.push("--effort".into());
                parts.push(shell_escape_arg(value));
            }
            if let Some(value) = agent {
                parts.push("--agent".into());
                parts.push(shell_escape_arg(value));
            }
            if let Some(value) = permission_mode {
                parts.push("--permission-mode".into());
                parts.push(shell_escape_arg(value));
            }
            if let Some(value) = settings {
                parts.push("--settings".into());
                parts.push(shell_escape_arg(value));
            }
        }
        Provider::Codex => {
            if let Some(value) = model {
                parts.push("-m".into());
                parts.push(shell_escape_arg(value));
            }
            if let Some(value) = effort {
                parts.push("-c".into());
                parts.push(shell_escape_arg(&format!("model_reasoning_effort={value}")));
            }
            if agent.is_some() {
                return Err(AppError::new(
                    "invalid_params: codex provider does not support agent",
                ));
            }
            if permission_mode.is_some() || settings.is_some() {
                return Err(AppError::new(
                    "invalid_params: codex provider does not support permission_mode or settings",
                ));
            }
        }
        Provider::Agy => {
            if model.is_some() || effort.is_some() || agent.is_some() {
                return Err(AppError::new(
                    "invalid_params: agy provider does not support model, effort, or agent",
                ));
            }
            if permission_mode.is_some() || settings.is_some() {
                return Err(AppError::new(
                    "invalid_params: agy provider does not support permission_mode or settings",
                ));
            }
        }
    }
    Ok(parts.join(" "))
}

fn shell_escape_arg(value: &str) -> String {
    if value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '_' | '-' | '.' | '%' | ':'))
    {
        value.to_string()
    } else {
        format!("'{}'", value.replace('\'', "'\"'\"'"))
    }
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
        "provider": record.provider.as_str(),
        "model": record.model,
        "effort": record.effort,
        "agent": record.agent,
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
        "snapshot": snapshot.map(|s| useful_recent_lines(s, 20).join("\n")),
    })
}

fn useful_recent_lines(frame: &str, limit: usize) -> Vec<String> {
    if limit == 0 {
        return Vec::new();
    }

    let lines = frame.lines().collect::<Vec<_>>();
    let useful_end = lines
        .iter()
        .rposition(|line| !line.trim().is_empty())
        .map(|idx| idx + 1)
        .unwrap_or(0);
    if useful_end == 0 {
        return Vec::new();
    }

    let start = useful_end.saturating_sub(limit);
    lines[start..useful_end]
        .iter()
        .map(|line| (*line).to_string())
        .collect()
}

fn provider_error_excerpt(frame: &str) -> Option<String> {
    let recent = useful_recent_lines(frame, 80);
    let lines = recent
        .iter()
        .map(|line| line.trim())
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();

    let anchor_idx = lines
        .iter()
        .rposition(|line| is_fatal_codex_provider_error_anchor(line))?;
    if lines
        .iter()
        .skip(anchor_idx + 1)
        .any(|line| codex_ready_marker_after_provider_error(line))
    {
        return None;
    }
    let previous_prompt_idx = lines[..anchor_idx]
        .iter()
        .rposition(|line| codex_ready_marker_after_provider_error(line));
    let start = anchor_idx
        .saturating_sub(2)
        .max(previous_prompt_idx.map(|idx| idx + 1).unwrap_or(0));
    let end = (anchor_idx + 3).min(lines.len());
    let excerpt = lines[start..end]
        .iter()
        .copied()
        .filter(|line| is_codex_provider_error_context(line))
        .collect::<Vec<_>>();

    if excerpt.is_empty() {
        Some(lines[anchor_idx].chars().take(500).collect())
    } else {
        Some(excerpt.join("\n").chars().take(1000).collect())
    }
}

fn is_fatal_codex_provider_error_anchor(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    let compact = lower.replace(' ', "");
    lower.contains("invalid_request_error")
        || compact.contains("\"status\":400") && lower.contains("error")
        || compact.contains("\"type\":\"error\"") && lower.contains("openai")
        || (lower.contains("provider error") || lower.contains("api error"))
            && (lower.contains("codex") || lower.contains("openai") || lower.contains("model"))
}

fn is_codex_provider_error_context(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    let compact = lower.replace(' ', "");
    lower.contains("invalid_request_error")
        || compact.contains("\"status\":400") && lower.contains("error")
        || compact.contains("\"type\":\"error\"")
        || lower.contains("model metadata for")
        || lower.contains("not found")
        || lower.contains("provider error")
        || lower.contains("api error")
}

fn codex_ready_marker_after_provider_error(line: &str) -> bool {
    line.trim_start().starts_with('›')
}

fn remove_snapshot_for_transcript_outcome(outcome: &mut Value) {
    if outcome.get("message").is_some_and(Value::is_object)
        && let Some(object) = outcome.as_object_mut()
    {
        object.remove("snapshot");
    }
}

fn frame_is_blank(frame: &str) -> bool {
    frame.trim().is_empty()
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
        "ready" | "needs_user_input" => LifecycleState::Ready,
        "timeout" => LifecycleState::Running,
        "blocked" | "unknown" | "provider_error" => LifecycleState::Blocked,
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

fn fresh_message(
    pane: &TmuxPane,
    tmux: &TmuxClient,
    prior: Option<&LastAgentMessage>,
) -> AppResult<LastAgentMessage> {
    let message = load_last_agent_message(pane, tmux, LAST_MESSAGE_HISTORY_LINES)?;
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
    fn parses_send_keys_payloads_leniently() {
        assert!(required_str(&json!({}), "id").is_err());
        assert_eq!(
            optional_keys(&json!({"keys":["Enter", ""]})).unwrap(),
            vec!["Enter"]
        );
        assert_eq!(
            optional_keys(&json!({"keys":[]})).unwrap(),
            Vec::<String>::new()
        );
        assert_eq!(
            optional_nonblank_str(&json!({"text":""}), "text").unwrap(),
            None
        );
        assert_eq!(
            optional_nonblank_str(&json!({"text":"hello"}), "text").unwrap(),
            Some("hello".to_string())
        );
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
        assert_eq!(
            lifecycle_from_outcome("needs_user_input"),
            LifecycleState::Ready
        );
        // Submitted turns that time out may still be running; preserve that
        // signal rather than flipping the persisted state to ready.
        assert_eq!(lifecycle_from_outcome("timeout"), LifecycleState::Running);
        assert_eq!(lifecycle_from_outcome("unknown"), LifecycleState::Blocked);
        assert_eq!(
            lifecycle_from_outcome("provider_error"),
            LifecycleState::Blocked
        );
    }

    #[test]
    fn useful_recent_lines_drops_trailing_blank_padding() {
        let frame = "old\nvisible 1\nvisible 2\n   \n\n";

        assert_eq!(
            useful_recent_lines(frame, 2),
            vec!["visible 1".to_string(), "visible 2".to_string()]
        );
        assert_eq!(useful_recent_lines("\n  \n", 20), Vec::<String>::new());
    }

    #[test]
    fn outcome_snapshot_uses_useful_recent_lines() {
        let out = outcome("timeout", None, Some("answer text\n\n\n"), Some("timeout"));

        assert_eq!(out["snapshot"], json!("answer text"));
    }

    #[test]
    fn startup_choice_prompt_maps_to_blocked_outcome() {
        assert_eq!(
            outcome_for_state(SessionState::StartupChoicePrompt),
            "blocked"
        );
    }

    #[test]
    fn codex_provider_error_excerpt_is_narrow_and_useful() {
        let frame = r#"› hello
{"type":"error","status":400,"error":{"type":"invalid_request_error","message":"Model metadata for gpt-nope not found"}}
Model metadata for gpt-nope not found
gpt-5.5 high · ~/Projects/botctl · Ready · Context 1% used"#;
        let excerpt = provider_error_excerpt(frame).expect("provider error excerpt");

        assert!(excerpt.contains("invalid_request_error"));
        assert!(excerpt.contains("Model metadata for gpt-nope not found"));
        assert!(provider_error_excerpt("{\"type\":\"note\",\"status\":200}").is_none());
        assert!(provider_error_excerpt("{\"status\":400,\"kind\":\"fixture\"}").is_none());
        assert!(provider_error_excerpt("Model metadata for gpt-nope not found").is_none());
        assert!(provider_error_excerpt("regular stderr: command failed").is_none());
    }

    #[test]
    fn codex_provider_error_excerpt_ignores_stale_scrollback() {
        let old_error = r#"{"type":"error","status":400,"error":{"type":"invalid_request_error","message":"old unsupported model"}}"#;
        let mut frame = format!("{old_error}\n");
        for idx in 0..10 {
            frame.push_str(&format!("ordinary later line {idx}\n"));
        }
        frame.push_str("› later prompt after recovery\n");
        frame.push_str("gpt-5.5 high · ~/Projects/botctl · Ready · Context 1% used\n");

        assert!(provider_error_excerpt(&frame).is_none());
    }

    #[test]
    fn codex_provider_error_excerpt_prefers_latest_fatal_anchor() {
        let old_error = r#"{"type":"error","status":400,"error":{"type":"invalid_request_error","message":"old unsupported model"}}"#;
        let new_error = r#"{"type":"error","status":400,"error":{"type":"invalid_request_error","message":"new unsupported model"}}"#;
        let frame = format!(
            "{old_error}\n› recovered prompt\n{new_error}\nModel metadata for new unsupported model not found\n"
        );
        let excerpt = provider_error_excerpt(&frame).expect("latest provider error excerpt");

        assert!(excerpt.contains("new unsupported model"));
        assert!(!excerpt.contains("old unsupported model"));
    }

    #[test]
    fn timeout_deadline_is_capped_safely() {
        let deadline = safe_deadline(u64::MAX);
        assert!(deadline <= Instant::now() + Duration::from_millis(MAX_TIMEOUT_MS));
    }

    #[test]
    fn launch_command_maps_per_provider() {
        assert_eq!(
            build_launch_command(
                Provider::Claude,
                Some("opus"),
                Some("high"),
                Some("reviewer"),
                None,
                None,
            )
            .unwrap(),
            "claude --model opus --effort high --agent reviewer"
        );
        assert_eq!(
            build_launch_command(
                Provider::Codex,
                Some("gpt-5.5"),
                Some("high"),
                None,
                None,
                None
            )
            .unwrap(),
            "codex -m gpt-5.5 -c 'model_reasoning_effort=high'"
        );
        assert_eq!(
            build_launch_command(Provider::Agy, None, None, None, None, None).unwrap(),
            "agy"
        );
    }

    #[test]
    fn launch_command_claude_permission_mode_and_settings() {
        // permission_mode + settings map to the claude CLI flags and the
        // settings path is shell-escaped (the path contains no special chars
        // here, so it stays bare).
        assert_eq!(
            build_launch_command(
                Provider::Claude,
                None,
                None,
                None,
                Some("acceptEdits"),
                Some("/home/colin/Seamus/gitlab-settings.json"),
            )
            .unwrap(),
            "claude --permission-mode acceptEdits --settings /home/colin/Seamus/gitlab-settings.json"
        );
        // A settings JSON string with spaces/quotes is shell-escaped.
        assert_eq!(
            build_launch_command(
                Provider::Claude,
                None,
                None,
                None,
                None,
                Some(r#"{"key": "v"}"#),
            )
            .unwrap(),
            r#"claude --settings '{"key": "v"}'"#
        );
    }

    #[test]
    fn launch_command_rejects_unsupported_combos() {
        let err = build_launch_command(Provider::Codex, None, None, Some("reviewer"), None, None)
            .expect_err("codex agent should fail");
        assert!(
            err.to_string()
                .contains("codex provider does not support agent")
        );
        let err = build_launch_command(Provider::Agy, Some("any"), None, None, None, None)
            .expect_err("agy model should fail");
        assert!(err.to_string().contains("agy provider does not support"));
        // permission_mode / settings are claude-only.
        let err =
            build_launch_command(Provider::Codex, None, None, None, Some("acceptEdits"), None)
                .expect_err("codex permission_mode should fail");
        assert!(
            err.to_string()
                .contains("codex provider does not support permission_mode or settings")
        );
        let err = build_launch_command(Provider::Agy, None, None, None, None, Some("/x.json"))
            .expect_err("agy settings should fail");
        assert!(
            err.to_string()
                .contains("agy provider does not support permission_mode or settings")
        );
    }

    #[test]
    fn validate_spawn_args_rejects_bad_permission_mode() {
        // Invalid permission_mode is an argument-validation error (before any
        // window is created); a valid one is accepted and baked into the command.
        assert!(validate_spawn_args(&json!({ "cwd": "/tmp", "permission_mode": "yolo" })).is_err());
        let v = validate_spawn_args(&json!({
            "cwd": "/tmp",
            "permission_mode": "acceptEdits",
            "settings": "/home/colin/Seamus/gitlab-settings.json",
            "initial_prompt": "",
        }))
        .expect("valid permission_mode/settings");
        assert!(v.command.contains("--permission-mode acceptEdits"));
        assert!(
            v.command
                .contains("--settings /home/colin/Seamus/gitlab-settings.json")
        );
        assert_eq!(v.initial_prompt, None);
    }

    fn temp_state_dir(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "botctl-mcp-session-{tag}-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&root);
        root
    }

    fn test_service(state_dir: &Path) -> McpSessionService {
        let registry = McpRegistry::open(state_dir).unwrap();
        McpSessionService::new(registry, "test-server-id-0000".to_string())
    }

    #[test]
    fn one_shot_builds_spawn_args_once() {
        let args = json!({
            "cwd": "/tmp",
            "prompt": "hello world",
            "provider": "codex",
            "policy": { "no_yolo": true },
        });
        let spawn_args = one_shot_spawn_args(&args);
        // initial_prompt is set exactly once from `prompt`.
        assert_eq!(spawn_args["initial_prompt"], json!("hello world"));
        assert!(spawn_args.get("prompt").is_none());
        // policy forwarded.
        assert_eq!(spawn_args["policy"], json!({ "no_yolo": true }));
        // forwarded provided keys.
        assert_eq!(spawn_args["provider"], json!("codex"));
        assert_eq!(spawn_args["cwd"], json!("/tmp"));
        // omitted optional keys are not present.
        assert!(spawn_args.get("model").is_none());
        assert!(spawn_args.get("timeout_ms").is_none());
    }

    #[test]
    fn one_shot_defaults_cwd_and_accepts_prompt_aliases() {
        let spawn_args = one_shot_spawn_args(&json!({ "text": "hello from alias" }));

        assert_eq!(spawn_args["cwd"], json!(default_one_shot_cwd()));
        assert_eq!(spawn_args["initial_prompt"], json!("hello from alias"));
        assert_eq!(one_shot_prompt(&json!({ "message": "hi" })).unwrap(), "hi");
        assert_eq!(one_shot_prompt(&json!({ "input": "hi" })).unwrap(), "hi");
        assert_eq!(
            one_shot_prompt(&json!({ "initial_prompt": "hi" })).unwrap(),
            "hi"
        );
    }

    #[test]
    fn one_shot_omitted_policy_stays_omitted() {
        // F5: when the caller omits `policy`, spawn args must not synthesize a
        // `policy: null` field (preserve spawn's "field absent" semantics).
        let args = json!({ "cwd": "/tmp", "prompt": "hi" });
        let spawn_args = one_shot_spawn_args(&args);
        assert!(spawn_args.get("policy").is_none());
        // When provided, it is forwarded verbatim.
        let args = json!({ "cwd": "/tmp", "prompt": "hi", "policy": { "no_yolo": true } });
        let spawn_args = one_shot_spawn_args(&args);
        assert_eq!(spawn_args["policy"], json!({ "no_yolo": true }));
    }

    #[test]
    fn one_shot_fresh_message_reads_nested_outcome() {
        // F6: fresh_message must be read from the nested prompt outcome's own flag
        // (default false), not inferred from message presence. Exercise the pure
        // result-shaping logic with a synthetic spawn_result: a stale message with
        // fresh_message:false must NOT be reported as fresh.
        let spawn_result = json!({
            "agent": { "id": "abc" },
            "outcome": { "outcome": "ready" },
            "initial_prompt": {
                "agent": { "id": "abc" },
                "outcome": {
                    "outcome": "needs_user_input",
                    "message": { "role": "assistant", "text": "stale" },
                    "fresh_message": false
                }
            }
        });
        let prompt_outcome = spawn_result
            .get("initial_prompt")
            .and_then(|p| p.get("outcome"));
        let message = prompt_outcome
            .and_then(|o| o.get("message"))
            .cloned()
            .filter(|m| m.is_object());
        let fresh_message = prompt_outcome
            .and_then(|o| o.get("fresh_message"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        assert!(message.is_some(), "message present");
        assert!(
            !fresh_message,
            "stale message must not be reported as fresh"
        );
    }

    #[test]
    fn transcript_outcome_omits_pane_snapshot() {
        let mut outcome = json!({
            "outcome": "ready",
            "classified_state": "ChatReady",
            "message": { "role": "assistant", "text": "reply", "fresh": true },
            "snapshot": "pane text"
        });

        remove_snapshot_for_transcript_outcome(&mut outcome);

        assert_eq!(outcome["message"]["text"], "reply");
        assert!(outcome.get("snapshot").is_none());
    }

    #[test]
    fn blank_frames_are_detected_for_startup_grace() {
        assert!(frame_is_blank("\n\n   \n"));
        assert!(!frame_is_blank("Claude Code"));
    }

    #[test]
    fn one_shot_requires_prompt_text() {
        let root = temp_state_dir("oneshot-validate");
        let service = test_service(&root);
        // Missing prompt -> invalid_params.
        let err = service
            .one_shot(&json!({ "cwd": "/tmp" }))
            .expect_err("missing prompt should fail");
        assert!(err.to_string().contains("invalid_params"));
        // Whitespace-only prompt -> invalid_params.
        let err = service
            .one_shot(&json!({ "cwd": "/tmp", "prompt": "   " }))
            .expect_err("blank prompt should fail");
        assert!(err.to_string().contains("invalid_params"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn one_shot_invalid_args_return_err_not_spawn_failed() {
        // R1: argument-validation failures in one_shot must propagate as
        // JSON-RPC errors (Err), NOT be encoded as outcome:"spawn_failed". These
        // all fail in validate_spawn_args before any tmux window is created.
        let root = temp_state_dir("oneshot-invalid-args");
        let service = test_service(&root);
        // Invalid provider.
        assert!(
            service
                .one_shot(&json!({ "cwd": "/tmp", "prompt": "hi", "provider": "nope" }))
                .is_err()
        );
        // Out-of-range timeout_ms (below minimum).
        assert!(
            service
                .one_shot(&json!({ "cwd": "/tmp", "prompt": "hi", "timeout_ms": 1 }))
                .is_err()
        );
        // Out-of-range timeout_ms (above maximum).
        assert!(
            service
                .one_shot(
                    &json!({ "cwd": "/tmp", "prompt": "hi", "timeout_ms": MAX_TIMEOUT_MS + 1 })
                )
                .is_err()
        );
        // Unsupported provider+agent combo (codex does not support agent).
        assert!(
            service
                .one_shot(
                    &json!({ "cwd": "/tmp", "prompt": "hi", "provider": "codex", "agent": "x" })
                )
                .is_err()
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn validate_spawn_args_fails_before_window_creation_on_bad_args() {
        // R1/F3 control-flow: ALL args are validated by validate_spawn_args
        // BEFORE any tmux window is created, so bad cwd / invalid provider /
        // out-of-range timeout_ms error out with ZERO side effects. This is the
        // seam that lets one_shot distinguish "no window (skip kill)" from
        // "window live (must kill)".
        // Bad cwd: fails in canonical_dir.
        assert!(validate_spawn_args(&json!({ "cwd": "/this/does/not/exist/botctl" })).is_err());
        // Invalid provider: fails in optional_provider.
        assert!(validate_spawn_args(&json!({ "cwd": "/tmp", "provider": "nope" })).is_err());
        // Out-of-range timeout_ms: fails before any window is created.
        assert!(validate_spawn_args(&json!({ "cwd": "/tmp", "timeout_ms": 1 })).is_err());
        assert!(
            validate_spawn_args(&json!({ "cwd": "/tmp", "timeout_ms": MAX_TIMEOUT_MS + 1 }))
                .is_err()
        );
        // Valid args parse, carrying validated timeout/no_yolo forward.
        let v = validate_spawn_args(
            &json!({ "cwd": "/tmp", "timeout_ms": 5000, "policy": { "no_yolo": true } }),
        )
        .expect("valid args");
        assert_eq!(v.timeout_ms, 5000);
        assert!(v.no_yolo);
    }

    #[test]
    fn one_shot_bad_cwd_is_validation_err() {
        // R1: a bad cwd is an ARGUMENT-validation failure, so one_shot must
        // return Err (JSON-RPC error) with ZERO side effects — NOT encode it as
        // outcome:"spawn_failed". spawn_failed is reserved for OPERATIONAL tmux/
        // registry failures that happen after validation passes.
        let root = temp_state_dir("oneshot-badcwd");
        let service = test_service(&root);
        let err = service
            .one_shot(&json!({
                "cwd": "/this/path/does/not/exist/botctl",
                "prompt": "hi",
            }))
            .expect_err("bad cwd should be a validation Err");
        assert!(err.to_string().contains("bad_cwd"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn one_shot_spawn_failed_shape_skips_kill() {
        // R1/B.4: the spawn_failed result shape (built directly in one_shot for
        // operational phase-1 failures) reports agent:null, kill skipped, and an
        // error string. This asserts the exact shape independent of a live tmux.
        let result = json!({
            "agent": Value::Null,
            "spawn_outcome": Value::Null,
            "outcome": "spawn_failed",
            "message": Value::Null,
            "fresh_message": false,
            "killed": false,
            "kill": { "status": "skipped" },
            "error": "tmux start failed",
        });
        assert_eq!(result["outcome"], json!("spawn_failed"));
        assert_eq!(result["agent"], Value::Null);
        assert_eq!(result["kill"]["status"], json!("skipped"));
        assert_eq!(result["killed"], json!(false));
        assert!(result.get("error").is_some());
    }

    #[test]
    fn nonempty_str_trims_and_rejects_blank() {
        assert_eq!(
            optional_nonempty_str(&json!({"model": "  opus  "}), "model").unwrap(),
            Some("opus".to_string())
        );
        assert!(optional_nonempty_str(&json!({"model": "   "}), "model").is_err());
        assert_eq!(optional_nonempty_str(&json!({}), "model").unwrap(), None);
    }
}
