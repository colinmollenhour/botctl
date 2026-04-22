use std::fs;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;
use uuid::Uuid;

use crate::app::{AppError, AppResult};
use crate::tmux::TmuxPane;
use crate::workspace::resolve_workspace_locator;

pub const CURRENT_SCHEMA_VERSION: i64 = 3;
const SCHEMA_VERSION_ROW_ID: i64 = 1;
const STATE_DB_FILENAME: &str = "state.db";
const ARTIFACTS_DIR: &str = "artifacts";
const ARTIFACTS_CAPTURES_SUBDIR: &str = "captures";
const ARTIFACTS_TAPES_SUBDIR: &str = "tapes";
const ARTIFACTS_EXPORTS_SUBDIR: &str = "exports";
const STATE_DB_BUSY_TIMEOUT_MS: u64 = 5_000;
const PLACEHOLDER_INSTANCE_KIND: &str = "prompt-placeholder";
const TMUX_INSTANCE_KIND: &str = "tmux-pane";
const LEGACY_WORKSPACE_ROOT: &str = "/__botctl_legacy_global_workspace__";
const CLAUDE_PROJECTS_DIR: &str = ".claude/projects";
const CLAUDE_SESSION_REVALIDATE_MS: i64 = 10_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceRecord {
    pub id: String,
    pub workspace_root: String,
    pub repo_key: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceRecord {
    pub id: String,
    pub workspace_id: String,
    pub session_name: String,
    pub pane_id: Option<String>,
    pub pane_tty: Option<String>,
    pub pane_pid: Option<u32>,
    pub session_id: Option<String>,
    pub window_id: Option<String>,
    pub window_name: Option<String>,
    pub current_command: Option<String>,
    pub current_path: Option<String>,
    pub kind: String,
    pub active: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BabysitRegistrationRecord {
    pub instance: InstanceRecord,
    pub enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceRuntimeState {
    pub instance_id: String,
    pub last_state: Option<String>,
    pub wait_started_at_unix_ms: Option<i64>,
    pub claude_session_id: Option<String>,
    pub claude_session_checked_at_unix_ms: Option<i64>,
}

pub fn state_db_path(state_dir: &Path) -> PathBuf {
    state_dir.join(STATE_DB_FILENAME)
}

pub fn runtime_artifacts_root(state_dir: &Path) -> PathBuf {
    state_dir.join(ARTIFACTS_DIR)
}

pub fn capture_artifact_path(
    state_dir: &Path,
    artifact_id: &str,
    file_name: &str,
) -> AppResult<PathBuf> {
    artifact_path(state_dir, ARTIFACTS_CAPTURES_SUBDIR, artifact_id, file_name)
}

pub fn tape_artifact_path(
    state_dir: &Path,
    artifact_id: &str,
    file_name: &str,
) -> AppResult<PathBuf> {
    artifact_path(state_dir, ARTIFACTS_TAPES_SUBDIR, artifact_id, file_name)
}

pub fn export_artifact_path(
    state_dir: &Path,
    artifact_id: &str,
    file_name: &str,
) -> AppResult<PathBuf> {
    artifact_path(state_dir, ARTIFACTS_EXPORTS_SUBDIR, artifact_id, file_name)
}

pub fn open_state_db(state_dir: &Path) -> AppResult<Connection> {
    let db_path = state_db_path(state_dir);
    if let Some(parent) = db_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let connection = Connection::open(&db_path)?;
    configure_connection(&connection)?;
    Ok(connection)
}

fn configure_connection(connection: &Connection) -> AppResult<()> {
    connection.busy_timeout(Duration::from_millis(STATE_DB_BUSY_TIMEOUT_MS))?;
    connection.pragma_update(None, "journal_mode", "WAL")?;
    let journal_mode: String = connection.query_row("PRAGMA journal_mode", [], |row| row.get(0))?;
    if !journal_mode.eq_ignore_ascii_case("wal") {
        return Err(AppError::new(format!(
            "failed to enable WAL journal mode for state.db: SQLite reported {journal_mode}"
        )));
    }
    connection.pragma_update(None, "foreign_keys", 1)?;
    Ok(())
}

pub fn ensure_schema_version_table(connection: &Connection) -> AppResult<()> {
    connection.execute(
        "CREATE TABLE IF NOT EXISTS schema_version (\
            id INTEGER PRIMARY KEY CHECK (id = 1), \
            version INTEGER NOT NULL\
        )",
        [],
    )?;
    Ok(())
}

fn read_schema_version(connection: &Connection) -> AppResult<Option<i64>> {
    Ok(connection
        .query_row(
            "SELECT version FROM schema_version WHERE id = ?1",
            params![SCHEMA_VERSION_ROW_ID],
            |row| row.get::<_, i64>(0),
        )
        .optional()?)
}

fn write_schema_version(connection: &Connection, version: i64) -> AppResult<()> {
    connection.execute(
        "INSERT INTO schema_version (id, version) VALUES (?1, ?2) \
         ON CONFLICT(id) DO UPDATE SET version = excluded.version",
        params![SCHEMA_VERSION_ROW_ID, version],
    )?;
    Ok(())
}

pub fn migrate_state_db(connection: &mut Connection) -> AppResult<()> {
    let tx = connection.transaction()?;
    ensure_schema_version_table(&tx)?;
    let mut version = read_schema_version(&tx)?.unwrap_or(0);
    let starting_version = version;

    if version > CURRENT_SCHEMA_VERSION {
        return Err(AppError::new(format!(
            "unsupported state.db schema version: expected <= {}, found {}",
            CURRENT_SCHEMA_VERSION, version
        )));
    }

        while version < CURRENT_SCHEMA_VERSION {
        match version {
            0 => migrate_to_v1(&tx)?,
            1 => migrate_to_v2(&tx)?,
            2 => migrate_to_v3(&tx)?,
            other => {
                return Err(AppError::new(format!(
                    "no state.db migration path from version {} to {}",
                    other, CURRENT_SCHEMA_VERSION
                )));
            }
        }

        version += 1;
        write_schema_version(&tx, version)?;
    }

    if starting_version == version {
        ensure_schema_layout_for_version(&tx, version)?;
    }

    tx.commit()?;
    Ok(())
}

fn ensure_schema_layout_for_version(connection: &Connection, version: i64) -> AppResult<()> {
    match version {
        0 => Ok(()),
        1 => migrate_to_v1(connection),
        2 => ensure_v2_tables(connection),
        3 => ensure_v3_tables(connection),
        other => Err(AppError::new(format!(
            "no state.db migration path from version {} to {}",
            other, CURRENT_SCHEMA_VERSION
        ))),
    }
}

fn schema_layout_matches_version(connection: &Connection, version: i64) -> AppResult<bool> {
    match version {
        0 => Ok(true),
        1 => Ok(table_exists(connection, "pending_prompts")?
            && table_exists(connection, "babysit_registrations")?
            && !table_exists(connection, "workspaces")?),
        2 => Ok(table_exists(connection, "workspaces")?
            && table_exists(connection, "instances")?
            && table_exists(connection, "pending_prompts")?
            && table_exists(connection, "babysit_registrations")?
            && table_exists(connection, "instance_runtime_state")?),
        3 => Ok(table_exists(connection, "workspaces")?
            && table_exists(connection, "instances")?
            && table_exists(connection, "pending_prompts")?
            && table_exists(connection, "babysit_registrations")?
            && table_exists(connection, "instance_runtime_state")?
            && table_has_column(connection, "instance_runtime_state", "claude_session_id")?
            && table_has_column(
                connection,
                "instance_runtime_state",
                "claude_session_checked_at_unix_ms",
            )?),
        other => Err(AppError::new(format!(
            "no state.db migration path from version {} to {}",
            other, CURRENT_SCHEMA_VERSION
        ))),
    }
}

fn table_exists(connection: &Connection, table_name: &str) -> AppResult<bool> {
    Ok(connection
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1",
            params![table_name],
            |_row| Ok(()),
        )
        .optional()?
        .is_some())
}

fn migrate_to_v1(connection: &Connection) -> AppResult<()> {
    connection.execute(
        "CREATE TABLE IF NOT EXISTS pending_prompts (\
            session_name TEXT PRIMARY KEY, \
            content TEXT NOT NULL\
        )",
        [],
    )?;
    connection.execute(
        "CREATE TABLE IF NOT EXISTS babysit_registrations (\
            pane_id TEXT PRIMARY KEY, \
            enabled INTEGER NOT NULL CHECK (enabled IN (0, 1)), \
            pane_tty TEXT NOT NULL, \
            pane_pid INTEGER, \
            session_id TEXT NOT NULL, \
            session_name TEXT NOT NULL, \
            window_id TEXT NOT NULL, \
            window_name TEXT NOT NULL, \
            current_command TEXT NOT NULL, \
            current_path TEXT NOT NULL\
        )",
        [],
    )?;
    Ok(())
}

fn migrate_to_v2(connection: &Connection) -> AppResult<()> {
    if table_exists(connection, "workspaces")?
        && table_exists(connection, "instances")?
        && table_has_column(connection, "pending_prompts", "instance_id")?
        && table_has_column(connection, "babysit_registrations", "instance_id")?
    {
        return Ok(());
    }

    connection.execute(
        "CREATE TABLE IF NOT EXISTS workspaces (\
            id TEXT PRIMARY KEY, \
            workspace_root TEXT NOT NULL UNIQUE, \
            repo_key TEXT\
        )",
        [],
    )?;
    connection.execute(
        "CREATE TABLE IF NOT EXISTS instances (\
            id TEXT PRIMARY KEY, \
            workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE, \
            session_name TEXT NOT NULL, \
            pane_id TEXT UNIQUE, \
            pane_tty TEXT, \
            pane_pid INTEGER, \
            session_id TEXT, \
            window_id TEXT, \
            window_name TEXT, \
            current_command TEXT, \
            current_path TEXT, \
            kind TEXT NOT NULL, \
            active INTEGER NOT NULL CHECK (active IN (0, 1))\
        )",
        [],
    )?;
    connection.execute(
        "CREATE UNIQUE INDEX IF NOT EXISTS instances_placeholder_session_unique \
         ON instances (workspace_id, session_name, kind) \
         WHERE kind = 'prompt-placeholder' AND active = 1",
        [],
    )?;

    if table_exists(connection, "babysit_registrations_v1")? {
        connection.execute("DROP TABLE babysit_registrations_v1", [])?;
    }
    if table_exists(connection, "pending_prompts_v1")? {
        connection.execute("DROP TABLE pending_prompts_v1", [])?;
    }

    let legacy_workspace_id = ensure_legacy_workspace(connection)?;

    connection.execute(
        "ALTER TABLE pending_prompts RENAME TO pending_prompts_v1",
        [],
    )?;
    connection.execute(
        "ALTER TABLE babysit_registrations RENAME TO babysit_registrations_v1",
        [],
    )?;

    ensure_v2_prompt_and_babysit_tables(connection)?;

    let mut pending_statement = connection
        .prepare("SELECT session_name, content FROM pending_prompts_v1 ORDER BY session_name")?;
    let pending_rows = pending_statement.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    for row in pending_rows {
        let (session_name, content) = row?;
        let instance = find_or_create_placeholder_instance_with_connection(
            connection,
            &legacy_workspace_id,
            &session_name,
        )?;
        connection.execute(
            "INSERT INTO pending_prompts (instance_id, content) VALUES (?1, ?2)",
            params![instance.id, content],
        )?;
    }

    let mut babysit_statement = connection.prepare(
        "SELECT enabled, pane_id, pane_tty, pane_pid, session_id, session_name, \
            window_id, window_name, current_command, current_path \
         FROM babysit_registrations_v1 ORDER BY pane_id",
    )?;
    let babysit_rows = babysit_statement.query_map([], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, Option<u32>>(3)?,
            row.get::<_, String>(4)?,
            row.get::<_, String>(5)?,
            row.get::<_, String>(6)?,
            row.get::<_, String>(7)?,
            row.get::<_, String>(8)?,
            row.get::<_, String>(9)?,
        ))
    })?;
    for row in babysit_rows {
        let (
            enabled,
            pane_id,
            pane_tty,
            pane_pid,
            session_id,
            session_name,
            window_id,
            window_name,
            current_command,
            current_path,
        ) = row?;

        let instance_id = new_uuid();
        connection.execute(
            "INSERT INTO instances (\
                id, workspace_id, session_name, pane_id, pane_tty, pane_pid, session_id, \
                window_id, window_name, current_command, current_path, kind, active\
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, 1)",
            params![
                instance_id,
                legacy_workspace_id,
                session_name,
                pane_id,
                pane_tty,
                pane_pid,
                session_id,
                window_id,
                window_name,
                current_command,
                current_path,
                TMUX_INSTANCE_KIND,
            ],
        )?;
        connection.execute(
            "INSERT INTO babysit_registrations (instance_id, enabled) VALUES (?1, ?2)",
            params![instance_id, enabled],
        )?;
    }

    connection.execute("DROP TABLE pending_prompts_v1", [])?;
    connection.execute("DROP TABLE babysit_registrations_v1", [])?;
    Ok(())
}

