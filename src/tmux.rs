use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use crate::app::{AppError, AppResult};

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
    pub pane_active: bool,
    pub cursor_x: Option<u16>,
    pub cursor_y: Option<u16>,
}

#[derive(Debug, Clone)]
pub struct TmuxCommandPlan {
    pub program: String,
    pub args: Vec<String>,
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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlModeReceive {
    Line(String),
    Idle,
    Closed,
}

pub struct ControlModeSession {
    session_name: String,
    child: Child,
    stdout_rx: mpsc::Receiver<ControlStreamItem>,
    stderr_rx: mpsc::Receiver<String>,
}

impl Default for TmuxClient {
    fn default() -> Self {
        Self {
            program: String::from("tmux"),
        }
    }
}

impl TmuxClient {
    pub fn plan_start_session(&self, request: &StartSessionRequest) -> TmuxCommandPlan {
        TmuxCommandPlan {
            program: self.program.clone(),
            args: vec![
                String::from("new-session"),
                String::from("-d"),
                String::from("-s"),
                request.session_name.clone(),
                String::from("-n"),
                request.window_name.clone(),
                String::from("-c"),
                request.cwd.display().to_string(),
                request.command.clone(),
            ],
        }
    }

    pub fn start_session(&self, request: &StartSessionRequest) -> AppResult<StartedSession> {
        self.run_status(self.plan_start_session(request).args)?;
        Ok(StartedSession {
            session_name: request.session_name.clone(),
            window_name: request.window_name.clone(),
            cwd: request.cwd.clone(),
        })
    }

    pub fn list_panes(&self) -> AppResult<Vec<TmuxPane>> {
        self.list_panes_for_target(None)
    }

    pub fn list_panes_for_target(&self, target: Option<&str>) -> AppResult<Vec<TmuxPane>> {
        let mut args = vec![String::from("list-panes")];
        if let Some(target) = target {
            args.push(String::from("-t"));
            args.push(target.to_string());
        } else {
            args.push(String::from("-a"));
        }
        args.push(String::from("-F"));
        args.push(String::from(
            "#{pane_id}\t#{pane_tty}\t#{pane_pid}\t#{session_id}\t#{session_name}\t#{window_id}\t#{window_index}\t#{window_name}\t#{pane_index}\t#{pane_current_command}\t#{pane_current_path}\t#{pane_active}\t#{cursor_x}\t#{cursor_y}",
        ));

        let output = self.run_output(args)?;
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

    pub fn control_mode_session(&self, session_name: &str) -> AppResult<ControlModeSession> {
        let control_command = format!(
            "{} -C attach-session -t {}",
            shell_escape(&self.program),
            shell_escape(session_name)
        );

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

        let (stdout_tx, stdout_rx) = mpsc::channel();
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

        let (stderr_tx, stderr_rx) = mpsc::channel();
        std::thread::spawn(move || {
            let reader = BufReader::new(stderr);
            for line in reader.lines() {
                match line {
                    Ok(line) => {
                        if stderr_tx.send(line).is_err() {
                            return;
                        }
                    }
                    Err(_) => return,
                }
            }
        });

        Ok(ControlModeSession {
            session_name: session_name.to_string(),
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

    pub fn send_keys<S: AsRef<str>>(&self, pane_id: &str, keys: &[S]) -> AppResult<()> {
        let mut args = vec![
            String::from("send-keys"),
            String::from("-t"),
            pane_id.to_string(),
        ];
        args.extend(keys.iter().map(|key| key.as_ref().to_string()));
        self.run_status(args)
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

    pub fn attach_session(&self, target_session: &str) -> AppResult<()> {
        self.run_status(vec![
            String::from("attach-session"),
            String::from("-t"),
            target_session.to_string(),
        ])
    }

    fn run_status(&self, args: Vec<String>) -> AppResult<()> {
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

    fn run_output(&self, args: Vec<String>) -> AppResult<String> {
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
                    self.session_name, stderr_text
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
    if parts.len() != 14 {
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
        pane_active: parts[11] == "1",
        cursor_x: parse_cursor(parts[12]),
        cursor_y: parse_cursor(parts[13]),
    })
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
        StartSessionRequest, TmuxClient, parse_explicit_pane_target_line, parse_pane_line,
        select_explicit_pane_target,
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
    fn parses_pane_listing() {
        let pane = parse_pane_line(
            "%1\t/dev/pts/1\t123\t$1\tdemo\t@2\t3\tclaude\t1\tclaude\t/tmp/demo\t1\t12\t4",
        )
        .expect("pane should parse");
        assert_eq!(pane.pane_id, "%1");
        assert_eq!(pane.window_index, 3);
        assert_eq!(pane.pane_index, 1);
        assert!(pane.pane_active);
        assert_eq!(pane.cursor_x, Some(12));
        assert_eq!(pane.cursor_y, Some(4));
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
