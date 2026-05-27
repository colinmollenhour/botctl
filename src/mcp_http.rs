use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::thread;
use std::time::Duration;

use serde_json::json;

use crate::app::{AppError, AppResult};
use crate::mcp::McpService;
use crate::mcp_protocol::{error, parse_request};

const READ_TIMEOUT_MS: u64 = 30_000;
const WRITE_TIMEOUT_MS: u64 = 30_000;
const MAX_HEADER_LINES: usize = 100;
const MAX_HEADER_LINE_BYTES: usize = 8192;
const MAX_BODY_BYTES: usize = 1024 * 1024;
const MAX_ACTIVE_CONNECTIONS: usize = 64;

struct ActiveConnection {
    active: Arc<AtomicUsize>,
}

impl Drop for ActiveConnection {
    fn drop(&mut self) {
        self.active.fetch_sub(1, Ordering::SeqCst);
    }
}

pub fn run_http(bind: &str, state_dir: Option<&Path>) -> AppResult<()> {
    let listener = TcpListener::bind(bind)
        .map_err(|error| AppError::new(format!("failed to bind MCP HTTP on {bind}: {error}")))?;
    let service = McpService::new(state_dir)?;
    let active = Arc::new(AtomicUsize::new(0));
    eprintln!("mcp http listening on http://{bind}/mcp (minimal JSON-RPC, not Streamable HTTP)");
    for stream in listener.incoming() {
        match stream {
            Ok(mut stream) => {
                if active.load(Ordering::SeqCst) >= MAX_ACTIVE_CONNECTIONS {
                    let _ = stream.set_write_timeout(Some(Duration::from_millis(WRITE_TIMEOUT_MS)));
                    let _ = write_response(
                        &mut stream,
                        503,
                        &json!({ "error": "too many active MCP HTTP connections" }),
                    );
                    continue;
                }
                active.fetch_add(1, Ordering::SeqCst);
                let guard = ActiveConnection {
                    active: active.clone(),
                };
                let service = service.clone();
                thread::spawn(move || {
                    let _guard = guard;
                    if let Err(error) = handle_connection(stream, &service) {
                        eprintln!("warning: mcp http request failed: {error}");
                    }
                });
            }
            Err(error) => return Err(AppError::new(format!("MCP HTTP accept failed: {error}"))),
        }
    }
    Ok(())
}

fn handle_connection(stream: TcpStream, service: &McpService) -> AppResult<()> {
    stream.set_read_timeout(Some(Duration::from_millis(READ_TIMEOUT_MS)))?;
    stream.set_write_timeout(Some(Duration::from_millis(WRITE_TIMEOUT_MS)))?;
    let mut reader = BufReader::new(stream);
    let Some(request) = read_http_request(&mut reader)? else {
        return Ok(());
    };
    let response = if request.status != 200 {
        (
            request.status,
            Some(json!({ "error": request.error.unwrap_or("bad request") })),
        )
    } else if request.method != "POST"
        || request.target.split('?').next().unwrap_or(&request.target) != "/mcp"
    {
        (
            404,
            Some(
                json!({ "error": "MCP v1 exposes only POST /mcp with JSON-RPC; this is not full Streamable HTTP" }),
            ),
        )
    } else {
        let body = String::from_utf8_lossy(&request.body);
        let value = match parse_request(&body) {
            Ok(request) => service.handle(request),
            Err(response) => Some(response),
        };
        (200, value)
    };
    let stream = &mut reader.into_inner();
    match response.1 {
        Some(body) => write_response(stream, response.0, &body),
        None => write_empty_response(stream, 204),
    }
}

#[derive(Debug)]
struct HttpRequest {
    status: u16,
    error: Option<&'static str>,
    method: String,
    target: String,
    body: Vec<u8>,
}