fn ensure_v2_tables(connection: &Connection) -> AppResult<()> {
    connection.execute(
        "CREATE TABLE IF NOT EXISTS workspaces (\
            id TEXT PRIMARY KEY, \
            workspace_root TEXT NOT NULL UNIQUE, \
            repo_key TEXT\
        )",
        [],
    )?;
    connection.execute(
        "CREATE TABLE IF NOT EXISTS instances (\
            id TEXT PRIMARY KEY, \
            workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE, \
            session_name TEXT NOT NULL, \
            pane_id TEXT UNIQUE, \
            pane_tty TEXT, \
            pane_pid INTEGER, \
            session_id TEXT, \
            window_id TEXT, \
            window_name TEXT, \
            current_command TEXT, \
            current_path TEXT, \
            kind TEXT NOT NULL, \
            active INTEGER NOT NULL CHECK (active IN (0, 1))\
        )",
        [],
    )?;
    connection.execute(
        "CREATE UNIQUE INDEX IF NOT EXISTS instances_placeholder_session_unique \
         ON instances (workspace_id, session_name, kind) \
         WHERE kind = 'prompt-placeholder' AND active = 1",
        [],
    )?;
    ensure_v2_prompt_and_babysit_tables(connection)?;
    Ok(())
}

fn migrate_to_v3(connection: &Connection) -> AppResult<()> {
    ensure_v2_tables(connection)?;
    ensure_v3_tables(connection)
}

