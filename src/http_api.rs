use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread;
use std::time::Duration;

use serde_json::json;

use crate::app::{
    AppError, AppResult, InspectedPane, extract_permission_prompt_details,
    keys_for_action, load_automation_keybindings,
};
use crate::automation::AutomationAction;
use crate::classifier::SessionState;
use crate::runtime::{
    RuntimeClient, build_instance_detail_json, build_instance_summary_json,
};
use crate::serve::ServeRequest;

const HTTP_POLL_MS: u64 = 200;

pub fn spawn_http_server(
    runtime: RuntimeClient,
    request: ServeRequest,
    bind_addr: String,
    interrupted: Arc<AtomicBool>,
) -> thread::JoinHandle<AppResult<()>> {
    thread::spawn(move || run_http_server(runtime, request, &bind_addr, interrupted))
}

fn run_http_server(
    runtime: RuntimeClient,
    request: ServeRequest,
    bind_addr: &str,
    interrupted: Arc<AtomicBool>,
) -> AppResult<()> {
    let listener = TcpListener::bind(bind_addr)
        .map_err(|error| AppError::new(format!("failed to bind http api on {bind_addr}: {error}")))?;
    listener
        .set_nonblocking(true)
        .map_err(|error| AppError::new(format!("failed to configure http api listener: {error}")))?;

    loop {
        if interrupted.load(Ordering::SeqCst) {
            return Ok(());
        }

        match listener.accept() {
            Ok((stream, _)) => {
                if let Err(error) = handle_connection(stream, &runtime, &request) {
                    eprintln!("warning: http api request failed: {error}");
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(HTTP_POLL_MS));
            }
            Err(error) => {
                return Err(AppError::new(format!("http api accept failed: {error}")));
            }
        }
    }
}

fn handle_connection(stream: TcpStream, runtime: &RuntimeClient, request: &ServeRequest) -> AppResult<()> {
    let mut reader = BufReader::new(stream);
    let mut request_line = String::new();
    if reader.read_line(&mut request_line)? == 0 {
        return Ok(());
    }

    let request_line = request_line.trim_end_matches(['\r', '\n']);
    let mut parts = request_line.split_whitespace();
    let method = parts
        .next()
        .ok_or_else(|| AppError::new("invalid http request line: missing method"))?;
    let target = parts
        .next()
        .ok_or_else(|| AppError::new("invalid http request line: missing path"))?;

    let mut content_length = 0usize;
    let mut origin = None;
    loop {
        let mut line = String::new();
        let read = reader.read_line(&mut line)?;
        if read == 0 || line == "\r\n" || line == "\n" {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                content_length = value.trim().parse::<usize>().unwrap_or(0);
            } else if name.eq_ignore_ascii_case("origin") {
                origin = Some(value.trim().to_string());
            }
        }
    }

    let mut body = vec![0; content_length];
    if content_length > 0 {
        reader.read_exact(&mut body)?;
    }

    let response = handle_request(method, target, &body, origin.as_deref(), runtime, request);
    let mut stream = reader.into_inner();
    write_response(&mut stream, response)
}

fn handle_request(
    method: &str,
    target: &str,
    body: &[u8],
    origin: Option<&str>,
    runtime: &RuntimeClient,
    request: &ServeRequest,
) -> HttpResponse {
    let cors_origin = match validate_origin(origin, &request.allowed_origins) {
        Ok(cors_origin) => cors_origin,
        Err(response) => return response,
    };

    if method == "OPTIONS" {
        return json_response(200, json!({ "ok": true }), cors_origin);
    }

    let path = target.split('?').next().unwrap_or(target);
    let segments = path
        .split('/')
        .filter(|segment| !segment.is_empty())
        .map(url_decode)
        .collect::<Result<Vec<_>, _>>();
    let segments = match segments {
        Ok(segments) => segments,
        Err(error) => return error_response(400, error, cors_origin),
    };
    let segment_refs = segments.iter().map(String::as_str).collect::<Vec<_>>();

    match (method, segment_refs.as_slice()) {
        ("GET", []) | ("GET", ["health"]) => json_response(
            200,
            json!({
                "ok": true,
                "session_name": request.session_name,
                "target_pane": request.target_pane,
            }),
            cors_origin,
        ),
        ("GET", ["instances"]) => match list_instances(runtime, request) {
            Ok(instances) => json_response(
                200,
                json!({
                    "session_name": request.session_name,
                    "target_pane": request.target_pane,
                    "instances": instances,
                }),
                cors_origin,
            ),
            Err(error) => error_response(500, error.to_string(), cors_origin),
        },
        ("GET", ["instances", pane_id]) => match instance_detail(runtime, request, pane_id) {
            Ok(instance) => json_response(200, json!({ "instance": instance }), cors_origin),
            Err(error) => map_app_error(error, cors_origin),
        },
        ("POST", ["instances", pane_id, "actions", action]) => {
            match run_instance_action(runtime, request, pane_id, action) {
                Ok(result) => json_response(200, result, cors_origin),
                Err(error) => map_app_error(error, cors_origin),
            }
        }
        ("POST", ["instances", pane_id, "interactions", option_id]) => {
            match run_instance_interaction(runtime, request, pane_id, option_id) {
                Ok(result) => json_response(200, result, cors_origin),
                Err(error) => map_app_error(error, cors_origin),
            }
        }
        ("POST", ["instances", pane_id, "prompt"]) => {
            let body = match serde_json::from_slice::<serde_json::Value>(body) {
                Ok(body) => body,
                Err(error) => {
                    return error_response(400, format!("invalid JSON body: {error}"), cors_origin)
                }
            };
            match run_instance_prompt(runtime, request, pane_id, &body) {
                Ok(result) => json_response(200, result, cors_origin),
                Err(error) => map_app_error(error, cors_origin),
            }
        }
        _ => error_response(404, format!("unknown route: {method} {path}"), cors_origin),
    }
}

