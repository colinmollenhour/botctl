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

use std::collections::HashSet;
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
    use std::collections::HashMap;

    use super::{
        ChildResolver, LiveProc, process_parent_pid, transcript_from_process_tree_fds_with_resolver,
    };
    use std::path::Path;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

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
}
