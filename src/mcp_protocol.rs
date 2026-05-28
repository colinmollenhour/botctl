use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub const PROTOCOL_VERSION: &str = "2024-11-05";
pub const TOOL_NAMES: [&str; 6] = ["spawn", "prompt", "wait", "kill", "snapshot", "send_keys"];

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

pub fn initialize_result() -> Value {
    json!({
        "protocolVersion": PROTOCOL_VERSION,
        "capabilities": { "tools": {} },
        "serverInfo": { "name": "botctl", "version": env!("CARGO_PKG_VERSION") }
    })
}

pub fn tools_list_result() -> Value {
    json!({ "tools": tool_catalog() })
}

pub fn tool_catalog() -> Vec<Value> {
    vec![
        tool(
            "spawn",
            "Start a persistent agent TUI in a managed tmux window. Provider defaults to claude. model/effort/agent are validated per provider.",
            json!({
                "type": "object", "required": ["cwd"],
                "properties": {
                    "cwd": {"type":"string"},
                    "provider": {"type":"string", "enum": ["claude", "codex", "agy"]},
                    "model": {"type":"string", "minLength":1},
                    "effort": {"type":"string", "enum": ["low", "medium", "high", "xhigh", "max"]},
                    "agent": {"type":"string", "minLength":1},
                    "timeout_ms": {"type":"integer", "minimum":1000},
                    "initial_prompt": {"type":"string"},
                    "policy": policy_schema()
                }
            }),
        ),
        tool(
            "prompt",
            "Submit a prompt to a managed session and keep it alive.",
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
            "Capture and classify the current managed pane.",
            json!({
                "type":"object", "required":["id"],
                "properties": { "id":{"type":"string"}, "capture_lines":{"type":"integer", "minimum":1, "maximum":5000} }
            }),
        ),
        tool(
            "send_keys",
            "Unsafe operator escape hatch; no progress is implied.",
            json!({
                "type":"object", "required":["id"],
                "properties": { "id":{"type":"string"}, "keys":{"type":"array", "items":{"type":"string"}}, "text":{"type":"string"}, "paste":{"type":"boolean"} },
                "oneOf": [{"required":["keys"]}, {"required":["text"]}]
            }),
        ),
    ]
}

fn policy_schema() -> Value {
    json!({ "type":"object", "properties": { "no_yolo": { "type":"boolean" } } })
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
        assert_eq!(names, TOOL_NAMES);
    }

    #[test]
    fn rejects_invalid_jsonrpc_version() {
        let err = parse_request(r#"{"jsonrpc":"1.0","id":1,"method":"initialize"}"#)
            .expect_err("bad version should fail");
        assert_eq!(err["error"]["code"], -32600);
    }
}