fn list_instances(runtime: &RuntimeClient, request: &ServeRequest) -> AppResult<Vec<serde_json::Value>> {
    runtime
        .list_panes(Some(&request.session_name), request.target_pane.as_deref())?
        .into_iter()
        .map(|snapshot| build_instance_summary_json(&snapshot))
        .collect()
}

fn instance_detail(
    runtime: &RuntimeClient,
    request: &ServeRequest,
    pane_id: &str,
) -> AppResult<serde_json::Value> {
    let snapshot = runtime
        .get_pane(pane_id, Some(&request.session_name))?
        .ok_or_else(|| AppError::with_exit_code(format!("pane not found: {pane_id}"), 404))?;
    let mut detail = build_instance_detail_json(&snapshot)?;
    if let Some(object) = detail.as_object_mut() {
        object.insert(
            String::from("interactions"),
            interaction_detail_json(&snapshot.to_inspected_pane()),
        );
        object.insert(String::from("controls"), serde_json::json!(available_controls_json()?));
    }
    Ok(detail)
}

fn run_instance_action(
    runtime: &RuntimeClient,
    request: &ServeRequest,
    pane_id: &str,
    action: &str,
) -> AppResult<serde_json::Value> {
    let result = runtime.run_action(pane_id, Some(&request.session_name), action)?;

    Ok(json!({
        "ok": true,
        "pane_id": result.pane_id,
        "action": result.action,
        "executed": result.executed,
        "after_state": result.after_state,
        "detail": result.detail,
    }))
}

fn run_instance_interaction(
    runtime: &RuntimeClient,
    request: &ServeRequest,
    pane_id: &str,
    option_id: &str,
) -> AppResult<serde_json::Value> {
    let snapshot = runtime
        .get_pane(pane_id, Some(&request.session_name))?
        .ok_or_else(|| AppError::with_exit_code(format!("pane not found: {pane_id}"), 404))?;
    let inspected = snapshot.to_inspected_pane();
    let interaction = parse_interaction_surface(&inspected).ok_or_else(|| {
        AppError::with_exit_code("no interactive options are visible for this pane", 409)
    })?;
    let option = interaction
        .options
        .iter()
        .find(|option| option.id == option_id)
        .ok_or_else(|| AppError::with_exit_code(format!("unknown option id: {option_id}"), 404))?;

    match interaction.mode {
        InteractionMode::SurveyDigits => {
            runtime.run_action(pane_id, Some(&request.session_name), &format!("send-text:{}", option.id))?;
        }
        InteractionMode::NumberedOptions => {
            let current = interaction.selected_option.unwrap_or(1);
            let target = option.id.parse::<usize>().map_err(|_| {
                AppError::new(format!("invalid numbered option id: {}", option.id))
            })?;
            if target > current {
                for _ in 0..(target - current) {
                    runtime.run_action(pane_id, Some(&request.session_name), AutomationAction::ConfirmNext.as_str())?;
                }
            } else if current > target {
                for _ in 0..(current - target) {
                    runtime.run_action(pane_id, Some(&request.session_name), AutomationAction::ConfirmPrevious.as_str())?;
                }
            }
            runtime.run_action(pane_id, Some(&request.session_name), "enter")?;
        }
        InteractionMode::Readonly => {
            return Err(AppError::with_exit_code(
                "visible options are not safely selectable yet; use low-level controls instead",
                409,
            ));
        }
    }

    Ok(json!({
        "ok": true,
        "pane_id": pane_id,
        "selected_option": interaction_option_json(option),
        "interaction_mode": interaction.mode.as_str(),
    }))
}

