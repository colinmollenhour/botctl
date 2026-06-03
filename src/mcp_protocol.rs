use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub const PROTOCOL_VERSION: &str = "2024-11-05";
pub const HTTP_PROTOCOL_VERSION: &str = "2025-03-26";
pub const TOOL_NAMES: [&str; 9] = [
    "spawn_claude",
    "spawn_codex",
    "spawn_agy",
    "prompt",
    "wait",
    "kill",
    "snapshot",
    "send_keys",
    "one_shot",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolAvailability {
    pub claude: bool,
    pub codex: bool,
    pub agy: bool,
}

impl ToolAvailability {
    pub fn all() -> Self {
        Self {
            claude: true,
            codex: true,
            agy: true,
        }
    }

    pub fn tool_names(self) -> Vec<&'static str> {
        let mut names = Vec::new();
        if self.claude {
            names.push("spawn_claude");
        }
        if self.codex {
            names.push("spawn_codex");
        }
        if self.agy {
            names.push("spawn_agy");
        }
        names.extend([
            "prompt",
            "wait",
            "kill",
            "snapshot",
            "send_keys",
            "one_shot",
        ]);
        names
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: Option<String>,
    pub id: Option<Value>,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

#[derive(Debug, Clone, Serialize)]
pub struct JsonRpcErrorBody {
    pub code: i64,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

pub fn success(id: Option<Value>, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id.unwrap_or(Value::Null), "result": result })
}

pub fn error(
    id: Option<Value>,
    code: i64,
    message: impl Into<String>,
    data: Option<Value>,
) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id.unwrap_or(Value::Null),
        "error": JsonRpcErrorBody { code, message: message.into(), data },
    })
}

pub fn initialize_result(protocol_version: &str) -> Value {
    json!({
        "protocolVersion": protocol_version,
        "capabilities": { "tools": {} },
        "serverInfo": { "name": "botctl", "version": env!("CARGO_PKG_VERSION") }
    })
}

/// Validate the `MCP-Protocol-Version` header for an HTTP request. Only invoked
/// by the HTTP transport (D3); stdio never calls it.
/// - `initialize` always succeeds (the client does not know the version yet).
/// - An absent header succeeds (assume the advertised `HTTP_PROTOCOL_VERSION`).
/// - A header equal to `HTTP_PROTOCOL_VERSION` succeeds; any other value fails.
// The unit error is intentional: callers only branch on ok/err and map the
// failure to a fixed transport-level 400, so a richer error type adds no value.
#[allow(clippy::result_unit_err)]
pub fn validate_protocol_version(method: &str, header: Option<&str>) -> Result<(), ()> {
    if method == "initialize" {
        return Ok(());
    }
    match header {
        None => Ok(()),
        Some(v) if v == HTTP_PROTOCOL_VERSION => Ok(()),
        Some(_) => Err(()),
    }
}

pub fn tools_list_result() -> Value {
    tools_list_result_for(ToolAvailability::all())
}

pub fn tools_list_result_for(availability: ToolAvailability) -> Value {
    json!({ "tools": tool_catalog_for(availability) })
}

pub fn tool_catalog() -> Vec<Value> {
    tool_catalog_for(ToolAvailability::all())
}