fn ensure_v2_prompt_and_babysit_tables(connection: &Connection) -> AppResult<()> {
    connection.execute(
        "CREATE TABLE IF NOT EXISTS pending_prompts (\
            instance_id TEXT PRIMARY KEY REFERENCES instances(id) ON DELETE CASCADE, \
            content TEXT NOT NULL\
        )",
        [],
    )?;
    connection.execute(
        "CREATE TABLE IF NOT EXISTS babysit_registrations (\
            instance_id TEXT PRIMARY KEY REFERENCES instances(id) ON DELETE CASCADE, \
            enabled INTEGER NOT NULL CHECK (enabled IN (0, 1))\
        )",
        [],
    )?;
    connection.execute(
        "CREATE TABLE IF NOT EXISTS instance_runtime_state (\
            instance_id TEXT PRIMARY KEY REFERENCES instances(id) ON DELETE CASCADE, \
            last_state TEXT, \
            wait_started_at_unix_ms INTEGER\
        )",
        [],
    )?;
    Ok(())
}

fn ensure_v3_tables(connection: &Connection) -> AppResult<()> {
    ensure_v2_tables(connection)?;
    if !table_has_column(connection, "instance_runtime_state", "claude_session_id")? {
        connection.execute(
            "ALTER TABLE instance_runtime_state ADD COLUMN claude_session_id TEXT",
            [],
        )?;
    }
    if !table_has_column(
        connection,
        "instance_runtime_state",
        "claude_session_checked_at_unix_ms",
    )? {
        connection.execute(
            "ALTER TABLE instance_runtime_state ADD COLUMN claude_session_checked_at_unix_ms INTEGER",
            [],
        )?;
    }
    Ok(())
}

pub fn sync_tmux_wait_state(
    state_dir: &Path,
    workspace_id: &str,
    pane: &TmuxPane,
    state: &str,
    waiting: bool,
) -> AppResult<Option<Duration>> {
    let connection = open_bootstrapped_state_db(state_dir)?;
    let instance = find_or_create_tmux_instance_with_connection(&connection, workspace_id, pane)?;
    let existing = load_instance_runtime_state_with_connection(&connection, &instance.id)?;
    let now_ms = current_unix_ms()?;

    let wait_started_at_unix_ms = if waiting {
        existing
            .as_ref()
            .and_then(|row| row.wait_started_at_unix_ms)
            .unwrap_or(now_ms)
    } else {
        0
    };

    connection.execute(
        "INSERT INTO instance_runtime_state (\
            instance_id, last_state, wait_started_at_unix_ms, claude_session_id, \
            claude_session_checked_at_unix_ms\
         ) VALUES (?1, ?2, ?3, ?4, ?5) \
         ON CONFLICT(instance_id) DO UPDATE SET \
             last_state = excluded.last_state, \
             wait_started_at_unix_ms = excluded.wait_started_at_unix_ms, \
             claude_session_id = excluded.claude_session_id, \
             claude_session_checked_at_unix_ms = excluded.claude_session_checked_at_unix_ms",
        params![
            instance.id,
            state,
            if waiting {
                Some(wait_started_at_unix_ms)
            } else {
                None::<i64>
            },
            existing.as_ref().and_then(|row| row.claude_session_id.clone()),
            existing
                .as_ref()
                .and_then(|row| row.claude_session_checked_at_unix_ms),
        ],
    )?;

    if waiting {
        Ok(Some(Duration::from_millis(
            now_ms.saturating_sub(wait_started_at_unix_ms) as u64,
        )))
    } else {
        Ok(None)
    }
}

pub fn sync_tmux_claude_session_id(
    state_dir: &Path,
    workspace_id: &str,
    pane: &TmuxPane,
) -> AppResult<Option<String>> {
    let connection = open_bootstrapped_state_db(state_dir)?;
    let instance = find_or_create_tmux_instance_with_connection(&connection, workspace_id, pane)?;
    let existing = load_instance_runtime_state_with_connection(&connection, &instance.id)?;
    let now_ms = current_unix_ms()?;

    if let Some(existing) = existing.as_ref().filter(|row| {
        row.claude_session_checked_at_unix_ms
            .map(|checked_at| now_ms.saturating_sub(checked_at) < CLAUDE_SESSION_REVALIDATE_MS)
            .unwrap_or(false)
    }) {
        return Ok(existing.claude_session_id.clone());
    }

    let claude_session_id = resolve_claude_session_id_for_pane(pane)?;
    connection.execute(
        "INSERT INTO instance_runtime_state (\
            instance_id, last_state, wait_started_at_unix_ms, claude_session_id, \
            claude_session_checked_at_unix_ms\
         ) VALUES (?1, ?2, ?3, ?4, ?5) \
         ON CONFLICT(instance_id) DO UPDATE SET \
             last_state = excluded.last_state, \
             wait_started_at_unix_ms = excluded.wait_started_at_unix_ms, \
             claude_session_id = excluded.claude_session_id, \
             claude_session_checked_at_unix_ms = excluded.claude_session_checked_at_unix_ms",
        params![
            instance.id,
            existing.as_ref().and_then(|row| row.last_state.clone()),
            existing.as_ref().and_then(|row| row.wait_started_at_unix_ms),
            claude_session_id,
            Some(now_ms),
        ],
    )?;
    Ok(claude_session_id)
}

fn table_has_column(
    connection: &Connection,
    table_name: &str,
    column_name: &str,
) -> AppResult<bool> {
    let pragma = format!("PRAGMA table_info({table_name})");
    let mut statement = connection.prepare(&pragma)?;
    let rows = statement.query_map([], |row| row.get::<_, String>(1))?;
    for row in rows {
        if row? == column_name {
            return Ok(true);
        }
    }
    Ok(false)
}

fn ensure_legacy_workspace(connection: &Connection) -> AppResult<String> {
    if let Some(existing) = connection
        .query_row(
            "SELECT id FROM workspaces WHERE workspace_root = ?1",
            params![LEGACY_WORKSPACE_ROOT],
            |row| row.get::<_, String>(0),
        )
        .optional()?
    {
        return Ok(existing);
    }

    let workspace_id = new_uuid();
    connection.execute(
        "INSERT INTO workspaces (id, workspace_root, repo_key) VALUES (?1, ?2, NULL)",
        params![workspace_id, LEGACY_WORKSPACE_ROOT],
    )?;
    Ok(workspace_id)
}

fn open_bootstrapped_state_db(state_dir: &Path) -> AppResult<Connection> {
    let mut connection = open_state_db(state_dir)?;
    ensure_schema_version_table(&connection)?;

    let version = read_schema_version(&connection)?;
    let needs_migration = match version {
        Some(current) if current == CURRENT_SCHEMA_VERSION => {
            !schema_layout_matches_version(&connection, current)?
        }
        Some(_) | None => true,
    };

    if needs_migration {
        migrate_state_db(&mut connection)?;
    }

    Ok(connection)
}

pub fn bootstrap_state_db(state_dir: &Path) -> AppResult<PathBuf> {
    let db_path = state_db_path(state_dir);
    let _connection = open_bootstrapped_state_db(state_dir)?;
    Ok(db_path)
}

pub fn resolve_workspace(
    state_dir: &Path,
    selector: Option<&str>,
    cwd: &Path,
) -> AppResult<WorkspaceRecord> {
    let connection = open_bootstrapped_state_db(state_dir)?;
    if let Some(selector) = selector.filter(|value| !value.trim().is_empty()) {
        if Uuid::parse_str(selector).is_ok() {
            return load_workspace_by_id(&connection, selector)?
                .ok_or_else(|| AppError::new(format!("unknown workspace id: {selector}")));
        }
    }

    let requested_path = match selector {
        Some(raw) => {
            let path = Path::new(raw);
            if path.is_absolute() {
                path.to_path_buf()
            } else {
                cwd.join(path)
            }
        }
        None => cwd.to_path_buf(),
    };

    let locator = resolve_workspace_locator(&requested_path)?;
    resolve_or_create_workspace_with_connection(
        &connection,
        &locator.workspace_root.display().to_string(),
        locator.repo_key.as_deref(),
    )
}

