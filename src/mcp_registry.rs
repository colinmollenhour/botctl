use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, params, types::Type};
use serde::{Deserialize, Serialize};
use uuid::{ContextV7, Timestamp, Uuid};

use crate::app::{AppError, AppResult};

const LOCK_TTL_MS: i64 = 10 * 60 * 1000;
const SQLITE_BUSY_TIMEOUT_MS: u64 = 5000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleState {
    Starting,
    Ready,
    Running,
    Blocked,
    Dead,
    Killed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Provider {
    Claude,
    Codex,
    Agy,
}

impl Provider {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Agy => "agy",
        }
    }

    pub fn command(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Agy => "agy",
        }
    }

    pub fn parse(value: &str) -> AppResult<Self> {
        match value {
            "claude" => Ok(Self::Claude),
            "codex" => Ok(Self::Codex),
            "agy" => Ok(Self::Agy),
            other => Err(AppError::new(format!(
                "invalid_params: unknown provider {other}"
            ))),
        }
    }

    fn from_db(value: &str) -> AppResult<Self> {
        Self::parse(value)
            .map_err(|_| AppError::new(format!("invalid mcp provider in database: {value}")))
    }
}

impl LifecycleState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Ready => "ready",
            Self::Running => "running",
            Self::Blocked => "blocked",
            Self::Dead => "dead",
            Self::Killed => "killed",
        }
    }

    fn from_str(value: &str) -> AppResult<Self> {
        match value {
            "starting" => Ok(Self::Starting),
            "ready" => Ok(Self::Ready),
            "running" => Ok(Self::Running),
            "blocked" => Ok(Self::Blocked),
            "dead" => Ok(Self::Dead),
            "killed" => Ok(Self::Killed),
            other => Err(AppError::new(format!(
                "invalid mcp lifecycle_state in database: {other}"
            ))),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpSessionRecord {
    pub id: String,
    pub owner_server_id: String,
    pub provider: Provider,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub agent: Option<String>,
    pub permission_mode: Option<String>,
    pub settings: Option<String>,
    pub tmux_session_name: String,
    pub tmux_window_id: String,
    pub tmux_window_name: String,
    pub tmux_pane_id: String,
    pub cwd: String,
    pub lifecycle_state: LifecycleState,
    pub last_state: Option<String>,
    pub last_message_id: Option<String>,
    pub last_message_text: Option<String>,
    pub last_message_seen_at_ms: Option<i64>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    pub dead_at_ms: Option<i64>,
    pub killed_at_ms: Option<i64>,
    pub blocked_reason: Option<String>,
    pub blocked_snapshot: Option<String>,
    pub blocked_at_ms: Option<i64>,
    pub resurrected_at_ms: Option<i64>,
    pub resurrection_count: i64,
}

#[derive(Debug, Clone)]
pub struct NewSessionRecord {
    pub owner_server_id: String,
    pub provider: Provider,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub agent: Option<String>,
    pub permission_mode: Option<String>,
    pub settings: Option<String>,
    pub tmux_session_name: String,
    pub tmux_window_id: String,
    pub tmux_window_name: String,
    pub tmux_pane_id: String,
    pub cwd: String,
}

#[derive(Debug, Clone)]
pub struct McpRegistry {
    db_path: PathBuf,
}

pub struct SessionLock {
    registry: McpRegistry,
    session_id: String,
    owner_server_id: String,
    // Discriminates this acquisition from a later one of the same
    // (session_id, owner_server_id): if this lock expires and the same server
    // reacquires it, the stale handle must not refresh or drop the newer row.
    acquired_at_ms: i64,
}

impl Drop for SessionLock {
    fn drop(&mut self) {
        if let Err(error) =
            self.registry
                .release_lock(&self.session_id, &self.owner_server_id, self.acquired_at_ms)
        {
            eprintln!(
                "warning: failed to release MCP session lock for {}: {error}",
                self.session_id
            );
        }
    }
}

impl SessionLock {
    pub fn refresh(&self) -> AppResult<()> {
        self.registry
            .refresh_lock(&self.session_id, &self.owner_server_id, self.acquired_at_ms)
    }
}

impl McpRegistry {
    pub fn open(state_dir: &Path) -> AppResult<Self> {
        fs::create_dir_all(state_dir)?;
        let registry = Self {
            db_path: state_dir.join("mcp.sqlite3"),
        };
        registry.init()?;
        Ok(registry)
    }

    fn conn(&self) -> AppResult<Connection> {
        let conn = Connection::open(&self.db_path)?;
        conn.busy_timeout(Duration::from_millis(SQLITE_BUSY_TIMEOUT_MS))?;
        Ok(conn)
    }

    fn init(&self) -> AppResult<()> {
        let conn = self.conn()?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS mcp_sessions (
                id TEXT PRIMARY KEY,
                owner_server_id TEXT NOT NULL,
                provider TEXT NOT NULL DEFAULT 'claude',
                model TEXT,
                effort TEXT,
                agent TEXT,
                permission_mode TEXT,
                settings TEXT,
                tmux_session_name TEXT NOT NULL,
                tmux_window_id TEXT NOT NULL,
                tmux_window_name TEXT NOT NULL,
                tmux_pane_id TEXT NOT NULL,
                cwd TEXT NOT NULL,
                lifecycle_state TEXT NOT NULL,
                last_state TEXT,
                last_message_id TEXT,
                last_message_text TEXT,
                last_message_seen_at_ms INTEGER,
                created_at_ms INTEGER NOT NULL,
                updated_at_ms INTEGER NOT NULL,
                dead_at_ms INTEGER,
                killed_at_ms INTEGER,
                blocked_reason TEXT,
                blocked_snapshot TEXT,
                blocked_at_ms INTEGER,
                resurrected_at_ms INTEGER,
                resurrection_count INTEGER NOT NULL DEFAULT 0
            );
            CREATE TABLE IF NOT EXISTS mcp_session_locks (
                session_id TEXT PRIMARY KEY,
                owner_server_id TEXT NOT NULL,
                operation TEXT NOT NULL,
                acquired_at_ms INTEGER NOT NULL,
                expires_at_ms INTEGER NOT NULL
            );",
        )?;
        for stmt in [
            "ALTER TABLE mcp_sessions ADD COLUMN provider TEXT NOT NULL DEFAULT 'claude'",
            "ALTER TABLE mcp_sessions ADD COLUMN model TEXT",
            "ALTER TABLE mcp_sessions ADD COLUMN effort TEXT",
            "ALTER TABLE mcp_sessions ADD COLUMN agent TEXT",
            "ALTER TABLE mcp_sessions ADD COLUMN permission_mode TEXT",
            "ALTER TABLE mcp_sessions ADD COLUMN settings TEXT",
            "ALTER TABLE mcp_sessions ADD COLUMN blocked_reason TEXT",
            "ALTER TABLE mcp_sessions ADD COLUMN blocked_snapshot TEXT",
            "ALTER TABLE mcp_sessions ADD COLUMN blocked_at_ms INTEGER",
            "ALTER TABLE mcp_sessions ADD COLUMN resurrected_at_ms INTEGER",
            "ALTER TABLE mcp_sessions ADD COLUMN resurrection_count INTEGER NOT NULL DEFAULT 0",
        ] {
            match conn.execute(stmt, []) {
                Ok(_) => {}
                Err(rusqlite::Error::SqliteFailure(_, Some(msg)))
                    if msg.contains("duplicate column") => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }

    pub fn insert_session(&self, new: NewSessionRecord) -> AppResult<McpSessionRecord> {
        let now = now_ms()?;
        let mut last_error = None;
        for _ in 0..16 {
            let id = Uuid::new_v7(Timestamp::now(ContextV7::new())).to_string();
            let record = McpSessionRecord {
                id,
                owner_server_id: new.owner_server_id.clone(),
                provider: new.provider,
                model: new.model.clone(),
                effort: new.effort.clone(),
                agent: new.agent.clone(),
                permission_mode: new.permission_mode.clone(),
                settings: new.settings.clone(),
                tmux_session_name: new.tmux_session_name.clone(),
                tmux_window_id: new.tmux_window_id.clone(),
                tmux_window_name: new.tmux_window_name.clone(),
                tmux_pane_id: new.tmux_pane_id.clone(),
                cwd: new.cwd.clone(),
                lifecycle_state: LifecycleState::Starting,
                last_state: None,
                last_message_id: None,
                last_message_text: None,
                last_message_seen_at_ms: None,
                created_at_ms: now,
                updated_at_ms: now,
                dead_at_ms: None,
                killed_at_ms: None,
                blocked_reason: None,
                blocked_snapshot: None,
                blocked_at_ms: None,
                resurrected_at_ms: None,
                resurrection_count: 0,
            };
            match self.insert_record(&record) {
                Ok(()) => return Ok(record),
                Err(error) => last_error = Some(error),
            }
        }
        Err(last_error.unwrap_or_else(|| AppError::new("failed to create mcp session id")))
    }

    fn insert_record(&self, r: &McpSessionRecord) -> AppResult<()> {
        self.conn()?.execute(
            "INSERT INTO mcp_sessions (id, owner_server_id, provider, model, effort, agent, permission_mode, settings, tmux_session_name, tmux_window_id, tmux_window_name, tmux_pane_id, cwd, lifecycle_state, created_at_ms, updated_at_ms, resurrection_count)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)",
            params![r.id, r.owner_server_id, r.provider.as_str(), r.model, r.effort, r.agent, r.permission_mode, r.settings, r.tmux_session_name, r.tmux_window_id, r.tmux_window_name, r.tmux_pane_id, r.cwd, r.lifecycle_state.as_str(), r.created_at_ms, r.updated_at_ms, r.resurrection_count],
        )?;
        Ok(())
    }

    pub fn get(&self, id: &str) -> AppResult<Option<McpSessionRecord>> {
        self.conn()?.query_row(
            "SELECT id, owner_server_id, provider, model, effort, agent, permission_mode, settings, tmux_session_name, tmux_window_id, tmux_window_name, tmux_pane_id, cwd, lifecycle_state, last_state, last_message_id, last_message_text, last_message_seen_at_ms, created_at_ms, updated_at_ms, dead_at_ms, killed_at_ms, blocked_reason, blocked_snapshot, blocked_at_ms, resurrected_at_ms, resurrection_count FROM mcp_sessions WHERE id=?1",
            params![id],
            row_to_record,
        ).optional().map_err(AppError::from)
    }

    pub fn update_state(
        &self,
        id: &str,
        state: LifecycleState,
        last_state: Option<&str>,
    ) -> AppResult<()> {
        let now = now_ms()?;
        let (dead_at, killed_at): (Option<i64>, Option<i64>) = match state {
            LifecycleState::Dead => (Some(now), None),
            LifecycleState::Killed => (None, Some(now)),
            _ => (None, None),
        };
        self.conn()?.execute(
            "UPDATE mcp_sessions SET lifecycle_state=?2, last_state=COALESCE(?3,last_state), updated_at_ms=?4, dead_at_ms=COALESCE(?5, dead_at_ms), killed_at_ms=COALESCE(?6, killed_at_ms), blocked_reason=NULL, blocked_snapshot=NULL, blocked_at_ms=NULL WHERE id=?1",
            params![id, state.as_str(), last_state, now, dead_at, killed_at],
        )?;
        Ok(())
    }

    pub fn update_blocked(
        &self,
        id: &str,
        last_state: Option<&str>,
        reason: &str,
        snapshot: Option<&str>,
    ) -> AppResult<()> {
        let now = now_ms()?;
        self.conn()?.execute(
            "UPDATE mcp_sessions SET lifecycle_state='blocked', last_state=COALESCE(?2,last_state), blocked_reason=?3, blocked_snapshot=?4, blocked_at_ms=?5, updated_at_ms=?5 WHERE id=?1",
            params![id, last_state, reason, snapshot, now],
        )?;
        Ok(())
    }

    pub fn mark_cleanup_killed_preserving_blocked(&self, id: &str) -> AppResult<()> {
        let now = now_ms()?;
        self.conn()?.execute(
            "UPDATE mcp_sessions SET lifecycle_state='killed', updated_at_ms=?2, killed_at_ms=COALESCE(killed_at_ms, ?2) WHERE id=?1",
            params![id, now],
        )?;
        Ok(())
    }

    pub fn replace_tmux_identity_for_resurrection(
        &self,
        id: &str,
        session_name: &str,
        window_id: &str,
        window_name: &str,
        pane_id: &str,
        owner_server_id: &str,
    ) -> AppResult<()> {
        let now = now_ms()?;
        self.conn()?.execute(
            "UPDATE mcp_sessions SET owner_server_id=?7, tmux_session_name=?2, tmux_window_id=?3, tmux_window_name=?4, tmux_pane_id=?5, lifecycle_state='starting', last_state=NULL, dead_at_ms=NULL, killed_at_ms=NULL, blocked_reason=NULL, blocked_snapshot=NULL, blocked_at_ms=NULL, resurrected_at_ms=?6, resurrection_count=resurrection_count+1, updated_at_ms=?6 WHERE id=?1",
            params![id, session_name, window_id, window_name, pane_id, now, owner_server_id],
        )?;
        Ok(())
    }

    pub fn cleanup_candidates(
        &self,
        now_ms: i64,
        min_age_ms: i64,
        limit: usize,
    ) -> AppResult<Vec<McpSessionRecord>> {
        let cutoff = now_ms.saturating_sub(min_age_ms);
        let conn = self.conn()?;
        let mut stmt = conn.prepare(
            "SELECT id, owner_server_id, provider, model, effort, agent, permission_mode, settings, tmux_session_name, tmux_window_id, tmux_window_name, tmux_pane_id, cwd, lifecycle_state, last_state, last_message_id, last_message_text, last_message_seen_at_ms, created_at_ms, updated_at_ms, dead_at_ms, killed_at_ms, blocked_reason, blocked_snapshot, blocked_at_ms, resurrected_at_ms, resurrection_count
             FROM mcp_sessions
              WHERE id NOT IN (SELECT session_id FROM mcp_session_locks WHERE expires_at_ms>=?1)
                AND created_at_ms<=?2
                AND ((lifecycle_state='blocked' AND blocked_at_ms IS NOT NULL AND blocked_at_ms<=?2)
                  OR (lifecycle_state='starting' AND updated_at_ms<=?2))
              ORDER BY COALESCE(blocked_at_ms, updated_at_ms, created_at_ms) ASC
              LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![now_ms, cutoff, limit as i64], row_to_record)?;
        let mut records = Vec::new();
        for row in rows {
            records.push(row?);
        }
        Ok(records)
    }

    pub fn is_cleanup_candidate(record: &McpSessionRecord, now_ms: i64, min_age_ms: i64) -> bool {
        let cutoff = now_ms.saturating_sub(min_age_ms);
        if record.created_at_ms > cutoff {
            return false;
        }
        match record.lifecycle_state {
            LifecycleState::Blocked => record.blocked_at_ms.is_some_and(|at| at <= cutoff),
            LifecycleState::Starting => record.updated_at_ms <= cutoff,
            _ => false,
        }
    }

    pub fn update_cursor(&self, id: &str, msg_id: Option<&str>, text: &str) -> AppResult<()> {
        let now = now_ms()?;
        self.conn()?.execute(
            "UPDATE mcp_sessions SET last_message_id=?2, last_message_text=?3, last_message_seen_at_ms=?4, updated_at_ms=?4 WHERE id=?1",
            params![id, msg_id, text, now],
        )?;
        Ok(())
    }

    pub fn acquire_lock(
        &self,
        session_id: &str,
        owner_server_id: &str,
        operation: &str,
    ) -> AppResult<Option<SessionLock>> {
        let now = now_ms()?;
        let expires = now + LOCK_TTL_MS;
        let conn = self.conn()?;
        conn.execute(
            "DELETE FROM mcp_session_locks WHERE session_id=?1 AND expires_at_ms<?2",
            params![session_id, now],
        )?;
        let inserted = conn.execute(
            "INSERT OR IGNORE INTO mcp_session_locks (session_id, owner_server_id, operation, acquired_at_ms, expires_at_ms) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![session_id, owner_server_id, operation, now, expires],
        )?;
        if inserted == 0 {
            Ok(None)
        } else {
            Ok(Some(SessionLock {
                registry: self.clone(),
                session_id: session_id.to_string(),
                owner_server_id: owner_server_id.to_string(),
                acquired_at_ms: now,
            }))
        }
    }

    fn release_lock(
        &self,
        session_id: &str,
        owner_server_id: &str,
        acquired_at_ms: i64,
    ) -> AppResult<()> {
        self.conn()?.execute(
            "DELETE FROM mcp_session_locks WHERE session_id=?1 AND owner_server_id=?2 AND acquired_at_ms=?3",
            params![session_id, owner_server_id, acquired_at_ms],
        )?;
        Ok(())
    }

    fn refresh_lock(
        &self,
        session_id: &str,
        owner_server_id: &str,
        acquired_at_ms: i64,
    ) -> AppResult<()> {
        let now = now_ms()?;
        let expires = now + LOCK_TTL_MS;
        let updated = self.conn()?.execute(
            "UPDATE mcp_session_locks SET expires_at_ms=?4 WHERE session_id=?1 AND owner_server_id=?2 AND acquired_at_ms=?3",
            params![session_id, owner_server_id, acquired_at_ms, expires],
        )?;
        if updated == 0 {
            return Err(AppError::new("mcp session lock is no longer held"));
        }
        Ok(())
    }
}

fn row_to_record(row: &rusqlite::Row<'_>) -> rusqlite::Result<McpSessionRecord> {
    let provider_str: String = row.get(2)?;
    let provider = Provider::from_db(&provider_str).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(2, Type::Text, Box::new(error))
    })?;
    let state: String = row.get(13)?;
    let lifecycle_state = LifecycleState::from_str(&state).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(13, Type::Text, Box::new(error))
    })?;
    Ok(McpSessionRecord {
        id: row.get(0)?,
        owner_server_id: row.get(1)?,
        provider,
        model: row.get(3)?,
        effort: row.get(4)?,
        agent: row.get(5)?,
        permission_mode: row.get(6)?,
        settings: row.get(7)?,
        tmux_session_name: row.get(8)?,
        tmux_window_id: row.get(9)?,
        tmux_window_name: row.get(10)?,
        tmux_pane_id: row.get(11)?,
        cwd: row.get(12)?,
        lifecycle_state,
        last_state: row.get(14)?,
        last_message_id: row.get(15)?,
        last_message_text: row.get(16)?,
        last_message_seen_at_ms: row.get(17)?,
        created_at_ms: row.get(18)?,
        updated_at_ms: row.get(19)?,
        dead_at_ms: row.get(20)?,
        killed_at_ms: row.get(21)?,
        blocked_reason: row.get(22)?,
        blocked_snapshot: row.get(23)?,
        blocked_at_ms: row.get(24)?,
        resurrected_at_ms: row.get(25)?,
        resurrection_count: row.get(26)?,
    })
}