pub fn tool_catalog_for(availability: ToolAvailability) -> Vec<Value> {
    let mut tools = Vec::new();
    if availability.claude {
        tools.push(tool(
            "spawn_claude",
            "Start a persistent Claude TUI in a managed tmux window.",
            json!({
                "type": "object", "required": ["cwd"],
                "properties": {
                    "cwd": {"type":"string", "description":"Existing working directory for the managed agent."},
                    "model": raw_model_schema("Claude"),
                    "model_preset": model_preset_schema("Claude"),
                    "effort": {"type":"string", "enum": ["low", "medium", "high", "xhigh", "max"]},
                    "agent": claude_agent_schema(),
                    "permission_mode": permission_mode_schema(),
                    "settings": {"type":"string", "minLength":1, "description":"Settings JSON file path or JSON string passed to Claude --settings."},
                    "timeout_ms": {"type":"integer", "minimum":1000},
                    "policy": policy_schema()
                }
            }),
        ));
    }
    if availability.codex {
        tools.push(tool(
            "spawn_codex",
            "Start a persistent Codex TUI in a managed tmux window.",
            json!({
                "type": "object", "required": ["cwd"],
                "properties": {
                    "cwd": {"type":"string", "description":"Existing working directory for the managed agent."},
                    "model": raw_model_schema("Codex"),
                    "model_preset": model_preset_schema("Codex"),
                    "effort": {"type":"string", "enum": ["low", "medium", "high", "xhigh", "max"]},
                    "timeout_ms": {"type":"integer", "minimum":1000},
                    "policy": policy_schema()
                }
            }),
        ));
    }
    if availability.agy {
        tools.push(tool(
            "spawn_agy",
            "Start a persistent agy/Antigravity TUI in a managed tmux window.",
            json!({
                "type": "object", "required": ["cwd"],
                "properties": {
                    "cwd": {"type":"string", "description":"Existing working directory for the managed agent."},
                    "timeout_ms": {"type":"integer", "minimum":1000},
                    "policy": policy_schema()
                }
            }),
        ));
    }
    tools.extend([
        tool(
            "prompt",
            "Primary tool for sending a natural-language prompt/message to an existing managed session. Use this for asking the agent to do work; waits for a terminal outcome and returns the transcript-backed reply while keeping the session alive. If the managed pane is killed/dead/missing, prompt may resurrect the same registry id before submission.",
            json!({
                "type": "object", "required": ["id", "prompt"],
                "properties": { "id": {"type":"string"}, "prompt": {"type":"string"}, "timeout_ms": {"type":"integer", "minimum":1000}, "policy": policy_schema() }
            }),
        ),
        tool(
            "wait",
            "Wait for a managed session to reach a terminal state.",
            json!({
                "type":"object", "required":["id"],
                "properties": { "id":{"type":"string"}, "timeout_ms":{"type":"integer", "minimum":1000}, "require_fresh_message":{"type":"boolean"} }
            }),
        ),
        tool(
            "kill",
            "Safely kill only the verified managed tmux window.",
            json!({
                "type":"object", "required":["id"],
                "properties": { "id":{"type":"string"}, "timeout_ms":{"type":"integer", "minimum":1000} }
            }),
        ),
        tool(
            "snapshot",
            "Capture and classify the current managed pane. Does not resurrect missing panes. Responses may include outcome.blocked_reason and agent.command_health for managed Codex panes.",
            json!({
                "type":"object", "required":["id"],
                "properties": { "id":{"type":"string"}, "capture_lines":{"type":"integer", "minimum":1, "maximum":5000} }
            }),
        ),
        tool(
            "send_keys",
            "Low-level raw tmux escape hatch only. Do not use for normal prompts/questions; use the prompt tool instead. Sends raw keys or pasted text only, does not wait for a reply, and no progress is implied.",
            json!({
                "type":"object", "required":["id"],
                "properties": { "id":{"type":"string"}, "keys":{"type":"array", "items":{"type":"string"}}, "text":{"type":"string"}, "paste":{"type":"boolean"} }
            }),
        ),
        tool(
            "one_shot",
            "Create a temporary managed session, run exactly one prompt to a terminal outcome, then always attempt to kill the window (best-effort cleanup). Uses managed auto-approval (no_yolo=false): only folder-trust and gated agy command-permission prompts auto-advance; all other approvals block.",
            json!({
                "type": "object",
                "properties": {
                    "cwd": {"type":"string", "description":"Existing working directory for the managed agent. Defaults to the MCP server current directory when omitted or blank."},
                    "prompt": {"type":"string", "minLength":1, "description":"Preferred prompt text field. The aliases text, message, input, and initial_prompt are also accepted at runtime."},
                    "text": {"type":"string", "minLength":1, "description":"Alias for prompt."},
                    "message": {"type":"string", "minLength":1, "description":"Alias for prompt."},
                    "input": {"type":"string", "minLength":1, "description":"Alias for prompt."},
                    "provider": {"type":"string", "enum": ["claude", "codex", "agy"], "description":"Agent provider to launch. Defaults to the first available provider binary in claude, codex, agy order. If agy, omit model/effort/agent/permission_mode/settings."},
                    "model": raw_model_schema("Claude/Codex"),
                    "model_preset": model_preset_schema("Claude/Codex"),
                    "effort": {"type":"string", "enum": ["low", "medium", "high", "xhigh", "max"], "description":"Claude and Codex only. Do not pass when provider is agy."},
                    "agent": claude_agent_schema(),
                    "permission_mode": permission_mode_schema(),
                    "settings": {"type":"string", "minLength":1, "description":"Claude only. Do not pass when provider is codex or agy."},
                    "timeout_ms": {"type":"integer", "minimum":1000},
                    "policy": policy_schema()
                }
            }),
        ),
    ]);
    tools
}

fn policy_schema() -> Value {
    json!({ "type":"object", "properties": { "no_yolo": { "type":"boolean" } } })
}

fn model_preset_schema(provider: &str) -> Value {
    json!({
        "type": "string",
        "enum": ["best", "balanced", "fast", "cheap"],
        "description": format!("Optional {provider} model preference. Leave unset unless the user explicitly asks for a model speed/cost/quality preference. Do not pass both model and model_preset; use model only for an exact downstream provider model override. Omitting both model and model_preset uses botctl's default.")
    })
}

