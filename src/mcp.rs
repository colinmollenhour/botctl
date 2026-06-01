use std::path::{Path, PathBuf};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use serde_json::{Value, json};
use uuid::{ContextV7, Timestamp, Uuid};

use crate::app::{AppError, AppResult};
use crate::mcp_protocol::{
    JsonRpcRequest, PROTOCOL_VERSION, TOOL_NAMES, ToolAvailability, error, initialize_result,
    success, tools_list_result_for,
};
use crate::mcp_registry::{McpRegistry, Provider};
use crate::mcp_session::McpSessionService;
use crate::prompt::resolve_state_dir;

#[derive(Debug, Clone)]
pub struct McpService {
    sessions: McpSessionService,
    protocol_version: &'static str,
    tool_availability: ToolAvailability,
}

impl McpService {
    pub fn new(state_dir: Option<&Path>) -> AppResult<Self> {
        Self::new_with_protocol(state_dir, PROTOCOL_VERSION)
    }

    pub fn new_with_protocol(
        state_dir: Option<&Path>,
        protocol_version: &'static str,
    ) -> AppResult<Self> {
        let state_dir = resolve_state_dir(state_dir)?;
        let registry = McpRegistry::open(&state_dir)?;
        let server_id = Uuid::new_v7(Timestamp::now(ContextV7::new())).to_string();
        let tool_availability = detect_tool_availability();
        Ok(Self {
            sessions: McpSessionService::new(registry, server_id),
            protocol_version,
            tool_availability,
        })
    }

    #[cfg(test)]
    fn new_with_availability(
        state_dir: Option<&Path>,
        protocol_version: &'static str,
        tool_availability: ToolAvailability,
    ) -> AppResult<Self> {
        let state_dir = resolve_state_dir(state_dir)?;
        let registry = McpRegistry::open(&state_dir)?;
        let server_id = Uuid::new_v7(Timestamp::now(ContextV7::new())).to_string();
        Ok(Self {
            sessions: McpSessionService::new(registry, server_id),
            protocol_version,
            tool_availability,
        })
    }

    pub fn handle(&self, request: JsonRpcRequest) -> Option<Value> {
        let id = request.id.clone();
        if id.is_none() {
            // Per JSON-RPC, notifications have no id and receive no response.
            // Only dispatch methods that are defined as notifications; ignore any
            // request method (e.g. tools/call) arriving without an id so that
            // side-effecting calls cannot be smuggled through the notification path.
            if request.method.starts_with("notifications/") {
                let _ = self.handle_result(&request);
            }
            return None;
        }
        match self.handle_result(&request) {
            Ok(result) => Some(success(id, result)),
            Err(err) if err.to_string().starts_with("invalid_params:") => Some(error(
                id,
                -32602,
                "invalid_params",
                Some(json!({ "detail": err.to_string() })),
            )),
            Err(err) if err.to_string().starts_with("method_not_found:") => Some(error(
                id,
                -32601,
                "method_not_found",
                Some(json!({ "detail": err.to_string() })),
            )),
            Err(err) => Some(error(
                id,
                -32603,
                "internal_error",
                Some(json!({ "detail": err.to_string() })),
            )),
        }
    }

    fn handle_result(&self, request: &JsonRpcRequest) -> AppResult<Value> {
        match request.method.as_str() {
            "initialize" => Ok(initialize_result(self.protocol_version)),
            "tools/list" => Ok(tools_list_result_for(self.tool_availability)),
            "tools/call" => self.call_tool(&request.params),
            "notifications/initialized" => Ok(json!({})),
            other => Err(AppError::with_exit_code(
                format!("method_not_found: {other}"),
                -32601,
            )),
        }
    }

