use std::collections::{BTreeMap, BTreeSet};

use crate::app::{AppError, AppResult};
use crate::classifier::{Classification, Classifier};
use crate::tmux::TmuxClient;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlEvent {
    Output {
        pane_id: String,
        payload: String,
    },
    ExtendedOutput {
        pane_id: String,
        lag_ms: u64,
        payload: String,
    },
    Notification(String),
}

#[derive(Debug, Clone)]
pub struct ObserveRequest {
    pub session_name: String,
    pub target_pane: Option<String>,
    pub max_events: usize,
    pub idle_timeout_ms: u64,
    pub history_lines: usize,
}

#[derive(Debug, Clone)]
pub struct ObserveReport {
    pub session_name: String,
    pub target_pane: String,
    pub output_events: usize,
    pub notifications: usize,
    pub seen_panes: Vec<String>,
    pub live_excerpt: String,
    pub captured_frame: String,
    pub classification: Classification,
}

#[derive(Debug, Clone)]
pub struct CollectedObservation {
    pub raw_control_lines: Vec<String>,
    pub report: ObserveReport,
}

impl ObserveReport {
    pub fn render(&self) -> String {
        format!(
            "session={}\npane={}\noutput_events={}\nnotifications={}\nseen_panes={}\n{}\nlive_excerpt:\n{}\n---\ncaptured_frame:\n{}",
            self.session_name,
            self.target_pane,
            self.output_events,
            self.notifications,
            if self.seen_panes.is_empty() {
                String::from("none")
            } else {
                self.seen_panes.join(", ")
            },
            self.classification.render(),
            trim_block(&self.live_excerpt, 20),
            trim_block(&self.captured_frame, 40)
        )
    }
}

pub fn observe_session(client: &TmuxClient, request: &ObserveRequest) -> AppResult<ObserveReport> {
    collect_observation(client, request).map(|collected| collected.report)
}

pub fn collect_observation(
    client: &TmuxClient,
    request: &ObserveRequest,
) -> AppResult<CollectedObservation> {
    let raw_lines = client.control_mode_lines(
        &request.session_name,
        request.max_events,
        request.idle_timeout_ms,
    )?;

    let mut output_events = 0usize;
    let mut notifications = 0usize;
    let mut seen_panes = BTreeSet::new();
    let mut pane_output = BTreeMap::<String, String>::new();

    for line in &raw_lines {
        match parse_control_line(line) {
            Some(ControlEvent::Output { pane_id, payload }) => {
                output_events += 1;
                seen_panes.insert(pane_id.clone());
                pane_output
                    .entry(pane_id)
                    .or_default()
                    .push_str(&decode_tmux_escaped(&payload));
            }
            Some(ControlEvent::ExtendedOutput {
                pane_id, payload, ..
            }) => {
                output_events += 1;
                seen_panes.insert(pane_id.clone());
                pane_output
                    .entry(pane_id)
                    .or_default()
                    .push_str(&decode_tmux_escaped(&payload));
            }
            Some(ControlEvent::Notification(_)) => {
                notifications += 1;
            }
            None => {}
        }
    }

    let target_pane = match &request.target_pane {
        Some(pane_id) => pane_id.clone(),
        None => client
            .active_pane_for_session(&request.session_name)?
            .map(|pane| pane.pane_id)
            .or_else(|| seen_panes.iter().next().cloned())
            .ok_or_else(|| {
                AppError::new(format!(
                    "could not resolve a target pane for session {}",
                    request.session_name
                ))
            })?,
    };

    let live_excerpt = pane_output.remove(&target_pane).unwrap_or_default();
    let captured_frame = client.capture_pane(&target_pane, request.history_lines)?;
    let classification = Classifier.classify(&target_pane, &captured_frame);

    Ok(CollectedObservation {
        raw_control_lines: raw_lines,
        report: ObserveReport {
            session_name: request.session_name.clone(),
            target_pane,
            output_events,
            notifications,
            seen_panes: seen_panes.into_iter().collect(),
            live_excerpt,
            captured_frame,
            classification,
        },
    })
}

pub fn parse_control_line(line: &str) -> Option<ControlEvent> {
    if let Some(rest) = line.strip_prefix("%output ") {
        let (pane_id, payload) = rest.split_once(' ')?;
        return Some(ControlEvent::Output {
            pane_id: pane_id.to_string(),
            payload: payload.to_string(),
        });
    }

    if let Some(rest) = line.strip_prefix("%extended-output ") {
        let mut parts = rest.splitn(4, ' ');
        let pane_id = parts.next()?.to_string();
        let lag_ms = parts.next()?.parse::<u64>().ok()?;
        let colon = parts.next()?;
        let payload = parts.next()?.to_string();
        if colon != ":" {
            return None;
        }
        return Some(ControlEvent::ExtendedOutput {
            pane_id,
            lag_ms,
            payload,
        });
    }

    if line.starts_with('%') {
        return Some(ControlEvent::Notification(line.to_string()));
    }

    None
}

pub fn decode_tmux_escaped(input: &str) -> String {
    let mut bytes = Vec::with_capacity(input.len());
    let raw = input.as_bytes();
    let mut i = 0usize;

    while i < raw.len() {
        if raw[i] == b'\\'
            && i + 3 < raw.len()
            && raw[i + 1].is_ascii_digit()
            && raw[i + 2].is_ascii_digit()
            && raw[i + 3].is_ascii_digit()
        {
            let octal = &input[i + 1..i + 4];
            if let Ok(value) = u8::from_str_radix(octal, 8) {
                bytes.push(value);
                i += 4;
                continue;
            }
        }

        bytes.push(raw[i]);
        i += 1;
    }

    String::from_utf8_lossy(&bytes).into_owned()
}

fn trim_block(input: &str, max_lines: usize) -> String {
    let lines: Vec<&str> = input.lines().collect();
    if lines.len() <= max_lines {
        return input.trim_end().to_string();
    }

    lines[lines.len() - max_lines..].join("\n")
}

#[cfg(test)]
mod tests {
    use super::{ControlEvent, decode_tmux_escaped, parse_control_line};

    #[test]
    fn parses_output_event() {
        let event = parse_control_line("%output %3 hello world").expect("output event");
        assert_eq!(
            event,
            ControlEvent::Output {
                pane_id: String::from("%3"),
                payload: String::from("hello world"),
            }
        );
    }

    #[test]
    fn parses_extended_output_event() {
        let event =
            parse_control_line("%extended-output %9 123 : prompt").expect("extended output event");
        assert_eq!(
            event,
            ControlEvent::ExtendedOutput {
                pane_id: String::from("%9"),
                lag_ms: 123,
                payload: String::from("prompt"),
            }
        );
    }

    #[test]
    fn decodes_tmux_octal_escapes() {
        let decoded = decode_tmux_escaped("hello\\040world\\015\\012");
        assert_eq!(decoded, "hello world\r\n");
    }
}
