use std::collections::{BTreeMap, BTreeSet};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

use crate::app::{AppError, AppResult, focused_frame_source};
use crate::classifier::{Classification, Classifier, SessionState};
use crate::observe::{ControlEvent, decode_tmux_escaped, parse_control_line};
use crate::screen_model::ScreenModel;
use crate::tmux::{ControlModeReceive, TmuxClient, TmuxPane};

const STREAM_DEBOUNCE_MS: u64 = 200;
const MAX_LIVE_EXCERPT_LINES: usize = 20;
const CONTROL_POLL_MS: u64 = 250;

#[derive(Debug, Clone)]
pub struct ServeRequest {
    pub session_name: String,
    pub target_pane: Option<String>,
    pub history_lines: usize,
    pub reconcile_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServeSnapshotReason {
    Initial,
    StreamActivity,
    Periodic,
    TopologyChange,
}

impl ServeSnapshotReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Initial => "initial",
            Self::StreamActivity => "stream-activity",
            Self::Periodic => "periodic",
            Self::TopologyChange => "topology-change",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ServePaneSnapshot {
    pub pane: TmuxPane,
    pub classification: Classification,
    pub live_excerpt: String,
    pub changed: bool,
    pub reason: ServeSnapshotReason,
}

#[derive(Debug, Clone)]
pub enum ServeEvent {
    Started {
        session_name: String,
        target_pane: Option<String>,
        tracked_panes: Vec<String>,
    },
    Snapshot(ServePaneSnapshot),
    Notification(String),
    PaneRemoved {
        pane_id: String,
    },
    Stopped {
        reason: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ServeSnapshotSignature {
    state: SessionState,
    recap_present: bool,
    recap_excerpt: Option<String>,
    signals: Vec<String>,
    current_path: String,
    current_command: String,
    window_name: String,
    pane_active: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClassificationFrameSource {
    Capture,
    Merged,
}

#[derive(Debug, Clone)]
struct FrameChoice {
    classification: Classification,
    frame_source: ClassificationFrameSource,
    live_excerpt: String,
}

impl ServeSnapshotSignature {
    fn from_snapshot(pane: &TmuxPane, classification: &Classification) -> Self {
        Self {
            state: classification.state,
            recap_present: classification.recap_present,
            recap_excerpt: classification.recap_excerpt.clone(),
            signals: classification.signals.clone(),
            current_path: pane.current_path.clone(),
            current_command: pane.current_command.clone(),
            window_name: pane.window_name.clone(),
            pane_active: pane.pane_active,
        }
    }
}

#[derive(Debug, Clone, Default)]
struct TrackedServePane {
    pane: Option<TmuxPane>,
    live_excerpt: String,
    screen_model: Option<ScreenModel>,
    screen_model_seeded: bool,
    dirty: bool,
    last_stream_activity: Option<Instant>,
    last_signature: Option<ServeSnapshotSignature>,
}

pub fn run_serve_loop<F>(
    client: &TmuxClient,
    request: &ServeRequest,
    interrupted: Arc<AtomicBool>,
    mut emit: F,
) -> AppResult<()>
where
    F: FnMut(ServeEvent) -> AppResult<()>,
{
    let mut tracked = seed_tracked_panes(client, request)?;
    if request.target_pane.is_some() && tracked.is_empty() {
        return Err(AppError::new(format!(
            "could not resolve pane {}",
            request.target_pane.as_deref().unwrap_or("unknown")
        )));
    }

    emit(ServeEvent::Started {
        session_name: request.session_name.clone(),
        target_pane: request.target_pane.clone(),
        tracked_panes: tracked.keys().cloned().collect(),
    })?;

    if let Some(reason) = reconcile_panes(
        client,
        request,
        &mut tracked,
        ServeSnapshotReason::Initial,
        &mut emit,
    )? {
        emit(ServeEvent::Stopped { reason })?;
        return Ok(());
    }

    let mut control = client.control_mode_session(&request.session_name)?;
    let mut last_periodic = Instant::now();
    let periodic_interval = Duration::from_millis(request.reconcile_ms);
    let stream_debounce = Duration::from_millis(STREAM_DEBOUNCE_MS);

    loop {
        if interrupted.load(Ordering::SeqCst) {
            emit(ServeEvent::Stopped {
                reason: String::from("sigint"),
            })?;
            return Ok(());
        }

        match control.recv_timeout(Duration::from_millis(CONTROL_POLL_MS))? {
            ControlModeReceive::Line(line) => {
                let force_topology_reconcile =
                    handle_control_line(&line, &mut tracked, &mut emit, request)?;
                if force_topology_reconcile {
                    if let Some(reason) = reconcile_panes(
                        client,
                        request,
                        &mut tracked,
                        ServeSnapshotReason::TopologyChange,
                        &mut emit,
                    )? {
                        emit(ServeEvent::Stopped { reason })?;
                        return Ok(());
                    }
                    last_periodic = Instant::now();
                }
            }
            ControlModeReceive::Idle => {}
            ControlModeReceive::Closed => {
                emit(ServeEvent::Stopped {
                    reason: String::from("control-mode-closed"),
                })?;
                return Ok(());
            }
        }

        if has_ready_dirty_panes(&tracked, stream_debounce) {
            if let Some(reason) = reconcile_panes(
                client,
                request,
                &mut tracked,
                ServeSnapshotReason::StreamActivity,
                &mut emit,
            )? {
                emit(ServeEvent::Stopped { reason })?;
                return Ok(());
            }
        }

        if last_periodic.elapsed() >= periodic_interval {
            if let Some(reason) = reconcile_panes(
                client,
                request,
                &mut tracked,
                ServeSnapshotReason::Periodic,
                &mut emit,
            )? {
                emit(ServeEvent::Stopped { reason })?;
                return Ok(());
            }
            last_periodic = Instant::now();
        }
    }
}

fn seed_tracked_panes(
    client: &TmuxClient,
    request: &ServeRequest,
) -> AppResult<BTreeMap<String, TrackedServePane>> {
    let mut tracked = BTreeMap::new();
    for pane in list_requested_panes(client, request)? {
        tracked.insert(
            pane.pane_id.clone(),
            TrackedServePane {
                pane: Some(pane),
                ..TrackedServePane::default()
            },
        );
    }
    Ok(tracked)
}

fn list_requested_panes(client: &TmuxClient, request: &ServeRequest) -> AppResult<Vec<TmuxPane>> {
    match &request.target_pane {
        Some(pane_id) => match client.pane_by_target(pane_id)? {
            Some(pane) if pane.session_name == request.session_name => Ok(vec![pane]),
            Some(pane) => Err(AppError::new(format!(
                "pane {} belongs to session {} but serve is attached to {}",
                pane_id, pane.session_name, request.session_name
            ))),
            None => Ok(Vec::new()),
        },
        None => client.list_panes_for_target(Some(&request.session_name)),
    }
}

fn handle_control_line<F>(
    line: &str,
    tracked: &mut BTreeMap<String, TrackedServePane>,
    emit: &mut F,
    request: &ServeRequest,
) -> AppResult<bool>
where
    F: FnMut(ServeEvent) -> AppResult<()>,
{
    match parse_control_line(line) {
        Some(ControlEvent::Output { pane_id, payload }) => {
            note_stream_output(tracked, request, &pane_id, &payload);
            Ok(false)
        }
        Some(ControlEvent::ExtendedOutput {
            pane_id, payload, ..
        }) => {
            note_stream_output(tracked, request, &pane_id, &payload);
            Ok(false)
        }
        Some(ControlEvent::Notification(message)) => {
            if should_emit_notification(&message) {
                emit(ServeEvent::Notification(message.clone()))?;
            }
            Ok(notification_requires_reconcile(&message))
        }
        None => Ok(false),
    }
}

fn note_stream_output(
    tracked: &mut BTreeMap<String, TrackedServePane>,
    request: &ServeRequest,
    pane_id: &str,
    payload: &str,
) {
    if let Some(target_pane) = &request.target_pane {
        if target_pane != pane_id {
            return;
        }
    }

    let entry = tracked.entry(pane_id.to_string()).or_default();
    let decoded = decode_tmux_escaped(payload);
    let model = entry
        .screen_model
        .get_or_insert_with(|| ScreenModel::new(screen_model_capacity(request)));
    model.ingest(&decoded);
    entry.live_excerpt = trim_visible_block(&model.render(), MAX_LIVE_EXCERPT_LINES);
    entry.dirty = true;
    entry.last_stream_activity = Some(Instant::now());
}

fn seed_screen_model(entry: &mut TrackedServePane, frame: &str, max_lines: usize) {
    let model = entry
        .screen_model
        .get_or_insert_with(|| ScreenModel::new(max_lines));
    model.seed(frame);
    entry.live_excerpt = trim_visible_block(&model.render(), MAX_LIVE_EXCERPT_LINES);
    entry.screen_model_seeded = true;
}

fn seed_screen_model_from_capture(entry: &mut TrackedServePane, frame: &str, max_lines: usize) {
    seed_screen_model(entry, frame, max_lines);
}

fn screen_model_capacity(request: &ServeRequest) -> usize {
    request.history_lines.max(MAX_LIVE_EXCERPT_LINES)
}

fn choose_frame_for_reconcile(
    pane_id: &str,
    reason: ServeSnapshotReason,
    entry: &TrackedServePane,
    capture_frame: &str,
) -> FrameChoice {
    let capture_source = focused_frame_source(capture_frame);
    let capture_classification = Classifier.classify(pane_id, &capture_source);
    let capture_excerpt = trim_visible_block(&capture_source, MAX_LIVE_EXCERPT_LINES);

    if reason != ServeSnapshotReason::StreamActivity {
        return FrameChoice {
            classification: capture_classification,
            frame_source: ClassificationFrameSource::Capture,
            live_excerpt: capture_excerpt,
        };
    }

    let Some(merged_frame) = entry
        .screen_model
        .as_ref()
        .filter(|model| entry.screen_model_seeded && !model.is_empty())
        .map(ScreenModel::render)
    else {
        return FrameChoice {
            classification: capture_classification,
            frame_source: ClassificationFrameSource::Capture,
            live_excerpt: capture_excerpt,
        };
    };

    let merged_source = focused_frame_source(&merged_frame);
    let merged_classification = Classifier.classify(pane_id, &merged_source);
    if should_prefer_merged_classification(&merged_classification, &capture_classification) {
        FrameChoice {
            classification: merged_classification,
            frame_source: ClassificationFrameSource::Merged,
            live_excerpt: trim_visible_block(&merged_source, MAX_LIVE_EXCERPT_LINES),
        }
    } else {
        FrameChoice {
            classification: capture_classification,
            frame_source: ClassificationFrameSource::Capture,
            live_excerpt: capture_excerpt,
        }
    }
}

fn should_prefer_merged_classification(merged: &Classification, capture: &Classification) -> bool {
    capture.state == SessionState::Unknown && merged.state != SessionState::Unknown
}

fn has_ready_dirty_panes(
    tracked: &BTreeMap<String, TrackedServePane>,
    stream_debounce: Duration,
) -> bool {
    tracked.values().any(|entry| {
        entry.dirty
            && entry
                .last_stream_activity
                .map(|timestamp| timestamp.elapsed() >= stream_debounce)
                .unwrap_or(false)
    })
}

fn reconcile_panes<F>(
    client: &TmuxClient,
    request: &ServeRequest,
    tracked: &mut BTreeMap<String, TrackedServePane>,
    reason: ServeSnapshotReason,
    emit: &mut F,
) -> AppResult<Option<String>>
where
    F: FnMut(ServeEvent) -> AppResult<()>,
{
    let current_panes = list_requested_panes(client, request)?;
    if request.target_pane.is_some() && current_panes.is_empty() {
        let pane_id = request.target_pane.as_ref().unwrap().clone();
        if tracked.remove(&pane_id).is_some() {
            emit(ServeEvent::PaneRemoved {
                pane_id: pane_id.clone(),
            })?;
        }
        return Ok(Some(String::from("missing-target-pane")));
    }

    let current_ids = current_panes
        .iter()
        .map(|pane| pane.pane_id.clone())
        .collect::<BTreeSet<_>>();

    let removed_ids = tracked
        .keys()
        .filter(|pane_id| !current_ids.contains(*pane_id))
        .cloned()
        .collect::<Vec<_>>();
    for pane_id in removed_ids {
        tracked.remove(&pane_id);
        emit(ServeEvent::PaneRemoved { pane_id })?;
    }

    let stream_targets = tracked
        .iter()
        .filter(|(_, entry)| entry.dirty)
        .map(|(pane_id, _)| pane_id.clone())
        .collect::<BTreeSet<_>>();

    for pane in current_panes {
        let pane_id = pane.pane_id.clone();
        let entry = tracked.entry(pane_id.clone()).or_default();
        entry.pane = Some(pane.clone());

        if reason == ServeSnapshotReason::StreamActivity && !stream_targets.contains(&pane_id) {
            continue;
        }

        let frame = client.capture_pane(&pane_id, request.history_lines)?;
        let frame_choice = choose_frame_for_reconcile(&pane_id, reason, entry, &frame);
        let classification = frame_choice.classification;
        let signature = ServeSnapshotSignature::from_snapshot(&pane, &classification);
        let had_previous = entry.last_signature.is_some();
        let changed = entry.last_signature.as_ref() != Some(&signature);
        entry.last_signature = Some(signature);
        entry.dirty = false;
        entry.last_stream_activity = None;

        let capture_seed = focused_frame_source(&frame);
        if !capture_seed.is_empty()
            && (reason != ServeSnapshotReason::StreamActivity
                || frame_choice.frame_source == ClassificationFrameSource::Capture)
        {
            seed_screen_model_from_capture(entry, &capture_seed, screen_model_capacity(request));
        }

        if should_emit_snapshot(reason, changed, had_previous) {
            emit(ServeEvent::Snapshot(ServePaneSnapshot {
                pane,
                classification,
                live_excerpt: frame_choice.live_excerpt,
                changed,
                reason,
            }))?;
            if reason == ServeSnapshotReason::StreamActivity {
                entry.live_excerpt.clear();
            }
        }
    }

    Ok(None)
}

fn should_emit_snapshot(reason: ServeSnapshotReason, changed: bool, had_previous: bool) -> bool {
    match reason {
        ServeSnapshotReason::Initial
        | ServeSnapshotReason::StreamActivity
        | ServeSnapshotReason::TopologyChange => true,
        ServeSnapshotReason::Periodic => !had_previous || changed,
    }
}

fn trim_visible_block(input: &str, max_lines: usize) -> String {
    if input.is_empty() {
        return String::new();
    }

    let lines = input.lines().collect::<Vec<_>>();
    if lines.len() <= max_lines {
        input.trim_start_matches('\n').to_string()
    } else {
        lines[lines.len() - max_lines..].join("\n")
    }
}

fn should_emit_notification(message: &str) -> bool {
    notification_requires_reconcile(message) || message.starts_with("%exit")
}

fn notification_requires_reconcile(message: &str) -> bool {
    message.contains("pane") || message.contains("window") || message.contains("session")
}

#[cfg(any(test, rust_analyzer))]
mod tests {
    use super::{
        ClassificationFrameSource, ServeSnapshotReason, TrackedServePane,
        choose_frame_for_reconcile, note_stream_output, seed_screen_model_from_capture,
        should_emit_notification, should_emit_snapshot, trim_visible_block,
    };
    use crate::classifier::SessionState;
    use crate::tmux::TmuxPane;

    #[test]
    fn trims_live_excerpt_to_recent_lines() {
        let excerpt = trim_visible_block("one\ntwo\nthree\nfour", 2);
        assert_eq!(excerpt, "three\nfour");
    }

    #[test]
    fn periodic_snapshots_only_emit_on_change() {
        assert!(should_emit_snapshot(
            ServeSnapshotReason::Periodic,
            true,
            true
        ));
        assert!(!should_emit_snapshot(
            ServeSnapshotReason::Periodic,
            false,
            true
        ));
        assert!(should_emit_snapshot(
            ServeSnapshotReason::Periodic,
            false,
            false
        ));
    }

    #[test]
    fn emits_topology_notifications() {
        assert!(should_emit_notification("%window-renamed @1 demo"));
        assert!(should_emit_notification("%pane-mode-changed %1"));
        assert!(!should_emit_notification("%begin 1 1"));
    }

    #[test]
    fn streamed_output_updates_screen_model() {
        let mut tracked = std::collections::BTreeMap::new();
        let request = super::ServeRequest {
            session_name: String::from("demo"),
            target_pane: Some(String::from("%1")),
            history_lines: 20,
            reconcile_ms: 1000,
        };
        tracked.insert(
            String::from("%1"),
            super::TrackedServePane {
                pane: Some(TmuxPane {
                    pane_id: String::from("%1"),
                    pane_tty: String::from("/dev/pts/1"),
                    pane_pid: None,
                    session_id: String::from("$1"),
                    session_name: String::from("demo"),
                    window_id: String::from("@1"),
                    window_index: 0,
                    window_name: String::from("main"),
                    pane_index: 0,
                    current_command: String::from("claude"),
                    current_path: String::from("/tmp"),
                    pane_active: true,
                    cursor_x: None,
                    cursor_y: None,
                }),
                ..Default::default()
            },
        );

        note_stream_output(
            &mut tracked,
            &request,
            "%1",
            "hello\rworld\nnext\x1b[31m line",
        );
        let entry = tracked.get("%1").expect("tracked pane");
        assert_eq!(
            entry.screen_model.as_ref().unwrap().render(),
            "world\nnext line"
        );
        assert_eq!(entry.live_excerpt, "world\nnext line");
    }

    #[test]
    fn seeded_capture_becomes_authoritative_base_for_streams() {
        let mut tracked = std::collections::BTreeMap::new();
        let mut entry = TrackedServePane::default();
        seed_screen_model_from_capture(&mut entry, "base\nframe", 20);
        tracked.insert(String::from("%1"), entry);

        note_stream_output(
            &mut tracked,
            &super::ServeRequest {
                session_name: String::from("demo"),
                target_pane: Some(String::from("%1")),
                history_lines: 20,
                reconcile_ms: 1000,
            },
            "%1",
            "\nstream",
        );

        let entry = tracked.get("%1").expect("tracked pane");
        assert_eq!(
            entry.screen_model.as_ref().unwrap().render(),
            "base\nframe\nstream"
        );
        assert_eq!(entry.live_excerpt, "base\nframe\nstream");
    }

    #[test]
    fn stream_activity_prefers_seeded_merged_frame_when_capture_is_unknown() {
        let mut entry = TrackedServePane::default();
        seed_screen_model_from_capture(
            &mut entry,
            "Claude Code\nMain chat input area\nEnter submit message",
            20,
        );

        let choice = choose_frame_for_reconcile(
            "%1",
            ServeSnapshotReason::StreamActivity,
            &entry,
            "stale scrollback without classifier anchors",
        );

        assert_eq!(choice.frame_source, ClassificationFrameSource::Merged);
        assert_eq!(choice.classification.state, SessionState::ChatReady);
    }

    #[test]
    fn stream_activity_falls_back_to_capture_without_seeded_model() {
        let mut entry = TrackedServePane::default();
        entry.screen_model = Some(crate::screen_model::ScreenModel::new(20));
        entry
            .screen_model
            .as_mut()
            .unwrap()
            .ingest("transient stream");

        let choice = choose_frame_for_reconcile(
            "%1",
            ServeSnapshotReason::StreamActivity,
            &entry,
            "Claude Code\nMain chat input area\nEnter submit message",
        );

        assert_eq!(choice.frame_source, ClassificationFrameSource::Capture);
        assert_eq!(choice.classification.state, SessionState::ChatReady);
    }

    #[test]
    fn stream_activity_prefers_capture_when_both_sources_agree() {
        let mut entry = TrackedServePane::default();
        seed_screen_model_from_capture(
            &mut entry,
            "Claude Code\nMain chat input area\nEnter submit message",
            20,
        );

        let choice = choose_frame_for_reconcile(
            "%1",
            ServeSnapshotReason::StreamActivity,
            &entry,
            "Claude Code\nMain chat input area\nEnter submit message",
        );

        assert_eq!(choice.frame_source, ClassificationFrameSource::Capture);
        assert_eq!(choice.classification.state, SessionState::ChatReady);
    }
}
