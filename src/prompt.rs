use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

use crate::app::{AppError, AppResult};
use crate::storage::{
    delete_pending_prompt, load_pending_prompt, state_db_path, store_pending_prompt,
};

#[derive(Debug, Clone)]
pub enum PromptSource<'a> {
    Text(&'a str),
    File(&'a Path),
}

pub fn default_state_dir() -> AppResult<PathBuf> {
    default_state_dir_from_env(
        std::env::var_os("XDG_STATE_HOME").as_deref(),
        std::env::var_os("HOME").as_deref(),
    )
}

pub fn resolve_state_dir(path: Option<&Path>) -> AppResult<PathBuf> {
    resolve_state_dir_from_env(
        path,
        std::env::var_os("XDG_STATE_HOME").as_deref(),
        std::env::var_os("HOME").as_deref(),
    )
}

fn resolve_state_dir_from_env(
    path: Option<&Path>,
    xdg_state_home: Option<&OsStr>,
    home: Option<&OsStr>,
) -> AppResult<PathBuf> {
    match path {
        Some(path) => Ok(path.to_path_buf()),
        None => default_state_dir_from_env(xdg_state_home, home),
    }
}

fn default_state_dir_from_env(
    xdg_state_home: Option<&OsStr>,
    home: Option<&OsStr>,
) -> AppResult<PathBuf> {
    if let Some(xdg_state_home) = xdg_state_home.filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(xdg_state_home).join("botctl"));
    }

    let home = home.filter(|value| !value.is_empty()).ok_or_else(|| {
        AppError::new(
            "failed to resolve default state directory: XDG_STATE_HOME is unset or empty and HOME is unset or empty",
        )
    })?;

    Ok(PathBuf::from(home)
        .join(".local")
        .join("state")
        .join("botctl"))
}

pub fn resolve_prompt_text(source: PromptSource<'_>) -> AppResult<String> {
    match source {
        PromptSource::Text(text) => Ok(text.to_string()),
        PromptSource::File(path) => fs::read_to_string(path).map_err(AppError::from),
    }
}

pub fn prepare_prompt(
    state_dir: &Path,
    workspace_id: &str,
    session_name: &str,
    content: &str,
) -> AppResult<()> {
    store_pending_prompt(state_dir, workspace_id, session_name, content)
}

pub fn pending_prompt_text(
    state_dir: &Path,
    workspace_id: &str,
    session_name: &str,
) -> AppResult<Option<String>> {
    load_pending_prompt(state_dir, workspace_id, session_name)
}

pub fn write_editor_target_from_pending(
    state_dir: &Path,
    workspace_id: &str,
    session_name: &str,
    target_path: &Path,
    consume: bool,
) -> AppResult<String> {
    let state_db = state_db_path(state_dir);
    let content = pending_prompt_text(state_dir, workspace_id, session_name)?.ok_or_else(|| {
        AppError::new(format!(
            "no pending prompt prepared for session {} in {}",
            session_name,
            state_db.display()
        ))
    })?;
    write_editor_target(target_path, &content)?;
    if consume && !delete_pending_prompt(state_dir, workspace_id, session_name)? {
        return Err(AppError::new(format!(
            "pending prompt disappeared before it could be consumed for session {} in {}",
            session_name,
            state_db.display()
        )));
    }
    Ok(content)
}

pub fn write_editor_target(target_path: &Path, content: &str) -> AppResult<()> {
    if let Some(parent) = target_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(target_path, content)?;
    Ok(())
}

#[cfg(any(test, rust_analyzer))]
mod tests {
    use std::ffi::OsStr;
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use crate::storage::resolve_workspace_for_path;

    use super::{
        PromptSource, default_state_dir_from_env, pending_prompt_text, prepare_prompt,
        resolve_prompt_text, resolve_state_dir_from_env, write_editor_target_from_pending,
    };

    #[test]
    fn default_state_dir_prefers_non_empty_xdg_state_home() {
        assert_eq!(
            default_state_dir_from_env(
                Some(OsStr::new("/tmp/xdg-state")),
                Some(OsStr::new("/tmp/home")),
            )
            .expect("xdg state home should win"),
            PathBuf::from("/tmp/xdg-state/botctl")
        );
    }

    #[test]
    fn default_state_dir_falls_back_to_home_local_state() {
        let expected = PathBuf::from("/tmp/home/.local/state/botctl");

        assert_eq!(
            default_state_dir_from_env(None, Some(OsStr::new("/tmp/home")))
                .expect("home fallback should work"),
            expected
        );
        assert_eq!(
            default_state_dir_from_env(Some(OsStr::new("")), Some(OsStr::new("/tmp/home")))
                .expect("empty xdg state home should fall back to home"),
            expected
        );
    }