fn run_instance_prompt(
    runtime: &RuntimeClient,
    request: &ServeRequest,
    pane_id: &str,
    body: &serde_json::Value,
) -> AppResult<serde_json::Value> {
    let prompt_text = body
        .get("text")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .ok_or_else(|| AppError::new("prompt body requires non-empty string field `text`"))?;
    let workspace = body.get("workspace").and_then(serde_json::Value::as_str);
    let submit_delay_ms = body
        .get("submit_delay_ms")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(100);
    if submit_delay_ms == 0 {
        return Err(AppError::new("prompt body requires `submit_delay_ms` to be at least 1"));
    }

    let submitted = runtime.submit_prompt(
        &request.session_name,
        pane_id,
        workspace,
        prompt_text,
        submit_delay_ms,
    )?;
    Ok(json!({
        "ok": true,
        "pane_id": submitted.pane_id,
        "action": submitted.action,
        "executed": submitted.executed,
        "after_state": submitted.after_state,
        "detail": submitted.detail,
    }))
}

fn classification_json(inspected: &InspectedPane) -> serde_json::Value {
    json!({
        "source": inspected.classification.source,
        "state": inspected.classification.state.as_str(),
        "has_questions": inspected.classification.has_questions,
        "recap_present": inspected.classification.recap_present,
        "recap_excerpt": inspected.classification.recap_excerpt,
        "signals": inspected.classification.signals,
    })
}

fn permission_prompt_json(inspected: &InspectedPane) -> serde_json::Value {
    match extract_permission_prompt_details(inspected) {
        Some(details) => json!({
            "prompt_type": details.prompt_type,
            "sandbox_mode": details.sandbox_mode,
            "command": details.command,
            "reason": details.reason,
            "question": details.question,
        }),
        None => serde_json::Value::Null,
    }
}

fn interaction_summary_json(inspected: &InspectedPane) -> serde_json::Value {
    match parse_interaction_surface(inspected) {
        Some(surface) => json!({
            "mode": surface.mode.as_str(),
            "selected_option": surface.selected_option,
            "options": surface
                .options
                .iter()
                .map(interaction_option_json)
                .collect::<Vec<_>>(),
        }),
        None => serde_json::Value::Null,
    }
}

fn interaction_detail_json(inspected: &InspectedPane) -> serde_json::Value {
    match parse_interaction_surface(inspected) {
        Some(surface) => json!({
            "mode": surface.mode.as_str(),
            "selected_option": surface.selected_option,
            "options": surface
                .options
                .iter()
                .map(interaction_option_json)
                .collect::<Vec<_>>(),
            "source": inspected.focused_source,
        }),
        None => serde_json::Value::Null,
    }
}

fn available_controls_json() -> AppResult<Vec<serde_json::Value>> {
    let bindings = load_automation_keybindings(None)?;
    let mut controls = Vec::new();
    for action in [
        AutomationAction::ConfirmPrevious,
        AutomationAction::ConfirmNext,
        AutomationAction::ConfirmYes,
        AutomationAction::ConfirmNo,
        AutomationAction::Interrupt,
    ] {
        let bound = keys_for_action(&bindings, action).is_ok();
        controls.push(json!({
            "id": action.as_str(),
            "bound": bound,
        }));
    }
    controls.push(json!({ "id": "enter", "bound": true }));
    Ok(controls)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InteractionMode {
    NumberedOptions,
    SurveyDigits,
    Readonly,
}

impl InteractionMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::NumberedOptions => "numbered-options",
            Self::SurveyDigits => "survey-digits",
            Self::Readonly => "readonly",
        }
    }
}

#[derive(Debug, Clone)]
struct InteractionSurface {
    mode: InteractionMode,
    selected_option: Option<usize>,
    options: Vec<InteractionOption>,
}

#[derive(Debug, Clone)]
struct InteractionOption {
    id: String,
    index: Option<usize>,
    label: String,
    selected: bool,
    kind: &'static str,
}

fn interaction_option_json(option: &InteractionOption) -> serde_json::Value {
    json!({
        "id": option.id,
        "index": option.index,
        "label": option.label,
        "selected": option.selected,
        "kind": option.kind,
    })
}

