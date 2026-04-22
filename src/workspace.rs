use std::path::{Path, PathBuf};
use std::process::Command;

use crate::app::{AppError, AppResult};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceLocator {
    pub workspace_root: PathBuf,
    pub repo_key: Option<String>,
}

pub fn resolve_workspace_locator(path: &Path) -> AppResult<WorkspaceLocator> {
    let resolved_path = path.canonicalize().map_err(|error| {
        AppError::new(format!(
            "failed to resolve workspace path {}: {error}",
            path.display()
        ))
    })?;

    if let Some(locator) = git_workspace_locator(&resolved_path)? {
        return Ok(locator);
    }

    Ok(WorkspaceLocator {
        workspace_root: resolved_path,
        repo_key: None,
    })
}

fn git_workspace_locator(path: &Path) -> AppResult<Option<WorkspaceLocator>> {
    let top_level = run_git_rev_parse(path, "--show-toplevel")?;
    let common_dir = run_git_rev_parse(path, "--git-common-dir")?;

    let (Some(top_level), Some(common_dir)) = (top_level, common_dir) else {
        return Ok(None);
    };

    Ok(Some(WorkspaceLocator {
        workspace_root: PathBuf::from(top_level),
        repo_key: Some(common_dir),
    }))
}

fn run_git_rev_parse(path: &Path, arg: &str) -> AppResult<Option<String>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .arg("rev-parse")
        .arg(arg)
        .output()?;

    if !output.status.success() {
        return Ok(None);
    }

    let stdout = String::from_utf8(output.stdout)
        .map_err(|_| AppError::new("git rev-parse output was not valid UTF-8"))?;
    let value = stdout.trim();
    if value.is_empty() {
        return Ok(None);
    }

    let value_path = PathBuf::from(value);
    let canonical_value = if value_path.is_absolute() {
        value_path.canonicalize().map_err(|error| {
            AppError::new(format!(
                "failed to canonicalize git workspace path {}: {error}",
                value_path.display()
            ))
        })?
    } else {
        path.join(value_path).canonicalize().map_err(|error| {
            AppError::new(format!(
                "failed to canonicalize git workspace path from {}: {error}",
                path.display()
            ))
        })?
    };

    Ok(Some(canonical_value.display().to_string()))
}

#[cfg(any(test, rust_analyzer))]
mod tests {
    use std::fs;
    use std::process::Command;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::resolve_workspace_locator;

    #[test]
    fn resolve_workspace_locator_uses_non_git_path_directly() {
        let root = unique_temp_dir("workspace-non-git");
        let nested = root.join("nested");
        fs::create_dir_all(&nested).expect("temp directories should create");

        let locator = resolve_workspace_locator(&nested).expect("locator should resolve");

        assert_eq!(
            locator.workspace_root,
            nested.canonicalize().expect("path should canonicalize")
        );
        assert_eq!(locator.repo_key, None);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn resolve_workspace_locator_collapses_git_subdir_to_repo_root() {
        let root = unique_temp_dir("workspace-git");
        fs::create_dir_all(root.join("nested/deeper")).expect("temp directories should create");
        let init = Command::new("git")
            .arg("init")
            .arg(&root)
            .output()
            .expect("git init should run");
        assert!(init.status.success(), "git init should succeed");

        let locator =
            resolve_workspace_locator(&root.join("nested/deeper")).expect("locator should resolve");

        assert_eq!(
            locator.workspace_root,
            root.canonicalize().expect("repo root should canonicalize")
        );
        assert!(locator.repo_key.is_some());

        let _ = fs::remove_dir_all(&root);
    }

    fn unique_temp_dir(label: &str) -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be valid")
            .as_nanos();
        std::env::temp_dir().join(format!("botctl-{label}-{}-{nanos}", std::process::id()))
    }
}