    fn call_tool(&self, params: &Value) -> AppResult<Value> {
        let name = params
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| AppError::new("invalid_params: missing tool name"))?;
        if !TOOL_NAMES.contains(&name) {
            return Err(AppError::new(format!(
                "invalid_params: unknown tool {name}"
            )));
        }
        if !self.tool_availability.tool_names().contains(&name) {
            return Err(AppError::new(format!(
                "invalid_params: unavailable tool {name}"
            )));
        }
        let args = params
            .get("arguments")
            .cloned()
            .unwrap_or_else(|| json!({}));
        let result = match name {
            "spawn_claude" => self
                .sessions
                .spawn(&spawn_args_for_provider(&args, "claude")?),
            "spawn_codex" => self
                .sessions
                .spawn(&spawn_args_for_provider(&args, "codex")?),
            "spawn_agy" => self.sessions.spawn(&spawn_args_for_provider(&args, "agy")?),
            "prompt" => self.sessions.prompt(&args),
            "wait" => self.sessions.wait(&args),
            "kill" => self.sessions.kill(&args),
            "snapshot" => self.sessions.snapshot(&args),
            "send_keys" => self.sessions.send_keys(&args),
            "one_shot" => self
                .sessions
                .one_shot(&one_shot_args_with_defaults(&args, self.tool_availability)?),
            _ => unreachable!(),
        }?;
        let content_text = tool_content_text(name, &result);
        Ok(json!({
            "content": [{ "type": "text", "text": content_text }],
            "structuredContent": result,
            "isError": false,
        }))
    }
}

fn detect_tool_availability() -> ToolAvailability {
    ToolAvailability {
        claude: command_available(Provider::Claude.command()),
        codex: command_available(Provider::Codex.command()),
        agy: command_available(Provider::Agy.command()),
    }
}

fn command_available(command: &str) -> bool {
    std::env::var_os("PATH")
        .map(|path| {
            std::env::split_paths(&path).any(|dir| {
                command_candidates(&dir, command)
                    .into_iter()
                    .any(is_executable_file)
            })
        })
        .unwrap_or(false)
}

#[cfg(unix)]
fn command_candidates(dir: &Path, command: &str) -> Vec<PathBuf> {
    vec![dir.join(command)]
}

#[cfg(windows)]
fn command_candidates(dir: &Path, command: &str) -> Vec<PathBuf> {
    let mut candidates = vec![dir.join(command)];
    if Path::new(command).extension().is_some() {
        return candidates;
    }
    if let Some(pathext) = std::env::var_os("PATHEXT") {
        candidates.extend(
            pathext
                .to_string_lossy()
                .split(';')
                .map(str::trim)
                .filter(|ext| !ext.is_empty())
                .map(|ext| dir.join(format!("{command}{ext}"))),
        );
    }
    candidates
}

