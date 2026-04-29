use std::path::{Path, PathBuf};

use crate::app::AppResult;
use crate::storage::{
    BabysitRegistrationRecord,
    disable_babysit_registration_by_pane_id as storage_disable_yolo_record,
    list_babysit_registration_pane_ids, load_babysit_registration_by_pane_id, state_db_path,
    store_babysit_registration,
};
use crate::tmux::TmuxPane;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct YoloRecord {
    pub instance_id: String,
    pub workspace_id: String,
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
    pub fn from_registration(registration: BabysitRegistrationRecord) -> Self {
        let instance = registration.instance;
        Self {
            instance_id: instance.id,
            workspace_id: instance.workspace_id,
            enabled: registration.enabled,
            pane_id: instance.pane_id.unwrap_or_default(),
            pane_tty: instance.pane_tty.unwrap_or_default(),
            pane_pid: instance.pane_pid,
            session_id: instance.session_id.unwrap_or_default(),
            session_name: instance.session_name,
            window_id: instance.window_id.unwrap_or_default(),
            window_name: instance.window_name.unwrap_or_default(),
            current_command: instance.current_command.unwrap_or_default(),
            current_path: instance.current_path.unwrap_or_default(),
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

pub fn write_yolo_record(
    state_dir: &Path,
    workspace_id: &str,
    pane: &TmuxPane,
) -> AppResult<PathBuf> {
    store_babysit_registration(state_dir, workspace_id, pane, true)?;
    Ok(state_db_path(state_dir))
}

pub fn read_yolo_record(state_dir: &Path, pane_id: &str) -> AppResult<Option<YoloRecord>> {
    Ok(
        load_babysit_registration_by_pane_id(state_dir, pane_id)?
            .map(YoloRecord::from_registration),
    )
}

pub fn disable_yolo_record(state_dir: &Path, pane_id: &str) -> AppResult<bool> {
    storage_disable_yolo_record(state_dir, pane_id)
}

pub fn list_yolo_pane_ids(state_dir: &Path, workspace_id: Option<&str>) -> AppResult<Vec<String>> {
    list_babysit_registration_pane_ids(state_dir, workspace_id)
}

#[cfg(any(test, rust_analyzer))]
mod tests {
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;
    use crate::storage::state_db_path;

    fn sample_pane() -> TmuxPane {
        TmuxPane {
            pane_id: String::from("%1"),
            pane_tty: String::from("/dev/pts/1"),
            pane_pid: Some(123),
            session_id: String::from("$1"),
            session_name: String::from("demo"),
            window_id: String::from("@2"),
            window_index: 0,
            window_name: String::from("claude"),
            pane_index: 0,
            current_command: String::from("claude"),
            current_path: String::from("/tmp/demo"),
            pane_title: String::new(),
            pane_active: true,
            cursor_x: Some(1),
            cursor_y: Some(2),
        }
    }

    #[test]
    fn record_round_trips_and_matches() {
        let pane = sample_pane();
        let record = YoloRecord {
            instance_id: String::from("instance-1"),
            workspace_id: String::from("workspace-1"),
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
        };
        assert!(record.matches_pane(&pane));
    }

    #[test]
    fn sqlite_backed_yolo_record_round_trips_by_pane_id() {
        let state_dir = unique_temp_dir("yolo-round-trip");
        let workspace_root = unique_temp_dir("yolo-workspace");
        let _ = fs::remove_dir_all(&state_dir);
        fs::create_dir_all(&workspace_root).expect("workspace should exist");
        let workspace = crate::storage::resolve_workspace_for_path(&state_dir, &workspace_root)
            .expect("workspace should resolve");

        let pane = sample_pane();
        let stored_path =
            write_yolo_record(&state_dir, &workspace.id, &pane).expect("record should store");

        assert_eq!(stored_path, state_db_path(&state_dir));
        assert_eq!(
            read_yolo_record(&state_dir, &pane.pane_id)
                .expect("record should load")
                .map(|record| record.matches_pane(&pane)),
            Some(true)
        );

        let _ = fs::remove_dir_all(&state_dir);
    }

    #[test]
    fn sqlite_backed_yolo_disable_and_list_preserve_records() {
        let state_dir = unique_temp_dir("yolo-disable-list");
        let workspace_root = unique_temp_dir("yolo-list-workspace");
        let _ = fs::remove_dir_all(&state_dir);
        fs::create_dir_all(&workspace_root).expect("workspace should exist");
        let workspace = crate::storage::resolve_workspace_for_path(&state_dir, &workspace_root)
            .expect("workspace should resolve");

        let first = sample_pane();
        let mut second = sample_pane();
        second.pane_id = String::from("%22");
        second.pane_tty = String::from("/dev/pts/22");
        second.pane_pid = Some(222);
        second.session_id = String::from("$22");
        second.session_name = String::from("demo-22");
        second.window_id = String::from("@22");
        second.window_name = String::from("claude-22");
        second.current_path = workspace_root.display().to_string();

        write_yolo_record(&state_dir, &workspace.id, &first).expect("first record should store");
        write_yolo_record(&state_dir, &workspace.id, &second).expect("second record should store");

        assert!(
            disable_yolo_record(&state_dir, &first.pane_id)
                .expect("existing record should disable")
        );
        assert!(
            disable_yolo_record(&state_dir, &first.pane_id)
                .expect("existing disabled record should still report tracked")
        );
        assert!(
            !disable_yolo_record(&state_dir, "%404").expect("missing record should report false")
        );

        assert_eq!(
            list_yolo_pane_ids(&state_dir, Some(&workspace.id)).expect("pane ids should list"),
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
