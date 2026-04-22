use std::path::{Path, PathBuf};

use crate::app::AppResult;
use crate::storage::{
    disable_babysit_record as storage_disable_babysit_record, list_babysit_record_pane_ids,
    load_babysit_record, state_db_path, store_babysit_record,
};
use crate::tmux::TmuxPane;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct YoloRecord {
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

impl YoloRecord {
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

pub fn write_yolo_record(state_dir: &Path, record: &YoloRecord) -> AppResult<PathBuf> {
    store_babysit_record(state_dir, record)?;
    Ok(state_db_path(state_dir))
}

pub fn read_yolo_record(state_dir: &Path, pane_id: &str) -> AppResult<Option<YoloRecord>> {
    load_babysit_record(state_dir, pane_id)
}

pub fn disable_yolo_record(state_dir: &Path, pane_id: &str) -> AppResult<bool> {
    storage_disable_babysit_record(state_dir, pane_id)
}

pub fn list_yolo_pane_ids(state_dir: &Path) -> AppResult<Vec<String>> {
    list_babysit_record_pane_ids(state_dir)
}

#[cfg(any(test, rust_analyzer))]
mod tests {
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;
    use crate::storage::state_db_path;
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
        let record = YoloRecord::from_pane(&pane);
        assert!(record.matches_pane(&pane));
    }

    #[test]
    fn sqlite_backed_babysit_record_round_trips_by_pane_id() {
        let state_dir = unique_temp_dir("permission-babysit-round-trip");
        let _ = fs::remove_dir_all(&state_dir);

        let record = YoloRecord::from_pane(&sample_pane());
        let stored_path = write_yolo_record(&state_dir, &record).expect("record should store");

        assert_eq!(stored_path, state_db_path(&state_dir));
        assert_eq!(
            read_yolo_record(&state_dir, &record.pane_id).expect("record should load"),
            Some(record)
        );

        let _ = fs::remove_dir_all(&state_dir);
    }

    #[test]
    fn sqlite_backed_babysit_disable_and_list_preserve_records() {
        let state_dir = unique_temp_dir("permission-babysit-disable-list");
        let _ = fs::remove_dir_all(&state_dir);

        let first = YoloRecord::from_pane(&sample_pane());
        let mut second_pane = sample_pane();
        second_pane.pane_id = String::from("%22");
        second_pane.pane_tty = String::from("/dev/pts/22");
        second_pane.pane_pid = Some(222);
        second_pane.session_id = String::from("$22");
        second_pane.session_name = String::from("demo-22");
        second_pane.window_id = String::from("@22");
        second_pane.window_name = String::from("claude-22");
        second_pane.current_path = String::from("/tmp/demo-22");
        let second = YoloRecord::from_pane(&second_pane);

        write_yolo_record(&state_dir, &first).expect("first record should store");
        write_yolo_record(&state_dir, &second).expect("second record should store");

        assert!(
            disable_yolo_record(&state_dir, &first.pane_id)
                .expect("existing record should disable")
        );
        assert!(
            disable_yolo_record(&state_dir, &first.pane_id)
                .expect("existing disabled record should still report tracked")
        );
        assert!(
            !disable_yolo_record(&state_dir, "%404")
                .expect("missing record should report false")
        );

        assert_eq!(
            list_yolo_pane_ids(&state_dir).expect("pane ids should list"),
            vec![String::from("%1"), String::from("%22")]
        );

        assert_eq!(
            read_yolo_record(&state_dir, &first.pane_id)
                .expect("disabled record should load")
                .expect("disabled record should still exist")
                .enabled,
            false
        );
        assert_eq!(
            read_yolo_record(&state_dir, &second.pane_id)
                .expect("second record should load")
                .expect("second record should still exist")
                .enabled,
            true
        );

        let _ = fs::remove_dir_all(&state_dir);
    }

    fn unique_temp_dir(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be valid")
            .as_nanos();
        std::env::temp_dir().join(format!("botctl-{label}-{}-{nanos}", std::process::id()))
    }
}
