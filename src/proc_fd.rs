//! Shared /proc/<pid>/fd walking helpers used by agy, Pi, and last-message resolution.
//!
//! ## Process-tree walking
//!
//! The hot path for dashboard refreshes calls `child_pids` once per pid in the
//! walk, which previously did a full `/proc` scan per call. The `ChildResolver`
//! trait decouples the "who are the children of pid X?" question from the
//! implementation, allowing callers to supply a pre-built snapshot (see
//! `dashboard::ProcessTreeSnapshot`) or fall back to live `/proc` scanning via
//! `LiveProc`.
//!
//! Implementation note: `transcript_from_process_tree_fds_with_resolver` uses a
//! stack with `Vec::pop()` (LIFO order — depth-first). The visit order is not
//! guaranteed to be breadth-first; callers should not rely on it. The behavioral
//! contract (find first matching fd, cycle-safe) does not depend on order.
//!
//! Cycle safety: a `HashSet` seen-set ensures that synthetic or malformed
//! process trees with parent–child cycles terminate in O(N) time rather than
//! looping forever.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use crate::app::AppResult;

/// Resolve the direct children of a process. Implementations may scan
/// `/proc` live or answer from a precomputed snapshot.
pub trait ChildResolver {
    /// Direct children of `pid`. Empty `Vec` when none. Order is
    /// implementation-defined.
    fn children_of(&self, pid: u32) -> Vec<u32>;
}

/// `/proc`-live resolver — does the original per-call `child_pids` scan.
pub struct LiveProc;

impl ChildResolver for LiveProc {
    fn children_of(&self, pid: u32) -> Vec<u32> {
        child_pids(pid).unwrap_or_default()
    }
}
const MAX_PROBE_TREE_NODES: usize = 4096;
const LIVE_CAPTURE_ATTEMPTS: usize = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeUnavailableReason {
    MissingRootPid,
    ClaudeProcessNotFound,
    AmbiguousClaudeProcess,
    UnsupportedPlatform,
    PermissionDenied,
    UnreadableMetadata,
    MalformedMetadata,
    IncompleteSnapshot,
    RootIdentityChanged,
    ParentLinkChurn,
    SnapshotDisagreement,
    RetryExhausted,
}