fn raw_model_schema(provider: &str) -> Value {
    let provider_note = if provider == "Claude/Codex" {
        "Must be valid for the selected provider. Do not pass when provider is agy."
    } else {
        "Must be valid for this provider."
    };
    json!({
        "type": "string",
        "minLength": 1,
        "description": format!("Advanced raw {provider} model override. Leave unset unless the user explicitly requested an exact downstream provider model. Do not pass both model and model_preset; if the user only asks for best/balanced/fast/cheap, use model_preset instead. Omitting both model and model_preset uses botctl's default. {provider_note} Do not copy the caller/orchestrator model ID into this field; for example, openai/gpt-5.5 is not a Claude model.")
    })
}

fn claude_agent_schema() -> Value {
    json!({
        "type": "string",
        "minLength": 1,
        "description": "Claude only. Do not pass for normal Claude sessions; omitting agent lets Claude use its default build agent. Only pass when the user explicitly requests a specific Claude subagent. Do not pass when provider is codex or agy."
    })
}

/// Schema for the claude-only `--permission-mode` flag. Kept in sync with
/// `CLAUDE_PERMISSION_MODES` in `mcp_session.rs`.
fn permission_mode_schema() -> Value {
    json!({
        "type": "string",
        "enum": ["acceptEdits", "auto", "bypassPermissions", "default", "dontAsk", "plan"],
        "description": "Claude only. Do not pass when provider is codex or agy."
    })
}

fn tool(name: &str, description: &str, input_schema: Value) -> Value {
    json!({ "name": name, "description": description, "inputSchema": input_schema })
}