#[cfg(unix)]
fn is_executable_file(path: PathBuf) -> bool {
    path.metadata()
        .map(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(windows)]
fn is_executable_file(path: PathBuf) -> bool {
    path.is_file()
}

fn spawn_args_for_provider(args: &Value, provider: &str) -> AppResult<Value> {
    let mut args = args.as_object().cloned().unwrap_or_default();
    args.remove("initial_prompt");
    args.insert("provider".into(), json!(provider));
    apply_model_defaults(&mut args, provider)?;
    Ok(Value::Object(args))
}

fn one_shot_args_with_defaults(args: &Value, availability: ToolAvailability) -> AppResult<Value> {
    let mut args = args.as_object().cloned().unwrap_or_default();
    let mut selected_provider = match args.get("provider") {
        Some(Value::Null) | None => None,
        Some(Value::String(provider)) if provider.trim().is_empty() => None,
        Some(Value::String(provider)) => Some(provider.trim().to_string()),
        Some(_) => return Ok(Value::Object(args)),
    };
    if selected_provider.is_none() {
        if availability.claude {
            args.insert("provider".into(), json!("claude"));
            selected_provider = Some("claude".to_string());
        } else if availability.codex {
            args.insert("provider".into(), json!("codex"));
            selected_provider = Some("codex".to_string());
        } else if availability.agy {
            args.insert("provider".into(), json!("agy"));
            selected_provider = Some("agy".to_string());
        } else {
            return Err(AppError::new(
                "invalid_params: no available provider binaries found",
            ));
        }
    }
    if let Some(provider) = selected_provider.as_deref() {
        if !provider_available(availability, provider) {
            return Err(AppError::new(format!(
                "invalid_params: unavailable provider {provider}"
            )));
        }
        apply_model_defaults(&mut args, provider)?;
    }
    Ok(Value::Object(args))
}

fn provider_available(availability: ToolAvailability, provider: &str) -> bool {
    match provider {
        "claude" => availability.claude,
        "codex" => availability.codex,
        "agy" => availability.agy,
        _ => true,
    }
}

fn apply_model_defaults(
    args: &mut serde_json::Map<String, Value>,
    provider: &str,
) -> AppResult<()> {
    let Some(model) = resolve_model(args, provider) else {
        args.remove("model_preset");
        return Ok(());
    };
    args.insert("model".into(), json!(model?));
    args.remove("model_preset");
    Ok(())
}

fn resolve_model(
    args: &serde_json::Map<String, Value>,
    provider: &str,
) -> Option<AppResult<&'static str>> {
    if args
        .get("model")
        .and_then(Value::as_str)
        .is_some_and(|model| !model.trim().is_empty())
    {
        return None;
    }
    match provider {
        "claude" => Some(model_preset(args).map(claude_model_for_preset)),
        "codex" => Some(model_preset(args).map(codex_model_for_preset)),
        _ => None,
    }
}

fn model_preset(args: &serde_json::Map<String, Value>) -> AppResult<&str> {
    match args.get("model_preset") {
        Some(Value::Null) | None => Ok("best"),
        Some(value) => {
            let preset = value
                .as_str()
                .ok_or_else(|| AppError::new("invalid_params: model_preset must be a string"))?
                .trim();
            if preset.is_empty() {
                return Ok("best");
            }
            if matches!(preset, "best" | "balanced" | "fast" | "cheap") {
                Ok(preset)
            } else {
                Err(AppError::new(
                    "invalid_params: model_preset must be one of best, balanced, fast, cheap",
                ))
            }
        }
    }
}

fn claude_model_for_preset(preset: &str) -> &'static str {
    match preset {
        "best" => "opus",
        "balanced" => "sonnet",
        "fast" | "cheap" => "haiku",
        _ => "opus",
    }
}

fn codex_model_for_preset(preset: &str) -> &'static str {
    match preset {
        "best" | "balanced" | "fast" | "cheap" => "gpt-5.5",
        _ => "gpt-5.5",
    }
}

fn tool_content_text(name: &str, result: &Value) -> String {
    match name {
        "prompt" => result
            .pointer("/outcome/message/text")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .unwrap_or_else(|| pretty_json(result)),
        "one_shot" => result
            .pointer("/message/text")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .unwrap_or_else(|| pretty_json(result)),
        _ => pretty_json(result),
    }
}

