//! Shared /proc/<pid>/fd walking helpers used by agy, Pi, and last-message resolution.

use std::fs;
use std::path::{Path, PathBuf};

use crate::app::AppResult;

/// Walk the process tree rooted at `pid` and return the first open file
/// descriptor target that satisfies `predicate`. The predicate receives the
/// resolved fd target path; this lets callers filter by directory prefix,
/// extension, or anything else.
pub fn transcript_from_process_tree_fds_with<F>(
    pid: u32,
    mut predicate: F,
) -> AppResult<Option<PathBuf>>
where
    F: FnMut(&Path) -> bool,
{
    let mut stack = vec![pid];
    let mut seen = Vec::<u32>::new();
    while let Some(current) = stack.pop() {
        if seen.contains(&current) {
            continue;
        }
        seen.push(current);
        if let Some(path) = transcript_from_process_fds_with(current, &mut predicate)? {
            return Ok(Some(path));
        }
        stack.extend(child_pids(current)?);
    }
    Ok(None)
}

/// Walk the open file descriptors of `pid` only and return the first
/// resolved target satisfying `predicate`.
pub(crate) fn transcript_from_process_fds_with<F>(
    pid: u32,
    mut predicate: F,
) -> AppResult<Option<PathBuf>>
where
    F: FnMut(&Path) -> bool,
{
    let fd_dir = PathBuf::from(format!("/proc/{pid}/fd"));
    let entries = match fs::read_dir(fd_dir) {
        Ok(entries) => entries,
        Err(_) => return Ok(None),
    };

    for entry in entries {
        let Ok(entry) = entry else {
            continue;
        };
        let Ok(target) = fs::read_link(entry.path()) else {
            continue;
        };
        if predicate(&target) {
            return Ok(Some(target));
        }
    }
    Ok(None)
}

/// Walk the process tree rooted at `pid` and return the first open fd whose
/// target lives under `transcript_root` and matches `extension`. Preserved
/// API used by Pi (and consumed by agy via a thin wrapper that passes
/// `extension = "pb"`).
pub fn transcript_from_process_tree_fds(
    pid: u32,
    transcript_root: &Path,
    extension: &str,
) -> AppResult<Option<PathBuf>> {
    transcript_from_process_tree_fds_with(pid, |target| {
        target.starts_with(transcript_root)
            && target.extension().and_then(|value| value.to_str()) == Some(extension)
    })
}

/// Walk the open file descriptors of `pid` and return the first target whose
/// path lives under `transcript_root` and matches `extension`. Preserved API
/// used by `last_message.rs` for Claude/Codex transcript discovery.
pub fn transcript_from_process_fds(
    pid: u32,
    transcript_root: &Path,
    extension: &str,
) -> AppResult<Option<PathBuf>> {
    transcript_from_process_fds_with(pid, |target| {
        target.starts_with(transcript_root)
            && target.extension().and_then(|value| value.to_str()) == Some(extension)
    })
}

pub(crate) fn child_pids(pid: u32) -> AppResult<Vec<u32>> {
    let mut children = Vec::new();
    let proc_dir = match fs::read_dir("/proc") {
        Ok(entries) => entries,
        Err(_) => return Ok(children),
    };
    for entry in proc_dir {
        let Ok(entry) = entry else {
            continue;
        };
        let Some(child_pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        let stat = match fs::read_to_string(entry.path().join("stat")) {
            Ok(stat) => stat,
            Err(_) => continue,
        };
        if process_parent_pid(&stat) == Some(pid) {
            children.push(child_pid);
        }
    }
    Ok(children)
}

pub(crate) fn process_parent_pid(stat: &str) -> Option<u32> {
    let close = stat.rfind(") ")?;
    let rest = stat.get(close + 2..)?;
    rest.split_whitespace().nth(1)?.parse().ok()
}

#[cfg(any(test, rust_analyzer))]
mod tests {
    use super::process_parent_pid;

    #[test]
    fn parses_proc_stat_parent_pid() {
        assert_eq!(
            process_parent_pid("123 (agent worker) S 7 1 1 0 -1 4194560"),
            Some(7)
        );
    }

    #[test]
    fn parses_parent_pid_with_paren_in_comm() {
        assert_eq!(
            process_parent_pid("12 (weird (name)) S 42 1 1 0 -1 4194560"),
            Some(42)
        );
    }
}