pub fn parse_request(line: &str) -> Result<JsonRpcRequest, Value> {
    let value: Value = serde_json::from_str(line).map_err(|err| {
        error(
            None,
            -32700,
            "parse_error",
            Some(json!({ "detail": err.to_string() })),
        )
    })?;
    let request: JsonRpcRequest = serde_json::from_value(value).map_err(|err| {
        error(
            None,
            -32600,
            "invalid_request",
            Some(json!({ "detail": err.to_string() })),
        )
    })?;
    if request.jsonrpc.as_deref() != Some("2.0") {
        return Err(error(
            request.id,
            -32600,
            "invalid_request",
            Some(json!({"detail":"jsonrpc must be 2.0"})),
        ));
    }
    Ok(request)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_list_has_final_names() {
        let names = tool_catalog()
            .into_iter()
            .map(|tool| tool["name"].as_str().unwrap().to_string())
            .collect::<Vec<_>>();
        assert_eq!(names, TOOL_NAMES.map(str::to_string));
    }

    #[test]
    fn tool_catalog_can_hide_unavailable_spawn_tools() {
        let names = tool_catalog_for(ToolAvailability {
            claude: false,
            codex: true,
            agy: false,
        })
        .into_iter()
        .map(|tool| tool["name"].as_str().unwrap().to_string())
        .collect::<Vec<_>>();

        assert_eq!(
            names,
            [
                "spawn_codex",
                "prompt",
                "wait",
                "kill",
                "snapshot",
                "send_keys",
                "one_shot",
            ]
            .map(str::to_string)
        );
    }

    #[test]
    fn tool_catalog_includes_one_shot() {
        let catalog = tool_catalog();
        let one_shot = catalog
            .iter()
            .find(|t| t["name"] == "one_shot")
            .expect("catalog has one_shot");
        assert!(one_shot["inputSchema"].get("required").is_none());
        assert_eq!(catalog.last().unwrap()["name"], "one_shot");
    }

    #[test]
    fn provider_spawn_schemas_are_narrow() {
        let catalog = tool_catalog();
        let spawn_claude = catalog
            .iter()
            .find(|t| t["name"] == "spawn_claude")
            .expect("catalog has spawn_claude");
        let spawn_codex = catalog
            .iter()
            .find(|t| t["name"] == "spawn_codex")
            .expect("catalog has spawn_codex");
        let spawn_agy = catalog
            .iter()
            .find(|t| t["name"] == "spawn_agy")
            .expect("catalog has spawn_agy");

        assert!(
            spawn_claude["inputSchema"]["properties"]
                .get("provider")
                .is_none()
        );
        assert!(
            spawn_codex["inputSchema"]["properties"]
                .get("provider")
                .is_none()
        );
        assert!(
            spawn_agy["inputSchema"]["properties"]
                .get("provider")
                .is_none()
        );
        assert!(
            spawn_claude["inputSchema"]["properties"]
                .get("initial_prompt")
                .is_none()
        );
        assert!(
            spawn_codex["inputSchema"]["properties"]
                .get("initial_prompt")
                .is_none()
        );
        assert!(
            spawn_agy["inputSchema"]["properties"]
                .get("initial_prompt")
                .is_none()
        );
        assert!(
            spawn_codex["inputSchema"]["properties"]
                .get("agent")
                .is_none()
        );
        assert!(
            spawn_codex["inputSchema"]["properties"]
                .get("permission_mode")
                .is_none()
        );
        assert!(
            spawn_codex["inputSchema"]["properties"]
                .get("settings")
                .is_none()
        );
        assert!(
            spawn_agy["inputSchema"]["properties"]
                .get("model")
                .is_none()
        );
        assert!(
            spawn_claude["inputSchema"]["properties"]
                .get("model_preset")
                .is_some()
        );
        assert!(
            spawn_codex["inputSchema"]["properties"]
                .get("model_preset")
                .is_some()
        );
        assert!(
            spawn_agy["inputSchema"]["properties"]
                .get("model_preset")
                .is_none()
        );
        assert!(
            spawn_agy["inputSchema"]["properties"]
                .get("effort")
                .is_none()
        );
    }

    #[test]
    fn model_schemas_tell_callers_to_omit_unrequested_models() {
        let catalog = tool_catalog();
        for tool_name in ["spawn_claude", "spawn_codex", "one_shot"] {
            let tool = catalog
                .iter()
                .find(|t| t["name"] == tool_name)
                .expect("tool exists");
            let description = tool["inputSchema"]["properties"]["model"]["description"]
                .as_str()
                .expect("model description is text");
            let preset_description =
                tool["inputSchema"]["properties"]["model_preset"]["description"]
                    .as_str()
                    .expect("model_preset description is text");

            assert!(description.contains("Leave unset unless the user explicitly requested"));
            assert!(description.contains("Do not pass both model and model_preset"));
            assert!(
                description.contains("Omitting both model and model_preset uses botctl's default")
            );
            assert!(description.contains("Do not copy the caller/orchestrator model ID"));
            assert!(description.contains("openai/gpt-5.5 is not a Claude model"));
            assert!(preset_description.contains("Leave unset unless the user explicitly asks"));
            assert!(preset_description.contains("Do not pass both model and model_preset"));
            assert!(
                preset_description
                    .contains("Omitting both model and model_preset uses botctl's default")
            );
        }
    }

    #[test]
    fn claude_agent_schema_tells_callers_to_omit_normal_agent() {
        let catalog = tool_catalog();
        for tool_name in ["spawn_claude", "one_shot"] {
            let tool = catalog
                .iter()
                .find(|t| t["name"] == tool_name)
                .expect("tool exists");
            let description = tool["inputSchema"]["properties"]["agent"]["description"]
                .as_str()
                .expect("agent description is text");

            assert!(description.contains("Do not pass for normal Claude sessions"));
            assert!(description.contains("default build agent"));
            assert!(description.contains("Only pass when the user explicitly requests"));
        }
    }

    #[test]
    fn send_keys_schema_avoids_top_level_combinators() {
        let catalog = tool_catalog();
        let send_keys = catalog
            .iter()
            .find(|t| t["name"] == "send_keys")
            .expect("catalog has send_keys");
        let schema = &send_keys["inputSchema"];

        assert_eq!(schema["type"], "object");
        for key in ["oneOf", "anyOf", "allOf", "not", "enum"] {
            assert!(
                schema.get(key).is_none(),
                "top-level {key} is rejected by some MCP clients"
            );
        }
    }

    #[test]
    fn initialize_result_uses_passed_version() {
        assert_eq!(
            initialize_result(HTTP_PROTOCOL_VERSION)["protocolVersion"],
            "2025-03-26"
        );
        assert_eq!(
            initialize_result(PROTOCOL_VERSION)["protocolVersion"],
            "2024-11-05"
        );
    }

    #[test]
    fn validate_protocol_version_rules() {
        // initialize ignores the header entirely.
        assert!(validate_protocol_version("initialize", Some("anything")).is_ok());
        // absent header is fine post-initialize.
        assert!(validate_protocol_version("tools/call", None).is_ok());
        // exact match is fine.
        assert!(validate_protocol_version("tools/call", Some(HTTP_PROTOCOL_VERSION)).is_ok());
        // any other value fails.
        assert!(validate_protocol_version("tools/call", Some("2024-11-05")).is_err());
    }

    #[test]
    fn rejects_invalid_jsonrpc_version() {
        let err = parse_request(r#"{"jsonrpc":"1.0","id":1,"method":"initialize"}"#)
            .expect_err("bad version should fail");
        assert_eq!(err["error"]["code"], -32600);
    }
}