pub fn resolve_workspace_for_path(state_dir: &Path, path: &Path) -> AppResult<WorkspaceRecord> {
    let connection = open_bootstrapped_state_db(state_dir)?;
    let locator = resolve_workspace_locator(path)?;
    resolve_or_create_workspace_with_connection(
        &connection,
        &locator.workspace_root.display().to_string(),
        locator.repo_key.as_deref(),
    )
}

fn resolve_or_create_workspace_with_connection(
    connection: &Connection,
    workspace_root: &str,
    repo_key: Option<&str>,
) -> AppResult<WorkspaceRecord> {
    if let Some(existing) = load_workspace_by_root(connection, workspace_root)? {
        return Ok(existing);
    }

    let workspace_id = new_uuid();
    connection.execute(
        "INSERT INTO workspaces (id, workspace_root, repo_key) VALUES (?1, ?2, ?3)",
        params![workspace_id, workspace_root, repo_key],
    )?;

    Ok(WorkspaceRecord {
        id: workspace_id,
        workspace_root: workspace_root.to_string(),
        repo_key: repo_key.map(str::to_string),
    })
}

fn load_workspace_by_id(
    connection: &Connection,
    workspace_id: &str,
) -> AppResult<Option<WorkspaceRecord>> {
    Ok(connection
        .query_row(
            "SELECT id, workspace_root, repo_key FROM workspaces WHERE id = ?1",
            params![workspace_id],
            |row| {
                Ok(WorkspaceRecord {
                    id: row.get(0)?,
                    workspace_root: row.get(1)?,
                    repo_key: row.get(2)?,
                })
            },
        )
        .optional()?)
}

fn load_workspace_by_root(
    connection: &Connection,
    workspace_root: &str,
) -> AppResult<Option<WorkspaceRecord>> {
    Ok(connection
        .query_row(
            "SELECT id, workspace_root, repo_key FROM workspaces WHERE workspace_root = ?1",
            params![workspace_root],
            |row| {
                Ok(WorkspaceRecord {
                    id: row.get(0)?,
                    workspace_root: row.get(1)?,
                    repo_key: row.get(2)?,
                })
            },
        )
        .optional()?)
}

pub fn store_pending_prompt(
    state_dir: &Path,
    workspace_id: &str,
    session_name: &str,
    content: &str,
) -> AppResult<()> {
    let connection = open_bootstrapped_state_db(state_dir)?;
    let instance = find_or_create_placeholder_instance_with_connection(
        &connection,
        workspace_id,
        session_name,
    )?;
    connection.execute(
        "INSERT INTO pending_prompts (instance_id, content) VALUES (?1, ?2) \
         ON CONFLICT(instance_id) DO UPDATE SET content = excluded.content",
        params![instance.id, content],
    )?;
    Ok(())
}

pub fn load_pending_prompt(
    state_dir: &Path,
    workspace_id: &str,
    session_name: &str,
) -> AppResult<Option<String>> {
    let connection = open_bootstrapped_state_db(state_dir)?;
    Ok(connection
        .query_row(
            "SELECT prompts.content \
             FROM pending_prompts prompts \
             JOIN instances ON instances.id = prompts.instance_id \
             WHERE instances.workspace_id = ?1 \
               AND instances.session_name = ?2 \
               AND instances.kind = ?3 \
               AND instances.active = 1",
            params![workspace_id, session_name, PLACEHOLDER_INSTANCE_KIND],
            |row| row.get::<_, String>(0),
        )
        .optional()?)
}

pub fn delete_pending_prompt(
    state_dir: &Path,
    workspace_id: &str,
    session_name: &str,
) -> AppResult<bool> {
    let connection = open_bootstrapped_state_db(state_dir)?;
    Ok(connection.execute(
        "DELETE FROM pending_prompts \
         WHERE instance_id IN (\
             SELECT id FROM instances \
             WHERE workspace_id = ?1 AND session_name = ?2 AND kind = ?3 AND active = 1\
         )",
        params![workspace_id, session_name, PLACEHOLDER_INSTANCE_KIND],
    )? > 0)
}

pub fn store_pending_prompt_for_tmux_instance(
    state_dir: &Path,
    workspace_id: &str,
    session_name: &str,
    pane: &TmuxPane,
    content: &str,
) -> AppResult<InstanceRecord> {
    let mut connection = open_bootstrapped_state_db(state_dir)?;
    let tx = connection.transaction()?;

    let tmux_instance = find_or_create_tmux_instance_with_connection(&tx, workspace_id, pane)?;
    if let Some(placeholder) =
        load_placeholder_instance_with_connection(&tx, workspace_id, session_name)?
    {
        if placeholder.id == tmux_instance.id {
            tx.execute(
                "UPDATE instances SET session_name = ?2 WHERE id = ?1",
                params![tmux_instance.id, session_name],
            )?;
        } else {
            tx.execute(
                "DELETE FROM pending_prompts WHERE instance_id = ?1",
                params![placeholder.id],
            )?;
            tx.execute(
                "UPDATE instances SET active = 0 WHERE id = ?1",
                params![placeholder.id],
            )?;
        }
    }

    tx.execute(
        "UPDATE instances SET session_name = ?2 WHERE id = ?1",
        params![tmux_instance.id, session_name],
    )?;
    tx.execute(
        "INSERT INTO pending_prompts (instance_id, content) VALUES (?1, ?2) \
         ON CONFLICT(instance_id) DO UPDATE SET content = excluded.content",
        params![tmux_instance.id, content],
    )?;
    tx.commit()?;
    load_instance_by_id(&connection, &tmux_instance.id)?.ok_or_else(|| {
        AppError::new(format!(
            "failed to reload instance {} after prompt store",
            tmux_instance.id
        ))
    })
}

pub fn store_babysit_registration(
    state_dir: &Path,
    workspace_id: &str,
    pane: &TmuxPane,
    enabled: bool,
) -> AppResult<InstanceRecord> {
    let connection = open_bootstrapped_state_db(state_dir)?;
    let instance = find_or_create_tmux_instance_with_connection(&connection, workspace_id, pane)?;
    connection.execute(
        "INSERT INTO babysit_registrations (instance_id, enabled) VALUES (?1, ?2) \
         ON CONFLICT(instance_id) DO UPDATE SET enabled = excluded.enabled",
        params![instance.id, enabled],
    )?;
    Ok(instance)
}

pub fn load_babysit_registration_by_pane_id(
    state_dir: &Path,
    pane_id: &str,
) -> AppResult<Option<BabysitRegistrationRecord>> {
    let connection = open_bootstrapped_state_db(state_dir)?;
    Ok(connection
        .query_row(
            "SELECT \
                instances.id, instances.workspace_id, instances.session_name, instances.pane_id, \
                instances.pane_tty, instances.pane_pid, instances.session_id, instances.window_id, \
                instances.window_name, instances.current_command, instances.current_path, \
                instances.kind, instances.active, babysit_registrations.enabled \
             FROM babysit_registrations \
             JOIN instances ON instances.id = babysit_registrations.instance_id \
             WHERE instances.pane_id = ?1",
            params![pane_id],
            row_to_babysit_registration,
        )
        .optional()?)
}

