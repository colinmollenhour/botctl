use std::fs;
use std::path::{Path, PathBuf};

use crate::app::{AppError, AppResult};
use crate::tmux::TmuxPane;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BabysitRecord {
    pub enabled: bool,
    pub pane_id: String,
    pub pane_tty: String,
    pub pane_pid: Option<u32>,
    pub session_id: String,
    pub session_name: String,
    pub window_id: String,
    pub window_name: String,
    pub current_command: String,
    pub current_path: String,
}

impl BabysitRecord {
    pub fn from_pane(pane: &TmuxPane) -> Self {
        Self {
            enabled: true,
            pane_id: pane.pane_id.clone(),
            pane_tty: pane.pane_tty.clone(),
            pane_pid: pane.pane_pid,
            session_id: pane.session_id.clone(),
            session_name: pane.session_name.clone(),
            window_id: pane.window_id.clone(),
            window_name: pane.window_name.clone(),
            current_command: pane.current_command.clone(),
            current_path: pane.current_path.clone(),
        }
    }

    pub fn matches_pane(&self, pane: &TmuxPane) -> bool {
        self.pane_id == pane.pane_id
            && self.pane_tty == pane.pane_tty
            && self.pane_pid == pane.pane_pid
            && self.session_id == pane.session_id
            && self.window_id == pane.window_id
            && self.current_command == pane.current_command
    }
}

pub fn babysit_record_path(state_dir: &Path, pane_id: &str) -> PathBuf {
    state_dir
        .join("permission-babysit")
        .join(sanitize(pane_id))
        .join("record.txt")
}

pub fn write_babysit_record(state_dir: &Path, record: &BabysitRecord) -> AppResult<PathBuf> {
    let path = babysit_record_path(state_dir, &record.pane_id);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&path, serialize_record(record))?;
    Ok(path)
}

pub fn read_babysit_record(state_dir: &Path, pane_id: &str) -> AppResult<Option<BabysitRecord>> {
    let path = babysit_record_path(state_dir, pane_id);
    match fs::read_to_string(&path) {
        Ok(contents) => Ok(Some(parse_record(&contents)?)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(AppError::from(error)),
    }
}

pub fn disable_babysit_record(state_dir: &Path, pane_id: &str) -> AppResult<bool> {
    let Some(mut record) = read_babysit_record(state_dir, pane_id)? else {
        return Ok(false);
    };
    record.enabled = false;
    write_babysit_record(state_dir, &record)?;
    Ok(true)
}

fn serialize_record(record: &BabysitRecord) -> String {
    format!(
        "enabled={}\npane_id={}\npane_tty={}\npane_pid={}\nsession_id={}\nsession_name={}\nwindow_id={}\nwindow_name={}\ncurrent_command={}\ncurrent_path={}\n",
        record.enabled,
        record.pane_id,
        record.pane_tty,
        record
            .pane_pid
            .map(|pid| pid.to_string())
            .unwrap_or_default(),
        record.session_id,
        record.session_name,
        record.window_id,
        record.window_name,
        record.current_command,
        record.current_path,
    )
}

fn parse_record(contents: &str) -> AppResult<BabysitRecord> {
    let mut enabled = false;
    let mut pane_id = None;
    let mut pane_tty = None;
    let mut pane_pid = None;
    let mut session_id = None;
    let mut session_name = None;
    let mut window_id = None;
    let mut window_name = None;
    let mut current_command = None;
    let mut current_path = None;
    for line in contents.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key {
            "enabled" => enabled = value == "true",
            "pane_id" => pane_id = Some(value.to_string()),
            "pane_tty" => pane_tty = Some(value.to_string()),
            "pane_pid" => pane_pid = value.parse::<u32>().ok(),
            "session_id" => session_id = Some(value.to_string()),
            "session_name" => session_name = Some(value.to_string()),
            "window_id" => window_id = Some(value.to_string()),
            "window_name" => window_name = Some(value.to_string()),
            "current_command" => current_command = Some(value.to_string()),
            "current_path" => current_path = Some(value.to_string()),
            _ => {}
        }
    }
    Ok(BabysitRecord {
        enabled,
        pane_id: pane_id.ok_or_else(|| AppError::new("missing pane_id"))?,
        pane_tty: pane_tty.ok_or_else(|| AppError::new("missing pane_tty"))?,
        pane_pid,
        session_id: session_id.ok_or_else(|| AppError::new("missing session_id"))?,
        session_name: session_name.ok_or_else(|| AppError::new("missing session_name"))?,
        window_id: window_id.ok_or_else(|| AppError::new("missing window_id"))?,
        window_name: window_name.ok_or_else(|| AppError::new("missing window_name"))?,
        current_command: current_command.ok_or_else(|| AppError::new("missing current_command"))?,
        current_path: current_path.ok_or_else(|| AppError::new("missing current_path"))?,
    })
}

fn sanitize(input: &str) -> String {
    input
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(any(test, rust_analyzer))]
mod tests {
    use super::*;
    use crate::tmux::TmuxPane;

    fn sample_pane() -> TmuxPane {
        TmuxPane {
            pane_id: String::from("%1"),
            pane_tty: String::from("/dev/pts/1"),
            pane_pid: Some(123),
            session_id: String::from("$1"),
            session_name: String::from("demo"),
            window_id: String::from("@2"),
            window_name: String::from("claude"),
            current_command: String::from("claude"),
            current_path: String::from("/tmp/demo"),
            pane_active: true,
            cursor_x: Some(1),
            cursor_y: Some(2),
        }
    }

    #[test]
    fn record_round_trips_and_matches() {
        let pane = sample_pane();
        let record = BabysitRecord::from_pane(&pane);
        assert!(record.matches_pane(&pane));
    }
}
