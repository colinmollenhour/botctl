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
    ACTION_GUARD_HISTORY_LINES, AppError, AppResult, ContinueOutcome, InspectedPane, classify_pane,
    continue_from_classification, ensure_pane_owned_by_claude, ensure_workflow_state,
    execute_automation_action, execute_classified_workflow, extract_permission_prompt_details,
    inspect_pane, keys_for_action, load_automation_keybindings, render_next_safe_action,
    render_screen_excerpt, submit_prompt_for_pane,
};
use crate::automation::{AutomationAction, GuardedWorkflow, inspect_keybindings};
use crate::classifier::SessionState;
use crate::serve::ServeRequest;
use crate::tmux::{TmuxClient, TmuxPane};

const HTTP_POLL_MS: u64 = 200;

pub fn spawn_http_server(
    client: TmuxClient,
    request: ServeRequest,
    bind_addr: String,
    interrupted: Arc<AtomicBool>,
) -> thread::JoinHandle<AppResult<()>> {
    thread::spawn(move || run_http_server(client, request, &bind_addr, interrupted))
}

fn run_http_server(
    client: TmuxClient,
    request: ServeRequest,
    bind_addr: &str,
    interrupted: Arc<AtomicBool>,
) -> AppResult<()> {
    let listener = TcpListener::bind(bind_addr).map_err(|error| {
        AppError::new(format!("failed to bind http api on {bind_addr}: {error}"))
    })?;
    listener.set_nonblocking(true).map_err(|error| {
        AppError::new(format!("failed to configure http api listener: {error}"))
    })?;

    loop {
        if interrupted.load(Ordering::SeqCst) {
            return Ok(());
        }

        match listener.accept() {
            Ok((stream, _)) => {
                if let Err(error) = handle_connection(stream, &client, &request) {
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

fn handle_connection(
    stream: TcpStream,
    client: &TmuxClient,
    request: &ServeRequest,
) -> AppResult<()> {
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

    let response = handle_request(method, target, &body, origin.as_deref(), client, request);
    let mut stream = reader.into_inner();
    write_response(&mut stream, response)
}

fn handle_request(
    method: &str,
    target: &str,
    body: &[u8],
    origin: Option<&str>,
    client: &TmuxClient,
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
        ("GET", ["instances"]) => match list_instances(client, request) {
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
        ("GET", ["instances", pane_id]) => match instance_detail(client, request, pane_id) {
            Ok(instance) => json_response(200, json!({ "instance": instance }), cors_origin),
            Err(error) => map_app_error(error, cors_origin),
        },
        ("POST", ["instances", pane_id, "actions", action]) => {
            match run_instance_action(client, request, pane_id, action) {
                Ok(result) => json_response(200, result, cors_origin),
                Err(error) => map_app_error(error, cors_origin),
            }
        }
        ("POST", ["instances", pane_id, "interactions", option_id]) => {
            match run_instance_interaction(client, request, pane_id, option_id) {
                Ok(result) => json_response(200, result, cors_origin),
                Err(error) => map_app_error(error, cors_origin),
            }
        }
        ("POST", ["instances", pane_id, "prompt"]) => {
            let body = match serde_json::from_slice::<serde_json::Value>(body) {
                Ok(body) => body,
                Err(error) => {
                    return error_response(400, format!("invalid JSON body: {error}"), cors_origin);
                }
            };
            match run_instance_prompt(client, request, pane_id, &body) {
                Ok(result) => json_response(200, result, cors_origin),
                Err(error) => map_app_error(error, cors_origin),
            }
        }
        _ => error_response(404, format!("unknown route: {method} {path}"), cors_origin),
    }
}

fn list_instances(
    client: &TmuxClient,
    request: &ServeRequest,
) -> AppResult<Vec<serde_json::Value>> {
    list_requested_panes(client, request)?
        .into_iter()
        .map(|pane| build_instance_summary(client, &pane))
        .collect()
}

fn instance_detail(
    client: &TmuxClient,
    request: &ServeRequest,
    pane_id: &str,
) -> AppResult<serde_json::Value> {
    let pane = resolve_api_pane(client, request, pane_id)?;
    let inspected = inspect_pane(client, &pane.pane_id, request.history_lines)?;
    Ok(build_instance_detail_value(&pane, &inspected)?)
}

fn build_instance_summary(client: &TmuxClient, pane: &TmuxPane) -> AppResult<serde_json::Value> {
    let inspected = inspect_pane(client, &pane.pane_id, ACTION_GUARD_HISTORY_LINES)?;
    let bindings = inspect_keybindings(None).map_err(AppError::new)?;
    Ok(json!({
        "id": pane.pane_id,
        "pane": pane_json(pane),
        "owned_by_claude": pane.current_command == "claude",
        "classification": classification_json(&inspected),
        "screen_excerpt": render_screen_excerpt(&inspected.raw_source),
        "next_safe_action": render_next_safe_action(&inspected.classification, pane, &bindings),
        "interactions": interaction_summary_json(&inspected),
    }))
}

fn build_instance_detail_value(
    pane: &TmuxPane,
    inspected: &InspectedPane,
) -> AppResult<serde_json::Value> {
    let bindings = inspect_keybindings(None).map_err(AppError::new)?;
    let controls = available_controls_json()?;
    Ok(json!({
        "id": pane.pane_id,
        "pane": pane_json(pane),
        "owned_by_claude": pane.current_command == "claude",
        "classification": classification_json(inspected),
        "screen": {
            "excerpt": render_screen_excerpt(&inspected.raw_source),
            "focused_source": inspected.focused_source,
        },
        "next_safe_action": render_next_safe_action(&inspected.classification, pane, &bindings),
        "prompt": permission_prompt_json(inspected),
        "interactions": interaction_detail_json(inspected),
        "controls": controls,
    }))
}

fn run_instance_action(
    client: &TmuxClient,
    request: &ServeRequest,
    pane_id: &str,
    action: &str,
) -> AppResult<serde_json::Value> {
    let pane = resolve_api_pane(client, request, pane_id)?;
    ensure_pane_owned_by_claude(&pane)?;
    let classification = classify_pane(client, &pane.pane_id, ACTION_GUARD_HISTORY_LINES)?;
    let result = match action {
        "approve-permission" => {
            ensure_workflow_state(GuardedWorkflow::ApprovePermission, &classification)?;
            execute_classified_workflow(
                client,
                &pane.pane_id,
                GuardedWorkflow::ApprovePermission,
                &classification,
            )?
        }
        "reject-permission" => {
            ensure_workflow_state(GuardedWorkflow::RejectPermission, &classification)?;
            execute_classified_workflow(
                client,
                &pane.pane_id,
                GuardedWorkflow::RejectPermission,
                &classification,
            )?
        }
        "dismiss-survey" => {
            ensure_workflow_state(GuardedWorkflow::DismissSurvey, &classification)?;
            execute_classified_workflow(
                client,
                &pane.pane_id,
                GuardedWorkflow::DismissSurvey,
                &classification,
            )?
        }
        "confirm-previous" => {
            execute_automation_action(client, &pane.pane_id, AutomationAction::ConfirmPrevious)?
        }
        "confirm-next" => {
            execute_automation_action(client, &pane.pane_id, AutomationAction::ConfirmNext)?
        }
        "confirm-yes" => {
            execute_automation_action(client, &pane.pane_id, AutomationAction::ConfirmYes)?
        }
        "confirm-no" => {
            execute_automation_action(client, &pane.pane_id, AutomationAction::ConfirmNo)?
        }
        "interrupt" => {
            execute_automation_action(client, &pane.pane_id, AutomationAction::Interrupt)?
        }
        "continue-session" => {
            let outcome = continue_from_classification(client, &pane, &classification)?;
            return Ok(continue_outcome_json(&pane, &outcome));
        }
        "auto-unstick" => {
            return run_instance_auto_unstick(client, &pane);
        }
        "enter" => {
            client.send_keys(&pane.pane_id, &["Enter"])?;
            String::from("enter")
        }
        _ => {
            return Err(AppError::with_exit_code(
                format!("unsupported action: {action}"),
                404,
            ));
        }
    };

    Ok(json!({
        "ok": true,
        "pane_id": pane.pane_id,
        "action": action,
        "executed": result,
    }))
}

fn run_instance_interaction(
    client: &TmuxClient,
    request: &ServeRequest,
    pane_id: &str,
    option_id: &str,
) -> AppResult<serde_json::Value> {
    let pane = resolve_api_pane(client, request, pane_id)?;
    ensure_pane_owned_by_claude(&pane)?;
    let inspected = inspect_pane(client, &pane.pane_id, request.history_lines)?;
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
            client.send_keys(&pane.pane_id, &[option.id.as_str()])?;
        }
        InteractionMode::NumberedOptions => {
            let bindings = load_automation_keybindings(None)?;
            let current = interaction.selected_option.unwrap_or(1);
            let target = option
                .id
                .parse::<usize>()
                .map_err(|_| AppError::new(format!("invalid numbered option id: {}", option.id)))?;
            if target > current {
                let keys = keys_for_action(&bindings, AutomationAction::ConfirmNext)?;
                for _ in 0..(target - current) {
                    client.send_keys(&pane.pane_id, keys)?;
                }
            } else if current > target {
                let keys = keys_for_action(&bindings, AutomationAction::ConfirmPrevious)?;
                for _ in 0..(current - target) {
                    client.send_keys(&pane.pane_id, keys)?;
                }
            }
            client.send_keys(&pane.pane_id, &["Enter"])?;
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
        "pane_id": pane.pane_id,
        "selected_option": interaction_option_json(option),
        "interaction_mode": interaction.mode.as_str(),
    }))
}

fn run_instance_prompt(
    client: &TmuxClient,
    request: &ServeRequest,
    pane_id: &str,
    body: &serde_json::Value,
) -> AppResult<serde_json::Value> {
    let pane = resolve_api_pane(client, request, pane_id)?;
    ensure_pane_owned_by_claude(&pane)?;
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
        return Err(AppError::new(
            "prompt body requires `submit_delay_ms` to be at least 1",
        ));
    }

    let submitted = submit_prompt_for_pane(
        client,
        &pane,
        &request.session_name,
        request.state_dir.as_deref(),
        workspace,
        prompt_text,
        submit_delay_ms,
    )?;
    Ok(json!({
        "ok": true,
        "pane_id": submitted.pane_id,
        "workspace_id": submitted.workspace_id,
        "state": submitted.state,
        "state_db": submitted.state_db.display().to_string(),
        "delay_ms": submitted.delay_ms,
    }))
}

fn run_instance_auto_unstick(client: &TmuxClient, pane: &TmuxPane) -> AppResult<serde_json::Value> {
    let mut actions = Vec::new();
    let mut approved_permission = false;
    let mut current = classify_pane(client, &pane.pane_id, ACTION_GUARD_HISTORY_LINES)?;

    for _ in 0..3 {
        if matches!(
            current.state,
            SessionState::ChatReady | SessionState::BusyResponding
        ) {
            return Ok(json!({
                "ok": true,
                "pane_id": pane.pane_id,
                "final_state": current.state.as_str(),
                "actions": actions,
                "steps": actions.len(),
            }));
        }
        if approved_permission && current.state == SessionState::PermissionDialog {
            return Err(AppError::with_exit_code(
                format!(
                    "auto-unstick refuses to approve more than one permission dialog for pane {}",
                    pane.pane_id
                ),
                409,
            ));
        }

        let outcome = continue_from_classification(client, pane, &current)?;
        if outcome.used_permission_approval {
            approved_permission = true;
        }
        actions.push(outcome.action.clone());
        current = outcome.after;
    }

    if matches!(
        current.state,
        SessionState::ChatReady | SessionState::BusyResponding
    ) {
        Ok(json!({
            "ok": true,
            "pane_id": pane.pane_id,
            "final_state": current.state.as_str(),
            "actions": actions,
            "steps": actions.len(),
        }))
    } else {
        Err(AppError::with_exit_code(
            format!(
                "auto-unstick stopped after {} steps with pane {} still in state {}",
                actions.len(),
                pane.pane_id,
                current.state.as_str()
            ),
            409,
        ))
    }
}

fn continue_outcome_json(pane: &TmuxPane, outcome: &ContinueOutcome) -> serde_json::Value {
    json!({
        "ok": true,
        "pane_id": pane.pane_id,
        "action": outcome.action,
        "outcome": outcome.outcome,
        "after_state": outcome.after.state.as_str(),
        "used_permission_approval": outcome.used_permission_approval,
    })
}

fn resolve_api_pane(
    client: &TmuxClient,
    request: &ServeRequest,
    pane_id: &str,
) -> AppResult<TmuxPane> {
    let pane = client
        .pane_by_target(pane_id)?
        .ok_or_else(|| AppError::with_exit_code(format!("pane not found: {pane_id}"), 404))?;
    if pane.session_name != request.session_name {
        return Err(AppError::with_exit_code(
            format!(
                "pane {} belongs to session {} but this api serves {}",
                pane_id, pane.session_name, request.session_name
            ),
            404,
        ));
    }
    Ok(pane)
}

fn list_requested_panes(client: &TmuxClient, request: &ServeRequest) -> AppResult<Vec<TmuxPane>> {
    match &request.target_pane {
        Some(pane_id) => Ok(vec![resolve_api_pane(client, request, pane_id)?]),
        None => client.list_panes_for_target(Some(&request.session_name)),
    }
}

fn pane_json(pane: &TmuxPane) -> serde_json::Value {
    json!({
        "pane_id": pane.pane_id,
        "pane_tty": pane.pane_tty,
        "pane_pid": pane.pane_pid,
        "session_id": pane.session_id,
        "session_name": pane.session_name,
        "window_id": pane.window_id,
        "window_index": pane.window_index,
        "window_name": pane.window_name,
        "pane_index": pane.pane_index,
        "current_command": pane.current_command,
        "current_path": pane.current_path,
        "pane_active": pane.pane_active,
        "cursor_x": pane.cursor_x,
        "cursor_y": pane.cursor_y,
    })
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

    let known = [
        "Keep changes",
        "Discard changes",
        "View details",
        "Accept",
        "Reject",
    ];
    let options = inspected
        .focused_source
        .lines()
        .map(str::trim)
        .filter(|line| known.iter().any(|known_line| line == known_line))
        .map(|line| InteractionOption {
            id: line.to_string(),
            index: None,
            label: line.to_string(),
            selected: false,
            kind: option_kind(line),
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
        let digit_count = candidate
            .chars()
            .take_while(|ch| ch.is_ascii_digit())
            .count();
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
    } else if lower.starts_with("no") || lower.starts_with("reject") || lower.starts_with("discard")
    {
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

fn json_response(
    status: u16,
    body: serde_json::Value,
    cors_origin: Option<String>,
) -> HttpResponse {
    HttpResponse {
        status,
        body: serde_json::to_vec_pretty(&body).unwrap_or_else(|_| b"{}".to_vec()),
        cors_origin,
    }
}

fn error_response(
    status: u16,
    message: impl Into<String>,
    cors_origin: Option<String>,
) -> HttpResponse {
    json_response(
        status,
        json!({ "ok": false, "error": message.into() }),
        cors_origin,
    )
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