pub fn disable_babysit_registration_by_pane_id(state_dir: &Path, pane_id: &str) -> AppResult<bool> {
    let connection = open_bootstrapped_state_db(state_dir)?;
    Ok(connection.execute(
        "UPDATE babysit_registrations SET enabled = 0 \
         WHERE instance_id IN (SELECT id FROM instances WHERE pane_id = ?1)",
        params![pane_id],
    )? > 0)
}

pub fn list_babysit_registration_pane_ids(
    state_dir: &Path,
    workspace_id: Option<&str>,
) -> AppResult<Vec<String>> {
    let connection = open_bootstrapped_state_db(state_dir)?;
    let sql = if workspace_id.is_some() {
        "SELECT instances.pane_id \
         FROM babysit_registrations \
         JOIN instances ON instances.id = babysit_registrations.instance_id \
         WHERE instances.pane_id IS NOT NULL AND instances.workspace_id = ?1 \
         ORDER BY instances.pane_id"
    } else {
        "SELECT instances.pane_id \
         FROM babysit_registrations \
         JOIN instances ON instances.id = babysit_registrations.instance_id \
         WHERE instances.pane_id IS NOT NULL \
         ORDER BY instances.pane_id"
    };

    let mut statement = connection.prepare(sql)?;
    let mut pane_ids = Vec::new();
    if let Some(workspace_id) = workspace_id {
        let rows = statement.query_map(params![workspace_id], |row| row.get::<_, String>(0))?;
        for row in rows {
            pane_ids.push(row?);
        }
    } else {
        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
        for row in rows {
            pane_ids.push(row?);
        }
    }
    Ok(pane_ids)
}

fn find_or_create_placeholder_instance_with_connection(
    connection: &Connection,
    workspace_id: &str,
    session_name: &str,
) -> AppResult<InstanceRecord> {
    if let Some(instance) =
        load_placeholder_instance_with_connection(connection, workspace_id, session_name)?
    {
        return Ok(instance);
    }

    let instance_id = new_uuid();
    connection.execute(
        "INSERT INTO instances (id, workspace_id, session_name, kind, active) \
         VALUES (?1, ?2, ?3, ?4, 1)",
        params![
            instance_id,
            workspace_id,
            session_name,
            PLACEHOLDER_INSTANCE_KIND
        ],
    )?;
    load_instance_by_id(connection, &instance_id)?
        .ok_or_else(|| AppError::new(format!("failed to load placeholder instance {instance_id}")))
}

fn load_placeholder_instance_with_connection(
    connection: &Connection,
    workspace_id: &str,
    session_name: &str,
) -> AppResult<Option<InstanceRecord>> {
    Ok(connection
        .query_row(
            "SELECT id, workspace_id, session_name, pane_id, pane_tty, pane_pid, session_id, \
                window_id, window_name, current_command, current_path, kind, active \
             FROM instances \
             WHERE workspace_id = ?1 AND session_name = ?2 AND kind = ?3 AND active = 1",
            params![workspace_id, session_name, PLACEHOLDER_INSTANCE_KIND],
            row_to_instance,
        )
        .optional()?)
}

fn find_or_create_tmux_instance_with_connection(
    connection: &Connection,
    workspace_id: &str,
    pane: &TmuxPane,
) -> AppResult<InstanceRecord> {
    if let Some(existing) = load_instance_by_pane_id(connection, &pane.pane_id)? {
        connection.execute(
            "UPDATE instances SET \
                workspace_id = ?2, session_name = ?3, pane_tty = ?4, pane_pid = ?5, session_id = ?6, \
                window_id = ?7, window_name = ?8, current_command = ?9, current_path = ?10, \
                kind = ?11, active = 1 \
             WHERE id = ?1",
            params![
                existing.id,
                workspace_id,
                pane.session_name,
                pane.pane_tty,
                pane.pane_pid,
                pane.session_id,
                pane.window_id,
                pane.window_name,
                pane.current_command,
                pane.current_path,
                TMUX_INSTANCE_KIND,
            ],
        )?;
        return load_instance_by_id(connection, &existing.id)?.ok_or_else(|| {
            AppError::new(format!("failed to reload tmux instance {}", existing.id))
        });
    }

    let instance_id = new_uuid();
    connection.execute(
        "INSERT INTO instances (\
            id, workspace_id, session_name, pane_id, pane_tty, pane_pid, session_id, \
            window_id, window_name, current_command, current_path, kind, active\
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, 1)",
        params![
            instance_id,
            workspace_id,
            pane.session_name,
            pane.pane_id,
            pane.pane_tty,
            pane.pane_pid,
            pane.session_id,
            pane.window_id,
            pane.window_name,
            pane.current_command,
            pane.current_path,
            TMUX_INSTANCE_KIND,
        ],
    )?;
    load_instance_by_id(connection, &instance_id)?
        .ok_or_else(|| AppError::new(format!("failed to load tmux instance {instance_id}")))
}

fn load_instance_by_pane_id(
    connection: &Connection,
    pane_id: &str,
) -> AppResult<Option<InstanceRecord>> {
    Ok(connection
        .query_row(
            "SELECT id, workspace_id, session_name, pane_id, pane_tty, pane_pid, session_id, \
                window_id, window_name, current_command, current_path, kind, active \
             FROM instances WHERE pane_id = ?1",
            params![pane_id],
            row_to_instance,
        )
        .optional()?)
}

fn load_instance_by_id(
    connection: &Connection,
    instance_id: &str,
) -> AppResult<Option<InstanceRecord>> {
    Ok(connection
        .query_row(
            "SELECT id, workspace_id, session_name, pane_id, pane_tty, pane_pid, session_id, \
                window_id, window_name, current_command, current_path, kind, active \
             FROM instances WHERE id = ?1",
            params![instance_id],
            row_to_instance,
        )
        .optional()?)
}

fn load_instance_runtime_state_with_connection(
    connection: &Connection,
    instance_id: &str,
) -> AppResult<Option<InstanceRuntimeState>> {
    Ok(connection
        .query_row(
            "SELECT instance_id, last_state, wait_started_at_unix_ms, \
                claude_session_id, claude_session_checked_at_unix_ms \
             FROM instance_runtime_state WHERE instance_id = ?1",
            params![instance_id],
            |row| {
                Ok(InstanceRuntimeState {
                    instance_id: row.get(0)?,
                    last_state: row.get(1)?,
                    wait_started_at_unix_ms: row.get(2)?,
                    claude_session_id: row.get(3)?,
                    claude_session_checked_at_unix_ms: row.get(4)?,
                })
            },
        )
        .optional()?)
}

fn resolve_claude_session_id_for_pane(pane: &TmuxPane) -> AppResult<Option<String>> {
    let Some(projects_root) = claude_projects_root() else {
        return Ok(None);
    };

    for project_dir in candidate_claude_project_dirs(&projects_root, &pane.current_path) {
        if let Some(pid) = pane.pane_pid
            && let Some(session_id) = claude_session_id_from_pid(pid, &project_dir)?
        {
            return Ok(Some(session_id));
        }
        if let Some(session_id) = latest_claude_session_id(&project_dir)? {
            return Ok(Some(session_id));
        }
    }

    Ok(None)
}

fn claude_projects_root() -> Option<PathBuf> {
    Some(PathBuf::from(std::env::var_os("HOME")?).join(CLAUDE_PROJECTS_DIR))
}

