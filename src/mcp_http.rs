use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::str::FromStr;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::app::{AppError, AppResult};
use crate::mcp::McpService;
use crate::mcp_protocol::{HTTP_PROTOCOL_VERSION, error, parse_request, validate_protocol_version};

const READ_TIMEOUT_MS: u64 = 30_000;
const WRITE_TIMEOUT_MS: u64 = 30_000;
/// Overall wall-clock deadline for reading a full request (headers + body),
/// independent of the per-syscall `READ_TIMEOUT_MS`. Bounds slowloris-style
/// clients that trickle bytes just under the per-read timeout. Relevant on the
/// opt-in `--allow-non-loopback` path; the loopback default mitigates exposure.
const REQUEST_DEADLINE_MS: u64 = 60_000;
const MAX_HEADER_LINES: usize = 100;
const MAX_HEADER_LINE_BYTES: usize = 8192;
const MAX_BODY_BYTES: usize = 1024 * 1024;
const MAX_ACTIVE_CONNECTIONS: usize = 64;

const ALLOW_POST: &str = "POST, DELETE, OPTIONS";
const ALLOW_OPTIONS: &str = "POST, GET, DELETE, OPTIONS";

struct ActiveConnection {
    active: Arc<AtomicUsize>,
}

impl Drop for ActiveConnection {
    fn drop(&mut self) {
        self.active.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Run the stateless, Streamable-HTTP-compatible MCP server (D1 JSON-only, D2
/// stateless, D3 advertises 2025-03-26). Hand-rolled blocking thread-per-connection.
///
/// Security (D4): refuses to bind a non-loopback address unless
/// `allow_non_loopback` is set, since the MCP control plane is unauthenticated.
pub fn run_http(bind: &str, state_dir: Option<&Path>, allow_non_loopback: bool) -> AppResult<()> {
    let bind_is_loopback = check_bind_policy(bind, allow_non_loopback)?;
    let listener = TcpListener::bind(bind)
        .map_err(|error| AppError::new(format!("failed to bind MCP HTTP on {bind}: {error}")))?;
    // HTTP advertises 2025-03-26; stdio keeps 2024-11-05.
    let service = McpService::new_with_protocol(state_dir, HTTP_PROTOCOL_VERSION)?;
    let active = Arc::new(AtomicUsize::new(0));
    eprintln!(
        "mcp http listening on http://{bind}/mcp (Streamable-HTTP-compatible (stateless JSON request/response; GET->405, no SSE))"
    );
    if allow_non_loopback && !bind_is_loopback {
        eprintln!(
            "WARNING: MCP HTTP is bound to a non-loopback address ({bind}); the MCP control plane has NO authentication and can spawn/kill agents. It is now network-exposed."
        );
    }
    for stream in listener.incoming() {
        match stream {
            Ok(mut stream) => {
                if active.load(Ordering::SeqCst) >= MAX_ACTIVE_CONNECTIONS {
                    let _ = stream.set_write_timeout(Some(Duration::from_millis(WRITE_TIMEOUT_MS)));
                    let _ = write_http_response(
                        &mut stream,
                        &HttpResponse::json(
                            503,
                            &json!({ "error": "too many active MCP HTTP connections" }),
                        ),
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
                    if let Err(error) = handle_connection(stream, &service, bind_is_loopback) {
                        eprintln!("warning: mcp http request failed: {error}");
                    }
                });
            }
            Err(error) => return Err(AppError::new(format!("MCP HTTP accept failed: {error}"))),
        }
    }
    Ok(())
}

/// Validate the bind policy (D4). Returns whether the bind host is loopback, or
/// an error if a non-loopback bind is requested without `--allow-non-loopback`.
fn check_bind_policy(bind: &str, allow_non_loopback: bool) -> AppResult<bool> {
    let bind_is_loopback = host_is_loopback(bind);
    if !bind_is_loopback && !allow_non_loopback {
        return Err(AppError::new(format!(
            "refusing to bind MCP HTTP to non-loopback address {bind} without --allow-non-loopback; the MCP control plane has no authentication"
        )));
    }
    Ok(bind_is_loopback)
}

fn handle_connection(
    stream: TcpStream,
    service: &McpService,
    bind_is_loopback: bool,
) -> AppResult<()> {
    stream.set_read_timeout(Some(Duration::from_millis(READ_TIMEOUT_MS)))?;
    stream.set_write_timeout(Some(Duration::from_millis(WRITE_TIMEOUT_MS)))?;
    let deadline = Instant::now() + Duration::from_millis(REQUEST_DEADLINE_MS);
    let mut reader = BufReader::new(stream);
    let Some(request) = read_http_request(&mut reader, deadline)? else {
        return Ok(());
    };
    let response = build_response(&request, service, bind_is_loopback);
    let stream = &mut reader.into_inner();
    write_http_response(stream, &response)
}

fn build_response(
    request: &HttpRequest,
    service: &McpService,
    bind_is_loopback: bool,
) -> HttpResponse {
    // Transport-framing errors detected while reading (413/431) short-circuit.
    if request.status != 200 {
        return HttpResponse::json(
            request.status,
            &json!({ "error": request.error.unwrap_or("bad request") }),
        );
    }

    let path = request.target.split('?').next().unwrap_or(&request.target);
    if path != "/mcp" {
        return HttpResponse::json(
            404,
            &json!({ "error": "not found; this server exposes /mcp" }),
        );
    }

    // Method routing.
    match request.method.as_str() {
        "OPTIONS" => {
            return HttpResponse::empty_with_headers(
                204,
                vec![("Allow".to_string(), ALLOW_OPTIONS.to_string())],
            );
        }
        "DELETE" => {
            // Stateless (D2): DELETE is an idempotent no-op.
            return HttpResponse::empty(204);
        }
        "GET" => {
            return HttpResponse::json_with_headers(
                405,
                &json!({ "error": "this MCP server does not provide a server-initiated SSE stream; use POST /mcp" }),
                vec![("Allow".to_string(), ALLOW_POST.to_string())],
            );
        }
        "POST" => {}
        _ => {
            return HttpResponse::json_with_headers(
                405,
                &json!({ "error": "method not allowed" }),
                vec![("Allow".to_string(), ALLOW_POST.to_string())],
            );
        }
    }

    // POST /mcp guard ladder: Origin -> Host -> Content-Type -> Accept ->
    // MCP-Protocol-Version -> UTF-8 -> batch peek -> parse -> dispatch.
    if let Err(resp) = validate_mcp_origin(request.header("origin")) {
        return resp;
    }
    if let Err(resp) = validate_host(request.header("host"), bind_is_loopback) {
        return resp;
    }
    if let Err(resp) = validate_content_type(request.header("content-type")) {
        return resp;
    }
    if let Err(resp) = negotiate_accept(request.header("accept")) {
        return resp;
    }

    let body = match std::str::from_utf8(&request.body) {
        Ok(body) => body,
        // Decode strictly: from_utf8_lossy would substitute U+FFFD for malformed
        // bytes and could hand parse_request a different, valid JSON document
        // than the client actually sent.
        Err(_) => {
            return HttpResponse::json(400, &json!({ "error": "request body is not valid UTF-8" }));
        }
    };

    // Batch peek (transport-level): a top-level JSON-RPC batch is unsupported. We
    // never iterate an array into handle, preserving the notification-smuggling
    // guard.
    if body
        .trim_start()
        .as_bytes()
        .first()
        .map(|b| *b == b'[')
        .unwrap_or(false)
    {
        return HttpResponse::json(
            200,
            &error(
                None,
                -32600,
                "invalid_request",
                Some(json!({ "detail": "JSON-RPC batches are not supported" })),
            ),
        );
    }

    // Lightweight top-level object peek (serves F4 response detection and the
    // F10 protocol-version method peek). A top-level JSON object lets us read the
    // `method` field even when the body is not a fully valid JSON-RPC request.
    let peeked_object = match serde_json::from_str::<Value>(body) {
        Ok(Value::Object(map)) => Some(map),
        _ => None,
    };

    // Response-only POST (client -> server JSON-RPC response/error, no `method`):
    // accept with 202 + empty body per the spec/docs. `parse_request` would
    // otherwise reject it as a parse error since it requires `method`. (botctl
    // never sends requests to clients, so this is for spec/doc parity.)
    if let Some(map) = &peeked_object
        && !map.contains_key("method")
        && (map.contains_key("result") || map.contains_key("error"))
    {
        return HttpResponse::empty(202);
    }

    // Protocol-version check is keyed off the request method. Peek it from the
    // top-level object so an unsupported version header yields a 400 even when the
    // body is otherwise malformed (absent method => treated as non-initialize, so
    // the header is still validated). A non-object body has no method to key on and
    // falls through to the JSON-RPC parse-error envelope below.
    if let Some(map) = &peeked_object {
        let method = map.get("method").and_then(Value::as_str).unwrap_or("");
        if validate_protocol_version(method, request.header("mcp-protocol-version")).is_err() {
            let version = request.header("mcp-protocol-version").unwrap_or("");
            return HttpResponse::json(
                400,
                &json!({ "error": format!("unsupported MCP-Protocol-Version {version}") }),
            );
        }
    }

    let parsed = parse_request(body);

    match parsed {
        Ok(req) => match service.handle(req) {
            Some(value) => HttpResponse::json(200, &value),
            // Notification-only / id-less responses: 202 Accepted, empty body.
            None => HttpResponse::empty(202),
        },
        Err(response) => HttpResponse::json(200, &response),
    }
}

/// Lenient `Accept` negotiation (D1). The only path to 406 is an explicit
/// `Accept` listing only concrete types, none of which is json/event-stream/wildcard.
fn negotiate_accept(accept: Option<&str>) -> Result<(), HttpResponse> {
    let Some(accept) = accept else {
        return Ok(());
    };
    let acceptable = accept.split(',').any(|part| {
        let media = part.split(';').next().unwrap_or("").trim();
        media.eq_ignore_ascii_case("*/*")
            || media.eq_ignore_ascii_case("application/*")
            || media.eq_ignore_ascii_case("application/json")
            // We ignore the SSE token and always return JSON (D1; the spec permits
            // a JSON response for a single result).
            || media.eq_ignore_ascii_case("text/event-stream")
    });
    if acceptable {
        Ok(())
    } else {
        Err(HttpResponse::json(
            406,
            &json!({ "error": "only application/json is produced" }),
        ))
    }
}

/// Lenient `Content-Type` validation. Missing is OK; present must be
/// `application/json` (ignoring `;charset`/case) or it is a 415.
fn validate_content_type(ct: Option<&str>) -> Result<(), HttpResponse> {
    let Some(ct) = ct else {
        return Ok(());
    };
    let media = ct.split(';').next().unwrap_or("").trim();
    if media.eq_ignore_ascii_case("application/json") {
        Ok(())
    } else {
        Err(HttpResponse::json(
            415,
            &json!({ "error": "content-type must be application/json" }),
        ))
    }
}

/// Validate the `Origin` header for DNS-rebinding protection. Only a *missing*
/// Origin is allowed (native MCP clients omit the header). A literal `null`
/// Origin (sandboxed/opaque browser contexts) is rejected with 403. Otherwise
/// the host authority must be loopback.
///
/// Deliberately NOT reusing `http_api.rs::validate_origin` (allow-list-based,
/// no loopback notion) to avoid coupling MCP to serve-mode CORS.
fn validate_mcp_origin(origin: Option<&str>) -> Result<(), HttpResponse> {
    let Some(origin) = origin else {
        return Ok(());
    };
    let origin = origin.trim();
    // Extract the host authority: take everything after `://`, cut at the first
    // `/`, `?`, or `#`, then strip any `userinfo@` so a loopback prefix in the
    // userinfo/path cannot be smuggled past the loopback check.
    let after_scheme = origin
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(origin);
    let authority = after_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or(after_scheme);
    let authority = authority
        .rsplit_once('@')
        .map(|(_, host)| host)
        .unwrap_or(authority);
    if host_is_loopback(authority) {
        Ok(())
    } else {
        Err(HttpResponse::json(
            403,
            &json!({ "error": "origin not allowed" }),
        ))
    }
}

/// When bound to loopback, the `Host` header (if present) must itself be
/// loopback (closes DNS-rebinding when Origin is absent). When bound
/// non-loopback the check is skipped (Origin validation still applies).
fn validate_host(host: Option<&str>, bind_is_loopback: bool) -> Result<(), HttpResponse> {
    if !bind_is_loopback {
        return Ok(());
    }
    let Some(host) = host else {
        return Ok(());
    };
    if host_is_loopback(host.trim()) {
        Ok(())
    } else {
        Err(HttpResponse::json(
            403,
            &json!({ "error": "host not allowed" }),
        ))
    }
}

/// Classify a host[:port] authority as loopback. Strips an optional `:port` and
/// `[ ]` brackets, then accepts ONLY a parseable IP that `is_loopback()` (covers
/// 127.0.0.0/8 and ::1) OR an exact, case-insensitive `localhost`. Names like
/// `127.0.0.1.evil.com` or `127.evil.com` are NOT parseable IPs and are rejected.
/// `0.0.0.0` and public addresses are not loopback.
fn host_is_loopback(host: &str) -> bool {
    let host = host.trim();
    // IPv6 bracketed form: [::1] or [::1]:port.
    let host = if let Some(rest) = host.strip_prefix('[') {
        match rest.split_once(']') {
            // The suffix after `]` must be empty or a numeric `:port`; anything
            // else (e.g. `[::1]evil.com`, `[::1]:notaport`) is not a valid
            // bracketed authority and must NOT be classified as loopback.
            Some((inner, suffix)) => {
                if suffix.is_empty() {
                    inner
                } else if let Some(port) = suffix.strip_prefix(':')
                    && !port.is_empty()
                    && port.bytes().all(|b| b.is_ascii_digit())
                {
                    inner
                } else {
                    return false;
                }
            }
            // A bracketed authority with no closing `]` (e.g. `[::1`) is
            // malformed and must not be classified as loopback.
            None => return false,
        }
    } else if host.matches(':').count() == 1 {
        // host:port (IPv4 or name). A bare IPv6 has multiple colons and no port
        // here. Only strip the suffix when it is an all-digit port; otherwise
        // keep the whole string as the host so it fails IP parsing below
        // (rejecting e.g. `127.0.0.1:notaport`).
        match host.split_once(':') {
            Some((h, port)) if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => h,
            _ => host,
        }
    } else {
        host
    };
    let host = host.trim();
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    // Strict: only a parseable IP that is itself loopback is accepted.
    std::net::IpAddr::from_str(host)
        .map(|ip| ip.is_loopback())
        .unwrap_or(false)
}

#[derive(Debug)]
struct HttpRequest {
    status: u16,
    error: Option<&'static str>,
    method: String,
    target: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl HttpRequest {
    /// Case-insensitive header lookup (header names are stored lowercased).
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

#[derive(Debug)]
struct HttpResponse {
    status: u16,
    /// `None` => no body, emits `Content-Length: 0` and no `Content-Type`.
    body: Option<Vec<u8>>,
    extra_headers: Vec<(String, String)>,
}

impl HttpResponse {
    fn json(status: u16, value: &Value) -> Self {
        HttpResponse {
            status,
            body: Some(encode_json(value)),
            extra_headers: Vec::new(),
        }
    }

    fn json_with_headers(status: u16, value: &Value, headers: Vec<(String, String)>) -> Self {
        HttpResponse {
            status,
            body: Some(encode_json(value)),
            extra_headers: headers,
        }
    }

    fn empty(status: u16) -> Self {
        HttpResponse {
            status,
            body: None,
            extra_headers: Vec::new(),
        }
    }

    fn empty_with_headers(status: u16, headers: Vec<(String, String)>) -> Self {
        HttpResponse {
            status,
            body: None,
            extra_headers: headers,
        }
    }
}

fn encode_json(value: &Value) -> Vec<u8> {
    serde_json::to_vec(value).unwrap_or_else(|_| {
        error(None, -32603, "internal_error", None)
            .to_string()
            .into_bytes()
    })
}

fn read_http_request<R: BufRead>(
    reader: &mut R,
    deadline: Instant,
) -> AppResult<Option<HttpRequest>> {
    let request_line = match read_limited_line(reader, deadline)? {
        LineOutcome::Eof => return Ok(None),
        LineOutcome::TooLarge => {
            return Ok(Some(headers_too_large(String::new(), String::new())));
        }
        LineOutcome::Line(line) => line,
    };
    let request_line = request_line.trim_end_matches(['\r', '\n']);
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let target = parts.next().unwrap_or("").to_string();
    let mut content_length = 0usize;
    let mut headers: Vec<(String, String)> = Vec::new();
    for _ in 0..MAX_HEADER_LINES {
        let line = match read_limited_line(reader, deadline)? {
            LineOutcome::Eof => break,
            LineOutcome::TooLarge => return Ok(Some(headers_too_large(method, target))),
            LineOutcome::Line(line) => line,
        };
        if line == "\r\n" || line == "\n" {
            if content_length > MAX_BODY_BYTES {
                return Ok(Some(HttpRequest {
                    status: 413,
                    error: Some("request body too large"),
                    method,
                    target,
                    headers,
                    body: Vec::new(),
                }));
            }
            if Instant::now() >= deadline {
                return Err(AppError::new("MCP HTTP request read deadline exceeded"));
            }
            let body = read_body(reader, content_length, deadline)?;
            return Ok(Some(HttpRequest {
                status: 200,
                error: None,
                method,
                target,
                headers,
                body,
            }));
        }
        if let Some((name, value)) = line.split_once(':') {
            let name = name.trim().to_ascii_lowercase();
            let value = value.trim_end_matches(['\r', '\n']).trim().to_string();
            if name == "content-length" {
                content_length = value.parse().unwrap_or(MAX_BODY_BYTES + 1);
            }
            headers.push((name, value));
        }
    }
    Ok(Some(headers_too_large(method, target)))
}

/// Read exactly `content_length` body bytes, checking the overall request
/// deadline before each chunk so a slow/stalled body cannot hold the connection
/// past `REQUEST_DEADLINE_MS`. Mirrors `read_exact`'s short-read behavior: a
/// premature EOF yields an `UnexpectedEof` error, matching the prior single
/// `read_exact(&mut body)` call. `MAX_BODY_BYTES` is still enforced upstream via
/// the `content_length` check before this is called.
fn read_body<R: BufRead>(
    reader: &mut R,
    content_length: usize,
    deadline: Instant,
) -> AppResult<Vec<u8>> {
    let mut body = Vec::with_capacity(content_length);
    while body.len() < content_length {
        if Instant::now() >= deadline {
            return Err(AppError::new("MCP HTTP request read deadline exceeded"));
        }
        let available = reader.fill_buf()?;
        if available.is_empty() {
            // Short read: fewer bytes than Content-Length promised. Preserve the
            // prior read_exact UnexpectedEof semantics.
            return Err(AppError::from(std::io::Error::from(
                std::io::ErrorKind::UnexpectedEof,
            )));
        }
        let remaining = content_length - body.len();
        let take = remaining.min(available.len());
        body.extend_from_slice(&available[..take]);
        reader.consume(take);
    }
    Ok(body)
}

fn headers_too_large(method: String, target: String) -> HttpRequest {
    HttpRequest {
        status: 431,
        error: Some("request headers too large"),
        method,
        target,
        headers: Vec::new(),
        body: Vec::new(),
    }
}

enum LineOutcome {
    Eof,
    TooLarge,
    Line(String),
}

fn read_limited_line<R: BufRead>(reader: &mut R, deadline: Instant) -> AppResult<LineOutcome> {
    let mut bytes = Vec::with_capacity(128);
    loop {
        if Instant::now() >= deadline {
            return Err(AppError::new("MCP HTTP request read deadline exceeded"));
        }
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return if bytes.is_empty() {
                Ok(LineOutcome::Eof)
            } else {
                decode_line(bytes)
            };
        }

        let take = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map(|index| index + 1)
            .unwrap_or(available.len());
        if bytes.len().saturating_add(take) > MAX_HEADER_LINE_BYTES {
            // Surface as a structured 431 rather than an Err that would only be
            // logged and would drop the socket without an HTTP response.
            return Ok(LineOutcome::TooLarge);
        }
        bytes.extend_from_slice(&available[..take]);
        reader.consume(take);
        if bytes.last() == Some(&b'\n') {
            break;
        }
    }
    decode_line(bytes)
}

fn decode_line(bytes: Vec<u8>) -> AppResult<LineOutcome> {
    String::from_utf8(bytes)
        .map(LineOutcome::Line)
        .map_err(|error| AppError::new(format!("invalid HTTP header encoding: {error}")))
}

fn write_http_response(stream: &mut TcpStream, response: &HttpResponse) -> AppResult<()> {
    let reason = match response.status {
        200 => "OK",
        202 => "Accepted",
        204 => "No Content",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        406 => "Not Acceptable",
        413 => "Payload Too Large",
        415 => "Unsupported Media Type",
        431 => "Request Header Fields Too Large",
        503 => "Service Unavailable",
        _ => "Error",
    };
    write!(stream, "HTTP/1.1 {} {reason}\r\n", response.status)?;
    match &response.body {
        Some(body) => {
            write!(stream, "Content-Type: application/json\r\n")?;
            write!(stream, "Content-Length: {}\r\n", body.len())?;
        }
        None => {
            write!(stream, "Content-Length: 0\r\n")?;
        }
    }
    for (name, value) in &response.extra_headers {
        write!(stream, "{name}: {value}\r\n")?;
    }
    write!(stream, "Connection: close\r\n\r\n")?;
    if let Some(body) = &response.body {
        stream.write_all(body)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn parse(raw: &str) -> HttpRequest {
        let mut cursor = Cursor::new(raw.as_bytes().to_vec());
        let deadline = Instant::now() + Duration::from_millis(REQUEST_DEADLINE_MS);
        read_http_request(&mut cursor, deadline).unwrap().unwrap()
    }

    #[test]
    fn read_request_enforces_overall_deadline() {
        // An already-elapsed deadline aborts the read with an error before any
        // request is returned (slowloris bound).
        let body = "POST /mcp HTTP/1.1\r\nContent-Length: 0\r\n\r\n";
        let mut cursor = Cursor::new(body.as_bytes().to_vec());
        let past = Instant::now() - Duration::from_millis(1);
        let err = read_http_request(&mut cursor, past).unwrap_err();
        assert!(err.to_string().contains("read deadline exceeded"));
    }

    #[test]
    fn rejects_oversized_http_body_before_allocation() {
        let request = format!(
            "POST /mcp HTTP/1.1\r\nContent-Length: {}\r\n\r\n",
            MAX_BODY_BYTES + 1
        );
        assert_eq!(parse(&request).status, 413);
    }

    #[test]
    fn parses_post_mcp_route_and_body() {
        let body = r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#;
        let request = format!(
            "POST /mcp HTTP/1.1\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let parsed = parse(&request);
        assert_eq!(parsed.status, 200);
        assert_eq!(parsed.method, "POST");
        assert_eq!(parsed.target, "/mcp");
        assert_eq!(parsed.body, body.as_bytes());
    }

    #[test]
    fn body_read_is_deadline_aware() {
        // R3: read_body checks the overall request deadline before each chunk,
        // so an elapsed deadline aborts the body read rather than blocking on a
        // slow/withheld body.
        let mut cursor = Cursor::new(b"abcd".to_vec());
        let past = Instant::now() - Duration::from_millis(1);
        let err = read_body(&mut cursor, 4, past).unwrap_err();
        assert!(err.to_string().contains("read deadline exceeded"));
    }

    #[test]
    fn body_read_full_and_short() {
        // Full body reads back exactly; a body shorter than Content-Length
        // surfaces as an error (mirrors the prior read_exact UnexpectedEof).
        let deadline = Instant::now() + Duration::from_millis(REQUEST_DEADLINE_MS);
        let mut full = Cursor::new(b"hello".to_vec());
        assert_eq!(read_body(&mut full, 5, deadline).unwrap(), b"hello");
        let mut short = Cursor::new(b"hi".to_vec());
        assert!(read_body(&mut short, 8, deadline).is_err());
    }

    #[test]
    fn body_short_read_errors() {
        // R3: a body shorter than Content-Length surfaces as an error through
        // the full request-read path.
        let request = "POST /mcp HTTP/1.1\r\nContent-Length: 8\r\n\r\nshort";
        let mut cursor = Cursor::new(request.as_bytes().to_vec());
        let deadline = Instant::now() + Duration::from_millis(REQUEST_DEADLINE_MS);
        assert!(read_http_request(&mut cursor, deadline).is_err());
    }

    #[test]
    fn oversized_header_line_yields_structured_431() {
        let request = format!(
            "GET /{} HTTP/1.1\r\n\r\n",
            "x".repeat(MAX_HEADER_LINE_BYTES)
        );
        assert_eq!(parse(&request).status, 431);
    }

    #[test]
    fn oversized_trailing_header_line_yields_431_with_method() {
        let request = format!(
            "POST /mcp HTTP/1.1\r\nX-Big: {}\r\n\r\n",
            "x".repeat(MAX_HEADER_LINE_BYTES)
        );
        let parsed = parse(&request);
        assert_eq!(parsed.status, 431);
        assert_eq!(parsed.method, "POST");
        assert_eq!(parsed.target, "/mcp");
    }

    #[test]
    fn captures_arbitrary_headers() {
        let request = "POST /mcp HTTP/1.1\r\nOrigin: http://localhost:1234\r\nAccept: application/json\r\nMCP-Protocol-Version: 2025-03-26\r\nContent-Length: 0\r\n\r\n";
        let parsed = parse(request);
        assert_eq!(parsed.header("origin"), Some("http://localhost:1234"));
        // Case-insensitive lookup.
        assert_eq!(parsed.header("ACCEPT"), Some("application/json"));
        assert_eq!(parsed.header("mcp-protocol-version"), Some("2025-03-26"));
    }

    // Accept negotiation.
    #[test]
    fn accept_missing_ok() {
        assert!(negotiate_accept(None).is_ok());
    }
    #[test]
    fn accept_wildcard_ok() {
        assert!(negotiate_accept(Some("*/*")).is_ok());
    }
    #[test]
    fn accept_json_ok() {
        assert!(negotiate_accept(Some("application/json, text/plain")).is_ok());
    }
    #[test]
    fn accept_event_stream_only_ok() {
        assert!(negotiate_accept(Some("text/event-stream")).is_ok());
    }
    #[test]
    fn accept_text_plain_only_406() {
        let err = negotiate_accept(Some("text/plain")).unwrap_err();
        assert_eq!(err.status, 406);
    }

    // Content-Type.
    #[test]
    fn content_type_json_ok() {
        assert!(validate_content_type(Some("application/json; charset=utf-8")).is_ok());
    }
    #[test]
    fn content_type_missing_ok() {
        assert!(validate_content_type(None).is_ok());
    }
    #[test]
    fn content_type_non_json_415() {
        let err = validate_content_type(Some("text/plain")).unwrap_err();
        assert_eq!(err.status, 415);
    }

    // Origin.
    #[test]
    fn origin_missing_allowed() {
        assert!(validate_mcp_origin(None).is_ok());
    }
    #[test]
    fn origin_localhost_allowed() {
        assert!(validate_mcp_origin(Some("http://localhost:8787")).is_ok());
    }
    #[test]
    fn origin_127_allowed() {
        assert!(validate_mcp_origin(Some("http://127.0.0.1:8787")).is_ok());
    }
    #[test]
    fn origin_null_blocked() {
        // A literal `null` Origin (sandboxed/opaque browser context) is rejected;
        // only a missing Origin is allowed.
        let err = validate_mcp_origin(Some("null")).unwrap_err();
        assert_eq!(err.status, 403);
    }
    #[test]
    fn origin_evil_403() {
        let err = validate_mcp_origin(Some("http://evil.example.com")).unwrap_err();
        assert_eq!(err.status, 403);
    }
    #[test]
    fn origin_loopback_prefix_smuggle_403() {
        // userinfo @ smuggling and loopback-prefixed hostnames must be rejected.
        for o in [
            "http://127.0.0.1@evil.com",
            "http://127.0.0.1.evil.com/x",
            "http://127.0.0.1.evil.com",
            "http://127.evil.com",
        ] {
            let err = validate_mcp_origin(Some(o)).unwrap_err();
            assert_eq!(err.status, 403, "origin {o} should be rejected");
        }
    }
    #[test]
    fn host_loopback_prefix_smuggle_403() {
        for h in [
            "127.0.0.1.evil.com",
            "127.0.0.1.evil.com:80",
            "127.evil.com",
        ] {
            assert!(!host_is_loopback(h), "host {h} must not be loopback");
        }
    }

    // Host.
    #[test]
    fn host_loopback_ok() {
        assert!(validate_host(Some("127.0.0.1:8787"), true).is_ok());
        assert!(validate_host(Some("localhost:8787"), true).is_ok());
    }
    #[test]
    fn host_mismatch_403() {
        let err = validate_host(Some("evil.example.com"), true).unwrap_err();
        assert_eq!(err.status, 403);
    }
    #[test]
    fn host_skipped_when_non_loopback_bind() {
        assert!(validate_host(Some("evil.example.com"), false).is_ok());
    }
    #[test]
    fn host_is_loopback_classifies() {
        assert!(host_is_loopback("127.0.0.1"));
        assert!(host_is_loopback("127.5.5.5:8080"));
        assert!(host_is_loopback("localhost"));
        assert!(host_is_loopback("[::1]:8787"));
        assert!(host_is_loopback("::1"));
        assert!(!host_is_loopback("0.0.0.0"));
        assert!(!host_is_loopback("0.0.0.0:8787"));
        assert!(!host_is_loopback("192.168.1.5"));
        assert!(!host_is_loopback("evil.example.com"));
        // R2: bracketed-IPv6 suffix must be empty or :<digits>.
        assert!(!host_is_loopback("[::1]evil.com"));
        assert!(!host_is_loopback("[::1]:notaport"));
        // Malformed bracketed authority with no closing `]` is rejected.
        assert!(!host_is_loopback("[::1"));
        // R2: unbracketed host:port must have an all-digit port, else the whole
        // string is treated as the host and fails IP parsing.
        assert!(!host_is_loopback("127.0.0.1:notaport"));
    }

    // Method routing via build_response.
    fn service() -> McpService {
        let root = std::env::temp_dir().join(format!(
            "botctl-mcp-http-test-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&root);
        McpService::new_with_protocol(Some(&root), HTTP_PROTOCOL_VERSION).unwrap()
    }

    fn req(method: &str, target: &str, headers: Vec<(&str, &str)>, body: &str) -> HttpRequest {
        HttpRequest {
            status: 200,
            error: None,
            method: method.to_string(),
            target: target.to_string(),
            headers: headers
                .into_iter()
                .map(|(k, v)| (k.to_ascii_lowercase(), v.to_string()))
                .collect(),
            body: body.as_bytes().to_vec(),
        }
    }

    #[test]
    fn get_mcp_returns_405_with_allow() {
        let resp = build_response(&req("GET", "/mcp", vec![], ""), &service(), true);
        assert_eq!(resp.status, 405);
        assert!(
            resp.extra_headers
                .iter()
                .any(|(k, v)| k == "Allow" && v == ALLOW_POST)
        );
    }

    #[test]
    fn options_returns_204_allow() {
        let resp = build_response(&req("OPTIONS", "/mcp", vec![], ""), &service(), true);
        assert_eq!(resp.status, 204);
        assert!(
            resp.extra_headers
                .iter()
                .any(|(k, v)| k == "Allow" && v == ALLOW_OPTIONS)
        );
        // No CORS headers.
        assert!(
            !resp
                .extra_headers
                .iter()
                .any(|(k, _)| k.to_ascii_lowercase().starts_with("access-control"))
        );
    }

    #[test]
    fn delete_returns_204() {
        let resp = build_response(&req("DELETE", "/mcp", vec![], ""), &service(), true);
        assert_eq!(resp.status, 204);
        assert!(resp.body.is_none());
    }

    #[test]
    fn unknown_path_404() {
        let resp = build_response(&req("POST", "/other", vec![], ""), &service(), true);
        assert_eq!(resp.status, 404);
    }

    #[test]
    fn batch_rejected_with_jsonrpc_error() {
        let resp = build_response(
            &req(
                "POST",
                "/mcp",
                vec![],
                r#"[{"jsonrpc":"2.0","id":1,"method":"initialize"}]"#,
            ),
            &service(),
            true,
        );
        assert_eq!(resp.status, 200);
        let body: Value = serde_json::from_slice(resp.body.as_ref().unwrap()).unwrap();
        assert_eq!(body["error"]["code"], -32600);
        assert_eq!(
            body["error"]["data"]["detail"],
            "JSON-RPC batches are not supported"
        );
    }

    #[test]
    fn notifications_only_post_returns_202() {
        let resp = build_response(
            &req(
                "POST",
                "/mcp",
                vec![],
                r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            ),
            &service(),
            true,
        );
        assert_eq!(resp.status, 202);
        assert!(resp.body.is_none());
    }

    #[test]
    fn response_object_post_returns_202() {
        // A JSON-RPC response object (no `method`, has `result`) is accepted with 202.
        let resp = build_response(
            &req(
                "POST",
                "/mcp",
                vec![],
                r#"{"jsonrpc":"2.0","id":1,"result":{}}"#,
            ),
            &service(),
            true,
        );
        assert_eq!(resp.status, 202);
        assert!(resp.body.is_none());
        // An error response object is likewise accepted with 202.
        let resp = build_response(
            &req(
                "POST",
                "/mcp",
                vec![],
                r#"{"jsonrpc":"2.0","id":1,"error":{"code":-1,"message":"x"}}"#,
            ),
            &service(),
            true,
        );
        assert_eq!(resp.status, 202);
        assert!(resp.body.is_none());
    }

    #[test]
    fn post_with_id_returns_200_json() {
        let resp = build_response(
            &req(
                "POST",
                "/mcp",
                vec![],
                r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#,
            ),
            &service(),
            true,
        );
        assert_eq!(resp.status, 200);
        let body: Value = serde_json::from_slice(resp.body.as_ref().unwrap()).unwrap();
        assert_eq!(body["result"]["protocolVersion"], "2025-03-26");
    }

    #[test]
    fn unsupported_protocol_version_header_400() {
        let resp = build_response(
            &req(
                "POST",
                "/mcp",
                vec![("MCP-Protocol-Version", "1999-01-01")],
                r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#,
            ),
            &service(),
            true,
        );
        assert_eq!(resp.status, 400);
    }

    #[test]
    fn unsupported_protocol_version_header_400_even_with_bad_body() {
        // F10: a bad version header yields 400 even when the JSON-RPC body is not a
        // valid request, as long as the body is a top-level object (method peek).
        let resp = build_response(
            &req(
                "POST",
                "/mcp",
                vec![("MCP-Protocol-Version", "1999-01-01")],
                r#"{"jsonrpc":"2.0","id":1}"#,
            ),
            &service(),
            true,
        );
        assert_eq!(resp.status, 400);
    }

    #[test]
    fn origin_evil_blocks_post_403() {
        let resp = build_response(
            &req(
                "POST",
                "/mcp",
                vec![("Origin", "http://evil.example.com")],
                r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#,
            ),
            &service(),
            true,
        );
        assert_eq!(resp.status, 403);
    }

    #[test]
    fn write_http_response_emits_custom_headers() {
        use std::io::Read as _;
        // Exercise the real writer over a loopback TcpStream pair.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = thread::spawn(move || {
            let mut client = TcpStream::connect(addr).unwrap();
            let mut buf = String::new();
            client.read_to_string(&mut buf).unwrap();
            buf
        });
        let (mut server, _) = listener.accept().unwrap();
        let resp = HttpResponse::empty_with_headers(
            204,
            vec![("Allow".to_string(), ALLOW_OPTIONS.to_string())],
        );
        write_http_response(&mut server, &resp).unwrap();
        drop(server);
        let raw = handle.join().unwrap();
        assert!(raw.starts_with("HTTP/1.1 204 No Content\r\n"));
        assert!(raw.contains("Allow: POST, GET, DELETE, OPTIONS\r\n"));
        assert!(raw.contains("Content-Length: 0\r\n"));
        // Empty body => no Content-Type.
        assert!(!raw.contains("Content-Type"));
    }

    #[test]
    fn check_bind_policy_rejects_non_loopback_without_flag() {
        let err = check_bind_policy("0.0.0.0:8787", false).unwrap_err();
        assert!(
            err.to_string()
                .contains("refusing to bind MCP HTTP to non-loopback address")
        );
        // Allowed with the flag (returns non-loopback = false).
        assert!(!check_bind_policy("0.0.0.0:8787", true).unwrap());
        // Loopback always allowed (returns loopback = true).
        assert!(check_bind_policy("127.0.0.1:8787", false).unwrap());
    }
}