fn read_http_request<R: BufRead>(reader: &mut R) -> AppResult<Option<HttpRequest>> {
    let Some(request_line) = read_limited_line(reader)? else {
        return Ok(None);
    };
    let request_line = request_line.trim_end_matches(['\r', '\n']);
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let target = parts.next().unwrap_or("").to_string();
    let mut content_length = 0usize;
    for _ in 0..MAX_HEADER_LINES {
        let Some(line) = read_limited_line(reader)? else {
            break;
        };
        if line == "\r\n" || line == "\n" {
            if content_length > MAX_BODY_BYTES {
                return Ok(Some(HttpRequest {
                    status: 413,
                    error: Some("request body too large"),
                    method,
                    target,
                    body: Vec::new(),
                }));
            }
            let mut body = vec![0; content_length];
            if content_length > 0 {
                reader.read_exact(&mut body)?;
            }
            return Ok(Some(HttpRequest {
                status: 200,
                error: None,
                method,
                target,
                body,
            }));
        }
        if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            content_length = value.trim().parse().unwrap_or(MAX_BODY_BYTES + 1);
        }
    }
    Ok(Some(HttpRequest {
        status: 431,
        error: Some("request headers too large"),
        method,
        target,
        body: Vec::new(),
    }))
}

fn read_limited_line<R: BufRead>(reader: &mut R) -> AppResult<Option<String>> {
    let mut bytes = Vec::with_capacity(128);
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return if bytes.is_empty() {
                Ok(None)
            } else {
                String::from_utf8(bytes).map(Some).map_err(|error| {
                    AppError::new(format!("invalid HTTP header encoding: {error}"))
                })
            };
        }

        let take = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map(|index| index + 1)
            .unwrap_or(available.len());
        if bytes.len().saturating_add(take) > MAX_HEADER_LINE_BYTES {
            return Err(AppError::new("request header line too large"));
        }
        bytes.extend_from_slice(&available[..take]);
        reader.consume(take);
        if bytes.last() == Some(&b'\n') {
            break;
        }
    }
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|error| AppError::new(format!("invalid HTTP header encoding: {error}")))
}

fn write_response(stream: &mut TcpStream, status: u16, body: &serde_json::Value) -> AppResult<()> {
    let body = serde_json::to_vec(body).unwrap_or_else(|_| {
        error(None, -32603, "internal_error", None)
            .to_string()
            .into_bytes()
    });
    let reason = match status {
        200 => "OK",
        413 => "Payload Too Large",
        431 => "Request Header Fields Too Large",
        503 => "Service Unavailable",
        404 => "Not Found",
        _ => "Error",
    };
    write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    stream.write_all(&body)?;
    Ok(())
}

fn write_empty_response(stream: &mut TcpStream, status: u16) -> AppResult<()> {
    let reason = match status {
        204 => "No Content",
        _ => "OK",
    };
    write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn rejects_oversized_http_body_before_allocation() {
        let request = format!(
            "POST /mcp HTTP/1.1\r\nContent-Length: {}\r\n\r\n",
            MAX_BODY_BYTES + 1
        );
        let mut cursor = Cursor::new(request.into_bytes());
        let parsed = read_http_request(&mut cursor).unwrap().unwrap();
        assert_eq!(parsed.status, 413);
    }

    #[test]
    fn parses_post_mcp_route_and_body() {
        let body = r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#;
        let request = format!(
            "POST /mcp HTTP/1.1\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let mut cursor = Cursor::new(request.into_bytes());
        let parsed = read_http_request(&mut cursor).unwrap().unwrap();
        assert_eq!(parsed.status, 200);
        assert_eq!(parsed.method, "POST");
        assert_eq!(parsed.target, "/mcp");
        assert_eq!(parsed.body, body.as_bytes());
    }

    #[test]
    fn rejects_oversized_header_line_without_allocating_more() {
        let request = format!(
            "GET /{} HTTP/1.1\r\n\r\n",
            "x".repeat(MAX_HEADER_LINE_BYTES)
        );
        let mut cursor = Cursor::new(request.into_bytes());
        let error = read_http_request(&mut cursor).expect_err("large header line should fail");
        assert!(error.to_string().contains("header line too large"));
    }
}