fn candidate_claude_project_dirs(projects_root: &Path, current_path: &str) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    for ancestor in Path::new(current_path).ancestors() {
        if ancestor.as_os_str().is_empty() {
            continue;
        }
        let candidate = projects_root.join(encode_claude_project_path(ancestor));
        if candidate.is_dir() {
            candidates.push(candidate);
        }
    }
    candidates
}

fn encode_claude_project_path(path: &Path) -> String {
    path.display().to_string().replace('/', "-")
}

fn claude_session_id_from_pid(pid: u32, project_dir: &Path) -> AppResult<Option<String>> {
    claude_session_id_from_fd_dir(&PathBuf::from(format!("/proc/{pid}/fd")), project_dir)
}

fn claude_session_id_from_fd_dir(fd_dir: &Path, project_dir: &Path) -> AppResult<Option<String>> {
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
        if let Some(session_id) = claude_session_id_from_transcript_path(&target, project_dir) {
            return Ok(Some(session_id));
        }
    }
    Ok(None)
}

fn latest_claude_session_id(project_dir: &Path) -> AppResult<Option<String>> {
    let mut latest = None::<(SystemTime, String)>;
    for entry in fs::read_dir(project_dir)? {
        let entry = entry?;
        let path = entry.path();
        let Some(session_id) = claude_session_id_from_transcript_path(&path, project_dir) else {
            continue;
        };
        let modified = entry
            .metadata()?
            .modified()
            .unwrap_or(SystemTime::UNIX_EPOCH);
        if latest
            .as_ref()
            .map(|(latest_modified, _)| modified > *latest_modified)
            .unwrap_or(true)
        {
            latest = Some((modified, session_id));
        }
    }
    Ok(latest.map(|(_, session_id)| session_id))
}

fn claude_session_id_from_transcript_path(path: &Path, project_dir: &Path) -> Option<String> {
    if path.parent()? != project_dir {
        return None;
    }
    if path.extension()? != "jsonl" {
        return None;
    }
    read_session_id_from_transcript(path).ok().flatten()
}

fn read_session_id_from_transcript(path: &Path) -> AppResult<Option<String>> {
    let content = fs::read_to_string(path)?;
    for line in content.lines().take(8) {
        let value: Value = match serde_json::from_str(line) {
            Ok(value) => value,
            Err(_) => continue,
        };
        if let Some(session_id) = value.get("sessionId").and_then(Value::as_str) {
            return Ok(Some(session_id.to_string()));
        }
    }
    Ok(path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .map(str::to_string))
}

fn row_to_instance(row: &rusqlite::Row<'_>) -> rusqlite::Result<InstanceRecord> {
    Ok(InstanceRecord {
        id: row.get(0)?,
        workspace_id: row.get(1)?,
        session_name: row.get(2)?,
        pane_id: row.get(3)?,
        pane_tty: row.get(4)?,
        pane_pid: row.get(5)?,
        session_id: row.get(6)?,
        window_id: row.get(7)?,
        window_name: row.get(8)?,
        current_command: row.get(9)?,
        current_path: row.get(10)?,
        kind: row.get(11)?,
        active: row.get::<_, i64>(12)? != 0,
    })
}

fn row_to_babysit_registration(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<BabysitRegistrationRecord> {
    Ok(BabysitRegistrationRecord {
        instance: InstanceRecord {
            id: row.get(0)?,
            workspace_id: row.get(1)?,
            session_name: row.get(2)?,
            pane_id: row.get(3)?,
            pane_tty: row.get(4)?,
            pane_pid: row.get(5)?,
            session_id: row.get(6)?,
            window_id: row.get(7)?,
            window_name: row.get(8)?,
            current_command: row.get(9)?,
            current_path: row.get(10)?,
            kind: row.get(11)?,
            active: row.get::<_, i64>(12)? != 0,
        },
        enabled: row.get::<_, i64>(13)? != 0,
    })
}

fn new_uuid() -> String {
    Uuid::now_v7().to_string()
}

fn current_unix_ms() -> AppResult<i64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| AppError::new(error.to_string()))?
        .as_millis()
        .try_into()
        .map_err(|_| AppError::new("system clock timestamp overflowed i64"))?)
}

fn artifact_path(
    state_dir: &Path,
    subdir: &str,
    artifact_id: &str,
    file_name: &str,
) -> AppResult<PathBuf> {
    validate_artifact_path_token("artifact id", artifact_id)?;
    validate_artifact_path_token("artifact file name", file_name)?;
    let root = runtime_artifacts_root(state_dir).join(subdir);
    let artifact_dir = root.join(artifact_id);
    fs::create_dir_all(&artifact_dir)?;
    Ok(artifact_dir.join(file_name))
}

fn validate_artifact_path_token(label: &str, value: &str) -> AppResult<()> {
    let path = Path::new(value);
    if path.as_os_str().is_empty() {
        return Err(AppError::new(format!("{label} must not be empty")));
    }

    for component in path.components() {
        if !matches!(component, Component::Normal(_)) {
            return Err(AppError::new(format!(
                "{label} must stay within the state artifacts directory: {value}"
            )));
        }
    }

    Ok(())
}