    #[test]
    fn state_dir_override_wins_without_env_defaults() {
        assert_eq!(
            resolve_state_dir_from_env(
                Some(std::path::Path::new("/tmp/override-state")),
                None,
                None,
            )
            .expect("explicit state dir should bypass env resolution"),
            PathBuf::from("/tmp/override-state")
        );
    }

    #[test]
    fn default_state_dir_errors_without_xdg_state_home_or_home() {
        let error = default_state_dir_from_env(Some(OsStr::new("")), None)
            .expect_err("missing env should fail");

        assert!(error.to_string().contains("HOME"));
    }

    #[test]
    fn prepare_and_consume_prompt_round_trip() {
        let root = unique_temp_dir("prompt-roundtrip");
        let state_dir = root.join("state");
        let workspace_root = root.join("workspace");
        let target = root.join("editor.txt");
        fs::create_dir_all(&workspace_root).expect("workspace should exist");
        let workspace = resolve_workspace_for_path(&state_dir, &workspace_root)
            .expect("workspace should resolve");

        prepare_prompt(&state_dir, &workspace.id, "demo/session", "hello world")
            .expect("prompt should be prepared");
        assert_eq!(
            pending_prompt_text(&state_dir, &workspace.id, "demo/session")
                .expect("pending prompt should load"),
            Some(String::from("hello world"))
        );

        let content = write_editor_target_from_pending(
            &state_dir,
            &workspace.id,
            "demo/session",
            &target,
            true,
        )
        .expect("editor helper should write target");
        assert_eq!(content, "hello world");
        assert_eq!(
            fs::read_to_string(&target).expect("target should exist"),
            "hello world"
        );
        assert_eq!(
            pending_prompt_text(&state_dir, &workspace.id, "demo/session")
                .expect("consumed prompt lookup should succeed"),
            None
        );
    }

    #[test]
    fn editor_helper_keep_pending_leaves_staged_prompt() {
        let root = unique_temp_dir("prompt-keep-pending");
        let state_dir = root.join("state");
        let workspace_root = root.join("workspace");
        let target = root.join("editor.txt");
        fs::create_dir_all(&workspace_root).expect("workspace should exist");
        let workspace = resolve_workspace_for_path(&state_dir, &workspace_root)
            .expect("workspace should resolve");

        prepare_prompt(&state_dir, &workspace.id, "demo/session", "hello world")
            .expect("prompt should be prepared");

        let content = write_editor_target_from_pending(
            &state_dir,
            &workspace.id,
            "demo/session",
            &target,
            false,
        )
        .expect("editor helper should write target without consuming");

        assert_eq!(content, "hello world");
        assert_eq!(
            fs::read_to_string(&target).expect("target should exist"),
            "hello world"
        );
        assert_eq!(
            pending_prompt_text(&state_dir, &workspace.id, "demo/session")
                .expect("pending prompt should still exist"),
            Some(String::from("hello world"))
        );
    }

    #[test]
    fn write_editor_target_from_pending_errors_when_prompt_is_missing() {
        let root = unique_temp_dir("prompt-missing");
        let state_dir = root.join("state");
        let workspace_root = root.join("workspace");
        let target = root.join("editor.txt");
        fs::create_dir_all(&workspace_root).expect("workspace should exist");
        let workspace = resolve_workspace_for_path(&state_dir, &workspace_root)
            .expect("workspace should resolve");

        let error = write_editor_target_from_pending(
            &state_dir,
            &workspace.id,
            "demo/session",
            &target,
            true,
        )
        .expect_err("missing pending prompt should fail");

        assert!(
            error
                .to_string()
                .contains("no pending prompt prepared for session demo/session")
        );
        assert!(!target.exists());
    }

    #[test]
    fn resolve_prompt_text_from_file() {
        let root = unique_temp_dir("prompt-source");
        let source = root.join("prompt.txt");
        fs::create_dir_all(&root).expect("temp dir should exist");
        fs::write(&source, "from file").expect("source should write");

        let content =
            resolve_prompt_text(PromptSource::File(&source)).expect("prompt should load from file");
        assert_eq!(content, "from file");
    }

    fn unique_temp_dir(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be valid")
            .as_nanos();
        std::env::temp_dir().join(format!("botctl-{label}-{}-{nanos}", std::process::id()))
    }
}