fn parse_interaction_surface(inspected: &InspectedPane) -> Option<InteractionSurface> {
    if let Some(surface) = parse_numbered_interaction(inspected) {
        return Some(surface);
    }
    if let Some(surface) = parse_survey_digit_interaction(inspected) {
        return Some(surface);
    }
    parse_diff_readonly_interaction(inspected)
}

fn parse_numbered_interaction(inspected: &InspectedPane) -> Option<InteractionSurface> {
    let mut options = Vec::new();
    let mut selected = None;
    for line in inspected.focused_source.lines() {
        let trimmed = line.trim();
        let is_selected = trimmed.starts_with('❯');
        let candidate = trimmed.trim_start_matches('❯').trim();
        let Some((index, label)) = split_numbered_option_line(candidate) else {
            continue;
        };
        if is_selected {
            selected = Some(index);
        }
        options.push(InteractionOption {
            id: index.to_string(),
            index: Some(index),
            label: label.clone(),
            selected: is_selected,
            kind: option_kind(&label),
        });
    }

    if options.is_empty() {
        return None;
    }

    if selected.is_none()
        && matches!(
            inspected.classification.state,
            SessionState::PermissionDialog
                | SessionState::PlanApprovalPrompt
                | SessionState::FolderTrustPrompt
        )
    {
        selected = Some(1);
    }

    Some(InteractionSurface {
        mode: InteractionMode::NumberedOptions,
        selected_option: selected,
        options,
    })
}

fn parse_survey_digit_interaction(inspected: &InspectedPane) -> Option<InteractionSurface> {
    if inspected.classification.state != SessionState::SurveyPrompt {
        return None;
    }

    let mut options = Vec::new();
    for line in inspected.focused_source.lines() {
        let mut rest = line.trim();
        while let Some((id, label, remainder)) = split_digit_option(rest) {
            options.push(InteractionOption {
                id: id.to_string(),
                index: Some(id),
                label: label.clone(),
                selected: false,
                kind: option_kind(&label),
            });
            rest = remainder.trim_start();
        }
    }

    if options.is_empty() {
        return None;
    }

    Some(InteractionSurface {
        mode: InteractionMode::SurveyDigits,
        selected_option: None,
        options,
    })
}

fn parse_diff_readonly_interaction(inspected: &InspectedPane) -> Option<InteractionSurface> {
    if inspected.classification.state != SessionState::DiffDialog {
        return None;
    }

    let known = ["Keep changes", "Discard changes", "View details", "Accept", "Reject"];
    let options = inspected
        .focused_source
        .lines()
        .map(str::trim)
        .filter(|line| known.iter().any(|known_line| line == known_line))
        .map(|line| {
            InteractionOption {
                id: line.to_string(),
                index: None,
                label: line.to_string(),
                selected: false,
                kind: option_kind(line),
            }
        })
        .collect::<Vec<_>>();
    if options.is_empty() {
        None
    } else {
        Some(InteractionSurface {
            mode: InteractionMode::Readonly,
            selected_option: None,
            options,
        })
    }
}

fn split_numbered_option_line(line: &str) -> Option<(usize, String)> {
    let digits = line.chars().take_while(|ch| ch.is_ascii_digit()).count();
    if digits == 0 || !line[digits..].starts_with('.') {
        return None;
    }
    let index = line[..digits].parse::<usize>().ok()?;
    let label = line[digits + 1..].trim();
    if label.is_empty() {
        None
    } else {
        Some((index, label.to_string()))
    }
}

fn split_digit_option(line: &str) -> Option<(usize, String, &str)> {
    let digits = line.chars().take_while(|ch| ch.is_ascii_digit()).count();
    if digits == 0 || !line[digits..].starts_with(':') {
        return None;
    }
    let index = line[..digits].parse::<usize>().ok()?;
    let rest = &line[digits + 1..];
    let next = rest.match_indices("  ").find_map(|(idx, _)| {
        let candidate = rest[idx..].trim_start();
        let digit_count = candidate.chars().take_while(|ch| ch.is_ascii_digit()).count();
        if digit_count > 0 && candidate[digit_count..].starts_with(':') {
            Some(idx)
        } else {
            None
        }
    });
    let (label, remainder) = match next {
        Some(idx) => (&rest[..idx], &rest[idx..]),
        None => (rest, ""),
    };
    let label = label.trim();
    if label.is_empty() {
        None
    } else {
        Some((index, label.to_string(), remainder))
    }
}