pub fn now_ms() -> AppResult<i64> {
    Ok(std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|error| AppError::new(format!("system clock before unix epoch: {error}")))?
        .as_millis() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_unique_session_ids_and_locks() {
        let root = std::env::temp_dir().join(format!("botctl-mcp-reg-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let registry = McpRegistry::open(&root).unwrap();
        let first = registry.insert_session(fake_new("%1", "@1")).unwrap();
        let second = registry.insert_session(fake_new("%2", "@2")).unwrap();
        assert_ne!(first.id, second.id);
        let lock = registry
            .acquire_lock(&first.id, "server-a", "prompt")
            .unwrap();
        assert!(lock.is_some());
        let busy = registry
            .acquire_lock(&first.id, "server-b", "prompt")
            .unwrap();
        assert!(busy.is_none());
        drop(lock);
        assert!(
            registry
                .acquire_lock(&first.id, "server-b", "prompt")
                .unwrap()
                .is_some()
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn stale_lock_handle_cannot_refresh_or_release_reacquired_row() {
        let root =
            std::env::temp_dir().join(format!("botctl-mcp-reg-stale-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let registry = McpRegistry::open(&root).unwrap();
        let record = registry.insert_session(fake_new("%1", "@1")).unwrap();

        let stale = registry
            .acquire_lock(&record.id, "server-a", "prompt")
            .unwrap()
            .expect("first acquisition should succeed");

        // Simulate the same server reacquiring after expiry: the live row now
        // carries a newer acquisition token than the stale handle holds.
        let newer_token = stale.acquired_at_ms + 5_000;
        registry
            .conn()
            .unwrap()
            .execute(
                "UPDATE mcp_session_locks SET acquired_at_ms=?2 WHERE session_id=?1",
                params![record.id, newer_token],
            )
            .unwrap();

        // The stale handle must not refresh the newer acquisition...
        assert!(stale.refresh().is_err());

        // ...nor delete its row on Drop.
        drop(stale);
        let busy = registry
            .acquire_lock(&record.id, "server-b", "prompt")
            .unwrap();
        assert!(
            busy.is_none(),
            "newer lock row must survive stale handle drop"
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn rejects_unknown_lifecycle_state() {
        let root =
            std::env::temp_dir().join(format!("botctl-mcp-reg-bad-state-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let registry = McpRegistry::open(&root).unwrap();
        let record = registry.insert_session(fake_new("%1", "@1")).unwrap();
        registry
            .conn()
            .unwrap()
            .execute(
                "UPDATE mcp_sessions SET lifecycle_state='mystery' WHERE id=?1",
                params![record.id],
            )
            .unwrap();
        let error = registry.get(&record.id).expect_err("bad state should fail");
        assert!(error.to_string().contains("invalid mcp lifecycle_state"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn old_schema_migrates_lifecycle_columns() {
        let root =
            std::env::temp_dir().join(format!("botctl-mcp-reg-migrate-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let db = root.join("mcp.sqlite3");
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE mcp_sessions (
                id TEXT PRIMARY KEY,
                owner_server_id TEXT NOT NULL,
                provider TEXT NOT NULL DEFAULT 'claude',
                model TEXT,
                effort TEXT,
                agent TEXT,
                tmux_session_name TEXT NOT NULL,
                tmux_window_id TEXT NOT NULL,
                tmux_window_name TEXT NOT NULL,
                tmux_pane_id TEXT NOT NULL,
                cwd TEXT NOT NULL,
                lifecycle_state TEXT NOT NULL,
                last_state TEXT,
                last_message_id TEXT,
                last_message_text TEXT,
                last_message_seen_at_ms INTEGER,
                created_at_ms INTEGER NOT NULL,
                updated_at_ms INTEGER NOT NULL,
                dead_at_ms INTEGER,
                killed_at_ms INTEGER
            );",
        )
        .unwrap();
        drop(conn);
        let registry = McpRegistry::open(&root).unwrap();
        let record = registry.insert_session(fake_new("%1", "@1")).unwrap();
        assert_eq!(record.permission_mode, None);
        assert_eq!(record.settings, None);
        assert_eq!(record.blocked_reason, None);
        assert_eq!(record.resurrection_count, 0);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn blocked_fields_persist_and_clear() {
        let root =
            std::env::temp_dir().join(format!("botctl-mcp-reg-blocked-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let registry = McpRegistry::open(&root).unwrap();
        let record = registry.insert_session(fake_new("%1", "@1")).unwrap();

        registry
            .update_blocked(
                &record.id,
                Some("StartupChoicePrompt"),
                "startup_choice_prompt",
                Some("bounded excerpt"),
            )
            .unwrap();
        let blocked = registry.get(&record.id).unwrap().unwrap();
        assert_eq!(blocked.lifecycle_state, LifecycleState::Blocked);
        assert_eq!(
            blocked.blocked_reason.as_deref(),
            Some("startup_choice_prompt")
        );
        assert_eq!(blocked.blocked_snapshot.as_deref(), Some("bounded excerpt"));
        assert!(blocked.blocked_at_ms.is_some());

        registry
            .update_state(&record.id, LifecycleState::Ready, Some("ChatReady"))
            .unwrap();
        let ready = registry.get(&record.id).unwrap().unwrap();
        assert_eq!(ready.blocked_reason, None);
        assert_eq!(ready.blocked_snapshot, None);
        assert_eq!(ready.blocked_at_ms, None);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn resurrection_replaces_identity_and_increments_count() {
        let root =
            std::env::temp_dir().join(format!("botctl-mcp-reg-resurrect-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let registry = McpRegistry::open(&root).unwrap();
        let record = registry.insert_session(fake_new("%1", "@1")).unwrap();
        registry
            .update_blocked(&record.id, Some("DiffDialog"), "diff_dialog", Some("diff"))
            .unwrap();
        registry
            .update_state(&record.id, LifecycleState::Killed, None)
            .unwrap();

        registry
            .replace_tmux_identity_for_resurrection(
                &record.id,
                "botctl-mcp",
                "@9",
                "mcp-new",
                "%9",
                "server-b",
            )
            .unwrap();
        let updated = registry.get(&record.id).unwrap().unwrap();
        assert_eq!(updated.id, record.id);
        assert_eq!(updated.owner_server_id, "server-b");
        assert_eq!(updated.tmux_window_id, "@9");
        assert_eq!(updated.tmux_pane_id, "%9");
        assert_eq!(updated.lifecycle_state, LifecycleState::Starting);
        assert_eq!(updated.killed_at_ms, None);
        assert_eq!(updated.blocked_reason, None);
        assert_eq!(updated.resurrection_count, 1);
        assert!(updated.resurrected_at_ms.is_some());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn cleanup_candidates_are_aged_unlocked_blocked_or_starting_only() {
        let root =
            std::env::temp_dir().join(format!("botctl-mcp-reg-cleanup-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let registry = McpRegistry::open(&root).unwrap();
        let blocked = registry.insert_session(fake_new("%1", "@1")).unwrap();
        let ready = registry.insert_session(fake_new("%2", "@2")).unwrap();
        let locked = registry.insert_session(fake_new("%3", "@3")).unwrap();
        let starting = registry.insert_session(fake_new("%4", "@4")).unwrap();
        registry
            .update_blocked(&blocked.id, Some("DiffDialog"), "diff_dialog", Some("diff"))
            .unwrap();
        registry
            .update_state(&ready.id, LifecycleState::Ready, Some("ChatReady"))
            .unwrap();
        registry
            .update_blocked(&locked.id, Some("DiffDialog"), "diff_dialog", Some("diff"))
            .unwrap();
        let lock = registry
            .acquire_lock(&locked.id, "server-a", "prompt")
            .unwrap();
        let now = now_ms().unwrap();
        registry.conn().unwrap().execute(
            "UPDATE mcp_sessions SET created_at_ms=?2, updated_at_ms=?2, blocked_at_ms=?2 WHERE id IN (?1, ?3)",
            params![blocked.id, now - 60_000, locked.id],
        ).unwrap();
        registry
            .conn()
            .unwrap()
            .execute(
                "UPDATE mcp_sessions SET created_at_ms=?2, updated_at_ms=?2 WHERE id=?1",
                params![starting.id, now - 60_000],
            )
            .unwrap();

        let ids = registry
            .cleanup_candidates(now, 30_000, 100)
            .unwrap()
            .into_iter()
            .map(|r| r.id)
            .collect::<Vec<_>>();
        assert!(ids.contains(&blocked.id));
        assert!(ids.contains(&starting.id));
        assert!(!ids.contains(&ready.id));
        assert!(!ids.contains(&locked.id));
        drop(lock);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn cleanup_kill_preserves_blocked_evidence() {
        let root = std::env::temp_dir().join(format!(
            "botctl-mcp-reg-cleanup-kill-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        let registry = McpRegistry::open(&root).unwrap();
        let record = registry.insert_session(fake_new("%1", "@1")).unwrap();
        registry
            .update_blocked(&record.id, Some("DiffDialog"), "diff_dialog", Some("diff"))
            .unwrap();

        registry
            .mark_cleanup_killed_preserving_blocked(&record.id)
            .unwrap();

        let killed = registry.get(&record.id).unwrap().unwrap();
        assert_eq!(killed.lifecycle_state, LifecycleState::Killed);
        assert!(killed.killed_at_ms.is_some());
        assert_eq!(killed.blocked_reason.as_deref(), Some("diff_dialog"));
        assert_eq!(killed.blocked_snapshot.as_deref(), Some("diff"));
        assert!(killed.blocked_at_ms.is_some());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn cleanup_candidate_recheck_respects_current_state_and_age() {
        let mut record = McpSessionRecord {
            id: "id".into(),
            owner_server_id: "server".into(),
            provider: Provider::Claude,
            model: None,
            effort: None,
            agent: None,
            permission_mode: None,
            settings: None,
            tmux_session_name: "botctl".into(),
            tmux_window_id: "@1".into(),
            tmux_window_name: "mcp".into(),
            tmux_pane_id: "%1".into(),
            cwd: "/tmp".into(),
            lifecycle_state: LifecycleState::Blocked,
            last_state: None,
            last_message_id: None,
            last_message_text: None,
            last_message_seen_at_ms: None,
            created_at_ms: 1_000,
            updated_at_ms: 1_000,
            dead_at_ms: None,
            killed_at_ms: None,
            blocked_reason: Some("unknown_state".into()),
            blocked_snapshot: None,
            blocked_at_ms: Some(1_000),
            resurrected_at_ms: None,
            resurrection_count: 0,
        };

        assert!(McpRegistry::is_cleanup_candidate(&record, 61_000, 30_000));
        record.created_at_ms = 60_000;
        assert!(!McpRegistry::is_cleanup_candidate(&record, 61_000, 30_000));
        record.created_at_ms = 1_000;
        record.blocked_at_ms = Some(60_000);
        assert!(!McpRegistry::is_cleanup_candidate(&record, 61_000, 30_000));
        record.lifecycle_state = LifecycleState::Ready;
        record.blocked_at_ms = Some(1_000);
        assert!(!McpRegistry::is_cleanup_candidate(&record, 61_000, 30_000));
    }

    fn fake_new(pane: &str, window: &str) -> NewSessionRecord {
        NewSessionRecord {
            owner_server_id: "server-a".into(),
            provider: Provider::Claude,
            model: None,
            effort: None,
            agent: None,
            permission_mode: None,
            settings: None,
            tmux_session_name: "botctl".into(),
            tmux_window_id: window.into(),
            tmux_window_name: "mcp".into(),
            tmux_pane_id: pane.into(),
            cwd: "/tmp".into(),
        }
    }
}