impl ProbeUnavailableReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::MissingRootPid => "missing-root-pid",
            Self::ClaudeProcessNotFound => "claude-process-not-found",
            Self::AmbiguousClaudeProcess => "ambiguous-claude-process",
            Self::UnsupportedPlatform => "unsupported-platform",
            Self::PermissionDenied => "permission-denied",
            Self::UnreadableMetadata => "unreadable-metadata",
            Self::MalformedMetadata => "malformed-metadata",
            Self::IncompleteSnapshot => "incomplete-snapshot",
            Self::RootIdentityChanged => "root-identity-changed",
            Self::ParentLinkChurn => "parent-link-churn",
            Self::SnapshotDisagreement => "snapshot-disagreement",
            Self::RetryExhausted => "retry-exhausted",
        }
    }

    pub fn recommendation(self) -> &'static str {
        match self {
            Self::ClaudeProcessNotFound | Self::AmbiguousClaudeProcess => {
                "retry after confirming the pane has exactly one foreground Claude process"
            }
            _ => "retry after confirming /proc access and that the tmux pane process is stable",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolChildProbe {
    Present,
    ConfirmedAbsent,
    Unavailable(ProbeUnavailableReason),
}

impl ToolChildProbe {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Present => "present",
            Self::ConfirmedAbsent => "confirmed-absent",
            Self::Unavailable(_) => "unavailable",
        }
    }

    pub fn unavailable_reason(self) -> Option<ProbeUnavailableReason> {
        match self {
            Self::Unavailable(reason) => Some(reason),
            Self::Present | Self::ConfirmedAbsent => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ProcessIdentity {
    pub pid: u32,
    pub start_time: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ProcessSnapshotEntry {
    pub identity: ProcessIdentity,
    pub parent_pid: u32,
    pub executable: PathBuf,
    pub argv: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessTreeSnapshot {
    pub root: ProcessIdentity,
    pub descendants: Vec<ProcessSnapshotEntry>,
}

pub trait ProcessTreeSnapshotSource {
    fn capture_tree(&self, root_pid: u32) -> Result<ProcessTreeSnapshot, ProbeUnavailableReason>;
}

trait ProcessMetadataSource {
    fn process_entry(&self, pid: u32) -> Result<ProcessSnapshotEntry, ProbeUnavailableReason>;
    fn complete_children(&self, pid: u32) -> Result<Vec<u32>, ProbeUnavailableReason>;
}

impl ProcessTreeSnapshotSource for LiveProc {
    fn capture_tree(&self, root_pid: u32) -> Result<ProcessTreeSnapshot, ProbeUnavailableReason> {
        #[cfg(not(target_os = "linux"))]
        {
            let _ = root_pid;
            return Err(ProbeUnavailableReason::UnsupportedPlatform);
        }
        #[cfg(target_os = "linux")]
        {
            capture_process_tree_with_retries(self, root_pid, LIVE_CAPTURE_ATTEMPTS)
        }
    }
}
fn capture_process_tree_with_retries(
    source: &dyn ProcessMetadataSource,
    root_pid: u32,
    attempts: usize,
) -> Result<ProcessTreeSnapshot, ProbeUnavailableReason> {
    for attempt in 0..attempts {
        match capture_process_tree_once(source, root_pid) {
            Ok(snapshot) => return Ok(snapshot),
            Err(
                ProbeUnavailableReason::IncompleteSnapshot
                | ProbeUnavailableReason::ParentLinkChurn,
            ) if attempt + 1 < attempts => {}
            Err(
                ProbeUnavailableReason::IncompleteSnapshot
                | ProbeUnavailableReason::ParentLinkChurn,
            ) => return Err(ProbeUnavailableReason::RetryExhausted),
            Err(error) => return Err(error),
        }
    }
    Err(ProbeUnavailableReason::RetryExhausted)
}

#[cfg(target_os = "linux")]
impl ProcessMetadataSource for LiveProc {
    fn process_entry(&self, pid: u32) -> Result<ProcessSnapshotEntry, ProbeUnavailableReason> {
        read_live_process_entry(pid)
    }

    fn complete_children(&self, pid: u32) -> Result<Vec<u32>, ProbeUnavailableReason> {
        let path = format!("/proc/{pid}/task/{pid}/children");
        let children = fs::read_to_string(path).map_err(map_proc_metadata_error)?;
        children
            .split_whitespace()
            .map(|value| {
                value
                    .parse::<u32>()
                    .map_err(|_| ProbeUnavailableReason::MalformedMetadata)
            })
            .collect()
    }
}

pub fn probe_tool_children(root_pid: Option<u32>) -> ToolChildProbe {
    let Some(root_pid) = root_pid else {
        return ToolChildProbe::Unavailable(ProbeUnavailableReason::MissingRootPid);
    };
    probe_tool_children_with_source(root_pid, &LiveProc)
}

pub fn probe_claude_tool_children(pane_root_pid: Option<u32>) -> ToolChildProbe {
    let Some(pane_root_pid) = pane_root_pid else {
        return ToolChildProbe::Unavailable(ProbeUnavailableReason::MissingRootPid);
    };
    #[cfg(not(target_os = "linux"))]
    {
        let _ = pane_root_pid;
        ToolChildProbe::Unavailable(ProbeUnavailableReason::UnsupportedPlatform)
    }
    #[cfg(target_os = "linux")]
    {
        probe_claude_tool_children_with_sources(pane_root_pid, &LiveProc, &LiveProc)
    }
}

fn probe_claude_tool_children_with_sources(
    pane_root_pid: u32,
    metadata: &dyn ProcessMetadataSource,
    snapshots: &dyn ProcessTreeSnapshotSource,
) -> ToolChildProbe {
    let identity = match resolve_claude_process_identity(pane_root_pid, metadata) {
        Ok(identity) => identity,
        Err(reason) => return ToolChildProbe::Unavailable(reason),
    };
    let first = match snapshots.capture_tree(identity.pid) {
        Ok(snapshot) => snapshot,
        Err(reason) => return ToolChildProbe::Unavailable(reason),
    };
    let probe = finish_tool_child_probe(identity.clone(), first, snapshots);
    if probe != ToolChildProbe::ConfirmedAbsent {
        return probe;
    }
    match resolve_claude_process_identity(pane_root_pid, metadata) {
        Ok(revalidated) if revalidated == identity => ToolChildProbe::ConfirmedAbsent,
        Ok(_) => ToolChildProbe::Unavailable(ProbeUnavailableReason::SnapshotDisagreement),
        Err(reason) => ToolChildProbe::Unavailable(reason),
    }
}

fn resolve_claude_process_identity(
    pane_root_pid: u32,
    source: &dyn ProcessMetadataSource,
) -> Result<ProcessIdentity, ProbeUnavailableReason> {
    let root = source.process_entry(pane_root_pid)?;
    if is_claude_process(&root) {
        return Ok(root.identity);
    }

    let tree = capture_process_tree_with_retries(source, pane_root_pid, LIVE_CAPTURE_ATTEMPTS)?;
    let candidates = tree
        .descendants
        .iter()
        .filter(|entry| is_claude_process(entry))
        .collect::<Vec<_>>();
    if candidates.is_empty() {
        return Err(ProbeUnavailableReason::ClaudeProcessNotFound);
    }

    let candidate_pids = candidates
        .iter()
        .map(|entry| entry.identity.pid)
        .collect::<HashSet<_>>();
    let parents = tree
        .descendants
        .iter()
        .map(|entry| (entry.identity.pid, entry.parent_pid))
        .collect::<HashMap<_, _>>();
    let top_level = candidates
        .into_iter()
        .filter(|entry| {
            let mut parent = entry.parent_pid;
            while parent != pane_root_pid {
                if candidate_pids.contains(&parent) {
                    return false;
                }
                let Some(next) = parents.get(&parent) else {
                    break;
                };
                parent = *next;
            }
            true
        })
        .collect::<Vec<_>>();
    match top_level.as_slice() {
        [entry] => Ok(entry.identity.clone()),
        [] => Err(ProbeUnavailableReason::ClaudeProcessNotFound),
        _ => Err(ProbeUnavailableReason::AmbiguousClaudeProcess),
    }
}

fn is_claude_process(entry: &ProcessSnapshotEntry) -> bool {
    argv0_basename(entry).is_some_and(|name| name.eq_ignore_ascii_case("claude"))
}

pub fn probe_tool_children_with_source(
    root_pid: u32,
    source: &dyn ProcessTreeSnapshotSource,
) -> ToolChildProbe {
    let first = match source.capture_tree(root_pid) {
        Ok(snapshot) => snapshot,
        Err(reason) => return ToolChildProbe::Unavailable(reason),
    };
    finish_tool_child_probe(first.root.clone(), first, source)
}

fn finish_tool_child_probe(
    expected_root: ProcessIdentity,
    first: ProcessTreeSnapshot,
    source: &dyn ProcessTreeSnapshotSource,
) -> ToolChildProbe {
    let root_pid = expected_root.pid;
    if first.root != expected_root {
        return ToolChildProbe::Unavailable(ProbeUnavailableReason::RootIdentityChanged);
    }
    if first
        .descendants
        .iter()
        .any(|entry| !is_persistent_claude_helper(entry, root_pid))
    {
        return ToolChildProbe::Present;
    }

    let second = match source.capture_tree(root_pid) {
        Ok(snapshot) => snapshot,
        Err(reason) => return ToolChildProbe::Unavailable(reason),
    };
    if first.root != second.root {
        return ToolChildProbe::Unavailable(ProbeUnavailableReason::RootIdentityChanged);
    }
    if first != second {
        return ToolChildProbe::Unavailable(ProbeUnavailableReason::SnapshotDisagreement);
    }
    ToolChildProbe::ConfirmedAbsent
}

fn capture_process_tree_once(
    source: &dyn ProcessMetadataSource,
    root_pid: u32,
) -> Result<ProcessTreeSnapshot, ProbeUnavailableReason> {
    let root_entry = source.process_entry(root_pid).map_err(|reason| {
        if reason == ProbeUnavailableReason::IncompleteSnapshot {
            ProbeUnavailableReason::MissingRootPid
        } else {
            reason
        }
    })?;
    let root = root_entry.identity;
    let mut descendants = Vec::new();
    let mut observed_children = Vec::new();
    let mut stack = vec![root_pid];
    let mut seen = HashSet::new();
    seen.insert(root_pid);

    while let Some(parent_pid) = stack.pop() {
        let mut children = source.complete_children(parent_pid)?;
        children.sort_unstable();
        observed_children.push((parent_pid, children.clone()));
        for child_pid in children {
            if !seen.insert(child_pid) {
                return Err(ProbeUnavailableReason::ParentLinkChurn);
            }
            if seen.len() > MAX_PROBE_TREE_NODES {
                return Err(ProbeUnavailableReason::IncompleteSnapshot);
            }
            let entry = source.process_entry(child_pid)?;
            if entry.parent_pid != parent_pid {
                return Err(ProbeUnavailableReason::ParentLinkChurn);
            }
            stack.push(child_pid);
            descendants.push(entry);
        }
    }

    let rechecked_root = source.process_entry(root_pid)?;
    if rechecked_root.identity != root {
        return Err(ProbeUnavailableReason::RootIdentityChanged);
    }
    for entry in &descendants {
        let rechecked = source.process_entry(entry.identity.pid)?;
        if rechecked.parent_pid != entry.parent_pid {
            return Err(ProbeUnavailableReason::ParentLinkChurn);
        }
        if rechecked != *entry {
            return Err(ProbeUnavailableReason::IncompleteSnapshot);
        }
    }
    for (parent_pid, expected) in observed_children {
        let mut rechecked = source.complete_children(parent_pid)?;
        rechecked.sort_unstable();
        if rechecked != expected {
            return Err(ProbeUnavailableReason::ParentLinkChurn);
        }
    }
    descendants.sort();
    Ok(ProcessTreeSnapshot { root, descendants })
}

#[cfg(target_os = "linux")]
fn read_live_process_entry(pid: u32) -> Result<ProcessSnapshotEntry, ProbeUnavailableReason> {
    let proc_path = PathBuf::from(format!("/proc/{pid}"));
    let stat = fs::read_to_string(proc_path.join("stat")).map_err(map_proc_metadata_error)?;
    let (parent_pid, start_time) =
        process_parent_and_start_time(&stat).ok_or(ProbeUnavailableReason::MalformedMetadata)?;
    let executable = fs::read_link(proc_path.join("exe")).map_err(map_proc_metadata_error)?;
    let cmdline = fs::read(proc_path.join("cmdline")).map_err(map_proc_metadata_error)?;
    let argv = cmdline
        .split(|byte| *byte == 0)
        .filter(|arg| !arg.is_empty())
        .map(|arg| {
            std::str::from_utf8(arg)
                .map(str::to_owned)
                .map_err(|_| ProbeUnavailableReason::MalformedMetadata)
        })
        .collect::<Result<Vec<_>, _>>()?;
    if argv.is_empty() {
        return Err(ProbeUnavailableReason::UnreadableMetadata);
    }
    Ok(ProcessSnapshotEntry {
        identity: ProcessIdentity { pid, start_time },
        parent_pid,
        executable,
        argv,
    })
}

#[cfg(target_os = "linux")]
fn map_proc_metadata_error(error: std::io::Error) -> ProbeUnavailableReason {
    match error.kind() {
        std::io::ErrorKind::PermissionDenied => ProbeUnavailableReason::PermissionDenied,
        std::io::ErrorKind::NotFound => ProbeUnavailableReason::IncompleteSnapshot,
        _ => ProbeUnavailableReason::UnreadableMetadata,
    }
}

fn process_parent_and_start_time(stat: &str) -> Option<(u32, u64)> {
    let close = stat.rfind(") ")?;
    let mut fields = stat.get(close + 2..)?.split_whitespace();
    let _state = fields.next()?;
    let parent_pid = fields.next()?.parse().ok()?;
    let start_time = fields.nth(17)?.parse().ok()?;
    Some((parent_pid, start_time))
}

fn is_persistent_claude_helper(entry: &ProcessSnapshotEntry, root_pid: u32) -> bool {
    entry.parent_pid == root_pid && (is_suv_mcp_serve(entry) || is_claude_socat_proxy(entry))
}

fn is_suv_mcp_serve(entry: &ProcessSnapshotEntry) -> bool {
    executable_basename(entry) == Some("suv")
        && entry.argv.len() == 2
        && argv0_basename(entry) == Some("suv")
        && entry.argv[1] == "mcp-serve"
}

fn is_claude_socat_proxy(entry: &ProcessSnapshotEntry) -> bool {
    if !matches!(executable_basename(entry), Some("socat" | "socat1"))
        || argv0_basename(entry) != Some("socat")
        || entry.argv.len() != 3
    {
        return false;
    }
    let listen = entry.argv[1].as_str();
    let Some(socket_id) = listen
        .strip_prefix("UNIX-LISTEN:/tmp/claude-http-")
        .and_then(|value| value.strip_suffix(".sock,fork,reuseaddr"))
    else {
        return false;
    };
    if !matches!(socket_id.len(), 16 | 32)
        || !socket_id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return false;
    }
    let Some(port) = entry.argv[2]
        .strip_prefix("TCP:localhost:")
        .and_then(|value| value.strip_suffix(",keepalive,keepidle=10,keepintvl=5,keepcnt=3"))
    else {
        return false;
    };
    port.parse::<u16>().is_ok_and(|port| port > 0)
}

fn executable_basename(entry: &ProcessSnapshotEntry) -> Option<&str> {
    entry.executable.file_name()?.to_str()
}

fn argv0_basename(entry: &ProcessSnapshotEntry) -> Option<&str> {
    Path::new(entry.argv.first()?).file_name()?.to_str()
}

/// Walk the process tree rooted at `pid` and return the first open file
/// descriptor target that satisfies `predicate`, using the supplied
/// `ChildResolver` to discover children. The predicate receives the resolved
/// fd target path; this lets callers filter by directory prefix, extension, or
/// anything else.
///
/// Uses a `HashSet` for the seen-set so that cycles in the resolver (which
/// should not occur in practice against real `/proc` but may appear in
/// synthetic snapshots) terminate safely.
pub fn transcript_from_process_tree_fds_with_resolver<F>(
    pid: u32,
    resolver: &dyn ChildResolver,
    mut predicate: F,
) -> AppResult<Option<PathBuf>>
where
    F: FnMut(&Path) -> bool,
{
    let mut stack = vec![pid];
    let mut seen = HashSet::<u32>::new();
    while let Some(current) = stack.pop() {
        if !seen.insert(current) {
            continue;
        }
        if let Some(path) = transcript_from_process_fds_with(current, &mut predicate)? {
            return Ok(Some(path));
        }
        stack.extend(resolver.children_of(current));
    }
    Ok(None)
}

/// Convenience variant: walk the process tree rooted at `pid` using the
/// supplied `ChildResolver` and return the first open fd whose target lives
/// under `transcript_root` and matches `extension`.
pub fn transcript_from_process_tree_fds_with_resolver_ext(
    pid: u32,
    resolver: &dyn ChildResolver,
    transcript_root: &Path,
    extension: &str,
) -> AppResult<Option<PathBuf>> {
    transcript_from_process_tree_fds_with_resolver(pid, resolver, |target| {
        target.starts_with(transcript_root)
            && target.extension().and_then(|value| value.to_str()) == Some(extension)
    })
}

/// Walk the process tree rooted at `pid` and return the first open file
/// descriptor target that satisfies `predicate`. The predicate receives the
/// resolved fd target path; this lets callers filter by directory prefix,
/// extension, or anything else.
///
/// This is a backward-compatible shim that delegates to
/// `transcript_from_process_tree_fds_with_resolver` with `&LiveProc`.
pub fn transcript_from_process_tree_fds_with<F>(
    pid: u32,
    predicate: F,
) -> AppResult<Option<PathBuf>>
where
    F: FnMut(&Path) -> bool,
{
    transcript_from_process_tree_fds_with_resolver(pid, &LiveProc, predicate)
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
///
/// This is a backward-compatible shim that delegates to
/// `transcript_from_process_tree_fds_with_resolver_ext` with `&LiveProc`.
pub fn transcript_from_process_tree_fds(
    pid: u32,
    transcript_root: &Path,
    extension: &str,
) -> AppResult<Option<PathBuf>> {
    transcript_from_process_tree_fds_with_resolver_ext(pid, &LiveProc, transcript_root, extension)
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
    use std::cell::{Cell, RefCell};
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::{
        ChildResolver, LiveProc, ProbeUnavailableReason, ProcessIdentity, ProcessMetadataSource,
        ProcessSnapshotEntry, ProcessTreeSnapshot, ProcessTreeSnapshotSource, ToolChildProbe,
        capture_process_tree_once, capture_process_tree_with_retries,
        probe_claude_tool_children_with_sources, probe_tool_children,
        probe_tool_children_with_source, process_parent_and_start_time, process_parent_pid,
        transcript_from_process_tree_fds_with_resolver,
    };

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

    /// Minimal in-memory resolver for unit-testing tree walks without touching
    /// `/proc`.
    struct MapResolver {
        /// ppid → children mapping.
        children: HashMap<u32, Vec<u32>>,
        /// Optional flag set when `children_of` is called, so callers can
        /// assert the resolver was consulted.
        consulted: Option<Arc<AtomicBool>>,
    }

    impl MapResolver {
        fn new(edges: &[(u32, u32)]) -> Self {
            let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
            for (parent, child) in edges {
                children.entry(*parent).or_default().push(*child);
            }
            Self {
                children,
                consulted: None,
            }
        }

        fn with_consulted_flag(mut self, flag: Arc<AtomicBool>) -> Self {
            self.consulted = Some(flag);
            self
        }
    }

    impl ChildResolver for MapResolver {
        fn children_of(&self, pid: u32) -> Vec<u32> {
            if let Some(flag) = &self.consulted {
                flag.store(true, Ordering::SeqCst);
            }
            self.children.get(&pid).cloned().unwrap_or_default()
        }
    }

    #[test]
    fn live_proc_resolver_returns_same_as_child_pids() {
        // Smoke test: LiveProc::children_of dispatches to child_pids. We
        // cannot assert the exact result (environment-dependent) but we can
        // confirm the call doesn't panic and that all returned pids are > 0
        // (pid 0 is not a valid user-space process).
        let resolver = LiveProc;
        // Use PID 1 (init/systemd) — always present on Linux; will have
        // children or an empty list.
        let result = resolver.children_of(1);
        assert!(
            result.iter().all(|&pid| pid > 0),
            "all child pids must be > 0; got: {result:?}"
        );
    }

    #[test]
    fn transcript_walk_with_synthetic_resolver_uses_snapshot() {
        // The resolver should be consulted for child discovery during the walk.
        // The predicate in this test never matches (no real /proc/pid/fd exists
        // for the fake pids), so the walk exhausts all nodes and returns None.
        let consulted = Arc::new(AtomicBool::new(false));
        let resolver =
            MapResolver::new(&[(100, 101), (101, 102)]).with_consulted_flag(consulted.clone());

        let result =
            transcript_from_process_tree_fds_with_resolver(100, &resolver, |_path: &Path| false)
                .expect("walk should not error");

        assert!(
            result.is_none(),
            "predicate never matches; result must be None"
        );
        assert!(
            consulted.load(Ordering::SeqCst),
            "resolver must have been consulted during the walk"
        );
    }

    #[test]
    fn transcript_walk_handles_cycle_safely() {
        // A resolver where 200 → 201 → 200 forms a cycle. The BFS must
        // terminate without infinite loop.
        let resolver = MapResolver::new(&[(200, 201), (201, 200)]);

        let result =
            transcript_from_process_tree_fds_with_resolver(200, &resolver, |_path: &Path| false)
                .expect("cycle walk should not error");

        // Predicate never matches; result is None but no infinite loop.
        assert!(result.is_none());
    }

    #[test]
    fn transcript_walk_descendants_visits_all_nodes() {
        // Tree: 300 → {301, 302}, 301 → {303}
        // The walk uses a stack (DFS/LIFO order); the important guarantee is
        // that all nodes are visited exactly once regardless of visit order.
        let consulted = Arc::new(AtomicBool::new(false));
        let resolver = MapResolver::new(&[(300, 301), (300, 302), (301, 303)])
            .with_consulted_flag(consulted.clone());

        let result =
            transcript_from_process_tree_fds_with_resolver(300, &resolver, |_path: &Path| {
                // We cannot easily capture visit order through the predicate
                // (it only sees fd targets), but we verify all pids are
                // reached by checking the resolver was called.
                false
            })
            .expect("walk should not error");

        assert!(result.is_none());
        assert!(consulted.load(Ordering::SeqCst));

        // Verify the walk visits all four pids by using a counting resolver.
        let counter = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let counter_clone = counter.clone();

        struct CountingResolver {
            inner: HashMap<u32, Vec<u32>>,
            counter: Arc<std::sync::atomic::AtomicU32>,
        }
        impl ChildResolver for CountingResolver {
            fn children_of(&self, pid: u32) -> Vec<u32> {
                self.counter.fetch_add(1, Ordering::SeqCst);
                self.inner.get(&pid).cloned().unwrap_or_default()
            }
        }

        let mut inner = HashMap::new();
        inner.insert(300, vec![301, 302]);
        inner.insert(301, vec![303]);
        let cr = CountingResolver {
            inner,
            counter: counter_clone,
        };

        transcript_from_process_tree_fds_with_resolver(300, &cr, |_| false)
            .expect("walk should not error on synthetic resolver");
        // children_of is called once per visited node: 300, 301, 302, 303
        // (4 nodes, 4 calls).
        assert_eq!(counter.load(Ordering::SeqCst), 4);
    }
    struct ScriptedSnapshots {
        captures: RefCell<Vec<Result<ProcessTreeSnapshot, ProbeUnavailableReason>>>,
    }

    impl ScriptedSnapshots {
        fn new(captures: Vec<Result<ProcessTreeSnapshot, ProbeUnavailableReason>>) -> Self {
            Self {
                captures: RefCell::new(captures.into_iter().rev().collect()),
            }
        }
    }

    impl ProcessTreeSnapshotSource for ScriptedSnapshots {
        fn capture_tree(
            &self,
            _root_pid: u32,
        ) -> Result<ProcessTreeSnapshot, ProbeUnavailableReason> {
            self.captures
                .borrow_mut()
                .pop()
                .unwrap_or(Err(ProbeUnavailableReason::RetryExhausted))
        }
    }

    fn entry(pid: u32, parent_pid: u32, executable: &str, argv: &[&str]) -> ProcessSnapshotEntry {
        ProcessSnapshotEntry {
            identity: ProcessIdentity {
                pid,
                start_time: u64::from(pid) * 10,
            },
            parent_pid,
            executable: PathBuf::from(executable),
            argv: argv.iter().map(|arg| (*arg).to_string()).collect(),
        }
    }

    fn snapshot(start_time: u64, descendants: Vec<ProcessSnapshotEntry>) -> ProcessTreeSnapshot {
        ProcessTreeSnapshot {
            root: ProcessIdentity {
                pid: 100,
                start_time,
            },
            descendants,
        }
    }

    fn exact_helpers() -> Vec<ProcessSnapshotEntry> {
        vec![
            entry(101, 100, "/usr/bin/suv", &["suv", "mcp-serve"]),
            entry(
                102,
                100,
                "/usr/bin/socat",
                &[
                    "socat",
                    "UNIX-LISTEN:/tmp/claude-http-fcf80b599db74d92feedfacecafebeef.sock,fork,reuseaddr",
                    "TCP:localhost:40763,keepalive,keepidle=10,keepintvl=5,keepcnt=3",
                ],
            ),
        ]
    }

    #[test]
    fn non_excluded_descendant_is_present() {
        let source = ScriptedSnapshots::new(vec![Ok(snapshot(
            1,
            vec![entry(
                103,
                100,
                "/usr/bin/bash",
                &["bash", "-lc", "cargo test"],
            )],
        ))]);

        assert_eq!(
            probe_tool_children_with_source(100, &source),
            ToolChildProbe::Present
        );
    }

    #[test]
    fn exact_correlated_helpers_require_two_stable_snapshots_for_absence() {
        let stable = snapshot(1, exact_helpers());
        let source = ScriptedSnapshots::new(vec![Ok(stable.clone()), Ok(stable)]);

        assert_eq!(
            probe_tool_children_with_source(100, &source),
            ToolChildProbe::ConfirmedAbsent
        );
        assert!(source.captures.borrow().is_empty());
    }

    #[test]
    fn packaged_socat1_proxy_is_an_exact_helper() {
        let mut helpers = exact_helpers();
        helpers[1].executable = PathBuf::from("/usr/bin/socat1");
        helpers[1].argv[1] =
            String::from("UNIX-LISTEN:/tmp/claude-http-fcf80b599db74d92.sock,fork,reuseaddr");
        let stable = snapshot(1, helpers);
        let source = ScriptedSnapshots::new(vec![Ok(stable.clone()), Ok(stable)]);

        assert_eq!(
            probe_tool_children_with_source(100, &source),
            ToolChildProbe::ConfirmedAbsent
        );
    }

    #[test]
    fn helper_basename_impostors_remain_present() {
        for impostor in [
            entry(101, 100, "/usr/bin/suv", &["suv", "serve"]),
            entry(101, 99, "/usr/bin/suv", &["suv", "mcp-serve"]),
            entry(102, 100, "/usr/bin/socat", &["socat"]),
            entry(
                102,
                100,
                "/usr/bin/socat",
                &[
                    "socat",
                    "UNIX-LISTEN:/tmp/other.sock",
                    "TCP:localhost:40763",
                ],
            ),
        ] {
            let source = ScriptedSnapshots::new(vec![Ok(snapshot(1, vec![impostor]))]);
            assert_eq!(
                probe_tool_children_with_source(100, &source),
                ToolChildProbe::Present
            );
        }
    }

    #[test]
    fn missing_root_and_snapshot_errors_are_unavailable() {
        assert_eq!(
            probe_tool_children(None),
            ToolChildProbe::Unavailable(ProbeUnavailableReason::MissingRootPid)
        );
        for reason in [
            ProbeUnavailableReason::UnsupportedPlatform,
            ProbeUnavailableReason::PermissionDenied,
            ProbeUnavailableReason::UnreadableMetadata,
            ProbeUnavailableReason::MalformedMetadata,
            ProbeUnavailableReason::IncompleteSnapshot,
            ProbeUnavailableReason::ParentLinkChurn,
            ProbeUnavailableReason::RetryExhausted,
        ] {
            let source = ScriptedSnapshots::new(vec![Err(reason)]);
            assert_eq!(
                probe_tool_children_with_source(100, &source),
                ToolChildProbe::Unavailable(reason)
            );
        }
    }

    #[test]
    fn root_identity_change_and_snapshot_disagreement_are_unavailable() {
        let first = snapshot(1, exact_helpers());
        let changed_root = snapshot(2, exact_helpers());
        let source = ScriptedSnapshots::new(vec![Ok(first.clone()), Ok(changed_root)]);
        assert_eq!(
            probe_tool_children_with_source(100, &source),
            ToolChildProbe::Unavailable(ProbeUnavailableReason::RootIdentityChanged)
        );

        let source = ScriptedSnapshots::new(vec![Ok(first), Ok(snapshot(1, Vec::new()))]);
        assert_eq!(
            probe_tool_children_with_source(100, &source),
            ToolChildProbe::Unavailable(ProbeUnavailableReason::SnapshotDisagreement)
        );
    }

    struct MetadataFixture {
        entries: HashMap<u32, ProcessSnapshotEntry>,
        children: HashMap<u32, Vec<u32>>,
    }

    impl ProcessMetadataSource for MetadataFixture {
        fn process_entry(&self, pid: u32) -> Result<ProcessSnapshotEntry, ProbeUnavailableReason> {
            self.entries
                .get(&pid)
                .cloned()
                .ok_or(ProbeUnavailableReason::IncompleteSnapshot)
        }

        fn complete_children(&self, pid: u32) -> Result<Vec<u32>, ProbeUnavailableReason> {
            Ok(self.children.get(&pid).cloned().unwrap_or_default())
        }
    }

    impl ProcessTreeSnapshotSource for MetadataFixture {
        fn capture_tree(
            &self,
            root_pid: u32,
        ) -> Result<ProcessTreeSnapshot, ProbeUnavailableReason> {
            capture_process_tree_once(self, root_pid)
        }
    }

    #[test]
    fn shell_root_resolves_unique_claude_before_probing_tools() {
        let fixture = MetadataFixture {
            entries: HashMap::from([
                (100, entry(100, 1, "/usr/bin/bash", &["bash"])),
                (
                    101,
                    entry(
                        101,
                        100,
                        "/home/user/.local/share/claude/versions/2.1.220",
                        &["claude", "--agent", "megamind"],
                    ),
                ),
                (102, entry(102, 101, "/usr/bin/suv", &["suv", "mcp-serve"])),
                (
                    103,
                    entry(
                        103,
                        101,
                        "/usr/bin/socat1",
                        &[
                            "socat",
                            "UNIX-LISTEN:/tmp/claude-http-fcf80b599db74d92.sock,fork,reuseaddr",
                            "TCP:localhost:40763,keepalive,keepidle=10,keepintvl=5,keepcnt=3",
                        ],
                    ),
                ),
            ]),
            children: HashMap::from([
                (100, vec![101]),
                (101, vec![102, 103]),
                (102, Vec::new()),
                (103, Vec::new()),
            ]),
        };

        assert_eq!(
            probe_claude_tool_children_with_sources(100, &fixture, &fixture),
            ToolChildProbe::ConfirmedAbsent
        );
    }

    #[test]
    fn ambiguous_shell_root_claude_processes_fail_closed() {
        let fixture = MetadataFixture {
            entries: HashMap::from([
                (100, entry(100, 1, "/usr/bin/bash", &["bash"])),
                (101, entry(101, 100, "/opt/claude/1", &["claude"])),
                (102, entry(102, 100, "/opt/claude/2", &["claude"])),
            ]),
            children: HashMap::from([(100, vec![101, 102]), (101, Vec::new()), (102, Vec::new())]),
        };

        assert_eq!(
            probe_claude_tool_children_with_sources(100, &fixture, &fixture),
            ToolChildProbe::Unavailable(ProbeUnavailableReason::AmbiguousClaudeProcess)
        );
    }

    struct RacingPaneRoot {
        root_reads: Cell<usize>,
        entries: HashMap<u32, ProcessSnapshotEntry>,
    }

    impl ProcessMetadataSource for RacingPaneRoot {
        fn process_entry(&self, pid: u32) -> Result<ProcessSnapshotEntry, ProbeUnavailableReason> {
            if pid == 100 {
                self.root_reads.set(self.root_reads.get() + 1);
            }
            self.entries
                .get(&pid)
                .cloned()
                .ok_or(ProbeUnavailableReason::IncompleteSnapshot)
        }

        fn complete_children(&self, pid: u32) -> Result<Vec<u32>, ProbeUnavailableReason> {
            Ok(match pid {
                100 if self.root_reads.get() >= 4 => vec![101, 102],
                100 => vec![101],
                101 | 102 => Vec::new(),
                _ => Vec::new(),
            })
        }
    }

    #[test]
    fn shell_root_uniqueness_is_revalidated_after_absence_probe() {
        let metadata = RacingPaneRoot {
            root_reads: Cell::new(0),
            entries: HashMap::from([
                (100, entry(100, 1, "/usr/bin/bash", &["bash"])),
                (101, entry(101, 100, "/opt/claude/1", &["claude"])),
                (102, entry(102, 100, "/opt/claude/2", &["claude"])),
            ]),
        };
        let stable = ProcessTreeSnapshot {
            root: ProcessIdentity {
                pid: 101,
                start_time: 1010,
            },
            descendants: Vec::new(),
        };
        let snapshots = ScriptedSnapshots::new(vec![Ok(stable.clone()), Ok(stable)]);

        assert_eq!(
            probe_claude_tool_children_with_sources(100, &metadata, &snapshots),
            ToolChildProbe::Unavailable(ProbeUnavailableReason::AmbiguousClaudeProcess)
        );
    }

    #[test]
    fn cyclic_or_churning_parent_links_terminate_unavailable() {
        let fixture = MetadataFixture {
            entries: HashMap::from([
                (100, entry(100, 1, "/usr/bin/claude", &["claude"])),
                (101, entry(101, 100, "/usr/bin/suv", &["suv", "mcp-serve"])),
            ]),
            children: HashMap::from([(100, vec![101]), (101, vec![100])]),
        };

        assert_eq!(
            capture_process_tree_once(&fixture, 100),
            Err(ProbeUnavailableReason::ParentLinkChurn)
        );
        assert_eq!(
            capture_process_tree_with_retries(&fixture, 100, 3),
            Err(ProbeUnavailableReason::RetryExhausted)
        );
    }

    #[test]
    fn parses_process_start_identity_and_rejects_malformed_stat() {
        let mut fields = vec!["0"; 20];
        fields[0] = "S";
        fields[1] = "7";
        fields[19] = "424242";
        let stat = format!("123 (agent worker) {}", fields.join(" "));

        assert_eq!(process_parent_and_start_time(&stat), Some((7, 424242)));
        assert_eq!(process_parent_and_start_time("malformed"), None);
    }
}