#[cfg(any(test, rust_analyzer))]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::os::unix::fs::symlink;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use rusqlite::params;

    use super::{
        CURRENT_SCHEMA_VERSION, LEGACY_WORKSPACE_ROOT, SCHEMA_VERSION_ROW_ID, bootstrap_state_db,
        capture_artifact_path, claude_session_id_from_fd_dir, current_unix_ms,
        disable_babysit_registration_by_pane_id, encode_claude_project_path,
        ensure_schema_version_table, export_artifact_path, latest_claude_session_id,
        list_babysit_registration_pane_ids, load_babysit_registration_by_pane_id,
        load_pending_prompt, migrate_state_db, open_state_db, read_session_id_from_transcript,
        resolve_workspace, resolve_workspace_for_path, runtime_artifacts_root, state_db_path,
        store_babysit_registration, store_pending_prompt, store_pending_prompt_for_tmux_instance,
        sync_tmux_claude_session_id, sync_tmux_wait_state, table_exists, tape_artifact_path,
    };
    use crate::tmux::TmuxPane;

    #[test]
    fn state_db_path_uses_state_root_filename() {
        assert_eq!(
            state_db_path(std::path::Path::new("/tmp/botctl-state")),
            PathBuf::from("/tmp/botctl-state/state.db")
        );
    }

    #[test]
    fn bootstrap_state_db_creates_v2_tables() {
        let state_dir = unique_temp_dir("storage-state-db");
        let _ = fs::remove_dir_all(&state_dir);

        let db_path = bootstrap_state_db(&state_dir).expect("bootstrap should succeed");

        let connection = open_state_db(&state_dir).expect("db should reopen");
        let version: i64 = connection
            .query_row(
                "SELECT version FROM schema_version WHERE id = ?1",
                params![SCHEMA_VERSION_ROW_ID],
                |row| row.get(0),
            )
            .expect("schema version row should exist");

        assert_eq!(db_path, state_db_path(&state_dir));
        assert_eq!(version, CURRENT_SCHEMA_VERSION);
        assert!(table_exists(&connection, "workspaces").expect("workspaces should exist"));
        assert!(table_exists(&connection, "instances").expect("instances should exist"));
        assert!(
            table_exists(&connection, "pending_prompts").expect("pending prompts should exist")
        );
        assert!(
            table_exists(&connection, "babysit_registrations")
                .expect("babysit registrations should exist")
        );
        assert!(
            table_exists(&connection, "instance_runtime_state")
                .expect("instance runtime state should exist")
        );

        let _ = fs::remove_dir_all(&state_dir);
    }

    #[test]
    fn resolve_workspace_uses_path_and_uuid() {
        let state_dir = unique_temp_dir("storage-workspace");
        let cwd = unique_temp_dir("storage-workspace-cwd");
        fs::create_dir_all(&cwd).expect("cwd should create");

        let first = resolve_workspace(&state_dir, None, &cwd).expect("workspace should resolve");
        let second = resolve_workspace(&state_dir, Some(&first.id), &cwd)
            .expect("workspace id should resolve");

        assert_eq!(first, second);

        let _ = fs::remove_dir_all(&state_dir);
        let _ = fs::remove_dir_all(&cwd);
    }

    #[test]
    fn pending_prompts_are_workspace_scoped() {
        let state_dir = unique_temp_dir("storage-pending-prompts");
        let workspace_a_root = unique_temp_dir("workspace-a");
        let workspace_b_root = unique_temp_dir("workspace-b");
        fs::create_dir_all(&workspace_a_root).expect("workspace a should exist");
        fs::create_dir_all(&workspace_b_root).expect("workspace b should exist");

        let workspace_a = resolve_workspace_for_path(&state_dir, &workspace_a_root)
            .expect("workspace a should resolve");
        let workspace_b = resolve_workspace_for_path(&state_dir, &workspace_b_root)
            .expect("workspace b should resolve");

        store_pending_prompt(&state_dir, &workspace_a.id, "demo", "hello")
            .expect("first prompt should store");
        store_pending_prompt(&state_dir, &workspace_b.id, "demo", "world")
            .expect("second prompt should store");

        assert_eq!(
            load_pending_prompt(&state_dir, &workspace_a.id, "demo")
                .expect("first prompt should load"),
            Some(String::from("hello"))
        );
        assert_eq!(
            load_pending_prompt(&state_dir, &workspace_b.id, "demo")
                .expect("second prompt should load"),
            Some(String::from("world"))
        );

        let _ = fs::remove_dir_all(&state_dir);
        let _ = fs::remove_dir_all(&workspace_a_root);
        let _ = fs::remove_dir_all(&workspace_b_root);
    }

    #[test]
    fn store_pending_prompt_for_tmux_instance_promotes_placeholder() {
        let state_dir = unique_temp_dir("storage-prompt-promote");
        let workspace_root = unique_temp_dir("workspace-prompt-promote");
        fs::create_dir_all(&workspace_root).expect("workspace should exist");
        let workspace = resolve_workspace_for_path(&state_dir, &workspace_root)
            .expect("workspace should resolve");

        store_pending_prompt(&state_dir, &workspace.id, "demo", "before")
            .expect("placeholder prompt should store");
        let instance = store_pending_prompt_for_tmux_instance(
            &state_dir,
            &workspace.id,
            "demo",
            &sample_pane(),
            "after",
        )
        .expect("tmux prompt should store");

        let registration =
            load_babysit_registration_by_pane_id(&state_dir, "%1").expect("lookup should succeed");
        assert!(registration.is_none());
        assert_eq!(instance.pane_id.as_deref(), Some("%1"));

        let connection = open_state_db(&state_dir).expect("db should reopen");
        let prompt_instance_count: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM pending_prompts WHERE instance_id = ?1",
                params![instance.id],
                |row| row.get(0),
            )
            .expect("prompt row should exist");
        assert_eq!(prompt_instance_count, 1);

        let _ = fs::remove_dir_all(&state_dir);
        let _ = fs::remove_dir_all(&workspace_root);
    }

    #[test]
    fn babysit_registration_round_trips_and_filters_by_workspace() {
        let state_dir = unique_temp_dir("storage-babysit");
        let workspace_a_root = unique_temp_dir("workspace-babysit-a");
        let workspace_b_root = unique_temp_dir("workspace-babysit-b");
        fs::create_dir_all(&workspace_a_root).expect("workspace a should exist");
        fs::create_dir_all(&workspace_b_root).expect("workspace b should exist");
        let workspace_a = resolve_workspace_for_path(&state_dir, &workspace_a_root)
            .expect("workspace a should resolve");
        let workspace_b = resolve_workspace_for_path(&state_dir, &workspace_b_root)
            .expect("workspace b should resolve");

        let first = sample_pane();
        let mut second = sample_pane();
        second.pane_id = String::from("%22");
        second.session_name = String::from("demo-22");
        second.session_id = String::from("$22");
        second.window_id = String::from("@22");
        second.window_name = String::from("claude-22");
        second.current_path = workspace_b_root.display().to_string();

        store_babysit_registration(&state_dir, &workspace_a.id, &first, true)
            .expect("first registration should store");
        store_babysit_registration(&state_dir, &workspace_b.id, &second, true)
            .expect("second registration should store");

        assert_eq!(
            list_babysit_registration_pane_ids(&state_dir, None)
                .expect("global pane ids should list"),
            vec![String::from("%1"), String::from("%22")]
        );
        assert_eq!(
            list_babysit_registration_pane_ids(&state_dir, Some(&workspace_a.id))
                .expect("workspace pane ids should list"),
            vec![String::from("%1")]
        );
        assert!(
            disable_babysit_registration_by_pane_id(&state_dir, "%1")
                .expect("disable should succeed")
        );

        let first_record = load_babysit_registration_by_pane_id(&state_dir, "%1")
            .expect("record should load")
            .expect("record should exist");
        assert!(!first_record.enabled);

        let _ = fs::remove_dir_all(&state_dir);
        let _ = fs::remove_dir_all(&workspace_a_root);
        let _ = fs::remove_dir_all(&workspace_b_root);
    }

    #[test]
    fn sync_tmux_wait_state_persists_wait_start_across_calls() {
        let state_dir = unique_temp_dir("storage-runtime-state");
        let workspace_root = unique_temp_dir("workspace-runtime-state");
        fs::create_dir_all(&workspace_root).expect("workspace should exist");
        let workspace = resolve_workspace_for_path(&state_dir, &workspace_root)
            .expect("workspace should resolve");

        let pane = sample_pane();
        let first = sync_tmux_wait_state(&state_dir, &workspace.id, &pane, "ChatReady", true)
            .expect("first sync should succeed")
            .expect("first sync should return duration");
        std::thread::sleep(Duration::from_millis(20));
        let second = sync_tmux_wait_state(&state_dir, &workspace.id, &pane, "ChatReady", true)
            .expect("second sync should succeed")
            .expect("second sync should return duration");
        let cleared =
            sync_tmux_wait_state(&state_dir, &workspace.id, &pane, "BusyResponding", false)
                .expect("clear sync should succeed");

        assert!(second >= first);
        assert!(cleared.is_none());

        let _ = fs::remove_dir_all(&state_dir);
        let _ = fs::remove_dir_all(&workspace_root);
    }

    #[test]
    fn encode_claude_project_path_rewrites_slashes() {
        assert_eq!(
            encode_claude_project_path(std::path::Path::new("/home/colin/Projects/botctl")),
            "-home-colin-Projects-botctl"
        );
    }

    #[test]
    fn transcript_reader_prefers_embedded_session_id() {
        let root = unique_temp_dir("storage-claude-transcript");
        fs::create_dir_all(&root).expect("root should exist");
        let transcript = root.join("fallback.jsonl");
        fs::write(
            &transcript,
            "{\"type\":\"permission-mode\",\"sessionId\":\"session-123\"}\n",
        )
        .expect("transcript should write");

        assert_eq!(
            read_session_id_from_transcript(&transcript).expect("session id should read"),
            Some(String::from("session-123"))
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn latest_claude_session_id_uses_newest_top_level_transcript() {
        let root = unique_temp_dir("storage-claude-project-dir");
        fs::create_dir_all(&root).expect("project dir should exist");
        let older = root.join("older.jsonl");
        let newer = root.join("newer.jsonl");
        fs::write(&older, "{\"sessionId\":\"older\"}\n").expect("older should write");
        std::thread::sleep(Duration::from_millis(5));
        fs::write(&newer, "{\"sessionId\":\"newer\"}\n").expect("newer should write");
        fs::create_dir_all(root.join("subagents")).expect("subagents dir should exist");
        fs::write(
            root.join("subagents/ignored.jsonl"),
            "{\"sessionId\":\"ignored\"}\n",
        )
        .expect("subagent transcript should write");

        assert_eq!(
            latest_claude_session_id(&root).expect("latest session should resolve"),
            Some(String::from("newer"))
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn claude_session_id_from_fd_dir_prefers_open_transcript() {
        let root = unique_temp_dir("storage-claude-fd");
        let project_dir = root.join("project");
        let fd_dir = root.join("fd");
        fs::create_dir_all(&project_dir).expect("project dir should exist");
        fs::create_dir_all(&fd_dir).expect("fd dir should exist");
        let transcript = project_dir.join("session-from-fd.jsonl");
        fs::write(
            &transcript,
            "{\"type\":\"permission-mode\",\"sessionId\":\"session-from-fd\"}\n",
        )
        .expect("transcript should write");
        symlink(&transcript, fd_dir.join("7")).expect("fd symlink should create");

        assert_eq!(
            claude_session_id_from_fd_dir(&fd_dir, &project_dir).expect("fd lookup should work"),
            Some(String::from("session-from-fd"))
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn sync_tmux_claude_session_id_resolves_from_project_transcript() {
        let state_dir = unique_temp_dir("storage-claude-sync-state");
        let home_dir = unique_temp_dir("storage-claude-sync-home");
        let workspace_root = home_dir.join("project");
        fs::create_dir_all(&workspace_root).expect("workspace root should exist");
        let project_dir = home_dir
            .join(".claude/projects")
            .join(encode_claude_project_path(&workspace_root));
        fs::create_dir_all(&project_dir).expect("project dir should exist");
        let transcript = project_dir.join("session-live.jsonl");
        fs::write(
            &transcript,
            format!(
                "{{\"type\":\"permission-mode\",\"sessionId\":\"session-live\"}}\n{{\"cwd\":\"{}\",\"sessionId\":\"session-live\"}}\n",
                workspace_root.display()
            ),
        )
        .expect("transcript should write");

        let workspace = resolve_workspace_for_path(&state_dir, &workspace_root)
            .expect("workspace should resolve");
        let mut pane = sample_pane();
        pane.current_path = workspace_root.display().to_string();
        pane.pane_pid = None;

        let previous_home = std::env::var_os("HOME");
        unsafe {
            std::env::set_var("HOME", &home_dir);
        }
        let resolved = sync_tmux_claude_session_id(&state_dir, &workspace.id, &pane)
            .expect("claude session should sync");
        match previous_home {
            Some(home) => unsafe { std::env::set_var("HOME", home) },
            None => unsafe { std::env::remove_var("HOME") },
        }

        assert_eq!(resolved, Some(String::from("session-live")));

        let _ = current_unix_ms().expect("clock should work");
        let _ = fs::remove_dir_all(&state_dir);
        let _ = fs::remove_dir_all(&home_dir);
    }

    #[test]
    fn migrate_state_db_upgrades_v1_rows_into_legacy_workspace() {
        let state_dir = unique_temp_dir("storage-migrate-v1");
        let _ = fs::remove_dir_all(&state_dir);
        let mut connection = open_state_db(&state_dir).expect("db should open");
        super::migrate_to_v1(&connection).expect("v1 tables should create");
        ensure_schema_version_table(&connection).expect("schema version should exist");
        connection
            .execute(
                "INSERT INTO schema_version (id, version) VALUES (?1, 1)",
                params![SCHEMA_VERSION_ROW_ID],
            )
            .expect("v1 version should store");
        connection
            .execute(
                "INSERT INTO pending_prompts (session_name, content) VALUES ('demo', 'hello')",
                [],
            )
            .expect("v1 pending prompt should insert");
        connection
            .execute(
                "INSERT INTO babysit_registrations (pane_id, enabled, pane_tty, pane_pid, session_id, session_name, window_id, window_name, current_command, current_path) \
                 VALUES ('%1', 1, '/dev/pts/1', 123, '$1', 'demo', '@1', 'claude', 'claude', '/tmp/demo')",
                [],
            )
            .expect("v1 babysit row should insert");

        migrate_state_db(&mut connection).expect("migration should succeed");

        let legacy_workspace_count: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM workspaces WHERE workspace_root = ?1",
                params![LEGACY_WORKSPACE_ROOT],
                |row| row.get(0),
            )
            .expect("legacy workspace should count");
        let instance_count: i64 = connection
            .query_row("SELECT COUNT(*) FROM instances", [], |row| row.get(0))
            .expect("instances should count");

        assert_eq!(legacy_workspace_count, 1);
        assert_eq!(instance_count, 2);

        let _ = fs::remove_dir_all(&state_dir);
    }

    #[test]
    fn runtime_artifact_paths_live_under_state_root_artifacts() {
        let state_dir = unique_temp_dir("storage-artifacts");
        let _ = fs::remove_dir_all(&state_dir);

        let captures = capture_artifact_path(&state_dir, "observe-demo", "capture.txt")
            .expect("capture artifact path should resolve");
        let tapes = tape_artifact_path(&state_dir, "serve-demo", "events.jsonl")
            .expect("tape artifact path should resolve");
        let exports = export_artifact_path(&state_dir, "observe-demo", "report.json")
            .expect("export artifact path should resolve");

        assert_eq!(
            runtime_artifacts_root(&state_dir),
            state_dir.join("artifacts")
        );
        assert_eq!(
            captures,
            state_dir.join("artifacts/captures/observe-demo/capture.txt")
        );
        assert_eq!(
            tapes,
            state_dir.join("artifacts/tapes/serve-demo/events.jsonl")
        );
        assert_eq!(
            exports,
            state_dir.join("artifacts/exports/observe-demo/report.json")
        );

        let _ = fs::remove_dir_all(&state_dir);
    }

    fn sample_pane() -> TmuxPane {
        TmuxPane {
            pane_id: String::from("%1"),
            pane_tty: String::from("/dev/pts/1"),
            pane_pid: Some(123),
            session_id: String::from("$1"),
            session_name: String::from("demo"),
            window_id: String::from("@1"),
            window_index: 0,
            window_name: String::from("claude"),
            pane_index: 0,
            current_command: String::from("claude"),
            current_path: String::from("/tmp/demo"),
            pane_active: true,
            cursor_x: Some(1),
            cursor_y: Some(2),
        }
    }

    fn unique_temp_dir(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be valid")
            .as_nanos();
        std::env::temp_dir().join(format!("botctl-{label}-{}-{nanos}", std::process::id()))
    }
}
