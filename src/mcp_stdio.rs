use std::io::{self, BufRead, Write};
use std::path::Path;

use crate::app::AppResult;
use crate::mcp::McpService;
use crate::mcp_protocol::parse_request;

pub fn run_stdio(state_dir: Option<&Path>) -> AppResult<()> {
    let service = McpService::new(state_dir)?;
    let stdin = io::stdin();
    let mut stdout = io::stdout().lock();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let response = match parse_request(&line) {
            Ok(request) => service.handle(request),
            Err(response) => Some(response),
        };
        let Some(response) = response else {
            continue;
        };
        serde_json::to_writer(&mut stdout, &response).map_err(|error| {
            crate::app::AppError::new(format!("failed to encode MCP response: {error}"))
        })?;
        stdout.write_all(b"\n")?;
        stdout.flush()?;
    }
    Ok(())
}
