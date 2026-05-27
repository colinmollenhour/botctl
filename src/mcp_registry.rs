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
    Opencode,
    Pi,
}

impl Provider {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Opencode => "opencode",
            Self::Pi => "pi",
        }
    }

    pub fn command(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Opencode => "opencode",
            Self::Pi => "pi",
        }
    }

    pub fn parse(value: &str) -> AppResult<Self> {
        match value {
            "claude" => Ok(Self::Claude),
            "codex" => Ok(Self::Codex),
            "opencode" => Ok(Self::Opencode),
            "pi" => Ok(Self::Pi),
            other => Err(AppError::new(format!(
                "invalid_params: unknown provider {other}"
            ))),
        }
    }

    fn from_db(value: &str) -> AppResult<Self> {
        Self::parse(value).map_err(|_| {
            AppError::new(format!("invalid mcp provider in database: {value}"))
        })
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
}

#[derive(Debug, Clone)]
pub struct NewSessionRecord {
    pub owner_server_id: String,
    pub provider: Provider,
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
}

impl Drop for SessionLock {
    fn drop(&mut self) {
        if let Err(error) = self
            .registry
            .release_lock(&self.session_id, &self.owner_server_id)
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
            .refresh_lock(&self.session_id, &self.owner_server_id)
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
            );
            CREATE TABLE IF NOT EXISTS mcp_session_locks (
                session_id TEXT PRIMARY KEY,
                owner_server_id TEXT NOT NULL,
                operation TEXT NOT NULL,
                acquired_at_ms INTEGER NOT NULL,
                expires_at_ms INTEGER NOT NULL
            );",
        )?;
        // Idempotent migration for DBs created before the provider column existed.
        match conn.execute(
            "ALTER TABLE mcp_sessions ADD COLUMN provider TEXT NOT NULL DEFAULT 'claude'",
            [],
        ) {
            Ok(_) => {}
            Err(rusqlite::Error::SqliteFailure(_, Some(msg))) if msg.contains("duplicate column") => {}
            Err(error) => return Err(error.into()),
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
            "INSERT INTO mcp_sessions (id, owner_server_id, provider, tmux_session_name, tmux_window_id, tmux_window_name, tmux_pane_id, cwd, lifecycle_state, created_at_ms, updated_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![r.id, r.owner_server_id, r.provider.as_str(), r.tmux_session_name, r.tmux_window_id, r.tmux_window_name, r.tmux_pane_id, r.cwd, r.lifecycle_state.as_str(), r.created_at_ms, r.updated_at_ms],
        )?;
        Ok(())
    }

    pub fn get(&self, id: &str) -> AppResult<Option<McpSessionRecord>> {
        self.conn()?.query_row(
            "SELECT id, owner_server_id, provider, tmux_session_name, tmux_window_id, tmux_window_name, tmux_pane_id, cwd, lifecycle_state, last_state, last_message_id, last_message_text, last_message_seen_at_ms, created_at_ms, updated_at_ms, dead_at_ms, killed_at_ms FROM mcp_sessions WHERE id=?1",
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
            "UPDATE mcp_sessions SET lifecycle_state=?2, last_state=COALESCE(?3,last_state), updated_at_ms=?4, dead_at_ms=COALESCE(?5, dead_at_ms), killed_at_ms=COALESCE(?6, killed_at_ms) WHERE id=?1",
            params![id, state.as_str(), last_state, now, dead_at, killed_at],
        )?;
        Ok(())
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
            }))
        }
    }

    fn release_lock(&self, session_id: &str, owner_server_id: &str) -> AppResult<()> {
        self.conn()?.execute(
            "DELETE FROM mcp_session_locks WHERE session_id=?1 AND owner_server_id=?2",
            params![session_id, owner_server_id],
        )?;
        Ok(())
    }

    fn refresh_lock(&self, session_id: &str, owner_server_id: &str) -> AppResult<()> {
        let now = now_ms()?;
        let expires = now + LOCK_TTL_MS;
        let updated = self.conn()?.execute(
            "UPDATE mcp_session_locks SET expires_at_ms=?3 WHERE session_id=?1 AND owner_server_id=?2",
            params![session_id, owner_server_id, expires],
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
    let state: String = row.get(8)?;
    let lifecycle_state = LifecycleState::from_str(&state).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(8, Type::Text, Box::new(error))
    })?;
    Ok(McpSessionRecord {
        id: row.get(0)?,
        owner_server_id: row.get(1)?,
        provider,
        tmux_session_name: row.get(3)?,
        tmux_window_id: row.get(4)?,
        tmux_window_name: row.get(5)?,
        tmux_pane_id: row.get(6)?,
        cwd: row.get(7)?,
        lifecycle_state,
        last_state: row.get(9)?,
        last_message_id: row.get(10)?,
        last_message_text: row.get(11)?,
        last_message_seen_at_ms: row.get(12)?,
        created_at_ms: row.get(13)?,
        updated_at_ms: row.get(14)?,
        dead_at_ms: row.get(15)?,
        killed_at_ms: row.get(16)?,
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

    fn fake_new(pane: &str, window: &str) -> NewSessionRecord {
        NewSessionRecord {
            owner_server_id: "server-a".into(),
            provider: Provider::Claude,
            tmux_session_name: "botctl".into(),
            tmux_window_id: window.into(),
            tmux_window_name: "mcp".into(),
            tmux_pane_id: pane.into(),
            cwd: "/tmp".into(),
        }
    }
}
