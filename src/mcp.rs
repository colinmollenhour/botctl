use std::path::Path;

use serde_json::{Value, json};
use uuid::{ContextV7, Timestamp, Uuid};

use crate::app::{AppError, AppResult};
use crate::mcp_protocol::{
    JsonRpcRequest, TOOL_NAMES, error, initialize_result, success, tools_list_result,
};
use crate::mcp_registry::McpRegistry;
use crate::mcp_session::McpSessionService;
use crate::prompt::resolve_state_dir;

#[derive(Debug, Clone)]
pub struct McpService {
    sessions: McpSessionService,
}

impl McpService {
    pub fn new(state_dir: Option<&Path>) -> AppResult<Self> {
        let state_dir = resolve_state_dir(state_dir)?;
        let registry = McpRegistry::open(&state_dir)?;
        let server_id = Uuid::new_v7(Timestamp::now(ContextV7::new())).to_string();
        Ok(Self {
            sessions: McpSessionService::new(registry, server_id),
        })
    }

    pub fn handle(&self, request: JsonRpcRequest) -> Option<Value> {
        let id = request.id.clone();
        if id.is_none() {
            if request.method != "notifications/initialized" {
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
            "initialize" => Ok(initialize_result()),
            "tools/list" => Ok(tools_list_result()),
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
        let args = params
            .get("arguments")
            .cloned()
            .unwrap_or_else(|| json!({}));
        let result = match name {
            "botctl_spawn" => self.sessions.spawn(&args),
            "botctl_prompt" => self.sessions.prompt(&args),
            "botctl_wait" => self.sessions.wait(&args),
            "botctl_kill" => self.sessions.kill(&args),
            "botctl_snapshot" => self.sessions.snapshot(&args),
            "botctl_send_keys" => self.sessions.send_keys(&args),
            _ => unreachable!(),
        }?;
        Ok(json!({
            "content": [{ "type": "text", "text": serde_json::to_string_pretty(&result).unwrap_or_else(|_| result.to_string()) }],
            "structuredContent": result,
            "isError": false,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp_protocol::JsonRpcRequest;

    #[test]
    fn handles_initialize_and_tools_list() {
        let root = std::env::temp_dir().join(format!("botctl-mcp-service-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let service = McpService::new(Some(&root)).unwrap();
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
        assert_eq!(tools["result"]["tools"].as_array().unwrap().len(), 6);
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
            params: json!({ "name": "botctl_send_keys", "arguments": {} }),
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
}
