use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Command, Stdio};
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
    pub session_name: String,
    pub window_name: String,
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
            "#{pane_id}\t#{session_name}\t#{window_name}\t#{pane_current_command}\t#{pane_current_path}\t#{pane_active}\t#{cursor_x}\t#{cursor_y}",
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

    pub fn control_mode_lines(
        &self,
        session_name: &str,
        max_events: usize,
        idle_timeout_ms: u64,
    ) -> AppResult<Vec<String>> {
        let mut child = Command::new(&self.program)
            .arg("-CC")
            .arg("attach-session")
            .arg("-t")
            .arg(session_name)
            .stdin(Stdio::null())
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

        let timeout = Duration::from_millis(idle_timeout_ms);
        let mut lines = Vec::new();

        while lines.len() < max_events {
            match stdout_rx.recv_timeout(timeout) {
                Ok(ControlStreamItem::Stdout(line)) => lines.push(line),
                Ok(ControlStreamItem::ReadError(message)) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(AppError::new(format!(
                        "failed to read tmux control mode output: {message}"
                    )));
                }
                Err(mpsc::RecvTimeoutError::Timeout) => break,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }

        let status_before_shutdown = child.try_wait()?;
        if status_before_shutdown.is_none() {
            let _ = child.kill();
        }
        let _ = child.wait()?;

        let stderr_lines = drain_channel(stderr_rx);
        if lines.is_empty() && matches!(status_before_shutdown, Some(status) if !status.success()) {
            let stderr_text = if stderr_lines.is_empty() {
                String::from("no stderr")
            } else {
                stderr_lines.join("\n")
            };
            return Err(AppError::new(format!(
                "tmux control mode attach failed for session {}: {}",
                session_name, stderr_text
            )));
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

    pub fn send_keys(&self, pane_id: &str, keys: &[&str]) -> AppResult<()> {
        let mut args = vec![
            String::from("send-keys"),
            String::from("-t"),
            pane_id.to_string(),
        ];
        args.extend(keys.iter().map(|key| (*key).to_string()));
        self.run_status(args)
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
}

enum ControlStreamItem {
    Stdout(String),
    ReadError(String),
}

fn parse_pane_line(line: &str) -> Option<TmuxPane> {
    let parts: Vec<&str> = line.split('\t').collect();
    if parts.len() != 8 {
        return None;
    }

    Some(TmuxPane {
        pane_id: parts[0].to_string(),
        session_name: parts[1].to_string(),
        window_name: parts[2].to_string(),
        current_command: parts[3].to_string(),
        current_path: parts[4].to_string(),
        pane_active: parts[5] == "1",
        cursor_x: parse_cursor(parts[6]),
        cursor_y: parse_cursor(parts[7]),
    })
}

fn parse_cursor(value: &str) -> Option<u16> {
    if value.is_empty() {
        None
    } else {
        value.parse::<u16>().ok()
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

fn drain_channel(rx: mpsc::Receiver<String>) -> Vec<String> {
    let mut lines = Vec::new();
    while let Ok(line) = rx.try_recv() {
        lines.push(line);
    }
    lines
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{StartSessionRequest, TmuxClient, parse_pane_line};

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
        let pane = parse_pane_line("%1\tdemo\tclaude\tclaude\t/tmp/demo\t1\t12\t4")
            .expect("pane should parse");
        assert_eq!(pane.pane_id, "%1");
        assert!(pane.pane_active);
        assert_eq!(pane.cursor_x, Some(12));
        assert_eq!(pane.cursor_y, Some(4));
    }
}
