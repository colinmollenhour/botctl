use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

use crate::app::{AppError, AppResult};

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

pub fn prepare_prompt(state_dir: &Path, session_name: &str, content: &str) -> AppResult<PathBuf> {
    let path = pending_prompt_path(state_dir, session_name);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&path, content)?;
    Ok(path)
}

pub fn write_editor_target_from_pending(
    state_dir: &Path,
    session_name: &str,
    target_path: &Path,
    consume: bool,
) -> AppResult<String> {
    let pending_path = pending_prompt_path(state_dir, session_name);
    let content = fs::read_to_string(&pending_path).map_err(|error| {
        AppError::new(format!(
            "failed to read pending prompt for session {} at {}: {}",
            session_name,
            pending_path.display(),
            error
        ))
    })?;
    write_editor_target(target_path, &content)?;
    if consume {
        fs::remove_file(&pending_path)?;
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

pub fn pending_prompt_path(state_dir: &Path, session_name: &str) -> PathBuf {
    state_dir
        .join("prompts")
        .join(sanitize_session_name(session_name))
        .join("pending-prompt.txt")
}

fn sanitize_session_name(session_name: &str) -> String {
    let mut out = String::with_capacity(session_name.len());
    for ch in session_name.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
            out.push(ch);
        } else {
            out.push('_');
        }
    }

    if out.is_empty() {
        String::from("default")
    } else {
        out
    }
}

#[cfg(any(test, rust_analyzer))]
mod tests {
    use std::ffi::OsStr;
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::{
        PromptSource, default_state_dir_from_env, pending_prompt_path, prepare_prompt,
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
        let target = root.join("editor.txt");

        let pending = prepare_prompt(&state_dir, "demo/session", "hello world")
            .expect("prompt should be prepared");
        assert_eq!(pending, pending_prompt_path(&state_dir, "demo/session"));

        let content = write_editor_target_from_pending(&state_dir, "demo/session", &target, true)
            .expect("editor helper should write target");
        assert_eq!(content, "hello world");
        assert_eq!(
            fs::read_to_string(&target).expect("target should exist"),
            "hello world"
        );
        assert!(!pending.exists());
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
