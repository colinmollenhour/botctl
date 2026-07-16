use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::app::{AppError, AppResult};

const CONTROL_STREAM_CAPACITY: usize = 256;

#[derive(Debug, Clone)]
pub struct StartSessionRequest {
    pub session_name: String,
    pub window_name: String,
    pub cwd: PathBuf,
    pub command: String,
}

#[derive(Debug, Clone)]
pub struct StartedSession {
    pub session_name: String,
    pub window_name: String,
    pub cwd: PathBuf,
}

#[derive(Debug, Clone)]
pub struct StartWindowRequest {
    pub session_name: String,
    pub window_name: String,
    pub cwd: PathBuf,
    pub command: String,
}

#[derive(Debug, Clone)]
pub struct StartedWindow {
    pub session_name: String,
    pub window_name: String,
    pub cwd: PathBuf,
    pub pane_id: String,
    pub window_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TmuxPane {
    pub pane_id: String,
    pub pane_tty: String,
    pub pane_pid: Option<u32>,
    pub session_id: String,
    pub session_name: String,
    pub window_id: String,
    pub window_index: u16,
    pub window_name: String,
    pub pane_index: u16,
    pub current_command: String,
    pub current_path: String,
    pub pane_title: String,
    pub pane_active: bool,
    pub cursor_x: Option<u16>,
    pub cursor_y: Option<u16>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TmuxServerIdentity {
    pub socket_path: String,
    pub pid: u32,
    pub start_time: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TmuxInventory {
    pub server: TmuxServerIdentity,
    pub panes: Vec<TmuxPane>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TmuxWindow {
    pub session_name: String,
    pub window_id: String,
    pub window_index: u16,
    pub window_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TmuxCommandPlan {
    pub program: String,
    pub args: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PasteTextPlan {
    pub set_buffer: TmuxCommandPlan,
    pub paste_buffer: TmuxCommandPlan,
}

impl TmuxCommandPlan {
    pub fn render(&self) -> String {
        let mut parts = vec![self.program.clone()];
        for arg in &self.args {
            parts.push(shell_escape(arg));
        }
        parts.join(" ")
    }
}

#[derive(Debug, Clone)]
pub struct TmuxClient {
    program: String,
    socket_name: Option<String>,
    socket_path: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlModeReceive {
    Line(String),
    Idle,
    Closed,
}

pub struct ControlModeSession {
    target_session: String,
    child: Child,
    stdout_rx: mpsc::Receiver<ControlStreamItem>,
    stderr_rx: mpsc::Receiver<String>,
}

impl Default for TmuxClient {
    fn default() -> Self {
        Self {
            program: String::from("tmux"),
            socket_name: None,
            socket_path: None,
        }
    }
}

impl TmuxClient {
    pub fn with_socket<S: Into<String>>(socket_name: S) -> Self {
        Self {
            program: String::from("tmux"),
            socket_name: Some(socket_name.into()),
            socket_path: None,
        }
    }

    pub fn with_socket_path<P: Into<PathBuf>>(socket_path: P) -> Self {
        Self {
            program: String::from("tmux"),
            socket_name: None,
            socket_path: Some(socket_path.into()),
        }
    }

    pub fn plan_start_session(&self, request: &StartSessionRequest) -> TmuxCommandPlan {
        TmuxCommandPlan {
            program: self.program.clone(),
            args: self.with_socket_args(vec![
                String::from("new-session"),
                String::from("-d"),
                String::from("-s"),
                request.session_name.clone(),
                String::from("-n"),
                request.window_name.clone(),
                String::from("-c"),
                request.cwd.display().to_string(),
                request.command.clone(),
            ]),
        }
    }

    pub fn plan_start_window(&self, request: &StartWindowRequest) -> TmuxCommandPlan {
        TmuxCommandPlan {
            program: self.program.clone(),
            args: self.with_socket_args(vec![
                String::from("new-window"),
                String::from("-d"),
                String::from("-t"),
                request.session_name.clone(),
                String::from("-n"),
                request.window_name.clone(),
                String::from("-c"),
                request.cwd.display().to_string(),
                String::from("-P"),
                String::from("-F"),
                String::from("#{pane_id}\t#{window_id}"),
                request.command.clone(),
            ]),
        }
    }

    pub fn plan_start_window_as_session(&self, request: &StartWindowRequest) -> TmuxCommandPlan {
        TmuxCommandPlan {
            program: self.program.clone(),
            args: self.with_socket_args(vec![
                String::from("new-session"),
                String::from("-d"),
                String::from("-s"),
                request.session_name.clone(),
                String::from("-n"),
                request.window_name.clone(),
                String::from("-c"),
                request.cwd.display().to_string(),
                String::from("-P"),
                String::from("-F"),
                String::from("#{pane_id}\t#{window_id}"),
                request.command.clone(),
            ]),
        }
    }

    pub fn plan_kill_session(&self, target_session: &str) -> TmuxCommandPlan {
        TmuxCommandPlan {
            program: self.program.clone(),
            args: self.with_socket_args(vec![
                String::from("kill-session"),
                String::from("-t"),
                target_session.to_string(),
            ]),
        }
    }

    pub fn start_session(&self, request: &StartSessionRequest) -> AppResult<StartedSession> {
        self.run_status(self.plan_start_session(request).args)?;
        if let Err(error) = self.disable_session_status(&request.session_name) {
            let _ = self.kill_session(&request.session_name);
            return Err(error);
        }
        Ok(StartedSession {
            session_name: request.session_name.clone(),
            window_name: request.window_name.clone(),
            cwd: request.cwd.clone(),
        })
    }

    pub fn start_window(&self, request: &StartWindowRequest) -> AppResult<StartedWindow> {
        let output = self.run_output(self.plan_start_window(request).args)?;
        Self::started_window_from_output(request, &output)
    }

    pub fn start_window_as_session(
        &self,
        request: &StartWindowRequest,
    ) -> AppResult<StartedWindow> {
        let output = self.run_output(self.plan_start_window_as_session(request).args)?;
        match self.disable_session_status(&request.session_name) {
            Ok(()) => Self::started_window_from_output(request, &output),
            Err(error) => {
                let _ = self.kill_session(&request.session_name);
                Err(error)
            }
        }
    }

    pub fn start_window_in_session(
        &self,
        request: &StartWindowRequest,
    ) -> AppResult<StartedWindow> {
        match self.start_window(request) {
            Ok(started) => Ok(started),
            Err(first_error) => match self.start_window_as_session(request) {
                Ok(started) => Ok(started),
                Err(second_error) => match self.start_window(request) {
                    Ok(started) => Ok(started),
                    Err(retry_error) => Err(AppError::new(format!(
                        "tmux window start failed: first attempt: {first_error}; fallback session creation: {second_error}; final retry: {retry_error}"
                    ))),
                },
            },
        }
    }

    fn started_window_from_output(
        request: &StartWindowRequest,
        output: &str,
    ) -> AppResult<StartedWindow> {
        let (pane_id, window_id) = parse_created_pane_window(output)
            .ok_or_else(|| AppError::new("tmux launch did not return pane_id and window_id"))?;
        Ok(StartedWindow {
            session_name: request.session_name.clone(),
            window_name: request.window_name.clone(),
            cwd: request.cwd.clone(),
            pane_id,
            window_id,
        })
    }

    pub fn list_panes(&self) -> AppResult<Vec<TmuxPane>> {
        self.list_panes_for_target(None)
    }

    /// Scheduler-critical pane discovery. Unlike `list_panes`, any malformed
    /// non-empty row fails the whole result so callers never interpret partial
    /// tmux output as pane removal.
    pub fn list_panes_strict(&self) -> AppResult<Vec<TmuxPane>> {
        let output = self.run_output(list_panes_args(None))?;
        parse_pane_listing_strict(&output)
    }

    /// Full server identity plus length-prefixed pane inventory for recovery.
    /// Rejects partial reads when server identity changes mid-collection.
    pub fn inventory(&self) -> AppResult<TmuxInventory> {
        let before = self.read_server_identity()?;
        let output = self.run_output(vec![
            String::from("list-panes"),
            String::from("-a"),
            String::from("-F"),
            pane_list_format(),
        ])?;
        let panes = parse_inventory_panes(&output)?;
        let after = self.read_server_identity()?;
        stable_inventory(before, panes, after)
    }

    fn read_server_identity(&self) -> AppResult<TmuxServerIdentity> {
        let output = self.run_output(vec![
            String::from("display-message"),
            String::from("-p"),
            String::from(
                "#{n:socket_path}:#{socket_path}#{n:pid}:#{pid}#{n:start_time}:#{start_time}",
            ),
        ])?;
        parse_server_identity(&output)
    }

    pub fn list_windows(&self) -> AppResult<Vec<TmuxWindow>> {
        let output = self.run_output(vec![
            String::from("list-windows"),
            String::from("-a"),
            String::from("-F"),
            String::from("#{session_name}\t#{window_id}\t#{window_index}\t#{window_name}"),
        ])?;
        Ok(output.lines().filter_map(parse_window_line).collect())
    }

    pub fn list_panes_for_target(&self, target: Option<&str>) -> AppResult<Vec<TmuxPane>> {
        let output = self.run_output(list_panes_args(target))?;
        let panes = output
            .lines()
            .filter_map(parse_pane_line)
            .collect::<Vec<_>>();
        Ok(panes)
    }

    pub fn active_pane_for_session(&self, session_name: &str) -> AppResult<Option<TmuxPane>> {
        let panes = self.list_panes_for_target(Some(session_name))?;
        Ok(panes.into_iter().find(|pane| pane.pane_active))
    }

    pub fn active_pane_for_window(
        &self,
        session_name: &str,
        window_name: &str,
    ) -> AppResult<Option<TmuxPane>> {
        let target = format!("{session_name}:{window_name}");
        let panes = self.list_panes_for_target(Some(&target))?;
        Ok(panes.into_iter().find(|pane| pane.pane_active))
    }

    pub fn pane_by_target(&self, target: &str) -> AppResult<Option<TmuxPane>> {
        if let Some(pane) = self.pane_by_id(target)? {
            return Ok(Some(pane));
        }

        let pane_id =
            select_explicit_pane_target(target, self.match_explicit_pane_targets(target)?)?;
        match pane_id {
            Some(pane_id) => self.pane_by_id(&pane_id),
            None => Ok(None),
        }
    }

    pub fn pane_by_id(&self, pane_id: &str) -> AppResult<Option<TmuxPane>> {
        let panes = self.list_panes()?;
        Ok(panes.into_iter().find(|pane| pane.pane_id == pane_id))
    }

    /// Refresh exactly one pane without invoking global `list-panes -a`.
    /// Parsing is strict for the same reason as `list_panes_strict`.
    pub fn pane_by_id_exact(&self, pane_id: &str) -> AppResult<Option<TmuxPane>> {
        let Some(output) = self.run_output_allow_missing_pane(list_panes_args(Some(pane_id)))?
        else {
            return Ok(None);
        };
        let panes = parse_pane_listing_strict(&output)?;
        Ok(panes.into_iter().find(|pane| pane.pane_id == pane_id))
    }

    pub fn control_mode_session(&self, target_session: &str) -> AppResult<ControlModeSession> {
        let control_command = self.control_mode_command(target_session);

        let mut child = Command::new("script")
            .arg("-q")
            .arg("-e")
            .arg("-c")
            .arg(control_command)
            .arg("/dev/null")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| AppError::new("failed to capture tmux control mode stdout"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| AppError::new("failed to capture tmux control mode stderr"))?;

        let (stdout_tx, stdout_rx) = mpsc::sync_channel(CONTROL_STREAM_CAPACITY);
        std::thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines() {
                match line {
                    Ok(line) => {
                        if stdout_tx.send(ControlStreamItem::Stdout(line)).is_err() {
                            return;
                        }
                    }
                    Err(error) => {
                        let _ = stdout_tx.send(ControlStreamItem::ReadError(error.to_string()));
                        return;
                    }
                }
            }
        });

        let (stderr_tx, stderr_rx) = mpsc::sync_channel(CONTROL_STREAM_CAPACITY);
        std::thread::spawn(move || {
            let reader = BufReader::new(stderr);
            for line in reader.lines() {
                match line {
                    Ok(line) => match stderr_tx.try_send(line) {
                        Ok(()) | Err(mpsc::TrySendError::Full(_)) => {}
                        Err(mpsc::TrySendError::Disconnected(_)) => return,
                    },
                    Err(_) => return,
                }
            }
        });

        Ok(ControlModeSession {
            target_session: target_session.to_string(),
            child,
            stdout_rx,
            stderr_rx,
        })
    }

    pub fn control_mode_lines(
        &self,
        session_name: &str,
        max_events: usize,
        idle_timeout_ms: u64,
    ) -> AppResult<Vec<String>> {
        let mut control = self.control_mode_session(session_name)?;
        let timeout = Duration::from_millis(idle_timeout_ms);
        let mut lines = Vec::new();

        while lines.len() < max_events {
            match control.recv_timeout(timeout)? {
                ControlModeReceive::Line(line) => lines.push(line),
                ControlModeReceive::Idle | ControlModeReceive::Closed => break,
            }
        }

        Ok(lines)
    }

    pub fn capture_pane(&self, pane_id: &str, history_lines: usize) -> AppResult<String> {
        self.run_output(vec![
            String::from("capture-pane"),
            String::from("-p"),
            String::from("-J"),
            String::from("-S"),
            format!("-{history_lines}"),
            String::from("-t"),
            pane_id.to_string(),
        ])
    }

    pub fn capture_pane_ansi(&self, pane_id: &str, history_lines: usize) -> AppResult<String> {
        self.run_output(vec![
            String::from("capture-pane"),
            String::from("-p"),
            String::from("-e"),
            String::from("-J"),
            String::from("-S"),
            format!("-{history_lines}"),
            String::from("-t"),
            pane_id.to_string(),
        ])
    }

    pub fn send_keys<S: AsRef<str>>(&self, pane_id: &str, keys: &[S]) -> AppResult<()> {
        let mut args = vec![
            String::from("send-keys"),
            String::from("-t"),
            pane_id.to_string(),
        ];
        args.extend(keys.iter().map(|key| key.as_ref().to_string()));
        self.run_status(args)
    }

    pub fn paste_text(&self, pane_id: &str, text: &str) -> AppResult<()> {
        let plan = self.plan_paste_text(pane_id, text);
        self.run_command_plan_status(&plan.set_buffer)
            .map_err(|_| AppError::new("tmux set-buffer failed while staging prompt text"))?;
        self.run_command_plan_status(&plan.paste_buffer)
    }

    pub fn plan_paste_text(&self, pane_id: &str, text: &str) -> PasteTextPlan {
        let buffer_name = format!("botctl-paste-{}", Uuid::now_v7());
        PasteTextPlan {
            set_buffer: TmuxCommandPlan {
                program: self.program.clone(),
                args: self.with_socket_args(vec![
                    String::from("set-buffer"),
                    String::from("-b"),
                    buffer_name.clone(),
                    String::from("--"),
                    text.to_string(),
                ]),
            },
            paste_buffer: TmuxCommandPlan {
                program: self.program.clone(),
                args: self.with_socket_args(vec![
                    String::from("paste-buffer"),
                    String::from("-d"),
                    String::from("-p"),
                    String::from("-b"),
                    buffer_name,
                    String::from("-t"),
                    pane_id.to_string(),
                ]),
            },
        }
    }

    pub fn select_window(&self, target: &str) -> AppResult<()> {
        self.run_status(vec![
            String::from("select-window"),
            String::from("-t"),
            target.to_string(),
        ])
    }

    pub fn select_pane(&self, target: &str) -> AppResult<()> {
        self.run_status(vec![
            String::from("select-pane"),
            String::from("-t"),
            target.to_string(),
        ])
    }

    pub fn switch_client(&self, target_session: &str) -> AppResult<()> {
        self.run_status(vec![
            String::from("switch-client"),
            String::from("-t"),
            target_session.to_string(),
        ])
    }

    pub fn rename_window(&self, target: &str, name: &str) -> AppResult<()> {
        self.run_status(vec![
            String::from("rename-window"),
            String::from("-t"),
            target.to_string(),
            name.to_string(),
        ])
    }

    pub fn set_global_option(&self, name: &str, value: &str) -> AppResult<()> {
        self.run_status(vec![
            String::from("set-option"),
            String::from("-g"),
            name.to_string(),
            value.to_string(),
        ])
    }

    pub fn set_session_option(
        &self,
        target_session: &str,
        name: &str,
        value: &str,
    ) -> AppResult<()> {
        self.run_status(vec![
            String::from("set-option"),
            String::from("-t"),
            target_session.to_string(),
            name.to_string(),
            value.to_string(),
        ])
    }

    fn disable_session_status(&self, target_session: &str) -> AppResult<()> {
        self.set_session_option(target_session, "status", "off")
    }

    pub fn kill_session(&self, target_session: &str) -> AppResult<()> {
        self.run_status(self.plan_kill_session(target_session).args)
    }

    pub fn kill_window(&self, target_window: &str) -> AppResult<()> {
        self.run_status(vec![
            String::from("kill-window"),
            String::from("-t"),
            target_window.to_string(),
        ])
    }

    pub fn attach_session(&self, target_session: &str) -> AppResult<()> {
        self.run_status(vec![
            String::from("attach-session"),
            String::from("-t"),
            target_session.to_string(),
        ])
    }

    pub fn detach_client(&self) -> AppResult<()> {
        self.run_status(vec![String::from("detach-client")])
    }

    pub fn has_session(&self, target_session: &str) -> AppResult<bool> {
        let status = Command::new(&self.program)
            .args(self.with_socket_args(vec![
                String::from("has-session"),
                String::from("-t"),
                target_session.to_string(),
            ]))
            .status()?;
        Ok(status.success())
    }

    pub fn display_popup(&self, width: &str, height: &str, command: &str) -> AppResult<()> {
        self.run_status(vec![
            String::from("display-popup"),
            String::from("-w"),
            width.to_string(),
            String::from("-h"),
            height.to_string(),
            String::from("-E"),
            command.to_string(),
        ])
    }

    pub fn display_message(&self, format: &str) -> AppResult<String> {
        Ok(self
            .run_output(vec![
                String::from("display-message"),
                String::from("-p"),
                format.to_string(),
            ])?
            .trim()
            .to_string())
    }

    pub fn session_attached_count(&self, target_session: &str) -> AppResult<u32> {
        let output = self.run_output(session_attached_count_args(target_session))?;
        parse_session_attached_count(&output)
    }

    fn run_status(&self, args: Vec<String>) -> AppResult<()> {
        let args = self.with_socket_args(args);
        let status = Command::new(&self.program).args(&args).status()?;
        if status.success() {
            Ok(())
        } else {
            Err(AppError::new(format!(
                "tmux command failed: {} {}",
                self.program,
                args.join(" ")
            )))
        }
    }

    fn run_command_plan_status(&self, plan: &TmuxCommandPlan) -> AppResult<()> {
        let status = Command::new(&plan.program).args(&plan.args).status()?;
        if status.success() {
            Ok(())
        } else {
            Err(AppError::new(format!(
                "tmux command failed: {} {}",
                plan.program,
                plan.args.join(" ")
            )))
        }
    }

    fn run_output(&self, args: Vec<String>) -> AppResult<String> {
        let args = self.with_socket_args(args);
        let output = Command::new(&self.program).args(&args).output()?;
        if !output.status.success() {
            return Err(AppError::new(format!(
                "tmux command failed: {} {}",
                self.program,
                args.join(" ")
            )));
        }

        String::from_utf8(output.stdout)
            .map_err(|_| AppError::new("tmux output was not valid UTF-8"))
    }

    fn run_output_allow_missing_pane(&self, args: Vec<String>) -> AppResult<Option<String>> {
        let args = self.with_socket_args(args);
        let output = Command::new(&self.program).args(&args).output()?;
        if output.status.success() {
            return String::from_utf8(output.stdout)
                .map(Some)
                .map_err(|_| AppError::new("tmux output was not valid UTF-8"));
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.to_ascii_lowercase().contains("can't find pane") {
            return Ok(None);
        }
        Err(AppError::new(format!(
            "tmux command failed: {} {}",
            self.program,
            args.join(" ")
        )))
    }

    fn control_mode_command(&self, target_session: &str) -> String {
        TmuxCommandPlan {
            program: self.program.clone(),
            args: self.with_socket_args(vec![
                String::from("-C"),
                String::from("attach-session"),
                String::from("-t"),
                target_session.to_string(),
            ]),
        }
        .render()
    }

    fn match_explicit_pane_targets(&self, target: &str) -> AppResult<Vec<String>> {
        let output = self.run_output(vec![
            String::from("list-panes"),
            String::from("-a"),
            String::from("-F"),
            String::from(
                "#{pane_id}\t#{session_name}:#{window_index}.#{pane_index}\t#{session_name}:#{window_name}.#{pane_index}",
            ),
        ])?;

        Ok(output
            .lines()
            .filter_map(parse_explicit_pane_target_line)
            .filter(|(_, indexed_target, named_target)| {
                indexed_target == target || named_target == target
            })
            .map(|(pane_id, _, _)| pane_id)
            .collect())
    }

    fn with_socket_args(&self, mut args: Vec<String>) -> Vec<String> {
        if let Some(socket_path) = &self.socket_path {
            args.splice(
                0..0,
                [String::from("-S"), socket_path.display().to_string()],
            );
        } else if let Some(socket_name) = &self.socket_name {
            args.splice(0..0, [String::from("-L"), socket_name.clone()]);
        }
        args
    }
}

enum ControlStreamItem {
    Stdout(String),
    ReadError(String),
}

impl ControlModeSession {
    pub fn recv_timeout(&mut self, timeout: Duration) -> AppResult<ControlModeReceive> {
        match self.stdout_rx.recv_timeout(timeout) {
            Ok(ControlStreamItem::Stdout(line)) => Ok(ControlModeReceive::Line(line)),
            Ok(ControlStreamItem::ReadError(message)) => {
                self.shutdown_child();
                Err(AppError::new(format!(
                    "failed to read tmux control mode output: {message}"
                )))
            }
            Err(mpsc::RecvTimeoutError::Timeout) => self.poll_child_state(),
            Err(mpsc::RecvTimeoutError::Disconnected) => self.poll_child_state(),
        }
    }

    fn poll_child_state(&mut self) -> AppResult<ControlModeReceive> {
        match self.child.try_wait()? {
            Some(status) if !status.success() => {
                let stderr_lines = drain_channel(&self.stderr_rx);
                let stderr_text = if stderr_lines.is_empty() {
                    String::from("no stderr")
                } else {
                    stderr_lines.join("\n")
                };
                Err(AppError::new(format!(
                    "tmux control mode attach failed for session {}: {}",
                    self.target_session, stderr_text
                )))
            }
            Some(_) => Ok(ControlModeReceive::Closed),
            None => Ok(ControlModeReceive::Idle),
        }
    }

    fn shutdown_child(&mut self) {
        if let Ok(None) = self.child.try_wait() {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
    }
}

impl Drop for ControlModeSession {
    fn drop(&mut self) {
        self.shutdown_child();
    }
}

fn parse_pane_line(line: &str) -> Option<TmuxPane> {
    let parts: Vec<&str> = line.split('\t').collect();
    if parts.len() != 15 {
        return None;
    }

    Some(TmuxPane {
        pane_id: parts[0].to_string(),
        pane_tty: parts[1].to_string(),
        pane_pid: parts[2].parse::<u32>().ok(),
        session_id: parts[3].to_string(),
        session_name: parts[4].to_string(),
        window_id: parts[5].to_string(),
        window_index: parts[6].parse::<u16>().ok()?,
        window_name: parts[7].to_string(),
        pane_index: parts[8].parse::<u16>().ok()?,
        current_command: parts[9].to_string(),
        current_path: parts[10].to_string(),
        pane_title: parts[11].to_string(),
        pane_active: parts[12] == "1",
        cursor_x: parse_cursor(parts[13]),
        cursor_y: parse_cursor(parts[14]),
    })
}

fn pane_from_fields(parts: &[String]) -> Option<TmuxPane> {
    if parts.len() != 15 {
        return None;
    }
    Some(TmuxPane {
        pane_id: parts[0].clone(),
        pane_tty: parts[1].clone(),
        pane_pid: parts[2].parse::<u32>().ok(),
        session_id: parts[3].clone(),
        session_name: parts[4].clone(),
        window_id: parts[5].clone(),
        window_index: parts[6].parse::<u16>().ok()?,
        window_name: parts[7].clone(),
        pane_index: parts[8].parse::<u16>().ok()?,
        current_command: parts[9].clone(),
        current_path: parts[10].clone(),
        pane_title: parts[11].clone(),
        pane_active: match parts[12].as_str() {
            "0" => false,
            "1" => true,
            _ => return None,
        },
        cursor_x: parse_cursor(&parts[13]),
        cursor_y: parse_cursor(&parts[14]),
    })
}

fn pane_list_format() -> String {
    [
        "pane_id",
        "pane_tty",
        "pane_pid",
        "session_id",
        "session_name",
        "window_id",
        "window_index",
        "window_name",
        "pane_index",
        "pane_current_command",
        "pane_current_path",
        "pane_title",
        "pane_active",
        "cursor_x",
        "cursor_y",
    ]
    .into_iter()
    .map(|field| format!("#{{n:{field}}}:#{{{field}}}"))
    .collect()
}

fn parse_server_identity(output: &str) -> AppResult<TmuxServerIdentity> {
    let bytes = output.as_bytes();
    let mut offset = 0;
    let parts = parse_length_prefixed_fields(bytes, &mut offset, 3)?;
    if bytes.get(offset) != Some(&b'\n') || offset + 1 != bytes.len() {
        return Err(AppError::new(
            "tmux server identity output had a malformed record terminator",
        ));
    }
    if parts.iter().any(|part| part.is_empty()) {
        return Err(AppError::new(
            "tmux server identity was missing socket_path, pid, or start_time",
        ));
    }
    let pid = parts[1]
        .parse::<u32>()
        .map_err(|_| AppError::new("tmux server pid was malformed"))?;
    let start_time = parts[2]
        .parse::<i64>()
        .map_err(|_| AppError::new("tmux server start_time was malformed"))?;
    if pid == 0 || start_time <= 0 {
        return Err(AppError::new(
            "tmux server pid and start_time must be positive",
        ));
    }
    Ok(TmuxServerIdentity {
        socket_path: parts[0].to_string(),
        pid,
        start_time,
    })
}

fn parse_inventory_panes(output: &str) -> AppResult<Vec<TmuxPane>> {
    if output.is_empty() {
        return Ok(Vec::new());
    }
    let bytes = output.as_bytes();
    let mut offset = 0;
    let mut panes = Vec::new();
    while offset < bytes.len() {
        let fields = parse_length_prefixed_fields(bytes, &mut offset, 15)?;
        if bytes.get(offset) != Some(&b'\n') {
            return Err(AppError::new(
                "raw tmux pane inventory record lacked its terminator",
            ));
        }
        offset += 1;
        panes
            .push(pane_from_fields(&fields).ok_or_else(|| {
                AppError::new("malformed typed field in raw tmux pane inventory")
            })?);
    }
    Ok(panes)
}

fn parse_length_prefixed_fields(
    bytes: &[u8],
    offset: &mut usize,
    count: usize,
) -> AppResult<Vec<String>> {
    let mut fields = Vec::with_capacity(count);
    for _ in 0..count {
        let length_start = *offset;
        while *offset < bytes.len() && bytes[*offset].is_ascii_digit() {
            *offset += 1;
        }
        if *offset == length_start || bytes.get(*offset) != Some(&b':') {
            return Err(AppError::new(
                "malformed length prefix in tmux structured output",
            ));
        }
        // tmux `#{n:field}` is a strlen() byte count, so multi-byte UTF-8
        // (emoji, accents) must advance by bytes, then validate UTF-8.
        let length = std::str::from_utf8(&bytes[length_start..*offset])
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .ok_or_else(|| AppError::new("invalid field length in tmux structured output"))?;
        *offset += 1;
        let end = (*offset)
            .checked_add(length)
            .filter(|end| *end <= bytes.len())
            .ok_or_else(|| AppError::new("truncated field in tmux structured output"))?;
        let value = std::str::from_utf8(&bytes[*offset..end])
            .map_err(|_| AppError::new("tmux structured output field was not valid UTF-8"))?;
        fields.push(value.to_string());
        *offset = end;
    }
    Ok(fields)
}

#[cfg(any(test, rust_analyzer))]
fn encode_pane_record_for_test(fields: &[&str]) -> String {
    let mut output = fields
        .iter()
        // Match tmux `#{n:...}` strlen() byte counts.
        .map(|field| format!("{}:{field}", field.len()))
        .collect::<String>();
    output.push('\n');
    output
}

fn stable_inventory(
    before: TmuxServerIdentity,
    panes: Vec<TmuxPane>,
    after: TmuxServerIdentity,
) -> AppResult<TmuxInventory> {
    if before != after {
        return Err(AppError::new(
            "tmux server identity changed while collecting pane inventory",
        ));
    }
    Ok(TmuxInventory {
        server: before,
        panes,
    })
}

fn list_panes_args(target: Option<&str>) -> Vec<String> {
    let mut args = vec![String::from("list-panes")];
    if let Some(target) = target {
        args.push(String::from("-t"));
        args.push(target.to_string());
    } else {
        args.push(String::from("-a"));
    }
    args.push(String::from("-F"));
    args.push(String::from(
        "#{pane_id}\t#{pane_tty}\t#{pane_pid}\t#{session_id}\t#{session_name}\t#{window_id}\t#{window_index}\t#{window_name}\t#{pane_index}\t#{pane_current_command}\t#{pane_current_path}\t#{pane_title}\t#{pane_active}\t#{cursor_x}\t#{cursor_y}",
    ));
    args
}

fn parse_pane_listing_strict(output: &str) -> AppResult<Vec<TmuxPane>> {
    output
        .lines()
        .filter(|line| !line.is_empty())
        .map(parse_pane_line_strict)
        .collect()
}

fn parse_pane_line_strict(line: &str) -> AppResult<TmuxPane> {
    let parts = line.split('\t').collect::<Vec<_>>();
    let valid_optional_u32 = |value: &str| value.is_empty() || value.parse::<u32>().is_ok();
    let valid_optional_u16 = |value: &str| value.is_empty() || value.parse::<u16>().is_ok();
    let valid = parts.len() == 15
        && valid_optional_u32(parts[2])
        && matches!(parts[12], "0" | "1")
        && valid_optional_u16(parts[13])
        && valid_optional_u16(parts[14]);
    if !valid {
        return Err(AppError::new(format!(
            "malformed tmux pane listing row: {line:?}"
        )));
    }
    parse_pane_line(line)
        .ok_or_else(|| AppError::new(format!("malformed tmux pane listing row: {line:?}")))
}

fn parse_session_attached_count(output: &str) -> AppResult<u32> {
    output.trim().parse::<u32>().map_err(|_| {
        AppError::new(format!(
            "tmux returned invalid session attachment count: {:?}",
            output.trim()
        ))
    })
}

fn session_attached_count_args(target_session: &str) -> Vec<String> {
    vec![
        String::from("display-message"),
        String::from("-p"),
        String::from("-t"),
        target_session.to_string(),
        String::from("#{session_attached}"),
    ]
}

fn parse_window_line(line: &str) -> Option<TmuxWindow> {
    let parts = line.split('\t').collect::<Vec<_>>();
    if parts.len() != 4 {
        return None;
    }

    Some(TmuxWindow {
        session_name: parts[0].to_string(),
        window_id: parts[1].to_string(),
        window_index: parts[2].parse::<u16>().ok()?,
        window_name: parts[3].to_string(),
    })
}

fn parse_created_pane_window(output: &str) -> Option<(String, String)> {
    let line = output.lines().find(|line| !line.trim().is_empty())?;
    let parts = line.split('\t').collect::<Vec<_>>();
    if parts.len() != 2 || parts[0].is_empty() || parts[1].is_empty() {
        return None;
    }

    Some((parts[0].to_string(), parts[1].to_string()))
}

fn parse_cursor(value: &str) -> Option<u16> {
    if value.is_empty() {
        None
    } else {
        value.parse::<u16>().ok()
    }
}

fn parse_explicit_pane_target_line(line: &str) -> Option<(String, String, String)> {
    let parts: Vec<&str> = line.split('\t').collect();
    if parts.len() != 3 {
        return None;
    }

    Some((
        parts[0].to_string(),
        parts[1].to_string(),
        parts[2].to_string(),
    ))
}

fn select_explicit_pane_target(
    target: &str,
    mut pane_ids: Vec<String>,
) -> AppResult<Option<String>> {
    match pane_ids.len() {
        0 => Ok(None),
        1 => Ok(pane_ids.pop()),
        _ => Err(AppError::new(format!(
            "pane target is ambiguous: {target}; use a unique %ID or session:window.pane target"
        ))),
    }
}

fn shell_escape(value: &str) -> String {
    if value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '_' | '-' | '.' | '%' | ':'))
    {
        value.to_string()
    } else {
        format!("'{}'", value.replace('\'', "'\"'\"'"))
    }
}

fn drain_channel(rx: &mpsc::Receiver<String>) -> Vec<String> {
    let mut lines = Vec::new();
    while let Ok(line) = rx.try_recv() {
        lines.push(line);
    }
    lines
}

#[cfg(any(test, rust_analyzer))]
mod tests {
    use std::path::PathBuf;

    use super::{
        StartSessionRequest, StartWindowRequest, TmuxClient, TmuxCommandPlan,
        encode_pane_record_for_test, list_panes_args, parse_created_pane_window,
        parse_explicit_pane_target_line, parse_inventory_panes, parse_pane_line,
        parse_pane_listing_strict, parse_server_identity, parse_session_attached_count,
        parse_window_line, select_explicit_pane_target, session_attached_count_args,
        stable_inventory,
    };

    #[test]
    fn renders_start_plan() {
        let client = TmuxClient::default();
        let plan = client.plan_start_session(&StartSessionRequest {
            session_name: String::from("demo"),
            window_name: String::from("claude"),
            cwd: PathBuf::from("/tmp/demo"),
            command: String::from("claude --print"),
        });

        let rendered = plan.render();
        assert!(rendered.contains("new-session"));
        assert!(rendered.contains("-s demo"));
        assert!(rendered.contains("'claude --print'"));
    }

    #[test]
    fn renders_start_plan_with_socket() {
        let client = TmuxClient::with_socket("botctl-dashboard");
        let plan = client.plan_start_session(&StartSessionRequest {
            session_name: String::from("demo"),
            window_name: String::from("claude"),
            cwd: PathBuf::from("/tmp/demo"),
            command: String::from("claude --print"),
        });

        let rendered = plan.render();
        assert!(rendered.contains("-L botctl-dashboard"));
        assert!(rendered.contains("new-session"));
    }

    #[test]
    fn renders_start_window_plan() {
        let client = TmuxClient::default();
        let plan = client.plan_start_window(&StartWindowRequest {
            session_name: String::from("botctl"),
            window_name: String::from("claude"),
            cwd: PathBuf::from("/tmp/demo"),
            command: String::from("claude"),
        });

        let rendered = plan.render();
        assert!(rendered.contains("new-window"));
        assert!(rendered.contains("-t botctl"));
        assert!(rendered.contains("'#{pane_id}\t#{window_id}'"));
    }

    #[test]
    fn renders_start_window_as_session_plan() {
        let client = TmuxClient::default();
        let plan = client.plan_start_window_as_session(&StartWindowRequest {
            session_name: String::from("botctl"),
            window_name: String::from("claude"),
            cwd: PathBuf::from("/tmp/demo"),
            command: String::from("claude"),
        });

        let rendered = plan.render();
        assert!(rendered.contains("new-session"));
        assert!(rendered.contains("-s botctl"));
        assert!(rendered.contains("'#{pane_id}\t#{window_id}'"));
    }

    #[test]
    fn set_global_option_uses_socket() {
        let client = TmuxClient::with_socket("botctl-dashboard");
        let rendered = TmuxCommandPlan {
            program: String::from("tmux"),
            args: client.with_socket_args(vec![
                String::from("set-option"),
                String::from("-g"),
                String::from("status"),
                String::from("off"),
            ]),
        }
        .render();

        assert!(rendered.contains("-L botctl-dashboard"));
        assert!(rendered.contains("set-option -g status off"));
    }

    #[test]
    fn set_session_option_targets_session_without_global_scope() {
        let client = TmuxClient::with_socket("botctl-dashboard");
        let rendered = TmuxCommandPlan {
            program: String::from("tmux"),
            args: client.with_socket_args(vec![
                String::from("set-option"),
                String::from("-t"),
                String::from("botctl-runtime"),
                String::from("status"),
                String::from("off"),
            ]),
        }
        .render();

        assert!(rendered.contains("-L botctl-dashboard"));
        assert!(rendered.contains("set-option -t botctl-runtime status off"));
        assert!(!rendered.contains("set-option -g"));
    }

    #[test]
    fn start_plan_uses_explicit_socket_path() {
        let client = TmuxClient::with_socket_path("/tmp/tmux-1000/default");
        let plan = client.plan_start_session(&StartSessionRequest {
            session_name: String::from("demo"),
            window_name: String::from("claude"),
            cwd: PathBuf::from("/tmp/demo"),
            command: String::from("claude --print"),
        });

        let rendered = plan.render();
        assert!(rendered.contains("-S /tmp/tmux-1000/default"));
        assert!(rendered.contains("new-session"));
    }

    #[test]
    fn parses_pane_listing() {
        let pane = parse_pane_line(
            "%1\t/dev/pts/1\t123\t$1\tdemo\t@2\t3\tclaude\t1\tclaude\t/tmp/demo\tClaude\t1\t12\t4",
        )
        .expect("pane should parse");
        assert_eq!(pane.pane_id, "%1");
        assert_eq!(pane.window_index, 3);
        assert_eq!(pane.pane_index, 1);
        assert_eq!(pane.pane_title, "Claude");
        assert!(pane.pane_active);
        assert_eq!(pane.cursor_x, Some(12));
        assert_eq!(pane.cursor_y, Some(4));
    }

    #[test]
    fn strict_pane_listing_rejects_partial_results() {
        let output = concat!(
            "%1\t/dev/pts/1\t123\t$1\tdemo\t@2\t3\tclaude\t1\tclaude\t/tmp/demo\tClaude\t1\t12\t4\n",
            "malformed\n"
        );
        let error = parse_pane_listing_strict(output).expect_err("listing should fail");

        assert!(
            error
                .to_string()
                .contains("malformed tmux pane listing row")
        );

        let invalid_pid = "%1\t/dev/pts/1\tnot-a-pid\t$1\tdemo\t@2\t3\tclaude\t1\tclaude\t/tmp/demo\tClaude\t1\t12\t4\n";
        assert!(parse_pane_listing_strict(invalid_pid).is_err());
        let invalid_active = "%1\t/dev/pts/1\t123\t$1\tdemo\t@2\t3\tclaude\t1\tclaude\t/tmp/demo\tClaude\tyes\t12\t4\n";
        assert!(parse_pane_listing_strict(invalid_active).is_err());
    }

    #[test]
    fn parses_stable_server_identity_fields() {
        let encoded = encode_pane_record_for_test(&["/tmp/tmux-1000/default", "123", "1710000000"]);
        let identity = parse_server_identity(&encoded).expect("identity should parse");
        assert_eq!(identity.socket_path, "/tmp/tmux-1000/default");
        assert_eq!(identity.pid, 123);
        assert_eq!(identity.start_time, 1_710_000_000);
        assert!(parse_server_identity("").is_err());
        assert!(
            parse_server_identity(&encode_pane_record_for_test(&["/tmp/socket", "", "1"])).is_err()
        );
        assert!(
            parse_server_identity(&encode_pane_record_for_test(&["/tmp/socket", "nope", "1"]))
                .is_err()
        );
        assert!(
            parse_server_identity(&encode_pane_record_for_test(&["/tmp/socket", "1", "0"]))
                .is_err()
        );
        let unusual = parse_server_identity(&encode_pane_record_for_test(&[
            "/tmp/socket\tline\nnext",
            "12",
            "34",
        ]))
        .unwrap();
        assert_eq!(unusual.socket_path, "/tmp/socket\tline\nnext");
    }

    #[test]
    fn raw_inventory_rejects_any_malformed_pane() {
        assert!(parse_inventory_panes("malformed").is_err());
        assert!(parse_inventory_panes("").unwrap().is_empty());
    }

    #[test]
    fn pane_inventory_round_trips_delimiters_and_newlines() {
        let encoded = encode_pane_record_for_test(&[
            "%1",
            "/dev/pts/1",
            "123",
            "$1",
            "session\tname",
            "@2",
            "3",
            "window\nname",
            "1",
            "bash",
            "/tmp/tab\tline\nnext",
            "title\twith\nlines",
            "1",
            "12",
            "4",
        ]);
        let panes = parse_inventory_panes(&encoded).unwrap();
        assert_eq!(panes.len(), 1);
        assert_eq!(panes[0].session_name, "session\tname");
        assert_eq!(panes[0].window_name, "window\nname");
        assert_eq!(panes[0].current_path, "/tmp/tab\tline\nnext");
        assert_eq!(panes[0].pane_title, "title\twith\nlines");
    }

    #[test]
    fn pane_inventory_advances_by_utf8_bytes_not_characters() {
        // "⚙" is 3 UTF-8 bytes but one scalar; tmux `#{n:...}` is strlen(),
        // so prefixes count bytes ("⚙ bloodraven" encodes as 14, not 12).
        let encoded = encode_pane_record_for_test(&[
            "%1",
            "/dev/pts/1",
            "123",
            "$1",
            "demo",
            "@2",
            "3",
            "⚙ bloodraven",
            "1",
            "claude",
            "/tmp/demo",
            "title 🚀",
            "1",
            "12",
            "4",
        ]);
        assert!(encoded.contains("14:⚙ bloodraven"));
        assert!(encoded.contains("10:title 🚀"));
        let panes = parse_inventory_panes(&encoded).unwrap();
        assert_eq!(panes.len(), 1);
        assert_eq!(panes[0].window_name, "⚙ bloodraven");
        assert_eq!(panes[0].pane_title, "title 🚀");
        assert_eq!(panes[0].pane_id, "%1");
    }

    #[test]
    fn pane_inventory_rejects_length_prefix_splitting_a_utf8_scalar() {
        // A prefix that ends mid-scalar (2 of ⚙'s 3 bytes) must fail cleanly
        // rather than emit garbage fields.
        let mut encoded = String::from("2:%1");
        for field in [
            "/dev/pts/1",
            "123",
            "$1",
            "demo",
            "@2",
            "3",
        ] {
            encoded.push_str(&format!("{}:{field}", field.len()));
        }
        encoded.push_str("2:⚙ bloodraven");
        encoded.push('\n');
        assert!(parse_inventory_panes(&encoded).is_err());
    }

    #[test]
    fn pane_inventory_preserves_other_rows_around_encoded_delimiters() {
        let first = encode_pane_record_for_test(&[
            "%1",
            "/dev/pts/1",
            "1",
            "$1",
            "one",
            "@1",
            "0",
            "one",
            "0",
            "bash",
            "/tmp/a\tb",
            "a\nb",
            "1",
            "",
            "",
        ]);
        let second = encode_pane_record_for_test(&[
            "%2",
            "/dev/pts/2",
            "2",
            "$2",
            "two",
            "@2",
            "1",
            "two",
            "0",
            "zsh",
            "/tmp/two",
            "two",
            "0",
            "",
            "",
        ]);
        let panes = parse_inventory_panes(&(first + &second)).unwrap();
        assert_eq!(
            panes
                .iter()
                .map(|pane| pane.pane_id.as_str())
                .collect::<Vec<_>>(),
            vec!["%1", "%2"]
        );
    }

    #[test]
    fn raw_inventory_rejects_changed_server_identity() {
        let before = parse_server_identity(&encode_pane_record_for_test(&[
            "/tmp/tmux/default",
            "123",
            "456",
        ]))
        .unwrap();
        let mut after = before.clone();
        after.pid += 1;
        assert!(stable_inventory(before, Vec::new(), after).is_err());
    }

    #[test]
    fn paste_plan_targets_only_explicit_pane_without_enter() {
        let plan = TmuxClient::with_socket_path("/tmp/tmux/default")
            .plan_paste_text("%7", "literal command");
        assert!(
            plan.set_buffer
                .args
                .iter()
                .any(|arg| arg == "literal command")
        );
        assert_eq!(
            plan.paste_buffer.args.last().map(String::as_str),
            Some("%7")
        );
        let rendered = format!(
            "{} {}",
            plan.set_buffer.render(),
            plan.paste_buffer.render()
        );
        assert!(rendered.contains("set-buffer"));
        assert!(rendered.contains("paste-buffer -d -p"));
        assert!(!rendered.contains("send-keys"));
        assert!(!rendered.contains("Enter"));
        assert!(!rendered.contains(['\n', '\r']));
    }

    #[test]
    fn exact_pane_listing_targets_only_requested_pane() {
        let args = list_panes_args(Some("%19"));

        assert_eq!(args[0..3], ["list-panes", "-t", "%19"]);
        assert!(!args.iter().any(|arg| arg == "-a"));
    }

    #[test]
    fn parses_session_attachment_count() {
        assert_eq!(parse_session_attached_count("2\n").unwrap(), 2);
        assert!(parse_session_attached_count("attached").is_err());
    }

    #[test]
    fn session_attachment_count_targets_private_session() {
        assert_eq!(
            session_attached_count_args("botctl-dashboard"),
            [
                "display-message",
                "-p",
                "-t",
                "botctl-dashboard",
                "#{session_attached}"
            ]
        );
    }

    #[test]
    fn control_mode_command_preserves_explicit_socket() {
        let command =
            TmuxClient::with_socket_path("/tmp/outer.sock").control_mode_command("work session");

        assert!(command.contains("-S /tmp/outer.sock -C attach-session -t 'work session'"));
    }

    #[test]
    fn control_mode_command_accepts_stable_session_id() {
        let command = TmuxClient::default().control_mode_command("$7");

        assert!(command.contains("-C attach-session -t '$7'"));
    }

    #[test]
    fn parses_window_listing() {
        let window = parse_window_line("work\t@4\t2\t⚙️ bloodraven").expect("window should parse");
        assert_eq!(window.session_name, "work");
        assert_eq!(window.window_id, "@4");
        assert_eq!(window.window_index, 2);
        assert_eq!(window.window_name, "⚙️ bloodraven");
    }

    #[test]
    fn parses_created_pane_window_ids() {
        let parsed = parse_created_pane_window("%21\t@7\n").expect("ids should parse");

        assert_eq!(parsed.0, "%21");
        assert_eq!(parsed.1, "@7");
    }

    #[test]
    fn explicit_pane_target_accepts_unique_tmux_target() {
        let pane_id = select_explicit_pane_target("0:2.3", vec![String::from("%21")])
            .expect("target should resolve")
            .expect("pane should exist");

        assert_eq!(pane_id, "%21");
    }

    #[test]
    fn explicit_pane_target_rejects_ambiguous_tmux_target() {
        let error = select_explicit_pane_target(
            "demo:claude.1",
            vec![String::from("%21"), String::from("%22")],
        )
        .expect_err("pane target should fail when it resolves to multiple panes");

        assert!(error.to_string().contains("pane target is ambiguous"));
    }

    #[test]
    fn parses_explicit_pane_target_mapping_line() {
        let parsed = parse_explicit_pane_target_line("%21\t0:2.3\t0:botctl.3")
            .expect("target mapping line should parse");

        assert_eq!(parsed.0, "%21");
        assert_eq!(parsed.1, "0:2.3");
        assert_eq!(parsed.2, "0:botctl.3");
    }
}