fn option_kind(label: &str) -> &'static str {
    let lower = label.to_ascii_lowercase();
    if lower.starts_with("yes") || lower.starts_with("allow") {
        "affirm"
    } else if lower.starts_with("no") || lower.starts_with("reject") || lower.starts_with("discard") {
        "deny"
    } else if lower.contains("always") || lower.contains("don't ask again") {
        "persist"
    } else if lower.contains("dismiss") {
        "dismiss"
    } else if lower.contains("detail") || lower.contains("review") {
        "details"
    } else {
        "option"
    }
}

fn url_decode(input: &str) -> Result<String, String> {
    let mut out = String::new();
    let bytes = input.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3])
                    .map_err(|_| format!("invalid percent-encoding in {input}"))?;
                let value = u8::from_str_radix(hex, 16)
                    .map_err(|_| format!("invalid percent-encoding in {input}"))?;
                out.push(char::from(value));
                i += 3;
            }
            b'+' => {
                out.push(' ');
                i += 1;
            }
            byte => {
                out.push(char::from(byte));
                i += 1;
            }
        }
    }
    Ok(out)
}

#[derive(Debug)]
struct HttpResponse {
    status: u16,
    body: Vec<u8>,
    cors_origin: Option<String>,
}

fn validate_origin(
    origin: Option<&str>,
    allowed_origins: &[String],
) -> Result<Option<String>, HttpResponse> {
    let Some(origin) = origin else {
        return Ok(None);
    };
    if allowed_origins.iter().any(|allowed| allowed == origin) {
        Ok(Some(origin.to_string()))
    } else {
        Err(error_response(
            403,
            format!("origin not allowed: {origin}"),
            None,
        ))
    }
}

fn json_response(status: u16, body: serde_json::Value, cors_origin: Option<String>) -> HttpResponse {
    HttpResponse {
        status,
        body: serde_json::to_vec_pretty(&body).unwrap_or_else(|_| b"{}".to_vec()),
        cors_origin,
    }
}

fn error_response(status: u16, message: impl Into<String>, cors_origin: Option<String>) -> HttpResponse {
    json_response(status, json!({ "ok": false, "error": message.into() }), cors_origin)
}

fn map_app_error(error: AppError, cors_origin: Option<String>) -> HttpResponse {
    let status = match error.exit_code() {
        404 => 404,
        409 => 409,
        _ => 400,
    };
    error_response(status, error.to_string(), cors_origin)
}

fn write_response(stream: &mut TcpStream, response: HttpResponse) -> AppResult<()> {
    let reason = match response.status {
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        409 => "Conflict",
        501 => "Not Implemented",
        _ => "Error",
    };
    let mut headers = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
        response.status,
        reason,
        response.body.len()
    );
    if let Some(origin) = &response.cors_origin {
        headers.push_str(&format!(
            "Access-Control-Allow-Origin: {origin}\r\nAccess-Control-Allow-Methods: GET, POST, OPTIONS\r\nAccess-Control-Allow-Headers: Content-Type\r\n"
        ));
    }
    headers.push_str("\r\n");
    stream.write_all(headers.as_bytes())?;
    stream.write_all(&response.body)?;
    stream.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{split_digit_option, split_numbered_option_line, url_decode, validate_origin};

    #[test]
    fn parses_numbered_option_line() {
        let parsed = split_numbered_option_line("2. Yes, always allow access").unwrap();
        assert_eq!(parsed.0, 2);
        assert_eq!(parsed.1, "Yes, always allow access");
    }

    #[test]
    fn parses_inline_digit_options() {
        let first = split_digit_option("1: Bad    2: Fine   3: Good   0: Dismiss").unwrap();
        assert_eq!(first.0, 1);
        assert_eq!(first.1, "Bad");
        let second = split_digit_option(first.2.trim_start()).unwrap();
        assert_eq!(second.0, 2);
        assert_eq!(second.1, "Fine");
    }

    #[test]
    fn decodes_percent_escaped_pane_ids() {
        assert_eq!(url_decode("%251").unwrap(), "%1");
    }

    #[test]
    fn allows_exact_configured_origin() {
        assert_eq!(
            validate_origin(
                Some("http://localhost:3000"),
                &[String::from("http://localhost:3000")],
            )
            .unwrap(),
            Some(String::from("http://localhost:3000"))
        );
    }

    #[test]
    fn rejects_unconfigured_origin() {
        let response = validate_origin(
            Some("http://evil.example"),
            &[String::from("http://localhost:3000")],
        )
        .expect_err("origin should be rejected");
        assert_eq!(response.status, 403);
    }
}