fn pretty_json(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp_protocol::JsonRpcRequest;

    #[test]
    fn handles_initialize_and_tools_list() {
        let root = std::env::temp_dir().join(format!("botctl-mcp-service-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let service = McpService::new_with_availability(
            Some(&root),
            PROTOCOL_VERSION,
            ToolAvailability::all(),
        )
        .unwrap();
        let init = service.handle(JsonRpcRequest {
            jsonrpc: Some("2.0".into()),
            id: Some(json!(1)),
            method: "initialize".into(),
            params: json!({}),
        });
        let init = init.unwrap();
        assert_eq!(init["result"]["serverInfo"]["name"], "botctl");
        let tools = service.handle(JsonRpcRequest {
            jsonrpc: Some("2.0".into()),
            id: Some(json!(2)),
            method: "tools/list".into(),
            params: json!({}),
        });
        let tools = tools.unwrap();
        assert_eq!(tools["result"]["tools"].as_array().unwrap().len(), 9);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn tools_list_hides_unavailable_provider_spawns() {
        let root = std::env::temp_dir().join(format!(
            "botctl-mcp-service-availability-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let service = McpService::new_with_availability(
            Some(&root),
            PROTOCOL_VERSION,
            ToolAvailability {
                claude: true,
                codex: false,
                agy: false,
            },
        )
        .unwrap();

        let tools = service
            .handle(JsonRpcRequest {
                jsonrpc: Some("2.0".into()),
                id: Some(json!(2)),
                method: "tools/list".into(),
                params: json!({}),
            })
            .unwrap();
        let names = tools["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert!(names.contains(&"spawn_claude"));
        assert!(!names.contains(&"spawn_codex"));
        assert!(!names.contains(&"spawn_agy"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn unavailable_provider_spawns_are_not_callable() {
        let root = std::env::temp_dir().join(format!(
            "botctl-mcp-service-unavailable-call-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let service = McpService::new_with_availability(
            Some(&root),
            PROTOCOL_VERSION,
            ToolAvailability {
                claude: true,
                codex: false,
                agy: true,
            },
        )
        .unwrap();

        let response = service
            .handle(JsonRpcRequest {
                jsonrpc: Some("2.0".into()),
                id: Some(json!(3)),
                method: "tools/call".into(),
                params: json!({ "name": "spawn_codex", "arguments": { "cwd": "/tmp" } }),
            })
            .unwrap();
        assert_eq!(response["error"]["code"], -32602);
        assert!(
            response["error"]["data"]["detail"]
                .as_str()
                .unwrap()
                .contains("unavailable tool spawn_codex")
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn http_service_advertises_2025_version() {
        let root =
            std::env::temp_dir().join(format!("botctl-mcp-service-http-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let service =
            McpService::new_with_protocol(Some(&root), crate::mcp_protocol::HTTP_PROTOCOL_VERSION)
                .unwrap();
        let init = service
            .handle(JsonRpcRequest {
                jsonrpc: Some("2.0".into()),
                id: Some(json!(1)),
                method: "initialize".into(),
                params: json!({}),
            })
            .unwrap();
        assert_eq!(init["result"]["protocolVersion"], "2025-03-26");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn maps_missing_tool_params_to_invalid_params() {
        let root =
            std::env::temp_dir().join(format!("botctl-mcp-service-params-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let service = McpService::new(Some(&root)).unwrap();
        let response = service.handle(JsonRpcRequest {
            jsonrpc: Some("2.0".into()),
            id: Some(json!(3)),
            method: "tools/call".into(),
            params: json!({ "name": "send_keys", "arguments": {} }),
        });
        let response = response.unwrap();
        assert_eq!(response["error"]["code"], -32602);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn notifications_emit_no_response() {
        let root =
            std::env::temp_dir().join(format!("botctl-mcp-service-notify-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let service = McpService::new(Some(&root)).unwrap();
        let response = service.handle(JsonRpcRequest {
            jsonrpc: Some("2.0".into()),
            id: None,
            method: "notifications/initialized".into(),
            params: json!({}),
        });
        assert!(response.is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn tools_call_without_id_is_not_dispatched() {
        let root = std::env::temp_dir().join(format!(
            "botctl-mcp-service-notify-call-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let service = McpService::new(Some(&root)).unwrap();
        // A tools/call sent without an id is not a defined notification.
        // The server must drop it rather than running the underlying tool,
        // since notifications never return errors and would otherwise allow
        // side-effecting calls (spawn, kill, send_keys) to be smuggled in.
        let response = service.handle(JsonRpcRequest {
            jsonrpc: Some("2.0".into()),
            id: None,
            method: "tools/call".into(),
            params: json!({ "name": "send_keys", "arguments": { "id": "missing" } }),
        });
        assert!(response.is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn provider_spawn_tools_force_provider() {
        let args = spawn_args_for_provider(
            &json!({ "cwd": "/tmp", "provider": "codex", "model": "sonnet", "initial_prompt": "ignored" }),
            "claude",
        )
        .unwrap();

        assert_eq!(args["provider"], "claude");
        assert_eq!(args["cwd"], "/tmp");
        assert_eq!(args["model"], "sonnet");
        assert!(args.get("initial_prompt").is_none());
    }

    #[test]
    fn provider_spawns_apply_model_presets_and_defaults() {
        let args = spawn_args_for_provider(&json!({ "cwd": "/tmp" }), "codex").unwrap();
        assert_eq!(args["model"], "gpt-5.5");

        let args = spawn_args_for_provider(
            &json!({ "cwd": "/tmp", "model_preset": "balanced" }),
            "claude",
        )
        .unwrap();
        assert_eq!(args["model"], "sonnet");
        assert!(args.get("model_preset").is_none());

        let args = spawn_args_for_provider(&json!({ "cwd": "/tmp" }), "agy").unwrap();
        assert!(args.get("model").is_none());
        assert!(args.get("model_preset").is_none());
    }

    #[test]
    fn one_shot_defaults_provider_to_available_binary() {
        let args = one_shot_args_with_defaults(
            &json!({ "prompt": "hi" }),
            ToolAvailability {
                claude: false,
                codex: true,
                agy: true,
            },
        )
        .unwrap();
        assert_eq!(args["provider"], "codex");

        let err = one_shot_args_with_defaults(
            &json!({ "prompt": "hi", "provider": "agy" }),
            ToolAvailability {
                claude: true,
                codex: true,
                agy: false,
            },
        )
        .expect_err("explicit unavailable provider should fail");
        assert!(err.to_string().contains("unavailable provider agy"));
    }

    #[test]
    fn one_shot_defaults_model_after_provider_selection() {
        let args = one_shot_args_with_defaults(
            &json!({ "prompt": "hi" }),
            ToolAvailability {
                claude: false,
                codex: true,
                agy: true,
            },
        )
        .unwrap();
        assert_eq!(args["provider"], "codex");
        assert_eq!(args["model"], "gpt-5.5");

        let args = one_shot_args_with_defaults(
            &json!({ "prompt": "hi", "provider": "codex", "model": "custom" }),
            ToolAvailability::all(),
        )
        .unwrap();
        assert_eq!(args["model"], "custom");
    }

    #[test]
    fn one_shot_rejects_when_no_provider_is_available() {
        let err = one_shot_args_with_defaults(
            &json!({ "prompt": "hi" }),
            ToolAvailability {
                claude: false,
                codex: false,
                agy: false,
            },
        )
        .expect_err("missing provider with no available binary should fail");

        assert!(err.to_string().contains("no available provider binaries"));
    }

    #[test]
    fn model_preset_rejects_invalid_values() {
        let err = spawn_args_for_provider(
            &json!({ "cwd": "/tmp", "model_preset": "surprise" }),
            "claude",
        )
        .expect_err("unknown model preset should fail");
        assert!(err.to_string().contains("model_preset must be one of"));

        let err = spawn_args_for_provider(&json!({ "cwd": "/tmp", "model_preset": 3 }), "codex")
            .expect_err("non-string model preset should fail");
        assert!(err.to_string().contains("model_preset must be a string"));
    }

    #[test]
    fn prompt_tool_content_is_assistant_text() {
        let result = json!({
            "agent": { "id": "agent-1" },
            "outcome": {
                "outcome": "ready",
                "message": { "role": "assistant", "text": "reply only", "fresh": true },
                "snapshot": "pane text should not be surfaced as tool content"
            }
        });

        assert_eq!(tool_content_text("prompt", &result), "reply only");
    }

    #[test]
    fn one_shot_tool_content_is_assistant_text() {
        let result = json!({
            "outcome": "ready",
            "message": { "role": "assistant", "text": "one shot reply", "fresh": true }
        });

        assert_eq!(tool_content_text("one_shot", &result), "one shot reply");
    }
}
